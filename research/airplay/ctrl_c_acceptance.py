"""验证 `miplay-bridge stream` 被 Ctrl-C 打断时会**先 TEARDOWN 再退出**。

Windows 上「向另一个进程发 Ctrl-C」没有现成 API：标准做法是把子进程放进**新的进程组**
（`CREATE_NEW_PROCESS_GROUP`），再向该组发 `CTRL_BREAK_EVENT`。tokio 的 `ctrl_c()` 在
Windows 上同时处理 `CTRL_C_EVENT` 与 `CTRL_BREAK_EVENT`，所以这条路径与用户按 Ctrl-C
走的是**同一段关闭代码**（打印统计 → `TEARDOWN` → 退出）。

用法::

    python ctrl_c_acceptance.py [--exe PATH] [--target 192.168.1.21] [--after 8]
                               [--out research/airplay/stream_ctrlc.log]

退出码：0 = 子进程优雅退出（说明 TEARDOWN 走完）；2 = 到点还没退（说明关不掉）；1 = 子进程非零退出。
"""

import argparse
import os
import signal
import subprocess
import sys
import time

DEFAULT_EXE = r"D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\target\lan-prober\miplay-bridge.exe"


def main() -> int:
    ap = argparse.ArgumentParser(description="Ctrl-C/Ctrl-Break 优雅收尾验收")
    ap.add_argument("--exe", default=DEFAULT_EXE)
    ap.add_argument("--target", default="192.168.1.21")
    ap.add_argument("--after", type=float, default=8.0, help="启动后等多少秒再打断")
    ap.add_argument("--out", default="stream_ctrlc.log")
    args = ap.parse_args()

    if not os.path.exists(args.exe):
        print(f"找不到 {args.exe}（先 cargo build -p miplay-server）", file=sys.stderr)
        return 1
    cmd = [args.exe, "stream", "--target", args.target, "--stats-interval", "1000"]
    print("$ " + " ".join(cmd), flush=True)

    with open(args.out, "wb") as log:
        proc = subprocess.Popen(
            cmd,
            stdout=log,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
        )
        print(f"已启动 pid={proc.pid}，等 {args.after:.0f} 秒让它把流建起来……", flush=True)
        time.sleep(args.after)
        print("发送 CTRL_BREAK_EVENT（= Ctrl-C 的同一段关闭路径）……", flush=True)
        t0 = time.monotonic()
        os.kill(proc.pid, signal.CTRL_BREAK_EVENT)
        try:
            code = proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            print("!! 20 秒内没退出，强杀（说明关闭路径有问题）", file=sys.stderr)
            proc.kill()
            proc.wait()
            code = 2
        dt = time.monotonic() - t0

    print(f"退出码 {code}，打断到退出 {dt:.2f} s")
    print("--- 子进程输出 ---", flush=True)
    with open(args.out, encoding="utf-8", errors="replace") as fh:
        print(fh.read(), flush=True)
    return 0 if code == 0 else (2 if code == 2 else 1)


if __name__ == "__main__":
    raise SystemExit(main())
