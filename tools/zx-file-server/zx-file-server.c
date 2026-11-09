// zx-file-server: a file system served from userspace, through the syscall
// redirect (eclipse_redirect_syscalls) and the client memory call
// (eclipse_redirect_memory).
//
// The parent is the server: it holds three read-only files in its own
// memory, under /srv. A forked child redirects open, openat, read, lseek
// and close to it. The server answers those that name /srv or one of its own
// descriptors and hands everything else back to the kernel, so the child
// sees one file tree with /srv grafted on, as a Linux personality server
// would graft a file system served by another process.
//
// It checks the served files read back right (whole, in pieces, after a
// seek), that writing is refused and a missing file is ENOENT, that native
// files still work, and then times open+close and seek+read of 64 bytes
// and of a page, on a native file before the redirect and on a served file
// after it. Under QEMU/TCG only the ratios mean much.
//
//   zx-file-server [ROUNDS]  (default 5000)
//
// Run it on Eclipse booted with `zircon.gateway=1`. Exit status: 0 on
// success, 2 when the gateway is off (ENOSYS), 1 on error.
//
// A prototype: one client process, descriptors from SERVED_FD_BASE up that
// the kernel's own could collide with in a process that opens a thousand
// files, and every read or close of a native descriptor pays a round trip
// to the server before the kernel does it.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_channel_create 7

// Eclipse's own gateway calls (loader/src/linux.rs, ECLIPSE_GATEWAY_EXT).
#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_fd_to_handle 0x4001
#define ECLIPSE_SYS_redirect_syscalls 0x4002
#define ECLIPSE_SYS_redirect_memory 0x4003
#define ECLIPSE_SYS_channel_reply_wait 0x4004
#define ECLIPSE_FD_TAKE 1
#define ECLIPSE_MEMORY_TO_CLIENT 1

// linux-object/src/process/syscall_redirect.rs
#define REDIRECT_KIND_SYSCALL 1
#define REDIRECT_REPLY_NATIVE 1

#define ZX_CHANNEL_READABLE (1u << 0)
#define ZX_CHANNEL_PEER_CLOSED (1u << 2)
#define ZX_TIME_INFINITE INT64_MAX
#define ZX_ERR_INTERNAL_INTR_RETRY (-6)
#define ZX_ERR_SHOULD_WAIT (-22)
#define ZX_ERR_PEER_CLOSED (-24)

#define SERVED_FD_BASE 1000
#define MAX_OPEN 64
#define PREFIX "/srv/"

typedef uint32_t zx_handle_t;

struct request {
    uint32_t txid, kind;
    uint64_t pid, tid, nr, args[6];
};

struct reply {
    uint32_t txid, flags;
    int64_t ret;
};

static long zx(long n, long a, long b, long c, long d, long e, long f) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d, e, f);
}

// musl's syscall() turns a negative return into errno for the -4095..-1
// range, which is where every zx_status_t lives: undo that.
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

// ---- the server ------------------------------------------------------------

struct file {
    const char *name; // under PREFIX
    unsigned char *data;
    size_t size;
};

static const char hello[] = "hello from the file server\n";
static unsigned char page[4096], big[65536];
static struct file files[] = {
    {"hello", (unsigned char *)hello, sizeof hello - 1},
    {"page", page, sizeof page},
    {"big", big, sizeof big},
};

struct open_file {
    int used;
    uint64_t pid;
    struct file *file;
    uint64_t off;
};

static struct open_file table[MAX_OPEN];
static zx_handle_t server_handle;
// Written by the server thread, read by main after the join.
static long served, handed_back, bad_requests;

// How the server loop ended: still running, the peer closed (the only good
// end), or an error from the channel.
enum { RUNNING, PEER_CLOSED, FAILED };
static atomic_int server_end = RUNNING;

static long client_memory(uint64_t pid, uint64_t addr, void *buf, long len, int to_client) {
    return zx_status(zx(ECLIPSE_SYS_redirect_memory, server_handle, (long)pid, (long)addr,
                        (long)buf, len, to_client ? ECLIPSE_MEMORY_TO_CLIENT : 0));
}

