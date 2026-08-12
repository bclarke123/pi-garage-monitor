// Rust guideline compliant 2026-08-12
//! `SQLite` storage for sensor readings.
//!
//! Readings are appended by the sampler thread and queried by the web
//! dashboard. All access goes through [`Db`], a cheaply clonable handle
//! whose clones share one `SQLite` connection behind a mutex — plenty for
//! one writer sampling every minute and the occasional dashboard query.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

/// A single temperature/humidity/pressure sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reading {
    /// Unix timestamp in seconds when the sample was taken.
    pub ts: i64,
    /// Temperature in degrees Celsius.
    pub temperature_c: f64,
    /// Relative humidity in percent (0–100).
    pub humidity_pct: f64,
    /// Barometric pressure in hectopascals.
    pub pressure_hpa: f64,
}

/// Shared handle to the readings database.
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS readings (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    temperature_c REAL NOT NULL,
    humidity_pct  REAL NOT NULL,
    pressure_hpa  REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS readings_ts ON readings (ts);
CREATE TABLE IF NOT EXISTS outdoor (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    temperature_c REAL NOT NULL,
    humidity_pct  REAL NOT NULL,
    dew_point_c   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS outdoor_ts ON outdoor (ts);
CREATE TABLE IF NOT EXISTS events (
    id    INTEGER PRIMARY KEY,
    ts    INTEGER NOT NULL,
    label TEXT NOT NULL
);
";

impl Db {
    /// Opens (or creates) the database at `path` and applies the schema.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened or the schema fails to apply.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Opens a fresh in-memory database, for tests.
    ///
    /// # Errors
    /// Returns an error if the schema fails to apply.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Appends one reading.
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn insert(&self, reading: &Reading) -> Result<()> {
        self.lock().execute(
            "INSERT INTO readings (ts, temperature_c, humidity_pct, pressure_hpa)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                reading.ts,
                reading.temperature_c,
                reading.humidity_pct,
                reading.pressure_hpa
            ],
        )?;
        Ok(())
    }

    /// Returns the most recent reading, if any exist.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn latest(&self) -> Result<Option<Reading>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, temperature_c, humidity_pct, pressure_hpa
             FROM readings ORDER BY ts DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], row_to_reading)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Returns readings at or after `from_ts`, averaged into `bucket_secs`-wide buckets.
    ///
    /// Bucketing keeps the dashboard payload bounded no matter how large the
    /// requested window is. Each returned reading's `ts` is its bucket start.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn since(&self, from_ts: i64, bucket_secs: i64) -> Result<Vec<Reading>> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT (ts / ?2) * ?2 AS bucket,
                    AVG(temperature_c), AVG(humidity_pct), AVG(pressure_hpa)
             FROM readings WHERE ts >= ?1
             GROUP BY bucket ORDER BY bucket",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts, bucket_secs], row_to_reading)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Returns all-time record extremes, or `None` while the database is empty.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn records(&self) -> Result<Option<Records>> {
        let conn = self.lock();
        let extreme = |column: &str, order: &str| -> Result<Option<Extreme>> {
            let sql =
                format!("SELECT ts, {column} FROM readings ORDER BY {column} {order}, ts LIMIT 1");
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query_map([], |row| {
                Ok(Extreme {
                    ts: row.get(0)?,
                    value: row.get(1)?,
                })
            })?;
            rows.next().transpose().map_err(Into::into)
        };
        let Some(highest_temperature_c) = extreme("temperature_c", "DESC")? else {
            return Ok(None);
        };
        Ok(Some(Records {
            highest_temperature_c,
            lowest_temperature_c: extreme("temperature_c", "ASC")?.expect("table is non-empty"),
            highest_humidity_pct: extreme("humidity_pct", "DESC")?.expect("table is non-empty"),
            lowest_humidity_pct: extreme("humidity_pct", "ASC")?.expect("table is non-empty"),
        }))
    }

    /// Returns per-calendar-day temperature extremes at or after `from_ts`.
    ///
    /// Days are boundaries in the server's local timezone, formatted
    /// `YYYY-MM-DD`. Outdoor extremes are included for days that have
    /// weather observations.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn daily_extremes(&self, from_ts: i64) -> Result<Vec<DailyExtremes>> {
        let conn = self.lock();

        let mut outdoor: std::collections::HashMap<String, (f64, f64)> =
            std::collections::HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT date(ts, 'unixepoch', 'localtime') AS day,
                    MIN(temperature_c), MAX(temperature_c)
             FROM outdoor WHERE ts >= ?1 GROUP BY day",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?;
        for row in rows {
            let (day, min, max) = row?;
            outdoor.insert(day, (min, max));
        }

        let mut stmt = conn.prepare(
            "SELECT date(ts, 'unixepoch', 'localtime') AS day,
                    MIN(temperature_c), MAX(temperature_c)
             FROM readings WHERE ts >= ?1
             GROUP BY day ORDER BY day",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts], |row| {
            Ok(DailyExtremes {
                date: row.get(0)?,
                min_c: row.get(1)?,
                max_c: row.get(2)?,
                outdoor_min_c: None,
                outdoor_max_c: None,
            })
        })?;
        rows.map(|row| {
            let mut day = row?;
            if let Some(&(min, max)) = outdoor.get(&day.date) {
                day.outdoor_min_c = Some(min);
                day.outdoor_max_c = Some(max);
            }
            Ok(day)
        })
        .collect()
    }

    /// Records a renovation/timeline event, returning its id.
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn add_event(&self, ts: i64, label: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (ts, label) VALUES (?1, ?2)",
            rusqlite::params![ts, label],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Returns all recorded events, oldest first.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn events(&self) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, ts, label FROM events ORDER BY ts")?;
        let rows = stmt.query_map([], |row| {
            Ok(Event {
                id: row.get(0)?,
                ts: row.get(1)?,
                label: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Deletes an event by id, returning whether it existed.
    ///
    /// # Errors
    /// Returns an error if the delete fails.
    pub fn delete_event(&self, id: i64) -> Result<bool> {
        let deleted = self
            .lock()
            .execute("DELETE FROM events WHERE id = ?1", rusqlite::params![id])?;
        Ok(deleted > 0)
    }

    /// Appends one outdoor weather observation.
    ///
    /// # Errors
    /// Returns an error if the insert fails.
    pub fn insert_outdoor(&self, outdoor: &OutdoorReading) -> Result<()> {
        self.lock().execute(
            "INSERT INTO outdoor (ts, temperature_c, humidity_pct, dew_point_c)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                outdoor.ts,
                outdoor.temperature_c,
                outdoor.humidity_pct,
                outdoor.dew_point_c
            ],
        )?;
        Ok(())
    }

    /// Returns the most recent outdoor observation, if any exist.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn latest_outdoor(&self) -> Result<Option<OutdoorReading>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, temperature_c, humidity_pct, dew_point_c
             FROM outdoor ORDER BY ts DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], |row| {
            Ok(OutdoorReading {
                ts: row.get(0)?,
                temperature_c: row.get(1)?,
                humidity_pct: row.get(2)?,
                dew_point_c: row.get(3)?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Returns indoor vs outdoor temperature joined into common time buckets.
    ///
    /// Buckets with no outdoor observation are omitted, so the result is
    /// empty until weather polling is enabled.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn temperature_delta(&self, from_ts: i64, bucket_secs: i64) -> Result<Vec<DeltaPoint>> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT i.bucket, i.t, o.t FROM
               (SELECT (ts / ?2) * ?2 AS bucket, AVG(temperature_c) AS t
                  FROM readings WHERE ts >= ?1 GROUP BY bucket) i
               JOIN
               (SELECT (ts / ?2) * ?2 AS bucket, AVG(temperature_c) AS t
                  FROM outdoor WHERE ts >= ?1 GROUP BY bucket) o
               USING (bucket)
             ORDER BY i.bucket",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts, bucket_secs], |row| {
            let indoor_c: f64 = row.get(1)?;
            let outdoor_c: f64 = row.get(2)?;
            Ok(DeltaPoint {
                ts: row.get(0)?,
                indoor_c,
                outdoor_c,
                delta_c: indoor_c - outdoor_c,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Summarizes condensation risk per local calendar day at or after `from_ts`.
    ///
    /// Streams the raw readings and, for each day, tallies how long indoor
    /// air spent saturated (dew-point spread < 1 °C) or near saturation
    /// (< 3 °C), plus the day's worst spread, humidity peak, temperature low,
    /// and the day's highest outdoor dew point when weather data exists.
    /// Minutes are counted as distinct sample minutes, so the tallies stay
    /// honest at any sample interval.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    pub fn daily_risk(&self, from_ts: i64) -> Result<Vec<DayRisk>> {
        // Spread bands mirror the live assessment in `weather::assess`.
        const SPREAD_SATURATED_C: f64 = 1.0;
        const SPREAD_NEAR_C: f64 = 3.0;

        struct Acc {
            saturated_minutes: u32,
            near_minutes: u32,
            last_saturated_minute: i64,
            last_near_minute: i64,
            min_spread_c: f64,
            max_humidity_pct: f64,
            min_temperature_c: f64,
        }

        let conn = self.lock();
        let mut days: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT date(ts, 'unixepoch', 'localtime'), ts, temperature_c, humidity_pct
             FROM readings WHERE ts >= ?1 ORDER BY ts",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?;
        for row in rows {
            let (day, ts, temperature_c, humidity_pct) = row?;
            let spread = temperature_c - crate::weather::dew_point_c(temperature_c, humidity_pct);
            let minute = ts.div_euclid(60);
            let acc = days.entry(day).or_insert(Acc {
                saturated_minutes: 0,
                near_minutes: 0,
                last_saturated_minute: i64::MIN,
                last_near_minute: i64::MIN,
                min_spread_c: f64::INFINITY,
                max_humidity_pct: f64::NEG_INFINITY,
                min_temperature_c: f64::INFINITY,
            });
            if spread < SPREAD_SATURATED_C && minute != acc.last_saturated_minute {
                acc.saturated_minutes += 1;
                acc.last_saturated_minute = minute;
            }
            if spread < SPREAD_NEAR_C && minute != acc.last_near_minute {
                acc.near_minutes += 1;
                acc.last_near_minute = minute;
            }
            acc.min_spread_c = acc.min_spread_c.min(spread);
            acc.max_humidity_pct = acc.max_humidity_pct.max(humidity_pct);
            acc.min_temperature_c = acc.min_temperature_c.min(temperature_c);
        }

        let mut outdoor_dew: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT date(ts, 'unixepoch', 'localtime') AS day, MAX(dew_point_c)
             FROM outdoor WHERE ts >= ?1 GROUP BY day",
        )?;
        let rows = stmt.query_map(rusqlite::params![from_ts], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (day, dew) = row?;
            outdoor_dew.insert(day, dew);
        }

        Ok(days
            .into_iter()
            .map(|(date, acc)| {
                let max_outdoor_dew_point_c = outdoor_dew.get(&date).copied();
                DayRisk {
                    level: crate::weather::assess_day(
                        acc.saturated_minutes,
                        acc.near_minutes,
                        acc.min_temperature_c,
                        max_outdoor_dew_point_c,
                    ),
                    date,
                    saturated_minutes: acc.saturated_minutes,
                    near_saturation_minutes: acc.near_minutes,
                    min_spread_c: acc.min_spread_c,
                    max_humidity_pct: acc.max_humidity_pct,
                    min_temperature_c: acc.min_temperature_c,
                    max_outdoor_dew_point_c,
                }
            })
            .collect())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned mutex means another thread already panicked; stop too.
        self.conn.lock().expect("database mutex poisoned")
    }
}

/// Indoor and outdoor temperature averaged over one shared time bucket.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DeltaPoint {
    /// Bucket start as a Unix timestamp in seconds.
    pub ts: i64,
    /// Average indoor temperature over the bucket, in °C.
    pub indoor_c: f64,
    /// Average outdoor temperature over the bucket, in °C.
    pub outdoor_c: f64,
    /// Indoor minus outdoor, in °C (positive = warmer inside).
    pub delta_c: f64,
}

/// One local-calendar-day's retroactive condensation-risk summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayRisk {
    /// Local date as `YYYY-MM-DD`.
    pub date: String,
    /// Worst condensation severity reached that day.
    pub level: crate::weather::Level,
    /// Distinct sample minutes with dew-point spread below 1 °C.
    pub saturated_minutes: u32,
    /// Distinct sample minutes with dew-point spread below 3 °C.
    pub near_saturation_minutes: u32,
    /// Smallest dew-point spread of the day, in °C.
    pub min_spread_c: f64,
    /// Highest indoor relative humidity of the day, in percent.
    pub max_humidity_pct: f64,
    /// Coldest indoor reading of the day, in °C (proxy for surface temperature).
    pub min_temperature_c: f64,
    /// Highest outdoor dew point of the day, in °C, if weather data exists.
    pub max_outdoor_dew_point_c: Option<f64>,
}

/// One outdoor weather observation from the weather API.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OutdoorReading {
    /// Unix timestamp in seconds when the observation was fetched.
    pub ts: i64,
    /// Outdoor temperature in degrees Celsius.
    pub temperature_c: f64,
    /// Outdoor relative humidity in percent (0–100).
    pub humidity_pct: f64,
    /// Outdoor dew point in degrees Celsius.
    pub dew_point_c: f64,
}

/// A record value and when it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Extreme {
    /// Unix timestamp in seconds of the observation.
    pub ts: i64,
    /// The observed value.
    pub value: f64,
}

/// All-time record extremes across the whole database.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Records {
    /// Hottest reading ever taken.
    pub highest_temperature_c: Extreme,
    /// Coldest reading ever taken.
    pub lowest_temperature_c: Extreme,
    /// Most humid reading ever taken.
    pub highest_humidity_pct: Extreme,
    /// Driest reading ever taken.
    pub lowest_humidity_pct: Extreme,
}

