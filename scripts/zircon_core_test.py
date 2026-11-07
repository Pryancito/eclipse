"""Run Zircon core-tests with explicit skips and separate kernel/guest logs."""
import argparse
import collections
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


# What the verdict reads out of a run: the gtest markers, the listing, and the
# exit line. Everything else the guest prints -- the kernel's own log at the
# level asked for, which is most of it -- goes to the guest log on disk and
# nowhere else. A bare-metal guest talks over a serial console that ends its
# lines with `\r\n`, and `$` only looks past a trailing `\n`: anchored without
# the `\r?`, the listing of a QEMU run was dropped whole and discovery came
# back empty while the libos run, which prints bare `\n`, was fine.
KEPT_LINE = re.compile(rb"^(\[ *(RUN|OK|FAILED|SKIPPED|-+|=+) *\]|\*\*\* Exit status|[\w/]+\r?$|  \.[\w/]+\r?$)")

# Where a line of the kernel's own log begins: the logger's grey escape, the
# bracket and the uptime. Six harts write one serial line, and a gtest line
# comes out cut in two by a kernel line now and then (measured here, 3 boots
# of 12 on riscv64: `ProcessTest.KillChannelHandleCycle` lost its `[ RUN ]`;
# and in CI on 2150924, `Threads.ProcessStartOnSecondThread` lost its `[ OK ]`
# and was reported DID-NOT-FINISH with 1002 cases passed around it). The two
# halves are put back together before the verdict reads them.
KERNEL_LINE_START = re.compile(rb"\x1b\[37m\[ *\d+\.\d+ ")

# How many of the guest's last lines a failed run shows.
TAIL_LINES = 60


def sift(stream, tail=TAIL_LINES):
    """Read a guest's output to its end, keeping only what the verdict needs
    and the last `tail` lines for the human.

    Everything used to be held in memory and then decoded, searched and sliced
    whole. A full libos run at `LOG=info` prints several gigabytes -- one
    `ChannelTest.*` run alone was 286 MB, `ReadFaultDoesNotLeakHandles` logs
    263K reads -- and the host killed the driver for memory (12 GB resident)
    before it could say what the guest had done.
    """
    kept, last = [], collections.deque(maxlen=tail)
    cut = b""
    for line in stream:
        last.append(line)
        if cut:
            # The rest of a gtest line the kernel interrupted: a whole
            # kernel line in between is skipped, anything else is its tail.
            if KERNEL_LINE_START.match(line):
                continue
            line, cut = cut + line, b""
        if not KEPT_LINE.match(line):
            continue
        interrupted = KERNEL_LINE_START.search(line, 1)
        if interrupted:
            cut = line[: interrupted.start()]
            continue
        kept.append(line)
    if cut:
        kept.append(cut + b"\n")
    return b"".join(kept).decode(errors="replace"), b"".join(last).decode(errors="replace")


def outcomes(output):
    """What the guest said about each test it started.

    A test that was started and never reported is `DID-NOT-FINISH`: the process
    died inside it. That is a result too, and the one that matters most, so it
    is named rather than counted as absent.
    """
    results = {name: "DID-NOT-FINISH" for name in re.findall(r"\[ RUN      \] (\S+)", output)}
    for status, name, timed in re.findall(
        r"\[(       OK |  FAILED  |  SKIPPED )\] (\S+\.\S+)( \(\d+ ms\))?", output
    ):
        # A per-case verdict carries its timing; the `[  FAILED  ]` lines of
        # gtest's closing list do not, and they name cases nobody asked for.
        # The verdict stands on its own: with six harts writing one serial
        # line, a `[ RUN ]` line comes out spliced now and then (measured
        # here, 3 boots of 12: `ProcessTest.KillChannelHandleCycle` had its
        # `[ OK ]` and no `[ RUN ]`, and was reported NOT-RUN).
        if name in results or timed:
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


