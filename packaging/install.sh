#!/bin/sh
# Installs or updates soundya. Run on the speaker as root:  sh /tmp/soundya/install.sh
#
# Since 0.5.00 an update is safe to interrupt and to fail:
#  1. the package is checked (SHA256SUMS) and the new binary must start on this
#     speaker before anything is touched;
#  2. the installed program is copied to backup/prev (one rollback slot) and the
#     user's data (settings, SoundCloud login, keys, history…) to
#     backup/data-<time>-<version> (last 5 kept);
#  3. only program files are replaced — config.json, client_id, adbkey,
#     history.json, install_id, soundya.env and every other data file are never
#     overwritten;
#  4. the new soundya must report itself ready and stay up; otherwise the
#     previous version and the data as they were before the update come back.
# The outcome goes to $DIR/update-result (shown on the settings page).
set -eu
SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# Overridable only for tests/install-test.sh.
DIR=${SOUNDYA_INSTALL_DIR:-/data/opt/soundya}
INIT=${SOUNDYA_INIT:-/etc/init.d/S99soundya}
RUN=${SOUNDYA_RUN:-/run}
BK=$DIR/backup
RESULT=$DIR/update-result
READY=$RUN/soundya.ready
# Files that belong to the program (replaced on update); everything else in $DIR is user data.
PROGRAM="soundya run.sh guard.sh uninstall.sh nfqws nfqws2 shairport-sync"
WAIT=${SOUNDYA_HEALTH_WAIT:-90}
# How long the new daemon must stay up after saying it is ready.
HOLD=${SOUNDYA_HEALTH_HOLD:-10}
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

version_of() { # the version a soundya binary prints first: «soundya 0.4.40»
    [ -x "$1" ] || return 0
    "$1" version 2>/dev/null | sed -n '1s/^soundya \([0-9][0-9A-Za-z.+-]*\).*/\1/p'
}
OLD_VER=$(version_of "$DIR/soundya")
NEW_VER=$(cat "$SRC/VERSION" 2>/dev/null || true)
[ -n "$NEW_VER" ] || NEW_VER=$(version_of "$SRC/soundya")

result() { # status reason
    {
        echo "status=$1"
        echo "from=$OLD_VER"
        echo "to=$NEW_VER"
        echo "time=$(date +%s)"
        echo "reason=$2"
    } > "$RESULT.tmp" 2>/dev/null && mv "$RESULT.tmp" "$RESULT" 2>/dev/null || true
    sync
    echo "update: $1${2:+: $2}"
}
fail() { result failed "$1"; exit 1; }

RO=0
rootfs_rw() { RO=0; [ -z "${SOUNDYA_INSTALL_DIR:-}" ] || return 0; if grep -q ' / [^ ]* ro[ ,]' /proc/mounts; then RO=1; mount -o remount,rw /; fi; }
rootfs_back() { sync; [ "$RO" -eq 0 ] || mount -o remount,ro /; }

# ---- 1. The package: complete, undamaged and runnable here ----
mkdir -p "$DIR"
if [ -f "$SRC/SHA256SUMS" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$SRC" && sha256sum -c SHA256SUMS >/tmp/soundya-sha.log 2>&1) \
            || fail "архив повреждён: $(grep -v ': OK' /tmp/soundya-sha.log | head -3 | tr '\n' ' ')"
        echo "package: checksums OK"
    else
        echo "package: sha256sum is not on this speaker, checksums not checked"
    fi
fi
[ -f "$SRC/soundya" ] || fail "в архиве нет программы soundya"
chmod 755 "$SRC/soundya"
"$SRC/soundya" version >/dev/null 2>&1 || fail "новая версия не запускается на этой колонке"

# ---- 2. Backups ----
mkdir -p "$BK"
TS=$(date +%Y%m%d-%H%M%S)
if [ -f "$DIR/soundya" ]; then
    rm -rf "$BK/prev.new" && mkdir -p "$BK/prev.new"
    for f in $PROGRAM; do
        if [ -f "$DIR/$f" ]; then cp -p "$DIR/$f" "$BK/prev.new/"; fi
    done
    for d in lua fake; do
        if [ -d "$DIR/$d" ]; then cp -rp "$DIR/$d" "$BK/prev.new/"; fi
    done
    if [ -f "$INIT" ]; then cp -p "$INIT" "$BK/prev.new/S99soundya"; fi
    echo "$OLD_VER" > "$BK/prev.new/VERSION"
    rm -rf "$BK/prev" && mv "$BK/prev.new" "$BK/prev"
fi
DATA=$BK/data-$TS-${OLD_VER:-new}
mkdir -p "$DATA"
for f in "$DIR"/*; do
    [ -f "$f" ] || continue
    name=${f##*/}
    case " $PROGRAM " in *" $name "*) continue ;; esac
    case "$name" in *.log | *.tmp | update-result) continue ;; esac
    cp -p "$f" "$DATA/"
