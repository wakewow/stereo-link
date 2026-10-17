#!/usr/bin/env python3
"""scream-udp-selftest.py - prove the receive path with a synthetic Scream stream.

The kernel driver cannot be loaded on this host without test signing (see
research/vad/driver-install.md), so the *receiver* half of the smoke test is verified
here instead: this script fabricates datagrams that are byte-for-byte what Scream emits
(5-byte header + interleaved little-endian 16-bit PCM), sends them to the real multicast
group, and decodes them with the very same parser the production capture uses.

Usage:
    python scream-udp-selftest.py                    # 1 kHz, 48 kHz, 16 bit, stereo
    python scream-udp-selftest.py --hz 440 --seconds 1
    python scream-udp-selftest.py --iface 192.168.1.4

Exit code 0 = PASS, 1 = FAIL, 2 = could not open the multicast path.
"""

from __future__ import annotations

import argparse
import math
import socket
import struct
import sys
import time

from importlib import import_module

cap = import_module("scream-udp-capture") if False else None  # placeholder, real import below

sys.path.insert(0, __file__.rsplit("\\", 1)[0] if "\\" in __file__ else ".")
import importlib.util as _ilu

_spec = _ilu.spec_from_file_location(
    "scream_capture",
    (__file__.rsplit("\\", 1)[0] if "\\" in __file__ else ".") + "\\scream-udp-capture.py",
)
_mod = _ilu.module_from_spec(_spec)
_spec.loader.exec_module(_mod)  # type: ignore[union-attr]

HEADER_LEN = _mod.HEADER_LEN
decode_header = _mod.decode_header
to_samples = _mod.to_samples
peak_of = _mod.peak_of
rms_of = _mod.rms_of
zero_cross_hz = _mod.zero_cross_hz
open_receiver = _mod.open_receiver


def build_packets(hz: float, rate: int, bits: int, channels: int, seconds: float, amplitude: int):
    """Yield Scream-format datagrams carrying a continuous sine wave."""
    frames_per_packet = 1152 // (channels * (bits // 8))
    total_frames = int(rate * seconds)
    sample = 0
    frame = 0
    hdr = bytes([0x00, bits, channels, 0x03, 0x00])  # 48 kHz base, 16 bit, 2 ch, mask FL|FR
    while frame < total_frames:
        pcm = bytearray()
        for _ in range(frames_per_packet):
            value = int(amplitude * math.sin(2.0 * math.pi * hz * sample / rate))
            for _ch in range(channels):
                pcm += struct.pack("<h", value)
            sample += 1
            frame += 1
        yield hdr + bytes(pcm)


def main() -> int:
    ap = argparse.ArgumentParser(description="synthetic Scream stream self-test")
    ap.add_argument("--group", default="239.255.77.77")
    ap.add_argument("--port", type=int, default=4010)
    ap.add_argument("--iface", default="192.168.1.4")
    ap.add_argument("--hz", type=float, default=1000.0)
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--bits", type=int, default=16)
    ap.add_argument("--channels", type=int, default=2)
    ap.add_argument("--seconds", type=float, default=0.3)
    ap.add_argument("--amplitude", type=int, default=30000)
    args = ap.parse_args()

    print("synthetic Scream stream: %.0f Hz, %d Hz, %d bit, %d ch, %.2fs -> %s:%d (iface %s)" % (
        args.hz, args.rate, args.bits, args.channels, args.seconds, args.group, args.port, args.iface))

    rx = open_receiver(args.group, args.port, args.iface, timeout=1.0)

    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    tx.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(args.iface))
    tx.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    tx.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)

    sent = 0
    for dgram in build_packets(args.hz, args.rate, args.bits, args.channels, args.seconds, args.amplitude):
        tx.sendto(dgram, (args.group, args.port))
        sent += 1
        time.sleep(0.001)  # 1 ms pacing, like the driver's full-speed frame cadence

    got = 0
    payload_bytes = 0
    hdr = None
    samples_all = None
    peak = 0
    deadline = time.time() + 3.0
    while got < sent and time.time() < deadline:
        try:
            data, _addr = rx.recvfrom(65535)
        except socket.timeout:
            break
        if len(data) <= HEADER_LEN:
            continue
        got += 1
        if hdr is None:
            hdr = decode_header(data[:HEADER_LEN])
        payload = data[HEADER_LEN:]
        payload_bytes += len(payload)
        s = to_samples(payload, hdr["bits"])
        peak = max(peak, peak_of(s))
        samples_all = s if samples_all is None else samples_all + s

    print("sent=%d  received=%d  pcm_bytes=%d" % (sent, got, payload_bytes))
    checks = []
    if hdr is None:
        print("FAIL: no datagram decoded")
        return 1
    print("header -> %s" % hdr)
    checks.append(("header sample_rate == %d" % args.rate, hdr["sample_rate"] == args.rate))
    checks.append(("header bits == %d" % args.bits, hdr["bits"] == args.bits))
    checks.append(("header channels == %d" % args.channels, hdr["channels"] == args.channels))
    checks.append(("all datagrams arrived", got == sent))
    checks.append(("peak > 0 (not silence)", peak > 0))
    measured = zero_cross_hz(samples_all, hdr["channels"], hdr["sample_rate"])
    err = abs(measured - args.hz) / args.hz * 100.0
    print("peak=%d  rms=%.1f  measured=%.1f Hz  (expected %.0f Hz, error %.2f%%)" % (
        peak, rms_of(samples_all), measured, args.hz, err))
    checks.append(("measured tone within 3%% of %.0f Hz" % args.hz, err < 3.0))

    failed = [name for name, ok in checks if not ok]
    for name, ok in checks:
        print("  [%s] %s" % ("PASS" if ok else "FAIL", name))
    print("RESULT: " + ("PASS" if not failed else "FAIL (%s)" % ", ".join(failed)))
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
