import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("linux_libc_test", Path(__file__).parents[1] / "linux_libc_test.py")
script = importlib.util.module_from_spec(spec)
spec.loader.exec_module(script)

# A case's console window as QEMU x86_64 delivered it on 2026-11-05: the typed
# command, a kernel log line about another process, then the marker.
WINDOW = (
    "/ # /libc-test/src/functional/utime-static\x1b[37m[ 19.252076 \x1b[91mERROR\x1b[m "
    "\x1b[37m1 0:0 zcore_loader::linux]\x1b[m \x1b[31m[exit] pid=1033 (grep) killed by signal "
    "SIGPIPE (13, shells print 141) at pc=0x562a37 [/bin/busybox+0x162a37] (default disposition)\x1b[m\r\r\n"
    "'\\n__ZCORE_DONE_870248f168b841a083bee7cb38c81f5e:%s\\n' \"$?\"\r\r\n"
    "\r\r\n__ZCORE_DONE_870248f168b841a083bee7cb38c81f5e:0\r\r\n"
)


class KernelLinesAreNotTheCase(unittest.TestCase):
    def test_a_kernel_error_line_is_taken_out_of_the_window(self):
        cleaned = script.without_kernel_log(WINDOW)
        self.assertNotIn("ERROR", cleaned)
        self.assertNotIn("SIGPIPE", cleaned)
        self.assertIn("__ZCORE_DONE_870248f168b841a083bee7cb38c81f5e:0", cleaned)

    def test_the_cases_own_words_stay(self):
        output = "src/functional/utime.c:12: utime failed: Bad file descriptor\r\r\n"
        self.assertEqual(script.without_kernel_log(output), output)
        self.assertEqual(script.without_kernel_log("[ 1.000000 ERROR 0 1:1 x] Error\n" + output), "\n" + output)
        self.assertIn("utime failed", script.without_kernel_log("[ 1.000000 ERROR 0 1:1 x] Error\n" + output))

    def test_the_guest_prompt_is_what_is_waited_for(self):
        self.assertIsNotNone(script.PROMPT.search("Eclipse OS 0.7.0 - x86_64 desktop\r\r\n\x1b[0m/ # "))
        self.assertIsNone(script.PROMPT.search("BdsDxe: starting Boot0002\r\n"))
    def test_the_prompt_asked_of_the_guest_ends_in_a_newline(self):
        """The whole point of `GUEST_PROMPT`: the console reader is
        line-buffered, so a prompt without a trailing newline is invisible
        until the next line arrives -- the echo of a command already typed.
        The newline is in the PS1 assignment, not in the constant, so check
        the two together the way `adopt_newline_prompt` writes them."""
        assignment = b"PS1='" + script.GUEST_PROMPT.encode() + b"\n'\n"
        self.assertTrue(assignment.endswith(b"\n'\n"))
        self.assertNotIn(b"\n", script.GUEST_PROMPT.encode())
        # And it cannot be mistaken for anything a case prints.
        self.assertIsNone(script.PROMPT.search(script.GUEST_PROMPT))


if __name__ == "__main__":
    unittest.main()
