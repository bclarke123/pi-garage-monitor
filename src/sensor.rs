// Rust guideline compliant 2026-08-12
//! Sensor sources and the sampling loop.
//!
//! [`Sensor`] is either a real BME280 on the Pi's I²C bus (Linux only) or a
//! simulated source for developing off-device, following the mockable-I/O
//! enum pattern. [`run_sampler`] drives whichever variant it is given on a
//! fixed interval and appends readings to the database.

use std::time::Duration;

use anyhow::Result;

use crate::db::{Db, Reading};
use crate::unix_ts_now;

/// A source of temperature/humidity/pressure readings.
#[derive(Debug)]
pub enum Sensor {
    /// A Bosch BME280 on I²C bus 1, address `0x76`.
    #[cfg(target_os = "linux")]
    Bme280(linux::Bme280),
    /// A simulated sensor producing plausible day-cycle values.
    Mock,
}

impl Sensor {
    /// Opens the BME280 on the Pi's I²C bus 1 at the primary address `0x76`.
    ///
    /// # Errors
    /// Returns an error if the I²C bus cannot be opened (is I²C enabled in
    /// `raspi-config`?) or the sensor does not respond.
    #[cfg(target_os = "linux")]
    pub fn bme280() -> Result<Self> {
        Ok(Self::Bme280(linux::Bme280::open()?))
    }

    /// Stub for non-Linux hosts, where no I²C hardware is available.
    ///
    /// # Errors
    /// Always fails; run with `--mock` instead.
    #[cfg(not(target_os = "linux"))]
    pub fn bme280() -> Result<Self> {
        anyhow::bail!("the BME280 is only supported on Linux; run with --mock for development")
    }

    /// Creates a simulated sensor.
    pub fn mock() -> Self {
        Self::Mock
    }

    /// Takes one reading, stamped with the current time.
    ///
    /// # Errors
    /// Returns an error if the hardware read fails (mock reads are infallible).
    #[cfg_attr(
        not(target_os = "linux"),
        expect(
            clippy::unnecessary_wraps,
            reason = "fallible on Linux, where the hardware variant exists"
        )
    )]
    pub fn read(&mut self) -> Result<Reading> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Bme280(device) => device.read(),
            Self::Mock => Ok(mock_reading(unix_ts_now())),
        }
    }
}

/// Simulates a garage day-cycle: readings follow a 24h sine wave so charts
/// have realistic shape during development.
fn mock_reading(now: i64) -> Reading {
    const SECS_PER_DAY: i64 = 86_400;
    let secs_into_day =
        u32::try_from(now.rem_euclid(SECS_PER_DAY)).expect("rem_euclid bounds the value");
    let phase = f64::from(secs_into_day) / 86_400.0 * std::f64::consts::TAU;
    Reading {
        ts: now,
        temperature_c: 18.0 + 6.0 * phase.sin(),
        humidity_pct: 55.0 - 12.0 * phase.sin(),
        pressure_hpa: 1013.0 + 3.0 * (2.0 * phase).sin(),
    }
}

/// Samples `sensor` every `interval` forever, appending readings to `db`.
///
/// Read failures are logged and skipped rather than aborting the loop, since
/// transient I²C errors are expected over a long deployment.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the sampler thread owns its Db handle for the process lifetime"
)]
pub fn run_sampler(mut sensor: Sensor, db: Db, interval: Duration) {
    loop {
        match sensor.read() {
            Ok(reading) => match db.insert(&reading) {
                Ok(()) => tracing::event!(
                    name: "sensor.read.success",
                    tracing::Level::INFO,
                    reading.ts = reading.ts,
                    reading.temperature_c = reading.temperature_c,
                    reading.humidity_pct = reading.humidity_pct,
                    reading.pressure_hpa = reading.pressure_hpa,
                    "stored reading",
                ),
                Err(error) => tracing::event!(
                    name: "db.insert.failure",
                    tracing::Level::ERROR,
                    error.message = %error,
                    "failed to store reading",
                ),
            },
            Err(error) => tracing::event!(
                name: "sensor.read.failure",
                tracing::Level::WARN,
                error.message = %error,
                "sensor read failed; will retry next interval",
            ),
        }
        std::thread::sleep(interval);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    //! Linux-only BME280 hardware access via rppal's embedded-hal impls.

    use anyhow::{Context, Result};
    use bme280::i2c::BME280;
    use rppal::hal::Delay;
    use rppal::i2c::I2c;

    use crate::db::Reading;
    use crate::unix_ts_now;

    /// An initialized BME280 device on I²C bus 1.
    pub struct Bme280 {
        device: BME280<I2c>,
        delay: Delay,
    }

    impl std::fmt::Debug for Bme280 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Bme280").finish_non_exhaustive()
        }
    }

    impl Bme280 {
        /// Opens and initializes the sensor at the primary address `0x76`.
        ///
        /// # Errors
        /// Returns an error if the bus cannot be opened or init fails.
        pub fn open() -> Result<Self> {
            let i2c = I2c::new()
                .context("opening I2C bus 1 (enable it with: sudo raspi-config nonint do_i2c 0)")?;
            let mut device = BME280::new_primary(i2c);
            let mut delay = Delay::new();
            device
                .init(&mut delay)
                .map_err(|e| anyhow::anyhow!("initializing BME280: {e:?}"))?;
            Ok(Self { device, delay })
        }

        /// Takes one measurement, stamped with the current time.
        ///
        /// # Errors
        /// Returns an error if the I²C transaction fails.
        pub fn read(&mut self) -> Result<Reading> {
            let m = self
                .device
                .measure(&mut self.delay)
                .map_err(|e| anyhow::anyhow!("reading BME280: {e:?}"))?;
            Ok(Reading {
                ts: unix_ts_now(),
                temperature_c: f64::from(m.temperature),
                humidity_pct: f64::from(m.humidity),
                // The sensor reports pascals; hectopascals are the
                // conventional unit for barometric pressure.
                pressure_hpa: f64::from(m.pressure) / 100.0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_readings_stay_in_plausible_ranges() {
        for hour in 0..24 {
            let r = mock_reading(hour * 3600);
            assert_eq!(r.ts, hour * 3600);
            assert!((10.0..=26.0).contains(&r.temperature_c));
            assert!((40.0..=70.0).contains(&r.humidity_pct));
            assert!((1005.0..=1020.0).contains(&r.pressure_hpa));
        }
    }

    #[test]
    fn mock_sensor_reads_successfully() {
        let mut sensor = Sensor::mock();
        let r = sensor.read().unwrap();
        assert!(r.ts > 0);
    }
}
