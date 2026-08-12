// Rust guideline compliant 2026-08-12
//! Garage climate monitor daemon.
//!
//! Reads temperature, humidity, and pressure from a BME280 on a Raspberry
//! Pi, stores readings in `SQLite`, and serves a small web dashboard. Run
//! with `--mock` on a development machine to use a simulated sensor.

mod db;
mod sensor;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "Garage temperature/humidity monitor")]
struct Args {
    /// Path to the `SQLite` database file.
    #[arg(long, default_value = "garage.db")]
    db: PathBuf,

    /// Address the dashboard HTTP server listens on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,

    /// Seconds between sensor readings.
    #[arg(long, default_value_t = 60)]
    interval_secs: u64,

    /// Use a simulated sensor instead of the BME280 (for development).
    #[arg(long)]
    mock: bool,
}

/// Returns the current Unix time in seconds.
///
/// # Panics
/// Panics if the system clock reads before 1970 or absurdly far in the
/// future — both indicate a broken clock this daemon can't work with.
#[must_use]
pub fn unix_ts_now() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock set before 1970")
        .as_secs();
    i64::try_from(secs).expect("system clock implausibly far in the future")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    let db = db::Db::open(&args.db)?;
    let sensor = if args.mock {
        sensor::Sensor::mock()
    } else {
        sensor::Sensor::bme280()?
    };

    let sampler_db = db.clone();
    let interval = Duration::from_secs(args.interval_secs.max(1));
    std::thread::spawn(move || sensor::run_sampler(sensor, sampler_db, interval));

    web::serve(db, args.listen).await
}
