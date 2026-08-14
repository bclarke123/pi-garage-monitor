// Rust guideline compliant 2026-08-13
//! Push alerts via [ntfy](https://ntfy.sh) on risk-level transitions.
//!
//! A dedicated thread watches the same broadcast bus that feeds the
//! dashboard's SSE stream, re-assesses conditions after every stored
//! reading, and POSTs a notification when the risk level *changes* —
//! escalations immediately, improvements only after a cooldown so a
//! reading hovering at a threshold can't flood the phone.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use crate::db::Db;
use crate::weather::{self, Level};
use crate::{DataEvent, unix_ts_now};

/// Minimum time between an alert and the follow-up "improved / all clear"
/// notification. Escalations are never delayed.
const IMPROVEMENT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Watches `events` forever, pushing an ntfy notification on level changes.
///
/// Exits only if the event bus closes (i.e. the process is shutting down).
#[expect(
    clippy::needless_pass_by_value,
    reason = "the notifier thread owns its handles for the process lifetime"
)]
pub fn run_notifier(url: String, db: Db, mut events: broadcast::Receiver<DataEvent>) {
    // Assume all-clear at startup: a healthy boot stays silent, a boot into
    // bad conditions escalates on the first reading.
    let mut last_sent = Level::Ok;
    let mut last_sent_at: Option<Instant> = None;
    loop {
        match events.blocking_recv() {
            Ok(DataEvent::Reading | DataEvent::Outdoor) => {}
            // Timeline edits don't change conditions; lagging just means
            // fresher state is already waiting on the next recv.
            Ok(DataEvent::Events) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
        let assessment = match assess_current(&db) {
            Ok(Some(assessment)) => assessment,
            Ok(None) => continue,
            Err(error) => {
                tracing::event!(
                    name: "notify.assess.failure",
                    tracing::Level::WARN,
                    error.message = %error,
                    "failed to assess conditions for alerting",
                );
                continue;
            }
        };
        let elapsed = last_sent_at.map(|at| at.elapsed());
        if !should_send(assessment.level, last_sent, elapsed) {
            continue;
        }
        match send(&url, assessment.level, &assessment.message) {
            Ok(()) => {
                tracing::event!(
                    name: "notify.send.success",
                    tracing::Level::INFO,
                    notify.level = ?assessment.level,
                    "sent push notification",
                );
                last_sent = assessment.level;
                last_sent_at = Some(Instant::now());
            }
            // Leave state untouched: the next reading retries the send.
            Err(error) => tracing::event!(
                name: "notify.send.failure",
                tracing::Level::WARN,
                error.message = %error,
                "failed to send push notification; will retry",
            ),
        }
    }
}

fn assess_current(db: &Db) -> Result<Option<weather::Assessment>> {
    let Some(indoor) = db.latest()? else {
        return Ok(None);
    };
    let outdoor = db
        .latest_outdoor()?
        .filter(|o| unix_ts_now() - o.ts <= weather::OUTDOOR_STALE_SECS);
    Ok(Some(weather::assess(&indoor, outdoor.as_ref())))
}

/// Decides whether a notification should go out.
///
/// Escalations always send; improvements (including all-clear) wait out
/// [`IMPROVEMENT_COOLDOWN`] since the last send, so conditions oscillating
/// around a threshold produce at most one alert pair per cooldown window.
fn should_send(level: Level, last_sent: Level, elapsed_since_send: Option<Duration>) -> bool {
    if level > last_sent {
        return true;
    }
    level < last_sent && elapsed_since_send.is_some_and(|e| e >= IMPROVEMENT_COOLDOWN)
}

fn send(url: &str, level: Level, message: &str) -> Result<()> {
    let (title, priority, tags) = match level {
        Level::Critical => ("Garage alert", "urgent", "rotating_light"),
        Level::Warning => ("Garage caution", "high", "warning"),
        Level::Ok => ("Garage all clear", "default", "white_check_mark"),
    };
    ureq::post(url)
        .header("Title", title)
        .header("Priority", priority)
        .header("Tags", tags)
        .send(message)
        .context("posting ntfy notification")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalations_send_immediately() {
        assert!(should_send(Level::Warning, Level::Ok, None));
        assert!(should_send(
            Level::Critical,
            Level::Warning,
            Some(Duration::from_secs(1))
        ));
    }

    #[test]
    fn unchanged_level_stays_silent() {
        assert!(!should_send(Level::Ok, Level::Ok, None));
        assert!(!should_send(
            Level::Critical,
            Level::Critical,
            Some(Duration::from_secs(9999))
        ));
    }

    #[test]
    fn improvements_wait_out_the_cooldown() {
        assert!(!should_send(
            Level::Ok,
            Level::Critical,
            Some(Duration::from_secs(60))
        ));
        assert!(should_send(
            Level::Ok,
            Level::Critical,
            Some(IMPROVEMENT_COOLDOWN)
        ));
        assert!(should_send(
            Level::Warning,
            Level::Critical,
            Some(IMPROVEMENT_COOLDOWN)
        ));
    }
}
