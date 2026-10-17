#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_ports.py -- gentle TCP/UDP port probe + banner grab for the two speakers.

Read-only, non-destructive. Concurrency is deliberately capped (default 32) and
every connect has a 1.2 s timeout, so this is not a flood.

TCP: connect() only. If the port opens, we grab a banner with a protocol-aware
     stimulus (RTSP OPTIONS for 7000/554, HTTP HEAD/GET for web ports, TLS
     ClientHello-less SNI probe is NOT sent -- we just read what the peer says).
UDP: SSDP M-SEARCH on 1900, mDNS PTR on 5353, and the standard miIO "hello"
     handshake on 54321 (the only way to see a miio speaker, since miio does not
     advertise via mDNS).

Usage:
    python probe_ports.py --out ports_report.json [--timeout 1.2] [--workers 32]
"""

import argparse
import concurrent.futures as futures
import json
import socket
import ssl
import struct
import sys
import time

IPS = ["192.168.1.171", "192.168.1.21"]

# port -> short label
TCP_PORTS = {
    21: "ftp", 22: "ssh", 23: "telnet", 53: "dns", 80: "http", 111: "rpcbind",
    139: "netbios", 443: "https", 445: "smb", 515: "lpd", 548: "afpovertcp",
    554: "rtsp", 631: "ipp", 1900: "ssdp", 2049: "nfs", 3000: "http-alt",
    3306: "mysql", 3689: "daap", 5000: "upnp/airplay-alt", 5060: "sip",
    5222: "xmpp", 5289: "airplay-alt2", 5297: "bonjour", 5298: "bonjour",
    5353: "mdns", 5432: "postgres", 5555: "adb", 5900: "vnc", 6881: "bittorrent",
    7000: "airplay/raop", 7100: "airplay-alt3", 7237: "raop-alt", 8000: "http-alt",
    8008: "cast", 8009: "cast-tls", 8080: "http-proxy", 8081: "http-alt",
    8082: "http-alt", 8085: "http-alt", 8123: "http-alt", 8200: "http-alt",
    8443: "https-alt", 8554: "rtsp-alt", 8888: "http-alt", 8899: "mixplay-ctrl",
    8989: "http-alt", 9000: "http-alt", 9090: "http-alt", 9100: "printer",
    10000: "webmin", 11223: "http-alt", 18080: "http-alt", 32200: "mi-?",
    32400: "plex", 32469: "dlna", 35000: "mi-?", 35500: "mi-?",
    36666: "mi-?", 40000: "mi-?", 41111: "mi-?", 43210: "mi-?",
    48000: "mi-?", 49152: "upnp", 49153: "upnp", 49154: "upnp",
    49155: "upnp", 49156: "upnp", 49157: "upnp", 49158: "upnp",
    49159: "upnp", 49160: "upnp", 52540: "raop-audio", 52541: "raop-audio",
    52542: "raop-audio", 52543: "raop-audio", 52544: "raop-audio",
    52545: "raop-audio", 52546: "raop-audio", 52547: "raop-audio",
    52548: "raop-audio", 52549: "raop-audio", 52550: "raop-audio",
    52551: "raop-audio", 52552: "raop-audio", 52553: "raop-audio",
    52554: "raop-audio", 52555: "raop-audio", 52556: "raop-audio",
    52557: "raop-audio", 52558: "raop-audio", 52559: "raop-audio",
    52560: "raop-audio", 54321: "miio", 55442: "mi-?", 55555: "mi-?",
    56666: "mi-connect", 62078: "iphone-sync", 63999: "mi-?",
}

RTSP_STIMULUS = (
    "OPTIONS * RTSP/1.0\r\n"
    "CSeq: 1\r\n"
    "User-Agent: iTunes/7.6.2 (Windows; N)\r\n"
    "Client-Instance: 0000000000000000\r\n"
    "DACP-ID: 0000000000000000\r\n"
    "Active-Remote: 1986535575\r\n"
    "\r\n"
).encode()

HTTP_STIMULUS = (
    "HEAD / HTTP/1.1\r\nHost: {ip}\r\nUser-Agent: probe/1.0\r\n"
    "Accept: */*\r\nConnection: close\r\n\r\n"
).encode()

HTTP_GET_STIMULUS = (
    "GET / HTTP/1.1\r\nHost: {ip}\r\nUser-Agent: probe/1.0\r\n"
    "Accept: */*\r\nConnection: close\r\n\r\n"
).encode()


def recv_all(sock, limit=4096, idle=1.0):
    sock.settimeout(idle)
    chunks = []
    total = 0
    try:
        while total < limit:
            b = sock.recv(min(2048, limit - total))
            if not b:
                break
            chunks.append(b)
            total += len(b)
    except socket.timeout:
        pass
    except OSError:
        pass
    return b"".join(chunks)


def probe_tcp(ip, port, timeout):
    res = {"ip": ip, "port": port, "label": TCP_PORTS.get(port, "?"),
           "tcp_open": False, "banner": None, "banner_ascii": None,
           "sent": None, "error": None}
    t0 = time.time()
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect((ip, port))
    except socket.timeout:
        res["error"] = "connect timeout"
        s.close()
        return res
    except OSError as exc:
        res["error"] = "connect: %s" % exc
        s.close()
        return res
    res["tcp_open"] = True
    res["connect_ms"] = round((time.time() - t0) * 1000, 1)

    # protocol-aware stimulus
    try:
        if port in (7000, 554, 8554, 7100, 7237, 5289, 5000):
            s.sendall(RTSP_STIMULUS)
            res["sent"] = "RTSP OPTIONS *"
        elif port in (80, 8008, 8080, 8081, 8082, 8085, 8123, 8200, 8888, 8899,
                      8989, 9000, 9090, 9100, 10000, 11223, 18080, 3000, 8000,
                      5000, 49152, 49153, 49154, 56666, 54321, 35000, 32469,
                      32200, 62078, 63999):
            s.sendall(HTTP_STIMULUS.format(ip=ip).encode())
            res["sent"] = "HTTP HEAD /"
        else:
            res["sent"] = "(none: read-only peek)"
    except OSError as exc:
        res["error"] = "send: %s" % exc
    data = recv_all(s, idle=min(1.5, timeout + 0.3))
    # if the HTTP HEAD got nothing, retry with GET on a fresh connection
    if not data and port in (80, 8080, 8008, 8009, 8899, 54321, 56666):
        try:
            s2 = socket.create_connection((ip, port), timeout=timeout)
            s2.sendall(HTTP_GET_STIMULUS.format(ip=ip).encode())
            data = recv_all(s2, idle=min(1.5, timeout + 0.3))
            res["sent"] = (res["sent"] or "") + " + HTTP GET /"
            s2.close()
        except OSError as exc:
            res["error"] = (res["error"] or "") + " | get-retry: %s" % exc
    s.close()
    res["banner_len"] = len(data)
    res["banner_ascii"] = data.decode("utf-8", "replace")[:1200]
    res["banner_hex"] = data[:160].hex()
    # first line is usually the most informative
    first = res["banner_ascii"].splitlines()[0] if res["banner_ascii"] else None
    res["banner"] = first
    return res


# ------------------------------------------------------------------ UDP probes
MIIO_HELLO = bytes.fromhex("21310020" + "ff" * 28)


def probe_udp_miio(ip, port=54321, timeout=2.0):
    """Send the standard miIO discovery hello. A miIO device answers 32 bytes."""
    out = {"ip": ip, "port": port, "test": "miio hello",
           "request_hex": MIIO_HELLO.hex(), "response_hex": None,
           "response_len": 0, "device_id": None, "stamp": None, "error": None}
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    try:
        s.sendto(MIIO_HELLO, (ip, port))
        data, addr = s.recvfrom(2048)
    except socket.timeout:
        out["error"] = "no response within %.1fs" % timeout
        s.close()
        return out
    except OSError as exc:
        out["error"] = "send/recv: %s" % exc
        s.close()
        return out
    s.close()
    out["response_hex"] = data.hex()
    out["response_len"] = len(data)
    out["from"] = addr[0]
    if len(data) >= 32 and data[0:2] == b"\x21\x31":
        out["magic"] = data[0:2].hex()
        out["length_field"] = struct.unpack("!H", data[2:4])[0]
        out["device_id"] = data[8:12].hex()
        out["stamp"] = struct.unpack("!I", data[12:16])[0]
        out["token_or_checksum_hex"] = data[16:32].hex()
    return out


def probe_udp_ssdp(timeout=3.0):
    """SSDP M-SEARCH -- does anything on the link speak UPnP/DLNA?"""
    msg = ("M-SEARCH * HTTP/1.1\r\n"
           "HOST: 239.255.255.250:1900\r\n"
           'MAN: "ssdp:discover"\r\n'
           "MX: 2\r\n"
           "ST: ssdp:all\r\n\r\n").encode()
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
    s.settimeout(0.5)
    s.bind(("", 0))
    s.sendto(msg, ("239.255.255.250", 1900))
    found = {}
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            data, addr = s.recvfrom(4096)
        except socket.timeout:
            continue
        except OSError:
            break
        found.setdefault(addr[0], []).append(
            data.decode("utf-8", "replace")[:600])
    s.close()
    return {"request": msg.decode(), "responders": found}


def probe_udp_port(ip, port, payload, timeout=1.5, label=""):
    """Blind UDP probe: send payload, see if anything comes back."""
    out = {"ip": ip, "port": port, "label": label, "payload_hex": payload.hex()[:200],
           "response_hex": None, "error": None}
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(timeout)
    try:
        s.sendto(payload, (ip, port))
        data, addr = s.recvfrom(4096)
        out["response_hex"] = data.hex()
        out["response_ascii"] = data.decode("utf-8", "replace")[:400]
        out["from"] = addr[0]
    except socket.timeout:
        out["error"] = "no response (may still be open+silent, or filtered)"
    except OSError as exc:
        # ICMP port-unreachable surfaces as ConnectionResetError on Windows
        out["error"] = "%s: %s" % (type(exc).__name__, exc)
        out["likely_closed"] = isinstance(exc, ConnectionResetError)
    s.close()
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="ports_report.json")
    ap.add_argument("--timeout", type=float, default=1.2)
    ap.add_argument("--workers", type=int, default=32)
    args = ap.parse_args()

    report = {"started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
              "timeout_s": args.timeout, "workers": args.workers,
              "tcp": {}, "udp": {}}

    jobs = [(ip, p) for ip in IPS for p in sorted(TCP_PORTS)]
    print("TCP: %d probes (%d ports x %d hosts), %d workers"
          % (len(jobs), len(TCP_PORTS), len(IPS), args.workers), flush=True)

    results = []
    with futures.ThreadPoolExecutor(max_workers=args.workers) as ex:
        futs = {ex.submit(probe_tcp, ip, p, args.timeout): (ip, p) for ip, p in jobs}
        done = 0
        for f in futures.as_completed(futs):
            done += 1
            try:
                results.append(f.result())
            except Exception as exc:                            # noqa: BLE001
                ip, p = futs[f]
                results.append({"ip": ip, "port": p, "error": repr(exc),
                                "tcp_open": False})
            if done % 40 == 0:
                print("  ... %d/%d" % (done, len(jobs)), flush=True)

    open_ports = []
    for r in sorted(results, key=lambda x: (x["ip"], x["port"])):
        key = "%s:%d" % (r["ip"], r["port"])
        report["tcp"][key] = r
        if r.get("tcp_open"):
            open_ports.append(r)
            print("  OPEN %-22s %-18s %s"
                  % (key, r["label"], (r.get("banner") or "")[:90]), flush=True)

    print("\nopen TCP ports: %d" % len(open_ports), flush=True)

    # ---- UDP ----
    print("\nUDP: miIO hello on 54321 ...", flush=True)
    report["udp"]["miio_54321"] = [probe_udp_miio(ip) for ip in IPS]
    print(json.dumps(report["udp"]["miio_54321"], indent=1)[:1500], flush=True)

    print("\nUDP: SSDP M-SEARCH ...", flush=True)
    report["udp"]["ssdp"] = probe_udp_ssdp()
    print("  ssdp responders: %s" % sorted(report["udp"]["ssdp"]["responders"]),
          flush=True)

    print("\nUDP: blind probes on candidate ports ...", flush=True)
    udp_candidates = {
        5353: ("", "mdns (see probe_dnssd_raw.py)"),
        56666: (b"\x00\x00\x00\x00", "mi-connect keepalive guess"),
        54321: (MIIO_HELLO, "miio hello (repeat)"),
        1900: (b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n"
               b'MAN: "ssdp:discover"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n', "ssdp unicast"),
        7000: (b"\x00", "airplay udp? (expect none)"),
        32200: (b"\x00", "mi guess"),
        35000: (b"\x00", "mi guess"),
        52540: (b"\x00", "raop control guess"),
    }
    report["udp"]["blind"] = []
    for ip in IPS:
        for port, (payload, label) in udp_candidates.items():
            if port == 5353:
                continue
            r = probe_udp_port(ip, port, payload or b"\x00", label=label)
            report["udp"]["blind"].append(r)
            if r.get("response_hex"):
                print("  UDP %s:%d (%s) -> %s" % (ip, port, label,
                                                  r["response_hex"][:120]), flush=True)

    report["open_tcp"] = ["%s:%d (%s)" % (r["ip"], r["port"], r["label"])
                          for r in open_ports]
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2, ensure_ascii=False)
    print("\nreport -> %s" % args.out)
    print("OPEN TCP: %s" % report["open_tcp"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
