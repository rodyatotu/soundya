#!/bin/sh
# On a Mac: builds the widget (mac-widget/) with Xcode from mac-widget/xcode and puts
# SoundyaWidget.appex into soundya.app/Contents/PlugIns, unsigned (build-installer.sh
# signs it inside out). macOS 14+, Intel and Apple Silicon.
#   sh scripts/mac-widget/build.sh dist/mac/soundya.app 0.5.0
# Needs full Xcode (not only the Command Line Tools): only xcodebuild links the extension
# with its entry point (_NSExtensionMain) and generates Metadata.appintents, without
# which macOS does not run the widget's configuration and buttons. If xcode-select points
# to the Command Line Tools, the first /Applications/Xcode*.app is used (nothing on the
# system is changed); SOUNDYA_XCODE=/Applications/Xcode-beta.app picks one explicitly.
set -eu
APP=$1
VERSION=$2
cd "$(dirname "$0")/../.."
if [ -n "${SOUNDYA_XCODE:-}" ]; then
    export DEVELOPER_DIR="$SOUNDYA_XCODE/Contents/Developer"
elif ! xcode-select -p 2>/dev/null | grep -q '\.app/Contents/Developer$'; then
    for x in /Applications/Xcode*.app; do
        [ -x "$x/Contents/Developer/usr/bin/xcodebuild" ] || continue
        export DEVELOPER_DIR="$x/Contents/Developer"
        break
    done
fi
if ! xcodebuild -version >/dev/null 2>&1; then
    echo "виджет: нужен Xcode (xcodebuild), не только Command Line Tools; задайте SOUNDYA_XCODE=/Applications/Xcode….app" >&2
    exit 1
fi
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
# -target: the project has no shared schemes. Signing is off here on purpose:
# build-installer.sh signs the widget with widget.entitlements, then the app.
xcodebuild -quiet -project mac-widget/xcode/SoundyaWidget.xcodeproj -target SoundyaWidget \
    -configuration Release build \
    ARCHS="arm64 x86_64" ONLY_ACTIVE_ARCH=NO \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$VERSION" \
    CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO \
    SYMROOT="$WORK/build" OBJROOT="$WORK/obj"
BUILT="$WORK/build/Release/SoundyaWidget.appex"
[ -d "$BUILT/Contents/Resources/Metadata.appintents" ] || {
    echo "виджет: Xcode не создал Metadata.appintents — кнопки и настройка виджета не будут работать" >&2
    exit 1
}
rm -rf "$APP/Contents/PlugIns/SoundyaWidget.appex"
mkdir -p "$APP/Contents/PlugIns"
# ditto keeps the bundle as Xcode made it (symlinks, attributes).
ditto "$BUILT" "$APP/Contents/PlugIns/SoundyaWidget.appex"
echo "widget: $APP/Contents/PlugIns/SoundyaWidget.appex ($(xcodebuild -version | head -1))"
