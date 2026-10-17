#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
airplay_ear_test.py -- 人耳判定工具：AirPlay 2 推送到 L16A 时，卧室「立体声组合」到底几只响？

为什么需要它
------------
`research/lan/lan-paths.md` §10 ① 留了一个**只能靠耳朵**回答的问题：

    局域网里**只有 L16A（Xiaomi Sound-左 / 192.168.1.21）广播 AirPlay**。
    那么把音频 AirPlay 推给 L16A 时，是
      (a) 整组一起响（左箱把音频转发给右箱 L16B）—— 那卧室这套在 ~300ms 延迟下就算解决了；还是
      (b) 只有左箱响 —— 那右箱必须另找一条路？

判别方法：**放一段左右声道分离的信号**。

    0.0s - 4.0s   左声道 440 Hz（低音），右声道静音
    5.5s - 9.5s   左声道静音，右声道 880 Hz（高音）
    其余           静音

然后问用户两个问题：
    第一次响（低音）是从**哪一只**出来的？
    第二次响（高音）是从**哪一只**出来的？

    两次都是同一只  -> (b) 单箱，AirPlay 只驱动 L16A
    两次分别不同只  -> (a) 整组，立体声分离正常
    两次都两只一起响 -> 整组，但没做左右分离（单声道下混）

音量说明
--------
`research/lan/probe_airplay2_stream.py` 生成的测试音振幅只有 **0.12 FS（−18.4 dBFS）**，
所以即使用户把 AirPlay 音量拉到 80 仍然「太轻」。本工具默认振幅 **0.70 FS（−3.1 dBFS）**，
并且**先把设备当前音量读出来、播完在 finally 里恢复**。

用法
----
    & "$env:TEMP\\raopvenv\\Scripts\\python.exe" .\\tools\\airplay_ear_test.py --mode lr-split --volume 55

    --mode tone    单纯 440 Hz（用于确认链路通不通）
    --mode lr-split  左右分离判别（默认）
    --wav PATH     直接播一个现成的 WAV，跳过生成
    --ip           默认 192.168.1.21
    --volume       0~100，播完恢复原值
    --amplitude    生成信号的振幅，默认 0.70
