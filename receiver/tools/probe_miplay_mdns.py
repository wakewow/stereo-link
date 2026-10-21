"""probe_miplay_mdns.py —— 自检：本机的「小米妙播」接收端是否真的在 mDNS 上应答。

用途：不用手机就能确认 miplay-recv 的发现服务是否正常。

用法：
  1) 先启动接收端（必须带 --diag-loopback，否则本机发出的 mDNS 探测收不到自己）：
         .\\target\\release\\miplay-recv.exe --diag-loopback
  2) 另开一个窗口运行：
         python tools\\probe_miplay_mdns.py
     看到 mi-connect 那行出现 "找到本机应答=True" 即说明手机端能发现这台电脑。

说明：--diag-loopback 只是打开 IP_MULTICAST_LOOP，方便同机自测；正式使用时不要加。
"""

import socket
import sys
import time

GROUP = "224.0.0.251"
PORT = 5353
LOCAL = "192.168.1.4"  # 改成你本机的局域网地址，或由命令行参数传入

QUERIES = {
    "lyra（设备能力）": bytes.fromhex(
        "0000000000010000000000000a5f6c7972612d6d646e73045f756470056c6f63616c00000c8001"
    ),
    "mi-connect（投放列表）": bytes.fromhex(
        "0000000000010000000000000b5f6d692d636f6e6e656374045f756470056c6f63616c00000c8001"
    ),
}


def main() -> int:
    global LOCAL
    if len(sys.argv) > 1:
        LOCAL = sys.argv[1]
    if len(sys.argv) > 2:
        expected = sys.argv[2]
    else:
        expected = "StereoLink"

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((LOCAL, 0))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(LOCAL))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 255)
    sock.settimeout(1.5)

    print(f"本机地址 {LOCAL}，从端口 {sock.getsockname()[1]} 发 mDNS 查询（期待名字：{expected}）")
    ok = True
    for label, query in QUERIES.items():
        print(f"=== {label}")
        sock.sendto(query, (GROUP, PORT))
        deadline = time.time() + 1.5
        replied_self = False
        name_hit = False
        while time.time() < deadline:
            try:
                data, addr = sock.recvfrom(4096)
            except socket.timeout:
                break
            own = addr[0] == LOCAL
            hit = expected.encode() in data
            replied_self = replied_self or own
            name_hit = name_hit or (own and hit)
            print(f"  来自 {addr[0]} 的 {len(data)}B 应答，来自本机={own}，包含「{expected}」={hit}")
        # lyra 记录里只有实例 ID、没有显示名，所以「本机有应答」就算通过；
        # mi-connect 是手机投送列表看到的那条，必须能读到显示名。
        good = replied_self and (name_hit or "lyra" in label)
        if not good:
            ok = False
            print("  ✗ 这一项没通过：确认 miplay-recv 正在运行，且启动时带了 --diag-loopback")
    sock.close()
    print("结论：" + ("✓ 本机妙播应答正常，手机端应该能看到设备" if ok else "✗ 本机应答缺失，见上面的提示"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
