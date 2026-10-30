# design

## layout

| path | role | runs on |
| --- | --- | --- |
| `crates/nanodrip-core` | sensor protocol, sans-io, `no_std` + alloc | host, ESP32 |
| `crates/nanodrip-app` | history, MQTT, HTTP, settings, on `std::net` | host, ESP32 |
| `crates/nanodrip-sim` | stdin/stdout harness around core and app | host |
| `firmware` | BLE, WiFi, NVS, on ESP-IDF | ESP32 |
| `tests/e2e` | emulated sensor and broker in Python | host |

everything but `firmware` is exercised by the end-to-end tests. keeping the
firmware down to glue is what makes that coverage meaningful: new logic
belongs in core or app.

the emulator in `tests/e2e/sensor.py` is written independently, including its
own P-256 arithmetic, so a misreading of the protocol in one implementation
shows up as a disagreement.

## session flow

`Session` consumes notifications and returns `Action`s. the order follows xDrip
`libkeks`:

1. J-PAKE: `0a 00`, then three rounds of 160 byte packets on the extra
   characteristic (x, y of the public point, x, y of the proof point, proof
   scalar). our packet goes first and its `0a nn` command after.
2. challenge: `02` + token + slot, `03` + proof + challenge, `04` + response,
   `05` status. the shared key is the first 16 bytes of SHA-256 over the x
   coordinate, used with AES over the doubled input.
3. four digit codes only: certificate exchange (`0b`, `0c`, `0d`), signing the
   sensor challenge with ECDSA over P-256.
4. `06 19`, the sensor echoes it, the BLE link bonds.
5. `4e` on control returns a reading. `59` requests backfill when the gap since
   the last reading exceeds 420 s.

reconnections skip straight to step 2 with the saved key, and the firmware
encrypts the link before using control because Android does that implicitly.

## decisions

- a glucose value is only reported when the sensor state is ok or needs
  calibration and the value is above 13, which are error codes. everything else
  is published with null glucose, so consumers see the state without a number.
- the sensor proof is verified after a fresh exchange but not on reconnect,
  matching xDrip. a wrong saved key is caught by the status message and cleared.
- the key is saved only after the sensor reports authenticated, so a wrong code
  never persists.
- history is kept packed at 13 bytes per reading and saved in six hour slots to
  a `history` partition, which a normal flash leaves alone. the `nvs` partition
  must never move, because it holds the pairing.
- backfill resumes from the oldest gap within the sensor's 24 hour window, not
  from the newest reading, so an interrupted backfill is retried by itself.
- `/api/readings` is streamed a page at a time: a week of JSON does not fit in
  memory, and building it in one piece crashed the device.
- MQTT uses a short connection per flush at QoS 1, and unconfirmed messages stay
  queued (up to one day). retained values live on `glucose` only.
- history is ordered by sensor clock, so it is cleared when the pairing changes:
  another sensor's clock has a different origin.
- settings changes bump `generation`. the BLE loop applies them between
  connections, so the web server never touches radio state.
- POST routes need an `x-nanodrip` header, which cross-origin pages cannot send
  without a preflight. this stops CSRF, it is not authentication.
- certificates are not distributed with nanodrip. `tools/import_keys.py` reads
  them from xDrip's public setup QR code and sends them to the device.
- ESP-IDF with NimBLE rather than a bare metal stack: WiFi, BLE bonding and
  flash storage all have to work at once on hardware we can only test by hand.

## unverified against a real sensor

a real Stelo (`DX01` name prefix) has completed J-PAKE, the challenge, the
certificate exchange, bonding and a glucose read on an ESP32-S3. it taught us
two things the emulator now reproduces or the code now follows:

- extra and backfill are subscribed as notifications and auth and control as
  indications, whatever else a characteristic offers.
- the sensor begins its certificate on extra before its `0b` reply on auth.

these still rest on reading xDrip rather than on observation:

- glucose values against a reference such as the Stelo app.

also observed: backfill records arrive one per notification, ahead of the `59`
reply on control, and a saved key is accepted on reconnect but the sensor still
reports the link as unbonded, so certificates and bonding repeat each session.

## known limits

- gaps the sensor has no data for are requested again every session until they
  leave the 24 hour window, because nothing records that a backfill finished.
- a failed WiFi join at boot stays on the setup network until reboot, by design
  simple: there is one radio, and retrying would interrupt the setup network.
