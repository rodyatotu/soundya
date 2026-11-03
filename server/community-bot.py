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

Запуск:  SOUNDYA_BOT_TOKEN=123:abc python3 community-bot.py
"""
import json
import os
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

TOKEN = os.environ.get("SOUNDYA_BOT_TOKEN", "").strip()
CHANNEL_INVITE = os.environ.get("SOUNDYA_CHANNEL_INVITE", "https://t.me/+lvaSgqYWrXUwYzA0").strip()
TARGET_CHAT = os.environ.get("SOUNDYA_TARGET_CHAT", "").strip()
CONSENTS = os.environ.get("SOUNDYA_CONSENTS", "consents.jsonl")

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

API = f"https://api.telegram.org/bot{TOKEN}"


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
    return {"inline_keyboard": [[{"text": "✅ Соглашаюсь", "callback_data": f"agree:{AGREEMENT_VERSION}"}]]}


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
    link = invite_link()
    api("answerCallbackQuery", callback_query_id=cq["id"], text="Спасибо! Ссылка отправлена.")
    api(
        "sendMessage",
        chat_id=chat_id,
        text=f"Согласие принято ✅\nВаша ссылка для входа:\n{link}\n\nДобро пожаловать в soundya!",
        disable_web_page_preview=True,
    )


def main():
    if not TOKEN:
        raise SystemExit("нет SOUNDYA_BOT_TOKEN: создайте бота у @BotFather и задайте токен")
    me = api("getMe")
    if me.get("ok"):
        print(f"бот @{me['result'].get('username')} запущен; соглашение версии {AGREEMENT_VERSION}")
    offset = 0
    while True:
        res = api("getUpdates", offset=offset, timeout=50, allowed_updates=["message", "callback_query"])
        if not res.get("ok"):
            time.sleep(3)
            continue
        for upd in res["result"]:
            offset = upd["update_id"] + 1
            try:
                if "message" in upd:
                    msg = upd["message"]
                    text = (msg.get("text") or "").strip()
                    if text.startswith("/start"):
                        on_start(msg["chat"]["id"])
                elif "callback_query" in upd:
                    cq = upd["callback_query"]
                    if (cq.get("data") or "").startswith("agree:"):
                        on_agree(cq)
            except Exception as e:
                print(f"update {upd.get('update_id')}: {e}")


if __name__ == "__main__":
    main()
