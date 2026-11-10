#!/bin/sh
# Builds the graphical installers with the speaker build inside:
#   dist/soundya-setup-windows.exe      (double-click; Windows 10/11, x64)
#   dist/soundya.dmg                    (soundya.app, drag to Applications; Intel and Apple Silicon)
#   dist/soundya-setup-mac.zip          (the same app, zipped)
# Needs: rustup targets x86_64-pc-windows-gnu, aarch64-apple-darwin, x86_64-apple-darwin; zig + cargo-zigbuild;
# python3 with Pillow; on Linux also xorriso. Only an image made on a Mac (hdiutil, signed bundle) may be handed out.
# SOUNDYA_DEV=1: the developer build (feedback mode in the wizard, the speaker's developer
# build inside) as dist/soundya-dev.dmg, soundya-setup-windows-dev.exe, soundya-setup-mac-dev.zip.
set -eu
cd "$(dirname "$0")/.."
FEAT="" SFX=""
if [ -n "${SOUNDYA_DEV:-}" ]; then
    FEAT="--features dev" SFX="-dev"
    [ -f dist/soundya-armv7-dev.tar ] || sh scripts/build-mac.sh --dev
else
    [ -f dist/soundya-armv7.tar ] || sh scripts/build-mac.sh
fi
cd installer
# shellcheck disable=SC2086
cargo zigbuild --release $FEAT --target x86_64-pc-windows-gnu
# Without this zig builds for its own default macOS (15.0), and the app would not
# start on the macOS 11–14 that Info.plist promises (check_app.py checks it).
# shellcheck disable=SC2086
MACOSX_DEPLOYMENT_TARGET=11.0 cargo zigbuild --release $FEAT --target universal2-apple-darwin
cd ..
cp installer/target/x86_64-pc-windows-gnu/release/soundya-installer.exe "dist/soundya-setup-windows$SFX.exe"
APP="dist/mac/soundya.app"
rm -rf dist/mac && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp installer/target/universal2-apple-darwin/release/soundya-installer "$APP/Contents/MacOS/soundya"
chmod 755 "$APP/Contents/MacOS/soundya"
python3 scripts/mac-dmg/make_assets.py target/dmg-assets >/dev/null
cp target/dmg-assets/soundya.icns "$APP/Contents/Resources/soundya.icns"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>soundya</string>
  <key>CFBundleDisplayName</key><string>soundya</string>
  <key>CFBundleIdentifier</key><string>io.github.rodyatotu.soundya</string>
  <key>CFBundleExecutable</key><string>soundya</string>
  <key>CFBundleIconFile</key><string>soundya</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleDevelopmentRegion</key><string>ru</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <!-- macOS 15: talking to the speaker needs «Локальная сеть»; this is the question's text. -->
  <key>NSLocalNetworkUsageDescription</key><string>soundya ищет вашу Яндекс Станцию в домашней сети и устанавливает на неё программу.</string>
  <key>NSBonjourServices</key><array><string>_soundya._tcp</string><string>_adb._tcp</string><string>_raop._tcp</string></array>
  <key>NSAppTransportSecurity</key><dict>
    <key>NSAllowsLocalNetworking</key><true/>
    <key>NSAllowsArbitraryLoads</key><true/>
  </dict>
</dict></plist>
PLIST
python3 scripts/mac-dmg/check_app.py "$APP"
if [ "$(uname -s)" = Darwin ]; then
    # Signed with a team's certificate when given: SOUNDYA_SIGN_ID="Apple Development: …" or
    # "Developer ID Application: …"; ad-hoc ("-") otherwise. (The desktop widget is gone:
    # soundya's own window under the menu bar icon replaced it.)
    SIGN_ID=${SOUNDYA_SIGN_ID:--}
    xattr -cr "$APP"
    codesign --force --options runtime --sign "$SIGN_ID" "$APP"
    codesign --verify --deep --strict "$APP"
    python3 scripts/mac-dmg/check_app.py "$APP"
fi
# The disk image: open it, drag soundya to Applications.
[ -d target/pydeps/ds_store ] || pip install -q --target target/pydeps ds_store mac_alias
# On a Mac: hdiutil, compressed UDZO, the signed app — ready to hand out.
python3 scripts/mac-dmg/build_dmg.py "$APP" target/dmg-assets "dist/soundya$SFX.dmg"
if [ "$(uname -s)" != Darwin ]; then
    echo "ВНИМАНИЕ: dist/soundya$SFX.dmg собран на $(uname -s) — это черновик, раздавать его нельзя:" >&2
    echo "  скачанный, он не монтируется без предупреждения, а приложение в нём не подписано." >&2
    echo "  На Mac: sh scripts/mac-dmg/sign-on-mac.sh dist/soundya$SFX.dmg → dist/soundya-signed.dmg" >&2
fi
# The zip too (for those who prefer it).
(cd dist/mac && rm -f "../soundya-setup-mac$SFX.zip" && zip -qry -X "../soundya-setup-mac$SFX.zip" soundya.app)
ls -la "dist/soundya-setup-windows$SFX.exe" "dist/soundya$SFX.dmg" "dist/soundya-setup-mac$SFX.zip"
