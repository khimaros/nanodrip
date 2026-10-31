"""readings outlive a reboot or a reflash, and only for the sensor they came from."""

import http.client
import itertools
import json
import pathlib
import struct
import subprocess
import sys

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CODE, NOW, ROOT, Run, backfill_requests
from sensor import ADDRESS, NAME, Sensor

LOG_MAGIC = b"NDLG"
LOG_HEADER = 40
LOG_RECORD = 9
OTHER_ADDRESS = bytes.fromhex("c0ffee000002")
RESTORE_TOOL = pathlib.Path(ROOT) / "tools" / "restore_history.py"


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


def test_history_before_the_oldest_reading_is_fetched_by_day(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=800100, age=0)
    sensor.history = [(600000, 101), (700000, 102), (750000, 103)]
    run = Run(sensor, tmp_path, args=args(history)).connect()
    assert sorted(b["clock"] for b in run.backfill) == [600000, 700000, 750000]
    assert backfill_requests(sensor) == [
        (713600, 799999), (627200, 713599), (540800, 627199), (454400, 540799)]


def test_oldest_chunk_is_not_requested_again_once_empty(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=800100, age=0)
    sensor.history = [(750000, 103)]
    run = Run(sensor, tmp_path, args=args(history)).session()
    asked = len(backfill_requests(sensor))
    assert asked > 1
    sensor.clock += 300
    run.session().close()
    assert all(start > 750000 for start, _ in backfill_requests(sensor)[asked:])


def test_interrupted_walk_continues_from_the_oldest_reading(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=800100, age=0)
    sensor.history = [(750000, 103)]
    run = Run(sensor, tmp_path, args=args(history)).pump("connected").pump("idle")
    assert [b["clock"] for b in run.backfill] == [750000]
    run.close()
    sensor = bonded_sensor(tmp_path, clock=800400, age=0)
    Run(sensor, tmp_path, args=args(history)).connect()
    assert (663600, 749999) in backfill_requests(sensor)


def test_full_history_is_not_extended(tmp_path, signer, history):
    limited = [*args(history), "--history-capacity", "2016"]
    first_session(tmp_path, signer, history, clock=864300, age=0)
    sensor = bonded_sensor(tmp_path, clock=864600, age=0)
    sensor.history = full_history(2015)
    Run(sensor, tmp_path, last_seen=1, args=limited).connect()
    sensor = bonded_sensor(tmp_path, clock=864700, age=0)
    Run(sensor, tmp_path, args=limited).connect()
    assert all(start > 800000 for start, _ in backfill_requests(sensor))


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
    assert len(rows) == 2017
    assert rows == sorted(rows, key=lambda row: row["clock"])
    assert len(get(run, "/api/readings?limit=7")) == 7
    run.close()


def test_up_to_date_history_only_looks_further_back_after_restart(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=864000, age=30)
    sensor = bonded_sensor(tmp_path, clock=864100, age=30)
    Run(sensor, tmp_path, args=args(history)).connect()
    assert backfill_requests(sensor) == [(777570, 863969)]


def pair(run, code):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("POST", "/api/pair", f"code={code}", {"x-nanodrip": "1"})
    assert conn.getresponse().status == 200


def pair_next_sensor(tmp_path, signer, history, now, **sensor_args):
    """pairs a sensor through the web ui and runs its first session."""
    sensor_args.setdefault("address", OTHER_ADDRESS)
    run = Run(Sensor(CODE, signer.public_key(), **sensor_args), tmp_path, signer,
              args=args(history), now=now)
    pair(run, CODE)
    return run.session()


def test_pairing_a_new_sensor_keeps_saved_history(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=143, clock=800000, age=0)
    pair_next_sensor(tmp_path, signer, history, NOW + 7200, glucose=99, clock=3600, age=0).close()
    run = Run(Sensor(CODE), tmp_path, args=args(history), now=NOW + 7200)
    rows = get(run, "/api/readings")
    assert [(r["glucose"], r["clock"], r["time"]) for r in rows] == [
        (143, 800000, NOW), (99, 3600, NOW + 7200)]
    run.close()


