// abiprobe: the ABI corners a Chromium/Electron main process touches before it
// prints anything. Each check says PASS/FAIL with the errno, so a run in the
// guest is a table rather than a crash.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/utsname.h>
#include <linux/futex.h>
#include <linux/memfd.h>
#include <sys/socket.h>
#include <sys/eventfd.h>
#include <sys/epoll.h>
#include <poll.h>
#include <time.h>
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#define F_SEAL_SEAL 0x0001
#define F_SEAL_SHRINK 0x0002
#define F_SEAL_GROW 0x0004
#define F_SEAL_WRITE 0x0008
#endif

static int fails;
static void ok(const char *what, int good, const char *detail) {
    printf("%-44s %s%s%s\n", what, good ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
    fflush(stdout);
    if (!good) fails++;
}
static const char *E(void) { return strerror(errno); }

/* V8's sandbox: reserve `size` of address space aligned to `size`, by mapping
   size+alignment PROT_NONE and trimming both ends. This is the single biggest
   allocation Electron makes, and it happens before anything is logged. */
static void probe_v8_cage(size_t size) {
    char name[80];
    snprintf(name, sizeof name, "v8 cage: %zu GiB reservation", size >> 30);
    size_t req = size + size;
    void *p = mmap(NULL, req, PROT_NONE, MAP_ANONYMOUS | MAP_PRIVATE | MAP_NORESERVE, -1, 0);
    if (p == MAP_FAILED) { ok(name, 0, E()); return; }
    uintptr_t base = ((uintptr_t)p + size - 1) & ~(uintptr_t)(size - 1);
    if (base > (uintptr_t)p) munmap(p, base - (uintptr_t)p);
    uintptr_t tail = base + size;
    if ((uintptr_t)p + req > tail) munmap((void *)tail, (uintptr_t)p + req - tail);
    // Now make one page of it readable+writable and touch it, like V8 does.
    if (mprotect((void *)base, 4096, PROT_READ | PROT_WRITE) != 0) {
        ok(name, 0, "mprotect inside the reservation failed"); return;
    }
    *(volatile char *)base = 42;
    if (madvise((void *)base, 4096, MADV_DONTNEED) != 0) { ok(name, 0, "madvise DONTNEED failed"); return; }
    munmap((void *)base, size);
    ok(name, 1, "reserved, aligned, trimmed, mprotected, touched");
}

static void *thread_body(void *arg) { (void)arg; sched_yield(); return NULL; }

int main(void) {
    struct utsname u;
    if (uname(&u) == 0) printf("# %s %s %s\n", u.sysname, u.release, u.machine);
    printf("# pid=%d uid=%d\n\n", getpid(), getuid());

    /* 1. The address-space reservations, from the one V8 asks for first down. */
    probe_v8_cage((size_t)1 << 40);   /* 1 TiB: Linux grants this */
    probe_v8_cage((size_t)32 << 30);  /* 32 GiB: V8's minimum sandbox */
    probe_v8_cage((size_t)4 << 30);   /* 4 GiB: the pointer-compression cage */

    /* 2. A PROT_NONE reservation that then grows a writable window, the way a
          JIT does; and /proc/self/maps read while it exists. */
    {
        size_t len = (size_t)1 << 30;
        void *p = mmap(NULL, len, PROT_NONE, MAP_ANONYMOUS | MAP_PRIVATE | MAP_NORESERVE, -1, 0);
        ok("1 GiB PROT_NONE reservation", p != MAP_FAILED, p == MAP_FAILED ? E() : "");
        if (p != MAP_FAILED) {
            int good = mprotect((char *)p + (512u << 20), 1 << 20, PROT_READ | PROT_WRITE) == 0;
            if (good) memset((char *)p + (512u << 20), 7, 1 << 20);
            ok("  mprotect RW in its middle, then write", good, good ? "" : E());
            int fd = open("/proc/self/maps", O_RDONLY);
            char buf[65536]; ssize_t n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
            if (fd >= 0) close(fd);
            char d[64]; snprintf(d, sizeof d, "%zd bytes", n);
            ok("  /proc/self/maps reads back", n > 0, d);
            munmap(p, len);
        }
    }

    /* 3. MAP_FIXED over a live mapping (the loader and the zygote both do it). */
    {
        size_t len = 4 * 4096;
        char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
        if (p == MAP_FAILED) ok("MAP_FIXED over a live mapping", 0, E());
        else {
            memset(p, 1, len);
            void *q = mmap(p + 4096, 4096, PROT_READ | PROT_WRITE,
                           MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED, -1, 0);
            int good = q == p + 4096 && *(p + 4096) == 0 && *(p + 8192) == 1;
            ok("MAP_FIXED over a live mapping", good, good ? "replaced one page, kept the rest" : E());
            munmap(p, len);
        }
    }

    /* 4. A file mapped past the end of its last page: Chromium maps icudtl.dat
          and the V8 snapshot this way and reads to the end of the page. */
    {
        const char *path = "/tmp/abiprobe.dat";
        int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
        int good = 0; char detail[96] = "";
        if (fd >= 0) {
            char data[100]; memset(data, 0xAB, sizeof data);
            if (write(fd, data, sizeof data) == (ssize_t)sizeof data) {
                char *m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
                if (m == MAP_FAILED) snprintf(detail, sizeof detail, "mmap: %s", E());
                else {
                    good = m[0] == (char)0xAB && m[99] == (char)0xAB && m[100] == 0 && m[4095] == 0;
                    if (!good) snprintf(detail, sizeof detail,
                                        "tail of the page is not zero-filled");
                    munmap(m, 4096);
                }
            }
            close(fd); unlink(path);
        } else snprintf(detail, sizeof detail, "open: %s", E());
        ok("100-byte file mapped as a whole page", good, detail);
    }


    /* 4b. The resource-bundle pattern: a multi-megabyte file mapped MAP_SHARED
           at its exact byte length (not page-rounded) and read end to end.
           Chromium maps every .pak and icudtl.dat exactly like this, right
           after the root check -- and dereferences the result without looking. */
    {
        const char *path = "/tmp/abiprobe.pak";
        const size_t len = 3 * 1024 * 1024 + 1234;
        int good = 0; char detail[120] = "";
        int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
        if (fd < 0) snprintf(detail, sizeof detail, "open: %s", E());
        else {
            unsigned char *src = malloc(len);
            for (size_t i = 0; i < len; i++) src[i] = (unsigned char)(i * 31 + (i >> 13));
            size_t off = 0;
            while (off < len) {
                ssize_t w = write(fd, src + off, len - off);
                if (w <= 0) break;
                off += (size_t)w;
            }
            if (off != len) snprintf(detail, sizeof detail, "short write: %zu of %zu", off, len);
            else {
                unsigned char *m = mmap(NULL, len, PROT_READ, MAP_SHARED, fd, 0);
                if (m == MAP_FAILED) snprintf(detail, sizeof detail, "MAP_SHARED mmap: %s", E());
                else {
                    size_t bad = (size_t)-1;
                    for (size_t i = 0; i < len; i++)
                        if (m[i] != src[i]) { bad = i; break; }
                    good = bad == (size_t)-1;
                    if (!good) snprintf(detail, sizeof detail,
                        "first wrong byte at %zu (page %zu): got %#x want %#x",
                        bad, bad / 4096, m[bad], src[bad]);
                    munmap(m, len);
                }
            }
            free(src); close(fd); unlink(path);
        }
        ok("3 MiB file, MAP_SHARED, read end to end", good, detail);
    }

    /* 5. The probes Chromium's sandbox makes even with --no-sandbox. */
    {
        int r = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        ok("prctl(PR_SET_NO_NEW_PRIVS)", r == 0, r == 0 ? "" : E());
        /* SECCOMP_SET_MODE_FILTER with a null program: Linux answers EFAULT,
           a kernel without seccomp answers ENOSYS. Never mode 0 (STRICT),
           which SUCCEEDS and then kills the process on its next syscall. */
        errno = 0;
        long s = syscall(SYS_seccomp, 1 /*SET_MODE_FILTER*/, 0, NULL);
        char d[96]; snprintf(d, sizeof d, "returned %ld, errno %d (%s)", s, errno, E());
        ok("seccomp() refuses rather than succeeding", s != 0, d);
        struct stat st;
        int r2 = stat("/proc/self/task", &st);
        char d2[80]; snprintf(d2, sizeof d2, "st_nlink=%lu (3 means single-threaded)", (unsigned long)st.st_nlink);
        ok("stat(/proc/self/task)", r2 == 0, r2 == 0 ? d2 : E());
    }

    /* 6. membarrier, which V8 and base/ use for their sequence locks. */
    {
        long q = syscall(SYS_membarrier, 0 /*CMD_QUERY*/, 0, 0);
        char d[80]; snprintf(d, sizeof d, "mask=%#lx", q < 0 ? 0UL : (unsigned long)q);
        ok("membarrier(QUERY)", q >= 0, q >= 0 ? d : E());
    }

    /* 7. Threads, affinity and the thread-pool sizing Chromium does first. */
    {
        cpu_set_t set; CPU_ZERO(&set);
        int r = sched_getaffinity(0, sizeof set, &set);
        int n = r == 0 ? CPU_COUNT(&set) : -1;
        char d[64]; snprintf(d, sizeof d, "%d cpu(s); sysconf says %ld", n, sysconf(_SC_NPROCESSORS_ONLN));
        ok("sched_getaffinity reports at least one cpu", n > 0, r == 0 ? d : E());
        pthread_t t[8]; int made = 0;
        for (int i = 0; i < 8; i++) if (pthread_create(&t[i], NULL, thread_body, NULL) == 0) made++;
        for (int i = 0; i < made; i++) pthread_join(t[i], NULL);
        char d2[48]; snprintf(d2, sizeof d2, "%d of 8", made);
        ok("8 pthreads start and join", made == 8, d2);
    }

    /* 8. fork() from a process that has had threads -- the zygote's first act. */
    {
        pid_t p = fork();
        if (p == 0) { _exit(77); }
        int st = 0, good = 0;
        if (p > 0 && waitpid(p, &st, 0) == p) good = WIFEXITED(st) && WEXITSTATUS(st) == 77;
        ok("fork() after threads, child exits 77", good, good ? "" : E());
    }

    /* 9. getrandom, which Chromium CHECKs on. */
    {
        unsigned char b[32];
        long r = syscall(SYS_getrandom, b, sizeof b, 0);
        char d[48]; snprintf(d, sizeof d, "%ld bytes", r);
        ok("getrandom(32)", r == (long)sizeof b, r == (long)sizeof b ? d : E());
    }


    /* 10. Chromium's shared memory: memfd_create(MFD_ALLOW_SEALING), grow it,
           seal it, map it read-write and then read-only. base/ CHECKs on most
           of these steps, and what it does with a region it failed to create
           is dereference it. */
    {
        char detail[140] = ""; int good = 0;
        int fd = (int)syscall(SYS_memfd_create, "chromium-shm", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        if (fd < 0) snprintf(detail, sizeof detail, "memfd_create: %s", E());
        else if (ftruncate(fd, 1 << 20) != 0) snprintf(detail, sizeof detail, "ftruncate: %s", E());
        else {
            errno = 0;
            int sealed = fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL);
            if (sealed != 0) snprintf(detail, sizeof detail, "F_ADD_SEALS: %s", E());
            else {
                int seals = fcntl(fd, F_GET_SEALS, 0);
                void *m = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
                if (m == MAP_FAILED) snprintf(detail, sizeof detail, "MAP_SHARED: %s", E());
                else {
                    memset(m, 0x5a, 1 << 20);
                    munmap(m, 1 << 20);
                    void *r = mmap(NULL, 1 << 20, PROT_READ, MAP_SHARED, fd, 0);
                    if (r == MAP_FAILED) snprintf(detail, sizeof detail, "read-only remap: %s", E());
                    else {
                        good = ((unsigned char *)r)[12345] == 0x5a;
                        if (!good) snprintf(detail, sizeof detail, "a shared write was not visible on remap");
                        else snprintf(detail, sizeof detail, "seals=%#x", seals);
                        munmap(r, 1 << 20);
                    }
                }
            }
            close(fd);
        }
        ok("memfd + seals + MAP_SHARED (Chromium shm)", good, detail);
    }

    /* 11. MAP_SHARED anonymous across fork: the zygote's shared counters. */
    {
        int *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_ANONYMOUS | MAP_SHARED, -1, 0);
        int good = 0;
        if (p != MAP_FAILED) {
            *p = 1;
            pid_t c = fork();
            if (c == 0) { *p = 99; _exit(0); }
            int st; waitpid(c, &st, 0);
            good = *p == 99;
            munmap(p, 4096);
        }
        ok("MAP_SHARED|MAP_ANONYMOUS survives fork", good, good ? "" : "the child's write was not visible");
    }

    /* 12. socketpair + SCM_RIGHTS + SO_PASSCRED: how the zygote is spoken to. */
    {
        int sv[2]; char detail[120] = ""; int good = 0;
        if (socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) != 0)
            snprintf(detail, sizeof detail, "socketpair(SOCK_SEQPACKET): %s", E());
        else {
            int on = 1;
            if (setsockopt(sv[0], SOL_SOCKET, SO_PASSCRED, &on, sizeof on) != 0)
                snprintf(detail, sizeof detail, "SO_PASSCRED: %s", E());
            else {
                int payload = open("/dev/null", O_RDONLY);
                char cbuf[CMSG_SPACE(sizeof(int))] = {0};
                char data = 'x';
                struct iovec iov = { &data, 1 };
                struct msghdr msg = {0};
                msg.msg_iov = &iov; msg.msg_iovlen = 1;
                msg.msg_control = cbuf; msg.msg_controllen = sizeof cbuf;
                struct cmsghdr *cm = CMSG_FIRSTHDR(&msg);
                cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS;
                cm->cmsg_len = CMSG_LEN(sizeof(int));
                memcpy(CMSG_DATA(cm), &payload, sizeof(int));
                if (sendmsg(sv[1], &msg, 0) < 0) snprintf(detail, sizeof detail, "sendmsg: %s", E());
                else {
                    char rbuf[8], rc[CMSG_SPACE(sizeof(int)) + 64];
                    struct iovec riov = { rbuf, sizeof rbuf };
                    struct msghdr rmsg = {0};
                    rmsg.msg_iov = &riov; rmsg.msg_iovlen = 1;
                    rmsg.msg_control = rc; rmsg.msg_controllen = sizeof rc;
                    if (recvmsg(sv[0], &rmsg, 0) < 0) snprintf(detail, sizeof detail, "recvmsg: %s", E());
                    else {
                        for (struct cmsghdr *c = CMSG_FIRSTHDR(&rmsg); c; c = CMSG_NXTHDR(&rmsg, c))
                            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) good = 1;
                        if (!good) snprintf(detail, sizeof detail, "no SCM_RIGHTS came back");
                    }
                }
                close(payload);
            }
            close(sv[0]); close(sv[1]);
        }
        ok("socketpair SEQPACKET + SCM_RIGHTS + PASSCRED", good, detail);
    }

    /* 13. The message loop: eventfd woken through epoll, edge-triggered. */
    {
        char detail[120] = ""; int good = 0;
        int ev = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
        int ep = epoll_create1(EPOLL_CLOEXEC);
        if (ev < 0 || ep < 0) snprintf(detail, sizeof detail, "eventfd/epoll_create1: %s", E());
        else {
            struct epoll_event e = { .events = EPOLLIN | EPOLLET, .data.fd = ev };
            if (epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e) != 0) snprintf(detail, sizeof detail, "epoll_ctl: %s", E());
            else {
                uint64_t one = 1;
                if (write(ev, &one, sizeof one) != sizeof one) snprintf(detail, sizeof detail, "write: %s", E());
                else {
                    struct epoll_event out[4];
                    int n = epoll_wait(ep, out, 4, 1000);
                    good = n == 1 && out[0].data.fd == ev;
                    if (!good) snprintf(detail, sizeof detail, "epoll_wait returned %d", n);
                }
            }
        }
        if (ev >= 0) close(ev);
        if (ep >= 0) close(ep);
        ok("eventfd + epoll edge-triggered wakeup", good, detail);
    }

    /* 14. The clocks base::TimeTicks is built on. */
    {
        static const struct { int id; const char *name; } cs[] = {
            { CLOCK_MONOTONIC, "MONOTONIC" },
            { CLOCK_MONOTONIC_COARSE, "MONOTONIC_COARSE" },
            { CLOCK_THREAD_CPUTIME_ID, "THREAD_CPUTIME_ID" },
            { CLOCK_PROCESS_CPUTIME_ID, "PROCESS_CPUTIME_ID" },
            { CLOCK_BOOTTIME, "BOOTTIME" },
        };
        for (unsigned i = 0; i < sizeof cs / sizeof *cs; i++) {
            struct timespec ts;
            int r = clock_gettime(cs[i].id, &ts);
            char n[64]; snprintf(n, sizeof n, "clock_gettime(CLOCK_%s)", cs[i].name);
            ok(n, r == 0, r == 0 ? "" : E());
        }
    }

    /* 15. A SIGSEGV handler on an alternate stack: Chromium installs one
           before anything else, and V8 relies on it for its trap handlers. */
    {
        ok("/proc/self/exe readlinks", readlink("/proc/self/exe", (char[256]){0}, 255) > 0, E());
        stack_t ss = { .ss_sp = malloc(SIGSTKSZ), .ss_size = SIGSTKSZ, .ss_flags = 0 };
        int r = sigaltstack(&ss, NULL);
        ok("sigaltstack", r == 0, r == 0 ? "" : E());
        struct sigaction sa = {0};
        sa.sa_handler = SIG_DFL; sa.sa_flags = SA_ONSTACK | SA_SIGINFO;
        ok("sigaction(SIGSEGV, SA_ONSTACK|SA_SIGINFO)", sigaction(SIGSEGV, &sa, NULL) == 0, E());
    }

    printf("\n%s: %d check(s) failed\n", fails ? "ABIPROBE FAILED" : "ABIPROBE OK", fails);
    return fails != 0;
}
