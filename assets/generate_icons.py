#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
应用图标生成脚本(可重复执行)。

输入: assets/source.png —— 原始方形/近方形图标素材
输出:
  assets/icon.png          1024x1024 圆角主图标(README/资产用)
  assets/nebula.ico        多尺寸 Windows 图标(16~256,嵌入 exe)
  assets/avatar.png        512x512 圆角(GitHub 头像/社交用)
  assets/social-preview.png 1280x640 仓库社交预览卡

设计: 圆角蒙版以 4 倍超采样后缩放,边缘平滑无锯齿;
      圆角半径取边长的 22%(接近 iOS/macOS 应用图标视觉比例)。
"""

import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "source.png")

RADIUS_RATIO = 0.22
SSAA = 4  # 超采样倍数


def rounded_square(size: int, radius_ratio: float) -> Image.Image:
    """生成带 Alpha 的圆角方形蒙版(超采样抗锯齿)。"""
    big = size * SSAA
    radius = int(big * radius_ratio)
    mask = Image.new("L", (big, big), 0)
    draw = ImageDraw.Draw(mask)
    draw.rounded_rectangle((0, 0, big - 1, big - 1), radius=radius, fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def center_square(img: Image.Image) -> Image.Image:
    """居中裁剪为正方形。"""
    w, h = img.size
    side = min(w, h)
    left = (w - side) // 2
    top = (h - side) // 2
    return img.crop((left, top, left + side, top + side))


def make_icon(size: int) -> Image.Image:
    base = center_square(Image.open(SRC).convert("RGBA")).resize(
        (size, size), Image.LANCZOS
    )
    mask = rounded_square(size, RADIUS_RATIO)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(base, (0, 0), mask)
    return out


def vertical_gradient(size_w, size_h, top, bottom):
    grad = Image.new("RGB", (1, size_h))
    for y in range(size_h):
        t = y / max(size_h - 1, 1)
        grad.putpixel(
            (0, y),
            tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(3)),
        )
    return grad.resize((size_w, size_h))


def load_font(candidates, size):
    for path in candidates:
        if os.path.exists(path):
            try:
                return ImageFont.truetype(path, size)
            except OSError:
                continue
    return ImageFont.load_default()


def make_social(icon: Image.Image) -> Image.Image:
    W, H = 1280, 640
    bg = vertical_gradient(
        W, H, top=(18, 20, 38), bottom=(42, 26, 74)
    ).convert("RGBA")
    # 装饰性柔光圆
    glow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    gd.ellipse((820, -180, 1500, 500), fill=(120, 90, 255, 40))
    gd.ellipse((-200, 360, 360, 900), fill=(70, 160, 255, 35))
    glow = glow.filter(ImageFilter.GaussianBlur(60))
    bg = Image.alpha_composite(bg, glow)

    # 图标居中偏左
    s = 400
    ic = icon.resize((s, s), Image.LANCZOS)
    bg.alpha_composite(ic, (150, (H - s) // 2))

    draw = ImageDraw.Draw(bg)
    title_font = load_font(
        [
            r"C:\Windows\Fonts\segoeuib.ttf",
            r"C:\Windows\Fonts\arialbd.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ],
        120,
    )
    sub_font = load_font(
        [
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\segoeui.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ],
        34,
    )
    tag_font = load_font(
        [
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
        ],
        26,
    )
    x = 620
    draw.text((x, 205), "Nebula", font=title_font, fill=(255, 255, 255, 255))
    draw.text(
        (x, 350),
        "本地优先的个人记忆检索引擎",
        font=sub_font,
        fill=(210, 214, 235, 255),
    )
    draw.rounded_rectangle(
        (x, 410, x + 360, 462), radius=14, outline=(150, 140, 255, 180), width=2
    )
    draw.text((x + 22, 420), "ES · Engineering Sample", font=tag_font, fill=(190, 185, 250, 255))
    return bg.convert("RGB")


def main():
    if not os.path.exists(SRC):
        sys.exit(f"missing source image: {SRC}")

    icon = make_icon(1024)
    icon.save(os.path.join(HERE, "icon.png"))

    icon.resize((512, 512), Image.LANCZOS).save(os.path.join(HERE, "avatar.png"))

    # Windows 多尺寸 ICO
    ico_sizes = [16, 24, 32, 48, 64, 128, 256]
    ico_images = [icon.resize((s, s), Image.LANCZOS) for s in ico_sizes]
    ico_images[0].save(
        os.path.join(HERE, "nebula.ico"),
        format="ICO",
        sizes=[(s, s) for s in ico_sizes],
        append_images=ico_images[1:],
    )

    make_social(icon).save(os.path.join(HERE, "social-preview.png"))
    print("icons generated:", ", ".join(os.listdir(HERE)))


if __name__ == "__main__":
    main()