def guest_filters(patterns, keep_out=()):
    """One `--gtest_filter` pattern per run, because the guest takes ONE.

    `--gtest_filter` is a single positive pattern there: a colon, which is
    what gtest separates patterns with, cannot travel because this kernel's
    command line separates its own options with colons. So a batch of
    individual names cannot be expressed. What can is one wildcard per test
    case, which is what this returns.

    `keep_out` names the cases that must not run in the guest at all -- the
    ones that never finish, which with gtest's shuffle take the rest of
    their suite's run with them. gtest's `POSITIVE-NEGATIVE:NEGATIVE` form
    carries them, with a comma for the colon: the loader turns a comma in
    this one option back into gtest's colon (`guest_options` in
    `loader/src/zircon.rs`), which is why a comma-joined list of real names
    used to match nothing: gtest never saw a separator.
    """
    suites = []
    for pattern in patterns:
        suite = pattern.split(".", 1)[0]
        if not suite.replace("_", "").isalnum():
            # A pattern with no literal test case to stand on (`*` among them):
            # nothing narrower is expressible, so ask for everything and let
            # the host sort the answers out.
            suites = ["*"]
            break
        if suite not in suites:
            suites.append(suite)
    filters = []
    for suite in suites or ["*"]:
        positive = "*" if suite == "*" else suite + ".*"
        # A hanger may be a pattern on the suite side too (`Sync*.SignalR*`),
        # so it is the suite that is matched against it, not the other way
        # round: a prefix test left `SyncCompletionTest.*` without its
        # negative, and the run hung on the very case the entry names.
        negatives = [
            name
            for name in keep_out
            if suite == "*" or fnmatch.fnmatchcase(suite, name.split(".", 1)[0])
        ]
        # `POSITIVE:-NEGATIVE`, with the colon BEFORE the minus. zxtest's
        # matcher walks the whole option as one C string and only `:` or the
        # end stops a pattern, so in `Suite.*-Suite.A` the positive wildcard
        # has to swallow `-Suite.A` too and matches nothing (measured: 0
        # tests from 0 test cases). With the colon the positive ends where
        # it should.
        filters.append(positive + (",-" + ",".join(negatives) if negatives else ""))
    return filters


def hanging_cases(overrides, platform):
    """The skips that must stay out of the guest, not just out of the tally:
    `"hangs": true` on a skip entry says the case never finishes here."""
    return {
        entry["test"]: entry["reason"]
        for entry in overrides.get("skips", [])
        if entry.get("hangs") and any(fnmatch.fnmatchcase(platform, pattern) for pattern in entry["platforms"])
    }


def discovered_tests(output):
    suite = None
    tests = []
    for line in output.splitlines():
        if re.fullmatch(r"[\w/]+", line):
            suite = line
        elif suite and re.fullmatch(r"  \.[\w/]+", line):
            tests.append(suite + line.strip())
    return tests


def expectations_file(platform, root=None):
    """The `Name  OK|FAILED` table for `platform` (`riscv64-bare`): this
    repository's `scripts/test_expectations/<platform>.txt` when it has one,
    else the `tests` submodule's, else the submodule's x86_64 table for the
    same mode.

    The submodule is upstream's (`rcore-os/zcore-tests`), so a table this
    repository measures on its own -- riscv64 bare metal, which upstream never
    classified -- lives here, next to `zircon.json`, where a tanda can commit
    it. With no table of its own an architecture used to fall back to x86_64's
    silently, and count only the cases x86_64 happened to pass."""
    root = root or ROOT
    name = platform.replace("-", "_") + ".txt"
    own = root / "scripts/test_expectations" / name
    if own.exists():
        return own
    path = root / "tests/testcases/zircon_core_test" / name
    if path.exists():
        return path
    return path.with_name(f"x86_64_{platform.split('-', 1)[1]}.txt")


def load_expectations(arch, libos):
    mode = "libos" if libos else "bare"
    platform = f"{arch}-{mode}"
    cases = {}
    for line in expectations_file(platform).read_text().splitlines():
        fields = line.split()
        if len(fields) == 2 and not line.startswith("#"):
            cases[fields[0]] = None if fields[1] == "OK" else f"Existing classification: {fields[1]}"
    overrides = json.loads((ROOT / "scripts/test_expectations/zircon.json").read_text())
    supported = supported_patterns(overrides, platform)
    for pattern in supported:
        for name in cases:
            if fnmatch.fnmatchcase(name, pattern):
                cases[name] = None
    cases.update(platform_entries(overrides["skips"], platform))
    return (
        cases,
        platform_entries(overrides.get("needs_hardware", []), platform),
        hanging_cases(overrides, platform),
        supported,
    )


