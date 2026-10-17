#!/usr/bin/env python3
"""把 PCM（默认来自被改名的虚拟端点回环采集）通过自建 WAV HTTP 服务推给小爱音箱。

链路（零驱动兜底 PoC 的下半段）：

    被改名的虚拟端点 `扬声器 (小米妙播设备)`
        --WASAPI loopback (capture_from_endpoint.py)--> 44100/16bit/2ch 裸 PCM
        --环形缓冲 + 自建 HTTP-->  http://<本机 LAN>:8931/audio.wav
        --控制指令 play_url-->  小爱音箱自己来拉 --> 出声

控制通道有两种后端：
  * `--backend mixplay`（默认，**已实测可用**）：POST
        http://127.0.0.1:8820/api/v1/control/play
        {"action":"play_url","target":"<did>","url":"http://<lan>:8931/audio.wav"}
    需要 MiXPlay 桌面端在跑。MiXPlay 内部就是用小米云的 player_play_url 让音箱拉流，
    等价于「play_by_url」。
  * `--backend mina`（**未验证**）：直接用 miservice 的 `MinaService.play_by_url`。
    本机 capture venv 里**没有** miservice（只有 soundcard/numpy/sounddevice），
    所以这条只在你自己有 miservice + 账号凭据的 python 环境里才可能跑通；
    缺依赖时脚本会明确报错而不是假装成功。

安全约束（来自任务要求）：
  * 默认目标 `--target-did 934947888`（Xiaomi Sound，192.168.1.171）。**不要占用
    501718792（192.168.1.21）**，那是另一条测试线的目标；脚本默认拒绝，除非 --force。
  * 短音 ≤10 秒（默认 8），音量默认不拉满（--volume 20）。

用法：
    # 1) 不看音箱，先自检 HTTP 流（本地拉 4 秒，校验 WAV 头 + 字节数）
    python push_to_speaker.py --local-selftest --seconds 4

    # 2) 用测试音推给音箱（不需要采集，链路最短）
    python push_to_speaker.py --source tone --seconds 5 --volume 20

    # 3) 端到端：采集「小米妙播设备」回环 -> 音箱；--play-tone 让虚拟端点有内容
    python push_to_speaker.py --source capture --endpoint-name 小米妙播设备 \\
        --play-tone 1000 --seconds 8 --volume 20

    # 4) 只停止音箱 / 只看控制通道是否可达
    python push_to_speaker.py --stop
    python push_to_speaker.py --check

依赖：**采集 venv**（soundcard + numpy）：
    D:\\Backup\\Documents\\deepseek-harness\\default-workspace\\mixplay-pc-audio\\capture\\.venv\\Scripts\\python.exe
"""

from __future__ import annotations

import argparse
import json
import math
import socket
import struct
import sys
import threading
import time
import urllib.error
import urllib.request

RATE = 44100
CHANNELS = 2
SAMPLE_WIDTH = 2
FRAME_BYTES = CHANNELS * SAMPLE_WIDTH
DATA_SIZE = 0x7FFFFF00          # 假装 2GB：与 MiXPlay /stream/live.wav 同款无限流做法
RING_SECONDS = 5.0

DEFAULT_DID = "934947888"       # Xiaomi Sound (L16B, 192.168.1.171)
RESERVED_DIDS = {"501718792"}   # Xiaomi Sound-左 (L16A, 192.168.1.21) —— 别的测试线在用


# --------------------------------------------------------------------------- #
def build_wav_header(channels: int = CHANNELS, rate: int = RATE, bits: int = 16,
                     data_size: int = DATA_SIZE) -> bytes:
    block_align = channels * bits // 8
    byte_rate = rate * block_align
    return (b"RIFF" + struct.pack("<I", 36 + data_size) + b"WAVE"
            + b"fmt " + struct.pack("<IHHIIHH", 16, 1, channels, rate, byte_rate,
                                    block_align, bits)
            + b"data" + struct.pack("<I", data_size))


