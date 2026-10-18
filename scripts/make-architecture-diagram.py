"""生成 StereoLink 信号流示意图（docs/images/architecture.png）。

只用 Pillow + 系统微软雅黑，无外部素材；深色配色与面板一致。
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

W, H = 1440, 620
BG = (11, 13, 17)
CARD = (20, 25, 34)
CARD_SOFT = (26, 32, 43)
BORDER = (42, 51, 65)
TEXT = (230, 237, 243)
MUTED = (148, 163, 184)
ACCENT = (56, 189, 248)
ACCENT2 = (167, 139, 250)
OK = (52, 211, 153)

FONT_REG = r"C:\Windows\Fonts\msyh.ttc"
FONT_BOLD = r"C:\Windows\Fonts\msyhbd.ttc"


def font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_BOLD if bold else FONT_REG, size)


img = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(img)


def card(x, y, w, h, accent=None):
    d.rounded_rectangle(
        [x, y, x + w, y + h],
        radius=18,
        fill=CARD,
        outline=accent or BORDER,
        width=2,
    )


def text(x, y, s, size=20, color=TEXT, bold=False, anchor="la"):
    d.text((x, y), s, font=font(size, bold), fill=color, anchor=anchor)


def arrow(x1, y1, x2, y2, color=MUTED, label=None, label_dy=-16, width=3):
    d.line([x1, y1, x2, y2], fill=color, width=width)
    # 箭头（水平/垂直都能用：按方向画三角）
    import math

    ang = math.atan2(y2 - y1, x2 - x1)
    size = 12
    for side in (0.45, -0.45):
        d.line(
            [
                x2,
                y2,
                x2 - size * math.cos(ang - side),
                y2 - size * math.sin(ang - side),
            ],
            fill=color,
            width=width,
        )
    if label:
        mx, my = (x1 + x2) / 2, (y1 + y2) / 2
        text(mx, my + label_dy, label, 16, color, anchor="mm")


# 标题
text(60, 44, "StereoLink 信号流", 32, TEXT, bold=True)
text(60, 92, "播放器 → 采集源 → WASAPI 回环 → 左右分离 → 两台设备", 18, MUTED)

# 主链路
card(60, 180, 240, 132)
text(180, 222, "播放器", 24, TEXT, bold=True, anchor="mm")
text(180, 258, "浏览器 · 音乐 · 视频 · 游戏", 15, MUTED, anchor="mm")
text(180, 284, "（不用改播放器设置）", 15, MUTED, anchor="mm")

card(350, 180, 270, 132, ACCENT)
text(485, 222, "采集源端点", 24, TEXT, bold=True, anchor="mm")
text(485, 258, "虚拟声卡 / 没接音箱的输出", 15, MUTED, anchor="mm")
text(485, 284, "= Windows 默认输出", 15, ACCENT, anchor="mm")

card(670, 180, 280, 132)
text(810, 222, "路由引擎", 24, TEXT, bold=True, anchor="mm")
text(810, 258, "WASAPI 回环采集", 15, MUTED, anchor="mm")
text(810, 284, "44.1 kHz / 2 ch / 16 bit", 15, MUTED, anchor="mm")

card(1060, 116, 340, 112, ACCENT)
text(1230, 152, "设备 A · 左声道", 24, TEXT, bold=True, anchor="mm")
text(1230, 188, "蓝牙音箱 1（Xiaomi Sound-1695）", 15, MUTED, anchor="mm")

card(1060, 272, 340, 112, ACCENT2)
text(1230, 308, "设备 B · 右声道", 24, TEXT, bold=True, anchor="mm")
text(1230, 344, "蓝牙音箱 2（Xiaomi Sound-5320）", 15, MUTED, anchor="mm")

arrow(300, 246, 350, 246, MUTED, "系统声音")
arrow(620, 246, 670, 246, MUTED, "回环采集")
arrow(950, 236, 1060, 172, ACCENT, "左声道", -18)
arrow(950, 256, 1060, 328, ACCENT2, "右声道", 24)

# 底部说明
d.line([60, 430, 1380, 430], fill=BORDER, width=1)
text(60, 458, "软件侧固定开销 ≈ 80 ms", 19, TEXT, bold=True)
text(60, 490, "20 ms 采集缓冲 + 40 ms 抖动缓冲 + 渲染缓冲 40 ms 的一半", 16, MUTED)

text(560, 458, "蓝牙链路 ≈ 0.2–0.35 s", 19, TEXT, bold=True)
text(560, 490, "取决于音箱固件；软件侧已压到下限", 16, MUTED)

text(1000, 458, "两路互相独立", 19, OK, bold=True)
text(1000, 490, "一台掉线不影响另一台；可各自微调延迟对齐", 16, MUTED)

out = Path(r"E:\AI\MiPlay\docs\images\architecture.png")
out.parent.mkdir(parents=True, exist_ok=True)
img.save(out)
print(f"SAVED {out} ({out.stat().st_size} bytes)")