def supported_patterns(overrides, platform):
    """The `supported` patterns that apply to `platform`: a bare string applies
    everywhere, a `{test, platforms}` entry only where it says.

    A pattern promoted on the strength of one platform's runs must not reach
    the others: the 284 cases measured green on libos x86_64 (#2499) went in
    as bare strings and the bare-metal x86_64 job, which cannot run most of
    them, asked for 744 cases instead of 468 and died in
    `Property.RegisterGsNoncanonical`, which QEMU cannot pass."""
    patterns = []
    for entry in overrides.get("supported", []):
        if isinstance(entry, str):
            patterns.append(entry)
        elif any(fnmatch.fnmatchcase(platform, pattern) for pattern in entry["platforms"]):
            patterns.append(entry["test"])
    return patterns


def platform_entries(entries, platform):
    """The `{test, platforms, reason}` entries that apply to this platform."""
    return {
        entry["test"]: entry["reason"]
        for entry in entries
        if any(fnmatch.fnmatchcase(platform, pattern) for pattern in entry["platforms"])
    }


def triage(available, patterns, expectations, hardware, supported, include_skipped, hangs=None):
    """Split the catalog into what this run asks the guest for, what it skips,
    and what it cannot even attempt here.

    A test in `hardware` is one measured to be unreachable on an emulated CPU,
    not one that fails: it is never run and never counted as passed, but it
    keeps its own name and reason so it does not vanish from the tally.

    A test in `hangs` never finishes in the guest, and one of those costs the
    rest of its suite's run; it stays skipped even when the known failures are
    opted into, unless a pattern names it by its full name -- which is someone
    debugging that very case.
    """
    hangs = hangs or {}
    selected, skipped, needs_hardware = [], {}, {}
    for name in sorted(available):
        if not any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns):
            continue
        if name in hardware and not include_skipped:
            needs_hardware[name] = hardware[name]
            continue
        # A hang entry may be a pattern (`InterruptTest.Untriggered*Wait`):
        # the guest option that keeps the hangers out is capped at
        # MAX_OPTION_VALUE characters, and a pattern is shorter than the name.
        hang = next((why for pattern, why in hangs.items() if fnmatch.fnmatchcase(name, pattern)), None)
        if hang is not None and not (include_skipped and name in patterns):
            skipped[name] = hang
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


