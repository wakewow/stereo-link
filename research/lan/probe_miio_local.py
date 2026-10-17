#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_miio_local.py -- can we drive the speakers over the LAN *without* the
Xiaomi cloud?

Two independent questions:
  Q1. Does the local miIO protocol (UDP/54321) answer, and can we complete an
      authenticated session with the device token?
  Q2. Do we even have a device token? miIO needs the 16-byte device token
      (NOT the cloud login token). We look for it, read-only, in:
        - MiXPlay's .mi.token store
        - the cloud device_list response  (field name is 'token')
      The token VALUE is never printed and never written to disk.

Read-only on every config file: we copy bytes into memory and parse. Nothing is
written outside research/lan/, and no field of the token file is modified.

Usage:
    python probe_miio_local.py --out miio_local.json
"""

import argparse
import base64
import binascii
import hashlib
import json
import os
import socket
import struct
import sys
import time

from Crypto.Cipher import AES

IPS = {"192.168.1.171": "L16B (Xiaomi Sound, did=934947888)",
       "192.168.1.21": "L16A (Xiaomi Sound-左, did=501718792)"}

CONF_CANDIDATES = [
    r"C:\Users\Administrator\conf\.mi.token",
    r"D:\MiXPlay\conf\.mi.token",
    r"C:\Users\Administrator\conf\config.json",
]


def redact(tok):
    """Describe a secret without revealing it."""
    if tok is None:
        return None
    if isinstance(tok, str):
        tok = tok.encode()
    return {"len": len(tok), "sha256_8": hashlib.sha256(tok).hexdigest()[:8],
            "first_2_hex": tok[:2].hex(), "looks_like_miio_token": len(tok) == 16}


# ------------------------------------------------------------------ miIO wire
def miio_hello(ip, port=54321, timeout=2.0):
    pkt = bytes.fromhex("21310020" + "ff" * 28)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    try:
        s.sendto(pkt, (ip, port))
        data, addr = s.recvfrom(2048)
    except OSError as exc:
        s.close()
        return {"ok": False, "error": repr(exc)}
    s.close()
    if len(data) < 32 or data[:2] != b"\x21\x31":
        return {"ok": False, "error": "bad hello reply", "hex": data.hex()}
    return {
        "ok": True,
        "reply_hex": data.hex(),
        "device_id_hex": data[8:12].hex(),
        "device_id_int": struct.unpack("!I", data[8:12])[0],
        "stamp": struct.unpack("!I", data[12:16])[0],
        "token_from_hello_hex": data[16:32].hex(),
        "token_from_hello_is_ff": data[16:32] == b"\xff" * 16,
    }


def miio_encrypt(payload: bytes, token: bytes, did: int, stamp: int):
    key = hashlib.md5(token).digest()
    iv = hashlib.md5(key + token).digest()
    cipher = AES.new(key, AES.MODE_CBC, iv)
    pad = 16 - (len(payload) % 16)
    data = cipher.encrypt(payload + bytes([pad]) * pad)
    header = struct.pack("!HHII", 0x2131, 32 + len(data), 0, did) + \
        struct.pack("!I", stamp) + b"\x00" * 12
    # canonical miIO header is 16 bytes; rebuild precisely:
    header = struct.pack("!HH", 0x2131, 32 + len(data)) + struct.pack("!I", 0)
    header += struct.pack("!I", did) + struct.pack("!I", stamp)
    assert len(header) == 16, len(header)
    checksum = hashlib.md5(header + token + data).digest()
    return header + checksum + data


def miio_decrypt(pkt: bytes, token: bytes):
    if len(pkt) < 32:
        return {"ok": False, "error": "short packet"}
    key = hashlib.md5(token).digest()
    iv = hashlib.md5(key + token).digest()
    body = pkt[32:]
    try:
        plain = AES.new(key, AES.MODE_CBC, iv).decrypt(body)
    except Exception as exc:                                    # noqa: BLE001
        return {"ok": False, "error": repr(exc)}
    if not plain:
        return {"ok": False, "error": "empty plaintext"}
    pad = plain[-1]
    if 1 <= pad <= 16:
        plain = plain[:-pad]
    return {"ok": True, "plaintext": plain.decode("utf-8", "replace"),
            "plaintext_hex": plain.hex()[:400],
            "header_hex": pkt[:16].hex(), "checksum_hex": pkt[16:32].hex()}


def miio_call(ip, token, method, params=None, did=None, stamp=None,
              port=54321, timeout=4.0):
    params = params if params is not None else []
    h = miio_hello(ip, port)
    if not h.get("ok"):
        return {"ok": False, "stage": "hello", "hello": h}
    real_did = did if did is not None else h["device_id_int"]
    real_stamp = stamp if stamp is not None else h["stamp"]
    payload = json.dumps({"id": 1, "method": method, "params": params},
                         separators=(",", ":")).encode()
    pkt = miio_encrypt(payload, token, real_did, real_stamp)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    try:
        s.sendto(pkt, (ip, port))
        data, addr = s.recvfrom(4096)
    except OSError as exc:
        s.close()
        return {"ok": False, "stage": "call", "error": repr(exc),
                "request_hex": pkt.hex(), "method": method}
    s.close()
    dec = miio_decrypt(data, token)
    return {"ok": True, "method": method, "params": params,
            "request_hex": pkt.hex(), "reply_hex": data.hex(),
            "reply_len": len(data), "decrypted": dec}


# ------------------------------------------------------------------ token hunt
def read_token_file(path):
    """Read-only inspection of a MiXPlay token store. Returns descriptors only."""
    out = {"path": path, "exists": os.path.exists(path)}
    if not out["exists"]:
        return out
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError as exc:
        out["error"] = repr(exc)
        return out
    out["size"] = len(raw)
    try:
        j = json.loads(raw.decode("utf-8", "replace"))
    except Exception:                                           # noqa: BLE001
        out["note"] = "not JSON"
        return out
    out["top_level_keys"] = sorted(j.keys()) if isinstance(j, dict) else None

    def walk(node, path_=()):
        found = []
        if isinstance(node, dict):
            for k, v in node.items():
                p = path_ + (str(k),)
                if isinstance(v, str) and k.lower() in ("token", "device_token",
                                                        "devicetoken"):
                    found.append({"key_path": ".".join(p),
                                  "descriptor": redact(v)})
                found += walk(v, p)
        elif isinstance(node, list):
            for i, v in enumerate(node[:50]):
                found += walk(v, path_ + ("[%d]" % i,))
        return found

    out["token_like_fields"] = walk(j)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="miio_local.json")
    args = ap.parse_args()

    report = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
              "note": "token VALUES are never emitted; only descriptors",
              "hello": {}, "calls": [], "token_sources": []}

    print("=== 1. miIO hello (unauthenticated, proves the local service exists) ===")
    for ip in IPS:
        h = miio_hello(ip)
        report["hello"][ip] = h
        print("  %-16s %s" % (ip, IPS[ip]))
        print("      reply_hex            = %s" % h.get("reply_hex"))
        print("      device_id_int        = %s  (expected did %s)"
              % (h.get("device_id_int"),
                 "934947888" if ip.endswith("171") else "501718792"))
        print("      token in hello is FF?= %s  <- device will NOT hand us the token"
              % h.get("token_from_hello_is_ff"))
        print("      stamp                = %s" % h.get("stamp"))

    print("\n=== 2. where could a miIO device token come from? ===")
    for p in CONF_CANDIDATES:
        d = read_token_file(p)
        report["token_sources"].append(d)
        print("  %s" % p)
        print("      exists=%s size=%s" % (d.get("exists"), d.get("size")))
        if d.get("top_level_keys"):
            print("      top_level_keys=%s" % d["top_level_keys"])
        for t in d.get("token_like_fields") or []:
            print("      token-like field %s -> %s"
                  % (t["key_path"], json.dumps(t["descriptor"])))

    # ---- 3. cloud device_list: does it expose a per-device 'token'? ---------
    print("\n=== 3. cloud device_list: is there a per-device miIO token? ===")
    token_desc = None
    try:
        sys.path.insert(0, r"C:\Users\Administrator\AppData\Roaming\uv\tools"
                           r"\mixplay-hub\Lib\site-packages")
        cand = read_token_file(r"C:\Users\Administrator\conf\.mi.token")
        report["device_list_probe"] = {"token_file": cand}
        raw = open(r"C:\Users\Administrator\conf\.mi.token", "rb").read()
        store = json.loads(raw.decode("utf-8", "replace"))
        report["device_list_probe"]["store_keys"] = sorted(store.keys())
        report["device_list_probe"]["store_key_types"] = {
            k: type(v).__name__ for k, v in store.items()}
        report["device_list_probe"]["store_value_shapes"] = {
            k: (sorted(v.keys()) if isinstance(v, dict) else
                (len(v) if isinstance(v, (list, str)) else v))
            for k, v in store.items()}
        print("  store keys: %s" % sorted(store.keys()))
        for k, v in store.items():
            if isinstance(v, dict):
                print("    %s: dict keys=%s" % (k, sorted(v.keys())))
            elif isinstance(v, list):
                print("    %s: list len=%d" % (k, len(v)))
                if v and isinstance(v[0], dict):
                    print("       item0 keys=%s" % sorted(v[0].keys()))
            else:
                print("    %s: %s" % (k, redact(v) if isinstance(v, str) else v))

        # If the store holds a list of devices with 'token', report descriptors
        found_tokens = []

        def hunt(node, path_=()):
            if isinstance(node, dict):
                for k, v in node.items():
                    p = path_ + (str(k),)
                    if isinstance(v, str) and k.lower() in ("token", "did", "deviceid"):
                        found_tokens.append(((".".join(p)), k, redact(v)))
                    hunt(v, p)
            elif isinstance(node, list):
                for i, v in enumerate(node):
                    hunt(v, path_ + ("[%d]" % i,))

        hunt(store)
        report["device_list_probe"]["found"] = [
            {"key_path": p, "key": k, "descriptor": d} for p, k, d in found_tokens]
        print("  per-device-ish string fields found:")
        for p, k, d in found_tokens[:40]:
            print("    %-60s %s -> %s" % (p, k, json.dumps(d)))
    except Exception as exc:                                    # noqa: BLE001
        report["device_list_probe"] = {"error": repr(exc)}
        print("  FAILED: %r" % (exc,))

    # ---- 4. try an authenticated local call with any 16-byte token we saw ---
    print("\n=== 4. authenticated LOCAL miIO call (no cloud involved) ===")
    cands = []
    for src in report["token_sources"]:
        for t in src.get("token_like_fields") or []:
            if t["descriptor"].get("looks_like_miio_token"):
                cands.append(("<from %s>" % src["path"], t["descriptor"]))
    dlp = report.get("device_list_probe") or {}
    for f in dlp.get("found", []):
        if f["key"].lower() == "token" and f["descriptor"].get("looks_like_miio_token"):
            cands.append(("<from .mi.token %s>" % f["key_path"], f["descriptor"]))
    report["candidate_tokens"] = [{"source": s, "descriptor": d} for s, d in cands]
    print("  candidate 16-byte tokens: %d" % len(cands))
    for s, d in cands:
        print("    %s %s" % (s, json.dumps(d)))

    if not cands:
        print("  -> NO usable miIO device token found. Authenticated local miIO "
              "cannot be attempted with what is on disk.")
        report["authenticated_local_miio"] = {
            "attempted": False,
            "reason": "no 16-byte device token found in .mi.token or config.json",
        }
    else:
        # We do not have the plaintext here (only descriptors), so re-read the
        # one token we found and use it in-memory only.
        results = []
        try:
            store = json.loads(open(r"C:\Users\Administrator\conf\.mi.token",
                                    "rb").read().decode("utf-8", "replace"))

            def collect(node, out):
                if isinstance(node, dict):
                    for k, v in node.items():
                        if isinstance(v, str) and k.lower() == "token":
                            out.append(v)
                        collect(v, out)
                elif isinstance(node, list):
                    for v in node:
                        collect(v, out)

            toks = []
            collect(store, toks)
            toks = [t for t in toks if len(t) == 16]
            for ip in IPS:
                for i, tk in enumerate(toks):
                    r = miio_call(ip, tk.encode(), "miIO.info")
                    results.append({"ip": ip, "token_index": i, "result": r})
                    dec = (r.get("decrypted") or {})
                    print("  %-16s token#%d miIO.info -> ok=%s %s"
                          % (ip, i, r.get("ok"),
                             (dec.get("plaintext") or r.get("error") or "")[:200]))
                    if dec.get("plaintext"):
                        break
        except Exception as exc:                                # noqa: BLE001
            results = [{"error": repr(exc)}]
        report["authenticated_local_miio"] = {"attempted": True, "results": results}

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2, ensure_ascii=False, default=str)
    print("\nreport -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
