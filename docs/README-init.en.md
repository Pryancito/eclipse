# `eclipse-init`: what it does, how it compares and what it lacks

[Español](README-init.md) · **English**

`tools/eclipse-init/src/main.rs` is PID 1 of the installed system. The Eclipse
kernel already mounts the root, brings up networking and opens the shell on
each VT, so the init does not need a heavyweight service manager (OpenRC's
`fork`/`exec` of `busybox sh` per step is exactly what stressed the kernel's
fragile paths). It does what PID 1 has to do: reap orphans, mount any missing
pseudo-filesystems, start what is declared in `/etc/eclipse/services/*.service`
and power off or reboot the machine with busybox's signals.

The keys of a `.service` are `exec`, `type` (`oneshot` | `respawn`), `after`,
`requires`, `wait_socket`, `wait_path`, `desktop`, `cmdline`, `log` and
`timeout`. They are documented in the `example.service.txt` the image itself
publishes in `/etc/eclipse/services/`.

## An honest comparison

| Mechanism | systemd | runit | s6-rc | OpenRC | launchd | SMF | eclipse-init |
|---|---|---|---|---|---|---|---|
| Supervision and restart | yes | yes | yes | no (boot only) | yes | yes | yes |
| Crash-loop backoff | `RestartSec` | fixed 1 s | fixed 1 s | — | 10 s minimum | yes | 250 ms → 8 s, exponential |
| **Giving up on a service that cannot work** | `StartLimitBurst` → `failed` | no | no | — | *throttle* | → `maintenance` | **yes (20 crashes in a row, or 20 starts in half an hour)** |
| **Start-up time limit** | `TimeoutStartSec` 90 s | no | `timeout-up` | yes | `ExitTimeOut` | mandatory | **yes (90 s, `timeout =`)** |
| Dependency ordering | yes | no | yes | yes | no | yes | yes (`after =`, topological) |
| Readiness wait | `Type=notify` | no | notification *fd* | no | *socket activation* | no | bounded socket/path polling |
| Propagating a dependency's failure | `Requires=` | — | yes | yes | no | yes | **yes (`requires =`)** |
| Log size limit | journald | `svlogd` | `s6-log` | logrotate | yes | yes | **yes (1 MiB + 1 generation)** |
| Killing leftovers when stopping a service | cgroup | process group | process group | yes | yes | contract | only on a `oneshot`'s `timeout` |
| Surviving its own failure | — | — | — | — | — | — | **yes (`unwind` + `catch_unwind`)** |
| Orderly shutdown | yes | yes | yes | yes | yes | yes | **no, on purpose** (see below) |

Two empty cells are decisions, not gaps:

- **There is no orderly shutdown.** `shutdown()` does `sync` + `reboot(2)`, the
  `busybox reboot -f` path. Killing labwc and the GPU clients before the call
  hung the reboot on this kernel, and device quiescing (GSP-RM/WPR2, NVMe
  CC.SHN) already happens inside `kernel_hal::cpu::reset`.
- **There is no readiness protocol.** The `wait_socket =` and `wait_path =`
  gates cover the two real cases (seatd and the `/dev/input` nodes) without
  asking anything of the service.

## What this round closed

1. **A `oneshot` that never finishes no longer hangs the boot.** A `oneshot`'s
   `waitpid` had no limit: every bounded wait in this file has one, but not that
   one, so a single hung wrapper left the machine dead — no later service
   started, the supervision loop was never reached and, since the shutdown flags
   are only read in there, Ctrl-Alt-Del did nothing either. There is now a 90 s
   limit (that of `TimeoutStartSec`), adjustable per service with
   `timeout = <seconds>` and disabled with `timeout = 0`. When it is exceeded:
   SIGTERM to the **process group** (the real work of these wrappers is a
   grandchild), SIGKILL two seconds later, and if even that does not kill it, it
   is left behind and boot goes on. A failing `oneshot` now also says so: before
   it was completely silent.
2. **A `respawn` that never comes up is no longer retried.** The backoff makes
   the crash loop cheaper but does not end it: a service that could not work
   printed its death every 8 s for as long as the machine was on, and that
   console is the box's only diagnostic. After 20 crashes in a row without ever
   reaching `HEALTHY_UPTIME` it is given up for the rest of the boot, with a line
   saying where its log is. Much more patient than systemd (5 attempts in 10 s)
   on purpose: with the backoff at 8 s that is about two and a half minutes of
   attempts, so nothing that is merely slow to find its dependency is dropped. A
   single healthy run resets the count.

## What the second round closed

