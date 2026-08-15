// Rust guideline compliant 2026-08-12
//! Outdoor weather polling and condensation-risk assessment.
//!
//! [`run_poller`] fetches current outdoor conditions (including dew point)
//! from the free, keyless [Open-Meteo](https://open-meteo.com/) API and
//! stores them alongside the indoor readings. [`assess`] combines indoor
//! and outdoor state into a dashboard warning: condensation — warm humid
//! air meeting cold surfaces — is what actually destroys electronics in a
//! semi-conditioned space, and frost is what kills the houseplants.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::db::{Db, OutdoorReading, Reading};
use crate::unix_ts_now;

/// How often to poll Open-Meteo.
///
/// The API's model output only refreshes every ~15 minutes, so polling
/// faster wastes their goodwill (it is free and keyless) for no new data.
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Outdoor observations older than this are treated as missing — a stale
/// value from a dead API poller must not silence (or raise) warnings.
pub const OUTDOOR_STALE_SECS: i64 = 2 * 3600;

/// Location to fetch outdoor weather for.
#[derive(Debug, Clone, Copy)]
pub struct Coordinates {
    /// Degrees north of the equator (negative for south).
    pub latitude: f64,
    /// Degrees east of Greenwich (negative for west).
    pub longitude: f64,
}

/// Polls Open-Meteo every [`POLL_INTERVAL`] forever, storing results in `db`.
///
/// Fetch failures are logged and skipped; the garage's own sensor keeps
/// working regardless of internet weather (pun intended).
#[expect(
    clippy::needless_pass_by_value,
    reason = "the poller thread owns its Db handle for the process lifetime"
)]
pub fn run_poller(
    coordinates: Coordinates,
    db: Db,
    events: tokio::sync::broadcast::Sender<crate::DataEvent>,
) {
    loop {
        match fetch_current(coordinates) {
            Ok(outdoor) => match db.insert_outdoor(&outdoor) {
                Ok(()) => {
                    // No receivers (nobody watching the dashboard) is fine.
                    let _ = events.send(crate::DataEvent::Outdoor);
                    // DEBUG for the same reason as sensor.read.success:
                    // routine success is journal spam at INFO.
                    tracing::event!(
                        name: "weather.fetch.success",
                        tracing::Level::DEBUG,
                        outdoor.temperature_c = outdoor.temperature_c,
                        outdoor.dew_point_c = outdoor.dew_point_c,
                        "stored outdoor reading",
                    );
                }
                Err(error) => tracing::event!(
                    name: "weather.store.failure",
                    tracing::Level::ERROR,
                    error.message = %error,
                    "failed to store outdoor reading",
                ),
            },
            Err(error) => tracing::event!(
                name: "weather.fetch.failure",
                tracing::Level::WARN,
                error.message = %error,
                "weather fetch failed; will retry next interval",
            ),
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    current: ApiCurrent,
}

#[derive(Debug, Deserialize)]
struct ApiCurrent {
    #[serde(rename = "temperature_2m")]
    temperature: f64,
    #[serde(rename = "relative_humidity_2m")]
    relative_humidity: f64,
    #[serde(rename = "dew_point_2m")]
    dew_point: f64,
    surface_pressure: f64,
}

fn fetch_current(coordinates: Coordinates) -> Result<OutdoorReading> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,relative_humidity_2m,dew_point_2m,surface_pressure",
        coordinates.latitude, coordinates.longitude
    );
    let mut response = ureq::get(&url).call().context("requesting Open-Meteo")?;
    let body: ApiResponse = response
        .body_mut()
        .read_json()
        .context("parsing Open-Meteo response")?;
    Ok(OutdoorReading {
        ts: unix_ts_now(),
        temperature_c: body.current.temperature,
        humidity_pct: body.current.relative_humidity,
        dew_point_c: body.current.dew_point,
        pressure_hpa: Some(body.current.surface_pressure),
    })
}

/// Computes the dew point in °C from temperature and relative humidity.
///
/// Uses the Magnus approximation (accurate to ~0.1 °C over -45..60 °C).
#[must_use]
pub fn dew_point_c(temperature_c: f64, humidity_pct: f64) -> f64 {
    // Magnus coefficients (Sonntag 1990).
    const A: f64 = 17.62;
    const B: f64 = 243.12;
    let humidity = (humidity_pct / 100.0).clamp(0.001, 1.0);
    let gamma = humidity.ln() + A * temperature_c / (B + temperature_c);
    B * gamma / (A - gamma)
}

/// Severity of a risk assessment; declaration order is severity order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Nothing to worry about.
    Ok,
    /// Conditions are drifting toward damage.
    Warning,
    /// Damaging conditions are likely happening now.
    Critical,
}

