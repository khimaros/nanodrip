#!/usr/bin/env python3
"""copies the keks certificates from xdrip's setup qr code into a nanodrip.

the qr code holds "xdp2:" plus unpadded base64 of a gzipped list of settings.
the key is checked here and never printed. it goes to a running device, to a
gitignored file of firmware build defaults, or both.

    tools/import_keys.py --host 10.1.150.194
    tools/import_keys.py --out nanodrip-keys.env
"""

import argparse
import base64
import gzip
import http.client
import os
import struct
import sys
import tempfile
import urllib.parse
import urllib.request

from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.serialization import load_der_private_key

QR_URL = "https://navid200.github.io/xDrip/docs/Dexcom/images/G7_keks_QR.png"
MARKER = "xdp2:"
BINARY_PREFIX = "b__"
PARTS = ("keks_p1", "keks_p2", "keks_p3")
TIMEOUT_SECONDS = 15


def read_qr_image(path):
    """imported late because only image input needs opencv."""
    import cv2

    text, _, _ = cv2.QRCodeDetector().detectAndDecode(cv2.imread(path))
    if not text:
        sys.exit(f"no qr code found in {path}")
    return text


def fetch_payload():
    with tempfile.NamedTemporaryFile(suffix=".png") as image:
        with urllib.request.urlopen(QR_URL, timeout=TIMEOUT_SECONDS) as response:
            image.write(response.read())
        image.flush()
        return read_qr_image(image.name)


def decode_settings(payload):
    """returns the settings map; binary values stay as bytes."""
    if not payload.startswith(MARKER):
        sys.exit("not an xdrip binary settings qr code")
    packed = payload.removeprefix(MARKER)
    data = gzip.decompress(base64.b64decode(packed + "=" * (-len(packed) % 4)))
    count = struct.unpack_from(">H", data)[0]
    settings, at = {}, 2
    for _ in range(count):
        size = struct.unpack_from(">H", data, at)[0]
        name = data[at + 2:at + 2 + size].decode()
        at += 2 + size
        size = struct.unpack_from(">H", data, at)[0]
        settings[name.removeprefix(BINARY_PREFIX)] = data[at + 2:at + 2 + size]
        at += 2 + size
    return settings


def check_key(der):
    key = load_der_private_key(der, password=None)
    if not isinstance(key, ec.EllipticCurvePrivateKey) or key.curve.name != "secp256r1":
        sys.exit("keks_p3 is not a p-256 private key")


def upload(host, settings):
    form = {f"p{i}": settings[name].hex() for i, name in enumerate(PARTS, 1)}
    conn = http.client.HTTPConnection(host, timeout=TIMEOUT_SECONDS)
    conn.request("POST", "/api/certificates", urllib.parse.urlencode(form), {"x-nanodrip": "1"})
    status = conn.getresponse().status
    if status != 200:
        sys.exit(f"device refused the certificates: http {status}")


def write_defaults(path, settings):
    """writes the file make feeds to the firmware build, readable by us alone."""
    lines = [f"NANODRIP_KEKS_P{i}={settings[name].hex()}\n" for i, name in enumerate(PARTS, 1)]
    with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as out:
        out.writelines(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", help="device to send the keys to, as host or host:port")
    parser.add_argument("--out", help="file of build defaults to write")
    parser.add_argument("--image", help="qr code image, instead of fetching xdrip's")
    parser.add_argument("--payload", help="decoded qr code text, instead of an image")
    args = parser.parse_args()
    payload = args.payload or (read_qr_image(args.image) if args.image else fetch_payload())
    settings = decode_settings(payload)
    missing = [name for name in PARTS if name not in settings]
    if missing:
        sys.exit(f"qr code lacks {', '.join(missing)}")
    check_key(settings["keks_p3"])
    print("found", ", ".join(f"{name} ({len(settings[name])} bytes)" for name in PARTS))
    if args.out:
        write_defaults(args.out, settings)
        print(f"wrote {args.out}")
    if args.host:
        upload(args.host, settings)
        print("saved to device")


if __name__ == "__main__":
    main()
