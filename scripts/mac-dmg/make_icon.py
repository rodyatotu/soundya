#!/usr/bin/env python3
"""The soundya app icon, minimal: a rounded square in the brand's orange → pink
gradient, a white speaker puck seen from the front, and the Lite's pixel «× ×»
eyes in the gradient's colours. Writes docs/icon.png (1024×1024, transparent
corners); make_assets.py builds the .icns from it, the panel uses small copies.

    python3 scripts/mac-dmg/make_icon.py [out.png]
"""
import os
import sys

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
N = 1024
K = 4  # drawn 4× larger, then scaled down: smooth edges
TOP, BOTTOM = (255, 122, 26), (255, 61, 120)  # the wordmark's «ya»: orange → pink


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def diag(size, c1, c2):
    """c1 at the top-left corner → c2 at the bottom-right, linear along x + y."""
    w, h = size
    corners = Image.new("L", (2, 2))
    corners.putdata([0, 128, 128, 255])
    mask = corners.resize((w, h), Image.BILINEAR, box=(0.5, 0.5, 1.5, 1.5))
    return Image.composite(Image.new("RGB", size, c2), Image.new("RGB", size, c1), mask)


def icon():
    s = N * K
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    # The plate: macOS-like margin (100 of 1024) and corner radius.
    m, r = 100 * K, 185 * K
    plate = Image.new("L", (s, s), 0)
    ImageDraw.Draw(plate).rounded_rectangle([m, m, s - m, s - m], radius=r, fill=255)
    # A soft shadow under the plate.
    shadow = plate.filter(ImageFilter.GaussianBlur(18 * K)).point(lambda v: v * 0.35)
    img.paste((0, 0, 0, 255), (0, 14 * K), shadow)
    img.paste(diag((s, s), TOP, BOTTOM), (0, 0), plate)
    d = ImageDraw.Draw(img)
    # The puck: a wide white rounded body.
    pw, ph = 600 * K, 400 * K
    x0, y0 = (s - pw) // 2, (s - ph) // 2 + 10 * K
    d.rounded_rectangle([x0, y0, x0 + pw, y0 + ph], radius=150 * K, fill=(255, 255, 255, 255))
    # The grille: a row of small dots near the bottom, the plate's colour, faint.
    dots = Image.new("L", (s, s), 0)
    dd = ImageDraw.Draw(dots)
    dr, step, n = 11 * K, 44 * K, 9
    dy = y0 + ph - 92 * K
    for i in range(n):
        cx = s // 2 + (i - (n - 1) / 2) * step
        dd.ellipse([cx - dr, dy - dr, cx + dr, dy + dr], fill=255)
    img.paste(diag((s, s), TOP, BOTTOM).convert("RGBA"), (0, 0), dots.point(lambda v: v * 0.45))
    # The eyes: two pixel «×», 5×5 grid, gradient-coloured.
    cell = 34 * K
    gap = 2 * K
    eyes = Image.new("L", (s, s), 0)
    ed = ImageDraw.Draw(eyes)
    ey = y0 + 82 * K
    for cx in (s // 2 - 130 * K, s // 2 + 130 * K):
        left = cx - 5 * cell // 2
        for i in range(5):
            for j in (i, 4 - i):
                ed.rounded_rectangle([left + j * cell + gap, ey + i * cell + gap, left + (j + 1) * cell - gap, ey + (i + 1) * cell - gap],
                                     radius=7 * K, fill=255)
    img.paste(diag((s, s), TOP, BOTTOM).convert("RGBA"), (0, 0), eyes)
    return img.resize((N, N), Image.LANCZOS)


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "docs", "icon.png")
    icon().save(out)
    print("icon:", out)