done
# Keep the five newest data snapshots.
set -- $(ls -1d "$BK"/data-* 2>/dev/null | sort)
while [ $# -gt 5 ]; do rm -rf "$1"; shift; done
sync
echo "backup: program in $BK/prev, data in $DATA"

# ---- 3. Program files ----
if [ -x "$INIT" ]; then "$INIT" stop || true; fi

put() { # file from the package → $DIR, atomically
    cp "$SRC/$1" "$DIR/$1.new" && chmod 755 "$DIR/$1.new" && mv "$DIR/$1.new" "$DIR/$1"
}
install_program() { # set -e does not apply here (called from «if»): every step checks itself
    put soundya || return 1
    put run.sh || return 1
    put guard.sh || return 1
    if [ -f "$SRC/uninstall.sh" ]; then put uninstall.sh || return 1; fi
    # zapret's nfqws (official release build) for «обход блокировок».
    if [ -f "$SRC/nfqws" ]; then killall nfqws 2>/dev/null; put nfqws || return 1; fi
    # zapret2's nfqws2 and its Lua library.
    if [ -f "$SRC/nfqws2" ]; then killall nfqws2 2>/dev/null; put nfqws2 || return 1; fi
    if [ -d "$SRC/lua" ]; then mkdir -p "$DIR/lua" && cp "$SRC"/lua/*.lua "$DIR/lua/" || return 1; fi
    # Fake TLS hellos for zapret strategies (from zapret's release).
    if [ -d "$SRC/fake" ]; then mkdir -p "$DIR/fake" && cp "$SRC"/fake/*.bin "$DIR/fake/" || return 1; fi
    # AirPlay receiver; the old one may still be running until soundya restarts.
    if [ -f "$SRC/shairport-sync" ]; then killall shairport-sync 2>/dev/null; put shairport-sync || return 1; fi
    # User settings: only when there are none yet.
    if [ ! -f "$DIR/soundya.env" ]; then cp "$SRC/soundya.env" "$DIR/" || return 1; fi
    # «Что поставить» from deploy.sh: soundya takes it on start (see settings.rs).
    if [ -f /tmp/soundya-features ]; then mv /tmp/soundya-features "$DIR/features"; fi
    # Root filesystem is read-only: open it just for the init script and the command link.
    rootfs_rw
    cp "$SRC/S99soundya" "$INIT.new" && chmod 755 "$INIT.new" && mv "$INIT.new" "$INIT"
    ok=$?
    [ -n "${SOUNDYA_INSTALL_DIR:-}" ] || ln -sf "$DIR/soundya" /usr/bin/soundya 2>/dev/null
    rootfs_back
    return $ok
}
if install_program; then INSTALL_ERR=""; else INSTALL_ERR="не удалось скопировать файлы"; fi
sync

# ---- 4. Start and check ----
start() {
    [ -f /tmp/soundya.log ] && mv /tmp/soundya.log /tmp/soundya.log.old
    : > /tmp/soundya.log
    rm -f "$RUN/soundya.stop" "$READY"
    "$INIT" start
}
healthy() { # version [legacy]: soundya said it is ready, and is still running 10 s later.
    # Versions before 0.5.00 do not write $READY: for them (a rollback) the
    # control port answering twice, 10 s apart, is the check.
    want=$1
    legacy=${2:-}
    pid=""
    i=0
    while [ "$i" -lt "$WAIT" ]; do
        set -- $(cat "$READY" 2>/dev/null) "" ""
        if [ "$1" = "$want" ]; then pid=$2; break; fi
        if [ -n "$legacy" ] && "$DIR/soundya" status >/dev/null 2>&1; then break; fi
        sleep 1
        i=$((i + 1))
    done
    [ "$i" -lt "$WAIT" ] || { REASON="не запустилась за $WAIT с"; return 1; }
    sleep "$HOLD"
    if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then REASON="запустилась и сразу упала"; return 1; fi
    "$DIR/soundya" status >/dev/null 2>&1 || { REASON="не отвечает на команды"; return 1; }
    return 0
}
rollback() { # reason
    set +e
    echo "update: $1; rolling back to $OLD_VER"
    tail -n 40 /tmp/soundya.log > "$DIR/failed-update.log" 2>/dev/null
    "$INIT" stop
    for f in $PROGRAM; do
        if [ -f "$BK/prev/$f" ]; then cp -p "$BK/prev/$f" "$DIR/$f.new" && mv "$DIR/$f.new" "$DIR/$f"; fi
    done
    for d in lua fake; do
        if [ -d "$BK/prev/$d" ]; then rm -rf "$DIR/$d" && cp -rp "$BK/prev/$d" "$DIR/$d"; fi
    done
    # The data as it was before the update (the new version may have migrated it).
    for f in "$DATA"/*; do
        if [ -f "$f" ]; then cp -p "$f" "$DIR/"; fi
    done
    if [ -f "$BK/prev/S99soundya" ]; then
        rootfs_rw
        cp -p "$BK/prev/S99soundya" "$INIT"
        rootfs_back
    fi
    sync
    start
    if healthy "$OLD_VER" legacy; then
        result rolledback "$1"
    else
        result failed "$1; откат тоже не запустился: $REASON"
    fi
}

REASON=""
if [ -n "$INSTALL_ERR" ]; then
    if [ -d "$BK/prev" ]; then rollback "$INSTALL_ERR"; exit 1; fi
    fail "$INSTALL_ERR"
fi
start
if healthy "$NEW_VER"; then
    result ok ""
elif [ -d "$BK/prev" ] && [ -n "$OLD_VER" ]; then
    rollback "версия $NEW_VER $REASON"
    exit 1
else
    result failed "$REASON"
fi

echo
echo "Installed soundya $NEW_VER. Try:  soundya play bonobo   or say «Алиса, включи Bonobo»"
IP=$(ip -4 addr show wlan0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)
echo "Web page: http://soundya.local:8765/  (or http://${IP:-<IP колонки>}:8765/)"
echo "AirPlay: the speaker shows up on iPhone and Mac as «Яндекс Станция» (name: settings page)"
echo "Log: /tmp/soundya.log (survives a reboot as $DIR/previous.log)"
