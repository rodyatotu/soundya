#!/usr/bin/env python3
"""server/stats.py against a ping flood: one address adds at most a few new speakers a day,
a known speaker always updates, the page and stats.json always answer, and --purge-since
removes one-day ids.

    python3 tests/stats_server_test.py
"""
import json
import os
import secrets
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PORT = 18961
URL = f"http://127.0.0.1:{PORT}"


def ping(sid, model="yandexmicro"):
    body = json.dumps({"id": sid, "version": "0.5.0", "model": model}).encode()
    try:
        return urllib.request.urlopen(urllib.request.Request(URL + "/ping", data=body, method="POST"), timeout=5).status
    except urllib.error.HTTPError as e:
        return e.code


def get(path):
    try:
        r = urllib.request.urlopen(URL + path, timeout=10)
        return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def main():
    tmp = tempfile.mkdtemp(prefix="soundya-stats-")
    db = os.path.join(tmp, "stats.db")
    env = {k: v for k, v in os.environ.items() if not k.startswith("SOUNDYA_")}
    env["SOUNDYA_RELEASES_URL"] = ""  # no GitHub in tests; the release check has its own case below
    srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                            "--port", str(PORT), "--db", db], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    fails = 0

    def check(name, ok):
        nonlocal fails
        print(("  ok   " if ok else "  FAIL ") + name)
        fails += 0 if ok else 1

    try:
        ids = [secrets.token_hex(16) for _ in range(10)]
        codes = [ping(i) for i in ids]
        check("an address adds at most 3 new speakers a day", codes[:3] == [200] * 3 and set(codes[3:]) == {429})
        check("a known speaker always updates", ping(ids[0]) == 200 and ping(ids[1]) == 200)
        srv.terminate()
        srv.wait()
        srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                                "--port", str(PORT), "--db", db], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        time.sleep(1.5)
        check("a restart does not reset the limit", ping(secrets.token_hex(16)) == 429)
        code, body = get("/badge.json")
        check("the counter counts 3, not 10", code == 200 and "3 колонках" in body.decode())
        code, body = get("/stats.json")
        check("stats.json answers", code == 200 and json.loads(body)["active"] == 3)
        code, body = get("/")
        check("the page answers", code == 200 and b"<html" in body.lower())
    finally:
        srv.terminate()
        srv.wait()

    # A flood written straight into the database (as if before the limit): purge keeps real ones.
    con = sqlite3.connect(db)
    con.execute("UPDATE speakers SET first_seen = '2026-11-01T10:00:00+00:00'")
    con.execute("INSERT INTO days VALUES ('2026-11-01', ?)", (ids[0],))  # ids[0]: seen on two days
    for _ in range(50):
        sid = secrets.token_hex(16)
        con.execute("INSERT INTO speakers VALUES (?, '2026-11-07T10:00:00+00:00', '2026-11-07T10:00:00+00:00', '1', 'x', 0, 0)", (sid,))
        con.execute("INSERT INTO days VALUES ('2026-11-07', ?)", (sid,))
    con.commit()
    con.close()
    out = subprocess.run([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--db", db, "--purge-since", "2026-11-05"],
                         env=env, capture_output=True, text=True).stdout
    con = sqlite3.connect(db)
    left = con.execute("SELECT COUNT(*) FROM speakers").fetchone()[0]
    check("--purge-since removes the flood, keeps earlier speakers", left == 3 and "удалено накрученных id: 50" in out)
    rep = subprocess.run([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--db", db, "--flood-report"],
                         env=env, capture_output=True, text=True).stdout
    check("--flood-report lists new ids per day", "2026-11-01  новых id: 3" in rep)
    # Made-up versions are not counted; --ban-version removes the ones counted before and
    # bans those speakers; --ban <speaker id> bans one speaker.
    con = sqlite3.connect(db)
    fake = [secrets.token_hex(16) for _ in range(3)]
    for sid in fake:
        con.execute("INSERT INTO speakers VALUES (?, '2026-11-08T10:00:00+00:00', '2026-11-08T10:00:00+00:00', '1.4.88', 'x', 0, 0)", (sid,))
        con.execute("INSERT INTO days VALUES ('2026-11-08', ?)", (sid,))
    con.commit()
    con.close()
    out = subprocess.run([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--db", db, "--ban-version", "1.4.88"],
                         env=env, capture_output=True, text=True).stdout
    con = sqlite3.connect(db)
    check("--ban-version removes and bans those speakers",
          "колонок: 3" in out and con.execute("SELECT COUNT(*) FROM speakers WHERE version = '1.4.88'").fetchone()[0] == 0
          and con.execute("SELECT COUNT(*) FROM bans WHERE kind = 'speaker'").fetchone()[0] == 3)
    con.close()
    out = subprocess.run([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--db", db, "--ban", ids[2]],
                         env=env, capture_output=True, text=True).stdout
    check("--ban <speaker id>", "забанена колонка" in out)
    import http.server
    import threading

    class Tags(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            data = json.dumps([{"name": "v0.5.9"}, {"name": "v0.5.8"}]).encode()
            self.send_response(200)
            self.end_headers()
            self.wfile.write(data)

    tags = http.server.ThreadingHTTPServer(("127.0.0.1", PORT + 7), Tags)
    threading.Thread(target=tags.serve_forever, daemon=True).start()
    venv = dict(env, SOUNDYA_RELEASES_URL=f"http://127.0.0.1:{PORT + 7}/tags?per_page=100")
    srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                            "--port", str(PORT), "--db", db], env=venv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    try:
        def ping_v(sid, version, ip="5.5.5.5"):
            body = json.dumps({"id": sid, "version": version, "model": "yandexmicro"}).encode()
            req = urllib.request.Request(URL + "/ping", data=body, method="POST", headers={"X-Forwarded-For": ip})
            return urllib.request.urlopen(req, timeout=5).status
        new = [secrets.token_hex(16) for _ in range(6)]
        codes = [ping_v(new[0], "1.3.37"), ping_v(fake[0], "0.5.9"), ping_v(ids[2], "0.5.9"),
                 ping_v(new[1], "0.5.9-plum.1"), ping_v(new[2], "0.5.10"), ping_v(new[3], "0.4.99"),
                 ping_v(new[4], "0.5.8", ip="6.6.6.6")]
        con = sqlite3.connect(db)
        have = {r[0] for r in con.execute("SELECT id FROM speakers")}
        con.close()
        check("made-up versions and banned speakers get 200 and are not counted",
              codes == [200] * 7 and new[0] not in have and fake[0] not in have and ids[2] not in have)
        check("only released versions count (a build suffix counts as its release)",
              new[1] in have and new[4] in have and new[2] not in have and new[3] not in have)
    finally:
        srv.terminate()
        srv.wait()
        tags.shutdown()
    # A server that cannot reach Telegram passes reports on (SOUNDYA_REPORT_RELAY).
    import http.server
    import threading
    got = []

    class Relay(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            got.append(self.rfile.read(int(self.headers["Content-Length"])))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"delivered with 0 files")

    relay = http.server.ThreadingHTTPServer(("127.0.0.1", PORT + 2), Relay)
    threading.Thread(target=relay.serve_forever, daemon=True).start()
    env2 = dict(env, SOUNDYA_REPORT_RELAY=f"http://127.0.0.1:{PORT + 2}/report")
    srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                            "--port", str(PORT), "--db", db], env=env2, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    try:
        report = json.dumps({"id": "SY-0000ABCD", "message": "BUG REPORT #SY-0000ABCD", "files": []}).encode()
        r = urllib.request.urlopen(urllib.request.Request(URL + "/report", data=report, method="POST"), timeout=10)
        check("a report is passed on to the relay as it came", r.status == 200 and got == [report])
    finally:
        srv.terminate()
        srv.wait()
        relay.shutdown()
    # /tg/<channel>: a server that reaches Telegram answers with the posts' text blocks only,
    # a server that cannot asks the other one (SOUNDYA_REPORT_RELAY's server).
    asked = []

    class FakeTelegram(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            asked.append(self.path)
            if self.path.endswith("/closedchan"):
                body = b"<html>no web preview</html>"
            else:
                body = ('<html><div class="tgme_widget_message js-widget_message" data-post="meduzalive/41" data-view="q">'
                        '<div class="tgme_widget_message_text js-message_text" dir="auto"><b>One</b><br/>x</div>'
                        '<span class="tgme_widget_message_views">12.3K</span><time datetime="2026-11-14T10:00:00+00:00" class="time">10:00</time></div>'
                        '<script>junk</script><div class="tgme_widget_message js-widget_message" data-post="meduzalive/42">'
                        '<div class="tgme_widget_message_text js-message_text" dir="auto">Two</div>'
                        '<span class="tgme_widget_message_views">845</span><time datetime="2026-11-14T11:30:00+00:00">11:30</time></div></html>').encode()
            self.send_response(200)
            self.end_headers()
            self.wfile.write(body)

    tg = http.server.ThreadingHTTPServer(("127.0.0.1", PORT + 3), FakeTelegram)
    threading.Thread(target=tg.serve_forever, daemon=True).start()
    abroad = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1", "--port", str(PORT + 4),
                               "--db", os.path.join(tmp, "abroad.db")], env=dict(env, SOUNDYA_TG_WEB=f"http://127.0.0.1:{PORT + 3}/s"),
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    home = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1", "--port", str(PORT),
                             "--db", db], env=dict(env, SOUNDYA_REPORT_RELAY=f"http://127.0.0.1:{PORT + 4}/report"),
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    try:
        code, body = get("/tg/meduzalive")
        text = body.decode()
        check("/tg/ through the relay: the posts' blocks only", code == 200 and "One" in text and "Two" in text and "junk" not in text)
        check("/tg/ tells each post's number, views and time",
              'data-post="meduzalive/41" data-views="12300" data-time="1794650400"' in text and 'data-post="meduzalive/42" data-views="845"' in text)
        get("/tg/meduzalive")
        check("/tg/ is kept: Telegram asked once", asked.count("/s/meduzalive") == 1)
        check("/tg/ refuses odd names", get("/tg/../etc")[0] in (400, 404) and get("/tg/a%20b")[0] == 400)
        check("/tg/ of a channel without a web page: 404", get("/tg/closedchan")[0] == 404)
    finally:
        for p in (home, abroad):
            p.terminate()
            p.wait()
        tg.shutdown()
    # Reports: an empty file does not make the report come again every hour; the same text
    # under a new id is dropped; one sender is limited; --ban silences a sender for good.
    calls = []

    docs = []

    class FakeBot(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            method = self.path.rsplit("/", 1)[-1]
            calls.append(method)
            if method == "sendDocument":
                at = body.index(b'filename="')
                name = body[at + 10: body.index(b'"', at + 10)].decode()
                docs.append((name, body[body.index(b"\r\n\r\n", at) + 4: body.rindex(b"\r\n--")]))
            if method == "sendDocument" and b'filename="system.log"' in body and b'filename="system.log"\r\nContent-Type: application/octet-stream\r\n\r\n\r\n' in body:
                out = {"ok": False, "description": "Bad Request: file must be non-empty"}
            else:
                out = {"ok": True, "result": {"message_id": len(calls)}}
            data = json.dumps(out).encode()
            self.send_response(200)
            self.end_headers()
            self.wfile.write(data)

    bot = http.server.ThreadingHTTPServer(("127.0.0.1", PORT + 5), FakeBot)
    threading.Thread(target=bot.serve_forever, daemon=True).start()
    rdb = os.path.join(tmp, "reports.db")
    renv = dict(env, SOUNDYA_TG_TOKEN="x", SOUNDYA_TG_CHAT="1", SOUNDYA_TG_API=f"http://127.0.0.1:{PORT + 5}")
    srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                            "--port", str(PORT), "--db", rdb], env=renv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)

    def send_report(rid, text, files=(), ip="1.1.1.1"):
        msg = f"BUG REPORT #{rid}\nDate: 2026-11-10 22:02:48 UTC\nDescription: {text}\n"
        body = json.dumps({"id": rid, "message": msg, "files": [{"name": n, "data": d} for n, d in files]}).encode()
        req = urllib.request.Request(URL + "/report", data=body, method="POST", headers={"X-Forwarded-For": ip})
        try:
            r = urllib.request.urlopen(req, timeout=10)
            return r.status, r.read().decode()
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()

    try:
        first = send_report("SY-670561F0", "продолжи", [("system.log", ""), ("error.log", "ZXJy")])
        again = send_report("SY-670561F0", "продолжи", [("system.log", ""), ("error.log", "ZXJy")])
        check("a report with an empty file is delivered once", first == (200, "ok") and again[0] == 200
              and calls.count("sendMessage") == 1)
        import io
        import zipfile
        names = sorted(zipfile.ZipFile(io.BytesIO(docs[0][1])).namelist()) if docs else []
        check("a report's files come as one archive with the report's text",
              [d[0] for d in docs] == ["SY-670561F0.zip"] and names == ["error.log", "report.txt", "system.log"]
              and "продолжи" in zipfile.ZipFile(io.BytesIO(docs[0][1])).read("report.txt").decode())
        time.sleep(21)
        check("the same text under a new id is dropped", send_report("SY-11111111", "  ПРОДОЛЖИ ") == (200, "duplicate")
              and calls.count("sendMessage") == 1)
        codes = []
        for i in range(4):
            codes.append(send_report(f"SY-2222222{i}", f"другое {i}", ip="2.2.2.2")[0])
            time.sleep(21)
        check("one sender: 3 reports an hour", codes == [200, 200, 200, 429])
    finally:
        srv.terminate()
        srv.wait()
    out = subprocess.run([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--db", rdb, "--ban", "SY-670561F0"],
                         env=renv, capture_output=True, text=True).stdout
    check("--ban by report id", "забанен отправитель" in out)
    srv = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                            "--port", str(PORT), "--db", rdb], env=renv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    try:
        before = calls.count("sendMessage")
        res = send_report("SY-33333333", "совсем новый текст")
        other = send_report("SY-44444444", "чужой отчёт", ip="3.3.3.3")
        check("a banned sender gets 200 and nothing reaches the bot",
              res == (200, "ok") and other == (200, "ok") and calls.count("sendMessage") == before + 1)
    finally:
        srv.terminate()
        srv.wait()
        bot.shutdown()
    print("all passed" if not fails else f"{fails} failed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
