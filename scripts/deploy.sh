#!/bin/sh
# Installs (or updates) soundya on the speaker:
#   sh scripts/deploy.sh soundya.local      (once soundya runs on it)
#   sh scripts/deploy.sh 192.168.0.112      (first install, by IP)
set -eu
cd "$(dirname "$0")/.."
IP=${1:?usage: scripts/deploy.sh <speaker-ip>}
adb connect "$IP:5555" >/dev/null
adb -s "$IP:5555" push dist/soundya-armv7.tar /tmp/
adb -s "$IP:5555" shell "cd /tmp && rm -rf soundya && tar -xf soundya-armv7.tar && sh /tmp/soundya/install.sh"
