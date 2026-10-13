//! The accounts flow against the test database: register, login, refresh, logout, and the
//! limits and refusals around them. Needs DATABASE_URL and TOKEN_SIGNING_KEY (scripts/test.sh
//! and CI set them). The binary's modules are included by path, as the crate has no library.

// The included modules carry what the binary uses and the tests do not.
#![allow(dead_code)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

mod common;

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
#[path = "../src/ingest.rs"]
mod ingest;
#[path = "../src/ratelimit.rs"]
mod ratelimit;

use common::*;

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
