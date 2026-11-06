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
# The prompt this runner asks the guest shell for, because the shell's own
# carries no newline and a line-buffered reader cannot see it: see
# `BootedRunner.adopt_newline_prompt`.
GUEST_PROMPT = "__ZCORE_PROMPT__"
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
# A line of eclipse-init's own on the console: `[eclipse-init] [<secs>s]
# <message>`. The supervisor shares the serial line with the shell the cases
# run in, and what it says about its services is not the case's output.
INIT_LOG_LINE = re.compile(r"\[eclipse-init\] \[ *\d+\.\d+s\] [^\r\n]*")


def without_kernel_log(output):
    """`output` with the kernel's and eclipse-init's console lines taken out.

    The submodule classifies a case by substring, and one of its failure
    patterns is "ERROR" -- the level the kernel prints every signal death at,
    on purpose, so that it survives `LOG=error`. A `grep` of eclipse-init's
    dying of SIGPIPE while `utime-static.exe` ran (QEMU x86_64, 2026-11-05:
    `[ 19.252076 ERROR 1 0:0 zcore_loader::linux] [exit] pid=1033 (grep)
    killed by signal SIGPIPE`) was enough to fail a case that had exited 0.
    The case's own words stay: libc-test prints `... failed` for a failing
    check, and exits non-zero besides.

    eclipse-init's lines go for the same reason. On riscv64 (QEMU,
    2026-11-06) the image carried a labwc session with no labwc behind it,
    and every minute the supervisor wrote `respawn: seatd exited after
    60.27s (exit 127 -- command not found, or execve failed; ...)` on the
    console: 50 cases that had exited 0 were FAILED on that `failed`. The
    image is fixed (`desktop_lands_in_image` in the xtask), and this keeps
    whatever init says next from being read as a case's verdict."""
    return INIT_LOG_LINE.sub("", KERNEL_LOG_LINE.sub("", ANSI.sub("", output)))


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
                self.adopt_newline_prompt()

            def adopt_newline_prompt(self):
                """Give the guest shell a prompt that ends in a newline, and
                from then on type a case only when that prompt is on screen.

                Why a new prompt at all: the console reader is line-buffered,
                and `/ # ` carries no newline, so the shell's own prompt is
                invisible until the next line arrives -- the echo of a command
                already typed. Waiting for it meant nudging the shell with a
                bare newline, and each nudge is one more interactive read, one
                more termios set, and one more chance to eat the command we
                were in the middle of writing (measured: `nextafter.exe` came
                back `sh: bc-test/src/math/nextafter.exe: not found`, with the
                prompt sitting three bytes into the path). A prompt that ends
                in a newline shows up the moment it is printed, which is after
                that termios set, so seeing it IS the permission to type.

                Best-effort: a guest that will not take the new prompt keeps
                the old behaviour of typing straight after the marker."""
                self.zcore_proc.stdin.write(b"PS1='" + GUEST_PROMPT.encode() + b"\n'\n")
                self.zcore_proc.stdin.flush()
                start = time.monotonic()
                took = False
                while time.monotonic() - start < PROMPT_TIMEOUT:
                    with self.lk:
                        took = GUEST_PROMPT in self.output
                    if took or self.zcore_proc.poll() is not None:
                        break
                    time.sleep(0.05)
                with self.lk:
                    self.output = ""
                if took:
                    self.prompt_pattern = re.compile(re.escape(GUEST_PROMPT))
                    self.prompt_sync = True
                else:
                    print(
                        f"warning: the guest would not take {GUEST_PROMPT} as its "
                        "prompt; cases are typed straight after the marker, as "
                        "they were before",
                        file=sys.stderr,
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
            # Three in libm that are not this kernel's, each run against the
            # same binaries on a Linux host on 2026-11-05: `powf` and `fmal`
            # fail the same way there natively (this musl's libm raises the
            # wrong FP exceptions; the values are right), and `remquol`
            # passes natively but fails identically under `qemu-x86_64` TCG
            # -- musl's x86_64 remquol reads the quotient bits off `fprem1`'s
            # condition codes (`fnstsw`), and QEMU 8.2 leaves C0/C1/C3 clear
            # where the hardware sets them (a two-instruction probe: native
            # 0x7000, TCG 0x3000). So it is the emulator the job runs on, and
            # it would pass on real hardware.
            "/libc-test/src/math/fmal.exe": "this musl's libm, same failure natively on Linux: special/fmal.h:46 wants INEXACT|UNDERFLOW, gets INEXACT",
            "/libc-test/src/math/powf.exe": "this musl's libm, same failure natively on Linux: ucb/powf.h pow(x,1) raises INEXACT|OVERFLOW / INEXACT|UNDERFLOW, five cases",
            "/libc-test/src/math/remquol.exe": "QEMU TCG: fprem1 leaves the quotient condition codes clear, so musl's remquol writes 0 into *quo (passes natively, fails the same under qemu-x86_64)",
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
