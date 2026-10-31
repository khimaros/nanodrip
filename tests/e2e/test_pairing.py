"""pairing a sensor from the web ui, with no reflash and no command line."""

import base64
import gzip
import http.client
import json
import struct
import subprocess
import sys
import urllib.parse

import pytest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec
from harness import CERT_A, CERT_B, CODE, ROOT, Run
from sensor import Sensor

HTTP = ["--http", "127.0.0.1:0"]
IMPORT_TOOL = ROOT / "tools" / "import_keys.py"


@pytest.fixture
def signer():
    return ec.generate_private_key(ec.SECP256R1())


def request(run, method, target, form=None, guard=True):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    headers = {"x-nanodrip": "1"} if guard else {}
    body = urllib.parse.urlencode(form) if form is not None else None
    conn.request(method, target, body, headers)
    response = conn.getresponse()
    return response.status, response.read()


def status(run):
    return json.loads(request(run, "GET", "/api/status")[1])


def private_key_der(signer):
    return signer.private_bytes(serialization.Encoding.DER, serialization.PrivateFormat.PKCS8,
                                serialization.NoEncryption())


def certificates(signer):
    der = private_key_der(signer)
    return {"p1": CERT_A.hex(), "p2": CERT_B.hex(), "p3": der.hex()}


def xdrip_qr_payload(signer):
    """builds what xdrip's DisplayQRCode produces for the keks settings."""
    settings = {"b__keks_p1": CERT_A, "b__keks_p2": CERT_B, "b__keks_p3": private_key_der(signer)}
    data = struct.pack(">H", len(settings))
    for name, value in settings.items():
        data += struct.pack(">H", len(name)) + name.encode()
        data += struct.pack(">H", len(value)) + value
    return "xdp2:" + base64.b64encode(gzip.compress(data)).decode().rstrip("=")


def run_import(run, payload):
    return subprocess.run(
        [sys.executable, str(IMPORT_TOOL), "--host", f"127.0.0.1:{run.http_port}",
         "--payload", payload],
        capture_output=True, text=True, timeout=20, check=False)


def test_hex_with_separators_and_capitals_is_accepted(tmp_path, signer):
    run = Run(Sensor(CODE), tmp_path, code=None, args=HTTP)
    form = {key: value.upper().replace("41", "41 ") for key, value in certificates(signer).items()}
    assert request(run, "POST", "/api/certificates", form)[0] == 200
    run.close()


def test_import_tool_loads_the_xdrip_qr_code(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=128)
    run = Run(sensor, tmp_path, code=None, args=HTTP)
    result = run_import(run, xdrip_qr_payload(signer))
    assert result.returncode == 0, result.stderr
    assert private_key_der(signer).hex() not in result.stdout + result.stderr
    assert request(run, "POST", "/api/pair", {"code": CODE})[0] == 200
    run.session()
    assert run.errors == []
    assert sensor.certs == [CERT_A, CERT_B]
    assert [r["glucose"] for r in run.readings] == [128]
    run.close()


def test_import_tool_writes_build_defaults(tmp_path, signer):
    out = tmp_path / "nanodrip-keys.env"
    result = subprocess.run(
        [sys.executable, str(IMPORT_TOOL), "--payload", xdrip_qr_payload(signer), "--out", str(out)],
        capture_output=True, text=True, timeout=20, check=False)
    assert result.returncode == 0, result.stderr
    assert out.stat().st_mode & 0o777 == 0o600, "the key must not be world readable"
    assert private_key_der(signer).hex() not in result.stdout + result.stderr
    values = dict(line.split("=", 1) for line in out.read_text().splitlines())
    assert values["NANODRIP_KEKS_P1"] == CERT_A.hex()
    assert values["NANODRIP_KEKS_P2"] == CERT_B.hex()
    assert values["NANODRIP_KEKS_P3"] == private_key_der(signer).hex()


def test_import_tool_refuses_other_qr_codes(tmp_path):
    run = Run(Sensor(CODE), tmp_path, code=None, args=HTTP)
    assert run_import(run, "xdpref:notkeys").returncode != 0
    assert status(run)["certificates"] is False
    run.close()


