"""Run Zircon core-tests with explicit skips and separate kernel/guest logs."""
import argparse
import fnmatch
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
GROUPS = {
    "ipc-port": ",".join([
        "ChannelCallEtcTest.*", "ChannelWriteEtcTest.*", "IOVecTest.*", "FifoTest.*",
        "SocketTest.*", "StreamTestCase.*", "TimerTest.*", "PortTest.PortTimeout",
        "PortTest.AsyncWait*", "PortTest.Event*", "PortTest.Channel*", "PortTest.Cancel*",
        "PortTest.ThreadEvents", "PortTest.Timestamp", "PortTest.Edge*", "PortTest.Create*",
        "PortTest.Wait*", "PortTest.QueueWaitVerifyUserPacket", "PortTest.QueueNullPtrReturnsInvalidArgs",
        "PortTest.QueueAndClose", "PortTest.QueueWrongType", "PortTest.QueueAccessDenied",
    ]),
    "port-stress": ",".join("PortStressTest." + name for name in [
        "CancelKeyDuringMatchRace", "CancelKeyActiveObserverRace", "CancelKeySharedKeyRace",
        "CancelKeyDuringRegistrationRace", "QueuePacketAfterPortClosedConcurrentRace",
        "CancelKeyDestructorReentersPortLock",
    ]),
}


# The longest value a kernel option carries into the guest. Measured, not
# guessed: a `--gtest_filter=` 257 characters long arrived as 187. What is past
# it is cut off without a word of warning, and a cut-off pattern matches
# nothing at all -- which is half of why no selection ever took effect.
MAX_OPTION_VALUE = 187


UNCLASSIFIED = "Not yet classified for this core-tests image"


def outcomes(output):
    """What the guest said about each test it started.

    A test that was started and never reported is `DID-NOT-FINISH`: the process
    died inside it. That is a result too, and the one that matters most, so it
    is named rather than counted as absent.
    """
    results = {name: "DID-NOT-FINISH" for name in re.findall(r"\[ RUN      \] (\S+)", output)}
    for status, name in re.findall(r"\[(       OK |  FAILED  |  SKIPPED )\] (\S+\.\S+)", output):
        if name in results:
            results[name] = status.strip()
    return results


def unmet(expected, results):
    """The requested tests that did not come back passing, in order."""
    return [name for name in expected if results.get(name) not in ("OK", "SKIPPED")]


def verdict(selection, text, expected, returncode, timed_out):
    """The status of one run, plus what came back and what did not.

    Pure on purpose, and the reason is the bug it fixes: the verdict used to be
    computed only on the path where the guest exited by itself. A run the
    timeout killed reported `0/N requested passed` with every name `NOT-RUN`,
    even with the whole log in hand -- 408 `[       OK ]` lines in the run that
    made this obvious. A kill says the run did not *finish*; it says nothing
    about the tests that did, and hiding them makes progress look like none.
    """
    results, missing = {}, list(expected)
    if selection == "-l":
        complete = bool(discovered_tests(text)) and "*** Exit status 0 ***" in text
    else:
        # The verdict is per requested test, not per boot: with one wildcard
        # per run the guest also runs tests nobody asked for, and whether
        # THOSE fail is the expectations file's business, not this run's.
        results = outcomes(text)
        missing = unmet(expected, results)
        complete = not missing
    if timed_out:
        return "TIMEOUT", results, missing
    return ("OK" if returncode == 0 and complete else "FAILED"), results, missing


