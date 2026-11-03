#!/usr/bin/env python3
"""soundya statistics server: counts speakers that run soundya.

Each speaker sends once a day (soundya src/stats.rs):
    POST /ping  {"id": "<random 32 hex>", "version": "0.4.1", "model": "yandexmicro",
                 "airplay": true, "pc": false}
Nothing else is stored: no IP addresses, no tracks, no accounts.

Anonymous feedback from the web panel is forwarded to a Telegram bot:
    POST /feedback {"text": "...", "version": "0.4.40", "model": "yandexmini"}
Set SOUNDYA_TG_TOKEN (bot token from @BotFather) and SOUNDYA_TG_CHAT (your
chat id) to enable it; without them /feedback replies 503. No IP is stored.

Pages:
    GET /badge.svg   "работает на 47 колонках" for the README
    GET /badge.json  the same for shields.io (needs HTTPS)
    GET /            counts by model and version, speakers per day
    GET /stats.json  the same as JSON

Python 3.8+, standard library only. Run:  python3 stats.py --port 8787 --db /var/lib/soundya-stats/stats.db
"""

import argparse
import collections
import datetime as dt
import html
import json
import os
import re
import sqlite3
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACTIVE_DAYS = 7  # a speaker counts as running if it pinged within this many days
MODELS = {
    "yandexmicro": "Станция Лайт",
    "yandexmini": "Станция Мини 1",
    "yandexmini_2": "Станция Мини 2",
    "yandexstation_2": "Станция 2",
    "yandexstation": "Станция 1",
    "yandexmidi": "Станция Миди",
}
def pretty_model(raw):
    """Collapse raw hostnames / device-tree names to a clean model name.
    Per-unit hostnames like 'yandex-mini2-224K' all become 'Станция Мини 2'."""
    m = (raw or "").lower().replace("_", "").replace("-", "").replace(" ", "")
    if "yandexmini2" in m or "mini2" in m:
        return "Станция Мини 2"
    if "yandexmicro" in m or "stationlite" in m or "micro" in m:
        return "Станция Лайт"
    if "yandexmidi" in m or "midi" in m:
        return "Станция Миди"
    if "yandexstation2" in m or "station2" in m:
        return "Станция 2"
    if "yandexmini" in m or "mini" in m:
        return "Станция Мини 1"
    if "yandexstation" in m:
        return "Станция 1"
    if raw in MODELS:
        return MODELS[raw]
    # Engineering samples (Plum/Cucumber/Bergamot EVT/DVT) and unknown Yandex units.
    if "yandex" in m:
        return "Станция (другая модель)"
    return "Прочие"


ID_RE = re.compile(r"^[0-9a-f]{16,64}$")
TEXT_RE = re.compile(r"^[\w.\- ]{0,40}$")

# Telegram bot for anonymous feedback from the web panel (set in the environment).
TG_TOKEN = os.environ.get("SOUNDYA_TG_TOKEN", "").strip()
TG_CHAT = os.environ.get("SOUNDYA_TG_CHAT", "").strip()
FEEDBACK_MAX = 2000  # characters
# Simple in-memory limits so the bot cannot be flooded (nothing is persisted).
_fb_lock = threading.Lock()
_fb_times = collections.deque()  # timestamps of accepted feedback, last hour
_fb_last = {}  # remote ip -> last time (kept in memory only, never stored or logged)

db_lock = threading.Lock()
db = None


def open_db(path):
    con = sqlite3.connect(path, check_same_thread=False)
    con.executescript(
        """
        CREATE TABLE IF NOT EXISTS speakers (
            id TEXT PRIMARY KEY, first_seen TEXT, last_seen TEXT,
            version TEXT, model TEXT, airplay INTEGER, pc INTEGER);
        CREATE TABLE IF NOT EXISTS days (day TEXT, id TEXT, PRIMARY KEY (day, id));
        """
    )
    return con


def now():
    return dt.datetime.now(dt.timezone.utc)


def active_count():
    since = (now() - dt.timedelta(days=ACTIVE_DAYS)).isoformat()
    with db_lock:
        return db.execute("SELECT COUNT(*) FROM speakers WHERE last_seen >= ?", (since,)).fetchone()[0]


def speakers_word(n):
    """«на 1 колонке», «на 21 колонке», «на 2 колонках», «на 11 колонках»."""
    return "колонке" if n % 10 == 1 and n % 100 != 11 else "колонках"


def badge_svg(label, value, color="ff5500"):
    # Width estimate for Verdana 11px (Cyrillic is a bit wider than Latin).
    def width(text):
        return int(sum(7.5 if ord(c) > 127 else 6.5 for c in text)) + 10

    lw, vw = width(label), width(value)
    total = lw + vw
    label_e, value_e = html.escape(label), html.escape(value)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="20" role="img" aria-label="{label_e}: {value_e}">
