"""Use guest exit markers instead of shell prompts to delimit QEMU test cases."""
import os
import re
import shlex
import signal
import subprocess
import time
import uuid


# The guest shell's prompt, and how long a case may wait for it to come back
# before being typed anyway.
#
# A case must not be typed between the previous case's exit and the shell's
# return to its interactive read: busybox ash restores the console's termios
# there, and that restore DISCARDS whatever is already queued on the serial
# line. The command then loses its leading bytes and the shell runs the tail,
# which is some suffix of a path and so "not found" -- an exit 127 that reads
# exactly like a missing binary. Measured in QEMU riscv64 (this container,
# 2026-11-05), 4 cases of 415:
#
#   Test 72: /libc-test/src/functional/string_strchr-static.exe
#   /libc-test/src/functional/string_strchr-static./ # exe; printf '\n__ZCORE...
#   sh: exe: not found
#     FAILED (0.196s, guest exit=127)
#
# The marker that ends a case is printed by `printf`, before the shell gets
# that far, so the marker alone is not permission to type the next one.
# Waiting for the prompt costs milliseconds per case and takes the race out.
PROMPT = re.compile(r"/ # ")
PROMPT_WAIT = 5.0
# How often to ask for a prompt with a bare newline while waiting. The prompt
# we are owed is almost always gone already -- cleared away with the previous
# case's output -- so asking is the normal path, not a fallback.
PROMPT_NUDGE = 0.2


def exit_code(output, marker):
    # The guest's console ends the marker line in "\r\r\n" (measured on the
    # x86_64 serial console, 2026-11-05): one "\r" is not enough, and with
    # `\r?$` no baremetal case ever completed -- every one was a TIMEOUT.
    match = re.search(r"(?m)^" + re.escape(marker) + r":(\d+)\r*$", output)
    return int(match[1]) if match else None


def completion_runner(base, statuses):
    class CompletionRunner(base):
        # Whether this console is known to show a shell prompt, and so whether
        # a case may only be typed once that prompt is back (see `PROMPT`). The
        # runner that waits for the prompt at boot sets it; a console that has
        # no prompt at all leaves it False, where waiting could only burn the
        # budget for nothing.
        prompt_sync = False

        def await_prompt(self):
            """Block until the guest shell is back at its prompt, then clear
            the console buffer. Returns whether the prompt was seen; a console
            that does not produce one in `PROMPT_WAIT` is typed at anyway,
            which is what every run did before this existed."""
            start = time.monotonic()
            asked = 0.0
            while True:
                with self.lk:
                    seen = PROMPT.search(self.output) is not None
                if seen or self.zcore_proc.poll() is not None:
                    break
                now = time.monotonic()
                if now - start >= PROMPT_WAIT:
                    break
                if asked == 0.0 or now - asked >= PROMPT_NUDGE:
                    # Ask for a prompt rather than wait for one: the one we
                    # were owed has almost always been cleared away with the
                    # previous case's output, and an idle shell answers a bare
                    # newline at once. Asked again every `PROMPT_NUDGE`,
                    # because a shell that is still mid-restore throws the
                    # newline away with everything else that was queued --
                    # which is the whole race this exists for.
                    self.zcore_proc.stdin.write(b"\n")
                    self.zcore_proc.stdin.flush()
                    asked = now
                time.sleep(0.005)
            with self.lk:
                seen = PROMPT.search(self.output) is not None
                self.output = ""
            return seen

        def guest_command(self, name):
            """What is typed at the guest shell for case `name`: its path,
            quoted. A subclass that has to run a case some other way (as
            another user, say) overrides this and keeps the marker below."""
            return shlex.quote(name)

        def classified_output(self, output):
            """The part of a case's console window that `check_output` is to
            classify; all of it, by default. A subclass overrides this, not
            `check_output`: the submodule's `LinuxTestRunner` defines its own
            `check_output` on top of whatever `TestRunner` is, so a
            `check_output` here is never called."""
            return output

        def run_one(self, name, fast=False, timeout=10):
            marker = "__ZCORE_DONE_" + uuid.uuid4().hex
            # The echoed command contains the marker, but only printf produces
            # a complete marker line with the actual command's exit status.
            command = self.guest_command(name) + "; printf '\\n" + marker + ":%s\\n' \"$?\"\n"
            if self.prompt_sync:
                self.await_prompt()
            else:
                with self.lk:
                    self.output = ""
            start = time.monotonic()
            self.zcore_proc.stdin.write(command.encode())
            self.zcore_proc.stdin.flush()
            code = None
            while time.monotonic() - start < timeout:
                with self.lk:
                    output = self.output
                code = exit_code(output, marker)
                if code is not None or self.zcore_proc.poll() is not None:
                    break
                time.sleep(0.01)
            with self.lk:
                output = self.output
                self.output = ""
            if code is None:
                status = statuses.TIMEOUT if self.zcore_proc.poll() is None else statuses.FAILED
            elif code != 0:
                status = statuses.FAILED
            else:
                status = self.check_output(self.classified_output(output))
            if status != statuses.OK or not fast:
                self.logger.println(output)
            else:
                self.logger.println_file_only(output)
            self.logger.println(f"  {status.name} ({time.monotonic() - start:.3f}s, guest exit={code})\n")
            return status

        def stop_qemu(self):
            # Unblock readline before joining it; the original order could
            # leave a reader from the old process alive after a QEMU restart.
            self.thread_stop_flag = True
            if self.zcore_proc.poll() is None:
                os.killpg(os.getpgid(self.zcore_proc.pid), signal.SIGTERM)
                try:
                    self.zcore_proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(os.getpgid(self.zcore_proc.pid), signal.SIGKILL)
                    self.zcore_proc.wait()
            self.receiver_thread.join(timeout=5)
            if self.receiver_thread.is_alive():
                raise RuntimeError("QEMU output reader did not stop")
            self.thread_stop_flag = False

    return CompletionRunner
