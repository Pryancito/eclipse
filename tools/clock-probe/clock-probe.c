// Eclipse OS: an integrity probe for the clocks and the timers.
//
// `eclipse-bench` answers "how fast is a clock read?" and `audio-probe`/
// `drm-probe` answer "which layer is broken?". Neither can answer the question
// this tool exists for: **is the time the kernel reports actually true?** A
// clock has the same failure mode sound does — every call returns 0 and the
// answer is wrong — and in this kernel that failure mode has already happened
// three times:
//
//   * `timer_now()` scaled the *absolute* TSC with no per-boot base, so uptime
//     was the time since the machine was last powered on (8.2 days on the
//     reporter's box) while time *advanced* correctly on top of it. Every
//     `clock_gettime` returned success. (#1613)
//   * the vDSO published the multiplier without that base, which is the same
//     bug again but in userspace only: the two clocks of one machine disagreed
//     by days, and only a program that read both could see it.
//   * `timerfd_settime(TFD_TIMER_ABSTIME)` and `timer_settime(TIMER_ABSTIME)`
//     handed a caller's *wall-clock date* to the kernel timer as a *monotonic
//     distance*: seconds since 1970 read as nanoseconds since boot, i.e. a
//     timer armed half a century out. `timerfd_settime` returned 0 and the
//     timer simply never fired.
//
// None of the three is visible in a return value, a speed figure, or a layer
// that breaks. All three are visible the moment two clocks are read against
// each other, which is all this probe does: it states an invariant, measures
// it, and prints the numbers whether it passes or fails.
//
// The sections, and the invariant each one holds the kernel to:
//
//   res        every clock `clock_getres` admits to can be read, its
//              resolution is sane, and `tv_nsec` is normalised (< 1e9) --
//              a denormal timespec is a `struct timespec` the C library
//              will turn into nonsense.
//   mono       the monotonic clocks never go backwards, over hundreds of
//              thousands of back-to-back reads.
//   vdso       the userspace clock and the kernel's are THE SAME CLOCK: a
//              vDSO read taken between two raw syscalls lies between them.
//              This is the only check that can catch the published-base bug,
//              and it catches it by nanoseconds, not by days.
//   offset     CLOCK_REALTIME minus CLOCK_MONOTONIC holds still. If it
//              drifts, the two clocks are running at different rates and one
//              of them is wrong.
//   coarse     the _COARSE clocks agree with their fine counterparts to
//              within their own advertised resolution.
//   uptime     CLOCK_BOOTTIME agrees with /proc/uptime, and is never behind
//              CLOCK_MONOTONIC. The #1613 bug is one subtraction away here.
//   epoch      CLOCK_REALTIME is a plausible date, and `time()` /
//              `gettimeofday()` agree with it -- the vDSO serves all three
//              from one data page, so they can disagree.
//   cpu        the CPU clocks advance when the process burns CPU, do not
//              advance while it sleeps, and the per-thread clock never
//              exceeds the per-process one.
//   affinity   the monotonic clock does not go backwards when the reader
//              moves between CPUs (unsynchronised TSCs; the kernel's
//              cross-CPU floor; a vDSO still answering after the floor was
//              demoted).
//   rate       over a wall-clock second, monotonic, realtime, boottime and
//              /proc/uptime all advance by the same amount. A wrong TSC
//              frequency shows up here and nowhere else.
//   sleep      no sleep ever returns early: `nanosleep`, and
//              `clock_nanosleep` with TIMER_ABSTIME on both clocks.
//   timerfd    a timerfd fires once per expiry, never early, loses no
//              expiration when periodic, and an absolute wall-clock deadline
//              is a date and not a distance.
//   timer      `timer_create` the same, plus `timer_gettime`'s remaining
//              time never exceeds what was armed and counts down.
//   itimer     `setitimer(ITIMER_REAL)` fires, and not early.
//   timeout    the blocking calls that take a timeout honour it as a floor:
//              poll, ppoll, select, epoll_wait, sem_timedwait,
//              pthread_cond_timedwait.
//
// Everything here is read-only with respect to the system clock: the probe
// never calls `clock_settime`, so it is safe on a machine doing real work.
//
// Build:  make                 (x86_64-linux-musl-gcc -O2 -static -pthread)
// Run:    ./clock-probe [--only SECTION] [--quick] [-v]
// Exit:   0 = every invariant held, 1 = at least one FAIL, 2 = bad usage.
//
// It is also meant to be run on Linux on the same box: every invariant here is
// one Linux holds, so a FAIL on Linux is a bug in the probe and a FAIL only on
// Eclipse is a bug in Eclipse.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

// Clock ids. Named here rather than trusted from the headers: the probe has to
// be able to ask for a clock the C library has never heard of, and to report
// the *number* when the name is what is in doubt.
#define CLK_REALTIME 0
#define CLK_MONOTONIC 1
#define CLK_PROCESS_CPUTIME 2
#define CLK_THREAD_CPUTIME 3
#define CLK_MONOTONIC_RAW 4
#define CLK_REALTIME_COARSE 5
#define CLK_MONOTONIC_COARSE 6
#define CLK_BOOTTIME 7
#define CLK_REALTIME_ALARM 8
#define CLK_BOOTTIME_ALARM 9
#define CLK_TAI 11

#define NS_PER_SEC 1000000000LL

// ---------------------------------------------------------------- reporting

static int g_pass, g_fail, g_skip;
static int g_verbose;

static void ok(const char *what, const char *fmt, ...) {
    va_list ap;
    printf("  PASS  %-34s ", what);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
    g_pass++;
}

static void bad(const char *what, const char *fmt, ...) {
    va_list ap;
    printf("  FAIL  %-34s ", what);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
    g_fail++;
}

static void skip(const char *what, const char *fmt, ...) {
    va_list ap;
    printf("  SKIP  %-34s ", what);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
    g_skip++;
}

