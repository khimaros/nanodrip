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
- history is per sensor, keyed by BLE address, the only identity that is stable
  and unique; the advertised name is kept for display. each sensor has its own
  clock, so readings share a timeline through wall clock time. pairing and
  forgetting leave history alone, and pairing a sensor again continues it.
- all history stays in memory: three bytes per reading, in runs on the sensor's
  five minute grid. a reading off the grid starts a new run, so no clock is ever
  rounded. state values go through a table, as xDrip names values up to `0xC3`.
  runs are capped at a week to avoid large contiguous allocations.
- the `log` partition is a ring of 4 KB sectors. each sector belongs to one
  sensor and starts with a 40 byte header naming it, followed by 9 byte records,
  so a raw copy is readable alone and sensors taking turns each keep an open
  sector instead of wasting one per switch. appending never rewrites, which
  evens out wear, and a damaged record loses only itself. NVS is not used here
  because its RAM use grows with partition size. a normal flash leaves `log`
  alone. the `nvs` partition must never move, because it holds the pairing.
- at boot the log is read back into memory, so nothing but saving touches flash.
  the store's capacity is what PSRAM holds, capped at what the log holds.
- tags are names on moments in wall clock time, not on readings or sensors, so
  they outlast both. the `tags` partition is a ring of its own, holding adds and
  removals that are replayed at boot, with room for about 25,000 changes. unlike
  readings, a tag has no other copy, so it is written to flash by the web thread
  before the request succeeds rather than after the next session. suggestions are
  worked out in the browser from the tags themselves. times are whole minutes, so
  tags in one minute are at the same time and share a table row.
- `tools/restore_history.py` turns a saved `/api/readings` into a log image. it is
  written from this description, independently of the Rust code.
- backfill resumes from the oldest gap within the last 24 hours, not from the
  newest reading, so an interrupted backfill is retried by itself.
- once that is done, the session walks back from the oldest reading, one day per
  `59` request, until a day comes back empty or history is full. Dexcom documents
  24 hours, but a real Stelo returned all of its first 52 hours, so the sensor's
  answer sets the limit. the result is kept in memory only: a reboot costs one
  empty request.
- `/api/readings` is streamed a page at a time: long history as JSON does not fit
  in memory, and building it in one piece crashed the device.
- MQTT uses a short connection per flush at QoS 1, and unconfirmed messages stay
  queued (up to one day). retained values live on `glucose` only. topics carry the
  sensor so two sensors cannot overwrite each other's retained value, at the cost
  of a new topic per sensor. the payload names the sensor too.
- the page keeps all readings in the browser and works out its statistics there,
  fetching again only when the count in `/api/status` changes, with one request
  at a time. tags work the same way through `tag_revision`. range changes need no request. the group chosen for targets lives in
  the browser's `localStorage`, and the published figures live in the page.
- the page never loads anything from outside, so it works offline. source links
  are the only external addresses, and open only when followed. the logo and tab
  icon are copied into it from `assets/`, a test keeps them equal, and the icon is
  a bolder drawing because the logo's small drips blur at 16 px.
- a watchdog restarts the board if the radio loop stops for 10 minutes, because
  some BLE calls can wait forever. readings are saved after each session.
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

- only one sensor is paired at a time. the API can filter by sensor, but nothing
  yet groups sensors by person or reads several at once.
- the page fetches all readings when the count changes, about 45 seconds per year
  of history over WiFi, so long history will need a request for only what is new.
- if the ring is ever full, a walk back into older history writes last and so
  overwrites the newest saved readings before the oldest.
- a full `tags` ring overwrites the oldest changes, and until a reboot those tags
  are still shown. removing a name's last tag also drops it from suggestions.
- gaps the sensor has no data for are requested again every session until they
  leave the 24 hour window, because nothing records that a backfill finished.
- a failed WiFi join at boot stays on the setup network until reboot, by design
  simple: there is one radio, and retrying would interrupt the setup network.
