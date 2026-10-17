#!/usr/bin/env python3
"""Environment recon for the fallback audio paths:
  1. full mDNS dump of the speakers' _mi-connect._udp / _lyra-mdns._udp (SRV + every TXT k/v)
  2. TCP reachability of candidate LOCAL control ports (miio 54321, http 80, miplay 8899)
  3. whether the mDNS-advertised port speaks plain HTTP
Read-only: only DNS queries and TCP connects; no commands are sent to the speakers.
"""
from __future__ import annotations

import json
import socket
import struct
import sys
import time

MCAST = ("224.0.0.251", 5353)
SERVICES = ["_mi-connect._udp.local", "_lyra-mdns._udp.local", "_mijiia._tcp.local", "_miio._udp.local"]
PORTS = [54321, 80, 8899, 8080, 443]


def enc_name(name: str) -> bytes:
    out = b""
    for label in name.split("."):
        if label:
            out += bytes([len(label)]) + label.encode()
    return out + b"\x00"


def build_query(service: str) -> bytes:
    hdr = struct.pack(">HHHHHH", 0, 0, 1, 0, 0, 0)
    return hdr + enc_name(service) + struct.pack(">HH", 12, 1)


def read_name(data: bytes, off: int) -> tuple[str, int]:
    labels, guard = [], 0
    while off < len(data) and guard < 128:
        guard += 1
        ln = data[off]
        if ln == 0:
            off += 1
            break
        if ln & 0xC0 == 0xC0:
            ptr = struct.unpack(">H", data[off:off + 2])[0] & 0x3FFF
            sub, _ = read_name(data, ptr)
            labels.append(sub)
            off += 2
            break
        labels.append(data[off + 1:off + 1 + ln].decode("latin1"))
        off += 1 + ln
    return ".".join(x for x in labels if x), off


def parse(data: bytes) -> list[dict]:
    if len(data) < 12:
        return []
    _id, _fl, qd, an, ns, ar = struct.unpack(">HHHHHH", data[:12])
    off = 12
    for _ in range(qd):
        _n, off = read_name(data, off)
        off += 4
    out = []
    for _ in range(an + ns + ar):
        name, off = read_name(data, off)
        if off + 10 > len(data):
            break
        rtype, rclass, ttl, rdlen = struct.unpack(">HHIH", data[off:off + 10])
        off += 10
        rdata = data[off:off + rdlen]
        rec = {"name": name, "type": rtype, "ttl": ttl}
        if rtype == 12:
            rec["ptr"], _ = read_name(data, off)
        elif rtype == 33:
            pri, wt, port = struct.unpack(">HHH", rdata[:6])
            tgt, _ = read_name(data, off + 6)
            rec.update(port=port, target=tgt)
        elif rtype == 16:
            txt, i, kvs = {}, 0, []
            while i < len(rdata):
                ln = rdata[i]
                kv = rdata[i + 1:i + 1 + ln].decode("utf-8", "replace")
                kvs.append(kv)
                if "=" in kv:
                    k, v = kv.split("=", 1)
                    txt[k] = v
                i += 1 + ln
            rec["txt"] = txt
        elif rtype in (1, 28):
            try:
                rec["addr"] = socket.inet_ntop(socket.AF_INET if rtype == 1 else socket.AF_INET6, rdata)
            except Exception:
                pass
        out.append(rec)
        off += rdlen
    return out


def mdns_dump(wait: float = 4.0) -> list[dict]:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
    s.bind(("", 0))
    for svc in SERVICES:
        try:
            s.sendto(build_query(svc), MCAST)
        except OSError as exc:
            print("send fail", svc, exc)
    recs, deadline = [], time.time() + wait
    s.settimeout(0.5)
    while time.time() < deadline:
        try:
            data, addr = s.recvfrom(65535)
        except (socket.timeout, TimeoutError):
            continue
        for r in parse(data):
            r["from"] = addr[0]
            recs.append(r)
    s.close()
    # dedupe
    uniq, seen = [], set()
    for r in recs:
        key = (r.get("name"), r.get("type"), r.get("ptr"), r.get("port"), json.dumps(r.get("txt", {}), sort_keys=True))
        if key not in seen:
            seen.add(key)
            uniq.append(r)
    return uniq


def tcp_probe(ip: str, port: int, timeout: float = 1.5) -> str:
    s = socket.socket()
    s.settimeout(timeout)
    try:
        s.connect((ip, port))
        try:
            s.sendall(b"GET / HTTP/1.0\r\n\r\n")
            s.settimeout(2.0)
            data = s.recv(200)
            return f"OPEN http_reply={data[:60]!r}"
        except Exception:
            return "OPEN (no http reply)"
    except Exception as exc:
        return f"closed ({type(exc).__name__})"
    finally:
        s.close()


def main() -> int:
    hosts = {}
    print("=== mDNS ===")
    for r in mdns_dump():
        if r["type"] == 12:
            print(f"  PTR  {r['name']} -> {r['ptr']}   (from {r['from']})")
        elif r["type"] == 33:
            print(f"  SRV  {r['name']} -> {r['target']}:{r['port']}   ttl={r['ttl']}  (from {r['from']})")
            hosts.setdefault(r["from"], r["port"])
        elif r["type"] == 16:
            print(f"  TXT  {r['name']}")
            for k, v in sorted(r.get("txt", {}).items()):
                print(f"         {k} = {v[:160]}")
        elif r.get("addr"):
            print(f"  A    {r['name']} = {r['addr']}")
    print("\n=== TCP ports on the two speakers ===")
    for ip, label in (("192.168.1.171", "L16B did=934947888"), ("192.168.1.21", "L16A did=501718792")):
        adv = hosts.get(ip)
        print(f"  {label} ({ip}) advertised_mdns_port={adv}")
        for port in ([adv] if adv else []) + PORTS:
            if port is None:
                continue
            print(f"     :{port:<6} {tcp_probe(ip, port)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
