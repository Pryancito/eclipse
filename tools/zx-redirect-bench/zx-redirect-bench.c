// zx-redirect-bench: Linux syscalls answered by a userspace server.
//
// The parent is the server. It gives one end of a channel to a child, which
// hands it to the kernel with eclipse_redirect_syscalls() for getppid and
// getuid. From then on the child's getppid is a channel call the parent
// answers (with the caller's pid, as the kernel stamped it in the request),
// and its getuid is one the parent sends back to run natively.
//
// It checks that, and that the call refuses what it must (exit_group in the
// mask, a second redirect), then times a native getppid, a redirected one
// and a pipe ping-pong between the same two processes. Under QEMU/TCG only
// the ratio redirected/pipe means much: both are one round trip between two
// processes, one through the kernel's redirect, the other through pipes.
//
//   zx-redirect-bench [ROUNDS]  (default 20000)
//
// Run it on Eclipse booted with `zircon.gateway=1`. Exit status: 0 on
// success, 2 when the gateway is off (ENOSYS), 1 on error.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000L

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_channel_create 7
#define ZX_SYS_channel_read 8
#define ZX_SYS_channel_write 10
#define ZX_SYS_object_wait_one 81

// Eclipse's own gateway calls (loader/src/linux.rs, ECLIPSE_GATEWAY_EXT).
#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_fd_to_handle 0x4001
#define ECLIPSE_SYS_redirect_syscalls 0x4002
#define ECLIPSE_FD_TAKE 1

// linux-object/src/process/syscall_redirect.rs
#define REDIRECT_KIND_SYSCALL 1
#define REDIRECT_REPLY_NATIVE 1

#define ZX_CHANNEL_READABLE (1u << 0)
#define ZX_CHANNEL_PEER_CLOSED (1u << 2)
#define ZX_TIME_INFINITE INT64_MAX
#define ZX_ERR_INVALID_ARGS (-10)
#define ZX_ERR_SHOULD_WAIT (-22)
#define ZX_ERR_ALREADY_BOUND (-27)

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

static long zx8(long n, long a0, long a1, long a2, long a3, long a4, long a5,
                long a6, long a7) {
    long args[8] = {a0, a1, a2, a3, a4, a5, a6, a7};
    return syscall(ZX_GATEWAY_BASE + (ZX_GATEWAY_INDIRECT | n), (long)args);
}

// musl's syscall() turns a negative return into errno for the -4095..-1
// range, which is where every zx_status_t lives: undo that.
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void set_bit(uint64_t *mask, long n) { mask[n / 64] |= 1ull << (n % 64); }

static long redirect(zx_handle_t h, const uint64_t *mask) {
    return zx_status(zx(ECLIPSE_SYS_redirect_syscalls, h, (long)mask, 0, 0, 0, 0));
}

// The server: answer getppid with the caller's pid, send anything else back
// to the kernel. Counts what it served; returns when the peer is gone.
static zx_handle_t server_handle;
static volatile long served, bad_requests;

static void *server(void *unused) {
    (void)unused;
    struct request req;
    for (;;) {
        uint32_t got = 0, nh = 0;
        long r = zx_status(zx8(ZX_SYS_channel_read, server_handle, 0, (long)&req, 0,
                               sizeof req, 0, (long)&got, (long)&nh));
        if (r == ZX_ERR_SHOULD_WAIT) {
            uint32_t observed = 0;
            r = zx_status(zx(ZX_SYS_object_wait_one, server_handle,
                             ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED,
                             ZX_TIME_INFINITE, (long)&observed, 0, 0));
            if (r < 0 || !(observed & ZX_CHANNEL_READABLE))
                break;
            continue;
        }
        if (r < 0)
            break;
        struct reply rep = {.txid = req.txid};
        if (got != sizeof req || req.kind != REDIRECT_KIND_SYSCALL) {
            bad_requests++;
            rep.ret = -EIO;
        } else if (req.nr == SYS_getppid) {
            rep.ret = (int64_t)req.pid;
        } else {
            rep.flags = REDIRECT_REPLY_NATIVE;
        }
        served++;
        if (zx_status(zx(ZX_SYS_channel_write, server_handle, 0, (long)&rep, sizeof rep,
                         0, 0)) < 0)
            break;
    }
    return NULL;
}

