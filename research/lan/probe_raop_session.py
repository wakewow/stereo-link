#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
probe_raop_session.py -- does the Xiaomi Sound accept a THIRD-PARTY RAOP push?

This is the decisive experiment for the miplay-bridge architecture question.
OPTIONS returning 200 only proves "something speaks RTSP on 7000".  A speaker
that is a genuine AirPlay *receiver* must also accept the full sender handshake
from a client it has never paired with:

    OPTIONS -> ANNOUNCE -> SETUP -> RECORD -> (audio) -> TEARDOWN

If ANNOUNCE/SETUP/RECORD all return 200 we have proven, with raw output, that
the box is an AirPlay receiver that accepts unpaired third-party pushes.  That
is the difference between "MiXPlay turns the PC into an AirPlay receiver and
forwards via the Xiaomi cloud" and "the speaker itself is an AirPlay target".

Two modes:
    --mode handshake   default.  Full RTSP handshake, NO audio, then TEARDOWN.
                       Silent, changes no persistent state.
    --mode tone        additionally streams a quiet sine tone over RTP so we can
                       hear whether audio actually comes out.

Protocol facts used (source: shairport-sync rtp.c / rtsp.c, cited in
lan-paths.md):
  * RTP audio = 12-byte RTP header + payload.  Payload type 0x60 = audio,
    0x56 = resend request (has a 4-byte prefix).  Receiver feeds the bytes
    after the 12-byte header straight to its decoder.
  * Sender -> receiver sync packets: 20 bytes, byte[1] == 0xd4, containing
    flags[2:4], rtp_timestamp_less_latency[4:8], NTP time[8:16],
    sync_rtp_timestamp[16:20].  The RECEIVER derives its latency as
    sync_rtp_timestamp - rtp_timestamp_less_latency, i.e. **the sender declares
    the latency in frames**.
  * Receiver -> sender timing probe: byte[1] == 0xd2 on the sender's timing
    port; the sender must echo it back as 0xd3.