class RingBuffer:
    """单生产者多消费者环形缓冲；每个客户端从"当前时刻"开始读，慢客户端自动跳帧。"""

    def __init__(self, capacity: int) -> None:
        self.cap = capacity
        self.buf = bytearray()
        self.total = 0
        self.cv = threading.Condition()
        self.closed = False
        self.bytes_written = 0

    def write(self, data: bytes) -> None:
        with self.cv:
            self.buf += data
            self.total += len(data)
            self.bytes_written += len(data)
            if len(self.buf) > self.cap:
                del self.buf[: len(self.buf) - self.cap]
            self.cv.notify_all()

    def cursor(self) -> int:
        with self.cv:
            return self.total

    def close(self) -> None:
        with self.cv:
            self.closed = True
            self.cv.notify_all()

    def read_from(self, pos: int, maxbytes: int, timeout: float = 0.5):
        with self.cv:
            if self.closed:
                return b"", pos
            earliest = self.total - len(self.buf)
            if pos < earliest:
                pos = earliest
            if pos >= self.total:
                self.cv.wait(timeout)
                if self.closed:
                    return b"", pos
                earliest = self.total - len(self.buf)
                if pos < earliest:
                    pos = earliest
            if pos >= self.total:
                return b"", pos
            off = pos - (self.total - len(self.buf))
            data = bytes(self.buf[off:off + maxbytes])
            return data, pos + len(data)


# --------------------------------------------------------------------------- #
# HTTP 服务
# --------------------------------------------------------------------------- #
def make_handler(ring: RingBuffer, stop: threading.Event, log=print):
    from http.server import BaseHTTPRequestHandler

    header_bytes = build_wav_header()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"       # 无 chunked，靠关连接表示结束
        server_version = "MiPlayZeroDriver/1.0"

        def log_message(self, fmt, *args):
            pass

        def _serve(self) -> None:
            ip = self.client_address[0]
            log(f"[HTTP] /audio.wav 客户端接入: {ip}")
            sent = 0
            try:
                self.send_response(200)
                self.send_header("Content-Type", "audio/wav")
                self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
                self.send_header("Connection", "close")
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(header_bytes)
                pos = ring.cursor()
                while not stop.is_set():
                    data, pos = ring.read_from(pos, 32768, timeout=0.5)
                    if data:
                        self.wfile.write(data)
                        sent += len(data)
                    elif ring.closed:
                        break
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, OSError):
                pass
            log(f"[HTTP] /audio.wav 结束: {ip} 共 {sent} 字节 ({sent / 1024:.1f} KB)")

        def do_GET(self):                    # noqa: N802
            path = self.path.split("?", 1)[0]
            if path in ("/audio.wav", "/live.wav"):
                self._serve()
            elif path == "/health":
                body = b'{"ok":true,"bytes_written":%d}' % ring.bytes_written
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_error(404)

    return Handler


# --------------------------------------------------------------------------- #
# 音源线程
# --------------------------------------------------------------------------- #
def tone_worker(ring: RingBuffer, stop: threading.Event, seconds: float,
                freq: float, level: float, log=print) -> None:
    import numpy as np

    log(f"[测试音] {freq:.0f}Hz {level:.2f} 幅度，{seconds}s")
    total = int(RATE * seconds)
    sent = 0
    t0 = time.time()
    while sent < total and not stop.is_set():
        n = min(1024, total - sent)
        idx = np.arange(sent, sent + n, dtype=np.float64)
        t = idx / RATE
        env = np.minimum(1.0, np.minimum(t / 0.05, (seconds - t) / 0.05))   # 淡入淡出
        env = np.clip(env, 0.0, 1.0)
        wave = (level * np.sin(2 * math.pi * freq * t) * env * 32767).astype(np.int16)
        ring.write(np.repeat(wave.reshape(-1, 1), CHANNELS, axis=1).tobytes())
        sent += n
        delay = (t0 + sent / RATE) - time.time()
        if delay > 0:
            time.sleep(delay)
    log("[测试音] 结束")


def file_worker(ring: RingBuffer, stop: threading.Event, path: str, log=print) -> None:
    """把 WAV 文件当音源（跳过 44 字节标准头），按 1x 实时速率喂环形缓冲。"""
    with open(path, "rb") as fh:
        raw = fh.read()
    if raw[:4] != b"RIFF":
        raise SystemExit(f"{path} 不是 WAV（缺 RIFF 头）")
    # 找到 data chunk
    pos = 12
    data = None
    while pos + 8 <= len(raw):
        cid = raw[pos:pos + 4]
        csz = struct.unpack("<I", raw[pos + 4:pos + 8])[0]
        if cid == b"data":
            data = raw[pos + 8:pos + 8 + csz]
            break
        pos += 8 + csz + (csz % 2)
    if data is None:
        raise SystemExit(f"{path} 里没有 data chunk")
    log(f"[文件] {path}: {len(data)} 字节 PCM，按 1x 速率推送")
    chunk = 1024 * FRAME_BYTES
    t0 = time.time()
    for off in range(0, len(data), chunk):
        if stop.is_set():
            break
        ring.write(data[off:off + chunk])
        delay = (t0 + (off + chunk) / (RATE * FRAME_BYTES)) - time.time()
        if delay > 0:
            time.sleep(delay)
    log("[文件] 播放结束")


