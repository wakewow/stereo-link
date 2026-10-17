#!/usr/bin/env python3
"""Definitive test: replay the FULL captured HyperOS sender sequence against a real
Xiaomi speaker and see whether it accepts the media session.

Sequence (FusionPlay protocol.rs const table + captured test at protocol.rs:3060-3105):
  1. SafetyAuth handshake                       (transport auth, no Xiaomi account)
  2. SET_PLAY_SOURCE 0x40  {"ref_channel":"controlcenter","ref_function":"single_room","ref_content":"music_qq"}
  3. GET_MEDIA_INFO  0x14  (empty, encrypted)   -> expect NOTIFY 0x22
  4. ALONE GET_STATE: outer 0x04 cmd 0x16 {"getState":"1"} -> expect outer 0x04 cmd 0x17
  5. GET_STATE 0x1c / GET_DEVICE_INFO 0x1e / HEART_BEAT 0x1a
  6. SET_MIRROR_KEY 0x6c (authKey/streamKey/streamIV)
  7. OPEN 0x00 encrypted "wfd://<pc>:<rtsp_port>?mirrorMode=1" -> expect OPEN_ACK 0x01
     and, decisively, an INBOUND RTSP connection from the speaker.

No audio is streamed; no volume or setting is written.
"""
from __future__ import annotations

import json
import sys
import threading
import time

sys.path.insert(0, __file__.rsplit("\\", 1)[0])
from probe_miplay_sender import RtspServer, Sender, cname, wrap_payload  # noqa: E402

PLAY_SOURCE = b'{"ref_channel":"controlcenter","ref_function":"single_room","ref_content":"music_qq"}'
MIRROR_KEY = json.dumps({"authKey": "ARiFLajTddLuFa2X", "streamKey": "0123456789abcdef", "streamIV": "fedcba9876543210"}).encode()


def main() -> int:
    ip = sys.argv[1] if len(sys.argv) > 1 else "192.168.1.171"
    rtsp = RtspServer()
    threading.Thread(target=rtsp.serve, daemon=True).start()
    print(f"local RTSP source on 0.0.0.0:{rtsp.port}")

    s = Sender(ip, f"full-sender/{ip}")
    if not s.handshake():
        print("!! transport auth failed")
        return 1
    print("   transport auth OK -- now replaying the captured HyperOS sequence")

    steps = [
        (0x00, 0x40, 12, s.tx.encrypt(PLAY_SOURCE), "SET_PLAY_SOURCE"),
        (0x00, 0x14, 13, s.tx.encrypt(b""), "GET_MEDIA_INFO"),
        (0x04, 0x16, 11, s.tx.encrypt(b'{"getState":"1"}'), "ALONE_GET_STATE"),
        (0x00, 0x1C, 14, s.tx.encrypt(b""), "GET_STATE"),
        (0x00, 0x1E, 15, s.tx.encrypt(b""), "GET_DEVICE_INFO"),
        (0x00, 0x1A, 16, s.tx.encrypt(b""), "HEART_BEAT"),
        (0x00, 0x6C, 17, s.tx.encrypt(wrap_payload("cmd", MIRROR_KEY)), "SET_MIRROR_KEY"),
    ]
    replies: list[str] = []
    for outer, cmd, seq, body, tag in steps:
        try:
            s.send(outer, cmd, seq, body, tag)
        except Exception as exc:
            print(f"   !! send {tag} failed: {exc!r}")
            break
        # drain whatever comes back within a short window
        deadline = time.time() + 1.6
        while time.time() < deadline:
            try:
                o, c, sq, plain, enc = s.read(max(0.3, deadline - time.time()))
            except (TimeoutError,):
                break
            except Exception as exc:
                print(f"   <- peer closed/error after {tag}: {exc!r}")
                deadline = 0
                break
            tag_r = f"{cname(c)}" if o != 0x04 else f"ALONE_{cname(c)}(outer=0x04)"
            replies.append(f"{tag} -> {tag_r}")
            print(f"   <- ACK {tag_r} seq={sq} enc={enc}")

    url = f"wfd://{s.local[0]}:{rtsp.port}?mirrorMode=1".encode()
    print(f"   OPEN url = {url.decode()} (encrypted body)")
    try:
        s.send(0x00, 0x00, 18, s.tx.encrypt(url), "OPEN")
    except Exception as exc:
        print(f"   !! OPEN send failed: {exc!r}")

    print("   waiting up to 12 s for OPEN_ACK and/or an inbound RTSP connection ...")
    got_open_ack = False
    deadline = time.time() + 12
    while time.time() < deadline:
        try:
            o, c, sq, plain, enc = s.read(max(0.4, deadline - time.time()))
        except (TimeoutError,):
            pass
        except Exception as exc:
            print(f"   <- peer closed after OPEN: {exc!r}")
            break
        else:
            if c == 0x01 and o == 0x00:
                got_open_ack = True
                print("   ** OPEN_ACK received **")
            else:
                print(f"   <- frame {cname(c)} outer=0x{o:02x} seq={sq} enc={enc}")
        conns = [r for r in rtsp.requests if r[0] == "CONNECT"]
        if conns:
            break

    conns = [r for r in rtsp.requests if r[0] == "CONNECT"]
    print("\n===== RESULT =====")
    print(f"  pre-OPEN ACKs: {replies if replies else 'NONE'}")
    print(f"  OPEN_ACK: {got_open_ack}")
    print(f"  inbound RTSP connections: {len(conns)}")
    for r in rtsp.requests[:25]:
        print(f"    rtsp: {r[0]} {r[1] if len(r) > 1 else ''}")
    print("  ==> " + ("LAN DIRECT PUSH ACCEPTED (speaker dialled our RTSP source)" if conns
                      else "speaker did NOT open a media session"))
    try:
        s.sock.close()
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
