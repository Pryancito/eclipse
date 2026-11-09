// zx-virtio-blk: a virtio block driver in userspace.
//
// The disk is one the kernel left alone (`udriver=virtio-blk@N`), claimed
// with eclipse_device_claim: its registers come as a physical VMO, mapped
// through an fd, and its DMA memory is a contiguous VMO pinned through the
// BTI the claim hands out. The driver brings the device up on either
// transport -- legacy virtio-mmio (aarch64, riscv64 under QEMU virt) or
// modern virtio-pci (x86_64 q35) -- sets up one split virtqueue and
// polls it, as the kernel's own virtio drivers do.
//
// It writes a pattern over the last sectors of the disk, reads it back
// and checks it, then times reads: one sector at a time and 4 KiB at a
// time, over the whole disk. The disk's old contents in that tail are
// lost, so give it a scratch image.
//
//   zx-virtio-blk [N [ROUNDS]]  (defaults: disk 0, 2 passes)
//   zx-virtio-blk /dev/vdX [OFFSET_MIB]
//
// The second form is the baseline: the same reads through the kernel's
// driver, with pread on its block device, starting OFFSET_MIB into it
// (default 0). The first pass is what reaches the device (the kernel
// caches what it reads, and reads ahead); the second is served from the
// cache.
//
// Run it on Eclipse booted with `zircon.gateway=1` and the udriver
// option. Exit status: 0 on success, 2 when the gateway is off, 1 on error.
//
// A prototype: one queue, one request in flight, no interrupts yet.

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

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_bti_pin 1
#define ZX_SYS_vmo_create_contiguous 193

#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_device_claim 0x4005
#define ECLIPSE_DEVICE_VIRTIO_BLK 1
#define ECLIPSE_TRANSPORT_VIRTIO_MMIO 1
#define ECLIPSE_TRANSPORT_VIRTIO_PCI 2

#define ZX_BTI_PERM_READ (1u << 0)
#define ZX_BTI_PERM_WRITE (1u << 1)
#define ZX_BTI_CONTIGUOUS (1u << 4)

#define PAGE 4096
#define SECTOR 512

// Device status bits (virtio 1.1 §2.1).
#define STATUS_ACKNOWLEDGE 1
#define STATUS_DRIVER 2
#define STATUS_DRIVER_OK 4
#define STATUS_FEATURES_OK 8
#define STATUS_FAILED 128

// Legacy virtio-mmio registers (virtio 1.1 §4.2.4).
#define MMIO_MAGIC 0x000
#define MMIO_VERSION 0x004
#define MMIO_DEVICE_ID 0x008
#define MMIO_HOST_FEATURES 0x010
#define MMIO_GUEST_FEATURES 0x020
#define MMIO_GUEST_PAGE_SIZE 0x028
#define MMIO_QUEUE_SEL 0x030
#define MMIO_QUEUE_NUM_MAX 0x034
#define MMIO_QUEUE_NUM 0x038
#define MMIO_QUEUE_ALIGN 0x03c
#define MMIO_QUEUE_PFN 0x040
#define MMIO_QUEUE_NOTIFY 0x050
#define MMIO_INTERRUPT_STATUS 0x060
#define MMIO_INTERRUPT_ACK 0x064
#define MMIO_STATUS 0x070
#define MMIO_CONFIG 0x100

// Modern virtio-pci common configuration (virtio 1.1 §4.1.4.3).
#define PCI_DEVICE_FEATURE_SELECT 0x00
#define PCI_DEVICE_FEATURE 0x04
#define PCI_DRIVER_FEATURE_SELECT 0x08
#define PCI_DRIVER_FEATURE 0x0c
#define PCI_DEVICE_STATUS 0x14
#define PCI_QUEUE_SELECT 0x16
#define PCI_QUEUE_SIZE 0x18
#define PCI_QUEUE_MSIX_VECTOR 0x1a
#define PCI_QUEUE_ENABLE 0x1c
#define PCI_QUEUE_NOTIFY_OFF 0x1e
#define PCI_QUEUE_DESC 0x20
#define PCI_QUEUE_DRIVER 0x28
#define PCI_QUEUE_DEVICE 0x30
#define VIRTIO_F_VERSION_1_HI 1 // bit 32: bit 0 of feature word 1
#define VIRTIO_MSI_NO_VECTOR 0xffff

