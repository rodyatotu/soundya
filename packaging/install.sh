#!/bin/sh
# Run on the speaker as root:  sh /tmp/soundya/install.sh
set -eu
SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DIR=/data/opt/soundya
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

[ -x /etc/init.d/S99soundya ] && /etc/init.d/S99soundya stop || true

mkdir -p "$DIR"
cp "$SRC/soundya" "$SRC/run.sh" "$SRC/guard.sh" "$DIR/"
[ -f "$SRC/uninstall.sh" ] && cp "$SRC/uninstall.sh" "$DIR/" && chmod 755 "$DIR/uninstall.sh"
# AirPlay receiver; the old one may still be running until soundya restarts.
if [ -f "$SRC/shairport-sync" ]; then
    killall shairport-sync 2>/dev/null || true
    cp "$SRC/shairport-sync" "$DIR/shairport-sync.new" && mv "$DIR/shairport-sync.new" "$DIR/shairport-sync"
    chmod 755 "$DIR/shairport-sync"
fi
[ -f "$DIR/soundya.env" ] || cp "$SRC/soundya.env" "$DIR/"
chmod 755 "$DIR/soundya" "$DIR/run.sh" "$DIR/guard.sh"

# Root filesystem is read-only: open it just for the init script and the command link.
RO=0
if grep -q ' / [^ ]* ro[ ,]' /proc/mounts; then RO=1; mount -o remount,rw /; fi
cp "$SRC/S99soundya" /etc/init.d/S99soundya
chmod 755 /etc/init.d/S99soundya
ln -sf "$DIR/soundya" /usr/bin/soundya 2>/dev/null || true
sync
[ "$RO" -eq 0 ] || mount -o remount,ro /

: > /tmp/soundya.log
rm -f /run/soundya.stop
/etc/init.d/S99soundya start
echo
echo "Installed. Try:  soundya play bonobo   or say «Алиса, включи Bonobo»"
IP=$(ip -4 addr show wlan0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)
echo "Web page: http://soundya.local:8765/  (or http://${IP:-<IP колонки>}:8765/)"
echo "AirPlay: the speaker shows up on iPhone and Mac as «Яндекс Станция» (name: settings page)"
echo "Log: /tmp/soundya.log (survives a reboot as $DIR/previous.log)"
