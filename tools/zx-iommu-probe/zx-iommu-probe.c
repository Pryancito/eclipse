// zx-iommu-probe: does an NVMe controller reach only what its driver pinned?
//
// Takes an NVMe controller the kernel left to userspace and nobody drives
// (a third QEMU `nvme`: the system's services drive the first two), brings
// it up with its admin queues in pinned memory, and asks it twice for its
// identity:
//
//   1. into a pinned page: the transfer must succeed, so a device with a
//      domain still reaches what its driver gave it;
//   2. into a page that was pinned, handed to nobody, and unpinned again:
//      with the IOMMU on (`iommu=on`) the device must fail the transfer and
//      the page must stay untouched; without it the device writes there.
//
// The second page stays the process's own memory throughout (an unpin does
// not decommit), so on a machine with no IOMMU the stray write lands on
// nothing but this test.
//
//   zx-iommu-probe [N] [expect] [ro]
//                                 N: the controller's place among those
//                                 left to userspace (default 2);
//                                 expect: "blocked" or "reached";
//                                 ro: the second page stays pinned, but
//                                 for the device to read only, so the
//                                 write is a permission fault rather than
//                                 a missing page (QEMU's AMD IOMMU logs
//                                 only the first kind)
//
// Exit status: 0 when what happened is what was expected (or, with no
// expectation, when the pinned transfer worked), 1 otherwise, 2 when the
// gateway is off.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000
#define ZX_SYS_bti_pin 1
#define ZX_SYS_pmt_unpin 89
#define ZX_SYS_vmo_create_contiguous 193
#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_device_claim 0x4005
#define ECLIPSE_DEVICE_NVME 4
#define ZX_BTI_PERM_READ (1u << 0)
#define ZX_BTI_PERM_WRITE (1u << 1)
#define ZX_BTI_CONTIGUOUS (1u << 4)
#define PAGE 4096

// NVMe 1.4 controller registers.
#define NVME_CAP 0x00
#define NVME_CC 0x14
#define NVME_CSTS 0x1c
#define NVME_AQA 0x24
#define NVME_ASQ 0x28
#define NVME_ACQ 0x30

typedef uint32_t zx_handle_t;

static long zx(long n, long a, long b, long c, long d) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d);
}
static long zx8(long n, const long args[8]) {
    return syscall(ZX_GATEWAY_BASE + (n | ZX_GATEWAY_INDIRECT), args);
}
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t words[4];
};

static void *map_handle(zx_handle_t h, size_t len) {
    int fd = (int)zx_status(zx(ECLIPSE_SYS_handle_to_fd, h, 0, 0, 0));
    if (fd < 0) {
        printf("zx-iommu-probe: handle_to_fd: %d\n", fd);
        exit(1);
    }
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) {
        printf("zx-iommu-probe: mmap: errno %d\n", errno);
        exit(1);
    }
    return p;
}

// `pages` of contiguous memory pinned through `bti`: its device address in
// `*daddr`, its pin token in `*pmt`, mapped here.
static volatile uint8_t *dma(zx_handle_t bti, size_t pages, uint64_t *daddr, zx_handle_t *pmt,
                             uint32_t perms) {
    zx_handle_t vmo;
    long r = zx_status(zx(ZX_SYS_vmo_create_contiguous, bti, pages * PAGE, 0, (long)&vmo));
    if (r != 0) {
        printf("zx-iommu-probe: vmo_create_contiguous: %ld\n", r);
        exit(1);
    }
    long pin[8] = {bti, perms | ZX_BTI_CONTIGUOUS,
                   vmo, 0, pages * PAGE, (long)daddr, 1, (long)pmt};
    r = zx_status(zx8(ZX_SYS_bti_pin, pin));
    if (r != 0) {
        printf("zx-iommu-probe: bti_pin: %ld\n", r);
        exit(1);
    }
    volatile uint8_t *p = map_handle(vmo, pages * PAGE);
    memset((void *)p, 0, pages * PAGE);
    return p;
}

static volatile uint8_t *regs;
static uint32_t r32(size_t off) { return *(volatile uint32_t *)(regs + off); }
static uint64_t r64(size_t off) {
    return r32(off) | (uint64_t)r32(off + 4) << 32;
}
static void w32(size_t off, uint32_t v) { *(volatile uint32_t *)(regs + off) = v; }
static void w64(size_t off, uint64_t v) {
    w32(off, (uint32_t)v);
    w32(off + 4, (uint32_t)(v >> 32));
}

static int wait_ready(int want) {
    for (int i = 0; i < 2000; i++) {
        if ((r32(NVME_CSTS) & 1) == want)
            return 1;
        usleep(1000);
    }
    return 0;
}

