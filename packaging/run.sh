#!/bin/sh
# Keeps the daemon running: restarts it a few seconds after any exit.
DIR=/data/opt/soundya
echo $$ > /run/soundya.pid
# shellcheck disable=SC1091
[ -f "$DIR/soundya.env" ] && . "$DIR/soundya.env"
export SOUNDYA_DIR="$DIR"
# Right after boot give Alice time to come up.
UP=$(cut -d. -f1 /proc/uptime)
[ "$UP" -lt 120 ] && sleep 20
# Keeps the log on flash and rescues a hung sound card (see guard.sh).
[ -x "$DIR/guard.sh" ] && "$DIR/guard.sh" &
while [ ! -f /run/soundya.stop ]; do
    "$DIR/soundya" daemon
    sleep 3
done
