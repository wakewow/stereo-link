#!/usr/bin/env python3
"""scream-udp-capture.py - receive and decode the Scream virtual-sound-card UDP stream.

Scream (duncanthrax/scream, MS-PL) pushes the audio captured by its kernel driver
straight onto the LAN as raw PCM inside UDP datagrams:

    default destination : 239.255.77.77:4010  (multicast, IGMP handled by the driver)
    datagram layout     : 5-byte header + at most 1152 bytes of interleaved PCM
    header byte 0       : sample rate - bit 7 selects the base (0 = 48 kHz, 1 = 44.1 kHz),
                          bits 0..6 are the multiplier
    header byte 1       : bits per sample
    header byte 2       : channel count
    header bytes 3..4   : channel mask, little endian (WAVEFORMATEXTENSIBLE dwChannelMask)

Usage:
    python scream-udp-capture.py --seconds 5
    python scream-udp-capture.py --seconds 5 --expect-hz 1000 --json capture.json
    python scream-udp-capture.py --iface 192.168.1.4 --group 239.255.77.77 --port 4010

Exit codes: 0 = at least one datagram decoded, 2 = nothing arrived (timeout).

No third-party dependencies on purpose: this has to run under the stock project Python.
"""

from __future__ import annotations

import argparse
import array
import json
import math
import socket
import struct
import sys
import time

HEADER_LEN = 5


def decode_header(h: bytes) -> dict:
    """Decode Scream's 5-byte datagram header."""
    if len(h) < HEADER_LEN:
        raise ValueError("short header: %d bytes" % len(h))
    base = 44100 if (h[0] & 0x80) else 48000
    mult = h[0] & 0x7F
    if mult == 0:
        mult = 1
    return {
        "sample_rate": base * mult,
        "bits": h[1],
        "channels": h[2],
        "channel_mask": h[3] | (h[4] << 8),
    }


def to_samples(payload: bytes, bits: int):
    """Interleaved little-endian PCM -> flat list of ints (channel-interleaved)."""
    if bits == 16:
        a = array.array("h")
        a.frombytes(payload[: len(payload) // 2 * 2])
        if sys.byteorder == "big":
            a.byteswap()
        return a
    if bits == 32:
        a = array.array("i")
        a.frombytes(payload[: len(payload) // 4 * 4])
        if sys.byteorder == "big":
            a.byteswap()
        return a
    if bits == 8:
        return array.array("b", payload)
    raise ValueError("unsupported bit depth %d" % bits)


def peak_of(samples) -> int:
    return max((abs(int(s)) for s in samples), default=0)


def rms_of(samples) -> float:
    if len(samples) == 0:
        return 0.0
    return math.sqrt(sum(float(s) * float(s) for s in samples) / len(samples))


def zero_cross_hz(samples, channels: int, sample_rate: int) -> float:
    """Rough frequency estimate of channel 0 from upward zero crossings."""
    ch0 = samples[0::channels]
    n = len(ch0)
    if n < 2:
        return 0.0
    crossings = 0
    prev = int(ch0[0])
    for s in ch0[1:]:
        s = int(s)
        if prev <= 0 < s:
            crossings += 1
        prev = s
    seconds = n / float(sample_rate)
    if seconds <= 0:
        return 0.0
    return crossings / seconds


def open_receiver(group: str, port: int, iface: str, timeout: float):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("", port))
    except OSError as exc:
        raise SystemExit("cannot bind UDP %d: %s" % (port, exc))
    mreq = socket.inet_aton(group) + socket.inet_aton(iface)
    try:
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
    except OSError as exc:
        raise SystemExit("cannot join %s on %s: %s" % (group, iface, exc))
    sock.settimeout(timeout)
    return sock


def capture(args) -> dict:
    sock = open_receiver(args.group, args.port, args.iface, args.timeout)
    started = time.time()
    deadline = started + args.seconds
    packets = 0
    payload_bytes = 0
    hdr = None
    all_samples = None
    peak = 0
    per_packet_peak = 0
    first_datagram = b""
    while time.time() < deadline:
        try:
            data, addr = sock.recvfrom(65535)
        except socket.timeout:
            continue
        if len(data) <= HEADER_LEN:
            continue
        packets += 1
        if not first_datagram:
            first_datagram = data[:64]
        if hdr is None:
            hdr = decode_header(data[:HEADER_LEN])
            if args.verbose:
                print("first datagram from %s:%d  header=%r  -> %s" % (addr[0], addr[1], data[:HEADER_LEN], hdr))
                print("header hex: " + data[:HEADER_LEN].hex(" "))
        payload = data[HEADER_LEN:]
        payload_bytes += len(payload)
        try:
            samples = to_samples(payload, hdr["bits"])
        except ValueError:
            continue
        p = peak_of(samples)
        peak = max(peak, p)
        per_packet_peak = max(per_packet_peak, p)
        all_samples = samples if all_samples is None else all_samples + samples

    result = {
        "group": args.group,
        "port": args.port,
        "iface": args.iface,
        "seconds": round(time.time() - started, 3),
        "packets": packets,
        "payload_bytes": payload_bytes,
        "bits": None if hdr is None else hdr["bits"],
        "channels": None if hdr is None else hdr["channels"],
        "sample_rate": None if hdr is None else hdr["sample_rate"],
        "channel_mask": None if hdr is None else hdr["channel_mask"],
        "peak": peak,
        "peak_dbfs": (round(20 * math.log10(peak / 32768.0), 2) if peak > 0 and hdr and hdr["bits"] == 16 else None),
        "rms": round(rms_of(all_samples), 1) if all_samples is not None else None,
        "measured_hz": (round(zero_cross_hz(all_samples, hdr["channels"], hdr["sample_rate"]), 1)
                        if all_samples is not None and hdr and hdr["channels"] else None),
        "first_datagram_hex": first_datagram.hex(" "),
    }
    if args.expect_hz and result["measured_hz"]:
        result["expect_hz"] = args.expect_hz
        result["expect_hz_error_pct"] = round(100.0 * abs(result["measured_hz"] - args.expect_hz) / args.expect_hz, 2)
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description="capture the Scream UDP PCM stream")
    ap.add_argument("--group", default="239.255.77.77")
    ap.add_argument("--port", type=int, default=4010)
    ap.add_argument("--iface", default="0.0.0.0", help="local interface address to join the group on")
    ap.add_argument("--seconds", type=float, default=5.0)
    ap.add_argument("--timeout", type=float, default=1.0, help="per-recv timeout while waiting for audio")
    ap.add_argument("--expect-hz", type=float, default=0.0, help="test tone frequency to verify")
    ap.add_argument("--json", default="", help="write the result as JSON to this path")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    res = capture(args)
    print(json.dumps(res, indent=2, ensure_ascii=False))
    if args.json:
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(res, fh, indent=2, ensure_ascii=False)
        print("written: %s" % args.json)
    if res["packets"] == 0:
        print("RESULT: NO AUDIO (0 datagrams in %.1fs) - driver not running, endpoint silent, "
              "or multicast blocked" % args.seconds)
        return 2
    print("RESULT: %d datagrams, %d PCM bytes, peak=%d%s" % (
        res["packets"], res["payload_bytes"], res["peak"],
        (", measured %.1f Hz (expected %.0f Hz)" % (res["measured_hz"], args.expect_hz)) if args.expect_hz else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
