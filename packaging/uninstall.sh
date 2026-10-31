#!/bin/sh
# Removes soundya from the speaker completely: the service, the AirPlay receiver,
# its files and settings (SoundCloud login, PC key). Alice stays as she was.
# Run as root:  sh /data/opt/soundya/uninstall.sh   (or the button on the settings page)
DIR=/data/opt/soundya
touch /run/soundya.stop
[ -x /etc/init.d/S99soundya ] && /etc/init.d/S99soundya stop
for p in $(pidof soundya shairport-sync nfqws 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
# zapret rules (soundya's own chain), if the bypass was on.
while iptables -t mangle -D POSTROUTING -j SOUNDYA_ZAPRET 2>/dev/null; do :; done
iptables -t mangle -F SOUNDYA_ZAPRET 2>/dev/null; iptables -t mangle -X SOUNDYA_ZAPRET 2>/dev/null
RO=0
if grep -q ' / [^ ]* ro[ ,]' /proc/mounts; then RO=1; mount -o remount,rw /; fi
rm -f /etc/init.d/S99soundya /usr/bin/soundya
sync
[ "$RO" -eq 0 ] || mount -o remount,ro /
rm -rf "$DIR" /tmp/soundya.log /tmp/soundya-update /tmp/soundya-update.tar /tmp/soundya-airplay.mdns /run/soundya.pid
rm -f /run/soundya.stop
echo "soundya removed"
