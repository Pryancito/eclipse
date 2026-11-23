#!/usr/bin/env python3
"""qemu-oom-stress.py: run tools/oom-stress in QEMU and report what the kernel did.

Boots the minimal x86_64 image (the same QEMU command `make justrun` runs, with
the memory and CPU count replaced), fetches the static `oomstress` binary from
a web server on the host, and runs each case. The cases only see errnos and
signals; this side watches the serial console for what they cannot see:

  panic   "KERNEL PANIC", "KERNEL STOP", "[oops]" (contained or not)
  hang    the case did not finish, or the shell does not answer afterwards
  leak    MemFree after the case far below MemFree before it

After a panic or a hang the guest is rebooted and the next case runs on a
fresh one, so one bad case does not hide the rest.

  gcc -static -O2 -o /tmp/srv/oomstress tools/oom-stress/oomstress.c -lpthread
  make image ARCH=x86_64 VARIANT=minimal
  scripts/qemu-oom-stress.py --srv /tmp/srv -m 1G -s 4 [CASE ...]

Exit status: 0 when every case passed, 1 otherwise. A summary table goes to
stdout and the full console to --log.
"""
import argparse
import http.server
import os
import re
import shlex
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

CASES = [
    "anon", "policy", "many_small", "fork_flat", "fork_bomb", "threads",
    "thread_churn", "pipes", "unix", "fdpass", "tcp", "udp", "fdobjs", "fdexit", "sysv",
    "vmas", "mmap_churn", "memfd", "served",
]

PROMPT = re.compile(r"/ # ")
# What a dead or wounded kernel prints. "[oops]" lines are the kernel saying
# it contained a fault: the machine lives, but it is a kernel bug all the same.
KERNEL_BAD = re.compile(
    r"KERNEL PANIC|KERNEL STOP|panicked at|\[oops\]|STACK NEAR OVERFLOW|"
    r"Alloction Stack Failed|memory allocation of \d+ bytes failed|"
    r"\[heap-wild\]|#DF double fault|DEADLOCK"
)
# What the kernel's memory machinery says while it works. Not bugs: context.
KERNEL_OOM = re.compile(r"\[oom\][^\r\n]*|reclaimd: freed[^\r\n]*|kmem[^\r\n]*refus[^\r\n]*")
END = re.compile(r"OOMS-END (\S+) status=(\S+)([^\r\n]*)")


def qemu_command(mem, smp):
    """The command `make justrun` would run for the minimal x86_64 image."""
    out = subprocess.run(
        ["make", "-s", "-n", "-C", os.path.join(ROOT, "zCore"), "justrun",
         "ARCH=x86_64", "LINUX=1", "MODE=release", "VARIANT=minimal"],
        capture_output=True, text=True, check=True,
    ).stdout
    line = next(l for l in out.splitlines() if l.lstrip().startswith("qemu-system-x86_64"))
    args = shlex.split(line)
    for i, a in enumerate(args):
        if a == "-m":
            args[i + 1] = mem
        elif a == "-smp":
            args[i + 1] = str(smp)
        elif a == "mon:stdio":
            args[i] = "stdio"
    # No monitor on stdio (so a stray Ctrl-A cannot reach it), no graphics.
    if "-display" not in args and "-nographic" not in args:
        args += ["-display", "none"]
    return args


