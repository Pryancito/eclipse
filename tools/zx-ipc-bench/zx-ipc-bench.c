// zx-ipc-bench: channel call/reply from userspace through the Zircon syscall
// gateway, against a pipe ping-pong: first between two threads, then
// between two processes that got the channel end through a handle fd.
//
// Run it on Eclipse booted with `zircon.gateway=1`. Each channel number has
// a pipe number taken the same way on the same machine: their ratio is what
// this measures, and it holds under QEMU/TCG, where absolute times do not.
// It also checks the handle fd calls (poll on a channel fd, take, refusal
// to duplicate a channel end).
//
//   zx-ipc-bench [ROUNDS [CPU]]  (default 20000, unpinned)
//
// Exit status: 0 on success, 2 when the gateway is off (ENOSYS), 1 on error.

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>
#include <sched.h>
#include <poll.h>
#include <signal.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000L

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_channel_call_noretry 6
#define ZX_SYS_channel_create 7
#define ZX_SYS_channel_read 8
#define ZX_SYS_channel_write 10
#define ZX_SYS_fifo_create 34
#define ZX_SYS_fifo_read 35
#define ZX_SYS_fifo_write 36
#define ZX_SYS_handle_close 45
#define ZX_SYS_object_wait_one 81

// Eclipse's own gateway calls (loader/src/linux.rs, ECLIPSE_GATEWAY_EXT).
#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_fd_to_handle 0x4001
#define ECLIPSE_FD_TAKE 1

#define ZX_CHANNEL_READABLE (1u << 0)
#define ZX_CHANNEL_PEER_CLOSED (1u << 2)
#define ZX_TIME_INFINITE INT64_MAX
#define ZX_ERR_SHOULD_WAIT (-22)
#define ZX_ERR_PEER_CLOSED (-24)
#define ZX_ERR_ACCESS_DENIED (-30)

typedef uint32_t zx_handle_t;

typedef struct {
    const void *wr_bytes;
    const zx_handle_t *wr_handles;
    void *rd_bytes;
    zx_handle_t *rd_handles;
    uint32_t wr_num_bytes;
    uint32_t wr_num_handles;
    uint32_t rd_num_bytes;
    uint32_t rd_num_handles;
} zx_channel_call_args_t;

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

static long channel_read(zx_handle_t h, void *buf, uint32_t cap, uint32_t *got) {
    uint32_t nh = 0;
    return zx_status(zx8(ZX_SYS_channel_read, h, 0, (long)buf, 0, cap, 0,
                         (long)got, (long)&nh));
}

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void *zx_server(void *arg) {
    zx_handle_t h = (zx_handle_t)(uintptr_t)arg;
    uint8_t buf[64];
    for (;;) {
        uint32_t got = 0;
        long r = channel_read(h, buf, sizeof buf, &got);
        if (r == ZX_ERR_SHOULD_WAIT) {
            uint32_t observed = 0;
            r = zx_status(zx(ZX_SYS_object_wait_one, h,
                             ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED,
                             ZX_TIME_INFINITE, (long)&observed, 0, 0));
            if (r < 0 || !(observed & ZX_CHANNEL_READABLE))
                break;
            continue;
        }
        if (r < 0)
            break;
        // Echo it back: its first four bytes are the txid, which makes it
        // the reply to the call that sent it.
        if (zx_status(zx(ZX_SYS_channel_write, h, 0, (long)buf, got, 0, 0)) < 0)
            break;
    }
    return NULL;
}

struct pipes { int req[2], rep[2]; };

static void *pipe_server(void *arg) {
    struct pipes *p = arg;
    uint8_t buf[32];
    while (read(p->req[0], buf, sizeof buf) == (ssize_t)sizeof buf)
        if (write(p->rep[1], buf, sizeof buf) != (ssize_t)sizeof buf)
            break;
    return NULL;
}

// Mean ns per channel call on `client`, `rounds` of them after a warm-up;
// 0 on failure.
static uint64_t channel_rounds(zx_handle_t client, long rounds) {
    uint8_t out[32] = {0}, in[64];
    uint32_t got_bytes = 0, got_handles = 0;
    zx_channel_call_args_t args = {
        .wr_bytes = out, .wr_num_bytes = sizeof out,
        .rd_bytes = in, .rd_num_bytes = sizeof in,
    };
    uint64_t t0 = 0;
    for (long i = -rounds / 10; i < rounds; i++) {
        if (i == 0)
            t0 = now_ns();
        long r = zx_status(zx(ZX_SYS_channel_call_noretry, client, 0, ZX_TIME_INFINITE,
                              (long)&args, (long)&got_bytes, (long)&got_handles));
        if (r < 0) {
            fprintf(stderr, "zx-ipc-bench: channel_call failed: %ld\n", r);
            return 0;
        }
    }
    return (now_ns() - t0) / rounds;
}

