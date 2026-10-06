// eclipse-bench — CPU / memory / syscall / VM / scheduler / sockets /
// filesystem / disk / process benchmark for Eclipse OS.
//
// It is deliberately dependency-free (POSIX + libc only) and statically linked,
// so it can be dropped straight into the rootfs and run from the shell. Every
// micro-benchmark is *time-bounded* (it runs for a target wall-clock budget and
// counts how much work it completed) so the whole suite finishes in well under a
// minute even on a slow USB stick, and adapts automatically to fast (QEMU) vs
// slow (real disk) machines.
//
// Build (musl, static):
//     x86_64-linux-musl-gcc -O2 -static -pthread -o eclipse-bench eclipse-bench.c
// then copy `eclipse-bench` into the rootfs and run:
//     ./eclipse-bench [DIR] [DISK_MB] [MEM_MB]
//
// DIR     directory for the disk tests — MUST live on the filesystem you want to
//         measure (the btrfs/ext2 root), NOT a tmpfs like /tmp or /run, or the
//         "disk" numbers will just measure RAM. Default: current directory.
// DISK_MB size of the disk test file in MiB (default 32).
// MEM_MB  size of the memory working set in MiB (default 32).
//
// Options (before the positional arguments):
//     --only SECTION   run one section: cpu, mem, syscall, vm, sched, psched,
//                      net, fs, smp, disk, proc, gfx
//     --drm PATH       DRM device for the gfx section (default /dev/dri/card0)
//     --quick          shorter time budgets (rough numbers, ~3x faster)
//     --budget MS      per-measurement wall-clock budget (default 200 ms for the
//                      small probes, 400 ms for the streaming ones)
//     --max MS         hard ceiling per measurement (default 20 s)
//
// Every time-bounded probe also collects at least MIN_SAMPLES samples even if
// that overruns its budget, so a slow kernel yields fewer-but-real numbers
// instead of a number derived from three or four samples. A slower kernel
// therefore makes the *run* longer, not the numbers worse — give the harness a
// correspondingly larger timeout.
//
// ---------------------------------------------------------------------------
// READ THIS BEFORE TRUSTING A NUMBER
// ---------------------------------------------------------------------------
//
// Every line is tagged with what it actually measures:
//
//   [user]   Runs entirely in userspace on already-faulted memory. The kernel
//            is not involved, so this number is a property of the CPU and the
//            compiler — NOT of the operating system. Two different OSes on the
//            same machine MUST produce the same figure; if they do not, the
//            difference is frequency scaling, not kernel quality. These lines
//            can never show that an OS is fast. They are here only to
//            establish the clock the kernel lines are measured against.
//
//   [kernel] Dominated by kernel code paths. This is where an OS is fast or
//            slow, and where a gap against Linux is real.
//
// The suite used to report only [user] figures plus `getpid()`, `fork`, and
// disk throughput. That is why it read as "on par with Linux" while the system
// did not feel like it: the things that decide perceived speed — wake-up
// latency, context-switch cost, page-fault cost, and how any of it behaves when
// more than one thing wants the CPU — were not measured at all. Every
// [user] benchmark also runs *alone* on an otherwise idle machine, which is the
// one condition under which a scheduler cannot be caught being slow.
//
// The SCHEDULER section is the one to watch. `wake latency under load` in
// particular: it is the delay between a task becoming runnable and it actually
// running while the CPUs are busy, and it is what a shell, a compositor or an
// editor spends its life waiting on.
//
// For a real comparison, run *this same binary* on Linux on the *same machine*
// and diff the output. The RATIOS section at the end is designed to survive
// even when you cannot: it reports kernel costs relative to this machine's own
// measured CPU speed, so a slow VM does not disguise a slow kernel.

#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/auxv.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/select.h>
#include <sys/epoll.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>

// ---------------------------------------------------------------------------
// Timing + anti-optimization helpers
// ---------------------------------------------------------------------------

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

// A volatile sink the loops feed into so the compiler can't elide them.
static volatile uint64_t g_sink;

// Per-microbench wall-clock budgets. `--quick` scales them down, `--budget MS`
// sets them explicitly.
static uint64_t g_budget_ns = 400000000ull;  // 0.4 s — the classic sections
static uint64_t g_short_ns = 200000000ull;   // 0.2 s — the many small probes

// Minimum samples a time-bounded measurement must collect before it may stop,
// regardless of the wall-clock budget.
//
// A pure time budget silently degrades as the operation gets slower: at 46 ms
// per `fork+exec`, a 0.2 s budget buys FOUR samples, and four samples of
// anything on a loaded emulated machine is not a measurement. The floor makes a
// slow path take longer rather than report a number nobody should trust — which
// is the right trade for a benchmark whose whole job is telling slow from fast.
// It also means a run gets *longer* as the kernel gets slower, so budget the
// harness timeout accordingly (scripts/qemu-bench.sh -t).
#define MIN_SAMPLES 24
// Ceiling on that generosity: without it a pathologically slow operation could
// hold the suite for hours. Reaching it is reported, not hidden.
static uint64_t g_max_ns = 20000000000ull; // 20 s per measurement

#define NA (-1.0)

// How many operations the last `timed_ns_per_op` / `pingpong_drive` call
// actually performed. A derived per-operation figure (a kernel counter divided
// by the work that caused it) needs the real count: those loops keep going past
// the wall-clock budget until MIN_SAMPLES is met, so dividing by
// budget/cost_per_op would silently understate a slow path by whatever factor
// the floor added.
static uint64_t g_last_ops;

// Run `fn(n)` with a growing `n` until one call lasts >= `budget_ns`, then
// return the achieved rate in operations/second. `fn` must return a value
// derived from its work (fed into g_sink) so it isn't optimized away.
static double timed_oprate(uint64_t (*fn)(uint64_t), uint64_t budget_ns) {
    uint64_t n = 1u << 16;
    for (;;) {
        uint64_t t0 = now_ns();
        uint64_t r = fn(n);
        uint64_t t1 = now_ns();
        g_sink += r;
        uint64_t dt = t1 - t0;
        if (dt >= budget_ns)
            return (double)n * 1e9 / (double)dt;
        if (dt < 1000) { // too fast to measure — grow aggressively
            n <<= 3;
            continue;
        }
        // Scale n to land a bit past the budget next time.
        double factor = (double)budget_ns / (double)dt * 1.3;
        uint64_t next = (uint64_t)((double)n * factor);
        n = next > n ? next : n * 2;
    }
}

// Run `fn()` repeatedly and return the mean nanoseconds per call. Stops once
// the wall-clock budget is spent *and* at least MIN_SAMPLES calls have been
// made, or when the hard ceiling is hit. `fn` returns 0 on success and non-zero
// to abort the measurement (the operation is unsupported on this kernel), in
// which case NA is returned.
static double timed_ns_per_op(int (*fn)(void), uint64_t budget_ns) {
    // Warm up once so a lazily-initialised path (first mmap of an arena, first
    // open of a device) is not charged to the measurement.
    if (fn() != 0)
        return NA;
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    // Batch of 1 until we know roughly how slow the operation is: batching 64
    // calls of a 46 ms operation would overshoot the budget by three seconds.
    int batch = 1;
    while (elapsed < budget_ns || ops < MIN_SAMPLES) {
        for (int k = 0; k < batch; k++) {
            if (fn() != 0)
                return NA;
            ops++;
        }
        elapsed = now_ns() - t0;
        if (elapsed >= g_max_ns)
            break;
        // Grow the batch only while calls are cheap enough that the timing
        // overhead would otherwise dominate.
        if (batch < 64 && ops > 0 && elapsed / ops < 100000)
            batch = 64;
    }
    g_last_ops = ops;
    return ops ? (double)elapsed / (double)ops : NA;
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

static void line(void) {
    printf("----------------------------------------------------------------------\n");
    fflush(stdout);
}

// One measured row. `unit` is printed after the value; `hint` is a short note
// (typically a Linux orientation figure). NA prints as "n/a".
static void row(const char *tag, const char *label, double value,
                const char *unit, const char *hint) {
    printf("  %-8s %-28s ", tag, label);
    if (value < 0)
        printf("%12s", "n/a");
    else if (value >= 1000.0)
        printf("%12.0f", value);
    else if (value >= 10.0)
        printf("%12.1f", value);
    else
        printf("%12.2f", value);
    printf(" %-9s", unit);
    if (hint && *hint)
        printf(" %s", hint);
    printf("\n");
    fflush(stdout);
}

static void hr_bytes(double bps, char *out, size_t n) {
    const char *u = "B/s";
    double v = bps;
    if (v >= 1e9) { v /= 1e9; u = "GB/s"; }
    else if (v >= 1e6) { v /= 1e6; u = "MB/s"; }
    else if (v >= 1e3) { v /= 1e3; u = "KB/s"; }
    snprintf(out, n, "%.1f %s", v, u);
}

// ---------------------------------------------------------------------------
// CPU  [user]
// ---------------------------------------------------------------------------

// Dependent 64-bit multiply-add chain (a PCG-style LCG). Each iteration depends
// on the previous, so the loop is latency-bound: its rate is ~proportional to
// effective core frequency / IPC and is the cleanest "what clock am I actually
// running at" signal.
static uint64_t cpu_int_chain(uint64_t iters) {
    uint64_t x = 0x9e3779b97f4a7c15ull;
    for (uint64_t i = 0; i < iters; i++)
        x = x * 6364136223846793005ull + 1442695040888963407ull;
    return x;
}

// Independent integer ops across 4 accumulators — measures instruction-level
// throughput (IPC * freq) rather than latency.
static uint64_t cpu_int_tput(uint64_t iters) {
    uint64_t a = 1, b = 2, c = 3, d = 4;
    for (uint64_t i = 0; i < iters; i++) {
        a = a * 2654435761u + 1;
        b = b * 2246822519u + 3;
        c = c * 3266489917u + 5;
        d = d * 668265263u + 7;
    }
    return a ^ b ^ c ^ d;
}

// Dependent double-precision multiply-add chain — float-unit frequency proxy.
static uint64_t cpu_double_chain(uint64_t iters) {
    double f = 1.0000001, a = 1.0000000007, b = 0.0000000003;
    for (uint64_t i = 0; i < iters; i++)
        f = f * a + b;
    return (uint64_t)(f * 1000.0);
}

// ---------------------------------------------------------------------------
// Memory  [user]
// ---------------------------------------------------------------------------

static size_t g_mem_bytes;
static unsigned char *g_mem_src, *g_mem_dst;
static size_t *g_chase; // permuted index cycle for pointer chasing

static uint64_t mem_copy(uint64_t passes) {
    uint64_t bytes = 0;
    for (uint64_t p = 0; p < passes; p++) {
        memcpy(g_mem_dst, g_mem_src, g_mem_bytes);
        bytes += g_mem_bytes;
    }
    return bytes;
}

static uint64_t mem_set(uint64_t passes) {
    uint64_t bytes = 0;
    for (uint64_t p = 0; p < passes; p++) {
        memset(g_mem_dst, (int)(p & 0xff), g_mem_bytes);
        bytes += g_mem_bytes;
    }
    return bytes;
}

// Pointer-chase latency: follow a random cycle so each load depends on the
// previous, defeating prefetch — measures the memory/cache miss latency.
static uint64_t mem_chase(uint64_t hops) {
    size_t i = 0;
    for (uint64_t h = 0; h < hops; h++)
        i = g_chase[i];
    return (uint64_t)i;
}

// Build a single random permutation cycle over `n` slots (Sattolo's algorithm),
// so following g_chase visits every slot exactly once before repeating.
static void build_chase(size_t n) {
    for (size_t i = 0; i < n; i++)
        g_chase[i] = i;
    uint64_t r = 0x243f6a8885a308d3ull;
    for (size_t i = n - 1; i > 0; i--) {
        r = r * 6364136223846793005ull + 1442695040888963407ull;
        size_t j = (size_t)((r >> 11) % i); // 0..i-1
        size_t t = g_chase[i];
        g_chase[i] = g_chase[j];
        g_chase[j] = t;
    }
}

// ---------------------------------------------------------------------------
// Syscall entry cost  [kernel]
// ---------------------------------------------------------------------------
//
// `getpid()` is the floor: trap in, read one field, trap out. Everything else
// here adds one specific kernel subsystem on top of that floor, so the
// *difference* between a row and the getpid row localises the cost.
//
// `clock_gettime` deserves special attention: on Linux it is served from the
// vDSO and never enters the kernel at all (~25 ns). A figure here in the same
// range as `getpid` means Eclipse is taking a real trap for it — and since
// timestamps are one of the most frequently issued operations in any real
// program (every log line, every timeout, every animation frame), that alone
// is worth a vDSO.

static int g_devnull = -1, g_devzero = -1;

static int sc_getpid(void)  { return getpid() > 0 ? 0 : -1; }

static int sc_clock_gettime(void) {
    struct timespec ts;
    return clock_gettime(CLOCK_MONOTONIC, &ts);
}

// Whether this process was handed a vDSO at all.
//
// The timing above says how expensive a clock read is; this says whether the
// kernel even offered a way to avoid the trap. The two answer different
// questions and the distinction matters when a number fails to move: a missing
// AT_SYSINFO_EHDR means the kernel published nothing, while a present one with
// trap-sized timings means the libc looked and declined — a malformed image, or
// a kernel that mapped it but left it disabled. Guessing between those from a
// timing alone costs a boot cycle each way.
#ifndef AT_SYSINFO_EHDR
#define AT_SYSINFO_EHDR 33
#endif

static const char *vdso_presence(void) {
#ifdef __linux__
    unsigned long base = getauxval(AT_SYSINFO_EHDR);
    // Two very different kernels land here: one with no vDSO at all, and one
    // that built and mapped it but declined to enable it (Eclipse withholds it
    // unless CPUID vouches for an invariant TSC, which QEMU cannot do under
    // TCG -- see VDSOFORCE=1). Both make every clock read a syscall, and the
    // rows below cannot tell them apart, so name the second possibility here
    // rather than let a harness artifact read as a missing feature.
    if (!base)
        return "absent (no AT_SYSINFO_EHDR: none built, or the kernel declined it)";
    // musl resolves the symbol itself and silently falls back if it cannot;
    // reporting the mapping base is enough to separate "no vDSO" from "vDSO
    // present but unused".
    static char buf[64];
    snprintf(buf, sizeof buf, "mapped at %#lx", base);
    return buf;
#else
    return "n/a";
#endif
}

static int sc_clock_gettime_real(void) {
    struct timespec ts;
    return clock_gettime(CLOCK_REALTIME, &ts);
}

static int sc_gettimeofday(void) {
    struct timeval tv;
    return gettimeofday(&tv, NULL);
}

static int sc_time(void) {
    return time(NULL) > 0 ? 0 : -1;
}

static volatile sig_atomic_t g_sig_seen;

static void bench_sig_handler(int sig) {
    (void)sig;
    g_sig_seen = 1;
}

static int sc_signal_self(void) {
    g_sig_seen = 0;
    if (raise(SIGUSR1) != 0)
        return -1;
    // Delivery for a self-raised signal happens before `raise` returns, so an
    // unset flag means the handler never ran and the row must be n/a rather
    // than a suspiciously fast number.
    return g_sig_seen ? 0 : -1;
}

static int sc_read1(void) {
    char c;
    return pread(g_devzero, &c, 1, 0) == 1 ? 0 : -1;
}

static int sc_write1(void) {
    return write(g_devnull, "x", 1) == 1 ? 0 : -1;
}

static int sc_open_close(void) {
    int fd = open("/dev/null", O_WRONLY);
    if (fd < 0)
        return -1;
    close(fd);
    return 0;
}

static int sc_fstat(void) {
    struct stat st;
    return fstat(g_devnull, &st);
}

static int sc_stat_path(void) {
    struct stat st;
    return stat("/dev/null", &st);
}

static int sc_sigprocmask(void) {
    sigset_t set;
    sigemptyset(&set);
    return sigprocmask(SIG_SETMASK, &set, NULL);
}

static int sc_sched_yield(void) { return sched_yield(); }

// ---------------------------------------------------------------------------
// Virtual memory  [kernel]
// ---------------------------------------------------------------------------

#define VM_PAGES 256 // 1 MiB worth of 4 KiB pages per mmap round

static int sc_mmap_munmap(void) {
    void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return -1;
    return munmap(p, 4096);
}

static int sc_mprotect(void) {
    static void *p;
    if (!p) {
        p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { p = NULL; return -1; }
    }
    if (mprotect(p, 4096, PROT_READ) != 0)
        return -1;
    return mprotect(p, 4096, PROT_READ | PROT_WRITE);
}

// Minor (demand-zero) fault cost: map a fresh anonymous region and touch one
// byte per page, so every touch is a first-touch fault. Amortises the
// mmap/munmap over VM_PAGES faults, then subtracts nothing — the residual is
// small next to the faults and is disclosed in the label.
static double vm_minor_fault_ns(uint64_t budget_ns) {
    size_t len = (size_t)VM_PAGES * 4096;
    uint64_t t0 = now_ns(), elapsed = 0, faults = 0, mapped_ns = 0;
    while (elapsed < budget_ns) {
        uint64_t m0 = now_ns();
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED)
            return NA;
        uint64_t m1 = now_ns();
        for (int i = 0; i < VM_PAGES; i++)
            p[(size_t)i * 4096] = (unsigned char)i;
        uint64_t m2 = now_ns();
        munmap(p, len);
        faults += VM_PAGES;
        mapped_ns += m2 - m1;
        (void)m0;
        elapsed = now_ns() - t0;
    }
    return faults ? (double)mapped_ns / (double)faults : NA;
}

// Copy-on-write *correctness*: the parent fills a private region with one
// pattern, forks, the child overwrites it with another and exits, and the
// parent then checks its own bytes are untouched — and vice versa.
//
// This is the test that has to pass before any COW speedup means anything. A
// `fork` that shares frames without write-protecting them is *fast* and
// *wrong*: the child's stores land in the parent's memory. Returns 0 on
// success, or the number of the first check that failed.
static int vm_cow_isolation_check(void) {
    size_t pages = 256; // 1 MiB
    size_t len = pages * 4096;
    unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return -1;
    for (size_t i = 0; i < pages; i++)
        p[i * 4096] = 0xA5;
    int fds[2];
    if (pipe(fds) != 0) { munmap(p, len); return -1; }
    pid_t c = fork();
    if (c < 0) { close(fds[0]); close(fds[1]); munmap(p, len); return -1; }
    if (c == 0) {
        close(fds[0]);
        // The child must still see the parent's pre-fork bytes...
        int bad = 0;
        for (size_t i = 0; i < pages; i++)
            if (p[i * 4096] != 0xA5) { bad = 1; break; }
        // ...then overwrite them privately.
        for (size_t i = 0; i < pages; i++)
            p[i * 4096] = 0x5A;
        for (size_t i = 0; i < pages; i++)
            if (p[i * 4096] != 0x5A) { bad = 2; break; }
        ssize_t w = write(fds[1], &bad, sizeof bad);
        (void)w;
        _exit(0);
    }
    close(fds[1]);
    int child_bad = -1;
    ssize_t r = read(fds[0], &child_bad, sizeof child_bad);
    close(fds[0]);
    int st;
    waitpid(c, &st, 0);
    int rc = 0;
    if (r != (ssize_t)sizeof child_bad) rc = 3;
    else if (child_bad != 0) rc = child_bad;
    else {
        // The parent's own bytes must be exactly as it left them.
        for (size_t i = 0; i < pages; i++)
            if (p[i * 4096] != 0xA5) { rc = 4; break; }
    }
    munmap(p, len);
    return rc;
}

// Copy-on-write fault cost: pre-fault a private region in the parent, fork, and
// have the child write one byte per page — every write breaks a COW share. The
// child reports through a pipe. This is the cost every `fork` of a real program
// pays as the child touches its inherited heap and stack.
static double vm_cow_fault_ns(void) {
    size_t pages = 1024; // 4 MiB
    size_t len = pages * 4096;
    unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return NA;
    for (size_t i = 0; i < pages; i++)
        p[i * 4096] = 1; // parent pre-faults, so the child's writes are COW
    int fds[2];
    if (pipe(fds) != 0) { munmap(p, len); return NA; }
    pid_t c = fork();
    if (c < 0) { close(fds[0]); close(fds[1]); munmap(p, len); return NA; }
    if (c == 0) {
        close(fds[0]);
        uint64_t t0 = now_ns();
        for (size_t i = 0; i < pages; i++)
            p[i * 4096] = 2;
        uint64_t dt = now_ns() - t0;
        ssize_t w = write(fds[1], &dt, sizeof dt);
        (void)w;
        _exit(0);
    }
    close(fds[1]);
    uint64_t dt = 0;
    ssize_t r = read(fds[0], &dt, sizeof dt);
    close(fds[0]);
    int st;
    waitpid(c, &st, 0);
    munmap(p, len);
    if (r != (ssize_t)sizeof dt)
        return NA;
    return (double)dt / (double)pages;
}

// ---------------------------------------------------------------------------
// Scheduler / IPC  [kernel]  — the section that decides how the system feels
// ---------------------------------------------------------------------------

// One pipe round trip: write a byte, block until the peer answers. The peer has
// to be woken, scheduled and run for this to complete, so the figure is
// (2 x context switch) + (4 x pipe syscall). It is the single best proxy for
// "how long does anything wait to be given a CPU".

struct pingpong { int a[2], b[2]; };

static int pingpong_open(struct pingpong *pp) {
    if (pipe(pp->a) != 0)
        return -1;
    if (pipe(pp->b) != 0) {
        close(pp->a[0]); close(pp->a[1]);
        return -1;
    }
    return 0;
}

static void pingpong_close(struct pingpong *pp) {
    close(pp->a[0]); close(pp->a[1]);
    close(pp->b[0]); close(pp->b[1]);
}

// Echo loop for the far side: read from `in`, write back to `out`, until EOF.
static void pingpong_echo(int in, int out) {
    char c;
    while (read(in, &c, 1) == 1) {
        if (write(out, &c, 1) != 1)
            break;
    }
}

static void *pingpong_thread(void *arg) {
    struct pingpong *pp = arg;
    pingpong_echo(pp->a[0], pp->b[1]);
    return NULL;
}

// Drive `iters`-bounded round trips over an already-open pair; returns ns per
// round trip, or NA if the peer stopped answering.
static double pingpong_drive(struct pingpong *pp, uint64_t budget_ns) {
    char c = 'x';
    // One untimed round trip so the peer is definitely parked in `read` before
    // the clock starts (otherwise the first iteration measures process startup).
    if (write(pp->a[1], &c, 1) != 1 || read(pp->b[0], &c, 1) != 1)
        return NA;
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    int batch = 1;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        for (int k = 0; k < batch; k++) {
            if (write(pp->a[1], &c, 1) != 1)
                return NA;
            if (read(pp->b[0], &c, 1) != 1)
                return NA;
            ops++;
        }
        elapsed = now_ns() - t0;
        // Batch only once a round trip is known to be cheap; at milliseconds
        // per round trip a batch of 32 overshoots the budget many times over.
        if (batch < 32 && ops > 0 && elapsed / ops < 100000)
            batch = 32;
    }
    g_last_ops = ops;
    return ops ? (double)elapsed / (double)ops : NA;
}

static double sched_pipe_rt_proc(uint64_t budget_ns) {
    struct pingpong pp;
    if (pingpong_open(&pp) != 0)
        return NA;
    pid_t c = fork();
    if (c < 0) { pingpong_close(&pp); return NA; }
    if (c == 0) {
        close(pp.a[1]); close(pp.b[0]);
        pingpong_echo(pp.a[0], pp.b[1]);
        _exit(0);
    }
    close(pp.a[0]); close(pp.b[1]);
    double ns = pingpong_drive(&pp, budget_ns);
    close(pp.a[1]); // EOF ends the child's echo loop
    close(pp.b[0]);
    int st;
    waitpid(c, &st, 0);
    return ns;
}

static double sched_pipe_rt_thread(uint64_t budget_ns) {
    struct pingpong pp;
    if (pingpong_open(&pp) != 0)
        return NA;
    pthread_t th;
    if (pthread_create(&th, NULL, pingpong_thread, &pp) != 0) {
        pingpong_close(&pp);
        return NA;
    }
    double ns = pingpong_drive(&pp, budget_ns);
    close(pp.a[1]);
    pthread_join(th, NULL);
    close(pp.a[0]); close(pp.b[0]); close(pp.b[1]);
    return ns;
}

// Thread creation: pthread_create + join of a no-op thread. Everything a
// threaded program pays before its thread runs: kernel thread object, stack
// mapping, TLS setup, wake, and the join handshake on exit.
static void *noop_thread_fn(void *a) { return a; }

static double sched_thread_spawn_ns(uint64_t budget_ns) {
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pthread_t t;
        if (pthread_create(&t, NULL, noop_thread_fn, NULL) != 0)
            return NA;
        pthread_join(t, NULL);
        ops++;
        elapsed = now_ns() - t0;
    }
    return ops ? (double)elapsed / (double)ops : NA;
}

// Futex wake round trip between two threads. This is the primitive under every
// mutex, condvar and `park` in every threaded program -- musl's pthreads are
// futex all the way down -- so its round trip bounds how fast two threads can
// hand work to each other. Distinct from the pipe row: no file descriptors, no
// data copy, just sleep/wake through the kernel.
// FUTEX_PRIVATE_FLAG. Without it every call below takes the INTER-PROCESS
// path, which has to resolve the word through the address space -- on Eclipse
// that walks the VMAR twice per operation. musl's pthread_mutex, pthread_cond
// and sem_t all set the private flag, so a bench that leaves it off reports a
// cost almost no real program pays, in the row people read as "a futex wake".
// Private is therefore the default here; the shared variants are kept so the
// difference can be measured and shown rather than silently chosen.
#define ECL_FUTEX_PRIVATE 128
#define ECL_FUTEX_WAIT (0 | ECL_FUTEX_PRIVATE)
#define ECL_FUTEX_WAKE (1 | ECL_FUTEX_PRIVATE)
#define ECL_FUTEX_WAIT_SHARED 0
#define ECL_FUTEX_WAKE_SHARED 1

static volatile int g_fx_ping, g_fx_pong;
static volatile int g_fx_stop;

static long futex_op(volatile int *uaddr, int op, int val) {
    return syscall(SYS_futex, uaddr, op, val, NULL, NULL, 0);
}

static void *futex_echo_thread(void *arg) {
    (void)arg;
    for (;;) {
        while (!__atomic_exchange_n(&g_fx_ping, 0, __ATOMIC_ACQ_REL)) {
            if (g_fx_stop)
                return NULL;
            futex_op(&g_fx_ping, ECL_FUTEX_WAIT, 0);
        }
        __atomic_store_n(&g_fx_pong, 1, __ATOMIC_RELEASE);
        futex_op(&g_fx_pong, ECL_FUTEX_WAKE, 1);
    }
}

static double sched_futex_rt_ns(uint64_t budget_ns) {
    g_fx_ping = g_fx_pong = 0;
    g_fx_stop = 0;
    pthread_t t;
    if (pthread_create(&t, NULL, futex_echo_thread, NULL) != 0)
        return NA;
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        __atomic_store_n(&g_fx_ping, 1, __ATOMIC_RELEASE);
        futex_op(&g_fx_ping, ECL_FUTEX_WAKE, 1);
        while (!__atomic_exchange_n(&g_fx_pong, 0, __ATOMIC_ACQ_REL))
            futex_op(&g_fx_pong, ECL_FUTEX_WAIT, 0);
        ops++;
        elapsed = now_ns() - t0;
    }
    g_fx_stop = 1;
    futex_op(&g_fx_ping, ECL_FUTEX_WAKE, 1);
    pthread_join(t, NULL);
    return ops ? (double)elapsed / (double)ops : NA;
}

// The same ping-pong as the pipe row, over an AF_UNIX socketpair. Sockets and
// pipes take different kernel paths (socket buffers and their wakeups against
// the pipe machinery), and a desktop is glued together with UNIX sockets --
// Wayland, D-Bus, X11 -- so a slow one is felt even when pipes are fast.
static double sched_socketpair_rt_proc(uint64_t budget_ns) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0)
        return NA;
    pid_t c = fork();
    if (c < 0) {
        close(sv[0]); close(sv[1]);
        return NA;
    }
    if (c == 0) {
        close(sv[0]);
        pingpong_echo(sv[1], sv[1]);
        _exit(0);
    }
    close(sv[1]);
    // A socketpair is full duplex: one fd both writes and reads. Dress it as a
    // `pingpong` so the driver (with its warm-up and batching) is shared.
    struct pingpong pp = {{-1, sv[0]}, {sv[0], -1}};
    double ns = pingpong_drive(&pp, budget_ns);
    close(sv[0]); // EOF ends the child's echo loop
    int st;
    waitpid(c, &st, 0);
    return ns;
}

// Pipe throughput with 64 KiB writes: the latency rows move one byte, this
// moves bulk. `cmd | cmd` pipelines and anything that streams through a pipe
// run at this speed, and it exercises a different path than the round trip --
// big copies in and out of the pipe buffer, and how often the reader wakes.
static double sched_pipe_bw_mbs(uint64_t budget_ns) {
    int p[2];
    if (pipe(p) != 0)
        return NA;
    pid_t c = fork();
    if (c < 0) {
        close(p[0]); close(p[1]);
        return NA;
    }
    static char buf[1 << 16];
    if (c == 0) {
        close(p[1]);
        while (read(p[0], buf, sizeof buf) > 0)
            ;
        _exit(0);
    }
    close(p[0]);
    memset(buf, 0x5a, sizeof buf);
    uint64_t t0 = now_ns(), elapsed = 0, bytes = 0;
    while ((elapsed < budget_ns || bytes < (4u << 20)) && elapsed < g_max_ns) {
        if (write(p[1], buf, sizeof buf) != (ssize_t)sizeof buf)
            break;
        bytes += sizeof buf;
        elapsed = now_ns() - t0;
    }
    close(p[1]); // EOF stops the reader
    int st;
    waitpid(c, &st, 0);
    if (!bytes || !elapsed)
        return NA;
    return (double)bytes / ((double)elapsed / 1e9) / 1e6;
}

// Sleep overshoot: ask for `req_us`, measure what you actually got. The excess
// is timer granularity plus the delay between the timer firing and this thread
// being put back on a CPU. Measured twice — idle and under load — because the
// difference between the two IS the interactive latency the user experiences.
//
// Returns the mean and, through `worst`, the largest single overshoot. The
// worst case is the one that matters: a scheduler that usually responds in
// 80 us but occasionally makes you wait out a full 20 ms timeslice is
// experienced as stuttering, and a mean would hide that entirely.
static double sleep_overshoot_us(uint64_t req_us, int rounds, double *worst) {
    struct timespec req = {
        .tv_sec = (time_t)(req_us / 1000000),
        .tv_nsec = (long)((req_us % 1000000) * 1000),
    };
    double total = 0, max = 0;
    int ok = 0;
    for (int i = 0; i < rounds; i++) {
        uint64_t t0 = now_ns();
        if (nanosleep(&req, NULL) != 0 && errno != EINTR)
            return NA;
        uint64_t dt = now_ns() - t0;
        double over = ((double)dt - (double)req_us * 1000.0) / 1000.0;
        if (over < 0)
            over = 0;
        total += over;
        if (over > max)
            max = over;
        ok++;
    }
    if (worst)
        *worst = ok ? max : NA;
    return ok ? total / ok : NA;
}

