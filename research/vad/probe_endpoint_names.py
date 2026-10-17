#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""列出 MMDevice（render / capture）端点名字 —— 每次都是**新进程**，用于绕过进程内缓存。

端点名字来自 MMDevice 的 PKEY_Device_FriendlyName：
  soundcard 通过 IMMDeviceEnumerator + IPropertyStore 读取，等价于
  Windows 声音设置 / 音量合成器 用来显示的名字（不含 UI 端自己加的修饰）。

用法（必须用 capture 的 venv）
  ...\\capture\\.venv\\Scripts\\python.exe -u probe_endpoint_names.py
输出：JSON 一行，{"speakers":[...],"microphones":[...]}
"""
from __future__ import annotations

import json
import sys

import soundcard as sc

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001
    pass


def main() -> int:
    speakers = [s.name for s in sc.all_speakers()]
    mics = [m.name for m in sc.all_microphones(include_loopback=True)]
    print(json.dumps({"speakers": speakers, "microphones": mics},
                     ensure_ascii=False), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
