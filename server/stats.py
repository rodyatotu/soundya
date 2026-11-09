#!/usr/bin/env python3
"""soundya statistics server: counts speakers that run soundya.

Each speaker sends once a day (soundya src/stats.rs):
    POST /ping  {"id": "<random 32 hex>", "version": "0.4.1", "model": "yandexmicro",
                 "airplay": true, "pc": false}
Nothing else is stored: no IP addresses, no tracks, no accounts.

Bug reports and feedback from the web panel are forwarded to a Telegram bot:
    POST /report   (soundya 0.5.00+, src/report.rs) {"id": "SY-…", "message": "BUG REPORT #…",
                   "files": [{"name": "system.log", "data": "<base64>"}], …}
                   → one message plus each file as a document in reply to it.
                   The same id is delivered once: the speaker retries until it gets 200.
    POST /feedback (0.4.40) {"text": "...", "version": "0.4.40", "model": "yandexmini"}
Set SOUNDYA_TG_TOKEN (bot token from @BotFather) and SOUNDYA_TG_CHAT (your
chat id) to enable it; without them both reply 503. No IP is stored.

Pages:
    GET /badge.svg   "работает на 47 колонках" for the README
    GET /badge.json  the same for shields.io (needs HTTPS)
    GET /            counts by model and version, speakers per day
    GET /stats.json  the same as JSON
    GET /tg/<channel> the latest posts of an open Telegram channel (soundya 0.5.2+ morning news,
                     src/morning.rs): only the posts' text blocks of t.me/s/<channel>, kept
                     10 minutes. A server that cannot reach Telegram asks the relay's.

Python 3.8+, standard library only. Run:  python3 stats.py --port 8787 --db /var/lib/soundya-stats/stats.db
"""

import argparse
import base64
import binascii
import collections
import datetime as dt
import html
import json
import os
import re
import sqlite3
import threading
import time
import urllib.error
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
    # Everything else (codenames like Plum/Cucumber/Bergamot, chip names) — shown
    # as its own cleaned name, not hidden in a bucket.
    name = re.sub(r"(?i)^yandex[\s_-]*", "", (raw or "").strip()).replace("_", " ").replace("-", " ").strip()
    return name or "неизвестная"


ID_RE = re.compile(r"^[0-9a-f]{16,64}$")
TEXT_RE = re.compile(r"^[\w.\- ]{0,40}$")

# Telegram bot for anonymous feedback from the web panel (set in the environment).
TG_TOKEN = os.environ.get("SOUNDYA_TG_TOKEN", "").strip()
TG_CHAT = os.environ.get("SOUNDYA_TG_CHAT", "").strip()
# Only for tests (a fake Telegram); the real one by default.
TG_API = os.environ.get("SOUNDYA_TG_API", "https://api.telegram.org").rstrip("/")
# SOUNDYA_REPORT_RELAY=http://<other server>:8787/report: this server cannot reach Telegram,
# so bug reports go on to that one (it sends them to the bot). Feedback and stats stay here.
REPORT_RELAY = os.environ.get("SOUNDYA_REPORT_RELAY", "").strip()
# On the receiving server: the relaying servers' addresses, whose X-Forwarded-For is trusted
# (else every relayed report would count as one sender: one per 20 s for all speakers).
RELAY_FROM = {a.strip() for a in os.environ.get("SOUNDYA_RELAY_FROM", "").split(",") if a.strip()}
# The relay's /tg/: the same server as SOUNDYA_REPORT_RELAY, or SOUNDYA_TG_RELAY if set.
TG_RELAY = os.environ.get("SOUNDYA_TG_RELAY", "").strip() or (REPORT_RELAY[: -len("/report")] if REPORT_RELAY.endswith("/report") else "")
TG_WEB = os.environ.get("SOUNDYA_TG_WEB", "https://t.me/s").rstrip("/")  # tests point it elsewhere
TG_NAME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]{3,31}$")
TG_TTL = 600            # seconds a channel's posts are kept
TG_FETCHES = 120        # at most this many fetches from Telegram per 10 minutes, for everyone
tg_cache = {}           # name -> (time, body)
tg_fetched = collections.deque()
tg_lock = threading.Lock()
REPORT_MAX = 8 << 20  # bytes of JSON (a screenshot of up to 3 MB in base64, logs)
REPORT_FILES = {"system.log", "error.log", "diagnostics.json", "screenshot.png", "screenshot.jpg"}
REPORT_ID_RE = re.compile(r"^SY-[0-9A-F]{8}$")
FEEDBACK_MAX = 2000  # characters
# Simple in-memory limits so the bot cannot be flooded (nothing is persisted).
_fb_lock = threading.Lock()
_fb_times = collections.deque()  # timestamps of accepted feedback, last hour
_fb_last = {}  # remote ip -> last time (kept in memory only, never stored or logged)

