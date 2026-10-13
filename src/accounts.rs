//! Accounts: registration with an invite code, login with the device's registration, refresh
//! tokens that rotate and are revoked per device, logout, and the invite-code CLI.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::{AuthUser, issue_access_token};
use crate::errors::ApiError;

const PASSWORD_MIN: usize = 10;
const PASSWORD_MAX: usize = 128;
const LOGIN_FAILURES_PER_EMAIL: usize = 10;
const LOGIN_FAILURES_PER_ADDRESS: usize = 30;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/logout", post(logout))
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    pub invite_code: String,
}

#[derive(Deserialize)]
pub struct DeviceRequest {
    pub id: Uuid,
    pub name: String,
    pub platform: String,
    pub app_version: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    pub device: DeviceRequest,
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub expires_in: i64,
    pub refresh_token: String,
}

fn check_password(password: &str) -> Result<(), ApiError> {
    let length = password.chars().count();
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "password_policy",
            format!("a password has {PASSWORD_MIN} to {PASSWORD_MAX} characters"),
        ));
    }
    Ok(())
}

fn normalise_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim();
    if email.is_empty()
        || !email.contains('@')
        || email.contains(char::is_whitespace)
        || email.len() > 254
    {
        return Err(ApiError::validation("email does not look like an address"));
    }
    Ok(email.to_string())
}

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(ApiError::internal)
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// The hash of a password nobody has, verified when the e-mail is unknown so that both
/// answers take argon2's time.
fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_password("a password nobody has").unwrap_or_default())
}

fn new_refresh_token() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let hash = token_hash(&token);
    (token, hash)
}

pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<StatusCode, ApiError> {
    let email = normalise_email(&body.email)?;
    check_password(&body.password)?;
    let code = body.invite_code.trim().to_ascii_uppercase();
    let hash = hash_password(&body.password)?;
    let mut tx = state.pool.begin().await?;
    let unused: Option<bool> =
        sqlx::query_scalar("select used_by is null from invite_codes where code = $1 for update")
            .bind(&code)
            .fetch_optional(&mut *tx)
            .await?;
    if unused != Some(true) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_invite_code",
            "the invite code is unknown or already used",
        ));
    }
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "insert into users (email, password_hash) values ($1, $2) returning id",
    )
    .bind(&email)
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await;
    let user_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(error)) if error.constraint() == Some("users_email_key") => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "email_taken",
                "an account with this e-mail exists",
            ));
        }
        Err(sqlx::Error::Database(error)) if error.constraint() == Some("users_email_check") => {
            return Err(ApiError::validation("email does not look like an address"));
        }
        Err(error) => return Err(error.into()),
    };
    sqlx::query("update invite_codes set used_by = $1, used_at = now() where code = $2")
        .bind(user_id)
        .bind(&code)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user_id = %user_id, "registered");
    Ok(StatusCode::CREATED)
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let email = normalise_email(&body.email)?;
    if body.device.platform != "ios" {
        return Err(ApiError::validation("device.platform has to be ios"));
    }
    if body.device.name.is_empty()
        || body.device.name.len() > 200
        || body.device.app_version.len() > 100
    {
        return Err(ApiError::validation(
            "device.name and device.app_version are required and short",
        ));
    }
    let email_key = format!("login:email:{}", email.to_ascii_lowercase());
    let address_key = format!("login:addr:{}", peer.ip());
    if let Err(retry) =
        state
            .login_limiter
            .hit(&address_key, LOGIN_FAILURES_PER_ADDRESS, LOGIN_WINDOW)
    {
        return Err(ApiError::rate_limited(retry));
    }
    if let Err(retry) = state
        .login_limiter
        .hit(&email_key, LOGIN_FAILURES_PER_EMAIL, LOGIN_WINDOW)
    {
        return Err(ApiError::rate_limited(retry));
    }
    let row: Option<(Uuid, String, Option<DateTime<Utc>>)> =
        sqlx::query_as("select id, password_hash, deleted_at from users where email = $1::citext")
            .bind(&email)
            .fetch_optional(&state.pool)
            .await?;
    let (user_id, hash, deleted_at) = match row {
        Some(row) => row,
        None => {
            verify_password(&body.password, dummy_hash());
            return Err(ApiError::unauthorized(
                "invalid_credentials",
                "the e-mail or the password is wrong",
            ));
        }
    };
    if deleted_at.is_some() || !verify_password(&body.password, &hash) {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "the e-mail or the password is wrong",
        ));
    }
    state.login_limiter.clear(&email_key);
    let mut tx = state.pool.begin().await?;
    let owner: Option<Uuid> = sqlx::query_scalar("select user_id from devices where id = $1")
        .bind(body.device.id)
        .fetch_optional(&mut *tx)
        .await?;
    match owner {
        Some(owner) if owner != user_id => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "device_owned_by_other_user",
                "this device id belongs to another account",
            ));
        }
        Some(_) => {
            sqlx::query("update devices set name = $2, app_version = $3, last_seen_at = now() where id = $1")
                .bind(body.device.id)
                .bind(&body.device.name)
                .bind(&body.device.app_version)
                .execute(&mut *tx)
                .await?;
        }
        None => {
            sqlx::query("insert into devices (id, user_id, name, platform, app_version) values ($1, $2, $3, 'ios', $4)")
                .bind(body.device.id)
                .bind(user_id)
                .bind(&body.device.name)
                .bind(&body.device.app_version)
                .execute(&mut *tx)
                .await?;
        }
    }
    let pair = issue_pair(&mut tx, &state, user_id, body.device.id).await?;
    tx.commit().await?;
    tracing::info!(user_id = %user_id, device_id = %body.device.id, "login");
    Ok(Json(pair))
}

