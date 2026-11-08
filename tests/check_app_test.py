#!/usr/bin/env python3
"""Tests scripts/mac-dmg/check_app.py on hand-made bundles (fake Mach-O headers,
not real binaries): the app alone, the app with a widget built by Xcode, and a
widget without what only xcodebuild produces (Metadata.appintents).

    python3 tests/check_app_test.py
"""
import os
import plistlib
import struct
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts", "mac-dmg"))
import check_app  # noqa: E402

ARM64 = 0x0100000C


def macho(minos):
    """A thin arm64 Mach-O header with one LC_BUILD_VERSION (enough for check_app)."""
    major, minor = minos
    cmd = struct.pack("<IIIIII", 0x32, 24, 1, (major << 16) | (minor << 8), 0, 0)
    return struct.pack("<IiiIIIII", 0xFEEDFACF, ARM64, 0, 2, 1, len(cmd), 0, 0) + cmd


def bundle(path, plist, minos, exe="bin"):
    os.makedirs(os.path.join(path, "Contents", "MacOS"))
    plist = dict(plist, CFBundleExecutable=exe)
    with open(os.path.join(path, "Contents", "Info.plist"), "wb") as f:
        plistlib.dump(plist, f, fmt=plistlib.FMT_BINARY)  # Xcode writes binary plists
    p = os.path.join(path, "Contents", "MacOS", exe)
    with open(p, "wb") as f:
        f.write(macho(minos))
    os.chmod(p, 0o755)


APP = {"CFBundleIdentifier": "io.github.rodyatotu.soundya", "LSMinimumSystemVersion": "11.0",
       "CFBundleShortVersionString": "0.5.0", "CFBundleVersion": "0.5.0"}
WIDGET = {"CFBundleIdentifier": "io.github.rodyatotu.soundya.widget", "LSMinimumSystemVersion": "14.0",
          "CFBundleShortVersionString": "0.5.0", "CFBundleVersion": "0.5.0",
          "NSExtension": {"NSExtensionPointIdentifier": "com.apple.widgetkit-extension"},
          "NSLocalNetworkUsageDescription": "x"}

failed = 0


def expect(name, errors, *needles):
    global failed
    ok = (not needles and not errors) or (needles and all(any(n in e for e in errors) for n in needles))
    print(("ok  " if ok else "FAIL"), name, "" if ok else errors)
    failed += not ok


def case(name, widget=None, metadata=True, minos_widget=(14, 0), app_minos=(11, 0)):
    d = tempfile.mkdtemp()
    app = os.path.join(d, "soundya.app")
    bundle(app, APP, app_minos, "soundya")
    if widget is not None:
        w = os.path.join(app, "Contents", "PlugIns", "SoundyaWidget.appex")
        bundle(w, widget, minos_widget, "SoundyaWidget")
        if metadata:
            os.makedirs(os.path.join(w, "Contents", "Resources", "Metadata.appintents"))
    return check_app.check(app)


expect("app without widget", case("a"))
expect("app with Xcode-built widget", case("b", WIDGET))
expect("app too new for its plist", case("c", app_minos=(15, 0)), "собран для macOS 15.0")
expect("widget without Metadata.appintents (swiftc build)", case("d", WIDGET, metadata=False), "Metadata.appintents")
expect("widget newer than its plist", case("e", WIDGET, minos_widget=(15, 0)), "виджет: arm64: собран для macOS 15.0")
expect("widget of another version", case("f", dict(WIDGET, CFBundleVersion="0.4.41")), "CFBundleVersion")
expect("widget without local network text", case("g", {k: v for k, v in WIDGET.items() if k != "NSLocalNetworkUsageDescription"}),
       "NSLocalNetworkUsageDescription")
expect("not a widget extension", case("h", dict(WIDGET, NSExtension={})), "NSExtensionPointIdentifier")
expect("widget id outside the app's", case("i", dict(WIDGET, CFBundleIdentifier="other.widget")), "CFBundleIdentifier")
print("failed:", failed)
sys.exit(1 if failed else 0)