// Spawn `n` CPU-bound child processes that spin until killed. Returns how many
// actually started; fills `pids`.
static int load_start(pid_t *pids, int n) {
    int started = 0;
    for (int i = 0; i < n; i++) {
        pid_t c = fork();
        if (c < 0)
            break;
        if (c == 0) {
            // Pure userspace spin: no syscalls, so the only way this child ever
            // gives up its CPU is if the kernel takes it away. That is exactly
            // the pressure we want to put on the scheduler.
            volatile uint64_t x = 1;
            for (;;)
                x = x * 6364136223846793005ull + 1442695040888963407ull;
        }
        pids[started++] = c;
    }
    return started;
}

static void load_stop(pid_t *pids, int n) {
    for (int i = 0; i < n; i++)
        kill(pids[i], SIGKILL);
    for (int i = 0; i < n; i++) {
        int st;
        waitpid(pids[i], &st, 0);
    }
}

// ---------------------------------------------------------------------------
// SMP scaling  [kernel]
// ---------------------------------------------------------------------------
//
// N threads each run the same dependent-MAC loop that the CPU section runs
// alone. Perfect scaling means N threads finish N times as much work. Anything
// less is the kernel: lock contention, timer overhead, a scheduler that will
// not spread the threads, or CPUs it never brought online.

// Threads spin on this until every one of them exists, so the measured window
// starts with the full width already running. Without the gate, thread
// creation (which on a loaded emulated machine can take longer than the whole
// budget) ate the measurement: threads created late found the deadline already
// past, did zero iterations, and the section reported 0 Mops/s.
static volatile int g_smp_go;
static volatile uint64_t g_smp_deadline_ns;

struct spin_arg {
    uint64_t iters;
};

static void *spin_thread(void *arg) {
    struct spin_arg *a = arg;
    while (!g_smp_go)
        sched_yield();
    uint64_t x = 0x9e3779b97f4a7c15ull, n = 0;
    while (now_ns() < g_smp_deadline_ns) {
        for (int k = 0; k < 4096; k++)
            x = x * 6364136223846793005ull + 1442695040888963407ull;
        n += 4096;
    }
    g_sink += x;
    a->iters = n;
    return NULL;
}

// Aggregate Mops/s across `n` threads over `budget_ns`. NA if the full width
// could not be started — a narrower run would understate scaling, not measure it.
static double smp_aggregate(int n, uint64_t budget_ns) {
    if (n < 1)
        return NA;
    pthread_t *th = calloc((size_t)n, sizeof *th);
    struct spin_arg *args = calloc((size_t)n, sizeof *args);
    if (!th || !args) { free(th); free(args); return NA; }
    g_smp_go = 0;
    int started = 0;
    for (int i = 0; i < n; i++) {
        args[i].iters = 0;
        if (pthread_create(&th[i], NULL, spin_thread, &args[i]) != 0)
            break;
        started++;
    }
    if (started != n) {
        // Release whatever did start so the joins below cannot hang.
        g_smp_deadline_ns = now_ns();
        g_smp_go = 1;
        for (int i = 0; i < started; i++)
            pthread_join(th[i], NULL);
        free(th);
        free(args);
        return NA;
    }
    uint64_t t0 = now_ns();
    g_smp_deadline_ns = t0 + budget_ns;
    g_smp_go = 1;
    uint64_t total = 0;
    for (int i = 0; i < started; i++) {
        pthread_join(th[i], NULL);
        total += args[i].iters;
    }
    uint64_t dt = now_ns() - t0;
    free(th);
    free(args);
    if (dt == 0 || total == 0)
        return NA;
    return (double)total * 1e9 / (double)dt / 1e6;
}

// ---------------------------------------------------------------------------
// SMP kernel paths  [kernel] — contention, TLB shootdowns, cross-CPU wakes
// ---------------------------------------------------------------------------
//
// The scaling block above runs pure userspace ALU: it proves the CPUs exist
// and the scheduler spreads threads, and nothing else. Everything an SMP
// kernel actually has to get right — syscall entry that does not serialize,
// VM locks that do not collapse under parallel mappers, TLB shootdowns that
// do not stall the world, futex queues under contention, wakes that cross
// CPUs — lives below, measured as x1 against xN so every row carries its own
// baseline.

static volatile int g_smpk_go, g_smpk_stop;

struct smpk_arg {
    uint64_t ops;
    int (*fn)(int);
    int idx;
};

static void *smpk_worker(void *argp) {
    struct smpk_arg *a = argp;
    while (!g_smpk_go)
        sched_yield();
    uint64_t n = 0;
    while (!g_smpk_stop) {
        if (a->fn(a->idx) < 0)
            break;
        n++;
    }
    a->ops = n;
    return NULL;
}

// Aggregate ops/s of `n` workers hammering `fn` for `budget_ns`. NA unless the
// full width started — a narrower run would flatter the contention rows.
static double smpk_rate(int n, int (*fn)(int), uint64_t budget_ns) {
    enum { MAXW = 64 };
    pthread_t th[MAXW];
    struct smpk_arg args[MAXW];
    if (n < 1 || n > MAXW)
        return NA;
    g_smpk_go = 0;
    g_smpk_stop = 0;
    int made = 0;
    for (int i = 0; i < n; i++) {
        args[i].ops = 0;
        args[i].fn = fn;
        args[i].idx = i;
        if (pthread_create(&th[i], NULL, smpk_worker, &args[i]) != 0)
            break;
        made++;
    }
    uint64_t t0 = now_ns();
    g_smpk_go = 1;
    if (made == n) {
        uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
        struct timespec ts = {(time_t)(ns / 1000000000ull),
                              (long)(ns % 1000000000ull)};
        nanosleep(&ts, NULL);
    }
    g_smpk_stop = 1;
    uint64_t total = 0;
    for (int i = 0; i < made; i++) {
        pthread_join(th[i], NULL);
        total += args[i].ops;
    }
    uint64_t el = now_ns() - t0;
    if (made != n || el == 0 || total == 0)
        return NA;
    return (double)total * 1e9 / (double)el;
}

static int smpk_getpid_op(int idx) {
    (void)idx;
    return getpid() > 0 ? 0 : -1;
}

// One op = map 64 KiB, touch it, unmap: the address-space lock and the page
// tables, exercised from every CPU at once.
static int smpk_mmap_op(int idx) {
    (void)idx;
    unsigned char *p = mmap(NULL, 1 << 16, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return -1;
    p[0] = 1;
    return munmap(p, 1 << 16);
}

// One op = 64 minor faults (256 KiB touched page by page) plus the teardown.
// The frame allocator and fault path under parallel load.
static int smpk_fault_op(int idx) {
    (void)idx;
    size_t len = 64 * 4096;
    unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return -1;
    for (size_t o = 0; o < len; o += 4096)
        p[o] = (unsigned char)o;
    return munmap(p, len);
}

// One shared mutex, zero-length critical section: the futex sleep/wake path at
// its most contended. musl's pthread_mutex is futex all the way down.
static pthread_mutex_t g_smpk_mutex = PTHREAD_MUTEX_INITIALIZER;
static volatile uint64_t g_smpk_mutex_word;

static int smpk_mutex_op(int idx) {
    (void)idx;
    pthread_mutex_lock(&g_smpk_mutex);
    g_smpk_mutex_word++;
    pthread_mutex_unlock(&g_smpk_mutex);
    return 0;
}

// ns per mprotect PTE flip with `peers` sibling threads spinning on other
// CPUs. Each flip must invalidate the sibling CPUs' TLBs; with zero peers the
// kernel may skip idle CPUs entirely, so the DIFFERENCE between the two rows
// is the cross-CPU shootdown cost — IPI round trips and ack waits — isolated
// from the local page-table work.
static volatile int g_smpk_spin_stop;

static void *smpk_spinner(void *arg) {
    (void)arg;
    volatile uint64_t x = 1;
    while (!g_smpk_spin_stop)
        x = x * 6364136223846793005ull + 1442695040888963407ull;
    return NULL;
}

static double smpk_mprotect_ns(int peers, uint64_t budget_ns) {
    enum { MAXP = 63 };
    pthread_t th[MAXP];
    if (peers < 0)
        peers = 0;
    if (peers > MAXP)
        peers = MAXP;
    unsigned char *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED)
        return NA;
    page[0] = 1; // committed: the flip has a live PTE to change
    g_smpk_spin_stop = 0;
    int made = 0;
    for (int i = 0; i < peers; i++)
        if (pthread_create(&th[i], NULL, smpk_spinner, NULL) == 0)
            made++;
    struct timespec settle = {0, 80 * 1000 * 1000};
    nanosleep(&settle, NULL); // let the spinners actually occupy their CPUs
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    int failed = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        if (mprotect(page, 4096, PROT_READ) != 0 ||
            mprotect(page, 4096, PROT_READ | PROT_WRITE) != 0) {
            failed = 1;
            break;
        }
        page[0]++;
        ops += 2;
        elapsed = now_ns() - t0;
    }
    g_smpk_spin_stop = 1;
    for (int i = 0; i < made; i++)
        pthread_join(th[i], NULL);
    munmap(page, 4096);
    if (failed || made != peers || ops == 0)
        return NA;
    return (double)elapsed / (double)ops;
}

// Pipe round trip with both ends pinned: same CPU against adjacent CPUs. The
// same-CPU case is a pure context-switch ping-pong (no IPI, hot cache); the
// cross-CPU case pays the remote wake. The ratio is what moving a wake across
// the machine costs.
static int smpk_pin_self(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return pthread_setaffinity_np(pthread_self(), sizeof set, &set);
}

struct smpk_pinned {
    struct pingpong pp;
    int cpu;
    volatile int pin_failed;
};

static void *smpk_pinned_echo(void *arg) {
    struct smpk_pinned *p = arg;
    if (p->cpu >= 0 && smpk_pin_self(p->cpu) != 0)
        p->pin_failed = 1;
    pingpong_echo(p->pp.a[0], p->pp.b[1]);
    return NULL;
}

static double smpk_pipe_rt_pinned(int cpu_a, int cpu_b, int ncpu,
                                  uint64_t budget_ns) {
    struct smpk_pinned c;
    if (pingpong_open(&c.pp) != 0)
        return NA;
    c.cpu = cpu_b;
    c.pin_failed = 0;
    int self_ok = smpk_pin_self(cpu_a) == 0;
    pthread_t t;
    if (pthread_create(&t, NULL, smpk_pinned_echo, &c) != 0) {
        pingpong_close(&c.pp);
        return NA;
    }
    double ns = self_ok ? pingpong_drive(&c.pp, budget_ns) : NA;
    close(c.pp.a[1]); // EOF ends the echo loop
    pthread_join(t, NULL);
    close(c.pp.a[0]);
    close(c.pp.b[0]);
    close(c.pp.b[1]);
    // Unpin so later sections are not accidentally confined to one CPU.
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < ncpu && i < CPU_SETSIZE; i++)
        CPU_SET(i, &all);
    pthread_setaffinity_np(pthread_self(), sizeof all, &all);
    if (!self_ok || c.pin_failed)
        return NA;
    return ns;
}

// Aggregate forks/s with `nproc` worker processes forking in parallel: the
// whole copy-on-write machinery — hidden-node creation, mapping walks, the
// family locks — colliding from every CPU at once.
static double smpk_forks_per_s(int nproc, uint64_t budget_ns) {
    enum { MAXF = 64 };
    if (nproc < 1 || nproc > MAXF)
        return NA;
    uint64_t *counts = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                            MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (counts == MAP_FAILED)
        return NA;
    memset(counts, 0, 4096);
    uint64_t t0 = now_ns();
    uint64_t deadline = t0 + (budget_ns < g_max_ns ? budget_ns : g_max_ns);
    pid_t kids[MAXF];
    int made = 0;
    for (int i = 0; i < nproc; i++) {
        pid_t c = fork();
        if (c == 0) {
            uint64_t n = 0;
            while (now_ns() < deadline) {
                pid_t g = fork();
                if (g == 0)
                    _exit(0);
                if (g < 0)
                    break;
                int st;
                waitpid(g, &st, 0);
                n++;
            }
            counts[i] = n;
            _exit(0);
        }
        if (c < 0)
            break;
        kids[made++] = c;
    }
    for (int i = 0; i < made; i++) {
        int st;
        waitpid(kids[i], &st, 0);
    }
    uint64_t el = now_ns() - t0;
    uint64_t total = 0;
    for (int i = 0; i < made; i++)
        total += counts[i];
    munmap(counts, 4096);
    if (made != nproc || el == 0 || total == 0)
        return NA;
    return (double)total * 1e9 / (double)el;
}

// Fairness: 2xN identical hogs racing for N CPUs; each counts its progress in
// its own cache line of a shared page. max/min after the window says whether
// the scheduler shares the machine or starves someone — a kernel can post a
// perfect aggregate while one hog gets 10x another's CPU time, and the starved
// one is the interactive shell you are typing into.
static double smpk_fairness_maxmin(int nhogs, uint64_t budget_ns) {
    enum { MAXH = 32, STRIDE = 8 };
    if (nhogs < 2 || nhogs > MAXH)
        return NA;
    volatile uint64_t *counts =
        mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS,
             -1, 0);
    if (counts == MAP_FAILED)
        return NA;
    memset((void *)counts, 0, 4096);
    pid_t kids[MAXH];
    int made = 0;
    for (int i = 0; i < nhogs; i++) {
        pid_t c = fork();
        if (c == 0) {
            volatile uint64_t *mine = &counts[i * STRIDE];
            uint64_t x = 1;
            for (;;) {
                for (int k = 0; k < 2048; k++)
                    x = x * 6364136223846793005ull + 1442695040888963407ull;
                *mine += 1;
            }
        }
        if (c < 0)
            break;
        kids[made++] = c;
    }
    uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
    struct timespec ts = {(time_t)(ns / 1000000000ull),
                          (long)(ns % 1000000000ull)};
    nanosleep(&ts, NULL);
    uint64_t mn = UINT64_MAX, mx = 0;
    for (int i = 0; i < made; i++) {
        uint64_t v = counts[i * STRIDE];
        if (v < mn)
            mn = v;
        if (v > mx)
            mx = v;
    }
    for (int i = 0; i < made; i++)
        kill(kids[i], SIGKILL);
    for (int i = 0; i < made; i++) {
        int st;
        waitpid(kids[i], &st, 0);
    }
    munmap((void *)counts, 4096);
    if (made != nhogs || mn == 0)
        return NA;
    return (double)mx / (double)mn;
}

// ---------------------------------------------------------------------------
// PreemptiveScheduler internals  [kernel]
// ---------------------------------------------------------------------------
//
// The SCHEDULER / IPC section above measures what a *user* feels: how long a
// woken task waits for a CPU. It does not say which of the scheduler's
// mechanisms produced that number, so a change to one of them cannot be
// attributed, only observed.
//
// Eclipse's scheduler is not a Linux runqueue: `vendor/PreemptiveScheduler` is
// a per-CPU async executor, and the policy in `zircon-object`'s thread code
// decides only the *length* of a timeslice, never which task runs next. That
// shape has its own distinct mechanisms, each of which can be wrong on its own
// while every row above still looks reasonable:
//
//   * the timeslice floor (RUN_TO_PARITY) that stops a high-frequency waker
//     from preempting the running thread every few microseconds;
//   * the slice remainder (EEVDF's lag) that stops a thread which parks just
//     short of its slice from renewing it forever and never being preempted;
//   * work stealing, and the hint that lets an idle CPU decide there is
//     nothing to steal without taking a single lock;
//   * per-task affinity, and the cross-CPU kick that has to find the right
//     CPU when a task cannot run where it was woken;
//   * the weak executors and 32 KiB stacks left behind by a task that yields
//     in the middle of a poll;
//   * the timer rearm the deadline path does per sleep.
//
// Each probe below isolates one of them from userspace and, where the kernel
// publishes the matching counter, prints the kernel's own view of the same
// event next to it. Every probe runs on both kernels; the counter rows need
// `/proc/perf/kernel` and are reported `n/a` where it does not exist, which on
// Linux is every one of them.

// --- kernel counters -------------------------------------------------------
//
// `/proc/perf/kernel` is a text report, not a stable ABI, so the numbers are
// located by the label that introduces the line and then by position within
// it. A line that is renamed or reordered makes the row read `n/a`; it never
// makes it read a wrong number from a neighbouring field, because a label that
// does not match is not found at all.

#define KSTAT_BUF 131072
static char g_kstat_a[KSTAT_BUF], g_kstat_b[KSTAT_BUF];
// Set once the first read succeeds, so the section can say "this kernel has no
// counters" rather than printing a wall of n/a with no explanation.
static int g_kstat_ok = -1;

static int kstat_snapshot(char *buf, size_t n) {
    int fd = open("/proc/perf/kernel", O_RDONLY);
    if (fd < 0) {
        buf[0] = 0;
        return 0;
    }
    size_t got = 0;
    for (;;) {
        ssize_t r = read(fd, buf + got, n - 1 - got);
        if (r <= 0)
            break;
        got += (size_t)r;
        if (got >= n - 1)
            break;
    }
    close(fd);
    buf[got] = 0;
    return got > 0;
}

// The `idx`-th number (0-based) on the first line whose first non-blank
// characters are `label`. Returns NA when the label or the field is absent.
static double kstat_field(const char *buf, const char *label, int idx) {
    size_t llen = strlen(label);
    const char *p = buf;
    while (*p) {
        const char *bol = p;
        while (*p == ' ' || *p == '\t')
            p++;
        if (strncmp(p, label, llen) == 0) {
            // Found the line. Walk its fields to `idx`.
            const char *q = p + llen;
            int seen = 0;
            while (*q && *q != '\n') {
                if ((*q >= '0' && *q <= '9')) {
                    const char *num = q;
                    double v = strtod(num, (char **)&q);
                    if (seen == idx)
                        return v;
                    seen++;
                    continue;
                }
                q++;
            }
            return NA;
        }
        // Not this line: advance past it.
        p = bol;
        while (*p && *p != '\n')
            p++;
        if (*p == '\n')
            p++;
    }
    return NA;
}

// after - before for one field, or NA if either end is missing. Counters in
// this report are monotonic since boot, so a negative delta means the field
// moved and the subtraction is meaningless; it is reported as n/a.
static double kstat_delta(const char *label, int idx) {
    double a = kstat_field(g_kstat_a, label, idx);
    double b = kstat_field(g_kstat_b, label, idx);
    if (a < 0 || b < 0 || b < a)
        return NA;
    return b - a;
}

// A counter delta divided by the number of userspace operations that caused
// it: the unit that makes two machines comparable. `ops <= 0` is n/a.
static double kstat_per_op(const char *label, int idx, double ops) {
    double d = kstat_delta(label, idx);
    if (d < 0 || ops <= 0)
        return NA;
    return d / ops;
}

// Join `t`, but only once it has actually finished; otherwise abandon it.
//
// Every wait in this section is bounded three ways, and then the probes
// joined unconditionally -- which is just another unbounded wait on a
// scheduler that may never run the thread again. On a kernel where a starved
// thread never observes `stop`, `pthread_join` blocks and the suite sits on
// one row with every later row unmeasured. Detaching instead lets the thread
// be reclaimed if it ever does finish, costs this probe its number, and lets
// the run continue. Returns 0 when the thread was abandoned.
static int join_or_abandon(pthread_t t, const volatile int *done,
                           unsigned secs) {
    // Timed off the clock, not off a count of sleeps. The watchdog in this
    // section installs SIGALRM deliberately WITHOUT SA_RESTART, so a
    // nanosleep() here can come back early with EINTR -- and counting sleeps
    // would then abandon a thread that still had most of its grace period
    // left, turning a slow kernel into a missing row.
    uint64_t deadline = now_ns() + (uint64_t)secs * 1000000000ull;
    for (;;) {
        if (__atomic_load_n(done, __ATOMIC_ACQUIRE)) {
            pthread_join(t, NULL);
            return 1;
        }
        if (now_ns() >= deadline)
            break;
        struct timespec tick = {0, 10 * 1000 * 1000}; // 10 ms
        nanosleep(&tick, NULL);
    }
    pthread_detach(t);
    return 0;
}

// A control block a probe shares with threads it may have to abandon.
//
// `join_or_abandon` can return while a worker is still running, so the block
// that worker writes through must NOT be the caller's stack frame: the suite
// puts the next probe's locals there, and a stray store would poison every
// later row while reporting a number -- the exact failure mode this section
// exists to detect, arriving silently from the measuring tool instead. So the
// block goes on the heap and is reference-counted: every thread handed it
// drops one reference when it exits, the probe drops its own once it has
// copied the figures out, and whoever drops the last one frees it.
//
// Each such struct carries `int refs`, set to 1 by the probe that allocates it.
// Taken BEFORE pthread_create, so a thread that exits at once cannot drop the
// count to zero before its reference has been accounted for; given back when
// the create fails.
#define CTL_LEND(c) __atomic_fetch_add(&(c)->refs, 1, __ATOMIC_RELAXED)
#define CTL_DROP(c)                                                           \
    do {                                                                      \
        if (__atomic_sub_fetch(&(c)->refs, 1, __ATOMIC_ACQ_REL) == 0)          \
            free(c);                                                          \
    } while (0)

// Wait for a start gate without using sched_yield().
//
// Every gate in this section used to yield, and the yield hand-off probe above
// shows why that is unsafe here: a kernel can leave a voluntary yielder parked
// indefinitely, which releases the gated thread late and makes its rate read
// low for reasons that have nothing to do with the mechanism under test. A
// short sleep parks the waiter just as effectively without depending on how
// yield is implemented, and is bounded so a gate that never opens cannot hang
// the suite.
static void gate_wait(const volatile int *go, const volatile int *stop) {
    struct timespec tick = {0, 50 * 1000}; // 50 us
    uint64_t rounds = 0;
    while (!__atomic_load_n(go, __ATOMIC_ACQUIRE)) {
        if (stop && *stop)
            return;
        nanosleep(&tick, NULL);
        // ~30 s at 50 us: a gate that never opens costs this probe its number,
        // never the run.
        if (++rounds > 600000)
            return;
    }
}

// --- timeslice floor (RUN_TO_PARITY) ---------------------------------------
//
// A CPU-bound thread and a thread that wakes thousands of times a second,
// deliberately pinned to the SAME CPU. Without a floor on the timeslice the
// waker preempts the spinner on every single wake-up, and the spinner's
// throughput collapses to whatever is left between the interruptions; with a
// floor the first N microseconds of a slice belong to whoever is running.
//
// The two numbers are reported together on purpose. The floor does not create
// throughput out of nothing: it buys the spinner's progress with the waker's
// latency, so a run that showed only the retained throughput would be
// advertising half of a trade. A kernel can be wrong in either direction here,
// and only the pair says which.

struct parity_ctl {
    int refs;
    volatile int go;
    volatile int stop;
    volatile uint64_t spins;   // spinner's completed work units
    volatile uint64_t wakes;   // waker's completed sleep/wake cycles
    volatile uint64_t late_sum_ns;
    volatile uint64_t late_max_ns;
    int cpu;
    unsigned period_us;
    volatile int pin_failed;
    volatile int spinner_done;
    volatile int waker_done;
};

