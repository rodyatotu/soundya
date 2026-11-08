#!/usr/bin/env python3
"""Builds the Mac disk image: open it, drag soundya to Applications.

    python3 scripts/mac-dmg/build_dmg.py <soundya.app> <assets dir> <out.dmg>

On a Mac it is made with hdiutil (build_mac): a compressed UDZO image that
macOS mounts quietly once downloaded, with the background alias made on the
mounted volume itself. On Linux (build, below) it is a draft for
sign-on-mac.sh — a downloaded xorriso image is not mounted without a question.

The image is a hybrid made by xorriso with an HFS+ volume «soundya» (what
macOS mounts): soundya.app, a symlink to /Applications, the background
picture in .background/ and a .DS_Store that tells Finder the window size,
icon size, where the two icons stand and which picture is the background.

The background is referred to by a classic alias, which names the picture's
CNIDs on the volume; xorriso numbers files in a fixed order, so the image is
made once to learn them and once more with the real .DS_Store (same names,
same numbers, same volume date from SOURCE_DATE_EPOCH). hfsinfo.py checks it.
"""
import datetime
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "target", "pydeps"))
sys.path.insert(0, HERE)

import hfsinfo  # noqa: E402
from ds_store import DSStore  # noqa: E402
from mac_alias import ALIAS_EJECTABLE_DISK, ALIAS_KIND_FILE, Alias, TargetInfo, VolumeInfo  # noqa: E402

VOLUME = "soundya"
APP = "soundya.app"
# Must match make_assets.py (points).
WINDOW = (660, 420)
APP_AT, APPS_AT = (170, 200), (490, 200)