/// One local-calendar-day's temperature extremes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyExtremes {
    /// Local date as `YYYY-MM-DD`.
    pub date: String,
    /// Coldest reading of the day, in degrees Celsius.
    pub min_c: f64,
    /// Hottest reading of the day, in degrees Celsius.
    pub max_c: f64,
    /// Coldest outdoor observation of the day, if weather data exists.
    pub outdoor_min_c: Option<f64>,
    /// Hottest outdoor observation of the day, if weather data exists.
    pub outdoor_max_c: Option<f64>,
}

/// A labeled point on the renovation timeline (e.g. "roof replaced").
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Database id, used to delete the event.
    pub id: i64,
    /// Unix timestamp in seconds the event applies to.
    pub ts: i64,
    /// Short human-readable description.
    pub label: String,
}

fn row_to_reading(row: &rusqlite::Row<'_>) -> rusqlite::Result<Reading> {
    Ok(Reading {
        ts: row.get(0)?,
        temperature_c: row.get(1)?,
        humidity_pct: row.get(2)?,
        pressure_hpa: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(ts: i64, temperature_c: f64) -> Reading {
        Reading {
            ts,
            temperature_c,
            humidity_pct: 55.0,
            pressure_hpa: 1013.2,
        }
    }

    #[test]
    fn latest_returns_none_on_empty_db() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.latest().unwrap(), None);
    }

    #[test]
    fn latest_returns_most_recent_by_timestamp() {
        let db = Db::open_in_memory().unwrap();
        db.insert(&reading(200, 21.0)).unwrap();
        db.insert(&reading(100, 19.0)).unwrap();
        let latest = db.latest().unwrap().unwrap();
        assert_eq!(latest.ts, 200);
        assert!((latest.temperature_c - 21.0).abs() < f64::EPSILON);
    }

    #[test]
    fn records_returns_none_on_empty_db() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.records().unwrap(), None);
    }

    #[test]
    fn records_tracks_extremes_with_timestamps() {
        let db = Db::open_in_memory().unwrap();
        db.insert(&reading(100, 19.0)).unwrap();
        db.insert(&reading(200, 32.0)).unwrap();
        db.insert(&reading(300, -22.0)).unwrap();
        let records = db.records().unwrap().unwrap();
        assert_eq!(records.highest_temperature_c.ts, 200);
        assert!((records.highest_temperature_c.value - 32.0).abs() < f64::EPSILON);
        assert_eq!(records.lowest_temperature_c.ts, 300);
        assert!((records.lowest_temperature_c.value + 22.0).abs() < f64::EPSILON);
    }

    #[test]
    fn daily_extremes_groups_by_local_day() {
        const DAY: i64 = 86_400;
        let db = Db::open_in_memory().unwrap();
        // Two readings well inside one local day, far from midnight in any
        // timezone offset: use noon UTC on consecutive days.
        db.insert(&reading(DAY / 2, 5.0)).unwrap();
        db.insert(&reading(DAY / 2 + 60, 15.0)).unwrap();
        db.insert(&reading(DAY + DAY / 2, 25.0)).unwrap();
        let days = db.daily_extremes(0).unwrap();
        assert_eq!(days.len(), 2);
        assert!((days[0].min_c - 5.0).abs() < f64::EPSILON);
        assert!((days[0].max_c - 15.0).abs() < f64::EPSILON);
        assert!((days[1].min_c - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn events_roundtrip_and_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add_event(1000, "roof replaced").unwrap();
        let events = db.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "roof replaced");
        assert!(db.delete_event(id).unwrap());
        assert!(!db.delete_event(id).unwrap());
        assert!(db.events().unwrap().is_empty());
    }

    #[test]
    fn daily_extremes_includes_outdoor_when_present() {
        const DAY: i64 = 86_400;
        let db = Db::open_in_memory().unwrap();
        db.insert(&reading(DAY / 2, 15.0)).unwrap();
        db.insert_outdoor(&OutdoorReading {
            ts: DAY / 2,
            temperature_c: 25.0,
            humidity_pct: 50.0,
            dew_point_c: 10.0,
        })
        .unwrap();
        db.insert(&reading(DAY + DAY / 2, 16.0)).unwrap(); // no outdoor this day
        let days = db.daily_extremes(0).unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].outdoor_max_c, Some(25.0));
        assert_eq!(days[1].outdoor_max_c, None);
    }

    #[test]
    fn temperature_delta_joins_matching_buckets_only() {
        let db = Db::open_in_memory().unwrap();
        db.insert(&reading(100, 15.0)).unwrap();
        db.insert(&reading(200, 17.0)).unwrap();
        // Outdoor data exists for the first 900s bucket only.
        db.insert_outdoor(&OutdoorReading {
            ts: 150,
            temperature_c: 10.0,
            humidity_pct: 70.0,
            dew_point_c: 4.0,
        })
        .unwrap();
        db.insert(&reading(2000, 20.0)).unwrap(); // second bucket, no outdoor

        let points = db.temperature_delta(0, 900).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].ts, 0);
        assert!((points[0].indoor_c - 16.0).abs() < f64::EPSILON);
        assert!((points[0].delta_c - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn daily_risk_flags_saturated_and_incoming_air_days() {
        const DAY: i64 = 86_400;
        let db = Db::open_in_memory().unwrap();
        // Day 1 (noon UTC): six minutes of saturated air → critical.
        for i in 0..6 {
            db.insert(&Reading {
                ts: DAY / 2 + i * 60,
                temperature_c: 12.0,
                humidity_pct: 99.0,
                pressure_hpa: 1013.0,
            })
            .unwrap();
        }
        // Day 2: dry indoors (min temp 10 °C) but outdoor dew point 15 °C → warning.
        db.insert(&Reading {
            ts: DAY + DAY / 2,
            temperature_c: 10.0,
            humidity_pct: 50.0,
            pressure_hpa: 1013.0,
        })
        .unwrap();
        db.insert_outdoor(&OutdoorReading {
            ts: DAY + DAY / 2,
            temperature_c: 22.0,
            humidity_pct: 80.0,
            dew_point_c: 15.0,
        })
        .unwrap();

        let days = db.daily_risk(0).unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].level, crate::weather::Level::Critical);
        assert_eq!(days[0].saturated_minutes, 6);
        assert_eq!(days[1].level, crate::weather::Level::Warning);
        assert!((days[1].max_outdoor_dew_point_c.unwrap() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn since_filters_and_buckets() {
        let db = Db::open_in_memory().unwrap();
        // Two readings in the same 60s bucket, one older than the window.
        db.insert(&reading(30, 10.0)).unwrap();
        db.insert(&reading(120, 20.0)).unwrap();
        db.insert(&reading(150, 30.0)).unwrap();
        let rows = db.since(100, 60).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 120);
        assert!((rows[0].temperature_c - 25.0).abs() < f64::EPSILON);
    }
}