static void *parity_spinner(void *arg) {
    struct parity_ctl *c = arg;
    if (smpk_pin_self(c->cpu) != 0)
        c->pin_failed = 1;
    gate_wait(&c->go, &c->stop);
    uint64_t x = 0x9e3779b97f4a7c15ull, n = 0;
    while (!c->stop) {
        // No syscall in the loop: the only way this thread loses the CPU is
        // the kernel taking it away, which is the event under test.
        for (int k = 0; k < 1024; k++)
            x = x * 6364136223846793005ull + 1442695040888963407ull;
        n++;
    }
    g_sink += x;
    c->spins = n;
    __atomic_store_n(&c->spinner_done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

static void *parity_waker(void *arg) {
    struct parity_ctl *c = arg;
    if (smpk_pin_self(c->cpu) != 0)
        c->pin_failed = 1;
    struct timespec ts = {0, (long)c->period_us * 1000};
    gate_wait(&c->go, &c->stop);
    uint64_t n = 0, sum = 0, mx = 0;
    while (!c->stop) {
        uint64_t t0 = now_ns();
        nanosleep(&ts, NULL);
        uint64_t late = now_ns() - t0;
        // Overshoot beyond the requested period: the part the kernel added.
        late = late > (uint64_t)c->period_us * 1000
                   ? late - (uint64_t)c->period_us * 1000
                   : 0;
        sum += late;
        if (late > mx)
            mx = late;
        n++;
    }
    c->wakes = n;
    c->late_sum_ns = sum;
    c->late_max_ns = mx;
    __atomic_store_n(&c->waker_done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

// Run the spinner on `cpu` for `budget_ns`, with `period_us > 0` adding a
// co-pinned waker at that period. Returns the spinner's work units per second,
// and fills the waker figures when asked for.
static double parity_run(int cpu, unsigned period_us, uint64_t budget_ns,
                         double *wake_hz, double *late_mean_us,
                         double *late_max_us) {
    struct parity_ctl *c = calloc(1, sizeof *c);
    if (!c)
        return NA;
    c->refs = 1;
    c->cpu = cpu;
    c->period_us = period_us;
    pthread_t sp, wk;
    CTL_LEND(c);
    if (pthread_create(&sp, NULL, parity_spinner, c) != 0) {
        CTL_DROP(c); // the spinner's reference back
        CTL_DROP(c); // and ours
        return NA;
    }
    int have_waker = 0;
    if (period_us > 0) {
        CTL_LEND(c);
        if (pthread_create(&wk, NULL, parity_waker, c) == 0)
            have_waker = 1;
        else
            CTL_DROP(c);
    }
    // Let both threads reach their gate and be placed before timing starts.
    struct timespec settle = {0, 30 * 1000 * 1000};
    nanosleep(&settle, NULL);
    uint64_t t0 = now_ns();
    __atomic_store_n(&c->go, 1, __ATOMIC_RELEASE);
    uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
    struct timespec run = {(time_t)(ns / 1000000000ull),
                           (long)(ns % 1000000000ull)};
    nanosleep(&run, NULL);
    c->stop = 1;
    // A spinner that is still inside its loop has already published its count
    // (it is bumped as it goes), and a waker that never gets the CPU again
    // must not take the run with it.
    int sp_ok = join_or_abandon(sp, &c->spinner_done, 10);
    int wk_ok = have_waker ? join_or_abandon(wk, &c->waker_done, 10) : 1;
    uint64_t dt = now_ns() - t0;
    double out = NA;
    if (!c->pin_failed && dt != 0 && c->spins != 0 && sp_ok) {
        out = (double)c->spins * 1e9 / (double)dt;
        // The spinner's figure stands -- it is what the probe is for -- but an
        // abandoned waker never published its own numbers, so they stay n/a
        // rather than being read out from under a thread still writing them.
        int wk_num = have_waker && wk_ok;
        if (wake_hz)
            *wake_hz = wk_num ? (double)c->wakes * 1e9 / (double)dt : NA;
        if (late_mean_us)
            *late_mean_us = (wk_num && c->wakes)
                                ? (double)c->late_sum_ns / (double)c->wakes /
                                      1000.0
                                : NA;
        if (late_max_us)
            *late_max_us = wk_num ? (double)c->late_max_ns / 1000.0 : NA;
    }
    // Every figure is now a local, so the block can go the moment the last
    // thread still holding it exits.
    CTL_DROP(c);
    return out;
}

// --- slice remainder (EEVDF lag) -------------------------------------------
//
// Two threads on ONE CPU. One is a plain spinner. The other spins for most of
// a timeslice and then parks very briefly — the shape of a thread that does a
// short sleep, or reads a descriptor whose data has already arrived, just
// before its slice would have expired.
//
// If a resumed thread is handed a whole new timeslice, that second thread is
// never preempted: it renews its slice indefinitely and the plain spinner gets
// only the scraps between the parks. If a resumption instead returns the
// remainder of the slice it already had, both threads are preempted on the
// same schedule and the split is even.
//
// The result is a share, so it has no units and no hardware in it: 1.00 means
// the plain spinner got exactly its half, and a figure near 0 means the parking
// thread is starving it. The park is a real sleep rather than a ready `read`
// because a sleep is guaranteed to park on both kernels, while a ready read may
// legitimately complete without ever yielding.

struct lag_ctl {
    int refs;
    volatile int go;
    volatile int stop;
    volatile uint64_t plain;
    volatile uint64_t parker;
    int cpu;
    uint64_t spin_ns;   // how long the parking thread runs between parks
    unsigned park_us;
    volatile int pin_failed;
    volatile int plain_done;
    volatile int parker_done;
};

// Spin for `ns` of wall clock. Time-based, not iteration-based, so the shape
// of the test is the same on a 4 GHz core and inside an emulator — an
// iteration count calibrated for one would be a whole slice on the other.
static uint64_t spin_for_ns(uint64_t ns) {
    uint64_t x = 0x9e3779b97f4a7c15ull, units = 0;
    uint64_t end = now_ns() + ns;
    do {
        for (int k = 0; k < 1024; k++)
            x = x * 6364136223846793005ull + 1442695040888963407ull;
        units++;
    } while (now_ns() < end);
    g_sink += x;
    return units;
}

static void *lag_plain(void *arg) {
    struct lag_ctl *c = arg;
    if (smpk_pin_self(c->cpu) != 0)
        c->pin_failed = 1;
    gate_wait(&c->go, &c->stop);
    uint64_t x = 0x9e3779b97f4a7c15ull, n = 0;
    while (!c->stop) {
        for (int k = 0; k < 1024; k++)
            x = x * 6364136223846793005ull + 1442695040888963407ull;
        n++;
    }
    g_sink += x;
    c->plain = n;
    __atomic_store_n(&c->plain_done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

static void *lag_parker(void *arg) {
    struct lag_ctl *c = arg;
    if (smpk_pin_self(c->cpu) != 0)
        c->pin_failed = 1;
    struct timespec ts = {0, (long)c->park_us * 1000};
    gate_wait(&c->go, &c->stop);
    uint64_t n = 0;
    while (!c->stop) {
        n += spin_for_ns(c->spin_ns);
        nanosleep(&ts, NULL);
    }
    c->parker = n;
    __atomic_store_n(&c->parker_done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

// Share of the work done by the plain spinner, normalised so 1.00 is an even
// split. NA if either thread could not be pinned or did no work.
static double lag_share(int cpu, uint64_t spin_ns, unsigned park_us,
                        uint64_t budget_ns, double *parker_share) {
    if (parker_share)
        *parker_share = NA;
    struct lag_ctl *c = calloc(1, sizeof *c);
    if (!c)
        return NA;
    c->refs = 1;
    c->cpu = cpu;
    c->spin_ns = spin_ns;
    c->park_us = park_us;
    pthread_t a, b;
    CTL_LEND(c);
    if (pthread_create(&a, NULL, lag_plain, c) != 0) {
        CTL_DROP(c);
        CTL_DROP(c);
        return NA;
    }
    CTL_LEND(c);
    if (pthread_create(&b, NULL, lag_parker, c) != 0) {
        CTL_DROP(c); // the parker's reference back
        __atomic_store_n(&c->stop, 1, __ATOMIC_RELEASE);
        __atomic_store_n(&c->go, 1, __ATOMIC_RELEASE);
        join_or_abandon(a, &c->plain_done, 10);
        CTL_DROP(c);
        return NA;
    }
    struct timespec settle = {0, 30 * 1000 * 1000};
    nanosleep(&settle, NULL);
    __atomic_store_n(&c->go, 1, __ATOMIC_RELEASE);
    uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
    struct timespec run = {(time_t)(ns / 1000000000ull),
                           (long)(ns % 1000000000ull)};
    nanosleep(&run, NULL);
    c->stop = 1;
    int a_ok = join_or_abandon(a, &c->plain_done, 10);
    int b_ok = join_or_abandon(b, &c->parker_done, 10);
    double out = NA;
    // Both halves are needed: a share computed from one published count and
    // one still being written is not a share of anything.
    if (a_ok && b_ok && !c->pin_failed) {
        uint64_t total = c->plain + c->parker;
        if (total) {
            // x2 so an even split reads 1.00 rather than 0.50.
            if (parker_share)
                *parker_share = (double)c->parker / (double)total * 2.0;
            out = (double)c->plain / (double)total * 2.0;
        }
    }
    CTL_DROP(c);
    return out;
}

// --- work stealing ---------------------------------------------------------
//
// Every worker is created by a parent confined to CPU 0, so every one of them
// is woken for the first time on that CPU. None is pinned, and all are
// CPU-bound: on a machine with N CPUs the right outcome is one worker per CPU,
// and the only thing that can produce it is the kernel either placing them
// elsewhere at creation or other CPUs noticing the backlog and taking work.
//
// The figure is the time until all N CPUs are occupied, which treats both of
// those routes as the success they are, and the occupancy actually reached,
// which is the row that says a kernel never spread the work at all rather than
// merely being slow about it.
//
// A worker has to widen its own mask first: a new thread inherits its
// creator's affinity, so without that it is confined to CPU 0 for life and
// this probe would measure nothing while reporting a number.

#define STEAL_MAX 64
struct steal_ctl {
    volatile int go;
    volatile int stop;
    // Bit per CPU any worker has been seen on. Updated with an atomic OR:
    // `mask |= bit` is a read-modify-write, and with one worker per CPU racing
    // on it the lost updates made a kernel that spread the work perfectly
    // report half the machine idle.
    uint64_t seen_mask;
    int ncpu;
    volatile int live;   // workers still running
};
static struct steal_ctl g_steal;

static void *steal_worker(void *arg) {
    struct steal_ctl *c = arg;
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < c->ncpu && i < CPU_SETSIZE; i++)
        CPU_SET(i, &all);
    pthread_setaffinity_np(pthread_self(), sizeof all, &all);
    gate_wait(&c->go, &c->stop);
    uint64_t x = 0x9e3779b97f4a7c15ull;
    while (!c->stop) {
        // Enough work between samples to be unambiguously CPU-bound, little
        // enough that the sampling interval does not dominate the latency.
        for (int k = 0; k < 4096; k++)
            x = x * 6364136223846793005ull + 1442695040888963407ull;
        int cpu = sched_getcpu();
        if (cpu >= 0 && cpu < 64)
            __atomic_fetch_or(&c->seen_mask, 1ull << cpu, __ATOMIC_RELAXED);
    }
    g_sink += x;
    __atomic_sub_fetch(&c->live, 1, __ATOMIC_RELAXED);
    return NULL;
}

static int popcount64(uint64_t v) {
    int n = 0;
    while (v) { v &= v - 1; n++; }
    return n;
}

// Microseconds until all `n` CPUs have been observed running a worker, and the
// occupancy reached. Returns -1 when the probe could not be set up at all.
static int steal_spread_us(int n, int spawn_cpu, uint64_t budget_ns,
                           double *full_us, int *occupied) {
    if (n < 2 || n > STEAL_MAX)
        return -1;
    if (sched_getcpu() < 0)
        return -1;
    pthread_t th[STEAL_MAX];
    memset(&g_steal, 0, sizeof g_steal);
    g_steal.ncpu = n;
    int made = 0;
    // Confine the parent first, so every worker is created from that one CPU.
    if (smpk_pin_self(spawn_cpu) != 0)
        return -1;
    for (int i = 0; i < n; i++) {
        // Counted up BEFORE the thread exists: a worker that decrements on
        // exit must never be able to drive the count negative early.
        __atomic_add_fetch(&g_steal.live, 1, __ATOMIC_RELAXED);
        if (pthread_create(&th[i], NULL, steal_worker, &g_steal) != 0) {
            __atomic_sub_fetch(&g_steal.live, 1, __ATOMIC_RELAXED);
            break;
        }
        made++;
    }
    // Release the parent before the workers run: a parent still holding the
    // spawn CPU is one more runnable task on it, and what is under test is how
    // fast the OTHER CPUs come looking.
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < n && i < CPU_SETSIZE; i++)
        CPU_SET(i, &all);
    pthread_setaffinity_np(pthread_self(), sizeof all, &all);
    if (made < 2) {
        g_steal.stop = 1;
        __atomic_store_n(&g_steal.go, 1, __ATOMIC_RELEASE);
        // Give the one worker a bounded chance to observe `stop` and exit
        // before the suite moves on. A worker still spinning here is one more
        // runnable CPU-bound task competing with whatever row comes next, and
        // that row would read low for a reason that has nothing to do with it.
        for (unsigned k = 0; k < 1000u; k++) {
            if (__atomic_load_n(&g_steal.live, __ATOMIC_RELAXED) <= 0)
                break;
            struct timespec tick = {0, 10 * 1000 * 1000};
            nanosleep(&tick, NULL);
        }
        for (int i = 0; i < made; i++) {
            if (__atomic_load_n(&g_steal.live, __ATOMIC_RELAXED) <= 0)
                pthread_join(th[i], NULL);
            else
                pthread_detach(th[i]);
        }
        return -1;
    }
    uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
    uint64_t start = now_ns();
    g_steal.go = 1;
    uint64_t when_full = 0;
    // Poll from here rather than having a worker notice: the observer must not
    // be one of the threads competing for the CPUs it is counting.
    while (now_ns() - start < ns) {
        if (!when_full &&
            popcount64(__atomic_load_n(&g_steal.seen_mask, __ATOMIC_RELAXED)) >=
                made)
            when_full = now_ns();
        struct timespec tick = {0, 1000000}; // 1 ms
        nanosleep(&tick, NULL);
    }
    g_steal.stop = 1;
    // The mask is already published; a worker the kernel will not run again
    // must not hold the suite.
    for (unsigned k = 0; k < 1000u; k++) {
        if (__atomic_load_n(&g_steal.live, __ATOMIC_RELAXED) <= 0)
            break;
        struct timespec tick = {0, 10 * 1000 * 1000};
        nanosleep(&tick, NULL);
    }
    for (int i = 0; i < made; i++) {
        if (__atomic_load_n(&g_steal.live, __ATOMIC_RELAXED) <= 0)
            pthread_join(th[i], NULL);
        else
            pthread_detach(th[i]);
    }
    if (occupied)
        *occupied =
            popcount64(__atomic_load_n(&g_steal.seen_mask, __ATOMIC_RELAXED));
    if (full_us)
        *full_us = when_full ? (double)(when_full - start) / 1000.0 : NA;
    return 0;
}

// --- affinity --------------------------------------------------------------
//
// Narrowing a task's affinity has to do more than store a mask: if the task is
// runnable where it no longer belongs, some CPU in the new mask must be told to
// come and get it. That kick walks the other CPUs' runtimes, so its cost is
// the cost of a scheduler operation and not of a store, and it is paid by
// anything that pins threads — a thread pool sizing itself to the machine, a
// compositor putting its render thread somewhere specific.

static int g_aff_ncpu = 1;
static int g_aff_flip;

static int sc_setaffinity(void) {
    cpu_set_t set;
    CPU_ZERO(&set);
    if (g_aff_flip) {
        // Single CPU: the narrow mask, which is the case that can require a
        // cross-CPU kick.
        CPU_SET(0, &set);
    } else {
        for (int i = 0; i < g_aff_ncpu && i < CPU_SETSIZE; i++)
            CPU_SET(i, &set);
    }
    g_aff_flip = !g_aff_flip;
    return sched_setaffinity(0, sizeof set, &set) == 0 ? 0 : -1;
}

static int sc_getaffinity(void) {
    cpu_set_t set;
    return sched_getaffinity(0, sizeof set, &set) == 0 ? 0 : -1;
}

static int sc_getcpu(void) { return sched_getcpu() >= 0 ? 0 : -1; }

// --- keeping a probe from hanging the suite --------------------------------
//
// This section pins threads together and then waits for one of them to be
// given the CPU, which is exactly the shape that stops dead on a kernel whose
// scheduler has a lost wake-up. A benchmark that runs unattended against a
// kernel under development must not be able to lose thirty working rows
// because the thirty-first probe never returned, so every wait here is bounded
// three independent ways:
//
//   * the wall clock, which is the normal case;
//   * a yield count, because if the CLOCK is the thing that is broken a
//     wall-clock deadline never arrives and the suite stops with no output at
//     all -- which is indistinguishable from a hung kernel;
//   * SIGALRM, because neither of the first two is ever evaluated if the
//     blocking call itself does not return. The handler does nothing: its only
//     job is to make a blocked syscall fail with EINTR so the loop around it
//     gets to look at its own deadlines again. It is installed WITHOUT
//     SA_RESTART for that reason -- with the flag the kernel would restart the
//     call and the signal would change nothing.
//
// Hitting the second or third bound is reported as what it is rather than
// folded into a plain "n/a", because "this operation is unsupported" and "this
// operation never came back" are different findings.

#define YIELD_CAP 20000000ull

static volatile sig_atomic_t g_watchdog_fired;

static void bench_watchdog(int sig) {
    (void)sig;
    g_watchdog_fired = 1;
}

// Arm (secs > 0) or disarm (secs == 0) the watchdog. Returns 0 if a watchdog
// could not be installed, in which case the clock and the yield count are the
// only bounds left and the caller carries on with them.
static int watchdog_set(unsigned secs) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = bench_watchdog;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0; // deliberately NOT SA_RESTART
    if (sigaction(SIGALRM, &sa, NULL) != 0)
        return 0;
    if (secs == 0)
        g_watchdog_fired = 0;
    alarm(secs);
    return 1;
}

// --- yield hand-off --------------------------------------------------------
//
// Two threads on one CPU, each giving the CPU straight back. No timer, no
// descriptor, no wake-up: just the executor being asked to pick the next task
// and switch to it. It is the floor under every other number in this section —
// whatever a pipe round trip costs, this is the part of it that is pure
// dispatch, and the difference is the pipe and the wake.

struct yield_ctl {
    int refs;
    volatile int go;
    volatile int stop;
    volatile uint64_t turn;   // whose turn it is: 0 = a, 1 = b
    volatile uint64_t ops;
    int cpu;
    volatile int pin_failed;
    volatile int peer_done;
};

static void *yield_peer(void *arg) {
    struct yield_ctl *c = arg;
    if (smpk_pin_self(c->cpu) != 0)
        c->pin_failed = 1;
    gate_wait(&c->go, &c->stop);
    uint64_t spins;
    while (!c->stop) {
        spins = 0;
        while (c->turn != 1 && !c->stop) {
            sched_yield();
            if (++spins > YIELD_CAP) {
                // Give up without claiming to be done -- the probe reads
                // `peer_done` to decide whether it may trust the block -- but
                // hand the reference back, or nothing ever frees it.
                CTL_DROP(c);
                return NULL;
            }
        }
        if (c->stop)
            break;
        c->turn = 0;
    }
    __atomic_store_n(&c->peer_done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

// Nanoseconds per hand-off (one full there-and-back is counted as two).
//
// `*why` is set to NULL on success and otherwise to the reason there is no
// number, because "n/a" alone cannot distinguish a kernel that refuses the
// affinity call from one where a co-pinned peer is never given the CPU at all
// — and those are a missing feature and a starvation bug respectively.
static double yield_handoff_ns(int cpu, uint64_t budget_ns, const char **why) {
    if (why)
        *why = NULL;
    struct yield_ctl *c = calloc(1, sizeof *c);
    if (!c)
        return NA;
    c->refs = 1;
    c->cpu = cpu;
    if (smpk_pin_self(cpu) != 0) {
        if (why) *why = "this kernel would not pin a thread to one CPU";
        CTL_DROP(c);
        return NA;
    }
    pthread_t t;
    CTL_LEND(c);
    if (pthread_create(&t, NULL, yield_peer, c) != 0) {
        if (why) *why = "could not create the peer thread";
        CTL_DROP(c);
        CTL_DROP(c);
        return NA;
    }
    struct timespec settle = {0, 20 * 1000 * 1000};
    nanosleep(&settle, NULL);
    __atomic_store_n(&c->go, 1, __ATOMIC_RELEASE);
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
    // Armed ONCE, around the whole loop, and generously: the alarm is a
    // backstop against a call that never returns, not a per-round budget.
    // Arming it per round puts a sigaction and an alarm between every
    // hand-off, which measurably inflates the figure being measured.
    g_watchdog_fired = 0;
    watchdog_set((unsigned)(ns / 1000000000ull) + 30);
    while (elapsed < ns || ops < MIN_SAMPLES) {
        c->turn = 1;
        uint64_t guard = now_ns() + 2000000000ull;
        uint64_t spins = 0;
        const char *bound = NULL;
        while (c->turn != 0) {
            sched_yield();
            if (++spins > YIELD_CAP) {
                // The yield count ran out while the clock said there was time
                // left. Either the clock is not advancing or yielding is not
                // getting the peer onto this CPU; the next row's timings say
                // which, and both are worth knowing.
                bound = "20M yields without the co-pinned peer running";
                break;
            }
            if (g_watchdog_fired) {
                // The yield itself did not come back until a signal made it.
                bound = "sched_yield() blocked until a signal interrupted it";
                break;
            }
            if (now_ns() > guard) {
                bound = "the co-pinned peer never took the CPU in 2 s";
                break;
            }
        }
        if (bound) {
            if (why)
                *why = bound;
            watchdog_set(0);
            c->stop = 1;
            c->turn = 0;
            // The peer may be the thread the kernel is not running; do not
            // wait on it indefinitely to confirm that.
            join_or_abandon(t, &c->peer_done, 5);
            CTL_DROP(c);
            return NA;
        }
        ops += 2;
        elapsed = now_ns() - t0;
        if (elapsed >= g_max_ns)
            break;
    }
    watchdog_set(0);
    c->stop = 1;
    c->turn = 1;
    join_or_abandon(t, &c->peer_done, 5);
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < g_aff_ncpu && i < CPU_SETSIZE; i++)
        CPU_SET(i, &all);
    pthread_setaffinity_np(pthread_self(), sizeof all, &all);
    int pin_failed = c->pin_failed;
    CTL_DROP(c);
    if (pin_failed) {
        if (why) *why = "this kernel would not pin a thread to one CPU";
        return NA;
    }
    if (ops == 0)
        return NA;
    return (double)elapsed / (double)ops;
}

// --- cross-CPU wake coalescing ---------------------------------------------
//
// Making a task runnable on another CPU ends in a request for that CPU to
// reschedule, and those requests are coalesced per CPU: waking eight tasks on
// one CPU should not cost eight times what waking one costs, because after the
// first the CPU has already been told.
//
// One waker, M sleepers all confined to a different single CPU, woken in a
// burst and then waited for. The cost per sleeper at M=1 against M=many is the
// coalescing, and a ratio near 1 means each wake is paying full price.

#define WAKE_MAX 32
struct wake_slot {
    volatile int word;      // futex: 0 = sleep, 1 = go
    volatile int done;
    int cpu;
    volatile int stop;
    volatile int pin_failed;
    volatile int done_exit;
    int wait_op;            // private or shared: see ECL_FUTEX_PRIVATE
    int wake_op;
};
static struct wake_slot g_wake[WAKE_MAX];

static void *wake_sleeper(void *arg) {
    struct wake_slot *s = arg;
    if (smpk_pin_self(s->cpu) != 0)
        s->pin_failed = 1;
    for (;;) {
        while (s->word == 0 && !s->stop)
            futex_op(&s->word, s->wait_op, 0);
        if (s->stop)
            break;
        s->word = 0;
        s->done = 1;
    }
    s->done_exit = 1;
    return NULL;
}

// Nanoseconds the waker spends ISSUING one of a burst of `m` cross-CPU wakes.
//
// Only the issuing loop is inside the clock. Waiting for the targets to run
// and park again is done outside it, because M targets confined to one CPU
// necessarily serialize there, and timing that would report the CPU's width as
// though it were the cost of a wake.
static double wake_burst_ns(int m, int waker_cpu, int target_cpu,
                            uint64_t budget_ns, int private_word) {
    if (m < 1 || m > WAKE_MAX)
        return NA;
    pthread_t th[WAKE_MAX];
    int made = 0;
    for (int i = 0; i < m; i++) {
        memset(&g_wake[i], 0, sizeof g_wake[i]);
        g_wake[i].cpu = target_cpu;
        g_wake[i].wait_op =
            private_word ? ECL_FUTEX_WAIT : ECL_FUTEX_WAIT_SHARED;
        g_wake[i].wake_op =
            private_word ? ECL_FUTEX_WAKE : ECL_FUTEX_WAKE_SHARED;
    }
    for (int i = 0; i < m; i++) {
        if (pthread_create(&th[i], NULL, wake_sleeper, &g_wake[i]) != 0)
            break;
        made++;
    }
    double out = NA;
    if (made == m && smpk_pin_self(waker_cpu) == 0) {
        struct timespec settle = {0, 30 * 1000 * 1000};
        nanosleep(&settle, NULL);
        uint64_t t_start = now_ns(), issue_ns = 0, rounds = 0;
        uint64_t ns = budget_ns < g_max_ns ? budget_ns : g_max_ns;
        int bad = 0;
        g_watchdog_fired = 0;
        watchdog_set((unsigned)(ns / 1000000000ull) + 30);
        while ((now_ns() - t_start < ns || rounds < MIN_SAMPLES) && !bad) {
            for (int i = 0; i < m; i++)
                g_wake[i].done = 0;
            uint64_t t0 = now_ns();
            for (int i = 0; i < m; i++) {
                g_wake[i].word = 1;
                futex_op(&g_wake[i].word, g_wake[i].wake_op, 1);
            }
            issue_ns += now_ns() - t0;
            // Untimed: let every target wake, run and park again, so the next
            // round issues into sleepers rather than into already-running
            // threads (which would cost nothing and flatter the burst).
            uint64_t guard = now_ns() + 2000000000ull;
            uint64_t spins = 0;
            for (int i = 0; i < m; i++) {
                while (!g_wake[i].done) {
                    // A short sleep, NOT sched_yield(): this wait is outside
                    // the clock, so parking costs the measurement nothing, and
                    // yielding here made the whole row read n/a on exactly the
                    // kernels whose yield starves its caller -- losing the
                    // coalescing figure on the one machine it mattered for.
                    struct timespec tick = {0, 50 * 1000};
                    nanosleep(&tick, NULL);
                    // Same three bounds as the hand-off probe: a sleeper that
                    // is never woken must cost this probe its number, not the
                    // whole run.
                    if (++spins > 600000 || g_watchdog_fired ||
                        now_ns() > guard) {
                        bad = 1;
                        break;
                    }
                }
                if (bad)
                    break;
            }
            rounds++;
            if (now_ns() - t_start >= g_max_ns)
                break;
        }
        watchdog_set(0);
        if (!bad && rounds)
            out = (double)issue_ns / (double)rounds / (double)m;
        for (int i = 0; i < made; i++)
            if (g_wake[i].pin_failed)
                out = NA;
    }
    for (int i = 0; i < made; i++) {
        g_wake[i].stop = 1;
        g_wake[i].word = 1;
        futex_op(&g_wake[i].word, g_wake[i].wake_op, 1);
    }
    for (int i = 0; i < made; i++)
        join_or_abandon(th[i], &g_wake[i].done_exit, 5);
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < g_aff_ncpu && i < CPU_SETSIZE; i++)
        CPU_SET(i, &all);
    pthread_setaffinity_np(pthread_self(), sizeof all, &all);
    return out;
}

// --- sleep storm -----------------------------------------------------------
//
// Short sleeps, back to back, so the timer path is the whole workload. Each one
// arms a deadline and takes it back again, and the kernel counter says how many
// rearms that actually cost — a number that should be close to one per sleep
// and, when it is not, says the timer is being reprogrammed by something other
// than the sleep that needed it.

static unsigned g_sleep_us = 200;

static int sc_short_sleep(void) {
    struct timespec ts = {0, (long)g_sleep_us * 1000};
    return nanosleep(&ts, NULL) == 0 ? 0 : 0; // EINTR is still a completed arm
}

// --- a deterministic reproducer for a sched_yield() that does not return ---
//
// See the `--yieldstall` block in main() for what this builds and why.

struct ys_lane {
    volatile uint64_t count;
    const volatile int *stop;
    volatile int pin_failed;
    // Set after the gate and before the first sched_yield(). Without it a
    // count of zero cannot distinguish a thread that never ran at all from
    // one that ran and then did not come back out of the call, and those are
    // different bugs.
    volatile int started;
    volatile int pin_rc;       // what sched_setaffinity returned for this thread
    volatile int cpu_seen;     // sched_getcpu() right after pinning
    volatile int cpu_last;     // and the last one it observed
};

#define YS_MAX_NOTIFY 8
struct ys_ctl {
    volatile int stop;
    volatile uint64_t notifies;
    struct ys_lane a, b;
    volatile int notify_started;
    // One slot per notifier rather than one shared field. Several notifier
    // threads wrote the single `notify_cpu` concurrently, which is both a data
    // race and a misleading row: it reported one placement for a group that
    // may not share one.
    volatile int notify_cpu[YS_MAX_NOTIFY];
    int nnotify;
};
static struct ys_ctl g_ys;

static void *ys_yielder(void *arg) {
    struct ys_lane *l = arg;
    // The reproducer confines the PARENT to CPU 0 before creating anything, so
    // a child reporting a different CPU here means the affinity mask was not
    // inherited across thread creation -- and a child reporting CPU 0 while
    // making no progress means something else entirely. The number settles it;
    // guessing from the outside does not.
    l->pin_rc = smpk_pin_self(0);
    if (l->pin_rc != 0)
        l->pin_failed = 1;
    l->cpu_seen = sched_getcpu();
    l->cpu_last = l->cpu_seen;
    l->started = 1;
    while (!*l->stop) {
        sched_yield();
        // Bumped AFTER the call returns, so a count that stops advancing means
        // the call did not come back -- not that the loop was merely slow.
        l->count++;
        // Cheap next to a syscall, and it says whether a thread that stopped
        // progressing was sitting on the CPU it was pinned to.
        if ((l->count & 0xfff) == 0)
            l->cpu_last = sched_getcpu();
    }
    return NULL;
}

static void *ys_notifier(void *arg) {
    struct ys_ctl *c = arg;
    if (smpk_pin_self(0) != 0)
        return NULL;
    // Short sleeps, on the same CPU as the yielders: each one ends in a timer
    // wake, which is a NOTIFY arriving at that CPU.
    //
    // Every counter here is shared with the other notifiers, so each is
    // incremented atomically. A plain `++` from several threads loses
    // increments, and this diagnostic decides whether a participant made
    // progress -- a lost increment is a wrong verdict, not a rounding error.
    int slot = __atomic_fetch_add(&c->notify_started, 1, __ATOMIC_RELAXED);
    if (slot >= 0 && slot < YS_MAX_NOTIFY)
        c->notify_cpu[slot] = sched_getcpu();
    struct timespec ts = {0, 200 * 1000}; // 200 us
    while (!c->stop) {
        nanosleep(&ts, NULL);
        __atomic_fetch_add(&c->notifies, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

// ---------------------------------------------------------------------------
// Disk  [kernel]
// ---------------------------------------------------------------------------

static double disk_seq_write(const char *path, size_t bytes, size_t chunk,
                             unsigned char *buf) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return NA;
    uint64_t t0 = now_ns();
    size_t done = 0;
    while (done < bytes) {
        size_t want = bytes - done < chunk ? bytes - done : chunk;
        ssize_t w = write(fd, buf, want);
        if (w <= 0) { close(fd); return NA; }
        done += (size_t)w;
    }
    fsync(fd);
    uint64_t t1 = now_ns();
    close(fd);
    return (double)bytes * 1e9 / (double)(t1 - t0);
}

static double disk_seq_read(const char *path, size_t chunk, unsigned char *buf) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return NA;
    uint64_t t0 = now_ns();
    uint64_t total = 0;
    for (;;) {
        ssize_t r = read(fd, buf, chunk);
        if (r < 0) { close(fd); return NA; }
        if (r == 0) break;
        total += (uint64_t)r;
    }
    uint64_t t1 = now_ns();
    close(fd);
    if (total == 0) return NA;
    return (double)total * 1e9 / (double)(t1 - t0);
}

static double disk_rand_read(const char *path, size_t bytes, uint64_t budget_ns,
                             double *avg_us) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return NA;
    const size_t blk = 4096;
    size_t nblk = bytes / blk;
    if (nblk == 0) { close(fd); return NA; }
    unsigned char b[4096];
    uint64_t r = 0x1234567890abcdefull;
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        for (int k = 0; k < 64; k++) {
            r = r * 6364136223846793005ull + 1442695040888963407ull;
            off_t off = (off_t)((r >> 12) % nblk) * (off_t)blk;
            if (pread(fd, b, blk, off) != (ssize_t)blk) { close(fd); return NA; }
            ops++;
        }
        elapsed = now_ns() - t0;
    }
    close(fd);
    double iops = (double)ops * 1e9 / (double)elapsed;
    if (avg_us) *avg_us = (double)elapsed / (double)ops / 1000.0;
    return iops;
}

static double disk_fsync_ms(const char *path, unsigned char *buf) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return NA;
    double best = 1e30;
    for (int i = 0; i < 8; i++) {
        if (write(fd, buf, 4096) != 4096) { close(fd); return NA; }
        uint64_t t0 = now_ns();
        fsync(fd);
        double ms = (double)(now_ns() - t0) / 1e6;
        if (ms < best) best = ms;
    }
    close(fd);
    return best;
}

// Metadata ops: create up to `max` small files in `dir` (time-bounded), then
// stat each, then unlink each. Reports the three rates via out params.
static void disk_metadata(const char *dir, uint64_t budget_ns, int max,
                          double *creates_s, double *stats_s, double *unlinks_s) {
    char path[512];
    *creates_s = *stats_s = *unlinks_s = NA;

    int made = 0;
    uint64_t t0 = now_ns();
    while (made < max && now_ns() - t0 < budget_ns) {
        snprintf(path, sizeof path, "%s/eb_%06d", dir, made);
        int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd < 0) break;
        if (write(fd, "x", 1) != 1) { close(fd); break; }
        close(fd);
        made++;
    }
    uint64_t t1 = now_ns();
    if (made > 0) *creates_s = (double)made * 1e9 / (double)(t1 - t0);

    t0 = now_ns();
    int ok = 0;
    for (int i = 0; i < made; i++) {
        snprintf(path, sizeof path, "%s/eb_%06d", dir, i);
        struct stat st;
        if (stat(path, &st) == 0) ok++;
    }
    t1 = now_ns();
    if (ok > 0) *stats_s = (double)ok * 1e9 / (double)(t1 - t0);

    t0 = now_ns();
    int rm = 0;
    for (int i = 0; i < made; i++) {
        snprintf(path, sizeof path, "%s/eb_%06d", dir, i);
        if (unlink(path) == 0) rm++;
    }
    t1 = now_ns();
    if (rm > 0) *unlinks_s = (double)rm * 1e9 / (double)(t1 - t0);
}

// ---------------------------------------------------------------------------
// Process creation  [kernel]
// ---------------------------------------------------------------------------

// fork + immediate child _exit, with `mib` MiB of pre-faulted private memory
// resident in the parent. Returns ns per fork, or NA if the region cannot be
// allocated.
//
// This is the measurement that tells copy-on-write `fork` from an eager one,
// and it is worth more than any single-size fork number. A COW kernel builds
// the child's address space by sharing frames and write-protecting them, so its
// cost barely moves with the resident set. A kernel that copies every resident
// frame at `fork` time is O(resident): the same shell, the same command, but a
// process holding 100 MiB pays a 100 MiB memcpy every time it forks.
//
// The `COW fault (after fork)` row above cannot reveal this on its own — with an
// eager `fork` the child's pages are already private, so its "COW faults" are
// plain stores and the row reports an implausibly *good* number.
static double proc_fork_resident_ns(size_t mib, uint64_t budget_ns) {
    size_t len = mib * 1024 * 1024;
    unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return NA;
    // Fault every page in so it is genuinely resident, and write a value so no
    // kernel can keep it shared with a global zero page.
    for (size_t i = 0; i < len; i += 4096)
        p[i] = (unsigned char)(i >> 12);
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pid_t c = fork();
        if (c == 0) _exit(0);
        if (c < 0) { munmap(p, len); return NA; }
        int st;
        waitpid(c, &st, 0);
        ops++;
        elapsed = now_ns() - t0;
    }
    munmap(p, len);
    return ops ? (double)elapsed / (double)ops : NA;
}

// fork with `extra` additional mappings present, holding the resident set fixed.
//
// The resident-size probe above answers "does fork copy the pages?". This one
// answers a question it cannot see at all: what does fork cost *per mapping*?
//
// The two are independent, and conflating them hid a real 3x regression. A
// copy-on-write fork stops paying per page but starts paying per mapping — it
// must write-protect each one and, if the kernel shoots down the other CPUs'
// TLBs once per mapping, that is an IPI round trip with an ack spin-wait each
// time. A process with a hundred small mappings then forks far more slowly than
// one with a single large one holding the same bytes, which no per-MiB number
// can express.
//
// Every mapping is one page and is touched once, so the resident set grows by
// `extra` pages -- negligible next to the 1 MiB baseline below, which is there
// precisely so the two runs differ in mapping count and in nothing else. They
// are also deliberately not adjacent: a kernel that merges neighbouring VMAs
// would otherwise collapse them into one and the probe would measure nothing.
// Create `extra` one-page mappings that no kernel can coalesce, and return how
// many were made (pointers in `*spots_out`, to be released with
// `scatter_free`). Reserving one run and punching every other page out of it
// guarantees the gaps without depending on where the kernel would otherwise
// place independent `mmap`s.
static int scatter_mappings(int extra, unsigned char ***spots_out) {
    *spots_out = NULL;
    if (extra <= 0)
        return 0;
    unsigned char **spots = calloc((size_t)extra, sizeof *spots);
    if (!spots)
        return 0;
    size_t run = (size_t)extra * 2 * 4096;
    unsigned char *arena = mmap(NULL, run, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (arena == MAP_FAILED) {
        free(spots);
        return 0;
    }
    int made = 0;
    for (int i = 0; i < extra; i++) {
        unsigned char *keep = arena + (size_t)i * 2 * 4096;
        munmap(keep + 4096, 4096);   // the gap that keeps `keep` separate
        keep[0] = (unsigned char)i;  // resident, so it is not merely reserved
        spots[made++] = keep;
    }
    *spots_out = spots;
    return made;
}

static void scatter_free(unsigned char **spots, int n) {
    for (int i = 0; i < n; i++)
        munmap(spots[i], 4096);
    free(spots);
}

static double proc_fork_mappings_ns(int extra, uint64_t budget_ns) {
    const size_t base_len = 1024 * 1024;
    unsigned char *base = mmap(NULL, base_len, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED)
        return NA;
    for (size_t i = 0; i < base_len; i += 4096)
        base[i] = (unsigned char)(i >> 12);

    unsigned char **spots = NULL;
    int made = scatter_mappings(extra, &spots);
    if (extra > 0 && made == 0) {
        munmap(base, base_len);
        return NA;
    }

    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pid_t c = fork();
        if (c == 0) _exit(0);
        if (c < 0) break;
        int st;
        waitpid(c, &st, 0);
        ops++;
        elapsed = now_ns() - t0;
    }

    scatter_free(spots, made);
    munmap(base, base_len);
    return ops ? (double)elapsed / (double)ops : NA;
}

// fork + immediate child _exit, parent waits. Returns ns per fork.
static double proc_fork_ns(uint64_t budget_ns) {
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pid_t p = fork();
        if (p == 0) _exit(0);
        if (p < 0) return NA;
        int st;
        waitpid(p, &st, 0);
        ops++;
        elapsed = now_ns() - t0;
    }
    return ops ? (double)elapsed / (double)ops : NA;
}

