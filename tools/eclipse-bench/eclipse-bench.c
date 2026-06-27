// eclipse-bench — a small, self-contained CPU / memory / disk / process
// benchmark for Eclipse OS.
//
// It is deliberately dependency-free (POSIX + libc only) and statically linked,
// so it can be dropped straight into the rootfs and run from the shell. Every
// micro-benchmark is *time-bounded* (it runs for a target wall-clock budget and
// counts how much work it completed) so the whole suite finishes in well under a
// minute even on a slow USB stick, and adapts automatically to fast (QEMU) vs
// slow (real disk) machines.
//
// Build (musl, static):
//     x86_64-linux-musl-gcc -O2 -static -o eclipse-bench eclipse-bench.c
// then copy `eclipse-bench` into the rootfs and run:
//     ./eclipse-bench [DIR] [DISK_MB] [MEM_MB]
//
// DIR     directory for the disk tests — MUST live on the filesystem you want to
//         measure (the btrfs/ext2 root), NOT a tmpfs like /tmp or /run, or the
//         "disk" numbers will just measure RAM. Default: current directory.
// DISK_MB size of the disk test file in MiB (default 32).
// MEM_MB  size of the memory working set in MiB (default 32).
//
// The CPU section is a clean proxy for effective core frequency: comparing its
// numbers before/after the P-state change (or QEMU vs USB) shows the clock
// you are actually getting. The disk metadata / random-read sections stress
// exactly the paths that make exec and boot slow.

#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <errno.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
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

// Default per-microbench wall-clock budget.
#define BUDGET_NS 400000000ull // 0.4 s

// Run `fn(n)` with a growing `n` until one call lasts >= BUDGET_NS, then return
// the achieved rate in operations/second. `fn` must return a value derived from
// its work (fed into g_sink) so it isn't optimized away.
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

// ---------------------------------------------------------------------------
// CPU
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
// Memory
// ---------------------------------------------------------------------------

static size_t g_mem_bytes;
static unsigned char *g_mem_src, *g_mem_dst;
static size_t *g_chase; // permuted index cycle for pointer chasing

// memcpy bandwidth: returns bytes copied this call.
static uint64_t mem_copy(uint64_t passes) {
    uint64_t bytes = 0;
    for (uint64_t p = 0; p < passes; p++) {
        memcpy(g_mem_dst, g_mem_src, g_mem_bytes);
        bytes += g_mem_bytes;
    }
    return bytes;
}

// memset bandwidth.
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
    // Sattolo: produces a single n-cycle.
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
// Syscall overhead
// ---------------------------------------------------------------------------

static uint64_t sys_getpid_loop(uint64_t iters) {
    uint64_t acc = 0;
    for (uint64_t i = 0; i < iters; i++)
        acc += (uint64_t)getpid();
    return acc;
}

// ---------------------------------------------------------------------------
// Disk
// ---------------------------------------------------------------------------

static void hr_bytes(double bps, char *out, size_t n) {
    const char *u = "B/s";
    double v = bps;
    if (v >= 1e9) { v /= 1e9; u = "GB/s"; }
    else if (v >= 1e6) { v /= 1e6; u = "MB/s"; }
    else if (v >= 1e3) { v /= 1e3; u = "KB/s"; }
    snprintf(out, n, "%.1f %s", v, u);
}

// Sequential write of `bytes` to `path` in `chunk`-sized writes, fsync at the
// end. Returns achieved bytes/sec including the fsync.
static double disk_seq_write(const char *path, size_t bytes, size_t chunk,
                             unsigned char *buf) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1.0;
    uint64_t t0 = now_ns();
    size_t done = 0;
    while (done < bytes) {
        size_t want = bytes - done < chunk ? bytes - done : chunk;
        ssize_t w = write(fd, buf, want);
        if (w <= 0) { close(fd); return -1.0; }
        done += (size_t)w;
    }
    fsync(fd);
    uint64_t t1 = now_ns();
    close(fd);
    return (double)bytes * 1e9 / (double)(t1 - t0);
}

// Sequential read of the whole file.
static double disk_seq_read(const char *path, size_t chunk, unsigned char *buf) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1.0;
    uint64_t t0 = now_ns();
    uint64_t total = 0;
    for (;;) {
        ssize_t r = read(fd, buf, chunk);
        if (r < 0) { close(fd); return -1.0; }
        if (r == 0) break;
        total += (uint64_t)r;
    }
    uint64_t t1 = now_ns();
    close(fd);
    if (total == 0) return -1.0;
    return (double)total * 1e9 / (double)(t1 - t0);
}