async fn issue_pair(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    state: &AppState,
    user_id: Uuid,
    device_id: Uuid,
) -> Result<TokenPair, ApiError> {
    let (refresh_token, hash) = new_refresh_token();
    sqlx::query(
        "insert into refresh_tokens (device_id, token_hash, expires_at) values ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(device_id)
    .bind(&hash)
    .bind(state.refresh_ttl_days as i32)
    .execute(&mut **tx)
    .await?;
    let access_token = issue_access_token(
        user_id,
        device_id,
        &state.signing_key,
        state.access_ttl_seconds,
    )?;
    Ok(TokenPair {
        access_token,
        expires_in: state.access_ttl_seconds,
        refresh_token,
    })
}

/// A refresh token's row with its owner's state: id, device, revoked, expires, owner deleted.
type RefreshRow = (
    Uuid,
    Uuid,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let hash = token_hash(body.refresh_token.trim());
    let mut tx = state.pool.begin().await?;
    let row: Option<RefreshRow> = sqlx::query_as(
        "select t.id, t.device_id, t.revoked_at, t.expires_at, u.deleted_at
         from refresh_tokens t join devices d on d.id = t.device_id join users u on u.id = d.user_id
         where t.token_hash = $1 for update of t",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((token_id, device_id, revoked_at, expires_at, deleted_at)) = row else {
        return Err(ApiError::unauthorized(
            "invalid_refresh_token",
            "the refresh token is unknown",
        ));
    };
    if revoked_at.is_some() {
        // A token presented after its rotation is reuse: everything of that device goes.
        sqlx::query("update refresh_tokens set revoked_at = now() where device_id = $1 and revoked_at is null")
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::warn!(device_id = %device_id, "refresh token reused; the device's tokens are revoked");
        return Err(ApiError::unauthorized(
            "invalid_refresh_token",
            "the refresh token was already used",
        ));
    }
    if expires_at <= Utc::now() || deleted_at.is_some() {
        return Err(ApiError::unauthorized(
            "invalid_refresh_token",
            "the refresh token has expired",
        ));
    }
    let user_id: Uuid = sqlx::query_scalar("select user_id from devices where id = $1")
        .bind(device_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("update refresh_tokens set revoked_at = now() where id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update devices set last_seen_at = now() where id = $1")
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
    let pair = issue_pair(&mut tx, &state, user_id, device_id).await?;
    tx.commit().await?;
    Ok(Json(pair))
}

async fn logout(State(state): State<AppState>, user: AuthUser) -> Result<StatusCode, ApiError> {
    sqlx::query(
        "update refresh_tokens set revoked_at = now() where device_id = $1 and revoked_at is null",
    )
    .bind(user.device_id)
    .execute(&state.pool)
    .await?;
    tracing::info!(user_id = %user.user_id, device_id = %user.device_id, "logout");
    Ok(StatusCode::NO_CONTENT)
}

/// `wdig invite --count N`: codes of 12 characters from A-Z and 2-9, without 0, 1, O or I.
pub async fn make_invites(pool: &PgPool, count: u32) -> Result<Vec<String>> {
    let mut codes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let code: String = {
            let mut rng = rand::rng();
            (0..12)
                .map(|_| INVITE_ALPHABET[rng.random_range(0..INVITE_ALPHABET.len())] as char)
                .collect()
        };
        sqlx::query("insert into invite_codes (code) values ($1)")
            .bind(&code)
            .execute(pool)
            .await?;
        codes.push(code);
    }
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_not_another() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("wrong horse", &hash));
        assert!(!verify_password("anything", dummy_hash()));
    }

    #[test]
    fn the_password_policy_is_ten_to_one_hundred_twenty_eight_characters() {
        assert!(check_password("123456789").is_err());
        assert!(check_password("1234567890").is_ok());
        assert!(check_password(&"x".repeat(128)).is_ok());
        assert!(check_password(&"x".repeat(129)).is_err());
    }

    #[test]
    fn a_refresh_token_is_forty_three_characters_and_its_hash_is_stable() {
        let (token, hash) = new_refresh_token();
        assert_eq!(token.len(), 43);
        assert_eq!(hash, token_hash(&token));
        assert_eq!(hash.len(), 64);
    }
}
