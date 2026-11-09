#!/usr/bin/env python3
"""Бот приёма в сообщество soundya (раздел 18 релиза 0.5.00).

Новый участник должен прочитать правила и явно нажать «Соглашаюсь», и только
после этого получает ссылку-приглашение. Работает автономно, на одной стандартной
библиотеке Python (long-polling, без внешних зависимостей), рядом со stats.py.

Настройка — через переменные окружения (ничего не хардкодим):
  SOUNDYA_BOT_TOKEN      токен бота от @BotFather (обязательно)
  SOUNDYA_CHANNEL_INVITE пригласительная ссылка в канал/группу (по умолчанию — наша)
  SOUNDYA_TARGET_CHAT    id закрытой группы/канала; если задан и бот там админ,
                         каждому выдаётся ОДНОРАЗОВАЯ ссылка (member_limit=1),
                         иначе отправляется SOUNDYA_CHANNEL_INVITE
  SOUNDYA_CONSENTS       файл журнала согласий (по умолчанию ./consents.jsonl)
  SOUNDYA_DECLINES       файл журнала отказов (по умолчанию ./declines.jsonl)
  SOUNDYA_ADMINS         Telegram ID администраторов через запятую: /stats, /ban, /unban

Заявки на вступление (0.5.00). Сделайте в группе/канале пригласительную ссылку
с «Одобрением администратором» (creates_join_request) и укажите её в
SOUNDYA_CHANNEL_INVITE: тогда по ней в группу попасть нельзя, пока бот не
одобрит заявку. Бот (админ группы с правом «Добавлять участников») получает
заявку, пишет человеку в личку правила с кнопками «Соглашаюсь» / «Не соглашаюсь»
и одобряет или отклоняет заявку. Согласие пишется в журнал: Telegram ID,
username, имя, время (UTC), версия правил.

Запуск:  SOUNDYA_BOT_TOKEN=123:abc python3 community-bot.py

Или внутри сервера отчётов (stats.py) тем же ботом, что присылает баг-репорты:
SOUNDYA_COMMUNITY=1 в /etc/soundya-stats.env — stats.py запустит main() в своём потоке
с токеном SOUNDYA_TG_TOKEN. Тогда SOUNDYA_BOT_TOKEN не нужен.
"""
import json
import os
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

# Свой бот или бот отчётов (stats.py): один и тот же токен можно использовать для обоих.
TOKEN = (os.environ.get("SOUNDYA_BOT_TOKEN", "") or os.environ.get("SOUNDYA_TG_TOKEN", "")).strip()
CHANNEL_INVITE = os.environ.get("SOUNDYA_CHANNEL_INVITE", "https://t.me/+ftjTac-KQkgxY2E0").strip()
TARGET_CHAT = os.environ.get("SOUNDYA_TARGET_CHAT", "").strip()
CONSENTS = os.environ.get("SOUNDYA_CONSENTS", "consents.jsonl")
DECLINES = os.environ.get("SOUNDYA_DECLINES", "declines.jsonl")
# Delete «X joined / was accepted / left» service messages in the group (the bot needs the
# admin right «Удаление сообщений»). On by default; SOUNDYA_HIDE_JOINS=0 keeps them.
HIDE_JOINS = os.environ.get("SOUNDYA_HIDE_JOINS", "1").strip() not in ("0", "no", "false")
SERVICE_KEYS = ("new_chat_members", "left_chat_member")
ADMINS = {int(x) for x in os.environ.get("SOUNDYA_ADMINS", "").replace(" ", "").split(",") if x.lstrip("-").isdigit()}
# Без SOUNDYA_ADMINS админ — тот, кому бот отчётов шлёт баг-репорты (если это человек, а не группа).
if not ADMINS and os.environ.get("SOUNDYA_TG_CHAT", "").strip().isdigit():
    ADMINS = {int(os.environ["SOUNDYA_TG_CHAT"].strip())}
# Заявки, ждущие ответа: user_id -> chat_id группы (в памяти; Telegram хранит заявки сам).
PENDING = {}

# Версия соглашения. Поднимите её при существенном изменении правил —
# согласие старой версии перестанет считаться действующим.
AGREEMENT_VERSION = "1"

RULES = (
    "<b>Сообщество soundya</b>\n\n"
    "soundya — независимый пользовательский проект. Это <b>не</b> официальный продукт "
    "Яндекса: не разработан, не одобрен и не поддерживается им. Яндекс, «Алиса» и "
    "связанные товарные знаки принадлежат их правообладателям.\n\n"
    "Сообщество — про легальное использование soundya: настройку, совместимость, "
    "автоматизацию и обмен опытом.\n\n"
    "<b>Правила:</b>\n"
    "• Запрещено обсуждать взлом колонок и обход подписочных ограничений Яндекса.\n"
    "• Запрещены инструкции по несанкционированному изменению прошивок, обходу защиты "
    "и доступу к чужим устройствам.\n"
    "• Запрещено распространять вредоносное ПО.\n"
    "• Запрещены угрозы, оскорбления, спам и чужие персональные данные.\n"
    "• Нельзя выдавать себя за представителей Яндекса или soundya.\n"
    "• Администрация вправе удалять сообщения и ограничивать доступ нарушителям.\n\n"
    "Вы сами отвечаете за свои действия и соблюдение применимого законодательства. "
    "Согласие с правилами не отменяет ответственности сторон, предусмотренной законом.\n\n"
    "Нажимая «Соглашаюсь», вы подтверждаете, что прочитали правила и принимаете их."
)

