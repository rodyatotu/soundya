#!/bin/sh
# Run on the speaker as root:  sh /tmp/soundya/install.sh
set -eu
SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DIR=/data/opt/soundya
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

[ -x /etc/init.d/S99soundya ] && /etc/init.d/S99soundya stop || true

mkdir -p "$DIR"
cp "$SRC/soundya" "$SRC/run.sh" "$DIR/"
[ -f "$DIR/soundya.env" ] || cp "$SRC/soundya.env" "$DIR/"
chmod 755 "$DIR/soundya" "$DIR/run.sh"

# Root filesystem is read-only: open it just for the init script and the command link.
RO=0
if grep -q ' / [^ ]* ro[ ,]' /proc/mounts; then RO=1; mount -o remount,rw /; fi
cp "$SRC/S99soundya" /etc/init.d/S99soundya
chmod 755 /etc/init.d/S99soundya
ln -sf "$DIR/soundya" /usr/bin/soundya 2>/dev/null || true
sync
[ "$RO" -eq 0 ] || mount -o remount,ro /

/etc/init.d/S99soundya start
echo
echo "Installed. Try:  soundya play bonobo   or say «Алиса, включи Bonobo»"
IP=$(ip -4 addr show wlan0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)
echo "Web page: http://${IP:-<IP колонки>}:8765/"
echo "Log: /tmp/soundya.log"