// virtio-blk requests (virtio 1.1 §5.2.6).
#define VIRTIO_BLK_T_IN 0
#define VIRTIO_BLK_T_OUT 1
#define VIRTIO_BLK_S_OK 0

// Split virtqueue (virtio 1.1 §2.6).
#define QUEUE_SIZE 8
#define VIRTQ_DESC_F_NEXT 1
#define VIRTQ_DESC_F_WRITE 2

struct virtq_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags, next;
};
struct virtq_avail {
    uint16_t flags, idx, ring[QUEUE_SIZE], used_event;
};
struct virtq_used_elem {
    uint32_t id, len;
};
struct virtq_used {
    uint16_t flags, idx;
    struct virtq_used_elem ring[QUEUE_SIZE];
    uint16_t avail_event;
};
struct blk_req {
    uint32_t type, reserved;
    uint64_t sector;
};

// The DMA area, one contiguous VMO: the queue in the first two pages laid
// out as legacy virtio wants it (descriptors and available ring, then the
// used ring on the next page), a page of request header and status, then
// the data buffer.
#define DMA_QUEUE 0
#define DMA_USED PAGE
#define DMA_REQ (2 * PAGE)
#define DMA_DATA (3 * PAGE)
#define DATA_MAX (64 * 1024)
#define DMA_SIZE (DMA_DATA + DATA_MAX)

typedef uint32_t zx_handle_t;

static long zx(long n, long a, long b, long c, long d) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d);
}
static long zx8(long n, const long args[8]) {
    return syscall(ZX_GATEWAY_BASE + (n | ZX_GATEWAY_INDIRECT), args);
}
static long zx_status(long r) { return r == -1 ? -(long)errno : r; }

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void die(const char *what, long v) {
    printf("zx-virtio-blk: %s: %ld\n", what, v);
    exit(1);
}

static void *map_handle(zx_handle_t h, size_t len) {
    int fd = (int)zx_status(zx(ECLIPSE_SYS_handle_to_fd, h, 0, 0, 0));
    if (fd < 0)
        die("handle_to_fd", fd);
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (p == MAP_FAILED)
        die("mmap", errno);
    return p;
}

#define mb() __sync_synchronize()

static volatile uint8_t *regs; // the transport's window
static uint32_t transport;
static uint32_t pci_common, pci_notify, pci_device, pci_mult;
static volatile uint8_t *notify_reg;

static uint8_t *dma;   // CPU view
static uint64_t daddr; // device view
static struct virtq_desc *desc;
static struct virtq_avail *avail;
static volatile struct virtq_used *used;
static uint16_t last_used;

static inline uint32_t rd32(uint32_t off) { return *(volatile uint32_t *)(regs + off); }
static inline uint16_t rd16(uint32_t off) { return *(volatile uint16_t *)(regs + off); }
static inline uint8_t rd8(uint32_t off) { return *(volatile uint8_t *)(regs + off); }
static inline void wr32(uint32_t off, uint32_t v) { *(volatile uint32_t *)(regs + off) = v; }
static inline void wr16(uint32_t off, uint16_t v) { *(volatile uint16_t *)(regs + off) = v; }
static inline void wr8(uint32_t off, uint8_t v) { *(volatile uint8_t *)(regs + off) = v; }

static uint64_t capacity;

