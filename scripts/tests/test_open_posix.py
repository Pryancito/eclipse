import importlib.util
import io
from contextlib import redirect_stdout
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "open_posix_run", Path(__file__).parents[1] / "open-posix" / "run.py"
)
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)

A = "conformance/interfaces/sem_post/1-1.run-test"
B = "conformance/interfaces/mq_open/1-1.run-test"
C = "conformance/interfaces/pthread_mutex_trylock/4-3.run-test"


class OpenPosixReport(unittest.TestCase):
    def report(self, results, expected):
        out = io.StringIO()
        with redirect_stdout(out):
            code = run.report(results, {}, expected, [A, B, C])
        return code, out.getvalue()

    def test_the_runner_lines_are_read_with_the_output_that_follows(self):
        text = "\n".join(
            [
                "OPTS> echo",
                f"R 0 0.1 {A}",
                f"R 2 0.0 {B}",
                "O mq_open() did not return success: Function not implemented",
                f"R 124 60.0 {C}",
                f"R 139 0.0 conformance/interfaces/x/1-1.run-test",
            ]
        )
        results, output = run.parse(text)
        self.assertEqual(results[A], "PASS")
        self.assertEqual(results[B], "UNRESOLVED")
        self.assertEqual(results[C], "HUNG")
        self.assertEqual(results["conformance/interfaces/x/1-1.run-test"], "SIGNAL")
        self.assertEqual(output[B], ["mq_open() did not return success: Function not implemented"])

    def test_a_test_that_stops_passing_is_a_regression(self):
        code, out = self.report({A: "FAIL", B: "UNRESOLVED", C: "PASS"}, {B: "UNRESOLVED"})
        self.assertEqual(code, 1)
        self.assertIn(A, out)

    def test_an_expected_failure_or_a_flaky_one_is_not(self):
        code, _ = self.report({A: "PASS", B: "UNRESOLVED", C: "HUNG"}, {B: "UNRESOLVED", C: "FLAKY"})
        self.assertEqual(code, 0)

    def test_an_expected_failure_that_passes_is_listed_to_be_removed(self):
        code, out = self.report({A: "PASS", B: "PASS", C: "PASS"}, {B: "UNRESOLVED", C: "FLAKY"})
        self.assertEqual(code, 0)
        self.assertIn(B, out)
        self.assertNotIn(C, out.split("quitarlos")[-1])

    def test_the_expected_list_reads(self):
        expected = run.load_expected()
        self.assertEqual(expected["conformance/interfaces/mq_open/1-1.run-test"], "FAIL")
        self.assertEqual(expected[C], "FLAKY")

    def test_areas(self):
        self.assertEqual(run.area(A), "semaforos")
        self.assertEqual(run.area(B), "colas de mensajes")
        self.assertEqual(run.area("conformance/interfaces/sigaction/1-1.run-test"), "senales")
        self.assertEqual(run.area("conformance/interfaces/fork/1-1.run-test"), "otros")


if __name__ == "__main__":
    unittest.main()
