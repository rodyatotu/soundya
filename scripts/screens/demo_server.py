#!/usr/bin/env python3
"""The panel (src/web.html) on demo data, for README screenshots: no speaker, no
personal tracks, tasks or names. Commands are answered «OK» and change nothing.

    python3 scripts/screens/demo_server.py 8899     → http://127.0.0.1:8899/
    python3 scripts/screens/shoot.py                → docs/screens/*.png (headless Chrome)
"""
import io
import json
import math
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
PAGE = os.path.join(ROOT, "src", "web.html")

PALETTES = [((255, 122, 26), (255, 47, 109)), ((42, 168, 255), (123, 59, 255)), ((60, 255, 180), (42, 212, 255)),
            ((255, 179, 71), (255, 95, 109)), ((164, 92, 255), (255, 92, 168)), ((0, 198, 255), (0, 114, 255)),
            ((255, 95, 109), (255, 195, 113)), ((67, 233, 123), (56, 249, 215))]


def art(n, size=300):
    """A cover: a gradient with a sun or bars and waves, like the old demo pictures."""
    a, b = PALETTES[n % len(PALETTES)]
    img = Image.new("RGB", (size, size))
    d = ImageDraw.Draw(img)
    for y in range(size):
        t = y / size
        d.line([(0, y), (size, y)], fill=tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3)))
    over = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    o = ImageDraw.Draw(over)
    if n % 2 == 0:
        r = size * 0.18
        cx, cy = size * (0.35 + 0.1 * (n % 3)), size * 0.38
        o.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(255, 235, 220, 230))
    else:
        for i in range(9):
            h = size * (0.12 + 0.28 * abs(math.sin(i * 1.7 + n)))
            x = size * (0.18 + i * 0.075)
            o.rectangle([x, size * 0.62 - h, x + size * 0.035, size * 0.62], fill=(255, 255, 255, 210))
    for k in range(3):
        pts = [(x, size * (0.68 + 0.07 * k) + math.sin(x / size * 6.28 * 2 + k + n) * size * 0.025) for x in range(0, size + 1, 6)]
        o.line(pts, fill=(255, 255, 255, 150), width=max(2, size // 120))
    img = Image.alpha_composite(img.convert("RGBA"), over).convert("RGB")
    buf = io.BytesIO()
    img.save(buf, "JPEG", quality=88)
    return buf.getvalue()


def art_url(n):
    return f"https://i1.sndcdn.com/artworks-demo{n}-t300x300.jpg"


TRACKS = [("Neon Avenue", "Night Drive"), ("Glass Harbor", "Low Tide"), ("Kite Season", "Paper Planes"),
          ("Mila Rowe", "Golden Hour"), ("Northern Lights", "Aurora"), ("Velvet Static", "Afterglow"),
          ("Sunday Club", "Slow Motion"), ("Echo Park", "Satellites")]

NOW = time.time()


def status():
    s = json.load(open(os.path.join(os.path.dirname(__file__), "demo_status.json")))
    started = NOW - 100
    s.update({
        "state": "playing", "track": "Neon Avenue — Night Drive", "cover": "/cover?demo",
        "position_ms": int((time.time() - started) * 1000) % 213000, "duration_ms": 213000, "current_id": 101,
        "queue": [f"{a} — {t}" for a, t in TRACKS], "pos": 0, "source": "soundcloud", "user": "Ваш аккаунт",
        "volume": 5, "loading": None,
    })
    return s


def plan():
    t0 = int(NOW // 86400 * 86400) - 3 * 3600
    item = lambda kind, title, h, m, **kw: dict({"kind": kind, "title": title, "source": "local", "id": title, "list": "", "at": t0 + h * 3600 + m * 60,
                                                "end": None, "all_day": False, "day": 0, "day_word": "сегодня", "time": f"{h:02d}:{m:02d}",
                                                "overdue": False, "priority": "normal"}, **kw)
    items = [item("event", "Созвон с командой", 12, 0, source="Google Календарь"), item("task", "Сдать отчёт", 18, 0),
             item("task", "Полить цветы", 20, 0), item("task", "Купить продукты", 21, 0)]
    tasks = [{"id": i, "title": t, "done": False, "due": t0 + h * 3600, "overdue": False, "priority": "normal", "remind": False,
              "repeat": "", "category": "", "check": False, "asks": 0, "created": t0, "g_id": "", "g_list": "", "profile": "",
              "ask_at": None, "unanswered": False} for i, (t, h) in enumerate([("Сдать отчёт", 18), ("Полить цветы", 20), ("Купить продукты", 21)], 1)]
    return {"enabled": True, "google": {"on": False}, "items": items, "tasks": tasks, "offset_min": 180,
            "today": "Сегодня 3 дела: сдать отчёт до 18:00, полить цветы и купить продукты. В 12:00 созвон с командой.",
            "plan": {"brief_at": 480, "brief_days": "weekdays", "brief_on": True, "brief_phrase": True, "due_time": 1080, "mode": "simple",
                     "target": "local", "warn": 30}}


def playlist(n, title, user="SoundCloud", count=40):
    return {"art": art_url(n), "count": count, "id": f"demo:{n}", "link": f"https://soundcloud.com/demo/sets/{n}", "title": title, "user": user}


ANSWERS = {
    "home": lambda: {"shelves": [
        {"title": "Для вас", "items": [playlist(1, "Your Mix 1"), playlist(2, "Your Mix 2"), playlist(3, "Daily Drops")]},
        {"title": "Похоже на ваши лайки", "items": [playlist(4, "Synthwave"), playlist(5, "Lo-fi beats"), playlist(6, "Deep House")]},
        {"title": "Mixed for you", "items": [playlist(7, "Chill Mix"), playlist(0, "Focus"), playlist(3, "Evening")]}]},
    "library": lambda: {"playlists": [playlist(i, t, "Вы", c) for i, (t, c) in enumerate([("Утро", 24), ("В дорогу", 51), ("Для работы", 37), ("Осень", 19)])]},
    "myplaylists": lambda: {"playlists": [playlist(i, t, "Вы", c) for i, (t, c) in enumerate([("Утро", 24), ("В дорогу", 51)])]},
    "history": lambda: {"tracks": [{"art": art_url(i), "at": int(NOW) - i * 900, "cmd": f"play demo{i}", "id": 200 + i, "name": f"{a} — {t}"}
                                   for i, (a, t) in enumerate(TRACKS)],
                        "sets": [{"at": int(NOW) - 3600, "cmd": "likes", "name": "Мои лайки"}]},
    "plan json": plan,
    "widgets": lambda: {"weather": {"city": "Санкт-Петербург", "code": 3, "temp": 7, "feels": 4, "max": 9, "min": 4, "rain": 20, "wind": 5, "text": "Пасмурно"},
                        "pcs": [{"name": "ноутбук", "cpu": 23, "temp": 54, "os": "Windows 11", "ram_total_gb": 16.0, "ram_used": 41, "uptime_h": 5}]},
    "macro json": lambda: {"commands": [], "tv_keys": [], "tv_names": {}, "macros": [
        {"phrase": "включи кино", "script": "tv wake"}, {"phrase": "доброе утро", "script": "likes"}, {"phrase": "рабочий режим", "script": "pc do открой телеграм"}]},
}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body, kind="application/json"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False)
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/wrap":
            # A phone-wide frame; after the page settles, the tab is opened as a user would.
            from urllib.parse import parse_qs, urlparse
            q = {k: v[0] for k, v in parse_qs(urlparse(self.path).query).items()}
            w, h, tab, frag = q.get("w", "390"), q.get("h", "844"), q.get("tab", ""), q.get("frag", "")
            # No animations in a still picture: a tab caught half faded in looked broken.
            js = ("setTimeout(() => { const f = document.querySelector('iframe').contentWindow;"
                  " f.document.head.insertAdjacentHTML('beforeend', '<style>*,*::before,*::after{animation:none!important;transition:none!important}</style>');"
                  f" if ('{tab}') f.openTab('{tab}'); }}, 2500);")
            return self.send(200, f'<html><body style="margin:0;background:#0d0b14"><iframe src="/{frag}" '
                                  f'style="border:0;width:{w}px;height:{h}px;display:block"></iframe><script>{js}</script></body></html>',
                             "text/html; charset=utf-8")
        if p in ("/", "/index.html"):
            return self.send(200, open(PAGE, encoding="utf-8").read(), "text/html; charset=utf-8")
        if p == "/api/status":
            return self.send(200, status())
        if p == "/api/settings":
            return self.send(200, status()["settings"])
        if p == "/cover":
            return self.send(200, art(0, 600), "image/jpeg")
        if p.startswith("/art/"):
            n = int("".join(c for c in p.split("demo")[-1].split("-")[0] if c.isdigit()) or 0)
            return self.send(200, art(n), "image/jpeg")
        if p == "/api/community":
            return self.send(200, {"ok": False})
        if p == "/api/report/list":
            return self.send(200, {"reports": []})
        return self.send(404, "not found", "text/plain")

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode("utf-8", "replace").strip()
        p = self.path.split("?")[0]
        if p == "/api/cmd" and body in ANSWERS:
            return self.send(200, ANSWERS[body]())
        if p == "/api/cmd" and body.startswith("plan text"):
            return self.send(200, "OK: " + plan()["today"], "text/plain; charset=utf-8")
        if p == "/api/listen":
            return self.send(200, {"listeners": 0, "gen": 0})
        return self.send(200, "OK", "text/plain; charset=utf-8")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
    print(f"demo panel: http://127.0.0.1:{port}/")
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
