#!/usr/bin/env python3
"""Which command does a real Xiaomi speaker accept as the FIRST post-auth frame?

Runs several variants, each in a fresh authenticated session:
  A SET_MIRROR_KEY first      (Xiaomi-account PC key exchange)
  B SET_DEVICE_INFO first
  C SET_MEDIA_INFO first
  D GET_VOLUME first          (read-only liveness)
  E OPEN(wfd://) first        (does the receiver dial our RTSP source?)

Reports replies, socket closes and any inbound RTSP connection.
Read-only: no audio is streamed; GET_VOLUME is a query.
"""

from __future__ import annotations

import json
import socket
import sys
import threading
import time

sys.path.insert(0, __file__.rsplit("\\", 1)[0])
from probe_miplay_sender import RtspServer, Sender, cname, wrap_payload  # noqa: E402

MIRROR_KEY = json.dumps({"authKey": "0123456789abcdef", "streamKey": "fedcba9876543210",
                         "streamIV": "abcdef0123456789"})
SOURCE_INFO = json.dumps({"sourceName": "miplay-bridge", "sourceType": "2", "sourceId": "probe"})
MEDIA_INFO = json.dumps({"title": "probe", "artist": "probe", "album": "probe", "duration": "10",
                         "position": "0", "playState": "1", "source": "local"})


def run_variant(ip: str, name: str, cmd: int, seq: int, body: bytes, outer: int, rtsp: RtspServer | None) -> dict:
    print(f"\n########## variant {name}: first post-auth frame = {cname(cmd)} (outer=0x{outer:02x}) ##########")
    out = {"variant": name, "replies": [], "closed": False}
    try:
        s = Sender(ip, name)
    except Exception as exc:
        print(f"  connect failed: {exc!r}")
        return out
    if not s.handshake():
        return out
    s.send(outer, cmd, seq, body, name)
    deadline = time.time() + 4.0
    while time.time() < deadline:
        try:
            outer_r, cmd_r, seq_r, plain, enc = s.read(max(0.4, deadline - time.time()))
            out["replies"].append(cname(cmd_r))
        except (socket.timeout, TimeoutError):
            break
        except ConnectionError:
            out["closed"] = True
            print("     !! peer CLOSED the socket")
            break
        except Exception as exc:
            print(f"     !! {exc!r}")
            break
    if not out["replies"] and not out["closed"]:
        print("     (no reply, socket still open)")
    # after a successful first frame, run the liveness query
    if out["replies"]:
        try:
            s.send(0x00, 0x0E, 30, s.tx.encrypt(b""), "GET_VOLUME follow-up")
            deadline = time.time() + 2.5
            while time.time() < deadline:
                outer_r, cmd_r, seq_r, plain, enc = s.read(max(0.4, deadline - time.time()))
                out["replies"].append(f"after:{cname(cmd_r)}")
        except Exception as exc:
            print(f"     follow-up GET_VOLUME: {exc!r}")
    if rtsp is not None:
        print(f"     waiting 5s for an inbound RTSP connection on port {rtsp.port} ...")
        time.sleep(5)
        conns = [r for r in rtsp.requests if r[0] == "CONNECT"]
        out["rtsp_connections"] = len(conns)
        print(f"     inbound RTSP connections: {len(conns)}")
    try:
        s.sock.close()
    except Exception:
        pass
    return out


def main() -> int:
    ip = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.171"
    rtsp = RtspServer()
    threading.Thread(target=rtsp.serve, daemon=True).start()
    print(f"local RTSP source listening on 0.0.0.0:{rtsp.port}")

    variants = [
        ("A_set_mirror_key", 0x6C, 3, lambda s: s.tx.encrypt(wrap_payload("cmd", MIRROR_KEY.encode())), 0x00, False),
        ("B_set_device_info", 0x58, 3, lambda s: s.tx.encrypt(wrap_payload("cmd", SOURCE_INFO.encode())), 0x00, False),
        ("C_set_media_info", 0x12, 3, lambda s: s.tx.encrypt(wrap_payload("cmd", MEDIA_INFO.encode())), 0x00, False),
        ("D_get_volume", 0x0E, 3, lambda s: s.tx.encrypt(b""), 0x00, False),
        ("E_open_wfd", 0x00, 3, lambda s: s.tx.encrypt(f"wfd://{s.local[0]}:{rtsp.port}?mirrorMode=1".encode()), 0x00, True),
        ("F_mirror_key_outer14", 0x6C, 3, lambda s: s.tx.encrypt(wrap_payload("cmd", MIRROR_KEY.encode())), 0x14, False),
    ]
    results = []
    for name, cmd, seq, make, outer, want_rtsp in variants:
        try:
            real = Sender(ip, name)
        except Exception as exc:
            print(f"connect failed for {name}: {exc!r}")
            continue
        if not real.handshake():
            continue
        body = make(real)
        print(f"  -> TX {cname(cmd)} outer=0x{outer:02x} seq={seq} len={len(body)}")
        real.send(outer, cmd, seq, body, name)
        rec = {"variant": name, "replies": [], "closed": False, "rtsp_connections": 0}
        deadline = time.time() + 4.0
        while time.time() < deadline:
            try:
                outer_r, cmd_r, seq_r, plain, enc = real.read(max(0.4, deadline - time.time()))
                rec["replies"].append(cname(cmd_r))
            except (socket.timeout, TimeoutError):
                break
            except ConnectionError:
                rec["closed"] = True
                print("     !! peer CLOSED the socket")
                break
            except Exception as exc:
                print(f"     !! {exc!r}")
                break
        if not rec["replies"] and not rec["closed"]:
            print("     (no reply, socket still open)")
        if rec["replies"]:
            try:
                real.send(0x00, 0x0E, 30, real.tx.encrypt(b""), "GET_VOLUME follow-up")
                deadline = time.time() + 2.5
                while time.time() < deadline:
                    outer_r, cmd_r, seq_r, plain, enc = real.read(max(0.4, deadline - time.time()))
                    rec["replies"].append(f"after:{cname(cmd_r)}")
            except Exception as exc:
                print(f"     follow-up: {exc!r}")
        if want_rtsp:
            print(f"     waiting 6s for an inbound RTSP connection on {rtsp.port} ...")
            time.sleep(6)
            conns = [r for r in rtsp.requests if r[0] == "CONNECT"]
            rec["rtsp_connections"] = len(conns)
            print(f"     inbound RTSP connections: {len(conns)}")
        try:
            real.sock.close()
        except Exception:
            pass
        results.append(rec)
        time.sleep(0.5)

    print("\n=== VARIANT SUMMARY ===")
    for r in results:
        print(f"  {r['variant']:<24} replies={r['replies'] or 'NONE':<28} closed={r['closed']} rtsp={r.get('rtsp_connections', 0)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
