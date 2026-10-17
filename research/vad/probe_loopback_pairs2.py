#!/usr/bin/env python3
"""定向复测：phase-1 全量矩阵里「检出的 8 条对」，用 6 s 持续音拿真实延迟/丢帧/最大接收间隔。

为什么单独写：全量矩阵（probe_loopback_matrix.py）第一相的 latency 因传了 play_start=0 而作废，
第二相因本底用 25 分位在长音下假阴性。两个 bug 已在 probe_loopback_matrix.py 修好
（本底改 5 分位、第一相传真实播放窗），全量重跑要十几分钟，而结论只需要这 8 条对。

复用 probe_loopback_matrix 的 Rec / analyze / run_round / 常量，保证判据完全一致。

用法：
  python -u probe_loopback_pairs2.py --tone-sec 6 --rounds 1 --steam-rounds 5
"""
from __future__ import annotations

import argparse
import json
import subprocess
import time
from pathlib import Path

import soundcard as sc

import probe_loopback_matrix as M

# (render 名, capture 名, capture 是否 loopback)  —— 来自 phase-1 全量矩阵的 8 条命中
PAIRS: list[tuple[str, str, bool]] = [
    ("CG345UK (NVIDIA High Definition Audio)", "CG345UK (NVIDIA High Definition Audio)", True),
    ("X41 (NVIDIA High Definition Audio)", "X41 (NVIDIA High Definition Audio)", True),
    ("扬声器 (Steam Streaming Speakers)", "扬声器 (Steam Streaming Speakers)", True),
    ("扬声器 (Steam Streaming Microphone)", "扬声器 (Steam Streaming Microphone)", True),
    ("扬声器 (UU远程虚拟音频设备)", "扬声器 (UU远程虚拟音频设备)", True),
    ("耳机 (Chu2 DSP)", "耳机 (Chu2 DSP)", True),
    ("扬声器 (Realtek High Definition Audio)", "扬声器 (Realtek High Definition Audio)", True),
    # 全矩阵里唯一一条「非自环」数字通路：
    ("扬声器 (Steam Streaming Microphone)", "麦克风 (Steam Streaming Microphone)", False),
]
STEAM_KEY = "Steam Streaming Microphone"


PS_SVC = ("$s=Get-Service -Name 'Steam*' -ErrorAction SilentlyContinue; "
          "if($s){($s | ForEach-Object { $_.Name + '=' + $_.Status }) -join ';'}else{'<no Steam* service>'}")