// Mean ns per 32-byte write+read pair on the pipes; 0 on failure.
static uint64_t pipe_rounds(int req, int rep, long rounds) {
    uint8_t buf[32] = {0};
    uint64_t t0 = 0;
    for (long i = -rounds / 10; i < rounds; i++) {
        if (i == 0)
            t0 = now_ns();
        if (write(req, buf, sizeof buf) != (ssize_t)sizeof buf ||
            read(rep, buf, sizeof buf) != (ssize_t)sizeof buf) {
            perror("pipe round trip");
            return 0;
        }
    }
    return (now_ns() - t0) / rounds;
}

// A FIFO element's worth, as a block FIFO request is.
#define FIFO_ELEM 32

static long fifo_read1(zx_handle_t h, void *buf) {
    size_t n = 0;
    return zx_status(zx(ZX_SYS_fifo_read, h, FIFO_ELEM, (long)buf, 1, (long)&n, 0));
}

static long fifo_write1(zx_handle_t h, const void *buf) {
    size_t n = 0;
    return zx_status(zx(ZX_SYS_fifo_write, h, FIFO_ELEM, (long)buf, 1, (long)&n, 0));
}

// One element out of `h`, sleeping in zx_object_wait_one while there is
// none, as a block client or server does. 0, or <0 once the peer is gone.
static long fifo_take(zx_handle_t h, void *buf) {
    for (;;) {
        long r = fifo_read1(h, buf);
        if (r != ZX_ERR_SHOULD_WAIT)
            return r;
        uint32_t observed = 0;
        r = zx_status(zx(ZX_SYS_object_wait_one, h, ZX_CHANNEL_READABLE | ZX_CHANNEL_PEER_CLOSED,
                         ZX_TIME_INFINITE, (long)&observed, 0, 0));
        if (r < 0 || !(observed & ZX_CHANNEL_READABLE))
            return -1;
    }
}

// The same, spinning on zx_fifo_read instead of sleeping: what the round
// trip costs without the sleep and the wake.
static long fifo_spin(zx_handle_t h, void *buf) {
    long r;
    while ((r = fifo_read1(h, buf)) == ZX_ERR_SHOULD_WAIT)
        ;
    return r;
}

// Echo until the peer goes. The first byte of each element says how to wait
// for the next one: 0 sleeps, 1 spins.
static void fifo_server(zx_handle_t h) {
    uint8_t buf[FIFO_ELEM] = {0};
    while ((buf[0] ? fifo_spin(h, buf) : fifo_take(h, buf)) == 0)
        if (fifo_write1(h, buf) < 0)
            break;
}

// Mean ns per FIFO write+read pair against an echoing server; 0 on failure.
static uint64_t fifo_rounds(zx_handle_t h, long rounds, int spin) {
    uint8_t buf[FIFO_ELEM] = {(uint8_t)spin};
    uint64_t t0 = 0;
    for (long i = -rounds / 10; i < rounds; i++) {
        if (i == 0)
            t0 = now_ns();
        if (fifo_write1(h, buf) < 0 || (spin ? fifo_spin(h, buf) : fifo_take(h, buf)) < 0) {
            fprintf(stderr, "zx-ipc-bench: fifo round trip failed\n");
            return 0;
        }
    }
    return (now_ns() - t0) / rounds;
}

// The scheduler's wake lines from /proc/perf/kernel (they count since boot:
// compare two dumps), with `zx-ipc-bench: [label]` in front.
static void dump_wake_stats(const char *label) {
    FILE *f = fopen("/proc/perf/kernel", "r");
    if (!f)
        return;
    char line[512];
    while (fgets(line, sizeof line, f))
        if (strstr(line, "wake") || strstr(line, "resched"))
            printf("zx-ipc-bench: [%s] %s", label, line);
    fclose(f);
}