// The path at `addr` in the client, if it is one of ours: its name under
// PREFIX in `name`. 0 when it is not ours (hand it back), 1 when it is.
static int served_path(uint64_t pid, uint64_t addr, char *name, size_t cap) {
    char path[256];
    long n = client_memory(pid, addr, path, sizeof path - 1, 0);
    if (n <= 0)
        return 0; // the kernel says EFAULT better than we would
    path[n] = 0;
    if (strlen(path) == (size_t)n)
        return 0; // no NUL in reach: not one of our short names
    if (strncmp(path, PREFIX, sizeof PREFIX - 1) != 0)
        return 0;
    snprintf(name, cap, "%s", path + sizeof PREFIX - 1);
    return 1;
}

static struct open_file *served_fd(const struct request *req, uint64_t fd) {
    if (fd < SERVED_FD_BASE || fd >= SERVED_FD_BASE + MAX_OPEN)
        return NULL;
    struct open_file *f = &table[fd - SERVED_FD_BASE];
    return f->used && f->pid == req->pid ? f : NULL;
}

// Answer `req` in `ret`, or return 0 to hand it back to the kernel.
static int serve_open(const struct request *req, uint64_t path, uint64_t flags, int64_t *ret) {
    char name[256];
    if (!served_path(req->pid, path, name, sizeof name))
        return 0;
    if ((flags & O_ACCMODE) != O_RDONLY || (flags & (O_CREAT | O_TRUNC))) {
        *ret = -EROFS;
        return 1;
    }
    struct file *file = NULL;
    for (size_t i = 0; i < sizeof files / sizeof files[0]; i++)
        if (strcmp(files[i].name, name) == 0)
            file = &files[i];
    if (!file) {
        *ret = -ENOENT;
        return 1;
    }
    for (int i = 0; i < MAX_OPEN; i++) {
        if (!table[i].used) {
            table[i] = (struct open_file){1, req->pid, file, 0};
            *ret = SERVED_FD_BASE + i;
            return 1;
        }
    }
    *ret = -EMFILE;
    return 1;
}

static int serve(const struct request *req, int64_t *ret) {
    const uint64_t *a = req->args;
    struct open_file *f;
    switch (req->nr) {
#ifdef SYS_open
    case SYS_open:
        return serve_open(req, a[0], a[1], ret);
#endif
    case SYS_openat:
        // An absolute path ignores the directory, which is all we serve.
        return serve_open(req, a[1], a[2], ret);
    case SYS_read: {
        if (!(f = served_fd(req, a[0])))
            return 0;
        uint64_t left = f->off < f->file->size ? f->file->size - f->off : 0;
        uint64_t n = a[2] < left ? a[2] : left;
        if (n) {
            long put = client_memory(req->pid, a[1], f->file->data + f->off, (long)n, 1);
            if (put <= 0) {
                *ret = -EFAULT;
                return 1;
            }
            n = (uint64_t)put;
        }
        f->off += n;
        *ret = (int64_t)n;
        return 1;
    }
    case SYS_lseek: {
        if (!(f = served_fd(req, a[0])))
            return 0;
        int64_t base = a[2] == SEEK_SET ? 0
                       : a[2] == SEEK_CUR ? (int64_t)f->off
                       : a[2] == SEEK_END ? (int64_t)f->file->size
                                          : -1;
        int64_t delta = (int64_t)a[1];
        if (base < 0 || delta < -base || (delta > 0 && delta > INT64_MAX - base)) {
            *ret = -EINVAL;
            return 1;
        }
        int64_t to = base + delta;
        f->off = (uint64_t)to; // past the end reads 0 bytes, as on Linux
        *ret = to;
        return 1;
    }
    case SYS_close:
        if (!(f = served_fd(req, a[0])))
            return 0;
        f->used = 0;
        *ret = 0;
        return 1;
    }
    return 0;
}

// One syscall per request: send the last reply, wait for the next request
// and read it (eclipse_channel_reply_wait).
static void *server(void *unused) {
    (void)unused;
    struct request req;
    struct reply rep;
    long rep_len = 0;
    for (;;) {
        uint32_t got = 0;
        long r = zx_status(zx(ECLIPSE_SYS_channel_reply_wait, server_handle, (long)&rep,
                              rep_len, (long)&req, sizeof req, (long)&got));
        rep_len = 0; // sent, even when a signal cut the wait short
        if (r == ZX_ERR_INTERNAL_INTR_RETRY)
            continue;
        if (r < 0) {
            atomic_store(&server_end, r == ZX_ERR_PEER_CLOSED ? PEER_CLOSED : FAILED);
            return NULL;
        }
        rep = (struct reply){.txid = req.txid};
        if (got != sizeof req || req.kind != REDIRECT_KIND_SYSCALL) {
            bad_requests++;
            rep.ret = -EIO;
        } else if (serve(&req, &rep.ret)) {
            served++;
        } else {
            rep.flags = REDIRECT_REPLY_NATIVE;
            handed_back++;
        }
        rep_len = sizeof rep;
    }
}

