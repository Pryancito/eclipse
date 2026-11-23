/* oomstress: memory-pressure stress tests for the Eclipse OS kernel.
 *
 * Each case pushes one kind of kernel or user memory until the kernel says no
 * (an errno, a refused allocation, an OOM kill) and then lets it all go. A
 * case passes when the kernel refused cleanly and the machine is still there
 * afterwards; a kernel panic, an oops, a hang or memory that never came back
 * is what the cases exist to find. The cases themselves cannot see a panic --
 * the driver (scripts/qemu-oom-stress.py) watches the serial console for that
 * and checks /proc/meminfo before and after.
 *
 *   gcc -static -O2 -o oomstress tools/oom-stress/oomstress.c -lpthread
 *   oomstress list            the cases
 *   oomstress CASE [ARG]      one case, in a child, with a deadline
 *   oomstress all             every case in turn
 *
 * Output: one "OOMS-BEGIN case" line, progress lines, and one
 * "OOMS-END case status=pass|fail|... detail" line per case. Test tool, so C;
 * nothing in it ships in the image.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/resource.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>

#define MIB (1024UL * 1024UL)
#define PAGE 4096UL

static long arg_n; /* the optional numeric argument of a case */

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    char buf[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    /* One write per line: several processes print at once. */
    size_t n = strlen(buf);
    buf[n++] = '\n';
    (void)!write(1, buf, n);
}

/* Free memory as /proc/meminfo has it, in KiB (MemAvailable, else MemFree). */
static long meminfo(const char *key) {
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f) return -1;
    char line[256];
    long v = -1;
    size_t kl = strlen(key);
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, key, kl) && line[kl] == ':') { v = atol(line + kl + 1); break; }
    fclose(f);
    return v;
}

static void raise_nofile(void) {
    struct rlimit r;
    /* As root, as far as fs.nr_open allows: the default hard limit (4096)
     * stops the fd cases long before memory does. */
    r.rlim_cur = r.rlim_max = 1 << 20;
    if (setrlimit(RLIMIT_NOFILE, &r) && getrlimit(RLIMIT_NOFILE, &r) == 0) {
        r.rlim_cur = r.rlim_max;
        setrlimit(RLIMIT_NOFILE, &r);
    }
}

static int set_adj(pid_t pid, int adj) {
    char path[64], val[16];
    snprintf(path, sizeof path, "/proc/%d/oom_score_adj", pid ? pid : getpid());
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    int n = snprintf(val, sizeof val, "%d", adj);
    int ok = write(fd, val, n) == n ? 0 : -1;
    close(fd);
    return ok;
}

static long read_long_file(const char *path) {
    char b[64] = {0};
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    (void)!read(fd, b, sizeof b - 1);
    close(fd);
    return atol(b);
}

/* ------------------------------------------------------------------ user memory */

/* Touch memory 1 MiB at a time until mmap refuses or we are killed. Up to
 * `limit` MiB (0: no limit). Answers MiB held. Never returns if killed. */
static long hog(long limit, int report) {
    long held = 0;
    for (;;) {
        if (limit && held >= limit) break;
        char *p = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) {
            if (report) say("  anon: mmap refused at %ld MiB: %s", held, strerror(errno));
            break;
        }
        for (unsigned long o = 0; o < MIB; o += PAGE) p[o] = (char)(o >> 12) | 1;
        held++;
        if (report && held % 128 == 0) say("  anon: %ld MiB touched", held);
    }
    return held;
}

/* Touch anonymous memory without bound. Linux answers with the OOM killer (the
 * hog dies by SIGKILL) or with ENOMEM from mmap; either is a pass, as long as
 * the kernel lives and the memory comes back. */
static int case_anon(void) {
    pid_t c = fork();
    if (c == 0) { hog(arg_n, 1); _exit(0); }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  anon: hog died by signal %d", WTERMSIG(st));
    else say("  anon: hog exited %d", WEXITSTATUS(st));
    /* SIGKILL is the OOM killer; SIGSEGV/SIGBUS is a fault that failed for
     * lack of memory, which Linux never answers a plain touch with. */
    return WIFSIGNALED(st) && WTERMSIG(st) != SIGKILL;
}

/* The OOM killer's choice, from userspace: three processes and the killer
 * must take them in the order oom_score_adj and size say.
 *   sacrificial: 16 MiB, adj +1000  -> must die first
 *   protected:   64 MiB, adj -1000  -> must never die
 *   hog:         grows forever, adj 0 -> must die once sacrificial is gone
 * and this process (the parent) must survive too, at adj -1000.
 * A kernel without an OOM killer for user memory fails this with the hog
 * getting ENOMEM, which is reported as such (Linux would kill). */
