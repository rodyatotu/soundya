#!/usr/bin/env python3
"""End-to-end test of the desktop installer's install step, with the real
install.sh: installer (host build) → ADB → tests/fake_station.py --exec, which
runs commands for real and hangs up the session's process group like the
speaker's adbd. The package is the real one with an x86 soundya inside.

Checks: install.sh starts in the background and survives the hang-up, its
log lines reach the window, the bar follows its steps, the result is «ok»;
a broken package fails with install.sh's own reason, not after minutes.

    cargo build && (cd installer && cargo build) && python3 tests/installer_e2e.py
"""
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ok = 0


def check(cond, what):
    global ok
    if not cond:
        print("FAIL:", what)
        sys.exit(1)
    ok += 1
    print("ok  ", what)


def package(tmp, broken=False):
    """dist/soundya with the host soundya inside, checksums redone."""
    src = os.path.join(tmp, "pkg", "soundya")
    shutil.rmtree(os.path.join(tmp, "pkg"), ignore_errors=True)
    shutil.copytree(os.path.join(ROOT, "dist", "soundya"), src)
    shutil.copy(os.path.join(ROOT, "target", "debug", "soundya"), os.path.join(src, "soundya"))
    # install.sh puts the package's init script in place: a fake one that the test can run.
    open(os.path.join(src, "S99soundya"), "w").write(INIT)
    for f in ("nfqws", "nfqws2", "shairport-sync"):
        if os.path.exists(os.path.join(src, f)):
            os.remove(os.path.join(src, f))
    sums = subprocess.run("find . -type f ! -name SHA256SUMS | sed 's|^./||' | sort | xargs sha256sum", shell=True, cwd=src,
                          capture_output=True, text=True, check=True).stdout
    open(os.path.join(src, "SHA256SUMS"), "w").write(sums)
    if broken:
        with open(os.path.join(src, "soundya"), "ab") as f:
            f.write(b"x")
    out = os.path.join(tmp, "broken.tar" if broken else "pkg.tar")
    with tarfile.open(out, "w") as t:
        t.add(src, arcname="soundya")
    return out


INIT = """#!/bin/sh
# The real (host) daemon, in the test's own folders.
DIR=$SOUNDYA_INSTALL_DIR; RUN=$SOUNDYA_RUN
case "$1" in
start) SOUNDYA_DIR="$DIR" SOUNDYA_NO_VOICE=1 SOUNDYA_WEB_PORT=18898 nohup "$DIR/soundya" daemon </dev/null >>"$DIR/daemon.log" 2>&1 &
       echo "soundya started";;
stop) pkill -f "$DIR/soundya daemon" 2>/dev/null; rm -f "$RUN/soundya.ready"; sleep 0.5; echo "soundya stopped";;
esac
"""


def fake_init(root):
    p = os.path.join(root, "init")
    open(p, "w").write(INIT)
    os.chmod(p, 0o755)


def get(port, path, body=None):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=None if body is None else json.dumps(body).encode(),
                                 headers={"Host": f"127.0.0.1:{port}", "Origin": f"http://127.0.0.1:{port}", "Content-Type": "application/json"},
                                 method="GET" if body is None else "POST")
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


def run_install(tmp, tar_path, label):
    root = os.path.join(tmp, "root-" + label)
    out = os.path.join(tmp, "out-" + label)
    for d in (root, out, os.path.join(root, "opt"), os.path.join(root, "run")):
        os.makedirs(d, exist_ok=True)
    fake_init(root)
    st = subprocess.Popen([sys.executable, os.path.join(ROOT, "tests", "fake_station.py"), out, "--exec", root])
    env = dict(os.environ, SOUNDYA_INSTALLER_NO_BROWSER="1", SOUNDYA_INSTALLER_TAR=tar_path, HOME=os.path.join(tmp, "home-" + label))
    os.makedirs(env["HOME"], exist_ok=True)
    inst = subprocess.Popen([os.path.join(ROOT, "installer", "target", "debug", "soundya-installer")], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    try:
        port = None
        end = time.time() + 15
        while time.time() < end and port is None:
            line = inst.stdout.readline()
            if "127.0.0.1:" in line:
                port = int(line.split("127.0.0.1:")[1].split("/")[0])
        check(port is not None, f"[{label}] installer is up")
        time.sleep(2)
        get(port, "/api/install", {"ip": "127.0.0.1", "features": {"soundcloud": True}})
        seen, percents, t0 = [], [], time.time()
        while time.time() - t0 < 240:
            j = get(port, "/api/job")
            percents.append(j["percent"])
            seen = j["log"]
            if not j["running"] and j.get("result"):
                return j, seen, percents, time.time() - t0, root, out
            time.sleep(1)
        check(False, f"[{label}] finished in 4 minutes ({seen[-3:]})")
    finally:
        subprocess.run(["pkill", "-f", os.path.join(root, "opt", "soundya") + " daemon"])
        inst.terminate()
        st.terminate()
        inst.wait()
        st.wait()


def main():
    tmp = tempfile.mkdtemp(prefix="soundya-inst-")
    for p in ("/tmp/soundya", "/tmp/soundya-install.done", "/tmp/soundya-install.log", "/tmp/soundya-armv7.tar"):
        if os.path.isdir(p):
            shutil.rmtree(p)
        elif os.path.exists(p):
            os.remove(p)

    j, log, pcts, secs, root, out = run_install(tmp, package(tmp), "ok")
    print("   log:", " | ".join(log))
    check(j["result"] == "ok", f"installed: result {j['result']} {j.get('reason')}")
    check(any("package: checksums OK" in l for l in log), "install.sh's own lines reach the window")
    check(any("update: ok" in l for l in log), "and its result line")
    check(len(set(pcts)) >= 4 and pcts == sorted(pcts), f"the bar moves forward with the steps ({sorted(set(pcts))})")
    check(os.path.exists(os.path.join(root, "opt", "soundya")), "the program is in place")
    check("background" not in " ".join(log).lower() and not any("обычном режиме" in l for l in log), "started in the background (no fallback needed)")
    print(f"   took {secs:.0f} s")

    j, log, pcts, secs, root, out = run_install(tmp, package(tmp, broken=True), "broken")
    check(j["result"] == "failed" and "повреждён" in (j.get("reason") or "") + " ".join(log), f"a damaged package fails with the reason ({j.get('reason')})")
    check(secs < 60, f"and says so at once ({secs:.0f} s)")

    # The old start command, for comparison: unprotected background job + hang-up.
    print(f"\n{ok} checks passed")


if __name__ == "__main__":
    main()