// A measurement that is evidence rather than a verdict. Printed always: a
// number nobody looked at is the reason a wrong clock survives a test suite.
static void note(const char *fmt, ...) {
    va_list ap;
    printf("        ");
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void vnote(const char *fmt, ...) {
    va_list ap;
    if (!g_verbose) return;
    printf("        ");
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
}

static void section(const char *name, const char *what) {
    printf("\n== %s -- %s\n", name, what);
    fflush(stdout);
}

// `cond` holds, or it does not; either way the caller's numbers get printed.
static void check(int cond, const char *what, const char *fmt, ...) {
    va_list ap;
    printf("  %s  %-34s ", cond ? "PASS" : "FAIL", what);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
    if (cond) g_pass++; else g_fail++;
}

// ------------------------------------------------------------- clock reading

static int64_t ts_ns(const struct timespec *ts) {
    return (int64_t)ts->tv_sec * NS_PER_SEC + ts->tv_nsec;
}

static void ns_ts(int64_t ns, struct timespec *ts) {
    ts->tv_sec = (time_t)(ns / NS_PER_SEC);
    ts->tv_nsec = (long)(ns % NS_PER_SEC);
}

// The C library's read: through the vDSO where there is one.
static int64_t clk_ns(int clk) {
    struct timespec ts;
    if (clock_gettime((clockid_t)clk, &ts) != 0) return -1;
    return ts_ns(&ts);
}

// The kernel's own answer, with the vDSO deliberately bypassed. Without this
// there is no way to tell the two clocks apart, and telling them apart is the
// whole point of the `vdso` section.
#if defined(SYS_clock_gettime)
static int raw_clock_gettime(int clk, struct timespec *ts) {
    long r = syscall(SYS_clock_gettime, (long)clk, ts);
    return r == 0 ? 0 : -1;
}
#define HAVE_RAW_CLOCK_GETTIME 1
#else
#define HAVE_RAW_CLOCK_GETTIME 0
static int raw_clock_gettime(int clk, struct timespec *ts) {
    (void)clk; (void)ts; errno = ENOSYS; return -1;
}
#endif

static int64_t raw_clk_ns(int clk) {
    struct timespec ts;
    if (raw_clock_gettime(clk, &ts) != 0) return -1;
    return ts_ns(&ts);
}

// Monotonic nanoseconds, for measuring the probe's own waits. Always read
// through the syscall: a section that is testing the vDSO must not time itself
// with it.
static int64_t mono_raw(void) {
    struct timespec ts;
    if (HAVE_RAW_CLOCK_GETTIME && raw_clock_gettime(CLK_MONOTONIC, &ts) == 0) return ts_ns(&ts);
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts_ns(&ts);
}

static const char *clk_name(int clk) {
    switch (clk) {
    case CLK_REALTIME: return "CLOCK_REALTIME";
    case CLK_MONOTONIC: return "CLOCK_MONOTONIC";
    case CLK_PROCESS_CPUTIME: return "CLOCK_PROCESS_CPUTIME_ID";
    case CLK_THREAD_CPUTIME: return "CLOCK_THREAD_CPUTIME_ID";
    case CLK_MONOTONIC_RAW: return "CLOCK_MONOTONIC_RAW";
    case CLK_REALTIME_COARSE: return "CLOCK_REALTIME_COARSE";
    case CLK_MONOTONIC_COARSE: return "CLOCK_MONOTONIC_COARSE";
    case CLK_BOOTTIME: return "CLOCK_BOOTTIME";
    case CLK_REALTIME_ALARM: return "CLOCK_REALTIME_ALARM";
    case CLK_BOOTTIME_ALARM: return "CLOCK_BOOTTIME_ALARM";
    case CLK_TAI: return "CLOCK_TAI";
    default: return "CLOCK_?";
    }
}

// Every clock this probe knows how to ask for, and whether the probe insists
// it exist. The _ALARM clocks need CAP_WAKE_ALARM to *arm*, but reading them
// is unprivileged in Linux, so they are checked and not required.
struct clock_entry {
    int id;
    int required;
    int monotonic;   // must never go backwards
};

static const struct clock_entry CLOCKS[] = {
    { CLK_REALTIME,          1, 0 },
    { CLK_MONOTONIC,         1, 1 },
    { CLK_PROCESS_CPUTIME,   1, 1 },
    { CLK_THREAD_CPUTIME,    1, 1 },
    { CLK_MONOTONIC_RAW,     1, 1 },
    { CLK_REALTIME_COARSE,   1, 0 },
    { CLK_MONOTONIC_COARSE,  1, 1 },
    { CLK_BOOTTIME,          1, 1 },
    { CLK_REALTIME_ALARM,    0, 0 },
    { CLK_BOOTTIME_ALARM,    0, 1 },
    { CLK_TAI,               0, 0 },
};
#define N_CLOCKS ((int)(sizeof(CLOCKS) / sizeof(CLOCKS[0])))

// ---------------------------------------------------------- interrupt guard
//
// Two of the bugs in the header leave a call that never returns: a wall-clock
// date armed as a monotonic distance is a timer fifty years out, and the
// process simply blocks forever. A probe that hangs there reports nothing at
// all, which is strictly worse than reporting the failure, so every blocking
// call with an absolute deadline runs under this guard: a watchdog thread
// sleeps for a budget and then signals the waiting thread until it comes back.
//
// The signal handler is installed WITHOUT SA_RESTART on purpose -- the point is
// to make the blocked syscall return EINTR -- and it does nothing but count,
// so a spurious one is harmless.

static volatile sig_atomic_t g_pokes;

static void poke_handler(int sig) {
    (void)sig;
    g_pokes++;
}

struct guard {
    pthread_t watchdog;
    pthread_t target;
    int budget_ms;
    volatile int done;
    int running;
};

static void sleep_ms(long ms) {
    struct timespec req;
    req.tv_sec = ms / 1000;
    req.tv_nsec = (ms % 1000) * 1000000L;
    while (nanosleep(&req, &req) != 0 && errno == EINTR) {
    }
}

static void *guard_thread(void *arg) {
    struct guard *g = (struct guard *)arg;
    long waited = 0;
    while (!g->done && waited < g->budget_ms) {
        sleep_ms(10);
        waited += 10;
    }
    // Keep poking: one EINTR may land inside a libc retry loop rather than in
    // the caller, and `sem_timedwait` and friends restart themselves.
    while (!g->done) {
        pthread_kill(g->target, SIGUSR1);
        sleep_ms(20);
    }
    return NULL;
}

static void guard_start(struct guard *g, int budget_ms) {
    g->target = pthread_self();
    g->budget_ms = budget_ms;
    g->done = 0;
    g->running = pthread_create(&g->watchdog, NULL, guard_thread, g) == 0;
}

// Returns non-zero when the guard had to intervene, i.e. the call under test
// did not come back inside its budget.
static int guard_stop(struct guard *g) {
    int poked;
    if (!g->running) return 0;
    g->done = 1;
    pthread_join(g->watchdog, NULL);
    poked = g_pokes != 0;
    g_pokes = 0;
    return poked;
}

// ------------------------------------------------------------------ res

static void sec_res(void) {
    int i;
    section("res", "every clock reads, and reads a normalised timespec");

    for (i = 0; i < N_CLOCKS; i++) {
        const struct clock_entry *c = &CLOCKS[i];
        struct timespec res, now;
        int have_res = clock_getres((clockid_t)c->id, &res) == 0;
        int res_err = errno;
        int have_now = clock_gettime((clockid_t)c->id, &now) == 0;
        int now_err = errno;
        char label[64];

        snprintf(label, sizeof label, "%s", clk_name(c->id));

        if (!have_res && !have_now) {
            if (c->required)
                bad(label, "no existe: getres %s, gettime %s", strerror(res_err),
                    strerror(now_err));
            else
                skip(label, "no existe (%s); opcional", strerror(now_err));
            continue;
        }

        // The invariant: a clock cannot be half there. `clock_getres` is how a
        // C library decides whether to use a clock at all, so one that admits
        // to a resolution and then refuses to be read is a clock every program
        // will try and every program will fail on.
        if (have_res != have_now) {
            bad(label, "medio presente: getres %s, gettime %s",
                have_res ? "ok" : strerror(res_err), have_now ? "ok" : strerror(now_err));
            continue;
        }

        if (res.tv_sec < 0 || res.tv_nsec < 0 || res.tv_nsec >= NS_PER_SEC) {
            bad(label, "resolucion denormal: %lld s + %ld ns", (long long)res.tv_sec,
                res.tv_nsec);
            continue;
        }
        if (ts_ns(&res) <= 0) {
            bad(label, "resolucion cero: un programa que divida por ella se rompe");
            continue;
        }
        if (ts_ns(&res) > NS_PER_SEC) {
            bad(label, "resolucion absurda: %lld ns (> 1 s)", (long long)ts_ns(&res));
            continue;
        }
        // A denormalised `tv_nsec` is the field bug that survives every test
        // written in nanoseconds: `ts_ns()` above is happy with 1_500_000_000
        // and `ctime()` is not.
        if (now.tv_sec < 0 || now.tv_nsec < 0 || now.tv_nsec >= NS_PER_SEC) {
            bad(label, "lectura denormal: %lld s + %ld ns", (long long)now.tv_sec,
                now.tv_nsec);
            continue;
        }
        ok(label, "res %lld ns, ahora %lld.%09ld", (long long)ts_ns(&res),
           (long long)now.tv_sec, now.tv_nsec);
    }
}

// ------------------------------------------------------------------ mono

static void sec_mono(int quick) {
    int i;
    long reads = quick ? 50000 : 400000;
    section("mono", "the monotonic clocks never go backwards");

    for (i = 0; i < N_CLOCKS; i++) {
        const struct clock_entry *c = &CLOCKS[i];
        int64_t prev, worst_back = 0, biggest_step = 0;
        long back_count = 0, n;
        char label[72];

        if (!c->monotonic) continue;
        if (clk_ns(c->id) < 0) {
            if (c->required) bad(clk_name(c->id), "no se puede leer: %s", strerror(errno));
            continue;
        }
        snprintf(label, sizeof label, "%s no retrocede", clk_name(c->id));

        prev = clk_ns(c->id);
        for (n = 0; n < reads; n++) {
            int64_t now = clk_ns(c->id);
            int64_t d = now - prev;
            if (d < 0) {
                back_count++;
                if (-d > worst_back) worst_back = -d;
            } else if (d > biggest_step) {
                biggest_step = d;
            }
            prev = now;
        }
        if (back_count)
            bad(label, "%ld retrocesos en %ld lecturas, el peor de %lld ns", back_count, reads,
                (long long)worst_back);
        else
            ok(label, "%ld lecturas, salto maximo %lld ns", reads, (long long)biggest_step);
    }
}

// ------------------------------------------------------------------ vdso

static void sec_vdso(int quick) {
    unsigned long ehdr;
    long rounds = quick ? 20000 : 100000;
    int clocks[] = { CLK_REALTIME, CLK_MONOTONIC, CLK_MONOTONIC_RAW, CLK_BOOTTIME };
    int i;
    section("vdso", "the userspace clock and the kernel's are the same clock");

    if (!HAVE_RAW_CLOCK_GETTIME) {
        skip("vdso", "esta arquitectura no tiene SYS_clock_gettime para comparar");
        return;
    }

    ehdr = getauxval(AT_SYSINFO_EHDR);
    if (ehdr)
        note("AT_SYSINFO_EHDR = %#lx (el kernel ofrece una vDSO)", ehdr);
    else
        note("AT_SYSINFO_EHDR ausente: cada clock_gettime es un syscall");

    for (i = 0; i < (int)(sizeof clocks / sizeof clocks[0]); i++) {
        int clk = clocks[i];
        int64_t worst_low = 0, worst_high = 0, span_max = 0;
        long out = 0, n;
        char label[72];
        struct timespec ts;

        if (raw_clock_gettime(clk, &ts) != 0) {
            skip(clk_name(clk), "el syscall no la sirve: %s", strerror(errno));
            continue;
        }
        if (clk_ns(clk) < 0) {
            bad(clk_name(clk), "el syscall la sirve y la libc no: %s", strerror(errno));
            continue;
        }
        snprintf(label, sizeof label, "%s vDSO entre syscalls", clk_name(clk));

        // The sandwich. `before` and `after` are the kernel's own answers
        // either side of the library's, so the library's has exactly one place
        // it can legitimately be. A base that is published wrong puts it days
        // outside; a multiplier that is published wrong walks it out of the
        // sandwich as the boot gets older.
        for (n = 0; n < rounds; n++) {
            int64_t before, mid, after;
            before = raw_clk_ns(clk);
            mid = clk_ns(clk);
            after = raw_clk_ns(clk);
            if (before < 0 || mid < 0 || after < 0) {
                bad(label, "una lectura fallo a mitad: %s", strerror(errno));
                out = -1;
                break;
            }
            if (after - before > span_max) span_max = after - before;
            if (mid < before) {
                out++;
                if (before - mid > worst_low) worst_low = before - mid;
            } else if (mid > after) {
                out++;
                if (mid - after > worst_high) worst_high = mid - after;
            }
        }
        if (out < 0) continue;
        if (out)
            bad(label, "%ld de %ld fuera: hasta %lld ns por detras, %lld ns por delante", out,
                rounds, (long long)worst_low, (long long)worst_high);
        else
            ok(label, "%ld rondas dentro, ventana maxima %lld ns", rounds, (long long)span_max);
    }

    // Not an invariant, evidence: if the library call is not cheaper than the
    // syscall, the vDSO is not being used, and then the section above passed
    // because it compared the syscall with itself.
    {
        int64_t t0, t1, lib_ns, sys_ns;
        long n, iters = quick ? 100000 : 400000;
        struct timespec ts;
        t0 = mono_raw();
        for (n = 0; n < iters; n++) clock_gettime(CLOCK_MONOTONIC, &ts);
        t1 = mono_raw();
        lib_ns = (t1 - t0) / iters;
        t0 = mono_raw();
        for (n = 0; n < iters; n++) raw_clock_gettime(CLK_MONOTONIC, &ts);
        t1 = mono_raw();
        sys_ns = (t1 - t0) / iters;
        note("clock_gettime: %lld ns por la libc, %lld ns por syscall -> %s",
             (long long)lib_ns, (long long)sys_ns,
             lib_ns * 2 < sys_ns ? "la vDSO esta sirviendo las lecturas"
                                 : "las lecturas van por syscall");
    }
}

// ------------------------------------------------------------------ offset

static void sec_offset(int quick) {
    long n, samples = quick ? 2000 : 20000;
    int64_t lo = INT64_MAX, hi = INT64_MIN;
    section("offset", "CLOCK_REALTIME minus CLOCK_MONOTONIC holds still");

    for (n = 0; n < samples; n++) {
        int64_t m0 = clk_ns(CLK_MONOTONIC);
        int64_t w = clk_ns(CLK_REALTIME);
        int64_t m1 = clk_ns(CLK_MONOTONIC);
        int64_t off;
        if (m0 < 0 || w < 0 || m1 < 0) {
            bad("offset", "una lectura fallo: %s", strerror(errno));
            return;
        }
        // Bracket the wall read with two monotonic ones so the sampling cost
        // itself cannot masquerade as drift.
        off = w - (m0 + m1) / 2;
        if (off < lo) lo = off;
        if (off > hi) hi = off;
    }
    // The spread is what the sampling cost and one tick of granularity buy;
    // anything past a millisecond is the two clocks running at different
    // rates, which is a wrong multiplier on one of them.
    check(hi - lo < 1000000, "el desfase pared-monotono es fijo",
          "%ld muestras, dispersion %lld ns, desfase %.3f s", samples, (long long)(hi - lo),
          (double)lo / 1e9);
}

// ------------------------------------------------------------------ coarse

static void sec_coarse(void) {
    struct {
        int coarse, fine;
    } pairs[] = {
        { CLK_REALTIME_COARSE, CLK_REALTIME },
        { CLK_MONOTONIC_COARSE, CLK_MONOTONIC },
    };
    int i;
    section("coarse", "the coarse clocks agree with the fine ones");

    for (i = 0; i < 2; i++) {
        struct timespec res;
        int64_t res_ns, worst_behind = 0, worst_ahead = 0;
        long n;
        char label[72];

        if (clk_ns(pairs[i].coarse) < 0) {
            skip(clk_name(pairs[i].coarse), "no existe: %s", strerror(errno));
            continue;
        }
        if (clock_getres((clockid_t)pairs[i].coarse, &res) != 0) res.tv_sec = 0, res.tv_nsec = 4000000;
        res_ns = ts_ns(&res);
        snprintf(label, sizeof label, "%s dentro de su resolucion", clk_name(pairs[i].coarse));

        for (n = 0; n < 2000; n++) {
            int64_t c = clk_ns(pairs[i].coarse);
            int64_t f = clk_ns(pairs[i].fine);
            if (f - c > worst_behind) worst_behind = f - c;
            if (c - f > worst_ahead) worst_ahead = c - f;
        }
        // A coarse clock is the last tick's reading, so it may be up to a tick
        // behind. Ahead is a different matter: it means the two are not the
        // same clock rounded, which is how a coarse clock served from a
        // separate counter shows up.
        if (worst_ahead > res_ns)
            bad(label, "adelanta %lld ns con resolucion %lld ns", (long long)worst_ahead,
                (long long)res_ns);
        else if (worst_behind > 8 * res_ns + 20000000)
            bad(label, "atrasa %lld ns con resolucion %lld ns", (long long)worst_behind,
                (long long)res_ns);
        else
            ok(label, "res %lld ns, atrasa hasta %lld, adelanta hasta %lld", (long long)res_ns,
               (long long)worst_behind, (long long)worst_ahead);
    }
}

// ------------------------------------------------------------------ uptime

static int read_proc_uptime(double *up) {
    FILE *f = fopen("/proc/uptime", "r");
    double a = 0, b = 0;
    int got;
    if (!f) return -1;
    got = fscanf(f, "%lf %lf", &a, &b);
    fclose(f);
    if (got < 1) return -1;
    *up = a;
    return 0;
}

static void sec_uptime(void) {
    int64_t boot, mono;
    double up;
    section("uptime", "boot time, monotonic time and /proc/uptime are one number");

    mono = clk_ns(CLK_MONOTONIC);
    boot = clk_ns(CLK_BOOTTIME);
    if (boot < 0) {
        skip("CLOCK_BOOTTIME", "no existe: %s", strerror(errno));
    } else {
        // No suspend in this kernel, so boot time IS monotonic time; it may
        // only ever be ahead, and only by the gap between the two reads.
        check(boot >= mono - 1000000, "CLOCK_BOOTTIME no va por detras del monotono",
              "boottime %.6f s, monotonic %.6f s, diferencia %lld ns", (double)boot / 1e9,
              (double)mono / 1e9, (long long)(boot - mono));
    }

    if (read_proc_uptime(&up) != 0) {
        skip("/proc/uptime", "no se puede leer: %s", strerror(errno));
        return;
    }
    // This is the check that would have caught #1613 from a shell: the kernel's
    // uptime and the clock userspace reads came from the same counter but not
    // from the same base, and they disagreed by 8.2 days while every
    // individual reading looked fine.
    {
        int64_t ref = boot >= 0 ? boot : mono;
        double diff = up - (double)ref / 1e9;
        check(diff > -2.0 && diff < 2.0, "/proc/uptime coincide con el reloj",
              "/proc/uptime %.3f s, %s %.3f s, diferencia %.3f s", up,
              boot >= 0 ? "boottime" : "monotonic", (double)ref / 1e9, diff);
    }
    // Y lo que esta seccion NO puede afirmar, dicho donde se lee. Un error en
    // la BASE del contador desplaza por igual al monotono, al boottime, a
    // /proc/uptime y al `idle` de /proc/stat, porque los cuatro salen del mismo
    // `timer_now()`: ninguna comparacion entre ellos lo ve. Lo unico que queda
    // es que alguien sepa cuanto lleva encendida la maquina, y eso es un dato
    // que vive fuera de ella. Asi que el aviso, que no falla nunca y se lee en
    // un segundo: el #1613 se reporto como «un dmesg que empieza en 8,2 dias».
    if (up > 86400.0)
        note("AVISO: la maquina dice llevar %.2f dias encendida. Si no es cierto, "
             "el contador se esta escalando sin base (el fallo #1613) y todos los "
             "relojes de arriba estan de acuerdo en el mismo error.",
             up / 86400.0);
}

// ------------------------------------------------------------------ epoch

static void sec_epoch(void) {
    int64_t wall = clk_ns(CLK_REALTIME);
    struct timeval tv;
    time_t t;
    section("epoch", "the wall clock is a date, and every way of asking agrees");

    // 2024-01-01 .. 2100-01-01. A clock that says 1970 is a clock nobody set;
    // one that says 2174 is nanoseconds read as seconds, or the other way
    // round, which is exactly the shape of the ABSTIME bug.
    check(wall > 1704067200LL * NS_PER_SEC && wall < 4102444800LL * NS_PER_SEC,
          "CLOCK_REALTIME es una fecha plausible", "%.3f s desde la epoca (%lld)",
          (double)wall / 1e9, (long long)(wall / NS_PER_SEC));

    if (gettimeofday(&tv, NULL) != 0) {
        bad("gettimeofday", "fallo: %s", strerror(errno));
    } else {
        int64_t gtod = (int64_t)tv.tv_sec * NS_PER_SEC + (int64_t)tv.tv_usec * 1000;
        int64_t after = clk_ns(CLK_REALTIME);
        int denorm = tv.tv_usec < 0 || tv.tv_usec >= 1000000;
        // The vDSO serves clock_gettime, gettimeofday and time from one data
        // page with three different pieces of arithmetic on top. They can, and
        // once did, disagree.
        // Y la ventana se imprime al lado del desvio, porque sin ella el
        // numero no decide nada: un `gettimeofday` 4,6 ms por delante de la
        // lectura anterior es el fallo que esta seccion busca si las dos
        // lecturas fueron seguidas, y no es nada si entre ellas pasaron 4,6 ms
        // de verdad (un syscall de TCG, una preempcion). Con las dos cifras se
        // lee de un golpe cual de las dos cosas paso.
        check(!denorm && gtod >= wall - 1000000 && gtod <= after + 1000000,
              "gettimeofday coincide con clock_gettime",
              "%s%+lld us respecto a la lectura anterior, en una ventana de %lld us",
              denorm ? "tv_usec denormal, " : "", (long long)((gtod - wall) / 1000),
              (long long)((after - wall) / 1000));
    }

    t = time(NULL);
    {
        int64_t after = clk_ns(CLK_REALTIME);
        check((int64_t)t >= wall / NS_PER_SEC - 1 && (int64_t)t <= after / NS_PER_SEC + 1,
              "time() coincide con clock_gettime", "time() %lld, clock %lld", (long long)t,
              (long long)(wall / NS_PER_SEC));
    }
}

// ------------------------------------------------------------------ cpu

static void burn_ms(long ms) {
    int64_t end = mono_raw() + (int64_t)ms * 1000000;
    volatile unsigned long x = 0;
    while (mono_raw() < end) {
        long i;
        for (i = 0; i < 1000; i++) x += i;
    }
}

static void sec_cpu(int quick) {
    long burn = quick ? 60 : 150;
    long nap = quick ? 60 : 150;
    int64_t p0, t0, p1, t1, wall0, wall1;
    section("cpu", "the CPU clocks follow the CPU this process actually burns");

    p0 = clk_ns(CLK_PROCESS_CPUTIME);
    t0 = clk_ns(CLK_THREAD_CPUTIME);
    if (p0 < 0 || t0 < 0) {
        // Both were EINVAL in this kernel once, which is `clock()` returning
        // -1 to every program in the distribution.
        bad("relojes de CPU", "no se pueden leer: proceso %s, hilo %s",
            p0 < 0 ? strerror(errno) : "ok", t0 < 0 ? strerror(errno) : "ok");
        return;
    }

    wall0 = mono_raw();
    burn_ms(burn);
    wall1 = mono_raw();
    p1 = clk_ns(CLK_PROCESS_CPUTIME);
    t1 = clk_ns(CLK_THREAD_CPUTIME);

    // Quemando CPU en un solo hilo, los dos relojes tienen que avanzar, y no
    // mas de lo que ha avanzado el reloj de pared.
    check(t1 - t0 > (int64_t)burn * 1000000 / 4 && t1 - t0 <= (wall1 - wall0) + 20000000,
          "el reloj de CPU del hilo sigue al trabajo",
          "%lld ms de CPU en %lld ms de pared", (long long)((t1 - t0) / 1000000),
          (long long)((wall1 - wall0) / 1000000));
    check(p1 - p0 >= t1 - t0 - 2000000, "el reloj del proceso incluye al del hilo",
          "proceso %lld ms, hilo %lld ms", (long long)((p1 - p0) / 1000000),
          (long long)((t1 - t0) / 1000000));

    // Y durmiendo no tiene que avanzar: un reloj de CPU que cuenta el tiempo
    // dormido es el reloj de pared con otro nombre, y `clock()` deja de medir
    // lo que dice medir.
    t0 = clk_ns(CLK_THREAD_CPUTIME);
    wall0 = mono_raw();
    sleep_ms(nap);
    wall1 = mono_raw();
    t1 = clk_ns(CLK_THREAD_CPUTIME);
    check(t1 - t0 >= 0 && t1 - t0 < (wall1 - wall0) / 2, "el reloj de CPU no cuenta el sueno",
          "%lld us de CPU en %lld ms dormido", (long long)((t1 - t0) / 1000),
          (long long)((wall1 - wall0) / 1000000));

    // La misma cuenta por el id que devuelve clock_getcpuclockid tiene que ser
    // la misma cuenta: son dos caminos al mismo reloj.
    {
        clockid_t id;
        if (clock_getcpuclockid(getpid(), &id) != 0) {
            skip("clock_getcpuclockid", "no lo sirve: %s", strerror(errno));
        } else {
            struct timespec a, b;
            int ra = clock_gettime(id, &a);
            int rb = clock_gettime((clockid_t)CLK_PROCESS_CPUTIME, &b);
            if (ra != 0 || rb != 0)
                bad("clock_getcpuclockid", "una de las dos lecturas fallo: %s", strerror(errno));
            else
                check(llabs((long long)(ts_ns(&a) - ts_ns(&b))) < 20000000,
                      "getcpuclockid da el reloj del proceso", "%lld us de diferencia",
                      (long long)((ts_ns(&a) - ts_ns(&b)) / 1000));
        }
    }
}

// ------------------------------------------------------------------ affinity

static void sec_affinity(void) {
    cpu_set_t set, old;
    int ncpu = (int)sysconf(_SC_NPROCESSORS_ONLN);
    int cpu, moved = 0;
    int64_t prev = 0, worst_back = 0;
    long backs = 0;
    section("affinity", "the monotonic clock does not go backwards between CPUs");

    if (ncpu <= 1) {
        skip("migracion", "una sola CPU en linea");
        return;
    }
    if (sched_getaffinity(0, sizeof old, &old) != 0) {
        skip("migracion", "sched_getaffinity: %s", strerror(errno));
        return;
    }

    prev = clk_ns(CLK_MONOTONIC);
    for (cpu = 0; cpu < ncpu && cpu < CPU_SETSIZE; cpu++) {
        int round;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        if (sched_setaffinity(0, sizeof set, &set) != 0) continue;
        sched_yield();
        if (sched_getcpu() != cpu) continue;
        moved++;
        // Veinte lecturas por CPU: lo que se busca es el salto en el cruce,
        // pero un TSC no sincronizado se delata igual dentro de la CPU nueva.
        for (round = 0; round < 20; round++) {
            int64_t now = clk_ns(CLK_MONOTONIC);
            if (now < prev) {
                backs++;
                if (prev - now > worst_back) worst_back = prev - now;
            }
            prev = now;
        }
    }
    sched_setaffinity(0, sizeof old, &old);

    if (moved < 2) {
        skip("migracion", "no se pudo fijar el hilo a dos CPUs distintas");
        return;
    }
    // Un retroceso aqui es o TSCs sin sincronizar con el suelo del kernel
    // desactivado, o una vDSO que sigue respondiendo despues de que el kernel
    // degradase al camino con suelo: el hilo que migra ve el reloj ir atras.
    if (backs)
        bad("el reloj no retrocede al migrar", "%ld retrocesos entre %d CPUs, el peor %lld ns",
            backs, moved, (long long)worst_back);
    else
        ok("el reloj no retrocede al migrar", "%d CPUs recorridas, ningun retroceso", moved);
}

// ------------------------------------------------------------------ rate

static void sec_rate(int quick) {
    long window_ms = quick ? 600 : 1500;
    int64_t m0, w0, b0, m1, w1, b1;
    double up0, up1;
    int have_up = read_proc_uptime(&up0) == 0;
    double window_s;
    section("rate", "every clock advances by the same amount over one window");

    m0 = clk_ns(CLK_MONOTONIC);
    w0 = clk_ns(CLK_REALTIME);
    b0 = clk_ns(CLK_BOOTTIME);
    sleep_ms(window_ms);
    m1 = clk_ns(CLK_MONOTONIC);
    w1 = clk_ns(CLK_REALTIME);
    b1 = clk_ns(CLK_BOOTTIME);
    if (have_up) have_up = read_proc_uptime(&up1) == 0;

    window_s = (double)(m1 - m0) / 1e9;
    note("ventana medida por el monotono: %.6f s (pedidos %ld ms)", window_s, window_ms);

    // Pared contra monotono: con NTP parado son el mismo avance. Un 0,5 % de
    // margen cubre la granularidad del tick y deja ver de sobra el 1,8 % con
    // que una frecuencia de TSC adivinada paso por aqui una vez.
    {
        double wall_s = (double)(w1 - w0) / 1e9;
        double rel = window_s > 0 ? (wall_s - window_s) / window_s : 1.0;
        check(rel > -0.005 && rel < 0.005, "la pared avanza como el monotono",
              "pared %.6f s, monotono %.6f s, %+.3f %%", wall_s, window_s, rel * 100);
    }
    if (b0 >= 0 && b1 >= 0) {
        double boot_s = (double)(b1 - b0) / 1e9;
        double rel = window_s > 0 ? (boot_s - window_s) / window_s : 1.0;
        check(rel > -0.005 && rel < 0.005, "el boottime avanza como el monotono",
              "boottime %.6f s, %+.3f %%", boot_s, rel * 100);
    }
    if (have_up) {
        // /proc/uptime viene del reloj del kernel sin pasar por la vDSO, asi
        // que esta comparacion es la del reloj de userspace contra el del
        // kernel a lo largo de una ventana: un multiplicador publicado de mas
        // o de menos se separa aqui y no en una lectura suelta.
        double up_s = up1 - up0;
        double rel = window_s > 0 ? (up_s - window_s) / window_s : 1.0;
        check(rel > -0.02 && rel < 0.02, "/proc/uptime avanza como el monotono",
              "uptime %+.3f s, %+.3f %% (resolucion del fichero: 10 ms)", up_s, rel * 100);
    }
}

// ------------------------------------------------------------------ sleep

struct ladder {
    const char *name;
    long ns;
};

static const struct ladder LADDER[] = {
    { "1 us", 1000 },      { "100 us", 100000 },  { "1 ms", 1000000 },
    { "10 ms", 10000000 }, { "50 ms", 50000000 }, { "200 ms", 200000000 },
};
#define N_LADDER ((int)(sizeof LADDER / sizeof LADDER[0]))

static void sec_sleep(int quick) {
    int i;
    int steps = quick ? N_LADDER - 1 : N_LADDER;
    section("sleep", "no sleep returns early");

    // `nanosleep` relativo: el contrato es «al menos», y el error que importa
    // es volver ANTES, porque un programa que duerme 1 ms y vuelve en 200 us
    // gira cinco veces mas rapido de lo que su autor decidio.
    for (i = 0; i < steps; i++) {
        struct timespec req;
        int64_t t0, t1, slept;
        long short_by;
        char label[64];
        ns_ts(LADDER[i].ns, &req);
        t0 = mono_raw();
        while (nanosleep(&req, &req) != 0 && errno == EINTR) {
        }
        t1 = mono_raw();
        slept = t1 - t0;
        short_by = (long)(LADDER[i].ns - slept);
        snprintf(label, sizeof label, "nanosleep %s no vuelve antes", LADDER[i].name);
        check(short_by <= 0, label, "pedido %ld ns, dormido %lld ns (%+ld)", LADDER[i].ns,
              (long long)slept, -short_by);
    }

    // `clock_nanosleep(TIMER_ABSTIME)` sobre el monotono: la mitad de los
    // temporizadores de un compositor son exactamente esto.
    {
        int64_t deadline, t1;
        struct timespec abs;
        int r;
        struct guard g;
        deadline = clk_ns(CLK_MONOTONIC) + 80000000;
        ns_ts(deadline, &abs);
        guard_start(&g, 3000);
        r = clock_nanosleep((clockid_t)CLK_MONOTONIC, TIMER_ABSTIME, &abs, NULL);
        t1 = clk_ns(CLK_MONOTONIC);
        if (guard_stop(&g))
            bad("clock_nanosleep ABSTIME monotono", "no volvio en 3 s");
        else if (r != 0 && r != EINTR)
            bad("clock_nanosleep ABSTIME monotono", "fallo: %s", strerror(r));
        else
            check(t1 >= deadline, "clock_nanosleep ABSTIME monotono",
                  "despierta %lld ns %s del plazo", (long long)llabs((long long)(t1 - deadline)),
                  t1 >= deadline ? "despues" : "ANTES");
    }

    // Y sobre el reloj de pared, que es el que se llevaba la fecha absoluta a
    // un plazo monotono: ahi la llamada no vuelve nunca, por eso el guardia.
    {
        int64_t deadline, t1;
        struct timespec abs;
        int r, poked;
        struct guard g;
        deadline = clk_ns(CLK_REALTIME) + 80000000;
        ns_ts(deadline, &abs);
        guard_start(&g, 3000);
        r = clock_nanosleep((clockid_t)CLK_REALTIME, TIMER_ABSTIME, &abs, NULL);
        t1 = clk_ns(CLK_REALTIME);
        poked = guard_stop(&g);
        if (poked)
            bad("clock_nanosleep ABSTIME pared",
                "no volvio en 3 s: la fecha absoluta se armo como una distancia");
        else if (r != 0 && r != EINTR)
            bad("clock_nanosleep ABSTIME pared", "fallo: %s", strerror(r));
        else
            check(t1 >= deadline && t1 < deadline + 500000000, "clock_nanosleep ABSTIME pared",
                  "despierta %+lld ms respecto al plazo",
                  (long long)((t1 - deadline) / 1000000));
    }

    // Un plazo absoluto ya pasado vence ya: en Linux vuelve de inmediato. Leido
    // como distancia seria un sueno de medio siglo.
    {
        int64_t t0, t1;
        struct timespec abs;
        int r;
        struct guard g;
        ns_ts(clk_ns(CLK_REALTIME) - 60LL * NS_PER_SEC, &abs);
        guard_start(&g, 3000);
        t0 = mono_raw();
        r = clock_nanosleep((clockid_t)CLK_REALTIME, TIMER_ABSTIME, &abs, NULL);
        t1 = mono_raw();
        if (guard_stop(&g))
            bad("un plazo ya pasado vence ya", "no volvio en 3 s");
        else if (r != 0 && r != EINTR)
            bad("un plazo ya pasado vence ya", "fallo: %s", strerror(r));
        else
            check(t1 - t0 < 50000000, "un plazo ya pasado vence ya", "volvio en %lld us",
                  (long long)((t1 - t0) / 1000));
    }
    (void)quick;
}

// ------------------------------------------------------------------ timerfd

static void sec_timerfd(void) {
    int fd;
    section("timerfd", "a timerfd fires once per expiry, never early, loses none");

    // Relativo, un disparo: el caso que ya funcionaba, y la linea base contra
    // la que se leen las dos siguientes.
    fd = timerfd_create((clockid_t)CLK_MONOTONIC, 0);
    if (fd < 0) {
        skip("timerfd", "timerfd_create: %s", strerror(errno));
        return;
    }
    {
        struct itimerspec its;
        uint64_t ticks = 0;
        int64_t t0, t1;
        ssize_t r;
        struct guard g;
        memset(&its, 0, sizeof its);
        its.it_value.tv_nsec = 80000000;
        t0 = mono_raw();
        if (timerfd_settime(fd, 0, &its, NULL) != 0) {
            bad("timerfd relativo", "timerfd_settime: %s", strerror(errno));
        } else {
            guard_start(&g, 3000);
            r = read(fd, &ticks, sizeof ticks);
            t1 = mono_raw();
            if (guard_stop(&g))
                bad("timerfd relativo", "no disparo en 3 s");
            else if (r != (ssize_t)sizeof ticks)
                bad("timerfd relativo", "read devolvio %zd: %s", r, strerror(errno));
            else
                check(ticks == 1 && t1 - t0 >= 80000000, "timerfd relativo",
                      "%llu expiracion(es) tras %lld ms", (unsigned long long)ticks,
                      (long long)((t1 - t0) / 1000000));
        }
    }

    // Antes de vencer no hay nada que leer. Un timerfd que entrega una
    // expiracion de mas es un bucle de compositor que gira libre.
    {
        struct itimerspec its;
        uint64_t ticks = 0;
        ssize_t r;
        int flags = fcntl(fd, F_GETFL);
        memset(&its, 0, sizeof its);
        its.it_value.tv_sec = 30;
        if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0) {
            skip("timerfd antes de vencer", "no se puede poner O_NONBLOCK: %s", strerror(errno));
        } else if (timerfd_settime(fd, 0, &its, NULL) != 0) {
            bad("timerfd antes de vencer", "timerfd_settime: %s", strerror(errno));
        } else {
            r = read(fd, &ticks, sizeof ticks);
            check(r < 0 && errno == EAGAIN, "timerfd antes de vencer no entrega nada",
                  "read -> %zd, %s", r, r < 0 ? strerror(errno) : "datos");
            // Y lo que queda no puede ser mas de lo armado ni haber crecido.
            {
                struct itimerspec left;
                if (timerfd_gettime(fd, &left) != 0)
                    bad("timerfd_gettime", "fallo: %s", strerror(errno));
                else
                    check(ts_ns(&left.it_value) > 0 && ts_ns(&left.it_value) <= 30 * NS_PER_SEC,
                          "timerfd_gettime cuenta hacia atras", "quedan %.3f s de 30",
                          (double)ts_ns(&left.it_value) / 1e9);
            }
            fcntl(fd, F_SETFL, flags);
        }
        memset(&its, 0, sizeof its);
        timerfd_settime(fd, 0, &its, NULL);
    }

    // Periodico: diez periodos de 20 ms. La suma de las expiraciones leidas
    // tiene que ser diez, ni nueve (una perdida) ni doce (una inventada), y el
    // total nunca menos de 200 ms.
    {
        struct itimerspec its;
        uint64_t total = 0;
        int64_t t0, t1;
        int reads = 0;
        struct guard g;
        memset(&its, 0, sizeof its);
        its.it_value.tv_nsec = 20000000;
        its.it_interval.tv_nsec = 20000000;
        t0 = mono_raw();
        if (timerfd_settime(fd, 0, &its, NULL) != 0) {
            bad("timerfd periodico", "timerfd_settime: %s", strerror(errno));
        } else {
            guard_start(&g, 5000);
            while (total < 10 && reads < 40) {
                uint64_t ticks = 0;
                ssize_t r = read(fd, &ticks, sizeof ticks);
                if (r != (ssize_t)sizeof ticks) break;
                total += ticks;
                reads++;
            }
            t1 = mono_raw();
            if (guard_stop(&g))
                bad("timerfd periodico", "se quedo colgado tras %llu expiraciones",
                    (unsigned long long)total);
            else
                check(total == 10 && t1 - t0 >= 200000000, "timerfd periodico no pierde ni inventa",
                      "%llu expiraciones en %d lecturas, %lld ms (minimo 200)",
                      (unsigned long long)total, reads, (long long)((t1 - t0) / 1000000));
            memset(&its, 0, sizeof its);
            timerfd_settime(fd, 0, &its, NULL);
        }
    }
    close(fd);

    // Y el caso del error: una FECHA absoluta de reloj de pared. Armada como
    // distancia monotona son cincuenta años y el read no vuelve nunca.
    {
        int wfd = timerfd_create((clockid_t)CLK_REALTIME, 0);
        if (wfd < 0) {
            skip("timerfd ABSTIME de pared", "timerfd_create: %s", strerror(errno));
        } else {
            struct itimerspec its;
            uint64_t ticks = 0;
            int64_t deadline, t1;
            ssize_t r;
            struct guard g;
            memset(&its, 0, sizeof its);
            deadline = clk_ns(CLK_REALTIME) + 80000000;
            ns_ts(deadline, &its.it_value);
            if (timerfd_settime(wfd, TFD_TIMER_ABSTIME, &its, NULL) != 0) {
                bad("timerfd ABSTIME de pared", "timerfd_settime: %s", strerror(errno));
            } else {
                guard_start(&g, 3000);
                r = read(wfd, &ticks, sizeof ticks);
                t1 = clk_ns(CLK_REALTIME);
                if (guard_stop(&g))
                    bad("timerfd ABSTIME de pared",
                        "no disparo en 3 s: la fecha se armo como una distancia");
                else if (r != (ssize_t)sizeof ticks)
                    bad("timerfd ABSTIME de pared", "read devolvio %zd: %s", r, strerror(errno));
                else
                    check(ticks == 1 && t1 >= deadline, "timerfd ABSTIME de pared es una fecha",
                          "%llu expiracion(es), %+lld ms respecto al plazo",
                          (unsigned long long)ticks, (long long)((t1 - deadline) / 1000000));
            }
            close(wfd);
        }
    }
}

