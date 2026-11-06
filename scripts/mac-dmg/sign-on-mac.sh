#!/bin/sh
# On a Mac: signs soundya.app (ad-hoc, or with a Developer ID if given) and
# rebuilds the disk image with Apple's own hdiutil, keeping the window layout.
#   sh scripts/mac-dmg/sign-on-mac.sh dist/soundya.dmg ["Developer ID Application: …"]
# Ad-hoc: macOS still asks «Открыть?» on first launch (right click → Открыть),
# but no longer calls the app «damaged». A Developer ID + notarytool removes the question.
set -eu
DMG=$1
ID=${2:--}
WORK=$(mktemp -d)
hdiutil attach -nobrowse -readonly -mountpoint "$WORK/in" "$DMG" >/dev/null
mkdir "$WORK/out"
ditto "$WORK/in" "$WORK/out"
hdiutil detach "$WORK/in" >/dev/null
xattr -cr "$WORK/out/soundya.app"
codesign --force --deep --options runtime --sign "$ID" "$WORK/out/soundya.app"
codesign --verify --deep --strict "$WORK/out/soundya.app"
OUT="${DMG%.dmg}-signed.dmg"
rm -f "$OUT"
hdiutil create -volname soundya -srcfolder "$WORK/out" -fs HFS+ -format UDZO -ov "$OUT" >/dev/null
[ "$ID" = "-" ] || codesign --sign "$ID" "$OUT"
rm -rf "$WORK"
echo "Готово: $OUT"