static int case_policy(void) {
    set_adj(0, -1000);
    int p[2];
    if (pipe(p)) return 1;
    pid_t sac = fork();
    if (sac == 0) {
        set_adj(0, 1000);
        hog(16, 0);
        (void)!write(p[1], "s", 1);
        for (;;) pause();
    }
    pid_t prot = fork();
    if (prot == 0) {
        if (set_adj(0, -1000)) say("  policy: protected could not set -1000: %s", strerror(errno));
        hog(64, 0);
        (void)!write(p[1], "p", 1);
        for (;;) pause();
    }
    char b;
    (void)!read(p[0], &b, 1);
    (void)!read(p[0], &b, 1);
    say("  policy: sacrificial pid %d (adj 1000), protected pid %d (adj -1000) ready", sac, prot);
    double t0 = now();
    pid_t hg = fork();
    if (hg == 0) {
        set_adj(0, 0);
        long h = hog(0, 1);
        /* mmap refused: no OOM kill. Hold it so the killer could still act. */
        say("  policy: hog got ENOMEM at %ld MiB instead of an OOM kill", h);
        sleep(5);
        _exit(3);
    }
    int sac_dead = 0, hog_dead = 0, order_ok = 1, hog_enomem = 0;
    while (!(sac_dead && hog_dead)) {
        int st;
        pid_t w = waitpid(-1, &st, 0);
        if (w < 0) break;
        const char *who = w == sac ? "sacrificial" : w == prot ? "PROTECTED" : w == hg ? "hog" : "?";
        if (WIFSIGNALED(st))
            say("  policy: %s (pid %d) killed by signal %d after %.1f s", who, w, WTERMSIG(st), now() - t0);
        else
            say("  policy: %s (pid %d) exited %d after %.1f s", who, w, WEXITSTATUS(st), now() - t0);
        if (w == prot) { order_ok = 0; break; }
        if (w == sac) sac_dead = 1;
        if (w == hg) {
            hog_dead = 1;
            if (!sac_dead) order_ok = 0;
            if (WIFSIGNALED(st) && WTERMSIG(st) != SIGKILL) order_ok = 0;
            if (WIFEXITED(st) && WEXITSTATUS(st) == 3) hog_enomem = 1;
            if (!sac_dead) break;
        }
    }
    int prot_alive = kill(prot, 0) == 0;
    kill(sac, SIGKILL);
    kill(prot, SIGKILL);
    kill(hg, SIGKILL);
    while (waitpid(-1, NULL, 0) > 0) {}
    say("  policy: order %s, protected %s, hog %s", order_ok ? "ok" : "WRONG",
        prot_alive ? "alive" : "DEAD", hog_enomem ? "saw ENOMEM (no kill)" : "was killed");
    return !(order_ok && prot_alive) ? 1 : hog_enomem ? 2 : 0;
}

/* Many processes each holding a little, so that no single one is a clear
 * victim; then a hog. The killer must pick the hog, not one of the many. */
