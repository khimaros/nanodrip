"""the target range, and time spent outside it over a chosen interval."""

import http.client
import json
import urllib.parse

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CODE, Run
from sensor import Sensor

HTTP = ["--http", "127.0.0.1:0"]
HOUR = 3600
CLOCK = 864000


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


def get(run, target):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", target)
    return json.loads(conn.getresponse().read())


def set_target(run, low, high, guard=True):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    headers = {"x-nanodrip": "1"} if guard else {}
    conn.request("POST", "/api/target", urllib.parse.urlencode({"low": low, "high": high}), headers)
    return conn.getresponse().status


def run_with_history(tmp_path, signer, rows, **sensor_args):
    Run(Sensor(CODE, signer.public_key()), tmp_path, signer).connect()
    sensor = Sensor(CODE, bonded=True, clock=CLOCK, age=0, **sensor_args)
    sensor.key = bytes.fromhex((tmp_path / "bond.key").read_text())
    sensor.history = rows
    return Run(sensor, tmp_path, last_seen=1, args=HTTP).session()


def series(*values):
    """one reading every five minutes, ending just before the live one."""
    start = CLOCK - len(values) * 300
    return [(start + i * 300, value) for i, value in enumerate(values)]


def test_default_target_range(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    status = get(run, "/api/status")
    assert (status["target_low"], status["target_high"]) == (63, 140)
    run.close()


def test_target_range_can_be_changed(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert set_target(run, 80, 140) == 200
    status = get(run, "/api/status")
    assert (status["target_low"], status["target_high"]) == (80, 140)
    run.close()


@pytest.mark.parametrize("low,high", [
    (140, 80), (100, 100), (39, 180), (70, 401), ("", ""), ("abc", 180),
])
def test_invalid_target_range_is_refused(tmp_path, low, high):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert set_target(run, low, high) == 400
    assert get(run, "/api/status")["target_high"] == 140
    run.close()


def test_changing_the_target_needs_the_csrf_header(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert set_target(run, 80, 140, guard=False) == 403
    run.close()


def test_time_above_and_below(tmp_path, signer):
    rows = series(60, 60, 100, 100, 100, 200)
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    summary = get(run, "/api/summary?hours=24")
    assert summary["below_seconds"] == 600
    assert summary["above_seconds"] == 300
    assert summary["in_range_seconds"] == 1200
    run.close()


def test_range_edges_count_as_in_range(tmp_path, signer):
    run = run_with_history(tmp_path, signer, series(63, 140), glucose=100)
    summary = get(run, "/api/summary?hours=24")
    assert summary["below_seconds"] == 0
    assert summary["above_seconds"] == 0
    run.close()


def test_totals_follow_the_target(tmp_path, signer):
    run = run_with_history(tmp_path, signer, series(100, 100), glucose=100)
    assert set_target(run, 110, 180) == 200
    assert get(run, "/api/summary?hours=24")["below_seconds"] == 900
    run.close()


def test_totals_follow_the_selected_interval(tmp_path, signer):
    rows = [(CLOCK - 10 * HOUR + i * 300, 200) for i in range(12)]
    rows += [(CLOCK - 2 * HOUR + i * 300, 60) for i in range(24)]
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    narrow = get(run, "/api/summary?hours=3")
    wide = get(run, "/api/summary?hours=24")
    assert narrow["hours"] == 3
    assert narrow["above_seconds"] == 0
    assert narrow["below_seconds"] == 2 * HOUR
    assert wide["above_seconds"] > HOUR
    run.close()


def test_outages_are_not_counted(tmp_path, signer):
    rows = [(CLOCK - 5 * HOUR, 200), (CLOCK - 300, 100)]
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    assert get(run, "/api/summary?hours=24")["above_seconds"] == 900
    run.close()


def test_warmup_readings_are_not_counted(tmp_path, signer):
    run = run_with_history(tmp_path, signer, [], glucose=100, state=2)
    summary = get(run, "/api/summary?hours=24")
    assert summary["in_range_seconds"] == 0
    run.close()


def test_summary_of_all_history_reaches_past_the_longest_range(tmp_path, signer):
    old = CLOCK - 200 * HOUR
    run = run_with_history(tmp_path, signer, [(old, 50), (old + 300, 50), *series(120)])
    everything = get(run, "/api/summary?since=0&until=4294967295")
    assert get(run, "/api/summary?hours=168")["below_seconds"] == 0
    assert everything["below_seconds"] == 300 + 900, "the last old reading holds to the span limit"
    run.close()


def test_summary_with_no_readings(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    summary = get(run, "/api/summary?hours=3")
    assert (summary["above_seconds"], summary["below_seconds"]) == (0, 0)
    assert summary["mean"] is None
    run.close()


def test_mean_is_weighted_by_time(tmp_path, signer):
    rows = series(60, 60, 100, 100, 100, 200)
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    assert get(run, "/api/summary?hours=24")["mean"] == pytest.approx(102.9, abs=0.05)
    run.close()


def test_mean_is_not_skewed_by_an_outage(tmp_path, signer):
    rows = [(CLOCK - 5 * HOUR, 300), (CLOCK - 300, 100)]
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    assert get(run, "/api/summary?hours=24")["mean"] == pytest.approx(220, abs=0.05)
    run.close()


def test_an_exact_range_can_be_selected(tmp_path, signer):
    rows = series(60, 60, 100, 100, 200, 200)
    run = run_with_history(tmp_path, signer, rows, glucose=100)
    start, end = CLOCK - 1800, CLOCK - 900
    summary = get(run, f"/api/summary?from={start}&to={end}")
    assert summary["below_seconds"] == 600
    assert summary["in_range_seconds"] == 300
    assert summary["above_seconds"] == 0
    assert summary["mean"] == pytest.approx(73.3, abs=0.05)
    run.close()


def test_a_reading_is_cut_off_at_the_end_of_the_range(tmp_path, signer):
    run = run_with_history(tmp_path, signer, series(60, 60), glucose=100)
    summary = get(run, f"/api/summary?from={CLOCK - 600}&to={CLOCK - 500}")
    assert summary["below_seconds"] == 100
    run.close()


def test_a_backwards_range_is_empty(tmp_path, signer):
    run = run_with_history(tmp_path, signer, series(60, 60), glucose=100)
    summary = get(run, f"/api/summary?from={CLOCK}&to={CLOCK - 600}")
    assert summary["below_seconds"] == 0
    assert summary["mean"] is None
    run.close()
