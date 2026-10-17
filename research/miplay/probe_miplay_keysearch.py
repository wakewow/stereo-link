#!/usr/bin/env python3
"""Capture one MiPlay SAFETY_AUTH challenge from a real Xiaomi speaker and
determine the exact AES-128 key/IV derivation by offline candidate search.

Read-only: no audio, no volume change, no persistent state on the device.
"""

from __future__ import annotations

import hashlib
import json
import socket
import struct
import sys

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

speaker = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.171"
port = 8899


def digits_mapped(value: str) -> str:
    return "".join(chr(ord(c) + 0x31) if c.isdigit() else c for c in value)


def md5_hex(local, remote) -> str:
    return hashlib.md5(digits_mapped(f"{local[0]}{local[1]}{remote[0]}{remote[1]}").encode()).hexdigest()


def wrap_payload(key: str, data: bytes) -> bytes:
    return bytes([len(key)]) + key.encode() + struct.pack("<I", 30) + bytes([len(data)]) + data


def read_frame(sock):
    header = b""
    while len(header) < 9:
        chunk = sock.recv(9 - len(header))
        if not chunk:
            raise ConnectionError("closed")
        header += chunk
    assert header[0] == 0x24, header.hex()
    length = int.from_bytes(header[5:9], "big")
    body = b""
    while len(body) < length:
        body += sock.recv(length - len(body))
    return header[1], header[2], int.from_bytes(header[3:5], "big"), body


def try_decrypt(body: bytes, key: bytes, iv: bytes):
    ct = body[9:]
    try:
        dec = Cipher(algorithms.AES(key), modes.CBC(iv)).decryptor()
        return dec.update(ct) + dec.finalize()
    except Exception as exc:
        return b"<error %r>" % (exc,)


def main() -> int:
    sock = socket.create_connection((speaker, port), timeout=3.0)
    local, peer = sock.getsockname(), sock.getpeername()
    sock.settimeout(2.0)
    print(f"connected local={local} peer={peer}")

    outer, cmd, seq, body = read_frame(sock)
    print(f"RX greeting cmd=0x{cmd:02x} seq={seq} body={body!r}")

    si = ("{\n\t\"aesIvTypes\": \"3\",\n\t\"aesKeyTypes\": \"3\",\n"
          "\t\"authAlgorithmTypes\": \"7\",\n\t\"authKeyTypes\": \"1\",\n"
          "\t\"integrityTypes\": \"1\" \n} \n").encode()
    frame = bytes([0x24, 0x14, 0x00, 0x00, 0x01]) + struct.pack(">I", len(wrap_payload("cmd", si))) + wrap_payload("cmd", si)
    sock.sendall(frame)
    print("TX SAFETY_INFO")

    challenge_body = None
    for _ in range(4):
        outer, cmd, seq, body = read_frame(sock)
        print(f"RX cmd=0x{cmd:02x} outer=0x{outer:02x} seq={seq} len={len(body)} body={body[:160]!r}")
        if cmd == 0x02:
            challenge_body = body
            break
    sock.close()
    if challenge_body is None:
        print("no SAFETY_AUTH received")
        return 1
    if challenge_body[:4] != bytes([0, 7, 1, 0xE0]):
        print("SAFETY_AUTH was NOT encrypted")
        return 1

    hex_server_first = md5_hex(peer, local)
    hex_client_first = md5_hex(local, peer)
    print(f"\nmd5 digit-mapped, server-first (ip,port,ip,port) = {hex_server_first}")
    print(f"md5 digit-mapped, client-first                   = {hex_client_first}")
    print(f"ciphertext = {challenge_body[9:].hex()}")

    candidates: list[tuple[str, bytes, bytes]] = []
    for order_name, digest in (("server_first", hex_server_first), ("client_first", hex_client_first)):
        ascii_prefix = digest[:16].encode()
        raw = bytes.fromhex(digest)
        for key_name, key in ((f"{order_name}:ascii16(md5hex[:16])", ascii_prefix), (f"{order_name}:raw_md5", raw)):
            candidates += [
                (f"{key_name} iv=key", key, key),
                (f"{key_name} iv=zero", key, bytes(16)),
                (f"{key_name} iv=ascii_prefix", key, ascii_prefix),
                (f"{key_name} iv=raw_md5", key, raw),
            ]

    print("\n=== candidate search (first 16 plaintext bytes) ===")
    hit = None
    for name, key, iv in candidates:
        plain = try_decrypt(challenge_body, key, iv)
        head = plain[:16]
        printable = all(32 <= b < 127 or b == 0 for b in head)
        flag = ""
        if plain[:1] == b"\x03" and b"cmd" in plain[:8]:
            flag = "  <== MATCH (wrap_payload header)"
            hit = (name, key, iv, plain)
        print(f"  {name:<48} {head.hex()} {head!r:<40}{flag}")
    if hit:
        name, key, iv, plain = hit
        print(f"\nWINNER: {name}")
        # unwrap
        key_len = plain[0]
        data_len = plain[5 + key_len]
        payload = plain[6 + key_len : 6 + key_len + data_len]
        print(f"  wrap key={plain[1:1+key_len]!r} dataType={int.from_bytes(plain[1+key_len:5+key_len],'little')}")
        print(f"  JSON: {payload.decode()!r}")
        try:
            print(f"  parsed: {json.loads(payload)}")
        except Exception as exc:
            print(f"  json parse failed: {exc!r}")
    else:
        print("\nno candidate matched -> derivation is something else")
    return 0


if __name__ == "__main__":
    sys.exit(main())
