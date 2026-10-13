//! Access tokens and the extractor every protected route uses. Access tokens are short-lived
//! HS256 JWTs verified without a database read; the extractor still looks the user up, so a
//! deleted account is refused within one request, not within fifteen minutes.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::AppState;
use crate::errors::ApiError;

#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub device_id: Uuid,
    pub iat: i64,
    pub exp: i64,
}

pub fn issue_access_token(
    user_id: Uuid,
    device_id: Uuid,
    key: &str,
    ttl_seconds: i64,
) -> Result<String, ApiError> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        device_id,
        iat: now.timestamp(),
        exp: (now + Duration::seconds(ttl_seconds)).timestamp(),
    };
    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(key.as_bytes()),
    )
    .map_err(ApiError::internal)
}

pub fn verify_access_token(token: &str, key: &str) -> Result<Claims, ApiError> {
    let mut validation = Validation::default();
    validation.leeway = 5;
    match jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(key.as_bytes()),
        &validation,
    ) {
        Ok(data) => Ok(data.claims),
        Err(error)
            if matches!(
                error.kind(),
                jsonwebtoken::errors::ErrorKind::ExpiredSignature
            ) =>
        {
            Err(ApiError::unauthorized(
                "token_expired",
                "the access token has expired; refresh it",
            ))
        }
        Err(_) => Err(ApiError::unauthorized(
            "unauthorized",
            "a valid bearer token is required",
        )),
    }
}

/// The caller: who and from which device, from the bearer token.
#[derive(Clone, Copy, Debug)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub device_id: Uuid,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let token = header.strip_prefix("Bearer ").ok_or_else(|| {
            ApiError::unauthorized("unauthorized", "a valid bearer token is required")
        })?;
        let claims = verify_access_token(token, &state.signing_key)?;
        let live: Option<bool> =
            sqlx::query_scalar("select deleted_at is null from users where id = $1")
                .bind(claims.sub)
                .fetch_optional(&state.pool)
                .await?;
        if live != Some(true) {
            return Err(ApiError::unauthorized(
                "unauthorized",
                "the account does not exist",
            ));
        }
        Ok(AuthUser {
            user_id: claims.sub,
            device_id: claims.device_id,
        })
    }
}
