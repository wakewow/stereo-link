"""在**自己的控制台**里跑 `miplay-bridge stream`，然后发真正的 `CTRL_C_EVENT`（= 用户按 Ctrl-C）。

为什么必须这么绕（Windows 的现实约束）：

* `GenerateConsoleCtrlEvent(CTRL_C_EVENT, pgid)` 对**进程组无效** —— MSDN 明说 CTRL+C
  不能定向到进程组；只能发给「调用者所在控制台」里的所有进程（pgid=0）。
* 所以本脚本先 `FreeConsole()` 脱离父控制台，再 `AllocConsole()` 拿一个**自己的**控制台，
  把 `stream` 起在里面；这样 `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` 只会打到它。
* 脚本自己装一个「忽略 Ctrl-C」的控制台处理函数，避免把自己也打断。

（对照实验：`ctrl_c_acceptance.py` 用的是 `CTRL_BREAK_EVENT`。tokio 的 `ctrl_c()` 只注册
`CTRL_C_EVENT`，所以 Ctrl-Break 会走 Windows 默认处理 → 进程被 0xC000013A 直接干掉，
**不会** TEARDOWN。这条差异本身就是产品要补的洞，见 docs/end-to-end.md。）

输出以文件为权威（新控制台下 stdout 可能失效）。
"""

import argparse
import ctypes
import os
import subprocess
import sys
import time
from ctypes import wintypes

CTRL_C_EVENT = 0
DEFAULT_EXE = r"D:\Backup\Documents\deepseek-harness\default-workspace\miplay-bridge\target\lan-prober\miplay-bridge.exe"

HANDLER = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.DWORD)


@HANDLER
def _ignore_ctrl_c(_event: int) -> bool:
    """本脚本自己的控制台处理函数：收到就吞掉（子进程不受影响）。"""
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description="真实 Ctrl-C（CTRL_C_EVENT）优雅收尾验收")
    ap.add_argument("--exe", default=DEFAULT_EXE)
    ap.add_argument("--target", default="192.168.1.21")
    ap.add_argument("--after", type=float, default=10.0, help="启动后等多少秒再按 Ctrl-C")
    ap.add_argument("--out", default="stream_ctrlc_console.log")
    args = ap.parse_args()

    if not os.path.exists(args.exe):
        print(f"找不到 {args.exe}", file=sys.stderr)
        return 1

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.FreeConsole()  # 脱离父控制台
    if not k32.AllocConsole():  # 拿一个自己的
        print(f"AllocConsole 失败：{ctypes.get_last_error()}", file=sys.stderr)
        return 1
    if not k32.SetConsoleCtrlHandler(_ignore_ctrl_c, True):
        print(f"SetConsoleCtrlHandler 失败：{ctypes.get_last_error()}", file=sys.stderr)
        return 1

    cmd = [args.exe, "stream", "--target", args.target, "--stats-interval", "1000"]
    with open(args.out, "wb") as log:
        proc = subprocess.Popen(
            cmd, stdout=log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL
        )
        print(f"pid={proc.pid} 已在独立控制台里启动，等 {args.after:.0f} 秒……", flush=True)
        time.sleep(args.after)
        ok = k32.GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
        err = ctypes.get_last_error()
        t0 = time.monotonic()
        code = None
        try:
            code = proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        dt = time.monotonic() - t0
    print(f"GenerateConsoleCtrlEvent ok={bool(ok)} err={err}；退出码 {code}；{dt:.2f} s",
          flush=True)
    print("--- 子进程输出 ---", flush=True)
    with open(args.out, encoding="utf-8", errors="replace") as fh:
        print(fh.read(), flush=True)
    return 0 if code == 0 else 2


if __name__ == "__main__":
    raise SystemExit(main())
