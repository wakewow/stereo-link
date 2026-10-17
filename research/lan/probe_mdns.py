#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_mdns.py -- Full DNS-SD / mDNS service enumeration of the local LAN.

Read-only. Sends only standard mDNS PTR/SRV/TXT/A queries on UDP 5353.
Writes nothing except the JSON report path given on the command line.

Usage:
    python probe_mdns.py [--duration 25] [--out mdns_report.json]

Pass 1: meta-query _services._dns-sd._udp.local.  -> enumerate ALL advertised
        service types on the link (this is what proves "all broadcast services").
Pass 2: browse every discovered type + a hardcoded list of known Mi/Apple/
        Google/Sonos/etc. types, resolving SRV/TXT/A for each instance.
Pass 3 (raw socket): send an explicit PTR query for _services._dns-sd._udp.local.
        with the QU bit unset, so we see unicast+multicast responders, and dump
        the raw DNS records for the two target devices.
"""

import argparse
import json
import socket
import struct
import sys
import time
from collections import OrderedDict

from zeroconf import ServiceBrowser, ServiceListener, Zeroconf

# ---------------------------------------------------------------- known types
KNOWN_TYPES = [
    "_services._dns-sd._udp.local.",
    # Apple / AirPlay / RAOP
    "_raop._tcp.local.",
    "_airplay._tcp.local.",
    "_airport._tcp.local.",
    "_companion-link._tcp.local.",
    "_touch-able._tcp.local.",
    "_appletv-v2._tcp.local.",
    "_hsc._tcp.local.",
    "_mediaremotetv._tcp.local.",
    "_sleep-proxy._udp.local.",
    # Xiaomi / Mi
    "_miplay._tcp.local.",
    "_miplay._udp.local.",
    "_micont._tcp.local.",
    "_miio._udp.local.",
    "_miio._tcp.local.",
    "_mihome._tcp.local.",
    "_xiaomi._tcp.local.",
    "_miot._tcp.local.",
    "_miot._udp.local.",
    "_mico._tcp.local.",
    # Google Cast / Spotify / Sonos
    "_googlecast._tcp.local.",
    "_spotify-connect._tcp.local.",
    "_sonos._tcp.local.",
    "_soundtouch._tcp.local.",
    "_matter._tcp.local.",
    "_matterc._udp.local.",
    # DLNA / UPnP
    "_dlna._tcp.local.",
    "_upnp._tcp.local.",
    "_ssdp._udp.local.",
    # generic
    "_http._tcp.local.",
    "_https._tcp.local.",
    "_sftp-ssh._tcp.local.",
    "_ssh._tcp.local.",
    "_telnet._tcp.local.",
    "_workstation._tcp.local.",
    "_device-info._tcp.local.",
    "_rdlink._tcp.local.",
    "_sdap._tcp.local.",
    "_amzn-wplay._tcp.local.",
    "_viziocast._tcp.local.",
    "_printer._tcp.local.",
    "_ipp._tcp.local.",
    "_hap._tcp.local.",
    "_esphomelib._tcp.local.",
    "_arduino._tcp.local.",
    "_wled._tcp.local.",
]

TARGETS = {"192.168.1.171", "192.168.1.21"}


class Collector(ServiceListener):
    def __init__(self, zc, tag):
        self.zc = zc
        self.tag = tag
        self.added = OrderedDict()   # fqdn -> info dict
        self.removed = []

    # zeroconf >=0.32 passes (zeroconf, service_type, name, state_change)
    def add_service(self, zc, type_, name):
        self._record(zc, type_, name)

    def update_service(self, zc, type_, name):
        self._record(zc, type_, name)

    def remove_service(self, zc, type_, name):
        self.removed.append({"type": type_, "name": name})

    def _record(self, zc, type_, name):
        try:
            info = zc.get_service_info(type_, name, timeout=4000)
        except Exception as exc:                                # noqa: BLE001
            self.added.setdefault(name, {})["_error"] = repr(exc)
            return
        if info is None:
            self.added.setdefault(name, {}).setdefault("_note", "unresolved (no SRV/TXT)")
            self.added[name].setdefault("type", type_)
            self.added[name].setdefault("name", name)
            return

        props = {}
        for k, v in (info.properties or {}).items():
            kk = k.decode("utf-8", "replace") if isinstance(k, bytes) else str(k)
            if isinstance(v, bytes):
                vv = v.decode("utf-8", "replace")
            else:
                vv = v
            props[kk] = vv

        addrs = []
        try:
            addrs = [socket.inet_ntoa(a) for a in (info.addresses or [])]
        except Exception:                                       # noqa: BLE001
            pass

        rec = {
            "type": type_,
            "name": name,
            "server": info.server,
            "port": info.port,
            "addresses": addrs,
            "host_ttl": info.host_ttl,
            "other_ttl": info.other_ttl,
            "priority": info.priority,
            "weight": info.weight,
            "properties": props,
        }
        # merge, keep the richest version
        prev = self.added.get(name, {})
        if prev.get("port") is None or len(json.dumps(rec)) >= len(json.dumps(prev)):
            self.added[name] = rec


def raw_ptr_query_services() -> dict:
    """Hand-rolled mDNS query for _services._dns-sd._udp.local. (PTR) + parse."""
    qname = b"".join(bytes([len(p)]) + p.encode() for p in
                     ["_services", "_dns-sd", "_udp", "local"]) + b"\x00"
    # header: id=0, flags=0 (standard query, QU=0 -> multicast response),
    # qdcount=1, rest 0
    pkt = struct.pack("!HHHHHH", 0, 0, 1, 0, 0, 0) + qname + struct.pack("!HH", 12, 1)

    out = {"query_hex": pkt.hex(), "responders": {}, "raw_records": []}
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
    except Exception:                                           # noqa: BLE001
        pass
    s.settimeout(3.0)
    s.bind(("", 0))
    s.sendto(pkt, ("224.0.0.251", 5353))
    deadline = time.time() + 6.0
    while time.time() < deadline:
        try:
            data, addr = s.recvfrom(9000)
        except socket.timeout:
            break
        ip = addr[0]
        out["responders"].setdefault(ip, 0)
        out["responders"][ip] += 1
        out["raw_records"].append({
            "from": ip,
            "len": len(data),
            "hex": data.hex(),
            "types": _scan_types(data),
        })
    s.close()
    return out


def _scan_types(data: bytes):
    """Crude but effective: pull every '_xxx._tcp/_udp.local' label out of a packet."""
    found = set()
    for proto in (b"_tcp", b"_udp"):
        idx = 0
        while True:
            i = data.find(proto + b"\x05local", idx)
            if i < 0:
                i = data.find(proto + b"\x00", idx)
                if i < 0:
                    break
            # walk backwards to the start of the service-type label chain
            j = i
            steps = 0
            while j - 1 >= 0 and data[j - 1] != 0 and steps < 4:
                ln = data[j - 1]
                if 1 <= ln <= 20 and j - 1 - ln >= 0:
                    j = j - 1 - ln
                    steps += 1
                else:
                    break
            try:
                found.add(data[j:i + len(proto)].decode("ascii", "replace"))
            except Exception:                                   # noqa: BLE001
                pass
            idx = i + 1
    # keep only plausible service-type labels
    return sorted(t for t in found if t.startswith("_") and "_" in t[1:] and len(t) < 40)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=float, default=25.0)
    ap.add_argument("--out", default="mdns_report.json")
    args = ap.parse_args()

    started = time.time()
    report = {
        "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "duration_s": args.duration,
        "hostname": socket.gethostname(),
        "advertised_service_types": {},
        "instances": {},
        "targets_192_168_1_171": {},
        "targets_192_168_1_21": {},
        "raw_services_ptr": None,
        "removed": [],
    }

    zc = Zeroconf()
    collectors = {}

    # ---- pass 1: meta-browse to discover advertised service types -----------
    meta = Collector(zc, "_services._dns-sd._udp.local.")
    b_meta = ServiceBrowser(zc, "_services._dns-sd._udp.local.", meta)
    print("[pass1] browsing _services._dns-sd._udp.local. for %.0fs ..." % args.duration,
          flush=True)
    time.sleep(args.duration / 2.0)

    # meta browse yields PTR records whose "name" IS the advertised service type
    advertised = set()
    for name in list(meta.added.keys()):
        svc = name.rstrip(".") + "."
        if svc.count(".") >= 2 and (svc.endswith("_tcp.local.") or svc.endswith("_udp.local.")):
            advertised.add(svc)
    print("[pass1] advertised types found: %d" % len(advertised), flush=True)
    for t in sorted(advertised):
        print("         %s" % t, flush=True)

    # ---- pass 2: browse every advertised type + every known type -----------
    to_browse = sorted(advertised | set(KNOWN_TYPES))
    browsers = []
    for t in to_browse:
        if t == "_services._dns-sd._udp.local.":
            continue
        c = Collector(zc, t)
        collectors[t] = c
        browsers.append(ServiceBrowser(zc, t, c))
    print("[pass2] browsing %d service types for remaining %.0fs ..."
          % (len(browsers), args.duration / 2.0), flush=True)
    time.sleep(args.duration / 2.0)

    b_meta.cancel()
    for b in browsers:
        b.cancel()
    time.sleep(1.0)

    # ---- raw pass 3 --------------------------------------------------------
    print("[pass3] raw multicast PTR query for _services._dns-sd._udp.local. ...",
          flush=True)
    try:
        report["raw_services_ptr"] = raw_ptr_query_services()
        print("[pass3] responders: %s"
              % sorted(report["raw_services_ptr"]["responders"].items()), flush=True)
    except Exception as exc:                                    # noqa: BLE001
        report["raw_services_ptr"] = {"error": repr(exc)}

    # ---- assemble ----------------------------------------------------------
    for t, c in collectors.items():
        if not c.added:
            continue
        report["instances"][t] = {
            "count": len(c.added),
            "instances": list(c.added.values()),
        }
    report["advertised_service_types"] = {
        t: len(collectors.get(t, Collector(zc, t)).added)
        for t in sorted(advertised)
    }
    report["removed"] = [r for c in collectors.values() for r in c.removed] + meta.removed

    # per-target rollup
    for t, c in collectors.items():
        for rec in c.added.values():
            for ip in rec.get("addresses") or []:
                if ip in TARGETS:
                    key = "targets_%s" % ip.replace(".", "_")
                    report[key].setdefault(t, []).append(rec)

    report["elapsed_s"] = round(time.time() - started, 2)
    zc.close()

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2, ensure_ascii=False, sort_keys=False)

    print("\n=== SUMMARY ===")
    print("advertised service types: %d" % len(advertised))
    n_inst = sum(v["count"] for v in report["instances"].values())
    print("resolved instances: %d across %d types" % (n_inst, len(report["instances"])))
    for ip in sorted(TARGETS):
        key = "targets_%s" % ip.replace(".", "_")
        print("  %s -> %s" % (ip, json.dumps(report[key], ensure_ascii=False)[:400]))
    print("report -> %s" % args.out)


if __name__ == "__main__":
    sys.exit(main())
