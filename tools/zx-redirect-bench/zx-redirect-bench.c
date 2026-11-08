// zx-redirect-bench: Linux syscalls answered by a userspace server.
//
// The parent is the server. It gives one end of a channel to a child, which
// hands it to the kernel with eclipse_redirect_syscalls() for getppid and
// getuid. From then on the child's getppid is a channel call the parent
// answers (with the caller's pid, as the kernel stamped it in the request),
// and its getuid is one the parent sends back to run natively.
//
// The parent also serves two syscalls with pointers, through the client
// memory call: getcwd, writing its answer into the child's buffer, and
// write to fd 1, reading the child's buffer and writing it out itself.
//
// It checks that, and that the calls refuse what they must (exit_group in
// the mask, a second redirect, memory of a process that is not this
// server's), then times a native getppid, a redirected one, a redirected
// getcwd and a pipe ping-pong between the same two processes. Under QEMU/TCG only
// the ratio redirected/pipe means much: both are one round trip between two
// processes, one through the kernel's redirect, the other through pipes.
//
//   zx-redirect-bench [ROUNDS [phases]]  (default 20000)
//
// With `phases` the server stamps each reply with when it received the
// request and when it replied, and the run ends with the kernel's split of
// a round trip into its phases (the redirect lines of /proc/perf/kernel).
// The stamps cost the server two clock reads per request.
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
#define ECLIPSE_SYS_redirect_memory 0x4003
#define ECLIPSE_MEMORY_TO_CLIENT 1
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
#define ZX_ERR_ACCESS_DENIED (-30)

#define SERVED_CWD "/served/by/the/server"

typedef uint32_t zx_handle_t;

struct request {
    uint32_t txid, kind;
    uint64_t pid, tid, nr, args[6];
};

struct reply {
    uint32_t txid, flags;
    int64_t ret;
    // CLOCK_MONOTONIC ns, only with `phases` (REPLY_LEN 32 then, else 16).
    uint64_t received, replied;
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
static int stamp;
static volatile long served, bad_requests;

static long client_memory(uint64_t pid, uint64_t addr, void *buf, long len, int to_client) {
    return zx_status(zx(ECLIPSE_SYS_redirect_memory, server_handle, (long)pid, (long)addr,
                        (long)buf, len, to_client ? ECLIPSE_MEMORY_TO_CLIENT : 0));
}

// getcwd(buf, size): the answer goes straight into the client's buffer.
static int64_t serve_getcwd(const struct request *req) {
    const char cwd[] = SERVED_CWD;
    if (req->args[1] < sizeof cwd)
        return -ERANGE;
    long n = client_memory(req->pid, req->args[0], (void *)cwd, sizeof cwd, 1);
    return n == (long)sizeof cwd ? (int64_t)sizeof cwd : -EFAULT;
}

// write(1, buf, len): the server reads the client's buffer and writes it.
static int64_t serve_write(const struct request *req) {
    char buf[4096];
    uint64_t len = req->args[2] < sizeof buf ? req->args[2] : sizeof buf;
    long n = client_memory(req->pid, req->args[1], buf, (long)len, 0);
    if (n < 0)
        return -EFAULT;
    if (n == 0 && len)
        return -EFAULT;
    return write(1, buf, (size_t)n);
}

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
        if (stamp)
            rep.received = now_ns();
        if (got != sizeof req || req.kind != REDIRECT_KIND_SYSCALL) {
            bad_requests++;
            rep.ret = -EIO;
        } else if (req.nr == SYS_getppid) {
            rep.ret = (int64_t)req.pid;
        } else if (req.nr == SYS_getcwd) {
            rep.ret = serve_getcwd(&req);
        } else if (req.nr == SYS_write && req.args[0] == 1) {
            rep.ret = serve_write(&req);
        } else {
            rep.flags = REDIRECT_REPLY_NATIVE;
        }
        served++;
        if (stamp)
            rep.replied = now_ns();
        if (zx_status(zx(ZX_SYS_channel_write, server_handle, 0, (long)&rep,
                         stamp ? sizeof rep : 16, 0, 0)) < 0)
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

    // The pipe before the redirect: after it, write is served (fd 1) or
    // handed back (any other fd), and each would cost a round trip more.
    char c = 0;
    t0 = now_ns();
    for (int i = 0; i < rounds; i++) {
        if (write(ping[1], &c, 1) != 1 || read(pong[0], &c, 1) != 1)
            return 1;
    }
    uint64_t pipe_ns = (now_ns() - t0) / rounds;
    close(ping[1]);

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
    set_bit(mask, SYS_getcwd);
    set_bit(mask, SYS_write);
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
    char cwd[64] = {0};
    long cwd_len = syscall(SYS_getcwd, cwd, sizeof cwd);
    if (cwd_len != (long)sizeof SERVED_CWD || strcmp(cwd, SERVED_CWD) != 0) {
        fprintf(stderr, "zx-redirect-bench: getcwd answered %ld \"%s\"\n", cwd_len, cwd);
        return 1;
    }
    if (syscall(SYS_getcwd, cwd, 4) != -1 || errno != ERANGE) {
        fprintf(stderr, "zx-redirect-bench: a short getcwd buffer was not ERANGE\n");
        return 1;
    }
    // stdout goes through the server now: printf's writev is native, so
    // this line is the one that proves it, written with write(2).
    const char line[] = "zx-redirect-bench: this line was written by the server\n";
    if (write(1, line, sizeof line - 1) != (long)sizeof line - 1) {
        fprintf(stderr, "zx-redirect-bench: the served write failed\n");
        return 1;
    }
    printf("zx-redirect-bench: getppid answered by the server, getuid handed back, "
           "getcwd and write served through client memory: ok\n");

    t0 = now_ns();
    for (int i = 0; i < rounds; i++)
        syscall(SYS_getppid);
    uint64_t redirected_ns = (now_ns() - t0) / rounds;

    t0 = now_ns();
    for (int i = 0; i < rounds; i++)
        syscall(SYS_getcwd, cwd, sizeof cwd);
    uint64_t getcwd_ns = (now_ns() - t0) / rounds;


    printf("zx-redirect-bench: getppid native     %8llu ns\n", (unsigned long long)native_ns);
    printf("zx-redirect-bench: getppid redirected %8llu ns\n", (unsigned long long)redirected_ns);
    printf("zx-redirect-bench: getcwd redirected  %8llu ns (one client memory write)\n",
           (unsigned long long)getcwd_ns);
    printf("zx-redirect-bench: pipe ping-pong     %8llu ns (2 procs)\n",
           (unsigned long long)pipe_ns);
    printf("zx-redirect-bench: redirected/pipe %.2f, redirected/native %.1f\n",
           (double)redirected_ns / pipe_ns, (double)redirected_ns / native_ns);
    return 0;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 20000;
    stamp = argc > 2 && strcmp(argv[2], "phases") == 0;
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
    // This process is not redirected, least of all to this server.
    char probe;
    long denied = client_memory((uint64_t)getpid(), (uint64_t)&probe, &probe, 1, 0);
    if (denied != ZX_ERR_ACCESS_DENIED) {
        fprintf(stderr, "zx-redirect-bench: memory of an unserved process answered %ld\n",
                denied);
        return 1;
    }
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
    if (stamp) {
        FILE *f = fopen("/proc/perf/kernel", "r");
        char line[512];
        while (f && fgets(line, sizeof line, f))
            if (strstr(line, "redirect"))
                printf("zx-redirect-bench: kernel: %s", line[0] == ' ' ? line + 2 : line);
        if (f)
            fclose(f);
    }
    return 0;
}
