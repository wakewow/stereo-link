#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""全矩阵自环测试：本机 *全部* render 端点 x *全部* capture 端点（含 [loopback]）。

方法
----
对每个 render 端点播一次 1 kHz 正弦（0.25 FS / 2 s），**同时对全部 capture 端点并行录音**。
对每条录音做 1 kHz 单频（Goertzel/相关）包络分析：
  - tone_dbfs  : 播放窗内 1 kHz 分量的幅度（dBFS）——数字自环 ≈ -12 dBFS，声学串音通常 < -40 dBFS
  - latency_ms : 播放开始 -> 采集到首个音频块 的墙钟延迟（含采集缓冲，粗略值）
  - dropout    : 播放窗内 1 kHz 包络跌落到中位数 -20 dB 以下的比例（丢帧/断续）
这样一轮播放即可得到「该 render 端点 -> 所有 capture 端点」的完整一行。

第二相：对第一阶段检出通路的候选对，跑 6 s 持续音，给出稳定性/丢帧/延迟的更细致数据。

输出
----
  research/vad/loopback_matrix.json   原始数值（机器可读）
  research/vad/loopback_matrix.txt    UTF-8 文本矩阵（人读）

用法（必须用 capture 的 venv，里面有 soundcard + numpy）
  D:\\...\\mixplay-pc-audio\\capture\\.venv\\Scripts\\python.exe -u probe_loopback_matrix.py
可选
  --rate 48000 --tone-sec 2.0 --skip-sustain
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
import time
from pathlib import Path

import numpy as np
import soundcard as sc

try:  # 中文输出兜底
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001
    pass

FREQ = 1000.0
LEVEL = 0.25          # 0.25 FS ~= -12 dBFS
WARMUP_SEC = 0.6
TAIL_SEC = 0.6
BLOCK = 1024          # 每块采样数
ENV_MS = 10.0         # 包络步长
DETECT_MARGIN_DB = 20.0   # 判决：峰值比本底高 20 dB 视为「收到该次播放」
STRONG_MARGIN_DB = 6.0    # 播放窗：包络在峰值 6 dB 内的块
DROPOUT_DROP_DB = 20.0    # 播放窗内低于中位数 20 dB 的块 = 丢帧


# ---------------------------------------------------------------- 录音线程
class Rec:
    """一个 capture 端点的后台录音线程。"""

    def __init__(self, mic, rate: int):
        self.mic = mic
        self.rate = rate
        self.name = mic.name
        self.loopback = bool(getattr(mic, "isloopback", False))
        self.channels = 1 if (getattr(mic, "channels", 1) or 1) < 2 else 2
        self.frames: list[np.ndarray] = []
        self.ts: list[float] = []          # 每块「收到」的 perf_counter
        self.err: str | None = None
        self.t_open0: float | None = None
        self.t_open1: float | None = None
        self._stop = threading.Event()
        self.thread: threading.Thread | None = None

    def start(self) -> None:
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self) -> None:
        try:
            self.t_open0 = time.perf_counter()
            with self.mic.recorder(samplerate=self.rate, channels=self.channels,
                                   blocksize=BLOCK) as rec:
                self.t_open1 = time.perf_counter()
                while not self._stop.is_set():
                    data = rec.record(numframes=BLOCK)
                    t = time.perf_counter()
                    if data is None or len(data) == 0:
                        continue
                    a = np.asarray(data, dtype=np.float32)
                    if a.ndim > 1:
                        a = a.mean(axis=1)
                    self.frames.append(a)
                    self.ts.append(t)
        except Exception as e:  # noqa: BLE001
            self.err = f"{type(e).__name__}: {e}"

    def stop(self) -> None:
        self._stop.set()
        if self.thread:
            self.thread.join(timeout=6)

    def audio(self) -> np.ndarray:
        if not self.frames:
            return np.zeros(0, dtype=np.float32)
        return np.concatenate(self.frames)


# ---------------------------------------------------------------- 信号分析
def tone_env_db(a: np.ndarray, rate: int, step: int) -> np.ndarray:
    """按 step 采样的 1 kHz 单频包络（dBFS）。"""
    n = len(a) // step
    if n <= 0:
        return np.zeros(0)
    idx = np.arange(step)
    ref = np.exp(-2j * np.pi * FREQ * idx / rate)
    out = np.empty(n)
    for i in range(n):
        seg = a[i * step:(i + 1) * step].astype(np.float64)
        out[i] = 20.0 * np.log10(max(abs(np.dot(seg, ref)) * 2.0 / step, 1e-9))
    return out


def refine_onset(a: np.ndarray, rate: int, lo: int, hi: int, thresh_db: float) -> int | None:
    """在样本区间 [lo,hi) 内以 1 ms 分辨率找第一个越过阈值的点，返回样本下标。"""
    step = max(8, int(rate / 1000))
    lo = max(0, lo)
    hi = min(len(a), hi)
    if hi - lo < step:
        return None
    env = tone_env_db(a[lo:hi], rate, step)
    for i, v in enumerate(env):
        if v >= thresh_db:
            return lo + i * step
    return None


