#!/usr/bin/env python3
"""Boot Eclipse OS in QEMU, kill and freeze every userspace server while a
client uses it, and fail when a client sees it.

A microkernel's promise is that a driver or a server can die, or hang, and
the system goes on: eclipse-init starts it again, the kernel keeps what was
in flight for the next one. Only a running system with every server up can
tell whether it does. This boots the image with a scratch disk on each
disk controller a userspace driver serves (virtio-blk, NVMe, AHCI), the
virtio keyboard and tablet, the e1000e and the HD Audio controller, and runs
`scripts/crash-servers-guest.sh` there, which says PASS, FAIL, XFAIL (a
known gap) or SKIP for each check.

    cargo image --arch x86_64 --variant minimal
    make -C zCore build ARCH=x86_64 LINUX=1 MODE=release VARIANT=minimal
    scripts/qemu-crash-servers.py [--arch ARCH] [--log FILE] [check...]

A check is a name or a family the guest script knows (disk, fs, net, input,
audio; disk.unvme.frozen). Needs mkfs.vfat and mtools for the scratch disks.

Exit status: 0 every check passed or is a known gap, 1 some failed (or a
known gap passed: take it off the guest script's list), 2 the guest never got
as far as the summary (no shell, a panic, a hang).
"""
import argparse
import base64
import functools
import hashlib
import http.server
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
GUEST_SCRIPT = ROOT / "scripts/crash-servers-guest.sh"
PROMPT_TIMEOUT = 900  # UEFI + kernel + init, without KVM
RUN_TIMEOUT = 2400
PANIC = re.compile(r"panicked at|KERNEL PANIC|#GP|General Protection|Double Fault")


class Guest:
    def __init__(self, arch, extra, log):
        cmd = [
            "make", "-C", str(ROOT / "zCore"), "justrun",
            "MODE=release", "LINUX=1", f"ARCH={arch}", "VARIANT=minimal",
            "GRAPHIC=off", "ACCEL=0", "AUDIO=off", "SMP=2",
        ] + extra
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, start_new_session=True,
        )
        self.lines = queue.Queue()
        self.log = open(log, "w", errors="replace")
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        buf = b""
        while True:
            chunk = self.proc.stdout.read1(4096)
            if not chunk:
                self.lines.put(None)
                return
            buf += chunk
            *done, buf = buf.split(b"\n")
            for raw in done:
                line = raw.decode("utf-8", "replace").rstrip("\r")
                self.log.write(line + "\n")
                self.log.flush()
                self.lines.put(line)

    def send(self, text):
        self.proc.stdin.write(text.encode())
        self.proc.stdin.flush()

    def expect(self, pattern, timeout, on_line=None):
        """The first line matching pattern, or None on timeout / exit."""
        end = time.time() + timeout
        rx = re.compile(pattern)
        while time.time() < end:
            try:
                line = self.lines.get(timeout=1)
            except queue.Empty:
                continue
            if line is None:
                return None
            if on_line:
                on_line(line)
            if PANIC.search(line):
                print(f"guest: {line}", file=sys.stderr)
            m = rx.search(line)
            if m:
                return m
        return None

    def run(self, command, timeout=120):
        """Run a shell command, wait for it, return its exit status. The
        marker goes on a line of its own: a command may end in a here
        document."""
        tag = f"DONE{time.time_ns() % 100000000}"
        self.send(f"{command}\necho {tag}=$?\n")
        m = self.expect(rf"^{tag}=(\d+)", timeout)
        return int(m.group(1)) if m else None

    def push(self, src, dst):
        """Copy a file in over the console, in base64 a few lines at a time:
        the serial port has a 16-byte FIFO and the shell reads it in cooked
        mode, so a whole file at once would lose bytes. Tried three times."""
        data = Path(src).read_bytes()
        want = hashlib.sha256(data).hexdigest()
        lines = base64.encodebytes(data).decode().split()
        for _ in range(3):
            self.run(f"rm -f {dst}.b64")
            for i in range(0, len(lines), 16):
                chunk = "\n".join(lines[i:i + 16])
                self.run(f"cat >> {dst}.b64 <<'EOF'\n{chunk}\nEOF", 60)
            self.run(f"base64 -d {dst}.b64 > {dst}; chmod 755 {dst}; rm -f {dst}.b64", 120)
            self.send(f"echo PUSHSUM $(sha256sum < {dst})\n")
            m = self.expect(r"PUSHSUM ([0-9a-f]{64})", 60)
            if m and m.group(1) == want:
                return
            print(f"push of {src}: the guest's copy differs; trying again", file=sys.stderr)
        raise RuntimeError(f"push of {src}: checksum mismatch in the guest")

    def stop(self):
        try:
            os.killpg(self.proc.pid, 15)
        except ProcessLookupError:
            pass
        self.proc.wait(timeout=30)
        self.log.close()


