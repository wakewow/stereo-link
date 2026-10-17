#!/usr/bin/env python3
"""task-14：妙播拉流链路端到端延迟实测探针。

被测对象：mixplay-pc-audio/pc_stereo_hub.py（已能出声的 Python 实现）。
本脚本 **不修改任何产品代码**：只 import 它，并在运行时 monkeypatch 三个点来打点：

  1. ``hub.RingBuffer.write`` / ``read_from``  → 写游标、读游标、跳帧量
  2. ``hub.make_handler(...)`` 生成的 Handler → 每个路径**首个 GET 到达时刻**、连接字节数
  3. ``sysaudio.SystemAudioCapture.read``     → 每次采集调用返回的字节数与耗时

核心指标（最客观的音箱侧缓冲深度）：
    depth_seconds = (ring.cursor() - 该连接最后一次 read_from 的新位置) / 字节每秒
稳态时的中位数/波动范围即为「已写进 TCP 但音箱还没消费掉」的音频时长。

阶段：
  --phase delay   采集侧链路时延（不连音箱）：winsound 播 1kHz 短音 → 检测它出现在采集流里的时差 ×5
  --phase steady  稳态：左右音箱各拉一条流，采样缓冲深度 N 秒
  --phase trigger 云端触发开销：POST control/play → 音箱首个 HTTP GET 的时差 ×5（左右同时下发）
  --phase all     顺序跑上面三个阶段，并写出 CSV/JSON

用法（必须用能匹配防火墙规则的 base 解释器，见 README/报告）：
  set PYTHONPATH=...\\mixplay-pc-audio\\capture\\.venv\\Lib\\site-packages
  <base python> -u hub_latency_probe.py --phase all
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import socket
import statistics
import sys
import threading
import time
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKSPACE = HERE.parents[2]                     # default-workspace
HUB_DIR = WORKSPACE / "mixplay-pc-audio"
CAPTURE_DIR = HUB_DIR / "capture"

sys.path.insert(0, str(HUB_DIR))
sys.path.insert(0, str(CAPTURE_DIR))

import numpy as np  # noqa: E402

import pc_stereo_hub as hub  # noqa: E402
import sysaudio  # noqa: E402

RATE = hub.RATE
CHANNELS = hub.CHANNELS
FRAME_BYTES = hub.FRAME_BYTES
BYTE_RATE = RATE * FRAME_BYTES                # 176400 B/s

# 环形缓冲秒数：None = 与 hub.RING_SECONDS 一致（5.0s）。用 --ring-seconds 覆盖，
# 用来做「环形缓冲大小是否影响延迟」的对照实验（详见 latency-report.md 第 3 节）。
RING_SECONDS: float | None = None


def ring_capacity_bytes() -> int:
    seconds = RING_SECONDS if RING_SECONDS is not None else hub.RING_SECONDS
    return int(RATE * seconds) * FRAME_BYTES


# --------------------------------------------------------------------------- #
# 打点数据
# --------------------------------------------------------------------------- #
class Probe:
    def __init__(self) -> None:
        # RLock：stat() 会被采样线程在已持锁的临界区里再次调用（普通 Lock 会自锁死）
        self.lock = threading.RLock()
        self.t0 = time.monotonic()
        self.ring_stat: dict[int, dict] = {}      # id(ring) -> {...}
        self.ring_tag: dict[int, str] = {}
        self.depth_samples: list[dict] = []       # 采样线程写
        self.get_events: list[dict] = []          # 每个 wav 请求
        self.conn_events: list[dict] = []         # 每条流结束
        self.capture_calls: list[dict] = []       # 每次 read()
        self.skips: list[dict] = []               # read_from 跳帧
        self.onsets: list[dict] = []              # 采集流里检测到的突发起音
        self.writes: list[dict] = []              # 每次 wfile.write 的耗时（>5ms 视为阻塞）
        self.sock_bufs: list[dict] = []
        self.capture_blocks = 0

    def now(self) -> float:
        return time.monotonic()

    def stat(self, ring) -> dict:
        key = id(ring)
        with self.lock:
            s = self.ring_stat.get(key)
            if s is None:
                s = dict(tag=self.ring_tag.get(key, "?"), bytes_written=0, writes=0,
                         bytes_read=0, reads=0, last_read_pos=None, last_read_time=None,
                         last_cursor=0)
                self.ring_stat[key] = s
            return s

    def register(self, ring, tag: str) -> None:
        self.ring_tag[id(ring)] = tag
        self.stat(ring)


P = Probe()


# --------------------------------------------------------------------------- #
# monkeypatch：RingBuffer
# --------------------------------------------------------------------------- #
_orig_write = hub.RingBuffer.write
_orig_read_from = hub.RingBuffer.read_from


def _write(self, data: bytes) -> None:
    _orig_write(self, data)
    s = P.stat(self)
    with P.lock:
        s["bytes_written"] += len(data)
        s["writes"] += 1
        s["last_cursor"] = self.total


def _read_from(self, pos: int, maxbytes: int, timeout: float = 0.5):
    before = pos
    data, newpos = _orig_read_from(self, pos, maxbytes, timeout)
    t = P.now()
    s = P.stat(self)
    with P.lock:
        s["bytes_read"] += len(data)
        s["reads"] += 1
        s["last_read_pos"] = newpos
        s["last_read_time"] = t
        s["last_cursor"] = self.total
        if newpos > before + len(data):            # 环形缓冲丢弃了旧数据（客户端落后 > 容量）
            s["skips"] = s.get("skips", 0) + 1
            s["skipped_bytes"] = s.get("skipped_bytes", 0) + (newpos - before - len(data))
            P.skips.append(dict(t=t - P.t0, ring=s["tag"],
                                skipped_bytes=newpos - before - len(data)))
    return data, newpos


hub.RingBuffer.write = _write            # type: ignore[method-assign]
hub.RingBuffer.read_from = _read_from    # type: ignore[method-assign]


# --------------------------------------------------------------------------- #
# monkeypatch：SystemAudioCapture.read（采集侧耗时 + 起音检测）
# --------------------------------------------------------------------------- #
_orig_cap_read = sysaudio.SystemAudioCapture.read

ONSET_THRESHOLD = 0.02          # 相对满量程，短窗 RMS 超过它算起音（>=2% FS）
ONSET_WIN = 128                 # 短窗帧数，约 2.9ms


def _detect_onset(samples: np.ndarray) -> int | None:
    """返回块内第一个起音窗的起始帧号。samples: (n, ch) int16。"""
    if samples.size == 0:
        return None
    mono = samples[:, 0].astype(np.float32)
    n = len(mono)
    for i in range(0, max(1, n - ONSET_WIN), ONSET_WIN // 2):
        w = mono[i:i + ONSET_WIN]
        rms = float(np.sqrt(np.mean(np.square(w)))) / 32768.0
        if rms >= ONSET_THRESHOLD:
            return i
    return None


def _cap_read(self, nbytes: int, timeout: float | None = None) -> bytes:
    t0 = P.now()
    block = _orig_cap_read(self, nbytes, timeout)
    t1 = P.now()
    P.capture_blocks += 1
    idx = None
    if block:
        a = np.frombuffer(block, dtype=np.int16)
        if a.size >= CHANNELS:
            a = a[: (a.size // CHANNELS) * CHANNELS].reshape(-1, CHANNELS)
            idx = _detect_onset(a)
    with P.lock:
        P.capture_calls.append(dict(t=t1 - P.t0, bytes=len(block), dt=t1 - t0, onset=idx))
        if idx is not None:
            frames = len(block) // FRAME_BYTES
            # read() 返回的是「最近的 n 帧」：块尾 ≈ t1，块首 ≈ t1 - frames/RATE
            onset_t = t1 - (frames - idx) / RATE
            P.onsets.append(dict(t=onset_t - P.t0, t_return=t1 - P.t0,
                                 idx=idx, frames=frames))
            del P.onsets[:-40]
    return block


sysaudio.SystemAudioCapture.read = _cap_read    # type: ignore[method-assign]


# --------------------------------------------------------------------------- #
# monkeypatch：HTTP Handler（首个 GET 时刻 + 连接统计）
# --------------------------------------------------------------------------- #
def make_instrumented_handler(ring_l, ring_r, stop):
    Base = hub.make_handler(ring_l, ring_r, stop)

    class BlockWriter:
        """包住 unbuffered wfile，记录每次 socket write 的耗时（阻塞 = 对端收得慢）。"""

        def __init__(self, wfile, tag: str, ip: str) -> None:
            self._w = wfile
            self.tag = tag
            self.ip = ip
            self.bytes = 0
            self.calls = 0
            self.blocked_ms = 0.0

        def write(self, data):
            t0 = time.monotonic()
            n = self._w.write(data)
            dt = time.monotonic() - t0
            self.bytes += len(data)
            self.calls += 1
            if dt > 0.005:
                self.blocked_ms += dt * 1000.0
                with P.lock:
                    P.writes.append(dict(t=round(t0 - P.t0, 3), tag=self.tag, ip=self.ip,
                                         bytes=len(data), ms=round(dt * 1000, 1)))
            return n

        def __getattr__(self, name):
            return getattr(self._w, name)

    class Handler(Base):  # type: ignore[misc,valid-type]
        def do_GET(self):                       # noqa: N802
            path = self.path.split("?", 1)[0]
            t = P.now()
            if path in ("/left.wav", "/right.wav"):
                with P.lock:
                    P.get_events.append(dict(t=t - P.t0, path=path,
                                             ip=self.client_address[0], bytes_sent=None))
            return super().do_GET()

        def _serve(self, ring, tag):
            t0 = P.now()
            s0 = P.stat(ring)
            with P.lock:
                b0, r0 = s0["bytes_read"], s0["reads"]
            sndbuf = None
            try:
                sndbuf = self.connection.getsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF)
            except OSError:
                pass
            bw = BlockWriter(self.wfile, tag, self.client_address[0])
            self.wfile = bw
            with P.lock:
                P.sock_bufs.append(dict(t=round(t0 - P.t0, 3), tag=tag,
                                        ip=self.client_address[0], so_sndbuf=sndbuf))
            try:
                return super()._serve(ring, tag)
            finally:
                t1 = P.now()
                s1 = P.stat(ring)
                with P.lock:
                    P.conn_events.append(dict(
                        tag=tag, ip=self.client_address[0], start=t0 - P.t0, end=t1 - P.t0,
                        seconds=t1 - t0, bytes=s1["bytes_read"] - b0,
                        reads=s1["reads"] - r0, blocked_ms=round(bw.blocked_ms, 1),
                        slow_writes=sum(1 for w in P.writes if w["ip"] == self.client_address[0])))

    return Handler


# --------------------------------------------------------------------------- #
# 采样线程
# --------------------------------------------------------------------------- #
def sampler(rings: dict[str, object], stop: threading.Event, interval: float = 0.05) -> None:
    while not stop.is_set():
        t = P.now() - P.t0
        for tag, ring in rings.items():
            s = P.stat(ring)
            with P.lock:
                if s["last_read_pos"] is None:
                    continue
                depth = s["last_cursor"] - s["last_read_pos"]
                age = None if s["last_read_time"] is None else (P.now() - s["last_read_time"])
                P.depth_samples.append(dict(
                    t=round(t, 3), ring=tag, cursor=s["last_cursor"],
                    read_pos=s["last_read_pos"], depth_bytes=depth,
                    depth_s=round(depth / BYTE_RATE, 4), read_age_s=None if age is None else round(age, 3),
                    bytes_written=s["bytes_written"], bytes_read=s["bytes_read"]))
        stop.wait(interval)


# --------------------------------------------------------------------------- #
# 工具
# --------------------------------------------------------------------------- #
def make_tone(path: Path, freq: float, ms: int, amp: float, loop_seconds: float = 0.0) -> Path:
    """生成 1kHz 短音（或循环音）WAV，供 winsound 播放。"""
    frames = int(RATE * (loop_seconds if loop_seconds else ms / 1000.0))
    idx = np.arange(frames, dtype=np.float64)
    t = idx / RATE
    sig = np.sin(2 * math.pi * freq * t)
    if loop_seconds:                        # 0.5s 响 / 0.5s 静，便于人耳与检测
        sig = sig * ((np.floor(t) % 1.0) < 0.5)
    pcm = np.clip(sig * amp * 32767.0, -32768, 32767).astype(np.int16)
    stereo = np.repeat(pcm.reshape(-1, 1), 2, axis=1)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(stereo.tobytes())
    return path


def stats_of(vals: list[float]) -> dict:
    if not vals:
        return dict(n=0)
    v = sorted(vals)
    return dict(n=len(v),
                min=round(v[0], 4), p10=round(v[int(0.10 * (len(v) - 1))], 4),
                median=round(statistics.median(v), 4),
                p90=round(v[int(0.90 * (len(v) - 1))], 4), max=round(v[-1], 4),
                mean=round(statistics.fmean(v), 4))


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def serve(stop, ring_l, ring_r, port):
    from http.server import ThreadingHTTPServer
    httpd = ThreadingHTTPServer(("0.0.0.0", port), make_instrumented_handler(ring_l, ring_r, stop))
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    log(f"[服务] WAV 服务监听 0.0.0.0:{port}")
    return httpd


def trigger(base: str, did: str, url: str) -> tuple[float, float, dict]:
    t0 = P.now()
    res = hub.mixplay_control(base, {"action": "play_url", "target": did, "url": url})
    t1 = P.now()
    return t0 - P.t0, t1 - P.t0, res


def stop_both(base: str, did_l: str, did_r: str) -> None:
    for did in (did_l, did_r):
        try:
            hub.mixplay_control(base, {"action": "stop", "target": did})
        except Exception as exc:                     # noqa: BLE001
            log(f"[停止] {did} 失败: {exc!r}")


def wait_for_get(path: str, after: float, deadline_s: float) -> dict | None:
    end = P.now() + deadline_s
    while P.now() < end:
        with P.lock:
            for e in P.get_events:
                if e["path"] == path and e["t"] >= after - 0.05:
                    return dict(e)
        time.sleep(0.02)
    return None


# --------------------------------------------------------------------------- #
# 阶段
# --------------------------------------------------------------------------- #
def phase_delay(rounds: int, out: Path, chunk_ms: int = 50) -> list[dict]:
    """采集侧链路时延：播 1kHz 短音，量它在采集流里出现的时差。"""
    import winsound
    log(f"=== 阶段 1：采集侧链路时延（{rounds} 次，不连音箱；chunk_ms={chunk_ms}）===")
    tone = make_tone(out / "tone_200ms_1k.wav", 1000.0, 200, 0.4, loop_seconds=0.0)

    stop = threading.Event()
    cap = sysaudio.SystemAudioCapture(samplerate=RATE, channels=CHANNELS, chunk_ms=chunk_ms)
    cap.start()
    log(f"[采集] 设备: {cap.device_name}  {cap.samplerate}Hz/{cap.channels}ch "
        f"chunk_ms={chunk_ms} 每次 read 请求 {4096 * FRAME_BYTES} B(93ms)")

    def drain() -> None:                        # 必须有人持续 read()，数据才会流过打点
        while not stop.is_set():
            try:
                if not cap.read(4096 * FRAME_BYTES):
                    time.sleep(0.004)
            except Exception:                   # noqa: BLE001
                return

    threading.Thread(target=drain, daemon=True).start()
    time.sleep(1.0)
    rows: list[dict] = []
    try:
        for i in range(rounds):
            with P.lock:
                n0 = len(P.onsets)
            time.sleep(1.0)
            t_play = P.now() - P.t0
            winsound.PlaySound(str(tone), winsound.SND_FILENAME | winsound.SND_ASYNC)
            got = None
            end = P.now() + 3.0
            while P.now() < end:
                with P.lock:
                    if len(P.onsets) > n0:
                        got = dict(P.onsets[-1])
                        break
                time.sleep(0.005)
            if got is None:
                log(f"  第 {i + 1} 次：未检测到（阈值 {ONSET_THRESHOLD:.0%} FS）")
                rows.append(dict(round=i + 1, t_play=round(t_play, 3), delta_ms=None,
                                 frames=None, idx=None))
                continue
            winsound.PlaySound(None, winsound.SND_PURGE)
            delta_ms = (got["t"] - t_play) * 1000.0
            rows.append(dict(round=i + 1, t_play=round(t_play, 3), onset_t=round(got["t"], 4),
                             delta_ms=round(delta_ms, 1), frames=got["frames"], idx=got["idx"]))
            log(f"  第 {i + 1} 次：起音时差 {delta_ms:+.1f} ms（块 {got['frames']} 帧，"
                f"块内第 {got['idx']} 帧）")
            time.sleep(1.0)
    finally:
        winsound.PlaySound(None, winsound.SND_PURGE)
        stop.set()
        time.sleep(0.1)
        cap.stop()
    return rows


def phase_steady(seconds: float, port: int, base: str, did_l: str, did_r: str,
                 gain: float, play_tone: bool) -> dict:
    import winsound
    log(f"=== 阶段 2：稳态缓冲深度（{seconds:.0f}s）===")
    stop = threading.Event()
    ring_bytes = ring_capacity_bytes()
    ring_l, ring_r = hub.RingBuffer(ring_bytes), hub.RingBuffer(ring_bytes)
    P.register(ring_l, "left")
    P.register(ring_r, "right")
    httpd = serve(stop, ring_l, ring_r, port)

    worker = threading.Thread(target=hub.capture_worker,
                              args=(ring_l, ring_r, stop, None, gain), daemon=True)
    worker.start()
    th = threading.Thread(target=sampler, args=({"left": ring_l, "right": ring_r}, stop), daemon=True)
    th.start()

    # 等环形缓冲垫起数据；若采集线程没产出要显式报警（否则后面的深度数据全是 0 的假象）
    end = time.time() + 10
    while time.time() < end and ring_l.cursor() < 0.2 * BYTE_RATE:
        time.sleep(0.05)
    if ring_l.cursor() == 0:
        log("[采集] !! 10s 内采集线程没有写出任何数据：检查 sysaudio/端点占用；本阶段结果不可用")
    else:
        log(f"[采集] 缓冲已就绪: left={ring_l.cursor() / BYTE_RATE:.2f}s right="
            f"{ring_r.cursor() / BYTE_RATE:.2f}s audio")

    loop_wav = make_tone(HERE / "tone_loop_1k.wav", 1000.0, 0, 0.3, loop_seconds=4.0)
    if play_tone:
        winsound.PlaySound(str(loop_wav),
                           winsound.SND_FILENAME | winsound.SND_ASYNC | winsound.SND_LOOP)
        log("[测试音] 1kHz/0.3FS 循环播放（0.5s 响 0.5s 静）→ 采集端有真实内容")

    time.sleep(0.5)
    ip = hub.local_ip()
    url_l, url_r = f"http://{ip}:{port}/left.wav", f"http://{ip}:{port}/right.wav"
    log(f"[下发] 本机 LAN {ip}；左 {did_l} <- {url_l}；右 {did_r} <- {url_r}")
    t_l0, t_l1, res_l = trigger(base, did_l, url_l)
    t_r0, t_r1, res_r = trigger(base, did_r, url_r)
    log(f"[下发] 左 POST {t_l0:.3f}->{t_l1:.3f}s => {res_l}")
    log(f"[下发] 右 POST {t_r0:.3f}->{t_r1:.3f}s => {res_r}")

    t_connect = P.now() - P.t0
    log(f"[下发] 音箱开始拉流的探针时刻 t={t_connect:.3f}s")
    t_start = P.now()
    while P.now() - t_start < seconds and not stop.is_set():
        time.sleep(0.5)

    winsound.PlaySound(None, winsound.SND_PURGE)
    with P.lock:
        samples = list(P.depth_samples)
        conns = list(P.conn_events)
        writes = list(P.writes)
        bufs = list(P.sock_bufs)

    def live(tag: str, after: float) -> list[float]:
        """只在『读者最近 500ms 内确实读过』的样本上统计，排除断线后的空转。"""
        return [r["depth_s"] for r in samples
                if r["ring"] == tag and r["t"] > after
                and r["read_age_s"] is not None and r["read_age_s"] < 0.5
                and r["bytes_read"] > 0]

    all_l = [r["depth_s"] for r in samples if r["ring"] == "left" and r["t"] > t_connect]
    all_r = [r["depth_s"] for r in samples if r["ring"] == "right" and r["t"] > t_connect]
    summary = dict(
        t_connect=round(t_connect, 3),
        left_live=stats_of(live("left", t_connect + 3.0)),
        right_live=stats_of(live("right", t_connect + 3.0)),
        left_all=stats_of(all_l), right_all=stats_of(all_r),
        skips=len(P.skips),
        gets=[e for e in P.get_events],
        conns=conns,
        sock_bufs=bufs,
        slow_writes=writes,
    )
    for tag, key in (("左", "left_live"), ("右", "right_live")):
        s = summary[key]
        log(f"[深度] {tag} 活跃期缓冲深度 中位数 {s.get('median')}s "
            f"(p10 {s.get('p10')} / p90 {s.get('p90')} / max {s.get('max')}) n={s.get('n')}"
            f"  [含断线空转: 中位数 {summary[key.replace('_live', '_all')].get('median')}s]")
    log(f"[深度] 跳帧次数 {summary['skips']}；GET 事件 {len(P.get_events)}")
    for c in conns:
        rate = c["bytes"] / max(c["seconds"], 1e-9)
        log(f"[连接] {c['tag']} {c['ip']} {c['start']:.2f}->{c['end']:.2f}s 持续 {c['seconds']:.1f}s "
            f"发送 {c['bytes'] / 1e6:.2f}MB ({rate / 1024:.0f} kB/s，实时={BYTE_RATE / 1024:.0f}) "
            f"write 阻塞 {c.get('blocked_ms')}ms")
    for b in bufs:
        log(f"[socket] {b['tag']} SO_SNDBUF={b['so_sndbuf']} B "
            f"({(b['so_sndbuf'] or 0) / BYTE_RATE * 1000:.0f} ms 音频)")
    if writes:
        log(f"[socket] 单次 write 阻塞 >5ms 的次数: {len(writes)}")

    stop.set()
    ring_l.close()
    ring_r.close()
    time.sleep(0.3)
    stop_both(base, did_l, did_r)
    httpd.shutdown()
    return summary


def phase_trigger(repeats: int, port: int, base: str, did_l: str, did_r: str,
                  settle: float = 2.0, hold: float = 6.0, timeout: float = 25.0,
                  prefill: float = 0.0) -> list[dict]:
    log(f"=== 阶段 3：云端触发开销（{repeats} 次）"
        + (f"；下发前先垫 {prefill:.0f}s 音频（无读者，环形缓冲积到上限）" if prefill > 0 else "")
        + "===")
    stop = threading.Event()
    ring_bytes = ring_capacity_bytes()
    ring_l, ring_r = hub.RingBuffer(ring_bytes), hub.RingBuffer(ring_bytes)
    P.register(ring_l, "left")
    P.register(ring_r, "right")
    httpd = serve(stop, ring_l, ring_r, port)
    worker = threading.Thread(target=hub.capture_worker,
                              args=(ring_l, ring_r, stop, None, 1.0), daemon=True)
    worker.start()
    time.sleep(1.5)
    if prefill > 0:
        # 没有客户端时环形缓冲会一直积到容量上限；音箱一连上就能一次性拿到这些数据。
        # 用它来探测「音箱一次连接最多预取多少音频」。
        time.sleep(prefill)
        log(f"[预垫] 环形缓冲现有 left={ring_l.cursor() / BYTE_RATE:.2f}s "
            f"right={ring_r.cursor() / BYTE_RATE:.2f}s（容量 {ring_bytes / BYTE_RATE:.2f}s）")
    ip = hub.local_ip()
    url_l, url_r = f"http://{ip}:{port}/left.wav", f"http://{ip}:{port}/right.wav"

    rows: list[dict] = []
    try:
        for i in range(repeats):
            stop_both(base, did_l, did_r)
            time.sleep(settle)
            with P.lock:
                n0 = len(P.get_events)
            t_send = P.now()
            tl0, tl1, rl = trigger(base, did_l, url_l)
            tr0, tr1, rr = trigger(base, did_r, url_r)
            log(f"  第 {i + 1} 次 POST 左 {tl0:.3f}->{tl1:.3f}s {rl}；"
                f"右 {tr0:.3f}->{tr1:.3f}s {rr}")
            for tag, path, t_post, t_ret in (("左", "/left.wav", tl0, tl1),
                                             ("右", "/right.wav", tr0, tr1)):
                e = wait_for_get(path, after=t_ret, deadline_s=timeout)
                if e is None:
                    log(f"    {tag} {path}: {timeout:.0f}s 内没有 GET")
                    rows.append(dict(attempt=i + 1, target=tag, path=path,
                                     t_post=round(t_post, 3), t_post_done=round(t_ret, 3),
                                     t_first_get=None, delta_s=None,
                                     post_return_ms=round((t_ret - t_post) * 1000, 1), ok=False))
                    continue
                delta = e["t"] - t_ret
                rows.append(dict(attempt=i + 1, target=tag, path=path, ip=e["ip"],
                                 t_post=round(t_post, 3), t_post_done=round(t_ret, 3),
                                 t_first_get=round(e["t"], 3), delta_s=round(delta, 3),
                                 post_return_ms=round((t_ret - t_post) * 1000, 1),
                                 get_since_post_ms=round((e["t"] - t_post) * 1000, 1), ok=True))
                log(f"    {tag} 首个 GET 在 POST 返回后 {delta * 1000:.0f} ms "
                    f"(距 POST 发出 {(e['t'] - t_post) * 1000:.0f} ms) 来自 {e['ip']}")
            time.sleep(hold)
            del t_send, n0
    finally:
        stop_both(base, did_l, did_r)
        stop.set()
        ring_l.close()
        ring_r.close()
        time.sleep(0.3)
        httpd.shutdown()
    return rows


def phase_single(seconds: float, port: int, base: str, targets: list[tuple[str, str]],
                 gain: float = 1.0) -> dict:
    """只对其中一只音箱下发，观察它的连接生命周期（用来解释「左箱 1.1s 就断」）。"""
    log(f"=== 阶段 4：单目标连接观察 {[t[0] for t in targets]} {seconds:.0f}s ===")
    stop = threading.Event()
    ring_bytes = ring_capacity_bytes()
    ring_l, ring_r = hub.RingBuffer(ring_bytes), hub.RingBuffer(ring_bytes)
    P.register(ring_l, "left")
    P.register(ring_r, "right")
    httpd = serve(stop, ring_l, ring_r, port)
    worker = threading.Thread(target=hub.capture_worker,
                              args=(ring_l, ring_r, stop, None, gain), daemon=True)
    worker.start()
    sampler_thread = threading.Thread(target=sampler,
                                      args=({"left": ring_l, "right": ring_r}, stop), daemon=True)
    sampler_thread.start()
    time.sleep(1.5)
    ip = hub.local_ip()
    for name, did in targets:
        path = "/left.wav" if name == "左" else "/right.wav"
        url = f"http://{ip}:{port}{path}"
        t0, t1, res = trigger(base, did, url)
        log(f"[下发] {name} {did} <- {url} POST {t0:.3f}->{t1:.3f}s => {res}")
    end = P.now() + seconds
    while P.now() < end:
        time.sleep(1.0)
    with P.lock:
        out = dict(gets=list(P.get_events), conns=list(P.conn_events), skips=list(P.skips),
                   sock_bufs=list(P.sock_bufs), slow_writes=list(P.writes))
    for c in out["conns"]:
        log(f"[连接] {c['tag']} {c['ip']} {c['start']:.2f}->{c['end']:.2f}s 持续 "
            f"{c['seconds']:.1f}s 发送 {c['bytes']} B ({c['bytes'] / max(c['seconds'], 1e-9) / 1024:.0f} kB/s)")
    stop.set()
    ring_l.close()
    ring_r.close()
    time.sleep(0.3)
    for _, did in targets:
        try:
            hub.mixplay_control(base, {"action": "stop", "target": did})
        except Exception as exc:                 # noqa: BLE001
            log(f"[停止] {did} 失败: {exc!r}")
    httpd.shutdown()
    return out


def dump_csv(path: Path, rows: list[dict]) -> None:
    if not rows:
        log(f"[输出] {path.name}: 无数据")
        return
    keys: list[str] = []
    for r in rows:
        for k in r:
            if k not in keys:
                keys.append(k)
    with path.open("w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=keys)
        w.writeheader()
        w.writerows(rows)
    log(f"[输出] {path.name}: {len(rows)} 行")


# --------------------------------------------------------------------------- #
def main() -> int:
    ap = argparse.ArgumentParser(description="妙播拉流链路延迟实测探针")
    ap.add_argument("--phase", default="all",
                    choices=["all", "delay", "steady", "trigger", "single"])
    ap.add_argument("--only", default="left", choices=["left", "right", "both"],
                    help="single 阶段对哪只音箱下发")
    ap.add_argument("--mixplay", default="http://127.0.0.1:8820")
    ap.add_argument("--port", type=int, default=8930)
    ap.add_argument("--left", default=hub.DID_LEFT)
    ap.add_argument("--right", default=hub.DID_RIGHT)
    ap.add_argument("--steady-seconds", type=float, default=45.0)
    ap.add_argument("--trigger-repeats", type=int, default=5)
    ap.add_argument("--prefill-seconds", type=float, default=0.0,
                    help="trigger 阶段：下发前先让环形缓冲积 N 秒（探测音箱预取上限）")
    ap.add_argument("--delay-rounds", type=int, default=5)
    ap.add_argument("--chunk-ms", type=int, default=50,
                    help="采集块大小（sysaudio 默认 50ms；用来做压缩对照实验）")
    ap.add_argument("--gain", type=float, default=1.0)
    ap.add_argument("--no-tone", action="store_true", help="稳态阶段不播测试音")
    ap.add_argument("--out", default=str(HERE))
    ap.add_argument("--tag", default="", help="写 JSON 时的后缀，避免多阶段互相覆盖")
    ap.add_argument("--ring-seconds", type=float, default=None,
                    help="覆盖环形缓冲秒数（对照实验；默认与 hub.RING_SECONDS 一致）")
    args = ap.parse_args()

    global RING_SECONDS
    RING_SECONDS = args.ring_seconds
    if RING_SECONDS is not None:
        log(f"[配置] 环形缓冲覆盖为 {RING_SECONDS}s "
            f"({ring_capacity_bytes()} B，hub 默认 {hub.RING_SECONDS}s)")

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    log(f"探针启动 phase={args.phase} 工作目录={out}")
    spk_name = "?"
    try:
        spk = sysaudio.list_speakers()
        first = spk[0] if spk else None
        spk_name = (first.get("name") if isinstance(first, dict) else str(first)) or "?"
    except Exception as exc:                     # noqa: BLE001
        spk_name = f"<枚举失败 {exc!r}>"
    log(f"Python {sys.version.split()[0]} / numpy {np.__version__} / 默认播放端点 {spk_name}")

    result: dict = dict(phase=args.phase, started=time.strftime("%Y-%m-%d %H:%M:%S"))
    try:
        if args.phase in ("all", "delay"):
            rows = phase_delay(args.delay_rounds, out, args.chunk_ms)
            dump_csv(out / f"capture_delay{args.tag}.csv", rows)
            result["capture_delay_ms"] = stats_of([r["delta_ms"] for r in rows if r["delta_ms"]])
            result["capture_delay_rows"] = rows
        if args.phase in ("all", "steady"):
            result["steady"] = phase_steady(args.steady_seconds, args.port, args.mixplay,
                                           args.left, args.right, args.gain, not args.no_tone)
            dump_csv(out / "depth_samples.csv", P.depth_samples)
            with P.lock:
                cap_calls = list(P.capture_calls)
            if cap_calls:
                dts = [c["dt"] * 1000 for c in cap_calls]
                result["capture_calls"] = dict(
                    n=len(cap_calls),
                    bytes_median=int(statistics.median([c["bytes"] for c in cap_calls])),
                    dt_ms_median=round(statistics.median(dts), 2),
                    dt_ms_p90=round(sorted(dts)[int(0.9 * (len(dts) - 1))], 2),
                    bytes_per_call_seconds=round(
                        statistics.median([c["bytes"] for c in cap_calls]) / BYTE_RATE, 3))
        if args.phase in ("all", "trigger"):
            rows = phase_trigger(args.trigger_repeats, args.port, args.mixplay,
                                 args.left, args.right, prefill=args.prefill_seconds)
            dump_csv(out / f"trigger_times{args.tag}.csv", rows)
            ok = [r["delta_s"] for r in rows if r["ok"]]
            result["trigger_to_first_get_s"] = stats_of(ok)
            result["trigger_post_return_ms"] = stats_of([r["post_return_ms"] for r in rows])
            result["trigger_rows"] = rows
        if args.phase == "single":
            tg = {"left": [("左", args.left)], "right": [("右", args.right)],
                  "both": [("左", args.left), ("右", args.right)]}[args.only]
            result["single"] = phase_single(args.steady_seconds, args.port, args.mixplay, tg,
                                            args.gain)
            dump_csv(out / "single_conns.csv", result["single"]["conns"])
    except KeyboardInterrupt:
        log("收到 Ctrl+C，收尾")
    finally:
        dump_csv(out / "depth_samples.csv", P.depth_samples)
        with P.lock:
            result["conn_events"] = P.conn_events
            result["get_events"] = P.get_events
            result["skips"] = P.skips
        (out / f"latency_summary{args.tag}.json").write_text(
            json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
        log(f"[输出] latency_summary{args.tag}.json 已写入")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
