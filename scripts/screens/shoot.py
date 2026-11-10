#!/usr/bin/env python3
"""Phone screenshots of the panel on demo data (demo_server.py must be running):

    python3 scripts/screens/demo_server.py 8899 &
    python3 scripts/screens/shoot.py            → docs/screens/{home,music,plans,remote}.jpg

Headless Chrome on a Mac keeps windows at least ~500 px wide, so the page goes into an
iframe exactly a phone wide (the demo server's /wrap) and the shot is cropped to it; the tab
is opened after the page has its settings, as a user would.
"""
import os
import subprocess
import sys
import tempfile

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
W, H, SCALE = 390, 844, 3
BASE = os.environ.get("DEMO", "http://127.0.0.1:8899/")
# name: (tab to open after loading, page fragment)
SHOTS = {"home": ("home", ""), "music": ("mine", ""), "plans": ("plans", ""), "remote": ("remote", ""), "glass": ("", "%23glass")}


def shoot(name, tab, frag, out_dir, w=W, h=H):
    tmp = tempfile.mkdtemp()
    png = os.path.join(tmp, "shot.png")
    url = f"{BASE}wrap?w={w}&h={h}&tab={tab}&frag={frag}"
    subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", f"--force-device-scale-factor={SCALE}",
                    f"--window-size={max(w, 600)},{h}", "--virtual-time-budget=9000", f"--user-data-dir={tmp}/profile",
                    f"--screenshot={png}", url], check=True, capture_output=True)
    img = Image.open(png).convert("RGB").crop((0, 0, w * SCALE, h * SCALE))
    out = os.path.join(out_dir, f"{name}.jpg")
    img.save(out, "JPEG", quality=90)
    return out


if __name__ == "__main__":
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "docs", "screens")
    os.makedirs(out_dir, exist_ok=True)
    for name, (tab, frag) in SHOTS.items():
        print(shoot(name, tab, frag, out_dir, w=430 if name == "glass" else W, h=780 if name == "glass" else H))
