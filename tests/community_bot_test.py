#!/usr/bin/env python3
"""Tests server/community-bot.py against a fake Telegram Bot API: a join request
answered «Соглашаюсь» is approved and logged, one answered «Не соглашаюсь» is
declined, /stats works for admins and is ignored for others.

    python3 tests/community_bot_test.py

Twice: as its own program, and inside the report server (stats.py with
SOUNDYA_COMMUNITY=1 and the reports bot's SOUNDYA_TG_TOKEN).
"""
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PORT = 18950
GROUP, ADMIN, ALICE, BOB = -100500, 7, 1001, 1002

UPDATES = [
    {"update_id": 1, "chat_join_request": {"chat": {"id": GROUP}, "from": {"id": ALICE, "username": "alice", "first_name": "Алиса"}, "user_chat_id": ALICE}},
    {"update_id": 2, "callback_query": {"id": "q1", "data": "agree:1", "from": {"id": ALICE, "username": "alice"}, "message": {"chat": {"id": ALICE}}}},
    {"update_id": 3, "chat_join_request": {"chat": {"id": GROUP}, "from": {"id": BOB, "username": "bob"}, "user_chat_id": BOB}},
    {"update_id": 4, "callback_query": {"id": "q2", "data": "decline", "from": {"id": BOB, "username": "bob"}, "message": {"chat": {"id": BOB}}}},
    {"update_id": 5, "message": {"chat": {"id": BOB}, "from": {"id": BOB}, "text": "/stats"}},
    {"update_id": 6, "message": {"chat": {"id": ADMIN}, "from": {"id": ADMIN}, "text": "/stats"}},
    # «Алиса was accepted to the group» and «Bob left»: service messages, removed.
    {"update_id": 7, "message": {"message_id": 70, "chat": {"id": GROUP, "type": "supergroup"}, "from": {"id": ALICE},
                                 "new_chat_members": [{"id": ALICE, "first_name": "Алиса"}]}},
    {"update_id": 8, "message": {"message_id": 80, "chat": {"id": GROUP, "type": "supergroup"}, "from": {"id": BOB},
                                 "left_chat_member": {"id": BOB}}},
    # An ordinary message in the group stays.
    {"update_id": 9, "message": {"message_id": 90, "chat": {"id": GROUP, "type": "supergroup"}, "from": {"id": ALICE}, "text": "привет"}},
]
calls = []


class Tg(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        method = self.path.rsplit("/", 1)[-1]
        if method == "getUpdates":
            off = body.get("offset", 0)
            result = [u for u in UPDATES if u["update_id"] >= off][:1]
            if not result:
                time.sleep(0.2)
        else:
            calls.append((method, body))
            result = {"username": "soundya_test_bot"} if method == "getMe" else {"message_id": len(calls)}
        data = json.dumps({"ok": True, "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(data)


def run(embedded):
    calls.clear()
    tmp = tempfile.mkdtemp(prefix="soundya-bot-")
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Tg)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    env = {k: v for k, v in os.environ.items() if not k.startswith("SOUNDYA_")}
    env.update(SOUNDYA_TG_API=f"http://127.0.0.1:{PORT}", SOUNDYA_TARGET_CHAT=str(GROUP))
    if embedded:
        # The reports bot's settings only: its chat (the admin) and token; journals next to the db.
        env.update(SOUNDYA_TG_TOKEN="1:test", SOUNDYA_TG_CHAT=str(ADMIN), SOUNDYA_COMMUNITY="1")
        cmd = [sys.executable, os.path.join(ROOT, "server", "stats.py"), "--host", "127.0.0.1", "--port", str(PORT + 1),
               "--db", os.path.join(tmp, "stats.db")]
    else:
        env.update(SOUNDYA_BOT_TOKEN="1:test", SOUNDYA_ADMINS=str(ADMIN),
                   SOUNDYA_CONSENTS=os.path.join(tmp, "consents.jsonl"), SOUNDYA_DECLINES=os.path.join(tmp, "declines.jsonl"))
        cmd = [sys.executable, os.path.join(ROOT, "server", "community-bot.py")]
    print("inside stats.py:" if embedded else "on its own:")
    bot = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    time.sleep(4)
    answers = False
    if embedded:
        try:
            import urllib.request
            answers = urllib.request.urlopen(f"http://127.0.0.1:{PORT + 1}/badge.json", timeout=3).status == 200
        except Exception as e:
            print(f"  report server: {e}")
    bot.terminate()
    bot.wait()
    srv.shutdown()
    srv.server_close()
    fails = 0

    def check(name, ok):
        nonlocal fails
        print(("  ok   " if ok else "  FAIL ") + name)
        fails += 0 if ok else 1

    m = lambda name: [b for c, b in calls if c == name]
    check("rules with both buttons sent to the requester",
          any(b.get("chat_id") == ALICE and "Соглашаюсь" in json.dumps(b.get("reply_markup", {}), ensure_ascii=False)
              and "Не соглашаюсь" in json.dumps(b.get("reply_markup", {}), ensure_ascii=False) for b in m("sendMessage")))
    check("agreeing approves the request", {"chat_id": GROUP, "user_id": ALICE} in m("approveChatJoinRequest"))
    check("declining declines it", {"chat_id": GROUP, "user_id": BOB} in m("declineChatJoinRequest"))
    check("no approval for the one who declined", {"chat_id": GROUP, "user_id": BOB} not in m("approveChatJoinRequest"))
    consents = [json.loads(l) for l in open(os.path.join(tmp, "consents.jsonl"), encoding="utf-8")]
    check("consent logged: id, time, rules version", consents and consents[0]["user_id"] == ALICE and consents[0]["version"] == "1" and consents[0]["ts"])
    check("decline logged", os.path.exists(os.path.join(tmp, "declines.jsonl")))
    stats = [b["text"] for b in m("sendMessage") if b.get("chat_id") == ADMIN]
    check("/stats for the admin", stats and "Согласились: 1" in stats[0] and "Отказались: 1" in stats[0])
    check("/stats ignored for others", not any("Сообщество soundya\nСогласились" in b.get("text", "") for b in m("sendMessage") if b.get("chat_id") == BOB))
    check("«was accepted» and «left» removed from the group",
          {"chat_id": GROUP, "message_id": 70} in m("deleteMessage") and {"chat_id": GROUP, "message_id": 80} in m("deleteMessage"))
    check("ordinary group messages stay", {"chat_id": GROUP, "message_id": 90} not in m("deleteMessage"))
    if embedded:
        check("the report server still answers next to the bot", answers)
    return fails


def main():
    fails = run(False) + run(True)
    print("all passed" if not fails else f"{fails} failed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
