# clock-probe

[Español](README.md) · **English**

An **integrity** probe for Eclipse OS's clocks and timers. A static musl
binary, with no dependencies, that is copied to the rootfs and run from the
shell.

It does not measure speed. That is what `eclipse-bench` is for. This probe
answers the other question, the one no speed figure answers: **is the time the
kernel gives true?**

## Why it exists

A clock has the same failure mode as sound: every call returns 0 and the
answer is wrong. In this kernel it has already happened three times.

1. `timer_now()` scaled the **absolute** TSC, without a per-boot base, so the
   uptime was the time since the machine was last powered on (8.2 days on the
   machine in the report) while the time *advanced* correctly on top of that
   offset. Every `clock_gettime` returned success. (#1613)
2. The vDSO published the multiplier without that base: the same failure
   again, but only in userspace. The two clocks of a single machine disagreed
   by days, and only a program reading both could see it.
3. `timerfd_settime(TFD_TIMER_ABSTIME)` and `timer_settime(TIMER_ABSTIME)`
   passed the caller's absolute **date** to the kernel timer as a monotonic
   **distance**: seconds since 1970 read as nanoseconds since boot, that is, a
   timer armed half a century ahead. `timerfd_settime` returned 0 and the
   timer never fired.

None of the three shows up in a return value, a speed figure, or a layer that
breaks. All three show up as soon as two clocks are read against each other,
which is all this probe does: it states an invariant, measures it, and prints
the numbers whether it passes or fails.

## Build

```sh
make                       # x86_64-linux-musl-gcc -O2 -static -pthread
make CC=musl-gcc
make linux                 # the same source with the host's cc
```

`cargo xtask rootfs` / `image` already builds it and copies it to
`/bin/clock-probe` inside the image, like the rest of the probes in `tools/`.

## Run

```sh
clock-probe [--only SECTION] [--quick] [-v]
```

- `--only SECTION` — a single section; can be repeated.
- `--quick` — fewer rounds, about 2 s in total instead of about 10.
- `-v` — also print the detail notes.

Exit: **0** if every invariant holds, **1** if any fails, **2** if the command
line is wrong. It never touches the system clock (it never calls
`clock_settime`), so it is safe on a working machine.

**Run it on Linux too, on the same machine.** Every invariant here is one Linux
keeps, so a FAIL on Linux is a bug in the probe and a FAIL only on Eclipse is a
bug in Eclipse.

## The sections, and each one's invariant

| section | invariant |
| --- | --- |
| `res` | every clock `clock_getres` accepts can be read, its resolution is sane and `tv_nsec` is normalised (< 1e9) |
| `mono` | monotonic clocks do not go backwards over hundreds of thousands of consecutive reads |
| `vdso` | the userspace clock and the kernel's are **the same clock**: a vDSO read taken between two raw syscalls falls between the two |
| `offset` | `CLOCK_REALTIME` minus `CLOCK_MONOTONIC` does not move; if it drifts, the two clocks run at different rates and one of them is wrong |
| `coarse` | the `_COARSE` clocks agree with the fine ones within their own resolution, and never run ahead |
| `uptime` | `CLOCK_BOOTTIME` agrees with `/proc/uptime` and is never behind the monotonic one |
| `epoch` | the wall time is a plausible date, and `time()` and `gettimeofday()` match `clock_gettime` (the vDSO serves all three from a single data page, so they can disagree) |
| `cpu` | CPU clocks advance when the process burns CPU, do not advance while it sleeps, and the thread's never exceeds the process's |
| `affinity` | the monotonic clock does not go backwards when the reader changes CPU |
| `rate` | over a one-second window, monotonic, wall, boottime and `/proc/uptime` advance by the same amount (a wrong TSC frequency shows up here and nowhere else) |
| `sleep` | no sleep returns early: `nanosleep`, and `clock_nanosleep` with `TIMER_ABSTIME` on both clocks |
| `timerfd` | a timerfd fires once per expiry, never early, does not lose expirations when periodic, and an absolute wall date is a date and not a distance |
| `timer` | the same with `timer_create`, plus `timer_gettime` never returning more than was armed and counting down |
| `itimer` | `setitimer(ITIMER_REAL)` fires, and not early |
| `timeout` | blocking calls with a deadline honour it as a floor: `poll`, `ppoll`, `select`, `epoll_wait`, `sem_timedwait`, `pthread_cond_timedwait` |

## What the probe CANNOT claim

An error in the counter's **base** shifts the monotonic clock, boottime,
`/proc/uptime` and the `idle` in `/proc/stat` by the same amount, because all
four come from the same `timer_now()`: no comparison between them sees it, and
on this machine there is no second independent clock (no RTC node) to anchor
them against. All that is left is someone knowing how long the machine has
been on. That is why the `uptime` section prints a **WARNING** — which never
fails — when the machine claims to have been on for more than a day: that way
it reads at a glance, which is how #1613 was reported ("a dmesg that starts at
8.2 days").

The userspace half of that same failure it does see, down to the nanosecond:
that is exactly what `vdso` measures.

## A failure, read

```
  FAIL  CLOCK_MONOTONIC vDSO entre syscalls 20000 de 20000 fuera: hasta 0 ns por detras, 711230999999945 ns por delante
  FAIL  timerfd ABSTIME de pared           no disparo en 3 s: la fecha se armo como una distancia
```

(The probe prints its messages in Spanish.) The first line is #1613 in its
userspace version; the second is the `TFD_TIMER_ABSTIME` failure. Both were
obtained by injecting each failure on purpose into the probe itself, to check
it detects them and, above all, that a call that does not return is reported
within three seconds instead of hanging the probe: every wait with an absolute
deadline runs under a watchdog thread that interrupts it.
