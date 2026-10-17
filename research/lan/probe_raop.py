#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_raop.py -- determine whether a device is a NATIVE RAOP/AirPlay RECEIVER
that accepts third-party pushes, as opposed to a PC-side receiver bridged
through the Xiaomi cloud.

Sequence (all per-session, NOTHING persistent is written to the device):
  1. TCP connect to 7000, RTSP  OPTIONS *             -> Public / Server headers
  2. RTSP  GET /info                                  -> AirPlay2 binary plist
  3. RTSP  ANNOUNCE  (SDP: ALAC 44100/16/2)           -> 200 means it will accept
                                                         a third-party stream
  4. RTSP  SETUP     (Transport RTP/AVP/UDP;mode=record) -> real audio ports
  5. RTSP  TEARDOWN  (cleanup; no RECORD was ever sent, so NO sound is produced)

Deliberately NOT done (would be persistent / destructive):
  - POST /pair-setup, /pair-verify   (writes a pairing record)
  - RECORD / SET_PARAMETER           (would actually play audio)
  - anything touching firmware, volume, EQ or the Xiaomi account

Usage:
    python probe_raop.py --ip 192.168.1.21 --out raop_probe.json
"""

import argparse
import base64
import binascii
import json
import os
import plistlib
import random
import socket
import sys
import time

DEFAULT_UA = "iTunes/12.9.5 (Windows; Microsoft Windows 10 x64) AppleWebKit/7604.400.15"
CLIENT_INSTANCE = "%016X" % random.getrandbits(64)


class Rtsp:
    def __init__(self, ip, port=7000, timeout=4.0):
        self.ip, self.port, self.timeout = ip, port, timeout
        self.sock = None
        self.cseq = 0
        self.session = None
        self.transcript = []

    def connect(self):
        self.sock = socket.create_connection((self.ip, self.port), timeout=self.timeout)
        self.sock.settimeout(self.timeout)
        return self

    def close(self):
        if self.sock:
            try:
                self.sock.close()
            except OSError:
                pass

    def send_raw(self, text: str, read_seconds=2.5, expect_reply=True):
        """Send literal RTSP text, return the raw byte reply (may be empty)."""
        return self.send_raw_bytes(text.encode(), read_seconds=read_seconds,
                                   expect_reply=expect_reply)

    def send_raw_bytes(self, raw: bytes, read_seconds=2.5, expect_reply=True):
        """Send raw bytes (headers + optional binary plist body)."""
        rec = {"sent": raw[:1500].decode("utf-8", "replace"),
               "sent_hex": raw.hex()[:2000], "sent_len": len(raw),
               "reply": None, "reply_hex": None, "t": time.time()}
        self.transcript.append(rec)
        try:
            self.sock.sendall(raw)
        except OSError as exc:
            rec["send_error"] = repr(exc)
            return b""
        if not expect_reply:
            return b""
        buf = b""
        deadline = time.time() + read_seconds
        self.sock.settimeout(0.4)
        while time.time() < deadline:
            try:
                chunk = self.sock.recv(8192)
            except socket.timeout:
                if buf and (b"\r\n\r\n" in buf):
                    # got complete headers; keep draining briefly for plist body
                    continue
                if not buf:
                    continue
            except OSError as exc:
                rec["recv_error"] = repr(exc)
                break
            if not chunk:
                break
            buf += chunk
            if b"Content-Length" in buf:
                try:
                    hdr, _, body = buf.partition(b"\r\n\r\n")
                    need = 0
                    for line in hdr.split(b"\r\n"):
                        if line.lower().startswith(b"content-length:"):
                            need = int(line.split(b":")[1].strip())
                    if len(body) >= need:
                        break
                except Exception:                               # noqa: BLE001
                    pass
            elif b"\r\n\r\n" in buf:
                break
        rec["reply"] = buf.decode("utf-8", "replace")[:4000]
        rec["reply_hex"] = buf[:400].hex()
        rec["reply_len"] = len(buf)
        return buf

    # ---- RTSP verbs -------------------------------------------------------
    def headers(self, extra=None):
        self.cseq += 1
        h = {
            "CSeq": str(self.cseq),
            "User-Agent": DEFAULT_UA,
            "Client-Instance": CLIENT_INSTANCE,
            "DACP-ID": "%016X" % random.getrandbits(64),
            "Active-Remote": str(random.getrandbits(32)),
        }
        if self.session:
            h["Session"] = self.session
        if extra:
            h.update(extra)
        return "".join("%s: %s\r\n" % (k, v) for k, v in h.items())

    def options(self, url="*", extra=None):
        req = "OPTIONS %s RTSP/1.0\r\n%s\r\n" % (url, self.headers(extra))
        return req, self.send_raw(req)

    def get_info(self):
        req = "GET /info RTSP/1.0\r\n%s\r\n" % self.headers()
        return req, self.send_raw(req, read_seconds=3.0)

    def announce(self, sdp, url=None, extra=None):
        url = url or self.rtsp_url()
        req = ("ANNOUNCE %s RTSP/1.0\r\n%sContent-Type: application/sdp\r\n"
               "Content-Length: %d\r\n\r\n%s"
               % (url, self.headers(extra), len(sdp), sdp))
        return req, self.send_raw(req)

    def setup(self, extra=None, url=None):
        url = url or self.rtsp_url() + "/1"
        req = ("SETUP %s RTSP/1.0\r\n%sTransport: RTP/AVP/UDP;unicast;"
               "mode=record;control;timing\r\n\r\n" % (url, self.headers(extra)))
        return req, self.send_raw(req)

    def teardown(self, url=None):
        url = url or self.rtsp_url()
        req = "TEARDOWN %s RTSP/1.0\r\n%s\r\n" % (url, self.headers())
        return req, self.send_raw(req, read_seconds=2.0)

    def rtsp_url(self):
        host = self.ip
        return "rtsp://%s:%d/%s" % (host, self.port, CLIENT_INSTANCE)


def parse_rtsp_reply(buf: bytes):
    out = {"status_line": None, "code": None, "headers": {}, "body_len": 0,
           "body_hex": None, "body_ascii": None, "plist": None}
    if not buf:
        out["error"] = "empty reply"
        return out
    head, _, body = buf.partition(b"\r\n\r\n")
    lines = head.decode("utf-8", "replace").split("\r\n")
    out["status_line"] = lines[0] if lines else ""
    try:
        out["code"] = int(lines[0].split()[1])
    except Exception:                                           # noqa: BLE001
        pass
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            out["headers"][k.strip()] = v.strip()
    out["body_len"] = len(body)
    out["body_hex"] = body[:200].hex()
    out["body_ascii"] = body[:600].decode("utf-8", "replace")
    if body.startswith(b"bplist00"):
        try:
            pl = plistlib.loads(body)
            out["plist"] = pl
        except Exception as exc:                                # noqa: BLE001
            out["plist_error"] = repr(exc)
    return out


SDP = (
    "v=0\r\n"
    "o=iTunes {ci} 0 IN IP4 {ip}\r\n"
    "s=iTunes\r\n"
    "c=IN IP4 {ip}\r\n"
    "t=0 0\r\n"
    "m=audio 0 RTP/AVP 96\r\n"
    "a=rtpmap:96 AppleLossless\r\n"
    "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n"
    "a=rsaaeskey:{key}\r\n"
    "a=aesiv:{iv}\r\n"
)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--port", type=int, default=7000)
    ap.add_argument("--out", default=None)
    ap.add_argument("--apple-challenge", action="store_true",
                    help="also send an Apple-Challenge header on OPTIONS")
    args = ap.parse_args()

    out = {
        "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "target": "%s:%d" % (args.ip, args.port),
        "client_instance": CLIENT_INSTANCE,
        "user_agent": DEFAULT_UA,
        "steps": [],
        "nothing_persistent_written": True,
        "no_audio_played": True,
    }

    r = Rtsp(args.ip, args.port)
    try:
        r.connect()
    except OSError as exc:
        out["connect_error"] = repr(exc)
        print(json.dumps(out, indent=1))
        return 1

    try:
        # --- 1. OPTIONS ----------------------------------------------------
        extra = None
        if args.apple_challenge:
            challenge = os.urandom(16)
            extra = {"Apple-Challenge": base64.b64encode(challenge).decode()}
            out["apple_challenge_b64"] = extra["Apple-Challenge"]
            out["apple_challenge_hex"] = challenge.hex()
        req, rep = r.options(extra=extra)
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "OPTIONS", "request": req, "parsed": p})
        print("== OPTIONS ==\n%s--- reply ---\n%s"
              % (req, rep.decode("utf-8", "replace")[:1500]), flush=True)

        # --- 2. GET /info --------------------------------------------------
        req, rep = r.get_info()
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "GET /info", "request": req, "parsed": p})
        print("== GET /info ==\n%s--- reply ---\n%s"
              % (req, rep.decode("utf-8", "replace")[:1500]), flush=True)
        if p.get("plist"):
            print("-- plist --\n%s"
                  % json.dumps(p["plist"], indent=1, default=str)[:2500], flush=True)

        # --- 3. ANNOUNCE (plain, no crypto) --------------------------------
        sdp_plain = SDP.format(ci=random.getrandbits(32), ip="192.168.1.4",
                               key="", iv="")
        sdp_plain = "\r\n".join(l for l in sdp_plain.split("\r\n")
                                if l and not l.startswith("a=rsaaeskey")
                                and not l.startswith("a=aesiv")) + "\r\n"
        req, rep = r.announce(sdp_plain)
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "ANNOUNCE (plain ALAC 44100/16/2)",
                             "request": req, "parsed": p})
        print("== ANNOUNCE ==\n%s--- reply ---\n%s"
              % (req, rep.decode("utf-8", "replace")[:1500]), flush=True)
        announce_ok = p.get("code") == 200

        # --- 4. SETUP (only if ANNOUNCE was accepted) ----------------------
        if announce_ok:
            req, rep = r.setup()
            p2 = parse_rtsp_reply(rep)
            out["steps"].append({"step": "SETUP", "request": req, "parsed": p2})
            print("== SETUP ==\n%s--- reply ---\n%s"
                  % (req, rep.decode("utf-8", "replace")[:1500]), flush=True)
            if p2["headers"].get("Session"):
                r.session = p2["headers"]["Session"].split(";")[0]
            # --- 5. TEARDOWN (no RECORD => silence) ------------------------
            req, rep = r.teardown()
            p3 = parse_rtsp_reply(rep)
            out["steps"].append({"step": "TEARDOWN", "request": req, "parsed": p3})
            print("== TEARDOWN ==\n--- reply ---\n%s"
                  % rep.decode("utf-8", "replace")[:800], flush=True)
        else:
            print("!! ANNOUNCE was rejected -> device will NOT accept a plain "
                  "third-party RAOP push on this session", flush=True)
    finally:
        r.close()

    out["transcript"] = r.transcript
    out["verdict"] = {
        "options_public": next((s["parsed"]["headers"].get("Public")
                                for s in out["steps"] if s["step"] == "OPTIONS"),
                               None),
        "server_header": next((s["parsed"]["headers"].get("Server")
                               for s in out["steps"] if s["step"] == "OPTIONS"),
                              None),
        "device_is_raop_receiver": bool(out["steps"][0]["parsed"].get("code")),
        "announce_accepted": announce_ok,
    }
    dest = args.out or ("raop_probe_%s.json" % args.ip.replace(".", "_"))
    with open(dest, "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=2, ensure_ascii=False, default=str)
    print("\nverdict: %s" % json.dumps(out["verdict"], ensure_ascii=False))
    print("report -> %s" % dest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
