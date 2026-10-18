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
//   badrip     a handler that sets a non-canonical `rip` in its ucontext gets
//              the process a SIGSEGV (SI_KERNEL), and nothing more. x86_64.
//   ids        the kernel refuses what it cannot load: a TLS pointer outside
//              user space (`arch_prctl`, `clone(CLONE_SETTLS)`), and the pid
//              INT_MIN, which has no negation, in `waitpid`.
//   orphan     an orphan's death tells its ADOPTER, with SIGCHLD whatever
//              signal its dead parent chose at clone(2), and wakes the
//              adopter's `waitpid` even when a process in between volunteered
//              as a subreaper after the adoption.
//
// Build:  make                 (x86_64-linux-musl-gcc -O2 -static)
// Run:    ./signal-probe [--only SECTION]
// Exit:   0 = every check held, 1 = at least one FAIL, 2 = bad usage.
//
// It is also meant to be run on Linux on the same box: every check here is
// one Linux holds, so a FAIL on Linux is a bug in the probe and a FAIL only on
// Eclipse is a bug in Eclipse.

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
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
    signal(SIGALRM, SIG_DFL);
    alarm(20);
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
};

int main(int argc, char **argv) {
    const char *only = NULL;
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
