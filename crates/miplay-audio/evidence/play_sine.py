"""跨进程播放测试音（用来证明「回环采集抓到的是别的进程放的声音」）。

用 `soundcard` 的 WASAPI 共享模式向默认扬声器端点播一段已知正弦。

用法（在装有 soundcard 的 Python 里跑）：

    python play_sine.py [freq_hz] [seconds] [amplitude] [samplerate]

默认 `1000Hz 2s amp=0.5 @48000Hz`。

配套的采集端：

    loopback.exe --no-play --seconds 8 --out crates/miplay-audio/evidence/crossprocess_capture.wav

先起采集（它会一直等有程序出声），2.5 秒后再起本脚本，采完看 peak/RMS/主频。
"""

import sys

import numpy as np
import soundcard as sc

freq = float(sys.argv[1]) if len(sys.argv) > 1 else 1000.0
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0
amp = float(sys.argv[3]) if len(sys.argv) > 3 else 0.5
rate = int(sys.argv[4]) if len(sys.argv) > 4 else 48000

speaker = sc.default_speaker()
t = np.arange(int(rate * secs)) / rate
x = (amp * np.sin(2 * np.pi * freq * t)).astype(np.float32)
stereo = np.repeat(x[:, None], 2, axis=1)

print(
    f"播放进程：{speaker.name} | {freq}Hz {secs}s amp={amp} @{rate}Hz",
    flush=True,
)
with speaker.player(samplerate=rate, channels=2, blocksize=2048) as player:
    player.play(stereo)
print("播放完成", flush=True)
