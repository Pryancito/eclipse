// zx-device-claim: take a disk the kernel left to userspace, and look at it.
//
// Boot with `udriver=virtio-blk@N` and a second virtio disk: the kernel
// leaves that disk alone, and eclipse_device_claim hands its registers and
// a BTI to whoever asks first. This checks the claim (once only, and only
// for a reserved device), maps the registers through an fd and reads the
// device's identity and capacity out of them, and makes DMA memory: a
// contiguous VMO, pinned through the BTI for its device address, mapped
// and written. It does not start the device; that is the driver's job.
//
//   zx-device-claim [N]  (default 0: the disk's place among virtio disks)
//
// Run it on Eclipse booted with `zircon.gateway=1`. Exit status: 0 on
// success, 2 when the gateway is off (ENOSYS), 1 on error.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L
// Arguments through a pointer to eight words (loader/src/linux.rs).
#define ZX_GATEWAY_INDIRECT 0x8000

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_bti_pin 1
#define ZX_SYS_pmt_unpin 89
#define ZX_SYS_vmo_create_contiguous 193

#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_device_claim 0x4005
#define ECLIPSE_DEVICE_VIRTIO_BLK 1
#define ECLIPSE_TRANSPORT_VIRTIO_MMIO 1

#define ZX_BTI_PERM_READ (1u << 0)
#define ZX_BTI_PERM_WRITE (1u << 1)
#define ZX_BTI_CONTIGUOUS (1u << 4)

#define ZX_ERR_NOT_FOUND (-25)
#define ZX_ERR_ALREADY_BOUND (-27)
#define ZX_ERR_INVALID_ARGS (-10)

// Legacy virtio-mmio registers (virtio 1.0 §4.2.4).
#define VIRTIO_MMIO_MAGIC 0x000
#define VIRTIO_MMIO_VERSION 0x004
#define VIRTIO_MMIO_DEVICE_ID 0x008
#define VIRTIO_MMIO_CONFIG 0x100
#define VIRTIO_MAGIC 0x74726976
#define VIRTIO_ID_BLOCK 2

#define PAGE 4096

typedef uint32_t zx_handle_t;

static long zx(long n, long a, long b, long c, long d) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d);
}

static long zx8(long n, const long args[8]) {
    return syscall(ZX_GATEWAY_BASE + (n | ZX_GATEWAY_INDIRECT), args);
}

// musl's syscall() turns a negative return into errno for the -4095..-1
// range, which is where every zx_status_t lives: undo that.
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("zx-device-claim: FAIL: %s\n", what);
        failures++;
    }
}

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t transport_words[4]; // zero for virtio-mmio
};

static long claim(long index, struct claim *c) {
    return zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_VIRTIO_BLK, index, (long)c, 0));
}

static void *map_handle(zx_handle_t h, size_t len) {
    int fd = (int)zx_status(zx(ECLIPSE_SYS_handle_to_fd, h, 0, 0, 0));
    if (fd < 0) {
        printf("zx-device-claim: handle_to_fd: %d\n", fd);
        exit(1);
    }
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED) {
        printf("zx-device-claim: mmap: errno %d\n", errno);
        exit(1);
    }
    return p;
}

static uint32_t reg32(volatile uint8_t *regs, size_t off) {
    return *(volatile uint32_t *)(regs + off);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    long index = argc > 1 ? atol(argv[1]) : 0;
    struct claim c;

    long r = claim(index + 1000, &c);
    if (r == -ENOSYS) {
        printf("zx-device-claim: the gateway is off (boot with zircon.gateway=1)\n");
        return 2;
    }
    check(r == ZX_ERR_NOT_FOUND, "a device nobody reserved is NOT_FOUND");
    check(zx_status(zx(ECLIPSE_SYS_device_claim, 99, index, (long)&c, 0)) == ZX_ERR_INVALID_ARGS,
          "an unknown kind is INVALID_ARGS");
    check(claim(index, NULL) == ZX_ERR_INVALID_ARGS, "a bad pointer is INVALID_ARGS");

    r = claim(index, &c);
    if (r != 0) {
        printf("zx-device-claim: claim virtio-blk #%ld: status %ld (boot with "
               "udriver=virtio-blk@%ld and a disk there)\n",
               index, r, index);
        return 1;
    }
    check(claim(index, &(struct claim){0}) == ZX_ERR_ALREADY_BOUND,
          "a second claim is ALREADY_BOUND");
    check(c.transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO, "the transport is virtio-mmio");
    check(!c.transport_words[0] && !c.transport_words[1] && !c.transport_words[2] &&
              !c.transport_words[3],
          "virtio-mmio needs no more words");
    printf("zx-device-claim: virtio-blk #%ld: window +%#llx size %#llx irq %d\n", index,
           (unsigned long long)c.offset, (unsigned long long)c.size, (int)c.irq);

    // The registers, read through the mapping.
    size_t span = (c.offset + c.size + PAGE - 1) / PAGE * PAGE;
    volatile uint8_t *regs = (volatile uint8_t *)map_handle(c.regs, span) + c.offset;
    uint32_t magic = reg32(regs, VIRTIO_MMIO_MAGIC);
    uint32_t version = reg32(regs, VIRTIO_MMIO_VERSION);
    uint32_t id = reg32(regs, VIRTIO_MMIO_DEVICE_ID);
    check(magic == VIRTIO_MAGIC, "the registers say virt");
    check(id == VIRTIO_ID_BLOCK, "the device is a block device");
    uint64_t sectors = reg32(regs, VIRTIO_MMIO_CONFIG) |
                       (uint64_t)reg32(regs, VIRTIO_MMIO_CONFIG + 4) << 32;
    printf("zx-device-claim: magic %#x version %u device %u, %llu sectors (%llu KiB)\n", magic,
           version, id, (unsigned long long)sectors, (unsigned long long)sectors / 2);
    check(sectors > 0, "the disk has a capacity");

    // DMA memory: contiguous, pinned for its device address, mapped.
    zx_handle_t dma;
    r = zx_status(zx(ZX_SYS_vmo_create_contiguous, c.bti, 4 * PAGE, 0, (long)&dma));
    check(r == 0, "vmo_create_contiguous with the BTI");
    if (r == 0) {
        uint64_t daddr = 0;
        zx_handle_t pmt = 0;
        long pin[8] = {c.bti, ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE | ZX_BTI_CONTIGUOUS,
                       dma,   0, 4 * PAGE, (long)&daddr, 1, (long)&pmt};
        r = zx_status(zx8(ZX_SYS_bti_pin, pin));
        check(r == 0 && daddr != 0 && daddr % PAGE == 0, "bti_pin gives a device address");
        printf("zx-device-claim: 16 KiB of DMA memory at device address %#llx\n",
               (unsigned long long)daddr);
        // Pin before mapping: handle_to_fd consumes the handle.
        volatile uint8_t *mem = map_handle(dma, 4 * PAGE);
        mem[0] = 0x5a;
        mem[4 * PAGE - 1] = 0xa5;
        check(mem[0] == 0x5a && mem[4 * PAGE - 1] == 0xa5, "the DMA memory maps and holds");
        if (pmt)
            check(zx_status(zx(ZX_SYS_pmt_unpin, pmt, 0, 0, 0)) == 0, "pmt_unpin");
    }

    if (failures == 0)
        printf("zx-device-claim: all checks passed\n");
    else
        printf("zx-device-claim: %d checks failed\n", failures);
    return failures != 0;
}
