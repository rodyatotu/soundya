#!/bin/sh
# Packs the one-click Mac installer: dist/soundya-mac.zip = «Установить soundya.command»
# + soundya-armv7.tar. Unzip on the Mac, double-click the .command file.
set -eu
cd "$(dirname "$0")/.."
[ -f dist/soundya-armv7.tar ] || sh scripts/build-mac.sh
rm -rf dist/soundya-mac && mkdir -p dist/soundya-mac
cp "scripts/mac-installer/Установить soundya.command" dist/soundya-mac/
chmod 755 "dist/soundya-mac/Установить soundya.command"
cp dist/soundya-armv7.tar dist/soundya-mac/
(cd dist && rm -f soundya-mac.zip && zip -qr -X soundya-mac.zip soundya-mac)
echo "Built dist/soundya-mac.zip"
