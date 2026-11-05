import importlib.util
from pathlib import Path
import re
import unittest
import enum
import subprocess
import tempfile
import threading
import time
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("qemu_completion", Path(__file__).parents[1] / "qemu_completion.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class CompletionProtocol(unittest.TestCase):
    def test_prompt_and_command_echo_are_not_completion(self):
        output = "/ # \n/ # /test; printf '\\n__CASE__:%s\\n' \"$?\"\n"
        self.assertIsNone(runner.exit_code(output, "__CASE__"))
        self.assertIsNone(runner.exit_code(output + "__PREVIOUS__:0\n", "__CASE__"))
        self.assertEqual(runner.exit_code(output + "__CASE__:0\r\n/ # ", "__CASE__"), 0)

    def test_real_shell_waits_for_exit_instead_of_prompt(self):
        class Status(enum.Enum):
            OK = 0
            FAILED = 1
            TIMEOUT = 2

        class Base:
            def check_output(self, output):
                return Status.OK

        instance = runner.completion_runner(Base, Status)()
        instance.lk = threading.Lock()
        instance.output = ""
        instance.thread_stop_flag = False
        instance.logger = SimpleNamespace(println=lambda message: None, println_file_only=lambda message: None)
        instance.zcore_proc = subprocess.Popen(["sh"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)

        def read():
            for line in instance.zcore_proc.stdout:
                with instance.lk:
                    instance.output += line.decode()

        instance.receiver_thread = threading.Thread(target=read)
        instance.receiver_thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                script = Path(directory) / "case.sh"
                script.write_text("#!/bin/sh\nprintf '/ # \\n'\nsleep 0.05\nexit 7\n")
                script.chmod(0o755)
                self.assertEqual(instance.run_one(str(script), timeout=2), Status.FAILED)
                script.write_text("#!/bin/sh\nexit 0\n")
                self.assertEqual(instance.run_one(str(script), timeout=2), Status.OK)
                script.write_text("#!/bin/sh\nsleep 10\n")
                self.assertEqual(instance.run_one(str(script), timeout=0.05), Status.TIMEOUT)
        finally:
            instance.stop_qemu()
            instance.zcore_proc.stdin.close()
            instance.zcore_proc.stdout.close()
        self.assertFalse(instance.receiver_thread.is_alive())

    def test_a_subclass_chooses_what_is_typed_for_a_case(self):
        """`guest_command` is the hook `scripts/linux_libc_test.py` uses to
        run a case as `nobody`: the marker still follows whatever it typed."""

        class Status(enum.Enum):
            OK = 0
            FAILED = 1
            TIMEOUT = 2

        class Base:
            def check_output(self, output):
                return Status.OK

        class AsSomeoneElse(runner.completion_runner(Base, Status)):
            def guest_command(self, name):
                return "env WHO=nobody " + super().guest_command(name)

        instance = AsSomeoneElse()
        instance.lk = threading.Lock()
        instance.output = ""
        instance.thread_stop_flag = False
        instance.logger = SimpleNamespace(println=lambda message: None, println_file_only=lambda message: None)
        instance.zcore_proc = subprocess.Popen(["sh"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)

        def read():
            for line in instance.zcore_proc.stdout:
                with instance.lk:
                    instance.output += line.decode()

        instance.receiver_thread = threading.Thread(target=read)
        instance.receiver_thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                script = Path(directory) / "who am i.sh"
                script.write_text("#!/bin/sh\n[ \"$WHO\" = nobody ]\n")
                script.chmod(0o755)
                self.assertEqual(instance.run_one(str(script), timeout=2), Status.OK)
        finally:
            instance.stop_qemu()
            instance.zcore_proc.stdin.close()
            instance.zcore_proc.stdout.close()

    def test_a_subclass_chooses_what_the_classifier_sees(self):
        """The hook for `scripts/linux_libc_test.py` to take the kernel's own
        console lines out before the submodule's substring check."""

        class Status(enum.Enum):
            OK = 0
            FAILED = 1
            TIMEOUT = 2

        class Base:
            def check_output(self, output):
                return Status.FAILED if "ERROR" in output else Status.OK

        class WithoutKernelLines(runner.completion_runner(Base, Status)):
            def classified_output(self, output):
                return output.replace("[kernel] ERROR", "")

        instance = WithoutKernelLines()
        instance.lk = threading.Lock()
        instance.output = ""
        instance.thread_stop_flag = False
        instance.logger = SimpleNamespace(println=lambda message: None, println_file_only=lambda message: None)
        instance.zcore_proc = subprocess.Popen(["sh"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)

        def read():
            for line in instance.zcore_proc.stdout:
                with instance.lk:
                    instance.output += line.decode()

        instance.receiver_thread = threading.Thread(target=read)
        instance.receiver_thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory:
                script = Path(directory) / "case.sh"
                script.write_text("#!/bin/sh\necho '[kernel] ERROR something else died'\n")
                script.chmod(0o755)
                self.assertEqual(instance.run_one(str(script), timeout=2), Status.OK)
                script.write_text("#!/bin/sh\necho 'ERROR in the case itself'\n")
                self.assertEqual(instance.run_one(str(script), timeout=2), Status.FAILED)
        finally:
            instance.stop_qemu()
            instance.zcore_proc.stdin.close()
            instance.zcore_proc.stdout.close()

    def test_the_guest_console_ends_the_marker_line_in_two_returns(self):
        # What the serial console of a booted x86_64 guest actually sends.
        self.assertEqual(runner.exit_code("\r\r\n__CASE__:0\r\r\n/ # ", "__CASE__"), 0)
        self.assertEqual(runner.exit_code("__CASE__:3\r\n", "__CASE__"), 3)

    def test_nonzero_exit_and_incomplete_marker(self):
        self.assertEqual(runner.exit_code("__CASE__:127\n", "__CASE__"), 127)
        self.assertIsNone(runner.exit_code("__CASE__:", "__CASE__"))


class PromptSynchronisation(unittest.TestCase):
    """A case may only be typed when the shell is back at its prompt. The
    marker that ends the previous case is printed by `printf`, before the
    shell has restored the console's termios, and that restore throws away
    whatever is already queued: the command loses its leading bytes and the
    shell runs the tail. See `PROMPT` in `scripts/qemu_completion.py` for the
    four riscv64 cases this cost."""

    class Console:
        """Stands in for QEMU: records what was typed, and lets the test
        decide what appears on screen in answer to it."""

        def __init__(self, on_write):
            self.writes = []
            self.stdin = self
            self._on_write = on_write

        def write(self, data):
            self.writes.append(data)
            self._on_write(data)

        def flush(self):
            pass

        def poll(self):
            return None

    @staticmethod
    def status_enum():
        class Status(enum.Enum):
            OK = 0
            FAILED = 1
            TIMEOUT = 2

        return Status

    def runner_for(self, on_write):
        status = self.status_enum()

        class Base:
            def check_output(self, output):
                return status.OK

        instance = runner.completion_runner(Base, status)()
        instance.lk = threading.Lock()
        instance.output = ""
        instance.logger = SimpleNamespace(
            println=lambda message: None, println_file_only=lambda message: None
        )
        instance.zcore_proc = self.Console(on_write)
        return instance, status

    def answer_marker(self, instance, data):
        """Complete the case the way a guest shell does: the marker line, then
        a fresh prompt."""
        found = re.search(r"__ZCORE_DONE_[0-9a-f]+", data.decode())
        if found is None:
            return False
        with instance.lk:
            instance.output += found[0] + ":0\r\r\n/ # "
        return True

    def test_a_case_is_not_typed_until_the_prompt_arrives(self):
        """And nothing is sent to the guest while waiting: a nudge is one more
        interactive read, one more termios set, and one more chance to eat the
        command being written."""
        instance = None

        def on_write(data):
            self.answer_marker(instance, data)

        instance, status = self.runner_for(on_write)
        instance.prompt_sync = True
        seen_while_waiting = []

        def prompt_later():
            # Long enough that a runner which did not wait would have typed.
            time.sleep(0.2)
            seen_while_waiting.append(list(instance.zcore_proc.writes))
            with instance.lk:
                instance.output += "/ # \n"

        shell = threading.Thread(target=prompt_later)
        shell.start()
        try:
            self.assertEqual(instance.run_one("/case.exe", timeout=5), status.OK)
        finally:
            shell.join()
        self.assertEqual(seen_while_waiting, [[]], "the guest was written to while waiting")
        self.assertIn(b"/case.exe", instance.zcore_proc.writes[0])

    def test_a_prompt_already_on_screen_is_not_waited_for(self):
        instance = None

        def on_write(data):
            self.answer_marker(instance, data)

        instance, status = self.runner_for(on_write)
        instance.prompt_sync = True
        with instance.lk:
            instance.output = "/ # "
        self.assertEqual(instance.run_one("/case.exe", timeout=5), status.OK)
        self.assertIn(b"/case.exe", instance.zcore_proc.writes[0])

    def test_a_console_that_never_shows_a_prompt_is_typed_at_anyway(self):
        """What every run did before the wait existed, and what the unit tests
        that drive a bare `sh` still need: a console with no prompt must not
        turn into a timeout of its own."""
        instance = None

        def on_write(data):
            self.answer_marker(instance, data)

        instance, status = self.runner_for(on_write)
        instance.prompt_sync = True
        patched, runner.PROMPT_WAIT = runner.PROMPT_WAIT, 0.2
        try:
            self.assertEqual(instance.run_one("/case.exe", timeout=5), status.OK)
        finally:
            runner.PROMPT_WAIT = patched
        self.assertTrue(any(b"/case.exe" in write for write in instance.zcore_proc.writes))

    def test_a_console_without_prompts_declared_does_not_wait_at_all(self):
        """`prompt_sync` is off until a runner has actually seen a prompt at
        boot, so a console that has none pays nothing."""
        instance = None

        def on_write(data):
            self.answer_marker(instance, data)

        instance, status = self.runner_for(on_write)
        self.assertFalse(instance.prompt_sync)
        self.assertEqual(instance.run_one("/case.exe", timeout=5), status.OK)
        self.assertIn(b"/case.exe", instance.zcore_proc.writes[0])
    def test_a_dead_guest_is_a_failed_case_and_not_a_traceback(self):
        """A `BrokenPipeError` out of `run_one` takes down the whole run and
        prints no console output at all, so the panic that killed the guest is
        nowhere to be found. Report the case and carry on to the summary."""

        class Dead:
            def __init__(self):
                self.stdin = self

            def write(self, data):
                raise BrokenPipeError(32, "Broken pipe")

            def flush(self):
                pass

            def poll(self):
                return 1

        printed = []
        status = self.status_enum()

        class Base:
            def check_output(self, output):
                return status.OK

        instance = runner.completion_runner(Base, status)()
        instance.lk = threading.Lock()
        instance.output = "[KERNEL PANIC] the last words\n"
        instance.logger = SimpleNamespace(
            println=printed.append, println_file_only=printed.append
        )
        instance.zcore_proc = Dead()

        self.assertEqual(instance.run_one("/case.exe", timeout=5), status.FAILED)
        self.assertIn("[KERNEL PANIC] the last words\n", printed)
        self.assertTrue(any("no longer running" in line for line in printed))


if __name__ == "__main__":
    unittest.main()
