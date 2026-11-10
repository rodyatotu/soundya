#!/usr/bin/env python3
"""The README's pictures, in its look (dark violet, neon gradient wordmark, glass cards,
a grid floor), from the panel's screenshots on demo data (scripts/screens/):

    docs/banner.jpg           1774×887  the README top
    docs/banner-whatsnew.jpg  1600×533  «Что нового»
    docs/showcase.jpg         1600×900  four phones with the panel
    docs/banner-web.jpg       1600×500  «Пульт в телефоне»
    docs/banner-mac.jpg       1600×700  soundya in the Mac menu bar

    python3 scripts/screens/demo_server.py 8899 & python3 scripts/screens/shoot.py
    python3 scripts/banners/make_banners.py

Needs Pillow and SF (a Mac) or Inter / DejaVu fonts.
"""
import math
import os

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BG_TOP, BG_BOTTOM = (22, 10, 46), (12, 6, 26)
NEON = [(255, 122, 26), (255, 61, 120), (196, 72, 255), (86, 214, 255)]  # the wordmark's gradient
PINK = (255, 150, 200)


def font(size, weight="Heavy", italic=False):
    cands = [("/System/Library/Fonts/SFNSItalic.ttf", weight + " Italic")] if italic else []
    cands += [("/System/Library/Fonts/SFNS.ttf", weight), ("/usr/share/fonts/opentype/inter/Inter-ExtraBold.otf", None),
              ("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", None)]
    for path, var in cands:
        if os.path.exists(path):
            f = ImageFont.truetype(path, size)
            if var:
                try:
                    f.set_variation_by_name(var)
                except Exception:
                    pass
            return f
    raise SystemExit("no font")


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def multi(colors, t):
    t = min(max(t, 0), 1) * (len(colors) - 1)
    i = min(int(t), len(colors) - 2)
    return lerp(colors[i], colors[i + 1], t - i)


def hgrad(w, h, colors):
    row = Image.new("RGB", (w, 1))
    for x in range(w):
        row.putpixel((x, 0), multi(colors, x / max(w - 1, 1)))
    return row.resize((w, h))


def background(w, h):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        d.line([(0, y), (w, y)], fill=lerp(BG_TOP, BG_BOTTOM, y / h))
    # Soft colour fields.
    glow = Image.new("RGB", (w, h), (0, 0, 0))
    g = ImageDraw.Draw(glow)
    for cx, cy, r, col in [(0.02, 0.05, 0.45, (120, 30, 40)), (0.78, 0.45, 0.42, (90, 30, 200)),
                           (0.5, 1.05, 0.35, (20, 60, 140)), (0.98, 0.95, 0.3, (140, 20, 110))]:
        R = r * w
        g.ellipse([cx * w - R, cy * h - R, cx * w + R, cy * h + R], fill=col)
    glow = glow.filter(ImageFilter.GaussianBlur(w * 0.12))
    img = Image.blend(img, Image.composite(glow, img, Image.new("L", (w, h), 255)), 0.55)
    # The grid floor.
    floor = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    f = ImageDraw.Draw(floor)
    horizon = h * 0.86
    for i in range(-24, 25):
        x0 = w / 2 + i * w * 0.012
        x1 = w / 2 + i * w * 0.09
        f.line([(x0, horizon), (x1, h)], fill=(120, 110, 255, 38), width=1)
    for k in range(1, 9):
        y = horizon + (h - horizon) * (k / 8) ** 1.8
        f.line([(0, y), (w, y)], fill=(120, 110, 255, 34), width=1)
    img = img.convert("RGBA")
    img.alpha_composite(floor)
    return img


