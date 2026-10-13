//! Configuration from the environment only (12-factor): nothing is read from a file by the
//! binary, Compose reads `.env` on its behalf in development.

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub app_env: String,
    pub rust_log: String,
    /// The crate version plus the build's git sha or release tag, for `/health`.
    pub version: String,
    /// Signs the access tokens; at least 32 bytes, required to serve.
    pub token_signing_key: String,
    pub access_token_ttl_seconds: i64,
    pub refresh_token_ttl_days: i64,
}

impl Config {
    /// What `serve` needs beyond the database.
    pub fn check_for_serving(&self) -> Result<()> {
        if self.token_signing_key.len() < 32 {
            anyhow::bail!(
                "TOKEN_SIGNING_KEY is missing or shorter than 32 bytes; the service cannot sign tokens"
            );
        }
        Ok(())
    }

    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .context("DATABASE_URL is not set; the service cannot start without its database")?;
        // The Dockerfile passes both as build arguments, empty when a local build sets none.
        let build = option_env!("APP_VERSION")
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let sha = option_env!("GIT_SHA")
                    .filter(|value| !value.is_empty())
                    .unwrap_or("dev");
                format!("{}+{sha}", env!("CARGO_PKG_VERSION"))
            });
        let token_signing_key = std::env::var("TOKEN_SIGNING_KEY").unwrap_or_default();
        Ok(Self {
            token_signing_key,
            access_token_ttl_seconds: std::env::var("ACCESS_TOKEN_TTL_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
            refresh_token_ttl_days: std::env::var("REFRESH_TOKEN_TTL_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(90),
            database_url,
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            app_env: std::env::var("APP_ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            version: build,
        })
    }
}
