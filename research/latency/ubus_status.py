#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""用 `miplay-bridge ubus` 查询音箱播放状态（延迟报告里「音箱内部进度」的唯一可查数据源）。

为什么要用 Python 起子进程：PowerShell 5.1 向原生 exe 传参时会把 JSON 里的双引号吃掉，
`miplay-bridge ubus ... '{"media":"app_ios"}'` 会报 “message 不是合法 JSON”。
Python 的 subprocess(argv 列表) 不经过 shell，引号原样保留。

只读：`MiToken::load_default()` 只读 MiXPlay 的凭据文件，不做刷新、不写任何文件。

用法：
    python ubus_status.py --once
    python ubus_status.py --seconds 40 --interval 5 --out speaker_status.csv
"""
from __future__ import annotations

import argparse
import csv
import json
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_EXE = (
    Path(r"D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge")
    / "target" / "debug" / "miplay-bridge.exe"
)
DIDS = {"left": "501718792", "right": "934947888"}
METHOD_ARGS = ("mediaplayer", "player_get_play_status")
MESSAGE = {"media": "app_ios"}

# 关注的状态字段（data.info 解析后）
INTERESTING = (
    "status", "play_status", "state", "position", "progress", "current_position",
    "currentPosition", "duration", "media_type", "play_type", "volume", "loop_type",
)


def query(exe: Path, did: str, timeout: float = 20.0) -> dict:
    """调一次 ubus，返回 {ok, t, raw, info, err, dt}。"""
    argv = [str(exe), "ubus", did, METHOD_ARGS[0], METHOD_ARGS[1],
            json.dumps(MESSAGE, separators=(",", ":"))]
    t0 = time.time()
    try:
        p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8",
                           errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"ok": False, "t": t0, "dt": time.time() - t0, "err": "timeout"}
    dt = time.time() - t0
    if p.returncode != 0:
        return {"ok": False, "t": t0, "dt": dt,
                "err": (p.stderr or p.stdout or "").strip()[:400]}
    text = p.stdout or ""
    # stdout 形如: "设备: xxx (uuid)\n{ ... pretty json ... }"
    brace = text.find("{")
    info = None
    raw = None
    if brace >= 0:
        try:
            raw = json.loads(text[brace:])
        except json.JSONDecodeError as exc:
            return {"ok": False, "t": t0, "dt": dt,
                    "err": f"stdout 不是 JSON: {exc}: {text[:300]}"}
    if isinstance(raw, dict):
        data = raw.get("data") or {}
        if isinstance(data, dict) and isinstance(data.get("info"), str):
            try:
                info = json.loads(data["info"])
            except json.JSONDecodeError:
                info = None
        if info is None and isinstance(data, dict):
            info = data
    return {"ok": True, "t": t0, "dt": dt, "raw": raw, "info": info,
            "code": (raw or {}).get("code")}


def flatten(info) -> dict:
    out = {}
    if isinstance(info, dict):
        for k, v in info.items():
            if isinstance(v, (str, int, float, bool)) or v is None:
                if k in INTERESTING or k.lower() in {s.lower() for s in INTERESTING}:
                    out[k] = v
        # 有些机型把状态塞在子对象里
        for k in ("player", "media", "play_info", "data"):
            sub = info.get(k)
            if isinstance(sub, dict):
                for kk, vv in sub.items():
                    if isinstance(vv, (str, int, float, bool)):
                        out[f"{k}.{kk}"] = vv
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="查询音箱播放状态（只读）")
    ap.add_argument("--exe", default=str(DEFAULT_EXE))
    ap.add_argument("--did", action="append", default=None,
                    help="设备 did，可重复；默认左右两只")
    ap.add_argument("--once", action="store_true")
    ap.add_argument("--seconds", type=float, default=0.0)
    ap.add_argument("--interval", type=float, default=5.0)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    exe = Path(args.exe)
    if not exe.is_file():
        print(f"[错误] 找不到 {exe}", file=sys.stderr)
        return 2
    dids = args.did or list(DIDS.values())
    names = {v: k for k, v in DIDS.items()}

    rows = []
    t_end = time.time() + args.seconds
    while True:
        for did in dids:
            r = query(exe, did)
            tag = names.get(did, did)
            flat = flatten(r.get("info")) if r.get("ok") else {}
            row = {"t": round(r["t"], 3), "did": did, "name": tag,
                   "ok": int(bool(r.get("ok"))), "dt": round(r["dt"], 3),
                   "code": r.get("code", ""), "err": r.get("err", "")}
            row.update({f"f_{k}": v for k, v in flat.items()})
            rows.append(row)
            if r.get("ok"):
                print(f"[{time.strftime('%H:%M:%S')}] {tag} ({did}) code={row['code']} "
                      f"dt={row['dt']:.2f}s {json.dumps(flat, ensure_ascii=False)}")
            else:
                print(f"[{time.strftime('%H:%M:%S')}] {tag} ({did}) 失败: {r.get('err')}")
            if args.once:
                print(json.dumps(r.get("raw"), ensure_ascii=False, indent=2)[:4000])
        if args.once or time.time() >= t_end:
            break
        time.sleep(max(0.0, args.interval))
    if args.seconds > 0 and args.out:
        out = Path(args.out)
        if not out.is_absolute():
            out = HERE / out
        keys = sorted({k for r in rows for k in r})
        with out.open("w", newline="", encoding="utf-8") as fh:
            w = csv.DictWriter(fh, fieldnames=keys)
            w.writeheader()
            w.writerows(rows)
        print(f"[输出] {out} ({len(rows)} 行)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
