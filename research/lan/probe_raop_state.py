#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_raop_state.py -- find the RTSP sequence this firmware accepts.

Known so far (probe_raop_variants.py, probe_raop_session.py):
    OPTIONS  (fresh conn)              -> 200
    ANNOUNCE (fresh conn, first req)   -> 200
    ANNOUNCE after OPTIONS, same conn  -> 403 Forbidden
    SETUP    (fresh conn, ANNOUNCE done on another conn) -> 455 Method Not Valid In This State

455 means "no session in the state you assume".  Two competing explanations:

  H1  session state is bound to the TCP connection, so the ANNOUNCE connection
      must stay open for SETUP/RECORD (and the 403 is a separate OPTIONS quirk);
  H2  the device needs AirPlay-2 pairing (/pair-setup + /pair-verify) before it
      will accept a stream, and 455 is how it says so.

This script separates them by driving RTSP request SEQUENCES over one or more
connections.  Read-only: no pairing, no config, every sequence ends in TEARDOWN.
"""

import argparse
import json
import socket
import struct
import sys
import time

IP = None
LOCAL = None
SESSION = str(int(time.time()))
URL = None


class Conn:
    def __init__(self, ip, port=7000, timeout=6.0):
        self.ip, self.port, self.timeout = ip, port, timeout
        self.sock = socket.create_connection((ip, port), timeout)
        self.sock.settimeout(timeout)

    def send(self, method, url, headers=None, body=None, cseq=None):
        global SEQ
        SEQ += 1
        h = {"CSeq": str(cseq if cseq is not None else SEQ),
             "User-Agent": "AirPlay/366.0",
             "Client-Instance": "0123456789abcdef"}
        if headers:
            h.update(headers)
        if body is not None:
            if isinstance(body, str):
                body = body.encode()
            h["Content-Length"] = str(len(body))
        raw = ("%s %s RTSP/1.0\r\n" % (method, url)
               + "".join("%s: %s\r\n" % (k, v) for k, v in h.items())
               + "\r\n").encode() + (body or b"")
        self.sock.sendall(raw)
        return self._read()

    def _read(self):
        buf = b""
        while b"\r\n\r\n" not in buf:
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            buf += chunk
        head, _, rest = buf.partition(b"\r\n\r\n")
        lines = head.decode("utf-8", "replace").split("\r\n")
        status = lines[0] if lines else ""
        code = 0
        p = status.split(" ", 2)
        if len(p) >= 2 and p[1].isdigit():
            code = int(p[1])
        hdrs = {}
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                hdrs[k.strip().lower()] = v.strip()
        body = rest
        clen = int(hdrs.get("content-length", "0") or 0)
        while len(body) < clen:
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            body += chunk
        return {"status": status, "code": code, "headers": hdrs,
                "raw": (head + b"\r\n\r\n" + body).decode("utf-8", "replace")}

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


SEQ = 0


def sdp(origin="iTunes"):
    return ("v=0\r\no=%s 1 0 IN IP4 %s\r\ns=iTunes\r\nc=IN IP4 %s\r\nt=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\na=rtpmap:96 L16/44100/2\r\na=fmtp:96 0\r\n"
            "a=ptime:352\r\n" % (origin, LOCAL, IP))


def udp_ports():
    a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    a.bind((LOCAL, 0))
    c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    c.bind((LOCAL, 0))
    t = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    t.bind((LOCAL, 0))
    ports = (a.getsockname()[1], c.getsockname()[1], t.getsockname()[1])
    return ports, (a, c, t)


def transport(cport, tport):
    return ("RTP/AVP/UDP;unicast;mode=record;control_port=%d;timing_port=%d"
            % (cport, tport))


def run_case(name, layout, extra_setup=None, extra_announce=None):
    """layout: list of ('new'|'keep', method, headers, body_kind)"""
    global SEQ
    SEQ = 0
    ports, socks = udp_ports()
    cport, _, tport = ports
    trace = []
    conn = None
    try:
        for item in layout:
            how, method, hdrs, kind = item
            if how == "new" or conn is None:
                if conn:
                    conn.close()
                conn = Conn(IP)
            h = dict(hdrs or {})
            body = None
            if kind == "sdp":
                body = sdp()
                h["Content-Type"] = "application/sdp"
            if method == "SETUP":
                h["Transport"] = transport(cport, tport)
                if extra_setup:
                    h.update(extra_setup)
            if method == "ANNOUNCE" and extra_announce:
                h.update(extra_announce)
            r = conn.send(method, URL, h, body)
            trace.append({"conn": how, "method": method, "code": r["code"],
                          "status": r["status"], "raw": r["raw"]})
            print("    [%s] %-9s -> %d %s" % (how, method, r["code"], r["status"]))
            if method == "SETUP" and r["code"] == 200:
                print("         Transport: %s" % r["headers"].get("transport"))
                print("         Session  : %s" % r["headers"].get("session"))
    except Exception as exc:                                        # noqa: BLE001
        trace.append({"error": repr(exc)})
        print("    EXCEPTION %r" % (exc,))
    finally:
        for s in socks:
            try:
                s.close()
            except OSError:
                pass
        if conn:
            conn.close()
    return {"case": name, "layout": [i[1] for i in layout], "trace": trace}


def main():
    global IP, LOCAL, URL
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--out", default="raop_state.json")
    args = ap.parse_args()
    IP = args.ip
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    probe.connect((IP, 7000))
    LOCAL = probe.getsockname()[0]
    probe.close()
    URL = "rtsp://%s/%s" % (IP, SESSION)

    print("=== RAOP RTSP state-machine probe: %s (local %s, session %s) ==="
          % (IP, LOCAL, SESSION))

    cases = []

    print("\n-- A. ANNOUNCE -> SETUP -> RECORD all on ONE connection --")
    cases.append(run_case("A_same_conn_full", [
        ("new", "ANNOUNCE", None, "sdp"),
        ("keep", "SETUP", None, None),
        ("keep", "RECORD", {"RTP-Info": "seq=0;rtptime=0", "Range": "npt=0-"}, None),
    ]))

    print("\n-- B. ANNOUNCE -> SETUP -> RECORD, fresh connection each time --")
    cases.append(run_case("B_fresh_conns", [
        ("new", "ANNOUNCE", None, "sdp"),
        ("new", "SETUP", None, None),
        ("new", "RECORD", {"RTP-Info": "seq=0;rtptime=0", "Range": "npt=0-"}, None),
    ]))

    print("\n-- C. fresh SETUP with Session header = url session id --")
    cases.append(run_case("C_fresh_setup_session_hdr", [
        ("new", "ANNOUNCE", None, "sdp"),
        ("new", "SETUP", {"Session": SESSION}, None),
    ]))

    print("\n-- D. OPTIONS -> ANNOUNCE -> SETUP on one connection --")
    cases.append(run_case("D_options_first", [
        ("new", "OPTIONS", None, None),
        ("keep", "ANNOUNCE", None, "sdp"),
        ("keep", "SETUP", None, None),
    ]))

    print("\n-- E. SETUP alone, no ANNOUNCE (expect 455) --")
    cases.append(run_case("E_setup_only", [
        ("new", "SETUP", None, None),
    ]))

    print("\n-- F. ANNOUNCE -> SETUP on one connection (no RECORD) --")
    cases.append(run_case("F_announce_setup", [
        ("new", "ANNOUNCE", None, "sdp"),
        ("keep", "SETUP", None, None),
    ]))

    print("\n-- G. ANNOUNCE(ALAC) -> SETUP -> RECORD, one connection --")
    alac = ("v=0\r\no=iTunes 1 0 IN IP4 %s\r\ns=iTunes\r\nc=IN IP4 %s\r\nt=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\n"
            "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n" % (LOCAL, IP))

    def run_alac():
        global SEQ
        SEQ = 0
        ports, socks = udp_ports()
        cport, _, tport = ports
        trace = []
        try:
            c = Conn(IP)
            r1 = c.send("ANNOUNCE", URL, {"Content-Type": "application/sdp"}, alac)
            print("    [new] ANNOUNCE  -> %d %s" % (r1["code"], r1["status"]))
            trace.append({"method": "ANNOUNCE", "code": r1["code"], "raw": r1["raw"]})
            r2 = c.send("SETUP", URL, {"Transport": transport(cport, tport)})
            print("    [keep] SETUP    -> %d %s" % (r2["code"], r2["status"]))
            trace.append({"method": "SETUP", "code": r2["code"], "raw": r2["raw"]})
            c.close()
        except Exception as exc:                                    # noqa: BLE001
            trace.append({"error": repr(exc)})
        finally:
            for s in socks:
                s.close()
        return {"case": "G_alac_same_conn", "trace": trace}

    cases.append(run_alac())

    print("\n-- H. Transport with no mode=record (plain unicast) --")
    cases.append(run_case("H_no_mode", [
        ("new", "ANNOUNCE", None, "sdp"),
        ("keep", "SETUP", {"Transport": "RTP/AVP/UDP;unicast;control_port=%d;timing_port=%d"
                           % (0, 0)}, None),
    ]))

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump({"ip": IP, "local_ip": LOCAL, "session": SESSION, "cases": cases},
                  fh, indent=2, ensure_ascii=False, default=str)
    print("\nreport -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