static int child_status(pid_t pid) {
    int st = 0;
    if (waitpid(pid, &st, 0) != pid)
        return -1;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

// The handle-in-an-fd calls: what a process needs to hand a channel end to
// another. Answers 0 when every check holds.
static int handle_fd_checks(void) {
    zx_handle_t a = 0, b = 0;
    if (zx_status(zx(ZX_SYS_channel_create, 0, (long)&a, (long)&b, 0, 0, 0)) < 0)
        return 1;
    long fd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, a, O_CLOEXEC, 0, 0, 0, 0));
    if (fd < 0) {
        fprintf(stderr, "zx-ipc-bench: handle_to_fd failed: %ld\n", fd);
        return 1;
    }
    struct pollfd pfd = {.fd = (int)fd, .events = POLLIN};
    if (poll(&pfd, 1, 0) != 0) {
        fprintf(stderr, "zx-ipc-bench: an empty channel fd polled ready\n");
        return 1;
    }
    uint8_t msg[8] = {1, 2, 3, 4, 5, 6, 7, 8};
    if (zx_status(zx(ZX_SYS_channel_write, b, 0, (long)msg, sizeof msg, 0, 0)) < 0)
        return 1;
    if (poll(&pfd, 1, 1000) != 1 || !(pfd.revents & POLLIN)) {
        fprintf(stderr, "zx-ipc-bench: a channel fd with a message did not poll readable\n");
        return 1;
    }
    zx_handle_t h = 0;
    long r = zx_status(zx(ECLIPSE_SYS_fd_to_handle, fd, 0, (long)&h, 0, 0, 0));
    if (r != ZX_ERR_ACCESS_DENIED) {
        fprintf(stderr, "zx-ipc-bench: duplicating a channel end answered %ld\n", r);
        return 1;
    }
    r = zx_status(zx(ECLIPSE_SYS_fd_to_handle, fd, ECLIPSE_FD_TAKE, (long)&h, 0, 0, 0));
    uint8_t in[8] = {0};
    uint32_t got = 0;
    if (r < 0 || channel_read(h, in, sizeof in, &got) < 0 || got != 8 ||
        memcmp(in, msg, 8) != 0) {
        fprintf(stderr, "zx-ipc-bench: taking the end back failed: %ld\n", r);
        return 1;
    }
    close((int)fd);
    zx(ZX_SYS_handle_close, h, 0, 0, 0, 0, 0);
    zx(ZX_SYS_handle_close, b, 0, 0, 0, 0, 0);
    return 0;
}

