#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_airplay2.py -- find out WHY the plain RAOP ANNOUNCE got 403, and whether
the AirPlay-2 (protocolVersion 1.1) flow works without pairing.

Tests, in order (none of them starts audio, none writes a pairing record):
  A. OPTIONS with an Apple-Challenge header  -> is there an Apple-Response?
  B. /info again + decode the `features` bitfield and `statusFlags`
  C. POST /pair-verify with a *malformed* body -> only proves the endpoint
     exists and reports the error code. It cannot complete a pairing.
  D. AirPlay-2 SETUP with an application/x-apple-binary-plist body (the real
     AirPlay-2 way: no ANNOUNCE at all). If this returns 200 we get real
     dataPort/controlPort/timingPort without any pairing.
  E. LEGACY-style SETUP straight after ANNOUNCE 403 (some AirTunes builds
     reject ANNOUNCE but accept SETUP+RECORD).
Every stream is TORN DOWN at the end. RECORD is never sent, so no sound.

Usage:
    python probe_airplay2.py --ip 192.168.1.21 --out airplay2_probe.json
"""

import argparse
import base64
import json
import os
import plistlib
import random
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from probe_raop import Rtsp, parse_rtsp_reply, CLIENT_INSTANCE  # noqa: E402

# AirPlay feature bits we are confident about (RFC-less but widely used)
FEATURE_BITS = {
    0: "SupportsAirPlayVideoV1",
    1: "SupportsAirPlayPhoto",
    3: "SupportsAirPlayVideoFairPlay",
    4: "SupportsAirPlayVideoVolumeControl",
    5: "SupportsAirPlayVideoHTTPLiveStreaming",
    6: "SupportsAirPlayVideoSlideShow",
    7: "SupportsAirPlayScreen",
    8: "SupportsAirPlayAudio",
    9: "AudioReceiver",
    11: "SupportsAirPlayAudioFormatALAC",
    12: "SupportsAirPlayAudioFormatPCM",
    13: "SupportsAirPlayAudioFormatAAC",
    14: "Authentication4 (MFi auth required for legacy RAOP)",
    15: "MetadataFeatures: text",
    16: "MetadataFeatures: artwork",
    17: "MetadataFeatures: progress",
    18: "SupportsUnifiedMediaControl",
    19: "SupportsAirPlayVideoPlayQueue",
    20: "SupportsAirPlayFromCloud",
    21: "SupportsTLS_PSK",
    22: "SupportsAirPlayScreenMultiDisplay",
    23: "SupportsAirPlayAudioFormatAAC_LC",
    26: "SupportsHKPairingAndAccessControl",
    27: "SupportsLegacyPairing",
    30: "SupportsAirPlayPTP",
    32: "SupportsAirPlayScreenMultiDisplay2",
    33: "SupportsCoreUtilsPairing",
    34: "SupportsHKPairingAndAccessControl2",
    38: "SupportsBufferedAudio",
    40: "SupportsPTP",
    41: "SupportsAirPlayAudioFormatALAC_2",
    43: "SupportsAirPlayAudioFormatAAC_ELD",
    46: "SupportsAirPlayAudioFormatAAC_ELD_2",
    48: "SupportsAirPlayVideoPlayQueue2",
    49: "SupportsHomeKit",
}


def decode_features(value: int):
    out = []
    for bit, name in sorted(FEATURE_BITS.items()):
        if value >> bit & 1:
            out.append({"bit": bit, "mask": "0x%x" % (1 << bit), "name": name})
    return out


def airplay2_setup_plist(timing_port=0, control_port=0):
    ssrc = random.getrandbits(32)
    return {
        "timingPort": timing_port,
        "timingProtocol": "NTP",
        "deviceID": "AA:BB:CC:DD:EE:FF",
        "sessionUUID": "%08X-%04X-%04X-%04X-%012X" % (
            random.getrandbits(32), random.getrandbits(16), random.getrandbits(16),
            random.getrandbits(16), random.getrandbits(48)),
        "isScreenMirroringSession": False,
        "streams": [
            {
                "type": 96,                    # audio
                "audioFormat": 0x40000,        # ALAC
                "audioMode": 0x0,
                "controlPort": control_port,
                "ct": 1,                       # compression type: 1 = none
                "spf": 352,                    # samples per frame
                "sr": 44100,
                "ss": 16,
                "et": 0, "cn": 0, "shk": 0,
                "ssrc": ssrc,
            }
        ],
    }


def send_plist_verb(rtsp, verb, url, body: dict, read_seconds=4.0):
    payload = plistlib.dumps(body, fmt=plistlib.FMT_BINARY)
    req = ("%s %s RTSP/1.0\r\n%sContent-Type: application/x-apple-binary-plist\r\n"
           "Content-Length: %d\r\n\r\n" % (verb, url, rtsp.headers(), len(payload)))
    return req, rtsp.send_raw_bytes(req.encode() + payload, read_seconds=read_seconds)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--port", type=int, default=7000)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    out = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
           "target": "%s:%d" % (args.ip, args.port),
           "steps": [], "no_pairing_written": True, "no_audio_played": True}

    r = Rtsp(args.ip, args.port)
    r.connect()
    try:
        # ---------------- A. OPTIONS + Apple-Challenge ---------------------
        challenge = os.urandom(16)
        req, rep = r.options(extra={"Apple-Challenge": base64.b64encode(challenge).decode()})
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "A. OPTIONS + Apple-Challenge", "request": req,
                             "parsed": p, "challenge_hex": challenge.hex()})
        print("== A. OPTIONS + Apple-Challenge ==")
        print(rep.decode("utf-8", "replace")[:900])
        print("Apple-Response present: %s"
              % ("Apple-Response" in p["headers"]), flush=True)

        # ---------------- B. /info + feature decode ------------------------
        req, rep = r.get_info()
        p = parse_rtsp_reply(rep)
        pl = p.get("plist") or {}
        out["steps"].append({"step": "B. GET /info", "request": req, "parsed": p})
        feats = pl.get("features")
        out["features_raw"] = feats
        out["features_hex"] = ("0x%X" % feats) if isinstance(feats, int) else None
        out["features_bits"] = decode_features(feats) if isinstance(feats, int) else []
        print("\n== B. /info ==")
        print(json.dumps(pl, indent=1, default=str))
        print("features = %s = %s" % (out["features_hex"], feats))
        for b in out["features_bits"]:
            print("   bit %2d %-10s %s" % (b["bit"], b["mask"], b["name"]))
        need = {b["name"] for b in out["features_bits"]}
        print("   -> LegacyPairing? %s | CoreUtilsPairing? %s | PTP? %s"
              % (any("LegacyPairing" in n for n in need),
                 any("CoreUtilsPairing" in n for n in need),
                 any("PTP" in n for n in need)), flush=True)

        # ---------------- C. /pair-verify probe (cannot complete) ----------
        buf = b"\x00" * 4
        req = ("POST /pair-verify RTSP/1.0\r\n%sContent-Type: "
               "application/octet-stream\r\nContent-Length: %d\r\n\r\n"
               % (r.headers(), len(buf)))
        rep = r.send_raw_bytes(req.encode() + buf, read_seconds=3.0)
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "C. POST /pair-verify (malformed)",
                             "request": req, "parsed": p})
        print("\n== C. POST /pair-verify (malformed body) ==")
        print(rep.decode("utf-8", "replace")[:800], flush=True)

        # ---------------- D. AirPlay-2 SETUP with plist --------------------
        body = airplay2_setup_plist()
        url = "rtsp://%s:%d/%s" % (args.ip, args.port, CLIENT_INSTANCE)
        req, rep = send_plist_verb(r, "SETUP", url, body)
        p = parse_rtsp_reply(rep)
        out["steps"].append({"step": "D. AirPlay2 SETUP (plist)", "request": req,
                             "body_sent": body, "parsed": p})
        print("\n== D. AirPlay2 SETUP (application/x-apple-binary-plist) ==")
        print("--- request body ---")
        print(json.dumps(body, indent=1, default=str))
        print("--- reply ---")
        print(rep.decode("utf-8", "replace")[:1500], flush=True)
        if p.get("plist"):
            print("--- reply plist ---")
            print(json.dumps(p["plist"], indent=1, default=str)[:2000], flush=True)
            sess = p["plist"].get("streams") or []
            for st in sess:
                # remember control port so we can TEARDOWN properly
                pass
        airplay2_ok = p.get("code") == 200
        session = p["headers"].get("Session")
        if session:
            r.session = session.split(";")[0]
            tr = r.teardown(url)
            out["steps"].append({"step": "D2. TEARDOWN (airplay2 session)",
                                 "parsed": parse_rtsp_reply(tr[1])})
        # ---------------- E. legacy SETUP (no ANNOUNCE) --------------------
        r.session = None
        req, rep = r.setup(url=url + "/1")
        p2 = parse_rtsp_reply(rep)
        out["steps"].append({"step": "E. legacy SETUP (no ANNOUNCE)", "request": req,
                             "parsed": p2})
        print("\n== E. legacy SETUP without ANNOUNCE ==")
        print(rep.decode("utf-8", "replace")[:900], flush=True)
        if p2["headers"].get("Session"):
            r.session = p2["headers"]["Session"].split(";")[0]
            tr = r.teardown(url)
            out["steps"].append({"step": "E2. TEARDOWN",
                                 "parsed": parse_rtsp_reply(tr[1])})
    finally:
        r.close()

    out["verdict"] = {
        "apple_response_header": any(
            s["parsed"]["headers"].get("Apple-Response")
            for s in out["steps"] if "parsed" in s),
        "server": next((s["parsed"]["headers"].get("Server")
                        for s in out["steps"] if s["step"].startswith("A.")), None),
        "public": next((s["parsed"]["headers"].get("Public")
                        for s in out["steps"] if s["step"].startswith("A.")), None),
        "airplay2_setup_ok": any(s["step"].startswith("D.")
                                 and s["parsed"].get("code") == 200
                                 for s in out["steps"]),
        "legacy_setup_ok": any(s["step"].startswith("E.")
                               and s["parsed"].get("code") == 200
                               for s in out["steps"]),
        "codes": {s["step"]: s["parsed"].get("code") for s in out["steps"]},
    }
    dest = args.out or ("airplay2_probe_%s.json" % args.ip.replace(".", "_"))
    with open(dest, "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=2, ensure_ascii=False, default=str)
    print("\nverdict: %s" % json.dumps(out["verdict"], ensure_ascii=False, indent=1))
    print("report -> %s" % dest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
