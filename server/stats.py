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
    # Everything else (codenames like Plum/Cucumber/Bergamot, chip names) — shown
    # as its own cleaned name, not hidden in a bucket.
    name = re.sub(r"(?i)^yandex[\s_-]*", "", (raw or "").strip()).replace("_", " ").replace("-", " ").strip()
    return name or "неизвестная"


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


# Realistic-ish Yandex-speaker illustrations (SVG), shaded to look 3D. One side
# slot shows them one at a time, cycling in place. Shared <defs> live in the page.
SPK_DEFS = """
<linearGradient id="cyl" x1="0" y1="0" x2="1" y2="0">
<stop offset="0" stop-color="#17161c"/><stop offset=".28" stop-color="#3a3844"/>
<stop offset=".52" stop-color="#4c4a58"/><stop offset=".74" stop-color="#2a2933"/><stop offset="1" stop-color="#121117"/></linearGradient>
<radialGradient id="topf" cx=".4" cy=".35" r=".8">
<stop offset="0" stop-color="#5a5866"/><stop offset=".7" stop-color="#34323c"/><stop offset="1" stop-color="#25242c"/></radialGradient>
<linearGradient id="fab" x1="0" y1="0" x2="1" y2="0">
<stop offset="0" stop-color="#201f26"/><stop offset=".3" stop-color="#46434e"/>
<stop offset=".55" stop-color="#565361"/><stop offset=".8" stop-color="#302e37"/><stop offset="1" stop-color="#171619"/></linearGradient>
<linearGradient id="ringg" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#ff7a00"/><stop offset=".5" stop-color="#ff2f6d"/><stop offset="1" stop-color="#9b4bff"/></linearGradient>
<filter id="glow" x="-40%" y="-40%" width="180%" height="180%"><feGaussianBlur stdDeviation="4" result="b"/>
<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>
<filter id="soft" x="-60%" y="-60%" width="220%" height="220%"><feGaussianBlur stdDeviation="9"/></filter>
"""

# Station Mini — wide low puck with a glowing top ring.
SPK_MINI = """<svg class="mdl" data-k="mini" viewBox="0 0 260 300" fill="none">
<ellipse cx="130" cy="270" rx="96" ry="18" fill="#000" opacity=".5" filter="url(#soft)"/>
<path d="M34,150 A96,32 0 0 0 226,150 L226,214 A96,32 0 0 1 34,214 Z" fill="url(#cyl)"/>
<ellipse cx="130" cy="150" rx="96" ry="32" fill="url(#topf)"/>
<ellipse cx="130" cy="150" rx="70" ry="23" fill="none" stroke="rgba(255,255,255,.055)" stroke-width="2"/>
<ellipse cx="130" cy="150" rx="46" ry="15" fill="none" stroke="rgba(255,255,255,.055)" stroke-width="2"/>
<ellipse cx="130" cy="150" rx="90" ry="29" fill="none" stroke="url(#ringg)" stroke-width="3" filter="url(#glow)" opacity=".92"/>
<ellipse cx="130" cy="150" rx="11" ry="3.6" fill="rgba(255,255,255,.22)"/>
<ellipse cx="100" cy="141" rx="32" ry="8" fill="rgba(255,255,255,.1)"/>
</svg>"""

# Station Lite — medium cylinder with fabric and a light ring near the base.
SPK_LITE = """<svg class="mdl" data-k="lite" viewBox="0 0 260 300" fill="none">
<ellipse cx="130" cy="282" rx="74" ry="15" fill="#000" opacity=".5" filter="url(#soft)"/>
<path d="M66,104 A64,23 0 0 0 194,104 L194,248 A64,23 0 0 1 66,248 Z" fill="url(#fab)"/>
<g stroke="rgba(0,0,0,.25)" stroke-width="1.5">
<path d="M70,134 A64,23 0 0 0 190,134"/><path d="M69,160 A64,23 0 0 0 191,160"/>
<path d="M69,186 A64,23 0 0 0 191,186"/><path d="M70,212 A64,23 0 0 0 190,212"/></g>
<ellipse cx="130" cy="104" rx="64" ry="23" fill="url(#topf)"/>
<ellipse cx="130" cy="104" rx="30" ry="10" fill="none" stroke="rgba(255,255,255,.07)" stroke-width="2"/>
<ellipse cx="130" cy="238" rx="62" ry="21" fill="none" stroke="url(#ringg)" stroke-width="3" filter="url(#glow)" opacity=".9"/>
<ellipse cx="104" cy="98" rx="24" ry="6" fill="rgba(255,255,255,.1)"/>
</svg>"""

