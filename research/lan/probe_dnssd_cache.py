#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_dnssd_cache.py -- raw DNS record dump, taken from zeroconf's own cache.

Rationale: a hand-rolled socket bound to a non-5353 source port gets ignored by
these speakers (verified: probe_dnssd_raw.py received 0 replies). zeroconf binds
UDP/5353 correctly, so its cache holds the ACTUAL wire records -- name, type,
class, TTL and rdata -- exactly as the device sent them.

This script only sends standard mDNS PTR/SRV/TXT queries (via ServiceBrowser)
and then prints/dumps every record zeroconf cached. Read-only.

Usage:
    python probe_dnssd_cache.py --seconds 30 --out dnssd_cache.json
"""

import argparse
import json
import socket
import sys
import time

from zeroconf import ServiceBrowser, ServiceListener, Zeroconf
from zeroconf._cache import _DNSRecordCacheType

TYPES = [
    "_services._dns-sd._udp.local.",
    "_raop._tcp.local.",
    "_airplay._tcp.local.",
    "_mi-connect._udp.local.",
    "_miplay._tcp.local.",
    "_miplay._udp.local.",
    "_micont._tcp.local.",
    "_miio._udp.local.",
    "_miio._tcp.local.",
    "_miot._tcp.local.",
    "_mihome._tcp.local.",
    "_googlecast._tcp.local.",
    "_spotify-connect._tcp.local.",
    "_sonos._tcp.local.",
    "_matter._tcp.local.",
    "_matterc._udp.local.",
    "_dlna._tcp.local.",
    "_upnp._tcp.local.",
    "_http._tcp.local.",
    "_https._tcp.local.",
    "_ssh._tcp.local.",
    "_device-info._tcp.local.",
    "_airport._tcp.local.",
    "_companion-link._tcp.local.",
    "_touch-able._tcp.local.",
    "_appletv-v2._tcp.local.",
    "_mediaremotetv._tcp.local.",
    "_sleep-proxy._udp.local.",
    "_hsc._tcp.local.",
    "_sdap._tcp.local.",
    "_amzn-wplay._tcp.local.",
    "_hap._tcp.local.",
    "_workstation._tcp.local.",
    "_printer._tcp.local.",
    "_ipp._tcp.local.",
    "_rdlink._tcp.local.",
    "_airplay._udp.local.",
]

TYPE_LABEL = {1: "A", 28: "AAAA", 12: "PTR", 16: "TXT", 33: "SRV", 47: "NSEC"}


class Noop(ServiceListener):
    def add_service(self, *a):
        pass

    def update_service(self, *a):
        pass

    def remove_service(self, *a):
        pass


def _safe_to_string(rec):
    """zeroconf's to_string(now) signature varies between versions."""
    for args in ((time.time(),), ()):
        try:
            return rec.to_string(*args)
        except TypeError:
            continue
        except Exception as exc:                                # noqa: BLE001
            return "<to_string failed: %r>" % (exc,)
    return "<unrenderable %s %s>" % (rec.name, rec.type)


def dump_cache(zc):
    """Every cached record, with its raw rdata, grouped by record type."""
    recs = []
    seen = set()
    cache = zc.cache.cache                       # type -> {DNSRecord: DNSRecord}
    for _ctype, table in cache.items():
        for rec in list(table.keys()):
            key = (rec.name, rec.type, getattr(rec, "port", None),
                   str(getattr(rec, "text", b""))[:200], getattr(rec, "alias", None))
            if key in seen:
                continue
            seen.add(key)
            entry = {
                "name": rec.name,
                "type": TYPE_LABEL.get(rec.type, str(rec.type)),
                "type_num": rec.type,
                "class": rec.class_label if hasattr(rec, "class_label") else str(rec.class_),
                "unique": bool(getattr(rec, "unique", False)),
                "ttl": rec.ttl,
                "raw_to_string": _safe_to_string(rec),
            }
            for attr in ("address", "server", "port", "priority", "weight",
                         "alias", "text"):
                if hasattr(rec, attr):
                    v = getattr(rec, attr)
                    if isinstance(v, bytes):
                        v = v.decode("utf-8", "replace")
                    entry[attr] = v
            # fully decoded TXT key/value pairs
            if rec.type == 16 and hasattr(rec, "text"):
                props = {}
                txt = rec.text or b""
                i = 0
                while i < len(txt):
                    ln = txt[i]
                    i += 1
                    s = txt[i:i + ln].decode("utf-8", "replace")
                    i += ln
                    if "=" in s:
                        k, v = s.split("=", 1)
                        props[k] = v
                    elif s:
                        props[s] = None
                entry["txt_props"] = props
            recs.append(entry)
    recs.sort(key=lambda r: (r["type"], r["name"]))
    return recs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--out", default="dnssd_cache.json")
    args = ap.parse_args()

    out = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
           "hostname": socket.gethostname(),
           "browsed_types": TYPES,
           "raw_queries": [],
           "records": [],
           "services_seen": {},
           "cache_type_buckets": {}}

    zc = Zeroconf()
    listener = Noop()
    browsers = []
    print("browsing %d service types for %.0fs ..." % (len(TYPES), args.seconds),
          flush=True)
    for t in TYPES:
        browsers.append(ServiceBrowser(zc, t, listener))
        time.sleep(0.05)
    time.sleep(args.seconds)
    for b in browsers:
        b.cancel()
    time.sleep(0.5)

    out["records"] = dump_cache(zc)
    out["cache_type_buckets"] = {str(k): len(v) for k, v in zc.cache.cache.items()}

    # resolve SRV/TXT via ServiceInfo for every SRV we cached -> definitive port/TXT
    srv = [r for r in out["records"] if r["type"] == "SRV"]
    print("cached SRV records: %d" % len(srv), flush=True)
    for r in srv:
        svc_type = None
        # find which of our browsed types this instance belongs to
        for t in TYPES:
            if r["name"].endswith("." + t) or r["name"].endswith(t):
                svc_type = t
                break
        if not svc_type:
            continue
        try:
            info = zc.get_service_info(svc_type, r["name"], timeout=3000)
        except Exception as exc:                                # noqa: BLE001
            info = None
            out["services_seen"].setdefault(svc_type, {})[r["name"]] = {
                "error": repr(exc)}
        if info is None:
            continue
        props = {}
        for k, v in (info.properties or {}).items():
            kk = k.decode("utf-8", "replace") if isinstance(k, bytes) else str(k)
            vv = (v.decode("utf-8", "replace") if isinstance(v, bytes) else v)
            props[kk] = vv
        out["services_seen"].setdefault(svc_type, {})[r["name"]] = {
            "server": info.server,
            "port": info.port,
            "addresses": [socket.inet_ntoa(a) for a in (info.addresses or [])],
            "properties": props,
            "txt_as_sent": getattr(info, "text", None).decode("utf-8", "replace")
            if isinstance(getattr(info, "text", None), bytes) else None,
        }

    zc.close()
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=2, ensure_ascii=False, default=str)

    print("\n===== RAW CACHED RECORDS (%d) =====" % len(out["records"]))
    for r in out["records"]:
        print("  %s" % r["raw_to_string"])
    print("\n===== SERVICES (name -> server:port / TXT) =====")
    for t, insts in out["services_seen"].items():
        for name, d in insts.items():
            print("  [%s] %s" % (t, name))
            print("      %s:%s %s" % (d.get("server"), d.get("port"),
                                      d.get("addresses")))
            print("      TXT %s" % json.dumps(d.get("properties"),
                                              ensure_ascii=False))
    print("\nreport -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