// Random 4 KiB reads at random aligned offsets for a time budget. Returns IOPS
// and writes the average latency (µs) to *avg_us.
static double disk_rand_read(const char *path, size_t bytes, uint64_t budget_ns,
                             double *avg_us) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1.0;
    const size_t blk = 4096;
    size_t nblk = bytes / blk;
    if (nblk == 0) { close(fd); return -1.0; }
    unsigned char b[4096];
    uint64_t r = 0x1234567890abcdefull;
    uint64_t ops = 0, t0 = now_ns(), elapsed = 0;
    while (elapsed < budget_ns) {
        for (int k = 0; k < 64; k++) {
            r = r * 6364136223846793005ull + 1442695040888963407ull;
            off_t off = (off_t)((r >> 12) % nblk) * (off_t)blk;
            if (pread(fd, b, blk, off) != (ssize_t)blk) { close(fd); return -1.0; }
            ops++;
        }
        elapsed = now_ns() - t0;
    }
    close(fd);
    double iops = (double)ops * 1e9 / (double)elapsed;
    if (avg_us) *avg_us = (double)elapsed / (double)ops / 1000.0;
    return iops;
}

// fsync latency: write one small block + fsync, repeat a few times, return the
// best (lowest) latency in milliseconds.
static double disk_fsync_ms(const char *path, unsigned char *buf) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1.0;
    double best = 1e30;
    for (int i = 0; i < 8; i++) {
        if (write(fd, buf, 4096) != 4096) { close(fd); return -1.0; }
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
    *creates_s = *stats_s = *unlinks_s = -1.0;

    // create
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

    // stat
    t0 = now_ns();
    int ok = 0;
    for (int i = 0; i < made; i++) {
        snprintf(path, sizeof path, "%s/eb_%06d", dir, i);
        struct stat st;
        if (stat(path, &st) == 0) ok++;
    }
    t1 = now_ns();
    if (ok > 0) *stats_s = (double)ok * 1e9 / (double)(t1 - t0);

    // unlink (also cleans up)
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
// Process creation
// ---------------------------------------------------------------------------

// fork + immediate child _exit, parent waits. Returns forks/sec.
static double proc_fork(uint64_t budget_ns, const char *unused) {
    (void)unused;
    uint64_t ops = 0, t0 = now_ns();
    while (now_ns() - t0 < budget_ns) {
        pid_t p = fork();
        if (p == 0) _exit(0);
        if (p < 0) return -1.0;
        int st;
        waitpid(p, &st, 0);
        ops++;
    }
    return (double)ops * 1e9 / (double)(now_ns() - t0);
}

// fork + execve(self, "--noop") which exits at once, parent waits. Measures the
// full process-replacement cost (address-space teardown + ELF load + setup) —
// the path that dominates "programs/boot are slow".
static double proc_fork_exec(uint64_t budget_ns, const char *self) {
    uint64_t ops = 0, t0 = now_ns();
    char *const argv[] = {(char *)self, (char *)"--noop", NULL};
    while (now_ns() - t0 < budget_ns) {
        pid_t p = fork();
        if (p == 0) {
            execv(self, argv);
            _exit(127);
        }
        if (p < 0) return -1.0;
        int st;
        waitpid(p, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) return -2.0;
        ops++;
    }
    return (double)ops * 1e9 / (double)(now_ns() - t0);
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

static void line(void) {
    printf("--------------------------------------------------------------\n");
}

int main(int argc, char **argv) {
    // Self-exec target for the fork+exec benchmark: exit immediately.
    if (argc > 1 && strcmp(argv[1], "--noop") == 0)
        return 0;

    const char *dir = (argc > 1) ? argv[1] : ".";
    size_t disk_mb = (argc > 2) ? (size_t)strtoul(argv[2], NULL, 10) : 32;
    size_t mem_mb = (argc > 3) ? (size_t)strtoul(argv[3], NULL, 10) : 32;
    if (disk_mb < 1) disk_mb = 1;
    if (mem_mb < 1) mem_mb = 1;

    char self[512];
    ssize_t sl = readlink("/proc/self/exe", self, sizeof self - 1);
    if (sl > 0) self[sl] = 0;
    else snprintf(self, sizeof self, "%s", argv[0]);

    printf("eclipse-bench — CPU / memory / disk / process\n");
    printf("dir=%s  disk=%zu MiB  mem=%zu MiB  self=%s\n", dir, disk_mb, mem_mb, self);
    printf("(disk dir must be on the real fs you want to measure, not a tmpfs)\n\n");

    char hb[32];

    // ---- CPU ----
    line();
    printf("CPU\n");
    double r;
    r = timed_oprate(cpu_int_chain, BUDGET_NS);
    printf("  int   latency (dependent MAC) : %8.1f Mops/s\n", r / 1e6);
    r = timed_oprate(cpu_int_tput, BUDGET_NS);
    printf("  int   throughput (4-wide)     : %8.1f Mops/s\n", r * 4 / 1e6);
    r = timed_oprate(cpu_double_chain, BUDGET_NS);
    printf("  float latency (dependent MAC) : %8.1f Mops/s\n", r / 1e6);
    r = timed_oprate(sys_getpid_loop, BUDGET_NS);
    printf("  syscall getpid()              : %8.1f M/s  (%.0f ns/call)\n",
           r / 1e6, 1e9 / r);

    // ---- Memory ----
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
        memset(g_mem_src, 0xa5, g_mem_bytes);
        r = timed_oprate(mem_copy, BUDGET_NS); // passes/sec
        hr_bytes(r * (double)g_mem_bytes, hb, sizeof hb);
        printf("  memcpy bandwidth              : %12s\n", hb);
        r = timed_oprate(mem_set, BUDGET_NS);
        hr_bytes(r * (double)g_mem_bytes, hb, sizeof hb);
        printf("  memset bandwidth              : %12s\n", hb);
        build_chase(chase_n);
        r = timed_oprate(mem_chase, BUDGET_NS); // hops/sec
        printf("  random access latency         : %8.1f ns/access (set %zu MiB)\n",
               1e9 / r, mem_mb);
    }
    free(g_mem_src); free(g_mem_dst); free(g_chase);

    // ---- Disk ----
    line();
    printf("DISK (in %s, %zu MiB requested)\n", dir, disk_mb);
    size_t dbytes = disk_mb * 1024 * 1024;
    int meta_max = 4000;
    // Cap the disk working set to a fraction of the FREE space. A small or
    // nearly-full filesystem (notably the in-RAM SFS root that `make qemu`
    // boots) must not be filled: some filesystems panic on ENOSPC instead of
    // failing the write, which would crash the whole machine mid-benchmark.
    // Leave a generous margin (use <= 1/3 of free).
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
            // Could not size the fs: stay tiny so we can't fill it.
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
            if (w < 0) printf("  seq write                     : FAILED (cannot write in %s)\n", dir);
            else { hr_bytes(w, hb, sizeof hb); printf("  seq write (+fsync)            : %12s\n", hb); }

            double rd = disk_seq_read(fpath, chunk, io);
            if (rd < 0) printf("  seq read                      : FAILED\n");
            else { hr_bytes(rd, hb, sizeof hb); printf("  seq read                      : %12s\n", hb); }

            double avg_us = 0;
            double iops = disk_rand_read(fpath, dbytes, BUDGET_NS, &avg_us);
            if (iops < 0) printf("  rand 4K read                  : FAILED\n");
            else printf("  rand 4K read                  : %8.0f IOPS (%.1f us avg)\n", iops, avg_us);

            double fs = disk_fsync_ms(fpath, io);
            if (fs >= 0) printf("  fsync latency (best)          : %8.2f ms\n", fs);

            unlink(fpath);
        } else {
            printf("  (too little free space for the streaming tests — point\n");
            printf("   DIR at a real disk/partition with more room)\n");
        }

        if (meta_max >= 20) {
            double cps, sps, ups;
            disk_metadata(dir, BUDGET_NS, meta_max, &cps, &sps, &ups);
            if (cps >= 0) printf("  meta create small files       : %8.0f files/s\n", cps);
            if (sps >= 0) printf("  meta stat                     : %8.0f stats/s\n", sps);
            if (ups >= 0) printf("  meta unlink                   : %8.0f unlinks/s\n", ups);
        } else {
            printf("  meta ops                      : skipped (low free space)\n");
        }
        free(io);
    }

    // ---- Process ----
    line();
    printf("PROCESS\n");
    double fr = proc_fork(BUDGET_NS, NULL);
    if (fr < 0) printf("  fork + exit                   : FAILED\n");
    else printf("  fork + exit                   : %8.0f /s  (%.2f ms each)\n", fr, 1000.0 / fr);
    double fe = proc_fork_exec(BUDGET_NS, self);
    if (fe == -2.0) printf("  fork + exec(self)             : child error (bad self path?)\n");
    else if (fe < 0) printf("  fork + exec(self)             : FAILED\n");
    else printf("  fork + exec(self)             : %8.0f /s  (%.2f ms each)\n", fe, 1000.0 / fe);

    line();
    printf("done. (g_sink=%llu)\n", (unsigned long long)g_sink);
    return 0;
}
