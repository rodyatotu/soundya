#!/bin/sh
# Started by run.sh next to soundya. Two jobs:
#  1. Keeps the end of soundya's log on flash, so it survives a power cut:
#     /data/opt/soundya/last.log (this boot) and previous.log (the boot before).
#  2. Watches the sound card. When it hangs in PREPARED (opened but never
#     started) nothing can play, Alice included, and the speaker looks dead.
#     After 90 s the guard restarts Alice's media service; if the card is still
#     stuck 2 minutes later it reboots the speaker (at most once an hour).
DIR=/data/opt/soundya
LOG=/tmp/soundya.log
STATUS=/proc/asound/card0/pcm0p/sub0/status

note() {
    echo "guard: $*" >> "$LOG"
    echo "$(date '+%Y-%m-%d %H:%M:%S') $*" >> "$DIR/guard.log"
    tail -n 50 "$DIR/guard.log" > "$DIR/guard.log.tmp" && mv "$DIR/guard.log.tmp" "$DIR/guard.log"
}

saved_size=-1
save_log() {
    size=$(wc -c < "$LOG" 2>/dev/null || echo 0)
    # Only when something new was logged: spares the flash.
    [ "$size" = "$saved_size" ] && [ "${1:-}" != force ] && return
    saved_size=$size
    tail -n 200 "$LOG" > "$DIR/last.log.tmp" 2>/dev/null && mv "$DIR/last.log.tmp" "$DIR/last.log"
}

# The log of the previous boot (or of before a power cut).
[ -f "$DIR/last.log" ] && mv "$DIR/last.log" "$DIR/previous.log"

stuck=0
tick=0
while [ ! -f /run/soundya.stop ]; do
    sleep 10
    tick=$((tick + 1))
    [ $((tick % 30)) -eq 0 ] && save_log    # every 5 minutes

    state=$(sed -n 's/^state: *//p' "$STATUS" 2>/dev/null)
    if [ "$state" = PREPARED ]; then stuck=$((stuck + 1)); else stuck=0; fi

    if [ "$stuck" -eq 9 ]; then
        note "sound card stuck in PREPARED for 90 s; restarting Alice's media service"
        save_log force
        for p in $(pidof maind); do
            grep -q -- '--mediad' "/proc/$p/cmdline" 2>/dev/null && kill "$p" 2>/dev/null
        done
    fi

    if [ "$stuck" -ge 21 ]; then
        now=$(date +%s)
        last=$(cat "$DIR/guard.reboot" 2>/dev/null || echo 0)
        if [ $((now - last)) -gt 3600 ]; then
            echo "$now" > "$DIR/guard.reboot"
            note "sound card still stuck; rebooting the speaker"
            save_log force
            sync
            reboot
        else
            note "sound card still stuck, but rebooted less than an hour ago; waiting"
        fi
        stuck=0
    fi
done
save_log force
