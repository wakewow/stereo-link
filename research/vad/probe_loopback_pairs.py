"""验证「本机已装的虚拟声卡」能否直接当虚拟音频线用。

思路：若某个 render 端点（如 Steam Streaming Speakers / UU远程虚拟音频设备）
与某个 capture 端点（如 Steam Streaming Microphone）自环，
就不需要写内核驱动，直接：系统输出 -> 该 render 端点 -> 从 capture 端点采回来。

用法（必须用 capture 的 venv，里面有 soundcard）：
  D:\\...\\mixplay-pc-audio\\capture\\.venv\\Scripts\\python.exe probe_loopback_pairs.py
"""
import sys
import time
import threading

import numpy as np

try:
    import soundcard as sc
except ImportError:
    sys.exit("需要 soundcard：请用 mixplay-pc-audio/capture/.venv/Scripts/python.exe 运行")

RATE = 44100
FREQ = 1000.0
LEVEL = 0.35


def tone(seconds, rate=RATE):
    t = np.arange(int(seconds * rate)) / rate
    mono = (LEVEL * 32767 * np.sin(2 * np.pi * FREQ * t)).astype(np.int16)
    return np.repeat(mono[:, None], 2, axis=1)  # (n, 2) int16


def rms_fs(block):
    if block.size == 0:
        return 0.0
    return float(np.sqrt(np.mean((block.astype(np.float64) / 32768.0) ** 2)))


def main():
    speakers = sc.all_speakers()
    mics = sc.all_microphones(include_loopback=True)
    print("=== render 端点 ===")
    for s in speakers:
        print(f"  {s.name}")
    print("=== capture 端点 ===")
    for m in mics:
        print(f"  {m.name}{'  [loopback]' if m.isloopback else ''}")

    default_spk = sc.default_speaker()
    default_mic = sc.default_microphone()
    print(f"\n默认输出: {default_spk.name}")
    print(f"默认输入: {default_mic.name if default_mic else None}")

    # 只测「名字看起来是虚拟声卡」的组合，避免真的从物理麦录到环境噪声
    virtual_hints = ("steam", "uu", "virtual", "cable", "vb-audio", "voicemeeter", "nvidia broadcast")
    cands = []
    for s in speakers:
        if any(h in s.name.lower() for h in virtual_hints):
            for m in mics:
                if any(h in m.name.lower() for h in virtual_hints):
                    cands.append((s, m))

    if not cands:
        print("\n没找到候选虚拟声卡对 —— 这条路走不通，需要装虚拟声卡驱动。")
        return 1

    results = []
    for spk, mic in cands:
        print(f"\n--- 试 {spk.name}  ->  {mic.name} ---")
        got = {"peak": 0, "rms": 0.0, "frames": 0, "err": None}

        def recorder():
            try:
                with mic.recorder(samplerate=RATE, channels=2, blocksize=2048) as rec:
                    deadline = time.time() + 3.0
                    while time.time() < deadline:
                        data = rec.record(numframes=2048)
                        if data is None or len(data) == 0:
                            continue
                        pcm = (np.clip(data, -1.0, 1.0) * 32767).astype(np.int16)
                        got["frames"] += len(pcm)
                        got["peak"] = max(got["peak"], int(np.abs(pcm).max()))
                        got["rms"] = max(got["rms"], rms_fs(pcm))
            except Exception as e:  # noqa: BLE001
                got["err"] = f"{type(e).__name__}: {e}"

        th = threading.Thread(target=recorder, daemon=True)
        th.start()
        time.sleep(0.5)  # 先开采集，避免漏掉起播

        try:
            with spk.player(samplerate=RATE, channels=2, blocksize=2048) as player:
                buf = tone(2.0)
                step = 2048
                for i in range(0, len(buf) - step, step):
                    player.play(buf[i:i + step].astype(np.float32) / 32768.0)
                    time.sleep(step / RATE)
        except Exception as e:  # noqa: BLE001
            print(f"  播放失败: {type(e).__name__}: {e}")
            got["err"] = got["err"] or f"play: {e}"
        th.join(timeout=6)

        verdict = "❌ 不通" if got["peak"] < 300 else "✅ 自环可用"
        print(f"  峰值={got['peak']}/32768  最大RMS={got['rms']:.4f} FS  采样帧={got['frames']}"
              f"  {verdict}" + (f"  err={got['err']}" if got["err"] else ""))
        results.append((verdict, spk.name, mic.name, got["peak"]))

    print("\n=== 汇总 ===")
    for v, s, m, pk in results:
        print(f"  {v}  {s} -> {m}  (peak={pk})")
    return 0 if any(r[0].startswith("✅") for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
