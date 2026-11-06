#!/bin/sh
# Builds the graphical installers with the speaker build inside:
#   dist/soundya-setup-windows.exe      (double-click; Windows 10/11, x64)
#   dist/soundya-setup-mac.zip          («Установка soundya.app», Intel and Apple Silicon)
# Needs: rustup targets x86_64-pc-windows-gnu, aarch64-apple-darwin, x86_64-apple-darwin; zig + cargo-zigbuild.
set -eu
cd "$(dirname "$0")/.."
[ -f dist/soundya-armv7.tar ] || sh scripts/build-mac.sh
cd installer
cargo zigbuild --release --target x86_64-pc-windows-gnu
cargo zigbuild --release --target universal2-apple-darwin
cd ..
cp installer/target/x86_64-pc-windows-gnu/release/soundya-installer.exe dist/soundya-setup-windows.exe
APP="dist/mac/Установка soundya.app"
rm -rf dist/mac && mkdir -p "$APP/Contents/MacOS"
cp installer/target/universal2-apple-darwin/release/soundya-installer "$APP/Contents/MacOS/soundya-installer"
chmod 755 "$APP/Contents/MacOS/soundya-installer"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Установка soundya</string>
  <key>CFBundleIdentifier</key><string>io.github.rodyatotu.soundya.installer</string>
  <key>CFBundleExecutable</key><string>soundya-installer</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
(cd dist/mac && rm -f ../soundya-setup-mac.zip && zip -qr -X ../soundya-setup-mac.zip "Установка soundya.app")
ls -la dist/soundya-setup-windows.exe dist/soundya-setup-mac.zip
