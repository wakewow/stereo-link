#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""read_device_volume.py -- 只读：用 pyatv 读出 AirPlay 接收端自己报的音量/状态。

为什么需要它
------------
我们的 Rust 发送端在 GET /info 里拿到的是 `initial_volume=None`（设备不上报），
所以「音箱 ACK 了一切但不出声」到底是**设备侧音量被压到 0**还是**我们的流有问题**，
单靠我们的代码看不出来。pyatv 走的是已验证可出声的那条实现，能直接问设备要音量。

用法:
    & "$env:TEMP\\raopvenv\\Scripts\\python.exe" .\\research\\airplay\\read_device_volume.py --ip 192.168.1.21
"""

import argparse
import asyncio
import sys

import pyatv


async def read(ip: str) -> int:
    loop = asyncio.get_event_loop()
    # 注意：pyatv 的 `hosts=` 过滤走的是 mDNS 结果过滤，对这台设备会漏掉；
    # 广播扫描能稳定看到 `Xiaomi Sound-1695 @ 192.168.1.21`，所以按地址挑。
    confs = await pyatv.scan(loop, timeout=8)
    confs = [c for c in confs if str(c.address) == ip]
    if not confs:
        print(f"[x] 没扫到 {ip} 的 AirPlay 服务")
        return 2
    conf = confs[0]
    print(f"[i] 扫到: {conf.name} @ {conf.address} services={[str(s.protocol) for s in conf.services]}")
    atv = await pyatv.connect(conf, loop)
    try:
        vol = atv.audio.volume
        if hasattr(vol, "__await__"):
            vol = await vol
        print(f"[=] 设备上报音量 volume = {vol}  (0~100)")
        for name, acc in (
            ("audio", atv.audio),
            ("power", getattr(atv, "power", None)),
        ):
            if acc is None:
                continue
            try:
                ps = acc.power_state
                if hasattr(ps, "__await__"):
                    ps = await ps
                print(f"[=] {name}.power_state = {ps}")
            except Exception as exc:  # noqa: BLE001 - 只读诊断，任何异常都只打印
                print(f"[!] {name}.power_state 读不到: {exc}")
    finally:
        atv.close()
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default="192.168.1.21")
    args = ap.parse_args()
    return asyncio.run(read(args.ip))


if __name__ == "__main__":
    sys.exit(main())
