#!/usr/bin/env python3
"""Measure how a Xiaomi speaker pulls an HTTP audio URL (the cloud play_url path).

Method: host a WAV on this PC, ask MiXPlay to play it on a speaker, and record the
speaker's own HTTP client behaviour: connect time, request line, headers, Range usage,
inter-chunk pacing, total bytes, whether it reads the whole file up-front (= big buffer)
or at roughly real-time rate (= streaming).

Usage: probe_speaker_pull.py <speaker_ip> <did> [mode]
  mode = length (default, Content-Length) | chunked (HTTP/1.1 chunked, no length)
Read-only w.r.t. the speaker: it only plays <=6 s of a quiet tone; volume is untouched.
"""
from __future__ import annotations

import http.server
import json
import math
import socket
import struct
import sys
import threading
import time
import urllib.request

PORT = 8788
SECONDS = 6.0
RATE = 44100
FREQ = 440.0
AMP = 600  # quiet


def wav_bytes(seconds: float = SECONDS) -> bytes:
    n = int(RATE * seconds)
    frames = bytearray()
    for i in range(n):
        v = int(AMP * math.sin(2 * math.pi * FREQ * i / RATE))
        frames += struct.pack("<h", v)
    data = bytes(frames)
    hdr = b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVEfmt " + struct.pack("<IHHIIHH", 16, 1, 1, RATE, RATE * 2, 2, 16)
    return hdr + b"data" + struct.pack("<I", len(data)) + data


WAV = wav_bytes()
LOG: list[tuple[float, str]] = []
T0 = time.time()


def note(msg: str) -> None:
    LOG.append((time.time() - T0, msg))
    print(f"  [{time.time() - T0:7.3f}s] {msg}", flush=True)


MODE = "length"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence
        pass

    def do_HEAD(self):
        note(f"HEAD {self.path} from {self.client_address[0]} ua={self.headers.get('User-Agent')!r}")
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(WAV)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        note(f"GET {self.path} from {self.client_address[0]}")
        for k, v in self.headers.items():
            note(f"    hdr {k}: {v}")
        rng = self.headers.get("Range")
        body = WAV
        status = 200
        if rng and rng.startswith("bytes="):
            spec = rng[6:].split("-")
            start = int(spec[0]) if spec[0] else 0
            end = int(spec[1]) if len(spec) > 1 and spec[1] else len(WAV) - 1
            body = WAV[start:end + 1]
            status = 206
            note(f"    RANGE -> {start}-{end} ({len(body)} bytes)")
        self.send_response(status)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Accept-Ranges", "bytes")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(WAV)}")
        if MODE == "chunked":
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            chunk = RATE * 2 // 10  # 100 ms of PCM per chunk
            sent = 0
            last = time.time()
            try:
                while sent < len(body):
                    part = body[sent:sent + chunk]
                    self.wfile.write(b"%x\r\n" % len(part) + part + b"\r\n")
                    sent += len(part)
                    now = time.time()
                    note(f"    sent {sent}/{len(body)} bytes (pace {now - last:.3f}s/chunk)")
                    last = now
                    time.sleep(0.1)  # real-time pacing: does the speaker tolerate it?
                self.wfile.write(b"0\r\n\r\n")
                note("    chunked upload complete")
            except Exception as exc:
                note(f"    write aborted: {exc!r}")
        else:
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
                note(f"    wrote {len(body)} bytes in one shot (Content-Length path)")
            except Exception as exc:
                note(f"    write aborted: {exc!r}")
        note("    handler done")


def play(url: str, did: str) -> None:
    req = urllib.request.Request(
        "http://127.0.0.1:8820/api/v1/control/play",
        data=json.dumps({"action": "play_url", "target": did, "url": url}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=10) as resp:
        note(f"play_url HTTP {resp.status}: {resp.read()[:200]!r}")


def main() -> int:
    global MODE
    ip = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.171"
    did = sys.argv[2] if len(sys.argv) > 2 else "934947888"
    MODE = sys.argv[3] if len(sys.argv) > 3 else "length"
    note(f"mode={MODE} wav={len(WAV)} bytes ({SECONDS}s of {RATE}Hz mono 16-bit)")
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = f"http://192.168.1.4:{PORT}/t.wav"
    note(f"serving {url}")
    note("issuing play_url via MiXPlay ...")
    try:
        play(url, did)
    except Exception as exc:
        note(f"play_url failed: {exc!r}")
    t_end = time.time() + (SECONDS + 8 if MODE == "length" else SECONDS + 14)
    while time.time() < t_end:
        time.sleep(0.5)
    srv.shutdown()
    conns = [l for l in LOG if "GET " in l[1] or "HEAD " in l[1]]
    note(f"RESULT mode={MODE}: http_requests={len(conns)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
