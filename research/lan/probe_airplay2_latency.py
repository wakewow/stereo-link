"""Measure the real, acoustic push-to-sound latency of the LAN AirPlay 2 path.

Idea
----
We want "how long after the sender decides to play does sound actually leave the
speaker".  The host clock and the microphone clock have an unknown offset, so an
absolute measurement is impossible.  Instead we put TWO events into the SAME
microphone recording:

  * a reference click played on the PC's own output device (near-zero latency,
    only the local audio device buffer),
  * the 440 Hz tone pushed to the speaker over AirPlay 2.

Both events also have a host-clock timestamp (when ffplay / the RTSP RECORD was
issued).  The microphone timeline difference minus the host timeline difference
cancels the unknown clock offset and yields the AirPlay latency *relative to
local playback*.

Errors
------
ffplay opens the audio device lazily (a few hundred ms), so the reference click
is emitted LATER than ``t_click``.  That inflates the host side of the
subtraction, i.e. the reported AirPlay latency is a LOWER bound.  We therefore
fire two clicks at a known host interval and compare with the microphone
interval: if they agree, the detector and the playback path are consistent and
the residual bias is only the (constant) ffplay device-open time.

Only stdlib + pyatv; the analysis uses a Goertzel filter, no numpy needed.
"""
from __future__ import annotations

import argparse
import ast
import asyncio
import json
import math
import os
import re
import struct
import subprocess
import sys
import time
import wave
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))

# ---------------------------------------------------------------- test signals

def write_wav(path: str, segments, rate: int = 44100, amp: float = 0.6) -> None:
    """segments: list of (seconds, freq_hz or None for silence)."""
    frames = bytearray()
    for seconds, freq in segments:
        n = int(seconds * rate)
        for i in range(n):
            if freq is None:
                frames += struct.pack("<h", 0)
            else:
                # fade in/out 5 ms to avoid clicks
                env = 1.0
                fade = int(0.005 * rate)
                if i < fade:
                    env = i / fade
                elif i > n - fade:
                    env = max(0.0, (n - i) / fade)
                v = int(32767 * amp * env * math.sin(2 * math.pi * freq * i / rate))
                frames += struct.pack("<h", max(-32768, min(32767, v)))
    with wave.open(path, "wb") as fh:
        fh.setnchannels(1)
        fh.setsampwidth(2)
        fh.setframerate(rate)
        fh.writeframes(bytes(frames))


# ---------------------------------------------------------------- mic capture

def start_mic(mic: str, seconds: float, path: str) -> subprocess.Popen:
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "dshow",
           "-i", "audio=%s" % mic, "-t", "%.2f" % seconds,
           "-ac", "1", "-ar", "48000", "-y", path]
    return subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def start_click(path: str) -> subprocess.Popen:
    cmd = ["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet", path]
    return subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


# ---------------------------------------------------------------- analysis