class Guest:
    def __init__(self, cmd, log):
        self.cmd = cmd
        self.log = log
        self.buf = ""
        self.lock = threading.Lock()
        self.proc = None

    def boot(self, timeout=300):
        self.buf = ""
        self.proc = subprocess.Popen(
            self.cmd, cwd=os.path.join(ROOT, "zCore"), stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0,
        )
        threading.Thread(target=self._reader, daemon=True).start()
        if not self.wait_for(PROMPT, timeout):
            return False
        time.sleep(3)  # let the boot's own services settle
        return self.sh("echo ready", 30) is not None

    def _reader(self):
        while True:
            data = self.proc.stdout.read(4096)
            if not data:
                return
            text = data.decode("utf-8", "replace")
            self.log.write(text)
            self.log.flush()
            with self.lock:
                self.buf += text

    def mark(self):
        with self.lock:
            return len(self.buf)

    def since(self, pos):
        with self.lock:
            return self.buf[pos:]

    def wait_for(self, pattern, timeout, pos=0):
        end = time.time() + timeout
        while time.time() < end:
            if pattern.search(self.since(pos)):
                return True
            if self.proc.poll() is not None:
                return False
            time.sleep(0.2)
        return False

    def type(self, line):
        try:
            self.proc.stdin.write(line.encode() + b"\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            pass

    def sh(self, cmd, timeout):
        """Run `cmd` in the guest shell; answer its output, or None if the
        shell did not come back in time."""
        tag = "QD%d" % int(time.time() * 1000)
        pos = self.mark()
        self.type("%s; echo %s:$?" % (cmd, tag))
        if not self.wait_for(re.compile(r"(?m)^%s:\d+\r*$" % tag), timeout, pos):
            return None
        out = self.since(pos)
        return out[: out.rfind(tag + ":")]

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()


def memfree(text):
    m = re.search(r"MemFree:\s+(\d+)", text or "")
    return int(m[1]) if m else None


def serve(directory, port):
    handler = lambda *a, **k: http.server.SimpleHTTPRequestHandler(*a, directory=directory, **k)
    http.server.SimpleHTTPRequestHandler.log_message = lambda *a: None
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", port), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("cases", nargs="*", default=CASES)
    ap.add_argument("-m", "--mem", default="1G")
    ap.add_argument("-s", "--smp", type=int, default=4)
    ap.add_argument("--srv", required=True, help="directory holding the static oomstress binary")
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--arg", action="append", default=[], help="CASE=ARG passed to a case")
    ap.add_argument("--log", default="oom-stress-console.log")
    ap.add_argument("--leak-kib", type=int, default=32768,
                    help="MemFree lost across a case that counts as a leak")
    opts = ap.parse_args()
    args = dict(a.split("=", 1) for a in opts.arg)

    serve(opts.srv, opts.port)
    cmd = qemu_command(opts.mem, opts.smp)
    log = open(opts.log, "w")
    guest = Guest(cmd, log)
    results = []
    booted = False

    for case in opts.cases:
        if not booted:
            guest.stop()
            if not guest.boot():
                results.append((case, "no-boot", "guest did not reach a shell", ""))
                continue
            got = guest.sh("cd /tmp && wget -q -O oomstress http://10.0.2.2:%d/oomstress && chmod +x oomstress"
                           % opts.port, 120)
            if got is None or "oomstress" not in (guest.sh("ls /tmp", 20) or ""):
                results.append((case, "no-tool", "wget of oomstress failed", ""))
                guest.stop()
                continue
            booted = True
        before = memfree(guest.sh("cat /proc/meminfo", 30))
        pos = guest.mark()
        log.write("\n=== CASE %s ===\n" % case)
        guest.type("/tmp/oomstress %s %s" % (case, args.get(case, "")))
        # The tool has its own deadline per case (at most 600 s) and kills the
        # case at it; past that plus a margin the guest itself is stuck.
        done = guest.wait_for(re.compile(r"OOMS-DONE"), 720, pos)
        out = guest.since(pos)
        bad = sorted(set(KERNEL_BAD.findall(out)))
        oom = KERNEL_OOM.findall(out)
        end = END.search(out)
        status = end[2] if end else "no-end"
        detail = end[3].strip() if end else ""
        alive = guest.sh("cat /proc/meminfo", 60) if guest.proc.poll() is None else None
        after = memfree(alive)
        if alive is None:
            status = "HANG" if guest.proc.poll() is None else "DEAD"
            booted = False
        elif not done:
            status = "HANG(case)"
            booted = False
        elif status == "hang":
            # The tool gave up on the case and killed it, but what it left
            # (a process still exiting, say) would weigh on every case after.
            booted = False
        if bad:
            status = "KERNEL:" + status
        leak = ""
        if before and after and before - after > opts.leak_kib:
            # Memory an exit gives back can take a few reclaim passes; only
            # what is still missing 10 s later is a leak.
            time.sleep(10)
            later = memfree(guest.sh("cat /proc/meminfo", 60))
            if later and before - later > opts.leak_kib:
                leak = "lost %d MiB" % ((before - later) // 1024)
        notes = "; ".join(bad + oom[:3] + ([leak] if leak else []))
        results.append((case, status, detail, notes))
        print("%-14s %-24s %s %s" % (case, status, detail, notes), flush=True)
        if bad and guest.proc.poll() is not None:
            booted = False

    guest.stop()
    print("\n%-14s %-24s %s" % ("case", "status", "notes"))
    for case, status, detail, notes in results:
        print("%-14s %-24s %s %s" % (case, status, detail, notes))
    return 0 if all(r[1] == "pass" for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
