"""播一段已知正弦到**默认扬声器端点**，给 `miplay-bridge stream` 当输入信号。

只依赖标准库（`wave` + `math` + `winsound`），不需要 numpy/soundcard：音频经默认 render
端点播放（本机是「耳机 (Chu2 DSP)」），`stream` 的 WASAPI 回环抓的就是它——所以这条路
能同时验证「回环采集到的是别人的声音」和「AirPlay 链路真的把声音送出去了」。

用法（Windows，任意 Python 3.8+）::

    python play_tone_wav.py [--freq 1000] [--seconds 12] [--amp 0.35] [--rate 44100]
                            [--generate-only] [--keep]

生成的 WAV 落在 `%TEMP%\\miplay-tone-<freq>hz.wav`（**故意不写进仓库**，几 MB 的临时文件）。
`--generate-only` 只生成不播放，方便先确认文件；`--keep` 保留文件（默认也保留，方便复听）。
"""

import argparse
import math
import os
import struct
import sys
import tempfile
import wave

try:
    import winsound
except ImportError:  # pragma: no cover - 只有 Windows 能用
    winsound = None


def build_wav(path: str, freq: float, seconds: float, amp: float, rate: int) -> int:
    """写 16-bit 立体声 WAV，返回帧数。`amp` 是相对满量程的幅度（0.35 = -9 dBFS）。"""
    frames = int(rate * seconds)
    peak = max(-1.0, min(1.0, amp))
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(rate)
        chunk = bytearray()
        for i in range(frames):
            v = int(peak * 32767.0 * math.sin(2.0 * math.pi * freq * i / rate))
            v = max(-32768, min(32767, v))
            chunk += struct.pack("<hh", v, v)  # 两声道同相
            if len(chunk) >= 1 << 16:  # 64 KiB 一批写，快且省内存
                w.writeframes(bytes(chunk))
                chunk.clear()
        if chunk:
            w.writeframes(bytes(chunk))
    return frames


def main() -> int:
    ap = argparse.ArgumentParser(description="播一段正弦到默认扬声器端点")
    ap.add_argument("--freq", type=float, default=1000.0, help="频率 Hz，默认 1000")
    ap.add_argument("--seconds", type=float, default=12.0, help="时长秒，默认 12")
    ap.add_argument("--amp", type=float, default=0.35, help="幅度（满量程比例），默认 0.35")
    ap.add_argument("--rate", type=int, default=44100, help="采样率，默认 44100")
    ap.add_argument("--generate-only", action="store_true", help="只生成 WAV，不播放")
    args = ap.parse_args()

    path = os.path.join(tempfile.gettempdir(), f"miplay-tone-{int(args.freq)}hz.wav")
    frames = build_wav(path, args.freq, args.seconds, args.amp, args.rate)
    print(
        f"已生成 {path}\n  {args.freq:.0f} Hz / {args.seconds:.1f} s / "
        f"amp={args.amp:.2f} / {args.rate} Hz / {frames} 帧 / "
        f"{os.path.getsize(path)} 字节",
        flush=True,
    )
    if args.generate_only:
        return 0
    if winsound is None:
        print("winsound 不可用（非 Windows），只生成不播放", file=sys.stderr)
        return 2
    print("开始播放（阻塞到播完）……", flush=True)
    winsound.PlaySound(path, winsound.SND_FILENAME)  # 同步播放到默认端点
    print("播放完成", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
