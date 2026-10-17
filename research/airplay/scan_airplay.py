#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""scan_airplay.py -- 只读：列出局域网上所有 AirPlay 接收端（pyatv 视角）。

用法:
    & "$env:TEMP\\raopvenv\\Scripts\\python.exe" .\\research\\airplay\\scan_airplay.py
    ... --ip 192.168.1.21     # 只看这一台（跳过 mDNS，直连探测）
"""

import argparse
import asyncio
import sys

import pyatv


async def main(ip: str | None) -> int:
    loop = asyncio.get_event_loop()
    kwargs = {"timeout": 8}
    if ip:
        kwargs["hosts"] = [ip]
    confs = await pyatv.scan(loop, **kwargs)
    if not confs:
        print("[x] 一台都没扫到")
        return 1
    for conf in confs:
        print(f"[+] {conf.name}")
        print(f"    address   = {conf.address}")
        print(f"    identifier= {conf.identifier}")
        for svc in conf.services:
            print(f"    service   = {svc.protocol} port={svc.port} creds={svc.credentials}")
    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", default=None)
    a = ap.parse_args()
    sys.exit(asyncio.run(main(a.ip)))