Safety: no config is written, no volume is changed (the tone is quiet by
amplitude, not by touching device state), session is torn down, and the whole
thing lasts seconds.
"""

import argparse
import json
import os
import socket
import struct
import sys
import threading
import time

# ----------------------------------------------------------------- RTSP client
class Rtsp:
    """RTSP over TCP.

    MEASURED DEVICE QUIRK (probe_raop_variants.py + the isolation matrix in
    raop_variants.log): on this AirTunes/366.0 firmware an ANNOUNCE that follows
    another RTSP request **on the same TCP connection** is answered
    `403 Forbidden`, while the identical request on a **fresh connection** is
    answered `200 OK`.  Six trials:

        fresh, CSeq=1, url=/1, Client-Instance        -> 200
        fresh, CSeq=1, url=/1792246338, Client-Instance-> 200
        fresh, CSeq=1, url=/1, no Client-Instance      -> 200
        OPTIONS then ANNOUNCE CSeq=2, url=/1           -> 403
        OPTIONS then ANNOUNCE CSeq=2, url=/1792246338  -> 403
        fresh, CSeq=2, url=/1, Client-Instance         -> 200

    User-Agent is irrelevant (tested '', 'iTunes/12.0', 'AirPlay/366.0',
    'miplay-bridge-probe/0.1' -> all 200).  So we default to ONE TCP CONNECTION
    PER RTSP REQUEST, which is what the device clearly wants.
    """

    def __init__(self, ip, port=7000, timeout=6.0, per_request=True):
        self.ip, self.port, self.timeout = ip, port, timeout
        self.sock = None
        self.cseq = 0
        self.per_request = per_request
        self.transcript = []

    def connect(self):
        if not self.per_request:
            self.sock = socket.create_connection((self.ip, self.port), self.timeout)
            self.sock.settimeout(self.timeout)
        return self

    def _conn(self):
        if self.per_request:
            s = socket.create_connection((self.ip, self.port), self.timeout)
            s.settimeout(self.timeout)
            return s
        return self.sock

    def request(self, method, url, headers=None, body=None, want_body=False):
        self.cseq += 1
        h = {"CSeq": str(self.cseq), "User-Agent": "miplay-bridge-probe/0.1",
             "Client-Instance": "0123456789abcdef"}
        if headers:
            h.update(headers)
        if body is not None:
            if isinstance(body, str):
                body = body.encode()
            h["Content-Length"] = str(len(body))
        head = "%s %s RTSP/1.0\r\n" % (method, url)
        head += "".join("%s: %s\r\n" % (k, v) for k, v in h.items()) + "\r\n"
        raw = head.encode() + (body or b"")
        sock = self._conn()
        try:
            sock.sendall(raw)
            resp = self._read_response(sock)
        finally:
            if self.per_request:
                try:
                    sock.close()
                except OSError:
                    pass
        self.transcript.append({"cseq": self.cseq, "request": raw.decode("utf-8", "replace"),
                                "connection": "fresh" if self.per_request else "reused",
                                "response": resp["raw"]})
        return resp

    def _read_response(self, sock):
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(4096)
            if not chunk:
                break
            buf += chunk
        head, _, rest = buf.partition(b"\r\n\r\n")
        lines = head.decode("utf-8", "replace").split("\r\n")
        status_line = lines[0] if lines else ""
        code = 0
        parts = status_line.split(" ", 2)
        if len(parts) >= 2 and parts[1].isdigit():
            code = int(parts[1])
        hdrs = {}
        for ln in lines[1:]:
            if ":" in ln:
                k, v = ln.split(":", 1)
                hdrs[k.strip().lower()] = v.strip()
        body = rest
        clen = int(hdrs.get("content-length", "0") or 0)
        while len(body) < clen:
            chunk = sock.recv(4096)
            if not chunk:
                break
            body += chunk
        return {"status_line": status_line, "code": code, "headers": hdrs,
                "body": body.decode("utf-8", "replace"),
                "raw": (head + b"\r\n\r\n" + body).decode("utf-8", "replace")}

    def close(self):
        try:
            if self.sock:
                self.sock.close()
        except OSError:
            pass
        self.sock = None


def sdp_l16(session_id, local_ip, target_ip, ptime=352):
    return ("v=0\r\n"
            "o=iTunes %s 0 IN IP4 %s\r\n"
            "s=iTunes\r\n"
            "c=IN IP4 %s\r\n"
            "t=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\n"
            "a=rtpmap:96 L16/44100/2\r\n"
            "a=fmtp:96 0\r\n"
            "a=ptime:%d\r\n" % (session_id, local_ip, target_ip, ptime))


def sdp_alac(session_id, local_ip, target_ip):
    return ("v=0\r\n"
            "o=iTunes %s 0 IN IP4 %s\r\n"
            "s=iTunes\r\n"
            "c=IN IP4 %s\r\n"
            "t=0 0\r\n"
            "m=audio 0 RTP/AVP 96\r\n"
            "a=rtpmap:96 AppleLossless\r\n"
            "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n"
            % (session_id, local_ip, target_ip))


class Sender:
    """Minimal RAOP sender: RTSP control + RTP audio + sync/timing UDP."""

    def __init__(self, ip, codec="l16", prefix=4, latency_frames=88200, verbose=True):
        self.ip = ip
        self.codec = codec
        self.prefix = prefix
        self.latency_frames = latency_frames
        self.verbose = verbose
        self.log = []
        self.rtsp = Rtsp(ip)
        self.local_ip = None
        self.audio_sock = None
        self.control_sock = None
        self.timing_sock = None
        self.audio_local_port = None
        self.control_local_port = None
        self.timing_local_port = None
        self.server_port = None
        self.server_control_port = None
        self.server_timing_port = None
        self.session_id = str(int(time.time()))
        self.session_id_hdr = None
        self.running = False
        self.ssrc = 0x01234567
        self.seq = 1
        self.rtp_time = 0
        self.stop_flag = threading.Event()
        self.timing_thread = None
        self.sync_thread = None

    def _note(self, msg):
        self.log.append(msg)
        print("    %s" % msg, flush=True)

    # -------------------------------------------------------------- handshake
    def handshake(self):
        self.rtsp.connect()
        r = self.rtsp.request("OPTIONS", "*")
        self.log.append({"step": "OPTIONS", "code": r["code"], "raw": r["raw"]})
        print("  OPTIONS  -> %d %s" % (r["code"], r["status_line"]))

        # local IP via a throw-away UDP socket (RTSP uses a fresh TCP per request)
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        probe.connect((self.ip, 7000))
        self.local_ip = probe.getsockname()[0]
        probe.close()
        print("  local IP -> %s" % self.local_ip)
        url = "rtsp://%s/%s" % (self.ip, self.session_id)
        sdp = sdp_l16(self.session_id, self.local_ip, self.ip) if self.codec == "l16" \
            else sdp_alac(self.session_id, self.local_ip, self.ip)
        r = self.rtsp.request("ANNOUNCE", url,
                              {"Content-Type": "application/sdp"}, sdp)
        self.log.append({"step": "ANNOUNCE", "code": r["code"], "raw": r["raw"], "sdp": sdp})
        print("  ANNOUNCE -> %d %s   (codec announced: %s)"
              % (r["code"], r["status_line"], self.codec.upper()))
        if r["code"] != 200:
            return False

        # bind our three UDP sockets (audio is passive; control+timing receive)
        self.audio_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.audio_sock.bind((self.local_ip, 0))
        self.audio_local_port = self.audio_sock.getsockname()[1]
        self.control_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.control_sock.bind((self.local_ip, 0))
        self.control_sock.settimeout(1.0)
        self.control_local_port = self.control_sock.getsockname()[1]
        self.timing_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.timing_sock.bind((self.local_ip, 0))
        self.timing_sock.settimeout(1.0)
        self.timing_local_port = self.timing_sock.getsockname()[1]

        transport = ("RTP/AVP/UDP;unicast;mode=record;control_port=%d;timing_port=%d"
                     % (self.control_local_port, self.timing_local_port))
        r = self.rtsp.request("SETUP", url, {"Transport": transport})
        self.log.append({"step": "SETUP", "code": r["code"], "raw": r["raw"],
                         "transport_request": transport})
        print("  SETUP    -> %d %s" % (r["code"], r["status_line"]))
        print("     request  Transport: %s" % transport)
        print("     response Transport: %s" % r["headers"].get("transport"))
        if r["code"] != 200:
            return False
        for tok in (r["headers"].get("transport") or "").split(";"):
            tok = tok.strip()
            if tok.startswith("server_port="):
                self.server_port = int(tok.split("=", 1)[1])
            elif tok.startswith("control_port="):
                self.server_control_port = int(tok.split("=", 1)[1])
            elif tok.startswith("timing_port="):
                self.server_timing_port = int(tok.split("=", 1)[1])
        if r["headers"].get("session"):
            self.session_id_hdr = r["headers"]["session"]
            self.log.append({"session_header": self.session_id_hdr})
        else:
            self.session_id_hdr = None

        r = self.rtsp.request("RECORD", url,
                              {"RTP-Info": "seq=0;rtptime=0",
                               "Range": "npt=0-",
                               "Session": self.session_id_hdr or self.session_id})
        self.log.append({"step": "RECORD", "code": r["code"], "raw": r["raw"]})
        print("  RECORD   -> %d %s" % (r["code"], r["status_line"]))
        return r["code"] == 200

    # ------------------------------------------------------------- udp helpers
    def _start_udp(self):
        self.running = True
        self.timing_thread = threading.Thread(target=self._timing_loop, daemon=True)
        self.timing_thread.start()
        self.sync_thread = threading.Thread(target=self._sync_loop, daemon=True)
        self.sync_thread.start()

    def _timing_loop(self):
        """Receiver probes us with 0xd2; we must echo 0xd3."""
        self.timing_sock.settimeout(0.5)
        while self.running:
            try:
                data, addr = self.timing_sock.recvfrom(2048)
            except socket.timeout:
                continue
            except OSError:
                break
            if len(data) >= 32 and data[1] == 0xd2:
                origin = data[8:16]
                now = time.time()
                ntp = int((now + 2208988800) * (1 << 32)) & 0xFFFFFFFFFFFFFFFF
                reply = (bytes([0x80, 0xD3]) + data[2:4] + b"\x00\x00\x00\x00"
                         + origin + struct.pack("!Q", ntp) + struct.pack("!Q", ntp))
                try:
                    self.timing_sock.sendto(reply, addr)
                except OSError:
                    break
                self.log.append({"timing_request_hex": data.hex()[:64],
                                 "timing_reply_hex": reply.hex()})
                print("    <- timing request 0xd2 seq=%s from %s:%d, replied 0xd3"
                      % (struct.unpack("!H", data[2:4])[0], addr[0], addr[1]), flush=True)

    def _sync_loop(self):
        """Send 0xd4 sync packets so the receiver learns its latency anchor.

        Latency in frames is DECLARED BY US: sync_rtp_timestamp -
        rtp_timestamp_less_latency.
        """
        target = (self.ip, self.server_control_port or 7000)
        while self.running:
            now = time.time()
            ntp = int((now + 2208988800) * (1 << 32)) & 0xFFFFFFFFFFFFFFFF
            less_latency = self.rtp_time & 0xFFFFFFFF
            sync_ts = (less_latency + self.latency_frames) & 0xFFFFFFFF
            pkt = (bytes([0x80, 0xD4]) + struct.pack("!H", 4)
                   + struct.pack("!I", less_latency) + struct.pack("!Q", ntp)
                   + struct.pack("!I", sync_ts) + b"\x00\x00\x00\x00")
            try:
                self.control_sock.sendto(pkt, target)
            except OSError as exc:
                self.log.append({"sync_error": repr(exc)})
                break
            self.log.append({"sync_pkt_hex": pkt.hex(),
                             "declared_latency_frames": self.latency_frames,
                             "declared_latency_s": self.latency_frames / 44100.0})
            time.sleep(1.0)

    # ------------------------------------------------------------------- audio
    def send_tone(self, seconds=2.0, freq=440.0, amplitude=0.02, rate=44100, ptime=352):
        import math
        target = (self.ip, self.server_port)
        nframes = int(rate * seconds)
        frames_per_pkt = ptime
        t0 = time.time()
        sent = 0
        while sent < nframes and self.running:
            n = min(frames_per_pkt, nframes - sent)
            buf = bytearray()
            for i in range(n):
                # fade in/out so there is no click
                idx = sent + i
                env = min(1.0, idx / (rate * 0.05),
                          1.0 if idx < nframes - rate * 0.05 else
                          max(0.0, (nframes - idx) / (rate * 0.05)))
                v = int(32767 * amplitude * env *
                        math.sin(2 * math.pi * freq * idx / rate))
                buf += struct.pack("<hh", v, v)   # stereo s16le
            payload = (b"\x00\x00\x00\x00" + bytes(buf)) if self.prefix == 4 else bytes(buf)
            hdr = struct.pack("!BBHII", 0x80, 0x60, self.seq & 0xFFFF,
                              self.rtp_time & 0xFFFFFFFF, self.ssrc)
            self.audio_sock.sendto(hdr + payload, target)
            self.log.append({"rtp_seq": self.seq, "rtp_time": self.rtp_time,
                             "frames": n, "pkt_bytes": len(hdr) + len(payload),
                             "prefix_bytes": self.prefix})
            self.seq = (self.seq + 1) & 0xFFFF
            self.rtp_time += n
            sent += n
            # pace at real time
            target_t = t0 + sent / rate
            dt = target_t - time.time()
            if dt > 0:
                time.sleep(dt)
        print("  sent %d frames (%d RTP audio packets, prefix=%d bytes) in %.2fs"
              % (sent, (sent + frames_per_pkt - 1) // frames_per_pkt,
                 self.prefix, time.time() - t0))

    def teardown(self):
        self.running = False
        try:
            url = "rtsp://%s/%s" % (self.ip, self.session_id)
            r = self.rtsp.request("TEARDOWN", url,
                                  {"Session": self.session_id_hdr or self.session_id})
            self.log.append({"step": "TEARDOWN", "code": r["code"], "raw": r["raw"]})
            print("  TEARDOWN -> %d %s" % (r["code"], r["status_line"]))
        except Exception as exc:                                  # noqa: BLE001
            self.log.append({"step": "TEARDOWN", "error": repr(exc)})
        self.rtsp.close()
        for s in (self.audio_sock, self.control_sock, self.timing_sock):
            try:
                if s:
                    s.close()
            except OSError:
                pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--mode", choices=["handshake", "tone"], default="handshake")
    ap.add_argument("--codec", choices=["l16", "alac"], default="l16")
    ap.add_argument("--prefix", type=int, choices=[0, 4], default=0,
                    help="extra RTP payload bytes before PCM.  0 is correct for "
                         "type-0x60 audio; the 4-byte prefix belongs to type-0x56 "
                         "resend REQUESTS (receiver -> sender), not to audio.")
    ap.add_argument("--latency-frames", type=int, default=88200,
                    help="latency we DECLARE to the receiver (88200 = 2.0s @44.1k)")
    ap.add_argument("--seconds", type=float, default=2.0)
    ap.add_argument("--freq", type=float, default=440.0)
    ap.add_argument("--amplitude", type=float, default=0.02)
    ap.add_argument("--out", default="raop_session.json")
    args = ap.parse_args()

    print("=== RAOP third-party push test: %s:%d (mode=%s) ===" % (args.ip, 7000, args.mode))
    s = Sender(args.ip, codec=args.codec, prefix=args.prefix,
               latency_frames=args.latency_frames)
    result = {"ip": args.ip, "mode": args.mode, "codec": args.codec,
              "prefix_bytes": args.prefix, "latency_frames": args.latency_frames,
              "started": time.strftime("%Y-%m-%dT%H:%M:%S%z")}
    try:
        ok = s.handshake()
        result["handshake_accepted"] = ok
        result["legs"] = [{"step": e.get("step"), "code": e.get("code")}
                          for e in s.log if isinstance(e, dict) and e.get("step")]
        result["server_ports"] = {"server_port": s.server_port,
                                  "server_control_port": s.server_control_port,
                                  "server_timing_port": s.server_timing_port}
        if ok and args.mode == "tone":
            s._start_udp()
            time.sleep(1.2)          # let sync + timing settle
            print("  streaming %.1fs of %.0f Hz tone at amplitude %.3f ..."
                  % (args.seconds, args.freq, args.amplitude))
            s.send_tone(seconds=args.seconds, freq=args.freq, amplitude=args.amplitude)
            time.sleep(0.5)
        elif ok:
            # even with no audio, run sync/timing briefly to prove the UDP path
            s._start_udp()
            time.sleep(2.5)
    except Exception as exc:                                      # noqa: BLE001
        result["exception"] = repr(exc)
        print("  EXCEPTION: %r" % (exc,))
    finally:
        try:
            s.teardown()
        except Exception as exc:                                  # noqa: BLE001
            result["teardown_exception"] = repr(exc)
        result["log"] = s.log
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(result, fh, indent=2, ensure_ascii=False, default=str)
        print("report -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