// ------------------------------------------------------------------ timer

static void sec_timer(void) {
    sigset_t set, old;
    timer_t tid;
    struct sigevent ev;
    section("timer", "timer_create fires once, not early, and counts down");

    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    if (sigprocmask(SIG_BLOCK, &set, &old) != 0) {
        skip("timer_create", "sigprocmask: %s", strerror(errno));
        return;
    }

    memset(&ev, 0, sizeof ev);
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = SIGUSR2;
    if (timer_create((clockid_t)CLK_MONOTONIC, &ev, &tid) != 0) {
        skip("timer_create", "no lo sirve: %s", strerror(errno));
        sigprocmask(SIG_SETMASK, &old, NULL);
        return;
    }

    {
        struct itimerspec its, left;
        int64_t t0, t1;
        struct guard g;
        memset(&its, 0, sizeof its);
        its.it_value.tv_nsec = 80000000;
        t0 = mono_raw();
        if (timer_settime(tid, 0, &its, NULL) != 0) {
            bad("timer monotono relativo", "timer_settime: %s", strerror(errno));
        } else {
            // Lo que queda no puede ser mas de lo armado: el `timer_gettime`
            // que devuelve el plazo absoluto en vez de la distancia es el
            // mismo error de confundir fecha y distancia, visto del otro lado.
            if (timer_gettime(tid, &left) == 0)
                check(ts_ns(&left.it_value) > 0 && ts_ns(&left.it_value) <= 80000000,
                      "timer_gettime da lo que queda", "quedan %lld us de 80000",
                      (long long)(ts_ns(&left.it_value) / 1000));
            guard_start(&g, 3000);
            {
                siginfo_t info;
                int r = sigwaitinfo(&set, &info);
                t1 = mono_raw();
                if (guard_stop(&g))
                    bad("timer monotono relativo", "no disparo en 3 s");
                else if (r < 0)
                    bad("timer monotono relativo", "sigwaitinfo: %s", strerror(errno));
                else
                    check(t1 - t0 >= 80000000, "timer monotono relativo no dispara antes",
                          "disparo tras %lld ms (pedidos 80)", (long long)((t1 - t0) / 1000000));
            }
        }
    }
    timer_delete(tid);

    // El mismo temporizador contra una fecha de pared, que es el camino que
    // armaba cincuenta años.
    if (timer_create((clockid_t)CLK_REALTIME, &ev, &tid) == 0) {
        struct itimerspec its;
        int64_t deadline, t1;
        struct guard g;
        memset(&its, 0, sizeof its);
        deadline = clk_ns(CLK_REALTIME) + 80000000;
        ns_ts(deadline, &its.it_value);
        if (timer_settime(tid, TIMER_ABSTIME, &its, NULL) != 0) {
            bad("timer ABSTIME de pared", "timer_settime: %s", strerror(errno));
        } else {
            int r;
            guard_start(&g, 3000);
            r = sigwaitinfo(&set, NULL);
            t1 = clk_ns(CLK_REALTIME);
            if (guard_stop(&g))
                bad("timer ABSTIME de pared", "no disparo en 3 s: la fecha se armo como distancia");
            else if (r < 0)
                bad("timer ABSTIME de pared", "sigwaitinfo: %s", strerror(errno));
            else
                check(t1 >= deadline, "timer ABSTIME de pared es una fecha",
                      "%+lld ms respecto al plazo", (long long)((t1 - deadline) / 1000000));
        }
        timer_delete(tid);
    } else {
        skip("timer ABSTIME de pared", "timer_create(CLOCK_REALTIME): %s", strerror(errno));
    }

    sigprocmask(SIG_SETMASK, &old, NULL);
}