# 踩坑：SCM 服务名就是带空格的 "Steam Client Service"；查 "SteamClientService" 会 FAILED 1060
#（"The specified service does not exist as an installed service"），第一版探针因此拿到 service=None。
def svc_state() -> dict:
    """当前 Steam 服务/进程状态（用来证明这条虚拟线不依赖 Steam 进程）。"""
    out = {"ts": time.time(), "ts_iso": time.strftime("%Y-%m-%dT%H:%M:%S"),
           "services": None, "steam_procs": None, "services_raw": None}
    try:
        r = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", PS_SVC],
                           capture_output=True, text=True, errors="replace", timeout=30)
        out["services"] = ((r.stdout or "").strip() or (r.stderr or "").strip())[:200]
    except Exception as e:  # noqa: BLE001
        out["services"] = f"Get-Service failed: {e}"
    try:
        r = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command",
                            "@(Get-Process -Name steam,steamwebhelper,steamservice -ErrorAction SilentlyContinue).Count"],
                           capture_output=True, text=True, errors="replace", timeout=30)
        out["steam_procs"] = ((r.stdout or "").strip() or "?")
    except Exception as e:  # noqa: BLE001
        out["steam_procs"] = f"Get-Process failed: {e}"
    out["steamclient"] = out["services"]   # 兼容旧字段名（第一版探针写入的 JSON 用这个 key）
    out["steam_exe"] = f"steam* procs={out['steam_procs']}"
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--tone-sec", type=float, default=6.0)
    ap.add_argument("--rounds", type=int, default=1, help="普通对的轮数")
    ap.add_argument("--steam-rounds", type=int, default=5, help="Steam 相关对的轮数（二次证据）")
    ap.add_argument("--filter", default="", help="只跑 key 里含该子串的对（例如 Steam）")
    ap.add_argument("--outfile", default="loopback_pairs2.json")
    ap.add_argument("--outdir", default=str(Path(__file__).resolve().parent))
    args = ap.parse_args()
    rate, outdir = args.rate, Path(args.outdir)

    spks = {s.name: s for s in sc.all_speakers()}
    mics = {}
    for m in sc.all_microphones(include_loopback=True):
        mics[m.name + (" [loopback]" if m.isloopback else "")] = m

    print(f"rate={rate}  tone={args.tone_sec}s  render={len(spks)}  capture={len(mics)}", flush=True)
    print(f"env: DETECT_MARGIN_DB={M.DETECT_MARGIN_DB} STRONG_MARGIN_DB={M.STRONG_MARGIN_DB} "
          f"DROPOUT_DROP_DB={M.DROPOUT_DROP_DB} BLOCK={M.BLOCK} LEVEL={M.LEVEL} FREQ={M.FREQ}", flush=True)

    results: dict[str, dict] = {}
    svc_before = svc_state()
    print(f"service before: {svc_before['steamclient']} | steam.exe: {svc_before['steam_exe']}", flush=True)

    for rname, cname, is_lb in PAIRS:
        spk = spks.get(rname)
        mic = mics.get(cname + (" [loopback]" if is_lb else ""))
        key = f"{rname} -> {cname}" + (" [loopback]" if is_lb else "")
        if args.filter and args.filter.lower() not in key.lower():
            continue
        if spk is None or mic is None:
            print(f"[SKIP] {key}: spk={spk is not None} mic={mic is not None}", flush=True)
            results[key] = {"error": "endpoint-not-found", "spk": spk is not None, "mic": mic is not None}
            continue
        n = args.steam_rounds if STEAM_KEY in rname else args.rounds
        rounds = []
        for r in range(n):
            svc = svc_state()
            rec = M.Rec(mic, rate)
            info = M.run_round(spk, [rec], rate, args.tone_sec, verbose=False)
            cell = M.analyze(rec, rate, info["play_start"], info["play_end"])
            cell.pop("onset_wall", None)
            cell["round"] = r + 1
            cell["ts_iso"] = time.strftime("%Y-%m-%dT%H:%M:%S")
            cell["play_err"] = info["play_err"]
            cell["tone_sec_requested"] = args.tone_sec
            cell["steamclient"] = svc["steamclient"]
            cell["steam_exe"] = svc["steam_exe"]
            rounds.append(cell)
            print(f"  [{key}] round {r + 1}/{n}: tone={cell['tone_dbfs']} dBFS  lat={cell['latency_ms']} ms  "
                  f"drop={cell['dropout_ratio']}  maxgap={cell['max_recv_gap_ms']} ms  frames={cell['blocks']}  "
                  f"detected={cell['detected']}  | {svc['steamclient']}", flush=True)
        results[key] = {"rounds": rounds, "n_rounds": n, "is_loopback": is_lb,
                        "tone_dbfs_all": [c["tone_dbfs"] for c in rounds],
                        "latency_ms_all": [c["latency_ms"] for c in rounds],
                        "dropout_all": [c["dropout_ratio"] for c in rounds],
                        "detected_all": [c["detected"] for c in rounds]}

    svc_after = svc_state()
    print(f"service after: {svc_after['steamclient']} | steam.exe: {svc_after['steam_exe']}", flush=True)
    payload = {"rate": rate, "tone_sec": args.tone_sec, "rounds": args.rounds,
               "steam_rounds": args.steam_rounds, "filter": args.filter, "service_before": svc_before,
               "service_after": svc_after, "results": results}
    (outdir / args.outfile).write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"\nwrote {outdir / args.outfile}", flush=True)


if __name__ == "__main__":
    main()
