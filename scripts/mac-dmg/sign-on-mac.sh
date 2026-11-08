#!/bin/sh
# On a Mac: signs soundya.app (ad-hoc, or with a Developer ID if given) and
# rebuilds the disk image with Apple's own hdiutil, keeping the window layout.
#   sh scripts/mac-dmg/sign-on-mac.sh dist/soundya.dmg ["Developer ID Application: …"]
# Writes dist/soundya-signed.dmg next to the input.
#
# Why: once downloaded (quarantined), the raw ISO/HFS+ hybrid xorriso makes on
# Linux is not mounted quietly: macOS 15 asks «Возможно, возникла проблема с этим
# образом диска» (a UDZO image from hdiutil with the same files is mounted at once). The app inside is signed
# only by the linker (Info.plist not bound), which Gatekeeper rejects.
# Ad-hoc signing does not make Gatekeeper silent either: on macOS 15 the first
# launch is still blocked until «Системные настройки» → «Конфиденциальность и
# безопасность» → «Открыть всё равно» (right click → «Открыть» no longer works).
# Only a Developer ID + notarytool removes that step.
set -eu
DMG=$1
ID=${2:--}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'hdiutil detach -quiet "$WORK/in" 2>/dev/null || true; rm -rf "$WORK"' EXIT
# A copy without quarantine: a downloaded image would otherwise not attach
# («attach canceled») when there is nobody to answer the question.
cp "$DMG" "$WORK/in.dmg"
xattr -c "$WORK/in.dmg"
hdiutil attach -nobrowse -readonly -mountpoint "$WORK/in" "$WORK/in.dmg" >/dev/null
mkdir "$WORK/out"
ditto "$WORK/in" "$WORK/out"
hdiutil detach "$WORK/in" >/dev/null
# xorriso leaves Finder info on every file; codesign refuses to sign with it.
xattr -cr "$WORK/out/soundya.app"
python3 "$HERE/check_app.py" "$WORK/out/soundya.app"
codesign --force --deep --options runtime --sign "$ID" "$WORK/out/soundya.app"
codesign --verify --deep --strict "$WORK/out/soundya.app"
OUT="${DMG%.dmg}-signed.dmg"
rm -f "$OUT"
hdiutil create -volname soundya -srcfolder "$WORK/out" -fs HFS+ -format UDZO -ov "$OUT" >/dev/null
hdiutil verify "$OUT" >/dev/null
[ "$ID" = "-" ] || codesign --sign "$ID" "$OUT"
echo "Готово: $OUT"