// ------------------------------------------------------------------ itimer

static void sec_itimer(void) {
    sigset_t set, old;
    struct itimerval itv, left;
    int64_t t0, t1;
    struct guard g;
    section("itimer", "setitimer(ITIMER_REAL) fires, and not early");

    sigemptyset(&set);
    sigaddset(&set, SIGALRM);
    if (sigprocmask(SIG_BLOCK, &set, &old) != 0) {
        skip("setitimer", "sigprocmask: %s", strerror(errno));
        return;
    }

    memset(&itv, 0, sizeof itv);
    itv.it_value.tv_usec = 80000;
    t0 = mono_raw();
    if (setitimer(ITIMER_REAL, &itv, NULL) != 0) {
        skip("setitimer", "no lo sirve: %s", strerror(errno));
        sigprocmask(SIG_SETMASK, &old, NULL);
        return;
    }
    if (getitimer(ITIMER_REAL, &left) == 0) {
        int64_t rem = (int64_t)left.it_value.tv_sec * NS_PER_SEC +
                      (int64_t)left.it_value.tv_usec * 1000;
        check(rem > 0 && rem <= 80000000, "getitimer da lo que queda", "quedan %lld us de 80000",
              (long long)(rem / 1000));
    }
    guard_start(&g, 3000);
    {
        int r = sigwaitinfo(&set, NULL);
        t1 = mono_raw();
        if (guard_stop(&g))
            bad("ITIMER_REAL", "no disparo en 3 s");
        else if (r < 0)
            bad("ITIMER_REAL", "sigwaitinfo: %s", strerror(errno));
        else
            check(t1 - t0 >= 80000000, "ITIMER_REAL no dispara antes",
                  "disparo tras %lld ms (pedidos 80)", (long long)((t1 - t0) / 1000000));
    }
    memset(&itv, 0, sizeof itv);
    setitimer(ITIMER_REAL, &itv, NULL);
    sigprocmask(SIG_SETMASK, &old, NULL);
}

