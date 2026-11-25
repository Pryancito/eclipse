/* sysfuzz: random syscall programs against the Eclipse OS kernel.
 *
 * Each program is a forked child that makes CALLS system calls chosen from
 * the table below, with arguments drawn from the values that break kernels:
 * fds of every kind the child has open, pointers that are NULL, kernel
 * addresses, non-canonical, misaligned, read-only, PROT_NONE or straddling
 * into an unmapped page, and lengths and flags at their edges. A user
 * process may get any errno or signal; what this hunts is the kernel
 * panicking, oopsing or hanging, which the driver (scripts/qemu-sysfuzz.py)
 * sees on the serial console. Not coverage-guided: the kernel has no kcov.
 *
 * Every program is a pure function of its seed, printed before it runs, so
 * a crash is replayed with `sysfuzz -r SEED -v`, which also prints every
 * call as it makes it -- the last line before the panic is the culprit.
 *
 *   gcc -static -O2 -o sysfuzz tools/sysfuzz/sysfuzz.c
 *   sysfuzz [-s SEED] [-n PROGRAMS] [-t SECONDS] [-c CALLS]
 *   sysfuzz -r SEED [-c CALLS] [-v]
 *
 * Output: "FZ-PROG n seed" per program, "FZ-HANG seed" for a child that
 * outlived its deadline, "FZ-DONE progs=N hangs=H" at the end. Test tool, so
 * C; nothing in it ships in the image.
 *
 * Kept out on purpose: what would end the run for reasons that are not bugs
 * (reboot, kill of other processes, ptrace, mount, module loading, writes
 * outside the scratch directory, the clock, the hostname, the console's tty).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>

#define PAGE 4096UL
#define NFDS 24
#define BUF_PAGES 16

/* ── deterministic randomness ─────────────────────────────────────────── */

static uint64_t rng;

