"""Use guest exit markers instead of shell prompts to delimit QEMU test cases."""
import os
import re
import shlex
import signal
import subprocess
import time
import uuid


def exit_code(output, marker):
    # The guest's console ends the marker line in "\r\r\n" (measured on the
    # x86_64 serial console, 2026-11-05): one "\r" is not enough, and with
    # `\r?$` no baremetal case ever completed -- every one was a TIMEOUT.
    match = re.search(r"(?m)^" + re.escape(marker) + r":(\d+)\r*$", output)
    return int(match[1]) if match else None


def completion_runner(base, statuses):
    class CompletionRunner(base):
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