WELCOME = "Привет! Это бот приёма в сообщество soundya. Прочитайте правила ниже и подтвердите согласие."

API = os.environ.get("SOUNDYA_TG_API", "https://api.telegram.org").rstrip("/") + f"/bot{TOKEN}"


def api(method, **params):
    data = json.dumps(params).encode("utf-8")
    req = urllib.request.Request(f"{API}/{method}", data=data, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        print(f"{method}: HTTP {e.code}: {body}")
        try:
            return json.loads(body)
        except Exception:
            return {"ok": False}
    except Exception as e:
        print(f"{method}: {e}")
        return {"ok": False}


def agree_keyboard():
    return {"inline_keyboard": [[
        {"text": "✅ Соглашаюсь", "callback_data": f"agree:{AGREEMENT_VERSION}"},
        {"text": "✖️ Не соглашаюсь", "callback_data": "decline"},
    ]]}


def append(path, row):
    try:
        with open(path, "a", encoding="utf-8") as f:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
    except Exception as e:
        print(f"journal {path}: {e}")


def read(path):
    try:
        with open(path, encoding="utf-8") as f:
            return [json.loads(l) for l in f if l.strip()]
    except FileNotFoundError:
        return []


def log_consent(user):
    row = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "user_id": user.get("id"),
        "username": user.get("username"),
        "name": " ".join(filter(None, [user.get("first_name"), user.get("last_name")])),
        "version": AGREEMENT_VERSION,
    }
    try:
        with open(CONSENTS, "a", encoding="utf-8") as f:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
    except Exception as e:
        print(f"consent log: {e}")
    return row


def invite_link():
    """Одноразовая ссылка, если задан TARGET_CHAT и бот там админ; иначе общая."""
    if TARGET_CHAT:
        res = api(
            "createChatInviteLink",
            chat_id=_chat_id(TARGET_CHAT),
            member_limit=1,
            expire_date=int(time.time()) + 3600,
            name="soundya consent",
        )
        if res.get("ok"):
            return res["result"]["invite_link"]
        print("createChatInviteLink не удался — отправляю общую ссылку")
    return CHANNEL_INVITE


def _chat_id(v):
    return int(v) if v.lstrip("-").isdigit() else v


def on_start(chat_id):
    api("sendMessage", chat_id=chat_id, text=WELCOME)
    api("sendMessage", chat_id=chat_id, text=RULES, parse_mode="HTML", reply_markup=agree_keyboard())


def on_agree(cq):
    user = cq["from"]
    chat_id = cq["message"]["chat"]["id"]
    version = cq["data"].split(":", 1)[1] if ":" in cq["data"] else ""
    if version != AGREEMENT_VERSION:
        api("answerCallbackQuery", callback_query_id=cq["id"], text="Правила обновились — откройте бота заново: /start", show_alert=True)
        return
    log_consent(user)
    group = PENDING.pop(user["id"], None)
    if group is not None:
        # A join request is waiting: approve it — the user is in, no link needed.
        res = api("approveChatJoinRequest", chat_id=group, user_id=user["id"])
        api("answerCallbackQuery", callback_query_id=cq["id"], text="Спасибо! Заявка одобрена.")
        text = "Согласие принято ✅\nЗаявка одобрена — добро пожаловать в soundya!" if res.get("ok") else \
            "Согласие принято ✅, но одобрить заявку не вышло (возможно, она устарела). Отправьте заявку ещё раз."
        api("sendMessage", chat_id=chat_id, text=text)
        return
    link = invite_link()
    api("answerCallbackQuery", callback_query_id=cq["id"], text="Спасибо! Ссылка отправлена.")
    api(
        "sendMessage",
        chat_id=chat_id,
        text=f"Согласие принято ✅\nВаша ссылка для входа:\n{link}\n\nДобро пожаловать в soundya!",
        disable_web_page_preview=True,
    )


def on_decline(cq):
    user = cq["from"]
    append(DECLINES, {"ts": datetime.now(timezone.utc).isoformat(), "user_id": user.get("id"),
                      "username": user.get("username"), "version": AGREEMENT_VERSION})
    group = PENDING.pop(user["id"], None)
    if group is not None:
        api("declineChatJoinRequest", chat_id=group, user_id=user["id"])
    api("answerCallbackQuery", callback_query_id=cq["id"], text="Понятно.")
    api("sendMessage", chat_id=cq["message"]["chat"]["id"],
        text="Без согласия с правилами доступа в сообщество нет. Передумаете — нажмите /start.")