"""

import argparse
import asyncio
import json
import math
import os
import struct
import sys
import time
import wave

# 这些 TXT 记录来自 research/lan/mdns_summary.txt（DNS-SD 实捕），
# 因为 pyatv 的单播扫描在本机多网卡环境下扫不到这台音箱。
RAOP_PROPS = {
    "cn": "0,1", "da": "true", "et": "0", "ft": "0xC05F8A00,0x1C340",
    "fv": "1.106.3", "md": "0,1,2", "am": "L16A", "sf": "0x4", "tp": "UDP",
    "vn": "65537", "vs": "366.0",
    "pk": "be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f",
}

AIRPLAY_PROPS = {
    "acl": "0", "deviceid": "5E:02:14:2B:B3:C6",
    "features": "0xC05F8A00,0x1C340", "rsf": "0x4", "fv": "1.106.3",
    "flags": "0x4", "model": "L16A", "manufacturer": "XiaoMi",
    "serialNumber": "34471/A1T321695", "protovers": "1.1", "srcvers": "366.0",
    "pi": "5E:02:14:2B:B3:C6", "gid": "5E:02:14:2B:B3:C6", "gcgl": "0",
    "pk": "be290bd4d4b9be518aaf69881f893cdcfcea477c40f361d17a14179cce25cd6f",
}


def manual_config(ip, name="Xiaomi Sound-1695"):
    """用我们自己的 mDNS 记录直接构造 pyatv 配置。

    credentials 必须留 None：pyatv 会通过 auth.extract_credentials() 自己推导出
    TRANSIENT_CREDENTIALS（因为 features 带 SupportsCoreUtilsPairingAndEncryption, bit 48）。
    传字面量 "transient" 会抛 InvalidCredentialsError。
    """
    import ipaddress
    import pyatv.conf as pconf

    cfg = pconf.AppleTV(ipaddress.IPv4Address(ip), name)
    cfg.add_service(pconf.RaopService(
        identifier="5E02142BB3C6", port=7000,
        credentials=None, properties=RAOP_PROPS))
    cfg.add_service(pconf.AirPlayService(
        identifier="5E:02:14:2B:B3:C6", port=7000,
        credentials=None, properties=AIRPLAY_PROPS))
    return cfg


def write_wav(path, segments, rate=44100, amplitude=0.70):
    """segments: [(duration_s, left_freq_or_None, right_freq_or_None), ...]"""
    fade = int(rate * 0.03)
    frames = bytearray()
    for dur, lf, rf in segments:
        n = int(rate * dur)
        for i in range(n):
            if i < fade:
                env = i / fade
            elif i > n - fade:
                env = max(0.0, (n - i) / fade)
            else:
                env = 1.0
            l = int(32767 * amplitude * env * math.sin(2 * math.pi * lf * i / rate)) if lf else 0
            r = int(32767 * amplitude * env * math.sin(2 * math.pi * rf * i / rate)) if rf else 0
            frames += struct.pack("<hh", l, r)
    with wave.open(path, "wb") as fh:
        fh.setnchannels(2)
        fh.setsampwidth(2)
        fh.setframerate(rate)
        fh.writeframes(bytes(frames))
    return path


def build_signal(mode, path, amplitude):
    if mode == "lr-split":
        # 左低音 -> 静音 -> 右高音 -> 静音
        segs = [(4.0, 440.0, None), (1.5, None, None),
                (4.0, None, 880.0), (1.5, None, None)]
        write_wav(path, segs, amplitude=amplitude)
        return ("左右分离判别信号：0-4s 左声道 440Hz（低音） / 5.5-9.5s 右声道 880Hz（高音）", 11.0)
    if mode == "alt":
        # 同一个 440Hz 音在左右声道之间每 1.2s 跳一次，共 12s。
        # 两只音箱都在响 -> 声音明显左右跳；只有一只响 -> 同一个位置断续响。
        # 频率不变，所以「跳不跳」只能来自声道位置，排除了音高干扰。
        segs = []
        for _ in range(5):
            segs.append((1.2, 440.0, None))
            segs.append((1.2, None, 440.0))
        segs.append((1.0, None, None))
        write_wav(path, segs, amplitude=amplitude)
        return ("左右交替信号：440Hz 每 1.2s 在左/右声道之间跳一次，共 12s", 13.0)
    segs = [(4.0, 440.0, 440.0), (1.0, None, None)]
    write_wav(path, segs, amplitude=amplitude)
    return ("双声道 440Hz 单音（链路确认）", 5.0)


async def run(args, result):
    import pyatv

    loop = asyncio.get_running_loop()
    conf = manual_config(args.ip)
    print("设备: %s  %s:%s" % (conf.name, conf.address, 7000))

    print("\n=== connect (AirPlay 2 / transient pairing) ===")
    atv = await pyatv.connect(conf, loop)
    result["connected"] = True
    print("  connected")

    old_volume = None
    try:
        try:
            old_volume = atv.audio.volume          # 普通属性，不可 await
            result["volume_before"] = old_volume
            print("  current volume  : %s" % old_volume)
        except Exception as exc:                                    # noqa: BLE001
            result["volume_read_error"] = repr(exc)
            print("  volume read failed: %r" % (exc,))
        try:
            await atv.audio.set_volume(args.volume)
            result["volume_set"] = args.volume
            print("  set volume      : %s" % args.volume)
        except Exception as exc:                                    # noqa: BLE001
            result["volume_set_error"] = repr(exc)
            print("  volume set failed: %r" % (exc,))

        print("\n=== stream %s ===" % args.wav)
        t0 = time.monotonic()
        result["stream_start_monotonic"] = t0
        await atv.stream.stream_file(args.wav)
        result["stream_ok"] = True
        result["stream_return_after_s"] = round(time.monotonic() - t0, 3)
        print("  stream_file returned after %.3fs" % result["stream_return_after_s"])
    except Exception as exc:                                        # noqa: BLE001
        result["stream_error"] = repr(exc)
        print("  stream_file FAILED: %r" % (exc,))
    finally:
        if old_volume is not None:
            try:
                await atv.audio.set_volume(old_volume)
                result["volume_restored"] = old_volume
                print("  volume restored to %s" % old_volume)
            except Exception as exc:                                # noqa: BLE001
                result["volume_restore_error"] = repr(exc)
                print("  volume restore FAILED: %r" % (exc,))
        try:
            atv.close()
        except Exception:                                           # noqa: BLE001
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    ap.add_argument("--mode", choices=["lr-split", "alt", "tone"], default="lr-split")
    ap.add_argument("--wav", default=None, help="直接播现成 WAV，跳过生成")
    ap.add_argument("--volume", type=float, default=55.0,
                    help="测试期间的 AirPlay 音量（0~100），播完恢复原值")
    ap.add_argument("--amplitude", type=float, default=0.70,
                    help="生成信号的振幅（FS），默认 0.70 = -3.1 dBFS")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    outdir = os.path.join(os.path.dirname(here), "research", "lan")
    os.makedirs(outdir, exist_ok=True)

    if args.wav:
        args.wav = os.path.abspath(args.wav)
        desc = "现成 WAV"
    else:
        args.wav = os.path.join(outdir, "ear_test_%s.wav" % args.mode)
        desc, _ = build_signal(args.mode, args.wav, args.amplitude)
        print("generated %s  (%s, amplitude=%.2f FS)" % (args.wav, desc, args.amplitude))

    args.out = args.out or os.path.join(outdir, "ear_test_%s.json" % args.mode)
    result = {"ip": args.ip, "wav": args.wav, "mode": args.mode,
              "amplitude": args.amplitude, "volume": args.volume}
    try:
        asyncio.run(run(args, result))
    except Exception as exc:                                        # noqa: BLE001
        result["fatal"] = repr(exc)
        print("FATAL: %r" % (exc,))

    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, ensure_ascii=False, default=str)
    print("\nreport -> %s" % args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