static uint64_t rnd(void) {
    /* splitmix64: every program is a function of its seed alone. */
    uint64_t z = (rng += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static uint64_t below(uint64_t n) { return n ? rnd() % n : 0; }

static int verbose;

/* Where `-v` writes, out of the way of fds 0-2: those are /dev/null in a
 * program, so what the calls write cannot reach the console the driver
 * reads (a stray escape sequence there garbles every line after it). */
#define LOGFD 1000

/* ── what the arguments are drawn from ────────────────────────────────── */

static unsigned char *buf;     /* BUF_PAGES read-write pages              */
static unsigned char *ro;      /* one read-only page                      */
static unsigned char *edge;    /* a read-write page with PROT_NONE after   */
static int fds[NFDS];
static int nfds;

static const uint64_t edge_values[] = {
    0, 1, 2, 3, 4, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256, 511, 512,
    1023, 1024, 4095, 4096, 4097, 8191, 8192, 65535, 65536, 65537,
    0x7fffffff, 0x80000000, 0xffffffff, 0x100000000ULL,
    0x7fffffffffffffffULL, 0x8000000000000000ULL, 0xffffffffffffffffULL,
    0xfffffffffffff000ULL, 0x00007ffffffff000ULL,
};

static uint64_t any_len(void) {
    switch (below(4)) {
    case 0: return edge_values[below(sizeof edge_values / sizeof *edge_values)];
    case 1: return below(PAGE * 2);
    case 2: return below(64);
    default: return rnd();
    }
}

static uint64_t any_flags(void) {
    switch (below(4)) {
    case 0: return 0;
    case 1: return 1ULL << below(32);
    case 2: return rnd() & 0xffff;
    default: return rnd();
    }
}

/* A pointer: mostly usable, the rest every way a pointer can be wrong. */
static uint64_t any_ptr(void) {
    switch (below(14)) {
    case 0: return 0;
    case 1: return 0xffff800000000000ULL;             /* kernel half      */
    case 2: return 0xffffffffffffffffULL - below(64);  /* top of the map   */
    case 3: return 0x0000800000000000ULL;             /* non-canonical    */
    case 4: return (uint64_t)ro;                      /* read-only        */
    case 5: return (uint64_t)edge + PAGE - below(16); /* runs into a hole */
    case 6: return (uint64_t)edge + PAGE;             /* the hole itself  */
    case 7: return (uint64_t)buf + 1 + below(7);      /* misaligned       */
    case 8: return below(PAGE * 16);                  /* low, unmapped    */
    default: return (uint64_t)buf + below(BUF_PAGES - 1) * PAGE + below(64) * 8;
    }
}

static int any_fd(void) {
    switch (below(10)) {
    case 0: return -1;
    case 1: return (int)below(1024);
    case 2: return (int)rnd();
    default: return nfds ? fds[below(nfds)] : -1;
    }
}

static uint64_t any_int(void) {
    return below(3) ? edge_values[below(sizeof edge_values / sizeof *edge_values)] : rnd();
}

/* A signal that cannot stop or end the child behind the driver's back. */
static int any_sig(void) {
    static const int ok[] = {0, SIGUSR1, SIGUSR2, SIGCHLD, SIGWINCH, SIGURG, SIGPIPE, SIGALRM};
    return ok[below(sizeof ok / sizeof *ok)];
}

/* Names only inside the scratch directory: relative, short, no "..". */
static uint64_t any_path(void) {
    static char paths[4][16];
    static int next;
    char *p = paths[next++ & 3];
    switch (below(6)) {
    case 0: return any_ptr();
    case 1: strcpy(p, "."); break;
    case 2: strcpy(p, "d"); break;
    case 3: strcpy(p, "d/f"); break;
    default: snprintf(p, sizeof paths[0], "f%d", (int)below(6)); break;
    }
    return (uint64_t)p;
}

static void keep_fd(long fd) {
    if (fd < 0 || fd == 0 || fd == 1 || fd == 2)
        return;
    if (nfds < NFDS)
        fds[nfds++] = (int)fd;
    else
        fds[below(NFDS)] = (int)fd;
}

/* Fill part of the buffer with values a kernel struct parser trips over:
 * small numbers (counts, lengths, fds), pointers into the buffer, edges. */
static void scribble(void) {
    uint64_t *w = (uint64_t *)buf;
    size_t n = (BUF_PAGES - 1) * PAGE / 8;
    for (int i = 0; i < 32; i++) {
        size_t at = below(n);
        switch (below(4)) {
        case 0: w[at] = below(16); break;
        case 1: w[at] = any_ptr(); break;
        case 2: w[at] = any_len(); break;
        default: w[at] = rnd(); break;
        }
    }
}

/* ── the calls ─────────────────────────────────────────────────────────── */

typedef long (*gen_fn)(uint64_t a[6]);

struct call {
    const char *name;
    long nr;
    /* per argument: f fd, p pointer, l length, F flags, i int, P path,
     * s signal, z zero, m pid of this process, - unused */
    const char *args;
    int keeps_fd; /* the result is an fd to put in the pool */
};

#define C(n, a, k) {#n, SYS_##n, a, k}
static const struct call calls[] = {
    C(read, "fpl", 0), C(write, "fpl", 0), C(pread64, "fpli", 0), C(pwrite64, "fpli", 0),
    C(readv, "fpl", 0), C(writev, "fpl", 0), C(preadv, "fplii", 0), C(pwritev, "fplii", 0),
    C(lseek, "fii", 0), C(close, "f", 0), C(dup, "f", 1), C(dup3, "ffF", 1),
    C(fcntl, "fip", 0), C(ioctl, "fip", 0), C(openat, "fPFi", 1), C(mkdirat, "fPi", 0),
    C(unlinkat, "fPF", 0), C(renameat2, "fPfPF", 0), C(linkat, "fPfPF", 0),
    C(symlinkat, "PfP", 0), C(readlinkat, "fPpl", 0), C(fstat, "fp", 0),
    C(newfstatat, "fPpF", 0), C(statx, "fPFip", 0), C(getdents64, "fpl", 0),
    C(ftruncate, "fl", 0), C(fallocate, "fill", 0), C(fsync, "f", 0), C(fdatasync, "f", 0),
    C(fchdir, "f", 0), C(flock, "fi", 0), C(utimensat, "fPpF", 0),
    C(faccessat2, "fPiF", 0), C(getcwd, "pl", 0), C(umask, "i", 0),
    C(mmap, "plFFfl", 0), C(munmap, "pl", 0), C(mprotect, "plF", 0), C(mremap, "pllFp", 0),
    C(madvise, "pli", 0), C(msync, "plF", 0), C(mincore, "plp", 0), C(mlock, "pl", 0),
    C(munlock, "pl", 0), C(brk, "p", 0),
    C(pipe2, "pF", 0), C(socketpair, "iiip", 0), C(socket, "iii", 1), C(bind, "fpl", 0),
    C(connect, "fpl", 0), C(listen, "fi", 0), C(accept4, "fppF", 1), C(sendto, "fplFpl", 0),
    C(recvfrom, "fplFpp", 0), C(sendmsg, "fpF", 0), C(recvmsg, "fpF", 0), C(shutdown, "fi", 0),
    C(getsockopt, "fiipp", 0), C(setsockopt, "fiipl", 0), C(getsockname, "fpp", 0),
    C(getpeername, "fpp", 0), C(sendfile, "ffpl", 0), C(splice, "fpfplF", 0),
    C(tee, "fflF", 0), C(vmsplice, "fplF", 0), C(copy_file_range, "fpfplF", 0),
    C(epoll_create1, "F", 1), C(epoll_ctl, "fifp", 0), C(epoll_pwait, "fpizpl", 0),
    C(poll, "plz", 0), C(ppoll, "plppl", 0), C(eventfd2, "iF", 1), C(timerfd_create, "iF", 1),
    C(timerfd_settime, "fFpp", 0), C(timerfd_gettime, "fp", 0), C(signalfd4, "fplF", 1),
    C(inotify_init1, "F", 1), C(inotify_add_watch, "fPF", 0), C(memfd_create, "PF", 1),
    C(futex, "pFipp", 0), C(clock_gettime, "ip", 0), C(clock_getres, "ip", 0),
    C(gettimeofday, "pp", 0), C(getrandom, "plF", 0), C(uname, "p", 0), C(sysinfo, "p", 0),
    C(getrlimit, "ip", 0), C(prlimit64, "zipp", 0), C(sched_yield, "", 0),
    C(sched_getaffinity, "zlp", 0), C(sched_setaffinity, "zlp", 0), C(getpid, "", 0),
    C(gettid, "", 0), C(getppid, "", 0), C(kill, "ms", 0), C(rt_sigaction, "sppl", 0),
    C(rt_sigprocmask, "ippl", 0), C(rt_sigpending, "pl", 0), C(sigaltstack, "pp", 0),
    C(prctl, "iiiii", 0), C(wait4, "ipFp", 0), C(getrusage, "ip", 0), C(times, "p", 0),
    C(shmget, "zlF", 0), C(shmat, "ipF", 0), C(shmdt, "p", 0), C(shmctl, "iip", 0),
    C(semget, "ziF", 0), C(semop, "ipl", 0), C(semctl, "iiip", 0),
    C(msgget, "zF", 0), C(msgsnd, "iplF", 0), C(msgrcv, "iplFF", 0), C(msgctl, "iip", 0),
    C(set_robust_list, "pl", 0), C(get_robust_list, "zpp", 0), C(rseq, "plFi", 0),
    C(membarrier, "Fiz", 0), C(userfaultfd, "F", 1), C(pidfd_open, "mF", 1),
    C(close_range, "iiF", 0), C(name_to_handle_at, "fPppF", 0), C(readahead, "fll", 0),
    C(sync_file_range, "fllF", 0), C(fadvise64, "flli", 0), C(capget, "pp", 0),
    C(getgroups, "ip", 0), C(getxattr, "Pppl", 0), C(fgetxattr, "fppl", 0),
    C(fsetxattr, "fpplF", 0), C(flistxattr, "fpl", 0), C(fremovexattr, "fp", 0),
};
#define NCALLS (sizeof calls / sizeof *calls)

static uint64_t gen_arg(char k) {
    switch (k) {
    case 'f': return (uint64_t)(int64_t)any_fd();
    case 'p': return any_ptr();
    case 'l': return any_len();
    case 'F': return any_flags();
    case 'i': return any_int();
    case 'P': return any_path();
    case 's': return (uint64_t)any_sig();
    case 'm': return (uint64_t)getpid();
    default: return 0;
    }
}

/* Calls that block for good on most arguments. Their timeouts come from
 * the pool too, so cap them: a hang must be the kernel's, not a wait. */
static void tame(long nr, uint64_t a[6]) {
    static struct timespec tiny = {0, 1000000};
    if (nr == SYS_futex)
        a[3] = (uint64_t)&tiny;
    if (nr == SYS_ppoll)
        a[2] = (uint64_t)&tiny;
    if (nr == SYS_epoll_pwait)
        a[3] = 0;
    if (nr == SYS_poll)
        a[2] = 0;
    if (nr == SYS_wait4)
        a[2] |= WNOHANG;
    if (nr == SYS_msgrcv)
        a[4] |= 04000; /* IPC_NOWAIT */
    /* semop, flock and the reads are bounded otherwise: the child's alarm,
     * and the fds of the pool are non-blocking. */
}

static void set_nonblock_all(void) {
    for (int i = 0; i < nfds; i++)
        fcntl(fds[i], F_SETFL, fcntl(fds[i], F_GETFL) | O_NONBLOCK);
}

/* The fds a program starts with: one of every kind the table can use. */
static void open_pool(void) {
    int p[2], s[2];
    keep_fd(open("f0", O_RDWR | O_CREAT, 0600));
    keep_fd(open(".", O_RDONLY | O_DIRECTORY));
    keep_fd(open("/dev/null", O_RDWR));
    keep_fd(open("/dev/zero", O_RDONLY));
    keep_fd(open("/dev/urandom", O_RDONLY));
    keep_fd(open("/proc/self/maps", O_RDONLY));
    keep_fd(open("/proc/self/status", O_RDONLY));
    if (pipe2(p, O_NONBLOCK) == 0) { keep_fd(p[0]); keep_fd(p[1]); }
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, s) == 0) { keep_fd(s[0]); keep_fd(s[1]); }
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0, s) == 0) { keep_fd(s[0]); keep_fd(s[1]); }
    keep_fd(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
    keep_fd(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    keep_fd(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    keep_fd(socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK, 0));
    keep_fd(syscall(SYS_eventfd2, 0, O_NONBLOCK));
    keep_fd(syscall(SYS_epoll_create1, 0));
    keep_fd(syscall(SYS_memfd_create, "fz", 0));
    keep_fd(syscall(SYS_timerfd_create, CLOCK_MONOTONIC, O_NONBLOCK));
    keep_fd(syscall(SYS_inotify_init1, O_NONBLOCK));
}

static void make_maps(void) {
    buf = mmap(NULL, BUF_PAGES * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ro = mmap(NULL, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    edge = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED || ro == MAP_FAILED || edge == MAP_FAILED)
        _exit(3);
    mprotect(edge + PAGE, PAGE, PROT_NONE);
}

/* One program: CALLS calls, in the child, from `seed`. */
static void run_program(uint64_t seed, int ncalls) {
    rng = seed;
    setpgid(0, 0);
    /* Bounds a user program has anyway; keep one program from filling the
     * machine, which would test the OOM path rather than the call. */
    struct rlimit as = {512UL << 20, 512UL << 20};
    setrlimit(RLIMIT_AS, &as);
    struct rlimit np = {64, 64};
    setrlimit(RLIMIT_NPROC, &np);
    struct rlimit fsz = {16UL << 20, 16UL << 20};
    setrlimit(RLIMIT_FSIZE, &fsz);
    signal(SIGPIPE, SIG_IGN);
    signal(SIGXFSZ, SIG_IGN);
    alarm(4);
    make_maps();
    mkdir("d", 0700);
    open_pool();
    set_nonblock_all();
    for (int i = 0; i < ncalls; i++) {
        const struct call *c = &calls[below(NCALLS)];
        uint64_t a[6] = {0};
        scribble();
        for (int j = 0; j < 6 && c->args[j]; j++)
            a[j] = gen_arg(c->args[j]);
        tame(c->nr, a);
        if (c->nr == SYS_munmap || c->nr == SYS_mremap || c->nr == SYS_mprotect ||
            c->nr == SYS_madvise || c->nr == SYS_brk) {
            /* Never take away the pages the generator itself lives on. */
            uint64_t lo = a[0], hi = a[0] + a[1];
            if ((lo < (uint64_t)buf + BUF_PAGES * PAGE && hi > (uint64_t)buf) ||
                (lo < (uint64_t)ro + PAGE && hi > (uint64_t)ro) ||
                (lo < (uint64_t)edge + 2 * PAGE && hi > (uint64_t)edge) || hi < lo)
                continue;
        }
        if (c->nr == SYS_close && (a[0] <= 2 || a[0] == LOGFD))
            continue;
        if (c->nr == SYS_dup3 && (a[1] <= 2 || a[1] == LOGFD))
            continue;
        if (verbose) {
            dprintf(LOGFD, "FZ-CALL %d %s(%#lx, %#lx, %#lx, %#lx, %#lx, %#lx)\n", i, c->name,
                    (long)a[0], (long)a[1], (long)a[2], (long)a[3], (long)a[4], (long)a[5]);
        }
        long r = syscall(c->nr, a[0], a[1], a[2], a[3], a[4], a[5]);
        if (verbose)
            dprintf(LOGFD, "FZ-RET %ld errno=%d\n", r, r < 0 ? errno : 0);
        if (c->keeps_fd && r >= 0)
            keep_fd(r);
        if (c->nr == SYS_pipe2 && r == 0 && a[0] == (uint64_t)buf) {
            keep_fd(((int *)buf)[0]);
            keep_fd(((int *)buf)[1]);
        }
    }
    _exit(0);
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* Run the program for `seed` in a child; 1 if it outlived its deadline. */
static int one(uint64_t seed, int ncalls) {
    pid_t pid = fork();
    if (pid < 0)
        return 0;
    if (pid == 0) {
        if (chdir("/tmp/fz") != 0)
            _exit(2);
        if (verbose)
            dup2(2, LOGFD);
        int null = open("/dev/null", O_RDWR);
        if (null >= 0) {
            dup2(null, 0);
            dup2(null, 1);
            dup2(null, 2);
            if (null > 2)
                close(null);
        }
        run_program(seed, ncalls);
    }
    double end = now() + 8;
    for (;;) {
        int st;
        pid_t got = waitpid(pid, &st, WNOHANG);
        if (got == pid)
            break;
        if (now() > end) {
            printf("FZ-HANG %#llx\n", (unsigned long long)seed);
            fflush(stdout);
            kill(-pid, SIGKILL);
            kill(pid, SIGKILL);
            waitpid(pid, &st, 0);
            return 1;
        }
        struct timespec ts = {0, 5000000};
        nanosleep(&ts, NULL);
    }
    /* Whatever the program's own children left behind. */
    kill(-pid, SIGKILL);
    while (waitpid(-1, NULL, WNOHANG) > 0)
        ;
    return 0;
}

int main(int argc, char **argv) {
    uint64_t seed = (uint64_t)time(NULL), replay = 0;
    long programs = 1000000, seconds = 300, ncalls = 48;
    int have_replay = 0, opt;
    while ((opt = getopt(argc, argv, "s:n:t:c:r:v")) != -1) {
        switch (opt) {
        case 's': seed = strtoull(optarg, NULL, 0); break;
        case 'n': programs = strtol(optarg, NULL, 0); break;
        case 't': seconds = strtol(optarg, NULL, 0); break;
        case 'c': ncalls = strtol(optarg, NULL, 0); break;
        case 'r': replay = strtoull(optarg, NULL, 0); have_replay = 1; break;
        case 'v': verbose = 1; break;
        default:
            fprintf(stderr, "usage: %s [-s SEED] [-n PROGRAMS] [-t SECONDS] [-c CALLS] | -r SEED [-v]\n", argv[0]);
            return 2;
        }
    }
    mkdir("/tmp/fz", 0700);
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (have_replay) {
        printf("FZ-PROG 0 %#llx\n", (unsigned long long)replay);
        int h = one(replay, (int)ncalls);
        printf("FZ-DONE progs=1 hangs=%d\n", h);
        return 0;
    }
    printf("FZ-START seed=%#llx calls=%ld table=%zu\n", (unsigned long long)seed, ncalls, NCALLS);
    double end = now() + seconds;
    long n = 0, hangs = 0;
    for (; n < programs && now() < end; n++) {
        rng = seed + (uint64_t)n * 0x632be59bd9b4e019ULL;
        uint64_t s = rnd();
        printf("FZ-PROG %ld %#llx\n", n, (unsigned long long)s);
        hangs += one(s, (int)ncalls);
    }
    printf("FZ-DONE progs=%ld hangs=%ld\n", n, hangs);
    return 0;
}
