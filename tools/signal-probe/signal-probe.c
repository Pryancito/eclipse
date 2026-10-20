// Eclipse OS: an integrity probe for signal delivery and return.
//
// A signal is the one place where the kernel takes a running program, runs
// some other code of it, and has to put it back exactly as it was. Every call
// involved can return 0 while the putting-back is wrong, and in this kernel it
// has been wrong in ways no syscall-level test sees:
//
//   * the context a signal interrupted was kept in ONE slot per thread, so a
//     second signal arriving inside a handler overwrote the first one's, and
//     the outer handler's `sigreturn` failed with EINVAL -- and a `sigreturn`
//     that fails RETURNS, into whatever follows libc's restorer. (#1943)
//   * a fault taken after leaving a handler with `siglongjmp` killed the
//     process even with a SIGSEGV handler installed. (#1943)
//   * a handler that rewrote the saved `rip` to a non-canonical address took
//     the whole kernel down: the CPU faults on the way back to user mode, in
//     ring 0. (#1943)
//   * an orphan kept the death signal its dead parent chose, so a child cloned
//     with SIGTERM or SIGUSR1 whose parent exited sent it to init, which
//     reboots or halts on those; and an orphan's death was routed from the
//     parent it was FORKED from, not the one that adopted it.
//   * a `waitpid` could reap a dead parent before its children had gone to
//     their adopter, so the subreaper that reaped it was told ECHILD for the
//     orphan it was about to be handed, about one run in thirty.
//   * a zombie gave its pid back before it was reaped, so a fork could be
//     handed it while the zombie still waited, and a `waitpid` for the new
//     child collected the dead one.
//   * an alarm a process left pending when it died went off in the process
//     its pid went to next, which died of it.
//   * a SIGSTOP (or Ctrl-Z's SIGTSTP) to a process blocked in a syscall did
//     not stop it until the syscall returned by itself -- never, for a child
//     in `pause()` -- and its parent's `waitpid(WUNTRACED)` waited with it.
//   * a kill did not wake a thread asleep in `pause`, `nanosleep`, `read` or
//     `waitpid`: `execve` from a second thread waited for good for its
//     sibling in `pause` to die, and a process that exited with a thread
//     asleep was reaped but never torn down, its memory never given back.
//   * the image an `execve` from a second thread started ran with that
//     thread's tid, not the pid: `gettid() != getpid()` in what Linux makes
//     the first thread, and `tgkill(pid, pid)` reached nobody.
//   * a parent waiting in `vfork` for a child that never got as far as
//     `exec` could not be killed, and a process that exited with a thread
//     in that wait was never torn down.
//   * a handler run over a thread waiting for a contended
//     `PTHREAD_PRIO_INHERIT` mutex gave that mutex a second owner: the lock
//     answered EINTR, which glibc's `pthread_mutex_lock` takes for the lock
//     taken. Linux makes the lock again whatever the handler.
//   * a `futex` wait with no timeout came back EINTR from a SA_RESTART
//     handler, which Linux makes again, so glibc's `sem_wait` returned -1
//     with no semaphore to a program that installed its handlers with
//     `signal()`.
//   * a `vfork` child ran in a COPY of its parent's memory, so what it wrote
//     before its `exec` or `_exit` the parent never saw: glibc's
//     `posix_spawn` of a program that does not exist returned 0, because it
//     hears about the failed `exec` through a word the child writes.
//   * an `execve` of a file that is not a program killed the caller with
//     SIGSEGV instead of returning: garbage, a script without `#!` (which a
//     shell then runs itself, on ENOEXEC), a directory, a binary for another
//     machine, a dynamic binary whose `PT_INTERP` is not installed.
//   * `exit(-1)` reached the parent as a death by SIGHUP: the whole `int`
//     was kept, and a negative exit code is how a kill is spelled inside.
//   * a forked child and a new thread started at nice 0, on SCHED_OTHER and
//     free to run on any CPU, whatever their parent had: `nice make` only
//     reniced make. And SCHED_RESET_ON_FORK was accepted and thrown away.
//   * a `#!` interpreter reached through two symbolic links was ELOOP (one
//     was the most), and an interpreter nobody may execute still ran.
//   * a static-pie -- what musl's `gcc -static` builds, this probe included
//     -- was loaded at address 0, so a null pointer read its ELF header
//     instead of faulting.
//   * the kernel time `getrusage` and `times` reported was the wall time of
//     one syscall in sixteen, times sixteen, so a shell that had waited
//     half a second used seconds of CPU; `wait4` reported almost nothing for
//     a child that had called `exit`; and the thread CPU clock left the
//     kernel out.
//   * `RLIMIT_FSIZE` held back `truncate`, `ftruncate` and `pwrite` only:
//     `write`, `writev` and `fallocate` went past it, a `pwrite` across it
//     failed instead of stopping at it, a file already past it could not be
//     cut down, and no `SIGXFSZ` came with any `EFBIG`.
//   * ITIMER_VIRTUAL and ITIMER_PROF counted wall-clock time, so they went
//     off in a process asleep, and RLIMIT_CPU sent neither SIGXCPU nor
//     SIGKILL: `ulimit -t` let a runaway loop run for ever.
//   * a zombie was not a pid to anybody but its parent once its last thread
//     was gone (no `getpgid`, no `pidfd_open`, no `/proc/<pid>`, so `ps`
//     never showed a `<defunct>`), a signal to one still finishing its
//     teardown was ESRCH, and a child reaped while it finished was still
//     found by `kill(pid, 0)` right after the `waitpid`.
//   * a new thread, and a forked child, were called by the program's name
//     instead of their creator's; field 38 of /proc/<pid>/stat read 0
//     instead of the signal the parent gets; and a parent woken by the EOF
//     of its child's close-on-exec pipe could still move it to a group.
//   * nobody counted context switches: `ru_nvcsw` and `ru_nivcsw` were 0
//     and /proc/<pid>/status had no lines for them; and what a child had
//     reaped of its own was not the child's, so `wait4` and RUSAGE_CHILDREN
//     left out the grandchildren that did the work.
//
// Every section states what Linux does and checks it, printing the numbers
// whether it passes or fails. Each section runs in a child of its own, so a
// section that kills its process is reported as a FAIL and the rest still run.
//
//   nest       a handler interrupted by a second signal comes back to where
//              it was, with its vector registers, and so does the program.
//   longjmp    a handler left with `siglongjmp` does not stop the next fault
//              from reaching its handler; neither does a fault taken inside
//              another signal's handler.
//   fork       a child forked inside a handler returns from that handler.
//   restart    SA_RESTART restarts `read` and `wait4` after the handler, and
//              does not restart `nanosleep` or `poll`, which Linux never does.
//              It restarts `sem_wait` too, an untimed futex wait. Taking a
//              contended PI mutex is made again by any handler.
//   badrip     a handler that sets a non-canonical `rip` in its ucontext gets
//              the process a SIGSEGV (SI_KERNEL), and nothing more. x86_64.
//   ids        the kernel refuses what it cannot load: a TLS pointer outside
//              user space (`arch_prctl`, `clone(CLONE_SETTLS)`), and the pid
//              INT_MIN, which has no negation, in `waitpid`.
//   orphan     an orphan's death tells its ADOPTER, with SIGCHLD whatever
//              signal its dead parent chose at clone(2), and wakes the
//              adopter's `waitpid` even when a process in between volunteered
//              as a subreaper after the adoption. And once `waitpid` has
//              reaped the dead parent, the orphan is already the adopter's.
//   inherit    what `fork` and `execve` hand down. A forked child keeps the
//              handlers, the mask and the alternate stack, and starts with no
//              pending signal and no alarm. An exec'd image keeps the mask,
//              the pending signals, the alarm and what was ignored, and loses
//              the handlers and the alternate stack.
//   wait       what `waitid` and `waitpid` report: an exit, a death by
//              signal, a stop and a continue (each once), `WNOWAIT` leaving the
//              child to be reaped again, a negative exit code as its low
//              byte, `P_PGID` 0 as the caller's group,
//              `SA_NOCLDSTOP`; and a process ended or exec'd from a thread
//              that is not its first.
//   stop       a stop reaches a process blocked in a syscall -- `nanosleep`,
//              `read`, `sigsuspend` -- and the syscall goes on once it is
//              continued: the read reads, the sigsuspend waits for its signal
//              and puts the old mask back.
//   teardown   a process that exits with a second thread asleep -- in
//              `pause`, `read`, `nanosleep`, `waitpid`, or the `vfork` of a
//              child that never execs -- has lost that thread by the time it
//              is reaped: a robust mutex the thread held comes back
//              EOWNERDEAD.
//   spawn      a `vfork` child runs in its parent's memory: what it writes
//              there the parent reads, so `posix_spawn` reports a program
//              that does not exist and a file action that fails. And an
//              `exec` from the child leaves the parent its program.
//   badexec    an `execve` of what is not a program comes back with the errno
//              Linux gives -- EACCES, ENOEXEC, ENOENT, EIO, ELIBBAD, ELOOP --
//              to a caller still alive to read it, and `sh` runs a script
//              that has no `#!` line.
//   sched      what a fork or a new thread takes from its parent: the nice
//              value, the CPU mask and the policy, or the default class when
//              the parent set SCHED_RESET_ON_FORK, which the parent keeps,
//              reports, and cannot take off without privilege. And the size
//              `sched_getaffinity` wants: whole longs.
//   image      the program is mapped above the null page, and reading
//              through a null pointer kills with SIGSEGV, however it was
//              linked (static, static-pie, dynamic).
//   cputime    the CPU time `getrusage`, `times`, `wait4` and the CPU clocks
//              report: none for a process asleep, the 300 ms a child worked
//              (a thread still working at `exit_group` included, and a
//              grandchild's, which the child reaped), and the kernel's part
//              of a loop of syscalls on the CPU clocks.
//   fsize      `RLIMIT_FSIZE`: a write across it stops at it and one at it is
//              EFBIG with a SIGXFSZ, which kills by default, whatever the
//              call; a new length past it is refused only when it grows the
//              file; and a pipe is not a file.
//   cputimer   ITIMER_VIRTUAL and ITIMER_PROF do not move while the process
//              sleeps and go off at 100 ms of its CPU time, every 20 ms of it
//              with an interval; RLIMIT_CPU sends SIGXCPU at the soft limit,
//              which moves up a second, and SIGKILL at the hard one.
//   cpuclock   a `timer_create` timer on a CPU clock counts that clock: the
//              process's, a thread's own, another thread's and another
//              process's (`pthread_getcpuclockid`, `clock_getcpuclockid`);
//              asleep it waits, an absolute expiry gone by fires at once.
//   aslimit    RLIMIT_AS 4 MiB above VmSize: 2 MiB of mmap fit, 8 do not
//              (ENOMEM), nor a 16 MiB malloc, a brk of 8 MiB, an mremap to
//              16 MiB or an 8 MiB shmat; a MAP_FIXED over mapped pages and
//              what munmap gave back fit.
//   nproc      RLIMIT_NPROC holds a user of its own to 4 tasks: three
//              children and then EAGAIN, for `fork` and `pthread_create`
//              alike; a zombie counts until it is reaped; root, real or
//              effective, forks at a limit of 0.
//   zombie     a dead child is a pid until it is reaped, for whoever asks:
//              kill and sigqueue answer 0, getpgid and getsid its group and
//              session, pidfd_open a pidfd that is ready, /proc/<pid> a `Z`
//              and an empty cmdline. Reaped, every one of them is ESRCH or
//              gone, however soon after the `waitpid`.
//   kin        a new thread and a forked child carry their creator's name;
//              stat field 38 is SIGCHLD, and -1 for a thread that is not
//              the leader, which `pidfd_open` refuses with ENOENT; a child
//              whose exec closed its close-on-exec pipe is EACCES to its
//              parent's setpgid, 20 times out of 20.
//   status     /proc/<pid>/status has Linux's lines in Linux's order, and they
//              say what the syscalls say: Umask, FDSize (256 with fd 255
//              open), the NS ids, the pending, blocked, ignored and caught
//              signals, SigQ, the capability sets, NoNewPrivs, the CPUs. A
//              thread's file is the thread's; a zombie's has no Umask, no Vm
//              lines and an FDSize of 0.
//   execat     `execveat` runs a file by a directory fd, by its own fd
//              (`O_PATH` and close-on-exec too) and by `fexecve`, and refuses
//              what Linux refuses with Linux's errno. `/proc/self/exe` is the
//              file from the root through every link, after `./prog`,
//              `/proc/self/fd/N` or a link; `comm` is the name the caller
//              used, a script's own and not its interpreter's.
//   ctxsw      a sleep is a voluntary context switch and sharing one CPU
//              with a process that never sleeps a nonvoluntary one, in
//              getrusage (RUSAGE_THREAD, RUSAGE_SELF with a thread gone,
//              RUSAGE_CHILDREN), wait4 (a child's and its child's) and
//              /proc/<pid>/status, where a thread's file is the thread's.
//
// Build:  make                 (x86_64-linux-musl-gcc -O2 -static)
// Run:    ./signal-probe [--only SECTION]
// Exit:   0 = every check held, 1 = at least one FAIL, 2 = bad usage.
//
// It is also meant to be run on Linux on the same box: every check here is
// one Linux holds, so a FAIL on Linux is a bug in the probe and a FAIL only on
// Eclipse is a bug in Eclipse.

#define _GNU_SOURCE
#include <dirent.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/times.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

// ---------------------------------------------------------------- reporting

static int g_fail;

static void check(int cond, const char *what, const char *fmt, ...) {
    va_list ap;
    printf("  %s  %-36s ", cond ? "PASS" : "FAIL", what);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
    if (!cond) g_fail++;
}

__attribute__((unused)) static void skip(const char *what, const char *why) {
    printf("  SKIP  %-36s %s\n", what, why);
    fflush(stdout);
}

static void install(int sig, void (*fn)(int, siginfo_t *, void *), int flags) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = fn;
    sa.sa_flags = SA_SIGINFO | flags;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, NULL);
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) == -1 && errno == EINTR) {
    }
}

// ------------------------------------------------------------------- nest

#if defined(__x86_64__)
// Send `sig` to this process with `v` in xmm8 for the whole of the syscall,
// and hand back what xmm8 holds once it returns. The signal is delivered on
// the way out of this very syscall, so xmm8 is live across the handler, and
// nothing but the kernel's saved context can put it back.
static double kill_self_holding_xmm8(int sig, double v) {
    double out;
    long ret;
    long pid = getpid();
    __asm__ volatile("movsd %[v], %%xmm8\n\t"
                     "syscall\n\t"
                     "movsd %%xmm8, %[out]\n\t"
                     : [out] "=m"(out), "=a"(ret)
                     : [v] "m"(v), "a"((long)SYS_kill), "D"(pid), "S"((long)sig)
                     : "rcx", "r11", "xmm8", "memory");
    (void)ret;
    return out;
}

static void clobber_xmm8(void) {
    __asm__ volatile("pxor %%xmm8, %%xmm8" ::: "xmm8");
}
#endif

static volatile sig_atomic_t g_depth, g_max_depth, g_inner_runs, g_outer_runs;
static volatile double g_outer_kept;

static void inner_handler(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_inner_runs++;
    if (g_depth + 1 > g_max_depth) g_max_depth = g_depth + 1;
#if defined(__x86_64__)
    clobber_xmm8();
#endif
}

static void outer_handler(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_outer_runs++;
    g_depth = 1;
#if defined(__x86_64__)
    // SIGUSR2 is not in this handler's mask, so it nests right here.
    g_outer_kept = kill_self_holding_xmm8(SIGUSR2, 26.0);
    clobber_xmm8();
#else
    kill(getpid(), SIGUSR2);
    g_outer_kept = 26.0;
#endif
    g_depth = 0;
}

static void sec_nest(void) {
    install(SIGUSR1, outer_handler, 0);
    install(SIGUSR2, inner_handler, 0);
#if defined(__x86_64__)
    double main_kept = kill_self_holding_xmm8(SIGUSR1, 7.5);
#else
    kill(getpid(), SIGUSR1);
    double main_kept = 7.5;
#endif
    check(g_outer_runs == 1 && g_inner_runs == 1, "both handlers ran once",
          "outer %d, inner %d", (int)g_outer_runs, (int)g_inner_runs);
    check(g_max_depth == 2, "the second one ran inside the first",
          "deepest nesting %d", (int)g_max_depth);
    check(g_outer_kept == 26.0, "the outer handler's xmm8 survived",
          "%.2f (want 26.00)", g_outer_kept);
    check(main_kept == 7.5, "the program's xmm8 survived both",
          "%.2f (want 7.50)", main_kept);
    check(1, "the program came back after both", "the outer sigreturn did not fail");
}

// ---------------------------------------------------------------- longjmp

static sigjmp_buf g_jb;
static volatile sig_atomic_t g_segv_runs, g_segv_code, g_recovered;
// An address in the page at 0, which nothing maps. Through a volatile pointer
// so the compiler cannot see the store is to a null page and drop it.
static int *volatile g_unmapped = (int *)16;

static void segv_jump(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)uc;
    g_segv_runs++;
    g_segv_code = si->si_code;
    siglongjmp(g_jb, 1);
}

static void usr1_faults(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    if (sigsetjmp(g_jb, 1) == 0) *g_unmapped = 1;
    g_recovered = 1;
}

static void sec_longjmp(void) {
    install(SIGSEGV, segv_jump, 0);
    for (volatile int i = 0; i < 3; i++) {
        if (sigsetjmp(g_jb, 1) == 0) *g_unmapped = 1;
    }
    check(g_segv_runs == 3, "three faults, three handler runs",
          "%d of 3 (each one left with siglongjmp)", (int)g_segv_runs);
    check(g_segv_code == SEGV_MAPERR, "a NULL page is SEGV_MAPERR", "si_code %d",
          (int)g_segv_code);

    // A fault inside the handler for ANOTHER signal: SIGSEGV is not blocked
    // there, so its handler runs nested.
    g_segv_runs = 0;
    install(SIGUSR1, usr1_faults, 0);
    kill(getpid(), SIGUSR1);
    check(g_segv_runs == 1 && g_recovered, "a fault inside a SIGUSR1 handler",
          "SIGSEGV handler ran %d time(s), SIGUSR1 handler finished: %s", (int)g_segv_runs,
          g_recovered ? "yes" : "no");
}

// ------------------------------------------------------------------- fork

static volatile sig_atomic_t g_forked_child;

static void usr1_forks(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    if (fork() == 0) g_forked_child = 1;
}

static void sec_fork(void) {
    install(SIGUSR1, usr1_forks, 0);
    kill(getpid(), SIGUSR1);
    // Both processes return from the handler to here.
    if (g_forked_child) _exit(42);
    int status = 0;
    pid_t got = wait(&status);
    check(got > 0 && WIFEXITED(status) && WEXITSTATUS(status) == 42,
          "the child returned from the handler",
          WIFSIGNALED(status) ? "child killed by signal %d" : "child status %#x",
          WIFSIGNALED(status) ? WTERMSIG(status) : status);
}

// ---------------------------------------------------------------- restart

static volatile sig_atomic_t g_usr1_runs;

static void usr1_counts(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_usr1_runs++;
}

// A helper that signals this process after `signal_ms`, then (optionally)
// writes one byte into `fd` after `write_ms` more, and exits.
static pid_t signal_me_later(long signal_ms, int fd, long write_ms) {
    pid_t parent = getpid();
    pid_t pid = fork();
    if (pid == 0) {
        sleep_ms(signal_ms);
        kill(parent, SIGUSR1);
        if (fd >= 0) {
            sleep_ms(write_ms);
            if (write(fd, "x", 1) != 1) _exit(1);
        }
        _exit(0);
    }
    return pid;
}

