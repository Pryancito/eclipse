// Eclipse OS: do a thread's x87, SSE and AVX registers survive the kernel?
//
// The kernel saves them when a thread comes in and, on the way out, loads
// them back only if the CPU is holding something else (see `FpOwner` in
// vendor/trapframe). Getting that wrong is silent: every call returns what it
// should and some other thread's numbers are in the registers. A compiler
// keeps anything in them -- a memcpy in flight, a loop counter, a double --
// so the symptom is corrupted data far from the cause.
//
// Each check loads a pattern of its own into xmm0-15 (the whole ymm where
// there is AVX), an x87 value and an MXCSR rounding mode, makes a system
// call from the same block of assembly, and compares what the registers hold
// afterwards:
//
//   * one thread alone, call after call (the kernel loads nothing);
//   * four processes, then four threads, yielding and sleeping (the kernel
//     must load each one's own every time, on whichever CPU);
//   * fork, where both sides come back with the parent's;
//   * a signal handler that dirties every register, every millisecond.
//
// Every check is one Linux holds: `make linux` and run it there.
//
// x86_64 only. `fp-probe noavx` checks xmm alone on a CPU that has AVX.
#define _GNU_SOURCE
#include <cpuid.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int have_avx;
struct regs {
    uint8_t v[16][32];
    long double st0;
    uint32_t mxcsr;
};
static void fill(struct regs *r, uint64_t seed) {
    for (int i = 0; i < 16; i++)
        for (int k = 0; k < 32; k++)
            r->v[i][k] = (uint8_t)(seed * 131 + i * 17 + k * 7 + (seed >> 8));
    r->st0 = (long double)seed * 1.25L + 0.5L;
    r->mxcsr = 0x1f80 | ((seed & 3) << 13);
}
// Load `in`, make system call `nr`, store what the registers hold into
// `out`. Returns the call's result.
__attribute__((noinline)) static long across(const struct regs *in, struct regs *out, long nr, long a0) {
    long ret;
    if (have_avx) {
        __asm__ volatile(
            "vmovdqu 0(%[i]), %%ymm0\n vmovdqu 32(%[i]), %%ymm1\n vmovdqu 64(%[i]), %%ymm2\n vmovdqu 96(%[i]), %%ymm3\n"
            "vmovdqu 128(%[i]), %%ymm4\n vmovdqu 160(%[i]), %%ymm5\n vmovdqu 192(%[i]), %%ymm6\n vmovdqu 224(%[i]), %%ymm7\n"
            "vmovdqu 256(%[i]), %%ymm8\n vmovdqu 288(%[i]), %%ymm9\n vmovdqu 320(%[i]), %%ymm10\n vmovdqu 352(%[i]), %%ymm11\n"
            "vmovdqu 384(%[i]), %%ymm12\n vmovdqu 416(%[i]), %%ymm13\n vmovdqu 448(%[i]), %%ymm14\n vmovdqu 480(%[i]), %%ymm15\n"
            "fldt 512(%[i])\n ldmxcsr 528(%[i])\n"
            "syscall\n"
            "vmovdqu %%ymm0, 0(%[o])\n vmovdqu %%ymm1, 32(%[o])\n vmovdqu %%ymm2, 64(%[o])\n vmovdqu %%ymm3, 96(%[o])\n"
            "vmovdqu %%ymm4, 128(%[o])\n vmovdqu %%ymm5, 160(%[o])\n vmovdqu %%ymm6, 192(%[o])\n vmovdqu %%ymm7, 224(%[o])\n"
            "vmovdqu %%ymm8, 256(%[o])\n vmovdqu %%ymm9, 288(%[o])\n vmovdqu %%ymm10, 320(%[o])\n vmovdqu %%ymm11, 352(%[o])\n"
            "vmovdqu %%ymm12, 384(%[o])\n vmovdqu %%ymm13, 416(%[o])\n vmovdqu %%ymm14, 448(%[o])\n vmovdqu %%ymm15, 480(%[o])\n"
            "fstpt 512(%[o])\n stmxcsr 528(%[o])\n vzeroupper\n"
            "ldmxcsr %[dflt]\n"
            : "=a"(ret)
            : "a"(nr), "D"(a0), [i] "r"(in), [o] "r"(out), [dflt] "m"((uint32_t){0x1f80})
            : "rcx", "r11", "memory", "cc", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8",
              "xmm9", "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15", "st");
    } else {
        __asm__ volatile(
            "movdqu 0(%[i]), %%xmm0\n movdqu 32(%[i]), %%xmm1\n movdqu 64(%[i]), %%xmm2\n movdqu 96(%[i]), %%xmm3\n"
            "movdqu 128(%[i]), %%xmm4\n movdqu 160(%[i]), %%xmm5\n movdqu 192(%[i]), %%xmm6\n movdqu 224(%[i]), %%xmm7\n"
            "movdqu 256(%[i]), %%xmm8\n movdqu 288(%[i]), %%xmm9\n movdqu 320(%[i]), %%xmm10\n movdqu 352(%[i]), %%xmm11\n"
            "movdqu 384(%[i]), %%xmm12\n movdqu 416(%[i]), %%xmm13\n movdqu 448(%[i]), %%xmm14\n movdqu 480(%[i]), %%xmm15\n"
            "fldt 512(%[i])\n ldmxcsr 528(%[i])\n"
            "syscall\n"
            "movdqu %%xmm0, 0(%[o])\n movdqu %%xmm1, 32(%[o])\n movdqu %%xmm2, 64(%[o])\n movdqu %%xmm3, 96(%[o])\n"
            "movdqu %%xmm4, 128(%[o])\n movdqu %%xmm5, 160(%[o])\n movdqu %%xmm6, 192(%[o])\n movdqu %%xmm7, 224(%[o])\n"
            "movdqu %%xmm8, 256(%[o])\n movdqu %%xmm9, 288(%[o])\n movdqu %%xmm10, 320(%[o])\n movdqu %%xmm11, 352(%[o])\n"
            "movdqu %%xmm12, 384(%[o])\n movdqu %%xmm13, 416(%[o])\n movdqu %%xmm14, 448(%[o])\n movdqu %%xmm15, 480(%[o])\n"
            "fstpt 512(%[o])\n stmxcsr 528(%[o])\n"
            "ldmxcsr %[dflt]\n"
            : "=a"(ret)
            : "a"(nr), "D"(a0), [i] "r"(in), [o] "r"(out), [dflt] "m"((uint32_t){0x1f80})
            : "rcx", "r11", "memory", "cc", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8",
              "xmm9", "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15", "st");
    }
    return ret;
}
static int same(const struct regs *a, const struct regs *b) {
    int w = have_avx ? 32 : 16;
    for (int i = 0; i < 16; i++)
        if (memcmp(a->v[i], b->v[i], w))
            return 0;
    return a->st0 == b->st0 && (a->mxcsr & 0x6000) == (b->mxcsr & 0x6000);
}
static volatile sig_atomic_t alarms;
static void on_alarm(int s) {
    (void)s;
    // Dirty every register the interrupted code may have been holding.
    struct regs junk, out;
    fill(&junk, 0xdead0000 + alarms);
    across(&junk, &out, 39 /* getpid */, 0);
    alarms++;
}
static long worker(uint64_t seed, long rounds, int with_sleep) {
    struct regs in, out;
    long bad = 0;
    struct timespec ms = {0, 1000000};
    for (long k = 0; k < rounds; k++) {
        fill(&in, seed + k);
        if (with_sleep && k % 997 == 0)
            nanosleep(&ms, NULL);
        across(&in, &out, k % 4 == 3 ? 24 /* sched_yield */ : 110 /* getppid */, 0);
        if (!same(&in, &out))
            bad++;
    }
    return bad;
}
static void *thread_main(void *arg) {
    return (void *)worker((uint64_t)(uintptr_t)arg, 20000, 1);
}
int main(int argc, char **argv) {
    unsigned a, b, c, d;
    __cpuid(1, a, b, c, d);
    have_avx = (c & (1u << 28)) && (c & (1u << 27));
    if (argc > 1 && !strcmp(argv[1], "noavx"))
        have_avx = 0;
    long total = 0;
    // 1. One thread alone: the registers come back from every call.
    long bad = worker(1000, 50000, 0);
    printf("fp-probe: alone, 50000 calls: %ld bad\n", bad);
    total += bad;
    // 2. Four processes taking turns.
    pid_t kids[4];
    for (int i = 0; i < 4; i++) {
        kids[i] = fork();
        if (kids[i] == 0)
            _exit(worker(5000 + i * 100000, 20000, 1) ? 1 : 0);
    }
    bad = 0;
    for (int i = 0; i < 4; i++) {
        int st;
        waitpid(kids[i], &st, 0);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    printf("fp-probe: four processes taking turns: %ld bad\n", bad);
    total += bad;
    // 3. Four threads of one process.
    pthread_t t[4];
    for (int i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, thread_main, (void *)(uintptr_t)(900000 + i * 100000));
    bad = 0;
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(t[i], &r);
        bad += (long)r;
    }
    printf("fp-probe: four threads taking turns: %ld bad\n", bad);
    total += bad;
    // 4. fork: both sides come back with the registers the parent had.
    bad = 0;
    for (int k = 0; k < 200; k++) {
        struct regs in, out;
        fill(&in, 77000 + k);
        long pid = across(&in, &out, 57 /* fork */, 0);
        if (pid == 0)
            _exit(same(&in, &out) ? 0 : 1);
        int st;
        waitpid((pid_t)pid, &st, 0);
        bad += !same(&in, &out);
        bad += !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    printf("fp-probe: 200 forks, parent and child: %ld bad\n", bad);
    total += bad;
    // 5. A signal handler that dirties everything, every millisecond.
    signal(SIGALRM, on_alarm);
    struct itimerval it = {{0, 1000}, {0, 1000}};
    setitimer(ITIMER_REAL, &it, NULL);
    bad = worker(3000000, 50000, 0);
    it = (struct itimerval){{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &it, NULL);
    printf("fp-probe: under %d signals whose handler dirties the registers: %ld bad\n", (int)alarms, bad);
    total += bad;
    printf("fp-probe: %s (%s)\n", total ? "FAIL" : "all checks passed", have_avx ? "ymm" : "xmm");
    return total != 0;
}
