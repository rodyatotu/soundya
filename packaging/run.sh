#!/bin/sh
# Keeps the daemon running: restarts it a few seconds after any exit.
DIR=/data/opt/soundya
echo $$ > /run/soundya.pid
# shellcheck disable=SC1091
[ -f "$DIR/soundya.env" ] && . "$DIR/soundya.env"
export SOUNDYA_DIR="$DIR"
# Play through the same ALSA libraries and plugins as Alice: her library path
# (the vendor libasound needs plugins such as the rate converter, whose own
# libraries live next to hers — Station Mini 1). Taken from her process.
alice_libs() {
    for p in $(pidof maind 2>/dev/null); do
        tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | sed -n 's/^LD_LIBRARY_PATH=//p' | head -1
        return
    done
}
ALICE_LD=$(alice_libs)
if [ ! -f /etc/asound.conf ] && [ -d /system/vendor/quasar/libs ]; then
    ALICE_LD="${ALICE_LD:+$ALICE_LD:}/system/vendor/quasar/libs"
fi
# The rate converter plugin's libsamplerate, wherever this firmware keeps it.
for d in /system/vendor/quasar/libs /lib/linkplay_a98 /lib/linkplay_a98/lib /system/vendor/lib /system/lib /usr/lib /lib; do
    if [ -e "$d/libsamplerate.so.0" ]; then
        case ":$ALICE_LD:" in *":$d:"*) ;; *) ALICE_LD="${ALICE_LD:+$ALICE_LD:}$d" ;; esac
        break
    fi
done
[ -n "$ALICE_LD" ] && export LD_LIBRARY_PATH="$ALICE_LD${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# Right after boot give Alice time to come up.
UP=$(cut -d. -f1 /proc/uptime)
[ "$UP" -lt 120 ] && sleep 20
# Keeps the log on flash and rescues a hung sound card (see guard.sh).
[ -x "$DIR/guard.sh" ] && "$DIR/guard.sh" &
while [ ! -f /run/soundya.stop ]; do
    "$DIR/soundya" daemon
    sleep 3
done