# Station (big) — tall wide cylinder with a glowing front light bar.
SPK_STATION = """<svg class="mdl" data-k="st" viewBox="0 0 260 300" fill="none">
<ellipse cx="130" cy="284" rx="94" ry="17" fill="#000" opacity=".5" filter="url(#soft)"/>
<path d="M40,96 A90,30 0 0 0 220,96 L220,250 A90,30 0 0 1 40,250 Z" fill="url(#fab)"/>
<g stroke="rgba(0,0,0,.22)" stroke-width="1.5">
<path d="M44,128 A90,30 0 0 0 216,128"/><path d="M43,158 A90,30 0 0 0 217,158"/>
<path d="M43,188 A90,30 0 0 0 217,188"/></g>
<ellipse cx="130" cy="96" rx="90" ry="30" fill="url(#topf)"/>
<ellipse cx="130" cy="96" rx="52" ry="17" fill="none" stroke="rgba(255,255,255,.06)" stroke-width="2"/>
<path d="M70,228 A90,30 0 0 0 190,228" stroke="url(#ringg)" stroke-width="6" filter="url(#glow)" opacity=".92" fill="none" stroke-linecap="round"/>
<ellipse cx="98" cy="90" rx="30" ry="8" fill="rgba(255,255,255,.1)"/>
</svg>"""

SPEAKERS = SPK_MINI + SPK_LITE + SPK_STATION


