// Rust guideline compliant 2026-08-13
//! Host health stats (`SoC` temperature, CPU, memory, throttling) for the
//! dashboard header.
//!
//! Everything here is best-effort: each field degrades to `None` instead of
//! erroring, because host introspection must never take down the dashboard
//! that reports on it. The sources are Linux procfs/sysfs paths; on other
//! platforms (the `--mock` dev loop on a Mac) the files simply don't exist
//! and every field is `None`.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

/// Snapshot of host health for the dashboard header.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    /// `SoC` temperature in °C — a sanity cross-check for the BME280 and an
    /// early warning if the Pi itself is overheating in a summer garage.
    pub cpu_temp_c: Option<f64>,
    /// CPU usage since the previous snapshot, 0–100 across all cores.
    pub cpu_usage_pct: Option<f64>,
    /// Physical memory in use (total minus reclaimable), percent.
    pub memory_used_pct: Option<f64>,
    /// Swap in use, percent; `None` when no swap is configured.
    pub swap_used_pct: Option<f64>,
    /// Live firmware throttling flags (Raspberry Pi specific).
    pub throttle: Option<Throttle>,
}

/// The "happening right now" bits of the Pi firmware's `get_throttled`
/// value (the low nibble; the high bits are "has happened since boot").
#[derive(Debug, Clone, Copy, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the firmware's four independent status bits"
)]
pub struct Throttle {
    /// Supply voltage is below 4.63 V right now.
    pub under_voltage: bool,
    /// ARM frequency is capped right now.
    pub frequency_capped: bool,
    /// The `SoC` is actively throttled right now.
    pub throttled: bool,
    /// The soft temperature limit is active right now.
    pub soft_temp_limit: bool,
}

/// Takes a best-effort snapshot of host health.
#[must_use]
pub fn sample() -> Stats {
    let (memory_used_pct, swap_used_pct) =
        std::fs::read_to_string("/proc/meminfo").map_or((None, None), |raw| parse_meminfo(&raw));
    Stats {
        cpu_temp_c: cpu_temp_c(),
        cpu_usage_pct: cpu_usage_pct(),
        memory_used_pct,
        swap_used_pct,
        throttle: throttle(),
    }
}

fn cpu_temp_c() -> Option<f64> {
    let raw = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    // The file holds millidegrees Celsius, e.g. "54608".
    raw.trim().parse::<f64>().ok().map(|milli| milli / 1000.0)
}

/// `(idle, total)` jiffies from the previous CPU sample; usage is only
/// meaningful as a delta between two reads. Interleaved requests from
/// multiple dashboard tabs just shorten each other's measurement window,
/// which is still a valid usage figure.
static PREV_CPU: Mutex<Option<(u64, u64)>> = Mutex::new(None);

fn cpu_usage_pct() -> Option<f64> {
    let first = read_cpu_times()?;
    let previous = lock_prev_cpu().replace(first);
    if let Some(previous) = previous {
        return usage_between(previous, first);
    }
    // First call since startup: no baseline yet. Take a short second sample
    // so the initial dashboard load still shows a number.
    std::thread::sleep(Duration::from_millis(250));
    let second = read_cpu_times()?;
    *lock_prev_cpu() = Some(second);
    usage_between(first, second)
}

fn lock_prev_cpu() -> std::sync::MutexGuard<'static, Option<(u64, u64)>> {
    PREV_CPU
        .lock()
        .expect("no code holding this lock can panic")
}

fn read_cpu_times() -> Option<(u64, u64)> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// Parses the aggregate `cpu` line of `/proc/stat` into `(idle, total)`.
fn parse_proc_stat(stat: &str) -> Option<(u64, u64)> {
    let mut fields = stat.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // user nice system idle iowait irq softirq steal guest guest_nice
    let values: Vec<u64> = fields.map_while(|field| field.parse().ok()).collect();
    if values.len() < 5 {
        return None;
    }
    let idle = values[3] + values[4]; // idle + iowait
    Some((idle, values.iter().sum()))
}

fn usage_between(previous: (u64, u64), current: (u64, u64)) -> Option<f64> {
    let idle = current.0.saturating_sub(previous.0);
    let total = current.1.saturating_sub(previous.1);
    if total == 0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "jiffy deltas are far below 2^52"
    )]
    Some(100.0 * (1.0 - idle as f64 / total as f64))
}

