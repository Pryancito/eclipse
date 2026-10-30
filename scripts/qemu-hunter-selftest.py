#!/usr/bin/env python3
"""Boot Eclipse OS in QEMU and run hunter-selftest on it.

`tools/hunterctl/hunter-selftest` checks every part of hunter on a running
system -- the kernel's gates, hunterctl, hunterd, the reports -- and only a
running system can tell whether they work together: the unit tests check each
piece on the build host, against fake /proc files. This boots the image the
way `make qemu ARCH=x86_64 GRAPHIC=off` does (headless: the serial console is
a root shell), runs the script there and exits with its result.

    make image ARCH=x86_64 && make -C zCore build MODE=release LINUX=1
    scripts/qemu-hunter-selftest.py [--push] [--seal] [--log FILE] [section...]

--push sends this tree's hunter-selftest and hunterctl over the console
first, so a change to either is tested without rebuilding the image (the
kernel and everything else still come from the image).

Exit status: 0 all checks passed, 1 some failed, 2 the guest never got as far
as the summary (no shell, a panic, a hang).
"""
import argparse
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
PROMPT_TIMEOUT = 600  # UEFI + kernel + init, without KVM
RUN_TIMEOUT = 1800
PANIC = re.compile(r"panicked at|KERNEL PANIC|#GP|General Protection|Double Fault")


class Guest:
    def __init__(self, log):
        cmd = [
            "make", "-C", str(ROOT / "zCore"), "justrun",
            "MODE=release", "LINUX=1", "ARCH=x86_64",
            "GRAPHIC=off", "ACCEL=0", "AUDIO=off",
        ]
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
        """Run a shell command, wait for it, return its exit status."""
        tag = f"DONE{time.time_ns() % 100000000}"
        self.send(f"{command}; echo {tag}=$?\n")
        m = self.expect(rf"^{tag}=(\d+)", timeout)
        return int(m.group(1)) if m else None

    def push(self, src, dst):
        """Copy a text file in over the console, a few lines at a time: the
        serial port has a 16-byte FIFO and the shell reads it in cooked mode,
        so a whole file at once would lose bytes."""
        data = Path(src).read_bytes()
        hexed = data.hex()
        self.run(f"rm -f {dst}.hex")
        step = 64
        lines = [hexed[i:i + step] for i in range(0, len(hexed), step)]
        for i in range(0, len(lines), 16):
            chunk = "\n".join(lines[i:i + 16])
            if self.run(f"cat >> {dst}.hex <<'EOF'\n{chunk}\nEOF", 60) != 0:
                raise RuntimeError(f"push of {src} failed")
        rc = self.run(
            f"tr -d '\\n' < {dst}.hex | sed 's/../\\\\x&/g' | xargs -0 printf '%b' > {dst}"
            f" && rm -f {dst}.hex && chmod 755 {dst}"
            f" && [ \"$(sha256sum < {dst} | cut -d' ' -f1)\" = {__import__('hashlib').sha256(data).hexdigest()} ]",
            120,
        )
        if rc != 0:
            raise RuntimeError(f"push of {src}: checksum mismatch in the guest")

    def stop(self):
        try:
            os.killpg(self.proc.pid, 15)
        except ProcessLookupError:
            pass
        self.proc.wait(timeout=30)
        self.log.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--push", action="store_true", help="send this tree's hunter-selftest and hunterctl first")
    ap.add_argument("--seal", action="store_true", help="also check the one-way seal")
    ap.add_argument("--log", default="hunter-selftest-qemu.log", help="the whole console output")
    ap.add_argument("sections", nargs="*")
    args = ap.parse_args()

    g = Guest(args.log)
    try:
        # The shell comes up on the serial console once init is done; ask
        # until it answers.
        deadline = time.time() + PROMPT_TIMEOUT
        alive = False
        while time.time() < deadline:
            g.send("\necho HUNTER_SELFTEST_SHELL_UP\n")
            if g.expect(r"^HUNTER_SELFTEST_SHELL_UP$", 15):
                alive = True
                break
            if g.proc.poll() is not None:
                break
        if not alive:
            print(f"no shell on the console (see {args.log})", file=sys.stderr)
            return 2
        # Quieter console: the kernel's log would interleave with the output.
        g.run("dmesg -n 1 2>/dev/null; stty -echo 2>/dev/null")
        if args.push:
            g.push(ROOT / "tools/hunterctl/hunterctl", "/sbin/hunterctl")
            g.push(ROOT / "tools/hunterctl/hunter-selftest", "/sbin/hunter-selftest")
        opts = " ".join((["--seal"] if args.seal else []) + args.sections)
        g.send(f"/sbin/hunter-selftest {opts} 2>&1; echo HUNTER_SELFTEST_RC=$?\n")
        results = []

        def show(line):
            if re.match(r"^(PASS|FAIL|SKIP) |^     \| |^hunter-selftest: ", line):
                print(line, flush=True)
                results.append(line)

        m = g.expect(r"^HUNTER_SELFTEST_RC=(\d+)", RUN_TIMEOUT, show)
        if not m:
            print(f"hunter-selftest did not finish (see {args.log})", file=sys.stderr)
            return 2
        rc = int(m.group(1))
        if rc >= 125:
            return 2
        return 1 if rc else 0
    finally:
        g.stop()


if __name__ == "__main__":
    sys.exit(main())
