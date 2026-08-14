# pi-garage-monitor

A small Rust daemon for a Raspberry Pi 3B that logs temperature, humidity,
and barometric pressure from a BME280 sensor to SQLite and serves a
self-hosted web dashboard. Built to track how livable the garage loft gets
as roofing and insulation go in.

## Hardware

- Raspberry Pi 3B running Raspberry Pi OS
- BME280 breakout board, 3.3V 6-pin variant (GY-BME280)
- 6 female-to-female jumper wires

### Wiring

All six sensor pins land directly on the Pi header — no splices. CSB and
SDO are strapped at the Pi end: CSB high locks the chip into I²C mode, and
SDO low selects address `0x76` (the address the daemon initializes).

| BME280 pin | Purpose                        | Pi header pin  |
|------------|--------------------------------|----------------|
| VCC        | power                          | 1 (3.3V)       |
| SDA        | I²C data                       | 3 (GPIO 2)     |
| SCL        | I²C clock                      | 5 (GPIO 3)     |
| GND        | ground                         | 6 (GND)        |
| SDO        | → GND selects address `0x76`   | 9 (GND)        |
| CSB        | → 3.3V locks I²C mode          | 17 (3.3V)      |

Header counting: pin 1 is the corner pin nearest the SD card (square pad);
odd pins are the row toward the middle of the board, even pins the row
along the board edge. Match the sensor end by silkscreen label, not
physical order — pin order varies between breakout revisions.

Keep the sensor a short distance away from the Pi itself — the SoC's heat
skews readings by a degree or two if the board sits directly above it.
Connect and disconnect only with the Pi powered off; hotplugging the GPIO
header can brown out the board.

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

## Cutting over from mock mode

If the daemon has been running with a `--mock` override while waiting on
hardware, switch to the real sensor with a blank database (mock-era data
would pollute the baseline):

```sh
# with the sensor wired up (Pi powered off during connection):
i2cdetect -y 1                       # must show 76 before going further

sudo systemctl stop garage-monitor
sudo systemctl revert garage-monitor # removes the --mock override drop-in
sudo rm /var/lib/garage-monitor/garage.db
sudo systemctl start garage-monitor

journalctl -u garage-monitor -f      # watch for sensor.read.success
```

The schema is recreated automatically on first start; records, daily
history, and risk assessments all begin fresh from the first real reading.
The dashboard needs no changes — reload it and the empty-state messages
fill in as data accumulates (charts after a few readings, daily ranges
after the first day, the outdoor delta within ~30 minutes).

## Nightly database backup (optional)

A year of renovation baseline shouldn't live only on an SD card in a hot
garage. `deploy/backup-garage-db.sh` snapshots the database with SQLite's
online-backup API (safe against the live daemon), verifies integrity, and
rsyncs it to another machine — keeping a self-rotating week of dailies
plus one archival copy per month. Setup instructions are in the script's
header comment; edit `DB`/`DEST` at the top for your hosts, and drive it
from cron:

```
17 3 * * * /usr/local/bin/backup-garage-db.sh 2>&1 | logger -t garage-backup
```

Restoring is just copying a snapshot back to
`/var/lib/garage-monitor/garage.db` with the daemon stopped.

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

## Push alerts via ntfy (optional)

Pass an [ntfy](https://ntfy.sh) URL to get push notifications when the
risk level changes:

```sh
pi-garage-monitor ... --ntfy-url https://ntfy.sh/<topic>
```

Subscribe to the same topic in the ntfy phone app. Escalations
(ok → caution → alert) push immediately; the follow-up "all clear" waits
out a 15-minute cooldown so conditions hovering at a threshold can't
flood the phone. Works identically against a self-hosted ntfy server —
just point the URL at it.

On the public ntfy.sh server the topic name is the only access control,
so pick something unguessable (`garage-<random suffix>`), not a word.

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
- `GET /api/outdoor?hours=24` — outdoor observation history, bucket-averaged the same way; empty until weather polling is enabled
- `GET /api/stream` — server-sent events; emits a `reading` or `outdoor` event the moment new data is stored (each carrying the full `/api/conditions` JSON) and an `events` event carrying the full timeline whenever an event is added or deleted — the dashboard applies these directly instead of polling
- `GET /api/records` — all-time extremes (high/low temperature and humidity, each with its timestamp)
- `GET /api/daily?days=30` — per-local-calendar-day temperature min/max
- `GET /api/conditions` — latest indoor + outdoor state plus the condensation assessment (`status.level` is `ok`/`warning`/`critical`)
- `GET /api/risk?days=30` — retroactive per-day condensation summary: minutes saturated / near saturation, humidity peak, indoor low, outdoor dew-point max, and the day's worst severity level
- `GET /api/delta?hours=24` — indoor vs outdoor temperature joined into shared ≥15-min buckets, with `delta_c` (positive = warmer inside); empty until weather polling is enabled
- `GET /api/events` / `POST /api/events` (`{"ts": <unix-secs, optional>, "label": "roof replaced"}`) / `DELETE /api/events/{id}` — renovation timeline markers, drawn as dashed vertical lines on the charts; add them from the dashboard's "＋ event" button or via curl
- `GET /api/conditions` also reports a `system` object — the Pi's SoC temperature, CPU/memory/swap usage, and live firmware throttle flags — for the dashboard's header stats

The dashboard can be pinned to a phone home screen (web-app manifest is
served at `/manifest.webmanifest`); on iOS use Share → Add to Home Screen.
Charts render sampling gaps (power cuts) as visible breaks in the line,
the header warns when the last reading is more than 10 minutes old, and
the "daily temperature swing" chart (7-day average, indoor vs outdoor) is
the single best measure of how well the insulation is working.

Readings are `{ ts, temperature_c, humidity_pct, pressure_hpa }` with `ts`
in Unix seconds.

## Storage

One reading per minute is ~525k rows/year — a few tens of MB of SQLite.
No retention policy needed; the whole point is the year-over-year record.
Back it up by copying `/var/lib/garage-monitor/garage.db`.
