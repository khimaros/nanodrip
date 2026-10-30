"""runs the nanodrip-sim binary against the emulated sensor and broker."""

import json
import pathlib
import socket
import struct
import subprocess
import threading

from cryptography.hazmat.primitives import serialization

ROOT = pathlib.Path(__file__).resolve().parents[2]
SIM = ROOT / "target" / "debug" / "nanodrip-sim"
CODE = "4271"
CERT_A, CERT_B = b"A" * 450, b"B" * 300
NOW = 1800000000


class Run:
    """one simulated ble connection, pumping both sides until they go quiet."""

    def __init__(self, sensor, tmp_path, signer=None, code=CODE, last_seen=None, args=()):
        self.sensor = sensor
        self.key_file = tmp_path / "bond.key"
        argv = [str(SIM), "--key-file", str(self.key_file), "--now", str(NOW)]
        if code:
            argv += ["--code", code]
        if signer:
            der = signer.private_bytes(serialization.Encoding.DER,
                                       serialization.PrivateFormat.PKCS8,
                                       serialization.NoEncryption())
            argv += ["--part-a", CERT_A.hex(), "--part-b", CERT_B.hex(), "--part-c", der.hex()]
        if last_seen is not None:
            argv += ["--last-seen", str(last_seen)]
        self.proc = subprocess.Popen([*argv, *args], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                     text=True, bufsize=1)
        self.readings, self.backfill, self.errors, self.bonds = [], [], [], 0
        self.disconnected = False
        self.http_port = None
        if "--http" in args:
            self.http_port = int(self.proc.stdout.readline().split()[1])

    def send(self, line):
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def replies(self):
        """reads client output up to the end-of-batch marker."""
        out = []
        while (line := self.proc.stdout.readline().strip()) != "ok":
            assert line, f"sim exited: {self.proc.stderr.read()}"
            out.append(line)
        return out

    def handle(self, line):
        kind, _, rest = line.partition(" ")
        if kind == "tx":
            channel, payload = rest.split()
            return getattr(self.sensor, f"on_{channel}")(bytes.fromhex(payload))
        if kind == "reading":
            self.readings.append(json.loads(rest))
        elif kind == "backfill":
            self.backfill.append(json.loads(rest))
        elif kind == "fail":
            self.errors.append(rest)
        elif kind == "bond":
            self.bonds += 1
            self.sensor.bonded = True
            return [("bonded", b"")]
        elif kind == "disconnect":
            self.disconnected = True
        return []

    def pump(self, command):
        queue = [command]
        while queue:
            self.send(queue.pop(0))
            for line in self.replies():
                for channel, data in self.handle(line):
                    queue.append("bonded" if channel == "bonded" else f"rx {channel} {data.hex()}")
        return self

    def session(self):
        return self.pump("connected").pump("idle")

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)
        return self

    def connect(self):
        return self.session().close()


def read_exact(conn, size):
    data = b""
    while len(data) < size:
        chunk = conn.recv(size - len(data))
        if not chunk:
            raise ConnectionError
        data += chunk
    return data


def read_packet(conn):
    header = read_exact(conn, 1)[0]
    size, shift = 0, 0
    while True:
        byte = read_exact(conn, 1)[0]
        size |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            return header, read_exact(conn, size)


def mqtt_string(data, at):
    size = struct.unpack(">H", data[at:at + 2])[0]
    return data[at + 2:at + 2 + size], at + 2 + size


class Broker:
    """just enough of an mqtt 3.1.1 broker to record what a client publishes."""

    def __init__(self, port=0):
        self.listener = socket.create_server(("127.0.0.1", port))
        self.port = self.listener.getsockname()[1]
        self.messages, self.logins = [], []
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self.client, args=(conn,), daemon=True).start()

    def client(self, conn):
        with conn:
            try:
                while self.on_packet(conn, *read_packet(conn)):
                    pass
            except ConnectionError:
                pass

    def on_packet(self, conn, header, body):
        kind = header >> 4
        if kind == 1:
            self.on_connect(body)
            conn.sendall(b"\x20\x02\x00\x00")
        elif kind == 3:
            qos, retain = (header >> 1) & 3, bool(header & 1)
            topic, at = mqtt_string(body, 0)
            assert qos == 1, "feed must publish at qos 1"
            packet_id = body[at:at + 2]
            self.messages.append((topic.decode(), json.loads(body[at + 2:]), retain))
            conn.sendall(b"\x40\x02" + packet_id)
        return kind != 14

    def on_connect(self, body):
        name, at = mqtt_string(body, 0)
        assert name == b"MQTT" and body[at] == 4, "mqtt 3.1.1 connect"
        flags = body[at + 1]
        at += 4
        client_id, at = mqtt_string(body, at)
        user, password = None, None
        if flags & 0x80:
            user, at = mqtt_string(body, at)
        if flags & 0x40:
            password, at = mqtt_string(body, at)
        self.logins.append((client_id, user, password))

    def close(self):
        self.listener.close()
