"""Run the busybox suite from the tests submodule with this tree's boot knobs.

`tests/` is the upstream `rcore-os/zcore-tests` submodule, so its scripts
cannot be edited here -- the same reason `scripts/linux_libc_test.py` exists.
Two things have to be adjusted for them to run against this kernel:

* `ROOTPROC=<cmd>` no longer means "run this and nothing else". In this tree it
  is a deprecated alias for `SHELL`, the program on each virtual terminal, and
  PID 1 is `INIT` (`/sbin/init` -> `eclipse-init`), which brings up the whole
  desktop and never exits. Every case therefore ran the desktop instead of the
  busybox applet under test. `SHELL=:INIT=<cmd>` is what the submodule means:
  no terminal shells, and the applet as PID 1.

* The submodule's 10 s budget covers the whole `make ... justrun`, UEFI and
  kernel boot included. A passing case takes ~14 s here, so every one of them
  was killed as a TIMEOUT before it could finish.
"""
import argparse
import os
from pathlib import Path
import runpy
import sys

ROOT = Path(__file__).resolve().parents[1]

# Per case, covering QEMU start, UEFI, the kernel boot and the applet itself.
# A passing case is ~14 s in QEMU without KVM; the rest is headroom so that a
# slow runner reports a real failure rather than a timeout.
TIMEOUT = 60


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", default="x86_64", choices=["x86_64", "aarch64", "riscv64"])
    parser.add_argument("--fast", action="store_true")
    args = parser.parse_args()
    os.chdir(ROOT / "tests")
    sys.path.insert(0, str(ROOT / "tests"))
    from utils import test as framework

    class EclipseRunner(framework.TestRunner):
        def run_one(self, name, fast=False, timeout=None):
            original = self.run_cmdline
            self.run_cmdline = lambda case: original(case).replace(
                "ROOTPROC=", "SHELL=:INIT="
            )
            try:
                return super().run_one(name, fast, TIMEOUT)
            finally:
                self.run_cmdline = original

    framework.TestRunner = EclipseRunner
    sys.argv = ["linux_other_test.py", "--arch", args.arch] + (["--fast"] if args.fast else [])
    runpy.run_path("linux_other_test.py", run_name="__main__")


if __name__ == "__main__":
    main()