// fork + execve(self, "--noop") which exits at once, parent waits. Measures the
// full process-replacement cost (address-space teardown + ELF load + setup).
static double proc_fork_exec_ns(uint64_t budget_ns, const char *self) {
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    char *const argv[] = {(char *)self, (char *)"--noop", NULL};
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pid_t p = fork();
        if (p == 0) {
            execv(self, argv);
            _exit(127);
        }
        if (p < 0) return NA;
        int st;
        waitpid(p, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) return NA;
        ops++;
        elapsed = now_ns() - t0;
    }
    return ops ? (double)elapsed / (double)ops : NA;
}

// fork + exec a *shell* running a no-op. This is what a script, a Makefile or an
// interactive prompt pays per command: the shell binary is usually dynamically
// linked, so it exercises the loader, the dynamic linker and a real path
// lookup, none of which the static self-exec above touches.
static double proc_spawn_shell_ns(uint64_t budget_ns, const char *sh) {
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    char *const argv[] = {(char *)sh, (char *)"-c", (char *)":", NULL};
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        pid_t p = fork();
        if (p == 0) {
            execv(sh, argv);
            _exit(127);
        }
        if (p < 0) return NA;
        int st;
        waitpid(p, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) == 127) return NA;
        ops++;
        elapsed = now_ns() - t0;
    }
    return ops ? (double)elapsed / (double)ops : NA;
}

static const char *find_shell(void) {
    static const char *candidates[] = {"/bin/sh", "/bin/busybox", "/bin/dash",
                                       "/bin/bash", NULL};
    for (int i = 0; candidates[i]; i++) {
        struct stat st;
        if (stat(candidates[i], &st) == 0 && (st.st_mode & S_IXUSR))
            return candidates[i];
    }
    return NULL;
}

// ---------------------------------------------------------------------------
// GRAPHICS / DRM-KMS  [kernel]
// ---------------------------------------------------------------------------
//
// Everything a compositor asks the kernel for, on the raw DRM path: no libdrm,
// no Mesa, no Wayland. The ioctl numbers and structures are declared here so
// the SAME binary runs on Eclipse and on any Linux without installing a thing —
// which is the whole point, because a benchmark that needs packages one side
// lacks measures the packaging, not the kernel.
//
// Why these numbers and not a frame rate: a GPU benchmark answers "how fast is
// this GPU", and under QEMU both sides drive the *same* emulated device, so
// that question has no kernel content at all. What differs between two kernels
// on one virtual GPU is the cost of the calls a compositor makes every frame —
// buffer allocation, framebuffer bookkeeping, the mapping's cache policy, the
// flip submission path and how faithfully flips are paced to the refresh. Those
// are what this section measures.
//
// The modifying probes (flip, cursor, atomic) run only if this process can
// become DRM master. On a machine with a live compositor SET_MASTER fails and
// they are reported n/a rather than fighting the desktop for the display.

#define DRM_IOCTL_BASE 'd'
#define BDRM_IO(nr) _IO(DRM_IOCTL_BASE, nr)
#define BDRM_IOW(nr, type) _IOW(DRM_IOCTL_BASE, nr, type)
#define BDRM_IOWR(nr, type) _IOWR(DRM_IOCTL_BASE, nr, type)

struct b_drm_get_cap { uint64_t capability, value; };
struct b_drm_set_client_cap { uint64_t capability, value; };

struct b_drm_mode_card_res {
    uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
    uint32_t min_width, max_width, min_height, max_height;
};

struct b_drm_mode_modeinfo {
    uint32_t clock;
    uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
    uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
    uint32_t vrefresh, flags, type;
    char name[32];
};

struct b_drm_mode_get_connector {
    uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
    uint32_t count_modes, count_props, count_encoders;
    uint32_t encoder_id, connector_id, connector_type, connector_type_id;
    uint32_t connection, mm_width, mm_height, subpixel;
    uint32_t pad;
};

struct b_drm_mode_crtc {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct b_drm_mode_modeinfo mode;
};

struct b_drm_mode_create_dumb {
    uint32_t height, width, bpp, flags, handle, pitch;
    uint64_t size;
};
struct b_drm_mode_map_dumb { uint32_t handle, pad; uint64_t offset; };
struct b_drm_mode_destroy_dumb { uint32_t handle; };

struct b_drm_mode_fb_cmd2 {
    uint32_t fb_id, width, height, pixel_format, flags;
    uint32_t handles[4], pitches[4], offsets[4];
    uint64_t modifier[4];
};

struct b_drm_mode_crtc_page_flip {
    uint32_t crtc_id, fb_id, flags, reserved;
    uint64_t user_data;
};

struct b_drm_mode_cursor {
    uint32_t flags, crtc_id;
    int32_t x, y;
    uint32_t width, height, handle;
};

struct b_drm_event { uint32_t type, length; };

union b_drm_wait_vblank {
    struct { uint32_t type, sequence; unsigned long signal; } request;
    struct { uint32_t type, sequence; long tval_sec, tval_usec; } reply;
};

#define B_DRM_IOCTL_GET_CAP           BDRM_IOWR(0x0c, struct b_drm_get_cap)
#define B_DRM_IOCTL_SET_CLIENT_CAP    BDRM_IOW(0x0d, struct b_drm_set_client_cap)
#define B_DRM_IOCTL_SET_MASTER        BDRM_IO(0x1e)
#define B_DRM_IOCTL_DROP_MASTER       BDRM_IO(0x1f)
#define B_DRM_IOCTL_WAIT_VBLANK       BDRM_IOWR(0x3a, union b_drm_wait_vblank)
#define B_DRM_IOCTL_MODE_GETRESOURCES BDRM_IOWR(0xa0, struct b_drm_mode_card_res)
#define B_DRM_IOCTL_MODE_GETCRTC      BDRM_IOWR(0xa1, struct b_drm_mode_crtc)
#define B_DRM_IOCTL_MODE_CURSOR       BDRM_IOWR(0xa3, struct b_drm_mode_cursor)
#define B_DRM_IOCTL_MODE_GETCONNECTOR BDRM_IOWR(0xa7, struct b_drm_mode_get_connector)
#define B_DRM_IOCTL_MODE_RMFB         BDRM_IOWR(0xaf, unsigned int)
#define B_DRM_IOCTL_MODE_PAGE_FLIP    BDRM_IOWR(0xb0, struct b_drm_mode_crtc_page_flip)
#define B_DRM_IOCTL_MODE_CREATE_DUMB  BDRM_IOWR(0xb2, struct b_drm_mode_create_dumb)
#define B_DRM_IOCTL_MODE_MAP_DUMB     BDRM_IOWR(0xb3, struct b_drm_mode_map_dumb)
#define B_DRM_IOCTL_MODE_DESTROY_DUMB BDRM_IOWR(0xb4, struct b_drm_mode_destroy_dumb)
#define B_DRM_IOCTL_MODE_ADDFB2       BDRM_IOWR(0xb8, struct b_drm_mode_fb_cmd2)

#define B_DRM_CAP_DUMB_BUFFER 0x1
#define B_DRM_MODE_PAGE_FLIP_EVENT 0x01
#define B_DRM_MODE_CURSOR_MOVE 0x02
#define B_DRM_VBLANK_RELATIVE 0x1
#define B_FMT_XRGB8888 0x34325258 /* fourcc 'XR24' */

// State shared by the probes (they must be no-argument functions for
// timed_ns_per_op, same shape as the syscall probes above).
static int g_drm = -1;
static const char *g_drm_path = "/dev/dri/card0";
static int g_drm_master = 0;
static uint32_t g_crtc_id, g_conn_id;
static uint32_t g_gw = 640, g_gh = 480; // measured surface size
static uint32_t g_dumb_handle, g_dumb_pitch;
static uint64_t g_dumb_size;
static uint32_t g_fb_a, g_fb_b;
static uint32_t g_dumb_b_handle;
static unsigned char *g_fb_map;
static uint32_t g_cursor_handle;
static int g_flip_parity;
// The framebuffer the CRTC was scanning out before we touched it. The flip
// probe puts OUR buffer on the display; without putting the original back the
// benchmark would leave the console showing a scratch buffer.
static uint32_t g_orig_fb;

static int drm_call(unsigned long req, void *arg) {
    int r;
    do { r = ioctl(g_drm, req, arg); } while (r < 0 && errno == EINTR);
    return r;
}

// ---- read-only probes: available even when another process is master ----

static int gfx_getcap(void) {
    struct b_drm_get_cap c;
    memset(&c, 0, sizeof c);
    c.capability = B_DRM_CAP_DUMB_BUFFER;
    return drm_call(B_DRM_IOCTL_GET_CAP, &c) < 0 ? -1 : 0;
}

static int gfx_getres(void) {
    struct b_drm_mode_card_res r;
    memset(&r, 0, sizeof r);
    // Count-only form: exactly what a compositor issues first, and what it
    // re-issues on every hotplug.
    return drm_call(B_DRM_IOCTL_MODE_GETRESOURCES, &r) < 0 ? -1 : 0;
}

static int gfx_getconnector(void) {
    struct b_drm_mode_get_connector c;
    memset(&c, 0, sizeof c);
    c.connector_id = g_conn_id;
    return drm_call(B_DRM_IOCTL_MODE_GETCONNECTOR, &c) < 0 ? -1 : 0;
}

static int gfx_getcrtc(void) {
    struct b_drm_mode_crtc c;
    memset(&c, 0, sizeof c);
    c.crtc_id = g_crtc_id;
    return drm_call(B_DRM_IOCTL_MODE_GETCRTC, &c) < 0 ? -1 : 0;
}

// ---- buffer lifecycle ----

static int gfx_dumb_cycle(void) {
    struct b_drm_mode_create_dumb c;
    memset(&c, 0, sizeof c);
    c.width = g_gw; c.height = g_gh; c.bpp = 32;
    if (drm_call(B_DRM_IOCTL_MODE_CREATE_DUMB, &c) < 0)
        return -1;
    struct b_drm_mode_destroy_dumb d;
    memset(&d, 0, sizeof d);
    d.handle = c.handle;
    return drm_call(B_DRM_IOCTL_MODE_DESTROY_DUMB, &d) < 0 ? -1 : 0;
}

static int gfx_fb_cycle(void) {
    struct b_drm_mode_fb_cmd2 fb;
    memset(&fb, 0, sizeof fb);
    fb.width = g_gw; fb.height = g_gh;
    fb.pixel_format = B_FMT_XRGB8888;
    fb.handles[0] = g_dumb_handle;
    fb.pitches[0] = g_dumb_pitch;
    if (drm_call(B_DRM_IOCTL_MODE_ADDFB2, &fb) < 0)
        return -1;
    unsigned int id = fb.fb_id;
    return drm_call(B_DRM_IOCTL_MODE_RMFB, &id) < 0 ? -1 : 0;
}

static int gfx_map_cycle(void) {
    struct b_drm_mode_map_dumb m;
    memset(&m, 0, sizeof m);
    m.handle = g_dumb_handle;
    if (drm_call(B_DRM_IOCTL_MODE_MAP_DUMB, &m) < 0)
        return -1;
    void *p = mmap(NULL, (size_t)g_dumb_size, PROT_READ | PROT_WRITE,
                   MAP_SHARED, g_drm, (off_t)m.offset);
    if (p == MAP_FAILED)
        return -1;
    munmap(p, (size_t)g_dumb_size);
    return 0;
}

// ---- present path (needs DRM master) ----

// One flip, then wait for its completion event. A flip without the wait would
// measure only how fast the ioctl returns, which is not what a frame costs: the
// event is the frame actually being on screen, and the wait is where a
// compositor spends its idle time.
static int gfx_pageflip(void) {
    struct b_drm_mode_crtc_page_flip f;
    memset(&f, 0, sizeof f);
    f.crtc_id = g_crtc_id;
    f.fb_id = (g_flip_parity ^= 1) ? g_fb_b : g_fb_a;
    f.flags = B_DRM_MODE_PAGE_FLIP_EVENT;
    f.user_data = 0x600df00d;
    if (drm_call(B_DRM_IOCTL_MODE_PAGE_FLIP, &f) < 0)
        return -1;
    // Drain exactly one completion. The event carries a header plus a payload;
    // a 96-byte read covers both the vblank and the flip-complete forms.
    unsigned char ev[96];
    ssize_t n = read(g_drm, ev, sizeof ev);
    return n > 0 ? 0 : -1;
}

static int gfx_cursor_move(void) {
    struct b_drm_mode_cursor c;
    memset(&c, 0, sizeof c);
    c.flags = B_DRM_MODE_CURSOR_MOVE;
    c.crtc_id = g_crtc_id;
    // Walk the pointer so a driver that early-outs on "same position" is not
    // measured doing nothing.
    static int32_t x;
    x = (x + 7) % 256;
    c.x = x; c.y = x;
    return drm_call(B_DRM_IOCTL_MODE_CURSOR, &c) < 0 ? -1 : 0;
}

// ---- discovery ----

// Find a connected connector with a mode, and the CRTC currently driving it.
// Returns 0 on success. Nothing here modifies state.
static int gfx_discover(void) {
    struct b_drm_mode_card_res res;
    memset(&res, 0, sizeof res);
    if (drm_call(B_DRM_IOCTL_MODE_GETRESOURCES, &res) < 0)
        return -1;
    if (!res.count_crtcs || !res.count_connectors)
        return -1;

    uint32_t crtcs[16], conns[16];
    uint32_t nc = res.count_crtcs > 16 ? 16 : res.count_crtcs;
    uint32_t nn = res.count_connectors > 16 ? 16 : res.count_connectors;
    memset(&res, 0, sizeof res);
    res.crtc_id_ptr = (uint64_t)(uintptr_t)crtcs;
    res.connector_id_ptr = (uint64_t)(uintptr_t)conns;
    res.count_crtcs = nc;
    res.count_connectors = nn;
    if (drm_call(B_DRM_IOCTL_MODE_GETRESOURCES, &res) < 0)
        return -1;
    if (!res.count_crtcs || !res.count_connectors)
        return -1;
    if (res.count_crtcs < nc) nc = res.count_crtcs;
    if (res.count_connectors < nn) nn = res.count_connectors;

    // Prefer a connector that is connected AND has modes; fall back to the
    // first one so a headless-but-present pipeline still reports ioctl costs.
    for (uint32_t i = 0; i < nn; i++) {
        struct b_drm_mode_get_connector gc;
        struct b_drm_mode_modeinfo modes[32];
        memset(&gc, 0, sizeof gc);
        gc.connector_id = conns[i];
        if (drm_call(B_DRM_IOCTL_MODE_GETCONNECTOR, &gc) < 0)
            continue;
        uint32_t want_modes = gc.count_modes > 32 ? 32 : gc.count_modes;
        memset(&gc, 0, sizeof gc);
        gc.connector_id = conns[i];
        gc.count_modes = want_modes;
        gc.modes_ptr = (uint64_t)(uintptr_t)modes;
        if (drm_call(B_DRM_IOCTL_MODE_GETCONNECTOR, &gc) < 0)
            continue;
        if (!g_conn_id)
            g_conn_id = conns[i];
        if (gc.connection == 1 && gc.count_modes) {
            g_conn_id = conns[i];
            uint32_t m = gc.count_modes > want_modes ? want_modes : gc.count_modes;
            if (m) {
                g_gw = modes[0].hdisplay;
                g_gh = modes[0].vdisplay;
            }
            break;
        }
    }

    // The CRTC that already has a mode programmed: flipping onto a live CRTC is
    // what a compositor does. Setting one up ourselves would be a modeset, which
    // is disruptive and is deliberately not part of this benchmark.
    for (uint32_t i = 0; i < nc; i++) {
        struct b_drm_mode_crtc gc;
        memset(&gc, 0, sizeof gc);
        gc.crtc_id = crtcs[i];
        if (drm_call(B_DRM_IOCTL_MODE_GETCRTC, &gc) < 0)
            continue;
        if (!g_crtc_id)
            g_crtc_id = crtcs[i];
        if (gc.mode_valid) {
            g_crtc_id = crtcs[i];
            g_orig_fb = gc.fb_id;
            if (gc.mode.hdisplay && gc.mode.vdisplay) {
                g_gw = gc.mode.hdisplay;
                g_gh = gc.mode.vdisplay;
            }
            return 0;
        }
    }
    return g_crtc_id ? 0 : -1;
}

// Allocate the working buffers: one mapped dumb buffer plus two framebuffers to
// flip between. Returns 0 on success.
static int gfx_alloc(void) {
    struct b_drm_mode_create_dumb c;
    memset(&c, 0, sizeof c);
    c.width = g_gw; c.height = g_gh; c.bpp = 32;
    if (drm_call(B_DRM_IOCTL_MODE_CREATE_DUMB, &c) < 0)
        return -1;
    g_dumb_handle = c.handle;
    g_dumb_pitch = c.pitch;
    g_dumb_size = c.size;

    struct b_drm_mode_map_dumb m;
    memset(&m, 0, sizeof m);
    m.handle = g_dumb_handle;
    if (drm_call(B_DRM_IOCTL_MODE_MAP_DUMB, &m) == 0) {
        void *p = mmap(NULL, (size_t)g_dumb_size, PROT_READ | PROT_WRITE,
                       MAP_SHARED, g_drm, (off_t)m.offset);
        if (p != MAP_FAILED)
            g_fb_map = p;
    }

    struct b_drm_mode_fb_cmd2 fb;
    memset(&fb, 0, sizeof fb);
    fb.width = g_gw; fb.height = g_gh;
    fb.pixel_format = B_FMT_XRGB8888;
    fb.handles[0] = g_dumb_handle;
    fb.pitches[0] = g_dumb_pitch;
    if (drm_call(B_DRM_IOCTL_MODE_ADDFB2, &fb) == 0)
        g_fb_a = fb.fb_id;

    // A second buffer so the flip probe alternates, as a double-buffered
    // compositor does. Flipping the same fb id repeatedly is a path some
    // drivers short-circuit.
    memset(&c, 0, sizeof c);
    c.width = g_gw; c.height = g_gh; c.bpp = 32;
    if (drm_call(B_DRM_IOCTL_MODE_CREATE_DUMB, &c) == 0) {
        g_dumb_b_handle = c.handle;
        memset(&fb, 0, sizeof fb);
        fb.width = g_gw; fb.height = g_gh;
        fb.pixel_format = B_FMT_XRGB8888;
        fb.handles[0] = c.handle;
        fb.pitches[0] = c.pitch;
        if (drm_call(B_DRM_IOCTL_MODE_ADDFB2, &fb) == 0)
            g_fb_b = fb.fb_id;
    }
    if (!g_fb_b)
        g_fb_b = g_fb_a;
    // Black, not whatever was in the pages: if anything below fails and leaves
    // one of these on screen, a black display is a far better outcome than a
    // window into freed memory.
    if (g_fb_map)
        memset(g_fb_map, 0, (size_t)g_dumb_size);
    return g_fb_a ? 0 : -1;
}

static void gfx_free(void) {
    if (g_fb_map) { munmap(g_fb_map, (size_t)g_dumb_size); g_fb_map = NULL; }
    unsigned int id;
    if (g_fb_b && g_fb_b != g_fb_a) { id = g_fb_b; drm_call(B_DRM_IOCTL_MODE_RMFB, &id); }
    if (g_fb_a) { id = g_fb_a; drm_call(B_DRM_IOCTL_MODE_RMFB, &id); }
    struct b_drm_mode_destroy_dumb d;
    if (g_dumb_b_handle) { memset(&d, 0, sizeof d); d.handle = g_dumb_b_handle;
                           drm_call(B_DRM_IOCTL_MODE_DESTROY_DUMB, &d); }
    if (g_cursor_handle) { memset(&d, 0, sizeof d); d.handle = g_cursor_handle;
                           drm_call(B_DRM_IOCTL_MODE_DESTROY_DUMB, &d); }
    if (g_dumb_handle) { memset(&d, 0, sizeof d); d.handle = g_dumb_handle;
                         drm_call(B_DRM_IOCTL_MODE_DESTROY_DUMB, &d); }
    g_fb_a = g_fb_b = g_dumb_handle = g_dumb_b_handle = g_cursor_handle = 0;
}

// Sequential stores into the mapped framebuffer, i.e. every pixel a software
// compositor (pixman, Cairo, a Wayland shm client) ever draws.
//
// This is tagged [kernel] even though the loop is a plain store loop, because
// the number is decided by the *cache policy the kernel chose for the mapping*.
// Write-combining runs at DRAM speed; uncached is 10-50x slower and turns a
// full-screen repaint from a millisecond into tens of them. Nothing else in
// this suite can catch that, and it is invisible in a memcpy benchmark over
// ordinary anonymous memory.
static double gfx_fb_write_mibs(uint64_t budget_ns) {
    if (!g_fb_map || !g_dumb_size)
        return NA;
    volatile uint32_t *p = (volatile uint32_t *)g_fb_map;
    size_t words = (size_t)(g_dumb_size / 4);
    uint64_t t0 = now_ns(), bytes = 0;
    uint32_t v = 0x00204060;
    do {
        for (size_t i = 0; i < words; i++)
            p[i] = v;
        bytes += (uint64_t)words * 4;
        v += 0x00010101;
    } while (now_ns() - t0 < budget_ns);
    double el = (double)(now_ns() - t0) / 1e9;
    g_sink += v;
    return el > 0 ? (double)bytes / (1024.0 * 1024.0) / el : NA;
}

// The same store loop over ordinary anonymous memory. It is the denominator of
// the `fb write / memcpy` ratio: identical code, identical CPU, the only
// difference being which mapping it writes into -- so the ratio isolates the
// cache policy the kernel gave the framebuffer and nothing else.
static double ref_store_mibs(uint64_t budget_ns, size_t bytes) {
    if (bytes < (1u << 16))
        bytes = 1u << 16;
    unsigned char *buf = malloc(bytes);
    if (!buf)
        return NA;
    memset(buf, 0, bytes); // pre-fault: this is not the page-fault benchmark
    volatile uint32_t *p = (volatile uint32_t *)buf;
    size_t words = bytes / 4;
    uint64_t t0 = now_ns(), total = 0;
    uint32_t v = 0x00204060;
    do {
        for (size_t i = 0; i < words; i++)
            p[i] = v;
        total += (uint64_t)words * 4;
        v += 0x00010101;
    } while (now_ns() - t0 < budget_ns);
    double el = (double)(now_ns() - t0) / 1e9;
    g_sink += v;
    free(buf);
    return el > 0 ? (double)total / (1024.0 * 1024.0) / el : NA;
}

// The refresh clock as the kernel actually delivers it. `mean` is the period;
// `jitter` is the spread. A compositor paces every frame off this, so a period
// that is right on average but arrives in bursts still stutters.
static int gfx_vblank_stats(int n, double *mean_ms, double *jitter_ms,
                            double *worst_ms) {
    if (n < 4)
        n = 4;
    double *iv = calloc((size_t)n, sizeof *iv);
    if (!iv)
        return -1;
    union b_drm_wait_vblank w;
    memset(&w, 0, sizeof w);
    w.request.type = B_DRM_VBLANK_RELATIVE;
    w.request.sequence = 1;
    if (drm_call(B_DRM_IOCTL_WAIT_VBLANK, &w) < 0) { free(iv); return -1; }
    uint64_t prev = now_ns();
    int got = 0;
    for (int i = 0; i < n; i++) {
        memset(&w, 0, sizeof w);
        w.request.type = B_DRM_VBLANK_RELATIVE;
        w.request.sequence = 1;
        if (drm_call(B_DRM_IOCTL_WAIT_VBLANK, &w) < 0)
            break;
        uint64_t t = now_ns();
        iv[got++] = (double)(t - prev) / 1e6;
        prev = t;
    }
    if (got < 2) { free(iv); return -1; }
    double sum = 0, hi = 0;
    for (int i = 0; i < got; i++) { sum += iv[i]; if (iv[i] > hi) hi = iv[i]; }
    double m = sum / got, var = 0;
    for (int i = 0; i < got; i++) var += (iv[i] - m) * (iv[i] - m);
    // Integer-free sqrt by Newton iteration: this binary links no libm.
    double vv = var / got, s = vv > 0 ? vv : 0, r = s > 1 ? s : 1;
    for (int k = 0; k < 40; k++) r = 0.5 * (r + s / r);
    *mean_ms = m;
    *jitter_ms = s > 0 ? r : 0;
    *worst_ms = hi;
    free(iv);
    return 0;
}

// ---------------------------------------------------------------------------
// Per-process syscall accounting  [kernel]
// ---------------------------------------------------------------------------
//
// `/proc/<pid>/perf` is Eclipse's per-process syscall table: one row per
// syscall the process has issued, with the call count and the time the kernel
// spent inside it. For the sections below that is a better pairing than the
// system-wide counters the `psched` section uses, for two reasons:
//
//   * it is OUR process, so an idle shell or a daemon waking up cannot move
//     the numbers. The system-wide table cannot promise that.
//   * it splits a measured round trip into "how many syscalls did libc really
//     issue" and "how much of my wall clock was inside the kernel". Those are
//     different bugs: the first is a wrapper doing more work than the row
//     claims to measure (a retry loop, a short read), the second is the kernel
//     path itself being slow. A single ns/op figure cannot tell them apart,
//     and this file's own history says that is exactly where a benchmark
//     starts lying.
//
// Linux has no such file, so every derived row reads `n/a` there — the same
// convention the kernel-counter rows already follow.

#define PSTAT_BUF 65536
static char g_pstat_a[PSTAT_BUF], g_pstat_b[PSTAT_BUF];
static int g_pstat_ok = -1;

static int pstat_snapshot(char *buf, size_t n) {
    int fd = open("/proc/self/perf", O_RDONLY);
    if (fd < 0) {
        buf[0] = 0;
        return 0;
    }
    size_t got = 0;
    for (;;) {
        ssize_t r = read(fd, buf + got, n - 1 - got);
        if (r <= 0)
            break;
        got += (size_t)r;
        if (got >= n - 1)
            break;
    }
    close(fd);
    buf[got] = 0;
    return got > 0;
}

// The `idx`-th number on the row for syscall `name` (0 = calls, 1 = total ms,
// 2 = mean us), or NA when the process has never issued it.
//
// The name must be followed by a separator. Without that check `dup` would
// match the `dup3` row and report a number for a syscall that was never
// called — the whole class of bug the label-matched kernel-counter reader
// above was written to avoid.
static double pstat_field(const char *buf, const char *name, int idx) {
    size_t nlen = strlen(name);
    const char *p = buf;
    while (*p) {
        const char *bol = p;
        while (*p == ' ' || *p == '\t')
            p++;
        if (strncmp(p, name, nlen) == 0 &&
            (p[nlen] == ' ' || p[nlen] == '\t')) {
            const char *q = p + nlen;
            int seen = 0;
            while (*q && *q != '\n') {
                if (*q >= '0' && *q <= '9') {
                    double v = strtod(q, (char **)&q);
                    if (seen == idx)
                        return v;
                    seen++;
                    continue;
                }
                q++;
            }
            return NA;
        }
        p = bol;
        while (*p && *p != '\n')
            p++;
        if (*p == '\n')
            p++;
    }
    return NA;
}

// What one snapshot costs in the syscalls it reports on.
//
// Reading the table is itself `openat` + `read` + `close`, and those land in
// the very rows a `read` or `open` probe wants to read back — so a delta taken
// around 24 opens would charge two of them to the measurement and report 1.08
// opens per open. Measured once, from two back-to-back snapshots with no work
// between them, and subtracted. Measured rather than assumed: it depends on
// how many `read` calls it takes to reach EOF, which depends on how long the
// table has grown.
static double g_pstat_ovh_open, g_pstat_ovh_read, g_pstat_ovh_close;

static void pstat_calibrate(void) {
    g_pstat_ovh_open = g_pstat_ovh_read = g_pstat_ovh_close = 0;
    if (!pstat_snapshot(g_pstat_a, sizeof g_pstat_a))
        return;
    if (!pstat_snapshot(g_pstat_b, sizeof g_pstat_b))
        return;
    double o = pstat_field(g_pstat_b, "openat", 0) -
               pstat_field(g_pstat_a, "openat", 0);
    double r = pstat_field(g_pstat_b, "read", 0) -
               pstat_field(g_pstat_a, "read", 0);
    double c = pstat_field(g_pstat_b, "close", 0) -
               pstat_field(g_pstat_a, "close", 0);
    if (o > 0) g_pstat_ovh_open = o;
    if (r > 0) g_pstat_ovh_read = r;
    if (c > 0) g_pstat_ovh_close = c;
}

static double pstat_overhead(const char *name) {
    if (strcmp(name, "openat") == 0) return g_pstat_ovh_open;
    if (strcmp(name, "read") == 0) return g_pstat_ovh_read;
    if (strcmp(name, "close") == 0) return g_pstat_ovh_close;
    return 0;
}

