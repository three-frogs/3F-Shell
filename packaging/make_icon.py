# -*- coding: utf-8 -*-
"""按内容包围盒裁剪空白并生成多尺寸 app.ico（PNG 内嵌，Vista+ 兼容）。"""
import struct
import sys
from PIL import Image

SRC = r"C:\Users\Administrator\WorkBuddy\2026-10-07-00-11-52\brand-icon-ssh-badge.png"
OUT = r"E:\dev\work\sky-workspace\sky-remote-assistance\packaging\app.ico"
SIZES = [16, 32, 48, 64, 128, 256]
THRESH = 24  # 与纯白(255) 的差值阈值


def trim_blank(img: Image.Image) -> Image.Image:
    """按内容包围盒裁剪四周空白：非白/非透明像素，居中裁成正方形，保留 2% 边距。"""
    rgba = img.convert("RGBA")
    w, h = rgba.size
    px = rgba.load()
    min_x, min_y, max_x, max_y = w, h, -1, -1
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            # 空白 = 完全透明 或 接近纯白；其余视为内容
            if a < 8:
                continue
            if (
                a >= 200
                and abs(r - 255) <= THRESH
                and abs(g - 255) <= THRESH
                and abs(b - 255) <= THRESH
            ):
                continue
            if x < min_x:
                min_x = x
            if x > max_x:
                max_x = x
            if y < min_y:
                min_y = y
            if y > max_y:
                max_y = y
    if max_x < 0:
        return img  # 全空白
    pad = max(w, h) * 0.02
    min_x = max(0, min_x - pad)
    min_y = max(0, min_y - pad)
    max_x = min(w - 1, max_x + pad)
    max_y = min(h - 1, max_y + pad)
    cw = max_x - min_x + 1
    ch = max_y - min_y + 1
    side = int(max(cw, ch))
    cx = int((min_x + max_x) / 2)
    cy = int((min_y + max_y) / 2)
    sx0 = max(0, cx - side // 2)
    sy0 = max(0, cy - side // 2)
    sx0 = min(sx0, w - side)
    sy0 = min(sy0, h - side)
    box = (sx0, sy0, sx0 + side, sy0 + side)
    return rgba.crop(box)


def make_ico(src: str, out: str) -> None:
    img = Image.open(src)
    trimmed = trim_blank(img)
    print("trimmed_size=", trimmed.size, flush=True)
    import io

    entries = []
    for s in SIZES:
        im = trimmed.resize((s, s), Image.LANCZOS)
        if im.mode != "RGBA":
            im = im.convert("RGBA")
        buf = io.BytesIO()
        im.save(buf, format="PNG")  # ICO 内嵌块必须是 PNG 编码
        entries.append((s, buf.getvalue()))
    # ICO 容器
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    out_bytes = bytearray(header)
    for s, buf in entries:
        w_byte = 0 if s >= 256 else s
        h_byte = 0 if s >= 256 else s
        entry = struct.pack(
            "<BBBBHHII", w_byte, h_byte, 0, 0, 1, 32, len(buf), offset
        )
        out_bytes += entry
        offset += len(buf)
    for _, buf in entries:
        out_bytes += buf
    with open(out, "wb") as f:
        f.write(out_bytes)
    print("ico_bytes=", len(out_bytes), flush=True)


if __name__ == "__main__":
    make_ico(SRC, OUT)
    sys.exit(0)
