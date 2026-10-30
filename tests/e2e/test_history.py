"""readings outlive a reboot or a reflash, and only for the sensor they came from."""

import http.client
import json
import struct

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CODE, Run
from sensor import Sensor


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


@pytest.fixture
def history(tmp_path):
    return tmp_path / "history"


def args(history):
    return ["--http", "127.0.0.1:0", "--history-dir", str(history)]


def get(run, target):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", target)
    return json.loads(conn.getresponse().read())


def first_session(tmp_path, signer, history, **sensor_args):
    sensor = Sensor(CODE, signer.public_key(), **sensor_args)
    Run(sensor, tmp_path, signer, args=args(history)).connect()


def bonded_sensor(tmp_path, **sensor_args):
    sensor = Sensor(CODE, bonded=True, **sensor_args)
    sensor.key = bytes.fromhex((tmp_path / "bond.key").read_text())
    return sensor


def test_readings_survive_a_restart(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=143, age=30)
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    rows = get(run, "/api/readings")
    assert [(r["glucose"], r["clock"], r["state"], r["trend"]) for r in rows] == [
        (143, 863970, 6, 1.1)]
    assert get(run, "/api/status")["latest"]["glucose"] == 143
    run.close()


def test_all_fields_survive_a_restart(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=120, state=2, age=30)
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert get(run, "/api/readings")[0]["glucose"] is None
    assert get(run, "/api/readings")[0]["state"] == 2
    assert get(run, "/api/readings")[0]["time"] is not None
    run.close()


def test_backfilled_readings_survive_a_restart(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=864000, age=30)
    sensor.history = [(830000, 101), (860000, 103)]
    Run(sensor, tmp_path, args=args(history)).connect()
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    rows = get(run, "/api/readings")
    assert [r["clock"] for r in rows] == [800000, 830000, 860000, 863970]
    assert [r["backfill"] for r in rows] == [False, True, True, False]
    run.close()


def test_restart_resumes_backfill_from_saved_history(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=864000, age=30)
    Run(sensor, tmp_path, args=args(history)).connect()
    request = next(c[1] for c in sensor.commands if c[1][0] == 0x59)
    assert struct.unpack("<I", request[1:5])[0] == 800001


def test_interrupted_backfill_is_retried(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    bare = bonded_sensor(tmp_path, clock=801000, age=0)
    Run(bare, tmp_path, args=args(history)).connect()
    sensor = bonded_sensor(tmp_path, clock=801300, age=0)
    sensor.history = [(800300, 101), (800600, 102)]
    run = Run(sensor, tmp_path, args=args(history)).connect()
    assert [b["clock"] for b in run.backfill] == [800300, 800600]


def test_backfill_never_asks_beyond_the_sensor_window(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=700000, age=0)
    bonded = bonded_sensor(tmp_path, clock=800000, age=0)
    Run(bonded, tmp_path, args=args(history)).connect()
    sensor = bonded_sensor(tmp_path, clock=800100, age=0)
    Run(sensor, tmp_path, args=args(history)).connect()
    request = next(c[1] for c in sensor.commands if c[1][0] == 0x59)
    assert struct.unpack("<I", request[1:5])[0] >= 800000 - 86400


def full_history(count):
    start = 864000 - count * 300
    return [(start + i * 300, 100 + i % 100) for i in range(count)]


def test_a_week_of_readings_is_streamed(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=864300, age=0)
    sensor = bonded_sensor(tmp_path, clock=864600, age=0)
    sensor.history = full_history(2015)
    run = Run(sensor, tmp_path, last_seen=1, args=args(history)).session()
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", "/api/readings")
    response = conn.getresponse()
    assert response.getheader("content-length") is None, "must not be built in memory first"
    rows = json.loads(response.read())
    assert len(rows) == 2016
    assert rows == sorted(rows, key=lambda row: row["clock"])
    assert len(get(run, "/api/readings?limit=7")) == 7
    run.close()


def test_up_to_date_history_needs_no_backfill_after_restart(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=864000, age=30)
    sensor = bonded_sensor(tmp_path, clock=864100, age=30)
    Run(sensor, tmp_path, args=args(history)).connect()
    assert not any(c[1][0] == 0x59 for c in sensor.commands)


def test_new_sensor_clears_saved_history(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=143)
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("POST", "/api/pair", "code=5150", {"x-nanodrip": "1"})
    assert conn.getresponse().status == 200
    run.pump("connected")
    run.close()
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert get(run, "/api/readings") == []
    run.close()


def test_damaged_history_is_ignored(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=143)
    for path in history.iterdir():
        path.write_bytes(path.read_bytes()[:-3])
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert get(run, "/api/readings") == []
    run.close()