def guest_filters(patterns):
    """One `--gtest_filter` pattern per run, because the guest takes ONE.

    `--gtest_filter` is a single pattern there: a comma does not separate
    patterns (measured -- a comma-joined list of 40 real test names matched
    nothing), and a colon, which is what gtest itself separates with, cannot
    travel because this kernel's command line separates its own options with
    colons. So a batch of individual names cannot be expressed at all. What can
    is one wildcard per test case, which is what this returns.
    """
    suites = []
    for pattern in patterns:
        suite = pattern.split(".", 1)[0]
        if not suite.replace("_", "").isalnum():
            # A pattern with no literal test case to stand on (`*` among them):
            # nothing narrower is expressible, so ask for everything and let
            # the host sort the answers out.
            return ["*"]
        if suite not in suites:
            suites.append(suite)
    return [suite + ".*" for suite in suites] or ["*"]


def discovered_tests(output):
    suite = None
    tests = []
    for line in output.splitlines():
        if re.fullmatch(r"[\w/]+", line):
            suite = line
        elif suite and re.fullmatch(r"  \.[\w/]+", line):
            tests.append(suite + line.strip())
    return tests


def load_expectations(arch, libos):
    mode = "libos" if libos else "bare"
    platform = f"{arch}-{mode}"
    path = ROOT / "tests/testcases/zircon_core_test" / f"{platform.replace('-', '_')}.txt"
    if not path.exists():
        path = path.with_name(f"x86_64_{mode}.txt")
    cases = {}
    for line in path.read_text().splitlines():
        fields = line.split()
        if len(fields) == 2 and not line.startswith("#"):
            cases[fields[0]] = None if fields[1] == "OK" else f"Existing classification: {fields[1]}"
    overrides = json.loads((ROOT / "scripts/test_expectations/zircon.json").read_text())
    for pattern in overrides.get("supported", []):
        for name in cases:
            if fnmatch.fnmatchcase(name, pattern):
                cases[name] = None
    cases.update(platform_entries(overrides["skips"], platform))
    return cases, platform_entries(overrides.get("needs_hardware", []), platform)


def platform_entries(entries, platform):
    """The `{test, platforms, reason}` entries that apply to this platform."""
    return {
        entry["test"]: entry["reason"]
        for entry in entries
        if any(fnmatch.fnmatchcase(platform, pattern) for pattern in entry["platforms"])
    }


def triage(available, patterns, expectations, hardware, supported, include_skipped):
    """Split the catalog into what this run asks the guest for, what it skips,
    and what it cannot even attempt here.

    A test in `hardware` is one measured to be unreachable on an emulated CPU,
    not one that fails: it is never run and never counted as passed, but it
    keeps its own name and reason so it does not vanish from the tally.
    """
    selected, skipped, needs_hardware = [], {}, {}
    for name in sorted(available):
        if not any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns):
            continue
        if name in hardware and not include_skipped:
            needs_hardware[name] = hardware[name]
            continue
        reason = expectations.get(name, UNCLASSIFIED)
        if name not in expectations and any(fnmatch.fnmatchcase(name, pattern) for pattern in supported):
            reason = None
        if reason and not include_skipped:
            skipped[name] = reason
        else:
            selected.append(name)
    return selected, skipped, needs_hardware


def summary_line(passed, requested, runs, failures, skipped, needs_hardware, log_dir):
    """The one line a reader takes the run's state from.

    `requested` counts only what the guest was asked for, so the tests held
    back for lack of hardware get their own number here instead of being
    folded into either side of the ratio.
    """
    return (
        f"{passed}/{requested} requested tests passed; {runs} runs, "
        f"{failures} failed; {skipped} explicitly skipped; "
        f"{needs_hardware} not run for lack of hardware. Logs: {log_dir}"
    )