def read_mono(path: str):
    with wave.open(path, "rb") as fh:
        rate = fh.getframerate()
        n = fh.getnframes()
        raw = fh.readframes(n)
    samples = struct.unpack("<%dh" % (len(raw) // 2), raw[:len(raw) // 2 * 2])
    return rate, samples


def goertzel_mag(samples, start: int, length: int, rate: int, freq: float) -> float:
    k = int(0.5 + length * freq / rate)
    w = 2.0 * math.pi * k / length
    coeff = 2.0 * math.cos(w)
    s_prev = s_prev2 = 0.0
    end = min(start + length, len(samples))
    if end - start < length:
        return 0.0
    for i in range(start, end):
        s = samples[i] + coeff * s_prev - s_prev2
        s_prev2, s_prev = s_prev, s
    power = s_prev2 * s_prev2 + s_prev * s_prev - coeff * s_prev * s_prev2
    return math.sqrt(max(0.0, power)) / length


def envelope(samples, rate: int, freq: float, win_ms: float = 5.0):
    win = int(rate * win_ms / 1000.0)
    hop = win
    out = []
    for start in range(0, len(samples) - win, hop):
        out.append(goertzel_mag(samples, start, win, rate, freq))
    return out, (hop / rate)


def find_onset(env, hop_s, start_idx, noise, ratio=6.0, sustain=3):
    """first window >= start_idx where env exceeds noise*ratio for `sustain` windows"""
    thr = max(noise * ratio, 1e-6)
    run = 0
    for i in range(start_idx, len(env)):
        if env[i] > thr:
            run += 1
            if run >= sustain:
                return (i - run + 1) * hop_s, i
        else:
            run = 0
    return None, None


def median(xs):
    xs = sorted(xs)
    if not xs:
        return 0.0
    return xs[len(xs) // 2]


# ---------------------------------------------------------------- host clocks

def parse_log_time(line: str):
    m = re.match(r"(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}),(\d{3})", line)
    if not m:
        return None
    base = datetime.strptime(m.group(1), "%Y-%m-%d %H:%M:%S")
    return time.mktime(base.timetuple()) + int(m.group(2)) / 1000.0


def find_record_time(logpath: str):
    """host epoch (time.time() scale) of the RTSP RECORD send, from pyatv's log"""
    try:
        with open(logpath, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if "Sending RTSP/1.0 message" in line and "RECORD" in line:
                    t = parse_log_time(line)
                    if t:
                        return t
    except OSError:
        pass
    return None


def find_setup_latency(logpath: str):
    """decoded latencyMin/latencyMax that pyatv declared in SETUP"""
    out = {}
    try:
        with open(logpath, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    except OSError:
        return out
    for m in re.finditer(r"Sending (?:RTSP|HTTP)/1\.0 message: b'(.*)'", text):
        try:
            raw = ast.literal_eval("b'" + m.group(1) + "'")
        except Exception:                                          # noqa: BLE001
            continue
        if b"latencyMin" not in raw:
            continue
        try:
            import plistlib
            body = raw.split(b"\r\n\r\n", 1)[1]
            obj = plistlib.loads(body[body.find(b"bplist00"):])
        except Exception:                                          # noqa: BLE001
            continue
        for stream in obj.get("streams", []):
            out["latencyMin_frames"] = stream.get("latencyMin")
            out["latencyMax_frames"] = stream.get("latencyMax")
            out["sr"] = stream.get("sr")
            out["spf"] = stream.get("spf")
            out["audioFormat"] = stream.get("audioFormat")
    if out.get("sr"):
        out["latencyMin_ms"] = round(1000.0 * out["latencyMin_frames"] / out["sr"], 1)
        out["latencyMax_ms"] = round(1000.0 * out["latencyMax_frames"] / out["sr"], 1)
    return out


# ---------------------------------------------------------------- main

async def run(args, result):
    import pyatv
    sys.path.insert(0, HERE)
    from probe_airplay2_stream import manual_config

    tone = os.path.join(HERE, "airplay_tone_440.wav")
    click = os.path.join(HERE, "ref_click_1500.wav")
    mic_wav = os.path.join(HERE, "mic_capture.wav")

    # 0.5 s lead-in, 1.0 s of 440 Hz, then silence so the tail is clean
    write_wav(tone, [(0.5, None), (1.0, 440.0), (0.8, None)])
    # 0.3 s lead-in, 60 ms of 1500 Hz, tail
    write_wav(click, [(0.3, None), (0.06, 1500.0), (0.4, None)])
    print("signals: %s / %s" % (os.path.basename(tone), os.path.basename(click)))

    loop = asyncio.get_running_loop()
    conf = manual_config(args.ip)

    print("=== connect (pairing happens here, OUTSIDE the measured window) ===")
    atv = await pyatv.connect(conf, loop)
    old_volume = atv.audio.volume
    print("  connected, volume=%s" % old_volume)

    try:
        await atv.audio.set_volume(args.volume)
        print("  volume set to %s" % args.volume)

        print("=== start microphone %r for %.1fs ===" % (args.mic, args.capture))
        mic = start_mic(args.mic, args.capture, mic_wav)
        t_mic_launch = time.time()
        await asyncio.sleep(2.2)          # let dshow settle

        # reference click #1
        t_click1 = time.time()
        start_click(click)
        await asyncio.sleep(1.4)
        # reference click #2 -- same host interval check
        t_click2 = time.time()
        start_click(click)
        await asyncio.sleep(1.4)

        # ...and now the AirPlay tone
        t_call = time.time()
        await atv.stream.stream_file(tone)
        t_done = time.time()
        print("  stream_file returned after %.3fs" % (t_done - t_call))

        await asyncio.sleep(0.8)
        try:
            mic.wait(timeout=args.capture + 15)
        except subprocess.TimeoutExpired:
            mic.kill()

        result["host"].update({
            "t_mic_launch": t_mic_launch, "t_click1": t_click1,
            "t_click2": t_click2, "t_call": t_call, "t_stream_done": t_done,
        })
        t_record = find_record_time(args.log)
        if t_record:
            # pyatv logs "Sending ... RECORD" in local wall-clock time
            result["host"]["t_record_from_log"] = t_record
            # the log clock and time.time() share the same epoch on Windows
            result["host"]["record_minus_call_s"] = round(t_record - t_call, 3)

        print("=== analysing %s ===" % mic_wav)
        rate, samples = read_mono(mic_wav)
        dur = len(samples) / rate
        print("  mic: %d samples @ %d Hz = %.2fs" % (len(samples), rate, dur))

        env440, hop = envelope(samples, rate, 440.0)
        env1500, _ = envelope(samples, rate, 1500.0)
        noise = median(env440[:int(0.8 / hop)]) + median(env1500[:int(0.8 / hop)])

        c1, _ = find_onset(env1500, hop, 0, noise)
        c2, _ = find_onset(env1500, hop, int((c1 or 0) / hop) + 40, noise)
        tone, _ = find_onset(env440, hop, int((c2 or 0) / hop) + 40, noise)

        print("  click#1 @ %.3fs   click#2 @ %.3fs   tone440 @ %.3fs (mic timeline)"
              % (c1 or -1, c2 or -1, tone if tone is not None else -1))
        result["mic"].update({"rate": rate, "duration_s": round(dur, 3),
                              "click1_s": c1, "click2_s": c2, "tone_onset_s": tone,
                              "noise_floor": noise})

        if c1 and c2:
            result["checks"]["click_host_interval_s"] = round(t_click2 - t_click1, 3)
            result["checks"]["click_mic_interval_s"] = round(c2 - c1, 3)
            result["checks"]["interval_error_ms"] = round(
                1000.0 * ((c2 - c1) - (t_click2 - t_click1)), 1)

        if tone is not None and c2:
            d_mic = tone - c2
            d_host = (t_record - t_click2) if t_record else None
            result["measured"]["mic_delta_s"] = round(d_mic, 3)
            if d_host is not None:
                result["measured"]["host_delta_s"] = round(d_host, 3)
                lat = d_mic - d_host
                result["measured"]["airplay_latency_vs_local_s"] = round(lat, 3)
                result["measured"]["airplay_latency_vs_local_ms"] = round(lat * 1000, 1)
                print("\n  ==> AirPlay push-to-sound, relative to local playback: "
                      "%.0f ms  (LOWER bound, see module docstring)" % (lat * 1000))
            else:
                print("\n  !! RTSP RECORD time not found in the log; "
                      "mic delta only: %.3fs" % d_mic)

        result["declared"] = find_setup_latency(args.log)
        print("  declared latency window in our own SETUP: %s" % result["declared"])
    finally:
        try:
            await atv.audio.set_volume(old_volume)
            print("  volume restored to %s" % old_volume)
            result["volume_restored"] = old_volume
        except Exception as exc:                                   # noqa: BLE001
            print("  volume restore failed: %r" % (exc,))
        try:
            res = atv.close()
            if asyncio.iscoroutine(res):        # facade.close() is sync in pyatv
                await res
        except Exception as exc:                                   # noqa: BLE001
            print("  close failed: %r" % (exc,))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--volume", type=float, default=30)
    ap.add_argument("--capture", type=float, default=11.0)
    ap.add_argument("--mic", default="麦克风 (Chu2 DSP)")
    ap.add_argument("--out", default=os.path.join(HERE, "airplay2_latency.json"))
    ap.add_argument("--log", default=os.path.join(HERE, "airplay2_latency_debug.log"))
    args = ap.parse_args()

    import logging
    logging.basicConfig(
        level=logging.DEBUG,
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
        handlers=[logging.FileHandler(args.log, mode="w", encoding="utf-8")],
    )

    result = {"target": args.ip, "host": {}, "mic": {}, "measured": {},
              "checks": {}, "declared": {}}
    try:
        asyncio.run(run(args, result))
    except Exception as exc:                                       # noqa: BLE001
        result["fatal"] = repr(exc)
        print("FATAL: %r" % (exc,))
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, ensure_ascii=False)
    print("\nreport -> %s" % args.out)


if __name__ == "__main__":
    main()
