//! The accounts flow against the test database: register, login, refresh, logout, and the
//! limits and refusals around them. Needs DATABASE_URL and TOKEN_SIGNING_KEY (scripts/test.sh
//! and CI set them). The binary's modules are included by path, as the crate has no library.

// The included modules carry what the binary uses and the tests do not.
#![allow(dead_code)]

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

#[path = "../src/accounts.rs"]
mod accounts;
#[path = "../src/app.rs"]
mod app;
#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/errors.rs"]
mod errors;
#[path = "../src/health.rs"]
mod health;
#[path = "../src/ratelimit.rs"]
mod ratelimit;

async fn test_app() -> (axum::Router, sqlx::PgPool) {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for the test database");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("the test database");
    let config = config::Config::from_env().unwrap();
    assert!(
        config.check_for_serving().is_ok(),
        "TOKEN_SIGNING_KEY for the tests"
    );
    let state = app::AppState::new(pool.clone(), &config);
    let router =
        app::router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
    (router, pool)
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn device(id: Uuid) -> Value {
    json!({"id": id, "name": "iPhone15,2", "platform": "ios", "app_version": "0.1.0 (1)"})
}

fn fresh_email() -> String {
    format!("kuba+{}@example.com", Uuid::now_v7().simple())
}

#[tokio::test]
async fn register_login_refresh_and_logout_work_in_that_order() {
    let (router, pool) = test_app().await;
    let code = accounts::make_invites(&pool, 1).await.unwrap().remove(0);
    assert_eq!(code.len(), 12);
    let email = fresh_email();
    let password = "correct horse battery";

    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email, "password": password, "invite_code": code}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let hash: String = sqlx::query_scalar("select password_hash from users where email = $1")
        .bind(&email)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": fresh_email(), "password": password, "invite_code": code}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_invite_code");

    let device_id = Uuid::now_v7();
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email.to_uppercase(), "password": password, "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["expires_in"], 900);
    let access = body["access_token"].as_str().unwrap().to_string();
    let refresh = body["refresh_token"].as_str().unwrap().to_string();
    let stored: String = sqlx::query_scalar(
        "select token_hash from refresh_tokens where device_id = $1 order by issued_at desc",
    )
    .bind(device_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, accounts::token_hash(&refresh));

    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email, "password": "wrong password here", "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_credentials");

    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/refresh",
        json!({"refresh_token": refresh}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let refresh2 = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(refresh2, refresh);
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/refresh",
        json!({"refresh_token": refresh}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_refresh_token");
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/refresh",
        json!({"refresh_token": refresh2}),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "reuse revokes every live token of the device"
    );

    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email, "password": password, "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let refresh3 = body["refresh_token"].as_str().unwrap().to_string();
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/logout",
        Value::Null,
        Some(&access),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/refresh",
        json!({"refresh_token": refresh3}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&router, "POST", "/api/v1/auth/logout", Value::Null, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn refusals_name_their_reason() {
    let (router, pool) = test_app().await;
    let code = accounts::make_invites(&pool, 1).await.unwrap().remove(0);
    let email = fresh_email();
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email, "password": "short", "invite_code": code}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "password_policy");
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email, "password": "long enough password", "invite_code": code}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let code2 = accounts::make_invites(&pool, 1).await.unwrap().remove(0);
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email.to_uppercase(), "password": "long enough password", "invite_code": code2}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "email_taken");

    let device_id = Uuid::now_v7();
    let android = json!({"id": device_id, "name": "x", "platform": "android", "app_version": "1"});
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email, "password": "long enough password", "device": android}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "validation");
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email, "password": "long enough password", "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let email2 = fresh_email();
    let code3 = accounts::make_invites(&pool, 1).await.unwrap().remove(0);
    let (status, _) = call(
        &router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email2, "password": "long enough password", "invite_code": code3}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email2, "password": "long enough password", "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "device_owned_by_other_user");

    for _ in 0..10 {
        let (status, _) = call(
            &router,
            "POST",
            "/api/v1/auth/login",
            json!({"email": email2, "password": "not the password", "device": device(Uuid::now_v7())}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email2, "password": "not the password", "device": device(Uuid::now_v7())}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "rate_limited");
}