// ---- the client ------------------------------------------------------------

static void set_bit(uint64_t *mask, long n) { mask[n / 64] |= 1ull << (n % 64); }

#define CHECK(cond, ...)                                                       \
    do {                                                                       \
        if (!(cond)) {                                                         \
            fprintf(stderr, "zx-file-server: " __VA_ARGS__);                  \
            fprintf(stderr, "\n");                                             \
            return 1;                                                          \
        }                                                                      \
    } while (0)

struct timing {
    uint64_t open_close, read64, read4k;
};

static int time_file(const char *path, int rounds, struct timing *t) {
    char buf[4096];
    uint64_t t0 = now_ns();
    for (int i = 0; i < rounds; i++) {
        int fd = open(path, O_RDONLY);
        CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
        close(fd);
    }
    t->open_close = (now_ns() - t0) / rounds;
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
    t0 = now_ns();
    for (int i = 0; i < rounds; i++) {
        lseek(fd, 0, SEEK_SET);
        CHECK(read(fd, buf, 64) == 64, "read 64 of %s", path);
    }
    t->read64 = (now_ns() - t0) / rounds;
    t0 = now_ns();
    for (int i = 0; i < rounds; i++) {
        lseek(fd, 0, SEEK_SET);
        CHECK(read(fd, buf, 4096) == 4096, "read 4096 of %s", path);
    }
    t->read4k = (now_ns() - t0) / rounds;
    close(fd);
    return 0;
}

static int check_served(const char *native) {
    char buf[8192];
    int fd = open("/srv/hello", O_RDONLY);
    CHECK(fd >= SERVED_FD_BASE, "open /srv/hello answered %d (%s)", fd, strerror(errno));
    ssize_t n = read(fd, buf, sizeof buf);
    CHECK(n == (ssize_t)sizeof hello - 1 && memcmp(buf, hello, n) == 0,
          "/srv/hello read back %zd bytes", n);
    CHECK(read(fd, buf, sizeof buf) == 0, "no end of file on /srv/hello");
    CHECK(lseek(fd, 6, SEEK_SET) == 6, "lseek on /srv/hello");
    n = read(fd, buf, 4);
    CHECK(n == 4 && memcmp(buf, "from", 4) == 0, "read after lseek gave %zd bytes", n);
    CHECK(close(fd) == 0, "close /srv/hello");
    CHECK(read(fd, buf, 1) == -1 && errno == EBADF, "a closed served fd still reads");

    // The big file, in pieces that straddle the client's pages.
    fd = openat(AT_FDCWD, "/srv/big", O_RDONLY);
    CHECK(fd >= SERVED_FD_BASE, "openat /srv/big answered %d", fd);
    size_t total = 0;
    while ((n = read(fd, buf + 1, 3000)) > 0) {
        for (ssize_t i = 0; i < n; i++)
            CHECK(buf[1 + i] == (char)((total + i) * 7), "/srv/big byte %zu", total + i);
        total += n;
    }
    CHECK(n == 0 && total == sizeof big, "/srv/big read %zu bytes", total);
    close(fd);

    CHECK(open("/srv/nothing", O_RDONLY) == -1 && errno == ENOENT, "a missing file was not ENOENT");
    CHECK(open("/srv/hello", O_WRONLY) == -1 && errno == EROFS, "writing was not EROFS");
    fd = open(native, O_RDONLY);
    CHECK(fd >= 0 && fd < SERVED_FD_BASE, "the native file did not open natively: %d", fd);
    CHECK(read(fd, buf, 16) == 16, "the native file did not read");
    close(fd);
    return 0;
}