static int child(int fd, int rounds, int ping[2], int pong[2]) {
    uint64_t t0 = now_ns();
    for (int i = 0; i < rounds; i++)
        syscall(SYS_getppid);
    uint64_t native_ns = (now_ns() - t0) / rounds;
    uid_t uid = getuid();

    zx_handle_t h = 0;
    long r = zx_status(zx(ECLIPSE_SYS_fd_to_handle, fd, ECLIPSE_FD_TAKE, (long)&h, 0, 0, 0));
    if (r < 0) {
        fprintf(stderr, "zx-redirect-bench: fd_to_handle failed: %ld\n", r);
        return 1;
    }
    close(fd);

    uint64_t mask[8] = {0};
    set_bit(mask, SYS_exit_group);
    if ((r = redirect(h, mask)) != ZX_ERR_INVALID_ARGS) {
        fprintf(stderr, "zx-redirect-bench: exit_group was not refused: %ld\n", r);
        return 1;
    }
    memset(mask, 0, sizeof mask);
    set_bit(mask, SYS_getppid);
    set_bit(mask, SYS_getuid);
    if ((r = redirect(h, mask)) != 0) {
        fprintf(stderr, "zx-redirect-bench: redirect failed: %ld\n", r);
        return 1;
    }
    if ((r = redirect(h, mask)) >= 0) {
        fprintf(stderr, "zx-redirect-bench: a second redirect answered %ld\n", r);
        return 1;
    }

    long ppid = syscall(SYS_getppid);
    if (ppid != getpid()) {
        fprintf(stderr, "zx-redirect-bench: the server answered %ld, not our pid %d\n",
                ppid, getpid());
        return 1;
    }
    if (getuid() != uid) {
        fprintf(stderr, "zx-redirect-bench: getuid sent back native answered %d, not %d\n",
                getuid(), uid);
        return 1;
    }
    printf("zx-redirect-bench: getppid answered by the server, getuid handed back: ok\n");

    t0 = now_ns();
    for (int i = 0; i < rounds; i++)
        syscall(SYS_getppid);
    uint64_t redirected_ns = (now_ns() - t0) / rounds;

    char c = 0;
    t0 = now_ns();
    for (int i = 0; i < rounds; i++) {
        if (write(ping[1], &c, 1) != 1 || read(pong[0], &c, 1) != 1)
            return 1;
    }
    uint64_t pipe_ns = (now_ns() - t0) / rounds;
    close(ping[1]);

    printf("zx-redirect-bench: getppid native     %8llu ns\n", (unsigned long long)native_ns);
    printf("zx-redirect-bench: getppid redirected %8llu ns\n", (unsigned long long)redirected_ns);
    printf("zx-redirect-bench: pipe ping-pong     %8llu ns (2 procs)\n",
           (unsigned long long)pipe_ns);
    printf("zx-redirect-bench: redirected/pipe %.2f, redirected/native %.1f\n",
           (double)redirected_ns / pipe_ns, (double)redirected_ns / native_ns);
    return 0;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 20000;
    if (rounds <= 0)
        rounds = 20000;
    setvbuf(stdout, NULL, _IONBF, 0);

    zx_handle_t a = 0, b = 0;
    long r = zx_status(zx(ZX_SYS_channel_create, 0, (long)&a, (long)&b, 0, 0, 0));
    if (r == -ENOSYS) {
        printf("zx-redirect-bench: gateway off (boot with zircon.gateway=1)\n");
        return 2;
    }
    if (r < 0) {
        fprintf(stderr, "zx-redirect-bench: channel_create failed: %ld\n", r);
        return 1;
    }
    long fd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, a, 0, 0, 0, 0, 0));
    if (fd < 0) {
        fprintf(stderr, "zx-redirect-bench: handle_to_fd failed: %ld\n", fd);
        return 1;
    }
    int ping[2], pong[2];
    if (pipe(ping) || pipe(pong)) {
        perror("pipe");
        return 1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        close(ping[0]);
        close(pong[1]);
        _exit(child((int)fd, rounds, ping, pong));
    }
    close((int)fd);
    close(ping[1]);
    close(pong[0]);

    server_handle = b;
    pthread_t t;
    if (pthread_create(&t, NULL, server, NULL)) {
        perror("pthread_create");
        return 1;
    }
    char c;
    while (read(ping[0], &c, 1) == 1)
        if (write(pong[1], &c, 1) != 1)
            break;

    int status = 0;
    waitpid(pid, &status, 0);
    // The kernel's end lives as long as the child process: reaped, it goes,
    // and the server sees its peer closed.
    for (int i = 0; i < 200 && served >= 0; i++) {
        if (pthread_tryjoin_np(t, NULL) == 0) {
            served = -served - 1;
            break;
        }
        usleep(10000);
    }
    if (served >= 0) {
        fprintf(stderr, "zx-redirect-bench: the server's peer did not close after the child was reaped\n");
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "zx-redirect-bench: the child failed (status %#x)\n", status);
        return 1;
    }
    if (bad_requests) {
        fprintf(stderr, "zx-redirect-bench: %ld malformed requests\n", bad_requests);
        return 1;
    }
    printf("zx-redirect-bench: served %ld requests, peer closed on reap: ok\n", -served - 1);
    return 0;
}