static void read_across_a_signal(int restart) {
    int p[2];
    if (pipe(p) != 0) {
        check(0, "pipe", "%s", strerror(errno));
        return;
    }
    install(SIGUSR1, usr1_counts, restart ? SA_RESTART : 0);
    g_usr1_runs = 0;
    pid_t helper = signal_me_later(150, p[1], 150);
    char c;
    errno = 0;
    ssize_t n = read(p[0], &c, 1);
    int e = errno;
    waitpid(helper, NULL, 0);
    if (restart)
        check(n == 1 && g_usr1_runs == 1, "read: restarted with SA_RESTART",
              "returned %zd (%s), handler ran %d time(s)", n, n < 0 ? strerror(e) : "data",
              (int)g_usr1_runs);
    else
        check(n == -1 && e == EINTR && g_usr1_runs == 1, "read: EINTR without it",
              "returned %zd (%s), handler ran %d time(s)", n, n < 0 ? strerror(e) : "data",
              (int)g_usr1_runs);
    close(p[0]);
    close(p[1]);
}

static sem_t g_sem;
static volatile int g_sem_err;

static void *waits_for_the_semaphore(void *arg) {
    (void)arg;
    g_sem_err = sem_wait(&g_sem) == 0 ? 0 : errno;
    return NULL;
}

// An untimed `sem_wait` is a FUTEX_WAIT_BITSET with no timeout, which Linux
// answers -ERESTARTSYS: a SA_RESTART handler leaves it waiting. glibc hands an
// EINTR from it to the caller.
static void sem_wait_across_a_signal(void) {
    if (sem_init(&g_sem, 0, 0) != 0) {
        check(0, "sem_wait: restarted with SA_RESTART", "sem_init: %s", strerror(errno));
        return;
    }
    install(SIGUSR1, usr1_counts, SA_RESTART);
    g_usr1_runs = 0;
    g_sem_err = -1;
    pthread_t t;
    if (pthread_create(&t, NULL, waits_for_the_semaphore, NULL) != 0) {
        check(0, "sem_wait: restarted with SA_RESTART", "pthread_create failed");
        return;
    }
    sleep_ms(100);
    pthread_kill(t, SIGUSR1);
    sleep_ms(100);
    int before_the_post = g_sem_err;
    sem_post(&g_sem);
    pthread_join(t, NULL);
    check(before_the_post == -1 && g_sem_err == 0 && g_usr1_runs == 1,
          "sem_wait: restarted with SA_RESTART",
          "before the post: %s, after it: %s, handler ran %d time(s)",
          before_the_post == -1 ? "waiting" : before_the_post ? strerror(before_the_post) : "took it",
          g_sem_err ? strerror(g_sem_err) : "took it", (int)g_usr1_runs);
    sem_destroy(&g_sem);
}

static pthread_mutex_t g_pi;
static volatile int g_pi_taken;

// Never unlocks: the mutex goes with the probe.
static void *takes_the_pi_mutex(void *arg) {
    (void)arg;
    if (pthread_mutex_lock(&g_pi) == 0) g_pi_taken = 1;
    return NULL;
}

// A thread waiting in FUTEX_LOCK_PI for a mutex this thread holds, and a
// handler run over it with no SA_RESTART. Linux makes the lock again
// (-ERESTARTNOINTR): glibc handles ESRCH and EDEADLK from it and takes
// anything else for the lock taken, so an EINTR here is a second owner. musl
// loops on EINTR itself, so only a glibc build sees it.
static void pi_lock_across_a_signal(void) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    int r = pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT);
    if (r == 0) r = pthread_mutex_init(&g_pi, &a);
    pthread_mutexattr_destroy(&a);
    if (r != 0) {
        check(0, "FUTEX_LOCK_PI: made again anyway", "PI mutex: %s", strerror(r));
        return;
    }
    install(SIGUSR1, usr1_counts, 0);
    g_usr1_runs = 0;
    g_pi_taken = 0;
    pthread_mutex_lock(&g_pi);
    pthread_t t;
    if (pthread_create(&t, NULL, takes_the_pi_mutex, NULL) != 0) {
        check(0, "FUTEX_LOCK_PI: made again anyway", "pthread_create failed");
        pthread_mutex_unlock(&g_pi);
        return;
    }
    sleep_ms(100);
    pthread_kill(t, SIGUSR1);
    sleep_ms(100);
    int while_held = g_pi_taken;
    pthread_mutex_unlock(&g_pi);
    pthread_join(t, NULL);
    check(!while_held && g_pi_taken && g_usr1_runs == 1,
          "FUTEX_LOCK_PI: made again anyway",
          "taken while held: %s, taken after the unlock: %s, handler ran %d time(s)",
          while_held ? "YES" : "no", g_pi_taken ? "yes" : "no", (int)g_usr1_runs);
}

static void sec_restart(void) {
    read_across_a_signal(1);
    read_across_a_signal(0);

    // wait4 for a child that is still running when the signal comes.
    install(SIGUSR1, usr1_counts, SA_RESTART);
    g_usr1_runs = 0;
    pid_t slow = fork();
    if (slow == 0) {
        sleep_ms(300);
        _exit(7);
    }
    pid_t helper = signal_me_later(100, -1, 0);
    int status = 0;
    errno = 0;
    pid_t got = waitpid(slow, &status, 0);
    int e = errno;
    waitpid(helper, NULL, 0);
    check(got == slow && WIFEXITED(status) && WEXITSTATUS(status) == 7,
          "wait4: restarted with SA_RESTART", "returned %d (%s), handler ran %d time(s)",
          (int)got, got < 0 ? strerror(e) : "the child", (int)g_usr1_runs);

    // Linux never restarts these after a handler, SA_RESTART or not.
    g_usr1_runs = 0;
    helper = signal_me_later(100, -1, 0);
    struct timespec ts = {2, 0};
    errno = 0;
    int r = nanosleep(&ts, &ts);
    e = errno;
    waitpid(helper, NULL, 0);
    check(r == -1 && e == EINTR, "nanosleep: EINTR even so", "returned %d (%s), %ld.%03lds left",
          r, r < 0 ? strerror(e) : "slept", (long)ts.tv_sec, ts.tv_nsec / 1000000L);

    int p[2];
    if (pipe(p) == 0) {
        g_usr1_runs = 0;
        helper = signal_me_later(100, -1, 0);
        struct pollfd pfd = {p[0], POLLIN, 0};
        errno = 0;
        r = poll(&pfd, 1, 2000);
        e = errno;
        waitpid(helper, NULL, 0);
        check(r == -1 && e == EINTR, "poll: EINTR even so", "returned %d (%s)", r,
              r < 0 ? strerror(e) : "ready");
        close(p[0]);
        close(p[1]);
    }

    sem_wait_across_a_signal();
    pi_lock_across_a_signal();
}

// ----------------------------------------------------------------- badrip

static volatile sig_atomic_t g_bad_segv_code = -1;

static void segv_records_and_jumps(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)uc;
    g_bad_segv_code = si->si_code;
    siglongjmp(g_jb, 1);
}

#if defined(__x86_64__)
static void usr1_breaks_rip(int sig, siginfo_t *si, void *ucv) {
    (void)sig, (void)si;
    ucontext_t *uc = ucv;
    // The first address past the lower canonical half.
    uc->uc_mcontext.gregs[REG_RIP] = (greg_t)0x0000800000000000ULL;
}
#endif

static void sec_badrip(void) {
#if defined(__x86_64__)
    install(SIGSEGV, segv_records_and_jumps, 0);
    install(SIGUSR1, usr1_breaks_rip, 0);
    int returned_normally = 0;
    if (sigsetjmp(g_jb, 1) == 0) {
        kill(getpid(), SIGUSR1);
        returned_normally = 1;
    }
    check(!returned_normally && g_bad_segv_code == SI_KERNEL,
          "a non-canonical rip is a SIGSEGV", "si_code %d (want SI_KERNEL, %d)%s",
          (int)g_bad_segv_code, SI_KERNEL,
          returned_normally ? "; the sigreturn came back to the program" : "");
#else
    skip("a non-canonical rip is a SIGSEGV", "x86_64 only");
#endif
}

// -------------------------------------------------------------------- ids

static void sec_ids(void) {
#if defined(__x86_64__)
    const unsigned long bad = 1UL << 63;
    errno = 0;
    long r = syscall(SYS_arch_prctl, 0x1002 /* ARCH_SET_FS */, bad);
    check(r == -1 && errno == EPERM, "arch_prctl(SET_FS, 1<<63)", "returned %ld (%s)", r,
          r < 0 ? strerror(errno) : "accepted");

    errno = 0;
    r = syscall(SYS_clone, (long)(SIGCHLD | CLONE_SETTLS), 0L, 0L, 0L, bad);
    if (r == 0) _exit(0);
    int e = errno;
    if (r > 0) waitpid((pid_t)r, NULL, 0);
    check(r == -1 && e == EPERM, "clone(CLONE_SETTLS, 1<<63)", "returned %ld (%s)", r,
          r < 0 ? strerror(e) : "a child was created");
#else
    skip("arch_prctl / clone TLS checks", "x86_64 only");
#endif

    int status;
    errno = 0;
    pid_t got = waitpid(INT_MIN, &status, WNOHANG);
    check(got == -1 && errno == ESRCH, "waitpid(INT_MIN)", "returned %d (%s)", (int)got,
          got < 0 ? strerror(errno) : "a child");
}

// ----------------------------------------------------------------- orphan

// A child's death signals its parent with the signal that parent chose at
// clone(2). An orphan's death signals its ADOPTER, and with SIGCHLD: Linux
// puts the choice back when it reparents (`reparent_leader`, "We don't want
// people slaying init"). The signal chosen here is SIGWINCH, which init
// discards: if the subreaper did not hold, the orphan would go to init, and
// eclipse-init reboots or halts on SIGTERM, SIGINT, SIGUSR1 and SIGUSR2.

static volatile sig_atomic_t g_winch_runs, g_chld_for, g_watch_pid;

static void winch_counts(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_winch_runs++;
}

static void chld_watches(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)uc;
    if (si->si_pid == g_watch_pid) g_chld_for++;
}

static void alarm_interrupts(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
}

static int send_pid(int fd, pid_t pid) {
    return write(fd, &pid, sizeof pid) == (ssize_t)sizeof pid;
}

static pid_t recv_pid(int fd) {
    pid_t pid;
    return read(fd, &pid, sizeof pid) == (ssize_t)sizeof pid ? pid : -1;
}

// Fork a parent that clones one child with `exit_sig` as its death signal and
// exits at once. The child waits until it has been adopted, then for one byte
// on `go`, and exits. Returns the orphan's pid (its parent already reaped).
static pid_t make_orphan(int exit_sig, int go) {
    int p[2];
    if (pipe(p) != 0) return -1;
    pid_t parent = fork();
    if (parent == 0) {
        pid_t me = getpid();
        long c = syscall(SYS_clone, (long)exit_sig, 0L, 0L, 0L, 0L);
        if (c == 0) {
            for (int i = 0; i < 400 && getppid() == me; i++) sleep_ms(5);
            char b;
            _exit(read(go, &b, 1) == 1 ? 0 : 3);
        }
        _exit(c > 0 && send_pid(p[1], (pid_t)c) ? 0 : 2);
    }
    close(p[1]);
    pid_t orphan = recv_pid(p[0]);
    close(p[0]);
    waitpid(parent, NULL, 0);
    return orphan;
}

static void sec_orphan(void) {
    if (prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) != 0) {
        skip("orphan checks", "PR_SET_CHILD_SUBREAPER refused: an orphan would go to init");
        return;
    }
    install(SIGWINCH, winch_counts, SA_RESTART);
    install(SIGCHLD, chld_watches, SA_RESTART);

    // 1. The signal the dead parent chose stays with the dead parent.
    int go[2];
    if (pipe(go) != 0) {
        check(0, "pipe", "%s", strerror(errno));
        return;
    }
    g_winch_runs = 0;
    g_chld_for = 0;
    pid_t orphan = make_orphan(SIGWINCH, go[0]);
    g_watch_pid = orphan;
    if (write(go[1], "x", 1) != 1) check(0, "write", "%s", strerror(errno));
    int st = 0;
    pid_t got;
    while ((got = waitpid(orphan, &st, 0)) == -1 && errno == EINTR) {
    }
    int e = errno;
    sleep_ms(50);
    check(got == orphan && orphan > 0, "we adopted the orphan and reaped it",
          "waitpid returned %d for %d (%s)", (int)got, (int)orphan,
          got < 0 ? strerror(e) : "the orphan");
    check(g_winch_runs == 0, "not sent its dead parent's signal", "SIGWINCH came %d time(s)",
          (int)g_winch_runs);
    check(g_chld_for == 1, "sent SIGCHLD instead", "SIGCHLD for it came %d time(s)",
          (int)g_chld_for);
    close(go[0]);
    close(go[1]);

    // 2. A process between us and the dead parent volunteers AFTER we
    //    adopted. The orphan is still ours: our blocked waitpid must wake,
    //    and the volunteer must not be handed a zombie of a stranger.
    int to_us[2], dead[2];
    if (pipe(go) != 0 || pipe(to_us) != 0 || pipe(dead) != 0) {
        check(0, "pipe", "%s", strerror(errno));
        return;
    }
    pid_t mid = fork();
    if (mid == 0) {
        pid_t o = make_orphan(SIGCHLD, go[0]);
        if (prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) != 0) _exit(4);
        if (!send_pid(to_us[1], o)) _exit(5);
        sleep_ms(300);  // so the adopter is asleep in waitpid first
        if (write(go[1], "x", 1) != 1) _exit(6);
        char b;
        if (read(dead[0], &b, 1) != 1) _exit(7);
        int s2;
        errno = 0;
        pid_t w = waitpid(-1, &s2, WNOHANG);
        _exit(w == -1 && errno == ECHILD ? 0 : w > 0 ? 1 : 8);
    }
    orphan = recv_pid(to_us[0]);
    g_watch_pid = orphan;
    g_chld_for = 0;
    install(SIGALRM, alarm_interrupts, 0);
    alarm(3);
    errno = 0;
    got = waitpid(orphan, &st, 0);
    e = errno;
    alarm(0);
    if (got != orphan) waitpid(orphan, &st, WNOHANG);
    sleep_ms(50);
    check(got == orphan, "a blocked waitpid woke when it died", "returned %d (%s)", (int)got,
          got < 0 ? strerror(e) : "the orphan");
    check(g_chld_for == 1, "and we were sent its SIGCHLD", "SIGCHLD for it came %d time(s)",
          (int)g_chld_for);
    if (write(dead[1], "x", 1) != 1) check(0, "write", "%s", strerror(errno));
    int ms = 0;
    waitpid(mid, &ms, 0);
    int code = WIFEXITED(ms) ? WEXITSTATUS(ms) : -1;
    check(code == 0, "the late volunteer got no stranger", "%s (exit %d)",
          code == 0 ? "its waitpid(-1) said ECHILD"
          : code == 1 ? "its waitpid(-1) returned the orphan, never its child"
                      : "the volunteer itself failed",
          code);
    close(go[0]);
    close(go[1]);
    signal(SIGALRM, SIG_DFL);
    alarm(20);

    // 3. Linux hands the children over in `exit_notify`, before the parent
    //    is a zombie anyone can reap: once our waitpid has reaped it, its
    //    child is ours. A waitpid that lands in between is a matter of
    //    timing, so it is asked many times.
    if (pipe(go) != 0) {
        check(0, "pipe", "%s", strerror(errno));
        return;
    }
    g_watch_pid = 0;
    int rounds = 128, strays = 0, unmade = 0;
    for (int i = 0; i < rounds; i++) {
        pid_t o = make_orphan(SIGCHLD, go[0]);
        if (o <= 0) {
            unmade++;
            continue;
        }
        if (waitpid(o, &st, WNOHANG) != 0) strays++;
        if (write(go[1], "x", 1) != 1) check(0, "write", "%s", strerror(errno));
        for (int tries = 0; tries < 200; tries++) {
            pid_t w = waitpid(o, &st, 0);
            if (w == o || (w == -1 && errno != ECHILD && errno != EINTR)) break;
            if (w == -1 && errno == ECHILD) sleep_ms(5);
        }
    }
    check(strays == 0 && unmade == 0, "its parent reaped, the orphan is ours",
          "%d of %d orphans not ours yet (%d not made)", strays, rounds, unmade);
    close(go[0]);
    close(go[1]);
}

// ---------------------------------------------------------------- inherit

// fork(2) and execve(2) each hand a process's signal state down by their own
// rules, and each item is a separate line of kernel code that can be missed:
//
//                      fork child          execve'd image
//   handlers           kept                reset to SIG_DFL (SIG_IGN stays)
//   mask               kept                kept
//   pending            empty               kept
//   alarm              none                kept
//   alternate stack    kept                gone
//
// The exec half is this same binary run again with --exec-report.

static const char *g_self = "signal-probe";
static char g_inherit_stack[65536];

static void handler_noop(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
}

static int is_blocked(int sig) {
    sigset_t cur;
    sigprocmask(SIG_BLOCK, NULL, &cur);
    return sigismember(&cur, sig);
}

static int is_pending(int sig) {
    sigset_t p;
    sigpending(&p);
    return sigismember(&p, sig);
}

static void *disposition(int sig) {
    struct sigaction sa;
    sigaction(sig, NULL, &sa);
    return (sa.sa_flags & SA_SIGINFO) ? (void *)sa.sa_sigaction : (void *)sa.sa_handler;
}

static const char *disposition_name(void *d) {
    if (d == (void *)SIG_DFL) return "SIG_DFL";
    if (d == (void *)SIG_IGN) return "SIG_IGN";
    if (d == (void *)handler_noop) return "the handler";
    return "something else";
}

// Runs in the image `sec_inherit` exec'd, and returns its failure count.
static int exec_report(void) {
    void *usr1 = disposition(SIGUSR1), *hup = disposition(SIGHUP);
    check(usr1 == (void *)SIG_DFL, "exec: a caught signal is SIG_DFL", "%s",
          disposition_name(usr1));
    check(hup == (void *)SIG_IGN, "exec: an ignored one stays ignored", "%s",
          disposition_name(hup));
    check(is_blocked(SIGUSR2), "exec: the mask is kept", "SIGUSR2 %s",
          is_blocked(SIGUSR2) ? "blocked" : "unblocked");
    check(is_pending(SIGUSR2), "exec: a pending signal is kept", "SIGUSR2 %s",
          is_pending(SIGUSR2) ? "pending" : "gone");
    unsigned left = alarm(0);
    check(left > 0 && left <= 100, "exec: the alarm is kept", "%u s left", left);
    stack_t ss;
    sigaltstack(NULL, &ss);
    check(ss.ss_flags & SS_DISABLE, "exec: the alternate stack is gone", "ss_flags %#x",
          (unsigned)ss.ss_flags);
    return g_fail;
}

