#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_airplay2_stream.py -- END-TO-END: push audio to the speaker over the LAN.

Why this exists
---------------
probe_raop_session.py / probe_raop_state.py proved the *legacy* AirPlay-1 RAOP
flow (OPTIONS/ANNOUNCE/SETUP/SDP) does NOT work against this firmware:

    OPTIONS  (fresh conn) -> 200
    ANNOUNCE (fresh conn) -> 200     (L16/ALAC accepted, AAC -> 406)
    SETUP                 -> 455 Method Not Valid In This State
    any 2nd RTSP request on the same TCP connection -> 403 Forbidden

The advertised capability bits explain why.  `features=0xC05F8A00,0x1C340`
decoded with pyatv's AirPlayFlags table contains
SupportsCoreUtilsPairingAndEncryption (bit 48) and SupportsUnifiedMediaControl
(bit 38), and does NOT contain SupportsLegacyPairing (bit 27).  That makes pyatv
classify the device as AirPlay **V2** with **transient** credentials, i.e. the
AirPlay-2 flow: pair-verify (HAP transient pairing, PIN 3939, no user PIN) ->
SETUP carrying a binary plist -> ChaCha20-Poly1305 encrypted RTP.

This script drives that flow through pyatv 0.18 (already installed) as an
independent third-party sender, which is exactly what miplay-bridge must do in
Rust.  It:

  1. scans 192.168.1.21 and prints every service pyatv found,
  2. connects (AirPlay 2 / transient),
  3. lowers the volume, plays a short 440 Hz tone from a generated WAV,
  4. restores the original volume, and
  5. writes a DEBUG protocol log so the pair-verify / SETUP exchange is on
     record.