def scratch_disk(path):
    subprocess.run(["bash", str(ROOT / "tools/zx-virtio-blk/mkfsdisk.sh"), str(path)], check=True)


def serve_blob(directory):
    """A web server on the host with an 8 MiB /blob: what the network checks
    fetch, at 10.0.2.2 from the guest. Its port."""
    (directory / "blob").write_bytes(os.urandom(8 << 20))
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_):
            pass

    handler = functools.partial(Quiet, directory=str(directory))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server.server_address[1]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--arch", default="x86_64", choices=["x86_64", "aarch64", "riscv64"])
    ap.add_argument("--log", default="crash-servers-qemu.log", help="the whole console output")
    ap.add_argument("checks", nargs="*")
    args = ap.parse_args()

    work = Path(tempfile.mkdtemp(prefix="crash-servers."))
    for name in ("udisk", "unvme", "uahci"):
        scratch_disk(work / f"{name}.img")
    port = serve_blob(work)
    extra = [f"UDISK={work}/udisk.img", f"UNVME={work}/unvme.img", "UINPUT=1"]
    if args.arch == "x86_64":
        # q35's own AHCI is there too; the HD Audio controller and the
        # i8042 only on x86_64.
        extra += [f"UAHCI={work}/uahci.img"]

    g = Guest(args.arch, extra, args.log)
    # QEMU runs in a session of its own: a `timeout` or ^C on this script
    # must still take it down, or the next run finds the ESP image locked.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(2))
    try:
        deadline = time.time() + PROMPT_TIMEOUT
        alive = False
        while time.time() < deadline:
            g.send("\necho CRASH_SERVERS_SHELL_UP\n")
            if g.expect(r"^CRASH_SERVERS_SHELL_UP$", 15):
                alive = True
                break
            if g.proc.poll() is not None:
                break
        if not alive:
            print(f"no shell on the console (see {args.log})", file=sys.stderr)
            return 2
        # Every driver up: init starts the last of them after the shell.
        if g.run("i=0; while [ $i -lt 60 ] && ! grep -q 'entering supervision loop'"
                 " /run/eclipse-init.log 2>/dev/null; do sleep 1; i=$((i+1)); done; sleep 10", 120) is None:
            return 2
        g.run("dmesg -n 1 2>/dev/null; stty -echo 2>/dev/null")
        g.push(GUEST_SCRIPT, "/tmp/crash-servers.sh")
        checks = " ".join(args.checks)
        g.send(f"/tmp/crash-servers.sh -p {port} {checks} 2>&1; echo CRASH_SERVERS_RC=$?\n")

        def show(line):
            if re.match(r"^(PASS|FAIL|XFAIL|XPASS|SKIP) |^     \| |^crash-servers: ", line):
                print(line, flush=True)

        m = g.expect(r"^CRASH_SERVERS_RC=(\d+)", RUN_TIMEOUT, show)
        if not m:
            print(f"the checks did not finish (see {args.log})", file=sys.stderr)
            return 2
        rc = int(m.group(1))
        if rc >= 2:
            return 2
        return 1 if rc else 0
    finally:
        g.stop()


if __name__ == "__main__":
    sys.exit(main())
