"""Run the libc suite with reviewed temporary skips outside the tests submodule."""
import argparse
import os
from pathlib import Path
import runpy
import re
import shlex
import sys
import time

ROOT = Path(__file__).resolve().parents[1]

# The guest shell's prompt, and how long one QEMU start may take to show it.
# Boot to userspace is ~30 s on x86_64 and ~15 s on aarch64 under TCG here;
# the rest is headroom for a slow runner, and a boot that never gets there
# is reported as such rather than as 476 timeouts.
PROMPT = re.compile(r"/ # ")
PROMPT_TIMEOUT = 300

# Cases that only mean something to a user the kernel can hold to a limit.
# `pthread_atfork-errno-clobber` sets RLIMIT_NPROC to 0 and expects `fork` to
# fail with EAGAIN; root is exempt from that limit here as in Linux (the same
# binary fails the same way on a Linux host when run as root, and passes as
# `nobody`), and the guest shell is root. So they run as `nobody`, where
# `may_start_task` is what they measure, and they are expected to pass.
# Measured in QEMU x86_64 on 2026-11-05: both pass, and `ulimit -u 1` leaves
# that shell unable to fork at all. Every arch's busybox is built from the same
# source here, `su` included.
UNPRIVILEGED = {
    "/libc-test/src/regression/pthread_atfork-errno-clobber.exe",
    "/libc-test/src/regression/pthread_atfork-errno-clobber-static.exe",
}