def neon_text(img, xy, text, fnt, colors=NEON, glow=0.55, anchor="la"):
    """Gradient text with a soft glow of its own colours."""
    w, h = img.size
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).text(xy, text, font=fnt, fill=255, anchor=anchor)
    box = mask.getbbox()
    if not box:
        return box
    # The gradient over the whole canvas (its end colours beyond the text): the glow
    # spreads past the letters, and a black margin there drew a dark box.
    row = Image.new("RGB", (w, 1))
    for x in range(w):
        row.putpixel((x, 0), multi(colors, (x - box[0]) / max(box[2] - box[0] - 1, 1)))
    grad = row.resize((w, h))
    blur = mask.filter(ImageFilter.GaussianBlur(fnt.size * 0.18)).point(lambda v: int(v * glow))
    img.paste(grad, (0, 0), blur)
    img.paste(grad, (0, 0), mask)
    return box


def text(img, xy, s, fnt, fill, anchor="la", spacing=0):
    d = ImageDraw.Draw(img)
    if spacing:
        x, y = xy
        for ch in s:
            d.text((x, y), ch, font=fnt, fill=fill)
            x += d.textlength(ch, font=fnt) + spacing
        return
    d.text(xy, s, font=fnt, fill=fill, anchor=anchor)


def glass(img, box, radius, alpha=40, edge=70):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    d.rounded_rectangle(box, radius=radius, fill=(255, 255, 255, alpha), outline=(255, 255, 255, edge), width=2)
    img.alpha_composite(layer)


