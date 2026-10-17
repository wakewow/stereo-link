#!/usr/bin/env python3
"""Xiaomi local miio control probe (UDP 54321) for the two L16 speakers.

The speakers answer the miio hello, so a cloud-free local channel exists. This
completes the handshake, then optionally calls read-only methods with the device
token (taken from the user's own cloud device list) to see what the local API exposes.

  python probe_miio_local.py --info
  python probe_miio_local.py --call player_get_play_status --call miIO.info
  python probe_miio_local.py --play-url http://192.168.1.4:8788/t.wav   # plays <=10s

Read-only unless --play-url is given. Never changes volume or settings.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import socket
import struct
import sys
import time

MAGIC = bytes.fromhex("2131")
HELLO = bytes.fromhex("21310020" + "ff" * 28)
DEVICE_LIST = pathlib.Path(r"D:\Backup\Documents\deepseek-harness\default-workspace\mixplay-pc-audio\scout\device_list_raw.json")
TARGETS = {"934947888": "192.168.1.171", "501718792": "192.168.1.21"}


def load_tokens() -> dict[str, str]:
    """Map miot did -> 32-hex miio token from the user's cloud device list (read-only)."""
    text = DEVICE_LIST.read_text(encoding="utf-8")
    data = json.loads(text)
    found: dict[str, str] = {}

    def walk(node):
        if isinstance(node, dict):
            did, tok = node.get("did"), node.get("token")
            if did and isinstance(tok, str) and len(tok) == 32:
                found[str(did)] = tok
            for v in node.values():
                walk(v)
        elif isinstance(node, list):
            for v in node:
                walk(v)

    walk(data)
    return found


def handshake(ip: str, timeout: float = 3.0) -> tuple[int, int, bytes]:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    s.sendto(HELLO, (ip, 54321))
    data, _ = s.recvfrom(4096)
    s.close()
    if data[:2] != MAGIC:
        raise ValueError(f"bad magic {data[:4].hex()}")
    device_id = int.from_bytes(data[8:12], "big")
    stamp = int.from_bytes(data[12:16], "big")
    return device_id, stamp, data


def encrypt(token_hex: str, payload: dict, device_id: int, stamp: int) -> bytes:
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

    token = bytes.fromhex(token_hex)
    key = hashlib.md5(token).digest()
    iv = hashlib.md5(key + token).digest()
    body = json.dumps(payload, separators=(",", ":")).encode()
    pad = 16 - (len(body) % 16)
    enc = Cipher(algorithms.AES(key), modes.CBC(iv)).encryptor()
    ct = enc.update(body + b"\x00" * pad) + enc.finalize()
    total = 32 + len(ct) + 16
    header = MAGIC + struct.pack(">H", total) + b"\x00\x00\x00\x00" + struct.pack(">I", device_id) + struct.pack(">I", stamp & 0xFFFFFFFF)
    packet = header + token + ct
    return packet + hashlib.md5(packet).digest()


def decrypt(token_hex: str, data: bytes) -> str:
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

    token = bytes.fromhex(token_hex)
    key = hashlib.md5(token).digest()
    iv = hashlib.md5(key + token).digest()
    ct = data[32:-16]
    if not ct:
        return ""
    dec = Cipher(algorithms.AES(key), modes.CBC(iv)).decryptor()
    out = dec.update(ct) + dec.finalize()
    return out.rstrip(b"\x00").decode("utf-8", "replace")


def call(ip: str, token_hex: str, device_id: int, stamp: int, method: str, params: list) -> str:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(5.0)
    pkt = encrypt(token_hex, {"id": 1, "method": method, "params": params}, device_id, stamp + 1)
    s.sendto(pkt, (ip, 54321))
    try:
        data, _ = s.recvfrom(8192)
        return decrypt(token_hex, data)
    except (socket.timeout, TimeoutError):
        return "<no reply>"
    finally:
        s.close()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--info", action="store_true")
    ap.add_argument("--call", action="append", default=[])
    ap.add_argument("--play-url")
    args = ap.parse_args()

    tokens = load_tokens()
    for did, ip in TARGETS.items():
        tok = tokens.get(did)
        print(f"\n=== did={did} ip={ip} token={'found' if tok else 'MISSING'} ===")
        if not tok:
            continue
        try:
            device_id, stamp, raw = handshake(ip)
        except Exception as exc:
            print(f"  handshake failed: {exc!r}")
            continue
        print(f"  hello ok: device_id={device_id} stamp={stamp}")
        methods = list(args.call)
        if args.info or not methods:
            methods = ["miIO.info"] + methods
        for m in methods:
            reply = call(ip, tok, device_id, stamp, m, [])
            print(f"  {m:<26} -> {reply[:400]}")
            stamp += 1
        if args.play_url:
            params = [{"url": args.play_url, "type": 1, "media": "app_ios"}]
            for m in ("player_play_url", "player_play_music", "play_url"):
                reply = call(ip, tok, device_id, stamp, m, params)
                print(f"  {m:<26} -> {reply[:400]}")
                stamp += 1
                if "<no reply>" not in reply and '"error"' not in reply:
                    break
            time.sleep(6)
    return 0


if __name__ == "__main__":
    sys.exit(main())
