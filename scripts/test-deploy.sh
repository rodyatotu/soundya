#!/bin/sh
# Заливает ТЕКУЩИЙ локальный код (без git pull и без релиза) на колонку —
# для проверки перед тем, как коммитить:
#   sh scripts/test-deploy.sh soundya.local
#   sh scripts/test-deploy.sh 192.168.0.112
#   sh scripts/test-deploy.sh 192.168.0.112 --choose   # заново спросить, что настроить
set -eu
cd "$(dirname "$0")/.."
: "${1:?usage: sh scripts/test-deploy.sh <ip-или-soundya.local> [--choose]}"
sh scripts/build-mac.sh
# SOUNDYA_FRESH=1 — не делать git pull (ставим локальный код),
# SOUNDYA_BUILT=1 — не пересобирать (берём только что собранный tar).
SOUNDYA_FRESH=1 SOUNDYA_BUILT=1 sh scripts/deploy.sh "$@"
