#!/usr/bin/env python3
"""qemu-sysfuzz.py: run tools/sysfuzz in QEMU and collect every kernel crash it finds.

Boots the minimal x86_64 image the way scripts/qemu-oom-stress.py does (and
with its Guest class), fetches the static `sysfuzz` binary from a web server
on the host and runs it in rounds of --round seconds, each from a new seed.
The console is watched for what a kernel says when it breaks (the same
patterns as the OOM stress driver) and for a round that stops printing.

On a crash the round's last "FZ-PROG n seed" line names the program that was
running. The guest is rebooted, that one program is replayed with -v so the
console shows every call it made up to the crash, and the run goes on from
the next seed. Each finding is written to --out as the replay's console.

  gcc -static -O2 -o /tmp/srv/sysfuzz tools/sysfuzz/sysfuzz.c
  make image ARCH=x86_64 VARIANT=minimal
  scripts/qemu-sysfuzz.py --srv /tmp/srv -m 2G -s 4 --minutes 60

Exit status: 0 when no round found anything, 1 otherwise.
"""
import argparse
import importlib.util
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("oomstress", os.path.join(HERE, "qemu-oom-stress.py"))
oom = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(oom)

PROG = re.compile(r"FZ-PROG (\d+) (0x[0-9a-f]+)")
DONE = re.compile(r"FZ-DONE progs=(\d+) hangs=(\d+)")
HANG = re.compile(r"FZ-HANG (0x[0-9a-f]+)")
CALL = re.compile(r"FZ-CALL \d+ [^\r\n]*")


def last_prog(text):
    found = PROG.findall(text)
    return found[-1][1] if found else None


def fetch(guest, port):
    got = guest.sh("cd /tmp && wget -q -O sysfuzz http://10.0.2.2:%d/sysfuzz && chmod +x sysfuzz" % port, 120)
    return got is not None and "sysfuzz" in (guest.sh("ls /tmp", 20) or "")


def boot(guest, port):
    guest.stop()
    return guest.boot() and fetch(guest, port)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("-m", "--mem", default="2G")
    ap.add_argument("-s", "--smp", type=int, default=4)
    ap.add_argument("--srv", required=True, help="directory holding the static sysfuzz binary")
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--seed", type=lambda v: int(v, 0), default=int(time.time()))
    ap.add_argument("--minutes", type=float, default=30)
    ap.add_argument("--round", type=int, default=300, help="seconds per sysfuzz run")
    ap.add_argument("--calls", type=int, default=48)
    ap.add_argument("--log", default="sysfuzz-console.log")
    ap.add_argument("--out", default="sysfuzz-findings")
    opts = ap.parse_args()

    os.makedirs(opts.out, exist_ok=True)
    oom.serve(opts.srv, opts.port)
    log = open(opts.log, "w")
    guest = oom.Guest(oom.qemu_command(opts.mem, opts.smp), log)
    deadline = time.time() + opts.minutes * 60
    seed = opts.seed
    findings, hangs, programs, rounds = [], [], 0, 0
    up = False

    while time.time() < deadline:
        if not up:
            if not boot(guest, opts.port):
                print("guest did not come up with the tool; stopping", flush=True)
                break
            up = True
        pos = guest.mark()
        log.write("\n=== ROUND seed=%#x ===\n" % seed)
        guest.type("/tmp/sysfuzz -s %#x -t %d -c %d" % (seed, opts.round, opts.calls))
        rounds += 1
        # The round ends itself; past it plus a margin, or when the console
        # goes quiet for a minute mid-round, the kernel is stuck.
        end = time.time() + opts.round + 120
        quiet_since, last_len = time.time(), guest.mark()
        status = None
        while time.time() < end:
            out = guest.since(pos)
            if DONE.search(out):
                status = "done"
                break
            if oom.KERNEL_BAD.search(out):
                time.sleep(3)  # let the rest of the report reach the log
                status = "crash"
                break
            if guest.proc.poll() is not None:
                status = "dead"
                break
            if guest.mark() != last_len:
                quiet_since, last_len = time.time(), guest.mark()
            elif time.time() - quiet_since > 60:
                status = "stuck"
                break
            time.sleep(0.5)
        out = guest.since(pos)
        programs += sum(1 for _ in PROG.finditer(out))
        hangs += HANG.findall(out)
        if status == "done" and guest.sh("echo alive", 60) is None:
            status = "stuck"
        if status in (None, "crash", "dead", "stuck"):
            culprit = last_prog(out)
            bad = sorted(set(oom.KERNEL_BAD.findall(out)))
            print("round %#x: %s, last program %s %s" % (seed, status or "timeout", culprit, bad), flush=True)
            up = False
            if culprit and boot(guest, opts.port):
                rpos = guest.mark()
                guest.type("/tmp/sysfuzz -r %s -c %d -v" % (culprit, opts.calls))
                guest.wait_for(re.compile(r"FZ-DONE|" + oom.KERNEL_BAD.pattern), 90, rpos)
                time.sleep(3)
                rout = guest.since(rpos)
                again = bool(oom.KERNEL_BAD.search(rout)) or guest.sh("echo alive", 60) is None
                calls = CALL.findall(rout)
                path = os.path.join(opts.out, "%s.txt" % culprit)
                with open(path, "w") as f:
                    f.write("round seed %#x, status %s, %s\n" % (seed, status, bad))
                    f.write("replay %s\n\n" % ("reproduces" if again else "did NOT reproduce"))
                    f.write("--- round console tail ---\n%s\n" % out[-6000:])
                    f.write("--- replay console ---\n%s\n" % rout)
                findings.append((culprit, status, again, calls[-1] if calls else "", bad))
                print("  replay %s: %s; last call %s" % (culprit, "reproduces" if again else "no repro",
                                                         calls[-1] if calls else "?"), flush=True)
                up = not again
        seed += 1

    guest.stop()
    print("\n%d rounds, %d programs, %d user-side hangs, %d kernel findings" %
          (rounds, programs, len(hangs), len(findings)))
    for culprit, status, again, call, bad in findings:
        print("  %s %-6s %-12s %s %s" % (culprit, status, "repro" if again else "no-repro", call, bad))
    if hangs:
        print("  programs that outlived their deadline (kernel or user, check by hand): %s" % " ".join(hangs[:20]))
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