// Calls of `name` between the two snapshots, with the reader's own calls taken
// back out. Never returns a negative count: a measurement smaller than the
// calibration is reported as zero, not as a negative rate.
static double pstat_calls(const char *name) {
    double a = pstat_field(g_pstat_a, name, 0);
    double b = pstat_field(g_pstat_b, name, 0);
    // A syscall issued for the first time *inside* the window has no row in
    // the first snapshot; that is a zero, not a missing measurement.
    if (a < 0 && b >= 0) a = 0;
    if (a < 0 || b < 0 || b < a)
        return NA;
    double d = b - a - pstat_overhead(name);
    return d > 0 ? d : 0;
}

// Nanoseconds the kernel spent inside `name` between the snapshots. The file
// reports milliseconds; the overhead correction is deliberately NOT applied
// here (the reader's own time is a few microseconds against measurements of
// hundreds of milliseconds, and subtracting an unmeasured constant from a
// time is how a benchmark invents precision it does not have).
static double pstat_kernel_ns(const char *name) {
    double a = pstat_field(g_pstat_a, name, 1);
    double b = pstat_field(g_pstat_b, name, 1);
    if (a < 0 && b >= 0) a = 0;
    if (a < 0 || b < 0 || b < a)
        return NA;
    return (b - a) * 1e6;
}

// Pair the row just printed with the kernel's own account of the syscall that
// was supposed to dominate it: how many of them userspace really issued per
// operation, and what share of the measured time was spent inside them.
//
// A calls/op that is not the expected integer means the row is not measuring
// what its label says. A low in-kernel share with a high ns/op means the cost
// is in entry/exit or in being rescheduled, not in the subsystem — and that
// distinction is the whole reason this pairing exists.
static void pstat_pair(const char *name, const char *label, double ops,
                       double measured_ns) {
    if (g_pstat_ok <= 0)
        return;
    double calls = pstat_calls(name);
    double kns = pstat_kernel_ns(name);
    char lbl[64];
    snprintf(lbl, sizeof lbl, "  %s calls/op", label);
    row("[kernel]", lbl, (ops > 0 && calls >= 0) ? calls / ops : NA, "x", "");
    if (measured_ns > 0 && kns >= 0 && ops > 0) {
        snprintf(lbl, sizeof lbl, "  %s in-kernel", label);
        row("[kernel]", lbl, kns / ops / measured_ns * 100.0, "%",
            "rest is entry/exit + resched");
    }
}

// ---------------------------------------------------------------------------
// Sockets / IPC  [kernel]
// ---------------------------------------------------------------------------
//
// The SCHEDULER section has one socket row: a round trip over an AF_UNIX
// socketpair. That measures the wake-up, not the socket — a socketpair is the
// shortest possible path through the stack, with no address, no listener, no
// protocol. Everything a real program uses on top of that is unmeasured: the
// loopback TCP path, a bound UNIX socket reached by name, datagrams, the
// accept path a server pays per connection, `sendmsg` with control data (the
// fd passing a browser's zygote lives on), and the readiness syscalls every
// event loop sits in.
//
// Each row is paired with the kernel's own account of the syscall that should
// dominate it, so a slow row says WHICH mechanism was slow.

// A peer thread that echoes single bytes back, used by every round-trip row.
// Stopped by closing the measuring end: the echo loop then sees EOF (stream)
// or an error (datagram) and returns, which is how the pipe rows already do
// it. `done` lets the probe use `join_or_abandon` instead of an unbounded
// join, because a lost wake-up on the peer's side must cost this row its
// number and nothing else.
struct echo_ctl {
    int fd;
    volatile int done;
    int refs;
};

static void *sock_echo_thread(void *arg) {
    struct echo_ctl *c = arg;
    char b;
    for (;;) {
        ssize_t r = recv(c->fd, &b, 1, 0);
        if (r != 1)
            break;
        if (send(c->fd, &b, 1, 0) != 1)
            break;
    }
    __atomic_store_n(&c->done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

// Drain everything the peer sends and count it, for the bandwidth rows.
struct drain_ctl {
    int fd;
    size_t buflen;
    volatile int done;
    int refs;
};

static void *sock_drain_thread(void *arg) {
    struct drain_ctl *c = arg;
    unsigned char *buf = malloc(c->buflen);
    if (buf) {
        for (;;) {
            ssize_t r = recv(c->fd, buf, c->buflen, 0);
            if (r <= 0)
                break;
        }
        free(buf);
    }
    __atomic_store_n(&c->done, 1, __ATOMIC_RELEASE);
    CTL_DROP(c);
    return NULL;
}

// Bound the measured side's receive so a lost wake-up cannot park the suite
// on one row. Best-effort: a kernel that does not implement SO_RCVTIMEO for
// this socket family simply leaves the row exposed to the blocking recv, so
// the budget-bounded loops are still the outer guard.
static void sock_set_timeout(int fd, int secs) {
    struct timeval tv = {secs, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
}

// One byte each way over an already-connected socket, mirroring
// `pingpong_drive` so the figures are directly comparable with the pipe rows.
static double sock_rt_drive(int fd, uint64_t budget_ns) {
    char c = 'x';
    // One untimed exchange, so the peer is parked in recv before the clock
    // starts and the first iteration does not measure thread startup.
    if (send(fd, &c, 1, 0) != 1 || recv(fd, &c, 1, 0) != 1)
        return NA;
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    int batch = 1;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        for (int k = 0; k < batch; k++) {
            if (send(fd, &c, 1, 0) != 1)
                return NA;
            if (recv(fd, &c, 1, 0) != 1)
                return NA;
            ops++;
        }
        elapsed = now_ns() - t0;
        if (batch < 32 && ops > 0 && elapsed / ops < 100000)
            batch = 32;
    }
    g_last_ops = ops;
    return ops ? (double)elapsed / (double)ops : NA;
}

// Connect a stream socket to a listener without needing a second thread.
//
// A blocking `connect` on a loopback listener is only safe if the stack
// completes the handshake without an `accept` — true of Linux, not something
// to assume of a stack that may hand the connection straight to the acceptor.
// Connecting non-blocking and then polling for writability works on either,
// and turns "the handshake never completed" into a bounded `n/a` rather than
// a hang. The poll on the LISTENER before accepting does the same for the
// other half.
static int net_stream_connect(int ls, int domain, const struct sockaddr *sa,
                             socklen_t salen, int *cfd, int *sfd) {
    int c = socket(domain, SOCK_STREAM, 0);
    if (c < 0)
        return -1;
    int fl = fcntl(c, F_GETFL, 0);
    int nonblock = (fl >= 0 && fcntl(c, F_SETFL, fl | O_NONBLOCK) == 0);
    int rc = connect(c, sa, salen);
    if (rc != 0 && !(nonblock && (errno == EINPROGRESS || errno == EAGAIN))) {
        close(c);
        return -1;
    }
    struct pollfd lp = {ls, POLLIN, 0};
    if (poll(&lp, 1, 5000) != 1) {
        close(c);
        return -1;
    }
    int s = accept(ls, NULL, NULL);
    if (s < 0) {
        close(c);
        return -1;
    }
    if (rc != 0) {
        struct pollfd cp = {c, POLLOUT, 0};
        int err = 0;
        socklen_t elen = sizeof err;
        if (poll(&cp, 1, 5000) != 1 ||
            getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &elen) != 0 || err != 0) {
            close(c);
            close(s);
            return -1;
        }
    }
    if (nonblock && fl >= 0)
        fcntl(c, F_SETFL, fl);
    *cfd = c;
    *sfd = s;
    return 0;
}

// A connected loopback TCP pair, on a kernel-chosen port so two runs (or a
// run against a machine that already has a server up) cannot collide.
static int net_tcp_pair(int *cfd, int *sfd) {
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls < 0)
        return -1;
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    sa.sin_port = 0;
    socklen_t salen = sizeof sa;
    if (bind(ls, (struct sockaddr *)&sa, salen) != 0 || listen(ls, 8) != 0 ||
        getsockname(ls, (struct sockaddr *)&sa, &salen) != 0) {
        close(ls);
        return -1;
    }
    int rc = net_stream_connect(ls, AF_INET, (struct sockaddr *)&sa,
                                sizeof sa, cfd, sfd);
    close(ls);
    return rc;
}

// Where the AF_UNIX rows put their sockets. Under the benchmark directory, not
// /tmp: the caller already chose a filesystem it wants measured, and a hidden
// hop onto a tmpfs is exactly the mistake the DIR argument warns about.
static char g_net_dir[512];

static int net_unix_addr(struct sockaddr_un *sa, const char *tag) {
    memset(sa, 0, sizeof *sa);
    sa->sun_family = AF_UNIX;
    int n = snprintf(sa->sun_path, sizeof sa->sun_path, "%s/bench-%s.sock",
                     g_net_dir, tag);
    if (n < 0 || (size_t)n >= sizeof sa->sun_path)
        return -1;
    unlink(sa->sun_path);
    return 0;
}

static int net_unix_pair(int *cfd, int *sfd, char *path, size_t pathn) {
    struct sockaddr_un sa;
    if (net_unix_addr(&sa, "stream") != 0)
        return -1;
    int ls = socket(AF_UNIX, SOCK_STREAM, 0);
    if (ls < 0)
        return -1;
    if (bind(ls, (struct sockaddr *)&sa, sizeof sa) != 0 ||
        listen(ls, 8) != 0) {
        close(ls);
        return -1;
    }
    int rc = net_stream_connect(ls, AF_UNIX, (struct sockaddr *)&sa,
                                sizeof sa, cfd, sfd);
    close(ls);
    snprintf(path, pathn, "%s", sa.sun_path);
    if (rc != 0)
        unlink(sa.sun_path);
    return rc;
}

// Two datagram sockets connected to each other, so the round-trip driver can
// use plain send/recv on both ends and the row stays comparable with the
// stream ones.
static int net_dgram_pair(int domain, int *a, int *b, char *pa, char *pb,
                          size_t pn) {
    int x = socket(domain, SOCK_DGRAM, 0), y = socket(domain, SOCK_DGRAM, 0);
    if (x < 0 || y < 0) {
        if (x >= 0) close(x);
        if (y >= 0) close(y);
        return -1;
    }
    if (pa) pa[0] = 0;
    if (pb) pb[0] = 0;
    if (domain == AF_UNIX) {
        struct sockaddr_un sx, sy;
        if (net_unix_addr(&sx, "dgram-a") != 0 ||
            net_unix_addr(&sy, "dgram-b") != 0)
            goto fail;
        if (bind(x, (struct sockaddr *)&sx, sizeof sx) != 0 ||
            bind(y, (struct sockaddr *)&sy, sizeof sy) != 0)
            goto fail;
        if (pa) snprintf(pa, pn, "%s", sx.sun_path);
        if (pb) snprintf(pb, pn, "%s", sy.sun_path);
        if (connect(x, (struct sockaddr *)&sy, sizeof sy) != 0 ||
            connect(y, (struct sockaddr *)&sx, sizeof sx) != 0)
            goto fail;
    } else {
        struct sockaddr_in sx, sy;
        socklen_t lx = sizeof sx, ly = sizeof sy;
        memset(&sx, 0, sizeof sx);
        memset(&sy, 0, sizeof sy);
        sx.sin_family = sy.sin_family = AF_INET;
        sx.sin_addr.s_addr = sy.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (bind(x, (struct sockaddr *)&sx, sizeof sx) != 0 ||
            bind(y, (struct sockaddr *)&sy, sizeof sy) != 0 ||
            getsockname(x, (struct sockaddr *)&sx, &lx) != 0 ||
            getsockname(y, (struct sockaddr *)&sy, &ly) != 0)
            goto fail;
        if (connect(x, (struct sockaddr *)&sy, sizeof sy) != 0 ||
            connect(y, (struct sockaddr *)&sx, sizeof sx) != 0)
            goto fail;
    }
    *a = x;
    *b = y;
    return 0;
fail:
    close(x);
    close(y);
    if (pa && pa[0]) unlink(pa);
    if (pb && pb[0]) unlink(pb);
    return -1;
}

// Round trip over a connected pair, with the far end echoed by a thread.
// `ops_out` carries the achieved count so a caller can pair the row with the
// kernel's own per-syscall account of it.
static double net_pair_rt(int mine, int theirs, uint64_t budget_ns,
                          double *ops_out) {
    struct echo_ctl *c = calloc(1, sizeof *c);
    if (!c) {
        close(mine);
        close(theirs);
        return NA;
    }
    c->fd = theirs;
    c->refs = 1;
    sock_set_timeout(mine, 5);
    pthread_t th;
    CTL_LEND(c);
    if (pthread_create(&th, NULL, sock_echo_thread, c) != 0) {
        CTL_DROP(c);
        CTL_DROP(c);
        // This probe owns both ends from here on, so a failure closes them:
        // the suite runs a dozen socket probes and a leaked pair per failure
        // would eventually meet the descriptor limit in a later row, which
        // would then report a number for the wrong reason.
        close(mine);
        close(theirs);
        return NA;
    }
    double ns = sock_rt_drive(mine, budget_ns);
    if (ops_out)
        *ops_out = ns < 0 ? 0 : (double)g_last_ops;
    // Closing our end ends the echo loop. The peer's fd is closed by whoever
    // outlives the other: if the thread is abandoned it owns `theirs`, so the
    // caller must not touch it — see the return value.
    shutdown(mine, SHUT_RDWR);
    close(mine);
    int joined = join_or_abandon(th, &c->done, 5);
    if (joined)
        close(theirs);
    CTL_DROP(c);
    return ns;
}

// socket() + close() with nothing attached: the floor every socket row sits
// on, the same role `getpid()` plays for the syscall section.
static int g_sock_floor_domain, g_sock_floor_type;

static int net_socket_floor_op(void) {
    int fd = socket(g_sock_floor_domain, g_sock_floor_type, 0);
    if (fd < 0)
        return 1;
    close(fd);
    return 0;
}

static double net_socket_floor_ns(int domain, int type, uint64_t budget_ns) {
    g_sock_floor_domain = domain;
    g_sock_floor_type = type;
    return timed_ns_per_op(net_socket_floor_op, budget_ns);
}

// What a server pays per connection: connect, accept, and tear both ends down
// again, against a listener that stays up for the whole measurement. Kept
// apart from the round-trip rows because a server that answers fast and
// accepts slowly is a real and common shape, and one row cannot show both.
static double net_accept_cycle_ns(uint64_t budget_ns) {
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls < 0)
        return NA;
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t salen = sizeof sa;
    if (bind(ls, (struct sockaddr *)&sa, salen) != 0 || listen(ls, 16) != 0 ||
        getsockname(ls, (struct sockaddr *)&sa, &salen) != 0) {
        close(ls);
        return NA;
    }
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        int c, s;
        if (net_stream_connect(ls, AF_INET, (struct sockaddr *)&sa, sizeof sa,
                               &c, &s) != 0) {
            close(ls);
            return ops >= MIN_SAMPLES ? (double)elapsed / (double)ops : NA;
        }
        close(c);
        close(s);
        ops++;
        elapsed = now_ns() - t0;
    }
    close(ls);
    g_last_ops = ops;
    return ops ? (double)elapsed / (double)ops : NA;
}

// Streaming throughput over a connected pair, with a thread draining the far
// end. A round trip measures latency; this measures how much the copy path
// and the buffer handover cost when nobody is waiting for an answer.
static double net_pair_bw_mbs(int mine, int theirs, size_t chunk,
                              uint64_t budget_ns) {
    struct drain_ctl *c = calloc(1, sizeof *c);
    unsigned char *buf = malloc(chunk);
    if (!c || !buf) {
        free(c);
        free(buf);
        close(mine);
        close(theirs);
        return NA;
    }
    memset(buf, 0x5a, chunk);
    c->fd = theirs;
    c->buflen = chunk;
    c->refs = 1;
    pthread_t th;
    CTL_LEND(c);
    if (pthread_create(&th, NULL, sock_drain_thread, c) != 0) {
        CTL_DROP(c);
        CTL_DROP(c);
        free(buf);
        close(mine);
        close(theirs);
        return NA;
    }
    uint64_t t0 = now_ns(), elapsed = 0, bytes = 0, ops = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        ssize_t w = send(mine, buf, chunk, 0);
        if (w <= 0)
            break;
        bytes += (uint64_t)w;
        ops++;
        elapsed = now_ns() - t0;
    }
    elapsed = now_ns() - t0;
    shutdown(mine, SHUT_RDWR);
    close(mine);
    int joined = join_or_abandon(th, &c->done, 5);
    if (joined)
        close(theirs);
    CTL_DROP(c);
    free(buf);
    if (!bytes || !elapsed)
        return NA;
    return (double)bytes / ((double)elapsed / 1e9) / 1e6;
}

// `sendmsg`/`recvmsg` carrying the same single byte as the plain row. The
// difference between the two is what the iovec and control-message plumbing
// costs — and every library that passes an fd, or that wants the sender's
// address back, is on this path rather than the plain one.
static int g_msg_fd;

static int net_sendmsg_rt_op(void) {
    char b = 'x';
    struct iovec iov = {&b, 1};
    struct msghdr mh;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    if (sendmsg(g_msg_fd, &mh, 0) != 1)
        return 1;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    if (recvmsg(g_msg_fd, &mh, 0) != 1)
        return 1;
    return 0;
}

// Passing an fd over a UNIX socket, which is how a browser's zygote and every
// seccomp-sandboxed helper hand each other work. Measured as a full round
// trip — send the fd, get a byte back — because the interesting cost is the
// kernel installing a descriptor in the receiver, not the write.
static int g_scm_tx, g_scm_rx, g_scm_payload;

static int net_scm_rights_op(void) {
    char b = 'x';
    struct iovec iov = {&b, 1};
    union {
        struct cmsghdr align;
        char buf[CMSG_SPACE(sizeof(int))];
    } cm;
    struct msghdr mh;
    memset(&mh, 0, sizeof mh);
    memset(&cm, 0, sizeof cm);
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cm.buf;
    mh.msg_controllen = sizeof cm.buf;
    struct cmsghdr *h = CMSG_FIRSTHDR(&mh);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &g_scm_payload, sizeof(int));
    if (sendmsg(g_scm_tx, &mh, 0) != 1)
        return 1;
    // Receive it back out, and close the descriptor the kernel installed —
    // otherwise this probe leaks one fd per iteration and dies on EMFILE
    // partway through, reporting a number from however many it managed.
    char rb;
    struct iovec riov = {&rb, 1};
    memset(&mh, 0, sizeof mh);
    memset(&cm, 0, sizeof cm);
    mh.msg_iov = &riov;
    mh.msg_iovlen = 1;
    mh.msg_control = cm.buf;
    mh.msg_controllen = sizeof cm.buf;
    if (recvmsg(g_scm_rx, &mh, 0) != 1)
        return 1;
    int got = -1;
    for (struct cmsghdr *r = CMSG_FIRSTHDR(&mh); r; r = CMSG_NXTHDR(&mh, r)) {
        if (r->cmsg_level == SOL_SOCKET && r->cmsg_type == SCM_RIGHTS &&
            r->cmsg_len == CMSG_LEN(sizeof(int)))
            memcpy(&got, CMSG_DATA(r), sizeof(int));
    }
    if (got < 0)
        return 1; // the fd did not arrive: unsupported, not slow
    close(got);
    return 0;
}

// The non-blocking empty-socket read. Every event loop issues one of these per
// readiness notification that turns out to be spurious, and it is pure kernel
// entry plus a queue check, so it isolates the socket layer's own overhead
// from the wake-up the round-trip rows are dominated by.
static int g_eagain_fd;

static int net_eagain_op(void) {
    char b;
    ssize_t r = recv(g_eagain_fd, &b, 1, MSG_DONTWAIT);
    if (r >= 0)
        return 1; // somebody wrote to it: the probe is not measuring EAGAIN
    return (errno == EAGAIN || errno == EWOULDBLOCK) ? 0 : 1;
}

// ---- readiness syscalls ---------------------------------------------------
//
// An event loop's floor. One fd is ready and it is the LAST one in the array,
// which is the worst case for an implementation that walks the set and the
// best case for one that keeps a ready list — so the per-fd slope between the
// 1-fd and N-fd rows is the answer, not either row alone.

#define RDY_MAX 64
static struct pollfd g_rdy_pfd[RDY_MAX];
static int g_rdy_n;
static int g_rdy_epfd = -1;

static int net_poll_op(void) {
    for (int i = 0; i < g_rdy_n; i++)
        g_rdy_pfd[i].revents = 0;
    int r = poll(g_rdy_pfd, (unsigned)g_rdy_n, 0);
    return r == 1 ? 0 : 1;
}

static int net_select_op(void) {
    fd_set rs;
    FD_ZERO(&rs);
    int max = -1;
    for (int i = 0; i < g_rdy_n; i++) {
        if (g_rdy_pfd[i].fd >= FD_SETSIZE)
            return 1;
        FD_SET(g_rdy_pfd[i].fd, &rs);
        if (g_rdy_pfd[i].fd > max)
            max = g_rdy_pfd[i].fd;
    }
    struct timeval tv = {0, 0};
    int r = select(max + 1, &rs, NULL, NULL, &tv);
    return r == 1 ? 0 : 1;
}

static int net_epoll_wait_op(void) {
    struct epoll_event ev[4];
    int r = epoll_wait(g_rdy_epfd, ev, 4, 0);
    return r == 1 ? 0 : 1;
}

static int net_epoll_ctl_op(void) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = g_rdy_pfd[0].fd;
    if (epoll_ctl(g_rdy_epfd, EPOLL_CTL_ADD, g_rdy_pfd[0].fd, &ev) != 0)
        return 1;
    if (epoll_ctl(g_rdy_epfd, EPOLL_CTL_DEL, g_rdy_pfd[0].fd, NULL) != 0)
        return 1;
    return 0;
}

// Build `n` pipes, leave a byte sitting in the LAST one, and fill the poll
// array with the read ends. Returns how many were made.
static int net_rdy_setup(int n, int rfd[], int wfd[]) {
    if (n > RDY_MAX)
        n = RDY_MAX;
    int made = 0;
    for (int i = 0; i < n; i++) {
        int p[2];
        if (pipe(p) != 0)
            break;
        rfd[i] = p[0];
        wfd[i] = p[1];
        made++;
    }
    if (made == 0)
        return 0;
    char b = 'r';
    if (write(wfd[made - 1], &b, 1) != 1) {
        for (int i = 0; i < made; i++) { close(rfd[i]); close(wfd[i]); }
        return 0;
    }
    g_rdy_n = made;
    for (int i = 0; i < made; i++) {
        g_rdy_pfd[i].fd = rfd[i];
        g_rdy_pfd[i].events = POLLIN;
        g_rdy_pfd[i].revents = 0;
    }
    return made;
}

static void net_rdy_teardown(int made, int rfd[], int wfd[]) {
    for (int i = 0; i < made; i++) {
        close(rfd[i]);
        close(wfd[i]);
    }
    g_rdy_n = 0;
}

// ---------------------------------------------------------------------------
// Filesystem / VFS  [kernel]
// ---------------------------------------------------------------------------
//
// Deliberately NOT the DISK section. That one streams megabytes and measures
// the block device; this one touches almost no data and measures the layer
// above it: resolving a path, opening a descriptor, answering `stat`, reading
// a directory, serving a warm page out of the cache, and answering the procfs
// files every tool on the system reads. Those are the operations a shell, a
// build and a process lister spend their time in, and a kernel can be fast at
// streaming and slow at every one of them.
//
// Every row here runs against files that were just written, so the data is in
// cache and the device is out of the picture. Where the distinction matters
// the row says so.

// Sized above the caller's DIR plus the longest name appended to it, so the
// compiler can see that none of these can be truncated: a silently shortened
// path would make a probe measure a file that is not the one it built.
static char g_fs_dir[512];      // our own subdirectory, removed at the end
static char g_fs_deep[576];     // a file eight components down
static char g_fs_shallow[576];  // the same file, one component down
static char g_fs_link[576];     // a symlink to it
static char g_fs_list[576];     // a directory with many entries
static int g_fs_dirfd = -1;     // open handle on g_fs_dir, for the *at rows
static int g_fs_filefd = -1;    // open handle on the data file
static int g_fs_listed;         // entries actually created in g_fs_list
static size_t g_fs_bytes;       // size of the data file

// Build the tree. Returns 0 on success; on failure the section is skipped
// rather than reporting numbers from a half-built tree.
static int fs_setup(const char *dir, size_t bytes) {
    snprintf(g_fs_dir, sizeof g_fs_dir, "%s/eclipse-bench-fs", dir);
    mkdir(g_fs_dir, 0755);
    snprintf(g_fs_shallow, sizeof g_fs_shallow, "%s/data", g_fs_dir);
    int fd = open(g_fs_shallow, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0)
        return -1;
    unsigned char *buf = malloc(65536);
    if (!buf) {
        close(fd);
        return -1;
    }
    memset(buf, 0x3c, 65536);
    size_t left = bytes;
    while (left) {
        size_t n = left > 65536 ? 65536 : left;
        ssize_t w = write(fd, buf, n);
        if (w <= 0)
            break;
        left -= (size_t)w;
    }
    free(buf);
    close(fd);
    g_fs_bytes = bytes - left;
    if (g_fs_bytes < 65536)
        return -1;

    // Eight nested components with the same file at the bottom. The pair of
    // open rows then differs in exactly one thing — how many components the
    // kernel had to resolve — so their difference is the per-component cost
    // of a lookup, which no single row can show.
    char path[512];
    size_t n = (size_t)snprintf(path, sizeof path, "%s", g_fs_dir);
    for (int i = 0; i < 8 && n < sizeof path - 4; i++) {
        n += (size_t)snprintf(path + n, sizeof path - n, "/d%d", i);
        mkdir(path, 0755);
    }
    snprintf(g_fs_deep, sizeof g_fs_deep, "%s/data", path);
    fd = open(g_fs_deep, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0)
        return -1;
    ssize_t w = write(fd, "x", 1);
    (void)w;
    close(fd);

    snprintf(g_fs_link, sizeof g_fs_link, "%s/data.link", g_fs_dir);
    unlink(g_fs_link);
    if (symlink("data", g_fs_link) != 0)
        g_fs_link[0] = 0; // no symlink support: that row reports n/a

    snprintf(g_fs_list, sizeof g_fs_list, "%s/many", g_fs_dir);
    mkdir(g_fs_list, 0755);
    g_fs_listed = 0;
    for (int i = 0; i < 256; i++) {
        char e[640];
        snprintf(e, sizeof e, "%s/e%03d", g_fs_list, i);
        int efd = open(e, O_CREAT | O_WRONLY, 0644);
        if (efd < 0)
            break;
        close(efd);
        g_fs_listed++;
    }

    g_fs_dirfd = open(g_fs_dir, O_RDONLY | O_DIRECTORY);
    g_fs_filefd = open(g_fs_shallow, O_RDONLY);
    return g_fs_filefd >= 0 ? 0 : -1;
}

static void fs_teardown(void) {
    if (g_fs_dirfd >= 0) { close(g_fs_dirfd); g_fs_dirfd = -1; }
    if (g_fs_filefd >= 0) { close(g_fs_filefd); g_fs_filefd = -1; }
    for (int i = 0; i < g_fs_listed; i++) {
        char e[640];
        snprintf(e, sizeof e, "%s/e%03d", g_fs_list, i);
        unlink(e);
    }
    rmdir(g_fs_list);
    unlink(g_fs_deep);
    // Unwind the chain from the bottom up: rmdir only removes empty ones.
    for (int depth = 8; depth >= 1; depth--) {
        char path[512];
        size_t n = (size_t)snprintf(path, sizeof path, "%s", g_fs_dir);
        for (int i = 0; i < depth && n < sizeof path - 4; i++)
            n += (size_t)snprintf(path + n, sizeof path - n, "/d%d", i);
        rmdir(path);
    }
    if (g_fs_link[0])
        unlink(g_fs_link);
    unlink(g_fs_shallow);
    rmdir(g_fs_dir);
}

static const char *g_fs_open_path;
static int g_fs_open_flags;

static int fs_open_op(void) {
    int fd = open(g_fs_open_path, g_fs_open_flags);
    if (fd < 0)
        return 1;
    close(fd);
    return 0;
}

static double fs_open_ns(const char *path, int flags, uint64_t budget_ns) {
    g_fs_open_path = path;
    g_fs_open_flags = flags;
    return timed_ns_per_op(fs_open_op, budget_ns);
}

// The same open, reached through a directory descriptor instead of a path.
// Every build tool and every `find` does this; the gap against the full-path
// row is what the resolution it skips was costing.
static int fs_openat_op(void) {
    int fd = openat(g_fs_dirfd, "data", O_RDONLY);
    if (fd < 0)
        return 1;
    close(fd);
    return 0;
}

static int fs_stat_op(void) {
    struct stat st;
    return stat(g_fs_shallow, &st) == 0 ? 0 : 1;
}

static int fs_lstat_op(void) {
    struct stat st;
    return lstat(g_fs_shallow, &st) == 0 ? 0 : 1;
}

static int fs_fstatat_op(void) {
    struct stat st;
    return fstatat(g_fs_dirfd, "data", &st, 0) == 0 ? 0 : 1;
}

static int fs_fstat_op(void) {
    struct stat st;
    return fstat(g_fs_filefd, &st) == 0 ? 0 : 1;
}

static int fs_access_op(void) {
    return access(g_fs_shallow, F_OK) == 0 ? 0 : 1;
}

static int fs_readlink_op(void) {
    char b[64];
    ssize_t r = readlinkat(g_fs_dirfd, "data.link", b, sizeof b);
    return r > 0 ? 0 : 1;
}

static int fs_lseek_op(void) {
    return lseek(g_fs_filefd, 0, SEEK_SET) == 0 ? 0 : 1;
}

static int fs_dup_op(void) {
    int fd = dup(g_fs_filefd);
    if (fd < 0)
        return 1;
    close(fd);
    return 0;
}

static int fs_fcntl_op(void) {
    return fcntl(g_fs_filefd, F_GETFL, 0) >= 0 ? 0 : 1;
}

static int fs_pipe_op(void) {
    int p[2];
    if (pipe(p) != 0)
        return 1;
    close(p[0]);
    close(p[1]);
    return 0;
}

// Warm reads. The file was written moments ago and is read from offset 0 every
// time, so this is the cache path: no device, no readahead decision, just the
// cost of getting bytes from the page cache into a userspace buffer. `pread`
// is the same work without the implicit seek, so the pair isolates what
// maintaining the file position costs.
static unsigned char *g_fs_rbuf;
static size_t g_fs_rlen;

static int fs_read_op(void) {
    if (lseek(g_fs_filefd, 0, SEEK_SET) != 0)
        return 1;
    ssize_t r = read(g_fs_filefd, g_fs_rbuf, g_fs_rlen);
    return r == (ssize_t)g_fs_rlen ? 0 : 1;
}

static int fs_pread_op(void) {
    ssize_t r = pread(g_fs_filefd, g_fs_rbuf, g_fs_rlen, 0);
    return r == (ssize_t)g_fs_rlen ? 0 : 1;
}

