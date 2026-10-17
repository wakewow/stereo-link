#!/usr/bin/env python3
"""Extract the exact request/response sequence a *known-good* AirPlay 2 client sent.

Source of truth: `research/lan/airplay2_stream_debug.log`, the DEBUG log of pyatv
0.18.0 successfully streaming a WAV to 192.168.1.21 (Xiaomi Sound-Left, L16A,
AirTunes/366.0). pyatv is the reference implementation we cross-check our Rust
sender (`crates/miplay-airplay`) against.

The log records every wire message as a Python bytes literal, so we can recover
the byte-exact bodies -- including the binary plists -- and decode them.

Offline: reads a file, opens no sockets.

Usage:
    python extract_reference_sequence.py airplay2_stream_debug.log [--json out.json]
"""

from __future__ import annotations

import argparse
import ast
import json
import plistlib
import re
import sys
from pathlib import Path

SEND = re.compile(r"Sending (?:RTSP/1\.0|HTTP/1\.1) message: (b'.*'|b\".*\")$")
RECV = re.compile(r"Received: (b'.*'|b\".*\")$")

# plist keys whose values are opaque blobs; keep them out of the readable dump.
BLOB_KEYS = {"shk", "pk", "pi", "deviceID", "gid", "streamConnectionID"}


def parse_message(raw: bytes) -> dict:
    """Split one wire message into start line, headers and body."""
    sep = raw.find(b"\r\n\r\n")
    head, body = (raw[:sep], raw[sep + 4 :]) if sep >= 0 else (raw, b"")
    lines = head.split(b"\r\n")
    start = lines[0].decode("utf-8", "replace")
    headers: dict[str, str] = {}
    for line in lines[1:]:
        if b":" in line:
            k, _, v = line.partition(b":")
            headers[k.decode("ascii", "replace").strip()] = v.decode("utf-8", "replace").strip()
    return {"start": start, "headers": headers, "body_len": len(body), "body": body}


def decode_body(body: bytes) -> object:
    """Binary plist, TLV, or text -- whatever the body is."""
    if body.startswith(b"bplist00"):
        try:
            return _readable(plistlib.loads(body))
        except Exception as exc:  # pragma: no cover - malformed body
            return f"<bplist parse failed: {exc}>"
    if body[:1] == b"\x06":  # HAP TLV: type, length, value...
        out = []
        i = 0
        while i + 2 <= len(body):
            t, ln = body[i], body[i + 1]
            out.append({"type": t, "len": ln, "value": body[i + 2 : i + 2 + ln].hex()[:64]})
            i += 2 + ln
        return {"hap_tlv": out}
    try:
        return body.decode("utf-8")
    except UnicodeDecodeError:
        return "<binary> " + body.hex()


def _readable(obj: object) -> object:
    """Replace crypto blobs with a length marker so the dump stays readable."""
    if isinstance(obj, dict):
        return {
            k: (f"<{len(v)} bytes>" if k in BLOB_KEYS and isinstance(v, bytes) else _readable(v))
            for k, v in obj.items()
        }
    if isinstance(obj, list):
        return [_readable(v) for v in obj]
    if isinstance(obj, bytes):
        return "<%d bytes>" % len(obj) if len(obj) > 8 else obj.hex()
    return obj


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("log", type=Path)
    ap.add_argument("--json", type=Path, default=None)
    args = ap.parse_args()

    exchanges: list[dict] = []
    pending: dict | None = None

    for line in args.log.read_text(encoding="utf-8", errors="replace").splitlines():
        m = SEND.search(line)
        if m:
            msg = parse_message(ast.literal_eval(m.group(1)))
            pending = {
                "request": msg["start"],
                "req_headers": msg["headers"],
                "req_body": decode_body(msg["body"]),
            }
            exchanges.append(pending)
            continue
        m = RECV.search(line)
        if m and pending is not None and "response" not in pending:
            msg = parse_message(ast.literal_eval(m.group(1)))
            pending["response"] = msg["start"]
            pending["resp_body"] = decode_body(msg["body"])
            pending = None

    for n, ex in enumerate(exchanges, 1):
        print("=== %2d. %s" % (n, ex["request"]))
        hdrs = {k: v for k, v in ex["req_headers"].items() if k.lower() != "content-length"}
        print("     headers : %s" % json.dumps(hdrs, ensure_ascii=False))
        body = ex["req_body"]
        if isinstance(body, dict):
            print("     body    : %s" % json.dumps(body, ensure_ascii=False, indent=None)[:1400])
        elif isinstance(body, str) and body.strip():
            print("     body    : %r" % body[:400])
        else:
            print("     body    : <%d bytes>" % ex["req_headers"].get("Content-Length", 0))
        print("     -> %s" % ex.get("response", "<no response logged>"))
        resp = ex.get("resp_body")
        if isinstance(resp, dict):
            print("        body : %s" % json.dumps(resp, ensure_ascii=False)[:900])
        elif isinstance(resp, str) and resp.strip():
            print("        body : %r" % resp[:200])

    if args.json:
        args.json.write_text(
            json.dumps(exchanges, ensure_ascii=False, indent=2, default=str), encoding="utf-8"
        )
        print("\nwrote %s (%d exchanges)" % (args.json, len(exchanges)), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