Safety: the tone is 3 seconds by default, the volume is read first and written
back in a finally block, and nothing persistent is created (transient pairing
stores no pairing record by design).
"""

import argparse
import asyncio
import json
import logging
import math
import os
import struct
import sys
import wave

LOG = None


def make_wav(path, seconds=3.0, freq=440.0, rate=44100, amplitude=0.12):
    """Generate a stereo 16-bit sine WAV (fade in/out so there is no click)."""
    n = int(rate * seconds)
    fade = int(rate * 0.05)
    frames = bytearray()
    for i in range(n):
        if i < fade:
            env = i / fade
        elif i > n - fade:
            env = max(0.0, (n - i) / fade)
        else:
            env = 1.0
        v = int(32767 * amplitude * env * math.sin(2 * math.pi * freq * i / rate))
        frames += struct.pack("<hh", v, v)
    with wave.open(path, "wb") as fh:
        fh.setnchannels(2)
        fh.setsampwidth(2)
        fh.setframerate(rate)
        fh.writeframes(bytes(frames))
    return path


RAOP_PROPS = {
    "cn": "0,1", "da": "true", "et": "0", "ft": "0xC05F8A00,0x1C340",
    "fv": "1.106.3", "md": "0,1,2", "am": "L16A", "sf": "0x4", "tp": "UDP",
    "vn": "65537", "vs": "366.0",
    "pk": "be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f",
}

AIRPLAY_PROPS = {
    "acl": "0", "deviceid": "5E:02:14:2B:B3:C6",
    "features": "0xC05F8A00,0x1C340", "rsf": "0x4", "fv": "1.106.3",
    "flags": "0x4", "model": "L16A", "manufacturer": "XiaoMi",
    "serialNumber": "34471/A1T321695", "protovers": "1.1", "srcvers": "366.0",
    "pi": "5E:02:14:2B:B3:C6", "gid": "5E:02:14:2B:B3:C6", "gcgl": "0",
    "pk": "be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f",
}


def manual_config(ip, name="Xiaomi Sound-1695"):
    """Build the pyatv config straight from the mDNS records we captured.

    pyatv's unicast scan cannot see this speaker: its `knock` only reached
    ports 3689/7000/49152/32498 and got no usable reply (see
    airplay2_stream_debug.log), and the device does not answer the unicast
    mDNS queries pyatv falls back on.  We already have the full DNS-SD record
    in mdns_summary.txt, so we hand pyatv the same data directly.
    """
    import ipaddress
    import pyatv.conf as pconf

    cfg = pconf.AppleTV(ipaddress.IPv4Address(ip), name)
    # credentials MUST stay None: pyatv then derives them itself via
    # auth.extract_credentials(), which returns TRANSIENT_CREDENTIALS because
    # `features` carries SupportsCoreUtilsPairingAndEncryption (bit 48).
    # Passing the literal string "transient" raises InvalidCredentialsError.
    cfg.add_service(pconf.RaopService(
        identifier="5E02142BB3C6", port=7000,
        credentials=None, properties=RAOP_PROPS))
    cfg.add_service(pconf.AirPlayService(
        identifier="5E:02:14:2B:B3:C6", port=7000,
        credentials=None, properties=AIRPLAY_PROPS))
    return cfg


async def run(args, result):
    import pyatv

    loop = asyncio.get_running_loop()

    print("=== 1. device config ===")
    if args.manual:
        conf = manual_config(args.ip)
        print("  built manually from the mDNS/DNS-SD records (see mdns_summary.txt)")
    else:
        try:
            confs = await pyatv.scan(loop, hosts=[args.ip], timeout=args.scan_timeout)
        except TypeError:
            confs = await pyatv.scan(hosts=[args.ip], timeout=args.scan_timeout)
        if not confs:
            result["error"] = "no device found"
            print("  NO DEVICE FOUND")
            return
        conf = confs[0]
    print("  name        : %s" % conf.name)
    print("  identifier  : %s" % conf.identifier)
    print("  address     : %s" % conf.address)
    services = []
    for svc in conf.services:
        info = {"protocol": str(svc.protocol), "port": svc.port,
                "credentials": svc.credentials,
                "pairing": str(getattr(svc, "pairing", None)),
                "requires_password": getattr(svc, "requires_password", None),
                "properties": dict(svc.properties)}
        services.append(info)
        print("  service %-10s port=%-6s pairing=%-12s credentials=%s"
              % (svc.protocol, svc.port, getattr(svc, "pairing", None), svc.credentials))
        for k in ("features", "ft", "pk", "pi", "deviceid", "model", "srcvers", "acl", "sf"):
            if k in svc.properties:
                print("      %-10s = %s" % (k, svc.properties[k]))
    result["services"] = services

    print("\n=== 2. connect (AirPlay 2 / transient pairing) ===")
    atv = await pyatv.connect(conf, loop)
    result["connected"] = True
    print("  connected")

    old_volume = None
    try:
        print("\n=== 3. volume: read -> lower -> restore ===")
        try:
            old_volume = atv.audio.volume          # plain property, NOT awaitable
            print("  current volume  : %s" % old_volume)
            result["volume_before"] = old_volume
        except Exception as exc:                                   # noqa: BLE001
            print("  volume read failed: %r" % (exc,))
            result["volume_read_error"] = repr(exc)
        try:
            await atv.audio.set_volume(args.volume)
            print("  set volume      : %s" % args.volume)
            result["volume_set"] = args.volume
        except Exception as exc:                                   # noqa: BLE001
            print("  volume set failed: %r" % (exc,))
            result["volume_set_error"] = repr(exc)

        print("\n=== 4. stream %s (%.1fs) ===" % (args.wav, args.seconds))
        try:
            import time
            t0 = time.monotonic()
            result["stream_start_monotonic"] = t0
            # stream_file lives on the Stream interface: atv.stream.stream_file()
            # (the facade itself has no stream_file attribute).
            await atv.stream.stream_file(args.wav)
            elapsed = time.monotonic() - t0
            result["stream_ok"] = True
            result["stream_return_after_s"] = round(elapsed, 3)
            print("  stream_file returned without error after %.3fs" % elapsed)
        except Exception as exc:                                   # noqa: BLE001
            result["stream_error"] = repr(exc)
            print("  stream_file FAILED: %r" % (exc,))
    finally:
        if old_volume is not None:
            try:
                await atv.audio.set_volume(old_volume)
                print("  volume restored to %s" % old_volume)
                result["volume_restored"] = old_volume
            except Exception as exc:                               # noqa: BLE001
                result["volume_restore_error"] = repr(exc)
                print("  volume restore FAILED: %r" % (exc,))
        try:
            atv.close()
            print("  connection closed")
        except Exception as exc:                                   # noqa: BLE001
            print("  close failed: %r" % (exc,))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--seconds", type=float, default=3.0)
    ap.add_argument("--freq", type=float, default=440.0)
    ap.add_argument("--volume", type=float, default=8.0,
                    help="volume percent used for the test tone (restored afterwards)")
    ap.add_argument("--scan-timeout", type=int, default=5)
    ap.add_argument("--manual", dest="manual", action="store_true", default=True,
                    help="build the pyatv config from our own mDNS records (default)")
    ap.add_argument("--scan", dest="manual", action="store_false",
                    help="use pyatv's own unicast scan instead")
    ap.add_argument("--out", default="airplay2_stream.json")
    ap.add_argument("--log", default="airplay2_stream_debug.log")
    args = ap.parse_args()

    global LOG
    args.wav = os.path.join(os.path.dirname(os.path.abspath(args.out)),
                            "tone_440_%.0fs.wav" % args.seconds)
    make_wav(args.wav, args.seconds, args.freq)
    print("generated %s" % args.wav)

    logging.basicConfig(
        level=logging.DEBUG,
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
        handlers=[logging.FileHandler(args.log, mode="w", encoding="utf-8")],
    )
    LOG = logging.getLogger("probe")

    result = {"ip": args.ip, "wav": args.wav, "seconds": args.seconds}
    try:
        asyncio.run(run(args, result))
    except Exception as exc:                                       # noqa: BLE001
        result["fatal"] = repr(exc)
        print("FATAL: %r" % (exc,))

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, ensure_ascii=False, default=str)
    print("\nreport -> %s" % args.out)
    print("debug log -> %s" % args.log)
    return 0


if __name__ == "__main__":
    sys.exit(main())
