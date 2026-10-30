# nanodrip

ESP32 reader for Dexcom G7, One+ and Stelo sensors. it pairs with the sensor
over BLE, then publishes readings to MQTT and serves a small web UI on your
network.

**experimental. do not use for treatment decisions.** the protocol is reverse
engineered, and a sensor only talks to one reader at a time, so nanodrip takes
the place of the Dexcom app on that sensor.

## hardware

an ESP32-S3 with at least 4 MB of flash. other chips need a different target in
`firmware/.cargo/config.toml` and matching settings in `firmware/sdkconfig.defaults`.

## setup

the toolchain is pinned in `mise.toml`:

    make toolchain

build time configuration comes from `nanodrip.env`, which Git ignores (start
from `nanodrip.env.example`), or from the environment. all of it is optional:

| variable | meaning |
| --- | --- |
| `NANODRIP_WIFI_SSID`, `NANODRIP_WIFI_PASS` | network to join until one is set in the web UI |
| `NANODRIP_AP_PASS` | password of the setup network (default `nanodrip-setup`) |
| `NANODRIP_DEVICE` | name used in MQTT topics (default `esp32`) |
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

the same values can be pasted into the web UI as hex instead: they are xDrip's
`keks_p1`, `keks_p2` and `keks_p3` settings.

remove the Dexcom app (or any other collector) from the sensor first. pairing a
new sensor clears the history held on the device.

## MQTT

set the broker under "mqtt broker" in the web UI, as `host:port` with an optional
login, or at build time with `NANODRIP_MQTT`. MQTT stays off until one is set.

published at QoS 1 as JSON. values wait in memory while the broker is away.

| topic | retained | content |
| --- | --- | --- |
| `nanodrip/<device>/glucose` | yes | each live reading |
| `nanodrip/<device>/backfill` | no | readings recovered after a gap |

    {"clock":863970,"time":1799999970,"glucose":143,"state":6,"trend":1.1,"backfill":false}

`glucose` is null unless the sensor reports a trustworthy value, for example
during warmup. `clock` is seconds since sensor start, `time` is Unix seconds (null
until the wall clock is known), `trend` is mg/dl per minute.

## web UI

| path | content |
| --- | --- |
| `/` | latest value, chart, time out of range, readings table, settings |
| `/api/status` | device state, without any secrets |
| `/api/summary?hours=N` | mean, and seconds above, below and within the target range |
| `/api/summary?from=C&to=C` | the same between two sensor clock values |
| `/api/readings?limit=N` | history, oldest first |

the target range defaults to 63 to 140 mg/dl and is set under "target range".
the mean and the time above and below follow the selected interval, which can
be one of the range buttons or an exact span dragged across the chart. each
reading counts until the next one, up to 15 minutes, so time with no data is
left out of these figures.

history covers up to seven days and is saved to flash after every session, so
it survives reboots and normal flashing. missing readings are recovered
automatically from the sensor, which keeps about the last 24 hours. pairing a
new sensor clears it, and so does erasing the whole flash.

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