def xorriso(stage, out, epoch):
    env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
    subprocess.run(
        ["xorriso", "-as", "mkisofs", "-quiet", "-V", VOLUME, "-hfsplus", "-apm-block-size", "2048", "-R", "-no-pad", "-o", out, stage],
        check=True, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def ds_store(path, info):
    """The .DS_Store for the xorriso image: the alias is made from its catalog."""
    by_path = {hfsinfo.path_of(info, e): e for e in info["entries"]}
    folder, pic = by_path["/.background"], by_path["/.background/background.png"]
    created = datetime.datetime(1904, 1, 1, tzinfo=datetime.timezone.utc) + datetime.timedelta(seconds=info["create_date"])
    volume = VolumeInfo(VOLUME, created, b"H+", ALIAS_EJECTABLE_DISK, 0, b"\0\0")
    target = TargetInfo(
        ALIAS_KIND_FILE, "background.png", folder["cnid"], pic["cnid"], created, b"\0\0\0\0", b"\0\0\0\0",
        folder_name=".background", cnid_path=[folder["cnid"]],
        carbon_path=f"{VOLUME}:.background:background.png", posix_path="/.background/background.png",
    )
    write_ds_store(path, Alias(volume=volume, target=target).to_bytes())


def write_ds_store(path, alias):
    """Window size, icon places and the background picture (by `alias`)."""
    if os.path.exists(path):
        os.remove(path)
    with DSStore.open(path, "w+") as d:
        d["."]["vSrn"] = ("long", 1)
        d["."]["bwsp"] = {
            # The frame: the content area is the picture, plus the title bar.
            "WindowBounds": "{{200, 160}, {%d, %d}}" % (WINDOW[0], WINDOW[1] + 28),
            "ShowStatusBar": False, "ShowToolbar": False, "ShowPathbar": False, "ShowSidebar": False,
            "ContainerShowSidebar": False, "SidebarWidth": 0, "ShowTabView": False, "PreviewPaneVisibility": False,
        }
        d["."]["icvp"] = {
            "viewOptionsVersion": 1, "backgroundType": 2, "backgroundImageAlias": alias,
            "backgroundColorRed": 0.055, "backgroundColorGreen": 0.047, "backgroundColorBlue": 0.086,
            "gridOffsetX": 0.0, "gridOffsetY": 0.0, "gridSpacing": 100.0, "arrangeBy": "none",
            "showIconPreview": False, "showItemInfo": False, "labelOnBottom": True,
            "textSize": 13.0, "iconSize": 112.0, "scrollPositionX": 0.0, "scrollPositionY": 0.0,
        }
        d["."]["icvl"] = ("type", b"icnv")
        d[APP]["Iloc"] = APP_AT
        d["Applications"]["Iloc"] = APPS_AT


def build(app, assets, out):
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH") or os.path.getmtime(app))
    stage = tempfile.mkdtemp(prefix="soundya-dmg-")
    try:
        shutil.copytree(app, os.path.join(stage, APP), symlinks=True)
        os.symlink("/Applications", os.path.join(stage, "Applications"))
        os.makedirs(os.path.join(stage, ".background"))
        shutil.copy(os.path.join(assets, "background.png"), os.path.join(stage, ".background", "background.png"))
        store = os.path.join(stage, ".DS_Store")
        open(store, "wb").write(b"\0" * 6148)
        xorriso(stage, out, epoch)  # pass 1: learn the CNIDs
        first = hfsinfo.read(out)
        ds_store(store, first)
        os.remove(out)
        xorriso(stage, out, epoch)  # pass 2: the real .DS_Store
        second = hfsinfo.read(out)
        key = lambda i: sorted((hfsinfo.path_of(i, e), e["cnid"]) for e in i["entries"] if e["parent"] != 1)
        if key(first) != key(second) or first["create_date"] != second["create_date"]:
            raise SystemExit("CNIDs changed between the two passes: the background alias would be wrong")
    finally:
        shutil.rmtree(stage)
    print(f"Built {out} ({os.path.getsize(out) // 1024} KB)")


def hdiutil(*args):
    subprocess.run(["hdiutil", *args], check=True, stdout=subprocess.DEVNULL)


def build_mac(app, assets, out):
    """On a Mac: a read-write image with the files, the .DS_Store written on the
    mounted volume (the alias names the real picture there), then compressed."""
    work = tempfile.mkdtemp(prefix="soundya-dmg-")
    stage, rw, mnt = os.path.join(work, "stage"), os.path.join(work, "rw.dmg"), None
    try:
        os.makedirs(os.path.join(stage, ".background"))
        subprocess.run(["ditto", app, os.path.join(stage, APP)], check=True)
        os.symlink("/Applications", os.path.join(stage, "Applications"))
        shutil.copy(os.path.join(assets, "background.png"), os.path.join(stage, ".background", "background.png"))
        hdiutil("create", "-volname", VOLUME, "-srcfolder", stage, "-fs", "HFS+", "-format", "UDRW", "-ov", rw)
        # Mounted where macOS puts volumes (/Volumes/…): the alias records the path from
        # the volume's root, and a mount point elsewhere would make it wrong.
        out_plist = subprocess.run(["hdiutil", "attach", "-nobrowse", "-noautoopen", "-noverify", "-plist", rw],
                                   check=True, capture_output=True).stdout
        mnt = next(e["mount-point"] for e in plistlib.loads(out_plist)["system-entities"] if e.get("mount-point"))
        try:
            alias = Alias.for_file(os.path.join(mnt, ".background", "background.png"))
            posix = alias.target.posix_path
            if (posix.decode() if isinstance(posix, bytes) else posix) != "/.background/background.png":
                raise SystemExit(f"background alias points to {alias.target.posix_path!r}")
            write_ds_store(os.path.join(mnt, ".DS_Store"), alias.to_bytes())
        finally:
            hdiutil("detach", mnt)
        if os.path.exists(out):
            os.remove(out)
        hdiutil("convert", rw, "-format", "UDZO", "-imagekey", "zlib-level=9", "-o", out)
        hdiutil("verify", out)
    finally:
        if mnt and os.path.ismount(mnt):
            subprocess.run(["hdiutil", "detach", "-quiet", mnt], stderr=subprocess.DEVNULL)
        shutil.rmtree(work, ignore_errors=True)
    print(f"Built {out} ({os.path.getsize(out) // 1024} KB, UDZO)")


if __name__ == "__main__":
    (build_mac if sys.platform == "darwin" else build)(*sys.argv[1:4])
