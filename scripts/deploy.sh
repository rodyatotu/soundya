#!/bin/sh
# Installs (or updates) soundya on the speaker:
#   sh scripts/deploy.sh soundya.local      (once soundya runs on it)
#   sh scripts/deploy.sh 192.168.0.112      (first install, by IP)
#
# Survives a flaky Wi-Fi: the upload is retried, and the installer runs on the
# speaker on its own, so a dropped adb connection cannot leave it half-done.
set -u
cd "$(dirname "$0")/.."
IP=${1:?usage: scripts/deploy.sh <speaker-ip>}
DEV="$IP:5555"
TAR=dist/soundya-armv7.tar
[ -f "$TAR" ] || { echo "no $TAR: run sh scripts/build-mac.sh first" >&2; exit 1; }
SIZE=$(wc -c < "$TAR" | tr -d ' ')

connect() {
    adb disconnect "$DEV" >/dev/null 2>&1
    adb connect "$DEV" 2>&1 | grep -q "connected to" && adb -s "$DEV" shell true >/dev/null 2>&1
}

retry() {
    i=1
    while [ "$i" -le 6 ]; do
        connect && "$@" && return 0
        echo "  связь с колонкой оборвалась, попытка $((i + 1)) из 6…" >&2
        i=$((i + 1))
        sleep 3
    done
    return 1
}

upload() {
    adb -s "$DEV" push "$TAR" /tmp/soundya-armv7.tar >/dev/null &&
        [ "$(adb -s "$DEV" shell "wc -c < /tmp/soundya-armv7.tar" | tr -d ' \r')" = "$SIZE" ]
}

start_install() {
    adb -s "$DEV" shell "cd /tmp && rm -rf soundya /tmp/soundya-install.done && tar -xf soundya-armv7.tar && \
        (setsid sh -c 'sh /tmp/soundya/install.sh > /tmp/soundya-install.log 2>&1; touch /tmp/soundya-install.done' \
        </dev/null >/dev/null 2>&1 &)"
}

installed() {
    LOGTEXT=$(adb -s "$DEV" shell "[ -f /tmp/soundya-install.done ] && cat /tmp/soundya-install.log" 2>/dev/null)
    printf '%s' "$LOGTEXT" | grep -q "Installed"
}

echo "Загружаю на колонку $IP…"
retry upload || { echo "Не удалось загрузить: колонка недоступна по сети." >&2; exit 1; }
echo "Устанавливаю…"
retry start_install || { echo "Не удалось запустить установку." >&2; exit 1; }
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    sleep 2
    connect
    if installed; then
        printf '%s\n' "$LOGTEXT"
        exit 0
    fi
done
echo "Установка запущена, но её завершения дождаться не удалось. Проверьте: adb shell cat /tmp/soundya-install.log" >&2
exit 1
