#!/usr/bin/env python3
"""MiPlay sender-feasibility probe against real Xiaomi speakers (read-only).

Proves or disproves that a third-party PC can be a MiPlay *sender*:
  phase 1  TCP 8899 control session + compat SafetyAuth (no Xiaomi account)
  phase 2  authenticated control traffic (GET_DEVICE_INFO / GET_MIRROR_MODE /
           GET_STATE / HEART_BEAT)
  phase 3  (--media) host a local RTSP server, send OPEN with
           wfd://<pc>:<port>?mirrorMode=1, and log what the receiver does:
           does it dial back, and what RTSP requests does it send?

NO audio is streamed, NO volume is changed, NO setting is written.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac as hmac_mod
import json
import socket
import struct
import sys
import threading
import time

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

CONTROL_PORT = 8899
SAFETY_INFO, SAFETY_INFO_ACK, SAFETY_AUTH, SAFETY_AUTH_ACK = 0x00, 0x01, 0x02, 0x03
DEVICE_ID = 0x28
GET_DEVICE_INFO, GET_DEVICE_INFO_ACK = 0x1E, 0x1F
GET_MIRROR_MODE, GET_MIRROR_MODE_ACK = 0x34, 0x35
GET_STATE, GET_STATE_ACK = 0x1C, 0x1D
HEART_BEAT, HEART_BEAT_ACK = 0x1A, 0x1B
OPEN, OPEN_ACK = 0x00, 0x01
NAMES = {DEVICE_ID: "DEVICE_ID", SAFETY_INFO: "SAFETY_INFO", SAFETY_INFO_ACK: "SAFETY_INFO_ACK",
         SAFETY_AUTH: "SAFETY_AUTH", SAFETY_AUTH_ACK: "SAFETY_AUTH_ACK",
         GET_DEVICE_INFO: "GET_DEVICE_INFO", GET_DEVICE_INFO_ACK: "GET_DEVICE_INFO_ACK",
         GET_MIRROR_MODE: "GET_MIRROR_MODE", GET_MIRROR_MODE_ACK: "GET_MIRROR_MODE_ACK",
         GET_STATE: "GET_STATE", GET_STATE_ACK: "GET_STATE_ACK",
         HEART_BEAT: "HEART_BEAT", HEART_BEAT_ACK: "HEART_BEAT_ACK",
         OPEN: "OPEN", OPEN_ACK: "OPEN_ACK", 0x12: "SET_MEDIA_INFO", 0x13: "SET_MEDIA_INFO_ACK",
         0x6C: "SET_MIRROR_KEY", 0x6D: "SET_MIRROR_KEY_ACK", 0x22: "NOTIFY",
         0x1E: "GET_DEVICE_INFO", 0x40: "SET_PLAY_SOURCE", 0x41: "SET_PLAY_SOURCE_ACK"}


def cname(cmd: int) -> str:
    return NAMES.get(cmd, f"0x{cmd:02x}")


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


def generate_auth_key(server: tuple[str, int], client: tuple[str, int]) -> str:
    """protocol.rs:1991. NOTE the order: receiver-local first, then peer."""
    raw = f"{server[0]}{server[1]}{client[0]}{client[1]}"
    return hashlib.md5("".join(chr(ord(c) + 0x31) if c.isdigit() else c for c in raw).encode()).hexdigest()


class ControlCipher:
    def __init__(self, key: bytes):
        self.key, self.iv = key, key

    def encrypt(self, plain: bytes) -> bytes:
        pad = 16 - (len(plain) % 16)
        padded = plain + b"\x00" * pad
        enc = Cipher(algorithms.AES(self.key), modes.CBC(self.iv)).encryptor()
        ct = enc.update(padded) + enc.finalize()
        self.iv = ct[-16:]
        return bytes([0x00, 0x07, 0x01, 0xE0, pad]) + miplay_integrity(ct).to_bytes(4, "big") + ct

    def decrypt(self, body: bytes) -> bytes:
        pad, ct, next_iv = body[4], body[9:], body[9:][-16:]
        if int.from_bytes(body[5:9], "big") != miplay_integrity(ct):
            raise ValueError("miplay integrity mismatch")
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


class Sender:
    def __init__(self, ip: str, label: str):
        self.ip, self.label = ip, label
        self.sock = socket.create_connection((ip, CONTROL_PORT), timeout=3.0)
        self.local, self.peer = self.sock.getsockname(), self.sock.getpeername()
        self.auth_key = generate_auth_key((self.peer[0], self.peer[1]), (self.local[0], self.local[1]))
        self.tx = ControlCipher(self.auth_key[:16].encode())
        self.rx = ControlCipher(self.auth_key[:16].encode())
        self.log: list[str] = []

    def note(self, msg: str) -> None:
        print(msg)
        self.log.append(msg)

    def send(self, outer: int, cmd: int, seq: int, body: bytes, tag: str = "") -> None:
        header = bytes([0x24, outer, cmd, (seq >> 8) & 0xFF, seq & 0xFF]) + struct.pack(">I", len(body))
        self.sock.sendall(header + body)
        self.note(f"  -> TX {cname(cmd)} outer=0x{outer:02x} seq={seq} len={len(body)} {tag}")

    def read(self, timeout: float = 2.0):
        self.sock.settimeout(timeout)
        header = b""
        while len(header) < 9:
            chunk = self.sock.recv(9 - len(header))
            if not chunk:
                raise ConnectionError("closed")
            header += chunk
        if header[0] != 0x24:
            raise ValueError(f"bad magic {header.hex()}")
        length = int.from_bytes(header[5:9], "big")
        body = b""
        while len(body) < length:
            chunk = self.sock.recv(length - len(body))
            if not chunk:
                raise ConnectionError("closed")
            body += chunk
        outer, cmd, seq = header[1], header[2], int.from_bytes(header[3:5], "big")
        encrypted = body[:4] == bytes([0x00, 0x07, 0x01, 0xE0])
        plain = self.rx.decrypt(body) if encrypted else body
        desc = ""
        try:
            if plain and plain[0] <= 8:
                key, data = unwrap_payload(plain)
                desc = f"wrap[{key}]={data[:250].decode('utf-8', 'replace')!r}"
            elif not encrypted:
                desc = f"plain={plain[:250].decode('utf-8', 'replace')!r}"
            else:
                desc = f"hex={plain[:64].hex()}"
        except Exception:
            desc = f"hex={plain[:64].hex()}"
        self.note(f"  <- RX {cname(cmd)} outer=0x{outer:02x} seq={seq} enc={encrypted} len={length} {desc}")
        return outer, cmd, seq, plain, encrypted

    def handshake(self) -> bool:
        self.note(f"\n=== [{self.label}] {self.ip}:{CONTROL_PORT} ===")
        self.note(f"  local={self.local} peer={self.peer} auth_key={self.auth_key} aes_key={self.auth_key[:16]!r}")
        try:
            outer, cmd, seq, plain, enc = self.read(1.5)
            if cmd != DEVICE_ID:
                self.note("  (no DEVICE_ID greeting)")
        except Exception as exc:
            self.note(f"  (no greeting: {exc!r})")
        self.sock.settimeout(None)

        si = ("{\n\t\"aesIvTypes\": \"3\",\n\t\"aesKeyTypes\": \"3\",\n"
              "\t\"authAlgorithmTypes\": \"7\",\n\t\"authKeyTypes\": \"1\",\n"
              "\t\"integrityTypes\": \"1\" \n} \n").encode()
        self.send(0x14, SAFETY_INFO, 1, wrap_payload("cmd", si), "compat SafetyInfo")
        client_challenge = hashlib.sha256(str(time.time()).encode()).hexdigest()[:32]
        sent = False
        deadline = time.time() + 6
        while time.time() < deadline:
            try:
                outer, cmd, seq, plain, enc = self.read(max(0.4, deadline - time.time()))
            except Exception:
                break
            if cmd == SAFETY_AUTH:
                try:
                    _, data = unwrap_payload(plain)
                    server_challenge = json.loads(data)["authMsg"]
                except Exception:
                    server_challenge = json.loads(plain)["authMsg"]
                body = ('{\n\t"authMsg": "%s" \n} \n' % client_challenge).encode()
                self.send(0x14, SAFETY_AUTH, 2, self.tx.encrypt(wrap_payload("cmd", body)), "client challenge")
                ack = hmac_sha256_hex(self.auth_key.encode(), server_challenge.encode())
                ack_body = ('{\n\t"authMsgAck": "%s",\n\t"result": "0" \n} \n' % ack).encode()
                self.send(0x14, SAFETY_AUTH_ACK, 0, self.tx.encrypt(wrap_payload("ack", ack_body)), f"hmac={ack[:16]}…")
                sent = True
            elif cmd == SAFETY_AUTH_ACK and sent:
                try:
                    _, data = unwrap_payload(plain)
                    payload = json.loads(data)
                except Exception:
                    payload = json.loads(plain)
                expected = hmac_sha256_hex(self.auth_key.encode(), client_challenge.encode())
                ok = payload.get("authMsgAck") == expected
                self.note(f"  ** MUTUAL AUTH {'VERIFIED' if ok else 'MISMATCH'} (result={payload.get('result')!r}) **")
                return ok
        self.note("  !! handshake did not complete")
        return False

    def probe_controls(self) -> None:
        for cmd, seq, label in ((GET_DEVICE_INFO, 3, "GET_DEVICE_INFO"), (GET_MIRROR_MODE, 4, "GET_MIRROR_MODE"),
                                (GET_STATE, 9, "GET_STATE"), (HEART_BEAT, 10, "HEART_BEAT")):
            self.send(0x00, cmd, seq, self.tx.encrypt(b""), label)
            try:
                self.read(2.5)
            except Exception as exc:
                self.note(f"     no reply to {label}: {exc!r}")
                break

    def open_stream(self, rtsp_port: int) -> None:
        url = f"wfd://{self.local[0]}:{rtsp_port}?mirrorMode=1"
        self.send(0x00, OPEN, 20, self.tx.encrypt(url.encode()), f"url={url!r}")
        deadline = time.time() + 4
        while time.time() < deadline:
            try:
                outer, cmd, seq, plain, enc = self.read(max(0.4, deadline - time.time()))
            except Exception:
                break
            if cmd == OPEN_ACK:
                self.note(f"     OPEN_ACK body hex={plain.hex()}")
        # keep the control socket alive while RTSP plays out
        time.sleep(6)


class RtspServer:
    """Minimal WFD/MiPlay RTSP *source* endpoint: logs what the receiver asks for."""

    def __init__(self):
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", 0))
        self.sock.listen(4)
        self.port = self.sock.getsockname()[1]
        self.requests: list[tuple[str, str]] = []
        self.sessions: dict[str, socket.socket] = {}
        self.stop = False

    def serve(self) -> None:
        self.sock.settimeout(0.5)
        while not self.stop:
            try:
                conn, addr = self.sock.accept()
            except (socket.timeout, TimeoutError):
                continue
            except OSError:
                break
            threading.Thread(target=self._handle, args=(conn, addr), daemon=True).start()

    def _handle(self, conn: socket.socket, addr) -> None:
        print(f"  >> RTSP CONNECTION from {addr}")
        self.requests.append(("CONNECT", str(addr)))
        conn.settimeout(6.0)
        buf = b""
        try:
            while not self.stop:
                try:
                    chunk = conn.recv(4096)
                except (socket.timeout, TimeoutError):
                    continue
                except OSError:
                    break
                if not chunk:
                    break
                buf += chunk
                while b"\r\n\r\n" in buf:
                    head, buf = buf.split(b"\r\n\r\n", 1)
                    text = head.decode("utf-8", "replace")
                    first = text.splitlines()[0] if text.splitlines() else ""
                    cseq = ""
                    session = ""
                    for line in text.splitlines()[1:]:
                        low = line.lower()
                        if low.startswith("cseq:"):
                            cseq = line.split(":", 1)[1].strip()
                        if low.startswith("session:"):
                            session = line.split(":", 1)[1].strip()
                    print(f"  >> RTSP [{addr[0]}] {first}  (cseq={cseq})")
                    print("     " + text.replace("\r\n", "\n     ")[:600])
                    self.requests.append((first, text))
                    reply = self._reply(first, cseq, session)
                    conn.sendall(reply)
                    print(f"  << RTSP reply {reply.splitlines()[0]}")
        except Exception as exc:
            print(f"  >> RTSP session error: {exc!r}")
        finally:
            conn.close()

    def _reply(self, request: str, cseq: str, session: str) -> bytes:
        method = request.split(" ")[0].upper()
        common = f"CSeq: {cseq}\r\n"
        if method == "OPTIONS":
            return (f"RTSP/1.0 200 OK\r\n{common}Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER\r\n"
                    "Require: org.wfa.wfd1.0\r\n\r\n").encode()
        if method == "GET_PARAMETER":
            body = "wfd_audio_codecs_v2: 15 3 3\r\nwfd_client_rtp_ports: RTP/AVP/TCP;interleaved mode=play\r\n"
            return (f"RTSP/1.0 200 OK\r\n{common}Content-Type: text/parameters\r\n"
                    f"Content-Length: {len(body)}\r\n\r\n{body}").encode()
        if method == "SETUP":
            return (f"RTSP/1.0 200 OK\r\n{common}Session: 12345678;timeout=60\r\n"
                    "Transport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n").encode()
        if method == "PLAY":
            return f"RTSP/1.0 200 OK\r\n{common}Session: 12345678\r\n\r\n".encode()
        return f"RTSP/1.0 200 OK\r\n{common}\r\n".encode()


def mdns_enumerate(timeout: float = 4.0) -> None:
    print("=== mDNS service types visible on this LAN ===")
    try:
        from zeroconf import ServiceBrowser, ServiceListener, Zeroconf, ZeroconfServiceTypes
        zc = Zeroconf()
        types = sorted(ZeroconfServiceTypes.find(zc=zc, timeout=timeout))
        for t in types:
            print(f"    - {t}")
        interesting = [t for t in types if any(k in t.lower() for k in ("lyra", "mi-", "xiaomi", "miot", "mijiia", "miplay", "wfd", "airplay", "raop", "googlecast"))]
        print(f"  relevant: {interesting}")

        found: dict[str, dict] = {}

        class L(ServiceListener):
            def add_service(self, zc, type_, name):
                info = zc.get_service_info(type_, name, timeout=2000)
                if info:
                    found[f"{name} {type_}"] = {
                        "type": type_, "name": name,
                        "addrs": [socket.inet_ntoa(a) for a in info.addresses],
                        "port": info.port, "server": info.server,
                        "txt": {k.decode(errors="replace"): (v.decode(errors="replace") if v else "")
                                for k, v in info.properties.items()},
                    }

            def update_service(self, zc, type_, name):
                self.add_service(zc, type_, name)

            def remove_service(self, zc, type_, name):
                pass

        for t in interesting or types:
            ServiceBrowser(zc, t, L())
        time.sleep(timeout)
        for key, rec in sorted(found.items()):
            print(f"  * {rec['name']}")
            print(f"      type={rec['type']} host={rec['server']} port={rec['port']} addrs={rec['addrs']}")
            for k, v in sorted(rec["txt"].items()):
                print(f"      TXT {k}={v!r}")
        zc.close()
    except Exception as exc:
        print(f"  mDNS enumeration failed: {exc!r}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--media", action="store_true", help="host an RTSP server and send OPEN")
    ap.add_argument("--target", default="192.168.1.171")
    ap.add_argument("--label", default="Xiaomi Sound L16B did=934947888")
    ap.add_argument("--both", action="store_true", help="handshake both speakers")
    args = ap.parse_args()

    print("MiPlay third-party SENDER feasibility probe — no audio, no config change")
    mdns_enumerate(4.0)
    targets = [("192.168.1.171", "Xiaomi Sound L16B did=934947888 (right)"),
               ("192.168.1.21", "Xiaomi Sound-左 L16A did=501718792 (left)")]
    if not args.both:
        targets = [(args.target, args.label)]

    results = []
    for ip, label in targets:
        try:
            s = Sender(ip, label)
        except Exception as exc:
            print(f"\n=== [{label}] {ip} connect FAILED {exc!r}")
            results.append((ip, label, False, False))
            continue
        ok = s.handshake()
        results.append((ip, label, True, ok))
        if ok:
            s.probe_controls()
        if ok and args.media and ip == args.target:
            server = RtspServer()
            threading.Thread(target=server.serve, daemon=True).start()
            print(f"  ** local RTSP source listening on 0.0.0.0:{server.port}")
            s.open_stream(server.port)
            server.stop = True
            print(f"  ** RTSP requests received: {len([r for r in server.requests if r[0] != 'CONNECT'])}")
            for req, _ in server.requests:
                print(f"     - {req}")
        try:
            s.sock.close()
        except Exception:
            pass

    print("\n=== SUMMARY ===")
    for ip, label, tcp, hs in results:
        print(f"  {ip:<15} {label[:42]:<44} tcp={tcp} handshake={hs}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
