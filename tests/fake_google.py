#!/usr/bin/env python3
"""A fake Google for testing soundya's planner without a Google account:
OAuth (authorization code with PKCE, refresh), Calendar API (calendarList,
events list/insert) and Tasks API (lists, tasks list/insert/patch/delete).
Point the daemon at it with SOUNDYA_GOOGLE_AUTH / _OAUTH / _API =
http://127.0.0.1:<port>. Writes are appended to <dir>/writes.jsonl.

    python3 tests/fake_google.py <port> <dir>

Data is made relative to now in UTC+3 (the speaker's default offset):
an event in 40 minutes and one at 20:00 today, a whole-day event tomorrow,
a task due today «до HH:MM» 20 minutes from now, an overdue task, one without a date.
"""
import base64
import datetime as dt
import hashlib
import http.server
import json
import os
import sys
import urllib.parse

PORT = int(sys.argv[1])
OUT = sys.argv[2]
os.makedirs(OUT, exist_ok=True)
CLIENT_ID = "123-test.apps.googleusercontent.com"
SECRET = "GOCSPX-test-secret"
TZ = dt.timezone(dt.timedelta(hours=3))
now = dt.datetime.now(TZ).replace(second=0, microsecond=0)
today = now.date()

codes = {}  # code -> challenge
tokens = {"refresh": "1//refresh-token-xyz", "access": set()}
counter = {"n": 0, "refresh_grants": 0}


def iso(t):
    return t.isoformat()


soon = now + dt.timedelta(minutes=40)
warn_at = now + dt.timedelta(minutes=20)
calendars = [
    {"id": "me@gmail.com", "summary": "me@gmail.com", "summaryOverride": "Личный", "primary": True, "accessRole": "owner", "selected": True},
    {"id": "ru.russian#holiday@group.v.calendar.google.com", "summary": "Праздники", "accessRole": "reader", "selected": True},
]
events = {
    "me@gmail.com": [
        {"id": "e1", "summary": "Созвон с командой", "start": {"dateTime": iso(soon)}, "end": {"dateTime": iso(soon + dt.timedelta(minutes=30))}},
        {"id": "e2", "summary": "Спортзал", "start": {"dateTime": iso(now.replace(hour=20, minute=0))}, "end": {"dateTime": iso(now.replace(hour=21, minute=0))}},
        {"id": "e3", "summary": "Отменили", "status": "cancelled", "start": {"dateTime": iso(soon)}},
    ],
    "ru.russian#holiday@group.v.calendar.google.com": [
        {"id": "h1", "summary": "Праздник", "start": {"date": str(today + dt.timedelta(days=1))}, "end": {"date": str(today + dt.timedelta(days=2))}},
    ],
}
lists = [{"id": "MDE", "title": "Мои задачи"}]
tasks = {
    "MDE": [
        {"id": "t1", "title": "Сдать отчёт", "notes": f"до {warn_at.hour}:{warn_at.minute:02d}", "due": f"{today}T00:00:00.000Z", "status": "needsAction"},
        {"id": "t2", "title": "Позвонить в банк", "due": f"{today - dt.timedelta(days=2)}T00:00:00.000Z", "status": "needsAction"},
        {"id": "t3", "title": "Прочитать книгу", "status": "needsAction"},
    ]
}


def log(kind, **kw):
    with open(os.path.join(OUT, "writes.jsonl"), "a") as f:
        f.write(json.dumps({"kind": kind, **kw}, ensure_ascii=False) + "\n")


