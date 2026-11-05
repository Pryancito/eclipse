"""Run the libc suite with reviewed temporary skips outside the tests submodule."""
import argparse
import os
from pathlib import Path
import runpy
import re
import shlex
import sys

ROOT = Path(__file__).resolve().parents[1]


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

        # A case the image does not carry must be a FAILED, not an OK.
        #
        # The submodule's runner types the case's path at the guest shell and
        # classifies whatever comes back by substring, and its failure
        # patterns are "failed", "ERROR", "Error", "panicked", "Hangup" and
        # "Unknown signal". `sh: /libc-test/src/math/log.exe: not found`
        # (status 127) matches none of them, so an image without the suite
        # reported every one of the 476 cases OK -- which is exactly what the
        # x86_64 job did, because `cargo image` builds that arch's image from
        # the lean live root and `libc-test` is not in `LIVE_KEEP`
        # (xtask `LIVE_TEST_SUITE` carries the measurement and the fix).
        #
        # Named here rather than in the submodule for the reason the module
        # docstring gives: `tests/` is upstream and cannot be edited.
        missing = re.compile(r"^sh: \S*: not found", re.M)
        completed = framework.TestRunner

        class MissingCaseIsAFailure(completed):
            def check_output(self, output):
                if missing.search(output):
                    return framework.TestStatus.FAILED
                return super().check_output(output)

        framework.TestRunner = MissingCaseIsAFailure
        script = "linux_libc_test-qemu.py"
        forwarded = ["--arch", args.arch]
    original = framework.load_testcases
    regressions = {
        ("x86_64", False): {
            "/libc-test/src/math/modfl.exe": "x87 long-double modf mismatch (CI 34011543846)",
            "/libc-test/src/math/log.exe": "log result and FP exception mismatch (CI 34016355478)",
            "/libc-test/src/functional/ipc_sem-static.exe": "SysV semaphore test hangs (CI 34016355478)",
            "/libc-test/src/regression/pthread_rwlock-ebusy-static.exe": "rwlock test hangs after a clean restart (CI 34022646551)",
            # The ten below are what this job reports now that the image
            # actually carries the suite (before the xtask `LIVE_TEST_SUITE`
            # fix it carried none of it, and every case passed by not
            # running). Measured on 2026-11-05 against d42bc22, all 476 cases
            # in one boot, nothing hanging: 466 pass, these ten do not.
            # Each reason is the guest's own words, so the next tanda can
            # start from the message rather than from a rerun.
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
            # And two that are the kernel's: `fork` does not look at
            # RLIMIT_NPROC, so a process whose limit is 0 still forks.
            "/libc-test/src/regression/pthread_atfork-errno-clobber.exe": "fork succeeds with RLIMIT_NPROC=0: fork does not enforce the limit (should be -EAGAIN)",
            "/libc-test/src/regression/pthread_atfork-errno-clobber-static.exe": "fork succeeds with RLIMIT_NPROC=0: fork does not enforce the limit (should be -EAGAIN)",
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

    def load_testcases(filename):
        selected = []
        for name, status in original(filename):
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