def capture_worker(ring: RingBuffer, stop: threading.Event, endpoint_name: str,
                   seconds: float, play_tone: float | None, level: float,
                   log=print) -> None:
    """用 capture_from_endpoint.py 采集被改名端点的回环，实时喂环形缓冲。"""
    import pathlib
    sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
    import capture_from_endpoint as cfe       # type: ignore

    while not stop.is_set() and time.time() < _capture_deadline[0]:
        try:
            info = cfe.capture_pcm(endpoint_name, seconds=min(1.0, max(0.2, _capture_deadline[0] - time.time())),
                                   rate=RATE, play_tone=play_tone, play_to=endpoint_name,
                                   level=level, on_pcm=ring.write, log=log)
        except SystemExit as exc:
            # pick_endpoint() 用 SystemExit 报"端点不存在"；在线程里它会静默杀掉线程，
            # 所以这里必须捕获并显式报出来，否则音箱只会静静地拉到空流。
            log(f"[采集] 端点选择失败，停止采集: {exc}")
            _capture_error[0] = str(exc)
            return
        except Exception as exc:  # noqa: BLE001
            log(f"[采集] 异常，停止采集: {exc!r}")
            _capture_error[0] = repr(exc)
            return
        if info["frames"] == 0:
            log("[采集] 这次没拿到帧，重试")


_capture_deadline = [float("inf")]
_capture_error: list = [None]


# --------------------------------------------------------------------------- #
# 控制通道
# --------------------------------------------------------------------------- #
def local_ip() -> str:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return s.getsockname()[0]
    finally:
        s.close()


def mixplay_control(base: str, payload: dict, timeout: float = 10.0) -> dict:
    req = urllib.request.Request(f"{base}/api/v1/control/play",
                                 data=json.dumps(payload).encode("utf-8"),
                                 headers={"Content-Type": "application/json"},
                                 method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def send_play_url(args, url: str) -> dict | None:
    if args.backend == "mixplay":
        payload = {"action": "play_url", "target": args.target_did, "url": url}
        try:
            res = mixplay_control(args.mixplay, payload)
            print(f"[下发] MixPlay play_url did={args.target_did} url={url} => {res}")
            return {"backend": "mixplay", "payload": payload, "response": res}
        except Exception as exc:  # noqa: BLE001
            print(f"[下发] MixPlay 失败: {exc!r}")
            print(f"       MiXPlay 桌面端在跑吗？（默认 {args.mixplay}，可用 --check 探活）")
            return {"backend": "mixplay", "payload": payload, "error": repr(exc)}
    # backend == mina
    try:
        from miservice.minaservice import MinaService        # type: ignore
    except Exception as exc:  # noqa: BLE001
        print("[下发] 缺少 miservice：本 venv 只装了 soundcard/numpy/sounddevice。")
        print("       要么用 MiXPlay 后端（--backend mixplay），要么换一个装了 miservice +")
        print(f"       账号凭据的 python 环境。原始错误: {exc!r}")
        return {"backend": "mina", "error": "miservice not installed"}
    device_id = args.mina_device_id or args.target_did
    try:
        svc = MinaService()
        res = svc.play_by_url(device_id, url)
        print(f"[下发] MiNA play_by_url deviceId={device_id} url={url} => {res}")
        return {"backend": "mina", "device_id": device_id, "response": repr(res)}
    except Exception as exc:  # noqa: BLE001
        print(f"[下发] MiNA play_by_url 失败（此路径【未验证】，API 形状需按你的 miservice 版本核对）: {exc!r}")
        return {"backend": "mina", "device_id": device_id, "error": repr(exc)}


def stop_target(args) -> None:
    if args.backend == "mixplay":
        try:
            res = mixplay_control(args.mixplay, {"action": "stop", "target": args.target_did})
            print(f"[停止] MixPlay stop did={args.target_did} => {res}")
        except Exception as exc:  # noqa: BLE001
            print(f"[停止] 失败: {exc!r}")
    else:
        try:
            from miservice.minaservice import MinaService    # type: ignore
            res = MinaService().stop(args.mina_device_id or args.target_did)
            print(f"[停止] MiNA stop => {res}")
        except Exception as exc:  # noqa: BLE001
            print(f"[停止] MiNA 失败: {exc!r}")


def set_volume(args, volume: int) -> None:
    if args.backend != "mixplay":
        print("[音量] 只有 mixplay 后端支持 set_volume")
        return
    try:
        res = mixplay_control(args.mixplay, {"action": "set_volume", "target": args.target_did,
                                             "volume": volume})
        print(f"[音量] did={args.target_did} -> {volume}% => {res}")
    except Exception as exc:  # noqa: BLE001
        print(f"[音量] 失败: {exc!r}")


def check_control(args) -> int:
    print(f"[探活] MiXPlay {args.mixplay} ...")
    try:
        with urllib.request.urlopen(args.mixplay, timeout=5) as resp:
            print(f"        HTTP {resp.status}")
        return 0
    except urllib.error.HTTPError as exc:
        print(f"        HTTP {exc.code}（服务在，但根路径返回错误码也算可达）")
        return 0
    except Exception as exc:  # noqa: BLE001
        print(f"        不可达: {exc!r}")
        print("        -> 先启动 MiXPlay 桌面端（默认 http://127.0.0.1:8820）")
        return 2


def ensure_firewall(port: int) -> None:
    import subprocess
    name = f"MiPlay zero-driver PoC {port}"
    ps = (f"if (-not (Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue)) "
          f"{{ New-NetFirewallRule -DisplayName '{name}' -Direction Inbound -Action Allow "
          f"-Protocol TCP -LocalPort {port} -Profile Any | Out-Null; 'added' }} else {{ 'exists' }}")
    try:
        r = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                           capture_output=True, text=True, timeout=30)
        print(f"[防火墙] TCP {port} 入站: {r.stdout.strip() or r.stderr.strip()}")
    except Exception as exc:  # noqa: BLE001
        print(f"[防火墙] 跳过（{exc!r}）")


