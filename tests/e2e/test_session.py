"""the sensor protocol: pairing, reconnection, and refusing to trust bad data."""

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CERT_A, CERT_B, CODE, NOW, Run
from sensor import Sensor, glucose_packet


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


def paired_key(tmp_path, signer):
    Run(Sensor(CODE, signer.public_key()), tmp_path, signer).connect()
    return (tmp_path / "bond.key").read_text().strip()


def test_first_pairing_yields_reading(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=143, age=30)
    run = Run(sensor, tmp_path, signer).connect()
    assert run.errors == []
    assert run.bonds == 1
    assert sensor.certs == [CERT_A, CERT_B]
    assert run.readings == [{"clock": 863970, "time": NOW - 30, "glucose": 143, "state": 6,
                             "trend": 1.1, "backfill": False}]
    assert run.disconnected
    assert ("control", b"\x09") in sensor.commands


def test_key_is_persisted_and_reused(tmp_path, signer):
    key = paired_key(tmp_path, signer)
    assert len(bytes.fromhex(key)) == 16
    sensor = Sensor(CODE, bonded=True, glucose=99)
    sensor.key = bytes.fromhex(key)
    run = Run(sensor, tmp_path).connect()
    assert [r["glucose"] for r in run.readings] == [99]
    assert run.bonds == 0
    assert sensor.client == [], "reconnect must skip j-pake"


def test_wrong_code_gets_no_reading(tmp_path, signer):
    run = Run(Sensor("9191", signer.public_key()), tmp_path, signer).connect()
    assert run.readings == []
    assert run.errors
    assert not (tmp_path / "bond.key").exists()


def test_stale_saved_key_is_cleared(tmp_path):
    (tmp_path / "bond.key").write_text("00" * 16)
    sensor = Sensor(CODE, bonded=True)
    sensor.key = bytes(range(16))
    run = Run(sensor, tmp_path).connect()
    assert run.errors == ["rejected"]
    assert not (tmp_path / "bond.key").exists()


@pytest.mark.parametrize("n", [0, 1, 2])
def test_tampered_proof_is_rejected(tmp_path, signer, n):
    sensor = Sensor(CODE, signer.public_key())
    sensor.corrupt_round = n
    run = Run(sensor, tmp_path, signer).connect()
    assert run.readings == []
    assert run.errors == ["proof"]


def test_impostor_challenge_is_rejected(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key())
    sensor.corrupt_hash = True
    run = Run(sensor, tmp_path, signer).connect()
    assert run.readings == []
    assert run.errors == ["challenge"]


def test_short_code_without_certificates_fails_clearly(tmp_path):
    run = Run(Sensor(CODE), tmp_path).connect()
    assert run.readings == []
    assert run.errors == ["missing-certificates"]


def test_warmup_reports_state_without_glucose(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=120, state=2)
    run = Run(sensor, tmp_path, signer).connect()
    assert [(r["glucose"], r["state"]) for r in run.readings] == [(None, 2)]


def test_error_codes_are_not_reported_as_glucose(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=5, state=6)
    run = Run(sensor, tmp_path, signer).connect()
    assert [r["glucose"] for r in run.readings] == [None]


def test_backfill_recovers_missed_readings(tmp_path, signer):
    key = paired_key(tmp_path, signer)
    sensor = Sensor(CODE, bonded=True, clock=864000, age=30)
    sensor.key = bytes.fromhex(key)
    sensor.history = [(863000, 101), (863300, 102), (863600, 103)]
    run = Run(sensor, tmp_path, last_seen=862900).connect()
    assert [(b["clock"], b["glucose"]) for b in run.backfill] == sensor.history
    assert [b["time"] for b in run.backfill] == [NOW - 1000, NOW - 700, NOW - 400]


def test_first_reading_recovers_the_last_day(tmp_path, signer):
    key = paired_key(tmp_path, signer)
    sensor = Sensor(CODE, bonded=True, clock=864000, age=30)
    sensor.key = bytes.fromhex(key)
    sensor.history = [(777000, 99), (780000, 101), (863000, 102)]
    run = Run(sensor, tmp_path).connect()
    assert [b["glucose"] for b in run.backfill] == [101, 102]


def test_no_backfill_when_up_to_date(tmp_path, signer):
    key = paired_key(tmp_path, signer)
    sensor = Sensor(CODE, bonded=True, clock=864000, age=30)
    sensor.key = bytes.fromhex(key)
    Run(sensor, tmp_path, last_seen=863970).connect()
    assert not any(c[1][0] == 0x59 for c in sensor.commands)


def test_reading_packet_layout_is_nineteen_bytes():
    assert len(glucose_packet(1, 2, 3, 4, 6)) == 19
