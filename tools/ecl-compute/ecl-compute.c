// ecl-compute — launch work on Eclipse's NVIDIA compute GPU from userspace.
//
// Talks to /dev/dri/card1 (or renderD129) with a driver-private DRM ioctl.
// No libdrm, no CUDA. The same binary runs on Eclipse; it is a no-op elsewhere
// unless that kernel implements the same ioctl.
//
// Usage:
//   ecl-compute              # SAXPY (default)
//   ecl-compute saxpy
//   ecl-compute bench
//   ecl-compute info
//   ecl-compute saxpy /dev/dri/card1
//
// Device search order: argv, $ECLIPSE_COMPUTE_DEVICE, card1, renderD129, card0.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define DRM_IOCTL_BASE 'd'
#define DRM_ECLIPSE_COMPUTE_NR 0x90

#define ECLIPSE_COMPUTE_INFO  0
#define ECLIPSE_COMPUTE_SAXPY 1
#define ECLIPSE_COMPUTE_BENCH 2

struct drm_eclipse_compute {
    uint32_t op;
    int32_t status;
    uint64_t elapsed_ns;
    uint32_t grid_threads;
    uint32_t reserved;
    char summary[512];
};

#define DRM_IOCTL_ECLIPSE_COMPUTE \
    _IOWR(DRM_IOCTL_BASE, DRM_ECLIPSE_COMPUTE_NR, struct drm_eclipse_compute)

static const char *k_candidates[] = {
    "/dev/dri/card1",
    "/dev/dri/renderD129",
    "/dev/dri/card0",
    NULL,
};

static int open_compute_node(const char *explicit) {
    if (explicit && explicit[0]) {
        int fd = open(explicit, O_RDWR);
        if (fd < 0)
            fprintf(stderr, "ecl-compute: open %s: %s\n", explicit, strerror(errno));
        return fd;
    }
    const char *env = getenv("ECLIPSE_COMPUTE_DEVICE");
    if (env && env[0]) {
        int fd = open(env, O_RDWR);
        if (fd < 0)
            fprintf(stderr, "ecl-compute: open %s: %s\n", env, strerror(errno));
        return fd;
    }
    for (const char **p = k_candidates; *p; p++) {
        int fd = open(*p, O_RDWR);
        if (fd >= 0) {
            fprintf(stderr, "ecl-compute: using %s\n", *p);
            return fd;
        }
    }
    fprintf(stderr, "ecl-compute: no compute DRM node (tried card1, renderD129, card0)\n");
    return -1;
}

static uint32_t parse_op(const char *s) {
    if (!s || !s[0] || strcmp(s, "saxpy") == 0)
        return ECLIPSE_COMPUTE_SAXPY;
    if (strcmp(s, "bench") == 0)
        return ECLIPSE_COMPUTE_BENCH;
    if (strcmp(s, "info") == 0)
        return ECLIPSE_COMPUTE_INFO;
    fprintf(stderr, "ecl-compute: unknown op '%s' (try saxpy, bench, info)\n", s);
    exit(2);
}

int main(int argc, char **argv) {
    const char *opname = NULL;
    const char *dev = NULL;
    for (int i = 1; i < argc; i++) {
        if (argv[i][0] == '/')
            dev = argv[i];
        else
            opname = argv[i];
    }

    uint32_t op = parse_op(opname);
    int fd = open_compute_node(dev);
    if (fd < 0)
        return 1;

    struct drm_eclipse_compute req;
    memset(&req, 0, sizeof(req));
    req.op = op;

    if (ioctl(fd, DRM_IOCTL_ECLIPSE_COMPUTE, &req) < 0) {
        fprintf(stderr, "ecl-compute: ioctl: %s (is this Eclipse with a compute GPU?)\n",
                strerror(errno));
        close(fd);
        return 1;
    }
    close(fd);

    fputs(req.summary, stdout);
    if (req.summary[0] && req.summary[strlen(req.summary) - 1] != '\n')
        fputc('\n', stdout);
    if (req.elapsed_ns)
        fprintf(stdout, "elapsed_ns=%llu grid_threads=%u status=%d\n",
                (unsigned long long)req.elapsed_ns, req.grid_threads, req.status);
    else if (op != ECLIPSE_COMPUTE_INFO)
        fprintf(stdout, "grid_threads=%u status=%d\n", req.grid_threads, req.status);

    return req.status == 0 ? 0 : 1;
}