// ------------------------------------------------------------------ timeout
//
// Toda llamada bloqueante con plazo promete lo mismo que `nanosleep`: el plazo
// es un suelo. Un `poll` que vuelve antes de su timeout es un bucle de evento
// girando en vacio, que es calor y bateria y nada mas; el sintoma se ha
// reportado en esta maquina y no se puede medir sin esta seccion.

#define TIMEOUT_MS 60

static void sec_timeout(void) {
    section("timeout", "poll, select, epoll and the timed waits honour their floor");

    {
        int64_t t0 = mono_raw(), t1;
        int r = poll(NULL, 0, TIMEOUT_MS);
        t1 = mono_raw();
        check(r == 0 && t1 - t0 >= (int64_t)TIMEOUT_MS * 1000000, "poll respeta su timeout",
              "r=%d, %lld us (pedidos %d ms)", r, (long long)((t1 - t0) / 1000), TIMEOUT_MS);
    }
    {
        struct timespec to;
        int64_t t0 = mono_raw(), t1;
        int r;
        ns_ts((int64_t)TIMEOUT_MS * 1000000, &to);
        r = ppoll(NULL, 0, &to, NULL);
        t1 = mono_raw();
        check(r == 0 && t1 - t0 >= (int64_t)TIMEOUT_MS * 1000000, "ppoll respeta su timeout",
              "r=%d, %lld us", r, (long long)((t1 - t0) / 1000));
    }
    {
        struct timeval to;
        int64_t t0 = mono_raw(), t1;
        int r;
        to.tv_sec = 0;
        to.tv_usec = TIMEOUT_MS * 1000;
        r = select(0, NULL, NULL, NULL, &to);
        t1 = mono_raw();
        check(r == 0 && t1 - t0 >= (int64_t)TIMEOUT_MS * 1000000, "select respeta su timeout",
              "r=%d, %lld us", r, (long long)((t1 - t0) / 1000));
    }
    {
        int ep = epoll_create1(0);
        if (ep < 0) {
            skip("epoll_wait", "epoll_create1: %s", strerror(errno));
        } else {
            struct epoll_event evs[1];
            int64_t t0 = mono_raw(), t1;
            int r = epoll_wait(ep, evs, 1, TIMEOUT_MS);
            t1 = mono_raw();
            check(r == 0 && t1 - t0 >= (int64_t)TIMEOUT_MS * 1000000,
                  "epoll_wait respeta su timeout", "r=%d, %lld us", r,
                  (long long)((t1 - t0) / 1000));
            close(ep);
        }
    }
    // Estas dos toman una FECHA de reloj de pared, no una distancia: son el
    // `pthread_cond_timedwait` de cualquier biblioteca de hilos, y el mismo
    // camino que confundia fecha con distancia en los temporizadores.
    {
        sem_t sem;
        struct timespec abs;
        int64_t deadline, t1;
        int r;
        struct guard g;
        if (sem_init(&sem, 0, 0) != 0) {
            skip("sem_timedwait", "sem_init: %s", strerror(errno));
        } else {
            deadline = clk_ns(CLK_REALTIME) + (int64_t)TIMEOUT_MS * 1000000;
            ns_ts(deadline, &abs);
            guard_start(&g, 3000);
            do {
                r = sem_timedwait(&sem, &abs);
            } while (r != 0 && errno == EINTR);
            t1 = clk_ns(CLK_REALTIME);
            if (guard_stop(&g))
                bad("sem_timedwait", "no volvio en 3 s con un plazo de %d ms", TIMEOUT_MS);
            else
                check(r < 0 && errno == ETIMEDOUT && t1 >= deadline, "sem_timedwait respeta la fecha",
                      "r=%d %s, %+lld us respecto al plazo", r, r < 0 ? strerror(errno) : "",
                      (long long)((t1 - deadline) / 1000));
            sem_destroy(&sem);
        }
    }
    {
        pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
        pthread_cond_t c = PTHREAD_COND_INITIALIZER;
        struct timespec abs;
        int64_t deadline, t1;
        int r;
        struct guard g;
        deadline = clk_ns(CLK_REALTIME) + (int64_t)TIMEOUT_MS * 1000000;
        ns_ts(deadline, &abs);
        pthread_mutex_lock(&m);
        guard_start(&g, 3000);
        do {
            r = pthread_cond_timedwait(&c, &m, &abs);
        } while (r == EINTR);
        t1 = clk_ns(CLK_REALTIME);
        pthread_mutex_unlock(&m);
        if (guard_stop(&g))
            bad("pthread_cond_timedwait", "no volvio en 3 s con un plazo de %d ms", TIMEOUT_MS);
        else
            check(r == ETIMEDOUT && t1 >= deadline, "pthread_cond_timedwait respeta la fecha",
                  "r=%s, %+lld us respecto al plazo", r == ETIMEDOUT ? "ETIMEDOUT" : strerror(r),
                  (long long)((t1 - deadline) / 1000));
    }
}