def budget(selection, arch, libos, timeout):
    """Seconds one boot may take before it is killed.

    A wildcard run is a different animal from the old one-name batches:
    `PortStressTest.*` took 511 s here in one boot, so the floor is set well
    clear of that rather than at it. Bare metal riscv64 runs under QEMU TCG
    (no KVM for a foreign architecture): the same whole-suite boot that takes
    60 s hosted was 1200 s in and still inside the Port suites here (PortTest
    alone 264 s, `PortTest.TooManyObservers` 196 s), so its floor is an hour.
    The cases that never finish stay out by the hang entries; a floor is for
    the ones that do.

    The whole suite is still the whole suite when hang entries trail it as
    negative patterns (`*,-Property.RegisterFsNoncanonical,...`): the first
    x86_64 boot with such a list was cut at the 90 s default with 284 of 468
    cases passed and the guest in the middle of `PortStressTest`, because the
    selection no longer compared equal to `*`."""
    whole = selection.split(",", 1)[0] in ("-l", "*")
    slow = whole or "PortStressTest." in selection
    floor = 3600 if arch == "riscv64" and not libos else 1200
    return max(timeout, floor) if slow else timeout


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
        # `kernel.test.ram.reserve`: RAM for the physical-VMO cases, which
        # otherwise take the test process down (see the loader's
        # `reserve_test_ram`). The size is the one the ZBI's own command line
        # asks for; the test reads the option back from `boot-options.txt`,
        # which this kernel builds from ITS command line, so it has to be
        # said here too. Bare metal as well: without it the riscv64 run died
        # in `StreamTestCase.NoStreamFromContiguousOrPhysicalVmo` (an
        # `EXPECT` on `boot_options->test_ram_reserve`, then the dereference
        # anyway) with 80 cases still to run.
        reserve = "kernel.test.ram.reserve=0x8000:"
        cmdline = f"LOG={self.args.guest_log}:userboot.shutdown:{reserve}" + option
        env = dict(os.environ, ZCORE_KERNEL_LOG=kernel_log)
        if self.args.libos:
            zircon_arch = {"x86_64": "x64", "aarch64": "arm64", "riscv64": "riscv64"}[self.args.arch]
            command = [str(ROOT / "target/release/zcore"), str(ROOT / f"prebuilt/zircon/{zircon_arch}/core-tests.zbi"), cmdline]
        else:
            command = self.make + [f"KERNEL_LOG={kernel_log}", f"CMDLINE={cmdline}", "justrun"]
        start = time.monotonic()
        limit = budget(selection, self.args.arch, self.args.libos, self.args.timeout)
        # The guest writes its log straight to disk; see `sift` for why it is
        # not read back whole.
        guest_log = str(prefix) + ".guest.log"
        with open(str(prefix) + ".host.log", "wb") as errors, open(guest_log, "wb") as guest:
            proc = subprocess.Popen(command, cwd=ROOT, env=env, stdout=guest, stderr=errors, start_new_session=True)
            try:
                proc.wait(timeout=limit)
                timed_out = False
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
                timed_out = True
        with open(guest_log, "rb") as guest:
            self.output, tail = sift(guest)
        status, results, missing = verdict(
            selection, self.output, expected, proc.returncode, timed_out
        )
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
            print(tail, flush=True)
            print(f"Diagnostics: {guest_log} and {prefix}.host.log", flush=True)
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
    # `warn` by default: at `info` the kernel traces every syscall, which for a
    # whole-suite run is gigabytes of guest log (and the artifact CI uploads),
    # and most of the run's wall time is spent printing it. `info` is for
    # reproducing one case: `-t Suite.Case --guest-log info`.
    parser.add_argument("--guest-log", choices=["error", "warn", "info", "debug", "trace"], default="warn",
                        help="the kernel's log level inside the guest (default: warn)")
    args = parser.parse_args()
    runner = Runner(args)
    if not args.skip_build:
        subprocess.run(runner.make + ["build"], check=True)
    if not runner.run("-l"):
        runner.finish({})
        return 1
    available = discovered_tests(runner.output)
    expectations, hardware, hangs, supported = load_expectations(args.arch, args.libos)
    selection = args.test or (GROUPS[args.group] if args.group else "*")
    selected, skipped, needs_hardware = triage(
        available, selection.split(","), expectations, hardware, supported, args.include_skipped, hangs
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
    for pattern, covered in planned_runs(selected, selection.split(","), hangs):
        runner.run(pattern, expected=covered)
    return 0 if runner.finish(skipped, needs_hardware) else 1


def planned_runs(selected, patterns, hangs):
    """The guest runs that cover `selected`, as `(filter, expected)` pairs.

    The cases that never finish stay out of the guest's filter. The ones
    the selection asked for by name (and that triage therefore selected)
    are the exception, and each gets a run of its own with its exact name
    as the filter: a hang entry is a pattern, and the pattern stays in the
    suite's negative filter so the other cases it covers keep out. Dropping
    the pattern instead let the opted-in case run but every other hanger
    under it too; keeping it with the case among `selected` left the case
    filtered out of the guest and reported missing.
    """
    opted_in = [
        name for name in selected if any(fnmatch.fnmatchcase(name, pattern) for pattern in hangs)
    ]
    runs = [(name, [name]) for name in opted_in]
    for pattern in guest_filters(patterns, sorted(hangs)):
        positive = pattern.split(",", 1)[0]
        prefix = positive[:-1] if positive.endswith(".*") else ""
        covered = [name for name in selected if name.startswith(prefix) and name not in opted_in]
        if covered:
            runs.append((pattern, covered))
    return runs


if __name__ == "__main__":
    sys.exit(main())
