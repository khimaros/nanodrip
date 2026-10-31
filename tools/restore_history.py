#!/usr/bin/env python3
"""turns readings saved from /api/readings into an image of the flash log, ready
to write to the log partition. written from the format in DESIGN.md rather than
from the firmware, so the two can disagree. one sensor per run."""

import argparse
import json
import pathlib
import struct

SECTOR_SIZE = 4096
HEADER_SIZE = 40
RECORD_SIZE = 9
RECORDS_PER_SECTOR = (SECTOR_SIZE - HEADER_SIZE) // RECORD_SIZE
MAGIC = b"NDLG"
VERSION = 1
HEADER_SEED = 0x4E444C47
CHECK_SEED = 0x5A
NAME_SIZE = 8
BACKFILL_FLAG = 1 << 15
NO_TREND = 127
TREND_SCALE = 10


def rotate_left(value, bits):
    return ((value << bits) | (value >> (32 - bits))) & 0xFFFFFFFF


def header(seq, sensor, name, base):
    body = bytearray(b"\xff" * HEADER_SIZE)
    body[:4] = MAGIC
    body[4] = VERSION
    body[8:12] = struct.pack("<I", seq)
    body[12:18] = sensor
    body[18:26] = name.encode("ascii").ljust(NAME_SIZE, b"\0")
    body[26:30] = struct.pack("<I", base)
    check = HEADER_SEED
    for byte in body[:HEADER_SIZE - 4]:
        check = rotate_left(check, 5) ^ byte
    body[HEADER_SIZE - 4:] = struct.pack("<I", check)
    return bytes(body)


def record(row):
    glucose = (row["glucose"] or 0) | (BACKFILL_FLAG if row["backfill"] else 0)
    trend = NO_TREND if row["trend"] is None else round(row["trend"] * TREND_SCALE)
    body = struct.pack("<IHBb", row["clock"], glucose, row["state"], trend)
    check = CHECK_SEED
    for byte in body:
        check ^= byte
    return body + bytes([check])


def image(rows, sensor, name):
    timed = next(row for row in rows if row["time"] is not None)
    base = timed["time"] - timed["clock"]
    out = b""
    for seq, at in enumerate(range(0, len(rows), RECORDS_PER_SECTOR), start=1):
        sector = b"".join(record(row) for row in rows[at:at + RECORDS_PER_SECTOR])
        out += (header(seq, sensor, name, base) + sector).ljust(SECTOR_SIZE, b"\xff")
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--readings", required=True, type=pathlib.Path)
    parser.add_argument("--sensor", required=True, help="BLE address as twelve hex digits")
    parser.add_argument("--name", required=True)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    args = parser.parse_args()
    rows = sorted(json.loads(args.readings.read_text()), key=lambda row: row["clock"])
    data = image(rows, bytes.fromhex(args.sensor), args.name)
    args.out.write_bytes(data)
    print(f"{len(rows)} readings in {len(data) // SECTOR_SIZE} sectors")


if __name__ == "__main__":
    main()