**A dependency's failure propagates.** `after =` only ordered; now there is
`requires =`, systemd's `Requires=`. When init gives up on a service for the
rest of the boot (its `exec =` does not exist, or it went over the crash limit),
everything that requires it is dropped too, in a chain and saying so once per
service. Before, with `seatd` dropped, labwc spent its twenty attempts paying
the 10 s gate each time against a socket nobody was going to create, and behind
it lunarbar and lunarbg did the same: three services filling the console with
their deaths instead of the one failure that mattered.

Two ways in which it is not systemd's `Requires=` as is:

- **`requires =` implies `after =`.** In systemd they are independent, and the
  trap that leaves is a unit that requires another without being ordered after
  it, so it starts alongside it. Here that cannot be written.
- **A requirement that is not a service of this boot is ignored**, just as the
  topological order ignores it. A session that does not bring that dependency
  does not lose everything behind it because of that.

It is carried by the eight pairs where it is not a matter of opinion: the
service waits in `wait_socket` for exactly the socket the other one creates
(labwc → seatd, lunarbar / lunarbg / xkbmap → labwc, dbus-selftest → dbus) or
makes no sense without it (boot-sound → pulseaudio). An `xtask` test pins that
list and another checks that no `requires =` names a service the image does not
write, because a misspelt name there would be silent.

## What the third round closed

**Logs no longer eat the RAM.** Every `log =` the image publishes is in `/tmp`,
which is a **tmpfs**: its bytes are the machine's memory, and nothing bounded
them. The hole was worst exactly where it hurts most, because the service that
cannot stop crashing is the one that writes the most. Init now checks the sizes
once a minute and, past 1 MiB, moves the contents to `<log>.1` and truncates the
file to zero: each service costs at most about 2 MiB of RAM however long the
machine has been up, and 1 MiB is still tens of thousands of lines.

Three details of the how:

