# contributing

- toolchain comes from `mise.toml`; run `make toolchain` once.
- add behavior to `crates/nanodrip-core` or `crates/nanodrip-app` and cover it
  in `tests/e2e`, written first and seen to fail. keep `firmware` as glue.
- `make test-e2e` and `make precommit` must pass.
- `make firmware` must build when `firmware` or the crates it uses change.
- keep `tests/e2e/sensor.py` independent of the Rust code. derive it from the
  wire format, never by porting.
- never commit certificate values, keys or WiFi credentials. test fixtures
  generate their own.
- documentation is lowercase, except acronyms, which are uppercase, and proper
  nouns, which are capitalized. "i" stays lowercase. glucose units are written
  `mg/dL`.
- when working from xDrip source, note that it has two `hexStringToByteArray`
  functions: the one in `libkeks` reverses the nibbles of each byte, used for
  its constants, and the one in `JoH` does not, used for the keks settings.
