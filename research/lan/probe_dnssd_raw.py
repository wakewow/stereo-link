#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_dnssd_raw.py -- dependency-free, raw DNS-SD scanner.

Why: zeroconf's ServiceBrowser can silently swallow records. This script speaks
raw mDNS on UDP/5353, with a real DNS message parser, so that every fact in
lan-paths.md can be traced to an actual wire record (name / type / class / TTL /
rdata / source IP).

Protocol used (RFC 6762 / RFC 6763):
  1. Q1: PTR  _services._dns-sd._udp.local.      -> list every advertised type
  2. Q2: PTR  <type>                             -> list instance names
  3. Q3: SRV+TXT+A/AAAA <instance>               -> port, host, target, props
Each query is sent twice, 250 ms apart, with the QU (unicast-response) bit
CLEARED so responders multicast and we can also collect answers addressed to
other queriers on the link.

Read-only. Usage:
    python probe_dnssd_raw.py --out dnssd_raw.json [--seconds 4]
"""

import argparse
import json
import random
import socket
import struct
import sys
import time

MDNS = ("224.0.0.251", 5353)

T_NEW_QTYPE = {12: "PTR", 16: "TXT", 33: "SRV", 1: "A", 28: "AAAA", 47: "NSEC",
               255: "ANY"}
T_QTYPE = dict(T_NEW_QTYPE)
T_QTYPE[41] = "OPT"

CLASS_TOP = {1: "IN", 255: "ANY"}


# ------------------------------------------------------------------ wire parse
def _name(buf: bytes, off: int):
    """Decode a (possibly compressed) DNS name. Returns (name, next_offset)."""
    labels = []
    jumped = False
    nxt = off
    hops = 0
    while True:
        if off >= len(buf):
            raise ValueError("name runs off end of packet")
        ln = buf[off]
        if ln == 0:
            off += 1
            if not jumped:
                nxt = off
            break
        if ln & 0xC0 == 0xC0:                     # compression pointer
            if off + 1 >= len(buf):
                raise ValueError("truncated compression pointer")
            ptr = ((ln & 0x3F) << 8) | buf[off + 1]
            if not jumped:
                nxt = off + 2
            jumped = True
            hops += 1
            if hops > 40:
                raise ValueError("compression loop")
            off = ptr
            continue
        off += 1
        labels.append(buf[off:off + ln].decode("utf-8", "replace"))
        off += ln
    return ".".join(labels) + "." if labels else ".", nxt


def parse_dns(buf: bytes) -> dict:
    """Parse a DNS message into header/questions/answers/authorities/additionals."""
    if len(buf) < 12:
        raise ValueError("short packet")
    (mid, flags, qd, an, ns, ar) = struct.unpack("!HHHHHH", buf[:12])
    msg = {
        "id": mid,
        "flags": "0x%04x" % flags,
        "is_response": bool(flags & 0x8000),
        "opcode": (flags >> 11) & 0xF,
        "rcode": flags & 0xF,
        "qdcount": qd, "ancount": an, "nscount": ns, "arcount": ar,
        "questions": [], "answers": [], "authorities": [], "additionals": [],
    }
    off = 12
    for _ in range(qd):
        name, off = _name(buf, off)
        qtype, qclass = struct.unpack("!HH", buf[off:off + 4])
        off += 4
        msg["questions"].append({
            "name": name, "qtype": T_QTYPE.get(qtype, str(qtype)),
            "qclass": CLASS_TOP.get(qclass & 0x7FFF, str(qclass & 0x7FFF)),
            "unicast_response_requested": bool(qclass & 0x8000),
        })

    for sect, count in (("answers", an), ("authorities", ns), ("additionals", ar)):
        for _ in range(count):
            name, off = _name(buf, off)
            rtype, rclass, ttl, rdlen = struct.unpack("!HHIH", buf[off:off + 10])
            off += 10
            rdata = buf[off:off + rdlen]
            off += rdlen
            rec = {
                "name": name,
                "type": T_NEW_QTYPE.get(rtype, str(rtype)),
                "class": CLASS_TOP.get(rclass & 0x7FFF, str(rclass & 0x7FFF)),
                "cache_flush": bool(rclass & 0x8000),
                "ttl": ttl,
                "rdlength": rdlen,
            }
            try:
                if rtype == 12:                       # PTR
                    rec["ptr"] = _name(buf, off - rdlen)[0]
                elif rtype == 33:                     # SRV
                    prio, weight, port = struct.unpack("!HHH", rdata[:6])
                    tgt, _ = _name(buf, off - rdlen + 6)
                    rec.update(priority=prio, weight=weight, port=port, target=tgt)
                elif rtype == 16:                     # TXT
                    props = {}
                    i = 0
                    while i < len(rdata):
                        ln = rdata[i]
                        i += 1
                        s = rdata[i:i + ln].decode("utf-8", "replace")
                        i += ln
                        if "=" in s:
                            k, v = s.split("=", 1)
                            props[k] = v
                        else:
                            props[s] = None
                    rec["txt"] = props
                elif rtype == 1:                      # A
                    rec["a"] = socket.inet_ntoa(rdata[:4])
                elif rtype == 28:                     # AAAA
                    rec["aaaa"] = socket.inet_ntop(socket.AF_INET6, rdata[:16])
                else:
                    rec["rdata_hex"] = rdata.hex()
            except Exception as exc:                  # noqa: BLE001
                rec["parse_error"] = repr(exc)
                rec["rdata_hex"] = rdata.hex()
            msg[sect].append(rec)
    msg["_bytes"] = len(buf)
    return msg


# ------------------------------------------------------------------ wire build
def encode_name(n: str) -> bytes:
    n = n.rstrip(".")
    if not n:
        return b"\x00"
    return b"".join(bytes([len(p)]) + p.encode("utf-8") for p in n.split(".")) + b"\x00"


def build_query(qname: str, qtype: int, unicast: bool = False,
                want_unicast_reply: bool = False) -> bytes:
    qclass = 0x0001 | (0x8000 if want_unicast_reply else 0)
    hdr = struct.pack("!HHHHHH", random.randint(0, 0xFFFF), 0x0000, 1, 0, 0, 0)
    return hdr + encode_name(qname) + struct.pack("!HH", qtype, qclass)


def _make_sock():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
    except Exception:                                   # noqa: BLE001
        pass
    s.bind(("", 5354))          # distinct port: we are NOT a 5353 responder
    s.settimeout(0.25)
    return s


def ask(sock, qname, qtype, seconds=4.0, unicast_bit=False):
    """Send the query twice, collect every packet that arrives."""
    pkt = build_query(qname, qtype, want_unicast_reply=unicast_bit)
    sent_at = time.time()
    sock.sendto(pkt, MDNS)
    time.sleep(0.25)
    sock.sendto(pkt, MDNS)
    records = []
    packets = []
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            data, addr = sock.recvfrom(9000)
        except socket.timeout:
            continue
        except OSError:
            break
        try:
            msg = parse_dns(data)
        except Exception as exc:                        # noqa: BLE001
            packets.append({"from": addr[0], "error": repr(exc), "hex": data.hex()})
            continue
        msg["from"] = addr[0]
        msg["at_ms"] = round((time.time() - sent_at) * 1000, 1)
        packets.append(msg)
        for sect in ("answers", "authorities", "additionals"):
            for r in msg[sect]:
                r["_from"] = addr[0]
                r["_at_ms"] = msg["at_ms"]
                r["_section"] = sect
                records.append(r)
    return {"query": {"qname": qname, "qtype": T_QTYPE.get(qtype, str(qtype)),
                      "wire_hex": pkt.hex(), "unicast_response_bit": unicast_bit},
            "packets": packets,
            "records": records}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="dnssd_raw.json")
    ap.add_argument("--seconds", type=float, default=4.0,
                    help="listen window per query")
    ap.add_argument("--extra-types", nargs="*", default=[],
                    help="additional service types to browse explicitly")
    args = ap.parse_args()

    out = {
        "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "local_hostname": socket.gethostname(),
        "steps": [],
        "advertised_types": [],
        "types": {},
    }
    sock = _make_sock()

    # --- step 1: enumerate advertised service types -------------------------
    print("== Q1: PTR _services._dns-sd._udp.local. ==", flush=True)
    r1 = ask(sock, "_services._dns-sd._udp.local.", 12, args.seconds)
    out["steps"].append({"step": "services_meta_query", "qname":
                         "_services._dns-sd._udp.local.", "qtype": "PTR",
                         "wire_hex": r1["query"]["wire_hex"],
                         "from": sorted({p.get("from") for p in r1["packets"] if p.get("from")}),
                         "records": r1["records"]})
    types = set()
    for rec in r1["records"]:
        if rec.get("type") == "PTR" and rec.get("ptr"):
            types.add(rec["ptr"])
    out["advertised_types"] = sorted(types)
    print("   advertised types (%d):" % len(types))
    for t in sorted(types):
        print("     %s" % t)
    if not types:
        print("   !! no PTR records came back from the meta query", flush=True)

    # --- step 2/3: per type, instances, then SRV/TXT/A ----------------------
    browse = sorted(types | set(args.extra_types))
    for t in browse:
        print("== PTR %s ==" % t, flush=True)
        rt = ask(sock, t, 12, args.seconds)
        insts = []
        for rec in rt["records"]:
            if rec.get("type") == "PTR" and rec.get("ptr"):
                insts.append(rec["ptr"])
        insts = sorted(set(insts))
        entry = {
            "qname": t,
            "wire_hex": rt["query"]["wire_hex"],
            "responders": sorted({p.get("from") for p in rt["packets"] if p.get("from")}),
            "ptr_records": [r for r in rt["records"] if r.get("type") == "PTR"],
            "instances": {},
        }
        print("   instances (%d): %s" % (len(insts), insts), flush=True)
        for inst in insts:
            rs = ask(sock, inst, 255, args.seconds)      # ANY
            keeps = [r for r in rs["records"]
                     if r.get("type") in ("SRV", "TXT", "A", "AAAA")]
            entry["instances"][inst] = {
                "wire_hex": rs["query"]["wire_hex"],
                "responders": sorted({p.get("from") for p in rs["packets"]
                                      if p.get("from")}),
                "records": keeps,
            }
            for r in keeps:
                if r["type"] == "SRV":
                    print("     SRV %s -> %s:%s" % (inst, r.get("target"), r.get("port")))
                if r["type"] == "TXT":
                    print("     TXT %s -> %s" % (inst, json.dumps(r.get("txt"),
                                                                  ensure_ascii=False)))
                if r["type"] == "A":
                    print("     A   %s -> %s" % (inst, r.get("a")))
        out["types"][t] = entry

    sock.close()
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=2, ensure_ascii=False)
    print("\nraw DNS-SD report -> %s" % args.out)
    print("advertised types: %s" % ", ".join(out["advertised_types"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