static int child(int fd, int rounds, const char *native) {
    struct timing before, after;
    if (time_file(native, rounds, &before))
        return 1;

    zx_handle_t h = 0;
    long r = zx_status(zx(ECLIPSE_SYS_fd_to_handle, fd, ECLIPSE_FD_TAKE, (long)&h, 0, 0, 0));
    CHECK(r >= 0, "fd_to_handle failed: %ld", r);
    close(fd);
    uint64_t mask[8] = {0};
#ifdef SYS_open
    set_bit(mask, SYS_open);
#endif
    set_bit(mask, SYS_openat);
    set_bit(mask, SYS_read);
    set_bit(mask, SYS_lseek);
    set_bit(mask, SYS_close);
    r = zx_status(zx(ECLIPSE_SYS_redirect_syscalls, h, (long)mask, 0, 0, 0, 0));
    CHECK(r == 0, "redirect failed: %ld", r);

    if (check_served(native))
        return 1;
    printf("zx-file-server: /srv served from userspace, native files handed back: ok\n");

    if (time_file("/srv/page", rounds, &after))
        return 1;
    printf("zx-file-server:                  native     served  served/native\n");
    printf("zx-file-server: open+close   %9llu  %9llu  %6.2f\n",
           (unsigned long long)before.open_close, (unsigned long long)after.open_close,
           (double)after.open_close / before.open_close);
    printf("zx-file-server: seek+read 64 %9llu  %9llu  %6.2f\n",
           (unsigned long long)before.read64, (unsigned long long)after.read64,
           (double)after.read64 / before.read64);
    printf("zx-file-server: seek+read 4K %9llu  %9llu  %6.2f\n",
           (unsigned long long)before.read4k, (unsigned long long)after.read4k,
           (double)after.read4k / before.read4k);
    return 0;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 5000;
    if (rounds <= 0)
        rounds = 5000;
    setvbuf(stdout, NULL, _IONBF, 0);
    for (size_t i = 0; i < sizeof page; i++)
        page[i] = (unsigned char)i;
    for (size_t i = 0; i < sizeof big; i++)
        big[i] = (unsigned char)(i * 7);

    zx_handle_t a = 0, b = 0;
    long r = zx_status(zx(ZX_SYS_channel_create, 0, (long)&a, (long)&b, 0, 0, 0));
    if (r == -ENOSYS) {
        printf("zx-file-server: gateway off (boot with zircon.gateway=1)\n");
        return 2;
    }
    if (r < 0) {
        fprintf(stderr, "zx-file-server: channel_create failed: %ld\n", r);
        return 1;
    }
    // The native file to compare with, the same size as /srv/page: a new
    // file of our own, never one that was already there.
    char native[] = "/tmp/zx-file-server.XXXXXX";
    int nfd = mkstemp(native);
    if (nfd < 0 || write(nfd, page, sizeof page) != (ssize_t)sizeof page) {
        fprintf(stderr, "zx-file-server: cannot make %s: %s\n", native, strerror(errno));
        if (nfd >= 0)
            unlink(native);
        return 1;
    }
    close(nfd);

    long fd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, a, 0, 0, 0, 0, 0));
    if (fd < 0) {
        fprintf(stderr, "zx-file-server: handle_to_fd failed: %ld\n", fd);
        unlink(native);
        return 1;
    }
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        unlink(native);
        return 1;
    }
    if (pid == 0)
        _exit(child((int)fd, rounds, native));
    close((int)fd);

    server_handle = b;
    pthread_t t;
    if (pthread_create(&t, NULL, server, NULL)) {
        perror("pthread_create");
        return 1;
    }
    int status = 0;
    waitpid(pid, &status, 0);
    for (int i = 0; i < 200 && atomic_load(&server_end) == RUNNING; i++)
        usleep(10000);
    unlink(native);
    int end = atomic_load(&server_end);
    if (end == RUNNING) {
        fprintf(stderr, "zx-file-server: the server's peer did not close after the child was reaped\n");
        return 1;
    }
    pthread_join(t, NULL);
    if (end != PEER_CLOSED) {
        fprintf(stderr, "zx-file-server: the server stopped on a channel error\n");
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "zx-file-server: the child failed (status %#x)\n", status);
        return 1;
    }
    if (bad_requests) {
        fprintf(stderr, "zx-file-server: %ld malformed requests\n", bad_requests);
        return 1;
    }
    printf("zx-file-server: served %ld, handed back %ld, peer closed on reap: ok\n", served,
           handed_back);
    return 0;
}
