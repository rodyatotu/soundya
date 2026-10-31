#!/bin/sh
# Cross-builds soundya for Yandex Station Mini 1 (ARMv7 hard-float, glibc <= 2.23).
# Runs on x86_64 Linux (Ubuntu/Debian, WSL, a Docker container or GitHub Actions).
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
CACHE=${SOUNDYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/soundya"}
TC_NAME=gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf
TC="$CACHE/$TC_NAME"
TARGET=armv7-unknown-linux-gnueabihf
mkdir -p "$CACHE"

if [ ! -x "$TC/bin/arm-linux-gnueabihf-gcc" ]; then
    archive="$CACHE/$TC_NAME.tar.xz"
    if [ ! -f "$archive" ]; then
        curl --fail --location --retry 3 --output "$archive.part" \
            "https://developer.arm.com/-/cdn-downloads/permalink/legacy-linaro-gnu-toolchains/6.5-2018.12/$TC_NAME.tar.xz"
        mv "$archive.part" "$archive"
    fi
    echo "a84b1239765617f3192014cb259c05e32771a64a4c831cf89577fcd87918eacd  $archive" | sha256sum -c -
    mkdir -p "$TC"
    tar -xJf "$archive" --strip-components=1 -C "$TC"
fi

rustup target add "$TARGET" >/dev/null
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$TC/bin/arm-linux-gnueabihf-gcc"
export CC_armv7_unknown_linux_gnueabihf="$TC/bin/arm-linux-gnueabihf-gcc"
export AR_armv7_unknown_linux_gnueabihf="$TC/bin/arm-linux-gnueabihf-ar"
# RPATH (not RUNPATH) so the speaker's ALSA plugins find their vendor libraries.
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=cortex-a53 -C link-arg=-Wl,-rpath,/system/vendor/quasar/libs -C link-arg=-Wl,--disable-new-dtags -C link-arg=-Wl,--gc-sections"

cd "$ROOT"
cargo build --release --target "$TARGET"
BIN="target/$TARGET/release/soundya"

# The Mini 1 firmware ships glibc 2.23: refuse a binary that needs anything newer.
"$TC/bin/arm-linux-gnueabihf-readelf" --version-info "$BIN" \
    | grep -o 'GLIBC_[0-9.]*' | sort -u -V | tee /dev/stderr | tail -1 \
    | awk -F_ '{ split($2, v, "."); if (v[1] > 2 || (v[1] == 2 && v[2] > 23)) { print "needs glibc " $2 " > 2.23" > "/dev/stderr"; exit 1 } }'

mkdir -p dist/soundya
cp "$BIN" dist/soundya/soundya
cp packaging/install.sh packaging/uninstall.sh packaging/run.sh packaging/guard.sh packaging/S99soundya packaging/soundya.env dist/soundya/
# AirPlay receiver (prebuilt; rebuild with scripts/build-shairport.sh).
cp packaging/shairport-sync dist/soundya/
# zapret nfqws v72.9, linux-arm static, from the official release (see packaging/NFQWS.md).
cp packaging/nfqws dist/soundya/
mkdir -p dist/soundya/fake && cp packaging/fake/*.bin dist/soundya/fake/
chmod 755 dist/soundya/soundya dist/soundya/shairport-sync dist/soundya/nfqws dist/soundya/install.sh dist/soundya/uninstall.sh dist/soundya/run.sh dist/soundya/guard.sh dist/soundya/S99soundya
COPYFILE_DISABLE=1 tar -C dist -cf dist/soundya-armv7.tar soundya
echo "Built dist/soundya-armv7.tar"