static void init_mmio(void) {
    if (rd32(MMIO_MAGIC) != 0x74726976 || rd32(MMIO_VERSION) != 1 || rd32(MMIO_DEVICE_ID) != 2)
        die("not a legacy virtio-mmio block device, version", rd32(MMIO_VERSION));
    wr32(MMIO_STATUS, 0);
    wr32(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    (void)rd32(MMIO_HOST_FEATURES);
    wr32(MMIO_GUEST_FEATURES, 0);
    wr32(MMIO_GUEST_PAGE_SIZE, PAGE);
    wr32(MMIO_QUEUE_SEL, 0);
    uint32_t max = rd32(MMIO_QUEUE_NUM_MAX);
    if (max < QUEUE_SIZE)
        die("queue too small", max);
    wr32(MMIO_QUEUE_NUM, QUEUE_SIZE);
    wr32(MMIO_QUEUE_ALIGN, PAGE);
    wr32(MMIO_QUEUE_PFN, (uint32_t)((daddr + DMA_QUEUE) / PAGE));
    capacity = rd32(MMIO_CONFIG) | (uint64_t)rd32(MMIO_CONFIG + 4) << 32;
    wr32(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    notify_reg = regs + MMIO_QUEUE_NOTIFY;
}

static void init_pci(void) {
    uint32_t c = pci_common;
    wr8(c + PCI_DEVICE_STATUS, 0);
    while (rd8(c + PCI_DEVICE_STATUS) != 0)
        ;
    wr8(c + PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    wr32(c + PCI_DEVICE_FEATURE_SELECT, 1);
    if (!(rd32(c + PCI_DEVICE_FEATURE) & VIRTIO_F_VERSION_1_HI))
        die("device without VIRTIO_F_VERSION_1", 0);
    wr32(c + PCI_DRIVER_FEATURE_SELECT, 0);
    wr32(c + PCI_DRIVER_FEATURE, 0);
    wr32(c + PCI_DRIVER_FEATURE_SELECT, 1);
    wr32(c + PCI_DRIVER_FEATURE, VIRTIO_F_VERSION_1_HI);
    wr8(c + PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if (!(rd8(c + PCI_DEVICE_STATUS) & STATUS_FEATURES_OK))
        die("device refused the features", rd8(c + PCI_DEVICE_STATUS));
    wr16(c + PCI_QUEUE_SELECT, 0);
    uint16_t max = rd16(c + PCI_QUEUE_SIZE);
    if (max < QUEUE_SIZE)
        die("queue too small", max);
    wr16(c + PCI_QUEUE_SIZE, QUEUE_SIZE);
    wr16(c + PCI_QUEUE_MSIX_VECTOR, VIRTIO_MSI_NO_VECTOR);
    uint64_t d = daddr + DMA_QUEUE;
    uint64_t a = d + QUEUE_SIZE * sizeof(struct virtq_desc);
    uint64_t u = daddr + DMA_USED;
    wr32(c + PCI_QUEUE_DESC, (uint32_t)d);
    wr32(c + PCI_QUEUE_DESC + 4, (uint32_t)(d >> 32));
    wr32(c + PCI_QUEUE_DRIVER, (uint32_t)a);
    wr32(c + PCI_QUEUE_DRIVER + 4, (uint32_t)(a >> 32));
    wr32(c + PCI_QUEUE_DEVICE, (uint32_t)u);
    wr32(c + PCI_QUEUE_DEVICE + 4, (uint32_t)(u >> 32));
    uint16_t notify_off = rd16(c + PCI_QUEUE_NOTIFY_OFF);
    wr16(c + PCI_QUEUE_ENABLE, 1);
    capacity = rd32(pci_device) | (uint64_t)rd32(pci_device + 4) << 32;
    wr8(c + PCI_DEVICE_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    notify_reg = regs + pci_notify + (uint32_t)notify_off * pci_mult;
}

// One request, start to end: three descriptors (header, data, status),
// one notify, then a spin on the used ring.
static int do_io(uint32_t type, uint64_t sector, size_t len) {
    struct blk_req *req = (struct blk_req *)(dma + DMA_REQ);
    volatile uint8_t *status = dma + DMA_REQ + sizeof *req;
    req->type = type;
    req->reserved = 0;
    req->sector = sector;
    *status = 0xff;
    desc[0] = (struct virtq_desc){daddr + DMA_REQ, sizeof *req, VIRTQ_DESC_F_NEXT, 1};
    desc[1] = (struct virtq_desc){daddr + DMA_DATA, (uint32_t)len,
                                  VIRTQ_DESC_F_NEXT |
                                      (type == VIRTIO_BLK_T_IN ? VIRTQ_DESC_F_WRITE : 0),
                                  2};
    desc[2] = (struct virtq_desc){daddr + DMA_REQ + sizeof *req, 1, VIRTQ_DESC_F_WRITE, 0};
    avail->ring[avail->idx % QUEUE_SIZE] = 0;
    mb();
    avail->idx++;
    mb();
    // virtio-mmio registers take 32-bit accesses only; QEMU drops a 16-bit
    // store to QueueNotify. The PCI notify register is 16 bits wide.
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO)
        *(volatile uint32_t *)notify_reg = 0;
    else
        *(volatile uint16_t *)notify_reg = 0;
    while (used->idx == last_used)
        ;
    mb();
    last_used++;
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO) {
        // Nobody takes the interrupt, but the line stays up until acked.
        uint32_t isr = rd32(MMIO_INTERRUPT_STATUS);
        if (isr)
            wr32(MMIO_INTERRUPT_ACK, isr);
    }
    return *status == VIRTIO_BLK_S_OK ? 0 : -1;
}

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t transport_words[4];
};

static double time_reads(size_t chunk, uint64_t sectors, int rounds) {
    uint64_t per = chunk / SECTOR, n = 0;
    uint64_t t0 = now_ns();
    for (int r = 0; r < rounds; r++)
        for (uint64_t s = 0; s + per <= sectors; s += per, n++)
            if (do_io(VIRTIO_BLK_T_IN, s, chunk))
                die("read failed at sector", (long)s);
    return (double)(now_ns() - t0) / n;
}

#include <fcntl.h>

static double time_preads(int fd, size_t chunk, off_t base, size_t span) {
    static char buf[4096];
    uint64_t n = 0, t0 = now_ns();
    for (off_t off = 0; off + (off_t)chunk <= (off_t)span; off += chunk, n++)
        if (pread(fd, buf, chunk, base + off) != (ssize_t)chunk)
            die("pread failed, errno", errno);
    return (double)(now_ns() - t0) / n;
}

// The kernel's driver, through its block device: a cold pass over one MiB
// and a warm one, each 512 bytes and then 4 KiB at a time (on separate
// MiBs, so the 4 KiB pass is cold too).
static int kernel_baseline(const char *path, long offset_mib) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        die("open the block device, errno", errno);
    off_t base = (off_t)offset_mib << 20;
    double one_cold = time_preads(fd, SECTOR, base, 1 << 20);
    double one_warm = time_preads(fd, SECTOR, base, 1 << 20);
    double four_cold = time_preads(fd, 4096, base + (1 << 20), 1 << 20);
    double four_warm = time_preads(fd, 4096, base + (1 << 20), 1 << 20);
    printf("zx-virtio-blk: kernel %s read 512 B  %8.1f us cold, %8.1f us cached\n", path,
           one_cold / 1000, one_warm / 1000);
    printf("zx-virtio-blk: kernel %s read 4 KiB  %8.1f us cold, %8.1f us cached\n", path,
           four_cold / 1000, four_warm / 1000);
    printf("zx-virtio-blk: all checks passed\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && argv[1][0] == '/')
        return kernel_baseline(argv[1], argc > 2 ? atol(argv[2]) : 0);
    long index = argc > 1 ? atol(argv[1]) : 0;
    int rounds = argc > 2 ? atoi(argv[2]) : 2;
    struct claim c;
    long r = zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_VIRTIO_BLK, index, (long)&c, 0));
    if (r == -ENOSYS) {
        printf("zx-virtio-blk: the gateway is off (boot with zircon.gateway=1)\n");
        return 2;
    }
    if (r != 0)
        die("claim (boot with udriver=virtio-blk@N and a disk there)", r);
    transport = c.transport;
    size_t span = (c.offset + c.size + PAGE - 1) / PAGE * PAGE;
    regs = (volatile uint8_t *)map_handle(c.regs, span) + c.offset;

    zx_handle_t vmo;
    r = zx_status(zx(ZX_SYS_vmo_create_contiguous, c.bti, DMA_SIZE, 0, (long)&vmo));
    if (r != 0)
        die("vmo_create_contiguous", r);
    zx_handle_t pmt;
    long pin[8] = {c.bti, ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE | ZX_BTI_CONTIGUOUS,
                   vmo,   0, DMA_SIZE, (long)&daddr, 1, (long)&pmt};
    r = zx_status(zx8(ZX_SYS_bti_pin, pin));
    if (r != 0)
        die("bti_pin", r);
    dma = map_handle(vmo, DMA_SIZE);
    memset(dma, 0, DMA_SIZE);
    desc = (struct virtq_desc *)(dma + DMA_QUEUE);
    avail = (struct virtq_avail *)(dma + DMA_QUEUE + QUEUE_SIZE * sizeof(struct virtq_desc));
    used = (volatile struct virtq_used *)(dma + DMA_USED);

    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO) {
        init_mmio();
    } else if (transport == ECLIPSE_TRANSPORT_VIRTIO_PCI) {
        pci_common = (uint32_t)c.transport_words[0];
        pci_notify = (uint32_t)(c.transport_words[0] >> 32);
        pci_device = (uint32_t)(c.transport_words[1] >> 32);
        pci_mult = (uint32_t)c.transport_words[2];
        init_pci();
    } else {
        die("unknown transport", transport);
    }
    printf("zx-virtio-blk: disk #%ld up (%s), %llu sectors, DMA at %#llx\n", index,
           transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO ? "virtio-mmio" : "virtio-pci",
           (unsigned long long)capacity, (unsigned long long)daddr);
    if (capacity < 64)
        die("disk too small, sectors", (long)capacity);

    // Write a pattern over the last 32 sectors, one 16 KiB request, then
    // read it back a sector at a time and in one piece.
    uint64_t tail = capacity - 32;
    for (size_t i = 0; i < 32 * SECTOR; i++)
        dma[DMA_DATA + i] = (uint8_t)(i * 7 + (i >> 9));
    if (do_io(VIRTIO_BLK_T_OUT, tail, 32 * SECTOR))
        die("write failed", 0);
    int bad = 0;
    for (int s = 0; s < 32; s++) {
        memset(dma + DMA_DATA, 0, SECTOR);
        if (do_io(VIRTIO_BLK_T_IN, tail + s, SECTOR))
            die("read failed", s);
        for (int i = 0; i < SECTOR; i++) {
            size_t at = (size_t)s * SECTOR + i;
            if (dma[DMA_DATA + i] != (uint8_t)(at * 7 + (at >> 9)))
                bad++;
        }
    }
    memset(dma + DMA_DATA, 0, 32 * SECTOR);
    if (do_io(VIRTIO_BLK_T_IN, tail, 32 * SECTOR))
        die("read failed", 32);
    for (size_t i = 0; i < 32 * SECTOR; i++)
        if (dma[DMA_DATA + i] != (uint8_t)(i * 7 + (i >> 9)))
            bad++;
    if (bad) {
        printf("zx-virtio-blk: FAIL: %d bytes read back wrong\n", bad);
        return 1;
    }
    printf("zx-virtio-blk: 16 KiB written and read back right\n");

    uint64_t sectors = capacity < 2048 ? capacity : 2048; // the first MiB
    double one = time_reads(SECTOR, sectors, rounds);
    double four = time_reads(4096, sectors, rounds);
    printf("zx-virtio-blk: read 512 B  %8.1f us per request\n", one / 1000);
    printf("zx-virtio-blk: read 4 KiB  %8.1f us per request (%.1f MiB/s)\n", four / 1000,
           4096.0 / four * 1e9 / (1 << 20));
    printf("zx-virtio-blk: all checks passed\n");
    return 0;
}
