# Containing kernel faults (oops)

[Español](README-oops.md) · **English**

A bug in a user program must not take the operating system down with it. This
document describes which part of that was already solved, what was missing,
and the mechanism that covers what was missing.

## What already worked: faults *in the program*

When the fault is committed by the program itself — dereferencing a null
pointer, dividing by zero, executing an illegal instruction, jumping through a
corrupt pointer — the CPU raises an exception in user mode and
`handle_user_trap` (`loader/src/linux.rs`) translates it into the equivalent
Linux signal:

| Trap | Signal |
|---|---|
| page fault that cannot be resolved against the VMAR | `SIGSEGV` |
| undefined instruction (`#UD`) | `SIGILL` |
| division by zero (`#DE`), FPU (`#MF`/`#XF`) | `SIGFPE` |
| misaligned access, segment not present (`#NP`) | `SIGBUS` |
| general protection (`#GP`), stack (`#SS`) | `SIGSEGV` |
| breakpoint | `SIGTRAP` |

The process dies (with its register dump, the code bytes around the PC and the
mapping the faulting address belonged to, all in `dmesg`) and the system keeps
going. That has not changed.

## What was missing: faults *in the kernel while serving that program*

The case that did stop the machine was the opposite one: a system call made by
the program that made the kernel fault. A `panic!`, an `unwrap` on an argument
nobody expected, a page fault in a driver path — all of that ended up in
`lang.rs` painting its red banner and entering `loop { spin_loop() }`. Every
other process died with it, including the shell from which it could have been
diagnosed.

It is the same distinction Linux makes between an *oops* (a kernel fault in
process context: the process is killed and the kernel keeps going, marked as
"tainted") and a *panic* (a full stop). Until now Eclipse only had the latter.

## The mechanism

`zCore/src/oops.rs`. When the fault path has finished printing its diagnostics,
it calls `oops::try_contain`, which, if it can:

1. **Kills the culprit process** through the same path as a `kill -9`
   (`Process::exit(128 + SIGKILL)`), so that the parent's `wait4` and the
   shell's `$?` see it for what it is.
2. **Retires the coroutine** that was faulting: it is marked as finished, taken
   out of the task collection and its `Future` is replaced by an inert one. The
   original is *leaked* on purpose — see "What it costs".
3. **Returns the CPU to the scheduler** by switching context to the runtime's
   stack. The abandoned executor is marked as dead, so the runtime creates a new
   one in its place and never goes back to that stack.

If it cannot, it prints *why* and stops as before.

## When it is NOT contained

Recovery is only safe if the fault can be left without leaving the kernel
half-way through something. It is refused — and the machine stops, with the
reason on the console — when:

- **The CPU holds a kernel lock** (`lock::lock_depth() != 0`). Every
  `MutexGuard` in `kernel-sync` brackets its lifetime with `push_off`/`pop_off`,
  so `noff == 0` proves none is alive: nothing was left half-modified and no
  lock will stay held forever.
- **The fault arrived in interrupt context** (`in_timer_callback`). There is no
  process to blame: the interrupted thread is a coincidence.
- **No coroutine was running**, the faulting stack is not its own (e.g. a
  `#GP`, which has its own IST stack), or its overflow canary is already
  trampled.
- **Heap corruption was already suspected** (`heap_smash_suspected`). With the
  heap trampled, carrying on only spreads the damage.
- **The fault is a hit on the stack guard** (`stack_guard`). That case does not
  even reach `try_contain`: `handle_page_fault` reports and stops. The probe
  that faulted was the *next* page down, so what is left of the stack below
  `RSP` is only what this frame had not yet claimed; running containment there
  (formatting, `Process::exit`) would grow into the guard again, and this time
  with `RSP` already inside it the CPU cannot even push the exception frame:
  double fault. Containing an overflow first requires the `#PF` handler to run
  on its own stack (IST), as `#DF` and `#GP` already do.
- **Another fault was already being contained on that same CPU**: if the
  recovery machinery is what fails, there is nothing to rescue.
- **The budget is exhausted** (`MAX_CONTAINED = 16` faults per boot).

These conditions are necessary, not sufficient. A few external `spin::Mutex`es
(scheduler, console, `init_once`) that do not count towards `lock_depth` are
left out of the check. The case that matters — kernel objects: process, thread,
VMAR, files — does use `lock::Mutex` and is covered.

## What it costs

Abandoning a coroutine mid-execution **does not undo what it was doing**: no
destructor in the interrupted call chain runs. Its memory, its `Arc`
references (including the victim's `Process` and `Thread` objects, though not
its mappings: `Process::terminate` frees the address space) and its 2 MiB
coroutine stack are lost. It is a bounded leak per contained fault, hence the
budget: a kernel that spends the day containing faults is not healthy, and at
that point a stop that can be diagnosed is preferable.

The interrupted `Future` is not re-run or destroyed either. Its generator was
left with the discriminant of the last `await` point, which no longer describes
the values it really holds: resuming it would repeat the code that faulted, and
destroying it could double-free something the aborted `poll` had already
consumed. That is why it is replaced by an inert `Pending` and the original is
leaked.

## What it looks like

The usual full diagnostics (panic banner, backtrace, page-fault dump) are
printed **before** containing, so nothing is lost. After them comes one of
these two lines:

```
[oops] kernel panic contained (1/16): killing pid=1234 "firefox" tid=1235 —
       the kernel stays up, no other process is affected
```

```
[oops] kernel panic NOT contained (the CPU holds 1 kernel lock(s)) — halting
```

A later panic also says how many faults this boot has already contained,
because it is usually the same root cause coming back:

```
[oops] kernel faults already contained this boot: 3
```

If the coroutine belonged to the kernel itself (network polling, a driver's
deferred work) there is no process to blame; it is retired anyway — stopping
the machine is strictly worse — but a warning says that subsystem may stay
inoperative until the next boot.

## Turning it off

```ini
cmdline=LOG=warn:PANICONOOPS=1
```

Restores the previous behaviour: stop on the first fault. That is what you want
while debugging one specific fault, because it leaves the machine frozen in
place, with the culprit coroutine not torn down and its stack intact to inspect
from the QEMU monitor. By default containment is **on**.

## Design: why a context switch and not a `longjmp`

Getting out of a fault requires abandoning the stack where it happened. The
classic alternative — a `setjmp`/`longjmp` to a recovery point — does not fit
here: the local variables of an `async fn` live in the `Future` object (the
heap), not on the machine stack, so a `jmp_buf` captured before an `.await`
stops being valid as soon as the coroutine is suspended and resumed.

The scheduler, on the other hand, already knows how to switch stacks: it is
what it does on every timer preemption (`sched_yield` demotes the current
executor and creates another). Containment reuses exactly that path, only
marking the executor as dead so that nobody ever resumes it. The new scheduler
code boils down to `runtime::abandon_current_task`, which is `sched_yield` plus
"and don't come back".
