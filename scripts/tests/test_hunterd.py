"""hunterd (`hunterctl persist` / `history`) against a fake /proc.

hunterctl puts every path under $HUNTERCTL_ROOT when it is set, so these run
the real script on the build host: what it writes to disk is the history
other tools read, and the learned file is something the kernel trusts at the
next boot.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HUNTERCTL = Path(__file__).parents[2] / "tools" / "hunterctl" / "hunterctl"

# 2026-10-29T18:47:35Z
NOW = 1793299655


def ring(*events, learned=()):
    out = "hunter security subsystem v0.4.0\n"
    if learned:
        out += f"learned-programs ({len(learned)}):\n"
        out += "".join(f"  {p}\n" for p in learned)
    out += "\nrecent events (oldest first):\n"
    for seq, ts, pid, sev, cat, rest in events:
        out += f"[{seq:>6}] +{ts:>10}s pid={pid:<5} {sev:<6} {cat:<9} {rest}\n"
    return out


class Hunterd(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "proc/sys/kernel/random").mkdir(parents=True)
        self.boot("aaaaaaaa-1111")

    def tearDown(self):
        self.tmp.cleanup()

    def boot(self, boot_id, uptime="100.00"):
        (self.root / "proc/sys/kernel/random/boot_id").write_text(boot_id + "\n")
        (self.root / "proc/uptime").write_text(f"{uptime} 1.00\n")

    def kernel(self, text):
        (self.root / "proc/hunter").write_text(text)

    def ctl(self, *args, **env):
        e = dict(os.environ, HUNTERCTL_ROOT=str(self.root), HUNTERCTL_NOW=str(NOW))
        e.update(env)
        return subprocess.run(
            ["sh", str(HUNTERCTL), *args], env=e, capture_output=True, text=True, check=True
        ).stdout

    def events(self):
        return [
            l.split("\t")
            for l in (self.root / "var/log/hunter/events.log").read_text().splitlines()
        ]

    def test_each_event_is_written_once_with_its_fields_and_a_wall_clock_time(self):
        self.kernel(ring((0, "0.000", 0, "INFO", "SYSTEM", "INIT: hunter up (v: 1)")))
        self.ctl("persist")
        self.kernel(
            ring(
                (0, "0.000", 0, "INFO", "SYSTEM", "INIT: hunter up (v: 1)"),
                (1, "4.500", 142, "CRIT", "EXEC", "BLOCKED: blacklisted program: /tmp/x"),
            )
        )
        self.ctl("persist")
        ev = self.events()
        self.assertEqual([e[7] for e in ev], ["BOOT", "INIT", "BLOCKED"])
        self.assertEqual(
            ev[2],
            [
                "2026-10-29T18:45:59Z",
                "aaaaaaaa",
                "1",
                "4.500",
                "142",
                "CRIT",
                "EXEC",
                "BLOCKED",
                "blacklisted program: /tmp/x",
            ],
        )
        self.assertEqual(ev[1][8], "hunter up (v: 1)", "only the first ': ' splits")

    def test_events_the_ring_evicted_are_written_down_as_lost(self):
        self.kernel(ring((5, "9.000", 1, "WARN", "ANOMALY", "WARNING: flood")))
        self.ctl("persist")
        lost = [e for e in self.events() if e[7] == "LOST"]
        self.assertEqual(len(lost), 1)
        self.assertIn("events 0..4", lost[0][8])

    def test_a_reboot_starts_numbering_again_and_opens_with_a_boot_line(self):
        self.kernel(ring((0, "0.000", 0, "INFO", "SYSTEM", "INIT: one"),
                         (1, "1.000", 0, "INFO", "SYSTEM", "INIT: two")))
        self.ctl("persist")
        self.boot("bbbbbbbb-2222")
        self.kernel(ring((0, "0.000", 0, "INFO", "SYSTEM", "INIT: again")))
        self.ctl("persist")
        ev = self.events()
        self.assertEqual([(e[1], e[7]) for e in ev][-2:], [("bbbbbbbb", "BOOT"), ("bbbbbbbb", "INIT")])
        out = self.ctl("history", "--this-boot")
        self.assertIn("again", out)
        self.assertNotIn("two", out)

    def test_the_learned_set_only_grows_and_is_root_only(self):
        self.kernel(ring(learned=["/usr/bin/vim", "/bin/busybox"]))
        self.ctl("persist")
        self.boot("cccccccc-3333")
        self.kernel(ring(learned=["/opt/new"]))
        self.ctl("persist")
        learned = self.root / "var/lib/hunter/learned"
        lines = [l for l in learned.read_text().splitlines() if not l.startswith("#")]
        self.assertEqual(lines, ["/bin/busybox", "/opt/new", "/usr/bin/vim"])
        self.assertEqual(learned.stat().st_mode & 0o777, 0o600)

    def test_history_filters_by_severity_and_count(self):
        self.kernel(
            ring(
                (0, "0.000", 0, "INFO", "SYSTEM", "INIT: up"),
                (1, "1.000", 9, "WARN", "WX", "WARNING: w+x"),
                (2, "2.000", 9, "CRIT", "EXEC", "BLOCKED: no"),
            )
        )
        self.ctl("persist")
        self.assertEqual(len(self.ctl("history", "--alerts").splitlines()), 2)
        crit = self.ctl("history", "--critical").splitlines()
        self.assertEqual(len(crit), 1)
        self.assertIn("BLOCKED: no (pid 9)", crit[0])
        self.assertEqual(len(self.ctl("history", "1").splitlines()), 1)

    def test_the_log_rotates_instead_of_growing_for_ever(self):
        for seq in range(6):
            self.kernel(ring((seq, f"{seq}.000", 1, "INFO", "SYSTEM", "X: " + "y" * 100)))
            self.ctl("persist", HUNTERD_LOG_MAX="300")
        logs = sorted(p.name for p in (self.root / "var/log/hunter").iterdir())
        self.assertIn("events.log.1", logs)
        self.assertLessEqual(len(logs), 5, logs)


if __name__ == "__main__":
    unittest.main()
