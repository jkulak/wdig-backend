//! The router and what every handler shares.

use std::sync::Arc;

use axum::Router;
use sqlx::PgPool;

use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::{accounts, health, ingest, read};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub version: String,
    pub signing_key: Arc<String>,
    pub access_ttl_seconds: i64,
    pub refresh_ttl_days: i64,
    pub login_limiter: Arc<RateLimiter>,
    pub ingest_limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn new(pool: PgPool, config: &Config) -> Self {
        Self {
            pool,
            version: config.version.clone(),
            signing_key: Arc::new(config.token_signing_key.clone()),
            access_ttl_seconds: config.access_token_ttl_seconds,
            refresh_ttl_days: config.refresh_token_ttl_days,
            login_limiter: Arc::new(RateLimiter::new()),
            ingest_limiter: Arc::new(RateLimiter::new()),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(health::router())
        .merge(accounts::router())
        .merge(ingest::router())
        .merge(read::router())
        .with_state(state)
}
