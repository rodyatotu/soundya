#!/usr/bin/env python3
"""Pictures for the Mac disk image: the window's background (drag soundya to
Applications, in the app's own look) and the app icon (.icns from docs/logo.png).

    python3 scripts/mac-dmg/make_assets.py <out dir>

Layout in points (the PNG is twice that, 144 dpi, so it is sharp on Retina);
build-dmg.sh puts the icons at APP_AT and APPS_AT.
"""
import io
import os
import struct
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
W, H = 660, 420
APP_AT, APPS_AT = (170, 200), (490, 200)
S = 2  # pixels per point


def font(names, size):
    for n in names:
        for d in ("/usr/share/fonts/opentype/inter", "/usr/share/fonts/truetype/dejavu", "/Library/Fonts", "/System/Library/Fonts"):
            p = os.path.join(d, n)
            if os.path.exists(p):
                return ImageFont.truetype(p, size * S)
    return ImageFont.load_default()


BOLD = ["InterDisplay-ExtraBold.otf", "Inter-ExtraBold.otf", "DejaVuSans-Bold.ttf"]
SEMI = ["Inter-SemiBold.otf", "DejaVuSans-Bold.ttf"]
REG = ["Inter-Medium.otf", "Inter-Regular.otf", "DejaVuSans.ttf"]


def gradient(size, c1, c2, horizontal=True):
    w, h = size
    g = Image.new("RGBA", size)
    px = g.load()
    for x in range(w):
        for y in range(h):
            t = (x / max(w - 1, 1)) if horizontal else (y / max(h - 1, 1))
            px[x, y] = tuple(int(c1[i] + (c2[i] - c1[i]) * t) for i in range(4))
    return g


def background(out):
    w, h = W * S, H * S
    img = Image.new("RGBA", (w, h), (14, 12, 22, 255))
    # Soft colour fields, as on the panel and the installer.
    blobs = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(blobs)
    for (cx, cy, r, col) in [
        (-40, -60, 300, (255, 110, 30, 170)),
        (700, 120, 260, (255, 60, 110, 110)),
        (180, 520, 300, (120, 50, 220, 150)),
        (620, 470, 220, (90, 70, 200, 110)),
    ]:
        d.ellipse([(cx - r) * S, (cy - r) * S, (cx + r) * S, (cy + r) * S], fill=col)
    blobs = blobs.filter(ImageFilter.GaussianBlur(90 * S))
    img = Image.alpha_composite(img, blobs)
    d = ImageDraw.Draw(img)

    # Wordmark: «sound» white, «ya» in the orange→pink gradient.
    f = font(BOLD, 30)
    x0, y0 = 36 * S, 26 * S
    d.text((x0, y0), "sound", font=f, fill=(255, 255, 255, 255))
    wsound = d.textlength("sound", font=f)
    ya_w = int(d.textlength("ya", font=f)) + 4
    ya_h = int(f.size * 1.4)
    mask = Image.new("L", (ya_w, ya_h), 0)
    ImageDraw.Draw(mask).text((0, 0), "ya", font=f, fill=255)
    img.paste(gradient((ya_w, ya_h), (255, 122, 26, 255), (255, 61, 90, 255)), (int(x0 + wsound), y0), mask)
    img = soft_text(img, (W * S - 36 * S, 38 * S), "установка", font(REG, 12), 120, "ra")
    d = ImageDraw.Draw(img)

    # Glass cards under the two icons (labels stay readable in light and dark Finder).
    glass = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glass)
    for (cx, cy) in (APP_AT, APPS_AT):
        box = [(cx - 82) * S, (cy - 78) * S, (cx + 82) * S, (cy + 92) * S]
        gd.rounded_rectangle(box, radius=26 * S, fill=(255, 255, 255, 34), outline=(255, 255, 255, 60), width=S)
    img = Image.alpha_composite(img, glass)
    d = ImageDraw.Draw(img)

    # The arrow, app → Applications.
    ax0, ax1, ay = APP_AT[0] + 98, APPS_AT[0] - 98, APP_AT[1] - 6
    arrow = Image.new("L", (w, h), 0)
    ad = ImageDraw.Draw(arrow)
    ad.rounded_rectangle([ax0 * S, (ay - 3) * S, (ax1 - 14) * S, (ay + 3) * S], radius=3 * S, fill=255)
    ad.polygon([((ax1 - 22) * S, (ay - 13) * S), (ax1 * S, ay * S), ((ax1 - 22) * S, (ay + 13) * S)], fill=255)
    arrow = arrow.filter(ImageFilter.GaussianBlur(0.6 * S))
    img.paste(gradient((w, h), (255, 122, 26, 255), (255, 61, 90, 255)), (0, 0), arrow)
    d = ImageDraw.Draw(img)

    # The words.
    d.text((W * S // 2, 318 * S), "Перетащите soundya в «Программы»", font=font(SEMI, 18), fill=(255, 255, 255, 240), anchor="ma")
    t = "Разреши все необходимые доступы <3"
    f2 = font(SEMI, 14)
    tw = int(d.textlength(t, font=f2)) + 4
    th = int(f2.size * 1.5)
    m = Image.new("L", (tw, th), 0)
    ImageDraw.Draw(m).text((0, 0), t, font=f2, fill=255)
    img.paste(gradient((tw, th), (255, 150, 60, 255), (255, 90, 160, 255)), (W * S // 2 - tw // 2, 348 * S), m)
    img = soft_text(img, (W * S // 2, 384 * S), "Первый запуск: правый клик по soundya → «Открыть». Затем разрешите доступ к локальной сети.",
                    font(REG, 10.5), 140, "ma")
    img.convert("RGB").save(out, dpi=(144, 144))


def soft_text(img, xy, text, f, alpha, anchor):
    """White text at `alpha`, blended (ImageDraw alone would overwrite the pixels' alpha)."""
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).text(xy, text, font=f, fill=(255, 255, 255, alpha), anchor=anchor)
    return Image.alpha_composite(img, layer)


def icns(out):
    """Apple icon file: PNG entries for 16…512 pt at 1× and 2×."""
    src = Image.open(os.path.join(ROOT, "docs", "logo.png")).convert("RGBA")
    # A little margin, as macOS icons have.
    canvas = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    big = src.resize((880, 880), Image.LANCZOS)
    canvas.paste(big, (72, 72), big)
    entries = [("icp4", 16), ("icp5", 32), ("icp6", 64), ("ic07", 128), ("ic08", 256), ("ic09", 512), ("ic10", 1024),
               ("ic11", 32), ("ic12", 64), ("ic13", 256), ("ic14", 512)]
    body = b""
    for kind, px in entries:
        buf = io.BytesIO()
        canvas.resize((px, px), Image.LANCZOS).save(buf, "PNG")
        data = buf.getvalue()
        body += kind.encode() + struct.pack(">I", len(data) + 8) + data
    open(out, "wb").write(b"icns" + struct.pack(">I", len(body) + 8) + body)


if __name__ == "__main__":
    d = sys.argv[1]
    os.makedirs(d, exist_ok=True)
    background(os.path.join(d, "background.png"))
    icns(os.path.join(d, "soundya.icns"))
    print("assets in", d)
