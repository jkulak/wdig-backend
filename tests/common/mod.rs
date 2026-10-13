//! What every integration test needs: the router over the test database, a way to call it,
//! and an account that is signed in. The binary's modules are included by path at each test
//! crate's root, as the crate has no library; this module reaches them through `crate::`.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::{accounts, app, config};

pub async fn test_app() -> (axum::Router, sqlx::PgPool) {
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

/// A JSON request; the answer's status and body.
pub async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _, value) = call_raw(router, method, path, body.to_string().into(), bearer).await;
    (status, value)
}

/// A request with any bytes as its body; the answer's status, headers and body.
pub async fn call_raw(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    bearer: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, value)
}

pub fn device(id: Uuid) -> Value {
    json!({"id": id, "name": "iPhone15,2", "platform": "ios", "app_version": "0.1.0 (1)"})
}

pub fn fresh_email() -> String {
    format!("kuba+{}@example.com", Uuid::now_v7().simple())
}

/// A signed-in account: its access token, its device and its id.
pub struct Account {
    pub token: String,
    pub device_id: Uuid,
    pub user_id: Uuid,
    pub email: String,
    pub password: String,
}

pub async fn signed_in(router: &axum::Router, pool: &sqlx::PgPool) -> Account {
    let code = accounts::make_invites(pool, 1).await.unwrap().remove(0);
    let email = fresh_email();
    let password = "correct horse battery";
    let (status, body) = call(
        router,
        "POST",
        "/api/v1/auth/register",
        json!({"email": email, "password": password, "invite_code": code}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let device_id = Uuid::now_v7();
    let (status, body) = call(
        router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": email, "password": password, "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_id: Uuid = sqlx::query_scalar("select id from users where email = $1::citext")
        .bind(&email)
        .fetch_one(pool)
        .await
        .unwrap();
    Account {
        token: body["access_token"].as_str().unwrap().to_string(),
        device_id,
        user_id,
        email,
        password: password.to_string(),
    }
}

/// The same account signed in on a second phone.
pub async fn another_device(router: &axum::Router, account: &Account) -> Account {
    let device_id = Uuid::now_v7();
    let (status, body) = call(
        router,
        "POST",
        "/api/v1/auth/login",
        json!({"email": account.email, "password": account.password, "device": device(device_id)}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Account {
        token: body["access_token"].as_str().unwrap().to_string(),
        device_id,
        user_id: account.user_id,
        email: account.email.clone(),
        password: account.password.clone(),
    }
}