class Runner:
    def __init__(self, args):
        self.args = args
        self.log_dir = Path(args.log_dir or ROOT / "target/test-logs" / f"zircon-{args.arch}-{'libos' if args.libos else 'bare'}").resolve()
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.results = []
        # LINUX=0 is not optional: zCore/Makefile defaults to `LINUX ?= 1`, so
        # without it `make` builds a Linux-personality kernel and this driver
        # then hands it a Zircon ZBI. It boots into create_root_fs looking for a
        # root disk that is not there and aborts before a single test is listed.
        self.make = ["make", "-C", str(ROOT / "zCore"), "MODE=release", "LINUX=0", "ZBI=core-tests", "TEST=1", "BOOT_DISK_READONLY=on", f"ARCH={args.arch}"]
        for key, value in [("SMP", args.smp), ("X64_CPU", args.x64_cpu), ("qemu", args.qemu)]:
            if value is not None:
                self.make.append(f"{key}={value}")
        if args.libos:
            self.make.append("LIBOS=1")

    def run(self, selection, expected=()):
        number = len(self.results)
        label = re.sub(r"[^a-zA-Z0-9_.-]", "_", selection)[:100]
        prefix = self.log_dir / f"{number:04}-{label}"
        kernel_log = str(prefix) + ".kernel.log"
        # The option the test program actually reads. Its `main` never looks at
        # `argc`/`argv` -- disassembled: it touches neither `edi` nor `rsi` --
        # and instead looks up five flag NAMES (`--gtest_filter=`,
        # `--gtest_repeat=`, `--gtest_list_tests`, `--gtest_shuffle`,
        # `--gtest_also_run_disabled_tests`) among the boot options and builds
        # its own argument list from them. So the selection travels as a kernel
        # option spelled exactly like the gtest flag.
        #
        # What was here before was `core-tests=<selection>`, a key that does not
        # occur anywhere in the test binary, so every run of this driver ran the
        # whole suite whatever was asked for -- the listing included.
        #
        # `VDSOFORCE=1` used to be here as well, because the vDSO clock came out
        # negative under TCG and hung the run. The kernel now routes the time
        # calls through itself when it cannot publish an honest scale, so the
        # flag is no longer load-bearing: three boots with it and four without
        # stop at the same test with the same code. `userboot=` went too; it
        # named a program that does not exist and the guest warned about it on
        # every boot.
        option = "--gtest_list_tests" if selection == "-l" else "--gtest_filter=" + selection
        if len(option) > MAX_OPTION_VALUE:
            raise ValueError(f"the guest would cut this option off: {option}")
        cmdline = "LOG=info:userboot.shutdown:" + option
        env = dict(os.environ, ZCORE_KERNEL_LOG=kernel_log)
        if self.args.libos:
            zircon_arch = {"x86_64": "x64", "aarch64": "arm64", "riscv64": "riscv64"}[self.args.arch]
            command = [str(ROOT / "target/release/zcore"), str(ROOT / f"prebuilt/zircon/{zircon_arch}/core-tests.zbi"), cmdline]
        else:
            command = self.make + [f"KERNEL_LOG={kernel_log}", f"CMDLINE={cmdline}", "justrun"]
        start = time.monotonic()
        # A wildcard run is a different animal from the old one-name batches:
        # `PortStressTest.*` took 511 s here in one boot, so the floor is set
        # well clear of that rather than at it.
        slow = selection in ("-l", "*") or "PortStressTest." in selection
        limit = max(self.args.timeout, 1200) if slow else self.args.timeout
        with open(str(prefix) + ".host.log", "wb") as errors:
            proc = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=errors, start_new_session=True)
            try:
                output, _ = proc.communicate(timeout=limit)
                timed_out = False
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                output, _ = proc.communicate()
                timed_out = True
        self.output = output.decode(errors="replace")
        status, results, missing = verdict(
            selection, self.output, expected, proc.returncode, timed_out
        )
        Path(str(prefix) + ".guest.log").write_bytes(output)
        record = {
            "selection": selection,
            "status": status,
            "seconds": round(time.monotonic() - start, 3),
            "returncode": proc.returncode,
            "requested": len(expected),
            "passed": len(expected) - len(missing),
            "unmet": {name: results.get(name, "NOT-RUN") for name in missing},
            "guest_skipped": re.findall(r"\[  SKIPPED \] (\S+\.\S+)", self.output),
            "log": str(prefix),
        }
        self.results.append(record)
        print(
            f"{status}: {selection} ({record['seconds']}s, exit={proc.returncode}, "
            f"{record['passed']}/{record['requested']} requested passed)",
            flush=True,
        )
        if status != "OK":
            # Name what was asked for and did not come back, which is the whole
            # point of asking. The tail of the log follows for the rest.
            for name, outcome in list(record["unmet"].items())[:20]:
                print(f"  {outcome}: {name}", flush=True)
            if len(missing) > 20:
                print(f"  ... and {len(missing) - 20} more", flush=True)
            print(output.decode(errors="replace")[-5000:], flush=True)
            print(f"Diagnostics: {prefix}.kernel.log and {prefix}.host.log", flush=True)
        return status == "OK"

    def finish(self, skipped, needs_hardware=None):
        needs_hardware = needs_hardware or {}
        (self.log_dir / "results.json").write_text(
            json.dumps({"skipped": skipped, "needs_hardware": needs_hardware, "runs": self.results}, indent=2) + "\n"
        )
        failures = sum(result["status"] != "OK" for result in self.results)
        passed = sum(result["passed"] for result in self.results)
        requested = sum(result.get("requested", 0) for result in self.results)
        # Named one by one, with the measurement next to each: a test nobody
        # could run here is the one outcome a bare number would read as green.
        for name, reason in sorted(needs_hardware.items()):
            print(f"  NOT RUN (needs hardware): {name}: {reason}")
        print(summary_line(passed, requested, len(self.results), failures, len(skipped), len(needs_hardware), self.log_dir))
        return failures == 0 and bool(self.results)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("-l", "--libos", action="store_true")
    parser.add_argument("-a", "--arch", choices=["x86_64", "aarch64", "riscv64"], default="x86_64")
    parser.add_argument("-f", "--fast", action="store_true", help="compatibility option; known failures are explicitly skipped in all CI runs")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("-t", "--test", help="comma-separated positive filter")
    selection.add_argument("--group", choices=GROUPS)
    parser.add_argument("--include-skipped", action="store_true", help="opt into known failing or unsupported cases for debugging")
    parser.add_argument("--smp", type=int, choices=range(1, 9))
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--x64-cpu")
    parser.add_argument("--qemu")
    parser.add_argument("--log-dir")
    args = parser.parse_args()
    runner = Runner(args)
    if not args.skip_build:
        subprocess.run(runner.make + ["build"], check=True)
    if not runner.run("-l"):
        runner.finish({})
        return 1
    available = discovered_tests(runner.output)
    expectations, hardware = load_expectations(args.arch, args.libos)
    config = json.loads((ROOT / "scripts/test_expectations/zircon.json").read_text())
    selection = args.test or (GROUPS[args.group] if args.group else "*")
    selected, skipped, needs_hardware = triage(
        available, selection.split(","), expectations, hardware, config.get("supported", []), args.include_skipped
    )
    for name, reason in sorted(needs_hardware.items()):
        print(f"NEEDS-HARDWARE: {name}: {reason}")
    for name, reason in sorted(skipped.items()):
        print(f"SKIP: {name}: {reason}")
    if not selected:
        print("No runnable tests matched the selection", file=sys.stderr)
        runner.finish(skipped, needs_hardware)
        return 1
    # One run per wildcard, and the wildcards come from the test cases the
    # selection names. Individual names cannot be batched into one run (see
    # `guest_filters`), so each run also sweeps up tests nobody asked for; they
    # are reported but they do not decide the run.
    for pattern in guest_filters(selection.split(",")):
        prefix = pattern[:-1] if pattern.endswith(".*") else ""
        covered = [name for name in selected if name.startswith(prefix)]
        if covered:
            runner.run(pattern, expected=covered)
    return 0 if runner.finish(skipped, needs_hardware) else 1


if __name__ == "__main__":
    sys.exit(main())
