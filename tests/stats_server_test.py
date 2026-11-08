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
    print("all passed" if not fails else f"{fails} failed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
