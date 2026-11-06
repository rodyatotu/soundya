# Бот приёма в сообщество soundya — быстрый запуск

Бот даёт ссылку в сообщество только после того, как человек прочитал правила и
нажал «Соглашаюсь». Работает автономно, нужен только Python 3 (внешних
зависимостей нет).

## 1. Создать бота
1. Напишите [@BotFather](https://t.me/BotFather) → `/newbot`, задайте имя и username.
2. Скопируйте выданный токен (вида `123456:ABC-...`).

## 2. (Опционально) одноразовые ссылки
Если хотите, чтобы каждому выдавалась **персональная одноразовая** ссылка, а не
общая:
1. Добавьте бота в вашу закрытую группу/канал **администратором** с правом
   «Пригласительные ссылки».
2. Узнайте id чата: временно добавьте в группу [@getidsbot](https://t.me/getidsbot)
   или посмотрите `chat.id` в логе бота — он отрицательный, вида `-100...`.

Без этого бот просто отправляет общую ссылку (`SOUNDYA_CHANNEL_INVITE`).

## 3. Запуск
```sh
export SOUNDYA_BOT_TOKEN='123456:ABC-...'            # обязательно
export SOUNDYA_CHANNEL_INVITE='https://t.me/+lvaSgqYWrXUwYzA0'   # ваша ссылка
# export SOUNDYA_TARGET_CHAT='-1001234567890'        # для одноразовых ссылок
python3 server/community-bot.py
```
Проверка: напишите боту `/start` — он пришлёт правила и кнопку «Соглашаюсь»,
а после нажатия — ссылку.

## 4. Как служба (чтобы работал всегда)
`/etc/systemd/system/soundya-bot.service`:
```ini
[Unit]
Description=soundya community bot
After=network-online.target

[Service]
Environment=SOUNDYA_BOT_TOKEN=123456:ABC-...
Environment=SOUNDYA_CHANNEL_INVITE=https://t.me/+lvaSgqYWrXUwYzA0
# Environment=SOUNDYA_TARGET_CHAT=-1001234567890
WorkingDirectory=/opt/soundya-bot
ExecStart=/usr/bin/python3 /opt/soundya-bot/community-bot.py
Restart=always

[Install]
WantedBy=multi-user.target
```
```sh
sudo mkdir -p /opt/soundya-bot && sudo cp server/community-bot.py /opt/soundya-bot/
sudo systemctl enable --now soundya-bot
sudo journalctl -u soundya-bot -f      # логи
```

## Что пишется в журнал согласий
Файл `consents.jsonl` (рядом с ботом), по строке на согласие: время (UTC),
`user_id`, username, имя, версия соглашения. При изменении правил поднимите
`AGREEMENT_VERSION` в `community-bot.py` — старые согласия перестанут считаться.

Токен и приглашение держите в окружении/юните, не в git.
