#!/usr/bin/env python3
"""Post-auth command ladder: which MiPlay command does a real Xiaomi speaker
actually answer, and under which framing?  Read-only (GET_VOLUME is a query;
SET_* frames here carry metadata only and never start audio)."""

from __future__ import annotations

import json
import socket
import sys
import time

sys.path.insert(0, __file__.rsplit("\\", 1)[0])
from probe_miplay_sender import Sender, cname, wrap_payload  # noqa: E402

GET_VOLUME, GET_VOLUME_ACK = 0x0E, 0x0F
SET_DEVICE_INFO, SET_DEVICE_INFO_ACK = 0x58, 0x59
SET_MEDIA_INFO, SET_MEDIA_INFO_ACK = 0x12, 0x13
SET_PLAY_SOURCE, SET_PLAY_SOURCE_ACK = 0x40, 0x41
SET_MIRROR_KEY, SET_MIRROR_KEY_ACK = 0x6C, 0x6D
OPEN = 0x00

MEDIA_INFO = json.dumps({
    "title": "miplay-bridge probe", "artist": "probe", "album": "probe",
    "duration": "10", "position": "0", "playState": "1", "source": "local",
})
SOURCE_INFO = json.dumps({"sourceName": "miplay-bridge", "sourceType": "2", "sourceId": "probe"})
PLAY_SOURCE = json.dumps({"source": "xiaomi", "sourceName": "miplay-bridge", "playSource": "1"})
MIRROR_KEY = json.dumps({"authKey": "0123456789abcdef", "streamKey": "fedcba9876543210",
                         "streamIV": "abcdef0123456789"})


def attempt(s: Sender, cmd: int, seq: int, body: bytes, tag: str, outer: int = 0x00) -> bool:
    s.send(outer, cmd, seq, body, tag)
    try:
        outer_r, cmd_r, seq_r, plain, enc = s.read(2.0)
        return True
    except (socket.timeout, TimeoutError):
        print("     (no reply within 2s)")
        return False
    except ConnectionError as exc:
        print(f"     !! peer closed the socket: {exc}")
        raise
    except Exception as exc:
        print(f"     !! {exc!r}")
        return False


def main() -> int:
    ip = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.171"
    s = Sender(ip, "L16B")
    if not s.handshake():
        return 1

    print("\n--- post-auth command ladder (authenticated session) ---")
    answered: list[str] = []
    steps = [
        ("HEART_BEAT", 0x1A, 10, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_VOLUME", GET_VOLUME, 11, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_STATE", 0x1C, 12, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_MIRROR_MODE", 0x34, 13, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_DEVICE_INFO", 0x1E, 14, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_VOLUME(seq=0x0100)", GET_VOLUME, 0x0100, lambda: s.tx.encrypt(b""), 0x00),
        ("GET_VOLUME(outer=0x14,wrap)", GET_VOLUME, 15,
         lambda: s.tx.encrypt(wrap_payload("cmd", b'{"getVolume":"1"}')), 0x14),
        ("SET_DEVICE_INFO", SET_DEVICE_INFO, 16,
         lambda: s.tx.encrypt(wrap_payload("cmd", SOURCE_INFO.encode())), 0x00),
        ("SET_MIRROR_KEY", SET_MIRROR_KEY, 17,
         lambda: s.tx.encrypt(wrap_payload("cmd", MIRROR_KEY.encode())), 0x00),
        ("SET_MEDIA_INFO", SET_MEDIA_INFO, 18,
         lambda: s.tx.encrypt(wrap_payload("cmd", MEDIA_INFO.encode())), 0x00),
        ("SET_PLAY_SOURCE", SET_PLAY_SOURCE, 19,
         lambda: s.tx.encrypt(wrap_payload("cmd", PLAY_SOURCE.encode())), 0x00),
        ("OPEN(wfd://)", OPEN, 20,
         lambda: s.tx.encrypt(f"wfd://{s.local[0]}:19999?mirrorMode=1".encode()), 0x00),
    ]
    for tag, cmd, seq, make_body, outer in steps:
        print(f"  [{tag}]")
        try:
            if attempt(s, cmd, seq, make_body(), tag, outer):
                answered.append(tag)
        except ConnectionError:
            print("  socket closed by peer -> stopping ladder")
            break

    print(f"\nanswered commands: {answered or 'NONE'}")
    print("\n--- idle 5s to see if the peer pushes anything spontaneously ---")
    try:
        while True:
            s.read(2.0)
    except Exception as exc:
        print(f"  idle read ended: {exc!r}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
