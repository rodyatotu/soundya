#!/bin/sh
# Builds the graphical installers with the speaker build inside:
#   dist/soundya-setup-windows.exe      (double-click; Windows 10/11, x64)
#   dist/soundya.dmg                    (soundya.app, drag to Applications; Intel and Apple Silicon)
#   dist/soundya-setup-mac.zip          (the same app, zipped)
# Needs: rustup targets x86_64-pc-windows-gnu, aarch64-apple-darwin, x86_64-apple-darwin; zig + cargo-zigbuild.
set -eu
cd "$(dirname "$0")/.."
[ -f dist/soundya-armv7.tar ] || sh scripts/build-mac.sh
cd installer
cargo zigbuild --release --target x86_64-pc-windows-gnu
cargo zigbuild --release --target universal2-apple-darwin
cd ..
cp installer/target/x86_64-pc-windows-gnu/release/soundya-installer.exe dist/soundya-setup-windows.exe
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
# The disk image: open it, drag soundya to Applications.
[ -d target/pydeps/ds_store ] || pip install -q --target target/pydeps ds_store mac_alias
python3 scripts/mac-dmg/build_dmg.py "$APP" target/dmg-assets dist/soundya.dmg
# The zip too (for those who prefer it).
(cd dist/mac && rm -f ../soundya-setup-mac.zip && zip -qry -X ../soundya-setup-mac.zip soundya.app)
ls -la dist/soundya-setup-windows.exe dist/soundya.dmg dist/soundya-setup-mac.zip