static void sec_inherit(void) {
    install(SIGUSR1, handler_noop, 0);
    signal(SIGHUP, SIG_IGN);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR2);  // blocked, so it stays pending
    stack_t ss = {.ss_sp = g_inherit_stack, .ss_size = sizeof g_inherit_stack, .ss_flags = 0};
    if (sigaltstack(&ss, NULL) != 0) check(0, "sigaltstack", "%s", strerror(errno));
    alarm(100);  // in place of the section's 20 s guard, put back below

    pid_t c = fork();
    if (c == 0) {
        g_fail = 0;
        void *usr1 = disposition(SIGUSR1);
        check(usr1 == (void *)handler_noop, "fork: the handler is kept", "%s",
              disposition_name(usr1));
        check(is_blocked(SIGUSR2), "fork: the mask is kept", "SIGUSR2 %s",
              is_blocked(SIGUSR2) ? "blocked" : "unblocked");
        check(!is_pending(SIGUSR2), "fork: nothing is pending", "SIGUSR2 %s",
              is_pending(SIGUSR2) ? "pending" : "not pending");
        unsigned left = alarm(0);
        check(left == 0, "fork: no alarm", "%u s left", left);
        stack_t cs;
        sigaltstack(NULL, &cs);
        check(!(cs.ss_flags & SS_DISABLE) && cs.ss_sp == g_inherit_stack,
              "fork: the alternate stack is kept", "ss_flags %#x, %s", (unsigned)cs.ss_flags,
              cs.ss_sp == g_inherit_stack ? "same stack" : "another stack");
        _exit(g_fail > 100 ? 100 : g_fail);
    }
    int st = 0;
    while (waitpid(c, &st, 0) == -1 && errno == EINTR) {
    }
    if (WIFEXITED(st))
        g_fail += WEXITSTATUS(st);
    else
        check(0, "fork child", "died of signal %d", WIFSIGNALED(st) ? WTERMSIG(st) : 0);

    c = fork();
    if (c == 0) {
        // The fork above took the pending signal and the alarm away: give
        // this child both again, so the exec has something to keep.
        raise(SIGUSR2);
        alarm(100);
        char *args[] = {(char *)g_self, "--exec-report", NULL};
        execv("/proc/self/exe", args);
        execvp(g_self, args);
        _exit(101);
    }
    while (waitpid(c, &st, 0) == -1 && errno == EINTR) {
    }
    if (WIFEXITED(st) && WEXITSTATUS(st) == 101)
        check(0, "exec of this probe", "neither /proc/self/exe nor %s could be exec'd", g_self);
    else if (WIFEXITED(st))
        g_fail += WEXITSTATUS(st);
    else
        check(0, "exec'd image", "died of signal %d", WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    alarm(20);
}

// ------------------------------------------------------------------- wait

static volatile sig_atomic_t g_chld_runs;

static void chld_counts(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_chld_runs++;
}

static pid_t child_that_exits(int code, long after_ms) {
    pid_t c = fork();
    if (c == 0) {
        sleep_ms(after_ms);
        _exit(code);
    }
    return c;
}

static void *thread_exits(void *arg) {
    (void)arg;
    exit(7);
}

static void *thread_execs(void *arg) {
    (void)arg;
    // The mask is per thread and kept across an exec: the new image has to
    // find this one, not the first thread's.
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &usr1, NULL);
    char *args[] = {(char *)g_self, "--exec-from-thread", NULL};
    execv("/proc/self/exe", args);
    execvp(g_self, args);
    _exit(101);
}

static volatile sig_atomic_t g_usr2_here;

static void usr2_here(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    g_usr2_here = 1;
}

// Runs in the image a non-first thread exec'd: the exec makes that thread
// the process's first (`de_thread()`), so it takes the process's pid as its
// tid, keeps its own mask, and `tgkill(pid, pid)` reaches it. Exit 0 for all
// of it; 1 the tid is not the pid, 2 the mask was lost, 3 the tgkill missed.
static int exec_from_thread_report(void) {
    long tid = syscall(SYS_gettid);
    if (tid != (long)getpid()) return 1;
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    if (!sigismember(&now, SIGUSR1)) return 2;
    install(SIGUSR2, usr2_here, 0);
    if (syscall(SYS_tgkill, (long)getpid(), (long)getpid(), SIGUSR2) != 0) return 3;
    for (int i = 0; i < 100 && !g_usr2_here; i++) sleep_ms(1);
    return g_usr2_here ? 0 : 3;
}

static void sec_wait(void) {
    install(SIGCHLD, chld_counts, SA_RESTART);

    // An exit, through waitid and WNOWAIT, then reaped by waitpid.
    pid_t c = child_that_exits(42, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    int r = waitid(P_PID, (id_t)c, &si, WEXITED | WNOWAIT);
    check(r == 0 && si.si_pid == c && si.si_code == CLD_EXITED && si.si_status == 42,
          "waitid: an exit, WNOWAIT", "returned %d, si_pid %s, si_code %d, si_status %d", r,
          si.si_pid == c ? "right" : "wrong", si.si_code, si.si_status);
    int st = 0;
    pid_t got = waitpid(c, &st, WNOHANG);
    check(got == c && WIFEXITED(st) && WEXITSTATUS(st) == 42, "waitpid after WNOWAIT reaps it",
          "returned %d, status %#x", (int)got, st);

    // A negative code is an exit with its low byte, through both ways a
    // process can end itself: exit_group, and exit from its only thread.
    const long ends[] = {SYS_exit_group, SYS_exit};
    const int codes[] = {-1, -3};
    const char *whats[] = {"exit_group(-1) is an exit with 255", "exit(-3) of the last thread: 253"};
    for (int i = 0; i < 2; i++) {
        c = fork();
        if (c == 0) syscall(ends[i], codes[i]);
        st = 0;
        got = waitpid(c, &st, 0);
        check(got == c && WIFEXITED(st) && WEXITSTATUS(st) == (codes[i] & 0xff), whats[i],
              "returned %d, status %#x", (int)got, st);
    }

    // A death by signal.
    c = fork();
    if (c == 0) {
        for (;;) pause();
    }
    sleep_ms(50);
    kill(c, SIGTERM);
    memset(&si, 0, sizeof si);
    r = waitid(P_PID, (id_t)c, &si, WEXITED);
    check(r == 0 && si.si_code == CLD_KILLED && si.si_status == SIGTERM, "waitid: killed by SIGTERM",
          "returned %d, si_code %d (want %d), si_status %d", r, si.si_code, CLD_KILLED,
          si.si_status);

    // A stop and a continue, each reported once.
    c = fork();
    if (c == 0) {
        for (;;) pause();
    }
    sleep_ms(50);
    kill(c, SIGSTOP);
    got = waitpid(c, &st, WUNTRACED);
    check(got == c && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, "waitpid: a stop",
          "returned %d, status %#x", (int)got, st);
    got = waitpid(c, &st, WUNTRACED | WNOHANG);
    check(got == 0, "the stop is reported once", "a second waitpid returned %d", (int)got);
    kill(c, SIGCONT);
    sleep_ms(50);
    got = waitpid(c, &st, WCONTINUED);
    check(got == c && WIFCONTINUED(st), "waitpid: a continue", "returned %d, status %#x",
          (int)got, st);
    got = waitpid(c, &st, WCONTINUED | WNOHANG);
    check(got == 0, "the continue is reported once", "a second waitpid returned %d", (int)got);
    kill(c, SIGKILL);
    waitpid(c, &st, 0);

    // P_PGID with 0 is the caller's own group (Linux 5.4).
    c = child_that_exits(3, 0);
    memset(&si, 0, sizeof si);
    r = waitid(P_PGID, 0, &si, WEXITED);
    check(r == 0 && si.si_pid == c, "waitid(P_PGID, 0): our group", "returned %d (%s), si_pid %s",
          r, r ? strerror(errno) : "ok", si.si_pid == c ? "the child" : "another");

    // SA_NOCLDSTOP: a stop sends no SIGCHLD; the exit still does.
    install(SIGCHLD, chld_counts, SA_RESTART | SA_NOCLDSTOP);
    c = fork();
    if (c == 0) {
        for (;;) pause();
    }
    sleep_ms(50);
    g_chld_runs = 0;
    kill(c, SIGSTOP);
    waitpid(c, &st, WUNTRACED);
    sleep_ms(50);
    int on_stop = g_chld_runs;
    kill(c, SIGKILL);
    waitpid(c, &st, 0);
    sleep_ms(50);
    check(on_stop == 0 && g_chld_runs >= 1, "SA_NOCLDSTOP: no SIGCHLD for a stop",
          "%d on the stop, %d by the death", on_stop, (int)g_chld_runs);

    // exit(3) from a thread that is not the first ends the whole process
    // with that status.
    c = fork();
    if (c == 0) {
        pthread_t t;
        if (pthread_create(&t, NULL, thread_exits, NULL) != 0) _exit(98);
        for (;;) pause();
    }
    got = waitpid(c, &st, 0);
    check(got == c && WIFEXITED(st) && WEXITSTATUS(st) == 7, "exit() from a second thread",
          "status %#x (want an exit with 7)", st);

    // execve from a thread that is not the first: the new image runs in
    // that thread, which takes the process's pid.
    c = fork();
    if (c == 0) {
        pthread_t t;
        if (pthread_create(&t, NULL, thread_execs, NULL) != 0) _exit(98);
        for (;;) pause();
    }
    got = waitpid(c, &st, 0);
    check(got == c && WIFEXITED(st) && WEXITSTATUS(st) == 0, "execve() from a second thread",
          "%s (status %#x)",
          WIFEXITED(st) && WEXITSTATUS(st) == 1     ? "the new image's tid is not the pid"
          : WIFEXITED(st) && WEXITSTATUS(st) == 2   ? "the exec'ing thread's mask was lost"
          : WIFEXITED(st) && WEXITSTATUS(st) == 3   ? "tgkill(pid, pid) missed the new image"
          : WIFEXITED(st) && WEXITSTATUS(st) == 101 ? "the exec failed"
                                                    : "tid == pid, its own mask, tgkill reaches it",
          st);

    // A zombie wears its pid until it is reaped. Linux counts pids up to
    // pid_max before it comes round; a kernel that hands out freed pids
    // first gives the zombie's to a fork within as many forks as it has
    // pids free.
    c = child_that_exits(0, 0);
    memset(&si, 0, sizeof si);
    waitid(P_PID, (id_t)c, &si, WEXITED | WNOWAIT);
    int forks = 512, given = 0;
    for (int i = 0; i < forks && !given; i++) {
        pid_t k = child_that_exits(0, 0);
        if (k == c) given = i + 1;
        if (k > 0) waitpid(k, NULL, 0);
    }
    got = waitpid(c, &st, 0);
    if (given)
        check(0, "a zombie's pid is not handed out again", "fork %d of %d was handed it", given,
              forks);
    else
        check(got == c, "a zombie's pid is not handed out again", "none of %d forks; waitpid: %s",
              forks, got == c ? "the zombie" : "not the zombie");

    // An alarm dies with the process that set it. Once its pid is free, a
    // fork can be handed it: that one arms an alarm of its own the same way
    // and lives past the dead one's. Linux takes too long to come round to
    // the pid for this to be asked there.
    c = fork();
    if (c == 0) {
        alarm(1);
        _exit(0);
    }
    waitpid(c, &st, 0);
    int heir = 0, heir_st = 0;
    for (int i = 0; i < forks && !heir; i++) {
        pid_t k = fork();
        if (k == 0) {
            if (getpid() != c) _exit(0);
            alarm(5);
            sleep_ms(1500);
            alarm(0);
            _exit(42);
        }
        if (k == c) heir = i + 1;
        if (k > 0) waitpid(k, &heir_st, 0);
    }
    if (!heir)
        check(1, "a dead process's alarm dies with it", "no fork of %d was handed its pid", forks);
    else
        check(WIFEXITED(heir_st) && WEXITSTATUS(heir_st) == 42, "a dead process's alarm dies with it",
              "fork %d was handed its pid and %s", heir,
              WIFSIGNALED(heir_st) && WTERMSIG(heir_st) == SIGALRM ? "died of its alarm"
              : WIFEXITED(heir_st) && WEXITSTATUS(heir_st) == 42 ? "lived past it"
                                                                 : "ended some other way");
}

// ------------------------------------------------------------------- stop

// Stop `c` and see the stop, continue it and see the continue: 0 if both
// were seen. A kernel that does not stop a process blocked in a syscall
// hangs in the first waitpid, and the section's alarm says so.
static int stop_and_continue(pid_t c) {
    int st = 0;
    kill(c, SIGSTOP);
    if (waitpid(c, &st, WUNTRACED) != c || !WIFSTOPPED(st)) return -1;
    kill(c, SIGCONT);
    if (waitpid(c, &st, WCONTINUED) != c || !WIFCONTINUED(st)) return -1;
    return 0;
}

static void sec_stop(void) {
    int st = 0;

    // A child asleep in nanosleep stops (Ctrl-Z on `sleep 100`).
    pid_t c = fork();
    if (c == 0) {
        sleep(30);
        _exit(0);
    }
    sleep_ms(50);
    int seen = stop_and_continue(c);
    check(seen == 0, "a sleeping child stops", "%s",
          seen ? "the stop or the continue was not reported" : "stopped and continued");
    kill(c, SIGKILL);
    waitpid(c, &st, 0);

    // A read stopped and continued goes on reading: Linux makes it again
    // (-ERESTARTSYS, and no handler ran), so the program never sees EINTR.
    int p[2];
    if (pipe(p) != 0) {
        check(0, "pipe", "%s", strerror(errno));
        return;
    }
    c = fork();
    if (c == 0) {
        close(p[1]);
        char ch = 0;
        ssize_t n = read(p[0], &ch, 1);
        _exit(n == 1 && ch == 'x' ? 0 : n < 0 && errno == EINTR ? 2 : 3);
    }
    close(p[0]);
    sleep_ms(50);
    seen = stop_and_continue(c);
    if (write(p[1], "x", 1) != 1) check(0, "write to the pipe", "%s", strerror(errno));
    close(p[1]);
    waitpid(c, &st, 0);
    check(seen == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "a read goes on past a stop", "%s",
          seen                                      ? "the stop or the continue was not reported"
          : !WIFEXITED(st)                          ? "the child died"
          : WEXITSTATUS(st) == 2                    ? "the read came back EINTR"
          : WEXITSTATUS(st) == 3                    ? "the read came back with something else"
                                                    : "it read the byte");

    // A sigsuspend stopped and continued is still waiting, under its own
    // mask; when the signal it waits for comes, the handler runs and the
    // mask from before the call is back.
    install(SIGUSR1, usr1_counts, 0);
    c = fork();
    if (c == 0) {
        sigset_t block, during, after;
        sigemptyset(&block);
        sigaddset(&block, SIGUSR1);
        sigprocmask(SIG_BLOCK, &block, NULL);
        sigemptyset(&during);
        g_usr1_runs = 0;
        int r = sigsuspend(&during);
        int e = errno;
        sigprocmask(SIG_BLOCK, NULL, &after);
        if (g_usr1_runs != 1) _exit(5);
        if (!sigismember(&after, SIGUSR1)) _exit(4);
        _exit(r == -1 && e == EINTR ? 0 : 6);
    }
    sleep_ms(50);
    seen = stop_and_continue(c);
    sleep_ms(50);
    kill(c, SIGUSR1);
    waitpid(c, &st, 0);
    check(seen == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0, "a sigsuspend outlasts a stop", "%s",
          seen                   ? "the stop or the continue was not reported"
          : !WIFEXITED(st)       ? "the child died"
          : WEXITSTATUS(st) == 5 ? "it returned without the handler running"
          : WEXITSTATUS(st) == 4 ? "the mask from before the call was not put back"
          : WEXITSTATUS(st) == 6 ? "it did not return EINTR"
                                 : "handler ran, mask back");
}

// --------------------------------------------------------------- teardown

// Whether a thread of a process that has exited is really gone is something
// no syscall answers, and /proc/meminfo is no use on Linux itself: a freed
// 512 MiB shows up there late, now and then by a few hundred MiB. A robust
// mutex is: the kernel hands it on as EOWNERDEAD when the thread that holds
// it dies, and not before. The thread takes it in memory it shares with the
// section, then goes to sleep. One mutex a check: a thread that outlives its
// process holds its mutex for good.
enum wait_kind { IN_PAUSE, IN_READ, IN_SLEEP, IN_WAITPID, IN_VFORK, WAIT_KINDS };
static struct {
    pthread_mutex_t m[WAIT_KINDS];
    volatile int held;
    volatile pid_t vforked;  // the child the vfork wait is for, to clean up
} *g_teardown;

static const char *const WAIT_KIND[] = {"pause", "read", "nanosleep", "waitpid", "vfork"};
static int g_teardown_pipe[2];

static void *waits_in(void *arg) {
    char b;
    int st;
    enum wait_kind kind = (enum wait_kind)(long)arg;
    pthread_mutex_lock(&g_teardown->m[kind]);
    g_teardown->held = 1;
    switch (kind) {
    case IN_PAUSE:
        for (;;) pause();
    case IN_READ:
        (void)!read(g_teardown_pipe[0], &b, 1);
        break;
    case IN_SLEEP:
        sleep(60);
        break;
    case IN_WAITPID:
        waitpid(-1, &st, 0);
        break;
    case IN_VFORK:
        // A child that never gets as far as exec: its parent waits in vfork
        // for as long as it lives, killably.
        if (vfork() == 0) {
            g_teardown->vforked = getpid();
            for (;;) pause();
        }
        break;
    case WAIT_KINDS:
        break;
    }
    return NULL;
}

// A child whose second thread takes the mutex and sleeps in `kind`, and which
// then ends with exit_group. Once it is reaped, what taking the mutex says,
// waiting up to two seconds: EOWNERDEAD if that thread died, ETIMEDOUT if it
// is still there; -1 if the child itself failed.
static int what_the_mutex_says_after(enum wait_kind kind) {
    g_teardown->held = 0;
    g_teardown->vforked = 0;
    pid_t c = fork();
    if (c == 0) {
        // Something to wait for that outlives this process.
        if (kind == IN_WAITPID && fork() == 0) {
            sleep(3);
            _exit(0);
        }
        pthread_t t;
        if (pthread_create(&t, NULL, waits_in, (void *)(long)kind) != 0) _exit(98);
        while (!g_teardown->held) sleep_ms(1);
        sleep_ms(50);
        _exit(0);  // exit_group: every thread goes with it
    }
    int st = 0;
    if (waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) != 0) return -1;
    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_sec += 2;
    int r = pthread_mutex_timedlock(&g_teardown->m[kind], &until);
    if (g_teardown->vforked > 0) kill(g_teardown->vforked, SIGKILL);
    return r;
}

static void sec_teardown(void) {
    g_teardown = mmap(NULL, sizeof *g_teardown, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS,
                      -1, 0);
    // Made here, so that the read end the child's thread waits on has a write
    // end that outlives the child: the child's own would close with its
    // files, and that ends the read by itself.
    if (g_teardown == MAP_FAILED || pipe(g_teardown_pipe) != 0) {
        check(0, "teardown", "setting up: %s", strerror(errno));
        return;
    }
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    for (int k = 0; k < WAIT_KINDS; k++) pthread_mutex_init(&g_teardown->m[k], &a);
    for (int k = 0; k < WAIT_KINDS; k++) {
        int r = what_the_mutex_says_after((enum wait_kind)k);
        char what[64];
        snprintf(what, sizeof what, "exit with a thread in %s", WAIT_KIND[k]);
        check(r == EOWNERDEAD, what, "%s",
              r == EOWNERDEAD  ? "the thread died with the process"
              : r == ETIMEDOUT ? "the thread outlived its process's reap"
              : r == -1        ? "the child failed"
                               : strerror(r));
    }
}

// ------------------------------------------------------------------ spawn

// The wait status of child `pid`, or -1.
static int status_of(pid_t pid) {
    int status;
    return waitpid(pid, &status, 0) == pid ? status : -1;
}

static void sec_spawn(void) {
    // vfork(2): the child runs in the parent's memory until it execs or
    // exits, so what it writes there is what the parent reads.
    volatile int word = 1;
    pid_t pid = vfork();
    if (pid == 0) {
        word = 2;
        _exit(0);
    }
    int status = pid > 0 ? status_of(pid) : -1;
    check(pid > 0 && word == 2 && status == 0, "vfork: the child writes the parent's memory",
          "the parent reads %d, child status %#x", (int)word, status);

    // glibc's posix_spawn hears about a failed exec through a word its
    // child writes in that memory; musl's, through a pipe.
    pid_t child;
    char *nothing[] = {"no-such-program", NULL};
    int r = posix_spawn(&child, "/no/such/program", NULL, NULL, nothing, environ);
    if (r == 0) status_of(child);
    check(r == ENOENT, "posix_spawn: a missing program is ENOENT", "returned %d (%s)", r,
          r ? strerror(r) : "a child");

    char *sh[] = {"sh", "-c", "exit 3", NULL};
    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    posix_spawn_file_actions_addopen(&fa, 3, "/no/such/file", O_RDONLY, 0);
    r = posix_spawn(&child, "/bin/sh", &fa, NULL, sh, environ);
    posix_spawn_file_actions_destroy(&fa);
    if (r == 0) status_of(child);
    check(r == ENOENT, "posix_spawn: a failed file action is ENOENT", "returned %d (%s)", r,
          r ? strerror(r) : "a child");

    // An exec in the child gives it memory of its own, and the parent comes
    // back to its program every time.
    enum { SPAWNS = 20 };
    int good = 0, last = 0;
    for (int i = 0; i < SPAWNS; i++) {
        r = posix_spawn(&child, "/bin/sh", NULL, NULL, sh, environ);
        status = r == 0 ? status_of(child) : -1;
        if (!(WIFEXITED(status) && WEXITSTATUS(status) == 3)) {
            last = r ? r : status;
            break;
        }
        good++;
    }
    if (good == SPAWNS)
        check(1, "posix_spawn: sh -c 'exit 3', 20 times", "all %d exited 3", SPAWNS);
    else
        check(0, "posix_spawn: sh -c 'exit 3', 20 times", "%d exited 3, then %#x", good, last);
}