def page(s):
    e = html.escape
    mpeak = max([m["count"] for m in s["models"]] + [1])
    model_rows = ""
    for m in s["models"]:
        model_rows += (
            f'<div class="row"><div class="nm">{e(m["model"])}</div>'
            f'<div class="bar"><i style="--_w:{100 * m["count"] / mpeak:.0f}%"></i></div>'
            f'<div class="ct">{m["count"]}</div></div>'
        )
    dpeak = max([d["count"] for d in s["days"]] + [1])
    day_bars = "".join(
        f'<div class="d" style="height:{max(5, 100 * d["count"] / dpeak):.0f}%" data-v="{d["day"][5:]} · {d["count"]}"></div>'
        for d in reversed(s["days"])
    )
    ver_rows = "".join(
        f'<div class="kv"><span>{e(v["version"])}</span><b>{v["count"]}</b></div>' for v in s["versions"]
    )
    return f"""<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>soundya · статистика</title><style>
:root{{--ink:#f4f2fa;--muted:#aaa2bb;--accent:#ff5500;--accent2:#ff2f6d;
  --glass:rgba(255,255,255,.055);--glass2:rgba(255,255,255,.09);--edge:rgba(255,255,255,.12)}}
*{{box-sizing:border-box}}
body{{margin:0;font:15px/1.5 -apple-system,Segoe UI,Roboto,sans-serif;color:var(--ink);min-height:100vh;background:#0b0a10;overflow-x:hidden}}
.bgblobs{{position:fixed;inset:0;z-index:-1;overflow:hidden}}
.bgblobs i{{position:absolute;border-radius:50%;filter:blur(80px)}}
.bgblobs i:nth-child(1){{width:520px;height:520px;background:#ff5500;top:-160px;right:-120px;opacity:.38}}
.bgblobs i:nth-child(2){{width:460px;height:460px;background:#ff2f6d;top:34%;left:-180px;opacity:.26}}
.bgblobs i:nth-child(3){{width:420px;height:420px;background:#7b3bff;bottom:-160px;right:10%;opacity:.26}}
.wrap{{max-width:720px;margin:0 auto;padding:30px 20px 56px;position:relative}}
.card{{background:var(--glass);backdrop-filter:blur(26px) saturate(150%);-webkit-backdrop-filter:blur(26px) saturate(150%);
  border:1px solid var(--edge);border-radius:22px;box-shadow:0 18px 50px rgba(0,0,0,.4),inset 0 1px 0 rgba(255,255,255,.1)}}
.top{{display:flex;align-items:center;justify-content:space-between;margin-bottom:18px}}
.logo{{font-size:26px;font-weight:800;letter-spacing:-.03em}}
.logo span{{background:linear-gradient(120deg,var(--accent),var(--accent2));-webkit-background-clip:text;background-clip:text;color:transparent}}
.gh{{color:var(--muted);transition:.15s;display:inline-flex}}.gh:hover{{color:var(--ink);transform:translateY(-1px)}}
.hero{{display:flex;align-items:center;gap:18px;padding:22px 26px;margin-bottom:16px}}
.bigwrap{{position:relative}}
.big{{font-size:78px;font-weight:800;line-height:1;letter-spacing:-.04em;cursor:pointer;user-select:none;
  background:linear-gradient(120deg,var(--accent),var(--accent2));-webkit-background-clip:text;background-clip:text;color:transparent}}
.big:active{{transform:scale(.96)}}
.btip{{position:absolute;left:0;top:100%;margin-top:6px;white-space:nowrap;font-size:13px;color:var(--ink);
  background:var(--glass2);border:1px solid var(--edge);border-radius:10px;padding:6px 10px;
  opacity:0;pointer-events:none;transition:.15s;transform:translateY(-4px);backdrop-filter:blur(10px)}}
.bigwrap:hover .btip{{opacity:1;transform:none}}
.heroL{{color:var(--muted);max-width:230px}}
.panel{{padding:18px 20px;margin-bottom:16px}}
h2{{font-size:12px;text-transform:uppercase;letter-spacing:.09em;color:var(--muted);margin:0 0 14px;font-weight:700}}
.row{{display:flex;align-items:center;gap:12px;margin:10px 0}}
.row .nm{{width:170px;font-size:14px;flex:none;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.bar{{flex:1;height:26px;background:rgba(255,255,255,.06);border-radius:9px;overflow:hidden}}
.bar i{{display:block;height:100%;border-radius:9px;background:linear-gradient(90deg,var(--accent),var(--accent2));
  width:0;box-shadow:0 0 18px rgba(255,85,0,.35);animation:g 1s cubic-bezier(.2,.8,.2,1) forwards}}
.row .ct{{width:40px;text-align:right;font-variant-numeric:tabular-nums;font-weight:600}}
@keyframes g{{to{{width:var(--_w)}}}}
.cols{{display:grid;grid-template-columns:1.4fr 1fr;gap:16px}}
@media(max-width:560px){{.cols{{grid-template-columns:1fr}}}}
.days{{display:flex;align-items:flex-end;gap:4px;height:100px}}
.days .d{{flex:1;background:linear-gradient(180deg,var(--accent),rgba(255,47,109,.35));border-radius:5px 5px 0 0;
  min-height:5px;position:relative;transition:.15s}}
.days .d:hover{{filter:brightness(1.3)}}
.days .d:hover::after{{content:attr(data-v);position:absolute;bottom:calc(100% + 6px);left:50%;transform:translateX(-50%);
  font-size:11px;white-space:nowrap;color:var(--ink);background:var(--glass2);border:1px solid var(--edge);padding:1px 7px;border-radius:7px;backdrop-filter:blur(8px)}}
.kv{{display:flex;justify-content:space-between;padding:7px 0;border-bottom:1px solid rgba(255,255,255,.07);font-size:14px}}
.kv:last-child{{border:0}}.kv b{{font-variant-numeric:tabular-nums}}
/* One speaker on the side: visible only while the page is hovered, cycling models in place. */
.stage{{position:fixed;top:50%;right:5vw;transform:translateY(-50%) translateX(16px);width:240px;height:300px;z-index:1;
  pointer-events:none;opacity:0;transition:opacity .45s,transform .45s;display:none}}
@media(min-width:1180px){{.stage{{display:block}}}}
body:hover .stage{{opacity:1;transform:translateY(-50%)}}
.stage .mdl{{position:absolute;inset:0;width:100%;height:100%;opacity:0;transition:opacity .8s}}
.stage .mdl.on{{opacity:1}}
.heart{{position:fixed;z-index:50;pointer-events:none;color:#ff2f6d;text-shadow:0 2px 8px rgba(255,47,109,.5);font-size:22px;animation:fly 1.4s ease-out forwards}}
@keyframes fly{{0%{{opacity:1;transform:translate(0,0) scale(.6)}}100%{{opacity:0;transform:translate(var(--dx),-170px) scale(1.2)}}}}
</style></head><body>
<div class="bgblobs"><i></i><i></i><i></i></div>
<svg width="0" height="0"><defs>{SPK_DEFS}</defs></svg>
<div class="stage">{SPEAKERS}</div>
<div class="wrap">
<div class="top">
  <div class="logo">sound<span>ya</span></div>
  <a class="gh" href="https://github.com/rodyatotu/soundya" target="_blank" title="GitHub">
    <svg width="26" height="26" viewBox="0 0 24 24" fill="currentColor"><path d="M12 .5A11.5 11.5 0 0 0 .5 12a11.5 11.5 0 0 0 7.86 10.92c.58.1.79-.25.79-.56v-2c-3.2.7-3.87-1.37-3.87-1.37-.53-1.33-1.3-1.69-1.3-1.69-1.06-.72.08-.71.08-.71 1.17.08 1.79 1.2 1.79 1.2 1.04 1.79 2.73 1.27 3.4.97.1-.76.41-1.27.74-1.56-2.55-.29-5.24-1.28-5.24-5.67 0-1.25.45-2.28 1.19-3.08-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.18 1.18a11 11 0 0 1 5.79 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.8 1.19 1.83 1.19 3.08 0 4.4-2.69 5.37-5.25 5.66.42.36.8 1.08.8 2.18v3.24c0 .31.21.67.8.56A11.5 11.5 0 0 0 23.5 12 11.5 11.5 0 0 0 12 .5z"/></svg>
  </a>
</div>
<div class="card hero">
  <div class="bigwrap"><div class="big" id="big">{s["active"]}</div><div class="btip">всего установлено за всё время: {s["ever"]}</div></div>
  <div class="heroL">колонок с soundya сейчас работают</div>
</div>
<div class="card panel"><h2>Модели колонок</h2>{model_rows}</div>
<div class="cols">
  <div class="card panel"><h2>Активность по дням</h2><div class="days">{day_bars}</div></div>
  <div class="card panel"><h2>Версии</h2>{ver_rows}</div>
</div>
</div>
<script>
(function(){{
  var m=document.querySelectorAll('.stage .mdl'),i=0;
  if(m.length){{m[0].classList.add('on');
    setInterval(function(){{m[i].classList.remove('on');i=(i+1)%m.length;m[i].classList.add('on');}},2600);}}
  var big=document.getElementById('big'),n=0,t;
  big.addEventListener('click',function(){{
    n++;clearTimeout(t);t=setTimeout(function(){{n=0;}},1200);
    if(n>=5){{n=0;hearts();}}
  }});
  function hearts(){{
    var r=big.getBoundingClientRect(),cx=r.left+r.width/2,cy=r.top+r.height/2;
    for(var k=0;k<16;k++){{
      var h=document.createElement('div');h.className='heart';h.textContent='❤';
      h.style.left=(cx-10)+'px';h.style.top=(cy-10)+'px';
      h.style.setProperty('--dx',(Math.random()*220-110).toFixed(0)+'px');
      h.style.animationDelay=(Math.random()*0.3).toFixed(2)+'s';
      h.style.fontSize=(16+Math.random()*16).toFixed(0)+'px';
      document.body.appendChild(h);
      h.addEventListener('animationend',function(){{this.remove();}});
    }}
  }}
}})();
</script>
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