def analyze(rec: Rec, rate: int, play_start: float, play_end: float) -> dict:
    """把一条录音 + 播放时间窗，变成「有没有传声 / 多响 / 多快 / 稳不稳」。"""
    a = rec.audio()
    out = {
        "capture": rec.name,
        "loopback": rec.loopback,
        "channels": rec.channels,
        "error": rec.err,
        "seconds": round(len(a) / rate, 3),
        "blocks": len(rec.frames),
        "tone_dbfs": None,
        "peak_dbfs": None,
        "noise_dbfs": None,
        "latency_ms": None,
        "dropout_ratio": None,
        "max_recv_gap_ms": None,
        "detected": False,
        "walls": [],
    }
    if rec.err or len(a) == 0:
        return out

    step = max(64, int(rate * ENV_MS / 1000))
    env = tone_env_db(a, rate, step)
    if env.size == 0:
        return out
    peak = float(np.percentile(env, 99.9))
    # 本底用 5 分位而不是 25 分位：当音比静音段还长时（如 6 s 持续音 + 1.2 s 静音，
    # 静音只占 ~17%），25 分位会落进音里 → peak-floor 变小 → detected=False 的假阴性。
    floor = float(np.percentile(env, 5))
    out["tone_dbfs"] = round(peak, 2)
    out["noise_dbfs"] = round(floor, 2)
    out["peak_dbfs"] = round(20 * np.log10(max(float(np.abs(a).max()), 1e-9)), 2)
    out["detected"] = bool(peak - floor >= DETECT_MARGIN_DB and peak > -60.0)
    if not out["detected"]:
        return out

    strong = np.where(env >= peak - STRONG_MARGIN_DB)[0]
    i0, i1 = int(strong[0]), int(strong[-1])
    # 墙钟时间映射（块粒度） -> 用块时间戳
    per_block = max(1, BLOCK // step)
    b0 = min(i0 // per_block, len(rec.ts) - 1)
    # 1 ms 细化起音点
    lo = max(0, (b0 - 1) * BLOCK)
    onset_sample = refine_onset(a, rate, lo, lo + 3 * BLOCK, peak - 6.0)
    if onset_sample is not None and rec.ts:
        # 块 b 覆盖的音频起点墙钟 ≈ ts[b] - BLOCK/rate
        blk = min(onset_sample // BLOCK, len(rec.ts) - 1)
        t_audio_start = rec.ts[blk] - BLOCK / rate
        out["latency_ms"] = round((t_audio_start - play_start) * 1000, 1)
        out["onset_wall"] = round(t_audio_start, 6)

    # 播放窗内稳定性：取一段连续强信号
    med = float(np.median(env[strong]))
    win = [i for i in range(i0, i1 + 1) if env[i] >= peak - STRONG_MARGIN_DB]
    # 补上中间的小坑（真实丢帧会让中间块掉下去）
    span = list(range(i0, i1 + 1))
    if span:
        low = [i for i in span if env[i] < med - DROPOUT_DROP_DB]
        out["dropout_ratio"] = round(len(low) / len(span), 4)
    if len(rec.ts) > 1:
        gaps = np.diff(np.array(rec.ts)) * 1000
        out["max_recv_gap_ms"] = round(float(gaps.max()), 1)
    out["tone_span_sec"] = round(len(span) * step / rate, 3)
    if i1 > i0:
        t_on = rec.ts[min(i0 // per_block, len(rec.ts) - 1)] - BLOCK / rate
        t_off = rec.ts[min(i1 // per_block, len(rec.ts) - 1)]
        out["tone_wall_sec"] = round(t_off - t_on, 3)
    return out


# ---------------------------------------------------------------- 播放
def play_tone(spk, rate: int, seconds: float) -> tuple[float, float, str | None]:
    """播 1 kHz 正弦；返回 (play_start, play_end, err)。"""
    n = int(seconds * rate)
    t = np.arange(n) / rate
    buf = (LEVEL * np.sin(2 * np.pi * FREQ * t)).astype(np.float32)
    err = None
    t0 = t1 = 0.0
    try:
        ch = 1 if (getattr(spk, "channels", 2) or 2) < 2 else 2
        with spk.player(samplerate=rate, channels=ch, blocksize=BLOCK) as player:
            data = buf if ch == 1 else np.repeat(buf[:, None], 2, axis=1)
            t0 = time.perf_counter()
            for i in range(0, len(data) - BLOCK, BLOCK):
                player.play(data[i:i + BLOCK])
            t1 = time.perf_counter()
    except Exception as e:  # noqa: BLE001
        err = f"{type(e).__name__}: {e}"
        t1 = time.perf_counter()
    if t0 == 0.0:
        t0 = t1
    return t0, t1, err


def run_round(spk, recs: list[Rec], rate: int, seconds: float, verbose=True) -> dict:
    for r in recs:
        r.start()
    time.sleep(WARMUP_SEC)
    t0, t1, err = play_tone(spk, rate, seconds)
    if verbose:
        print(f"    播放 {spk.name!r} -> err={err}  ({t1 - t0:.2f}s)", flush=True)
    time.sleep(TAIL_SEC)
    for r in recs:
        r.stop()
    return {"play_err": err, "play_start": t0, "play_end": t1}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--tone-sec", type=float, default=2.0)
    ap.add_argument("--sustain-sec", type=float, default=6.0)
    ap.add_argument("--skip-sustain", action="store_true")
    ap.add_argument("--outdir", default=str(Path(__file__).resolve().parent))
    args = ap.parse_args()
    rate = args.rate
    outdir = Path(args.outdir)

    spks = sc.all_speakers()
    mics = sc.all_microphones(include_loopback=True)
    print(f"rate={rate}  render={len(spks)}  capture={len(mics)}", flush=True)

    matrix: dict[str, dict] = {}
    for si, spk in enumerate(spks):
        print(f"[{si + 1}/{len(spks)}] render: {spk.name}", flush=True)
        recs = [Rec(m, rate) for m in mics]
        info = run_round(spk, recs, rate, args.tone_sec)
        row = {}
        for rec in recs:
            a = analyze(rec, rate, info["play_start"], info["play_end"])
            a.pop("onset_wall", None)
            row[rec.name + (" [loopback]" if rec.loopback else "")] = a
            mark = "OK " if a["detected"] else "   "
            print(f"    {mark} {a['tone_dbfs'] if a['tone_dbfs'] is not None else 'NA':>7} dBFS"
                  f"  lat={a['latency_ms'] if a['latency_ms'] is not None else 'NA':>8}"
                  f"  drop={a['dropout_ratio']}  <- {rec.name}"
                  + ("  [loopback]" if rec.loopback else "")
                  + (f"  ERR={rec.err}" if rec.err else ""), flush=True)
        matrix[spk.name] = row

    # ---------------- 第二相：持续音稳定性 ----------------
    sustain: dict[str, dict] = {}
    if not args.skip_sustain:
        # 候选：第一阶段检出的 (render, capture) 对
        cands = []
        for rname, row in matrix.items():
            spk = next((s for s in spks if s.name == rname), None)
            if spk is None:
                continue
            for cname, cell in row.items():
                if cell["detected"] and cell["tone_dbfs"] is not None and cell["tone_dbfs"] > -40:
                    mic = next((m for m in mics
                                if (m.name + (" [loopback]" if m.isloopback else "")) == cname), None)
                    if mic is not None:
                        cands.append((spk, mic))
        print(f"\n=== 第二相：{len(cands)} 个候选对，持续音 {args.sustain_sec}s ===", flush=True)
        for k, (spk, mic) in enumerate(cands):
            key = f"{spk.name} -> {mic.name}" + (" [loopback]" if mic.isloopback else "")
            print(f"[{k + 1}/{len(cands)}] {key}", flush=True)
            rec = Rec(mic, rate)
            info = run_round(spk, [rec], rate, args.sustain_sec, verbose=False)
            cell = analyze(rec, rate, info["play_start"], info["play_end"])
            cell["play_err"] = info["play_err"]
            cell["tone_sec_requested"] = args.sustain_sec
            sustain[key] = cell
            print(f"    tone={cell['tone_dbfs']} dBFS  lat={cell['latency_ms']} ms"
                  f"  drop={cell['dropout_ratio']}  frames={cell['blocks']}"
                  f"  maxgap={cell['max_recv_gap_ms']} ms  err={cell['error']}", flush=True)

    payload = {"rate": rate, "tone_sec": args.tone_sec, "level_fs": LEVEL,
               "freq": FREQ, "warmup": WARMUP_SEC, "tail": TAIL_SEC,
               "block": BLOCK, "render_count": len(spks), "capture_count": len(mics),
               "render_names": [s.name for s in spks],
               "capture_names": [m.name + (" [loopback]" if m.isloopback else "") for m in mics],
               "matrix": matrix, "sustain": sustain}
    (outdir / "loopback_matrix.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")

    # ---------------- 文本矩阵 ----------------
    caps = payload["capture_names"]
    lines = [f"# 自环矩阵 @ {rate} Hz, 1 kHz {LEVEL} FS, {args.tone_sec}s / 格",
             f"# 单元格 = 1 kHz 分量 dBFS；'-' = 未检出（比本底低 {DETECT_MARGIN_DB} dB 以上）", ""]
    hdr = "render \\ capture".ljust(34) + "".join(f"| C{j:02d} " for j in range(len(caps)))
    lines.append(hdr)
    for rname, row in matrix.items():
        cells = []
        for c in caps:
            v = row.get(c, {}).get("tone_dbfs")
            ok = row.get(c, {}).get("detected")
            cells.append(f"|{' ' if ok else '-'}{v:>6.1f}" if v is not None else "|  n/a ")
        lines.append(rname.ljust(34) + "".join(cells) + "|")
    lines.append("")
    for j, c in enumerate(caps):
        lines.append(f"C{j:02d} = {c}")
    (outdir / "loopback_matrix.txt").write_text("\n".join(lines), encoding="utf-8")

    hits = [(r, c) for r, row in matrix.items() for c, v in row.items() if v["detected"]]
    print(f"\n检出通路 {len(hits)} 条；结果已写 {outdir / 'loopback_matrix.json'}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