def chip(img, x, y, label, fnt, hot=False, pad=(26, 14)):
    d = ImageDraw.Draw(img)
    tw = d.textlength(label, font=fnt)
    h = fnt.size + pad[1] * 2
    box = [x, y, x + tw + pad[0] * 2, y + h]
    if hot:
        shadow = Image.new("RGBA", img.size, (0, 0, 0, 0))
        ImageDraw.Draw(shadow).rounded_rectangle(box, radius=h // 2, fill=(255, 80, 60, 150))
        img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(16)))
        m = Image.new("L", img.size, 0)
        ImageDraw.Draw(m).rounded_rectangle(box, radius=h // 2, fill=255)
        g = Image.new("RGB", img.size)
        g.paste(hgrad(int(box[2] - box[0]), h, NEON[:2]), (int(box[0]), int(box[1])))
        img.paste(g, (0, 0), m)
    else:
        layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
        ImageDraw.Draw(layer).rounded_rectangle(box, radius=h // 2, fill=(40, 20, 90, 170), outline=(150, 130, 255, 90), width=2)
        img.alpha_composite(layer)
    d = ImageDraw.Draw(img)
    d.text((x + pad[0], y + h / 2), label, font=fnt, fill=(255, 255, 255), anchor="lm")
    return box[2]


def chips(img, x, y, labels, fnt, max_x, gap=14, line_gap=16, hot_first=True):
    x0 = x
    d = ImageDraw.Draw(img)
    for i, label in enumerate(labels):
        w = d.textlength(label, font=fnt) + 52
        if x + w > max_x:
            x = x0
            y += fnt.size + 28 + line_gap
        x = chip(img, x, y, label, fnt, hot=hot_first and i == 0) + gap
    return y + fnt.size + 28


def ring(img, cx, cy, r0, bars=72, length=(40, 150), width=14, seed=7):
    """Radial equaliser bars, blue at the top → violet → pink at the bottom, glowing."""
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    for i in range(bars):
        a = 2 * math.pi * i / bars - math.pi / 2
        v = (math.sin(i * 0.9 + seed) * 0.5 + 0.5) * 0.6 + (math.sin(i * 0.23 + seed * 2) * 0.5 + 0.5) * 0.4
        L = length[0] + (length[1] - length[0]) * v
        t = (math.sin(a) + 1) / 2  # 0 top, 1 bottom
        col = multi([(70, 210, 255), (150, 80, 255), (255, 60, 190)], t)
        p0 = (cx + math.cos(a) * r0, cy + math.sin(a) * r0)
        p1 = (cx + math.cos(a) * (r0 + L), cy + math.sin(a) * (r0 + L))
        d.line([p0, p1], fill=col + (235,), width=width)
    img.alpha_composite(layer.filter(ImageFilter.GaussianBlur(14)))
    img.alpha_composite(layer.filter(ImageFilter.GaussianBlur(2)))


def paste_icon(img, cx, cy, size):
    icon = Image.open(os.path.join(ROOT, "docs", "icon.png")).convert("RGBA").resize((size, size), Image.LANCZOS)
    img.alpha_composite(icon, (int(cx - size / 2), int(cy - size / 2)))


VERSION = "0.5.8"
FEATURES = ["Голосом Алисы", "Mac: строка меню", "Пульт ТВ", "Планы + Google", "Будильник", "AirPlay", "Ambilight"]


def stat_card(img, cx, cy, big, small):
    """A glass card «100+ колонок / уже с soundya», laid out by the text's own widths."""
    d = ImageDraw.Draw(img)
    fb, f1, f2 = font(big, "Black", italic=True), font(small, "Bold"), font(small, "Medium")
    bw = d.textlength("100+", font=fb)
    tw = max(d.textlength("колонок", font=f1), d.textlength("уже с soundya", font=f2))
    pad, gap = big * 0.42, big * 0.32
    cw = pad * 2 + bw + gap + tw
    ch = big * 1.6
    box = [cx - cw / 2, cy - ch / 2, cx + cw / 2, cy + ch / 2]
    glass(img, box, int(ch * 0.26), alpha=34, edge=80)
    neon_text(img, (box[0] + pad, cy), "100+", fb, anchor="lm")
    tx = box[0] + pad + bw + gap
    text(img, (tx, cy - small * 0.65), "колонок", f1, (255, 255, 255), anchor="lm")
    text(img, (tx, cy + small * 0.65), "уже с soundya", f2, (210, 200, 240), anchor="lm")


def hero(out):
    w, h = 1774, 887
    img = background(w, h)
    # Right: the new icon in a ring of equaliser bars, a «100+» card under it.
    cx, cy = 1340, 345
    ring(img, cx, cy, 185, bars=64, length=(26, 100), width=13)
    paste_icon(img, cx, cy, 310)
    stat_card(img, cx, 700, 84, 30)
    # Left: version, wordmark, thanks, features.
    text(img, (112, 150), f"ВЕРСИЯ {VERSION}", font(32, "Heavy"), PINK, spacing=7)
    neon_text(img, (100, 196), "soundya", font(210, "Black", italic=True))
    text(img, (112, 455), "SoundCloud на Яндекс Станции", font(46, "Bold"), (255, 255, 255))
    text(img, (112, 520), "Голосом Алисы, с телефона и из строки меню Mac", font(30, "Medium"), (205, 195, 235))
    chips(img, 112, 590, FEATURES, font(28, "Semibold"), max_x=1060)
    img.convert("RGB").save(out, quality=92)


def whatsnew(out):
    w, h = 1600, 533
    img = background(w, h)
    glass(img, [90, 105, 905, 300], 40, alpha=28, edge=70)
    neon_text(img, (140, 205), "Что нового", font(112, "Black", italic=True), anchor="lm")
    text(img, (140, 380), f"в soundya {VERSION}", font(48, "Bold"), (255, 255, 255), anchor="lm")
    # Right: a stack of small glass tiles with the headline features.
    tiles = [("", "Mac: трей"), ("", "Фильмы ТВ"), ("", "Планы"), ("", "Будильник"), ("", "Ambilight"), ("", "Обложки")]
    f = font(30, "Semibold")
    x0, y0, tw, th, gap = 990, 95, 250, 96, 22
    for i, (_, label) in enumerate(tiles):
        x = x0 + (i % 2) * (tw + gap)
        y = y0 + (i // 2) * (th + gap)
        glass(img, [x, y, x + tw, y + th], 26, alpha=30, edge=80)
        dot = [x + 26, y + th / 2 - 12, x + 50, y + th / 2 + 12]
        m = Image.new("L", img.size, 0)
        ImageDraw.Draw(m).ellipse(dot, fill=255)
        g = Image.new("RGB", img.size, multi(NEON, i / 5))
        img.paste(g, (0, 0), m)
        text(img, (x + 70, y + th / 2), label, f, (255, 255, 255), anchor="lm")
    img.convert("RGB").save(out, quality=92)


def telegram(out):
    w, h = 1280, 720
    img = background(w, h)
    cx, cy = 1010, 270
    ring(img, cx, cy, 135, bars=56, length=(20, 75), width=11)
    paste_icon(img, cx, cy, 225)
    text(img, (78, 96), "ОБНОВЛЕНИЕ 0.5.00", font(26, "Heavy"), PINK, spacing=6)
    neon_text(img, (66, 128), "soundya", font(150, "Black", italic=True))
    text(img, (78, 330), "Нас больше ста!", font(56, "Heavy"), (255, 255, 255))
    text(img, (78, 400), "Спасибо, что слушаете и рассказываете друзьям", font(28, "Medium"), (210, 200, 240))
    chips(img, 78, 470, ["Пульт ТВ", "Планы", "Помодоро", "Bluetooth", "Mac и Windows", "Откат"], font(25, "Semibold"), max_x=800)
    stat_card(img, cx, 560, 60, 24)
    img.convert("RGB").save(out, quality=93)


def phone(shot, width, tilt=0.0):
    """A screenshot in a phone frame: rounded glass body, a thin bezel, soft shadow, optional tilt."""
    im = Image.open(os.path.join(ROOT, "docs", "screens", shot)).convert("RGB")
    sw = width - 28
    sh = int(im.height * sw / im.width)
    sh = min(sh, int(sw * 2.05))  # a phone's proportions: cut long screenshots at the bottom
    im = im.resize((sw, int(im.height * sw / im.width)), Image.LANCZOS).crop((0, 0, sw, sh))
    W, H = width, sh + 28
    body = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(body)
    d.rounded_rectangle([0, 0, W - 1, H - 1], radius=int(W * 0.13), fill=(24, 18, 40, 255), outline=(150, 130, 220, 120), width=3)
    m = Image.new("L", (sw, sh), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, sw - 1, sh - 1], radius=int(W * 0.11), fill=255)
    body.paste(im, (14, 14), m)
    if tilt:
        body = body.rotate(tilt, resample=Image.BICUBIC, expand=True)
    return body


def drop(img, el, x, y, blur=40, alpha=150):
    sh = Image.new("RGBA", img.size, (0, 0, 0, 0))
    a = el.split()[3].point(lambda v: v * alpha // 255)
    sh.paste((10, 0, 30, 255), (x + 10, y + 30), a)
    img.alpha_composite(sh.filter(ImageFilter.GaussianBlur(blur)))
    img.alpha_composite(el, (x, y))


def showcase(out):
    """Four phones with the panel, captions on top."""
    w, h = 1600, 900
    img = background(w, h)
    items = [("home.jpg", "Главная"), ("music.jpg", "Ваша музыка"), ("plans.jpg", "Планы"), ("remote.jpg", "Пульт ТВ")]
    pw = 330
    gap = (w - len(items) * pw) // (len(items) + 1)
    for i, (shot, title) in enumerate(items):
        x = gap + i * (pw + gap)
        neon_text(img, (x + pw // 2, 62), title, font(38, "Heavy"), colors=[(255, 255, 255), (230, 215, 255)], glow=0.35, anchor="mm")
        ph = phone(shot, pw)
        drop(img, ph, x, 110)
    img.convert("RGB").save(out, quality=90)


def web_banner(out):
    """«Пульт в телефоне»: the README section banner, two tilted phones with the new panel."""
    w, h = 1600, 500
    img = background(w, h)
    big = font(112, "Black", italic=True)
    white = [(255, 255, 255), (240, 225, 255)]
    neon_text(img, (90, 140), "Пульт", big, colors=white, glow=0.5, anchor="lm")
    neon_text(img, (90, 262), "в телефоне", big, colors=white, glow=0.5, anchor="lm")
    text(img, (96, 350), "плеер, музыка, пульт ТВ, планы и Ambilight", font(30, "Medium"), (215, 205, 240))
    text(img, (96, 392), "на soundya.local:8765", font(30, "Medium"), (215, 205, 240))
    p1 = phone("home.jpg", 330, tilt=8)
    p2 = phone("remote.jpg", 330, tilt=-6)
    drop(img, p1, 940, 40)
    drop(img, p2, 1235, 10)
    img.convert("RGB").save(out, quality=90)


def mac_banner(out):
    """soundya in the Mac menu bar: a strip of the menu bar with its icon, the glass window under it."""
    w, h = 1600, 700
    img = background(w, h)
    # The menu bar.
    bar = Image.new("RGBA", (w, 52), (18, 14, 30, 210))
    img.alpha_composite(bar, (0, 0))
    ImageDraw.Draw(img).line([(0, 52), (w, 52)], fill=(255, 255, 255, 30), width=1)
    icon = Image.open(os.path.join(ROOT, "installer", "src", "menubar.png")).convert("RGBA")
    icon = icon.resize((44, 36), Image.LANCZOS)
    white = Image.new("RGBA", icon.size, (255, 255, 255, 255))
    white.putalpha(icon.split()[3])
    hl = Image.new("RGBA", (62, 40), (0, 0, 0, 0))
    ImageDraw.Draw(hl).rounded_rectangle([0, 0, 61, 39], radius=10, fill=(255, 255, 255, 50))
    ix = 1120
    img.alpha_composite(hl, (ix - 31, 6))
    img.alpha_composite(white, (ix - 22, 8))
    text(img, (w - 40, 26), "Вт 10:00", font(24, "Semibold"), (240, 235, 250), anchor="rm")
    # The window, hanging from the icon.
    shot = Image.open(os.path.join(ROOT, "docs", "screens", "glass.jpg")).convert("RGB")
    ww = 430
    wh = 540
    sh = shot.resize((ww, int(shot.height * ww / shot.width)), Image.LANCZOS).crop((0, 0, ww, wh))
    win = Image.new("RGBA", (ww, wh), (0, 0, 0, 0))
    m = Image.new("L", (ww, wh), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, ww - 1, wh - 1], radius=22, fill=255)
    win.paste(sh, (0, 0), m)
    ImageDraw.Draw(win).rounded_rectangle([0, 0, ww - 1, wh - 1], radius=22, outline=(255, 255, 255, 50), width=2)
    drop(img, win, ix - ww // 2, 70)
    # Left: what it is.
    neon_text(img, (90, 150), "soundya", font(120, "Black", italic=True))
    text(img, (96, 345), "в строке меню Mac", font(54, "Heavy"), (255, 255, 255))
    for i, line in enumerate(["Клик по значку — и плеер под рукой, без браузера",
                              "Музыка с колонки или прямо на Mac",
                              "Сценарии, пульт ТВ, планы — что сами выберете"]):
        text(img, (96, 440 + i * 46), "•  " + line, font(28, "Medium"), (215, 205, 240))
    img.convert("RGB").save(out, quality=90)


if __name__ == "__main__":
    hero(os.path.join(ROOT, "docs", "banner.jpg"))
    whatsnew(os.path.join(ROOT, "docs", "banner-whatsnew.jpg"))
    showcase(os.path.join(ROOT, "docs", "showcase.jpg"))
    web_banner(os.path.join(ROOT, "docs", "banner-web.jpg"))
    mac_banner(os.path.join(ROOT, "docs", "banner-mac.jpg"))
    print("banners: docs/banner.jpg, banner-whatsnew.jpg, showcase.jpg, banner-web.jpg, banner-mac.jpg")