int main(int argc, char **argv) {
    long rounds = argc > 1 ? atol(argv[1]) : 20000;
    if (rounds <= 0)
        rounds = 20000;
    // `zx-ipc-bench N 0`: everything on CPU 0, to tell wake-up cost on one
    // CPU apart from the cost of waking a thread on the other.
    // `zx-ipc-bench N CPU stats` (CPU -1: unpinned): the scheduler's wake
    // statistics around the two-process runs.
    int stats = argc > 3 && strcmp(argv[3], "stats") == 0;
    if (argc > 2 && atoi(argv[2]) >= 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(atoi(argv[2]), &set);
        if (sched_setaffinity(0, sizeof set, &set) != 0)
            perror("sched_setaffinity");
        else
            printf("zx-ipc-bench: pinned to CPU %d\n", atoi(argv[2]));
    }

    zx_handle_t client = 0, server = 0;
    long r = zx_status(zx(ZX_SYS_channel_create, 0, (long)&client, (long)&server, 0, 0, 0));
    if (r == -ENOSYS) {
        fprintf(stderr, "zx-ipc-bench: Zircon gateway off (boot with zircon.gateway=1)\n");
        return 2;
    }
    if (r < 0) {
        fprintf(stderr, "zx-ipc-bench: channel_create failed: %ld\n", r);
        return 1;
    }

    // Two threads of one process.
    pthread_t t;
    int err = pthread_create(&t, NULL, zx_server, (void *)(uintptr_t)server);
    if (err != 0) {
        fprintf(stderr, "zx-ipc-bench: pthread_create: %s\n", strerror(err));
        return 1;
    }
    uint64_t zx_ns = channel_rounds(client, rounds);
    zx(ZX_SYS_handle_close, client, 0, 0, 0, 0, 0);
    pthread_join(t, NULL);
    if (!zx_ns)
        return 1;

    struct pipes p;
    if (pipe(p.req) || pipe(p.rep)) {
        perror("pipe");
        return 1;
    }
    err = pthread_create(&t, NULL, pipe_server, &p);
    if (err != 0) {
        fprintf(stderr, "zx-ipc-bench: pthread_create: %s\n", strerror(err));
        return 1;
    }
    uint64_t pipe_ns = pipe_rounds(p.req[1], p.rep[0], rounds);
    close(p.req[1]);
    pthread_join(t, NULL);
    close(p.req[0]);
    close(p.rep[0]);
    close(p.rep[1]);
    if (!pipe_ns)
        return 1;

    printf("zx-ipc-bench: %ld round trips, 32 bytes, two threads\n", rounds);
    printf("zx-ipc-bench: channel call   %8llu ns\n", (unsigned long long)zx_ns);
    printf("zx-ipc-bench: pipe ping-pong %8llu ns\n", (unsigned long long)pipe_ns);
    printf("zx-ipc-bench: channel/pipe   %8.2f\n", (double)zx_ns / (double)pipe_ns);

    if (handle_fd_checks())
        return 1;
    printf("zx-ipc-bench: handle fd checks ok\n");

    // Two processes: the server end goes to the child in an fd that fork
    // copies, and the child takes it out into its own handle table.
    if (zx_status(zx(ZX_SYS_channel_create, 0, (long)&client, (long)&server, 0, 0, 0)) < 0)
        return 1;
    long sfd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, server, 0, 0, 0, 0, 0));
    if (sfd < 0) {
        fprintf(stderr, "zx-ipc-bench: handle_to_fd failed: %ld\n", sfd);
        return 1;
    }
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        zx_handle_t h = 0;
        if (zx_status(zx(ECLIPSE_SYS_fd_to_handle, sfd, ECLIPSE_FD_TAKE, (long)&h, 0, 0, 0)) < 0)
            _exit(1);
        close((int)sfd);
        zx_server((void *)(uintptr_t)h);
        _exit(0);
    }
    close((int)sfd);
    if (stats)
        dump_wake_stats("before channel");
    uint64_t zx2_ns = channel_rounds(client, rounds);
    if (stats)
        dump_wake_stats("after channel");
    zx(ZX_SYS_handle_close, client, 0, 0, 0, 0, 0);
    if (child_status(pid) != 0 || !zx2_ns) {
        fprintf(stderr, "zx-ipc-bench: the channel server process failed\n");
        return 1;
    }

    if (pipe(p.req) || pipe(p.rep)) {
        perror("pipe");
        return 1;
    }
    pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        close(p.req[1]);
        close(p.rep[0]);
        pipe_server(&p);
        _exit(0);
    }
    close(p.req[0]);
    close(p.rep[1]);
    uint64_t pipe2_ns = pipe_rounds(p.req[1], p.rep[0], rounds);
    if (stats)
        dump_wake_stats("after pipe");
    close(p.req[1]);
    if (child_status(pid) != 0 || !pipe2_ns)
        return 1;

    // A FIFO, as the block driver serves its client: write, then read,
    // sleeping in object_wait_one when there is nothing yet.
    zx_handle_t f0 = 0, f1 = 0;
    if (zx_status(zx(ZX_SYS_fifo_create, 16, FIFO_ELEM, 0, (long)&f0, (long)&f1, 0)) < 0)
        return 1;
    long ffd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, f1, 0, 0, 0, 0, 0));
    if (ffd < 0)
        return 1;
    pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        zx_handle_t h = 0;
        if (zx_status(zx(ECLIPSE_SYS_fd_to_handle, ffd, ECLIPSE_FD_TAKE, (long)&h, 0, 0, 0)) < 0)
            _exit(1);
        close((int)ffd);
        fifo_server(h);
        _exit(0);
    }
    close((int)ffd);
    if (stats)
        dump_wake_stats("before fifo");
    uint64_t fifo2_ns = fifo_rounds(f0, rounds, 0);
    uint64_t fifo2_spin_ns = fifo_rounds(f0, rounds, 1);
    // Back to sleeping, so the server is not left spinning on the close.
    if (!fifo_rounds(f0, 1, 0))
        fifo2_ns = 0;
    if (stats)
        dump_wake_stats("after fifo");
    zx(ZX_SYS_handle_close, f0, 0, 0, 0, 0, 0);
    if (child_status(pid) != 0 || !fifo2_ns) {
        fprintf(stderr, "zx-ipc-bench: the fifo server process failed\n");
        return 1;
    }

    printf("zx-ipc-bench: %ld round trips, 32 bytes, two processes\n", rounds);
    printf("zx-ipc-bench: fifo ping-pong %8llu ns (2 procs)\n", (unsigned long long)fifo2_ns);
    printf("zx-ipc-bench: fifo spinning  %8llu ns (2 procs)\n", (unsigned long long)fifo2_spin_ns);
    printf("zx-ipc-bench: channel call   %8llu ns (2 procs)\n", (unsigned long long)zx2_ns);
    printf("zx-ipc-bench: pipe ping-pong %8llu ns (2 procs)\n", (unsigned long long)pipe2_ns);
    printf("zx-ipc-bench: channel/pipe   %8.2f (2 procs)\n", (double)zx2_ns / (double)pipe2_ns);
    return 0;
}
