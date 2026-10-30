"""choosing the mqtt broker from the web ui."""

import http.client
import json
import urllib.parse

import pytest
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CODE, Broker, Run
from sensor import Sensor

HTTP = ["--device", "kitchen", "--http", "127.0.0.1:0"]


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


@pytest.fixture
def broker():
    broker = Broker()
    yield broker
    broker.close()


def post(run, form, guard=True):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    headers = {"x-nanodrip": "1"} if guard else {}
    conn.request("POST", "/api/mqtt", urllib.parse.urlencode(form), headers)
    return conn.getresponse().status


def status(run):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", "/api/status")
    return json.loads(conn.getresponse().read())


def test_broker_can_be_set_from_the_web_ui(tmp_path, signer, broker):
    run = Run(Sensor(CODE, signer.public_key(), glucose=131), tmp_path, signer, args=HTTP)
    assert status(run)["mqtt_broker"] is None
    assert post(run, {"address": f"127.0.0.1:{broker.port}", "user": "drip",
                      "password": "s3cret"}) == 200
    assert status(run)["mqtt_broker"] == f"127.0.0.1:{broker.port}"
    run.session()
    assert [m[1]["glucose"] for m in broker.messages] == [131]
    assert broker.logins[0][1:] == (b"drip", b"s3cret")
    run.close()


def test_mqtt_password_is_never_reported(tmp_path, broker):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    post(run, {"address": f"127.0.0.1:{broker.port}", "user": "drip", "password": "s3cret"})
    text = json.dumps(status(run))
    assert "s3cret" not in text and "drip" not in text
    run.close()


def test_readings_stay_local_until_a_broker_is_set(tmp_path, signer):
    run = Run(Sensor(CODE, signer.public_key()), tmp_path, signer, args=HTTP).session()
    assert status(run)["mqtt_pending"] == 0
    run.close()


def test_empty_address_turns_mqtt_off(tmp_path, signer, broker):
    run = Run(Sensor(CODE, signer.public_key()), tmp_path, signer, args=HTTP)
    post(run, {"address": f"127.0.0.1:{broker.port}"})
    assert post(run, {"address": ""}) == 200
    assert status(run)["mqtt_broker"] is None
    run.session()
    assert broker.messages == []
    run.close()


@pytest.mark.parametrize("address", ["nohost", "host:", ":1883", "host:notaport", "host:99999",
                                     "bad host:1883"])
def test_invalid_broker_is_refused(tmp_path, address):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert post(run, {"address": address}) == 400
    assert status(run)["mqtt_broker"] is None
    run.close()


def test_changing_the_broker_needs_the_csrf_header(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert post(run, {"address": "broker.lan:1883"}, guard=False) == 403
    run.close()


def test_address_flag_still_works(tmp_path, signer, broker):
    args = [*HTTP, "--mqtt", f"127.0.0.1:{broker.port}"]
    run = Run(Sensor(CODE, signer.public_key()), tmp_path, signer, args=args)
    assert status(run)["mqtt_broker"] == f"127.0.0.1:{broker.port}"
    run.close()
