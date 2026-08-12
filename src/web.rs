// Rust guideline compliant 2026-08-12
//! Dashboard HTTP server.
//!
//! Serves the single-page dashboard at `/` and two JSON endpoints:
//! `/api/latest` (most recent reading) and `/api/readings?hours=N`
//! (bucket-averaged history, capped at roughly 1000 points).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::db::{Db, Reading};
use crate::unix_ts_now;

const INDEX_HTML: &str = include_str!("index.html");

/// Longest queryable window; a year covers the full renovation timeline.
const MAX_HOURS: u32 = 24 * 366;

/// Rough cap on points returned per query; keeps chart payloads small.
const MAX_POINTS: i64 = 1000;

/// Serves the dashboard on `listen` until the process exits.
///
/// # Errors
/// Returns an error if the listener cannot bind or the server fails.
pub async fn serve(db: Db, listen: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/latest", get(latest))
        .route("/api/readings", get(readings))
        .with_state(db);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::event!(
        name: "server.listen.start",
        tracing::Level::INFO,
        server.address = %listen,
        "dashboard listening",
    );
    axum::serve(listener, app)
        .await
        .context("serving dashboard")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn latest(State(db): State<Db>) -> Result<Json<Option<Reading>>, AppError> {
    let reading = tokio::task::spawn_blocking(move || db.latest()).await??;
    Ok(Json(reading))
}

#[derive(Debug, Deserialize)]
struct ReadingsParams {
    hours: Option<u32>,
}

async fn readings(
    State(db): State<Db>,
    Query(params): Query<ReadingsParams>,
) -> Result<Json<Vec<Reading>>, AppError> {
    let hours = params.hours.unwrap_or(24).clamp(1, MAX_HOURS);
    let window_secs = i64::from(hours) * 3600;
    let from_ts = unix_ts_now() - window_secs;
    let bucket_secs = (window_secs / MAX_POINTS).max(1);
    let rows = tokio::task::spawn_blocking(move || db.since(from_ts, bucket_secs)).await??;
    Ok(Json(rows))
}

/// Maps any internal error to a plain 500 response.
#[derive(Debug)]
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::event!(
            name: "server.request.failure",
            tracing::Level::ERROR,
            error.message = %self.0,
            "request failed",
        );
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}
