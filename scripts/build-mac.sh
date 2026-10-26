#!/bin/sh
# Builds soundya on macOS (or Linux) with cargo-zigbuild:
#   brew install zig cargo-zigbuild && rustup target add armv7-unknown-linux-gnueabihf
set -eu
cd "$(dirname "$0")/.."
cargo zigbuild --release --target armv7-unknown-linux-gnueabihf.2.23
rm -rf dist && mkdir -p dist/soundya
cp target/armv7-unknown-linux-gnueabihf/release/soundya dist/soundya/
cp packaging/install.sh packaging/run.sh packaging/S99soundya packaging/soundya.env dist/soundya/
chmod 755 dist/soundya/*
# Plain tar: the speaker's busybox tar cannot read gzip.
COPYFILE_DISABLE=1 tar -C dist -cf dist/soundya-armv7.tar soundya
echo "Built dist/soundya-armv7.tar"
