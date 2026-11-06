#!/bin/bash
# soundya: установка на Яндекс Станцию с Mac в один клик.
# Рядом с этим файлом должен лежать soundya-armv7.tar. Колонка уже с root и
# сетевым ADB (README, шаг 1). Двойной щелчок — дальше всё в окнах.
# Если macOS не даёт открыть: правой кнопкой → «Открыть» → «Открыть».
cd "$(dirname "$0")" || exit 1
HERE=$(pwd)
TAR="$HERE/soundya-armv7.tar"
TOOLS="$HOME/Library/Application Support/soundya/platform-tools"
LOG="$HOME/Library/Logs/soundya-install.log"
mkdir -p "$(dirname "$LOG")" && : > "$LOG"
log() { echo "$(date '+%H:%M:%S') $*" | tee -a "$LOG"; }

say_dialog() { # text [buttons…] → нажатая кнопка
    local text=$1; shift
    local buttons='{"OK"}'
    [ $# -gt 0 ] && buttons="{$(printf '"%s",' "$@" | sed 's/,$//')}"
    osascript -e "button returned of (display dialog \"$text\" with title \"soundya\" buttons $buttons default button 1 with icon note)" 2>/dev/null
}
fail() {
    log "ОШИБКА: $1"
    osascript -e "display dialog \"$1\n\nПодробности: $LOG\" with title \"soundya\" buttons {\"Закрыть\"} default button 1 with icon stop" >/dev/null 2>&1
    exit 1
}
note() { log "$1"; osascript -e "display notification \"$1\" with title \"soundya\"" >/dev/null 2>&1; }

[ -f "$TAR" ] || fail "Рядом с установщиком нет файла soundya-armv7.tar. Распакуйте архив целиком и запустите снова."

[ "$(say_dialog "Установить soundya на Яндекс Станцию?\n\nКолонка должна быть с root и в той же Wi-Fi сети, что и этот Mac. Настройки, вход SoundCloud и ключи сохранятся: перед установкой делается копия, а если новая версия не запустится — вернётся прежняя." "Установить" "Отмена")" = "Установить" ] || exit 0

# ---- adb ----
ADB=$(command -v adb)
if [ -z "$ADB" ] && [ -x "$TOOLS/adb" ]; then ADB="$TOOLS/adb"; fi
if [ -z "$ADB" ]; then
    note "Скачиваю adb от Google…"
    mkdir -p "$(dirname "$TOOLS")"
    TMPZ=$(mktemp -t platform-tools).zip
    curl -fsSL -o "$TMPZ" https://dl.google.com/android/repository/platform-tools-latest-darwin.zip \
        || fail "Не удалось скачать adb (нет интернета?)."
    (cd "$(dirname "$TOOLS")" && unzip -oq "$TMPZ") || fail "Не удалось распаковать adb."
    rm -f "$TMPZ"
    xattr -dr com.apple.quarantine "$TOOLS" 2>/dev/null
    ADB="$TOOLS/adb"
fi
log "adb: $ADB"
"$ADB" start-server >>"$LOG" 2>&1

# ---- поиск колонки ----
is_station() { # ip → 0, если это Яндекс Станция с root
    "$ADB" connect "$1:5555" 2>&1 | grep -q "connected to" || return 1
    local h
    # root, и это колонка: каталог Алисы на месте (имя хоста у моделей разное).
    h=$("$ADB" -s "$1:5555" shell "id -u; [ -d /system/vendor/quasar ] && echo quasar; cat /proc/sys/kernel/hostname" 2>/dev/null | tr -d '\r')
    [ "$(echo "$h" | head -1)" = "0" ] && echo "$h" | grep -qi "quasar\|yandex"
}
note "Ищу колонку в сети…"
FOUND=()
for host in soundya.local; do
    ip=$(dscacheutil -q host -a name "$host" 2>/dev/null | awk '/ip_address/ {print $2; exit}')
    [ -n "$ip" ] && is_station "$ip" && FOUND+=("$ip")
done
if [ ${#FOUND[@]} -eq 0 ]; then
    MY=""
    for ifc in en0 en1 en2 en3 en4; do MY=$(ipconfig getifaddr "$ifc" 2>/dev/null) && [ -n "$MY" ] && break; done
    if [ -n "$MY" ]; then
        NET=${MY%.*}
        CANDS=$(for i in $(seq 1 254); do
            ( nc -z -G 1 "$NET.$i" 5555 >/dev/null 2>&1 && echo "$NET.$i" ) &
        done; wait)
        for ip in $CANDS; do is_station "$ip" && FOUND+=("$ip"); done
    fi
fi
log "найдено: ${FOUND[*]:-ничего}"
if [ ${#FOUND[@]} -eq 1 ]; then
    IP=${FOUND[0]}
elif [ ${#FOUND[@]} -gt 1 ]; then
    LIST=$(printf '"%s",' "${FOUND[@]}" | sed 's/,$//')
    IP=$(osascript -e "choose from list {$LIST} with title \"soundya\" with prompt \"Нашлось несколько колонок. На какую ставить?\"" 2>/dev/null)
    [ "$IP" = "false" ] || [ -z "$IP" ] && exit 0
else
    IP=$(osascript -e 'text returned of (display dialog "Колонка не нашлась сама. Впишите её IP-адрес (в приложении роутера, список устройств):" with title "soundya" default answer "192.168." buttons {"Отмена", "Дальше"} default button 2)' 2>/dev/null) || exit 0
    IP=$(echo "$IP" | tr -d ' ')
    is_station "$IP" || fail "По адресу $IP нет колонки с root и ADB по сети.\n\nПроверьте: колонка включена, в той же сети, root получен (README, шаг 1)."
fi
DEV="$IP:5555"
log "колонка: $DEV"

# ---- установка ----
note "Загружаю soundya на колонку ($IP)…"
SIZE=$(wc -c < "$TAR" | tr -d ' ')
ok=0
for try in 1 2 3 4 5; do
    "$ADB" connect "$DEV" >/dev/null 2>&1
    "$ADB" -s "$DEV" push "$TAR" /tmp/soundya-armv7.tar >>"$LOG" 2>&1 \
        && [ "$("$ADB" -s "$DEV" shell 'wc -c < /tmp/soundya-armv7.tar' 2>/dev/null | tr -d ' \r')" = "$SIZE" ] && { ok=1; break; }
    log "загрузка оборвалась, попытка $((try + 1))"; sleep 3
done
[ $ok = 1 ] || fail "Не удалось загрузить файл на колонку: связь всё время обрывается."

note "Устанавливаю… (до двух минут, музыка прервётся)"
# Установщик работает на колонке сам: обрыв adb ему не помешает.
"$ADB" -s "$DEV" shell "cd /tmp && rm -rf /tmp/soundya /tmp/soundya-install.done && tar -xf soundya-armv7.tar && \
    trap '' HUP; (trap '' HUP; nohup setsid sh -c 'sh /tmp/soundya/install.sh > /tmp/soundya-install.log 2>&1; touch /tmp/soundya-install.done' </dev/null >/dev/null 2>&1 &); \
    i=0; while [ \$i -lt 10 ] && [ ! -e /tmp/soundya-install.log ]; do sleep 1; i=\$((i+1)); done" >>"$LOG" 2>&1
for i in $(seq 1 90); do
    sleep 2
    "$ADB" connect "$DEV" >/dev/null 2>&1
    "$ADB" -s "$DEV" shell '[ -f /tmp/soundya-install.done ] && echo done' 2>/dev/null | grep -q done && break
done
"$ADB" -s "$DEV" shell 'cat /tmp/soundya-install.log' >>"$LOG" 2>&1
RES=$("$ADB" -s "$DEV" shell 'cat /data/opt/soundya/update-result 2>/dev/null' | tr -d '\r')
STATUS=$(echo "$RES" | sed -n 's/^status=//p'); REASON=$(echo "$RES" | sed -n 's/^reason=//p'); TO=$(echo "$RES" | sed -n 's/^to=//p')
log "итог: ${STATUS:-нет} ${REASON}"

URL="http://$IP:8765/"
case "$STATUS" in
    ok)
        [ "$(say_dialog "Готово! soundya $TO установлена.\n\nПанель: $URL\n(или http://soundya.local:8765)\n\nСкажите: «Алиса, повтори адрес» — она напомнит адрес." "Открыть панель" "Закрыть")" = "Открыть панель" ] && open "$URL" ;;
    rolledback)
        fail "Новая версия не запустилась, колонка вернула прежнюю — всё работает как раньше.\nПричина: $REASON" ;;
    *)
        fail "Установка не завершилась как надо (${STATUS:-нет итога}). ${REASON}" ;;
esac
