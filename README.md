# pi-garage-monitor

A small Rust daemon for a Raspberry Pi 3B that logs temperature, humidity,
and barometric pressure from a BME280 sensor to SQLite and serves a
self-hosted web dashboard. Built to track how livable the garage loft gets
as roofing and insulation go in.

## Hardware

- Raspberry Pi 3B running Raspberry Pi OS
- BME280 breakout board (I²C, address `0x76` — the common GY-BME280 default)
- 4 female-to-female jumper wires

### Wiring

| BME280 pin | Pi pin | Pi header position |
|------------|--------|--------------------|
| VIN / VCC  | 3.3V   | pin 1              |
| GND        | GND    | pin 6              |
| SDA        | SDA1   | pin 3 (GPIO 2)     |
| SCL        | SCL1   | pin 5 (GPIO 3)     |

Keep the sensor a short distance away from the Pi itself — the SoC's heat
skews readings by a degree or two if the board sits directly above it.

### Enable I²C and verify the sensor

```sh
sudo raspi-config nonint do_i2c 0
sudo apt install -y i2c-tools
i2cdetect -y 1   # expect "76" in the grid
```

If the sensor shows up at `77` instead, your breakout uses the secondary
address; change `new_primary` to `new_secondary` in `src/sensor.rs`.

## Getting a binary (GitHub Actions)

Every push to `main` cross-compiles static musl binaries for both Pi
flavors — no local toolchain needed. Grab one from the workflow run's
artifacts (or from a release if you push a `v*` tag):

- `pi-garage-monitor-armv7-unknown-linux-musleabihf` — Raspberry Pi OS 32-bit (the usual choice for a 3B)
- `pi-garage-monitor-aarch64-unknown-linux-musl` — Raspberry Pi OS 64-bit

Check which one you need with `uname -m` on the Pi: `armv7l` → armv7,
`aarch64` → aarch64.

## Deploy to the Pi

```sh
# from this repo on your workstation (binary downloaded from CI into ~/Downloads)
scp ~/Downloads/pi-garage-monitor-aarch64-unknown-linux-musl jamin@raspberrypi.local:/tmp/pi-garage-monitor
scp deploy/garage-monitor.service jamin@raspberrypi.local:/tmp/

# on the Pi
sudo install -m 755 /tmp/pi-garage-monitor /usr/local/bin/pi-garage-monitor
sudo install -m 644 /tmp/garage-monitor.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now garage-monitor
journalctl -u garage-monitor -f   # watch it take its first readings
```

Then open <http://garage-pi.local:8080> from anything on your network.
The dashboard shows current values plus charts over 24h/48h/7d/30d.

## Outdoor weather &amp; condensation warnings (optional)

Pass your location to enable outdoor weather via the free, keyless
[Open-Meteo](https://open-meteo.com/) API (polled every 15 minutes):

```sh
pi-garage-monitor ... --latitude 43.65 --longitude -79.38
```

Add the flags to `ExecStart` in the systemd unit for a permanent setup.
The dashboard then shows outdoor temperature/dew point and a warning
banner when condensation threatens electronics in the space:

- **Critical** — indoor air within 1 °C of its dew point: condensation is
  likely forming on surfaces right now.
- **Caution** — indoor air within 3 °C of its dew point, or the outdoor
  dew point is above the indoor temperature (incoming air will condense
  on cold contents — keep the space closed up).

Without the flags, the daemon never touches the network and the dashboard
still shows the indoor dew point derived from the BME280 itself.

## Local development (no Pi needed)

The sensor stack only compiles on Linux; everywhere else the daemon builds
with the hardware paths stubbed out. Use the simulated sensor:

```sh
cargo run -- --mock --interval-secs 1
open http://localhost:8080
```

## API

- `GET /api/latest` — most recent reading
- `GET /api/readings?hours=24` — history, bucket-averaged to ≤ ~1000 points
- `GET /api/records` — all-time extremes (high/low temperature and humidity, each with its timestamp)
- `GET /api/daily?days=30` — per-local-calendar-day temperature min/max
- `GET /api/conditions` — latest indoor + outdoor state plus the condensation assessment (`status.level` is `ok`/`warning`/`critical`)
- `GET /api/risk?days=30` — retroactive per-day condensation summary: minutes saturated / near saturation, humidity peak, indoor low, outdoor dew-point max, and the day's worst severity level
- `GET /api/delta?hours=24` — indoor vs outdoor temperature joined into shared ≥15-min buckets, with `delta_c` (positive = warmer inside); empty until weather polling is enabled

Readings are `{ ts, temperature_c, humidity_pct, pressure_hpa }` with `ts`
in Unix seconds.

## Storage

One reading per minute is ~525k rows/year — a few tens of MB of SQLite.
No retention policy needed; the whole point is the year-over-year record.
Back it up by copying `/var/lib/garage-monitor/garage.db`.