/// A risk verdict for the dashboard banner.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    /// Severity level.
    pub level: Level,
    /// Human-readable explanation.
    pub message: String,
    /// Indoor dew point in °C, derived from the latest indoor reading.
    pub indoor_dew_point_c: f64,
    /// How far indoor air is from saturating, in °C.
    pub dew_point_spread_c: f64,
}

/// Assesses condensation and frost risk from indoor conditions and, when
/// available, outdoor conditions.
///
/// The failure modes it watches for:
/// - indoor air already near saturation (spread between temperature and
///   dew point closing to zero — moisture will film onto every surface);
/// - outdoor dew point above indoor temperature (opening the door lets air
///   in that will condense onto still-cold contents);
/// - indoor temperature at or approaching freezing (frost kills houseplants
///   and anything that holds water long before it bothers electronics).
///
/// The worst level wins; a doubly bad day reports both problems.
#[must_use]
pub fn assess(indoor: &Reading, outdoor: Option<&OutdoorReading>) -> Assessment {
    // Spreads below these thresholds are the standard "condensation watch"
    // bands used in HVAC practice; 1 °C is effectively saturated once
    // sensor error (±0.5 °C) is accounted for.
    const SPREAD_CRITICAL_C: f64 = 1.0;
    const SPREAD_WARNING_C: f64 = 3.0;
    // Frost bands: air at 0.5 °C means colder surfaces (floor, window
    // glass) are already below freezing; 3 °C is the "move the plants" band.
    const FROST_CRITICAL_C: f64 = 0.5;
    const FROST_WARNING_C: f64 = 3.0;

    let indoor_dew_point_c = dew_point_c(indoor.temperature_c, indoor.humidity_pct);
    let dew_point_spread_c = indoor.temperature_c - indoor_dew_point_c;

    let condensation = if dew_point_spread_c < SPREAD_CRITICAL_C {
        (
            Level::Critical,
            "Indoor air is saturated — condensation is likely forming on surfaces right now."
                .to_owned(),
        )
    } else if dew_point_spread_c < SPREAD_WARNING_C {
        (
            Level::Warning,
            format!(
                "Indoor air is near saturation (only {dew_point_spread_c:.1} °C above the dew \
                 point) — condensation risk on cold surfaces."
            ),
        )
    } else if let Some(outdoor) = outdoor.filter(|o| o.dew_point_c > indoor.temperature_c) {
        (
            Level::Warning,
            // "at or above": the comparison is strictly >, but at one
            // decimal of display the two values can render as equal.
            format!(
                "Outdoor dew point ({:.1} °C) is at or above the indoor temperature ({:.1} °C) — \
                 incoming air will condense on anything in here. Keep the space closed up.",
                outdoor.dew_point_c, indoor.temperature_c
            ),
        )
    } else {
        (Level::Ok, String::new())
    };

    let frost = if indoor.temperature_c <= FROST_CRITICAL_C {
        (
            Level::Critical,
            format!(
                "Freezing in here ({:.1} °C) — frost damages houseplants and anything that \
                 holds water.",
                indoor.temperature_c
            ),
        )
    } else if indoor.temperature_c <= FROST_WARNING_C {
        (
            Level::Warning,
            format!(
                "Near freezing ({:.1} °C) — cold spots at the floor can frost before the air \
                 does. Move houseplants somewhere warmer.",
                indoor.temperature_c
            ),
        )
    } else {
        (Level::Ok, String::new())
    };

    let level = condensation.0.max(frost.0);
    let message = if level == Level::Ok {
        "No condensation or frost risk.".to_owned()
    } else {
        let mut issues = [condensation, frost];
        issues.sort_by_key(|(issue_level, _)| std::cmp::Reverse(*issue_level)); // most severe first
        let texts: Vec<String> = issues
            .into_iter()
            .filter(|(issue_level, _)| *issue_level != Level::Ok)
            .map(|(_, text)| text)
            .collect();
        texts.join(" ")
    };

    Assessment {
        level,
        message,
        indoor_dew_point_c,
        dew_point_spread_c,
    }
}

