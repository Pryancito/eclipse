# signal-probe

[Español](README.md) · **English**

An **integrity** probe for Eclipse OS's signal delivery: whether a signal
returns the program to where it was, as it was. A static musl binary, with no
dependencies, that is copied to the rootfs and run from the shell.

## Why it exists

A signal is the one place where the kernel takes a running program, makes it
run another piece of itself and has to leave it exactly as it was. Every call
involved can return 0 with the return done wrong, and in this kernel it has
happened in ways no syscall test sees:

1. The context a signal interrupts was saved in **one** slot per thread, so a
   second signal inside a handler overwrote the first one's, and the outer
   handler's `sigreturn` failed with `EINVAL`. A `sigreturn` that fails
   **returns**, to whatever is behind libc's trampoline. (#1943)
2. A fault after leaving a handler with `siglongjmp` killed the process even
   though it had its SIGSEGV handler installed. (#1943)
3. A handler that rewrote the saved `rip` with a non-canonical address brought
   down **the whole kernel**: the CPU faults on the return to user mode, in
   ring 0. (#1943)
4. An orphan kept the death signal its dead parent had chosen, so a child
   cloned with SIGTERM or SIGUSR1 whose parent exited sent it to init when it
   died, and eclipse-init reboots or powers off on those. And an orphan's death
   was reported to whoever came looking from the parent that **created** it,
   not the one that adopted it.
5. A `SIGSTOP` (or Ctrl-Z's `SIGTSTP`) to a process inside a waiting syscall
   did not stop it until the syscall returned by itself: a child in `pause()`
   **never** stopped, and the parent's `waitpid(WUNTRACED)`, the one a shell
   uses to wait before printing "Stopped", kept waiting with it.
6. Killing a thread did not wake it if it slept in `pause`, `nanosleep`, a
   `read` or a `waitpid`: an `execve` from a second thread waited forever for
   its sibling in `pause()` to die, and a process that exited with a sleeping
   thread was reaped but never torn down, and its memory never came back.
7. The image an `execve` from a second thread started ran with that thread's
   tid and not with the pid: `gettid() != getpid()` in what Linux makes the
   first thread, and a `tgkill(pid, pid)` reached nobody.
8. A parent waiting in `vfork` for a child that never got to `exec` could not
   be killed, and a process that exited with a thread in that wait was never
   torn down.
9. A handler running on a thread waiting for a busy `PTHREAD_PRIO_INHERIT`
   mutex gave the mutex a second owner: `FUTEX_LOCK_PI` returned `EINTR`, and
   glibc's `pthread_mutex_lock` takes that as the lock acquired. Linux redoes
   it whatever the handler.
10. A futex wait without a timeout returned `EINTR` from a handler with
    `SA_RESTART`, which Linux redoes, so glibc's `sem_wait` returned -1
    without the semaphore to a program that installs its handlers with
    `signal()`.
11. A `vfork` child ran in a COPY of its parent's memory, so what it wrote
    before `exec` or `_exit` the parent did not see: glibc's `posix_spawn` of
    a program that does not exist returned 0, because it learns about the
    failed `exec` through a word the child writes.
12. An `execve` of something that is not a program killed the caller with
    SIGSEGV instead of returning an errno: garbage, a script without `#!`
    (which the shell runs itself on getting `ENOEXEC`), a directory, a binary
    for another machine, a dynamic binary whose `PT_INTERP` is not installed.
13. `exit(-1)` reached the parent as a death by SIGHUP: the whole `int` was
    stored, and inside the kernel a negative code is how a death by signal is
    written.
14. A `fork` child and a new thread were born with nice 0, in `SCHED_OTHER`
    and free to run on any CPU, whatever their parent had: `nice make` only
    lowered `make`'s priority. And `SCHED_RESET_ON_FORK` was accepted and
    thrown away.
15. A static-pie (what musl's `gcc -static` builds, this probe included) was
    loaded at address 0, so a null pointer read its ELF header instead of
    faulting.
16. A `waitpid` could reap a dead parent before its children passed to the
    adopter, and the subreaper reaping it got `ECHILD` for the orphan it was
    about to inherit, about one time in thirty.
17. A zombie gave its pid back before it was reaped, so a `fork` could get it
    while the zombie was still waiting, and a `waitpid` for the new child
    collected the dead one.
18. An alarm a process left pending when it died went off in the process that
    inherited its pid, which died of it. The `orphan` probe died like that of
    SIGALRM two or three rounds in thirty, long before its own 20 s alarm.
19. A `#!` interpreter could only be a symlink to something, not a link to a
    link (a `PT_INTERP`'s could be four), so a script whose interpreter goes
    through `/etc/alternatives` gave `ELOOP`. And an interpreter nobody may
    execute ran anyway.
20. `getrusage` and `times` system time was the wall time of one syscall in
    sixteen, multiplied by sixteen: a shell that had waited half a second for a
    child had seconds of CPU. `wait4` gave almost nothing for a child exiting
    with `exit`, because it only counted the threads that had already exited,
    and a thread's CPU clock left out the time in the kernel.
21. `RLIMIT_FSIZE` only stopped `truncate`, `ftruncate` and `pwrite`:
    `write`, `writev` and `fallocate` ignored it, a `pwrite` crossing it
    failed entirely instead of writing up to it, a file already past it could
    not be shrunk, and no `EFBIG` came with its `SIGXFSZ`.
22. `ITIMER_VIRTUAL` and `ITIMER_PROF` counted wall time, so they went off in a
    sleeping process, and `RLIMIT_CPU` sent neither `SIGXCPU` nor `SIGKILL`: a
    runaway loop under `ulimit -t` went on forever.
23. A `timer_create` timer on a CPU clock counted wall time: the
    `CLOCK_PROCESS_CPUTIME_ID` one went off with the process asleep, the
    `CLOCK_THREAD_CPUTIME_ID` one with another thread's work (Go's profiler
    arms one like that on every thread), and the clocks of another thread or
    process (`pthread_getcpuclockid`, `clock_getcpuclockid`) gave `EINVAL`.
24. `RLIMIT_AS` was stored and nobody looked at it: with `ulimit -v`, `mmap`,
    `brk`, `mremap` and `shmat` kept growing the address space, and a
    `malloc` that should have given `NULL` gave memory.
25. `RLIMIT_NPROC` was stored and nobody looked at it: with `ulimit -u`, a
    user's `fork`, `vfork` and `pthread_create` kept succeeding above the
    limit.
26. A zombie was only a pid to its parent once its last thread had gone:
    `getpgid`, `getsid`, `pidfd_open` and `/proc/<pid>` gave `ESRCH` (`ps`
    never showed a `<defunct>`), as did a `kill` from anyone else; a signal to
    one that had only just died gave `ESRCH`, and a child reaped while it was
    finishing still answered `kill(pid, 0)` right after the `waitpid`.
27. A new thread, and a `fork` child, were named after the program and not
    after whoever created them; field 38 of `/proc/<pid>/stat` gave 0 instead
    of the signal the parent receives; and a parent that learned about its
    child's `exec` through the EOF of a close-on-exec pipe could still move it
    to another group with `setpgid`.
28. `/proc/<pid>/status` stopped at eleven lines: `Umask:`, `TracerPid:`,
    `FDSize:`, `NSpid:`, the signal masks, the capabilities, `NoNewPrivs:` and
    `Cpus_allowed_list:` were missing, and a zombie still wrote `VmSize:` and
    `VmRSS:`.
29. `execveat` answered `ENOSYS`, and `fexecve`, which then falls back to
    `execve("/proc/self/fd/N")`, left `/proc/self/exe` at `/proc/self/fd/N`
    and `comm` at `N`. `/proc/self/exe` was the path exactly as the caller
    wrote it: relative after `./prog`, and the link's after a link; a script's
    `comm` was its interpreter's. And `pidfd_open` of a thread that is not the
    leader said `ESRCH` instead of `ENOENT`.
30. Nobody counted context switches: `ru_nvcsw` and `ru_nivcsw` were 0 in
    `getrusage` and `wait4`, and `/proc/<pid>/status` had no
    `voluntary_ctxt_switches:` or `nonvoluntary_ctxt_switches:`. And what a
    child had collected from its own children was not the child's: `wait4`
    and `RUSAGE_CHILDREN` left out the grandchildren, so `time sh -c make`
    did not see the compilers.

## Build

```sh
make                       # x86_64-linux-musl-gcc -O2 -static
make CC=musl-gcc
make linux                 # the same source with the host's cc
```

`cargo xtask rootfs` / `image` already builds it and copies it to
`/bin/signal-probe` inside the image, like the rest of the probes in `tools/`.

## Run

```sh
signal-probe [--only SECTION]
```

Exit: **0** if everything holds, **1** if something fails, **2** if the
command line is wrong. Each section runs in its own child with a 20 s alarm,
so a section that kills its process or hangs comes out as FAIL and the rest
carry on.

**Run it on Linux too, on the same machine.** Everything it checks is
something Linux does, so a FAIL on Linux is a bug in the probe and a FAIL only
on Eclipse is a bug in Eclipse.

## The sections

| section | what it checks |
| --- | --- |
| `nest` | a handler interrupted by another signal returns to where it was, with its vector registers (`xmm8`), and so does the program |
| `longjmp` | leaving a handler with `siglongjmp` does not stop the next fault from reaching its handler; nor does a fault inside another signal's handler |
| `fork` | a child made with `fork` inside a handler returns from that handler |
| `restart` | with `SA_RESTART`, `read` and `wait4` resume after the handler; `nanosleep` and `poll` give `EINTR` anyway, as on Linux; `sem_wait` (a futex wait without a timeout) also resumes; taking a busy PI mutex is redone after any handler, with `SA_RESTART` or without |
| `badrip` | a handler that puts a non-canonical `rip` in its `ucontext` costs the process a SIGSEGV (`SI_KERNEL`) and nothing more. x86_64 only |
| `ids` | the kernel refuses what it cannot load: a TLS pointer outside user space (`arch_prctl`, `clone(CLONE_SETTLS)`) and the pid `INT_MIN`, which has no negative, in `waitpid` |
| `orphan` | an orphan's death notifies whoever **adopted** it, with SIGCHLD even if its dead parent chose another signal in `clone(2)`, and wakes the adopter's `waitpid` even if a process in between made itself a subreaper after the adoption. And as soon as `waitpid` has reaped the dead parent, the orphan already belongs to the adopter (asked 128 times, because it is a matter of timing). The chosen signal is SIGWINCH, which init discards, in case the subreaper did not hold |
| `inherit` | what passes through `fork` (handlers, mask and alternate stack; neither pending signals nor the alarm) and through `execve` (mask, pending, alarm and `SIG_IGN`; handlers go back to `SIG_DFL` and the alternate stack goes away). The `execve` half is reported by the probe itself relaunched with `--exec-report` |
| `wait` | what `waitid` and `waitpid` report: an exit with `WNOWAIT` and then collected; a negative exit code, which arrives as its low byte (`exit_group(-1)` is 255 and the last thread's `exit(-3)`, 253); a death by SIGTERM (`CLD_KILLED`); a stop of a child in `pause()` and a continue, each reported once; `waitid(P_PGID, 0)`, which is the own group; `SA_NOCLDSTOP`; `exit()` from a second thread, which ends the whole process; and `execve()` from a second thread, whose new image takes the pid as its tid, keeps that thread's mask and receives a `tgkill(pid, pid)`. And a zombie keeps its pid until it is reaped: none of 512 `fork`s gets it; and a dead process's alarm dies with it, even if someone else gets its pid (on Linux the pid takes too long to come back to ask) |
| `stop` | a stop reaches a process inside a waiting syscall (`nanosleep`, `read`, `sigsuspend`) and the syscall carries on when it is continued: the `read` reads, without `EINTR`, and the `sigsuspend` keeps waiting for its signal and on return restores the previous mask |
| `teardown` | a process that exits (`exit_group`) with a second thread asleep in `pause`, `read`, `nanosleep`, `waitpid` or in the `vfork` of a child that never `exec`s no longer has that thread when it is reaped: a shared robust mutex the thread held comes back with `EOWNERDEAD`. `/proc/meminfo` is no use for this even on Linux, which sometimes shows a few hundred MiB freed late |
| `spawn` | a `vfork` child runs in its parent's memory: what it writes there the parent reads, so `posix_spawn` reports a program that does not exist and a file action that fails with `ENOENT`, and twenty `posix_spawn`s of `sh -c 'exit 3'` in a row each return their child with its status |
| `badexec` | an `execve` of what is not a program returns, with Linux's errno: `EACCES` without the execute bit, for a directory or with a `PT_INTERP` that is a directory; `ENOEXEC` for garbage, a script without `#!`, an empty `#!` or one naming garbage, and an ELF for another machine; `ENOENT` for a `#!` or a `PT_INTERP` that do not exist; `EIO` for a 10-byte `PT_INTERP` and `ELIBBAD` for one of 128 bytes of garbage; a `#!` or a `PT_INTERP` naming a file without the execute bit give `EACCES`, and a `#!` that goes through three links runs; five chained scripts run and six give `ELOOP`. And `sh -c` of a script without `#!` runs it |
| `sched` | what a `fork` child and a new thread inherit from the parent: the nice value, the CPU mask and the policy. With `SCHED_RESET_ON_FORK` the parent keeps it and reports it (`sched_getscheduler`, `sched_getattr`) and the child is born without it: in `SCHED_OTHER` if the parent was real-time, with nice 0 if the parent's was negative and with the parent's if it was positive. Without privilege it cannot be removed, `sched_setattr` does not accept it in the policy word and `sched_getaffinity` wants a buffer of whole `long`s. The section runs as root |
| `image` | the program is above the null page and reading through a null pointer kills with SIGSEGV, however it was linked (static, static-pie, dynamic). Built with `cargo image`'s musl the probe is static-pie, which is the case that used to fail |
| `cputime` | the CPU time `getrusage`, `times`, `wait4` and the CPU clocks count: nothing for a process asleep or waiting; the 300 ms of a working child, in `wait4`, in `RUSAGE_CHILDREN` and in `times()`, also those of a thread still working when the process calls `exit_group`; the kernel part of a syscall loop in `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_PROCESS_CPUTIME_ID`; and the 300 ms of a grandchild the child collects, in the child's `wait4` and in `RUSAGE_CHILDREN` |
| `fsize` | `RLIMIT_FSIZE` on every write to a file: a `write`, `pwrite`, `writev` or `copy_file_range` crossing the limit writes up to it, without a signal; one starting at the limit or past it gives `EFBIG` and a `SIGXFSZ`, and an empty write at the limit gives 0. An `ftruncate` or `fallocate` that grows the file beyond it gives `EFBIG` and `SIGXFSZ`; shrinking a file already past it is allowed, even if it stays past, and `FALLOC_FL_KEEP_SIZE` is not growing. A pipe has no limit, and without a handler `SIGXFSZ` kills |
| `cputimer` | 100 ms `ITIMER_VIRTUAL` and `ITIMER_PROF` do not move while the process sleeps 300 ms and go off at 100 ms of its CPU time (`ITIMER_VIRTUAL`, user time); with a 20 ms interval they go off between 8 and 20 times in 300 ms of CPU. With `RLIMIT_CPU` 1/2, a `SIGXCPU` at one second of CPU (not of sleep), the soft limit moves to 2 and `SIGKILL` at 2 s; with 1 and no handler, `SIGXCPU` kills |
| `cpuclock` | a 100 ms `timer_create` timer on a CPU clock moves only by what that clock's owner spends (what `getrusage` says, with a 10 ms margin: under an emulator the first round charges tens of ms of system time, and the line gives the user/system split) and goes off at its 100 ms with `SI_TIMER` and its value: the process's, which does not move while it sleeps 300 ms; the thread's own, which does not move with 300 ms of another thread's work; another thread's (`pthread_getcpuclockid`) and a child's (`clock_getcpuclockid`), whose signal reaches the parent, none while the child sleeps. With a 20 ms interval it goes off between 8 and 20 times in 300 ms of CPU; with `TIMER_ABSTIME` and a reading already past it goes off before `timer_settime` returns; and a dynamic clock gives `EOPNOTSUPP` |
| `aslimit` | with `RLIMIT_AS` 4 MiB above the `VmSize` in `/proc/self/status` (only the soft one, restored at the end): 2 MiB of `mmap` fit and 8 do not (`ENOMEM`), nor does a 16 MiB `malloc` (`NULL`), nor an 8 MiB `brk` (it returns the previous one; a 64 KiB one does fit), nor an `mremap` from 2 to 16 MiB, nor an 8 MiB `shmat`; a `MAP_FIXED` over pages already mapped does not add up, and what `munmap` frees can be mapped again |
| `nproc` | a child with a uid of its own (45399) and a soft `RLIMIT_NPROC` of 4 under a hard 6: it and three children are four and the next `fork` gives `EAGAIN`, and so does `pthread_create`; a dead child not yet reaped still counts, and once reaped makes room; with the soft limit raised to 6, two more fit. Root, real or effective, can `fork` with the limit at 0 |
| `zombie` | a dead child not yet reaped, 40 times because each round is a race with its own end: `kill` with SIGTERM and with 0 gives 0, `getpgid` and `getsid` give its group and session, and once reaped `kill(pid, 0)` gives `ESRCH`. A sibling, which is not its parent, finds it too; `/proc/<pid>/stat` says `Z` and our pid, `cmdline` is empty and `ls /proc` lists it; `sigqueue` gives 0; its pidfd is ready when opened, accepts a signal and `waitid(P_PIDFD)` collects it. Once reaped, `/proc/<pid>` is gone and its pidfd and a new `pidfd_open` give `ESRCH` |
| `kin` | with `PR_SET_NAME` set to "jefe", a new thread and a `fork` child are called "jefe"; field 38 of `/proc/self/stat` is SIGCHLD and that of `/proc/self/task/<other thread>/stat` is -1; `pidfd_open` of that other thread gives `ENOENT`; and 20 times, as soon as a child's `exec` closes its close-on-exec pipe, `setpgid` on it gives `EACCES` |
| `status` | `/proc/self/status` has Linux's lines in its order, and they say the same as the calls: `Umask` after `umask(027)` is `0027`, `TracerPid` 0, `FDSize` 256 with fd 255 open, the `NS*` are `getpid`/`getpgid`/`getsid`, `SigPnd`/`ShdPnd`/`SigBlk`/`SigIgn`/`SigCgt` what was raised, sent, blocked, ignored and caught, `SigQ` counts those two and its limit is `RLIMIT_SIGPENDING`, `CapEff`/`CapPrm`/`CapBnd` what `capget` and `PR_CAPBSET_READ` say, `NoNewPrivs` follows the `prctl` and `Cpus_allowed` the affinity; a thread's is its own, and a zombie's has no `Umask` or `Vm` lines and has `FDSize` 0 |
| `execat` | `execveat` executes through a directory fd, through the fd itself (also `O_PATH` and close-on-exec), with an absolute path and a `dirfd` that does not exist, and `fexecve`; and refuses with Linux's errno: `""` without `AT_EMPTY_PATH` `ENOENT`, an unknown flag `EINVAL`, `AT_SYMLINK_NOFOLLOW` on a link or `/proc/self/exe` `ELOOP`, a file as `dirfd` `ENOTDIR`, an fd that does not exist `EBADF`, a directory, `AT_FDCWD` or a socket `EACCES`. `/proc/self/exe` is the file from the root and without links after `./prog`, `/proc/self/fd/N`, a link or a linked directory; `comm` is the name the caller used (`link`, `N`), and a script's is its own and not the interpreter's |
| `ctxsw` | 25 sleeps of 2 ms are 25 voluntary context switches and fewer than 25 involuntary ones (`RUSAGE_THREAD`); `voluntary_ctxt_switches:` and `nonvoluntary_ctxt_switches:` in `/proc/self/status` are the thread's, and those in `/proc/self/task/<tid>/status` that thread's; `RUSAGE_SELF` counts those of a thread that has already finished; `wait4` gives a child's and its grandchild's, and `RUSAGE_CHILDREN` adds them up; and two processes that do not sleep, on the same CPU, take it from each other: involuntary on both |