def on_join_request(req):
    """Someone asked to join through the request link: rules first."""
    user = req["from"]
    PENDING[user["id"]] = req["chat"]["id"]
    # user_chat_id: the bot may write to them for 5 minutes after the request.
    dm = req.get("user_chat_id", user["id"])
    api("sendMessage", chat_id=dm, text=WELCOME)
    api("sendMessage", chat_id=dm, text=RULES, parse_mode="HTML", reply_markup=agree_keyboard())


def stats_text():
    consents, declines = read(CONSENTS), read(DECLINES)
    now = datetime.now(timezone.utc)
    week = [c for c in consents if (now - datetime.fromisoformat(c["ts"])).days < 7]
    users = {c.get("user_id") for c in consents}
    current = {c.get("user_id") for c in consents if c.get("version") == AGREEMENT_VERSION}
    by_version = {}
    for c in consents:
        by_version[c.get("version")] = by_version.get(c.get("version"), 0) + 1
    versions = ", ".join(f"v{k}: {v}" for k, v in sorted(by_version.items(), key=lambda kv: str(kv[0]))) or "—"
    return (f"📊 Сообщество soundya\n"
            f"Согласились: {len(users)} (с текущими правилами v{AGREEMENT_VERSION}: {len(current)})\n"
            f"За 7 дней: {len(week)}\nОтказались: {len({d.get('user_id') for d in declines})}\n"
            f"Ждут ответа на заявку: {len(PENDING)}\nПо версиям правил: {versions}")


def on_admin(msg, text):
    chat = msg["chat"]["id"]
    if msg["from"]["id"] not in ADMINS:
        return False
    cmd, _, arg = text.partition(" ")
    if cmd == "/stats":
        api("sendMessage", chat_id=chat, text=stats_text())
    elif cmd in ("/ban", "/unban"):
        if not TARGET_CHAT or not arg.strip().lstrip("-").isdigit():
            api("sendMessage", chat_id=chat, text=f"{cmd} <Telegram ID> (нужен SOUNDYA_TARGET_CHAT)")
        else:
            method = "banChatMember" if cmd == "/ban" else "unbanChatMember"
            res = api(method, chat_id=_chat_id(TARGET_CHAT), user_id=int(arg.strip()))
            api("sendMessage", chat_id=chat, text=("✅ " if res.get("ok") else "❌ ") + f"{cmd} {arg.strip()}: {res.get('description', 'готово')}")
    else:
        return False
    return True


def is_join_notice(msg):
    """«Rodion was accepted to the group», «X added Y», «X left the group»: in a group, not a DM."""
    return msg.get("chat", {}).get("type") in ("group", "supergroup") and any(k in msg for k in SERVICE_KEYS)


def hide(msg):
    res = api("deleteMessage", chat_id=msg["chat"]["id"], message_id=msg["message_id"])
    if not res.get("ok"):
        print(f"не удалось убрать служебное сообщение (у бота нет права «Удаление сообщений»?): {res.get('description', '')}")


def main():
    if not TOKEN:
        raise SystemExit("нет SOUNDYA_BOT_TOKEN (или SOUNDYA_TG_TOKEN): создайте бота у @BotFather и задайте токен")
    # A webhook on the token makes getUpdates fail (409) forever; the reports bot uses none.
    api("deleteWebhook")
    me = api("getMe")
    if me.get("ok"):
        print(f"бот @{me['result'].get('username')} запущен; соглашение версии {AGREEMENT_VERSION}")
    offset = 0
    while True:
        res = api("getUpdates", offset=offset, timeout=50, allowed_updates=["message", "callback_query", "chat_join_request"])
        if not res.get("ok"):
            time.sleep(3)
            continue
        for upd in res["result"]:
            offset = upd["update_id"] + 1
            try:
                if "message" in upd and HIDE_JOINS and is_join_notice(upd["message"]):
                    hide(upd["message"])
                elif "message" in upd:
                    msg = upd["message"]
                    text = (msg.get("text") or "").strip()
                    if text.startswith("/") and on_admin(msg, text):
                        pass
                    elif text.startswith("/start"):
                        on_start(msg["chat"]["id"])
                elif "chat_join_request" in upd:
                    on_join_request(upd["chat_join_request"])
                elif "callback_query" in upd:
                    cq = upd["callback_query"]
                    data = cq.get("data") or ""
                    if data.startswith("agree:"):
                        on_agree(cq)
                    elif data == "decline":
                        on_decline(cq)
            except Exception as e:
                print(f"update {upd.get('update_id')}: {e}")


if __name__ == "__main__":
    main()
