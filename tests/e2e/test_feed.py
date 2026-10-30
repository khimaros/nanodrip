"""what leaves the device: the mqtt feed and the web ui."""

import http.client
import json
import socket

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CODE, NOW, Broker, Run
from sensor import Sensor

GLUCOSE_TOPIC = "nanodrip/kitchen/glucose"
BACKFILL_TOPIC = "nanodrip/kitchen/backfill"
SVG_NAMESPACE = "http://www.w3.org/2000/svg"


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


@pytest.fixture
def broker():
    broker = Broker()
    yield broker
    broker.close()


def mqtt_args(port):
    return ["--device", "kitchen", "--mqtt", f"127.0.0.1:{port}"]


def web(run, target, method="GET"):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request(method, target)
    response = conn.getresponse()
    return response.status, response.getheader("content-type"), response.read()


def web_json(run, target):
    status, _, body = web(run, target)
    assert status == 200
    return json.loads(body)


def test_reading_is_published_retained(tmp_path, signer, broker):
    sensor = Sensor(CODE, signer.public_key(), glucose=143, age=30)
    Run(sensor, tmp_path, signer, args=mqtt_args(broker.port)).connect()
    assert broker.messages == [(GLUCOSE_TOPIC, {"clock": 863970, "time": NOW - 30, "glucose": 143,
                                                "state": 6, "trend": 1.1, "backfill": False},
                                True)]


def test_credentials_are_sent(tmp_path, signer, broker):
    args = [*mqtt_args(broker.port), "--mqtt-user", "drip", "--mqtt-pass", "s3cret"]
    Run(Sensor(CODE, signer.public_key()), tmp_path, signer, args=args).connect()
    assert broker.logins[0] == (b"nanodrip-kitchen", b"drip", b"s3cret")


def test_warmup_is_published_with_null_glucose(tmp_path, signer, broker):
    sensor = Sensor(CODE, signer.public_key(), state=2)
    Run(sensor, tmp_path, signer, args=mqtt_args(broker.port)).connect()
    assert [m[1]["glucose"] for m in broker.messages] == [None]


def test_backfill_is_published_unretained(tmp_path, signer, broker):
    Run(Sensor(CODE, signer.public_key()), tmp_path, signer).connect()
    sensor = Sensor(CODE, bonded=True)
    sensor.key = bytes.fromhex((tmp_path / "bond.key").read_text())
    sensor.history = [(863000, 101), (863300, 102)]
    Run(sensor, tmp_path, last_seen=862900, args=mqtt_args(broker.port)).connect()
    backfill = [m for m in broker.messages if m[0] == BACKFILL_TOPIC]
    assert [(m[1]["glucose"], m[1]["backfill"], m[2]) for m in backfill] == [
        (101, True, False), (102, True, False)]
    assert [m[2] for m in broker.messages if m[0] == GLUCOSE_TOPIC] == [True]


def test_broker_outage_delivers_after_recovery(tmp_path, signer):
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    run = Run(Sensor(CODE, signer.public_key(), glucose=155), tmp_path, signer,
              args=[*mqtt_args(port), "--http", "127.0.0.1:0"]).session()
    assert run.errors == []
    assert web_json(run, "/api/status")["mqtt_pending"] == 1
    broker = Broker(port)
    run.pump("flush")
    assert [m[1]["glucose"] for m in broker.messages] == [155]
    assert web_json(run, "/api/status")["mqtt_pending"] == 0
    run.close()
    broker.close()


def test_status_api(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=143)
    run = Run(sensor, tmp_path, signer, args=["--device", "kitchen", "--http", "127.0.0.1:0"])
    assert web_json(run, "/api/status")["latest"] is None
    run.session()
    status = web_json(run, "/api/status")
    assert status["device"] == "kitchen"
    assert status["latest"]["glucose"] == 143
    assert status["count"] == 1
    run.close()


def backfilled_run(tmp_path, signer):
    Run(Sensor(CODE, signer.public_key()), tmp_path, signer).connect()
    sensor = Sensor(CODE, bonded=True, glucose=110, clock=864000, age=30)
    sensor.key = bytes.fromhex((tmp_path / "bond.key").read_text())
    sensor.history = [(863370, 104), (863070, 101), (863670, 107), (863970, 109)]
    return Run(sensor, tmp_path, last_seen=862900, args=["--http", "127.0.0.1:0"]).session()


def test_readings_are_ordered_and_deduplicated(tmp_path, signer):
    run = backfilled_run(tmp_path, signer)
    rows = web_json(run, "/api/readings")
    assert [r["clock"] for r in rows] == [863070, 863370, 863670, 863970]
    assert rows[-1]["glucose"] == 110, "the live reading wins over its backfill twin"
    run.close()


def test_readings_limit_keeps_newest(tmp_path, signer):
    run = backfilled_run(tmp_path, signer)
    assert [r["glucose"] for r in web_json(run, "/api/readings?limit=2")] == [107, 110]
    run.close()


def test_index_is_self_contained(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=["--http", "127.0.0.1:0"])
    status, content_type, body = web(run, "/")
    page = body.decode("ascii").replace(SVG_NAMESPACE, "")
    assert status == 200 and content_type.startswith("text/html")
    assert "nanodrip" in page and "/api/readings" in page
    assert "http://" not in page and "https://" not in page, "the page must work offline"
    run.close()


def test_idle_connections_do_not_block_requests(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=["--http", "127.0.0.1:0"])
    idle = [socket.create_connection(("127.0.0.1", run.http_port)) for _ in range(3)]
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=1)
    conn.request("GET", "/api/status")
    assert conn.getresponse().status == 200
    for sock in idle:
        sock.close()
    run.close()


def test_web_errors(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=["--http", "127.0.0.1:0"])
    assert web(run, "/nope")[0] == 404
    assert web(run, "/", "POST")[0] == 405
    run.close()