# A kernel log line on the console: `[ <secs>.<us> LEVEL <cpu> <pid>:<tid>
# <target>] <message>`, once its colours are stripped. Not anchored to a line
# start: the console interleaves it with whatever userspace was printing, and
# the one measured landed in the middle of the echo of the typed command.
KERNEL_LOG_LINE = re.compile(
    r"\[ *\d+\.\d{6} (?:ERROR|WARN |INFO |DEBUG|TRACE) \d+ \d+:\d+ \S+\] [^\r\n]*"
)
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def without_kernel_log(output):
    """`output` with the kernel's own console lines taken out.

    The submodule classifies a case by substring, and one of its failure
    patterns is "ERROR" -- the level the kernel prints every signal death at,
    on purpose, so that it survives `LOG=error`. A `grep` of eclipse-init's
    dying of SIGPIPE while `utime-static.exe` ran (QEMU x86_64, 2026-11-05:
    `[ 19.252076 ERROR 1 0:0 zcore_loader::linux] [exit] pid=1033 (grep)
    killed by signal SIGPIPE`) was enough to fail a case that had exited 0.
    The case's own words stay: libc-test prints `... failed` for a failing
    check, and exits non-zero besides."""
    return KERNEL_LOG_LINE.sub("", ANSI.sub("", output))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", default="x86_64", choices=["x86_64", "aarch64", "riscv64"])
    parser.add_argument("--libos", action="store_true")
    parser.add_argument("--fast", action="store_true")
    args = parser.parse_args()
    os.chdir(ROOT / "tests")
    sys.path.insert(0, str(ROOT / "tests"))
    if args.libos:
        from utils import test as framework

        class LoggedRunner(framework.TestRunner):
            def run_one(self, name, fast=False, timeout=None):
                log_dir = ROOT / "target/test-logs/linux-libc-libos"
                log_dir.mkdir(parents=True, exist_ok=True)
                prefix = log_dir / re.sub(r"[^a-zA-Z0-9_.-]", "_", name)
                command = self.run_cmdline
                self.run_cmdline = lambda case: (
                    command(case).replace("LOG=error", "LOG=info")
                    + " 2>" + shlex.quote(str(prefix) + ".host.log")
                )
                previous = os.environ.get("ZCORE_KERNEL_LOG")
                os.environ["ZCORE_KERNEL_LOG"] = str(prefix) + ".kernel.log"
                try:
                    return super().run_one(name, fast, timeout)
                finally:
                    self.run_cmdline = command
                    if previous is None:
                        os.environ.pop("ZCORE_KERNEL_LOG", None)
                    else:
                        os.environ["ZCORE_KERNEL_LOG"] = previous

        framework.TestRunner = LoggedRunner
        script = "linux_libc_test.py"
        forwarded = ["--libos"]
    else:
        from utils import test_qemu as framework
        from qemu_completion import completion_runner
        framework.TestRunner = completion_runner(framework.TestRunner, framework.TestStatus)

        completed = framework.TestRunner

        class BootedRunner(completed):
            """The submodule's `run_qemu` gives the guest 5 s to show its
            prompt, then types the first case at it. Booting to the shell
            takes ~30 s on x86_64 and ~15 s on aarch64 under TCG (the busybox
            job, one boot per case, measures the first: ~30 s each), so every
            case of the x86_64 and aarch64 jobs was typed into a guest that
            was still in the firmware, waited 10 s, and came back TIMEOUT with
            no output -- 410 of 410 and 313 of 313 on CI 41946889764 (2026-11-05),
            twice each, with a QEMU restart in between. Wait for the prompt
            itself, within a budget a slow runner can still meet."""

            def run_qemu(self):
                super().run_qemu()
                # A newline makes a shell that is already up print a fresh
                # prompt, and one that is not yet up print it on arrival.
                self.zcore_proc.stdin.write(b"\n")
                self.zcore_proc.stdin.flush()
                start = time.monotonic()
                up = False
                while time.monotonic() - start < PROMPT_TIMEOUT:
                    with self.lk:
                        up = PROMPT.search(self.output) is not None
                    if up or self.zcore_proc.poll() is not None:
                        break
                    time.sleep(0.2)
                with self.lk:
                    output = self.output
                    self.output = ""
                if not up:
                    raise RuntimeError(
                        f"no guest prompt within {PROMPT_TIMEOUT}s of QEMU starting:\n{output}"
                    )

            def classified_output(self, output):
                return without_kernel_log(output)

            def guest_command(self, name):
                if name in unprivileged:
                    return "su -s /bin/sh nobody -c " + shlex.quote(name)
                return super().guest_command(name)

        framework.TestRunner = BootedRunner
        script = "linux_libc_test-qemu.py"
        forwarded = ["--arch", args.arch]
    original = framework.load_testcases
    regressions = {
        ("x86_64", False): {
            "/libc-test/src/math/modfl.exe": "x87 long-double modf mismatch (CI 34011543846)",
            "/libc-test/src/math/log.exe": "log result and FP exception mismatch (CI 34016355478)",
            "/libc-test/src/functional/ipc_sem-static.exe": "SysV semaphore test hangs (CI 34016355478)",
            "/libc-test/src/regression/pthread_rwlock-ebusy-static.exe": "rwlock test hangs after a clean restart (CI 34022646551)",
            # The eight below are what the suite reports now that the image
            # carries it (before the xtask `LIVE_TEST_SUITE` fix the x86_64
            # image had none of it). Measured on 2026-11-05 against d42bc22,
            # all 476 cases typed at one booted guest, nothing hanging: 466
            # pass, these eight do not, and the two `RLIMIT_NPROC` cases pass
            # once run as `nobody` (`UNPRIVILEGED`). Each reason is the
            # guest's own words, so the next tanda can start from the message
            # rather than from a rerun.
            #
            # Three that the harness cannot run from `/`: the DSO is named by
            # a path relative to the working directory, and every case is
            # typed at a shell whose cwd is `/`.
            "/libc-test/src/functional/tls_align.exe": "needs src/functional/tls_align_dso.so, a cwd-relative DT_NEEDED, and the harness runs from / (exit 127)",
            "/libc-test/src/functional/tls_align_dlopen.exe": "dlopen of a cwd-relative src/functional/tls_align_dso.so, and the harness runs from /",
            "/libc-test/src/functional/tls_init_dlopen.exe": "same cwd-relative dlopen; then calls the NULL dlsym result and dies at rip=0 (SIGSEGV), which the kernel reports correctly",
            # Two the suite does not build on this musl at all: absent from
            # rootfs/x86_64/libc-test/src/regression, yet listed OK in
            # tests/testcases/linux_libc_test/x86_64_qemu.txt.
            "/libc-test/src/regression/sem_close-unmap.exe": "not built by libc-test on this musl; the binary is not in the rootfs (exit 127)",
            "/libc-test/src/regression/sem_close-unmap-static.exe": "not built by libc-test on this musl; the binary is not in the rootfs (exit 127)",
            # Three in libm, not in the kernel: the value is right every time
            # and it is the raised FP exceptions, or an out-parameter, that
            # differ.
            "/libc-test/src/math/fmal.exe": "special/fmal.h:46 wants INEXACT|UNDERFLOW, gets INEXACT; the value is exact",
            "/libc-test/src/math/powf.exe": "ucb/powf.h: pow(x,1) raises INEXACT|OVERFLOW and INEXACT|UNDERFLOW where none is wanted, five cases, value right",
            "/libc-test/src/math/remquol.exe": "remquol writes 0 into *quo in EVERY case while returning the right remainder",
        },
        ("aarch64", False): {
            "/libc-test/src/functional/pthread_tsd-static.exe": "thread-specific destructor test hangs (CI 34011543846)",
        },
        ("riscv64", False): {
            "/libc-test/src/regression/pthread_rwlock-ebusy-static.exe": "rwlock contention test hangs (CI 34016355478)",
        },
    }.get((args.arch, args.libos), {})
    if not args.libos:
        for suffix in ("", "-static"):
            regressions[f"/libc-test/src/functional/pthread_cancel{suffix}.exe"] = (
                "cancellation cleanup handlers fail intermittently on baremetal (x64 CI 34023903336, RISC-V CI 34024969778)"
            )
    if not args.libos and args.arch in ("aarch64", "riscv64"):
        # Both linkage variants exercise the same thread exit/join paths.
        # These also hung when retried in a fresh QEMU (CI 34018139132).
        thread_cases = ["regression/pthread_once-deadlock", "functional/tls_init", "regression/pthread_exit-cancel"]
        if args.arch == "aarch64":
            thread_cases.append("functional/pthread_tsd")
            regressions["/libc-test/src/functional/tls_local_exec-static.exe"] = "TLS thread test hangs in fresh QEMU (CI 34018139132)"
            regressions["/libc-test/src/regression/pthread_rwlock-ebusy-static.exe"] = "rwlock test hangs in fresh QEMU (local run with completion markers, 2026-09-06)"
        else:
            thread_cases.append("regression/pthread_rwlock-ebusy")
        for case in thread_cases:
            for suffix in ("", "-static"):
                regressions[f"/libc-test/src/{case}{suffix}.exe"] = (
                    "thread cancellation/exit/join test hangs in fresh QEMU (CI 34016355478, 34018139132)"
                )

    unprivileged = UNPRIVILEGED if not args.libos else set()

    def load_testcases(filename):
        selected = []
        for name, status in original(filename):
            if name in unprivileged:
                status = framework.TestStatus.OK
            reason = regressions.get(name)
            if status != framework.TestStatus.OK:
                reason = reason or f"Existing classification: {status.name}"
            if reason:
                print(f"SKIP: {name}: {reason}", flush=True)
            else:
                selected.append((name, status))
        if not selected:
            raise RuntimeError("No runnable libc tests")
        return selected

    framework.load_testcases = load_testcases
    # The allowlist above applies equally to PR and manual runs.
    sys.argv = [script] + forwarded + (["--fast"] if args.fast else [])
    runpy.run_path(script, run_name="__main__")


if __name__ == "__main__":
    main()
