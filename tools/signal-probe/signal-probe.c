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
//
// Build:  make                 (x86_64-linux-musl-gcc -O2 -static)
// Run:    ./signal-probe [--only SECTION]
// Exit:   0 = every check held, 1 = at least one FAIL, 2 = bad usage.
//
// It is also meant to be run on Linux on the same box: every check here is
// one Linux holds, so a FAIL on Linux is a bug in the probe and a FAIL only on
// Eclipse is a bug in Eclipse.

#define _GNU_SOURCE
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
#include <sys/stat.h>
#include <sys/syscall.h>
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
};

int main(int argc, char **argv) {
    const char *only = NULL;
    if (argc > 0) g_self = argv[0];
    if (argc == 2 && strcmp(argv[1], "--exec-report") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return exec_report();
    }
    if (argc == 2 && strcmp(argv[1], "--exec-from-thread") == 0) return exec_from_thread_report();
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
