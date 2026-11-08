#!/bin/sh
# Updates the soundya report/statistics server on the VPS and checks that reports reach the bot.
#   sh scripts/deploy-server.sh root@195.209.211.30
# The bot's token and chat id, if the server has none yet (or to change them):
#   SOUNDYA_TG_TOKEN='123456:AA…' SOUNDYA_TG_CHAT='123456789' sh scripts/deploy-server.sh root@195.209.211.30
# One SSH connection for all steps: the password (if any) is asked once, by ssh itself.
# The token goes over that connection's stdin into /etc/soundya-stats.env (600), never on a command line.
set -eu
HOST=${1:?"использование: sh scripts/deploy-server.sh root@<адрес VPS>"}
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SOCK=$(mktemp -u /tmp/soundya-ssh.XXXXXX)
ssh -M -S "$SOCK" -o ControlPersist=120 -fN "$HOST"
trap 'ssh -S "$SOCK" -O exit "$HOST" 2>/dev/null || true' EXIT
run() { ssh -S "$SOCK" "$HOST" "$@"; }

echo "→ копирую server/ на $HOST"
run 'rm -rf /root/soundya-stats && mkdir -p /root/soundya-stats'
tar -C "$ROOT/server" -cf - . | run 'tar -C /root/soundya-stats -xf -'

if [ -n "${SOUNDYA_TG_TOKEN:-}" ] && [ -n "${SOUNDYA_TG_CHAT:-}" ]; then
    echo "→ записываю токен бота и chat id в /etc/soundya-stats.env"
    printf 'SOUNDYA_TG_TOKEN=%s\nSOUNDYA_TG_CHAT=%s\n' "$SOUNDYA_TG_TOKEN" "$SOUNDYA_TG_CHAT" |
        run 'umask 077; cat > /etc/soundya-stats.env; chown soundya-stats:soundya-stats /etc/soundya-stats.env 2>/dev/null || true'
fi
if ! run 'grep -q "^SOUNDYA_TG_TOKEN=." /etc/soundya-stats.env 2>/dev/null && grep -q "^SOUNDYA_TG_CHAT=." /etc/soundya-stats.env'; then
    echo "✗ у сервера нет токена бота: отчёты некуда пересылать."
    echo "  Запустите так: SOUNDYA_TG_TOKEN='…' SOUNDYA_TG_CHAT='…' sh scripts/deploy-server.sh $HOST"
    echo "  (токен — @BotFather → /mybots → бот → API Token; chat id — напишите @userinfobot /start; и один раз напишите своему боту)"
    exit 1
fi

echo "→ ставлю и перезапускаю службу"
run 'sh /root/soundya-stats/install.sh' | tail -3

CODE=$(run "curl -s -o /dev/null -w '%{http_code}' -X POST -d '{}' http://127.0.0.1:8787/report")
if [ "$CODE" = "404" ]; then
    echo "✗ /report всё ещё 404 — служба не перезапустилась: ssh $HOST journalctl -u soundya-stats -n 50"
    exit 1
fi
echo "✓ /report отвечает ($CODE — значит новый сервер)"

echo "→ тестовый отчёт в бота"
if run 'set -a; . /etc/soundya-stats.env; set +a; python3 /opt/soundya-stats/stats.py --db /tmp/selftest.db --selftest'; then
    echo "✓ готово: в Telegram должен прийти BUG REPORT #SY-… и три файла."
    echo "  Старые отчёты: панель колонки → Настройки → Колонка → Обратная связь и ошибки → «повторить»."
else
    echo "✗ бот не принял сообщение: проверьте токен/chat id и что вы писали боту /start"
    exit 1
fi
