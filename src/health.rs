//! `GET /api/v1/health`: 200 when the database answers, 503 when it does not, and the
//! version of the running build either way.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::app::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
    pub db: &'static str,
    pub version: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/health", get(health))
}

async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    match sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                db: "ok",
                version: state.version.clone(),
            }),
        ),
        Err(error) => {
            tracing::warn!(error = %error, "health: the database does not answer");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(Health {
                    status: "degraded",
                    db: "unavailable",
                    version: state.version.clone(),
                }),
            )
        }
    }
}