/// Classifies one past day's risk from its aggregates.
///
/// `incoming_air_minutes` counts sample minutes where the *concurrent*
/// outdoor dew point exceeded the indoor temperature — the same moment-by-
/// moment comparison the live banner makes. If the space lags outdoor
/// conditions, that lag is already present in the sensor's own readings,
/// so no day-extreme cross-combination is needed (or wanted: comparing a
/// morning low against an afternoon dew peak flags crossings that never
/// physically happened).
#[must_use]
pub fn assess_day(
    saturated_minutes: u32,
    near_saturation_minutes: u32,
    incoming_air_minutes: u32,
    min_temperature_c: f64,
) -> Level {
    // A few saturated minutes can be sensor noise around a real spread of
    // ~1 °C; a quarter hour near saturation (or of incoming-air exposure)
    // is a genuine damp spell.
    const SATURATED_CRITICAL_MINUTES: u32 = 5;
    const NEAR_SATURATION_WARNING_MINUTES: u32 = 15;
    const INCOMING_AIR_WARNING_MINUTES: u32 = 15;
    // Retrospective frost only flags an actual sub-zero dip, not the wider
    // "approaching freezing" band the live banner warns about.
    const FROST_C: f64 = 0.0;

    if saturated_minutes >= SATURATED_CRITICAL_MINUTES {
        Level::Critical
    } else if near_saturation_minutes >= NEAR_SATURATION_WARNING_MINUTES
        || incoming_air_minutes >= INCOMING_AIR_WARNING_MINUTES
        || min_temperature_c <= FROST_C
    {
        Level::Warning
    } else {
        Level::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indoor(temperature_c: f64, humidity_pct: f64) -> Reading {
        Reading {
            ts: 0,
            temperature_c,
            humidity_pct,
            pressure_hpa: 1013.0,
        }
    }

    fn outdoor(dew_point_c: f64) -> OutdoorReading {
        OutdoorReading {
            ts: 0,
            temperature_c: 20.0,
            humidity_pct: 80.0,
            dew_point_c,
            pressure_hpa: Some(1010.0),
        }
    }

    #[test]
    fn dew_point_matches_reference_values() {
        // Reference: 20 °C at 50% RH → dew point ≈ 9.3 °C.
        assert!((dew_point_c(20.0, 50.0) - 9.3).abs() < 0.1);
        // Saturated air: dew point equals temperature.
        assert!((dew_point_c(15.0, 100.0) - 15.0).abs() < 0.01);
    }

    #[test]
    fn saturated_indoor_air_is_critical() {
        let a = assess(&indoor(12.0, 99.0), None);
        assert_eq!(a.level, Level::Critical);
    }

    #[test]
    fn near_saturated_indoor_air_is_warning() {
        let a = assess(&indoor(12.0, 88.0), None);
        assert_eq!(a.level, Level::Warning);
        assert!(a.message.contains("near saturation"));
    }

    #[test]
    fn humid_outdoor_air_above_indoor_temperature_is_warning() {
        let a = assess(&indoor(12.0, 50.0), Some(&outdoor(15.0)));
        assert_eq!(a.level, Level::Warning);
        assert!(a.message.contains("Outdoor dew point"));
    }

    #[test]
    fn day_with_sustained_saturation_is_critical() {
        assert_eq!(assess_day(10, 60, 0, 5.0), Level::Critical);
    }

    #[test]
    fn day_with_damp_spell_or_incoming_air_exposure_is_warning() {
        assert_eq!(assess_day(0, 30, 0, 5.0), Level::Warning);
        assert_eq!(assess_day(0, 0, 20, 10.0), Level::Warning);
    }

    #[test]
    fn brief_blips_and_dry_days_are_ok() {
        assert_eq!(assess_day(2, 10, 5, 10.0), Level::Ok);
    }

    #[test]
    fn freezing_indoor_air_is_critical() {
        let a = assess(&indoor(-4.0, 50.0), None);
        assert_eq!(a.level, Level::Critical);
        assert!(a.message.contains("Freezing"));
    }

    #[test]
    fn near_freezing_indoor_air_is_warning() {
        let a = assess(&indoor(2.0, 40.0), None);
        assert_eq!(a.level, Level::Warning);
        assert!(a.message.contains("Near freezing"));
    }

    #[test]
    fn cold_and_saturated_reports_both_problems() {
        let a = assess(&indoor(-1.0, 99.0), None);
        assert_eq!(a.level, Level::Critical);
        assert!(a.message.contains("saturated"));
        assert!(a.message.contains("Freezing"));
    }

    #[test]
    fn day_dipping_below_freezing_is_warning() {
        assert_eq!(assess_day(0, 0, 0, -2.0), Level::Warning);
        assert_eq!(assess_day(0, 0, 0, 1.5), Level::Ok);
    }

    #[test]
    fn dry_conditions_are_ok() {
        let a = assess(&indoor(20.0, 50.0), Some(&outdoor(5.0)));
        assert_eq!(a.level, Level::Ok);
        assert!(a.dew_point_spread_c > 3.0);
    }
}