<title>{label_e}: {value_e}</title>
<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<clipPath id="r"><rect width="{total}" height="20" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{vw}" height="20" fill="#{color}"/><rect width="{total}" height="20" fill="url(#s)"/></g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
<text x="{lw / 2}" y="15" fill="#010101" fill-opacity=".3">{label_e}</text><text x="{lw / 2}" y="14">{label_e}</text>
<text x="{lw + vw / 2}" y="15" fill="#010101" fill-opacity=".3">{value_e}</text><text x="{lw + vw / 2}" y="14">{value_e}</text>
</g></svg>"""


def collapse(rows):
    """Fold (raw_model, count) rows into [{model, count}] by pretty name, desc."""
    agg = {}
    for m, c in rows:
        agg[pretty_model(m)] = agg.get(pretty_model(m), 0) + c
    return [{"model": k, "count": v} for k, v in sorted(agg.items(), key=lambda kv: -kv[1])]


def stats():
    since = (now() - dt.timedelta(days=ACTIVE_DAYS)).isoformat()
    with db_lock:
        total = db.execute("SELECT COUNT(*) FROM speakers").fetchone()[0]
        active = db.execute("SELECT COUNT(*) FROM speakers WHERE last_seen >= ?", (since,)).fetchone()[0]
        raw_models = db.execute(
            "SELECT model, COUNT(*) FROM speakers WHERE last_seen >= ? GROUP BY model", (since,)
        ).fetchall()
        versions = db.execute(
            "SELECT version, COUNT(*) FROM speakers WHERE last_seen >= ? GROUP BY version ORDER BY 2 DESC", (since,)
        ).fetchall()
        airplay = db.execute("SELECT COUNT(*) FROM speakers WHERE last_seen >= ? AND airplay", (since,)).fetchone()[0]
        pc = db.execute("SELECT COUNT(*) FROM speakers WHERE last_seen >= ? AND pc", (since,)).fetchone()[0]
        days = db.execute("SELECT day, COUNT(*) FROM days GROUP BY day ORDER BY day DESC LIMIT 60").fetchall()
    return {
        "active": active,
        "active_days": ACTIVE_DAYS,
        "ever": total,
        "models": collapse(raw_models),
        "versions": [{"version": v or "?", "count": c} for v, c in versions],
        "airplay": airplay,
        "pc": pc,
        "days": [{"day": d, "count": c} for d, c in days],
    }


def page(s):
    rows = lambda items, key: "".join(
        f"<tr><td>{html.escape(str(i[key]))}</td><td>{i['count']}</td></tr>" for i in items
    )
    peak = max([d["count"] for d in s["days"]] + [1])
    bars = "".join(
        f'<div class="bar"><span style="width:{100 * d["count"] / peak:.0f}%"></span><em>{d["day"][5:]} · {d["count"]}</em></div>'
        for d in reversed(s["days"])
    )
    return f"""<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>soundya: статистика</title><style>
