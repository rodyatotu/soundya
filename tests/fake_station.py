#!/usr/bin/env python3
"""A fake Yandex Station for testing the installer without one: ADB on :5555
(CNXN without auth, shell:, sync: SEND/DATA/DONE), the panel on :8765.
Files pushed and settings received are written to the directory given.

    python3 tests/fake_station.py <dir> [--existing] [--exec <root>]

--exec runs shell commands for real (install.sh included, with its test
overrides pointing into <root>), the way the speaker's adbd does with the
«shell:» service: on a terminal, so when a session ends its process group
gets SIGHUP. A background job not protected from that dies, as on a speaker.
"""
import http.server
import json
import os
import socket
import struct
import sys
import threading
import time

OUT = sys.argv[1]
EXISTING = "--existing" in sys.argv
EXEC = sys.argv[sys.argv.index("--exec") + 1] if "--exec" in sys.argv else None
os.makedirs(OUT, exist_ok=True)
A_CNXN, A_OPEN, A_OKAY, A_CLSE, A_WRTE = (struct.unpack("<I", x)[0] for x in (b"CNXN", b"OPEN", b"OKAY", b"CLSE", b"WRTE"))
MAXDATA = 4096
state = {"installed_at": None}


def pkt(cmd, a0, a1, data=b""):
    s = sum(data) & 0xFFFFFFFF
    return struct.pack("<6I", cmd, a0, a1, len(data), s, cmd ^ 0xFFFFFFFF) + data


def recv(c):
    h = b""
    while len(h) < 24:
        x = c.recv(24 - len(h))
        if not x:
            raise EOFError
        h += x
    cmd, a0, a1, n, _, _ = struct.unpack("<6I", h)
    d = b""
    while len(d) < n:
        x = c.recv(n - len(d))
        if not x:
            raise EOFError
        d += x
    return cmd, a0, a1, d


def run_real(cmd):
    import signal
    import subprocess
    cmd = cmd.replace("/data/opt/soundya", os.path.join(EXEC, "opt"))
    env = dict(os.environ, SOUNDYA_INSTALL_DIR=os.path.join(EXEC, "opt"), SOUNDYA_INIT=os.path.join(EXEC, "init"),
               SOUNDYA_RUN=os.path.join(EXEC, "run"), SOUNDYA_HEALTH_WAIT="20", SOUNDYA_HEALTH_HOLD="2")
    p = subprocess.Popen(["sh", "-c", cmd], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         preexec_fn=os.setpgrp, env=env)
    out = b""
    while True:
        chunk = p.stdout.read1(65536) if hasattr(p.stdout, "read1") else p.stdout.read(65536)
        if not chunk:
            break
        out += chunk
        if p.poll() is not None and not chunk:
            break
    p.wait()
    # The terminal hangs up: SIGHUP to the session's process group, as adbd's pty does.
    try:
        os.killpg(p.pid, signal.SIGHUP)
    except ProcessLookupError:
        pass
    with open(os.path.join(OUT, "shell.log"), "a") as f:
        f.write(f"$ {cmd[:160]}\n{out.decode(errors='replace')[:400]}\n")
    return out.decode(errors="replace")


def shell(cmd):
    tar = os.path.join(OUT, "soundya-armv7.tar")
    if EXEC and not cmd.startswith("id -u"):
        return run_real(cmd)
    if cmd.startswith("id -u"):
        v = "0.4.40" if EXISTING else ""
        return f"0\nQUASAR\nHOST=yandexmini_2\nMODEL=\nSOUNDYA={'soundya ' + v if v else ''}\n" + ("CONFIG\n" if EXISTING else "")
    if cmd.startswith("wc -c"):
        return str(os.path.getsize(tar)) if os.path.exists(tar) else "0"
    if cmd.startswith("printf"):
        text = cmd.split("'")[1].encode().decode("unicode_escape")
        open(os.path.join(OUT, "features"), "w").write(text)
        return ""
    if "install.sh" in cmd:
        state["installed_at"] = time.time()
        return "STARTED\n"
    if cmd.startswith("cat /tmp/soundya-install.log"):
        t = state["installed_at"]
        if t is None:
            return ""
        lines = ["package: checksums OK", "backup: program in /data/opt/soundya/backup/prev"]
        if time.time() - t > 3:
            lines += ["update: ok", "__DONE__"]
        return "\n".join(lines) + "\n"
    if "update-result" in cmd:
        return "status=ok\nfrom=0.4.40\nto=0.4.41\ntime=1\nreason=\n"
    return ""


def serve(c):
    files = {}
    try:
        cmd, *_ = recv(c)
        assert cmd == A_CNXN
        c.sendall(pkt(A_CNXN, 0x01000001, MAXDATA, b"device::ro.product.name=yandexmini_2;\0"))
        while True:
            cmd, a0, a1, d = recv(c)
            if cmd == A_OPEN:
                svc = d.rstrip(b"\0").decode()
                local = 100 + a0
                c.sendall(pkt(A_OKAY, local, a0))
                if svc.startswith("shell:"):
                    out = shell(svc[6:]).encode()
                    for i in range(0, len(out), MAXDATA):
                        c.sendall(pkt(A_WRTE, local, a0, out[i:i + MAXDATA]))
                        recv(c)  # their OKAY
                    c.sendall(pkt(A_CLSE, local, a0))
                elif svc == "sync:":
                    files[local] = {"buf": b"", "remote": a0, "data": b"", "path": None}
            elif cmd == A_WRTE:
                st = files.get(a1)
                c.sendall(pkt(A_OKAY, a1, a0))
                if st is None:
                    continue
                st["buf"] += d
                while len(st["buf"]) >= 8:
                    tag, n = st["buf"][:4], struct.unpack("<I", st["buf"][4:8])[0]
                    if tag in (b"SEND", b"DATA") and len(st["buf"]) < 8 + n:
                        break
                    if tag == b"SEND":
                        st["path"] = st["buf"][8:8 + n].decode()
                        st["buf"] = st["buf"][8 + n:]
                    elif tag == b"DATA":
                        st["data"] += st["buf"][8:8 + n]
                        st["buf"] = st["buf"][8 + n:]
                    elif tag == b"DONE":
                        st["buf"] = st["buf"][8:]
                        path = st["path"].split(",")[0]
                        name = os.path.basename(path)
                        open(path if EXEC else os.path.join(OUT, name), "wb").write(st["data"])
                        c.sendall(pkt(A_WRTE, a1, a0, b"OKAY\0\0\0\0"))
                    elif tag == b"QUIT":
                        st["buf"] = st["buf"][8:]
                    else:
                        st["buf"] = b""
            elif cmd == A_CLSE:
                pass
    except (EOFError, ConnectionError, OSError):
        pass
    finally:
        c.close()


class Panel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"{}")

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with open(os.path.join(OUT, "settings.jsonl"), "a") as f:
            f.write(body.decode() + "\n")
        self.send_response(200)
        self.end_headers()
        self.wfile.write("OK: сохранено".encode())


threading.Thread(target=http.server.ThreadingHTTPServer(("0.0.0.0", 8765), Panel).serve_forever, daemon=True).start()
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", 5555))
srv.listen(16)
print("fake station ready", flush=True)
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
