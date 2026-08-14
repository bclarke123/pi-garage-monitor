// Rust guideline compliant 2026-08-12
//! Dashboard HTTP server.
//!
//! Serves the single-page dashboard at `/` and two JSON endpoints:
//! `/api/latest` (most recent reading) and `/api/readings?hours=N`
//! (bucket-averaged history, capped at roughly 1000 points).

use std::convert::Infallible;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use crate::db::{DailyExtremes, DayRisk, Db, DeltaPoint, Event, OutdoorReading, Reading, Records};
use crate::system::{self, Stats};
use crate::weather::{self, Assessment};
use crate::{DataEvent, unix_ts_now};

const INDEX_HTML: &str = include_str!("index.html");

/// Longest queryable window; a year covers the full renovation timeline.
const MAX_HOURS: u32 = 24 * 366;

/// Rough cap on points returned per query; keeps chart payloads small.
const MAX_POINTS: i64 = 1000;

/// Shared state for all handlers; most only need the [`Db`] half.
#[derive(Debug, Clone)]
struct AppState {
    db: Db,
    events: broadcast::Sender<DataEvent>,
}

impl FromRef<AppState> for Db {
    fn from_ref(input: &AppState) -> Self {
        input.db.clone()
    }
}

impl FromRef<AppState> for broadcast::Sender<DataEvent> {
    fn from_ref(input: &AppState) -> Self {
        input.events.clone()
    }
}

/// Serves the dashboard on `listen` until the process exits.
///
/// # Errors
/// Returns an error if the listener cannot bind or the server fails.
pub async fn serve(db: Db, listen: SocketAddr, events: broadcast::Sender<DataEvent>) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/latest", get(latest))
        .route("/api/readings", get(readings))
        .route("/api/records", get(records))
        .route("/api/daily", get(daily))
        .route("/api/conditions", get(conditions))
        .route("/api/outdoor", get(outdoor))
        .route("/api/stream", get(stream))
        .route("/api/risk", get(risk))
        .route("/api/delta", get(delta))
        .route("/api/events", get(list_events).post(create_event))
        .route("/api/events/{id}", axum::routing::delete(delete_event))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .with_state(AppState { db, events });
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

async fn outdoor(
    State(db): State<Db>,
    Query(params): Query<ReadingsParams>,
) -> Result<Json<Vec<OutdoorReading>>, AppError> {
    let hours = params.hours.unwrap_or(24).clamp(1, MAX_HOURS);
    let window_secs = i64::from(hours) * 3600;
    let from_ts = unix_ts_now() - window_secs;
    let bucket_secs = (window_secs / MAX_POINTS).max(1);
    let rows =
        tokio::task::spawn_blocking(move || db.outdoor_since(from_ts, bucket_secs)).await??;
    Ok(Json(rows))
}

/// Server-sent events: one message per stored reading (`event: reading`) or
/// outdoor observation (`event: outdoor`), each carrying the same JSON as
/// `/api/conditions`. The dashboard applies the payload directly — no
/// follow-up request — and only re-fetches its window-dependent charts.
async fn stream(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<sse::Event, Infallible>>> {
    let db = state.db.clone();
    let stream = BroadcastStream::new(state.events.subscribe())
        .then(move |item| {
            let db = db.clone();
            async move {
                // A lagged receiver just skips ahead; every message carries
                // the full current state, so dropped ones cost nothing.
                let kind = item.ok()?;
                let built: anyhow::Result<sse::Event> = async {
                    Ok(match kind {
                        DataEvent::Reading => sse::Event::default()
                            .event("reading")
                            .json_data(&current_conditions(db).await?)?,
                        DataEvent::Outdoor => sse::Event::default()
                            .event("outdoor")
                            .json_data(&current_conditions(db).await?)?,
                        DataEvent::Events => {
                            let list = tokio::task::spawn_blocking(move || db.events()).await??;
                            sse::Event::default().event("events").json_data(&list)?
                        }
                    })
                }
                .await;
                match built {
                    Ok(event) => Some(Ok(event)),
                    Err(error) => {
                        tracing::event!(
                            name: "server.stream.failure",
                            tracing::Level::ERROR,
                            error.message = %error,
                            "failed to build SSE payload",
                        );
                        None
                    }
                }
            }
        })
        .filter_map(std::convert::identity);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn list_events(State(db): State<Db>) -> Result<Json<Vec<Event>>, AppError> {
    let events = tokio::task::spawn_blocking(move || db.events()).await??;
    Ok(Json(events))
}

#[derive(Debug, Deserialize)]
struct NewEvent {
    /// Unix seconds the event applies to; defaults to now.
    ts: Option<i64>,
    label: String,
}

async fn create_event(
    State(db): State<Db>,
    State(events): State<broadcast::Sender<DataEvent>>,
    Json(new_event): Json<NewEvent>,
) -> Result<(StatusCode, Json<Event>), AppError> {
    let label = new_event.label.trim().to_owned();
    if label.is_empty() || label.len() > 100 {
        return Err(AppError(anyhow::anyhow!(
            "event label must be 1-100 characters"
        )));
    }
    let ts = new_event.ts.unwrap_or_else(unix_ts_now);
    let event = tokio::task::spawn_blocking(move || -> anyhow::Result<Event> {
        let id = db.add_event(ts, &label)?;
        Ok(Event { id, ts, label })
    })
    .await??;
    let _ = events.send(DataEvent::Events);
    Ok((StatusCode::CREATED, Json(event)))
}

async fn delete_event(
    State(db): State<Db>,
    State(events): State<broadcast::Sender<DataEvent>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let existed = tokio::task::spawn_blocking(move || db.delete_event(id)).await??;
    if existed {
        let _ = events.send(DataEvent::Events);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
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

/// Combined current state for the dashboard header and warning banner.
#[derive(Debug, Serialize)]
struct Conditions {
    indoor: Option<Reading>,
    outdoor: Option<OutdoorReading>,
    status: Option<Assessment>,
    system: Stats,
}

/// Builds the current-state payload served by `/api/conditions` and pushed
/// over `/api/stream`.
async fn current_conditions(db: Db) -> anyhow::Result<Conditions> {
    // system::sample() reads procfs/sysfs (and may briefly sleep for its
    // first CPU baseline), so it belongs on the blocking pool too.
    let (indoor, outdoor, system) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        Ok((db.latest()?, db.latest_outdoor()?, system::sample()))
    })
    .await??;
    let outdoor = outdoor.filter(|o| unix_ts_now() - o.ts <= weather::OUTDOOR_STALE_SECS);
    let status = indoor
        .as_ref()
        .map(|reading| weather::assess(reading, outdoor.as_ref()));
    Ok(Conditions {
        indoor,
        outdoor,
        status,
        system,
    })
}

async fn conditions(State(db): State<Db>) -> Result<Json<Conditions>, AppError> {
    Ok(Json(current_conditions(db).await?))
}

/// Web-app manifest so the dashboard can be pinned to a phone home screen.
const MANIFEST: &str = r##"{
  "name": "Garage Monitor",
  "short_name": "Garage",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#0d0d0d",
  "theme_color": "#0d0d0d",
  "icons": [{ "src": "/icon.svg", "sizes": "any", "type": "image/svg+xml" }]
}"##;

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST,
    )
}

const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
<rect width="100" height="100" rx="20" fill="#1a1a19"/>
<text x="50" y="50" font-size="58" text-anchor="middle" dominant-baseline="central">🌡️</text>
</svg>"##;

async fn icon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG)
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