def test_a_new_sensor_starts_its_own_backfill(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    run = pair_next_sensor(tmp_path, signer, history, NOW + 7200, clock=3600, age=0)
    assert backfill_requests(run.sensor) == [(1, 3600)], "the old sensor's clock must not count"
    run.close()


def test_summary_spans_sensors_by_wall_clock(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=50, clock=800000, age=0)
    run = pair_next_sensor(tmp_path, signer, history, NOW + 7200, glucose=99, clock=3600, age=0)
    every = get(run, "/api/summary?since=0&until=4294967295")
    assert every["below_seconds"] == 300 and every["in_range_seconds"] == 300
    only_old = get(run, f"/api/summary?since={NOW - 60}&until={NOW + 60}")
    assert only_old["below_seconds"] == 60 and only_old["in_range_seconds"] == 0
    assert get(run, "/api/summary?hours=1")["below_seconds"] == 0
    run.close()


def test_pairing_the_same_sensor_again_adds_no_duplicates(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    run = pair_next_sensor(tmp_path, signer, history, NOW + 300, clock=800300, age=0,
                           address=ADDRESS)
    run.sensor.history = [(800000, 120)]
    run.session().close()
    run = Run(Sensor(CODE), tmp_path, args=args(history), now=NOW + 300)
    assert [r["clock"] for r in get(run, "/api/readings")] == [800000, 800300]
    assert len(get(run, "/api/status")["sensors"]) == 1
    run.close()


def test_readings_name_their_sensor(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    run = pair_next_sensor(tmp_path, signer, history, NOW + 7200, clock=3600, age=0, name="DX0202")
    assert [r["sensor"] for r in get(run, "/api/readings")] == [ADDRESS.hex(), OTHER_ADDRESS.hex()]
    sensors = get(run, "/api/status")["sensors"]
    assert [(s["id"], s["name"], s["count"]) for s in sensors] == [
        (ADDRESS.hex(), NAME, 1), (OTHER_ADDRESS.hex(), "DX0202", 1)]
    run.close()


def test_one_sensor_can_be_selected(tmp_path, signer, history):
    first_session(tmp_path, signer, history, glucose=50, clock=800000, age=0)
    run = pair_next_sensor(tmp_path, signer, history, NOW + 7200, glucose=99, clock=3600, age=0)
    chosen = f"sensor={ADDRESS.hex()}"
    assert [r["glucose"] for r in get(run, f"/api/readings?{chosen}")] == [50]
    assert [r["glucose"] for r in get(run, f"/api/readings?limit=1&{chosen}")] == [50]
    summary = get(run, f"/api/summary?since=0&until=4294967295&{chosen}")
    assert summary["below_seconds"] == 300 and summary["in_range_seconds"] == 0
    run.close()


def test_overlapping_sensors_stay_apart_and_in_time_order(tmp_path, signer, history):
    sensor = Sensor(CODE, signer.public_key(), glucose=120, clock=800600, age=0)
    sensor.history = [(800300, 118)]
    Run(sensor, tmp_path, signer, args=args(history)).connect()
    run = pair_next_sensor(tmp_path, signer, history, NOW + 150, glucose=90, clock=1000, age=0)
    run.sensor.history = [(700, 88)]
    run.session()
    rows = get(run, "/api/readings")
    assert [r["time"] for r in rows] == sorted(r["time"] for r in rows)
    assert {r["sensor"] for r in rows if r["glucose"] > 100} == {ADDRESS.hex()}
    assert {r["sensor"] for r in rows if r["glucose"] < 100} == {OTHER_ADDRESS.hex()}
    run.close()


def test_history_is_not_limited_to_a_week(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=1800300, age=0)
    sensor = bonded_sensor(tmp_path, clock=1800600, age=0)
    sensor.history = [(1800000 - i * 300, 100) for i in range(4000)]
    Run(sensor, tmp_path, last_seen=1, args=args(history)).connect()
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert get(run, "/api/status")["count"] == 4002
    run.close()


def test_uncommon_clocks_and_states_are_kept_exactly(tmp_path, signer, history):
    first_session(tmp_path, signer, history, clock=800000, age=0)
    sensor = bonded_sensor(tmp_path, clock=800997, age=0, state=0xC1)
    sensor.history = [(800301, 101), (800350 + 300, 102)]
    Run(sensor, tmp_path, args=args(history)).connect()
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    rows = get(run, "/api/readings")
    assert [r["clock"] for r in rows] == [800000, 800301, 800650, 800997]
    assert rows[-1]["state"] == 0xC1 and rows[-1]["glucose"] is None
    run.close()


def test_a_full_log_overwrites_only_the_oldest(tmp_path, signer, history):
    small = [*args(history), "--log-sectors", "3"]
    sensor = Sensor(CODE, signer.public_key(), clock=1800000, age=0)
    sensor.history = [(1800000 - i * 300, 100) for i in range(1499, 0, -1)]
    Run(sensor, tmp_path, signer, last_seen=1, args=small).connect()
    run = Run(Sensor(CODE), tmp_path, args=small)
    rows = get(run, "/api/readings")
    assert 900 <= len(rows) <= 1350, "two full sectors at least, three at most"
    assert rows[-1]["clock"] == 1800000 - 300
    assert {b["clock"] - a["clock"] for a, b in itertools.pairwise(rows)} == {300}
    run.close()


def test_sensors_taking_turns_do_not_waste_sectors(tmp_path, signer, history):
    log = history / "log.bin"
    for turn in range(40):
        address = ADDRESS if turn % 2 else OTHER_ADDRESS
        sensor = Sensor(CODE, signer.public_key(), clock=800000 + turn * 300, age=0, address=address)
        (tmp_path / "bond.key").unlink(missing_ok=True)
        Run(sensor, tmp_path, signer, last_seen=800000 + turn * 300, args=args(history)).connect()
    used = sum(log.read_bytes()[at:at + 4] == LOG_MAGIC for at in range(0, 4096 * 64, 4096))
    assert used == 2


def test_memory_keeps_the_newest_readings_when_full(tmp_path, signer, history):
    sensor = Sensor(CODE, signer.public_key(), clock=864000, age=0)
    sensor.history = [(864000 - i * 300, 100) for i in range(1, 300)]
    run = Run(sensor, tmp_path, signer, args=[*args(history), "--history-capacity", "100"]).session()
    rows = get(run, "/api/readings")
    assert get(run, "/api/status")["capacity"] == 100
    assert len(rows) == 100 and rows[-1]["clock"] == 864000
    run.close()


def test_a_damaged_record_loses_only_itself(tmp_path, signer, history):
    sensor = Sensor(CODE, signer.public_key(), clock=864000, age=0)
    sensor.history = [(864000 - i * 300, 100) for i in range(1, 6)]
    Run(sensor, tmp_path, signer, args=args(history)).connect()
    log = history / "log.bin"
    data = bytearray(log.read_bytes())
    data[LOG_HEADER + LOG_RECORD * 2] ^= 0xFF
    log.write_bytes(data)
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert len(get(run, "/api/readings")) == 5
    run.close()


def test_a_damaged_sector_header_loses_only_that_sector(tmp_path, signer, history):
    sensor = Sensor(CODE, signer.public_key(), clock=1800000, age=0)
    sensor.history = [(1800000 - i * 300, 100) for i in range(1, 800)]
    Run(sensor, tmp_path, signer, args=args(history)).connect()
    log = history / "log.bin"
    data = bytearray(log.read_bytes())
    data[8] ^= 0xFF
    log.write_bytes(data)
    run = Run(Sensor(CODE), tmp_path, args=args(history))
    assert 300 < len(get(run, "/api/readings")) < 800
    run.close()


def restore(backup, sensor, name, out):
    subprocess.run([sys.executable, str(RESTORE_TOOL), "--readings", str(backup), "--sensor",
                    sensor, "--name", name, "--out", str(out)], check=True)


def test_backup_restores_to_the_same_readings(tmp_path, signer, history):
    sensor = Sensor(CODE, signer.public_key(), clock=864000, age=0, glucose=143)
    sensor.history = [(864000 - i * 300, 100 + i) for i in range(1, 200)]
    run = Run(sensor, tmp_path, signer, args=args(history)).session()
    original = get(run, "/api/readings")
    run.close()
    backup = tmp_path / "readings.json"
    backup.write_text(json.dumps(original))
    restored = tmp_path / "restored"
    restored.mkdir()
    restore(backup, ADDRESS.hex(), NAME, restored / "log.bin")
    run = Run(Sensor(CODE), tmp_path, args=args(restored))
    assert get(run, "/api/readings") == original
    assert get(run, "/api/status")["sensors"][0]["name"] == NAME
    run.close()


def test_a_session_continues_restored_history(tmp_path, signer, history):
    rows = [{"clock": 21000 + i * 300, "time": NOW - 2000 + i * 300, "glucose": 90 + i, "state": 6,
             "trend": -3.9, "backfill": i == 1} for i in range(3)]
    backup = tmp_path / "readings.json"
    backup.write_text(json.dumps(rows))
    history.mkdir()
    restore(backup, ADDRESS.hex(), NAME, history / "log.bin")
    sensor = Sensor(CODE, signer.public_key(), clock=21900, age=0, glucose=93)
    Run(sensor, tmp_path, signer, args=args(history), now=NOW - 1100).connect()
    assert backfill_requests(sensor) == [(1, 20999)], "only what lies before the restored readings"
    run = Run(Sensor(CODE), tmp_path, args=args(history), now=NOW - 1100)
    assert [r["glucose"] for r in get(run, "/api/readings")] == [90, 91, 92, 93]
    run.close()