// ------------------------------------------------------------------- main

struct sec {
    const char *name;
    void (*run)(int quick);
};

static void w_res(int q) { (void)q; sec_res(); }
static void w_mono(int q) { sec_mono(q); }
static void w_vdso(int q) { sec_vdso(q); }
static void w_offset(int q) { sec_offset(q); }
static void w_coarse(int q) { (void)q; sec_coarse(); }
static void w_uptime(int q) { (void)q; sec_uptime(); }
static void w_epoch(int q) { (void)q; sec_epoch(); }
static void w_cpu(int q) { sec_cpu(q); }
static void w_affinity(int q) { (void)q; sec_affinity(); }
static void w_rate(int q) { sec_rate(q); }
static void w_sleep(int q) { sec_sleep(q); }
static void w_timerfd(int q) { (void)q; sec_timerfd(); }
static void w_timer(int q) { (void)q; sec_timer(); }
static void w_itimer(int q) { (void)q; sec_itimer(); }
static void w_timeout(int q) { (void)q; sec_timeout(); }

static const struct sec SECTIONS[] = {
    { "res", w_res },         { "mono", w_mono },       { "vdso", w_vdso },
    { "offset", w_offset },   { "coarse", w_coarse },   { "uptime", w_uptime },
    { "epoch", w_epoch },     { "cpu", w_cpu },         { "affinity", w_affinity },
    { "rate", w_rate },       { "sleep", w_sleep },     { "timerfd", w_timerfd },
    { "timer", w_timer },     { "itimer", w_itimer },   { "timeout", w_timeout },
};
#define N_SECTIONS ((int)(sizeof SECTIONS / sizeof SECTIONS[0]))

