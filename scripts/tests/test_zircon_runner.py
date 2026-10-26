import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("zircon_runner", Path(__file__).parents[1] / "zircon_core_test.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)

outcomes, unmet, verdict = runner.outcomes, runner.unmet, runner.verdict
triage, platform_entries, summary_line = runner.triage, runner.platform_entries, runner.summary_line

HARDWARE = {"Property.RegisterFsNoncanonical": "QEMU TCG lets the non-canonical WRFSBASE through"}


class PerTestOutcomes(unittest.TestCase):
    def test_a_passing_case_is_met_and_a_missing_one_is_not(self):
        output = "[ RUN      ] Suite.Case\n[       OK ] Suite.Case (0 ms)\n*** Exit status 0 ***"
        self.assertEqual(runner.outcomes(output), {"Suite.Case": "OK"})
        self.assertEqual(runner.unmet(["Suite.Case"], runner.outcomes(output)), [])
        self.assertEqual(runner.unmet(["Suite.Missing"], runner.outcomes(output)), ["Suite.Missing"])

    def test_a_runtime_skip_counts_as_met(self):
        output = "[ RUN      ] Suite.Case\n[  SKIPPED ] Suite.Case (0 ms)\n"
        self.assertEqual(runner.outcomes(output), {"Suite.Case": "SKIPPED"})
        self.assertEqual(runner.unmet(["Suite.Case"], runner.outcomes(output)), [])

    def test_a_failure_is_not_met(self):
        output = "[ RUN      ] Suite.Case\n[  FAILED  ] Suite.Case (0 ms)\n"
        self.assertEqual(runner.outcomes(output), {"Suite.Case": "FAILED"})
        self.assertEqual(runner.unmet(["Suite.Case"], runner.outcomes(output)), ["Suite.Case"])

    def test_a_case_that_killed_the_process_is_named_as_such(self):
        """The one outcome worth its own word: started, never reported."""
        output = "[ RUN      ] Suite.Dies\n*** Exit status -1028 ***"
        self.assertEqual(runner.outcomes(output), {"Suite.Dies": "DID-NOT-FINISH"})
        self.assertEqual(runner.unmet(["Suite.Dies"], runner.outcomes(output)), ["Suite.Dies"])

    def test_a_case_never_started_is_unmet_without_an_outcome(self):
        self.assertEqual(runner.outcomes(""), {})
        self.assertEqual(runner.unmet(["Suite.Case"], {}), ["Suite.Case"])

    def test_the_summary_tally_is_not_what_decides(self):
        """A run sweeps up tests nobody asked for, so the guest's own total and
        its `[  FAILED  ]` lines cannot be the verdict: only the requested
        names are."""
        output = (
            "[ RUN      ] Asked.Case\n[       OK ] Asked.Case (0 ms)\n"
            "[ RUN      ] Unasked.Case\n[  FAILED  ] Unasked.Case (0 ms)\n"
            "[==========] 2 tests from 2 test cases ran (0 ms total).\n"
            "[  FAILED  ] 1 test, listed below:\n[  FAILED  ] Unasked.Case\n"
        )
        self.assertEqual(runner.unmet(["Asked.Case"], runner.outcomes(output)), [])


class Verdict(unittest.TestCase):
    PASSES = (
        "[ RUN      ] A.One\n[       OK ] A.One (1 ms)\n"
        "[ RUN      ] A.Two\n[       OK ] A.Two (1 ms)\n"
    )

    def test_a_killed_run_still_counts_what_came_back(self):
        """The bug this covers: a run the timeout killed reported every
        requested test `NOT-RUN` and `0/N passed`, with the whole log in hand.
        A kill says the run did not finish, not that nothing ran."""
        status, results, missing = verdict(
            "*", self.PASSES + "[ RUN      ] A.Hangs\n", ["A.One", "A.Two", "A.Hangs"], -9, True
        )
        self.assertEqual(status, "TIMEOUT")
        self.assertEqual(missing, ["A.Hangs"])
        self.assertEqual(results["A.One"], "OK")
        self.assertEqual(results["A.Hangs"], "DID-NOT-FINISH")

    def test_a_clean_run_with_everything_met_is_ok(self):
        status, _, missing = verdict("*", self.PASSES, ["A.One", "A.Two"], 0, False)
        self.assertEqual((status, missing), ("OK", []))

    def test_a_clean_run_is_not_ok_if_the_guest_exited_badly(self):
        """Both halves of the guard: a run can meet every name it was asked
        for and still have taken the whole process down afterwards."""
        status, _, missing = verdict("*", self.PASSES, ["A.One", "A.Two"], 1, False)
        self.assertEqual((status, missing), ("FAILED", []))
        status, _, missing = verdict("*", self.PASSES, ["A.One", "A.Three"], 0, False)
        self.assertEqual((status, missing), ("FAILED", ["A.Three"]))

    def test_the_listing_pass_is_judged_by_the_catalog_and_the_exit_line(self):
        listing = "startup\nSuite\n  .One\n*** Exit status 0 ***"
        self.assertEqual(verdict("-l", listing, [], 0, False)[0], "OK")
        # A catalog that came back empty, or a boot that did not reach the exit
        # line, is not a listing however clean the exit code looks.
        self.assertEqual(verdict("-l", "*** Exit status 0 ***", [], 0, False)[0], "FAILED")
        self.assertEqual(verdict("-l", "startup\nSuite\n  .One\n", [], 0, False)[0], "FAILED")
        self.assertEqual(verdict("-l", listing, [], 0, True)[0], "TIMEOUT")


class NeedsHardware(unittest.TestCase):
    """A case measured to be unreachable on an emulated CPU is neither run nor
    skipped quietly: it keeps its name, its reason and its own number."""

    catalog = ["Property.RegisterFs", "Property.RegisterFsNoncanonical"]

    def triage(self, include_skipped=False):
        return triage(self.catalog, ["*"], {"Property.RegisterFs": None}, HARDWARE, [], include_skipped)

    def test_it_is_never_asked_of_the_guest_and_never_counted_as_passed(self):
        selected, skipped, needs_hardware = self.triage()
        self.assertEqual(selected, ["Property.RegisterFs"])
        self.assertEqual(needs_hardware, HARDWARE)
        self.assertNotIn("Property.RegisterFsNoncanonical", skipped)

    def test_the_reason_travels_with_the_name(self):
        _, _, needs_hardware = self.triage()
        self.assertIn("WRFSBASE", needs_hardware["Property.RegisterFsNoncanonical"])

    def test_debugging_can_still_opt_into_running_it(self):
        selected, _, needs_hardware = self.triage(include_skipped=True)
        self.assertEqual(selected, self.catalog)
        self.assertEqual(needs_hardware, {})

    def test_it_is_not_swallowed_by_the_unclassified_bucket(self):
        """Without the category it was one more `Not yet classified` line, which
        reads as `we have not looked at it` rather than `it cannot run here`."""
        _, skipped, needs_hardware = triage(self.catalog, ["*"], {}, {}, [], False)
        self.assertEqual(skipped["Property.RegisterFsNoncanonical"], runner.UNCLASSIFIED)
        self.assertEqual(needs_hardware, {})

    def test_the_summary_line_gives_it_its_own_number(self):
        line = summary_line(5, 20, 1, 0, 3, 1, "target/test-logs")
        self.assertIn("5/20 requested tests passed", line)
        self.assertIn("1 not run for lack of hardware", line)

    def test_an_entry_only_applies_to_the_platforms_it_names(self):
        entries = [{"test": "Suite.Case", "platforms": ["x86_64-bare"], "reason": "why"}]
        self.assertEqual(platform_entries(entries, "x86_64-bare"), {"Suite.Case": "why"})
        self.assertEqual(platform_entries(entries, "x86_64-libos"), {})
        self.assertEqual(platform_entries([], "x86_64-bare"), {})


class GuestFilters(unittest.TestCase):
    def test_one_wildcard_per_test_case_and_in_the_order_asked(self):
        self.assertEqual(
            runner.guest_filters(["FifoTest.One", "SocketTest.*", "FifoTest.Two"]),
            ["FifoTest.*", "SocketTest.*"],
        )

    def test_a_pattern_with_no_literal_test_case_asks_for_everything(self):
        """`*` cannot be narrowed, and half a wildcard would silently drop the
        tests it does not cover."""
        self.assertEqual(runner.guest_filters(["*"]), ["*"])
        self.assertEqual(runner.guest_filters(["Port*.Cancel*"]), ["*"])
        self.assertEqual(runner.guest_filters(["FifoTest.One", "*"]), ["*"])

    def test_the_groups_this_driver_ships_all_resolve(self):
        for name, selection in runner.GROUPS.items():
            filters = runner.guest_filters(selection.split(","))
            self.assertTrue(filters, name)
            for pattern in filters:
                option = "--gtest_filter=" + pattern
                self.assertLessEqual(len(option), runner.MAX_OPTION_VALUE, f"{name}: {option}")

    def test_port_stress_is_one_run_and_ipc_port_is_one_per_test_case(self):
        self.assertEqual(runner.guest_filters(runner.GROUPS["port-stress"].split(",")), ["PortStressTest.*"])
        self.assertIn("FifoTest.*", runner.guest_filters(runner.GROUPS["ipc-port"].split(",")))


class Catalog(unittest.TestCase):
    def test_catalog_preserves_parameterized_names(self):
        self.assertEqual(runner.discovered_tests("startup text\nSuite\n  .One\n  .Two\n/Variant\n  .Case/arm64\n"), ["Suite.One", "Suite.Two", "/Variant.Case/arm64"])


if __name__ == "__main__":
    unittest.main()
