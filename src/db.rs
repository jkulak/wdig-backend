//! The connection pool. Every connection gets a statement timeout, so one slow query never
//! holds the pool; the worker that needs longer takes its own setting on its own connection.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::config::Config;

pub async fn connect(config: &Config) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("set statement_timeout = '15s'")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy(&config.database_url)
        .context("DATABASE_URL does not parse")
}
