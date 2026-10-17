#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_raop_variants.py -- why does ANNOUNCE get 403?

probe_raop_session.py proved the speaker answers OPTIONS with 200 but rejects a
bare AirPlay-1 ANNOUNCE with 403 Forbidden.  This script narrows down the cause
by (a) reading the device's own /info document and (b) trying the ANNOUNCE
variants a real Apple sender would use.

Everything here is read-only / non-persistent: we never complete a pairing, we
never change device state, and every RTSP attempt is followed by TEARDOWN.

Usage:
    python probe_raop_variants.py --ip 192.168.1.21 --out raop_variants.json
"""

import argparse
import base64
import json
import os
import socket
import sys
import time

HOST = None


def rtsp_raw(ip, port, raw, read_timeout=5.0, max_bytes=65536):
    """Send raw bytes, return everything the peer says."""
    s = socket.create_connection((ip, port), read_timeout)
    s.settimeout(read_timeout)
    s.sendall(raw)
    buf = b""
    try:
        while len(buf) < max_bytes:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
            if b"\r\n\r\n" in buf:
                head, _, body = buf.partition(b"\r\n\r\n")
                clen = 0
                for ln in head.decode("utf-8", "replace").split("\r\n"):
                    if ln.lower().startswith("content-length:"):
                        try:
                            clen = int(ln.split(":", 1)[1].strip())
                        except ValueError:
                            clen = 0
                if len(body) >= clen:
                    break
    except socket.timeout:
        pass
    s.close()
    return buf.decode("utf-8", "replace")


def get_info(ip, port=7000):
    """Apple senders GET /info before streaming.  Try both RTSP and HTTP."""
    out = {}
    for label, raw in (
        ("GET /info RTSP/1.0",
         "GET /info RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: AirPlay/366.0\r\n"
         "X-Apple-ProtocolVersion: 1\r\nContent-Length: 0\r\n\r\n"),
        ("GET /info HTTP/1.1",
         "GET /info HTTP/1.1\r\nHost: %s\r\nUser-Agent: AirPlay/366.0\r\n"
         "X-Apple-ProtocolVersion: 1\r\nConnection: close\r\n\r\n" % ip),
    ):
        try:
            out[label] = rtsp_raw(ip, port, raw.encode())
        except OSError as exc:
            out[label] = "ERROR %r" % (exc,)
    return out


def announce(ip, url, sdp, extra_headers=None, port=7000):
    hdrs = {"CSeq": "1", "Content-Type": "application/sdp",
            "User-Agent": "AirPlay/366.0", "Client-Instance": "0123456789abcdef"}
    if extra_headers:
        hdrs.update(extra_headers)
    body = sdp.encode()
    hdrs["Content-Length"] = str(len(body))
    raw = ("ANNOUNCE %s RTSP/1.0\r\n" % url
           + "".join("%s: %s\r\n" % (k, v) for k, v in hdrs.items())
           + "\r\n").encode() + body
    return rtsp_raw(ip, port, raw)


def sdp_l16(ip, extra=""):
    return ("v=0\r\no=iTunes 1 0 IN IP4 %s\r\ns=iTunes\r\nc=IN IP4 %s\r\nt=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\na=rtpmap:96 L16/44100/2\r\na=fmtp:96 0\r\n"
            "a=ptime:352\r\n%s" % (HOST, ip, extra))


def sdp_alac(ip, extra=""):
    return ("v=0\r\no=iTunes 1 0 IN IP4 %s\r\ns=iTunes\r\nc=IN IP4 %s\r\nt=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n"
            "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n%s" % (HOST, ip, extra))


def sdp_aac(ip, extra=""):
    return ("v=0\r\no=iTunes 1 0 IN IP4 %s\r\ns=iTunes\r\nc=IN IP4 %s\r\nt=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AAC/44100/2\r\n"
            "a=fmtp:96 0\r\na=ptime:352\r\n%s" % (HOST, ip, extra))


def main():
    global HOST
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--out", default="raop_variants.json")
    args = ap.parse_args()

    # local IP used in the SDP origin
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    probe.connect((args.ip, 7000))
    HOST = probe.getsockname()[0]
    probe.close()

    result = {"ip": args.ip, "local_ip": HOST,
              "started": time.strftime("%Y-%m-%dT%H:%M:%S%z")}

    print("=== A. device /info document (read-only) ===")
    info = get_info(args.ip)
    result["info"] = info
    for k, v in info.items():
        print("--- %s ---" % k)
        print(v[:3000])
        print()

    print("=== B. ANNOUNCE variants ===")
    url_ip = "rtsp://%s/1" % args.ip
    challenge = base64.b64encode(os.urandom(16)).decode()
    variants = [
        ("bare L16", url_ip, sdp_l16(args.ip), None),
        ("L16 + Apple-Challenge", url_ip, sdp_l16(args.ip),
         {"Apple-Challenge": challenge}),
        ("ALAC", url_ip, sdp_alac(args.ip), None),
        ("ALAC + Apple-Challenge", url_ip, sdp_alac(args.ip),
         {"Apple-Challenge": challenge}),
        ("AAC", url_ip, sdp_aac(args.ip), None),
        ("ALAC + Apple-Challenge + X-Apple headers", url_ip, sdp_alac(args.ip),
         {"Apple-Challenge": challenge,
          "X-Apple-Device-ID": "5E:02:14:2B:B3:C6",
          "X-Apple-Session-ID": "00000000-0000-0000-0000-000000000001",
          "X-Apple-ProtocolVersion": "1"}),
        ("L16 + X-Apple headers", url_ip, sdp_l16(args.ip),
         {"X-Apple-Device-ID": "5E:02:14:2B:B3:C6",
          "X-Apple-ProtocolVersion": "1"}),
        ("ALAC, url=rtsp://ip/<sessionid>", "rtsp://%s/%d" % (args.ip, int(time.time())),
         sdp_alac(args.ip), {"Apple-Challenge": challenge}),
        ("ALAC, c=IN IP4 0.0.0.0", url_ip,
         sdp_alac(args.ip).replace("c=IN IP4 %s" % args.ip, "c=IN IP4 0.0.0.0"),
         {"Apple-Challenge": challenge}),
    ]
    rows = []
    for name, url, sdp, hdrs in variants:
        try:
            resp = announce(args.ip, url, sdp, hdrs)
        except OSError as exc:
            resp = "ERROR %r" % (exc,)
        first = resp.split("\r\n")[0] if resp else "(empty)"
        rows.append({"variant": name, "url": url, "response": resp})
        print("  %-42s -> %s" % (name, first))
    result["announce_variants"] = rows

    print("\n=== C. full text of the non-200 replies (for the record) ===")
    for r in rows:
        if "200 OK" not in r["response"].split("\r\n")[0]:
            print("--- %s ---" % r["variant"])
            print(r["response"][:1500])
            print()

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, ensure_ascii=False, default=str)
    print("report -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
