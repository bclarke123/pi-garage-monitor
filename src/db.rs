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

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned mutex means another thread already panicked; stop too.
        self.conn.lock().expect("database mutex poisoned")
    }
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