// The whole file through one mapping versus the same bytes through `read`.
// A program that mmaps a file pays faults instead of copies; which is cheaper
// is a property of this kernel, not a given, and the ratio row says which.
static double fs_mmap_read_mibs(uint64_t budget_ns) {
    uint64_t t0 = now_ns(), elapsed = 0, bytes = 0, ops = 0;
    while ((elapsed < budget_ns || ops < 4) && elapsed < g_max_ns) {
        void *m = mmap(NULL, g_fs_bytes, PROT_READ, MAP_PRIVATE, g_fs_filefd, 0);
        if (m == MAP_FAILED)
            return NA;
        // Touch one byte per page rather than memcpy the lot: the subject is
        // the fault path, and a copy would bury it under memory bandwidth.
        volatile unsigned char sink = 0;
        for (size_t off = 0; off < g_fs_bytes; off += 4096)
            sink ^= ((unsigned char *)m)[off];
        g_sink += sink;
        munmap(m, g_fs_bytes);
        bytes += g_fs_bytes;
        ops++;
        elapsed = now_ns() - t0;
    }
    if (!bytes || !elapsed)
        return NA;
    g_last_ops = ops;
    return (double)bytes / ((double)elapsed / 1e9) / (1024.0 * 1024.0);
}

// Directory reads, charged per entry. `ps`, a shell glob and every `ls` live
// here, and the per-entry figure is what scales with a big directory while the
// per-call one hides it.
static double fs_getdents_per_entry_ns(const char *path, uint64_t budget_ns,
                                       double *entries_out, double *calls_out) {
    char *buf = malloc(32768);
    if (!buf)
        return NA;
    uint64_t t0 = now_ns(), elapsed = 0, ops = 0, entries = 0, calls = 0;
    while ((elapsed < budget_ns || ops < MIN_SAMPLES) && elapsed < g_max_ns) {
        int fd = open(path, O_RDONLY | O_DIRECTORY);
        if (fd < 0) {
            free(buf);
            return NA;
        }
        for (;;) {
            long n = syscall(SYS_getdents64, fd, buf, (size_t)32768);
            if (n <= 0)
                break;
            // Only calls that returned entries, so the derived row below is a
            // batch size and not a batch size diluted by the one call every
            // walk makes to learn it has reached the end.
            calls++;
            // Walk the records to count them: the byte count is not an entry
            // count, and a kernel returning one entry per call would otherwise
            // look identical to one returning sixty.
            for (long off = 0; off < n;) {
                // struct linux_dirent64: ino(8) off(8) reclen(2) type(1) name
                unsigned short reclen;
                memcpy(&reclen, buf + off + 16, sizeof reclen);
                if (reclen == 0)
                    break;
                entries++;
                off += reclen;
            }
        }
        close(fd);
        ops++;
        elapsed = now_ns() - t0;
    }
    free(buf);
    if (entries_out)
        *entries_out = (double)entries;
    if (calls_out)
        *calls_out = (double)calls;
    if (!entries || !elapsed)
        return NA;
    g_last_ops = ops;
    return (double)elapsed / (double)entries;
}

// procfs, read the way real tools read it: open, read to EOF, close. Each of
// these files is generated on demand, so the cost is the kernel formatting a
// report, not a filesystem lookup — and a tool like `ps` pays it once per
// process on the machine, which is why a slow one is felt and not just
// measured.
static const char *g_fs_proc_path;

static int fs_proc_read_op(void) {
    int fd = open(g_fs_proc_path, O_RDONLY);
    if (fd < 0)
        return 1;
    char buf[4096];
    ssize_t total = 0;
    for (;;) {
        ssize_t r = read(fd, buf, sizeof buf);
        if (r <= 0)
            break;
        total += r;
    }
    close(fd);
    return total > 0 ? 0 : 1;
}