// ---------------------------------------------------------------- badexec

static void write_file(const char *path, const void *bytes, size_t len, mode_t mode) {
    unlink(path);
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, mode);
    if (fd < 0 || write(fd, bytes, len) != (ssize_t)len) printf("  (could not write %s)\n", path);
    close(fd);
}

// A static ELF64 for `machine` with a PT_INTERP naming `interp`.
static void write_elf_with_interp(const char *path, const char *interp, int machine) {
    unsigned char buf[512];
    memset(buf, 0, sizeof buf);
    Elf64_Ehdr *eh = (Elf64_Ehdr *)buf;
    memcpy(eh->e_ident, ELFMAG, SELFMAG);
    eh->e_ident[EI_CLASS] = ELFCLASS64;
    eh->e_ident[EI_DATA] = ELFDATA2LSB;
    eh->e_ident[EI_VERSION] = EV_CURRENT;
    eh->e_type = ET_EXEC;
    eh->e_machine = machine;
    eh->e_version = EV_CURRENT;
    eh->e_entry = 0x400078;
    eh->e_phoff = sizeof(Elf64_Ehdr);
    eh->e_ehsize = sizeof(Elf64_Ehdr);
    eh->e_phentsize = sizeof(Elf64_Phdr);
    eh->e_phnum = 2;
    Elf64_Phdr *ph = (Elf64_Phdr *)(buf + sizeof(Elf64_Ehdr));
    size_t at = sizeof(Elf64_Ehdr) + 2 * sizeof(Elf64_Phdr);
    size_t len = strlen(interp) + 1;
    ph[0].p_type = PT_INTERP;
    ph[0].p_offset = at;
    ph[0].p_vaddr = 0x400000 + at;
    ph[0].p_filesz = len;
    ph[0].p_memsz = len;
    ph[0].p_flags = PF_R;
    ph[1].p_type = PT_LOAD;
    ph[1].p_vaddr = 0x400000;
    ph[1].p_filesz = at + len;
    ph[1].p_memsz = at + len;
    ph[1].p_flags = PF_R | PF_X;
    ph[1].p_align = 0x1000;
    memcpy(buf + at, interp, len);
    write_file(path, buf, at + len, 0755);
}

// Exec `path` in a child; its exit status is 100 + the errno, or what the
// program exits with if the exec worked.
static int exec_status(const char *path) {
    pid_t c = fork();
    if (c == 0) {
        char *av[] = {"x", NULL};
        syscall(SYS_execve, path, av, environ);
        _exit(100 + errno);
    }
    return status_of(c);
}

static void check_exec(const char *path, int want, const char *what) {
    int st = exec_status(path);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 100 + want, what, "st=%#x (exit %d, signal %d)", st,
          WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
}

static void sec_badexec(void) {
#if defined(__x86_64__)
    int em = EM_X86_64, other = EM_AARCH64;
#elif defined(__aarch64__)
    int em = EM_AARCH64, other = EM_X86_64;
#else
    int em = EM_RISCV, other = EM_X86_64;
#endif
    char junk[128];
    memset(junk, 'z', sizeof junk);
    write_file("/tmp/bx-junk10", "\x01\x02garbage\n", 10, 0644);
    check_exec("/tmp/bx-junk10", EACCES, "no execute bit: EACCES");
    chmod("/tmp/bx-junk10", 0755);
    check_exec("/tmp/bx-junk10", ENOEXEC, "garbage: ENOEXEC");
    check_exec("/tmp", EACCES, "a directory: EACCES");
    write_file("/tmp/bx-noshebang", "exit 3\n", 7, 0755);
    check_exec("/tmp/bx-noshebang", ENOEXEC, "a script without #!: ENOEXEC");
    write_file("/tmp/bx-emptyshebang", "#!\n", 3, 0755);
    check_exec("/tmp/bx-emptyshebang", ENOEXEC, "an empty #! line: ENOEXEC");
    write_file("/tmp/bx-badinterp", "#!/tmp/bx-junk10\n", 17, 0755);
    check_exec("/tmp/bx-badinterp", ENOEXEC, "#! naming garbage: ENOEXEC");
    write_file("/tmp/bx-nointerp", "#!/no/such/sh\n", 14, 0755);
    check_exec("/tmp/bx-nointerp", ENOENT, "#! naming nothing: ENOENT");
    write_file("/tmp/bx-junk128", junk, sizeof junk, 0755);
    write_elf_with_interp("/tmp/bx-elf-nointerp", "/no/such/ld.so", em);
    check_exec("/tmp/bx-elf-nointerp", ENOENT, "PT_INTERP naming nothing: ENOENT");
    write_elf_with_interp("/tmp/bx-elf-junk", "/tmp/bx-junk128", em);
    check_exec("/tmp/bx-elf-junk", ELIBBAD, "PT_INTERP of garbage: ELIBBAD");
    write_elf_with_interp("/tmp/bx-elf-short", "/tmp/bx-junk10", em);
    check_exec("/tmp/bx-elf-short", EIO, "PT_INTERP of 10 bytes: EIO");
    write_elf_with_interp("/tmp/bx-elf-dir", "/tmp", em);
    check_exec("/tmp/bx-elf-dir", EACCES, "PT_INTERP a directory: EACCES");
    write_elf_with_interp("/tmp/bx-elf-other", "/no/such/ld.so", other);
    check_exec("/tmp/bx-elf-other", ENOEXEC, "an ELF for another machine: ENOEXEC");

    // An interpreter is opened for execution like the program: one nobody
    // may execute is EACCES before anything is read from it.
    write_file("/tmp/bx-junk-rw", junk, sizeof junk, 0644);
    write_file("/tmp/bx-rwinterp", "#!/tmp/bx-junk-rw\n", 18, 0755);
    check_exec("/tmp/bx-rwinterp", EACCES, "#! naming no executable: EACCES");
    write_elf_with_interp("/tmp/bx-elf-rw", "/tmp/bx-junk-rw", em);
    check_exec("/tmp/bx-elf-rw", EACCES, "PT_INTERP of no executable: EACCES");

    // And its path goes through links like any other path. The last one is
    // called `sh`, because busybox picks what to be by the name it was run
    // under, and an interpreter is run under the name in the `#!` line.
    mkdir("/tmp/bx-links", 0755);
    const char *links[][2] = {{"/bin/sh", "/tmp/bx-l1"}, {"/tmp/bx-l1", "/tmp/bx-l2"}, {"/tmp/bx-l2", "/tmp/bx-links/sh"}};
    int linked_up = 1;
    for (size_t i = 0; i < sizeof links / sizeof links[0] && linked_up; i++) {
        unlink(links[i][1]);
        linked_up = symlink(links[i][0], links[i][1]) == 0;
    }
    check(linked_up, "three links to /bin/sh", "%s", linked_up ? "" : strerror(errno));
    write_file("/tmp/bx-linked", "#!/tmp/bx-links/sh\nexit 9\n", 26, 0755);
    int linked = exec_status("/tmp/bx-linked");
    check(WIFEXITED(linked) && WEXITSTATUS(linked) == 9, "#! through three links runs",
          "st=%#x (exit %d, signal %d)", linked, WIFEXITED(linked) ? WEXITSTATUS(linked) : -1,
          WIFSIGNALED(linked) ? WTERMSIG(linked) : 0);

    // Five scripts in front of /bin/sh run; six are ELOOP.
    for (int len = 5; len <= 6; len++) {
        char name[64], line[64];
        for (int i = 0; i < len; i++) {
            snprintf(name, sizeof name, "/tmp/bx-chain%d", i);
            if (i + 1 == len)
                snprintf(line, sizeof line, "#!/bin/sh\nexit 7\n");
            else
                snprintf(line, sizeof line, "#!/tmp/bx-chain%d\n", i + 1);
            write_file(name, line, strlen(line), 0755);
        }
        int st = exec_status("/tmp/bx-chain0");
        int want = len == 5 ? 7 : 100 + ELOOP;
        check(WIFEXITED(st) && WEXITSTATUS(st) == want,
              len == 5 ? "five chained scripts run" : "six chained scripts: ELOOP", "st=%#x (exit %d, signal %d)",
              st, WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
        for (int i = 0; i < len; i++) {
            snprintf(name, sizeof name, "/tmp/bx-chain%d", i);
            unlink(name);
        }
    }

    // What the errno is for: a shell runs a file the kernel answers with
    // ENOEXEC as a script of its own.
    pid_t c = fork();
    if (c == 0) {
        execl("/bin/sh", "sh", "-c", "/tmp/bx-noshebang", (char *)NULL);
        _exit(127);
    }
    int st = status_of(c);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 3, "sh runs a script without #!", "st=%#x", st);

    const char *made[] = {
        "/tmp/bx-junk10",       "/tmp/bx-noshebang", "/tmp/bx-emptyshebang", "/tmp/bx-badinterp",
        "/tmp/bx-nointerp",     "/tmp/bx-junk128",   "/tmp/bx-elf-nointerp", "/tmp/bx-elf-junk",
        "/tmp/bx-elf-short",    "/tmp/bx-elf-dir",   "/tmp/bx-elf-other",    "/tmp/bx-junk-rw",
        "/tmp/bx-rwinterp",     "/tmp/bx-elf-rw",    "/tmp/bx-l1",           "/tmp/bx-l2",
        "/tmp/bx-links/sh",     "/tmp/bx-linked",
    };
    for (size_t i = 0; i < sizeof made / sizeof made[0]; i++) unlink(made[i]);
    rmdir("/tmp/bx-links");
}

// ------------------------------------------------------------------- sched

#ifndef SCHED_FLAG_RESET_ON_FORK
#define SCHED_FLAG_RESET_ON_FORK 0x01
#endif

// What a task reads back about its own scheduling.
struct sched_seen {
    int nice;
    int policy;  // sched_getscheduler(): the policy, SCHED_RESET_ON_FORK included
    int prio;    // sched_getparam()
    unsigned long mask;  // the first word of sched_getaffinity()
    long attr_flags;     // sched_getattr()'s sched_flags, -1 if it failed
};

// `struct sched_attr`, the 48-byte first version.
struct probe_sched_attr {
    uint32_t size, policy;
    uint64_t flags;
    int32_t nice;
    uint32_t priority;
    uint64_t runtime, deadline, period;
};

// musl's sched_setscheduler() and friends are ENOSYS on purpose (they are
// per thread, POSIX says per process), so the section makes the syscalls.
static int get_policy(void) {
    return (int)syscall(SYS_sched_getscheduler, 0);
}

static void sched_seen_now(struct sched_seen *s) {
    s->nice = getpriority(PRIO_PROCESS, 0);
    s->policy = get_policy();
    struct sched_param sp = {0};
    syscall(SYS_sched_getparam, 0, &sp);
    s->prio = sp.sched_priority;
    s->mask = 0;
    syscall(SYS_sched_getaffinity, 0, sizeof s->mask, &s->mask);
    struct probe_sched_attr a = {0};
    s->attr_flags =
        syscall(SYS_sched_getattr, 0, &a, sizeof a, 0) == 0 ? (long)a.flags : -1;
}

// What a child forked now sees of itself.
static struct sched_seen forked_sees(void) {
    struct sched_seen s;
    memset(&s, 0xff, sizeof s);
    int fd[2];
    if (pipe(fd) != 0) return s;
    pid_t c = fork();
    if (c == 0) {
        struct sched_seen mine;
        sched_seen_now(&mine);
        _exit(write(fd[1], &mine, sizeof mine) == sizeof mine ? 0 : 1);
    }
    close(fd[1]);
    if (read(fd[0], &s, sizeof s) != sizeof s) memset(&s, 0xff, sizeof s);
    close(fd[0]);
    while (waitpid(c, NULL, 0) == -1 && errno == EINTR) {
    }
    return s;
}

static void *sched_seen_thread(void *out) {
    sched_seen_now(out);
    return NULL;
}

// What a thread created now with the default attributes sees of itself.
static struct sched_seen thread_sees(void) {
    struct sched_seen s;
    memset(&s, 0xff, sizeof s);
    pthread_t t;
    if (pthread_create(&t, NULL, sched_seen_thread, &s) == 0) pthread_join(t, NULL);
    return s;
}

static int set_policy(int policy, int priority) {
    struct sched_param sp = {.sched_priority = priority};
    return (int)syscall(SYS_sched_setscheduler, 0, policy, &sp);
}

static void sec_sched(void) {
    struct sched_seen s;

    setpriority(PRIO_PROCESS, 0, 5);
    s = forked_sees();
    check(s.nice == 5, "fork: the nice value is kept", "child nice %d", s.nice);
    s = thread_sees();
    check(s.nice == 5, "a new thread keeps the nice value", "thread nice %d", s.nice);
    setpriority(PRIO_PROCESS, 0, 0);

    unsigned long all = 0;
    syscall(SYS_sched_getaffinity, 0, sizeof all, &all);
    unsigned long one = all & -all;  // the lowest CPU this task may use
    if (syscall(SYS_sched_setaffinity, 0, sizeof one, &one) == 0) {
        s = forked_sees();
        check(s.mask == one, "fork: the CPU mask is kept", "child %#lx, parent %#lx", s.mask,
              one);
        s = thread_sees();
        check(s.mask == one, "a new thread keeps the CPU mask", "thread %#lx, parent %#lx",
              s.mask, one);
        syscall(SYS_sched_setaffinity, 0, sizeof all, &all);
    } else {
        check(0, "sched_setaffinity to one CPU", "%s", strerror(errno));
    }

    set_policy(SCHED_BATCH, 0);
    s = forked_sees();
    check(s.policy == SCHED_BATCH, "fork: the policy is kept", "child policy %#x", s.policy);
    s = thread_sees();
    check(s.policy == SCHED_BATCH, "a new thread keeps the policy", "thread policy %#x",
          s.policy);
    set_policy(SCHED_OTHER, 0);

    // SCHED_RESET_ON_FORK: kept, reported, and undone in every child.
    if (set_policy(SCHED_FIFO | SCHED_RESET_ON_FORK, 10) != 0) {
        check(0, "SCHED_FIFO|SCHED_RESET_ON_FORK", "%s (this section runs as root)",
              strerror(errno));
        return;
    }
    struct sched_seen me;
    sched_seen_now(&me);
    check(me.policy == (SCHED_FIFO | SCHED_RESET_ON_FORK),
          "getscheduler reports RESET_ON_FORK", "policy %#x", me.policy);
    check(me.attr_flags == SCHED_FLAG_RESET_ON_FORK, "getattr reports RESET_ON_FORK",
          "sched_flags %#lx", me.attr_flags);
    s = forked_sees();
    check(s.policy == SCHED_OTHER && s.prio == 0 && s.attr_flags == 0,
          "reset on fork: the child is SCHED_OTHER", "policy %#x prio %d flags %#lx", s.policy,
          s.prio, s.attr_flags);
    s = thread_sees();
    check(s.policy == SCHED_OTHER && s.prio == 0, "reset on fork: and so is a new thread",
          "policy %#x prio %d", s.policy, s.prio);
    struct sched_param sp = {.sched_priority = 20};
    syscall(SYS_sched_setparam, 0, &sp);
    int pol = get_policy();
    check(pol == (SCHED_FIFO | SCHED_RESET_ON_FORK), "sched_setparam keeps RESET_ON_FORK",
          "policy %#x", pol);

    // A fair policy keeps its class and a positive nice; a negative one is
    // put back to 0.
    set_policy(SCHED_OTHER, 0);
    setpriority(PRIO_PROCESS, 0, -5);
    set_policy(SCHED_BATCH | SCHED_RESET_ON_FORK, 0);
    s = forked_sees();
    check(s.policy == SCHED_BATCH && s.nice == 0, "reset on fork: a negative nice is 0",
          "child policy %#x nice %d", s.policy, s.nice);
    setpriority(PRIO_PROCESS, 0, 7);
    s = forked_sees();
    check(s.nice == 7, "reset on fork: a positive nice is kept", "child nice %d", s.nice);

    // Only privilege takes the flag off; anybody may put it on. The child
    // sets it itself: the one its parent had is the one a fork takes away.
    set_policy(SCHED_OTHER, 0);
    setpriority(PRIO_PROCESS, 0, 0);
    pid_t c = fork();
    if (c == 0) {
        if (set_policy(SCHED_OTHER | SCHED_RESET_ON_FORK, 0) != 0 || setuid(65534) != 0)
            _exit(10);
        int off = set_policy(SCHED_OTHER, 0) == -1 && errno == EPERM;
        int kept = set_policy(SCHED_OTHER | SCHED_RESET_ON_FORK, 0) == 0;
        _exit(off && kept ? 0 : 1 + off + 2 * kept);
    }
    int st = 0;
    while (waitpid(c, &st, 0) == -1 && errno == EINTR) {
    }
    check(WIFEXITED(st) && WEXITSTATUS(st) == 0, "unprivileged: the flag cannot come off",
          "status %#x", st);

    // The legacy bit belongs to sched_setscheduler; sched_setattr takes the
    // flag in sched_flags and refuses it in the policy word.
    struct probe_sched_attr a = {.size = sizeof a, .policy = SCHED_OTHER | SCHED_RESET_ON_FORK};
    errno = 0;
    long r = syscall(SYS_sched_setattr, 0, &a, 0);
    check(r == -1 && errno == EINVAL, "setattr: the bit in the policy word", "ret %ld errno %d",
          r, errno);

    // sched_getaffinity needs a whole number of longs, with a bit per CPU.
    unsigned long buf[2];
    errno = 0;
    r = syscall(SYS_sched_getaffinity, 0, 1, buf);
    check(r == -1 && errno == EINVAL, "getaffinity: a 1-byte buffer", "ret %ld errno %d", r,
          errno);
    errno = 0;
    r = syscall(SYS_sched_getaffinity, 0, 12, buf);
    check(r == -1 && errno == EINVAL, "getaffinity: a 12-byte buffer", "ret %ld errno %d", r,
          errno);
    r = syscall(SYS_sched_getaffinity, 0, 16, buf);
    check(r > 0 && r <= 16 && r % 8 == 0, "getaffinity: a 16-byte buffer", "ret %ld", r);
}

// ------------------------------------------------------------------- image

static void sec_image(void) {
    // Linux keeps the low 64 KiB unmappable (`vm.mmap_min_addr`); an image
    // loaded at 0 has its first function in the first few pages.
    uintptr_t code = (uintptr_t)&sec_image;
    check(code >= 0x10000, "the program is above the null page", "sec_image at %#lx",
          (unsigned long)code);
    pid_t c = fork();
    if (c == 0) {
        signal(SIGSEGV, SIG_DFL);
        volatile int word = *g_unmapped;
        (void)word;
        _exit(3);  // the read came back
    }
    int st = 0;
    while (waitpid(c, &st, 0) == -1 && errno == EINTR) {
    }
    check(WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV, "a null read is a SIGSEGV",
          "status %#x%s", st,
          WIFEXITED(st) && WEXITSTATUS(st) == 3 ? " (the read came back)" : "");
}

// ----------------------------------------------------------------- cputime

