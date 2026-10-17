#!/usr/bin/env python3
"""Read-only MiPlay (Xiaomi 妙播) probr: mDNS enumeration + SafetyAuth handshake.

Purpose
-------
Determine empirically whether a third-party host can act as a MiPlay *sender*
against a real Xiaomi speaker: does the speaker (receiver) accept an inbound
TCP 8899 control connection from a non-Xiaomi device and complete the
"compat" SafetyAuth handshake that FusionPlay-Android documents?

Safety
------
* Does NOT play audio, does NOT change volume, does NOT touch any setting.
* Only: mDNS browse (passive/multicast query) + TCP connect + protocol frames.
* Wire format mirrors FusionPlay.MiPlaySdk protocol.rs (read-only reference).

Run with MiXPlay's interpreter (has cryptography/zeroconf):
  C:\\Users\\Administrator\\AppData\\Roaming\\uv\\tools\\mixplay-hub\\Scripts\\python.exe
"""

from __future__ import annotations

import hashlib
import hmac as hmac_mod
import json
import socket
import struct
import sys
import time

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

CONTROL_PORT = 8899

SAFETY_INFO, SAFETY_INFO_ACK, SAFETY_AUTH, SAFETY_AUTH_ACK = 0x00, 0x01, 0x02, 0x03
DEVICE_ID = 0x28
GET_DEVICE_INFO, GET_DEVICE_INFO_ACK = 0x1E, 0x1F
GET_STATE, GET_STATE_ACK = 0x1C, 0x1D
HEART_BEAT, HEART_BEAT_ACK = 0x1A, 0x1B
GET_MIRROR_MODE, GET_MIRROR_MODE_ACK = 0x34, 0x35
SET_PLAY_SOURCE, SET_PLAY_SOURCE_ACK = 0x40, 0x41

NAMES = {
    0x28: "DEVICE_ID", 0x00: "SAFETY_INFO", 0x01: "SAFETY_INFO_ACK",
    0x02: "SAFETY_AUTH", 0x03: "SAFETY_AUTH_ACK", 0x1E: "GET_DEVICE_INFO",
    0x1F: "GET_DEVICE_INFO_ACK", 0x1C: "GET_STATE", 0x1D: "GET_STATE_ACK",
    0x1A: "HEART_BEAT", 0x1B: "HEART_BEAT_ACK", 0x34: "GET_MIRROR_MODE",
    0x35: "GET_MIRROR_MODE_ACK", 0x40: "SET_PLAY_SOURCE", 0x41: "SET_PLAY_SOURCE_ACK",
    0x12: "SET_MEDIA_INFO", 0x13: "SET_MEDIA_INFO_ACK", 0x6C: "SET_MIRROR_KEY",
    0x6D: "SET_MIRROR_KEY_ACK", 0x22: "NOTIFY",
}


def cname(cmd: int) -> str:
    return NAMES.get(cmd, f"0x{cmd:02x}")


