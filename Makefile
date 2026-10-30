-include nanodrip.env
export NANODRIP_DEVICE NANODRIP_WIFI_SSID NANODRIP_WIFI_PASS NANODRIP_AP_PASS
export NANODRIP_MQTT NANODRIP_MQTT_USER NANODRIP_MQTT_PASS

ESP_EXPORT := .espup-export.sh
PORT ?= /dev/ttyACM0
FIRMWARE := firmware/target/xtensa-esp32s3-espidf/release/nanodrip-firmware
# mise pins RUSTUP_TOOLCHAIN for the host crates, which would hide the esp one.
# esp-idf also refuses to run from a venv, so it gets the bare mise python, and
# its esp-clang links an older libxml2 than current distributions ship.
ESP_PYTHON := $(shell mise where python)/bin
ESP_LIBS := $(shell mise where conda:libxml2)/lib
ESP_ENV := . ./$(ESP_EXPORT) && cd firmware && env -u VIRTUAL_ENV PATH="$(ESP_PYTHON):$$PATH" LD_LIBRARY_PATH="$(ESP_LIBS)" RUSTUP_TOOLCHAIN=esp

build:
	cargo build --release
.PHONY: build

toolchain:
	mise install
	espup install --targets esp32s3 --export-file $(ESP_EXPORT)
.PHONY: toolchain

firmware:
	$(ESP_ENV) cargo build --release
.PHONY: firmware

flash: firmware
	cd firmware && espflash flash --non-interactive --port $(PORT) --partition-table partitions.csv ../$(FIRMWARE)
.PHONY: flash

monitor:
	espflash monitor --non-interactive --port $(PORT)
.PHONY: monitor

import-keys:
	uv run --with opencv-python-headless tools/import_keys.py --host $(HOST)
.PHONY: import-keys

test-e2e:
	cargo build --bin nanodrip-sim
	uv run pytest
.PHONY: test-e2e

precommit:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	uv run --with ruff ruff check tests tools
.PHONY: precommit