# --------------------------------------------------------------------------- #
def local_selftest(ring: RingBuffer, stop: threading.Event, port: int, seconds: float,
                   log=print) -> dict:
    """本地拉一次 /audio.wav，校验 WAV 头与字节数——不碰音箱。"""
    url = f"http://127.0.0.1:{port}/audio.wav"
    log(f"[自检] GET {url} 拉 {seconds}s")
    got = bytearray()
    t0 = time.time()
    # HTTP/1.0 无限流：服务端不会主动关连接，音源结束后流只是"没有新数据"。
    # 所以用短 socket 超时，超时即视为流结束（而不是等满默认 10s 后抛异常）。
    with urllib.request.urlopen(url, timeout=1.5) as resp:
        hdr = resp.read(44)
        while time.time() - t0 < seconds and not stop.is_set():
            try:
                chunk = resp.read1(65536)
            except (TimeoutError, socket.timeout):
                break
            if not chunk:
                break
            got += chunk
    info = {
        "url": url,
        "header_ok": (hdr[:4] == b"RIFF" and hdr[8:12] == b"WAVE" and hdr[36:40] == b"data"),
        "header_hex": hdr.hex(),
        "pcm_bytes": len(got),
        "pcm_seconds": round(len(got) / (RATE * FRAME_BYTES), 3),
        "elapsed": round(time.time() - t0, 3),
    }
    log("[自检] " + json.dumps(info, ensure_ascii=False))
    return info


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="把本机 PCM 经自建 WAV 服务推给小爱音箱（零驱动 PoC）")
    ap.add_argument("--backend", choices=("mixplay", "mina"), default="mixplay")
    ap.add_argument("--mixplay", default="http://127.0.0.1:8820")
    ap.add_argument("--target-did", default=DEFAULT_DID,
                    help=f"目标音箱 did（默认 {DEFAULT_DID} = Xiaomi Sound 192.168.1.171）")
    ap.add_argument("--mina-device-id", default=None, help="--backend mina 时的 MiNA deviceId")
    ap.add_argument("--force", action="store_true", help="允许使用保留 did（501718792 / .21）")
    ap.add_argument("--port", type=int, default=8931)
    ap.add_argument("--source", choices=("capture", "tone", "file"), default="capture")
    ap.add_argument("--endpoint-name", default="小米妙播设备", help="capture 源：端点名子串")
    ap.add_argument("--play-tone", type=float, default=1000.0,
                    help="capture 源：同时往该端点播测试音（虚拟端点不会物理出声）；0=不播")
    ap.add_argument("--level", type=float, default=0.25)
    ap.add_argument("--wav", default=None, help="--source file 时的 WAV 路径")
    ap.add_argument("--freq", type=float, default=1000.0, help="--source tone 的频率")
    ap.add_argument("--gain", type=float, default=1.0)
    ap.add_argument("--seconds", type=float, default=8.0, help="总时长（默认 8s，别超 10s）")
    ap.add_argument("--volume", type=int, default=None, help="下发前设置音量百分比")
    ap.add_argument("--stop", action="store_true", help="只停止目标音箱后退出")
    ap.add_argument("--check", action="store_true", help="只探活控制通道")
    ap.add_argument("--local-selftest", action="store_true", help="只在本地拉流校验，不碰音箱")
    ap.add_argument("--no-firewall", action="store_true")
    ap.add_argument("--evidence", default=None, help="把下发/HTTP 证据写进 JSON")
    args = ap.parse_args(argv)

    if args.target_did in RESERVED_DIDS and not args.force:
        print(f"[拒绝] did {args.target_did} 是保留目标（192.168.1.21，别的测试线在用）。"
              f"确认要用请加 --force。")
        return 2
    if args.check:
        return check_control(args)
    if args.stop:
        stop_target(args)
        return 0
    if args.seconds > 10:
        print(f"[警告] --seconds {args.seconds} 超过 10 秒；任务要求短音 ≤10s。")
        if not args.force:
            args.seconds = 10.0
            print("[警告] 已自动截到 10s（要更长请加 --force）。")

    ring_bytes = int(RATE * RING_SECONDS) * FRAME_BYTES
    ring = RingBuffer(ring_bytes)
    stop = threading.Event()

    ip = local_ip()
    url = f"http://{ip}:{args.port}/audio.wav"
    print(f"本机 LAN: {ip}   服务端口: {args.port}")
    print(f"目标音箱: did={args.target_did}  后端={args.backend}")
    print(f"音箱将拉取: {url}")

    from http.server import ThreadingHTTPServer
    httpd = ThreadingHTTPServer(("0.0.0.0", args.port), make_handler(ring, stop))
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    print(f"[服务] WAV 服务已监听 0.0.0.0:{args.port}")

    evidence: dict = {"at": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "args": vars(args),
                      "lan_ip": ip, "url": url}

    try:
        if args.source == "tone":
            worker = threading.Thread(target=tone_worker,
                                      args=(ring, stop, args.seconds, args.freq, args.level),
                                      daemon=True)
        elif args.source == "file":
            if not args.wav:
                raise SystemExit("--source file 需要 --wav <path>")
            worker = threading.Thread(target=file_worker, args=(ring, stop, args.wav), daemon=True)
        else:
            _capture_deadline[0] = time.time() + args.seconds
            worker = threading.Thread(
                target=capture_worker,
                args=(ring, stop, args.endpoint_name, args.seconds,
                      (args.play_tone or None), args.level), daemon=True)
        worker.start()
        time.sleep(1.0)          # 先垫一点数据，避免音箱拉到空流

        if args.local_selftest:
            evidence["local_selftest"] = local_selftest(ring, stop, args.port, args.seconds)
        else:
            if not args.no_firewall:
                ensure_firewall(args.port)
            if args.volume is not None:
                set_volume(args, args.volume)
            evidence["play_url"] = send_play_url(args, url)

        deadline = time.time() + args.seconds + 1.5
        while time.time() < deadline and not stop.is_set():
            time.sleep(0.2)
    except KeyboardInterrupt:
        print("\n收到 Ctrl+C")
    finally:
        stop.set()
        ring.close()
        time.sleep(0.3)
        if not args.local_selftest:
            stop_target(args)
        httpd.shutdown()
        httpd.server_close()

    evidence["bytes_written"] = ring.bytes_written
    evidence["pcm_seconds_written"] = round(ring.bytes_written / (RATE * FRAME_BYTES), 3)
    if _capture_error[0]:
        evidence["capture_error"] = _capture_error[0]
    print("[汇总] " + json.dumps({k: v for k, v in evidence.items() if k != "args"},
                                 ensure_ascii=False))
    if args.evidence:
        with open(args.evidence, "w", encoding="utf-8") as fh:
            json.dump(evidence, fh, ensure_ascii=False, indent=2)
        print(f"[汇总] 证据已写入 {args.evidence}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
