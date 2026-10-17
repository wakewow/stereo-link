#!/usr/bin/env python3
"""按**端点友好名子串**选端点，做 WASAPI loopback 采集，输出 44100Hz/2ch/16bit 裸 PCM。

为什么需要它：
    「零驱动兜底」路线把本机一个已有虚拟端点改名成 `扬声器 (小米妙播设备)`，让用户
    在 Windows 声音列表里看到它。这个脚本负责链路的下半段：从**那个端点**（而不是
    系统默认输出）把声音抓下来，供 push_to_speaker.py 推给小爱音箱。

怎么选端点：
    Windows 上任意 render 端点都能被它的 `[loopback]` 采到（研究结论文档
    `research/vad/loopback-matrix.md`：7/7 个 render 端点的自 loopback 全部有声，
    -12.0 ~ -20.8 dBFS）。所以这里按**名字子串**匹配 capture/loopback 端点：

        python capture_from_endpoint.py --list
        python capture_from_endpoint.py --endpoint-name 小米妙播设备 --seconds 5 --out out.raw

    * 默认只接受 loopback 端点（`[loopback]`），因为它们采的是「送到某个 render
      端点的声音」，正是我们要的；`--allow-real-mic` 才允许选真实麦克风。
    * `--play-tone 1000 --play-to 小米妙播设备` 会在采集的同时**往该 render 端点播放
      一段测试音**——虚拟端点（如 Steam Streaming Speakers）不会从物理喇叭出声，
      因此这是完全安静的端到端自检手段。

输出：
    * `--out FILE`  裸 PCM（s16le, 44100, 2ch），送不进音箱时可离线分析
    * `--pipe`      写到 stdout.buffer（注意别经 PowerShell 管道，二进制会被改）
    * 结束时打印客观数值：peak / RMS dBFS、帧数、时长、采集块数、断流警告数
    * `--stats-json FILE` 把同样数值落盘，作为 PoC 证据

依赖（**必须用采集 venv**）：
    D:\\Backup\\Documents\\deepseek-harness\\default-workspace\\mixplay-pc-audio\\capture\\.venv\\Scripts\\python.exe
    （内含 soundcard 0.4.6 + numpy）
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import threading
import time
import warnings

RATE_DEFAULT = 44100
CHANNELS = 2
SAMPLE_WIDTH = 2
FRAME_BYTES = CHANNELS * SAMPLE_WIDTH
BLOCK_FRAMES = 1024


# --------------------------------------------------------------------------- #
# 端点发现
# --------------------------------------------------------------------------- #
def list_endpoints(substring: str | None = None) -> list[dict]:
    """列出 capture 端点（含 loopback），可选按名字子串过滤。"""
    import soundcard as sc

    rows: list[dict] = []
    for m in sc.all_microphones(include_loopback=True):
        name = str(m.name)
        is_loop = bool(getattr(m, "isloopback", False)) or name.endswith("[loopback]")
        if substring and substring not in name:
            continue
        rows.append({"name": name, "loopback": is_loop, "id": str(getattr(m, "id", ""))})
    return rows


def pick_endpoint(substring: str, allow_real_mic: bool = False):
    """按名字子串挑一个 capture 端点；优先 loopback，其次（可选）真实麦克风。"""
    rows = list_endpoints(substring)
    if not rows:
        raise SystemExit(
            f"[选择失败] 没有名字包含 '{substring}' 的 capture 端点。\n"
            f"  用 --list 看看现有哪些端点；端点没出现在声音列表里时先跑 rename-endpoint.ps1。"
        )
    loops = [r for r in rows if r["loopback"]]
    if loops:
        return loops[0], rows
    if allow_real_mic:
        return rows[0], rows
    raise SystemExit(
        f"[选择失败] 名字包含 '{substring}' 的端点只有真实麦克风，没有 loopback：\n"
        + "\n".join(f"    {r['name']}" for r in rows)
        + "\n  要采真实麦克风请加 --allow-real-mic（声学串音比数字回环低 50+ dB）。"
    )


# --------------------------------------------------------------------------- #
# 采集
# --------------------------------------------------------------------------- #
class Stats:
    def __init__(self) -> None:
        self.blocks = 0
        self.frames = 0
        self.peak = 0.0
        self.sumsq = 0.0
        self.discontinuities = 0

    def feed(self, block_i16) -> None:
        import numpy as np

        self.blocks += 1
        self.frames += int(block_i16.size // CHANNELS)
        a = block_i16.astype(np.float32) / 32768.0
        self.peak = max(self.peak, float(np.max(np.abs(a))) if a.size else 0.0)
        self.sumsq += float(np.sum(a.astype(np.float64) ** 2))

    def as_dict(self, rate: int) -> dict:
        n = self.frames * CHANNELS
        rms = math.sqrt(self.sumsq / n) if n else 0.0
        return {
            "blocks": self.blocks,
            "frames": self.frames,
            "seconds": round(self.frames / rate, 3) if rate else None,
            "peak_dbfs": round(20 * math.log10(self.peak), 2) if self.peak > 0 else None,
            "rms_dbfs": round(20 * math.log10(rms), 2) if rms > 0 else None,
            "discontinuity_warnings": self.discontinuities,
        }


def _tone_player(device_name: str, freq: float, seconds: float, level: float, rate: int,
                 stop: threading.Event, log=print) -> threading.Thread:
    """往指定 render 端点循环播放测试音，直到 stop 被置位（虚拟端点不会物理出声）。"""
    import numpy as np
    import soundcard as sc

    def run() -> None:
        try:
            spk = sc.get_speaker(device_name)
        except Exception as exc:  # noqa: BLE001
            log(f"[测试音] 打不开 render 端点 '{device_name}': {exc!r}")
            return
        log(f"[测试音] {freq:.0f}Hz, {level:.2f} 幅度 -> {spk.name}")
        deadline = time.time() + seconds
        while time.time() < deadline and not stop.is_set():
            n = int(rate * 0.25)
            t = np.arange(n, dtype=np.float64) / rate
            wave = (level * np.sin(2 * math.pi * freq * t)).astype(np.float32)
            stereo = np.repeat(wave.reshape(-1, 1), CHANNELS, axis=1)
            try:
                spk.play(stereo, samplerate=rate)
            except Exception as exc:  # noqa: BLE001
                log(f"[测试音] 播放失败: {exc!r}")
                return

    th = threading.Thread(target=run, daemon=True)
    th.start()
    return th


def capture_pcm(endpoint_name: str, seconds: float, rate: int = RATE_DEFAULT,
                out_path: str | None = None, pipe: bool = False,
                play_tone: float | None = None, play_to: str | None = None,
                level: float = 0.25, gain: float = 1.0,
                allow_real_mic: bool = False, stats_path: str | None = None,
                on_pcm=None, log=print) -> dict:
    """采集 seconds 秒，可选边放测试音；返回 stats dict。

    on_pcm: 可选回调，每块 PCM(s16le bytes) 都会传给它（push_to_speaker.py 用它接环形缓冲）。
    """
    import numpy as np
    import soundcard as sc

    mic, candidates = pick_endpoint(endpoint_name, allow_real_mic=allow_real_mic)
    log(f"[端点] 选中: {mic['name']}  (loopback={mic['loopback']})")
    if len(candidates) > 1:
        log("[端点] 其它同名字候选: " + " | ".join(c["name"] for c in candidates if c["name"] != mic["name"]))

    stop = threading.Event()
    player = None
    if play_tone is not None:
        player = _tone_player(play_to or endpoint_name, play_tone, seconds + 1.0, level, rate, stop, log)
        time.sleep(0.3)   # 让播放先起来，避免开头一段静音被当成结论

    dev = (sc.get_microphone(id=str(mic["id"]), include_loopback=True)
           if mic.get("id") else sc.get_microphone(id=mic["name"], include_loopback=True))

    stats = Stats()
    sink = None
    if out_path:
        sink = open(out_path, "wb")

    # soundcard 在 loopback 采集时会发 SoundcardRuntimeWarning("data discontinuity in
    # recording")。我们**不**把它变成异常（可能打断内部录制线程），只计数：
    # 它是判断这条通路稳不稳的客观指标之一。
    original_showwarning = warnings.showwarning

    def _counting_showwarning(message, category, filename, lineno, file=None, line=None):
        if category.__name__ == "SoundcardRuntimeWarning":
            stats.discontinuities += 1
            return
        original_showwarning(message, category, filename, lineno, file, line)

    warnings.showwarning = _counting_showwarning
    warnings.simplefilter("always")

    log(f"[采集] {rate}Hz/{CHANNELS}ch/s16le, 目标 {seconds}s, 增益 {gain}")
    t0 = time.time()
    try:
        with dev.recorder(samplerate=rate, channels=CHANNELS, blocksize=BLOCK_FRAMES) as rec:
            while True:
                if seconds and (time.time() - t0) >= seconds:
                    break
                try:
                    block = rec.record(numframes=BLOCK_FRAMES)
                except Exception as exc:  # noqa: BLE001
                    if "discontinuity" in repr(exc):
                        stats.discontinuities += 1
                        continue
                    raise
                a = np.asarray(block, dtype=np.float32)
                if a.ndim == 1:
                    a = a.reshape(-1, 1)
                if a.shape[1] == 1:
                    a = np.repeat(a, CHANNELS, axis=1)
                if gain != 1.0:
                    a = np.clip(a * gain, -1.0, 1.0)
                i16 = np.clip(a * 32767.0, -32768, 32767).astype(np.int16)
                stats.feed(i16)
                payload = i16.tobytes()
                if sink:
                    sink.write(payload)
                if on_pcm:
                    on_pcm(payload)
                if pipe:
                    sys.stdout.buffer.write(payload)
    finally:
        stop.set()
        if player:
            player.join(timeout=2.0)
        if sink:
            sink.close()
        if pipe:
            sys.stdout.buffer.flush()
        warnings.showwarning = original_showwarning

    info = stats.as_dict(rate)
    info.update({"endpoint": mic["name"], "loopback": mic["loopback"], "rate": rate,
                 "channels": CHANNELS, "out": out_path, "gain": gain,
                 "tone_hz": play_tone, "tone_to": play_to if play_tone is not None else None})
    log("[结果] " + json.dumps(info, ensure_ascii=False))
    if stats_path:
        with open(stats_path, "w", encoding="utf-8") as fh:
            json.dump(info, fh, ensure_ascii=False, indent=2)
        log(f"[结果] 已写入 {stats_path}")
    return info


# --------------------------------------------------------------------------- #
def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="按端点名做 WASAPI loopback 采集 -> 44100/2ch/s16le")
    ap.add_argument("--list", action="store_true", help="列出 capture 端点（含 loopback）后退出")
    ap.add_argument("--endpoint-name", default="小米妙播设备",
                    help="端点友好名子串（默认 小米妙播设备）")
    ap.add_argument("--seconds", type=float, default=5.0, help="采集秒数（0=一直采到 Ctrl+C）")
    ap.add_argument("--rate", type=int, default=RATE_DEFAULT)
    ap.add_argument("--out", default=None, help="裸 PCM 输出文件（可选）")
    ap.add_argument("--pipe", action="store_true", help="把裸 PCM 写到 stdout.buffer")
    ap.add_argument("--gain", type=float, default=1.0)
    ap.add_argument("--allow-real-mic", action="store_true", help="允许选真实麦克风端点")
    ap.add_argument("--play-tone", type=float, default=None, metavar="HZ",
                    help="采集期间往 render 端点播测试音（虚拟端点不会物理出声）")
    ap.add_argument("--play-to", default=None, help="测试音播到哪个 render 端点（默认同名字）")
    ap.add_argument("--level", type=float, default=0.25, help="测试音幅度（默认 0.25 = -12 dBFS）")
    ap.add_argument("--stats-json", default=None, help="把结果数值写成 JSON")
    args = ap.parse_args(argv)

    if args.list:
        rows = list_endpoints(None)
        for i, r in enumerate(rows):
            print(f"{i:2d}  {'[loopback]' if r['loopback'] else '   mic   '}  {r['name']}")
        print(f"共 {len(rows)} 个 capture 端点")
        return 0

    capture_pcm(args.endpoint_name, args.seconds, rate=args.rate, out_path=args.out,
                pipe=args.pipe, play_tone=args.play_tone, play_to=args.play_to,
                level=args.level, gain=args.gain, allow_real_mic=args.allow_real_mic,
                stats_path=args.stats_json)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