static long ms_of(const struct timeval *a, const struct timeval *b) {
    return (a->tv_sec + b->tv_sec) * 1000L + (a->tv_usec + b->tv_usec) / 1000;
}

static long ru_ms(int who) {
    struct rusage ru;
    getrusage(who, &ru);
    return ms_of(&ru.ru_utime, &ru.ru_stime);
}

static long clock_ms(clockid_t clock) {
    struct timespec t;
    clock_gettime(clock, &t);
    return t.tv_sec * 1000L + t.tv_nsec / 1000000;
}

// `ms` of wall time spent in a loop of syscalls, so half of it or more is
// the kernel's: what a program that works through the kernel looks like.
static void busy_ms(long ms) {
    long end = clock_ms(CLOCK_MONOTONIC) + ms;
    while (clock_ms(CLOCK_MONOTONIC) < end) syscall(SYS_getppid);
}

static void *busy_forever(void *arg) {
    (void)arg;
    for (;;) busy_ms(1000);
    return NULL;
}

static void sec_cputime(void) {
    // Asleep is not on the CPU: 32 sleeps of 10 ms and a wait for a child
    // that sleeps 400 ms cost microseconds.
    pid_t c = fork();
    if (c == 0) {
        sleep_ms(400);
        _exit(0);
    }
    long before = ru_ms(RUSAGE_SELF);
    for (int i = 0; i < 32; i++) sleep_ms(10);
    status_of(c);
    long slept = ru_ms(RUSAGE_SELF) - before;
    check(slept < 50, "sleeping and waiting take no CPU", "%ld ms of CPU over ~400 ms asleep", slept);

    // A child that works for 300 ms has used about 300 ms, which `wait4`
    // reports and the reaper's RUSAGE_CHILDREN and times() add up.
    long children = ru_ms(RUSAGE_CHILDREN);
    struct tms t0, t1;
    times(&t0);
    c = fork();
    if (c == 0) {
        busy_ms(300);
        _exit(0);
    }
    int st;
    struct rusage ru;
    pid_t w;
    while ((w = wait4(c, &st, 0, &ru)) == -1 && errno == EINTR) {
    }
    long used = ms_of(&ru.ru_utime, &ru.ru_stime);
    check(w == c && used >= 150 && used <= 450, "wait4 reports a child's 300 ms",
          "%ld ms (user %ld.%03ld s)", used, (long)ru.ru_utime.tv_sec, (long)ru.ru_utime.tv_usec / 1000);
    children = ru_ms(RUSAGE_CHILDREN) - children;
    check(children >= 150 && children <= 450, "RUSAGE_CHILDREN adds it", "%ld ms", children);
    times(&t1);
    long hz = sysconf(_SC_CLK_TCK);
    long ticks = (long)(t1.tms_cutime + t1.tms_cstime - t0.tms_cutime - t0.tms_cstime);
    check(ticks >= hz * 15 / 100, "times() adds it", "%ld ticks at %ld Hz", ticks, hz);

    // What a child reaped of its own is the child's too: wait4 hands out
    // its children's time with its own, and the reaper adds both up. This
    // is the 300 ms `time sh -c make` has to show, made by a grandchild.
    children = ru_ms(RUSAGE_CHILDREN);
    c = fork();
    if (c == 0) {
        pid_t g = fork();
        if (g == 0) {
            busy_ms(300);
            _exit(0);
        }
        status_of(g);
        _exit(0);
    }
    while ((w = wait4(c, &st, 0, &ru)) == -1 && errno == EINTR) {
    }
    used = ms_of(&ru.ru_utime, &ru.ru_stime);
    children = ru_ms(RUSAGE_CHILDREN) - children;
    check(w == c && used >= 150 && used <= 450 && children >= 150 && children <= 450,
          "a grandchild's 300 ms: in wait4 and in RUSAGE_CHILDREN",
          "wait4 %ld ms, RUSAGE_CHILDREN %ld ms", used, children);

    // A thread still working when the process calls exit_group is counted:
    // the main thread sleeps, the other one works.
    c = fork();
    if (c == 0) {
        pthread_t th;
        if (pthread_create(&th, NULL, busy_forever, NULL)) _exit(2);
        sleep_ms(300);
        _exit(0);
    }
    while ((w = wait4(c, &st, 0, &ru)) == -1 && errno == EINTR) {
    }
    used = ms_of(&ru.ru_utime, &ru.ru_stime);
    check(w == c && WIFEXITED(st) && WEXITSTATUS(st) == 0 && used >= 150 && used <= 450,
          "a thread busy at exit_group counts", "%ld ms, status %#x", used, st);

    // And the two CPU clocks count the kernel's part of a loop of syscalls.
    long a = clock_ms(CLOCK_THREAD_CPUTIME_ID);
    busy_ms(100);
    long d = clock_ms(CLOCK_THREAD_CPUTIME_ID) - a;
    check(d >= 60 && d <= 150, "CLOCK_THREAD_CPUTIME_ID: 100 ms", "%ld ms", d);
    a = clock_ms(CLOCK_PROCESS_CPUTIME_ID);
    busy_ms(100);
    d = clock_ms(CLOCK_PROCESS_CPUTIME_ID) - a;
    check(d >= 60 && d <= 150, "CLOCK_PROCESS_CPUTIME_ID: 100 ms", "%ld ms", d);
}

// ------------------------------------------------------------------- fsize

static volatile sig_atomic_t g_xfsz;

static void on_xfsz(int sig) {
    (void)sig;
    g_xfsz++;
}

static void fsize_checks(void) {
    // Linux holds every write to a regular file to RLIMIT_FSIZE
    // (`generic_write_check_limits`): a write across the limit stops at it,
    // and one that starts at or past it is EFBIG and a SIGXFSZ. A call that
    // sets a length (`inode_newsize_ok`) is held to it only when it grows the
    // file.
    static char buf[8192];
    memset(buf, 'x', sizeof buf);
    const char *path = "/tmp/fz-file", *big = "/tmp/fz-big";
    write_file(big, buf, sizeof buf, 0644);  // made before the limit
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_xfsz;
    sigaction(SIGXFSZ, &sa, NULL);
    struct rlimit rl = {4096, 4096};
    int r = setrlimit(RLIMIT_FSIZE, &rl);
    check(fd >= 0 && r == 0, "a file, and RLIMIT_FSIZE 4096", "fd %d, %s", fd,
          r ? strerror(errno) : "");

    g_xfsz = 0;
    ssize_t n = write(fd, buf, sizeof buf);
    check(n == 4096 && g_xfsz == 0, "write across the limit stops at it",
          "ret %zd, %d SIGXFSZ", n, (int)g_xfsz);
    errno = 0;
    n = write(fd, buf, 1);
    int e = errno;
    check(n == -1 && e == EFBIG && g_xfsz == 1, "write at the limit: EFBIG and SIGXFSZ",
          "ret %zd errno %d, %d SIGXFSZ", n, e, (int)g_xfsz);
    g_xfsz = 0;
    n = write(fd, buf, 0);
    check(n == 0 && g_xfsz == 0, "an empty write at the limit is 0", "ret %zd, %d SIGXFSZ", n,
          (int)g_xfsz);
    n = pwrite(fd, buf, 200, 4000);
    check(n == 96 && g_xfsz == 0, "pwrite across the limit stops at it", "ret %zd, %d SIGXFSZ",
          n, (int)g_xfsz);
    errno = 0;
    n = pwrite(fd, buf, 10, 5000);
    e = errno;
    check(n == -1 && e == EFBIG && g_xfsz == 1, "pwrite past the limit: EFBIG and SIGXFSZ",
          "ret %zd errno %d, %d SIGXFSZ", n, e, (int)g_xfsz);
    g_xfsz = 0;
    struct iovec iov[2] = {{buf, 3000}, {buf, 3000}};
    lseek(fd, 0, SEEK_SET);
    n = writev(fd, iov, 2);
    check(n == 4096 && g_xfsz == 0, "writev across the limit stops at it", "ret %zd, %d SIGXFSZ",
          n, (int)g_xfsz);
    int fb = open(big, O_RDONLY);
    loff_t in = 0, out = 0;
    n = syscall(SYS_copy_file_range, fb, &in, fd, &out, sizeof buf, 0);
    check(n == 4096 && g_xfsz == 0, "copy_file_range across the limit stops at it",
          "ret %zd errno %d, %d SIGXFSZ", n, n < 0 ? errno : 0, (int)g_xfsz);
    close(fb);

    // A new length: growing past the limit is refused, cutting down is not,
    // even to a length still past it.
    errno = 0;
    r = ftruncate(fd, 8192);
    e = errno;
    check(r == -1 && e == EFBIG && g_xfsz == 1, "ftruncate past the limit: EFBIG and SIGXFSZ",
          "ret %d errno %d, %d SIGXFSZ", r, e, (int)g_xfsz);
    g_xfsz = 0;
    fb = open(big, O_RDWR);
    r = ftruncate(fb, 6000);
    check(r == 0 && g_xfsz == 0, "a file past the limit can be cut down",
          "ret %d errno %d, %d SIGXFSZ", r, r ? errno : 0, (int)g_xfsz);
    errno = 0;
    r = ftruncate(fb, 7000);
    e = errno;
    check(r == -1 && e == EFBIG && g_xfsz == 1, "but not grown", "ret %d errno %d, %d SIGXFSZ",
          r, e, (int)g_xfsz);
    close(fb);
    g_xfsz = 0;
    errno = 0;
    r = fallocate(fd, 0, 0, 8192);
    e = errno;
    check(r == -1 && e == EFBIG && g_xfsz == 1, "fallocate past the limit: EFBIG and SIGXFSZ",
          "ret %d errno %d, %d SIGXFSZ", r, e, (int)g_xfsz);
    g_xfsz = 0;
    r = fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 8192);
    struct stat st;
    fstat(fd, &st);
    check(r == 0 && g_xfsz == 0 && st.st_size == 4096,
          "fallocate past it with FALLOC_FL_KEEP_SIZE is not growing",
          "ret %d errno %d, %d SIGXFSZ, size %lld", r, r ? errno : 0, (int)g_xfsz,
          (long long)st.st_size);

    // A pipe has no size to limit.
    int p[2];
    r = pipe(p);
    n = r ? -1 : write(p[1], buf, sizeof buf);
    check(n == (ssize_t)sizeof buf && g_xfsz == 0, "a pipe is not held to it",
          "ret %zd, %d SIGXFSZ", n, (int)g_xfsz);
    if (!r) {
        close(p[0]);
        close(p[1]);
    }
    close(fd);

    // And SIGXFSZ kills a process that has not asked to be told: an
    // O_APPEND write to the file now at the limit.
    pid_t c = fork();
    if (c == 0) {
        struct rlimit no_core = {0, 0};
        setrlimit(RLIMIT_CORE, &no_core);
        signal(SIGXFSZ, SIG_DFL);
        int a = open(path, O_WRONLY | O_APPEND);
        ssize_t w = write(a, buf, 1);
        _exit(w == -1 && errno == EFBIG ? 3 : 4);  // came back
    }
    int status = status_of(c);
    check(WIFSIGNALED(status) && WTERMSIG(status) == SIGXFSZ, "unhandled, SIGXFSZ kills",
          "status %#x", status);
    unlink(path);
    unlink(big);
}

static void sec_fsize(void) {
    // The probe's own output may be a file already past the limit, which
    // would answer the section's lines with EFBIG and SIGXFSZ: the checks
    // run in a child that prints through a pipe, which has no limit.
    int out[2];
    if (pipe(out)) {
        check(0, "a pipe for the output", "%s", strerror(errno));
        return;
    }
    pid_t c = fork();
    if (c == 0) {
        close(out[0]);
        dup2(out[1], STDOUT_FILENO);
        close(out[1]);
        g_fail = 0;
        fsize_checks();
        fflush(stdout);
        _exit(g_fail > 100 ? 100 : g_fail);
    }
    close(out[1]);
    char chunk[512];
    ssize_t n;
    while ((n = read(out[0], chunk, sizeof chunk)) != 0) {
        if (n > 0) {
            fwrite(chunk, 1, (size_t)n, stdout);
        } else if (errno != EINTR) {
            break;
        }
    }
    close(out[0]);
    fflush(stdout);
    int st = status_of(c);
    if (WIFEXITED(st)) {
        g_fail += WEXITSTATUS(st);
    } else {
        check(0, "the checks' process", "died of signal %d",
              WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    }
}

// ---------------------------------------------------------------- cputimer

static volatile sig_atomic_t g_cpu_ticks;
static volatile long g_cpu_at_tick;  // the process's CPU ms when the first came

static void on_cpu_tick(int sig) {
    (void)sig;
    if (g_cpu_ticks++ == 0) g_cpu_at_tick = clock_ms(CLOCK_PROCESS_CPUTIME_ID);
}

// Work in user mode, with a syscall only every 200000 rounds, until `ticks`
// signals have come or `ms` of wall time have gone by.
static void spin_until(int ticks, long ms) {
    long end = clock_ms(CLOCK_MONOTONIC) + ms;
    while (g_cpu_ticks < ticks && clock_ms(CLOCK_MONOTONIC) < end) {
        for (volatile int i = 0; i < 200000; i++) {
        }
    }
}

// One 100 ms `which` timer: it does not move while the process sleeps, and
// goes off once the process has used 100 ms of CPU.
static void itimer_checks(int which, const char *asleep, const char *used_what) {
    g_cpu_ticks = 0;
    struct itimerval it = {{0, 0}, {0, 100000}};
    setitimer(which, &it, NULL);
    sleep_ms(300);
    struct itimerval left;
    getitimer(which, &left);
    long left_ms = left.it_value.tv_sec * 1000L + left.it_value.tv_usec / 1000;
    check(g_cpu_ticks == 0 && left_ms >= 80, asleep, "%d signal(s), %ld ms left after 300 ms asleep",
          (int)g_cpu_ticks, left_ms);
    long start = clock_ms(CLOCK_PROCESS_CPUTIME_ID);
    spin_until(1, 3000);
    long used = g_cpu_at_tick - start;
    check(g_cpu_ticks == 1 && used >= 80, used_what, "%d signal(s), at %ld ms of CPU",
          (int)g_cpu_ticks, used);
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(which, &off, NULL);
}

static volatile int *g_xcpu;  // count, CPU ms at the first, soft limit after it

static void on_xcpu(int sig) {
    (void)sig;
    if (g_xcpu[0]++ == 0) {
        g_xcpu[1] = (int)clock_ms(CLOCK_PROCESS_CPUTIME_ID);
        struct rlimit rl;
        getrlimit(RLIMIT_CPU, &rl);
        g_xcpu[2] = (int)rl.rlim_cur;
    }
}

// A child with RLIMIT_CPU `soft`/`hard` that sleeps 1.5 s and then works,
// for 5 s of wall time at most; with `handled`, it counts its SIGXCPUs in
// `g_xcpu`. Its status.
static int rlimit_cpu_child(rlim_t soft, rlim_t hard, int handled) {
    pid_t c = fork();
    if (c == 0) {
        struct rlimit no_core = {0, 0}, rl = {soft, hard};
        setrlimit(RLIMIT_CORE, &no_core);
        if (setrlimit(RLIMIT_CPU, &rl)) _exit(5);
        signal(SIGXCPU, handled ? on_xcpu : SIG_DFL);
        sleep_ms(1500);
        g_xcpu[3] = g_xcpu[0];  // what came while asleep
        long end = clock_ms(CLOCK_MONOTONIC) + 5000;
        while (clock_ms(CLOCK_MONOTONIC) < end) {
            for (volatile int i = 0; i < 200000; i++) {
            }
        }
        _exit(7);  // still alive
    }
    return status_of(c);
}

static void sec_cputimer(void) {
    // ITIMER_VIRTUAL counts the process's user time, ITIMER_PROF its user
    // and kernel time: neither moves while it sleeps.
    signal(SIGVTALRM, on_cpu_tick);
    signal(SIGPROF, on_cpu_tick);
    itimer_checks(ITIMER_VIRTUAL, "ITIMER_VIRTUAL: asleep, it waits",
                  "ITIMER_VIRTUAL: 100 ms of user time");
    itimer_checks(ITIMER_PROF, "ITIMER_PROF: asleep, it waits", "ITIMER_PROF: 100 ms of CPU");

    // An interval goes on counting CPU time: every 20 ms over 300 ms of it.
    g_cpu_ticks = 0;
    struct itimerval every = {{0, 20000}, {0, 20000}}, off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_PROF, &every, NULL);
    long start = clock_ms(CLOCK_PROCESS_CPUTIME_ID);
    while (clock_ms(CLOCK_PROCESS_CPUTIME_ID) - start < 300) {
        for (volatile int i = 0; i < 200000; i++) {
        }
    }
    setitimer(ITIMER_PROF, &off, NULL);
    check(g_cpu_ticks >= 8 && g_cpu_ticks <= 20, "ITIMER_PROF every 20 ms of CPU",
          "%d in 300 ms of CPU", (int)g_cpu_ticks);

    // RLIMIT_CPU: a SIGXCPU at the soft limit, which then goes up by one
    // second, and SIGKILL at the hard one. Time asleep does not count.
    g_xcpu = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (g_xcpu == MAP_FAILED) {
        check(0, "a shared page", "%s", strerror(errno));
        return;
    }
    int st = rlimit_cpu_child(1, 2, 1);
    check(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "RLIMIT_CPU 1/2: SIGKILL at the hard limit",
          "status %#x", st);
    check(g_xcpu[3] == 0 && g_xcpu[0] == 1 && g_xcpu[1] >= 900 && g_xcpu[2] == 2,
          "one SIGXCPU at 1 s of CPU, and soft becomes 2",
          "%d while asleep, %d in all, the first at %d ms, soft then %d", g_xcpu[3], g_xcpu[0],
          g_xcpu[1], g_xcpu[2]);
    memset((void *)g_xcpu, 0, 4 * sizeof *g_xcpu);
    st = rlimit_cpu_child(1, RLIM_INFINITY, 0);
    check(WIFSIGNALED(st) && WTERMSIG(st) == SIGXCPU, "RLIMIT_CPU 1: unhandled, SIGXCPU kills",
          "status %#x", st);
    munmap((void *)g_xcpu, 4096);
}

// ---------------------------------------------------------------- cpuclock

static volatile sig_atomic_t g_clk_ticks;
static volatile long g_clk_at;  // what `g_clk_watched` read at the first
static volatile int g_clk_code, g_clk_value;
static clockid_t g_clk_watched;

static void on_clk(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    if (g_clk_ticks++ == 0) {
        g_clk_at = clock_ms(g_clk_watched);
        g_clk_code = si->si_code;
        g_clk_value = si->si_value.sival_int;
    }
}

// A timer on `clock` in `*t` that sends SIGUSR2 with `value`; whether
// timer_create took it, with errno set if not. (Linux numbers a process's
// timers from 0, so the id itself cannot say.)
static int clk_timer(clockid_t clock, int value, timer_t *t) {
    struct sigevent ev;
    memset(&ev, 0, sizeof ev);
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = SIGUSR2;
    ev.sigev_value.sival_int = value;
    return timer_create(clock, &ev, t) == 0;
}

static void clk_arm(timer_t t, long first_ms, long every_ms) {
    struct itimerspec it = {{every_ms / 1000, (every_ms % 1000) * 1000000L},
                            {first_ms / 1000, (first_ms % 1000) * 1000000L}};
    timer_settime(t, 0, &it, NULL);
}

static long clk_left_ms(timer_t t) {
    struct itimerspec it;
    timer_gettime(t, &it);
    return it.it_value.tv_sec * 1000L + it.it_value.tv_nsec / 1000000;
}

// Work in user mode until the calling thread has used `cpu_ms` of CPU or
// `wall_ms` of wall time has gone by.
static void clk_burn(long cpu_ms, long wall_ms) {
    long cpu0 = clock_ms(CLOCK_THREAD_CPUTIME_ID), end = clock_ms(CLOCK_MONOTONIC) + wall_ms;
    while (clock_ms(CLOCK_THREAD_CPUTIME_ID) - cpu0 < cpu_ms && clock_ms(CLOCK_MONOTONIC) < end) {
        for (volatile int i = 0; i < 200000; i++) {
        }
    }
}

