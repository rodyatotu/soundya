#!/usr/bin/env python3
"""Checks soundya.app before it goes into the disk image (runs on Linux too):
the executable named in Info.plist is there and executable, and every slice
of it runs on the macOS that Info.plist promises (LSMinimumSystemVersion).
If the widget is in PlugIns, it is checked the same way, plus what macOS needs
to run it: the WidgetKit extension point, Metadata.appintents (only xcodebuild
makes it) and the same version as the app.

A slice built for a newer macOS than the plist says simply does not start on
the older one, so this fails the build instead.

    python3 scripts/mac-dmg/check_app.py dist/mac/soundya.app
"""
import os
import plistlib
import struct
import sys

LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32
PLATFORM_MACOS = 1
CPU = {0x01000007: "x86_64", 0x0100000C: "arm64"}


def version(n):
    """Mach-O xxxx.yy.zz nibbles → (major, minor, patch)."""
    return (n >> 16, (n >> 8) & 0xFF, n & 0xFF)


def parse_version(text):
    parts = [int(p) for p in text.split(".")]
    return tuple((parts + [0, 0, 0])[:3])


def slice_minos(data, off):
    """(cpu name, minos) of the thin Mach-O at `off`; minos is None if absent."""
    magic = struct.unpack_from("<I", data, off)[0]
    if magic != 0xFEEDFACF:
        raise ValueError(f"not a 64-bit Mach-O at {off:#x} (magic {magic:#x})")
    cputype, _, _, ncmds, _, _, _ = struct.unpack_from("<iiIIIII", data, off + 4)
    p = off + 32
    minos = None
    for _ in range(ncmds):
        cmd, size = struct.unpack_from("<II", data, p)
        if cmd == LC_BUILD_VERSION:
            platform, mn = struct.unpack_from("<II", data, p + 8)
            if platform == PLATFORM_MACOS:
                minos = version(mn)
        elif cmd == LC_VERSION_MIN_MACOSX:
            minos = version(struct.unpack_from("<I", data, p + 8)[0])
        p += size
    return CPU.get(cputype & 0xFFFFFFFF, hex(cputype)), minos


def slices(data):
    if struct.unpack_from(">I", data, 0)[0] == 0xCAFEBABE:  # universal
        n = struct.unpack_from(">I", data, 4)[0]
        return [slice_minos(data, struct.unpack_from(">iiIII", data, 8 + 20 * i)[2]) for i in range(n)]
    return [slice_minos(data, 0)]


WIDGET = os.path.join("Contents", "PlugIns", "SoundyaWidget.appex")


def check(app):
    errors, plist = check_bundle(app)
    widget = os.path.join(app, WIDGET)
    if plist is not None and os.path.isdir(widget):
        werr, wplist = check_bundle(widget)
        errors += [f"виджет: {e}" for e in werr]
        if wplist is not None:
            ext = wplist.get("NSExtension", {}).get("NSExtensionPointIdentifier")
            if ext != "com.apple.widgetkit-extension":
                errors.append(f"виджет: NSExtensionPointIdentifier = {ext!r}, нужен com.apple.widgetkit-extension")
            if not os.path.isdir(os.path.join(widget, "Contents", "Resources", "Metadata.appintents")):
                errors.append("виджет: нет Contents/Resources/Metadata.appintents (соберите xcodebuild: scripts/mac-widget/build.sh)")
            for key in ("CFBundleShortVersionString", "CFBundleVersion"):
                if wplist.get(key) != plist.get(key):
                    errors.append(f"виджет: {key} = {wplist.get(key)!r}, у приложения {plist.get(key)!r}")
            if not str(wplist.get("CFBundleIdentifier", "")).startswith(plist.get("CFBundleIdentifier", "") + "."):
                errors.append("виджет: CFBundleIdentifier должен начинаться с идентификатора приложения и точки")
            if "NSLocalNetworkUsageDescription" not in wplist:
                errors.append("виджет: нет NSLocalNetworkUsageDescription — macOS 15 не пустит его к колонке")
    return errors


def check_bundle(app):
    """Errors of one bundle (app or extension) and its Info.plist (None if unreadable)."""
    errors = []
    try:
        with open(os.path.join(app, "Contents", "Info.plist"), "rb") as f:
            plist = plistlib.load(f)
    except (OSError, plistlib.InvalidFileException) as e:
        return [f"не читается Info.plist: {e}"], None
    exe = os.path.join(app, "Contents", "MacOS", plist.get("CFBundleExecutable", ""))
    if not os.path.isfile(exe):
        return [f"нет исполняемого файла {exe}"], plist
    if not os.access(exe, os.X_OK):
        errors.append(f"{exe} не исполняемый (chmod 755)")
    promised = parse_version(plist.get("LSMinimumSystemVersion", "10.0"))
    with open(exe, "rb") as f:
        data = f.read()
    for cpu, minos in slices(data):
        if minos is None:
            errors.append(f"{cpu}: в бинарнике нет минимальной версии macOS")
        elif minos > promised:
            errors.append(
                f"{cpu}: собран для macOS {'.'.join(map(str, minos))}, а Info.plist обещает "
                f"{'.'.join(map(str, promised))} — на более старой системе не запустится "
                "(соберите с MACOSX_DEPLOYMENT_TARGET)"
            )
    return errors, plist


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    problems = check(sys.argv[1])
    for p in problems:
        print("check_app:", p, file=sys.stderr)
    if problems:
        sys.exit(1)
    print("check_app: ok")