static double fs_proc_read_ns(const char *path, uint64_t budget_ns) {
    g_fs_proc_path = path;
    return timed_ns_per_op(fs_proc_read_op, budget_ns);
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

static int want(const char *only, const char *section) {
    return !only || strcmp(only, section) == 0;
}

int main(int argc, char **argv) {
    // Self-exec target for the fork+exec benchmark: exit immediately.
    if (argc > 1 && strcmp(argv[1], "--noop") == 0)
        return 0;

    // Line-buffer stdout. When this runs on a serial console under QEMU, or
    // with its output piped to a file, libc would otherwise pick full
    // buffering and hold everything until the 4 KiB buffer fills — so a run
    // that hangs or panics mid-suite loses the very rows that would say where.
    setvbuf(stdout, NULL, _IOLBF, 0);

    // `--forkloop N MIB`: fork/exit N times with MIB MiB of pre-faulted private
    // memory resident, printing the cost of EVERY iteration as it happens.
    //
    // Not a benchmark — a diagnostic. When `fork` hangs intermittently, waiting
    // for a random hang and then staring at a silent console says nothing about
    // which of the two possible shapes it is, and each attempt costs a full boot.
    // A per-iteration trace answers it in one run:
    //
    //   * iterations getting steadily slower  -> algorithmic. Something is
    //     accumulating per fork (a copy-on-write tree that is not collapsing,
    //     a growing mapping list), and the "hang" is just the curve going
    //     vertical.
    //   * iterations flat, then one never finishes -> a stall. A deadlock, a
    //     lost wakeup, or an unresolvable fault loop, and the iteration number
    //     says how much state it took to get there.
    //
    // The optional third argument adds that many extra one-page mappings, which
    // turns the same trace into the answer to a different question: the
    // benchmark's `fork cost per mapping` row times `fork + exit` together, so a
    // large per-mapping cost could live in either half. Splitting `fork` from
    // `wait` says which — the parent's copy-on-write setup, or the child tearing
    // its address space down again on exit — and those are different bugs in
    // different files.
    //
    // Line-buffered, so the last line printed is the last iteration that
    // completed even if the machine dies mid-fork.
    if (argc > 1 && strcmp(argv[1], "--forkloop") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        long iters = argc > 2 ? strtol(argv[2], NULL, 10) : 40;
        size_t mib = argc > 3 ? (size_t)strtoul(argv[3], NULL, 10) : 1;
        int maps = argc > 4 ? (int)strtol(argv[4], NULL, 10) : 0;
        size_t len = mib * 1024 * 1024;
        unsigned char *p = NULL;
        if (len) {
            p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (p == MAP_FAILED) {
                printf("forkloop: mmap %zu MiB failed\n", mib);
                return 1;
            }
            for (size_t i = 0; i < len; i += 4096)
                p[i] = (unsigned char)(i >> 12);
        }
        unsigned char **spots = NULL;
        int made = scatter_mappings(maps, &spots);
        printf("forkloop: %ld iterations, %zu MiB resident, %d extra mappings\n",
               iters, mib, made);
        for (long i = 0; i < iters; i++) {
            uint64_t t0 = now_ns();
            pid_t c = fork();
            if (c == 0) _exit(0);
            if (c < 0) { printf("forkloop: fork failed at %ld\n", i); return 1; }
            uint64_t t1 = now_ns();          // fork returned in the parent
            int st;
            waitpid(c, &st, 0);
            uint64_t t2 = now_ns();
            // Split so a stall can be attributed to fork itself versus the
            // child's exit and reaping.
            printf("forkloop %3ld: fork %8.0f us  wait %8.0f us\n", i,
                   (double)(t1 - t0) / 1000.0, (double)(t2 - t1) / 1000.0);
        }
        scatter_free(spots, made);
        printf("forkloop: done\n");
        return 0;
    }

    // `--yieldstall [SECONDS] [NOTIFIERS]`: a deterministic reproducer for a
    // sched_yield() that never returns.
    //
    // Not a benchmark — a diagnostic, like `--forkloop`. The `psched` section
    // reports that Eclipse's `sched_yield()` can block indefinitely, but it
    // finds it as a side effect of measuring something else and only
    // sometimes, which is no use to anyone trying to fix it. This builds the
    // condition on purpose instead of waiting for it.
    //
    // The recipe follows the shape of the executor. A voluntary yield parks
    // its self-wake in a *yielded* lane, and that lane is drained only once
    // nothing is notified on the CPU. So: confine everything to ONE CPU, put
    // two threads in a tight `sched_yield()` loop, and keep a steady stream of
    // notifies arriving at that same CPU from a third thread doing short
    // sleeps. If the yielded lane is strictly lower priority than the notified
    // one, the two yielders are starved for as long as the notifier keeps
    // going, and neither `sched_yield()` call ever comes back.
    //
    // The observer is pinned OFF that CPU: a reporter sharing the CPU under
    // test would be one more runnable task on it and would change the thing
    // being observed. One line per second, so a stall is visible as it happens
    // rather than inferred from a silent console, and the verdict at the end
    // names which thread stopped and when.
    if (argc > 1 && strcmp(argv[1], "--yieldstall") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        int secs = argc > 2 ? (int)strtol(argv[2], NULL, 10) : 20;
        int nnotify = argc > 3 ? (int)strtol(argv[3], NULL, 10) : 1;
        if (secs < 1) secs = 1;
        if (nnotify < 0) nnotify = 0;
        if (nnotify > YS_MAX_NOTIFY) nnotify = YS_MAX_NOTIFY;
        long nc = sysconf(_SC_NPROCESSORS_ONLN);
        int ncpus = (nc > 0 && nc < 4096) ? (int)nc : 1;
        if (ncpus < 2) {
            printf("yieldstall: needs at least 2 CPUs (the observer must not "
                   "share the CPU under test)\n");
            return 2;
        }
        memset(&g_ys, 0, sizeof g_ys);
        g_ys.nnotify = nnotify;
        g_ys.a.stop = &g_ys.stop;
        g_ys.b.stop = &g_ys.stop;
        // Confine this thread to CPU 0 FIRST, so every thread created below
        // inherits that mask and starts life on the CPU under test.
        if (smpk_pin_self(0) != 0) {
            printf("yieldstall: could not pin to CPU 0 — nothing to test\n");
            return 2;
        }
        pthread_t ya, yb, nt[8];
        int made_n = 0;
        if (pthread_create(&ya, NULL, ys_yielder, &g_ys.a) != 0 ||
            pthread_create(&yb, NULL, ys_yielder, &g_ys.b) != 0) {
            printf("yieldstall: could not create the yielder threads\n");
            return 2;
        }
        for (int i = 0; i < nnotify; i++) {
            if (pthread_create(&nt[i], NULL, ys_notifier, &g_ys) != 0)
                break;
            made_n++;
        }
        // Move the observer off CPU 0 so it is not competing with what it
        // watches.
        cpu_set_t obs;
        CPU_ZERO(&obs);
        for (int i = 1; i < ncpus && i < CPU_SETSIZE; i++)
            CPU_SET(i, &obs);
        pthread_setaffinity_np(pthread_self(), sizeof obs, &obs);
        printf("yieldstall: 2 yielders + %d notifier(s), all on CPU 0; "
               "observer on CPU 1..%d; %d s\n", made_n, ncpus - 1, secs);
        printf("yieldstall: a yields/s of 0 while notifies keep arriving IS "
               "the bug\n");
        int have_kstat = kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
        uint64_t pa = 0, pb = 0, pn = 0;
        uint64_t stall_a = 0, stall_b = 0, stall_n = 0;
        uint64_t t0 = now_ns();
        for (int s = 0; s < secs; s++) {
            struct timespec one = {1, 0};
            nanosleep(&one, NULL);
            uint64_t ca = g_ys.a.count, cb = g_ys.b.count,
                     cn = __atomic_load_n(&g_ys.notifies, __ATOMIC_RELAXED);
            uint64_t da = ca - pa, db = cb - pb, dn = cn - pn;
            double at = (double)(now_ns() - t0) / 1e9;
            printf("yieldstall %5.1fs: A %10llu/s  B %10llu/s  notifies %8llu/s\n",
                   at, (unsigned long long)da, (unsigned long long)db,
                   (unsigned long long)dn);
            // First second in which a yielder made no progress at all while
            // the notifier did: that is the lane being starved, not the thread
            // merely running slowly.
            // A participant is starved when it made no progress in a whole
            // second while at least one OTHER participant on the same CPU
            // did. That phrasing is deliberate: it catches a starved notifier
            // just as well as a starved yielder, and it does not fire when
            // the whole CPU simply stopped.
            int others_ran = (da > 0) + (db > 0) + (dn > 0);
            if (da == 0 && others_ran > 0 && !stall_a)
                stall_a = (uint64_t)(at * 1000);
            if (db == 0 && others_ran > 0 && !stall_b)
                stall_b = (uint64_t)(at * 1000);
            if (made_n > 0 && dn == 0 && others_ran > 0 && !stall_n)
                stall_n = (uint64_t)(at * 1000);
            pa = ca; pb = cb; pn = cn;
        }
        if (have_kstat)
            have_kstat = kstat_snapshot(g_kstat_b, sizeof g_kstat_b);
        g_ys.stop = 1;
        int participants = 2 + made_n;
        int starved = (stall_a != 0) + (stall_b != 0) + (stall_n != 0);
        printf("yieldstall: verdict:\n");
        printf("  %d threads on CPU 0; %d of them were starved at least one "
               "whole second\n  while another was running.\n",
               participants, starved == 0 ? 0 : starved);
        if (stall_a)
            printf("    yielder A: no progress from %llu ms%s\n",
                   (unsigned long long)stall_a,
                   g_ys.a.started ? "" : " (and never reached its first yield)");
        if (stall_b)
            printf("    yielder B: no progress from %llu ms%s\n",
                   (unsigned long long)stall_b,
                   g_ys.b.started ? "" : " (and never reached its first yield)");
        if (stall_n)
            printf("    notifier(s): no progress from %llu ms%s\n",
                   (unsigned long long)stall_n,
                   __atomic_load_n(&g_ys.notify_started, __ATOMIC_RELAXED) >=
                           made_n
                       ? ""
                       : " (and not all of them started)");
        if (starved == 0)
            printf("    none: every thread kept advancing for the whole run.\n");
        // Where each thread actually sat. All of them were pinned to CPU 0 by
        // a parent that was already confined to CPU 0, so anything other than
        // 0 here is the answer on its own.
        printf("  placement: A pin=%d cpu=%d->%d   B pin=%d cpu=%d->%d\n",
               g_ys.a.pin_rc, g_ys.a.cpu_seen, g_ys.a.cpu_last,
               g_ys.b.pin_rc, g_ys.b.cpu_seen, g_ys.b.cpu_last);
        int notify_elsewhere = 0;
        if (made_n > 0) {
            printf("             notifier cpu=");
            for (int i = 0; i < made_n && i < YS_MAX_NOTIFY; i++) {
                int cp = g_ys.notify_cpu[i];
                printf("%s%d", i ? "," : "", cp);
                if (cp != 0)
                    notify_elsewhere = 1;
            }
            printf("\n");
        }
        if (g_ys.a.cpu_seen != 0 || g_ys.b.cpu_seen != 0 || notify_elsewhere)
            printf("    ^ every thread was pinned to CPU 0 by an already-"
                   "confined parent, so a\n      thread reporting another CPU "
                   "means the mask did not take effect\n      where it ran: it "
                   "is stranded, not merely descheduled.\n");
        if (have_kstat) {
            // Disjoint counters: `skipped` is bumped on an early return that
            // happens before `scans`. See the psched section.
            double scans = kstat_delta("sched steal:", 0);
            double probed = kstat_delta("sched steal:", 1);
            double ok = kstat_delta("sched steal:", 2);
            double aff = kstat_delta("sched steal:", 3);
            double skip = kstat_delta("sched steal:", 5);
            printf("  steal over the run: %.0f scans, %.0f probed, %.0f ok, "
                   "%.0f affinity-empty, %.0f skipped\n",
                   scans, probed, ok, aff, skip);
            printf("    ^ scans near zero, or affinity-empty climbing, is a "
                   "CPU that never came\n      looking or looked and found "
                   "nothing it was allowed to take. `ok` rising\n      "
                   "instead means work WAS being moved and the stall is "
                   "elsewhere.\n");
        }
        printf("  totals: A %llu yields (started %d), B %llu yields "
               "(started %d), %llu notifies (%d of %d notifiers started)\n",
               (unsigned long long)g_ys.a.count, g_ys.a.started,
               (unsigned long long)g_ys.b.count, g_ys.b.started,
               (unsigned long long)__atomic_load_n(&g_ys.notifies,
                                                  __ATOMIC_RELAXED),
               __atomic_load_n(&g_ys.notify_started, __ATOMIC_RELAXED),
               made_n);
        // What this does and does not establish. A starved YIELDER is
        // consistent with the voluntary lane being passed over; a starved
        // NOTIFIER is not, because a sleeper's timer wake is the notified
        // lane, and a run where the notifiers are the starved ones rules the
        // lane ordering out as the whole story. So the tool reports which
        // threads starved and leaves the mechanism to whoever reads it.
        if (starved > 0)
            printf("  note: a starved notifier is a sleeper whose timer wake "
                   "went unserved,\n  which the lane ordering alone does not "
                   "explain. Read the rows above for\n  WHICH threads "
                   "progressed, not just how many.\n");
        // Do NOT join a starved thread: it may be inside the call that did
        // not return, and the join would hang the diagnostic that just
        // proved it.
        if (starved > 0)
            printf("  (not joining: a starved thread may be inside the call "
                   "that did not return)\n");
        fflush(stdout);
        _exit(0);
    }

    const char *only = NULL;
    int argi = 1;
    while (argi < argc && argv[argi][0] == '-' && argv[argi][1] == '-') {
        if (strcmp(argv[argi], "--drm") == 0 && argi + 1 < argc) {
            g_drm_path = argv[++argi];
        } else if (strcmp(argv[argi], "--only") == 0 && argi + 1 < argc) {
            only = argv[++argi];
        } else if (strcmp(argv[argi], "--quick") == 0) {
            g_budget_ns = 150000000ull;
            g_short_ns = 60000000ull;
        } else if (strcmp(argv[argi], "--budget") == 0 && argi + 1 < argc) {
            // Per-measurement wall-clock budget in milliseconds. Raise it when
            // the numbers are noisy: every time-bounded probe simply collects
            // more samples.
            uint64_t ms = strtoull(argv[++argi], NULL, 10);
            if (ms < 10) ms = 10;
            g_short_ns = ms * 1000000ull;
            g_budget_ns = g_short_ns * 2;
        } else if (strcmp(argv[argi], "--max") == 0 && argi + 1 < argc) {
            // Ceiling per measurement in milliseconds; raise it if a slow path
            // is being truncated, lower it to bound a run.
            uint64_t ms = strtoull(argv[++argi], NULL, 10);
            if (ms < 100) ms = 100;
            g_max_ns = ms * 1000000ull;
        } else {
            fprintf(stderr,
                    "usage: %s [--only SECTION] [--quick] [--drm PATH] [--budget MS] [--max MS]"
                    " [DIR] [DISK_MB] [MEM_MB]\n",
                    argv[0]);
            fprintf(stderr,
                    "sections: cpu mem syscall vm sched psched net fs smp disk\n"
                    "          proc gfx\n");
            fprintf(stderr,
                    "diagnostics: --forkloop N MIB [MAPS], "
                    "--yieldstall [SECONDS] [NOTIFIERS]\n");
            return 2;
        }
        argi++;
    }

    const char *dir = (argc > argi) ? argv[argi] : ".";
    size_t disk_mb = (argc > argi + 1) ? (size_t)strtoul(argv[argi + 1], NULL, 10) : 32;
    size_t mem_mb = (argc > argi + 2) ? (size_t)strtoul(argv[argi + 2], NULL, 10) : 32;
    if (disk_mb < 1) disk_mb = 1;
    if (mem_mb < 1) mem_mb = 1;

    char self[512];
    ssize_t sl = readlink("/proc/self/exe", self, sizeof self - 1);
    if (sl > 0) self[sl] = 0;
    else snprintf(self, sizeof self, "%s", argv[0]);

    long ncpu_l = sysconf(_SC_NPROCESSORS_ONLN);
    int ncpu = (ncpu_l > 0 && ncpu_l < 4096) ? (int)ncpu_l : 2;

    printf("eclipse-bench — CPU / memory / syscall / VM / scheduler / sockets /\n");
    printf("                filesystem / disk / process\n");
    printf("dir=%s  disk=%zu MiB  mem=%zu MiB  cpus=%d\n", dir, disk_mb, mem_mb, ncpu);
    printf("self=%s\n", self);
    printf("\n");
    printf("[user]   userspace only — a property of the CPU, NOT of the OS.\n");
    printf("         Linux on this machine must produce the same numbers.\n");
    printf("[kernel] dominated by kernel code — this is where an OS is fast or slow.\n");
    printf("The `linux:` hints are order-of-magnitude orientation for a modern\n");
    printf("x86_64 box, not a target. For a real comparison run THIS binary on\n");
    printf("Linux on the SAME machine and diff the output.\n");
    printf("\n");

    char hb[32];
    double r;

    // Values kept for the RATIOS section.
    double gfx_ioctl_ns = NA, gfx_flip_us = NA, gfx_vblank_ms = NA;
    double gfx_fbwrite_mibs = NA, mem_copy_mibs = NA;
    double cpu_chain_mops = NA, getpid_ns = NA, pipe_proc_ns = NA;
    double sleep_idle_us = NA, sleep_load_us = NA;
    double sleep_idle_max_us = NA, sleep_load_max_us = NA;
    double smp1 = NA, smpn = NA;
    double fork_copy_ratio = NA;

    // ---- CPU ----
    if (want(only, "cpu")) {
        line();
        printf("CPU\n");
        r = timed_oprate(cpu_int_chain, g_budget_ns);
        cpu_chain_mops = r / 1e6;
        row("[user]", "int latency (dependent)", cpu_chain_mops, "Mops/s", "");
        r = timed_oprate(cpu_int_tput, g_budget_ns);
        row("[user]", "int throughput (4-wide)", r * 4 / 1e6, "Mops/s", "");
        r = timed_oprate(cpu_double_chain, g_budget_ns);
        row("[user]", "float latency (dependent)", r / 1e6, "Mops/s", "");
        printf("  (if these differ from Linux on this machine, it is the P-state\n");
        printf("   governor or the hypervisor — no kernel path is being measured.)\n");
    } else {
        // The RATIOS section needs a clock reference even when CPU is skipped.
        cpu_chain_mops = timed_oprate(cpu_int_chain, g_short_ns) / 1e6;
    }

    // ---- Memory ----
    if (want(only, "mem")) {
        line();
        printf("MEMORY (working set %zu MiB)\n", mem_mb);
        g_mem_bytes = mem_mb * 1024 * 1024;
        g_mem_src = malloc(g_mem_bytes);
        g_mem_dst = malloc(g_mem_bytes);
        size_t chase_n = g_mem_bytes / sizeof(size_t);
        g_chase = malloc(chase_n * sizeof(size_t));
        if (!g_mem_src || !g_mem_dst || !g_chase) {
            printf("  (allocation failed — try a smaller MEM_MB)\n");
        } else {
            // Pre-fault both buffers: this section is meant to measure the
            // memory system, not the page-fault path (that is the VM section).
            memset(g_mem_src, 0xa5, g_mem_bytes);
            memset(g_mem_dst, 0x5a, g_mem_bytes);
            r = timed_oprate(mem_copy, g_budget_ns);
            hr_bytes(r * (double)g_mem_bytes, hb, sizeof hb);
            printf("  %-8s %-28s %12s\n", "[user]", "memcpy bandwidth", hb);
            mem_copy_mibs = r * (double)g_mem_bytes / (1024.0 * 1024.0);
            r = timed_oprate(mem_set, g_budget_ns);
            hr_bytes(r * (double)g_mem_bytes, hb, sizeof hb);
            printf("  %-8s %-28s %12s\n", "[user]", "memset bandwidth", hb);
            build_chase(chase_n);
            r = timed_oprate(mem_chase, g_budget_ns);
            row("[user]", "random access latency", 1e9 / r, "ns", "(pre-faulted)");
        }
        free(g_mem_src); free(g_mem_dst); free(g_chase);
        g_mem_src = g_mem_dst = NULL; g_chase = NULL;
    }

    // ---- Syscall ----
    g_devnull = open("/dev/null", O_RDWR);
    g_devzero = open("/dev/zero", O_RDONLY);
    if (want(only, "syscall")) {
        line();
        printf("SYSCALL (round trip into the kernel and back)\n");
        printf("  vDSO: %s\n", vdso_presence());
        getpid_ns = timed_ns_per_op(sc_getpid, g_short_ns);
        row("[kernel]", "getpid()", getpid_ns, "ns", "linux: ~55");
        row("[kernel]", "clock_gettime(MONOTONIC)",
            timed_ns_per_op(sc_clock_gettime, g_short_ns), "ns",
            "linux: ~25 (vDSO, no trap)");
        // REALTIME takes a different path inside the vDSO (it adds the kernel's
        // wall-clock offset), and `gettimeofday`/`time` are separate entry
        // points that a glibc program calls directly. A vDSO that serves only
        // MONOTONIC would look complete in the row above and still leave every
        // timestamp in a log line going through a trap.
        row("[kernel]", "clock_gettime(REALTIME)",
            timed_ns_per_op(sc_clock_gettime_real, g_short_ns), "ns",
            "linux: ~25 (vDSO, no trap)");
        row("[kernel]", "gettimeofday()",
            timed_ns_per_op(sc_gettimeofday, g_short_ns), "ns",
            "linux: ~25 (vDSO, no trap)");
        row("[kernel]", "time()",
            timed_ns_per_op(sc_time, g_short_ns), "ns",
            "linux: ~25 (vDSO, no trap)");
        // Full signal delivery: trap in on `kill`, frame set-up on the user
        // stack, run the handler, `rt_sigreturn` back out. Two kernel entries
        // and a context save/restore -- the path every Ctrl-C, timer signal and
        // crash handler takes.
        {
            struct sigaction sa;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = bench_sig_handler;
            sigaction(SIGUSR1, &sa, NULL);
        }
        row("[kernel]", "raise(SIGUSR1)+handler",
            timed_ns_per_op(sc_signal_self, g_short_ns), "ns", "linux: ~1500");
        row("[kernel]", "sigprocmask()",
            timed_ns_per_op(sc_sigprocmask, g_short_ns), "ns", "linux: ~90");
        row("[kernel]", "sched_yield()",
            timed_ns_per_op(sc_sched_yield, g_short_ns), "ns", "linux: ~300");
        row("[kernel]", "pread(/dev/zero, 1B)",
            g_devzero >= 0 ? timed_ns_per_op(sc_read1, g_short_ns) : NA, "ns",
            "linux: ~250");
        row("[kernel]", "write(/dev/null, 1B)",
            g_devnull >= 0 ? timed_ns_per_op(sc_write1, g_short_ns) : NA, "ns",
            "linux: ~250");
        row("[kernel]", "fstat()",
            g_devnull >= 0 ? timed_ns_per_op(sc_fstat, g_short_ns) : NA, "ns",
            "linux: ~300");
        row("[kernel]", "stat(\"/dev/null\")",
            timed_ns_per_op(sc_stat_path, g_short_ns), "ns",
            "linux: ~900 (path walk)");
        row("[kernel]", "open+close(/dev/null)",
            timed_ns_per_op(sc_open_close, g_short_ns), "ns", "linux: ~1200");
        printf("  (subtract the getpid row from any other to isolate that\n");
        printf("   subsystem's cost from raw trap overhead.)\n");
    } else {
        getpid_ns = timed_ns_per_op(sc_getpid, g_short_ns / 2);
    }

    // ---- VM ----
    if (want(only, "vm")) {
        line();
        printf("VM / PAGE FAULTS\n");
        row("[kernel]", "mmap+munmap (4 KiB)",
            timed_ns_per_op(sc_mmap_munmap, g_short_ns), "ns", "linux: ~2500");
        row("[kernel]", "mprotect (4 KiB, x2)",
            timed_ns_per_op(sc_mprotect, g_short_ns), "ns", "linux: ~1800");
        row("[kernel]", "minor fault (anon touch)",
            vm_minor_fault_ns(g_short_ns), "ns", "linux: ~500");
        row("[kernel]", "COW fault (after fork)", vm_cow_fault_ns(), "ns",
            "linux: ~1500");
        {
            int cow = vm_cow_isolation_check();
            printf("  %-8s %-28s %12s", "[kernel]", "fork memory isolation",
                   cow == 0 ? "PASS" : (cow < 0 ? "n/a" : "FAIL"));
            if (cow > 0)
                printf("  <-- check %d: parent and child are NOT isolated", cow);
            printf("\n");
        }
        printf("  (every program pays these on startup and on every allocation\n");
        printf("   it touches; they never appear in a memcpy benchmark.)\n");
    }

    // ---- Scheduler ----
    if (want(only, "sched")) {
        line();
        printf("SCHEDULER / IPC   <-- what makes a system feel fast or slow\n");
        pipe_proc_ns = sched_pipe_rt_proc(g_short_ns);
        row("[kernel]", "pipe round trip (2 procs)",
            pipe_proc_ns < 0 ? NA : pipe_proc_ns / 1000.0, "us",
            "linux: ~6");
        r = sched_pipe_rt_thread(g_short_ns);
        row("[kernel]", "pipe round trip (2 thrds)", r < 0 ? NA : r / 1000.0, "us",
            "linux: ~4");
        r = sched_socketpair_rt_proc(g_short_ns);
        row("[kernel]", "socketpair round trip",
            r < 0 ? NA : r / 1000.0, "us", "linux: ~8");
        r = sched_futex_rt_ns(g_short_ns);
        row("[kernel]", "futex wake round trip",
            r < 0 ? NA : r / 1000.0, "us", "linux: ~3");
        r = sched_thread_spawn_ns(g_short_ns);
        row("[kernel]", "pthread_create + join",
            r < 0 ? NA : r / 1000.0, "us", "linux: ~15");
        row("[kernel]", "pipe bandwidth (64K writes)",
            sched_pipe_bw_mbs(g_short_ns), "MB/s", "linux: >1000");

        sleep_idle_us = sleep_overshoot_us(1000, 40, &sleep_idle_max_us);
        row("[kernel]", "sleep 1ms late, idle (mean)", sleep_idle_us, "us",
            "linux: ~60");
        row("[kernel]", "sleep 1ms late, idle (worst)", sleep_idle_max_us, "us",
            "linux: ~200");

        // The headline. Saturate every CPU with a userspace spinner that never
        // issues a syscall, then measure the same sleep and the same pipe round
        // trip. On a kernel that preempts on wake-up these barely move; on one
        // that makes a woken task wait out the running thread's timeslice they
        // jump to milliseconds — and that is what using the machine feels like.
        pid_t *hogs = calloc((size_t)ncpu, sizeof *hogs);
        int nhogs = hogs ? load_start(hogs, ncpu) : 0;
        if (nhogs > 0) {
            // Let the hogs actually get scheduled before measuring.
            struct timespec settle = {0, 50 * 1000 * 1000};
            nanosleep(&settle, NULL);
            printf("  -- now with %d CPU-bound processes competing for the CPUs --\n",
                   nhogs);
            sleep_load_us = sleep_overshoot_us(1000, 40, &sleep_load_max_us);
            row("[kernel]", "sleep 1ms late, load (mean)", sleep_load_us, "us",
                "linux: ~100");
            row("[kernel]", "sleep 1ms late, load (worst)", sleep_load_max_us,
                "us", "linux: ~500  <-- stutter");
            double loaded_pipe = sched_pipe_rt_proc(g_short_ns);
            row("[kernel]", "pipe round trip, load",
                loaded_pipe < 0 ? NA : loaded_pipe / 1000.0, "us", "linux: ~20");
            load_stop(hogs, nhogs);
        } else {
            printf("  (could not start CPU hogs — loaded latency not measured)\n");
        }
        free(hogs);
        printf("  wake-up latency under load is THE interactivity metric. A kernel\n");
        printf("  that only reschedules at timeslice expiry shows a large jump\n");
        printf("  between the idle and loaded rows; one that preempts on wake-up\n");
        printf("  (Linux does) shows almost none.\n");
    }

    // ---- PreemptiveScheduler internals ----
    if (want(only, "psched")) {
        line();
        printf("PREEMPTIVE SCHEDULER INTERNALS   <-- one mechanism per row\n");
        g_aff_ncpu = ncpu;
        g_kstat_ok = kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
        if (!g_kstat_ok)
            printf("  (no /proc/perf/kernel: the kernel-side counter rows are n/a.\n");
        if (!g_kstat_ok)
            printf("   Expected on Linux — every userspace row below still runs.)\n");
        else {
            // The boot's own description of which scheduler switches are on,
            // so a captured report cannot be compared against another whose
            // policy differed.
            const char *m = strstr(g_kstat_a, "sched mode:");
            if (m) {
                const char *e = strchr(m, '\n');
                printf("  %.*s\n", (int)(e ? (size_t)(e - m) : strlen(m)), m);
            }
        }

        // --- dispatch floor ---
        const char *yh_why = NULL;
        double yh = yield_handoff_ns(0, g_short_ns, &yh_why);
        row("[kernel]", "yield hand-off (1 CPU)", yh, "ns", "linux: ~600");
        if (yh < 0 && yh_why)
            printf("           ^ %s\n", yh_why);
        if (yh > 0 && pipe_proc_ns > 0)
            row("[kernel]", "pipe RT / yield hand-off", pipe_proc_ns / yh, "x",
                "how much of a round trip is not dispatch");

        // --- timeslice floor (RUN_TO_PARITY) ---
        printf("  -- timeslice floor: a spinner and a kHz waker on ONE CPU --\n");
        fflush(stdout);
        double solo = parity_run(0, 0, g_short_ns, NULL, NULL, NULL);
        row("[kernel]", "spinner alone, 1 CPU", solo < 0 ? NA : solo / 1e3,
            "kunit/s", "");
        double w_hz = NA, w_mean = NA, w_max = NA;
        double shared = parity_run(0, 200, g_short_ns, &w_hz, &w_mean, &w_max);
        row("[kernel]", "spinner + 200us waker", shared < 0 ? NA : shared / 1e3,
            "kunit/s", "");
        // A spinner cannot do MORE work with a waker stealing its CPU than it
        // does alone, so a retention above 100% does not mean "no cost": it
        // means the SOLO leg was itself disturbed and the two legs are not
        // comparable. Printing the ratio anyway reports a 264% that reads like
        // a spectacular result and is nothing of the sort -- a measuring tool
        // must refuse the number rather than dress up a broken baseline.
        int legs_comparable = solo > 0 && shared > 0 && shared / solo <= 1.05;
        row("[kernel]", "throughput retained",
            legs_comparable ? shared / solo * 100.0 : NA, "%",
            "low = preempted on every wake");
        if (solo > 0 && shared > 0 && !legs_comparable)
            printf("           ^ the solo leg did LESS work than the loaded one,"
                   " so it was\n             itself starved: compare the two"
                   " kunit/s rows above, not\n             their ratio. Nothing"
                   " here is a measurement of the floor.\n");
        row("[kernel]", "  waker rate achieved", w_hz, "wake/s", "");
        row("[kernel]", "  waker late (mean)", w_mean, "us", "");
        row("[kernel]", "  waker late (worst)", w_max, "us", "");
        printf("  the floor buys the spinner's throughput with the waker's\n");
        printf("  latency, so these rows are one result, not two. High retention\n");
        printf("  WITH low waker latency is the only unambiguously good outcome.\n");

        // --- slice remainder (EEVDF lag) ---
        printf("  -- slice remainder: a plain spinner against one that parks --\n");
        fflush(stdout);
        // 600 us of work then a 50 us park: just under a default slice, which
        // is the case a resumption that renews the whole slice never preempts.
        double parker_share = NA;
        double share = lag_share(0, 600000ull, 50, g_short_ns, &parker_share);
        row("[kernel]", "plain spinner fair share", share, "x",
            "1.00 = even; 0 = starved by the parker");
        row("[kernel]", "parking thread fair share", parker_share, "x",
            "the two sum to 2.00 by construction");
        if (share >= 1.99 || parker_share >= 1.99)
            printf("           ^ one of the two did NO work: the pair was not\n"
                   "             sharing the CPU at all, which is a different\n"
                   "             finding from an uneven split.\n");
        printf("  a thread that parks just short of its slice must come back\n");
        printf("  with the REMAINDER, not a fresh slice, or it is never\n");
        printf("  preempted and its neighbour on that CPU gets the scraps.\n");

        // --- work stealing ---
        if (ncpu > 1) {
            printf("  -- work stealing: %d workers all created on CPU 0 --\n", ncpu);
            fflush(stdout);
            double full_us = NA;
            int occupied = 0;
            // The long budget, not the short one: this probe is a latency
            // with a tail, and a window shorter than the kernel's balancing
            // period reports "never spread" for a kernel that merely took one
            // more period to do it.
            if (steal_spread_us(ncpu, 0, g_budget_ns, &full_us, &occupied) == 0) {
                char lbl[64];
                row("[kernel]", "time to occupy all CPUs", full_us, "us",
                    "linux: ~0-200");
                snprintf(lbl, sizeof lbl, "CPUs occupied of %d", ncpu);
                row("[kernel]", lbl, (double)occupied, "",
                    occupied < ncpu ? "<-- work never spread this wide" : "");
                printf("  n/a on the first row with full occupancy on the second\n");
                printf("  means the spread took longer than this probe's budget.\n");
            } else {
                printf("  (no per-CPU id from this kernel — spread not measurable)\n");
            }
        }

        // --- idle steal scans: the no-peer-stealable hint ---
        if (g_kstat_ok) {
            printf("  -- idle second: what the steal path does with nothing to do --\n");
            fflush(stdout);
            kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
            struct timespec idle = {1, 0};
            nanosleep(&idle, NULL);
            kstat_snapshot(g_kstat_b, sizeof g_kstat_b);
            double scans = kstat_delta("sched steal:", 0);
            double probed = kstat_delta("sched steal:", 1);
            double skipped = kstat_delta("sched steal:", 5);
            // The two counters are DISJOINT. The hint's early return bumps
            // `skipped` and returns before `scans` is touched, so `skipped` is
            // the number of scans avoided, not a subset of those performed:
            // the denominator is their sum, and dividing by `scans` alone
            // yields shares above 100%.
            double attempts = (scans >= 0 && skipped >= 0) ? scans + skipped : NA;
            row("[kernel]", "steal attempts, idle", attempts, "/s", "");
            row("[kernel]", "  scans actually walked", scans, "/s", "");
            if (attempts > 0 && skipped >= 0)
                row("[kernel]", "  avoided by the hint", skipped / attempts * 100.0,
                    "%", "answered without taking a lock");
            if (scans > 0 && probed >= 0)
                row("[kernel]", "  victims probed per scan", probed / scans, "x",
                    "each probe is a lock on a peer");
            printf("  an idle CPU leaves halt on every interrupt. Walking every\n");
            printf("  peer's runtime before concluding there is nothing to steal\n");
            printf("  costs a lock per peer per wake-up, and those lines are\n");
            printf("  written by the CPUs that own them.\n");
        }

        // --- affinity ---
        printf("  -- affinity: the mask, and the kick it implies --\n");
        fflush(stdout);
        row("[kernel]", "sched_getcpu()",
            timed_ns_per_op(sc_getcpu, g_short_ns), "ns", "linux: ~25 (vDSO)");
        row("[kernel]", "sched_getaffinity()",
            timed_ns_per_op(sc_getaffinity, g_short_ns), "ns", "linux: ~250");
        if (g_kstat_ok)
            kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
        double aff_ns = timed_ns_per_op(sc_setaffinity, g_short_ns);
        row("[kernel]", "sched_setaffinity() flip", aff_ns, "ns", "linux: ~700");
        if (g_kstat_ok && aff_ns > 0) {
            kstat_snapshot(g_kstat_b, sizeof g_kstat_b);
            double ops = (double)g_last_ops;
            row("[kernel]", "  affinity-empty scans",
                kstat_per_op("sched steal:", 3, ops), "/flip",
                "a peer whose queue has no task this CPU may run");
        }
        printf("  narrowing a mask has to kick a CPU in the new mask when the\n");
        printf("  task is runnable outside it, and that kick walks the other\n");
        printf("  CPUs' runtimes — a scheduler operation, not a store.\n");

        // --- cross-CPU wake coalescing ---
        if (ncpu > 1) {
            printf("  -- cross-CPU wake: one at a time against a burst --\n");
            fflush(stdout);
            int burst = ncpu * 2 > WAKE_MAX ? WAKE_MAX : ncpu * 2;
            if (burst < 2)
                burst = 2;
            // Private word: what musl's pthread primitives actually use, so
            // this is the cost a real program pays.
            double one = wake_burst_ns(1, 0, 1, g_short_ns, 1);
            double many = wake_burst_ns(burst, 0, 1, g_short_ns, 1);
            // Shared word: the inter-process path, which has to resolve the
            // address through the VMAR. Reported next to it because the gap is
            // the resolution cost, and because the bench used to measure ONLY
            // this one while labelling it "a futex wake".
            double one_sh = wake_burst_ns(1, 0, 1, g_short_ns, 0);
            char lbl[64];
            row("[kernel]", "wake issue, 1 target",
                one < 0 ? NA : one / 1000.0, "us", "private word (what musl uses)");
            row("[kernel]", "wake issue, 1 target, shared word",
                one_sh < 0 ? NA : one_sh / 1000.0, "us",
                "inter-process path: resolves through the VMAR");
            if (one > 0 && one_sh > 0)
                row("[kernel]", "  shared / private", one_sh / one, "x",
                    ">1 = the address resolution is the cost");
            snprintf(lbl, sizeof lbl, "wake issue, %d in a burst", burst);
            row("[kernel]", lbl, many < 0 ? NA : many / 1000.0, "us",
                "per target");
            if (one > 0 && many > 0)
                row("[kernel]", "burst / single, per wake", many / one, "x",
                    "<1 = later wakes are cheaper");
            printf("  Eclipse folds the reschedule request per CPU, so wakes 2..N\n");
            printf("  into one CPU can be much cheaper than the first. Linux does\n");
            printf("  not fold distinct futex wakes and reports ABOVE 1 here, so\n");
            printf("  this row is the two kernels against each other, not a score:\n");
            printf("  read it next to the single-target row, which carries the\n");
            printf("  absolute cost.\n");
        }

        // --- timer rearms and executor polls, per operation ---
        printf("  -- per-operation kernel work --\n");
        fflush(stdout);
        if (g_kstat_ok)
            kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
        g_sleep_us = 200;
        double sl_ns = timed_ns_per_op(sc_short_sleep, g_short_ns);
        row("[kernel]", "nanosleep(200us) cost", sl_ns < 0 ? NA : sl_ns / 1000.0,
            "us", "linux: ~260");
        if (g_kstat_ok && sl_ns > 0) {
            kstat_snapshot(g_kstat_b, sizeof g_kstat_b);
            double sleeps = (double)g_last_ops;
            row("[kernel]", "  timer rearms per sleep",
                kstat_per_op("timer rearms:", 0, sleeps), "/sleep",
                "~1 is right; more means extra reprogramming");
            row("[kernel]", "  task polls per sleep",
                kstat_per_op("sched:", 0, sleeps), "/sleep", "");
        }

        if (g_kstat_ok) {
            // A blocking round trip is the operation that can leave a weak
            // executor behind: the task yields in the middle of a poll, and
            // what it was using has to be kept somewhere until it resumes.
            kstat_snapshot(g_kstat_a, sizeof g_kstat_a);
            double rt = sched_pipe_rt_thread(g_short_ns);
            kstat_snapshot(g_kstat_b, sizeof g_kstat_b);
            if (rt > 0 && g_last_ops > 0) {
                double ops = (double)g_last_ops; // round trips actually driven
                row("[kernel]", "task polls per pipe RT",
                    kstat_per_op("sched:", 0, ops), "/RT", "");
                row("[kernel]", "weak-exec yields per RT",
                    kstat_per_op("sched:", 2, ops), "/RT", "");
                row("[kernel]", "weak execs created per RT",
                    kstat_per_op("sched weak:", 0, ops), "/RT",
                    "each one holds a 32 KiB stack");
            }
            // Absolute high-water marks: these say whether the churn above is
            // being absorbed by the pool or is growing without bound.
            row("[kernel]", "weak execs live, peak",
                kstat_field(g_kstat_b, "sched weak:", 1), "", "");
            row("[kernel]", "weak soft-cap hits",
                kstat_field(g_kstat_b, "sched weak:", 2), "",
                "non-zero = the cap is being reached");
            row("[kernel]", "stack-pool overflows",
                kstat_field(g_kstat_b, "sched weak:", 4), "",
                "a stack the pool could not supply");
            row("[kernel]", "stack high-water",
                kstat_field(g_kstat_b, "sched stack:", 2), "%",
                "of the per-executor stack");
            row("[kernel]", "wakeup preempt honoured",
                kstat_field(g_kstat_b, "wakeup preempt:", 3), "%",
                "kernel-side twin of wake late loaded/idle");
        }
        printf("  these counters are the kernel's own view of the same events.\n");
        printf("  They need /proc/perf/kernel, so on Linux they are n/a and the\n");
        printf("  userspace rows above are the whole comparison.\n");
    }


    // ---- Sockets / IPC ----
    if (want(only, "net")) {
        line();
        printf("SOCKETS / IPC   <-- everything above a bare socketpair\n");
        snprintf(g_net_dir, sizeof g_net_dir, "%s", dir);
        g_pstat_ok = pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
        if (!g_pstat_ok)
            printf("  (no /proc/self/perf: the calls/op and in-kernel rows are\n"
                   "   Eclipse-only and read n/a here)\n");
        else
            pstat_calibrate();

        double sock_ns = net_socket_floor_ns(AF_INET, SOCK_STREAM, g_short_ns);
        row("[kernel]", "socket()+close() TCP", sock_ns, "ns", "linux: ~1500");
        row("[kernel]", "socket()+close() UNIX",
            net_socket_floor_ns(AF_UNIX, SOCK_STREAM, g_short_ns), "ns",
            "linux: ~1200");

        // Loopback TCP. The same one-byte exchange as the pipe row, with the
        // whole IP stack in the path: a gap against the socketpair row is the
        // protocol processing, not the wake-up.
        {
            int c = -1, s = -1;
            double tcp_ops = 0, tcp_ns = NA;
            if (net_tcp_pair(&c, &s) == 0) {
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
                tcp_ns = net_pair_rt(c, s, g_short_ns, &tcp_ops);
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_b, sizeof g_pstat_b);
                row("[kernel]", "TCP loopback round trip",
                    tcp_ns < 0 ? NA : tcp_ns / 1000.0, "us", "linux: ~12");
                pstat_pair("sendto", "sendto", tcp_ops, tcp_ns);
            } else {
                row("[kernel]", "TCP loopback round trip", NA, "us",
                    "(no loopback TCP)");
            }
        }
        {
            int c = -1, s = -1;
            if (net_tcp_pair(&c, &s) == 0)
                row("[kernel]", "TCP loopback bandwidth",
                    net_pair_bw_mbs(c, s, 64 * 1024, g_short_ns), "MB/s",
                    "linux: >2000");
            else
                row("[kernel]", "TCP loopback bandwidth", NA, "MB/s", "");
        }
        row("[kernel]", "TCP connect+accept+close",
            (r = net_accept_cycle_ns(g_short_ns)) < 0 ? NA : r / 1000.0, "us",
            "linux: ~30");

        // A UNIX socket reached by NAME, not a socketpair: bind, listen,
        // connect through the filesystem. The difference against the
        // socketpair row in the SCHEDULER section is what the name and the
        // listener cost, which every desktop bus and compositor pays.
        {
            int c = -1, s = -1;
            char path[512] = {0};
            if (net_unix_pair(&c, &s, path, sizeof path) == 0) {
                double ops = 0;
                double ns = net_pair_rt(c, s, g_short_ns, &ops);
                row("[kernel]", "UNIX stream RT (bound)",
                    ns < 0 ? NA : ns / 1000.0, "us", "linux: ~9");
                if (path[0])
                    unlink(path);
            } else {
                row("[kernel]", "UNIX stream RT (bound)", NA, "us", "");
            }
        }
        {
            int a = -1, b = -1;
            char pa[512] = {0}, pb[512] = {0};
            if (net_dgram_pair(AF_UNIX, &a, &b, pa, pb, sizeof pa) == 0) {
                double ops = 0;
                double ns = net_pair_rt(a, b, g_short_ns, &ops);
                row("[kernel]", "UNIX datagram round trip",
                    ns < 0 ? NA : ns / 1000.0, "us", "linux: ~8");
                if (pa[0]) unlink(pa);
                if (pb[0]) unlink(pb);
            } else {
                row("[kernel]", "UNIX datagram round trip", NA, "us", "");
            }
        }
        {
            int a = -1, b = -1;
            if (net_dgram_pair(AF_INET, &a, &b, NULL, NULL, 0) == 0) {
                double ops = 0;
                double ns = net_pair_rt(a, b, g_short_ns, &ops);
                row("[kernel]", "UDP loopback round trip",
                    ns < 0 ? NA : ns / 1000.0, "us", "linux: ~10");
            } else {
                row("[kernel]", "UDP loopback round trip", NA, "us",
                    "(no loopback UDP)");
            }
        }

        // sendmsg/recvmsg against plain send/recv, same byte, same socket
        // family, so the ratio is the iovec and control plumbing and nothing
        // else. Then the fd pass, which is the same path carrying an actual
        // descriptor.
        {
            int sv[2];
            double plain = NA, msgv = NA;
            if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0) {
                struct echo_ctl *c = calloc(1, sizeof *c);
                pthread_t th;
                if (c) {
                    c->fd = sv[1];
                    c->refs = 1;
                    CTL_LEND(c);
                    if (pthread_create(&th, NULL, sock_echo_thread, c) == 0) {
                        sock_set_timeout(sv[0], 5);
                        plain = sock_rt_drive(sv[0], g_short_ns);
                        g_msg_fd = sv[0];
                        msgv = timed_ns_per_op(net_sendmsg_rt_op, g_short_ns);
                        shutdown(sv[0], SHUT_RDWR);
                        close(sv[0]);
                        if (join_or_abandon(th, &c->done, 5))
                            close(sv[1]);
                    } else {
                        CTL_DROP(c);
                        close(sv[0]);
                        close(sv[1]);
                    }
                    CTL_DROP(c);
                }
            }
            row("[kernel]", "socketpair RT (send/recv)",
                plain < 0 ? NA : plain / 1000.0, "us", "");
            row("[kernel]", "socketpair RT (sendmsg)",
                msgv < 0 ? NA : msgv / 1000.0, "us", "");
            if (plain > 0 && msgv > 0)
                row("[kernel]", "  sendmsg / send", msgv / plain, "x",
                    "linux: ~1.1");
        }
        {
            int sv[2];
            double ns = NA;
            if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0) {
                g_scm_tx = sv[0];
                g_scm_rx = sv[1];
                g_scm_payload = sv[0]; // any valid descriptor will do
                sock_set_timeout(sv[1], 5);
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
                ns = timed_ns_per_op(net_scm_rights_op, g_short_ns);
                double ops = ns < 0 ? 0 : (double)g_last_ops;
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_b, sizeof g_pstat_b);
                row("[kernel]", "SCM_RIGHTS fd pass", ns < 0 ? NA : ns / 1000.0,
                    "us", "linux: ~7");
                pstat_pair("recvmsg", "recvmsg", ops, ns);
                close(sv[0]);
                close(sv[1]);
            } else {
                row("[kernel]", "SCM_RIGHTS fd pass", NA, "us", "");
            }
            if (ns < 0)
                printf("  (the fd did not arrive — SCM_RIGHTS unsupported here,\n"
                       "   which is a correctness result, not a slow one)\n");
        }
        {
            int sv[2];
            if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0) {
                g_eagain_fd = sv[0];
                row("[kernel]", "recv on empty (EAGAIN)",
                    timed_ns_per_op(net_eagain_op, g_short_ns), "ns",
                    "linux: ~400");
                close(sv[0]);
                close(sv[1]);
            }
        }

        // Readiness. One ready fd, placed LAST, measured over 1 and 64 fds so
        // the slope says whether the implementation walks the set.
        {
            int rfd[RDY_MAX], wfd[RDY_MAX];
            double p1 = NA, p64 = NA, s64 = NA, e64 = NA, ectl = NA;
            int made = net_rdy_setup(1, rfd, wfd);
            if (made == 1) {
                p1 = timed_ns_per_op(net_poll_op, g_short_ns);
                net_rdy_teardown(made, rfd, wfd);
            }
            made = net_rdy_setup(RDY_MAX, rfd, wfd);
            if (made > 1) {
                p64 = timed_ns_per_op(net_poll_op, g_short_ns);
                s64 = timed_ns_per_op(net_select_op, g_short_ns);
                g_rdy_epfd = epoll_create1(0);
                if (g_rdy_epfd >= 0) {
                    int added = 0;
                    for (int i = 0; i < made; i++) {
                        struct epoll_event ev;
                        memset(&ev, 0, sizeof ev);
                        ev.events = EPOLLIN;
                        ev.data.fd = rfd[i];
                        if (epoll_ctl(g_rdy_epfd, EPOLL_CTL_ADD, rfd[i], &ev) == 0)
                            added++;
                    }
                    if (added == made)
                        e64 = timed_ns_per_op(net_epoll_wait_op, g_short_ns);
                    // Measure ADD+DEL with the set already populated, which is
                    // what an event loop does on every new connection.
                    epoll_ctl(g_rdy_epfd, EPOLL_CTL_DEL, g_rdy_pfd[0].fd, NULL);
                    ectl = timed_ns_per_op(net_epoll_ctl_op, g_short_ns);
                    close(g_rdy_epfd);
                    g_rdy_epfd = -1;
                }
            }
            row("[kernel]", "poll(1 fd, ready)", p1, "ns", "linux: ~700");
            char lbl[48];
            snprintf(lbl, sizeof lbl, "poll(%d fds, last ready)", made > 1 ? made : RDY_MAX);
            row("[kernel]", lbl, p64, "ns", "linux: ~2500");
            if (p1 > 0 && p64 > 0 && made > 1)
                row("[kernel]", "  per extra fd", (p64 - p1) / (made - 1), "ns",
                    "linux: ~30");
            snprintf(lbl, sizeof lbl, "select(%d fds)", made > 1 ? made : RDY_MAX);
            row("[kernel]", lbl, s64, "ns", "linux: ~2500");
            snprintf(lbl, sizeof lbl, "epoll_wait(%d fds)", made > 1 ? made : RDY_MAX);
            row("[kernel]", lbl, e64, "ns", "linux: ~800");
            if (p64 > 0 && e64 > 0)
                row("[kernel]", "  epoll / poll", e64 / p64, "x",
                    "<1 means epoll pays off");
            row("[kernel]", "epoll_ctl ADD+DEL", ectl, "ns", "linux: ~1200");
            if (made > 1)
                net_rdy_teardown(made, rfd, wfd);
        }
        printf("  the slope between the 1-fd and %d-fd poll rows is the one that\n",
               RDY_MAX);
        printf("  scales: an event loop with a hundred connections pays it on\n");
        printf("  every single wake-up. `epoll / poll` above 1 means epoll is\n");
        printf("  walking the same set the poll call does, and buys nothing.\n");
    }

    // ---- SMP ----
    if (want(only, "smp")) {
        line();
        printf("SMP SCALING (%d CPUs online)\n", ncpu);
        smp1 = smp_aggregate(1, g_short_ns);
        row("[kernel]", "1 thread aggregate", smp1, "Mops/s", "");
        if (ncpu > 1) {
            smpn = smp_aggregate(ncpu, g_short_ns);
            char lbl[64];
            snprintf(lbl, sizeof lbl, "%d threads aggregate", ncpu);
            row("[kernel]", lbl, smpn, "Mops/s", "");
            if (smp1 > 0 && smpn > 0) {
                double eff = smpn / (smp1 * ncpu) * 100.0;
                row("[kernel]", "scaling efficiency", eff, "%",
                    "linux: >90 on this workload");
            }
        }
        printf("  (the work is pure userspace ALU, so anything short of linear\n");
        printf("   scaling is the kernel: placement, contention or offline CPUs.)\n");
        printf("  -- kernel SMP paths: contention, shootdowns, cross-CPU wakes --\n");
        {
            char lbl[64];
            // Syscall entry from every CPU at once. Per-cpu state done right
            // scales ~linearly; a shared hot lock on the entry path shows up
            // as efficiency collapsing.
            double s1 = smpk_rate(1, smpk_getpid_op, g_short_ns);
            double sn = ncpu > 1 ? smpk_rate(ncpu, smpk_getpid_op, g_short_ns) : NA;
            row("[kernel]", "getpid/s x1", s1 < 0 ? NA : s1 / 1e6, "Mops/s", "");
            snprintf(lbl, sizeof lbl, "getpid/s x%d", ncpu);
            row("[kernel]", lbl, sn < 0 ? NA : sn / 1e6, "Mops/s", "");
            if (s1 > 0 && sn > 0)
                row("[kernel]", "syscall scaling", sn / (s1 * ncpu) * 100.0, "%",
                    "linux: >85");

            // The TLB shootdown, isolated: same flip, idle peers vs spinning
            // peers. The difference is the cross-CPU invalidation.
            double m0 = smpk_mprotect_ns(0, g_short_ns);
            double mn = ncpu > 1 ? smpk_mprotect_ns(ncpu - 1, g_short_ns) : NA;
            row("[kernel]", "mprotect flip, peers idle", m0, "ns", "");
            snprintf(lbl, sizeof lbl, "mprotect flip, %d spinning", ncpu - 1);
            row("[kernel]", lbl, mn, "ns", "");
            if (m0 > 0 && mn > 0)
                row("[kernel]", "shootdown cost", mn / m0, "x",
                    "linux: ~2-4 (IPI + ack wait)");

            // Parallel mappers and faulters: the VM locks.
            double mm1 = smpk_rate(1, smpk_mmap_op, g_short_ns);
            double mmn = ncpu > 1 ? smpk_rate(ncpu, smpk_mmap_op, g_short_ns) : NA;
            snprintf(lbl, sizeof lbl, "mmap+touch+munmap x%d vs x1", ncpu);
            if (mm1 > 0 && mmn > 0)
                row("[kernel]", lbl, mmn / (mm1 * ncpu) * 100.0, "%",
                    "linux: ~40-70 (mmap_lock)");
            double f1 = smpk_rate(1, smpk_fault_op, g_short_ns);
            double fn = ncpu > 1 ? smpk_rate(ncpu, smpk_fault_op, g_short_ns) : NA;
            row("[kernel]", "minor faults/s x1",
                f1 < 0 ? NA : f1 * 64.0 / 1e3, "kflt/s", "");
            snprintf(lbl, sizeof lbl, "minor faults/s x%d", ncpu);
            row("[kernel]", lbl, fn < 0 ? NA : fn * 64.0 / 1e3, "kflt/s", "");
            if (f1 > 0 && fn > 0)
                row("[kernel]", "fault scaling", fn / (f1 * ncpu) * 100.0, "%",
                    "linux: >60");

            // One mutex, everyone. The x1 row is the uncontended fast path
            // (never enters the kernel); the xN row is the futex sleep/wake
            // machinery under fire. The collapse factor is what contention
            // costs on this kernel.
            double x1 = smpk_rate(1, smpk_mutex_op, g_short_ns);
            double xn = ncpu > 1 ? smpk_rate(ncpu, smpk_mutex_op, g_short_ns) : NA;
            if (x1 > 0 && xn > 0)
                row("[kernel]", "contended mutex collapse", x1 / xn, "x",
                    "linux: ~10-40 under full contention");

            // Where a wake lands: same CPU (context switch, hot cache) against
            // a neighbouring CPU (remote wake, IPI).
            if (ncpu > 1) {
                printf("  -- pipe RT pinned (affinity) --\n");
                fflush(stdout);
                double same = smpk_pipe_rt_pinned(0, 0, ncpu, g_short_ns);
                double cross = smpk_pipe_rt_pinned(0, 1, ncpu, g_short_ns);
                row("[kernel]", "pipe RT pinned same-CPU",
                    same < 0 ? NA : same / 1000.0, "us", "");
                row("[kernel]", "pipe RT pinned cross-CPU",
                    cross < 0 ? NA : cross / 1000.0, "us", "");
                if (same > 0 && cross > 0)
                    row("[kernel]", "cross-CPU wake cost", cross / same, "x",
                        "linux: ~0.5-2");
            }

            // Everybody forks at once: the copy-on-write machinery colliding.
            printf("  -- parallel forks / fairness (can be slow) --\n");
            fflush(stdout);
            double fk1 = smpk_forks_per_s(1, g_short_ns);
            double fkn = ncpu > 1 ? smpk_forks_per_s(ncpu, g_short_ns) : NA;
            row("[kernel]", "forks/s x1", fk1, "forks/s", "");
            snprintf(lbl, sizeof lbl, "forks/s x%d procs", ncpu);
            row("[kernel]", lbl, fkn, "forks/s", "");
            if (fk1 > 0 && fkn > 0)
                row("[kernel]", "fork scaling", fkn / (fk1 * ncpu) * 100.0, "%",
                    "linux: ~50-80");

            // 2xN hogs on N CPUs for a while: does everyone get a fair share?
            double fair = smpk_fairness_maxmin(2 * ncpu, g_short_ns);
            row("[kernel]", "fairness max/min (2x hogs)", fair, "x",
                "1.0 = perfectly fair; linux: <1.5");
            printf("  x1 rows are each xN row's own baseline, so every scaling\n");
            printf("  figure is hardware-independent. The shootdown row is the\n");
            printf("  one that punishes a slow IPI/ack path; the fairness row\n");
            printf("  catches a scheduler that posts great aggregates by\n");
            printf("  starving somebody.\n");
            fflush(stdout);
        }
    }

    // ---- Disk ----
    if (want(only, "disk")) {
        line();
        printf("DISK (in %s, %zu MiB requested)\n", dir, disk_mb);
        size_t dbytes = disk_mb * 1024 * 1024;
        int meta_max = 4000;
        // Cap the disk working set to a fraction of the FREE space. A small or
        // nearly-full filesystem (notably the in-RAM SFS root that `make qemu`
        // boots) must not be filled: some filesystems panic on ENOSPC instead of
        // failing the write, which would crash the whole machine mid-benchmark.
        {
            struct statvfs vfs;
            if (statvfs(dir, &vfs) == 0 && vfs.f_bavail > 0) {
                unsigned long bs = vfs.f_frsize ? vfs.f_frsize : vfs.f_bsize;
                unsigned long long freeb = (unsigned long long)vfs.f_bavail * bs;
                unsigned long long usable = freeb / 3;
                if ((unsigned long long)dbytes > usable) dbytes = (size_t)usable;
                unsigned long long mm = usable / (8 * 1024); // ~8 KiB/small file
                if (mm < (unsigned long long)meta_max) meta_max = (int)mm;
                printf("  free=%llu MiB -> file %zu MiB, up to %d meta files\n",
                       freeb / (1024 * 1024), dbytes / (1024 * 1024), meta_max);
            } else {
                if (dbytes > 4u * 1024 * 1024) dbytes = 4u * 1024 * 1024;
                if (meta_max > 500) meta_max = 500;
                printf("  (statvfs unavailable — capping to %zu MiB / %d files)\n",
                       dbytes / (1024 * 1024), meta_max);
            }
        }

        const size_t chunk = 256 * 1024;
        unsigned char *io = malloc(chunk);
        if (!io) {
            printf("  (io buffer alloc failed)\n");
        } else {
            memset(io, 0x5a, chunk);
            char fpath[512];
            snprintf(fpath, sizeof fpath, "%s/eclipse-bench.dat", dir);

            if (dbytes >= 1u * 1024 * 1024) {
                double w = disk_seq_write(fpath, dbytes, chunk, io);
                if (w < 0) printf("  %-8s %-28s %12s\n", "[kernel]", "seq write (+fsync)", "FAILED");
                else { hr_bytes(w, hb, sizeof hb);
                       printf("  %-8s %-28s %12s\n", "[kernel]", "seq write (+fsync)", hb); }

                double rd = disk_seq_read(fpath, chunk, io);
                if (rd < 0) printf("  %-8s %-28s %12s\n", "[kernel]", "seq read", "FAILED");
                else { hr_bytes(rd, hb, sizeof hb);
                       printf("  %-8s %-28s %12s\n", "[kernel]", "seq read", hb); }

                double avg_us = 0;
                double iops = disk_rand_read(fpath, dbytes, g_budget_ns, &avg_us);
                row("[kernel]", "rand 4K read", iops, "IOPS", "");
                if (iops > 0)
                    row("[kernel]", "rand 4K read latency", avg_us, "us", "");

                row("[kernel]", "fsync latency (best)", disk_fsync_ms(fpath, io),
                    "ms", "");
                unlink(fpath);
            } else {
                printf("  (too little free space for the streaming tests — point\n");
                printf("   DIR at a real disk/partition with more room)\n");
            }

            if (meta_max >= 20) {
                double cps, sps, ups;
                disk_metadata(dir, g_budget_ns, meta_max, &cps, &sps, &ups);
                row("[kernel]", "meta create small files", cps, "files/s", "");
                row("[kernel]", "meta stat", sps, "stats/s", "");
                row("[kernel]", "meta unlink", ups, "unlinks/s", "");
            } else {
                printf("  meta ops: skipped (low free space)\n");
            }
            free(io);
        }
    }


    // ---- Filesystem / VFS ----
    if (want(only, "fs")) {
        line();
        printf("FILESYSTEM / VFS (warm, in %s)   <-- paths, not megabytes\n", dir);
        g_pstat_ok = pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
        if (!g_pstat_ok)
            printf("  (no /proc/self/perf: the calls/op and in-kernel rows are\n"
                   "   Eclipse-only and read n/a here)\n");
        else
            pstat_calibrate();
        if (fs_setup(dir, 1024 * 1024) != 0) {
            printf("  (could not build the test tree under %s — skipped. Point\n"
                   "   DIR at a writable directory with a few MiB free.)\n", dir);
            fs_teardown();
        } else {
            double shallow = NA, deep = NA;
            if (g_pstat_ok)
                pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
            shallow = fs_open_ns(g_fs_shallow, O_RDONLY, g_short_ns);
            double open_ops = shallow < 0 ? 0 : (double)g_last_ops;
            if (g_pstat_ok)
                pstat_snapshot(g_pstat_b, sizeof g_pstat_b);
            row("[kernel]", "open+close, 1 component", shallow, "ns",
                "linux: ~1400");
            pstat_pair("openat", "openat", open_ops, shallow);
            deep = fs_open_ns(g_fs_deep, O_RDONLY, g_short_ns);
            row("[kernel]", "open+close, 9 components", deep, "ns",
                "linux: ~2200");
            if (shallow > 0 && deep > 0)
                row("[kernel]", "  path cost per component",
                    (deep - shallow) / 8.0, "ns", "linux: ~60");
            double at = timed_ns_per_op(fs_openat_op, g_short_ns);
            row("[kernel]", "openat(dirfd, name)", at, "ns", "linux: ~1300");
            if (shallow > 0 && at > 0)
                row("[kernel]", "  openat / open", at / shallow, "x", "");
            // O_PATH resolves the path and hands back a handle that cannot be
            // read. `ps` uses it, and Eclipse has had it wrong twice, so it
            // gets a row of its own rather than being assumed equal to a
            // normal open.
            row("[kernel]", "open+close (O_PATH)",
                fs_open_ns(g_fs_shallow, O_PATH, g_short_ns), "ns",
                "linux: ~1100");

            row("[kernel]", "stat(path)",
                timed_ns_per_op(fs_stat_op, g_short_ns), "ns", "linux: ~800");
            row("[kernel]", "lstat(path)",
                timed_ns_per_op(fs_lstat_op, g_short_ns), "ns", "linux: ~850");
            row("[kernel]", "fstatat(dirfd, name)",
                timed_ns_per_op(fs_fstatat_op, g_short_ns), "ns", "linux: ~700");
            row("[kernel]", "fstat(fd)",
                timed_ns_per_op(fs_fstat_op, g_short_ns), "ns", "linux: ~400");
            row("[kernel]", "access(F_OK)",
                timed_ns_per_op(fs_access_op, g_short_ns), "ns", "linux: ~750");
            if (g_fs_link[0])
                row("[kernel]", "readlinkat(symlink)",
                    timed_ns_per_op(fs_readlink_op, g_short_ns), "ns",
                    "linux: ~700");
            else
                row("[kernel]", "readlinkat(symlink)", NA, "ns",
                    "(no symlink support)");
            row("[kernel]", "lseek(SEEK_SET)",
                timed_ns_per_op(fs_lseek_op, g_short_ns), "ns", "linux: ~250");
            row("[kernel]", "dup+close",
                timed_ns_per_op(fs_dup_op, g_short_ns), "ns", "linux: ~700");
            row("[kernel]", "fcntl(F_GETFL)",
                timed_ns_per_op(fs_fcntl_op, g_short_ns), "ns", "linux: ~250");
            row("[kernel]", "pipe()+close x2",
                timed_ns_per_op(fs_pipe_op, g_short_ns), "ns", "linux: ~3000");

            // Warm data path.
            g_fs_rbuf = malloc(64 * 1024);
            if (g_fs_rbuf) {
                g_fs_rlen = 4096;
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_a, sizeof g_pstat_a);
                double r4 = timed_ns_per_op(fs_read_op, g_short_ns);
                double r4ops = r4 < 0 ? 0 : (double)g_last_ops;
                if (g_pstat_ok)
                    pstat_snapshot(g_pstat_b, sizeof g_pstat_b);
                row("[kernel]", "lseek+read 4 KiB (cache)", r4, "ns",
                    "linux: ~800");
                pstat_pair("read", "read", r4ops, r4);
                double p4 = timed_ns_per_op(fs_pread_op, g_short_ns);
                row("[kernel]", "pread 4 KiB (cache)", p4, "ns",
                    "linux: ~500");
                // Two syscalls against one: `pread` carries the offset, so
                // the gap is what rewinding the file position costs. The
                // `lseek` row above gives the other half of the arithmetic.
                if (r4 > 0 && p4 > 0)
                    row("[kernel]", "  (lseek+read) / pread", r4 / p4, "x",
                        "the file position");
                g_fs_rlen = 64 * 1024;
                double r64 = timed_ns_per_op(fs_read_op, g_short_ns);
                row("[kernel]", "lseek+read 64 KiB (cache)",
                    r64 < 0 ? NA : 65536.0 / r64 * 1e9 / (1024.0 * 1024.0),
                    "MiB/s", "linux: >5000");
                free(g_fs_rbuf);
                g_fs_rbuf = NULL;
            }
            row("[kernel]", "mmap+touch 1 MiB file", fs_mmap_read_mibs(g_short_ns),
                "MiB/s", "linux: >1500");

            if (g_fs_listed > 0) {
                double entries = 0, calls = 0;
                double per = fs_getdents_per_entry_ns(g_fs_list, g_short_ns,
                                                      &entries, &calls);
                char lbl[48];
                snprintf(lbl, sizeof lbl, "getdents, %d entries", g_fs_listed);
                row("[kernel]", lbl, per, "ns/entry", "linux: ~100");
                // How many entries one getdents call returned. A kernel that
                // hands back one entry per syscall costs a directory walk a
                // syscall per file, and the per-entry row above cannot show
                // that on its own — it would just read "slow".
                if (per > 0 && calls > 0)
                    row("[kernel]", "  entries per getdents call",
                        entries / calls, "x", "linux: all of them in one call");
            }

            // procfs. Every one of these is a report the kernel formats on
            // demand, and `ps aux` reads several per process on the machine.
            printf("  -- procfs (generated on read; what `ps` and `top` pay) --\n");
            row("[kernel]", "/proc/self/stat",
                fs_proc_read_ns("/proc/self/stat", g_short_ns), "ns",
                "linux: ~5000");
            row("[kernel]", "/proc/self/status",
                fs_proc_read_ns("/proc/self/status", g_short_ns), "ns",
                "linux: ~9000");
            row("[kernel]", "/proc/self/maps",
                fs_proc_read_ns("/proc/self/maps", g_short_ns), "ns",
                "linux: ~15000");
            row("[kernel]", "/proc/uptime",
                fs_proc_read_ns("/proc/uptime", g_short_ns), "ns",
                "linux: ~4000");
            row("[kernel]", "/proc/meminfo",
                fs_proc_read_ns("/proc/meminfo", g_short_ns), "ns",
                "linux: ~8000");
            {
                double entries = 0, calls = 0;
                double per = fs_getdents_per_entry_ns("/proc/self/task",
                                                      g_short_ns, &entries,
                                                      &calls);
                row("[kernel]", "/proc/self/task listing", per, "ns/entry",
                    "linux: ~2000");
                if (per < 0)
                    printf("  (/proc/<pid>/task would not list — that directory\n"
                           "   missing is what used to kill chromium's zygote)\n");
            }
            fs_teardown();
            printf("  these rows are the cache and the VFS, not the device: the\n");
            printf("  data was written moments earlier and every read starts at\n");
            printf("  offset 0. A gap against Linux here is path resolution,\n");
            printf("  descriptor handling or procfs formatting — the DISK section\n");
            printf("  is where the block device answers for itself.\n");
        }
    }

    // ---- Process ----
    if (want(only, "proc")) {
        line();
        printf("PROCESS CREATION\n");
        double fr = proc_fork_ns(g_short_ns);
        row("[kernel]", "fork + exit", fr < 0 ? NA : fr / 1000.0, "us",
            "linux: ~70");
        // Copy-on-write check. The slope between the two sizes is the cost the
        // kernel charges per MiB of the parent's resident set, every fork.
        double f1 = proc_fork_resident_ns(1, g_short_ns);
        double f16 = proc_fork_resident_ns(16, g_short_ns);
        row("[kernel]", "fork + exit, 1 MiB resident",
            f1 < 0 ? NA : f1 / 1000.0, "us", "");
        row("[kernel]", "fork + exit, 16 MiB resident",
            f16 < 0 ? NA : f16 / 1000.0, "us", "");
        if (f1 > 0 && f16 > 0) {
            double per_mib = (f16 - f1) / 15.0 / 1000.0;
            // A negative slope is not a failed measurement — it is the answer.
            // Copy-on-write makes fork cost independent of the resident set, so
            // the two sizes land within noise of each other and the difference
            // can come out either side of zero on a loaded host. Clamping to
            // zero reports "flat" instead of the "n/a" that a negative value
            // used to produce, which read like the probe had broken.
            if (per_mib < 0)
                per_mib = 0;
            row("[kernel]", "fork cost per MiB resident", per_mib, "us/MiB",
                per_mib == 0 ? "flat within noise" : "");
            // Absolute microseconds per MiB say nothing on their own: a slow
            // machine is slow at everything. What settles it is how that cost
            // compares to what copying a MiB *costs on this very machine*. An
            // eager fork must pay at least one memcpy per resident MiB, so the
            // ratio lands near (or above) 1. A copy-on-write fork only touches
            // page tables — measured at ~0.3 on Linux, where the residual is
            // the page-table copy plus the child's teardown of the mappings on
            // exit, both of which every kernel pays.
            uint64_t mb = 1024 * 1024;
            unsigned char *a = malloc(mb), *b = malloc(mb);
            if (a && b) {
                memset(a, 0x5a, mb);
                memset(b, 0, mb);
                uint64_t c0 = now_ns();
                int reps = 16;
                for (int i = 0; i < reps; i++)
                    memcpy(b, a, mb);
                double memcpy_us_per_mib =
                    (double)(now_ns() - c0) / (double)reps / 1000.0;
                g_sink += b[0];
                fork_copy_ratio = per_mib / memcpy_us_per_mib;
                row("[kernel]", "  vs memcpy 1 MiB here", memcpy_us_per_mib,
                    "us/MiB", "");
                row("", "fork copy ratio", fork_copy_ratio, "x",
                    "COW ~0.3, eager copy >=1");
            }
            free(a); free(b);
            // The other axis: cost per mapping, with the resident set held
            // fixed. Copy-on-write trades a per-page cost for a per-mapping one,
            // and if the kernel shoots down every other CPU's TLB once per
            // mapping that trade can lose badly for an ordinary process, which
            // has many small mappings and few large ones.
            double m8 = proc_fork_mappings_ns(8, g_short_ns);
            double m256 = proc_fork_mappings_ns(256, g_short_ns);
            row("[kernel]", "fork + exit, 8 mappings",
                m8 < 0 ? NA : m8 / 1000.0, "us", "");
            row("[kernel]", "fork + exit, 256 mappings",
                m256 < 0 ? NA : m256 / 1000.0, "us", "");
            if (m8 > 0 && m256 > 0) {
                double per_map = (m256 - m8) / 248.0 / 1000.0;
                if (per_map < 0)
                    per_map = 0;
                row("[kernel]", "fork cost per mapping", per_map, "us/mapping",
                    per_map == 0 ? "flat within noise" : "");
            }
            // The same probe with every other CPU busy, which is a different
            // measurement and not merely a noisier one.
            //
            // A fork write-protects each mapping and must then invalidate the
            // other CPUs' TLBs. A kernel is free to skip the wait for a CPU that
            // is *halted* — it holds no live entry and will flush before it runs
            // anything — so on an otherwise idle machine the shootdown costs
            // almost nothing and this whole class of cost is invisible. Put the
            // other CPUs to work and each shootdown becomes a real IPI round
            // trip with an acknowledgement to wait for.
            //
            // That is also the honest case: a shell forks while the machine is
            // doing something, not while it sits idle.
            int nload = ncpu > 1 ? ncpu - 1 : 0;
            pid_t *fl = nload > 0 ? calloc((size_t)nload, sizeof *fl) : NULL;
            int nfl = fl ? load_start(fl, nload) : 0;
            if (nfl > 0) {
                // Let the hogs actually get scheduled before measuring.
                struct timespec settle = {0, 120 * 1000 * 1000};
                nanosleep(&settle, NULL);
                double lm8 = proc_fork_mappings_ns(8, g_short_ns);
                double lm256 = proc_fork_mappings_ns(256, g_short_ns);
                load_stop(fl, nfl);
                printf("  -- now with %d CPU-bound processes competing --\n", nfl);
                row("[kernel]", "fork+exit, 8 maps, load",
                    lm8 < 0 ? NA : lm8 / 1000.0, "us", "");
                row("[kernel]", "fork+exit, 256 maps, load",
                    lm256 < 0 ? NA : lm256 / 1000.0, "us", "");
                if (lm8 > 0 && lm256 > 0) {
                    double per_map_load = (lm256 - lm8) / 248.0 / 1000.0;
                    if (per_map_load < 0)
                        per_map_load = 0;
                    row("[kernel]", "fork cost per mapping, load", per_map_load,
                        "us/mapping", per_map_load == 0 ? "flat within noise" : "");
                }
            }
            free(fl);
            printf("  Per-mapping cost is the blind spot of the per-MiB row above.\n");
            printf("  A copy-on-write fork stops paying per page and starts paying\n");
            printf("  per mapping, so a process with a hundred small mappings can\n");
            printf("  fork far more slowly than one holding the same bytes in a\n");
            printf("  single large one -- which no per-MiB number can express.\n");
            printf("  Compare the idle and loaded rows to separate the two things\n");
            printf("  that cost per mapping: the bookkeeping (same in both) and the\n");
            printf("  cross-CPU TLB shootdown (only paid when a peer is awake).\n");
            printf("  This is the copy-on-write test, and it matters more than any\n");
            printf("  single fork number. A copy-on-write fork shares the parent's\n");
            printf("  frames and write-protects them, so a process holding 100 MiB\n");
            printf("  forks about as cheaply as one holding nothing. A fork that\n");
            printf("  copies every resident frame up front is O(resident): the same\n");
            printf("  shell running the same command pays a full memcpy of the\n");
            printf("  process every single time it forks.\n");
            printf("  NOTE: when fork copies eagerly, the `COW fault (after fork)`\n");
            printf("  row in the VM section is meaningless — the child's pages are\n");
            printf("  already private, so it times plain stores and reports an\n");
            printf("  implausibly good number.\n");
        }
        double fe = proc_fork_exec_ns(g_short_ns, self);
        row("[kernel]", "fork + exec(self, static)", fe < 0 ? NA : fe / 1000.0,
            "us", "linux: ~350");
        const char *sh = find_shell();
        if (sh) {
            double sp = proc_spawn_shell_ns(g_short_ns, sh);
            char lbl[64];
            snprintf(lbl, sizeof lbl, "fork + exec(%s -c :)", sh);
            row("[kernel]", lbl, sp < 0 ? NA : sp / 1000.0, "us", "linux: ~1200");
        } else {
            printf("  (no shell found — the real 'launch a command' cost was\n");
            printf("   not measured; it is the one a script actually pays)\n");
        }
    }

    // ---- Graphics ----
    if (want(only, "gfx")) {
        line();
        printf("GRAPHICS / DRM-KMS   <-- what a compositor pays per frame\n");
        g_drm = open(g_drm_path, O_RDWR | O_CLOEXEC);
        if (g_drm < 0) {
            printf("  no DRM device at %s (%s) — section skipped.\n",
                   g_drm_path, strerror(errno));
            printf("  Pass --drm PATH if the node is elsewhere. On a Linux\n");
            printf("  control guest this usually means the kernel has no driver\n");
            printf("  bound to the emulated GPU: boot it with a DRM driver for\n");
            printf("  the same virtual device Eclipse is driving, or the two\n");
            printf("  sides are not running the same workload.\n");
        } else if (gfx_discover() != 0) {
            printf("  %s opened, but it exposes no usable CRTC/connector —\n",
                   g_drm_path);
            printf("  only the ioctl floor below is meaningful.\n");
            row("[kernel]", "DRM ioctl floor (GET_CAP)",
                timed_ns_per_op(gfx_getcap, g_short_ns), "ns", "linux: ~700");
        } else {
            printf("  device %s, %ux%u, crtc %u, connector %u\n", g_drm_path,
                   g_gw, g_gh, g_crtc_id, g_conn_id);

            // The graphics equivalent of the getpid row: a DRM ioctl that does
            // essentially nothing. Subtract it from any row below to separate
            // that operation's real work from generic ioctl dispatch.
            double capns = timed_ns_per_op(gfx_getcap, g_short_ns);
            row("[kernel]", "DRM ioctl floor (GET_CAP)", capns, "ns",
                "linux: ~700");
            gfx_ioctl_ns = capns;
            row("[kernel]", "MODE_GETRESOURCES",
                timed_ns_per_op(gfx_getres, g_short_ns), "ns", "linux: ~1200");
            row("[kernel]", "MODE_GETCONNECTOR",
                timed_ns_per_op(gfx_getconnector, g_short_ns), "ns",
                "linux: ~4000 (modes+props)");
            row("[kernel]", "MODE_GETCRTC",
                timed_ns_per_op(gfx_getcrtc, g_short_ns), "ns", "linux: ~1000");

            // Buffer lifecycle. A compositor allocates on every window resize
            // and on every swapchain rebuild, and a client on every surface.
            row("[kernel]", "CREATE+DESTROY_DUMB",
                timed_ns_per_op(gfx_dumb_cycle, g_short_ns) / 1000.0, "us",
                "linux: ~40");

            if (gfx_alloc() != 0) {
                printf("  (dumb buffer / framebuffer allocation failed — the\n");
                printf("   present-path rows below cannot run)\n");
            } else {
                row("[kernel]", "ADDFB2+RMFB",
                    timed_ns_per_op(gfx_fb_cycle, g_short_ns) / 1000.0, "us",
                    "linux: ~20");
                row("[kernel]", "MAP_DUMB+mmap+munmap",
                    timed_ns_per_op(gfx_map_cycle, g_short_ns) / 1000.0, "us",
                    "linux: ~15");
                // The cache-policy row. See the comment on the function.
                gfx_fbwrite_mibs = gfx_fb_write_mibs(g_budget_ns);
                // The ratio below needs an ordinary-memory reference. Measure
                // one here too, so `--only gfx` -- the mode anyone doing
                // graphics A/B will use -- is self-sufficient.
                // Same size as the framebuffer, deliberately: a reference
                // over a different working set measures the cache hierarchy
                // instead of the mapping, and the ratio stops meaning what it
                // claims to.
                // Same budget as the framebuffer loop as well as the same
                // size: the two figures are only a ratio if nothing but the
                // mapping differs between them.
                mem_copy_mibs =
                    ref_store_mibs(g_budget_ns, (size_t)g_dumb_size);
                row("[kernel]", "mapped fb write", gfx_fbwrite_mibs, "MiB/s",
                    "linux: >2000 (WC); <200 means uncached");
                if (gfx_fbwrite_mibs > 0) {
                    double frame_ms =
                        (double)g_gw * g_gh * 4.0 / (1024.0 * 1024.0)
                        / gfx_fbwrite_mibs * 1000.0;
                    row("[kernel]", "full-screen repaint", frame_ms, "ms",
                        "one CPU pass over the visible surface");
                }

                // Everything below changes what is on the display, so it runs
                // only when nothing else owns it.
                g_drm_master = drm_call(B_DRM_IOCTL_SET_MASTER, NULL) == 0;
                if (!g_drm_master) {
                    printf("  -- not DRM master (%s): a compositor owns the\n",
                           strerror(errno));
                    printf("     display, so the flip/cursor rows are skipped\n");
                    printf("     rather than fought over. Stop the desktop (or\n");
                    printf("     run from a text console) to measure them.\n");
                    row("[kernel]", "page flip -> event", NA, "us", "");
                    row("[kernel]", "MODE_CURSOR move", NA, "us", "");
                } else {
                    double mean_ms = 0, jit_ms = 0, worst_ms = 0;
                    if (gfx_vblank_stats(24, &mean_ms, &jit_ms, &worst_ms) == 0) {
                        // A real vblank wait returns at the refresh, so an
                        // interval far shorter than any display period means
                        // the ioctl is not waiting at all -- it is answering
                        // with the current sequence and returning. That is a
                        // correctness difference, not a fast kernel: every
                        // client that paces itself with WAIT_VBLANK (X's
                        // Present, SDL, older toolkits) spins instead of
                        // sleeping, burning a core to render frames nobody
                        // will see. Say so instead of printing a number that
                        // reads like a win.
                        int blocks = mean_ms >= 2.0;
                        row("[kernel]", "vblank interval", mean_ms, "ms",
                            blocks ? "60 Hz = 16.7"
                                   : "<-- WAIT_VBLANK returns without waiting");
                        row("[kernel]", "vblank jitter (stddev)", jit_ms, "ms",
                            blocks ? "linux: <1" : "(not a wait; see above)");
                        row("[kernel]", "vblank interval (worst)", worst_ms,
                            "ms", blocks ? "one late frame is one visible stutter"
                                         : "");
                        if (blocks) {
                            gfx_vblank_ms = mean_ms;
                        } else {
                            printf("  WAIT_VBLANK did not block: a client pacing\n");
                            printf("  itself on it will spin at full CPU instead of\n");
                            printf("  sleeping until the next frame.\n");
                        }
                    } else {
                        row("[kernel]", "vblank interval", NA, "ms",
                            "WAIT_VBLANK unsupported");
                    }

                    // The headline. A flip paced to the refresh SHOULD land on
                    // the vblank period: a figure far below it means frames are
                    // not paced (tearing, and a compositor that spins), and far
                    // above it means the present path itself is slow.
                    double flip_ns = timed_ns_per_op(gfx_pageflip, g_budget_ns);
                    gfx_flip_us = flip_ns < 0 ? NA : flip_ns / 1000.0;
                    row("[kernel]", "page flip -> event", gfx_flip_us, "us",
                        "60 Hz pacing = 16700");
                    if (flip_ns > 0)
                        row("[kernel]", "flip rate", 1e9 / flip_ns, "flips/s",
                            "60 Hz pacing = 60");
                    row("[kernel]", "MODE_CURSOR move",
                        timed_ns_per_op(gfx_cursor_move, g_short_ns) / 1000.0,
                        "us", "linux: ~30 (pointer rate is ~1 kHz)");
                    // Hand the display back to whatever was on it. Skipping
                    // this leaves the console scanning out our scratch buffer,
                    // which looks exactly like the benchmark broke the machine.
                    if (g_orig_fb && g_orig_fb != g_fb_a && g_orig_fb != g_fb_b) {
                        struct b_drm_mode_crtc_page_flip f;
                        memset(&f, 0, sizeof f);
                        f.crtc_id = g_crtc_id;
                        f.fb_id = g_orig_fb;
                        f.flags = B_DRM_MODE_PAGE_FLIP_EVENT;
                        if (drm_call(B_DRM_IOCTL_MODE_PAGE_FLIP, &f) == 0) {
                            unsigned char ev[96];
                            (void)!read(g_drm, ev, sizeof ev);
                        }
                    }
                    drm_call(B_DRM_IOCTL_DROP_MASTER, NULL);
                }
            }
            gfx_free();
            printf("  Under QEMU both kernels drive the SAME emulated GPU, so\n");
            printf("  every difference here is kernel code, not hardware. The\n");
            printf("  rows that decide how a desktop feels are `mapped fb write`\n");
            printf("  (a software compositor's whole life) and the flip pacing.\n");
        }
        if (g_drm >= 0) { close(g_drm); g_drm = -1; }
    }

    // ---- Ratios ----
    // These are the numbers to quote when someone says "but we are in a VM".
    // Each is a kernel cost divided by something measured on the *same* machine
    // in the *same* run, so hardware speed cancels out.
    line();
    printf("RATIOS (hardware-independent — these survive running in a VM)\n");
    if (cpu_chain_mops > 0 && getpid_ns > 0) {
        // How many dependent integer ops this CPU could have retired in the time
        // one minimal syscall takes.
        double ops = getpid_ns * cpu_chain_mops / 1000.0;
        row("", "syscall cost in CPU ops", ops, "ops", "linux: ~150-250");
    }
    if (getpid_ns > 0 && pipe_proc_ns > 0)
        row("", "context switch / syscall", pipe_proc_ns / getpid_ns, "x",
            "linux: ~100");
    if (sleep_idle_us > 0 && sleep_load_us > 0)
        row("", "wake late loaded/idle (mean)", sleep_load_us / sleep_idle_us,
            "x", "linux: ~1-3");
    if (sleep_idle_max_us > 0 && sleep_load_max_us > 0)
        row("", "wake late loaded/idle (worst)",
            sleep_load_max_us / sleep_idle_max_us, "x",
            "linux: ~1-5  <-- interactivity");
    if (smp1 > 0 && smpn > 0 && ncpu > 1)
        row("", "SMP efficiency", smpn / (smp1 * ncpu) * 100.0, "%",
            "linux: >90");
    if (fork_copy_ratio > 0)
        row("", "fork copy ratio", fork_copy_ratio, "x",
            "COW ~0.3, eager copy >=1");
    // A DRM ioctl is a trap plus a driver dispatch. Dividing by the bare trap
    // measured on this same machine says how much of it is the graphics stack
    // rather than the syscall path -- and, unlike the raw nanoseconds, it means
    // the same thing on an emulated CPU.
    if (gfx_ioctl_ns > 0 && getpid_ns > 0)
        row("", "DRM ioctl / syscall", gfx_ioctl_ns / getpid_ns, "x",
            "linux: ~10-20 native; compressed under an emulator");
    // 1.0 means flips land exactly on the refresh, which is what a correctly
    // paced compositor gets. Well below 1 means frames are not being paced to
    // the display at all; well above 1 means the present path misses vblanks.
    if (gfx_flip_us > 0 && gfx_vblank_ms > 0)
        row("", "flip period / vblank", gfx_flip_us / 1000.0 / gfx_vblank_ms,
            "x", "1.0 = paced to the refresh");
    // The framebuffer mapping against ordinary anonymous memory, same store
    // loop and same working-set size, so the only difference is the mapping.
    // Near 1 means the framebuffer mapping is as fast as ordinary RAM (write
    // combining); a small fraction means the kernel mapped it uncached and
    // every repaint pays for it.
    if (gfx_fbwrite_mibs > 0 && mem_copy_mibs > 0)
        row("", "fb write / memcpy", gfx_fbwrite_mibs / mem_copy_mibs, "x",
            "1.0 = write-combined; <0.1 = uncached");
    printf("\n");
    printf("  `wake late loaded/idle` is the headline. A value near 1 means a\n");
    printf("  woken task gets a CPU straight away even when the machine is busy.\n");
    printf("  A large value means it waits for someone else's timeslice to run\n");
    printf("  out — the system will feel sluggish no matter how good the [user]\n");
    printf("  numbers above look. The (worst) row is the stutter you actually\n");
    printf("  notice; a mean can hide one 20 ms stall in forty prompt wakes.\n");

    if (g_devnull >= 0) close(g_devnull);
    if (g_devzero >= 0) close(g_devzero);

    line();
    printf("done. (g_sink=%llu)\n", (unsigned long long)g_sink);
    return 0;
}
