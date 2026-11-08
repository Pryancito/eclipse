// zx-ipc-bench: channel call/reply from userspace through the Zircon syscall
// gateway, against a pipe ping-pong between the same two threads.
//
// Both run in one process on Eclipse (boot with `zircon.gateway=1`), so the
// two numbers come from the same machine, the same emulation and the same
// threads: their ratio is what this measures, and it holds under QEMU/TCG,
// where absolute times do not.
//
//   zx-ipc-bench [ROUNDS]        (default 20000)
//
// Exit status: 0 on success, 2 when the gateway is off (ENOSYS), 1 on error.

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000L

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_channel_call_noretry 6
#define ZX_SYS_channel_create 7
#define ZX_SYS_channel_read 8
#define ZX_SYS_channel_write 10
#define ZX_SYS_handle_close 45
#define ZX_SYS_object_wait_one 81

#define ZX_CHANNEL_READABLE (1u << 0)
#define ZX_CHANNEL_PEER_CLOSED (1u << 2)
#define ZX_TIME_INFINITE INT64_MAX
#define ZX_ERR_SHOULD_WAIT (-22)
#define ZX_ERR_PEER_CLOSED (-24)

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

int main(int argc, char **argv) {
    long rounds = argc > 1 ? atol(argv[1]) : 20000;
    if (rounds <= 0)
        rounds = 20000;

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

    pthread_t t;
    pthread_create(&t, NULL, zx_server, (void *)(uintptr_t)server);
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
        r = zx_status(zx(ZX_SYS_channel_call_noretry, client, 0, ZX_TIME_INFINITE,
                         (long)&args, (long)&got_bytes, (long)&got_handles));
        if (r < 0) {
            fprintf(stderr, "zx-ipc-bench: channel_call failed: %ld\n", r);
            return 1;
        }
    }
    uint64_t zx_ns = (now_ns() - t0) / rounds;
    zx(ZX_SYS_handle_close, client, 0, 0, 0, 0, 0);
    pthread_join(t, NULL);

    struct pipes p;
    if (pipe(p.req) || pipe(p.rep)) {
        perror("pipe");
        return 1;
    }
    pthread_create(&t, NULL, pipe_server, &p);
    for (long i = -rounds / 10; i < rounds; i++) {
        if (i == 0)
            t0 = now_ns();
        if (write(p.req[1], out, sizeof out) != (ssize_t)sizeof out ||
            read(p.rep[0], in, sizeof out) != (ssize_t)sizeof out) {
            perror("pipe round trip");
            return 1;
        }
    }
    uint64_t pipe_ns = (now_ns() - t0) / rounds;
    close(p.req[1]);
    pthread_join(t, NULL);

    printf("zx-ipc-bench: %ld round trips, 32 bytes, two threads\n", rounds);
    printf("zx-ipc-bench: channel call   %8llu ns\n", (unsigned long long)zx_ns);
    printf("zx-ipc-bench: pipe ping-pong %8llu ns\n", (unsigned long long)pipe_ns);
    printf("zx-ipc-bench: channel/pipe   %8.2f\n", (double)zx_ns / (double)pipe_ns);
    return 0;
}