- **Copy and truncate, do not rename** (logrotate's `copytruncate`). The service
  has the file open, so a `rename` would leave it writing to the renamed inode
  forever and the new file empty. What is lost is a line written between the
  copy and the truncate, the same race logrotate has had for twenty years, and
  it is the price of not having to reopen another process's fd. The child's
  `O_APPEND` is what makes its next write land at the start of the new file and
  not after a megabyte-long hole.
- **SIGALRM is the only clock PID 1 has.** The supervision loop blocks in
  `waitpid` while nothing is pending, so without an alarm it would never look;
  the handler only raises the flag and the loop re-arms. It is a one-shot alarm,
  not an interval timer, so that it does not keep firing during shutdown.
- **By distinct path**: two services can share a `log =` (`boot-sound` and
  `boot-sound-xorg` both write to `/tmp/boot-sound.log`) and rotating it twice
  in the same pass would take away the generation just saved.

What it does **not** cover: a file a wrapper writes on its own that is not any
service's `log =`. Init only knows the table.

## What the fourth round closed

**An init failure is no longer a machine failure.** The release profile was
`panic = "abort"`, so any `panic` in PID 1 — an index, an arithmetic edge, an
`unwrap` on something the disk said — was instantly "Attempted to kill init":
the whole machine stopped by an error in a decision. No other init in the table
has to solve this, because they are all processes the kernel can afford to lose;
this one is not.

The profile is now `panic = "unwind"` and the two places that run decision code
are inside a `guard`: starting a service (a failure costs that service, not the
boot) and the supervision loop (it is re-entered, with a 1 s pause so that a
`panic` in the first statement is not a hot loop filling the console). A `panic`
hook names the file and line on the console before unwinding, because on a
machine with no diagnostic other than that console, that line is the whole
failure report.

Two details: `AssertUnwindSafe` is valid here because the only thing crossing
the boundary is init's service table, and there is no invariant a half-written
field can break — a service with `pid` set and `started_at` not set reads as
"started just now", which is what the next reap would have concluded; and the
only thing unwinding can really poison is the `RENDERER_SAID` `Mutex`, which
already treated a poisoned lock as "say it". Unwinding costs about 17 KiB of
binary, 4%.

## What the fifth round closed

**The bounded boot waits now reap, and it was not cosmetic.** During a
`wait_socket` gate — 10 s in labwc's case — init reaped nobody, and the problem
was not the zombies: a service's lifetime was measured **when it was reaped**,
so a child dying in that window was credited with the whole gate as time lived,
went over `HEALTHY_UPTIME`, was considered healthy and was restarted at once
with its backoff at zero. It is exactly the bug the backoff had when it slept
inside the supervision loop, in another window.

The waits now reap without blocking and **queue** what they reaped with the
time of death; the loop drains that queue at the start of each turn and accounts
for each exit with its own timestamp. The queue is what makes it safe: a bare
`waitpid(-1)` in a wait **would swallow** a `respawn`'s death, the loop would
never see that pid, the service's `pid` would stay set forever and nobody would
restart it.

The `oneshot` wait is the only one that does not reap, on purpose: it is
watching one specific child and an "any child" reap would snatch it from under
the `waitpid` waiting for it.

The accounting moved out of the loop into `note_exit`, which can now be tested:
four tests, and the one that matters pins that a 40 ms crash during a 10 s gate
is still a crash.

## What the sixth round closed

**A `respawn` that sleeps before giving up is no longer immortal.** The crash
limit from the first round only fires for a service that *never* gets past
`HEALTHY_UPTIME`, and that leaves out a whole family of loops: a wrapper that
**sleeps** before exiting. All the `eclipse-*` wrappers in the images do;
`eclipse-pulseaudio` is

```sh
command -v pulseaudio >/dev/null 2>&1 || { echo ...; sleep 8; exit 127; }
```

so each attempt lived 8 s, read as a healthy run, reset the crash count and was
restarted at once with the backoff reset. On a `minimal` boot (which has no
pulseaudio) that is, for as long as the machine is on:

```text
respawn: pulseaudio exited after 8.125682158s (exit 127 ...), restarting
respawn: pulseaudio exited after 8.032575278s (exit 127 ...), restarting
```

A `sleep` inside the service is exactly what the supervisor's backoff is for,
and on top of that it disabled the backoff **and** the crash limit. Two things
close it:

1. **A 126 or a 127 is not a healthy run**, however long it lasts. They are the
   only two codes that are not the program's opinion but the report that it
   never got to run: the binary is not there, or it is there and cannot be
   executed. No retry fixes either, so they count as a crash and the service
   ends up given up with its line and its log.
2. **Starts are counted**, not just crashes: more than 20 in half an hour and it
   is given up, with any exit code and any lifetime. It is the net that catches
   the `sleep 60; exit 1`s, which no code betrays. The window is long on purpose
   — the wrappers sleep up to 60 s — because systemd's (5 starts in 10 s) would
   see none of these loops; and a healthy `respawn` is never restarted, so every
   start is a death. One that crashes once an hour is never dropped.

It is counted in `start_service`, the only place every start goes through: a
service whose attempt sleeps looks healthy on exit and completely normal on
entry. The refusal is **not** recorded in the window, or it would push a real
start out of it.

The wrappers' `sleep`s stay as they are: they now only pace the retries, and
what had to change was the supervisor, which no `sleep` can fool.

## The three boot settings, at once

`eclipse-kbd --boot`, `eclipse-locale --boot` and `eclipse-tz --boot` are three
shell scripts, each with eight or ten `fork`/`execve`s of busybox applets
inside (`awk`, `tr`, `dd`, `grep`, `mv`). Running one after another they were
**85 % of the init's entire boot**: 1.13 s out of 1.46 s on the QEMU boot that
`docs/README-boot.md` measures, against 150 ms for everything else the init does
before the first service. On this kernel what is expensive is
`fork`/`execve`, and that is thirty in a row on the critical path with PID 1
blocked in `waitpid`.

Nothing orders them relative to each other: each reads its own file in
`/etc/eclipse` and the command line, and writes its own. So all three are
launched and **then** all three are waited for. They still all finish before the
first service starts, because that service must already see the language and
time zone in its environment; the only change is that the three waits overlap.

What makes it safe, and without which this would be a keyboard layout bug one
boot later: the three **do** write a common file, labwc's `environment`, and
three simultaneous read-modify-writes of one file lose keys (each reads the old
file and the last `mv` wins). The scripts now take a lock around exactly that —
a directory, because `mkdir` either creates it or fails, atomically, on any file
system —, **bounded**: after three seconds it takes it anyway and carries on. A
boot script that waits forever on a stale lock is exactly how a whole machine
hangs, and losing a key on one boot is the cheap failure. The xtask tests hold
the lock against the three scripts and check both things: that they wait, and
that none of them gets stuck.

**Escape hatch**: `init.serial_setup` on the kernel command line runs them one
after another again, in the same order as before, without recompiling anything.

## Remaining

Nothing pending from this review. What is left are decisions already made (the
forced shutdown, the absence of a readiness protocol) and what the log ceiling
does not cover: a file a wrapper writes on its own that is not any service's
`log =`.
