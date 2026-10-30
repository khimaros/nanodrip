"""changing the wifi network from the web ui."""

import http.client
import json
import urllib.parse

import pytest
from harness import CODE, Run
from sensor import Sensor

HTTP = ["--http", "127.0.0.1:0"]


def post(run, form, guard=True):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    headers = {"x-nanodrip": "1"} if guard else {}
    conn.request("POST", "/api/wifi", urllib.parse.urlencode(form), headers)
    return conn.getresponse().status


def status(run):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", "/api/status")
    return json.loads(conn.getresponse().read())


def test_network_can_be_changed(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert status(run)["wifi_ssid"] is None
    assert post(run, {"ssid": "Flax", "password": "correct horse"}) == 200
    assert status(run)["wifi_ssid"] == "Flax"
    run.close()


def test_password_is_never_reported(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    post(run, {"ssid": "Flax", "password": "correct horse"})
    assert "horse" not in json.dumps(status(run))
    run.close()


def test_names_with_symbols_survive(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    name = 'cafe "wifi" & <guest>=1+1'
    assert post(run, {"ssid": name, "password": "p&ss=w+rd%20"}) == 200
    assert status(run)["wifi_ssid"] == name
    run.close()


def test_open_network_has_no_password(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert post(run, {"ssid": "cafe", "password": ""}) == 200
    run.close()


@pytest.mark.parametrize("form", [
    {"ssid": "", "password": "longenough"},
    {"ssid": "x" * 33, "password": "longenough"},
    {"ssid": "Flax", "password": "short"},
    {"ssid": "Flax", "password": "x" * 64},
])
def test_invalid_network_is_refused(tmp_path, form):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert post(run, form) == 400
    assert status(run)["wifi_ssid"] is None
    run.close()


def test_changing_wifi_keeps_the_sensor(tmp_path):
    sensor = Sensor(CODE, bonded=True, glucose=120)
    sensor.key = bytes(range(16))
    (tmp_path / "bond.key").write_text(sensor.key.hex())
    run = Run(sensor, tmp_path, args=HTTP).session()
    assert post(run, {"ssid": "Flax", "password": "longenough"}) == 200
    run.session()
    assert (tmp_path / "bond.key").exists()
    assert sensor.client == []
    assert status(run)["count"] == 1
    run.close()


def test_changes_need_the_csrf_header(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert post(run, {"ssid": "Flax", "password": "longenough"}, guard=False) == 403
    run.close()
