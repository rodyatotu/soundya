#!/bin/sh
# Builds soundya on macOS (or Linux) with cargo-zigbuild:
#   brew install zig cargo-zigbuild && rustup target add armv7-unknown-linux-gnueabihf
# --dev: the developer build (feedback mode, version «+dev», no updates from GitHub)
# into dist/soundya-armv7-dev.tar; the rest of dist/ is left alone then.
set -eu
cd "$(dirname "$0")/.."
DEV=""
[ "${1:-}" = "--dev" ] && DEV=1
if [ -n "$DEV" ]; then
    cargo zigbuild --release --features dev --target armv7-unknown-linux-gnueabihf.2.23
    rm -rf dist/soundya && mkdir -p dist/soundya
else
    cargo zigbuild --release --target armv7-unknown-linux-gnueabihf.2.23
    rm -rf dist && mkdir -p dist/soundya
fi
cp target/armv7-unknown-linux-gnueabihf/release/soundya dist/soundya/
cp packaging/install.sh packaging/uninstall.sh packaging/run.sh packaging/guard.sh packaging/S99soundya packaging/soundya.env dist/soundya/
# AirPlay receiver (prebuilt; rebuild with scripts/build-shairport.sh).
cp packaging/shairport-sync dist/soundya/
# zapret nfqws v72.9, linux-arm static, from the official release (see packaging/NFQWS.md).
cp packaging/nfqws dist/soundya/
mkdir -p dist/soundya/fake && cp packaging/fake/*.bin dist/soundya/fake/
# zapret2 nfqws2 v1.0.6, linux-arm static, and its Lua library, from the official release.
cp packaging/nfqws2 dist/soundya/
mkdir -p dist/soundya/lua && cp packaging/lua/*.lua dist/soundya/lua/
chmod 755 dist/soundya/*
# Plain tar: the speaker's busybox tar cannot read gzip.
# Version and checksums: install.sh refuses a damaged package and checks the update started.
# The same as the program says it is (src/main.rs VERSION): install.sh compares them.
echo "$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)${DEV:++dev}" > dist/soundya/VERSION
(cd dist/soundya && find . -type f ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
    | while read -r f; do if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi; done > SHA256SUMS)
TAR=dist/soundya-armv7${DEV:+-dev}.tar
COPYFILE_DISABLE=1 tar -C dist -cf "$TAR" soundya
echo "Built $TAR"