body{{font:15px/1.5 system-ui,sans-serif;max-width:640px;margin:24px auto;padding:0 16px;background:#111;color:#eee}}
h1{{font-size:22px}} .big{{font-size:48px;font-weight:700;color:#ff5500}} table{{border-collapse:collapse;width:100%;margin:8px 0 20px}}
td{{padding:4px 0;border-bottom:1px solid #333}} td:last-child{{text-align:right}}
.bar{{position:relative;height:20px;margin:3px 0;background:#222;border-radius:3px}} .bar span{{display:block;height:100%;background:#ff5500;border-radius:3px;opacity:.6}}
.bar em{{position:absolute;left:6px;top:0;font-style:normal;font-size:12px;line-height:20px}}
</style></head><body><h1>soundya</h1>
<div class="big">{s["active"]}</div><p>колонок работали за последние {s["active_days"]} дней · всего было {s["ever"]}<br>
AirPlay включён: {s["airplay"]} · компьютер подключён: {s["pc"]}</p>
<h2>Модели</h2><table>{rows(s["models"], "model")}</table>
<h2>Версии</h2><table>{rows(s["versions"], "version")}</table>
<h2>По дням</h2>{bars}
<p style="color:#888;font-size:13px">Колонки присылают раз в день только случайный номер, версию и модель. Никаких IP, треков и аккаунтов.</p>
</body></html>"""


# Person-directed insults and slurs: feedback matching these is still delivered,
# but hidden behind a Telegram spoiler so the author need not read it unless they
# tap. General swearing (мат) is NOT here — it comes through normally.
INSULT_WORDS = (
    "уебок уёбок мудак долбоеб долбоёб долбаеб долбаёб дебил идиот кретин придурок "
    "тупой тупая тупорыл ничтожество урод гандон гондон чмо мразь тварь скотина "
    "ублюдок даун пидор пидар пидорас педик гнида выблядок чурка хач нигер негр "
    "дегенерат имбецил овца овуляшка быдло ушлепок ушлёпок хуесос козлина"
).split()


def looks_like_insult(text):
    low = text.lower().replace("ё", "е")
    return any(w.replace("ё", "е") in low for w in INSULT_WORDS)


def send_telegram(text, html=False):
    """Posts the text to the Telegram bot. Returns True on success."""
    url = f"https://api.telegram.org/bot{TG_TOKEN}/sendMessage"
    body = {"chat_id": TG_CHAT, "text": text, "disable_web_page_preview": True}
    if html:
        body["parse_mode"] = "HTML"
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status == 200
    except Exception as e:  # network, bad token, wrong chat id
        print(f"feedback: telegram error: {e}", flush=True)
        return False


def accept_feedback(ip):
    """In-memory rate check: at most 1 per 20 s per sender, 30 per hour total."""
    nowt = time.monotonic()
    with _fb_lock:
        while _fb_times and nowt - _fb_times[0] > 3600:
            _fb_times.popleft()
        if len(_fb_times) >= 30:
            return False
        if nowt - _fb_last.get(ip, 0) < 20:
            return False
        # Forget old per-sender entries so the dict stays small.
        for k in [k for k, t in _fb_last.items() if nowt - t > 3600]:
            del _fb_last[k]
        _fb_last[ip] = nowt
        _fb_times.append(nowt)
    return True


class Handler(BaseHTTPRequestHandler):
    server_version = "soundya-stats"

    def log_message(self, *args):
        pass  # no access log: it would hold IP addresses

    def send(self, code, body, kind, cache="no-cache, max-age=0"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", cache)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/badge.svg":
            n = active_count()
            self.send(200, badge_svg("работает на", f"{n} {speakers_word(n)}"), "image/svg+xml; charset=utf-8")
        elif path == "/badge.json":
            n = active_count()
            body = {"schemaVersion": 1, "label": "работает на", "message": f"{n} {speakers_word(n)}", "color": "ff5500"}
            self.send(200, json.dumps(body, ensure_ascii=False), "application/json; charset=utf-8")
        elif path == "/stats.json":
            self.send(200, json.dumps(stats(), ensure_ascii=False), "application/json; charset=utf-8")
        elif path == "/":
            self.send(200, page(stats()), "text/html; charset=utf-8")
        else:
            self.send(404, "not found", "text/plain")

    def do_POST(self):
        route = self.path.split("?")[0]
        if route == "/feedback":
            return self.do_feedback()
        if route != "/ping":
            return self.send(404, "not found", "text/plain")
        try:
            length = min(int(self.headers.get("Content-Length", 0)), 2048)
            p = json.loads(self.rfile.read(length) or b"{}")
            sid = str(p.get("id", "")).lower()
            version = str(p.get("version", ""))[:20]
            model = str(p.get("model", ""))[:40]
            if not ID_RE.match(sid) or not TEXT_RE.match(version) or not TEXT_RE.match(model):
                return self.send(400, "bad ping", "text/plain")
            t = now()
            with db_lock:
                db.execute(
                    """INSERT INTO speakers VALUES (?, ?, ?, ?, ?, ?, ?)
                       ON CONFLICT(id) DO UPDATE SET last_seen=excluded.last_seen, version=excluded.version,
                       model=excluded.model, airplay=excluded.airplay, pc=excluded.pc""",
                    (sid, t.isoformat(), t.isoformat(), version, model, int(bool(p.get("airplay"))), int(bool(p.get("pc")))),
                )
                db.execute("INSERT OR IGNORE INTO days VALUES (?, ?)", (t.date().isoformat(), sid))
                db.commit()
            self.send(200, "ok", "text/plain")
        except (ValueError, json.JSONDecodeError):
            self.send(400, "bad ping", "text/plain")

    def do_feedback(self):
        if not TG_TOKEN or not TG_CHAT:
            return self.send(503, "feedback disabled", "text/plain")
        try:
            length = min(int(self.headers.get("Content-Length", 0)), FEEDBACK_MAX + 512)
            p = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self.send(400, "bad feedback", "text/plain")
        text = str(p.get("text", "")).strip()[:FEEDBACK_MAX]
        if not text:
            return self.send(400, "empty", "text/plain")
        # The sender ip is used only for the rate limit, held in memory, never stored.
        ip = self.client_address[0] if self.client_address else "?"
        if not accept_feedback(ip):
            return self.send(429, "too many", "text/plain")
        version = str(p.get("version", ""))[:20]
        model = str(p.get("model", ""))[:40]
        tag = " · ".join(x for x in (pretty_model(model), version) if x)
        head = "💬 Обратная связь soundya" + (f" ({tag})" if tag else "")
        if looks_like_insult(text):
            # Delivered, but hidden behind a spoiler: tap in Telegram to read.
            body = html.escape(head) + " ⚠️\n\n<tg-spoiler>" + html.escape(text) + "</tg-spoiler>"
            ok = send_telegram(body, html=True)
        else:
            ok = send_telegram(head + "\n\n" + text)
        self.send(200, "ok", "text/plain") if ok else self.send(502, "not delivered", "text/plain")


def main():
    global db
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--db", default="stats.db")
    a = ap.parse_args()
    db = open_db(a.db)
    print(f"soundya-stats on {a.host}:{a.port}, db {a.db}", flush=True)
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
