#!/usr/bin/env python
"""把一段测试音播放到**指定的输出端点**（按名字子串匹配），用于验证虚拟串流声卡链路。

为什么需要它：hub 抓的是虚拟声卡端点「扬声器 (小米妙播设备)」的回环。要验证
「播放器 -> 虚拟声卡 -> hub -> 音箱」这条链，就必须把声音送进那个端点，
而 winsound / [console]::beep 只能送到系统默认设备。

用法:
    python play_to_endpoint.py [名字子串] [秒数] [频率]

默认: 小米妙播设备 / 8 秒 / 440 Hz
左右声道交替（每秒换一边），方便人耳判断左右分离是否正确。
"""
import math
import sys

import numpy as np
import soundcard as sc


def main() -> int:
    needle = sys.argv[1] if len(sys.argv) > 1 else "小米妙播设备"
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0
    freq = float(sys.argv[3]) if len(sys.argv) > 3 else 440.0

    speakers = sc.all_speakers()
    target = next((s for s in speakers if needle in s.name), None)
    if target is None:
        print(f"[X] 找不到名字含 {needle!r} 的输出端点。现有端点:", flush=True)
        for s in speakers:
            print(f"    - {s.name}", flush=True)
        return 1

    print(f"[i] 播放到: {target.name}", flush=True)

    rate = 48000
    t = np.arange(int(rate * secs)) / rate
    # 每秒一次平滑包络，避免爆音；振幅 0.7 FS（0.12 FS 已三次验证听不清）
    env = np.sin(np.pi * (t % 1.0))
    on_left = (np.floor(t) % 2.0) < 1.0
    tone = 0.70 * env * np.sin(2.0 * math.pi * freq * t)
    stereo = np.stack([tone * on_left, tone * (~on_left)], axis=1).astype(np.float32)

    with target.player(samplerate=rate, channels=2) as player:
        player.play(stereo)

    print(f"[✓] 播放完毕 {secs:.1f}s @ {freq:.0f} Hz", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