# The page template lives next to this script (copied there by install.sh).
BASE = os.path.dirname(os.path.abspath(__file__))
_tpl = None


def _read(name):
    with open(os.path.join(BASE, name), "r", encoding="utf-8") as f:
        return f.read()

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
        CREATE TABLE IF NOT EXISTS reports (id TEXT PRIMARY KEY, received TEXT, message_id INTEGER);
        CREATE INDEX IF NOT EXISTS speakers_last_seen ON speakers (last_seen);
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
    """Fold (raw_model, count) rows into [{model, count, raw:[(name,count)]}] by
    pretty name, most common first. `raw` lets the page show what's inside."""
    agg = {}
    for m, c in rows:
        k = pretty_model(m)
        e = agg.setdefault(k, {"count": 0, "raw": []})
        e["count"] += c
        e["raw"].append((m or "?", c))
    out = [{"model": k, "count": v["count"], "raw": sorted(v["raw"], key=lambda x: -x[1])} for k, v in agg.items()]
    return sorted(out, key=lambda x: -x["count"])


_stats_cache = {"at": 0.0, "value": None}


def invalidate_stats():
    _stats_cache["at"] = 0.0


def stats():
    """Cached for a minute: the page and stats.json no longer scan the tables on every visit."""
    t = time.monotonic()
    if _stats_cache["value"] is not None and t - _stats_cache["at"] < 60:
        return _stats_cache["value"]
    value = _stats()
    _stats_cache.update(at=t, value=value)
    return value


def _stats():
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
    # The page shows the top 30: a flood of made-up models and versions once made it
    # half a million rows long, and it never finished building.
    return {
        "active": active,
        "active_days": ACTIVE_DAYS,
        "ever": total,
        "models": collapse(raw_models)[:30],
        "versions": [{"version": v or "?", "count": c} for v, c in versions[:30]],
        "airplay": airplay,
        "pc": pc,
        "days": [{"day": d, "count": c} for d, c in days],
    }


# Realistic-ish Yandex-speaker illustrations (SVG), shaded to look 3D. One side
# slot shows them one at a time, cycling in place. Shared <defs> live in the page.
def page(s):
    """The stats page (server/showcase.html) with live data injected."""
    global _tpl
    if _tpl is None:
        _tpl = _read("showcase.html")
    data = {
        "active": s["active"],
        "ever": s["ever"],
        "models": [{"model": m["model"], "count": m["count"]} for m in s["models"]],
        "versions": s["versions"],
    }
    blob = json.dumps(data, ensure_ascii=False).replace("<", "\\u003c")
    return _tpl.replace("/*DATA*/", blob, 1)


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