static void usage(const char *argv0) {
    int i;
    printf("uso: %s [--only SECCION] [--quick] [-v]\n\n", argv0);
    printf("secciones:");
    for (i = 0; i < N_SECTIONS; i++) printf(" %s", SECTIONS[i].name);
    printf("\n\n"
           "  --only SECCION  una sola seccion (se puede repetir)\n"
           "  --quick         menos vueltas; mas rapido y mas ruidoso\n"
           "  -v              imprime tambien las notas de detalle\n\n"
           "Salida: 0 si todo cuadra, 1 si algo falla, 2 si la linea de orden es mala.\n");
}

int main(int argc, char **argv) {
    const char *only[N_SECTIONS];
    int n_only = 0, quick = 0, i, a;
    struct sigaction sa;

    for (a = 1; a < argc; a++) {
        if (!strcmp(argv[a], "--only") && a + 1 < argc) {
            if (n_only >= N_SECTIONS) {
                usage(argv[0]);
                return 2;
            }
            only[n_only++] = argv[++a];
        } else if (!strcmp(argv[a], "--quick")) {
            quick = 1;
        } else if (!strcmp(argv[a], "-v")) {
            g_verbose = 1;
        } else {
            usage(argv[0]);
            return !strcmp(argv[a], "-h") || !strcmp(argv[a], "--help") ? 0 : 2;
        }
    }
    for (i = 0; i < n_only; i++) {
        int found = 0, j;
        for (j = 0; j < N_SECTIONS; j++)
            if (!strcmp(only[i], SECTIONS[j].name)) found = 1;
        if (!found) {
            printf("seccion desconocida: %s\n\n", only[i]);
            usage(argv[0]);
            return 2;
        }
    }

    // Sin SA_RESTART: el guardia existe para que una llamada bloqueada vuelva.
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = poke_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    sigaction(SIGUSR1, &sa, NULL);

    printf("clock-probe: integridad de los relojes y los temporizadores\n");
    note("%d CPU(s) en linea%s", (int)sysconf(_SC_NPROCESSORS_ONLN),
         quick ? ", modo --quick" : "");
    vnote("un FAIL aqui es una afirmacion sobre el reloj, no una medida de velocidad");

    for (i = 0; i < N_SECTIONS; i++) {
        int run = n_only == 0, j;
        for (j = 0; j < n_only; j++)
            if (!strcmp(only[j], SECTIONS[i].name)) run = 1;
        if (run) SECTIONS[i].run(quick);
    }

    printf("\n== resumen\n  %d bien, %d mal, %d omitidas\n", g_pass, g_fail, g_skip);
    if (g_fail) printf("  el reloj o los temporizadores de esta maquina no cumplen lo anterior\n");
    return g_fail ? 1 : 0;
}
