#!/usr/bin/env python3
"""Does a L16A/L16B speaker answer the miio LAN protocol on UDP 54321?

miio is UDP, so a TCP probe proves nothing. This sends the standard 32-byte miio
hello (0x21310020 + 0xff*28) and waits for the device's token/deviceid reply,
which would reveal the device's local uid and confirm a local control channel.

Read-only: a single UDP hello, no commands.
"""
from __future__ import annotations

import socket
import sys

HELLO = bytes.fromhex("21310020" + "ff" * 28)


def probe(ip: str, port: int = 54321, wait: float = 3.0) -> None:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(wait)
    try:
        s.sendto(HELLO, (ip, port))
        data, addr = s.recvfrom(1024)
        print(f"  {ip}:{port} REPLY {len(data)}B from {addr}: {data.hex()}")
        if len(data) >= 32:
            print(f"      header, device_id={int.from_bytes(data[8:12], 'big')}, stamp={int.from_bytes(data[12:16], 'big')}")
    except (socket.timeout, TimeoutError):
        print(f"  {ip}:{port} no reply within {wait}s (miio not exposed / filtered)")
    except Exception as exc:
        print(f"  {ip}:{port} {type(exc).__name__}: {exc}")
    finally:
        s.close()


if __name__ == "__main__":
    hosts = sys.argv[1:] or ["192.168.1.171", "192.168.1.21", "192.168.1.10"]
    for h in hosts:
        probe(h)