/// Parses `/proc/meminfo` into `(memory used %, swap used %)`.
///
/// Memory use is measured against `MemAvailable` (what the kernel could
/// actually give an application), not `MemFree` — page cache on a Pi eats
/// all "free" memory by design and would read as a permanent false alarm.
fn parse_meminfo(raw: &str) -> (Option<f64>, Option<f64>) {
    let mut total = None;
    let mut available = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in raw.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok());
        match key {
            "MemTotal" => total = value,
            "MemAvailable" => available = value,
            "SwapTotal" => swap_total = value,
            "SwapFree" => swap_free = value,
            _ => {}
        }
    }
    let used_pct =
        |total: f64, unused: f64| (total > 0.0).then(|| 100.0 * (total - unused) / total);
    (
        total.zip(available).and_then(|(t, a)| used_pct(t, a)),
        swap_total.zip(swap_free).and_then(|(t, f)| used_pct(t, f)),
    )
}

fn throttle() -> Option<Throttle> {
    // The firmware node's sysfs path varies across kernel/device-tree
    // versions; fall back to vcgencmd (present on every Pi OS image).
    let raw = std::fs::read_to_string("/sys/devices/platform/soc/soc:firmware/get_throttled")
        .ok()
        .or_else(|| {
            let output = std::process::Command::new("vcgencmd")
                .arg("get_throttled")
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        })?;
    parse_throttled(&raw)
}

/// Parses the firmware throttle bitmask: bare hex with or without `0x`,
/// or vcgencmd's `throttled=0x...` form.
fn parse_throttled(raw: &str) -> Option<Throttle> {
    let hex = raw.trim().trim_start_matches("throttled=");
    let bits = u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok()?;
    Some(Throttle {
        under_voltage: bits & 0x1 != 0,
        frequency_capped: bits & 0x2 != 0,
        throttled: bits & 0x4 != 0,
        soft_temp_limit: bits & 0x8 != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_yields_idle_and_total() {
        let stat = "cpu  1000 50 300 8000 200 10 20 0 0 0\ncpu0 250 12 75 2000 50 2 5 0 0 0\n";
        let (idle, total) = parse_proc_stat(stat).expect("parses");
        assert_eq!(idle, 8200);
        assert_eq!(total, 9580);
    }

    #[test]
    fn malformed_proc_stat_is_none() {
        assert!(parse_proc_stat("intr 12345").is_none());
        assert!(parse_proc_stat("cpu 1 2").is_none());
    }

    #[test]
    fn usage_is_busy_share_of_delta() {
        // 100 jiffies elapsed, 25 of them idle → 75% busy.
        let usage = usage_between((1000, 5000), (1025, 5100)).expect("some");
        assert!((usage - 75.0).abs() < 1e-9);
        assert!(usage_between((1000, 5000), (1000, 5000)).is_none());
    }

    #[test]
    fn meminfo_yields_used_percentages() {
        let raw = "MemTotal:       1000000 kB\nMemFree:         100000 kB\n\
                   MemAvailable:    400000 kB\nSwapTotal:       200000 kB\n\
                   SwapFree:        150000 kB\n";
        let (memory, swap) = parse_meminfo(raw);
        assert!((memory.expect("some") - 60.0).abs() < 1e-9);
        assert!((swap.expect("some") - 25.0).abs() < 1e-9);
    }

    #[test]
    fn no_swap_configured_is_none() {
        let raw = "MemTotal: 1000 kB\nMemAvailable: 500 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
        let (memory, swap) = parse_meminfo(raw);
        assert!(memory.is_some());
        assert!(swap.is_none());
    }

    #[test]
    fn vcgencmd_output_form_decodes() {
        let t = parse_throttled("throttled=0x50005\n").expect("parses");
        assert!(t.under_voltage && t.throttled);
    }

    #[test]
    fn throttle_bits_decode() {
        let t = parse_throttled("0x50005").expect("parses");
        assert!(t.under_voltage);
        assert!(!t.frequency_capped);
        assert!(t.throttled);
        assert!(!t.soft_temp_limit);
        let clear = parse_throttled("0\n").expect("parses");
        assert!(!clear.under_voltage && !clear.throttled);
    }
}