// Work until `ticks` signals have come, or 3 s of wall time have gone by.
static void clk_burn_until(int ticks) {
    long end = clock_ms(CLOCK_MONOTONIC) + 3000;
    while (g_clk_ticks < ticks && clock_ms(CLOCK_MONOTONIC) < end) {
        for (volatile int i = 0; i < 200000; i++) {
        }
    }
}

static void *clk_worker(void *arg) {
    clk_burn((long)arg, 5000);
    return NULL;
}

// Watch `clock` with a fresh 100 ms timer: nothing while `idle` runs, then
// one SI_TIMER with the timer's value once the clock has moved 100 ms.
static void clk_watch(clockid_t clock, const char *name, void (*idle)(void), void (*busy)(void)) {
    char what[64];
    g_clk_watched = clock;
    g_clk_ticks = 0;
    timer_t t;
    int made = clk_timer(clock, 0x55, &t);
    snprintf(what, sizeof what, "%s: timer_create", name);
    check(made, what, "%s", made ? "" : strerror(errno));
    if (!made) return;
    clk_arm(t, 100, 0);
    long start = clock_ms(clock);
    // The clock's owner does a little work of its own meanwhile (a sleep, a
    // thread made and joined), and under an emulator that can be tens of
    // ms of kernel time: the vCPU is preempted inside a syscall. So the
    // check is that the timer moved by what the owner itself used, as
    // getrusage counts it, within 10 ms, and not by the 300 ms someone
    // else used, which would have fired it.
    int who = clock == CLOCK_THREAD_CPUTIME_ID ? RUSAGE_THREAD : RUSAGE_SELF;
    struct rusage r0, r1;
    getrusage(who, &r0);
    idle();
    getrusage(who, &r1);
    long left = clk_left_ms(t);
    struct timeval zero = {0, 0}, du, ds;
    timersub(&r1.ru_utime, &r0.ru_utime, &du);
    timersub(&r1.ru_stime, &r0.ru_stime, &ds);
    long user = ms_of(&du, &zero), sys = ms_of(&ds, &zero);
    snprintf(what, sizeof what, "%s: idle, it waits", name);
    check(g_clk_ticks == 0 && labs(100 - left - (user + sys)) <= 10, what,
          "%d signal(s), %ld ms left, its own user %ld + sys %ld ms", (int)g_clk_ticks, left, user,
          sys);
    busy();
    snprintf(what, sizeof what, "%s: 100 ms of it", name);
    check(g_clk_ticks == 1 && g_clk_at - start >= 80 && g_clk_code == SI_TIMER &&
              g_clk_value == 0x55,
          what, "%d signal(s), at %ld ms, si_code %d, value %#x", (int)g_clk_ticks,
          g_clk_at - start, g_clk_code, g_clk_value);
    timer_delete(t);
}

static void clk_sleep(void) { sleep_ms(300); }
static void clk_until_one(void) { clk_burn_until(1); }

// Another thread works 300 ms while this one waits for it.
static void clk_other_works(void) {
    pthread_t w;
    if (pthread_create(&w, NULL, clk_worker, (void *)300L) == 0) pthread_join(w, NULL);
}

static pthread_t g_clk_worker;
static void clk_join_worker(void) { pthread_join(g_clk_worker, NULL); }

static pid_t g_clk_child;
static void clk_wait_for_one(void) {
    long end = clock_ms(CLOCK_MONOTONIC) + 3500;
    while (g_clk_ticks == 0 && clock_ms(CLOCK_MONOTONIC) < end) sleep_ms(10);
}

static void sec_cpuclock(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_clk;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR2, &sa, NULL);

    clk_watch(CLOCK_PROCESS_CPUTIME_ID, "the process clock", clk_sleep, clk_until_one);
    // The caller's own thread clock: another thread's work does not move it.
    clk_watch(CLOCK_THREAD_CPUTIME_ID, "this thread's clock", clk_other_works, clk_until_one);

    // An interval goes on counting CPU time: every 20 ms over 300 ms of it.
    timer_t t;
    clk_timer(CLOCK_PROCESS_CPUTIME_ID, 1, &t);
    g_clk_watched = CLOCK_PROCESS_CPUTIME_ID;
    g_clk_ticks = 0;
    clk_arm(t, 20, 20);
    clk_burn(300, 3000);
    clk_arm(t, 0, 0);
    check(g_clk_ticks >= 8 && g_clk_ticks <= 20, "every 20 ms of CPU", "%d in 300 ms of CPU",
          (int)g_clk_ticks);

    // TIMER_ABSTIME is a reading of the clock, and one gone by fires before
    // timer_settime returns.
    g_clk_ticks = 0;
    struct itimerspec past = {{0, 0}, {0, 1}};
    int r = timer_settime(t, TIMER_ABSTIME, &past, NULL);
    check(r == 0 && g_clk_ticks == 1, "an absolute expiry gone by: at once", "ret %d, %d signal(s)",
          r, (int)g_clk_ticks);
    timer_delete(t);

    // Another thread's clock, as pthread_getcpuclockid names it: its work
    // moves it while this thread waits.
    clockid_t cid;
    if (pthread_create(&g_clk_worker, NULL, clk_worker, (void *)300L) == 0 &&
        pthread_getcpuclockid(g_clk_worker, &cid) == 0) {
        g_clk_watched = cid;
        g_clk_ticks = 0;
        int made = clk_timer(cid, 0x55, &t);
        check(made, "another thread's clock: timer_create", "%s", made ? "" : strerror(errno));
        if (made) clk_arm(t, 100, 0);
        long start = clock_ms(cid);
        clk_join_worker();
        check(g_clk_ticks == 1 && g_clk_at - start >= 80, "another thread's clock: its 100 ms",
              "%d signal(s), at %ld ms of it", (int)g_clk_ticks, g_clk_at - start);
        if (made) timer_delete(t);
    }

    // Another process's clock, as clock_getcpuclockid names it: the signal
    // comes here, at the child's 100 ms of CPU, none of it while it sleeps.
    g_clk_child = fork();
    if (g_clk_child == 0) {
        sleep_ms(300);
        clk_burn(3000, 4000);
        _exit(0);
    }
    if (clock_getcpuclockid(g_clk_child, &cid) == 0) {
        g_clk_watched = cid;
        g_clk_ticks = 0;
        int made = clk_timer(cid, 0x55, &t);
        check(made, "a child's clock: timer_create", "%s", made ? "" : strerror(errno));
        if (made) clk_arm(t, 100, 0);
        sleep_ms(200);
        int asleep = g_clk_ticks;
        clk_wait_for_one();
        long at = g_clk_at;
        check(asleep == 0 && g_clk_ticks == 1 && at >= 80, "a child's clock: its 100 ms, here",
              "%d while it slept, %d in all, at %ld ms of it", asleep, (int)g_clk_ticks, at);
        if (made) timer_delete(t);
    }
    kill(g_clk_child, SIGKILL);
    status_of(g_clk_child);

    // A dynamic clock (a PTP device's fd) carries no timer_create:
    // FD_TO_CLOCKID(0) is -5.
    timer_t none;
    errno = 0;
    r = timer_create((clockid_t)-5, NULL, &none);
    check(r == -1 && errno == EOPNOTSUPP, "a dynamic clock: EOPNOTSUPP", "ret %d errno %d", r,
          errno);
    signal(SIGUSR2, SIG_DFL);
}

// ----------------------------------------------------------------- aslimit

// `VmSize` of /proc/self/status in bytes, -1 if it is not there: what
// RLIMIT_AS is measured against, in Linux `mm->total_vm`.
static long vm_size(void) {
    FILE *f = fopen("/proc/self/status", "r");
    if (!f) return -1;
    char line[256];
    long kb = -1;
    while (fgets(line, sizeof line, f) && sscanf(line, "VmSize: %ld kB", &kb) != 1) {
    }
    fclose(f);
    return kb < 0 ? -1 : kb * 1024;
}

#define AS_MIB (1024L * 1024)

static void *as_map(void *at, long len, int fixed) {
    return mmap(at, (size_t)len, PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | (fixed ? MAP_FIXED : 0), -1, 0);
}

static void sec_aslimit(void) {
    // Everything the checks need that could map memory is made first, then
    // the soft limit goes to 4 MiB above what is mapped, the calls run with
    // nothing printed in between, and the limit comes back before a line is
    // written. The hard limit is never touched, so it can come back.
    int shm = shmget(IPC_PRIVATE, (size_t)(8 * AS_MIB), IPC_CREAT | 0600);
    struct rlimit was;
    getrlimit(RLIMIT_AS, &was);
    long size = vm_size();
    if (size < 0 || was.rlim_cur != RLIM_INFINITY) {
        check(0, "VmSize, and no RLIMIT_AS yet", "VmSize %ld, RLIMIT_AS %lu", size,
              (unsigned long)was.rlim_cur);
        if (shm >= 0) shmctl(shm, IPC_RMID, NULL);
        return;
    }
    struct rlimit tight = {(rlim_t)(size + 4 * AS_MIB), was.rlim_max};
    int set = setrlimit(RLIMIT_AS, &tight);

    void *two = as_map(NULL, 2 * AS_MIB, 0);            // 2 MiB of room left
    void *eight = as_map(NULL, 8 * AS_MIB, 0);          // no
    int eight_errno = errno;
    void *big = malloc((size_t)(16 * AS_MIB));          // no
    long brk0 = syscall(SYS_brk, 0);
    long brk_big = syscall(SYS_brk, brk0 + 8 * AS_MIB);  // no: the old break
    long brk_small = syscall(SYS_brk, brk0 + 64 * 1024);
    syscall(SYS_brk, brk0);
    errno = 0;
    void *grown = two == MAP_FAILED ? MAP_FAILED
                                    : mremap(two, (size_t)(2 * AS_MIB), (size_t)(16 * AS_MIB),
                                             MREMAP_MAYMOVE);
    int grown_errno = errno;
    // 1 MiB of room left: a MAP_FIXED of 2 MiB over the 2 already mapped
    // adds nothing, so it fits; counted whole it would not.
    void *fill = as_map(NULL, 1 * AS_MIB, 0);
    void *over = two == MAP_FAILED ? MAP_FAILED : as_map(two, 2 * AS_MIB, 1);
    // What munmap gives back can be mapped again.
    if (two != MAP_FAILED) munmap(two, (size_t)(2 * AS_MIB));
    void *again = as_map(NULL, 2 * AS_MIB, 0);
    errno = 0;
    void *att = shm >= 0 ? shmat(shm, NULL, 0) : (void *)-1;
    int att_errno = errno;

    setrlimit(RLIMIT_AS, &was);
    void *after = as_map(NULL, 64 * AS_MIB, 0);

    check(set == 0, "RLIMIT_AS: set 4 MiB above VmSize", "VmSize %ld kB", size / 1024);
    check(two != MAP_FAILED, "RLIMIT_AS: 2 MiB of mmap in 4 of room", "%p", two);
    check(eight == MAP_FAILED && eight_errno == ENOMEM, "RLIMIT_AS: 8 MiB of mmap, ENOMEM",
          "%p errno %d", eight, eight == MAP_FAILED ? eight_errno : 0);
    check(big == NULL, "RLIMIT_AS: malloc of 16 MiB is NULL", "%p", big);
    check(brk_big == brk0 && brk_small == brk0 + 64 * 1024, "RLIMIT_AS: brk +8 MiB no, +64 KiB yes",
          "+8 MiB moved it %+ld, +64 KiB %+ld", brk_big - brk0, brk_small - brk0);
    check(grown == MAP_FAILED && grown_errno == ENOMEM, "RLIMIT_AS: mremap 2 to 16 MiB, ENOMEM",
          "%p errno %d", grown, grown == MAP_FAILED ? grown_errno : 0);
    check(over != MAP_FAILED, "RLIMIT_AS: MAP_FIXED over mapped pages", "%p (fill %p)", over,
          fill);
    check(again != MAP_FAILED, "RLIMIT_AS: what munmap freed maps again", "%p", again);
    check(att == (void *)-1 && att_errno == ENOMEM, "RLIMIT_AS: shmat of 8 MiB, ENOMEM",
          "%p errno %d", att, att == (void *)-1 ? att_errno : 0);
    check(after != MAP_FAILED, "RLIMIT_AS: limit back, 64 MiB maps", "%p", after);

    if (eight != MAP_FAILED) munmap(eight, (size_t)(8 * AS_MIB));
    free(big);
    if (grown != MAP_FAILED) munmap(grown, (size_t)(16 * AS_MIB));
    if (fill != MAP_FAILED) munmap(fill, (size_t)AS_MIB);
    if (again != MAP_FAILED) munmap(again, (size_t)(2 * AS_MIB));
    if (att != (void *)-1) shmdt(att);
    if (after != MAP_FAILED) munmap(after, (size_t)(64 * AS_MIB));
    if (shm >= 0) shmctl(shm, IPC_RMID, NULL);
}

// ------------------------------------------------------------------ nproc

// A uid nothing else on the machine runs as, so every task it has is one
// this section made.
#define NPROC_UID 45399

static void *nproc_thread(void *arg) {
    (void)arg;
    return NULL;
}

// A child of the user that lives until the write end of `rel` is closed.
static pid_t nproc_child(const int rel[2]) {
    pid_t pid = fork();
    if (pid == 0) {
        char c;
        close(rel[1]);
        while (read(rel[0], &c, 1) == -1 && errno == EINTR) {
        }
        _exit(0);
    }
    return pid;
}

// Children until a fork fails, at most `max`; the failure's errno in `err`.
static int nproc_fill(const int rel[2], pid_t *kids, int max, int *err) {
    int n = 0;
    *err = 0;
    while (n < max) {
        pid_t pid = nproc_child(rel);
        if (pid < 0) {
            *err = errno;
            break;
        }
        kids[n++] = pid;
    }
    return n;
}

static void nproc_as_user(void) {
    int rel[2];
    if (pipe(rel) != 0) {
        check(0, "RLIMIT_NPROC: a pipe", "errno %d", errno);
        return;
    }
    // Soft 4 under a hard 6, set as root; then all three ids are the user's.
    struct rlimit lim = {4, 6};
    int set = setrlimit(RLIMIT_NPROC, &lim);
    int became = setresuid(NPROC_UID, NPROC_UID, NPROC_UID);
    pid_t kids[16];
    int err;
    // This process is one task, so three children make four.
    int first = nproc_fill(rel, kids, 8, &err), n = first;
    int full_err = err;
    pthread_t t;
    int thread_err = pthread_create(&t, NULL, nproc_thread, NULL);
    if (thread_err == 0) pthread_join(t, NULL);
    // A zombie is still a task until it is reaped.
    pid_t zombie_fork = -1, reaped_fork = -1;
    int zombie_err = 0;
    if (n > 0) {
        siginfo_t si;
        kill(kids[0], SIGKILL);
        waitid(P_PID, (id_t)kids[0], &si, WEXITED | WNOWAIT);
        errno = 0;
        zombie_fork = nproc_child(rel);
        zombie_err = errno;
        if (zombie_fork > 0) kids[n++] = zombie_fork;
        waitpid(kids[0], NULL, 0);
        kids[0] = -1;
        reaped_fork = nproc_child(rel);
        if (reaped_fork > 0) kids[n++] = reaped_fork;
    }
    // It is the soft limit that is held: raised to the hard one, two more.
    lim.rlim_cur = 6;
    int raised = setrlimit(RLIMIT_NPROC, &lim);
    int more = nproc_fill(rel, kids + n, 4, &err);
    n += more;

    close(rel[1]);
    close(rel[0]);
    for (int i = 0; i < n; i++)
        if (kids[i] > 0) waitpid(kids[i], NULL, 0);

    check(set == 0 && became == 0, "RLIMIT_NPROC: 4 for a user of its own",
          "setrlimit %d setresuid %d", set, became);
    check(first == 3 && full_err == EAGAIN, "RLIMIT_NPROC: 3 children, then EAGAIN",
          "%d children, then errno %d", first, full_err);
    check(thread_err == EAGAIN, "RLIMIT_NPROC: a thread is a task too", "pthread_create %d",
          thread_err);
    check(zombie_fork < 0 && zombie_err == EAGAIN, "RLIMIT_NPROC: a zombie still counts",
          "fork %d errno %d", (int)zombie_fork, zombie_err);
    check(reaped_fork > 0, "RLIMIT_NPROC: reaped, its room is back", "fork %d",
          (int)reaped_fork);
    check(raised == 0 && more == 2 && err == EAGAIN, "RLIMIT_NPROC: soft raised to 6, two more",
          "%d more, then errno %d", more, err);
}

// Whether a fork goes through with RLIMIT_NPROC at 0 and these three ids.
static int nproc_exempt(uid_t ruid, uid_t euid) {
    struct rlimit zero = {0, 0};
    if (setrlimit(RLIMIT_NPROC, &zero) != 0 || setresuid(ruid, euid, euid) != 0) return -1;
    pid_t pid = fork();
    if (pid == 0) _exit(0);
    if (pid > 0) waitpid(pid, NULL, 0);
    return pid > 0;
}

static void sec_nproc(void) {
    int st = 0;
    pid_t pid = fork();
    if (pid == 0) {
        g_fail = 0;
        nproc_as_user();
        fflush(stdout);
        _exit(g_fail > 100 ? 100 : g_fail);
    }
    waitpid(pid, &st, 0);
    g_fail += WIFEXITED(st) ? WEXITSTATUS(st) : 1;

    // Root, real or effective, is not held to it.
    struct {
        uid_t ruid, euid;
        const char *what;
    } exempt[] = {
        {0, 0, "RLIMIT_NPROC: root forks at a limit of 0"},
        {NPROC_UID, 0, "RLIMIT_NPROC: so does an effective root"},
    };
    for (size_t i = 0; i < sizeof exempt / sizeof exempt[0]; i++) {
        pid = fork();
        if (pid == 0) _exit(nproc_exempt(exempt[i].ruid, exempt[i].euid) == 1 ? 0 : 1);
        waitpid(pid, &st, 0);
        check(WIFEXITED(st) && WEXITSTATUS(st) == 0, exempt[i].what, "st=%#x", st);
    }
}

// ----------------------------------------------------------------- zombie

// A dead child is a pid until somebody reaps it, and nothing after: kill,
// sigqueue, getpgid, getsid, pidfd_open and /proc/<pid> all find a zombie,
// whoever asks, and none of them finds a reaped one. Every round is a race
// against the child's own teardown, which goes on after its exit status is
// on file and after it is reaped, so the round is made many times.

#ifndef P_PIDFD
#define P_PIDFD 3
#endif

#define ZOMBIE_ROUNDS 40