def tg_call(method, data=None, files=None):
    """Calls the Bot API. Returns the decoded result or raises."""
    url = f"{TG_API}/bot{TG_TOKEN}/{method}"
    if files:
        boundary = "soundya" + os.urandom(8).hex()
        parts = []
        for k, v in (data or {}).items():
            parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode())
        for field, (name, content) in files.items():
            parts.append(
                f'--{boundary}\r\nContent-Disposition: form-data; name="{field}"; filename="{name}"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n".encode() + content + b"\r\n"
            )
        parts.append(f"--{boundary}--\r\n".encode())
        body, kind = b"".join(parts), f"multipart/form-data; boundary={boundary}"
    else:
        body, kind = json.dumps(data or {}).encode(), "application/json"
    req = urllib.request.Request(url, data=body, headers={"Content-Type": kind})
    with urllib.request.urlopen(req, timeout=30) as r:
        out = json.loads(r.read() or b"{}")
    if not out.get("ok"):
        raise RuntimeError(out.get("description", "telegram error"))
    return out.get("result")


def send_telegram(text, html=False):
    """Posts the text to the Telegram bot. Returns True on success."""
    body = {"chat_id": TG_CHAT, "text": text, "disable_web_page_preview": True}
    if html:
        body["parse_mode"] = "HTML"
    try:
        tg_call("sendMessage", body)
        return True
    except Exception as e:  # network, bad token, wrong chat id
        print(f"feedback: telegram error: {e}", flush=True)
        return False


def sender_key(handler):
    """Who is sending, for the rate limit only (memory, never stored). Behind
    Caddy every request comes from 127.0.0.1: then its X-Forwarded-For counts."""
    ip = handler.client_address[0] if handler.client_address else "?"
    # Caddy on this machine, or a server that relays reports here (SOUNDYA_RELAY_FROM).
    if ip in ("127.0.0.1", "::1") or ip in RELAY_FROM:
        fwd = handler.headers.get("X-Forwarded-For", "").split(",")[0].strip()
        if fwd:
            return fwd
    return ip


def deliver_report(p):
    """Sends a report to Telegram. Returns (http code, text)."""
    rid = str(p.get("id", ""))
    if not REPORT_ID_RE.match(rid):
        return 400, "bad id"
    with db_lock:
        done = db.execute("SELECT 1 FROM reports WHERE id = ?", (rid,)).fetchone()
    if done:
        return 200, "already delivered"
    message = str(p.get("message", "")).strip()
    if not message:
        return 400, "empty"
    files = []
    for f in p.get("files") or []:
        name = str(f.get("name", ""))
        if name not in REPORT_FILES:
            return 400, f"unexpected file {name[:40]}"
        try:
            files.append((name, base64.b64decode(str(f.get("data", "")), validate=True)))
        except (ValueError, binascii.Error):
            return 400, f"bad file {name}"
    text = message[:3900]
    if looks_like_insult(message):
        body = {"chat_id": TG_CHAT, "parse_mode": "HTML", "disable_web_page_preview": True,
                "text": "⚠️ <tg-spoiler>" + html.escape(text) + "</tg-spoiler>"}
    else:
        body = {"chat_id": TG_CHAT, "text": text, "disable_web_page_preview": True}
    try:
        msg = tg_call("sendMessage", body)
        mid = msg.get("message_id") if isinstance(msg, dict) else None
        for name, content in files:
            data = {"chat_id": TG_CHAT, "caption": f"{rid} · {name}"}
            if mid:
                data["reply_to_message_id"] = mid
            tg_call("sendDocument", data, {"document": (name, content)})
    except Exception as e:
        print(f"report {rid}: telegram error: {e}", flush=True)
        return 502, "not delivered"
    with db_lock:
        db.execute("INSERT OR IGNORE INTO reports VALUES (?, ?, ?)", (rid, now().isoformat(), mid))
        db.commit()
    print(f"report {rid}: delivered with {len(files)} files", flush=True)
    return 200, "ok"


def accept_feedback(ip, record=True):
    """In-memory rate check: at most 1 per 20 s per sender, 30 per hour total.
    record=False only checks; note_feedback() then counts a delivered one, so a
    delivery that failed (Telegram down) does not block the speaker's retry."""
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
        if record:
            _fb_last[ip] = nowt
            _fb_times.append(nowt)
    return True