def web_paired_run(tmp_path, sensor, signer, **kwargs):
    run = Run(sensor, tmp_path, code=None, args=HTTP, **kwargs)
    assert request(run, "POST", "/api/certificates", certificates(signer))[0] == 200
    assert request(run, "POST", "/api/pair", {"code": CODE})[0] == 200
    return run


def test_unpaired_device_says_so(tmp_path):
    run = Run(Sensor(CODE), tmp_path, code=None, args=HTTP)
    assert status(run)["paired"] is False
    run.pump("connected")
    assert run.errors == ["unpaired"]
    assert status(run)["last_error"] == "unpaired"
    run.close()


def test_pairing_from_the_web_produces_readings(tmp_path, signer):
    sensor = Sensor(CODE, signer.public_key(), glucose=131)
    run = web_paired_run(tmp_path, sensor, signer).session()
    assert run.errors == []
    assert [r["glucose"] for r in run.readings] == [131]
    assert sensor.certs == [CERT_A, CERT_B]
    state = status(run)
    assert state["paired"] is True and state["certificates"] is True
    assert state["last_error"] is None
    run.close()


def test_status_never_reveals_secrets(tmp_path, signer):
    run = web_paired_run(tmp_path, Sensor(CODE, signer.public_key()), signer).session()
    text = json.dumps(status(run))
    assert CODE not in text
    assert certificates(signer)["p3"] not in text
    run.close()


def test_failed_pairing_is_visible(tmp_path, signer):
    sensor = Sensor("9191", signer.public_key())
    run = web_paired_run(tmp_path, sensor, signer).session()
    assert status(run)["last_error"] == "challenge"
    run.close()


@pytest.mark.parametrize("code", ["", "123", "12345", "1234567", "12 4", "12&4", "1234\n"])
def test_invalid_code_is_refused(tmp_path, code):
    run = Run(Sensor(CODE), tmp_path, code=None, args=HTTP)
    assert request(run, "POST", "/api/pair", {"code": code})[0] == 400
    assert status(run)["paired"] is False
    run.close()


def test_invalid_certificates_are_refused(tmp_path):
    run = Run(Sensor(CODE), tmp_path, code=None, args=HTTP)
    form = {"p1": "abcd", "p2": "abcd", "p3": "abcd"}
    assert request(run, "POST", "/api/certificates", form)[0] == 400
    assert status(run)["certificates"] is False
    run.close()


@pytest.mark.parametrize("target", ["/api/pair", "/api/forget", "/api/certificates"])
def test_changes_need_the_csrf_header(tmp_path, target):
    run = Run(Sensor(CODE), tmp_path, args=HTTP)
    assert request(run, "POST", target, {"code": "1111"}, guard=False)[0] == 403
    assert status(run)["paired"] is True
    run.close()


def test_new_code_replaces_key_and_keeps_history(tmp_path, signer):
    old = Sensor(CODE, signer.public_key(), glucose=140)
    run = web_paired_run(tmp_path, old, signer).session()
    assert (tmp_path / "bond.key").exists()
    assert request(run, "POST", "/api/pair", {"code": "5150"})[0] == 200
    new = Sensor("5150", signer.public_key(), glucose=95, clock=7200, age=10,
                 address=bytes.fromhex("c0ffee000002"))
    run.sensor = new
    run.session()
    assert run.errors == []
    assert new.client, "a new sensor needs a full j-pake exchange"
    rows = json.loads(request(run, "GET", "/api/readings")[1])
    assert [r["glucose"] for r in rows] == [140, 95]
    run.close()


def test_forget_returns_to_unpaired(tmp_path, signer):
    run = web_paired_run(tmp_path, Sensor(CODE, signer.public_key()), signer).session()
    assert request(run, "POST", "/api/forget", {})[0] == 200
    assert status(run)["paired"] is False
    run.pump("connected")
    assert run.errors == ["unpaired"]
    assert not (tmp_path / "bond.key").exists()
    run.close()