// A child that has exited, and that nobody has reaped yet.
static pid_t zombie_child(void) {
    pid_t c = fork();
    if (c == 0) _exit(0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    while (waitid(P_PID, (id_t)c, &si, WEXITED | WNOWAIT) == -1 && errno == EINTR) {
    }
    return c;
}

// Fields 3 and 4 of /proc/<pid>/stat, as "Z 123"; empty when there is none.
static void stat_state_and_ppid(pid_t pid, char *out, size_t len) {
    char path[64], buf[512];
    out[0] = 0;
    snprintf(path, sizeof path, "/proc/%d/stat", (int)pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return;
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    char state;
    int ppid;
    if (p && sscanf(p + 2, "%c %d", &state, &ppid) == 2) snprintf(out, len, "%c %d", state, ppid);
}

static long cmdline_bytes(pid_t pid) {
    char path[64], buf[256];
    snprintf(path, sizeof path, "/proc/%d/cmdline", (int)pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    long n = (long)read(fd, buf, sizeof buf);
    close(fd);
    return n;
}

static int proc_lists(pid_t pid) {
    DIR *d = opendir("/proc");
    if (!d) return 0;
    int found = 0;
    struct dirent *e;
    while (!found && (e = readdir(d)) != NULL) found = atoi(e->d_name) == pid;
    closedir(d);
    return found;
}

static void sec_zombie(void) {
    int term = 0, zero = 0, pgid = 0, sid = 0, reaped = 0;
    for (int i = 0; i < ZOMBIE_ROUNDS; i++) {
        pid_t c = zombie_child();
        term += kill(c, SIGTERM) != 0;
        zero += kill(c, 0) != 0;
        pgid += getpgid(c) != getpgrp();
        sid += getsid(c) != getsid(0);
        waitpid(c, NULL, 0);
        reaped += !(kill(c, 0) == -1 && errno == ESRCH);
    }
    check(term == 0, "kill(zombie, SIGTERM) is 0", "%d of %d ESRCH", term, ZOMBIE_ROUNDS);
    check(zero == 0, "kill(zombie, 0) is 0", "%d of %d ESRCH", zero, ZOMBIE_ROUNDS);
    check(pgid == 0, "getpgid of a zombie: its group", "%d of %d not", pgid, ZOMBIE_ROUNDS);
    check(sid == 0, "getsid of a zombie: its session", "%d of %d not", sid, ZOMBIE_ROUNDS);
    check(reaped == 0, "reaped, kill(pid, 0) is ESRCH", "%d of %d still there", reaped,
          ZOMBIE_ROUNDS);

    // Somebody else's: a sibling asks, which is not the zombie's parent.
    pid_t z = zombie_child();
    pid_t k = fork();
    if (k == 0) _exit((kill(z, SIGTERM) != 0) | (kill(z, 0) != 0) << 1 | (getpgid(z) < 0) << 2);
    int st = 0;
    waitpid(k, &st, 0);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 0, "a sibling finds the zombie too",
          "kill(SIGTERM) %s, kill(0) %s, getpgid %s",
          WEXITSTATUS(st) & 1 ? "ESRCH" : "0", WEXITSTATUS(st) & 2 ? "ESRCH" : "0",
          WEXITSTATUS(st) & 4 ? "ESRCH" : "ok");

    char got[32], want[32];
    stat_state_and_ppid(z, got, sizeof got);
    snprintf(want, sizeof want, "Z %d", (int)getpid());
    check(strcmp(got, want) == 0, "/proc/<zombie>/stat: Z, and us", "'%s'", got);
    long len = cmdline_bytes(z);
    check(len == 0, "/proc/<zombie>/cmdline is empty", "%ld bytes", len);
    check(proc_lists(z), "`ls /proc` lists the zombie", "%s", proc_lists(z) ? "listed" : "not");

    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_signo = SIGUSR1;
    si.si_code = SI_QUEUE;
    si.si_pid = getpid();
    si.si_uid = getuid();
    long r = syscall(SYS_rt_sigqueueinfo, z, SIGUSR1, &si);
    check(r == 0, "sigqueue to a zombie is 0", "%s", r == 0 ? "0" : strerror(errno));

    int fd = (int)syscall(SYS_pidfd_open, z, 0);
    struct pollfd pf = {fd, POLLIN, 0};
    int ready = fd >= 0 ? poll(&pf, 1, 0) : -1;
    long sent = fd >= 0 ? syscall(SYS_pidfd_send_signal, fd, SIGTERM, NULL, 0) : -1;
    check(fd >= 0 && ready == 1 && sent == 0, "pidfd of a zombie: ready, signalled",
          "pidfd_open %s, poll %d, send %ld", fd >= 0 ? "ok" : strerror(errno), ready, sent);

    memset(&si, 0, sizeof si);
    int w = fd >= 0 ? waitid(P_PIDFD, (id_t)fd, &si, WEXITED) : waitid(P_PID, (id_t)z, &si, WEXITED);
    check(w == 0 && si.si_pid == z, "reaped", "waitid %d, si_pid %d", w, (int)si.si_pid);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d", (int)z);
    struct stat sb;
    int there = stat(path, &sb) == 0;
    int listed = proc_lists(z);
    check(!there && !listed, "reaped, /proc/<pid> is gone", "%s",
          there ? "still there" : listed ? "still listed" : "gone");
    if (fd >= 0) {
        sent = syscall(SYS_pidfd_send_signal, fd, 0, NULL, 0);
        check(sent == -1 && errno == ESRCH, "reaped, its pidfd says ESRCH", "%ld", sent);
        close(fd);
    }
    int again = (int)syscall(SYS_pidfd_open, z, 0);
    check(again == -1 && errno == ESRCH, "reaped, pidfd_open is ESRCH", "%d", again);
}

// -------------------------------------------------------------------- kin

static char g_kin_name[16];
static volatile pid_t g_kin_tid;
static volatile int g_kin_go;

static void *kin_reports_name(void *arg) {
    (void)arg;
    prctl(PR_GET_NAME, g_kin_name);
    return NULL;
}

static void *kin_waits(void *arg) {
    (void)arg;
    g_kin_tid = (pid_t)syscall(SYS_gettid);
    while (!g_kin_go) sleep_ms(1);
    return NULL;
}

// Field `n` of a stat file, as proc(5) numbers them; -999 when unreadable.
static long stat_field(const char *path, int n) {
    char buf[1024];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -999;
    ssize_t len = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (len <= 0) return -999;
    buf[len] = 0;
    char *p = strrchr(buf, ')');
    for (int i = 3; p && i <= n; i++) {
        p = strchr(p, ' ');
        if (p) p++;
    }
    return p ? strtol(p, NULL, 10) : -999;
}

#define KIN_EXECS 20

static void sec_kin(void) {
    char old[16] = {0};
    prctl(PR_GET_NAME, old);
    prctl(PR_SET_NAME, "jefe");
    pthread_t t;
    pthread_create(&t, NULL, kin_reports_name, NULL);
    pthread_join(t, NULL);
    check(strcmp(g_kin_name, "jefe") == 0, "a new thread is named as its creator", "'%s'",
          g_kin_name);
    pid_t c = fork();
    if (c == 0) {
        char name[16] = {0};
        prctl(PR_GET_NAME, name);
        _exit(strcmp(name, "jefe") != 0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    check(WIFEXITED(st) && WEXITSTATUS(st) == 0, "a forked child is named as its parent",
          "status %#x", st);
    prctl(PR_SET_NAME, old);

    long ours = stat_field("/proc/self/stat", 38);
    check(ours == SIGCHLD, "stat field 38 is SIGCHLD", "%ld", ours);
    pthread_create(&t, NULL, kin_waits, NULL);
    while (!g_kin_tid) sleep_ms(1);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat", (int)g_kin_tid);
    long theirs = stat_field(path, 38);
    // That thread is there, but it is no process: without PIDFD_THREAD,
    // `pidfd_open` says ENOENT for it, not "no such process".
    errno = 0;
    long pidfd = syscall(SYS_pidfd_open, g_kin_tid, 0);
    int pidfd_err = errno;
    if (pidfd >= 0) close((int)pidfd);
    g_kin_go = 1;
    pthread_join(t, NULL);
    check(theirs == -1, "a thread's stat field 38 is -1", "%ld", theirs);
    check(pidfd == -1 && pidfd_err == ENOENT, "pidfd_open of a thread: ENOENT", "%s",
          pidfd >= 0 ? "a pidfd" : strerror(pidfd_err));

    // The EOF of a close-on-exec pipe is how `posix_spawn` and Rust's
    // `Command` learn the exec happened; Linux clears PF_FORKNOEXEC before
    // it closes those descriptors.
    int refused = 0, failed = 0;
    for (int i = 0; i < KIN_EXECS; i++) {
        int p[2];
        if (pipe2(p, O_CLOEXEC)) {
            check(0, "pipe2", "%s", strerror(errno));
            return;
        }
        c = fork();
        if (c == 0) {
            char *args[] = {"sleep", "5", NULL};
            execv("/bin/sleep", args);
            _exit(127);
        }
        close(p[1]);
        char b;
        ssize_t n = read(p[0], &b, 1);
        close(p[0]);
        int r = setpgid(c, c);
        int e = errno;
        kill(c, SIGKILL);
        waitpid(c, &st, 0);
        if (n != 0 || (WIFEXITED(st) && WEXITSTATUS(st) == 127))
            failed++;
        else if (r == -1 && e == EACCES)
            refused++;
    }
    check(refused == KIN_EXECS, "setpgid on a child past its exec: EACCES",
          "%d of %d refused, %d did not exec", refused, KIN_EXECS, failed);
}

// ----------------------------------------------------------------- status

// The value of line `key` of a status file into `out`; 0 when the file or
// the line is not there.
static int status_value(const char *path, const char *key, char *out, size_t size) {
    char buf[8192];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return 0;
    ssize_t len = 0, n;
    while (len < (ssize_t)sizeof buf - 1 && (n = read(fd, buf + len, sizeof buf - 1 - len)) > 0)
        len += n;
    close(fd);
    buf[len < 0 ? 0 : len] = 0;
    size_t klen = strlen(key);
    for (char *l = buf; l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        if (strncmp(l, key, klen) || l[klen] != ':' || l[klen + 1] != '\t') continue;
        char *v = l + klen + 2, *end = strchr(v, '\n');
        size_t vlen = end ? (size_t)(end - v) : strlen(v);
        if (vlen >= size) vlen = size - 1;
        memcpy(out, v, vlen);
        out[vlen] = 0;
        return 1;
    }
    return 0;
}

static unsigned long long status_hex(const char *path, const char *key) {
    char v[256];
    if (!status_value(path, key, v, sizeof v)) return ~0ULL;
    char digits[256];
    size_t n = 0;
    for (char *p = v; *p && n < sizeof digits - 1; p++)
        if (*p != ',') digits[n++] = *p;
    digits[n] = 0;
    return strtoull(digits, NULL, 16);
}

// The order `proc_pid_status()` writes these in. Linux writes more (the
// memory lines, `Seccomp:`, the context switches), so this is the order of
// the ones named, not the whole file.
static const char *const STATUS_KEYS[] = {
    "Name",     "Umask",  "State",  "Tgid",        "Ngid",   "Pid",    "PPid",
    "TracerPid", "Uid",   "Gid",    "FDSize",      "Groups", "NStgid", "NSpid",
    "NSpgid",   "NSsid",  "Threads", "SigQ",       "SigPnd", "ShdPnd", "SigBlk",
    "SigIgn",   "SigCgt", "CapInh", "CapPrm",      "CapEff", "CapBnd", "CapAmb",
    "NoNewPrivs", "Cpus_allowed", "Cpus_allowed_list", "Mems_allowed_list",
};

// The first key of STATUS_KEYS missing from the file or out of its order.
static const char *status_order(const char *path) {
    char buf[8192];
    int fd = open(path, O_RDONLY);
    if (fd < 0) return "the file";
    ssize_t len = 0, n;
    while (len < (ssize_t)sizeof buf - 1 && (n = read(fd, buf + len, sizeof buf - 1 - len)) > 0)
        len += n;
    close(fd);
    buf[len < 0 ? 0 : len] = 0;
    size_t want = 0, nkeys = sizeof STATUS_KEYS / sizeof STATUS_KEYS[0];
    for (char *l = buf; l && *l && want < nkeys; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        size_t klen = strlen(STATUS_KEYS[want]);
        if (!strncmp(l, STATUS_KEYS[want], klen) && l[klen] == ':') want++;
    }
    return want == nkeys ? NULL : STATUS_KEYS[want];
}

static volatile pid_t g_status_tid;
static volatile int g_status_go;

static void *status_thread(void *arg) {
    (void)arg;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGHUP);
    pthread_sigmask(SIG_SETMASK, &s, NULL);
    g_status_tid = (pid_t)syscall(SYS_gettid);
    while (!g_status_go) sleep_ms(1);
    return NULL;
}

static void status_caught(int sig) { (void)sig; }

static void sec_status(void) {
    const char *self = "/proc/self/status";
    char v[256], want[256];
    const char *missing = status_order(self);
    check(!missing, "status has Linux's lines in Linux's order", "first missing or out of place: %s",
          missing ? missing : "-");

    mode_t old = umask(027);
    int got = status_value(self, "Umask", v, sizeof v);
    umask(old);
    check(got && !strcmp(v, "0027"), "Umask is the process's, as %#04o", "'%s'", got ? v : "(none)");
    got = status_value(self, "TracerPid", v, sizeof v);
    check(got && !strcmp(v, "0"), "TracerPid is 0 for a process nobody traces", "'%s'",
          got ? v : "(none)");

    // A shell keeps its terminal on fd 255: 256 slots.
    dup2(0, 255);
    got = status_value(self, "FDSize", v, sizeof v);
    close(255);
    check(got && !strcmp(v, "256"), "FDSize with fd 255 open is 256", "'%s'", got ? v : "(none)");

    char ids[4][32];
    const char *keys[4] = {"NStgid", "NSpid", "NSpgid", "NSsid"};
    long expect[4] = {getpid(), getpid(), getpgid(0), getsid(0)};
    int ids_ok = 1;
    for (int i = 0; i < 4; i++) {
        snprintf(want, sizeof want, "%ld", expect[i]);
        if (!status_value(self, keys[i], ids[i], sizeof ids[i]) || strcmp(ids[i], want)) ids_ok = 0;
    }
    check(ids_ok, "NStgid, NSpid, NSpgid and NSsid are getpid, getpid, getpgid and getsid",
          "%s %s %s %s", ids[0], ids[1], ids[2], ids[3]);

    // Pending on the thread (`raise` is a tgkill) and on the process (`kill`),
    // both blocked; SIGINT ignored and SIGTERM caught.
    sigset_t block, before;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigaddset(&block, SIGUSR2);
    sigaddset(&block, SIGWINCH);
    sigprocmask(SIG_SETMASK, &block, &before);
    signal(SIGINT, SIG_IGN);
    signal(SIGTERM, status_caught);
    raise(SIGUSR1);
    kill(getpid(), SIGUSR2);
    unsigned long long pnd = status_hex(self, "SigPnd"), shd = status_hex(self, "ShdPnd");
    unsigned long long blk = status_hex(self, "SigBlk");
    unsigned long long ign = status_hex(self, "SigIgn") & 0x7fffffff;
    unsigned long long cgt = status_hex(self, "SigCgt") & 0x7fffffff;
    got = status_value(self, "SigQ", v, sizeof v);
    unsigned long long expect_ign = 0, expect_cgt = 0;
    for (int sig = 1; sig < 32; sig++) {
        struct sigaction sa;
        if (sigaction(sig, NULL, &sa)) continue;
        if (sa.sa_handler == SIG_IGN) expect_ign |= 1ULL << (sig - 1);
        else if (sa.sa_handler != SIG_DFL) expect_cgt |= 1ULL << (sig - 1);
    }
    signal(SIGUSR1, SIG_IGN);  // throws the two pending ones away
    signal(SIGUSR2, SIG_IGN);
    sigprocmask(SIG_SETMASK, &before, NULL);
    check(pnd == 1ULL << (SIGUSR1 - 1), "SigPnd is what was raised on the thread", "%016llx", pnd);
    check(shd == 1ULL << (SIGUSR2 - 1), "ShdPnd is what was sent to the process", "%016llx", shd);
    unsigned long long expect_blk =
        1ULL << (SIGUSR1 - 1) | 1ULL << (SIGUSR2 - 1) | 1ULL << (SIGWINCH - 1);
    check(blk == expect_blk, "SigBlk is the mask", "%016llx, want %016llx", blk, expect_blk);
    check(ign == expect_ign && (ign & 1ULL << (SIGINT - 1)), "SigIgn is what is ignored",
          "%016llx, want %016llx", ign, expect_ign);
    check(cgt == expect_cgt && (cgt & 1ULL << (SIGTERM - 1)), "SigCgt is what is caught",
          "%016llx, want %016llx", cgt, expect_cgt);
    struct rlimit rl;
    getrlimit(RLIMIT_SIGPENDING, &rl);
    unsigned long long queued = 0, limit = 0;
    int parsed = got && sscanf(v, "%llu/%llu", &queued, &limit) == 2;
    check(parsed && limit == (unsigned long long)rl.rlim_cur && queued >= 2,
          "SigQ counts our two pending and its limit is RLIMIT_SIGPENDING", "'%s', limit %llu",
          got ? v : "(none)", (unsigned long long)rl.rlim_cur);

    struct {
        uint32_t version;
        int pid;
    } hdr = {0x20080522, 0};
    struct {
        uint32_t effective, permitted, inheritable;
    } data[2] = {{0, 0, 0}, {0, 0, 0}};
    syscall(SYS_capget, &hdr, data);
    unsigned long long eff = data[0].effective | (unsigned long long)data[1].effective << 32;
    unsigned long long prm = data[0].permitted | (unsigned long long)data[1].permitted << 32;
    unsigned long long bnd = 0;
    for (int cap = 0; cap < 64; cap++)
        if (prctl(PR_CAPBSET_READ, cap, 0, 0, 0) == 1) bnd |= 1ULL << cap;
    unsigned long long s_eff = status_hex(self, "CapEff"), s_prm = status_hex(self, "CapPrm");
    unsigned long long s_bnd = status_hex(self, "CapBnd");
    check(s_eff == eff && s_prm == prm && s_bnd == bnd,
          "CapEff, CapPrm and CapBnd are what capget and PR_CAPBSET_READ say",
          "%llx %llx %llx, want %llx %llx %llx", s_eff, s_prm, s_bnd, eff, prm, bnd);

    int nnp_before = status_value(self, "NoNewPrivs", v, sizeof v) && !strcmp(v, "0");
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    int nnp_after = status_value(self, "NoNewPrivs", v, sizeof v) && !strcmp(v, "1");
    check(nnp_before && nnp_after, "NoNewPrivs follows PR_SET_NO_NEW_PRIVS", "%d then %d",
          nnp_before, nnp_after);

    unsigned long mask = 0;
    syscall(SYS_sched_getaffinity, 0, sizeof mask, &mask);
    unsigned long long hex = status_hex(self, "Cpus_allowed");
    check(hex == mask, "Cpus_allowed is the affinity", "%llx, want %lx", hex, mask);
    int first = __builtin_ctzl(mask);
    unsigned long one = 1UL << first;
    syscall(SYS_sched_setaffinity, 0, sizeof one, &one);
    snprintf(want, sizeof want, "%d", first);
    got = status_value(self, "Cpus_allowed_list", v, sizeof v);
    syscall(SYS_sched_setaffinity, 0, sizeof mask, &mask);
    check(got && !strcmp(v, want), "Cpus_allowed_list follows sched_setaffinity", "'%s', want '%s'",
          got ? v : "(none)", want);

    pthread_t t;
    g_status_tid = 0;
    g_status_go = 0;
    pthread_create(&t, NULL, status_thread, NULL);
    while (!g_status_tid) sleep_ms(1);
    char path[64], tid[32], pid[32], tgid[32];
    snprintf(path, sizeof path, "/proc/self/task/%d/status", (int)g_status_tid);
    snprintf(want, sizeof want, "%d", (int)g_status_tid);
    int pid_ok = status_value(path, "Pid", pid, sizeof pid) && !strcmp(pid, want);
    int nspid_ok = status_value(path, "NSpid", tid, sizeof tid) && !strcmp(tid, want);
    snprintf(want, sizeof want, "%d", (int)getpid());
    int tgid_ok = status_value(path, "Tgid", tgid, sizeof tgid) && !strcmp(tgid, want);
    unsigned long long their_blk = status_hex(path, "SigBlk");
    g_status_go = 1;
    pthread_join(t, NULL);
    check(pid_ok && nspid_ok && tgid_ok && their_blk == 1ULL << (SIGHUP - 1),
          "a thread's status has its own Pid, NSpid and SigBlk, and the process's Tgid",
          "Pid %d NSpid %d Tgid %d SigBlk %016llx", pid_ok, nspid_ok, tgid_ok, their_blk);

    pid_t z = fork();
    if (z == 0) _exit(0);
    snprintf(path, sizeof path, "/proc/%d/status", (int)z);
    int zombie = 0;
    for (int i = 0; i < 2000 && !zombie; i++) {
        zombie = status_value(path, "State", v, sizeof v) && v[0] == 'Z';
        if (!zombie) sleep_ms(1);
    }
    int no_umask = !status_value(path, "Umask", v, sizeof v);
    int no_vm = !status_value(path, "VmSize", v, sizeof v);
    int fd0 = status_value(path, "FDSize", v, sizeof v) && !strcmp(v, "0");
    waitpid(z, NULL, 0);
    check(zombie && no_umask && no_vm && fd0, "a zombie's status has no Umask, no Vm lines and FDSize 0",
          "zombie %d, no Umask %d, no VmSize %d, FDSize 0 %d", zombie, no_umask, no_vm, fd0);
}

// ----------------------------------------------------------------- execat

// The far side of `execat`: what this image is called, written to `fd` as
// "exe\ncomm\n".
static int whoami_report(int fd) {
    char exe[PATH_MAX] = {0}, comm[32] = {0};
    if (readlink("/proc/self/exe", exe, sizeof exe - 1) < 0) strcpy(exe, "?");
    int c = open("/proc/self/comm", O_RDONLY);
    ssize_t n = c >= 0 ? read(c, comm, sizeof comm - 1) : -1;
    if (c >= 0) close(c);
    if (n > 0 && comm[n - 1] == '\n') comm[n - 1] = 0;
    dprintf(fd, "%s\n%s\n", exe, comm);
    return 0;
}

enum exec_by { BY_EXECVEAT, BY_FEXECVE, BY_EXECVE };

// What an exec did: refused with `err`, or ran an image that calls itself
// `exe` and `comm`.
struct who {
    int err;
    char exe[PATH_MAX];
    char comm[32];
};

// Exec this probe as `--whoami` from a child, by `by`; the image it starts
// writes its names into a pipe, and a refused exec writes its errno there.
static struct who exec_who(enum exec_by by, int dirfd, const char *path, int flags) {
    struct who w;
    memset(&w, 0, sizeof w);
    int p[2];
    if (pipe(p)) {
        w.err = -1;
        return w;
    }
    pid_t c = fork();
    if (c == 0) {
        close(p[0]);
        char fd[16];
        snprintf(fd, sizeof fd, "%d", p[1]);
        char *args[] = {"ea-who", "--whoami", fd, NULL};
        if (by == BY_EXECVEAT)
            syscall(SYS_execveat, dirfd, path, args, environ, flags);
        else if (by == BY_FEXECVE)
            fexecve(dirfd, args, environ);
        else
            execve(path, args, environ);
        dprintf(p[1], "!%d\n", errno);
        _exit(0);
    }
    close(p[1]);
    char buf[sizeof w.exe];
    size_t len = 0;
    ssize_t n;
    while (len < sizeof buf - 1 && (n = read(p[0], buf + len, sizeof buf - 1 - len)) > 0)
        len += (size_t)n;
    buf[len] = 0;
    close(p[0]);
    status_of(c);
    char *nl = strchr(buf, '\n');
    if (buf[0] == '!' || !nl) {
        // No line at all: the image died before it could say anything.
        w.err = buf[0] == '!' ? atoi(buf + 1) : -1;
        return w;
    }
    *nl = 0;
    memcpy(w.exe, buf, (size_t)(nl - buf) + 1);
    nl[1 + strcspn(nl + 1, "\n")] = 0;
    snprintf(w.comm, sizeof w.comm, "%s", nl + 1);
    return w;
}

static void check_ran(struct who w, const char *exe, const char *comm, const char *what) {
    check(w.err == 0 && strcmp(w.exe, exe) == 0 && strcmp(w.comm, comm) == 0, what,
          w.err ? "refused: %s" : "exe %s, comm %s", w.err > 0 ? strerror(w.err) : w.exe,
          w.comm);
}

static void check_refused(struct who w, int want, const char *what) {
    check(w.err == want, what, "%s", w.err == 0 ? "it ran" : w.err > 0 ? strerror(w.err) : "died");
}

#define EA "/tmp/ea"

static void sec_execat(void) {
    // A copy of this probe where the section knows the whole path, a link
    // to it, a link to its directory, and a script it interprets.
    const char *prog = EA "/real/prog";
    mkdir(EA, 0755);
    mkdir(EA "/real", 0755);
    unlink(prog);
    int in = open("/proc/self/exe", O_RDONLY), out = open(prog, O_CREAT | O_WRONLY, 0755);
    char chunk[65536];
    ssize_t n;
    while (in >= 0 && out >= 0 && (n = read(in, chunk, sizeof chunk)) > 0)
        if (write(out, chunk, (size_t)n) != n) break;
    close(in);
    close(out);
    unlink(EA "/link");
    unlink(EA "/dlink");
    if (symlink("real/prog", EA "/link") || symlink("real", EA "/dlink")) {
        check(0, "execat setup", "symlink: %s", strerror(errno));
        return;
    }
    const char script[] = "#!" EA "/link\n";
    write_file(EA "/s.sh", script, sizeof script - 1, 0755);

    int dir = open(EA "/real", O_RDONLY | O_DIRECTORY);
    int fd = open(prog, O_RDONLY);
    int path_fd = open(prog, O_PATH);
    int cloexec_fd = open(prog, O_RDONLY | O_CLOEXEC);
    check_ran(exec_who(BY_EXECVEAT, dir, "prog", 0), prog, "prog", "execveat(dirfd, \"prog\")");
    check_ran(exec_who(BY_EXECVEAT, fd, "", AT_EMPTY_PATH), prog, "prog",
              "execveat(fd, \"\", AT_EMPTY_PATH)");
    check_ran(exec_who(BY_EXECVEAT, path_fd, "", AT_EMPTY_PATH), prog, "prog",
              "... of an O_PATH fd");
    check_ran(exec_who(BY_EXECVEAT, cloexec_fd, "", AT_EMPTY_PATH), prog, "prog",
              "... of a close-on-exec fd");
    check_ran(exec_who(BY_EXECVEAT, 999, prog, 0), prog, "prog",
              "an absolute path ignores dirfd");
    check_ran(exec_who(BY_FEXECVE, fd, NULL, 0), prog, "prog", "fexecve");
    char magic[64], fd_name[16];
    snprintf(magic, sizeof magic, "/proc/self/fd/%d", fd);
    snprintf(fd_name, sizeof fd_name, "%d", fd);
    check_ran(exec_who(BY_EXECVE, -1, magic, 0), prog, fd_name, "execve(\"/proc/self/fd/N\")");

    // /proc/self/exe is the file, from the root and through no link; comm
    // is cut from the name the caller used.
    check_ran(exec_who(BY_EXECVE, -1, EA "/link", 0), prog, "link", "through a link: the file");
    check_ran(exec_who(BY_EXECVE, -1, EA "/dlink/prog", 0), prog, "prog",
              "through a linked directory");
    if (chdir(EA "/real") == 0) {
        check_ran(exec_who(BY_EXECVE, -1, "./prog", 0), prog, "prog",
                  "execve(\"./prog\"): from the root");
        if (chdir("/")) {
        }
    }
    check_ran(exec_who(BY_EXECVE, -1, EA "/s.sh", 0), prog, "s.sh",
              "a script: own comm, interpreter exe");

    check_refused(exec_who(BY_EXECVEAT, fd, "", 0), ENOENT, "\"\" without AT_EMPTY_PATH: ENOENT");
    check_refused(exec_who(BY_EXECVEAT, dir, "prog", 0x8000), EINVAL, "an unknown flag: EINVAL");
    check_refused(exec_who(BY_EXECVEAT, AT_FDCWD, EA "/link", AT_SYMLINK_NOFOLLOW), ELOOP,
                  "NOFOLLOW on a link: ELOOP");
    check_refused(exec_who(BY_EXECVEAT, AT_FDCWD, "/proc/self/exe", AT_SYMLINK_NOFOLLOW), ELOOP,
                  "NOFOLLOW on /proc/self/exe: ELOOP");
    check_refused(exec_who(BY_EXECVEAT, fd, "x", 0), ENOTDIR, "a file as dirfd: ENOTDIR");
    check_refused(exec_who(BY_EXECVEAT, 999, "prog", 0), EBADF, "no such dirfd: EBADF");
    check_refused(exec_who(BY_EXECVEAT, 999, "", AT_EMPTY_PATH), EBADF,
                  "no such fd, AT_EMPTY_PATH: EBADF");
    check_refused(exec_who(BY_EXECVEAT, dir, "", AT_EMPTY_PATH), EACCES,
                  "a directory's fd: EACCES");
    check_refused(exec_who(BY_EXECVEAT, AT_FDCWD, "", AT_EMPTY_PATH), EACCES,
                  "AT_FDCWD itself: EACCES");
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0)
        check_refused(exec_who(BY_EXECVEAT, sv[0], "", AT_EMPTY_PATH), EACCES,
                      "a socket's fd: EACCES");
}

// ------------------------------------------------------------------ ctxsw

// The context switches getrusage reports for `who`: voluntary, nonvoluntary.
static void ru_switches(int who, long *vol, long *invol) {
    struct rusage ru;
    memset(&ru, 0, sizeof ru);
    getrusage(who, &ru);
    *vol = ru.ru_nvcsw;
    *invol = ru.ru_nivcsw;
}

static long status_long(const char *path, const char *key) {
    char v[64];
    return status_value(path, key, v, sizeof v) ? strtol(v, NULL, 10) : -1;
}

#define CTX_SLEEPS 25

static void ctx_sleeps(void) {
    for (int i = 0; i < CTX_SLEEPS; i++) sleep_ms(2);
}

static volatile pid_t g_ctx_tid;
static volatile long g_ctx_vol;
static int g_ctx_go[2];

// Sleeps, says how often it went to sleep, and waits on a pipe for leave.
static void *ctx_thread(void *arg) {
    (void)arg;
    g_ctx_tid = (pid_t)syscall(SYS_gettid);
    ctx_sleeps();
    long vol, invol;
    ru_switches(RUSAGE_THREAD, &vol, &invol);
    g_ctx_vol = vol;
    char c;
    while (read(g_ctx_go[0], &c, 1) == -1 && errno == EINTR) {
    }
    return NULL;
}

static void *ctx_sleeper(void *arg) {
    (void)arg;
    ctx_sleeps();
    return NULL;
}

static void sec_ctxsw(void) {
    const char *self = "/proc/self/status";
    long v0, i0, v1, i1;

    // Going to sleep is giving the CPU up: one voluntary switch a sleep.
    ru_switches(RUSAGE_THREAD, &v0, &i0);
    ctx_sleeps();
    ru_switches(RUSAGE_THREAD, &v1, &i1);
    check(v1 - v0 >= CTX_SLEEPS && i1 - i0 < CTX_SLEEPS, "a sleep is a voluntary switch",
          "%ld voluntary and %ld nonvoluntary for %d sleeps", v1 - v0, i1 - i0, CTX_SLEEPS);

    // The status file has both lines, and they are the thread's.
    long sv = status_long(self, "voluntary_ctxt_switches");
    long si = status_long(self, "nonvoluntary_ctxt_switches");
    ru_switches(RUSAGE_THREAD, &v1, &i1);
    check(sv >= v0 + CTX_SLEEPS && sv <= v1 && si >= 0 && si <= i1,
          "status: voluntary_ctxt_switches and nonvoluntary_ctxt_switches are the thread's",
          "status %ld/%ld, getrusage %ld/%ld", sv, si, v1, i1);

    // Another thread's file is that thread's own count; this one's does not
    // move with it.
    pthread_t t;
    if (pipe(g_ctx_go) == 0 && pthread_create(&t, NULL, ctx_thread, NULL) == 0) {
        long mine0 = status_long(self, "voluntary_ctxt_switches");
        long end = clock_ms(CLOCK_MONOTONIC) + 5000;
        while (!g_ctx_vol && clock_ms(CLOCK_MONOTONIC) < end) {
        }
        char path[64];
        snprintf(path, sizeof path, "/proc/self/task/%d/status", (int)g_ctx_tid);
        long theirs = status_long(path, "voluntary_ctxt_switches");
        long mine1 = status_long(self, "voluntary_ctxt_switches");
        check(g_ctx_vol >= CTX_SLEEPS && theirs >= g_ctx_vol && theirs <= g_ctx_vol + 3 &&
                  mine1 - mine0 < CTX_SLEEPS,
              "task/<tid>/status: that thread's switches",
              "it counted %ld, its file %ld; this thread's file moved %ld", (long)g_ctx_vol,
              theirs, mine1 - mine0);
        if (write(g_ctx_go[1], "x", 1) != 1)
            check(0, "task/<tid>/status: let it go", "%s", strerror(errno));
        pthread_join(t, NULL);
        close(g_ctx_go[0]);
        close(g_ctx_go[1]);
    }

    // RUSAGE_SELF is every thread's, those that have exited included.
    ru_switches(RUSAGE_SELF, &v0, &i0);
    if (pthread_create(&t, NULL, ctx_sleeper, NULL) == 0) pthread_join(t, NULL);
    ru_switches(RUSAGE_SELF, &v1, &i1);
    check(v1 - v0 >= CTX_SLEEPS, "RUSAGE_SELF: a thread's switches, after it is gone",
          "%ld voluntary for its %d sleeps", v1 - v0, CTX_SLEEPS);

    // wait4 hands out the child's, with its own reaped children's; and
    // RUSAGE_CHILDREN adds them up.
    ru_switches(RUSAGE_CHILDREN, &v0, &i0);
    pid_t pid = fork();
    if (pid == 0) {
        pid_t grand = fork();
        if (grand == 0) {
            ctx_sleeps();
            _exit(0);
        }
        status_of(grand);
        ctx_sleeps();
        _exit(0);
    }
    struct rusage ru;
    memset(&ru, 0, sizeof ru);
    int status;
    pid_t got = wait4(pid, &status, 0, &ru);
    ru_switches(RUSAGE_CHILDREN, &v1, &i1);
    check(got == pid && ru.ru_nvcsw >= 2 * CTX_SLEEPS,
          "wait4: the child's switches and its children's", "%ld voluntary for %d sleeps",
          ru.ru_nvcsw, 2 * CTX_SLEEPS);
    check(v1 - v0 >= 2 * CTX_SLEEPS && v1 - v0 <= ru.ru_nvcsw + 2,
          "RUSAGE_CHILDREN: what wait4 handed out", "%ld voluntary, wait4 said %ld", v1 - v0,
          ru.ru_nvcsw);

    // Two processes that never sleep, on one CPU: each takes the CPU from
    // the other, and that is a nonvoluntary switch.
    cpu_set_t one, before;
    CPU_ZERO(&one);
    CPU_SET(0, &one);
    sched_getaffinity(0, sizeof before, &before);
    sched_setaffinity(0, sizeof one, &one);
    pid = fork();
    if (pid == 0) {
        clk_burn(300, 3000);
        _exit(0);
    }
    ru_switches(RUSAGE_THREAD, &v0, &i0);
    clk_burn(300, 3000);
    ru_switches(RUSAGE_THREAD, &v1, &i1);
    memset(&ru, 0, sizeof ru);
    got = wait4(pid, &status, 0, &ru);
    sched_setaffinity(0, sizeof before, &before);
    check(i1 - i0 >= 2 && got == pid && ru.ru_nivcsw >= 2,
          "sharing a CPU: nonvoluntary switches, both sides",
          "this one %ld nonvoluntary (%ld voluntary), the child %ld", i1 - i0, v1 - v0,
          ru.ru_nivcsw);
}

// ------------------------------------------------------------------- main

static const struct {
    const char *name;
    const char *what;
    void (*fn)(void);
} SECTIONS[] = {
    {"nest", "a handler inside a handler comes back, with its registers", sec_nest},
    {"longjmp", "faults after siglongjmp, and inside another handler", sec_longjmp},
    {"fork", "a child forked inside a handler returns from it", sec_fork},
    {"restart", "SA_RESTART: what Linux restarts and what it does not", sec_restart},
    {"badrip", "a non-canonical rip from a handler is a SIGSEGV", sec_badrip},
    {"ids", "what the kernel must refuse", sec_ids},
    {"orphan", "an orphan's death tells its adopter, with SIGCHLD", sec_orphan},
    {"inherit", "what fork and execve hand down", sec_inherit},
    {"wait", "what waitid and waitpid report", sec_wait},
    {"stop", "a stop reaches a process blocked in a syscall", sec_stop},
    {"teardown", "an exit ends every thread, whatever it waits in", sec_teardown},
    {"spawn", "a vfork child runs in its parent's memory", sec_spawn},
    {"badexec", "a failed execve returns, with Linux's errno", sec_badexec},
    {"sched", "what a fork hands down to the scheduler", sec_sched},
    {"image", "the program is not mapped at the null page", sec_image},
    {"cputime", "what getrusage, times, wait4 and the CPU clocks count", sec_cputime},
    {"fsize", "RLIMIT_FSIZE holds every write to a file, with SIGXFSZ", sec_fsize},
    {"cputimer", "ITIMER_VIRTUAL, ITIMER_PROF and RLIMIT_CPU count CPU time", sec_cputimer},
    {"cpuclock", "a timer_create timer on a CPU clock counts that clock", sec_cpuclock},
    {"aslimit", "RLIMIT_AS holds mmap, brk, mremap and shmat", sec_aslimit},
    {"nproc", "RLIMIT_NPROC holds fork and pthread_create", sec_nproc},
    {"zombie", "a dead child is a pid until it is reaped, and not after", sec_zombie},
    {"kin", "what a new thread or child is named, and when its parent lets go", sec_kin},
    {"status", "/proc/<pid>/status: Linux's lines, with the task's own numbers", sec_status},
    {"execat", "what execveat runs, and what an exec calls the program", sec_execat},
    {"ctxsw", "the context switches getrusage, wait4 and /proc count", sec_ctxsw},
};

int main(int argc, char **argv) {
    const char *only = NULL;
    if (argc > 0) g_self = argv[0];
    if (argc == 2 && strcmp(argv[1], "--exec-report") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return exec_report();
    }
    if (argc == 2 && strcmp(argv[1], "--exec-from-thread") == 0) return exec_from_thread_report();
    // Last, after the script a `#!` line put in front of it.
    if (argc >= 3 && strcmp(argv[argc - 2], "--whoami") == 0)
        return whoami_report(atoi(argv[argc - 1]));
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--only") == 0 && i + 1 < argc) {
            only = argv[++i];
        } else {
            fprintf(stderr, "usage: %s [--only SECTION]\n", argv[0]);
            return 2;
        }
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    int ran = 0;
    for (size_t i = 0; i < sizeof SECTIONS / sizeof SECTIONS[0]; i++) {
        if (only && strcmp(only, SECTIONS[i].name) != 0) continue;
        ran++;
        printf("\n== %s -- %s\n", SECTIONS[i].name, SECTIONS[i].what);
        fflush(stdout);
        pid_t pid = fork();
        if (pid == 0) {
            // The count of THIS section: the child starts with the parent's
            // running total, and handing that back would count it twice.
            g_fail = 0;
            // A section that hangs is a FAIL, not a hung probe.
            alarm(20);
            SECTIONS[i].fn();
            fflush(stdout);
            _exit(g_fail > 100 ? 100 : g_fail);
        }
        int status = 0;
        while (waitpid(pid, &status, 0) == -1 && errno == EINTR) {
        }
        if (WIFEXITED(status)) {
            g_fail += WEXITSTATUS(status);
        } else {
            int sig = WIFSIGNALED(status) ? WTERMSIG(status) : 0;
            check(0, SECTIONS[i].name, "the section's process died of signal %d (%s)", sig,
                  strsignal(sig));
        }
    }
    if (!ran) {
        fprintf(stderr, "no section named %s\n", only);
        return 2;
    }
    printf("\n%d failure(s)\n", g_fail);
    return g_fail ? 1 : 0;
}