# /ping flood guard (memory only, nothing about senders is stored). A speaker pings once a
# day; new installation ids are rare. A flood of random ids made the counter show 470 000
# «speakers» and the page unusable.
PING_NEW_PER_IP_DAY = 3        # new ids one address may add per day (a home has 1–3 speakers)
PING_NEW_PER_HOUR = 120        # new ids per hour from everyone together
_ping_lock = threading.Lock()
_ping_ip = {}                  # sender -> (day, new ids that day)
_ping_hour = collections.deque()


def allow_new_speaker(key):
    """May this sender add one more new id now? Counts it if so."""
    today = dt.date.today().isoformat()
    t = time.monotonic()
    with _ping_lock:
        while _ping_hour and t - _ping_hour[0] > 3600:
            _ping_hour.popleft()
        day, n = _ping_ip.get(key, (today, 0))
        if day != today:
            n = 0
        if n >= PING_NEW_PER_IP_DAY or len(_ping_hour) >= PING_NEW_PER_HOUR:
            return False
        _ping_ip[key] = (today, n + 1)
        _ping_hour.append(t)
        if len(_ping_ip) > 100_000:  # a new day, or a huge spread of addresses: start over
            _ping_ip.clear()
        return True


def note_feedback(ip):
    with _fb_lock:
        nowt = time.monotonic()
        _fb_last[ip] = nowt
        _fb_times.append(nowt)


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
        # Any failure answers 500 with a line in the journal: an unhandled error used to close
        # the connection with nothing (the browser's «ERR_EMPTY_RESPONSE»).
        try:
            self.get()
        except Exception as e:
            print(f"GET {self.path.split('?')[0]}: {type(e).__name__}: {e}", flush=True)
            try:
                self.send(500, "server error, see journalctl -u soundya-stats", "text/plain; charset=utf-8")
            except Exception:
                pass

    def get(self):
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
        elif path.startswith("/tg/"):
            code, body = tg_posts(path[4:])
            self.send(code, body, "text/html; charset=utf-8" if code == 200 else "text/plain; charset=utf-8", "public, max-age=300")
        else:
            self.send(404, "not found", "text/plain")

    def do_POST(self):
        route = self.path.split("?")[0]
        if route == "/feedback":
            return self.do_feedback()
        if route == "/report":
            return self.do_report()
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
                known = db.execute("SELECT 1 FROM speakers WHERE id = ?", (sid,)).fetchone() is not None
            # A known speaker always updates; a new id only within the limits.
            if not known and not allow_new_speaker(sender_key(self)):
                return self.send(429, "too many new speakers", "text/plain")
            with db_lock:
                db.execute(
                    """INSERT INTO speakers VALUES (?, ?, ?, ?, ?, ?, ?)
                       ON CONFLICT(id) DO UPDATE SET last_seen=excluded.last_seen, version=excluded.version,
                       model=excluded.model, airplay=excluded.airplay, pc=excluded.pc""",
                    (sid, t.isoformat(), t.isoformat(), version, model, int(bool(p.get("airplay"))), int(bool(p.get("pc")))),
                )
                db.execute("INSERT OR IGNORE INTO days VALUES (?, ?)", (t.date().isoformat(), sid))
                db.commit()
            if not known:
                invalidate_stats()
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
        ip = sender_key(self)
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


    def do_report(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        if length > REPORT_MAX:
            return self.send(413, "too large", "text/plain")
        # A server that cannot reach Telegram passes reports on to one that can.
        if REPORT_RELAY:
            return self.relay_report(length)
        if not TG_TOKEN or not TG_CHAT:
            return self.send(503, "reports disabled", "text/plain")
        try:
            p = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self.send(400, "bad report", "text/plain")
        # Retries of a delivered report are answered without counting them.
        with db_lock:
            seen = db.execute("SELECT 1 FROM reports WHERE id = ?", (str(p.get("id", "")),)).fetchone()
        key = sender_key(self)
        if not seen and not accept_feedback(key, record=False):
            return self.send(429, "too many", "text/plain")
        code, text = deliver_report(p)
        if code == 200 and not seen:
            note_feedback(key)
        self.send(code, text, "text/plain")


    def relay_report(self, length):
        body = self.rfile.read(length)
        req = urllib.request.Request(REPORT_RELAY, data=body, method="POST",
                                     headers={"Content-Type": "application/json", "X-Forwarded-For": sender_key(self)})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                code, text = r.status, r.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            code, text = e.code, e.read().decode("utf-8", "replace")
        except Exception as e:
            print(f"report relay {REPORT_RELAY}: {e}", flush=True)
            # 502: the speaker keeps the report and tries again later.
            code, text = 502, "relay unreachable"
        self.send(code, text[:500], "text/plain; charset=utf-8")


TG_POST_RE = re.compile(r'<div class="tgme_widget_message_text js-message_text"[^>]*>.*?</div>', re.S)


def tg_posts(name):
    """(code, body): the text blocks of an open channel's last posts, as t.me/s/ has them."""
    if not TG_NAME_RE.match(name):
        return 400, "bad channel name"
    now = time.time()
    with tg_lock:
        hit = tg_cache.get(name.lower())
        if hit and now - hit[0] < TG_TTL:
            return 200, hit[1]
        while tg_fetched and now - tg_fetched[0] > TG_TTL:
            tg_fetched.popleft()
        if len(tg_fetched) >= TG_FETCHES:
            return 429, "too many channels right now"
        tg_fetched.append(now)
    url = f"{TG_RELAY}/tg/{name}" if TG_RELAY else f"{TG_WEB}/{name}"
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 (soundya news)"})
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            page = r.read(2 << 20).decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return (e.code if e.code in (400, 404, 429) else 502), "telegram error"
    except Exception as e:
        print(f"tg {name}: {e}", flush=True)
        return 502, "telegram unreachable"
    body = page if TG_RELAY else "\n".join(TG_POST_RE.findall(page))
    if not body.strip():
        return 404, "no posts (the channel has no web page, or it is private)"
    with tg_lock:
        if len(tg_cache) > 500:
            tg_cache.clear()
        tg_cache[name.lower()] = (now, body)
    return 200, body


def purge(since):
    """Ids first seen on/after `since` that pinged on one day only: a real speaker pings every
    day it is on, so it comes back by itself tomorrow if it was caught here. One pass over
    each table (days has no index by id alone: a per-row count took hours on a flood)."""
    if not re.match(r"^\d{4}-\d{2}-\d{2}$", since):
        print("дата — в виде 2026-11-01")
        return 2
    with db_lock:
        db.executescript(
            """DROP TABLE IF EXISTS temp.dc; DROP TABLE IF EXISTS temp.junk;
               CREATE TEMP TABLE dc (id TEXT PRIMARY KEY, c INTEGER);
               INSERT INTO dc SELECT id, COUNT(*) FROM days GROUP BY id;
               CREATE TEMP TABLE junk (id TEXT PRIMARY KEY);"""
        )
        db.execute(
            """INSERT INTO junk SELECT s.id FROM speakers s LEFT JOIN dc ON dc.id = s.id
               WHERE substr(s.first_seen, 1, 10) >= ? AND COALESCE(dc.c, 0) <= 1""",
            (since,),
        )
        n = db.execute("SELECT COUNT(*) FROM junk").fetchone()[0]
        db.execute("DELETE FROM days WHERE id IN (SELECT id FROM junk)")
        db.execute("DELETE FROM speakers WHERE id IN (SELECT id FROM junk)")
        db.commit()
        db.executescript("DROP TABLE temp.dc; DROP TABLE temp.junk;")
        db.execute("VACUUM")
        left = db.execute("SELECT COUNT(*) FROM speakers").fetchone()[0]
    print(f"удалено накрученных id: {n}; осталось колонок за всё время: {left}")
    return 0


def selftest():
    """Sends one test report through the real bot: proves the last hop
    (server → Telegram) with the token and chat from the environment."""
    rid = "SY-" + os.urandom(4).hex().upper()
    msg = (f"BUG REPORT #{rid}\nSoundya Version: selftest\nDevice: server\nModule: Проверка сервера\n"
           f"Date: {now().strftime('%Y-%m-%d %H:%M:%S UTC')}\nUser/Installation ID: не передан\n"
           "Description: проверка доставки отчётов (stats.py --selftest)\nExpected: сообщение и 3 файла\n"
           "Actual: —\nAttachments:\n* system.log\n* error.log\n* diagnostics.json\n")
    files = [{"name": n, "data": base64.b64encode(t.encode()).decode()} for n, t in (
        ("system.log", "selftest: system log\n"), ("error.log", "selftest: no errors\n"),
        ("diagnostics.json", json.dumps({"selftest": True, "time": now().isoformat()})))]
    code, text = deliver_report({"id": rid, "message": msg, "files": files})
    print(f"selftest {rid}: {code} {text}")
    return 0 if code == 200 else 1


def start_community(data_dir):
    """SOUNDYA_COMMUNITY=1: the same bot also lets people into the Telegram community
    after they accept its rules (community-bot.py, next to this file), in its own thread.
    Its journals go next to the database (the service may write only there)."""
    if os.environ.get("SOUNDYA_COMMUNITY", "").strip() not in ("1", "yes", "true"):
        return
    if not TG_TOKEN:
        print("community: SOUNDYA_COMMUNITY=1, но нет SOUNDYA_TG_TOKEN — бот приёма не запущен", flush=True)
        return
    import importlib.util
    os.environ.setdefault("SOUNDYA_CONSENTS", os.path.join(data_dir, "consents.jsonl"))
    os.environ.setdefault("SOUNDYA_DECLINES", os.path.join(data_dir, "declines.jsonl"))
    try:
        spec = importlib.util.spec_from_file_location("community_bot", os.path.join(BASE, "community-bot.py"))
        bot = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bot)
    except Exception as e:  # an old install without the file must not stop the reports
        print(f"community: бот приёма не загрузился: {e}", flush=True)
        return
    # Its log lines straight to the journal (stdout is buffered under systemd).
    import functools
    bot.print = functools.partial(print, flush=True)

    def run():
        while True:
            try:
                bot.main()
            except Exception as e:  # never let the bot take the report server down
                print(f"community: {e}; restarting in 10 s", flush=True)
                time.sleep(10)

    threading.Thread(target=run, name="community-bot", daemon=True).start()
    print("community: бот приёма в сообщество запущен в этом же процессе", flush=True)


def main():
    global db
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--db", default="stats.db")
    ap.add_argument("--selftest", action="store_true", help="send a test report to the bot and exit")
    ap.add_argument("--flood-report", action="store_true", help="new ids per day (to see when a flood started) and exit")
    ap.add_argument("--purge-since", metavar="YYYY-MM-DD",
                    help="remove ids first seen on/after this day that pinged on one day only (a flood), and exit")
    a = ap.parse_args()
    db = open_db(a.db)
    if a.flood_report:
        for day, n in db.execute("SELECT substr(first_seen, 1, 10), COUNT(*) FROM speakers GROUP BY 1 ORDER BY 1 DESC LIMIT 30"):
            print(f"{day}  новых id: {n}")
        raise SystemExit(0)
    if a.purge_since:
        raise SystemExit(purge(a.purge_since))
    if a.selftest:
        if not TG_TOKEN or not TG_CHAT:
            print("selftest: set SOUNDYA_TG_TOKEN and SOUNDYA_TG_CHAT (or source /etc/soundya-stats.env)")
            raise SystemExit(2)
        raise SystemExit(selftest())
    print(f"soundya-stats on {a.host}:{a.port}, db {a.db}", flush=True)
    start_community(os.path.dirname(os.path.abspath(a.db)))
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
