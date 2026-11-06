#!/usr/bin/env python3
"""End-to-end test of bug reports without a speaker and without real Telegram:

  web panel API (soundya daemon, host build) → queue on «flash» → server/stats.py
  → fake Telegram Bot API (this script) → message + documents received.

Also: Telegram down → report waits, survives a daemon restart, is delivered
later; a repeated delivery is not duplicated; secrets are not in the files;
a request from another site is refused.

    cargo build && python3 tests/report_e2e.py
"""
import http.server
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "debug", "soundya")
TG_PORT, STATS_PORT, WEB_PORT = 18901, 18902, 18903
SECRET = "2-424242-TopSecretToken"

calls = []  # (method, info) received by the fake Telegram
tg_up = threading.Event()


class FakeTelegram(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        method = self.path.rsplit("/", 1)[-1]
        if not tg_up.is_set():
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b'{"ok":false,"description":"down"}')
            return
        if method == "sendMessage":
            calls.append(("sendMessage", json.loads(body)["text"]))
        else:
            calls.append((method, body))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"ok": True, "result": {"message_id": len(calls)}}).encode())


def wait_port(port, t=20):
    end = time.time() + t
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", port), 0.5).close()
            return
        except OSError:
            time.sleep(0.2)
    raise SystemExit(f"port {port} did not open")


def web(path, body=None, origin=None):
    req = urllib.request.Request(f"http://127.0.0.1:{WEB_PORT}{path}", data=body.encode() if body is not None else None)
    if origin:
        req.add_header("Origin", origin)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def until(cond, t=30, what=""):
    end = time.time() + t
    while time.time() < end:
        v = cond()
        if v:
            return v
        time.sleep(0.5)
    raise AssertionError(f"timed out: {what}")


fails = 0


def check(name, ok):
    global fails
    print(("  ok   " if ok else "  FAIL ") + name)
    fails += 0 if ok else 1


def main():
    tmp = tempfile.mkdtemp(prefix="soundya-e2e-")
    state, run = os.path.join(tmp, "state"), os.path.join(tmp, "run")
    os.makedirs(state)
    os.makedirs(run)
    with open(os.path.join(state, "config.json"), "w") as f:
        json.dump({"token": SECRET, "pc_token": "PcSecretKey", "proxy": "", "stats": False, "airplay": False}, f)
    with open("/tmp/soundya.log", "a") as f:
        f.write(f"login with {SECRET}\nvoice: \"напиши маме привет\" -> Pc\n")

    tg = http.server.ThreadingHTTPServer(("127.0.0.1", TG_PORT), FakeTelegram)
    threading.Thread(target=tg.serve_forever, daemon=True).start()
    env_server = dict(os.environ, SOUNDYA_TG_TOKEN="123456:TEST", SOUNDYA_TG_CHAT="42", SOUNDYA_TG_API=f"http://127.0.0.1:{TG_PORT}")
    server = subprocess.Popen([sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1",
                               "--port", str(STATS_PORT), "--db", os.path.join(tmp, "stats.db")], env=env_server)
    env = dict(os.environ, SOUNDYA_DIR=state, SOUNDYA_RUN=run, SOUNDYA_WEB_PORT=str(WEB_PORT), SOUNDYA_NO_VOICE="1",
               SOUNDYA_REPORT_URL=f"http://127.0.0.1:{STATS_PORT}/report", SOUNDYA_NAME="soundya-e2e")
    daemon = None
    try:
        wait_port(STATS_PORT)

        def start():
            d = subprocess.Popen([BIN, "daemon"], env=env, stdout=subprocess.DEVNULL, stderr=open(os.path.join(tmp, "daemon.log"), "a"))
            wait_port(WEB_PORT)
            return d

        daemon = start()
        print("1. ready marker and version")
        ready = until(lambda: os.path.exists(os.path.join(run, "soundya.ready")) and open(os.path.join(run, "soundya.ready")).read(), 10, "ready")
        check("ready file holds version and pid", ready.split()[1] == str(daemon.pid))

        print("2. a request from another site is refused")
        code, _ = web("/api/cmd", "uninstall", origin="https://evil.example")
        check("CSRF refused with 403", code == 403)

        print("3. preview shows exactly what is sent, without secrets")
        req = {"kind": "bug", "module": "airplay", "description": "Звук пропадает", "expected": "играет", "actual": "тишина",
               "logs": True, "phrases": False, "install_id": False, "frontend_errors": ["TypeError: x at web.html:1"]}
        code, body = web("/api/report/preview", json.dumps(req))
        pv = json.loads(body)
        names = [f["name"] for f in pv["files"]]
        check("three files", names == ["system.log", "error.log", "diagnostics.json"])
        alltext = pv["message"] + "".join(f["text"] or "" for f in pv["files"])
        check("token not in the preview", SECRET not in alltext and "PcSecretKey" not in alltext)
        check("phrase hidden", "напиши маме" not in alltext)
        check("frontend error included", "TypeError: x" in alltext)
        check("message layout", "BUG REPORT #" in pv["message"] and "Module: AirPlay" in pv["message"])

        print("4. Telegram down: the report waits")
        tg_up.clear()
        code, body = web("/api/report", json.dumps(req))
        rid = json.loads(body)["id"]
        check("queued with an id", rid.startswith("SY-"))
        st = until(lambda: next((r for r in json.loads(web("/api/report/list")[1])["reports"] if r["id"] == rid and r["attempts"] > 0), None), 20, "attempt")
        check("still queued after a failed attempt", st["status"] == "queued" and st["error"])
        check("nothing reached Telegram", not calls)

        print("5. survives a restart and is delivered when Telegram is back")
        daemon.terminate()
        daemon.wait(10)
        tg_up.set()
        daemon = start()
        check("still in the list after restart", any(r["id"] == rid for r in json.loads(web("/api/report/list")[1])["reports"]))
        web("/api/report/retry", rid)
        until(lambda: any(r["id"] == rid and r["status"] == "sent" for r in json.loads(web("/api/report/list")[1])["reports"]), 30, "delivery")
        msgs = [c for c in calls if c[0] == "sendMessage"]
        docs = [c for c in calls if c[0] == "sendDocument"]
        check("one message with the report id", len(msgs) == 1 and f"BUG REPORT #{rid}" in msgs[0][1])
        check("three documents", len(docs) == 3)
        raw = b"".join(d[1] for d in docs)
        check("documents carry the logs", b"system.log" in raw and b"diagnostics.json" in raw)
        check("no secret in the documents", SECRET.encode() not in raw and b"PcSecretKey" not in raw)

        print("6. a second delivery of the same id is not duplicated")
        payload = json.dumps({"id": rid, "message": "BUG REPORT #" + rid, "files": []}).encode()
        r = urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{STATS_PORT}/report", data=payload), timeout=10)
        check("server answers already delivered", r.read() == b"already delivered")
        check("still one message", len([c for c in calls if c[0] == "sendMessage"]) == 1)

        print("7. the old «Написать автору» endpoint still works")
        time.sleep(21)  # the server allows one report per 20 s per sender
        code, body = web("/api/feedback", "Спасибо за проект")
        check("accepted", body == "ok")
        until(lambda: any(c[0] == "sendMessage" and "FEEDBACK #" in c[1] for c in calls), 30, "feedback")
        check("delivered as FEEDBACK", True)
    finally:
        if daemon:
            daemon.terminate()
        server.terminate()
        tg.shutdown()
        if fails:
            print(open(os.path.join(tmp, "daemon.log")).read()[-3000:])
        shutil.rmtree(tmp, ignore_errors=True)
    print("all passed" if not fails else f"{fails} failed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
