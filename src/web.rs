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
use serde::{Deserialize, Serialize};

use crate::db::{DailyExtremes, DayRisk, Db, DeltaPoint, OutdoorReading, Reading, Records};
use crate::unix_ts_now;
use crate::weather::{self, Assessment};

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
        .route("/api/records", get(records))
        .route("/api/daily", get(daily))
        .route("/api/conditions", get(conditions))
        .route("/api/risk", get(risk))
        .route("/api/delta", get(delta))
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

async fn records(State(db): State<Db>) -> Result<Json<Option<Records>>, AppError> {
    let records = tokio::task::spawn_blocking(move || db.records()).await??;
    Ok(Json(records))
}

#[derive(Debug, Deserialize)]
struct DailyParams {
    days: Option<u32>,
}

async fn daily(
    State(db): State<Db>,
    Query(params): Query<DailyParams>,
) -> Result<Json<Vec<DailyExtremes>>, AppError> {
    let days = params.days.unwrap_or(30).clamp(1, 366);
    let from_ts = unix_ts_now() - i64::from(days) * 86_400;
    let rows = tokio::task::spawn_blocking(move || db.daily_extremes(from_ts)).await??;
    Ok(Json(rows))
}

/// Minimum delta bucket width; outdoor observations only arrive every
/// 15 minutes, so finer buckets would mostly be empty on the outdoor side.
const DELTA_MIN_BUCKET_SECS: i64 = 900;

async fn delta(
    State(db): State<Db>,
    Query(params): Query<ReadingsParams>,
) -> Result<Json<Vec<DeltaPoint>>, AppError> {
    let hours = params.hours.unwrap_or(24).clamp(1, MAX_HOURS);
    let window_secs = i64::from(hours) * 3600;
    let from_ts = unix_ts_now() - window_secs;
    let bucket_secs = (window_secs / MAX_POINTS).max(DELTA_MIN_BUCKET_SECS);
    let rows =
        tokio::task::spawn_blocking(move || db.temperature_delta(from_ts, bucket_secs)).await??;
    Ok(Json(rows))
}

async fn risk(
    State(db): State<Db>,
    Query(params): Query<DailyParams>,
) -> Result<Json<Vec<DayRisk>>, AppError> {
    let days = params.days.unwrap_or(30).clamp(1, 366);
    let from_ts = unix_ts_now() - i64::from(days) * 86_400;
    let rows = tokio::task::spawn_blocking(move || db.daily_risk(from_ts)).await??;
    Ok(Json(rows))
}

/// Outdoor observations older than this are treated as missing — a stale
/// value from a dead API poller must not silence (or raise) warnings.
const OUTDOOR_STALE_SECS: i64 = 2 * 3600;

/// Combined current state for the dashboard header and warning banner.
#[derive(Debug, Serialize)]
struct Conditions {
    indoor: Option<Reading>,
    outdoor: Option<OutdoorReading>,
    status: Option<Assessment>,
}

async fn conditions(State(db): State<Db>) -> Result<Json<Conditions>, AppError> {
    let (indoor, outdoor) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        Ok((db.latest()?, db.latest_outdoor()?))
    })
    .await??;
    let outdoor = outdoor.filter(|o| unix_ts_now() - o.ts <= OUTDOOR_STALE_SECS);
    let status = indoor
        .as_ref()
        .map(|reading| weather::assess(reading, outdoor.as_ref()));
    Ok(Json(Conditions {
        indoor,
        outdoor,
        status,
    }))
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
