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
scp ~/Downloads/pi-garage-monitor-armv7-unknown-linux-musleabihf pi@garage-pi.local:/tmp/pi-garage-monitor
scp deploy/garage-monitor.service pi@garage-pi.local:/tmp/

# on the Pi
sudo install -m 755 /tmp/pi-garage-monitor /usr/local/bin/pi-garage-monitor
sudo install -m 644 /tmp/garage-monitor.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now garage-monitor
journalctl -u garage-monitor -f   # watch it take its first readings
```

Then open <http://garage-pi.local:8080> from anything on your network.
The dashboard shows current values plus charts over 24h/48h/7d/30d.

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

Readings are `{ ts, temperature_c, humidity_pct, pressure_hpa }` with `ts`
in Unix seconds.

## Storage

One reading per minute is ~525k rows/year — a few tens of MB of SQLite.
No retention policy needed; the whole point is the year-over-year record.
Back it up by copying `/var/lib/garage-monitor/garage.db`.