static volatile uint32_t *sq, *cq;
static uint32_t sq_tail, cq_head, phase = 1, dstrd;

// Identify (CNS 1, the controller) into device address `prp`; the
// completion's status field (0 = success), or -1 when none came.
static int identify(uint16_t cid, uint64_t prp) {
    volatile uint32_t *cmd = sq + sq_tail * 16;
    for (int i = 0; i < 16; i++)
        cmd[i] = 0;
    cmd[0] = 0x06 | (uint32_t)cid << 16;
    cmd[6] = (uint32_t)prp;
    cmd[7] = (uint32_t)(prp >> 32);
    cmd[10] = 1;
    sq_tail = (sq_tail + 1) % 2;
    __sync_synchronize();
    w32(0x1000, sq_tail);
    for (int i = 0; i < 2000; i++) {
        volatile uint32_t *cqe = cq + cq_head * 4;
        if (((cqe[3] >> 16) & 1) == phase) {
            int status = (cqe[3] >> 17) & 0x7fff;
            cq_head = (cq_head + 1) % 2;
            if (cq_head == 0)
                phase ^= 1;
            w32(0x1000 + (4u << dstrd), cq_head);
            return status;
        }
        usleep(1000);
    }
    return -1;
}

static int all_zero(volatile uint8_t *p, size_t len) {
    for (size_t i = 0; i < len; i++)
        if (p[i])
            return 0;
    return 1;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    long index = argc > 1 ? atol(argv[1]) : 2;
    const char *expect = argc > 2 ? argv[2] : NULL;
    int read_only = argc > 3 && strcmp(argv[3], "ro") == 0;
    const uint32_t rw = ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE;
    struct claim c;
    long r = zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_NVME, index, (long)&c, 0));
    if (r == -ENOSYS) {
        printf("zx-iommu-probe: the gateway is off\n");
        return 2;
    }
    if (r != 0) {
        printf("zx-iommu-probe: claim nvme #%ld: status %ld\n", index, r);
        return 1;
    }
    size_t span = (c.offset + c.size + PAGE - 1) / PAGE * PAGE;
    regs = (volatile uint8_t *)map_handle(c.regs, span) + c.offset;
    dstrd = (uint32_t)(r64(NVME_CAP) >> 32) & 0xf;

    uint64_t queues_d, good_d, stray_d;
    zx_handle_t queues_pmt, good_pmt, stray_pmt;
    volatile uint8_t *queues = dma(c.bti, 2, &queues_d, &queues_pmt, rw);
    volatile uint8_t *good = dma(c.bti, 1, &good_d, &good_pmt, rw);
    volatile uint8_t *stray =
        dma(c.bti, 1, &stray_d, &stray_pmt, read_only ? ZX_BTI_PERM_READ : rw);
    // The stray page goes back: the device was never told about it, and
    // from here it is memory no pin names.
    if (!read_only) {
        r = zx_status(zx(ZX_SYS_pmt_unpin, stray_pmt, 0, 0, 0));
        if (r != 0) {
            printf("zx-iommu-probe: pmt_unpin: %ld\n", r);
            return 1;
        }
    }
    sq = (volatile uint32_t *)queues;
    cq = (volatile uint32_t *)(queues + PAGE);

    w32(NVME_CC, 0);
    if (!wait_ready(0)) {
        printf("zx-iommu-probe: the controller would not stop\n");
        return 1;
    }
    w32(NVME_AQA, 1 << 16 | 1);
    w64(NVME_ASQ, queues_d);
    w64(NVME_ACQ, queues_d + PAGE);
    w32(NVME_CC, 1 | 6 << 16 | 4 << 20);
    if (!wait_ready(1)) {
        printf("zx-iommu-probe: the controller would not start\n");
        return 1;
    }

    int ok = identify(1, good_d);
    uint16_t vid = good[0] | good[1] << 8;
    printf("zx-iommu-probe: identify into the pinned page %#llx: status %#x, vendor %#06x\n",
           (unsigned long long)good_d, ok, vid);
    int stray_status = identify(2, stray_d);
    int written = !all_zero(stray, PAGE);
    printf("zx-iommu-probe: identify into the %s page %#llx: status %#x, page %s\n",
           read_only ? "read-only" : "unpinned", (unsigned long long)stray_d, stray_status, written ? "WRITTEN" : "untouched");
    w32(NVME_CC, 0);
    wait_ready(0);

    const char *what = written ? "reached" : "blocked";
    printf("zx-iommu-probe: RESULT pinned=%s stray=%s\n", ok == 0 && vid ? "ok" : "FAILED",
           what);
    if (ok != 0 || !vid)
        return 1;
    if (expect && strcmp(expect, what) != 0)
        return 1;
    return 0;
}
