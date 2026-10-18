#!/usr/bin/env python3
"""生成 StereoLink 的 Windows 图标（installer/stereo-link.ico）。

图形与面板/托盘图标同源：深色圆角底 + 左侧青色声道条 + 右侧紫色声道条 +
中间连接线（"Link"）。几何与配色直接对应 crates/stereo-link/src/main.rs 里
app_icon() 的 64×64 代码绘制版本，这里是它的矢量/位图复刻，用于安装包、
快捷方式和将来的 exe 资源。

用法：
    python scripts/make-icon.py            # 写 installer/stereo-link.ico
    python scripts/make-icon.py --png DIR  # 额外导出各尺寸 PNG 便于目视检查

只依赖 Pillow。
"""

from __future__ import annotations

import argparse
import os
import sys

from PIL import Image, ImageDraw

# 与 app_icon() 一致的 64×64 设计坐标（这里归一化到 0..1）
SQUARE_CENTER = (0.5, 0.5)
SQUARE_HALF = 28.0 / 64.0
SQUARE_RADIUS = 12.0 / 64.0

LEFT_BAR = (16.0 / 64.0, 30.0 / 64.0, 24.0 / 64.0, 46.0 / 64.0)
RIGHT_BAR = (30.0 / 64.0, 20.0 / 64.0, 38.0 / 64.0, 46.0 / 64.0)
LINK_BAR = (24.0 / 64.0, 36.0 / 64.0, 30.0 / 64.0, 39.0 / 64.0)

COLOR_LEFT = (56, 189, 248)      # 青
COLOR_RIGHT = (167, 139, 250)    # 紫
COLOR_LINK = (120, 200, 240)
COLOR_BG_TOP = (30, 34, 43)      # 石墨渐变（上浅下深）
COLOR_BG_BOTTOM = (11, 13, 17)

ICO_SIZES = (256, 128, 64, 48, 32, 24, 16)
MASTER = 1024


def render(size: int) -> Image.Image:
    """渲染单个尺寸的 RGBA 图标（先在 MASTER 上画，再高质量缩放）。"""
    s = MASTER
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))

    # 圆角底：竖直渐变 + 圆角遮罩
    grad = Image.new("RGB", (1, s))
    for y in range(s):
        t = y / (s - 1)
        grad.putpixel(
            (0, y),
            (
                round(COLOR_BG_TOP[0] + (COLOR_BG_BOTTOM[0] - COLOR_BG_TOP[0]) * t),
                round(COLOR_BG_TOP[1] + (COLOR_BG_BOTTOM[1] - COLOR_BG_TOP[1]) * t),
                round(COLOR_BG_TOP[2] + (COLOR_BG_BOTTOM[2] - COLOR_BG_TOP[2]) * t),
            ),
        )
    grad = grad.resize((s, s), Image.NEAREST)

    half = SQUARE_HALF * s
    radius = SQUARE_RADIUS * s
    box = (
        SQUARE_CENTER[0] * s - half,
        SQUARE_CENTER[1] * s - half,
        SQUARE_CENTER[0] * s + half,
        SQUARE_CENTER[1] * s + half,
    )
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, radius=radius, fill=255)
    img.paste(grad, (0, 0), mask)

    draw = ImageDraw.Draw(img)

    def rect(spec, color):
        x0, y0, x1, y1 = spec
        draw.rectangle((x0 * s, y0 * s, x1 * s - 1, y1 * s - 1), fill=color + (255,))

    rect(LEFT_BAR, COLOR_LEFT)
    rect(RIGHT_BAR, COLOR_RIGHT)
    rect(LINK_BAR, COLOR_LINK)

    return img.resize((size, size), Image.LANCZOS)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=None, help="输出的 .ico 路径")
    ap.add_argument("--png", default=None, help="额外导出各尺寸 PNG 的目录")
    args = ap.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = args.out or os.path.join(root, "installer", "stereo-link.ico")
    os.makedirs(os.path.dirname(out), exist_ok=True)

    frames = [render(n) for n in ICO_SIZES]
    # 每一档都独立渲染，交给 ICO 插件按原尺寸写入（append_images 保留各自尺寸）
    frames[0].save(
        out,
        format="ICO",
        append_images=frames[1:],
        sizes=[(n, n) for n in ICO_SIZES],
    )

    if args.png:
        os.makedirs(args.png, exist_ok=True)
        for n, frame in zip(ICO_SIZES, frames):
            frame.save(os.path.join(args.png, f"icon-{n}.png"))

    with Image.open(out) as check:
        got = sorted(check.info.get("sizes", []))
    print(f"已写入 {out}  ({os.path.getsize(out)} B)")
    print(f"内含尺寸: {', '.join(f'{w}x{h}' for w, h in got)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
