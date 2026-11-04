<img src="assets/logo.svg" alt="" width="96">

# nanodrip

ESP32 reader for Dexcom G7, One+ and Stelo sensors. it pairs with the sensor
over BLE, then publishes readings to MQTT and serves a small web UI on your
network.

**experimental. do not use for treatment decisions.** the protocol is reverse
engineered, and a sensor only talks to one reader at a time, so nanodrip takes
the place of the Dexcom app on that sensor.

## hardware

an ESP32-S3 with 16 MB of flash, ideally with octal PSRAM (an N16R8 module). less
flash needs smaller `log` and `tags` partitions in `firmware/partitions.csv`, and other chips
need a different target in `firmware/.cargo/config.toml` and matching settings in
`firmware/sdkconfig.defaults`.

## setup

the toolchain is pinned in `mise.toml`:

    make toolchain

build time configuration comes from `nanodrip.env`, which Git ignores (start
from `nanodrip.env.example`), or from the environment. all of it is optional:

| variable | meaning |
| --- | --- |
| `NANODRIP_WIFI_SSID`, `NANODRIP_WIFI_PASS` | network to join until one is set in the web UI |
| `NANODRIP_AP_PASS` | password of the setup network (default `nanodrip-setup`) |
| `NANODRIP_DEVICE` | name shown in the web UI and used in MQTT topics (default `esp32`) |
| `NANODRIP_MQTT` | broker as `host:port` |
| `NANODRIP_MQTT_USER`, `NANODRIP_MQTT_PASS` | broker login |

build and flash:

    make flash
    make monitor

the "wifi network" section of the web UI changes the network later, and that
choice survives a reboot and takes priority over the build time values.

with no network configured, or if the configured one cannot be joined at boot,
the board hosts a WiFi network called `nanodrip-setup`. join it and open
<http://192.168.71.1/>. a dropped network is only retried while the device is
running; after a reboot during an outage it stays on the setup network until
the next reboot or until a network is saved.

## pairing

Stelo and G7 sensors with a four digit code need certificates, which are not
distributed with nanodrip. this one-time step reads them from xDrip's public
setup QR code and stores them on the device:

    HOST=<device address> make import-keys

then open the web UI, expand "sensor pairing" and enter the code printed on the
sensor applicator.

remove the Dexcom app (or any other collector) from the sensor first.

## MQTT

set the broker under "mqtt broker" in the web UI, as `host:port` with an optional
login, or at build time with `NANODRIP_MQTT`. MQTT stays off until one is set.

published at QoS 1 as JSON. values wait in memory while the broker is away.

| topic | retained | content |
| --- | --- | --- |
| `nanodrip/<device>/<sensor>/glucose` | yes | each live reading |
| `nanodrip/<device>/<sensor>/backfill` | no | readings recovered after a gap |

`<sensor>` changes with each new sensor, so subscribe to
`nanodrip/<device>/+/glucose` to keep following along. the last value of a
finished sensor stays retained on the broker until it is cleared.

    {"sensor":"db5e369f16c2","clock":863970,"time":1799999970,"glucose":143,"state":6,"trend":1.1,"backfill":false}

`sensor` is the BLE address of the sensor. `glucose` is null unless the sensor
reports a trustworthy value, for example during warmup. `clock` is seconds since
sensor start, `time` is Unix seconds (null until the wall clock is known), `trend`
is mg/dL per minute.

## web UI

| path | content |
| --- | --- |
| `/` | latest value, chart, statistics, targets, readings table, settings |
| `/api/status` | device state and known sensors, without any secrets |
| `/api/summary?hours=N` | mean, and seconds above, below and within the target range |
| `/api/summary?since=T&until=T` | the same between two Unix times |
| `/api/summary?from=C&to=C` | the same between two clock values of the latest sensor |
| `/api/readings?limit=N` | history, oldest first |
| `/api/tags` | tags, oldest first |
| `POST /api/tag` | add `time` and `name`, or rename with `old` as well |
| `POST /api/tag/remove` | remove `time` and `name` |

`/api/summary` and `/api/readings` take `sensor=<id>` to show one sensor. POST
routes need an `x-nanodrip` header, and `time` is in Unix seconds.

click or tap the chart to tag that moment, or press "+" to choose a date and time,
which starts at now. tags you have used are suggested, most used first, and typing
filters them. a tag shows as a dot on the line, named when there is room, and
hovering near one shows it beside the reading. clicking the dot renames or removes
it. the readings table gives tags their own row, one row per exact time, among the
newest 50 readings. names are lowercase, up to 32 bytes, and times are whole minutes.
tags live on the device, so every browser sees the same ones, and they survive
reboots, flashing and changing sensors.

the statistics follow the selected window, which is one of the range buttons,
including "max" for everything held, or a span picked on the chart: ctrl+drag with
a mouse or trackpad, or drag on a touch screen. with a mouse or trackpad over the
chart, shift+scroll moves through history and ctrl+scroll zooms around the pointer.
the mean
counts each reading until the next one, up to 15 minutes, so time with no data is
left out.

"targets" compares the window with published figures for a chosen group: no
diabetes, type 1 or 2, older or high risk, and pregnancy. each figure links to a
summary of its source. "met" and "not met" are used only for published targets;
typical values and suggestions say how the reading compares instead. the group is
remembered per browser. "my own range" uses the range set under "target range",
which defaults to 63 to 140 mg/dL. none of this is medical advice.

history is kept per sensor and saved to flash after every session, so it
survives reboots, normal flashing and pairing a new sensor. the log holds about
12 years of readings on 16 MB of flash, then overwrites the oldest; on a board
with no PSRAM, memory limits it to about 70 days. erasing the whole flash clears
it. missing readings are recovered automatically from the sensor, a day at a
time, going back as far as it will answer. Dexcom documents 24 hours, but a Stelo
has returned more than twice that.

to keep a copy, save `/api/readings`. `tools/restore_history.py` turns that file
into an image for the `log` partition at `0x400000`.

## security

MQTT and HTTP are unencrypted and the web UI has no login, so anyone on the
network can read glucose data and change the pairing. keep it on a network you
trust. WiFi credentials, the pairing key and the certificates sit unencrypted
in flash.

## development

    make test-e2e    # Python tests against an emulated sensor and broker
    make precommit   # formatting and lints

## license

GPL-3.0-only. the sensor protocol is derived from
[xDrip+](https://github.com/NightscoutFoundation/xDrip) (GPL-3.0), with G5/G6
background from
[Dexcom-ESP32-Reader](https://github.com/TheEpicBigBoss/Dexcom-ESP32-Reader)
(Apache-2.0).