static int case_many_small(void) {
    int n = arg_n ? arg_n : 64;
    pid_t *kids = calloc(n, sizeof *kids);
    for (int i = 0; i < n; i++) {
        kids[i] = fork();
        if (kids[i] == 0) { hog(4, 0); for (;;) pause(); }
        if (kids[i] < 0) { say("  many_small: fork %d: %s", i, strerror(errno)); n = i; break; }
    }
    sleep(1);
    pid_t hg = fork();
    if (hg == 0) { long h = hog(0, 1); say("  many_small: hog ENOMEM at %ld MiB", h); sleep(3); _exit(3); }
    int st, small_lost = 0, rc = 0;
    for (;;) {
        pid_t w = waitpid(-1, &st, 0);
        if (w < 0) break;
        if (w == hg) {
            say("  many_small: hog %s (signal %d)", WIFSIGNALED(st) ? "killed" : "exited (ENOMEM)",
                WIFSIGNALED(st) ? WTERMSIG(st) : 0);
            if (!WIFSIGNALED(st)) rc = 2;
            else if (WTERMSIG(st) != SIGKILL) rc = 1;
            break;
        }
        small_lost++;
        say("  many_small: small pid %d died first (signal %d)", w, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    }
    for (int i = 0; i < n; i++) kill(kids[i], SIGKILL);
    while (waitpid(-1, NULL, 0) > 0) {}
    say("  many_small: %d small processes lost before the hog", small_lost);
    free(kids);
    return small_lost ? 1 : rc;
}

/* ------------------------------------------------------------------ processes */

/* Flat fork until fork refuses (or N), every child parked; then kill them all.
 * Each process costs the kernel a task, page tables and a coroutine stack. */
static int case_fork_flat(void) {
    long max = arg_n ? arg_n : 100000;
    pid_t *kids = calloc(max, sizeof *kids);
    long n = 0;
    double t0 = now();
    int err = 0;
    while (n < max) {
        pid_t c = fork();
        if (c < 0) { err = errno; break; }
        if (c == 0) { for (;;) pause(); }
        kids[n++] = c;
        if (n % 500 == 0) say("  fork_flat: %ld children, %ld KiB free", n, meminfo("MemFree"));
    }
    say("  fork_flat: %ld children in %.1f s, stopped by %s", n, now() - t0, err ? strerror(err) : "the cap");
    for (long i = 0; i < n; i++) kill(kids[i], SIGKILL);
    long reaped = 0;
    while (waitpid(-1, NULL, 0) > 0) reaped++;
    say("  fork_flat: reaped %ld in %.1f s", reaped, now() - t0);
    free(kids);
    return reaped == n ? 0 : 1;
}

/* A real fork bomb: every process forks twice and the tree grows until fork
 * refuses. Bounded in time: everyone in the tree dies at the deadline (their
 * own alarm), and the leader kills the process group to be sure. */
static int case_fork_bomb(void) {
    int secs = arg_n ? arg_n : 20;
    pid_t leader = fork();
    if (leader == 0) {
        setpgid(0, 0);
        alarm(secs);
        for (;;) {
            if (fork() < 0) { usleep(1000); }
        }
    }
    setpgid(leader, leader);
    sleep(secs / 2);
    say("  fork_bomb: mid-bomb, %ld KiB free, loadavg %ld", meminfo("MemFree"), read_long_file("/proc/loadavg"));
    sleep(secs / 2 + 2);
    kill(-leader, SIGKILL);
    double t0 = now();
    while (waitpid(-1, NULL, 0) > 0) {}
    /* The grandchildren are reparented to init; wait for the group to empty. */
    int left = 1, tries = 0;
    while (left && tries++ < 300) {
        left = kill(-leader, 0) == 0;
        if (left) { kill(-leader, SIGKILL); usleep(100000); }
    }
    say("  fork_bomb: group %s %.1f s after the kill", left ? "STILL ALIVE" : "gone", now() - t0);
    return left;
}

static void *parked(void *a) { (void)a; for (;;) pause(); return NULL; }

/* pthread_create until it refuses; each thread is a kernel task with its own
 * kernel stack. Then the process exits with them all. */
static int case_threads(void) {
    pid_t c = fork();
    if (c == 0) {
        long max = arg_n ? arg_n : 100000, n = 0;
        pthread_attr_t at;
        pthread_attr_init(&at);
        pthread_attr_setstacksize(&at, 64 * 1024);
        double t0 = now();
        int e = 0;
        while (n < max) {
            pthread_t t;
            if ((e = pthread_create(&t, &at, parked, NULL))) break;
            n++;
            if (n % 500 == 0) say("  threads: %ld, %ld KiB free", n, meminfo("MemFree"));
        }
        say("  threads: %ld threads in %.1f s, stopped by %s", n, now() - t0, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    return !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* Threads in many processes at once, created and joined in a loop: churn of
 * kernel stacks rather than a pile of them. */
static void *nothing(void *a) { return a; }
static int case_thread_churn(void) {
    int procs = 8, secs = arg_n ? arg_n : 15;
    for (int i = 0; i < procs; i++) {
        if (fork() == 0) {
            double end = now() + secs;
            long made = 0;
            while (now() < end) {
                pthread_t t[32];
                int k = 0;
                for (; k < 32; k++) if (pthread_create(&t[k], NULL, nothing, NULL)) break;
                for (int j = 0; j < k; j++) pthread_join(t[j], NULL);
                made += k;
            }
            say("  thread_churn[%d]: %ld threads made and joined", i, made);
            _exit(0);
        }
    }
    int bad = 0, st;
    while (wait(&st) > 0) bad += !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    return bad != 0;
}

/* ------------------------------------------------------------------ kernel buffers */

static char junk[65536];

/* Fill `fd` (non-blocking) until it would block. Answers bytes queued. */
static long fill_fd(int fd) {
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    long total = 0;
    for (;;) {
        ssize_t w = write(fd, junk, sizeof junk);
        if (w <= 0) break;
        total += w;
    }
    return total;
}

/* Kernel memory queued in pipes. In a child, so it all goes at exit. */
static int case_pipes(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        long n = 0, bytes = 0, max = arg_n ? arg_n : 1000000;
        int e = 0;
        while (n < max) {
            int p[2];
            if (pipe(p)) { e = errno; break; }
            fcntl(p[1], F_SETPIPE_SZ, 1 << 20);
            long b = fill_fd(p[1]);
            if (b == 0) { e = errno; say("  pipes: pipe %ld took nothing: %s", n, strerror(errno)); close(p[0]); close(p[1]); break; }
            bytes += b;
            n++;
            if (n % 256 == 0) say("  pipes: %ld pipes, %ld MiB queued, %ld KiB free", n, bytes / MIB, meminfo("MemFree"));
        }
        say("  pipes: %ld pipes, %ld MiB queued, stopped by %s", n, bytes / MIB, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  pipes: filler killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* Kernel memory queued in unix socketpairs (both directions). */
static int case_unix(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        long n = 0, bytes = 0, max = arg_n ? arg_n : 1000000;
        int e = 0;
        while (n < max) {
            int s[2];
            if (socketpair(AF_UNIX, n & 1 ? SOCK_DGRAM : SOCK_STREAM, 0, s)) { e = errno; break; }
            long b = fill_fd(s[0]) + fill_fd(s[1]);
            if (b == 0) { e = errno; break; }
            bytes += b;
            n++;
            if (n % 128 == 0) say("  unix: %ld pairs, %ld MiB queued, %ld KiB free", n, bytes / MIB, meminfo("MemFree"));
        }
        say("  unix: %ld pairs, %ld MiB queued, stopped by %s", n, bytes / MIB, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  unix: filler killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* File descriptors in flight: SCM_RIGHTS messages nobody receives. Each one
 * pins open files in the kernel. */
static int case_fdpass(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        int s[2];
        if (socketpair(AF_UNIX, SOCK_DGRAM, 0, s)) _exit(1);
        int one = 1 << 30;
        setsockopt(s[0], SOL_SOCKET, SO_SNDBUF, &one, sizeof one);
        setsockopt(s[1], SOL_SOCKET, SO_RCVBUF, &one, sizeof one);
        fcntl(s[0], F_SETFL, O_NONBLOCK);
        int fds[250];
        int k = 0;
        for (; k < 250; k++) { fds[k] = eventfd(0, 0); if (fds[k] < 0) break; }
        long sent = 0, max = arg_n ? arg_n : 10000000;
        int e = 0;
        char cbuf[CMSG_SPACE(sizeof fds)];
        while (sent < max) {
            char b = 'x';
            struct iovec iov = {&b, 1};
            struct msghdr m = {0};
            m.msg_iov = &iov;
            m.msg_iovlen = 1;
            m.msg_control = cbuf;
            m.msg_controllen = CMSG_SPACE(k * sizeof(int));
            struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
            cm->cmsg_level = SOL_SOCKET;
            cm->cmsg_type = SCM_RIGHTS;
            cm->cmsg_len = CMSG_LEN(k * sizeof(int));
            memcpy(CMSG_DATA(cm), fds, k * sizeof(int));
            if (sendmsg(s[0], &m, 0) < 0) { e = errno; break; }
            sent += k;
            if (sent % 250000 < (unsigned)k) say("  fdpass: %ld fds in flight, %ld KiB free", sent, meminfo("MemFree"));
        }
        say("  fdpass: %ld fds in flight, stopped by %s", sent, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  fdpass: sender killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* TCP over loopback through the network stack process: N connections, each
 * filled until the stack's window says no. Load on a served component. */
static int case_tcp(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        int l = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        if (l < 0 || bind(l, (void *)&a, sizeof a) || listen(l, 4096)) { say("  tcp: listen: %s", strerror(errno)); _exit(1); }
        socklen_t al = sizeof a;
        getsockname(l, (void *)&a, &al);
        long n = 0, bytes = 0, max = arg_n ? arg_n : 2000;
        int e = 0;
        double t0 = now();
        while (n < max) {
            int s = socket(AF_INET, SOCK_STREAM, 0);
            if (s < 0) { e = errno; break; }
            if (connect(s, (void *)&a, sizeof a)) { e = errno; close(s); break; }
            int r = accept(l, NULL, NULL);
            if (r < 0) { e = errno; break; }
            bytes += fill_fd(s);
            n++;
            if (n % 100 == 0) say("  tcp: %ld connections, %ld MiB queued, %.1f s", n, bytes / MIB, now() - t0);
        }
        say("  tcp: %ld connections, %ld MiB queued, stopped by %s", n, bytes / MIB, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    return !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* UDP datagrams to a socket nobody reads, from many sockets. */
static int case_udp(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        int r = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        int big = 1 << 30;
        setsockopt(r, SOL_SOCKET, SO_RCVBUF, &big, sizeof big);
        bind(r, (void *)&a, sizeof a);
        socklen_t al = sizeof a;
        getsockname(r, (void *)&a, &al);
        long socks = 0, sent = 0, max = arg_n ? arg_n : 2000;
        int e = 0;
        while (socks < max) {
            int s = socket(AF_INET, SOCK_DGRAM, 0);
            if (s < 0) { e = errno; break; }
            fcntl(s, F_SETFL, O_NONBLOCK);
            for (int i = 0; i < 64; i++)
                if (sendto(s, junk, 8192, 0, (void *)&a, sizeof a) > 0) sent++;
            socks++;
            if (socks % 200 == 0) say("  udp: %ld sockets, %ld datagrams sent", socks, sent);
        }
        say("  udp: %ld sockets, %ld datagrams, stopped by %s", socks, sent, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    return !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* Kernel objects behind file descriptors: epoll instances with watches,
 * timerfds armed far in the future, eventfds. Until EMFILE/ENOMEM. */
static int case_fdobjs(void) {
    pid_t c = fork();
    if (c == 0) {
        raise_nofile();
        long n = 0, max = arg_n ? arg_n : 2000000;
        int e = 0, ep = -1;
        while (n < max) {
            int fd;
            switch (n % 3) {
            case 0: fd = epoll_create1(0); break;
            case 1: {
                fd = timerfd_create(CLOCK_MONOTONIC, 0);
                struct itimerspec it = {.it_value = {3600, 0}, .it_interval = {1, 0}};
                if (fd >= 0) timerfd_settime(fd, 0, &it, NULL);
                break;
            }
            default: fd = eventfd(0, 0); break;
            }
            if (fd < 0) { e = errno; break; }
            if (n % 3 == 0) {
                if (ep >= 0) {
                    struct epoll_event ev = {.events = EPOLLIN};
                    epoll_ctl(fd, EPOLL_CTL_ADD, ep, &ev);
                }
                ep = fd;
            }
            n++;
            if (n % 50000 == 0) say("  fdobjs: %ld objects, %ld KiB free", n, meminfo("MemFree"));
        }
        say("  fdobjs: %ld objects, stopped by %s", n, e ? strerror(e) : "the cap");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  fdobjs: killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* How long a process takes to die with many fds open, by kind, at two
 * sizes. Exit should be linear in the fds: twice the fds, about twice the
 * time. fdobjs left a process with a million of them that took over nine
 * minutes to exit (QEMU 2 GiB, 2026-11-23). */
struct fdtimes { double open, exit; };
static struct fdtimes exit_with_fds(int kind, long n) {
    int p[2];
    struct fdtimes t = {0, 0};
    if (pipe(p)) return t;
    double t0 = now();
    pid_t c = fork();
    if (c == 0) {
        close(p[0]);
        raise_nofile();
        for (long i = 0; i < n; i++) {
            int fd = kind == 0 ? eventfd(0, 0) : kind == 1 ? timerfd_create(CLOCK_MONOTONIC, 0)
                   : kind == 2 ? epoll_create1(0) : open("/dev/null", O_RDONLY);
            if (fd < 0) { say("  fdexit: kind %d stopped at %ld: %s", kind, i, strerror(errno)); break; }
            if (kind == 1) {
                struct itimerspec it = {.it_value = {3600, 0}};
                timerfd_settime(fd, 0, &it, NULL);
            }
        }
        double opened = now();
        (void)!write(p[1], &opened, sizeof opened);
        _exit(0);
    }
    close(p[1]);
    double opened = t0;
    (void)!read(p[0], &opened, sizeof opened);
    close(p[0]);
    waitpid(c, NULL, 0);
    t.open = opened - t0;
    t.exit = now() - opened;
    return t;
}
static int case_fdexit(void) {
    static const char *names[] = {"eventfd", "timerfd", "epoll", "file"};
    long n = arg_n ? arg_n : 20000;
    int bad = 0;
    for (int k = 0; k < 4; k++) {
        struct fdtimes a = exit_with_fds(k, n), b = exit_with_fds(k, 2 * n);
        say("  fdexit: %-7s %ld fds: open %.2f s exit %.2f s | %ld fds: open %.2f s (x%.1f) exit %.2f s (x%.1f)",
            names[k], n, a.open, a.exit, 2 * n, b.open, b.open / (a.open + 1e-9), b.exit, b.exit / (a.exit + 1e-9));
        if ((b.open > 3.5 * a.open && b.open > 2) || (b.exit > 3.5 * a.exit && b.exit > 2)) bad = 1;
    }
    return bad;
}

/* System V: message queues filled and shared memory segments made until the
 * kernel refuses, then removed (they outlive the process otherwise). */
static int case_sysv(void) {
    long qs = 0, msgs = 0, segs = 0;
    int qid[4096], sid[4096];
    struct { long t; char b[4096]; } m = {1, {0}};
    while (qs < 4096) {
        int q = msgget(IPC_PRIVATE, 0600);
        if (q < 0) break;
        qid[qs++] = q;
        while (msgsnd(q, &m, sizeof m.b, IPC_NOWAIT) == 0) msgs++;
    }
    say("  sysv: %ld queues, %ld messages (%ld MiB): %s", qs, msgs, msgs * 4096 / MIB, strerror(errno));
    /* Segments stay attached and are marked for removal at once, so whatever
     * kills this process (the OOM killer, the deadline) takes them with it:
     * an unattached segment outlives everyone and holds the memory until the
     * next boot. Bounded by what is available, or ARG MiB. */
    long cap = arg_n ? arg_n / 4 : meminfo("MemAvailable") / 4096;
    if (cap > 4096 || cap <= 0) cap = 4096;
    while (segs < cap) {
        int s = shmget(IPC_PRIVATE, 4 * MIB, 0600);
        if (s < 0) break;
        sid[segs++] = s;
        char *p = shmat(s, NULL, 0);
        shmctl(s, IPC_RMID, NULL);
        if (p == (void *)-1) break;
        for (unsigned long o = 0; o < 4 * MIB; o += PAGE) p[o] = 1;
    }
    say("  sysv: %ld shm segments (%ld MiB): %s", segs, segs * 4, strerror(errno));
    for (long i = 0; i < qs; i++) msgctl(qid[i], IPC_RMID, NULL);
    for (long i = 0; i < segs; i++) shmctl(sid[i], IPC_RMID, NULL);
    return 0;
}

/* ------------------------------------------------------------------ address space */

/* As many separate mappings as the kernel allows: alternating protections so
 * neighbours cannot merge. Each is a VMA in the kernel heap. Several rounds,
 * so a leak per mapping shows as less free memory each round. */
static int case_vmas(void) {
    pid_t c = fork();
    if (c == 0) {
        long max = arg_n ? arg_n : 1000000;
        for (int round = 0; round < 3; round++) {
            char *base = mmap(NULL, max * PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
            if (base == MAP_FAILED) { say("  vmas: reserve: %s", strerror(errno)); _exit(1); }
            long n = 0;
            int e = 0;
            for (; n < max; n++) {
                if (mprotect(base + n * PAGE, PAGE, n & 1 ? PROT_READ : PROT_READ | PROT_WRITE)) { e = errno; break; }
                if (!(n & 1)) base[n * PAGE] = 1;
            }
            say("  vmas: round %d: %ld split mappings, stopped by %s, %ld KiB free", round, n,
                e ? strerror(e) : "the cap", meminfo("MemFree"));
            munmap(base, max * PAGE);
            say("  vmas: round %d unmapped, %ld KiB free", round, meminfo("MemFree"));
        }
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  vmas: killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* Threads mapping, touching, protecting and unmapping random sizes at once,
 * with forks in between: the VM's locks and its heap under churn. */
static volatile int churn_stop;
static void *churner(void *a) {
    unsigned s = (unsigned)(uintptr_t)a;
    long ops = 0;
    while (!churn_stop) {
        size_t len = ((rand_r(&s) % 512) + 1) * PAGE;
        char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | (rand_r(&s) & 1 ? MAP_POPULATE : 0), -1, 0);
        if (p == MAP_FAILED) { usleep(1000); continue; }
        for (size_t o = 0; o < len; o += PAGE * (1 + rand_r(&s) % 4)) p[o] = 1;
        if (len > 2 * PAGE) mprotect(p + PAGE, PAGE, PROT_READ);
        if (rand_r(&s) % 4 == 0) madvise(p, len, MADV_DONTNEED);
        munmap(p, len);
        ops++;
    }
    return (void *)ops;
}
static int case_mmap_churn(void) {
    int secs = arg_n ? arg_n : 15;
    pid_t c = fork();
    if (c == 0) {
        pthread_t t[8];
        for (int i = 0; i < 8; i++) pthread_create(&t[i], NULL, churner, (void *)(uintptr_t)(i * 7919 + 1));
        double end = now() + secs;
        long forks = 0;
        while (now() < end) {
            pid_t k = fork();
            if (k == 0) _exit(0);
            if (k > 0) { waitpid(k, NULL, 0); forks++; }
            usleep(20000);
        }
        churn_stop = 1;
        long ops = 0;
        for (int i = 0; i < 8; i++) { void *r; pthread_join(t[i], &r); ops += (long)r; }
        say("  mmap_churn: %ld map/unmap ops, %ld forks in %d s", ops, forks, secs);
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    return !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

/* The page cache side: a memfd grown until the kernel refuses, then a sparse
 * ftruncate far past RAM (must not allocate anything). */
static int case_memfd(void) {
    pid_t c = fork();
    if (c == 0) {
        int fd = syscall(SYS_memfd_create, "oomstress", 0);
        if (fd < 0) { say("  memfd: %s", strerror(errno)); _exit(1); }
        long mb = 0, max = arg_n ? arg_n : 1000000;
        memset(junk, 0x5a, sizeof junk);
        while (mb < max) {
            int ok = 1;
            for (int i = 0; i < 16 && ok; i++) ok = write(fd, junk, sizeof junk) == sizeof junk;
            if (!ok) { say("  memfd: write refused at %ld MiB: %s", mb, strerror(errno)); break; }
            mb++;
            if (mb % 256 == 0) say("  memfd: %ld MiB written, %ld KiB free", mb, meminfo("MemFree"));
        }
        long before = meminfo("MemFree");
        int r = ftruncate(fd, 1L << 40);
        say("  memfd: ftruncate 1 TiB -> %d (%s), free %ld -> %ld KiB", r, r ? strerror(errno) : "ok", before, meminfo("MemFree"));
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    if (WIFSIGNALED(st)) say("  memfd: killed by signal %d", WTERMSIG(st));
    return !(WIFEXITED(st) || (WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL));
}

/* ------------------------------------------------------------------ served drivers under pressure */

/* Readers of a served filesystem while user memory is nearly gone: the FS
 * server, the disk driver and the kernel's hop all need memory to answer, and
 * none of them may be the one the OOM killer takes first or the one that
 * hangs. ARG: the file to read (default /bin/busybox). */
static const char *served_file = "/bin/busybox";
static int case_served(void) {
    /* How much user memory there really is: a probe touches until it dies
     * (or mmap refuses) and reports as it goes. MemAvailable is not the
     * answer -- on Eclipse it counts the kernel heap's free space too. */
    int pp[2];
    if (pipe(pp)) return 1;
    pid_t probe = fork();
    if (probe == 0) {
        close(pp[0]);
        set_adj(0, 1000);
        for (long held = 0;; held++) {
            char *p = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (p == MAP_FAILED) _exit(0);
            for (unsigned long o = 0; o < MIB; o += PAGE) p[o] = 1;
            (void)!write(pp[1], &held, sizeof held);
        }
    }
    close(pp[1]);
    long most = 0, got;
    while (read(pp[0], &got, sizeof got) == sizeof got) most = got + 1;
    close(pp[0]);
    waitpid(probe, NULL, 0);
    sleep(1);
    long keep = most - 48; /* leave ~48 MiB */
    say("  served: a probe reached %ld MiB of user memory", most);
    pid_t hg = fork();
    if (hg == 0) { set_adj(0, 500); hog(keep > 0 ? keep : 1, 0); for (;;) pause(); }
    sleep(2);
    say("  served: hog holds ~%ld MiB, %ld KiB free; 64 readers of %s", keep, meminfo("MemFree"), served_file);
    double t0 = now();
    int readers = 64;
    for (int i = 0; i < readers; i++) {
        if (fork() == 0) {
            char *b = malloc(1 << 16);
            for (int r = 0; r < 4; r++) {
                int fd = open(served_file, O_RDONLY);
                if (fd < 0) _exit(10);
                while (read(fd, b, 1 << 16) > 0) {}
                close(fd);
            }
            _exit(0);
        }
    }
    int ok = 0, bad = 0, killed = 0, st;
    pid_t w;
    while ((w = wait(&st)) > 0) {
        if (w == hg) continue;
        if (WIFEXITED(st) && WEXITSTATUS(st) == 0) ok++;
        else if (WIFSIGNALED(st)) killed++;
        else bad++;
        if (ok + bad + killed == readers) break;
    }
    say("  served: %d readers ok, %d failed, %d killed in %.1f s", ok, bad, killed, now() - t0);
    kill(hg, SIGKILL);
    waitpid(hg, NULL, 0);
    return bad != 0;
}

/* ------------------------------------------------------------------ driver */

struct tcase { const char *name; int (*fn)(void); int deadline; const char *what; };
static const struct tcase cases[] = {
    {"anon", case_anon, 300, "touch anonymous memory until refused or killed"},
    {"policy", case_policy, 300, "OOM killer order: adj +1000 first, -1000 never, then the hog"},
    {"many_small", case_many_small, 300, "OOM killer takes the hog, not one of 64 small"},
    {"fork_flat", case_fork_flat, 600, "fork until refused, then kill all"},
    {"fork_bomb", case_fork_bomb, 300, "exponential fork bomb for ARG seconds"},
    {"threads", case_threads, 600, "pthread_create until refused"},
    {"thread_churn", case_thread_churn, 300, "8 processes creating/joining threads"},
    {"pipes", case_pipes, 600, "pipes filled until refused"},
    {"unix", case_unix, 600, "unix socketpairs filled until refused"},
    {"fdpass", case_fdpass, 600, "SCM_RIGHTS fds in flight until refused"},
    {"tcp", case_tcp, 600, "loopback TCP connections filled (network stack process)"},
    {"udp", case_udp, 300, "UDP datagrams to an unread socket"},
    {"fdobjs", case_fdobjs, 600, "epoll/timerfd/eventfd until refused"},
    {"fdexit", case_fdexit, 600, "open+exit time with N and 2N fds: linear or not"},
    {"sysv", case_sysv, 300, "System V queues and shm until refused"},
    {"vmas", case_vmas, 600, "split mappings until refused, 3 rounds"},
    {"mmap_churn", case_mmap_churn, 300, "8 threads map/unmap + forks"},
    {"memfd", case_memfd, 600, "memfd grown until refused, sparse 1 TiB truncate"},
    {"served", case_served, 300, "64 readers of a served file with memory nearly gone"},
};
#define NCASES (int)(sizeof cases / sizeof cases[0])

static int run_case(const struct tcase *c) {
    say("OOMS-BEGIN %s free=%ld avail=%ld", c->name, meminfo("MemFree"), meminfo("MemAvailable"));
    double t0 = now();
    pid_t k = fork();
    if (k == 0) {
        setpgid(0, 0);
        _exit(c->fn());
    }
    setpgid(k, k);
    int st = 0;
    double end = t0 + c->deadline;
    pid_t w = 0;
    while ((w = waitpid(k, &st, WNOHANG)) == 0 && now() < end) usleep(100000);
    const char *verdict;
    char detail[64] = "";
    if (w == 0) {
        verdict = "hang";
        kill(-k, SIGKILL);
        waitpid(k, &st, 0);
    } else if (WIFSIGNALED(st)) {
        verdict = WTERMSIG(st) == SIGKILL ? "killed" : "crash";
        snprintf(detail, sizeof detail, "signal=%d", WTERMSIG(st));
    } else {
        int rc = WEXITSTATUS(st);
        verdict = rc == 0 ? "pass" : rc == 2 ? "enomem-not-killed" : "fail";
        snprintf(detail, sizeof detail, "rc=%d", rc);
    }
    /* Anything of the case's group still around goes now. */
    kill(-k, SIGKILL);
    while (waitpid(-1, NULL, WNOHANG) > 0) {}
    sleep(2); /* let reclaim give memory back before the reading */
    say("OOMS-END %s status=%s %s secs=%.1f free=%ld avail=%ld", c->name, verdict, detail, now() - t0,
        meminfo("MemFree"), meminfo("MemAvailable"));
    return strcmp(verdict, "pass") != 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    signal(SIGPIPE, SIG_IGN);
    if (argc < 2 || !strcmp(argv[1], "list")) {
        for (int i = 0; i < NCASES; i++) printf("%-14s %s\n", cases[i].name, cases[i].what);
        return argc < 2;
    }
    if (argc > 2) {
        arg_n = atol(argv[2]);
        if (!arg_n) served_file = argv[2];
    }
    int bad = 0, ran = 0;
    for (int i = 0; i < NCASES; i++) {
        if (strcmp(argv[1], "all") && strcmp(argv[1], cases[i].name)) continue;
        bad += run_case(&cases[i]);
        ran++;
    }
    if (!ran) { fprintf(stderr, "oomstress: no case %s (try: oomstress list)\n", argv[1]); return 2; }
    say("OOMS-DONE ran=%d not_pass=%d", ran, bad);
    return bad != 0;
}