# ---------------------------------------------------------------- integrity
def _table_entry(index: int) -> int:
    value = index << 24
    for _ in range(8):
        value = ((value << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if value & 0x80000000 else (value << 1) & 0xFFFFFFFF
    return int.from_bytes(value.to_bytes(4, "big"), "little")


_TABLE = [_table_entry(i) for i in range(256)]


def miplay_integrity(data: bytes) -> int:
    value = 0xFFFFFFFF
    for byte in data:
        value = _TABLE[(value & 0xFF) ^ byte] ^ (value >> 8)
    return value


# ---------------------------------------------------------------- key + cipher
def generate_auth_key(local: tuple[str, int], remote: tuple[str, int]) -> str:
    """MD5 of '<local_ip><local_port><remote_ip><remote_port>' with every ASCII
    digit shifted by +0x31 (protocol.rs:1991). Symmetric: both peers compute it."""
    raw = f"{local[0]}{local[1]}{remote[0]}{remote[1]}"
    mapped = "".join(chr(ord(c) + 0x31) if c.isdigit() else c for c in raw)
    return hashlib.md5(mapped.encode()).hexdigest()


class ControlCipher:
    def __init__(self, key: bytes):
        self.key = key
        self.iv = key

    def encrypt(self, plain: bytes) -> bytes:
        pad = 16 - (len(plain) % 16)
        padded = plain + b"\x00" * pad
        enc = Cipher(algorithms.AES(self.key), modes.CBC(self.iv)).encryptor()
        ct = enc.update(padded) + enc.finalize()
        self.iv = ct[-16:]
        out = bytes([0x00, 0x07, 0x01, 0xE0, pad])
        out += miplay_integrity(ct).to_bytes(4, "big")
        return out + ct

    def decrypt(self, body: bytes) -> bytes:
        pad = body[4]
        ct = body[9:]
        received = int.from_bytes(body[5:9], "big")
        expected = miplay_integrity(ct)
        if received != expected:
            raise ValueError(f"integrity mismatch rx={received:08x} want={expected:08x}")
        next_iv = ct[-16:]
        dec = Cipher(algorithms.AES(self.key), modes.CBC(self.iv)).decryptor()
        plain = dec.update(ct) + dec.finalize()
        self.iv = next_iv
        return plain[: len(plain) - pad]


def wrap_payload(key: str, data: bytes) -> bytes:
    return bytes([len(key)]) + key.encode() + struct.pack("<I", 30) + bytes([len(data)]) + data


def unwrap_payload(body: bytes):
    key_len = body[0]
    key = body[1 : 1 + key_len].decode("utf-8", "replace")
    data_len = body[5 + key_len]
    return key, body[6 + key_len : 6 + key_len + data_len]


def hmac_sha256_hex(key: bytes, msg: bytes) -> str:
    return hmac_mod.new(key, msg, hashlib.sha256).hexdigest()


# ---------------------------------------------------------------- framing
def send_frame(sock: socket.socket, outer: int, cmd: int, seq: int, body: bytes) -> None:
    header = bytes([0x24, outer, cmd, (seq >> 8) & 0xFF, seq & 0xFF]) + struct.pack(">I", len(body))
    sock.sendall(header + body)


def read_frame(sock: socket.socket, cipher: ControlCipher):
    """Returns (outer, cmd, seq, plaintext_body, encrypted, raw_len) or None on timeout."""
    header = b""
    while len(header) < 9:
        chunk = sock.recv(9 - len(header))
        if not chunk:
            raise ConnectionError("peer closed while reading header")
        header += chunk
    if header[0] != 0x24:
        raise ValueError(f"bad magic {header.hex()}")
    outer, cmd = header[1], header[2]
    seq = int.from_bytes(header[3:5], "big")
    length = int.from_bytes(header[5:9], "big")
    body = b""
    while len(body) < length:
        chunk = sock.recv(length - len(body))
        if not chunk:
            raise ConnectionError("peer closed while reading body")
        body += chunk
    encrypted = body[:4] == bytes([0x00, 0x07, 0x01, 0xE0])
    plain = cipher.decrypt(body) if encrypted else body
    return outer, cmd, seq, plain, encrypted, len(body)


def body_repr(plain: bytes, encrypted: bool) -> str:
    try:
        if plain and plain[0] <= 8:
            key, data = unwrap_payload(plain)
            return f"wrap[{key}]={data[:200].decode('utf-8', 'replace')!r}"
    except Exception:
        pass
    if not encrypted:
        try:
            return f"plain={plain[:200].decode('utf-8')!r}"
        except Exception:
            pass
    return f"hex={plain[:64].hex()}"


# ---------------------------------------------------------------- mdns
def mdns_enumerate(timeout: float = 4.0) -> None:
    print("=== mDNS service enumeration (_services._dns-sd._udp.local) ===")
    try:
        from zeroconf import ServiceBrowser, ServiceListener, Zeroconf
        from zeroconf.const import _TYPE_SERVICES

        found: dict[str, dict] = {}

        class L(ServiceListener):
            def add_service(self, zc, type_, name):
                info = zc.get_service_info(type_, name, timeout=2000)
                found[f"{name}|{type_}"] = {
                    "type": type_, "name": name,
                    "addresses": [socket.inet_ntoa(a) for a in (info.addresses if info else [])],
                    "port": info.port if info else None,
                    "server": info.server if info else None,
                    "properties": {k.decode(): v.decode() if v else "" for k, v in (info.properties.items() if info else [])},
                }

            def update_service(self, zc, type_, name):
                self.add_service(zc, type_, name)

            def remove_service(self, zc, type_, name):
                pass

        zc = Zeroconf()
        types = {}
        class TL(ServiceListener):
            def add_service(self, zc, type_, name):
                info = zc.get_service_info(type_, name, timeout=1500)
                if info:
                    types[name] = True
                    for _ in [0]:
                        pass
            def update_service(self, zc, type_, name):
                self.add_service(zc, type_, name)
            def remove_service(self, zc, type_, name):
                pass

        # zeroconf exposes an enumeration helper
        from zeroconf import ZeroconfServiceTypes
        all_types = ZeroconfServiceTypes.find(zc=zc, timeout=timeout)
        print(f"  {len(all_types)} service types on LAN:")
        for t in sorted(all_types):
            print(f"    - {t}")
        xiaomi = [t for t in all_types if any(k in t.lower() for k in ("lyra", "mi-", "xiaomi", "miot", "mijiia", "miplay", "wfd"))]
        print(f"  Xiaomi-ish types: {xiaomi}")
        for t in sorted(all_types):
            ServiceBrowser(zc, t, L())
        time.sleep(timeout)
        for key, rec in sorted(found.items()):
            print(f"  * {rec['name']}")
            print(f"      type={rec['type']} host={rec['server']} port={rec['port']} addrs={rec['addresses']}")
            if rec["properties"]:
                for k, v in sorted(rec["properties"].items()):
                    print(f"      TXT {k}={v!r}")
        zc.close()
    except Exception as exc:  # pragma: no cover
        print(f"  mDNS enumeration failed: {exc!r}")


# ---------------------------------------------------------------- handshake
def probe(ip: str, label: str, deep: bool = True) -> dict:
    print(f"\n=== TCP {ip}:{CONTROL_PORT} [{label}] ===")
    result = {"ip": ip, "label": label, "tcp": False, "handshake": False}
    try:
        sock = socket.create_connection((ip, CONTROL_PORT), timeout=3.0)
    except Exception as exc:
        print(f"  connect FAILED: {exc!r}")
        return result
    result["tcp"] = True
    local = sock.getsockname()
    peer = sock.getpeername()
    auth_key = generate_auth_key((local[0], local[1]), (peer[0], peer[1]))
    print(f"  connected local={local} peer={peer}")
    print(f"  derived auth_key (md5 4-tuple) = {auth_key}")
    key = auth_key[:16].encode()
    print(f"  AES-128 key = {key!r} (first 16 ASCII bytes of the md5 hex string)")
    tx, rx = ControlCipher(key), ControlCipher(key)
    result["auth_key"] = auth_key

    # The receiver publishes its numeric device id first (plaintext).
    sock.settimeout(1.2)
    try:
        frame = read_frame(sock, rx)
        if frame:
            outer, cmd, seq, plain, enc, raw = frame
            print(f"  <- RX {cname(cmd)} outer=0x{outer:02x} seq={seq} enc={enc} len={raw} {body_repr(plain, enc)}")
            result["device_id_frame"] = cname(cmd)
    except Exception as exc:
        print(f"  (no greeting frame: {exc!r})")

    # SafetyInfo: the exact byte string MiPCAudio sends (protocol.rs:2958).
    safety_info = (
        "{\n\t\"aesIvTypes\": \"3\",\n\t\"aesKeyTypes\": \"3\",\n"
        "\t\"authAlgorithmTypes\": \"7\",\n\t\"authKeyTypes\": \"1\",\n"
        "\t\"integrityTypes\": \"1\" \n} \n"
    )
    send_frame(sock, 0x14, SAFETY_INFO, 1, wrap_payload("cmd", safety_info.encode()))
    print("  -> TX SAFETY_INFO outer=0x14 seq=1 (compat: aesIvType/aesKeyType 3, authAlgorithmTypes 7, authKeyTypes 1, integrityTypes 1)")

    server_challenge = None
    client_challenge = "0123456789abcdeffedcba9876543210"
    sent_auth = False
    deadline = time.time() + 6.0
    while time.time() < deadline:
        sock.settimeout(max(0.3, deadline - time.time()))
        try:
            outer, cmd, seq, plain, enc, raw = read_frame(sock, rx)
        except (socket.timeout, TimeoutError):
            break
        except Exception as exc:
            print(f"  RX error: {exc!r}")
            break
        print(f"  <- RX {cname(cmd)} outer=0x{outer:02x} seq={seq} enc={enc} len={raw} {body_repr(plain, enc)}")
        if cmd == SAFETY_AUTH:
            try:
                _, data = unwrap_payload(plain)
                payload = json.loads(data)
            except Exception:
                payload = json.loads(plain)
            server_challenge = payload.get("authMsg")
            print(f"     server challenge = {server_challenge}")
            # Client challenge + ack of the server challenge (protocol.rs:2978-2991).
            challenge = json.dumps({"authMsg": client_challenge})  # noqa: F841
            body = ("{\n\t\"authMsg\": \"%s\" \n} \n" % client_challenge).encode()
            send_frame(sock, 0x14, SAFETY_AUTH, 2, tx.encrypt(wrap_payload("cmd", body)))
            ack = hmac_sha256_hex(auth_key.encode(), server_challenge.encode())
            ack_body = ("{\n\t\"authMsgAck\": \"%s\",\n\t\"result\": \"0\" \n} \n" % ack).encode()
            send_frame(sock, 0x14, SAFETY_AUTH_ACK, 0, tx.encrypt(wrap_payload("ack", ack_body)))
            print(f"  -> TX SAFETY_AUTH seq=2 (client challenge {client_challenge})")
            print(f"  -> TX SAFETY_AUTH_ACK seq=0 hmac={ack[:16]}... result=0")
            sent_auth = True
        if cmd == SAFETY_AUTH_ACK and sent_auth:
            try:
                _, data = unwrap_payload(plain)
                payload = json.loads(data)
            except Exception:
                payload = {}
            expected = hmac_sha256_hex(auth_key.encode(), client_challenge.encode())
            got = payload.get("authMsgAck")
            ok = got == expected
            print(f"     mutual auth verified = {ok}")
            result["handshake"] = ok
            break
    if not sent_auth:
        print("  !! peer never sent SAFETY_AUTH -> no compat handshake")
        sock.close()
        return result

    if deep and result["handshake"]:
        for cmd, seq, label_ in ((GET_DEVICE_INFO, 3, "GET_DEVICE_INFO"),
                                 (GET_MIRROR_MODE, 4, "GET_MIRROR_MODE"),
                                 (GET_STATE, 9, "GET_STATE"),
                                 (HEART_BEAT, 10, "HEART_BEAT")):
            send_frame(sock, 0x00, cmd, seq, tx.encrypt(b""))
            sock.settimeout(2.5)
            try:
                outer, rcmd, rseq, plain, enc, raw = read_frame(sock, rx)
                print(f"  <- RX after {label_}: {cname(rcmd)} seq={rseq} enc={enc} {body_repr(plain, enc)}")
                result[label_] = cname(rcmd)
            except Exception as exc:
                print(f"  <- no reply to {label_}: {exc!r}")
                result[label_] = f"timeout:{exc!r}"
                break
    sock.close()
    return result


def main() -> int:
    print("MiPlay sender-feasibility probe (read-only: no audio, no config change)")
    mdns_enumerate(timeout=4.0)
    targets = [
        ("192.168.1.171", "Xiaomi Sound L16B did=934947888 (right)"),
        ("192.168.1.21", "Xiaomi Sound-左 L16A did=501718792 (left)"),
        ("192.168.1.4", "MiXPlay on this PC (control: expect no MiPlay listener)"),
    ]
    results = [probe(ip, label) for ip, label in targets]
    print("\n=== SUMMARY ===")
    for r in results:
        print(f"  {r['ip']:<15} {r['label'][:38]:<40} tcp={r['tcp']} handshake={r.get('handshake')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
