#!/usr/bin/env python3
"""End-to-end test of the planner with Google, without a Google account:

  soundya daemon (host build) ⇄ tests/fake_google.py (OAuth + Calendar + Tasks)

Sign-in by pasting the browser's address (PKCE checked by the fake), agenda
by voice phrases, adding a task and an event to Google, completing a task,
the 30-minute deadline warning, a renewed access key, secrets kept out of the
page and of bug reports, sign-out.

    cargo build && python3 tests/planner_e2e.py
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "soundya")
G_PORT, WEB_PORT = 18810, 18811
BASE = f"http://127.0.0.1:{WEB_PORT}"
H = {"Host": f"127.0.0.1:{WEB_PORT}", "Origin": BASE}
CLIENT_ID = "123-test.apps.googleusercontent.com"
SECRET = "GOCSPX-test-secret"
REFRESH = "1//refresh-token-xyz"

ok = 0


def check(cond, what):
    global ok
    if not cond:
        print("FAIL:", what)
        sys.exit(1)
    ok += 1
    print("ok  ", what)


def post(path, body):
    data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, headers=H, method="POST")
    with urllib.request.urlopen(req, timeout=40) as r:
        return r.read().decode()


def status():
    req = urllib.request.Request(BASE + "/api/status", headers=H)
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def say(phrase):
    return post("/api/cmd", "plan say " + phrase)


def writes(d):
    p = os.path.join(d, "writes.jsonl")
    return [json.loads(l) for l in open(p)] if os.path.exists(p) else []


def wait(cond, t=30):
    end = time.time() + t
    while time.time() < end:
        if cond():
            return True
        time.sleep(0.5)
    return False


def main():
    tmp = tempfile.mkdtemp(prefix="soundya-planner-")
    gdir, sdir, run = (os.path.join(tmp, x) for x in ("google", "state", "run"))
    for d in (gdir, sdir, run):
        os.makedirs(d)
    g = subprocess.Popen([sys.executable, os.path.join(ROOT, "tests", "fake_google.py"), str(G_PORT), gdir])
    env = dict(os.environ, SOUNDYA_DIR=sdir, SOUNDYA_RUN=run, SOUNDYA_NO_VOICE="1", SOUNDYA_WEB_PORT=str(WEB_PORT))
    for k in ("SOUNDYA_GOOGLE_AUTH", "SOUNDYA_GOOGLE_OAUTH", "SOUNDYA_GOOGLE_API"):
        env[k] = f"http://127.0.0.1:{G_PORT}"
    env.pop("HTTPS_PROXY", None)
    env.pop("HTTP_PROXY", None)
    log = open(os.path.join(tmp, "daemon.log"), "w")
    d = subprocess.Popen([BIN, "daemon"], env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        time.sleep(3)
        logtext = lambda: open(os.path.join(tmp, "daemon.log")).read()

        # 1. Client settings; a wrong ID is refused.
        r = post("/api/settings", {"g_client_id": "nonsense", "g_client_secret": SECRET})
        check(r.startswith("error"), "a client ID that is not Google's is refused")
        r = post("/api/settings", {"onboarded": True, "planner_on": True, "g_client_id": CLIENT_ID, "g_client_secret": SECRET, "tz_offset": 180})
        check(r.startswith("OK"), "client ID and secret saved")
        s = status()["settings"]
        check(s["g_client_secret"] == "••••••••" and not s["g_connected"], "the secret is masked on the page, not signed in yet")

        # 2. Sign-in: open the address, Google sends the browser to 127.0.0.1 with a code; the user pastes it.
        r = post("/api/cmd", "google code http://127.0.0.1:53682/soundya?code=early")
        check(r.startswith("error") and "Войти" in r, "a code before «Войти в Google» is refused")
        url = post("/api/cmd", "google url")
        check(url.startswith("OK http://127.0.0.1:%d/o/oauth2/v2/auth?" % G_PORT), "sign-in address made")
        url = url[3:]
        try:
            urllib.request.build_opener(type("N", (urllib.request.HTTPRedirectHandler,), {"redirect_request": lambda *a, **k: None})).open(url)
            check(False, "Google should redirect")
        except urllib.error.HTTPError as e:
            landed = e.headers["Location"]
        check(landed.startswith("http://127.0.0.1:53682/soundya?state="), "Google sends the browser to the loopback address")
        r = post("/api/cmd", "google code " + landed)
        check(r == "OK: Google подключён", f"signed in by the pasted address ({r})")
        check(any(w["kind"] == "signed_in" for w in writes(gdir)), "the fake checked the PKCE verifier")
        r = post("/api/cmd", "google code " + landed)
        check(r.startswith("error"), "the same address cannot be used twice")
        conf = json.load(open(os.path.join(sdir, "config.json")))
        check(conf["g_refresh"] == REFRESH and conf["g_on"] is True, "the refresh token is saved on the speaker")
        check(REFRESH not in json.dumps(status()), "the refresh token is not on the page")

        # 3. Google data arrives.
        check(wait(lambda: status()["calendar"]["google"]["events"] >= 3), "events fetched from both calendars")
        gs = status()["calendar"]["google"]
        check(gs["tasks"] == 3 and gs["error"] == "", f"3 open tasks fetched ({gs})")
        check([c["name"] for c in gs["calendars"]] == ["Личный", "Праздники"] and all(c["read"] for c in gs["calendars"]), "calendars listed")

        # 4. Agenda by voice.
        r = say("какие планы на сегодня")
        check("Созвон с командой" in r and "Спортзал" in r and "Сдать отчёт до" in r and "Просрочено: Позвонить в банк" in r, f"«какие планы на сегодня»: {r}")
        r = say("что мне сегодня нужно сделать")
        check("Сдать отчёт" in r and "Без срока: Прочитать книгу" in r and "Спортзал" not in r, f"«что нужно сделать»: {r}")
        r = say("что у меня дальше")
        check("Сдать отчёт" in r and re.search(r"через (19|20) минут", r), f"«что у меня дальше»: {r}")
        r = say("какие планы на завтра")
        check("Праздник (весь день)" in r, f"«на завтра»: {r}")
        r = say("какое у меня расписание")
        check(r.index("Созвон") < r.index("Сдать отчёт") or r.index("Сдать отчёт") < r.index("Созвон"), f"«расписание»: {r}")

        # 5. The deadline warning, 30 minutes before «Сдать отчёт до HH:MM» (20 min from now).
        check(wait(lambda: "planner: Через 20 минут срок: Сдать отчёт" in logtext() or "planner: Через 19 минут срок: Сдать отчёт" in logtext(), 25),
              "warned before the deadline")
        check(logtext().count("срок: Сдать отчёт") == 1, "warned once")
        # The calendar event in 40 min is said 10 min before (not yet).
        check("calendar: Через" not in logtext(), "the event is not announced too early")

        # 6. Writing to Google.
        r = say("добавь задачу сдать проект завтра в 18")
        check("Добавила задачу в «Мои задачи»: Сдать проект, срок завтра до 18:00" in r, f"task added: {r}")
        t = [w for w in writes(gdir) if w["kind"] == "task"][-1]["body"]
        check(t["title"] == "Сдать проект" and t["notes"] == "до 18:00" and t["due"].endswith("T00:00:00.000Z"), f"task in Google: {t}")
        r = say("добавь в календарь встречу с петей завтра в 15 на 2 часа")
        check("Добавила в календарь «Личный»: Встреча с петей, завтра в 15:00" in r, f"event added: {r}")
        e = [w for w in writes(gdir) if w["kind"] == "event"][-1]
        check(e["cal"] == "me@gmail.com" and e["body"]["start"]["dateTime"].endswith("T15:00:00+03:00") and e["body"]["end"]["dateTime"].endswith("T17:00:00+03:00"),
              f"event in the primary calendar, 2 hours: {e}")
        r = say("запланируй отпуск 12 марта")
        e = [w for w in writes(gdir) if w["kind"] == "event"][-1]["body"]
        check("date" in e["start"] and e["start"]["date"].endswith("-03-12"), f"a day without a time is a whole-day event: {r} {e}")
        r = say("отметь выполненной позвонить в банк")
        check(r == "OK: Отметила в Google: Позвонить в банк", f"completed in Google: {r}")
        p = [w for w in writes(gdir) if w["kind"] == "patch"][-1]
        check(p["id"] == "t2" and p["body"] == {"status": "completed"}, "the fake got status=completed")
        r = say("какие планы на сегодня")
        check("Позвонить в банк" not in r, "the completed task is gone from the agenda")
        r = post("/api/cmd", 'plan gadd {"title":"Купить подарок","day":"2026-12-30","time":"19:00"}')
        check(r.startswith("OK"), f"a task from the page: {r}")
        r = post("/api/cmd", "plan gdel MDE t3")
        check(r == "OK" and any(w["kind"] == "delete" for w in writes(gdir)), "a task deleted from the page")

        # 7. The short-lived access key was renewed with the refresh token.
        check(wait(lambda: any(w["kind"] == "refreshed" for w in writes(gdir)), 30) or (say("какие планы на завтра") and any(w["kind"] == "refreshed" for w in writes(gdir))),
              "the access key renewed by itself")

        # 8. Bug report preview: no secret, no refresh token.
        pv = post("/api/report/preview", {"module": "calendar", "logs": True, "description": "x"})
        check(SECRET not in pv and REFRESH not in pv, "no Google secrets in a bug report")

        # 9. The page's timeline.
        pj = json.loads(post("/api/cmd", "plan json"))
        titles = [i["title"] for i in pj["items"]]
        check("Созвон с командой" in titles and "Сдать проект" in titles and pj["google"] is True, "timeline for the page")

        # 10. The morning brief at its minute, once.
        import datetime as dt
        m = dt.datetime.now(dt.timezone(dt.timedelta(hours=3)))
        r = post("/api/settings", {"brief_on": True, "brief_at": m.hour * 60 + m.minute, "brief_days": "all"})
        check(r.startswith("OK"), "morning brief switched on for this minute")
        check(wait(lambda: "planner: brief: Доброе утро! Сегодня" in logtext(), 20), "the morning brief is said")
        time.sleep(12)
        check(logtext().count("planner: brief:") == 1, "and only once a day")
        check(open(os.path.join(sdir, "brief-day")).read().strip().isdigit(), "the day is kept on flash (no repeat after a restart)")

        # 11. Sign-out.
        r = post("/api/cmd", "google off")
        check(r == "OK: Google отключён", "signed out")
        conf = json.load(open(os.path.join(sdir, "config.json")))
        check(conf["g_refresh"] == "" and conf["g_client_id"] == CLIENT_ID, "refresh token removed, client kept")
        check(wait(lambda: status()["calendar"]["google"]["on"] is False, 5), "Google off in the status")
        r = say("какие планы на сегодня")
        check("Созвон" not in r, "Google's events are gone")
        print(f"\n{ok} checks passed")
    finally:
        d.terminate()
        g.terminate()
        d.wait()
        g.wait()


if __name__ == "__main__":
    main()
