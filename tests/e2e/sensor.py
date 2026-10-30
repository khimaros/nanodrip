"""dexcom g7/stelo sensor emulator, written from the wire format rather than
from nanodrip-core so that the two implementations can disagree."""

import hashlib
import secrets
import struct

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import encode_dss_signature
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

P = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
A = P - 3
G = (
    0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
    0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5,
)
PACKET_SIZE = 160
CERT_SIZE = 100
CERT_EARLY = 80
CLIENT = b"client"
SERVER = b"server"


def add(p, q):
    if p is None:
        return q
    if q is None:
        return p
    if p[0] == q[0]:
        if (p[1] + q[1]) % P == 0:
            return None
        s = (3 * p[0] * p[0] + A) * pow(2 * p[1], -1, P) % P
    else:
        s = (q[1] - p[1]) * pow(q[0] - p[0], -1, P) % P
    x = (s * s - p[0] - q[0]) % P
    return x, (s * (p[0] - x) - p[1]) % P


def mul(k, p):
    r = None
    while k:
        if k & 1:
            r = add(r, p)
        p = add(p, p)
        k >>= 1
    return r


def neg(p):
    return p[0], P - p[1]


def scalar():
    return secrets.randbelow(N - 1) + 1


def encode(p):
    return p[0].to_bytes(32, "big") + p[1].to_bytes(32, "big")


def decode(b):
    return int.from_bytes(b[:32], "big"), int.from_bytes(b[32:64], "big")


def zkp_hash(g, gv, gx, party):
    h = hashlib.sha256()
    for item in (g, gv, gx):
        h.update(struct.pack(">I", 65) + b"\x04" + encode(item))
    h.update(struct.pack(">I", len(party)) + party)
    return int.from_bytes(h.digest(), "big") % N


def prove(g, x, party):
    public, v = mul(x, g), scalar()
    gv = mul(v, g)
    r = (v - zkp_hash(g, gv, public, party) * x) % N
    return encode(public) + encode(gv) + r.to_bytes(32, "big")


def verify(g, packet, party):
    public, gv = decode(packet), decode(packet[64:])
    r = int.from_bytes(packet[128:160], "big")
    return add(mul(r, g), mul(zkp_hash(g, gv, public, party), public)) == gv


def aes_hash(key, data):
    enc = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return (enc.update(data + data) + enc.finalize())[:8]


def glucose_packet(clock, sequence, age, glucose, state, trend=0x0B):
    return (
        struct.pack("<BBIHHHHBBHB", 0x4E, 0, clock, sequence, 0, age, glucose, state,
                    trend & 0xFF, 0x3FF, 0)
    )


class Sensor:
    """answers client writes the way a sensor does: every hook returns the
    (channel, bytes) notifications it would emit in response."""

    def __init__(self, code, signer_public=None, bonded=False, glucose=120, state=6,
                 clock=864000, age=30):
        self.secret = int.from_bytes(code.encode(), "big")
        self.signer_public = signer_public
        self.bonded = bonded
        self.glucose, self.state, self.clock, self.age = glucose, state, clock, age
        self.key = None
        self.x3, self.x4 = scalar(), scalar()
        self.client = []
        self.extra = b""
        self.challenge = secrets.token_bytes(8)
        self.sign_challenge = secrets.token_bytes(16)
        self.cert_len = 0
        self.certs = []
        self.corrupt_round = None
        self.corrupt_hash = False
        self.history = []
        self.commands = []

    def server_packet(self, n):
        if n < 2:
            packet = prove(G, self.x3 if n == 0 else self.x4, SERVER)
        else:
            x1, x2 = self.client
            g = add(add(x1, x2), mul(self.x3, G))
            packet = prove(g, self.x4 * self.secret % N, SERVER)
        if self.corrupt_round == n:
            packet = packet[:-1] + bytes([packet[-1] ^ 1])
        return packet

    def take_client_packet(self):
        packet, self.extra = self.extra, b""
        assert len(packet) == PACKET_SIZE, f"client packet size {len(packet)}"
        return packet

    def on_round(self, n):
        if n > 0:
            packet = self.take_client_packet()
            assert verify(G, packet, CLIENT), f"client round {n} proof rejected"
            self.client.append(decode(packet))
        return [("extra", self.server_packet(n))]

    def finish_exchange(self):
        packet = self.take_client_packet()
        x1, x2 = self.client
        g = add(add(x1, mul(self.x3, G)), mul(self.x4, G))
        assert verify(g, packet, CLIENT), "client round 3 proof rejected"
        k = mul(self.x4, add(decode(packet), neg(mul(self.x4 * self.secret % N, x2))))
        self.key = hashlib.sha256(k[0].to_bytes(32, "big")).digest()[:16]

    def on_auth(self, data):
        self.commands.append(("auth", data))
        op = data[0]
        if op == 0x0A:
            return self.on_round(data[1])
        if op == 0x02:
            assert len(data) == 10, "auth request size"
            if self.key is None:
                self.finish_exchange()
            proof = bytes(8) if self.corrupt_hash else aes_hash(self.key, data[1:9])
            return [("auth", b"\x03" + proof + self.challenge)]
        if op == 0x04:
            authed = data[1:9] == aes_hash(self.key, self.challenge)
            return [("auth", bytes([5, 1 if authed else 2, 1 if self.bonded else 2]))]
        if op == 0x0B:
            self.cert_len = struct.unpack("<I", data[2:6])[0]
            # a real stelo starts its certificate ahead of this reply
            return [("extra", bytes(CERT_EARLY)),
                    ("auth", bytes([0x0B, 0, data[1]]) + struct.pack("<I", CERT_SIZE))]
        if op == 0x0C:
            assert len(data) == 17, "sign challenge size"
            return [("auth", b"\x0c\x00" + self.sign_challenge), ("extra", bytes(64))]
        if op == 0x0D:
            assert data == b"\x0d\x00\x02", "challenge out command"
            r, s = self.extra[:32], self.extra[32:]
            self.extra = b""
            der = encode_dss_signature(int.from_bytes(r, "big"), int.from_bytes(s, "big"))
            self.signer_public.verify(der, self.sign_challenge, ec.ECDSA(hashes.SHA256()))
            return [("auth", b"\x0d\x00")]
        if op == 0x06:
            return [("auth", data)]
        raise AssertionError(f"unknown auth opcode {op:#x}")

    def on_extra(self, data):
        self.extra += data
        if self.cert_len and len(self.extra) >= self.cert_len:
            self.certs.append(self.extra)
            self.extra, self.cert_len = b"", 0
            return [("extra", bytes(CERT_SIZE - CERT_EARLY))]
        return []

    def on_control(self, data):
        self.commands.append(("control", data))
        if data[0] == 0x4E:
            assert self.bonded, "glucose requested on an unbonded link"
            return [("control", glucose_packet(self.clock, 7, self.age, self.glucose, self.state))]
        if data[0] == 0x59:
            start, end = struct.unpack("<II", data[1:9])
            rows = [r for r in self.history if start <= r[0] <= end]
            out = [("control", b"\x59\x00")]
            for clock, glucose in rows:
                out.append(("backfill", struct.pack("<IHBBB", clock, glucose, 6, 0, 0)))
            return out
        if data[0] == 0x09:
            return []
        raise AssertionError(f"unknown control opcode {data[0]:#x}")