def b64url(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, obj=None, headers=None):
        body = b"" if obj is None else json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if obj is not None:
            self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n).decode() if n else ""

    def authed(self):
        a = self.headers.get("Authorization", "")
        if a.startswith("Bearer ") and a[7:] in tokens["access"]:
            return True
        self.send(401, {"error": {"code": 401, "status": "UNAUTHENTICATED", "message": "Invalid Credentials"}})
        return False

    def new_access(self):
        counter["n"] += 1
        t = f"ya29.access-{counter['n']}"
        tokens["access"].add(t)
        return t

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(u.query))
        path = urllib.parse.unquote(u.path)
        if path == "/o/oauth2/v2/auth":
            assert q["client_id"] == CLIENT_ID, q
            assert q["code_challenge_method"] == "S256" and q["access_type"] == "offline", q
            assert "calendar.events" in q["scope"] and "auth/tasks" in q["scope"], q
            code = "4/0Afake-" + str(len(codes))
            codes[code] = q["code_challenge"]
            loc = q["redirect_uri"] + "?" + urllib.parse.urlencode({"state": q["state"], "code": code, "scope": q["scope"]})
            return self.send(302, None, {"Location": loc})
        if not self.authed():
            return
        if path == "/calendar/v3/users/me/calendarList":
            return self.send(200, {"items": calendars})
        if path.startswith("/calendar/v3/calendars/") and path.endswith("/events"):
            cal = path[len("/calendar/v3/calendars/"):-len("/events")]
            assert q.get("singleEvents") == "true", q
            return self.send(200, {"items": events.get(cal, [])})
        if path == "/tasks/v1/users/@me/lists":
            return self.send(200, {"items": lists})
        if path.startswith("/tasks/v1/lists/") and path.endswith("/tasks"):
            lid = path.split("/")[4]
            return self.send(200, {"items": [t for t in tasks.get(lid, []) if t["status"] != "completed"]})
        self.send(404, {"error": {"message": "no " + path}})

    def do_POST(self):
        u = urllib.parse.urlparse(self.path)
        path = urllib.parse.unquote(u.path)
        if path == "/token":
            f = dict(urllib.parse.parse_qsl(self.body()))
            if f.get("client_id") != CLIENT_ID or f.get("client_secret") != SECRET:
                return self.send(401, {"error": "invalid_client", "error_description": "The OAuth client was not found."})
            if f["grant_type"] == "authorization_code":
                ch = codes.pop(f.get("code", ""), None)
                if ch is None or b64url(hashlib.sha256(f.get("code_verifier", "").encode()).digest()) != ch:
                    return self.send(400, {"error": "invalid_grant", "error_description": "Bad code or verifier"})
                log("signed_in")
                # A short-lived key, so the speaker has to renew it during the test.
                return self.send(200, {"access_token": self.new_access(), "expires_in": 130, "refresh_token": tokens["refresh"], "token_type": "Bearer"})
            if f["grant_type"] == "refresh_token":
                if f.get("refresh_token") != tokens["refresh"]:
                    return self.send(400, {"error": "invalid_grant", "error_description": "Token has been expired or revoked."})
                counter["refresh_grants"] += 1
                log("refreshed", n=counter["refresh_grants"])
                return self.send(200, {"access_token": self.new_access(), "expires_in": 3599, "token_type": "Bearer"})
            return self.send(400, {"error": "unsupported_grant_type"})
        if not self.authed():
            return
        body = json.loads(self.body() or "{}")
        if path.startswith("/calendar/v3/calendars/") and path.endswith("/events"):
            cal = path[len("/calendar/v3/calendars/"):-len("/events")]
            cal = "me@gmail.com" if cal == "primary" else cal
            body["id"] = "new-e" + str(len(events.get(cal, [])))
            events.setdefault(cal, []).append(body)
            log("event", cal=cal, body=body)
            return self.send(200, body)
        if path.startswith("/tasks/v1/lists/") and path.endswith("/tasks"):
            lid = path.split("/")[4]
            body["id"] = "new-t" + str(len(tasks.get(lid, [])))
            body["status"] = "needsAction"
            tasks.setdefault(lid, []).append(body)
            log("task", list=lid, body=body)
            return self.send(200, body)
        self.send(404, {"error": {"message": "no " + path}})

    def do_PATCH(self):
        if not self.authed():
            return
        path = urllib.parse.unquote(urllib.parse.urlparse(self.path).path)
        parts = path.split("/")
        lid, tid = parts[4], parts[6]
        body = json.loads(self.body() or "{}")
        for t in tasks.get(lid, []):
            if t["id"] == tid:
                t.update(body)
                log("patch", list=lid, id=tid, body=body)
                return self.send(200, t)
        self.send(404, {"error": {"message": "no task"}})

    def do_DELETE(self):
        if not self.authed():
            return
        parts = urllib.parse.unquote(urllib.parse.urlparse(self.path).path).split("/")
        lid, tid = parts[4], parts[6]
        tasks[lid] = [t for t in tasks.get(lid, []) if t["id"] != tid]
        log("delete", list=lid, id=tid)
        self.send(204)


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
