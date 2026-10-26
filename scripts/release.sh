#!/bin/sh
# Собирает soundya и публикует релиз на GitHub: sh scripts/release.sh v0.2.0
set -e
TAG="${1:?usage: sh scripts/release.sh vX.Y.Z}"
cd "$(dirname "$0")/.."
sh scripts/build-mac.sh
NOTES=$(awk -v v="## ${TAG#v}" '$0==v{f=1;next} /^## /{f=0} f' CHANGELOG.md)
gh release create "$TAG" dist/soundya-armv7.tar --title "soundya $TAG" --notes "$NOTES"
