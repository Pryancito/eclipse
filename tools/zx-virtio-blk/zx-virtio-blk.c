// zx-virtio-blk: a virtio block driver in userspace.
//
// The disk is one the kernel left alone (`udriver=virtio-blk@N`), claimed
// with eclipse_device_claim: its registers come as a physical VMO, mapped
// through an fd, its DMA memory is a contiguous VMO pinned through the
// BTI the claim hands out, and its interrupt is an interrupt object. The
// driver brings the device up on either transport -- legacy virtio-mmio
// (aarch64, riscv64 under QEMU virt) or modern virtio-pci (x86_64 q35,
// MSI-X vector 0) -- and sets up one split virtqueue. It waits for each
// request with zx_interrupt_wait, and times reads that way and by
// polling the used ring, as the kernel's own virtio drivers do.
//
// It writes a pattern over the last sectors of the disk, reads it back
// and checks it, then times reads: one sector at a time and 4 KiB at a
// time, over the whole disk. The disk's old contents in that tail are
// lost, so give it a scratch image.
//
//   zx-virtio-blk [N [ROUNDS]]  (defaults: disk 0, 2 passes)
//   zx-virtio-blk serve [N [ROUNDS]]
//   zx-virtio-blk /dev/vdX [OFFSET_MIB]
//
// With `serve` the driver goes on to serve the disk to another process,
// over a FIFO and a window of its DMA memory, and that process checks and
// times it (see "Serving another process" below). The third form is the baseline: the same reads through the kernel's
// driver, with pread on its block device, starting OFFSET_MIB into it
// (default 0). The first pass is what reaches the device (the kernel
// caches what it reads, and reads ahead); the second is served from the
// cache.
//
// Run it on Eclipse booted with `zircon.gateway=1` and the udriver
// option. Exit status: 0 on success, 2 when the gateway is off, 1 on error.
//
// A prototype: one queue, one request in flight, one client.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000

// zircon-syscall/src/zx-syscall-numbers.h
#define ZX_SYS_bti_pin 1
#define ZX_SYS_interrupt_wait 54
#define ZX_SYS_handle_close 45
#define ZX_SYS_vmo_create_contiguous 193
#define ZX_SYS_vmo_create_child 192
#define ZX_SYS_fifo_create 34
#define ZX_SYS_fifo_read 35
#define ZX_SYS_fifo_write 36
#define ZX_SYS_object_wait_one 81
#define ZX_VMO_CHILD_SLICE (1u << 3)
#define ZX_FIFO_READABLE (1u << 0)
#define ZX_FIFO_PEER_CLOSED (1u << 2)
#define ZX_TIME_INFINITE INT64_MAX
#define ZX_ERR_SHOULD_WAIT (-22)
#define ZX_ERR_PEER_CLOSED (-24)
#define ECLIPSE_SYS_fd_to_handle 0x4001
#define ECLIPSE_FD_TAKE 1

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
static long zx6(long n, long a, long b, long c, long d, long e, long f) {
    return syscall(ZX_GATEWAY_BASE + n, a, b, c, d, e, f);
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
static zx_handle_t irq_handle; // 0: none, poll
static int use_irq;            // wait on it, or poll anyway
static uint64_t irq_waits;     // waits that came back, for the report
// Where an interrupt-driven request's time goes, summed over the requests
// that slept: notify to the kernel's interrupt timestamp (the device and
// the controller), and that timestamp to the wait's return (the wake).
static uint64_t slept, to_irq_ns, to_wake_ns;

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
    // MSI-X vector 0 is the one the kernel armed, and the one the claim's
    // interrupt object listens on. The device answers NO_VECTOR when it
    // cannot take it; then there is nothing to wait on.
    wr16(c + PCI_QUEUE_MSIX_VECTOR, irq_handle ? 0 : VIRTIO_MSI_NO_VECTOR);
    if (irq_handle && rd16(c + PCI_QUEUE_MSIX_VECTOR) != 0) {
        printf("zx-virtio-blk: the device refused MSI-X vector 0; polling\n");
        irq_handle = 0;
    }
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

// Quiet the device's interrupt. virtio-mmio holds its line up until the
// status is acked, and the kernel unmasks the line on the next wait; an
// MSI-X message needs nothing.
static void ack_device(void) {
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO) {
        uint32_t isr = rd32(MMIO_INTERRUPT_STATUS);
        if (isr)
            wr32(MMIO_INTERRUPT_ACK, isr);
    }
}

// One request, start to end: three descriptors (header, data, status),
// one notify, then either waits on the interrupt or a spin on the used
// ring until the device has put it there.
static int do_io_at(uint32_t type, uint64_t sector, size_t len, size_t off) {
    struct blk_req *req = (struct blk_req *)(dma + DMA_REQ);
    volatile uint8_t *status = dma + DMA_REQ + sizeof *req;
    req->type = type;
    req->reserved = 0;
    req->sector = sector;
    *status = 0xff;
    desc[0] = (struct virtq_desc){daddr + DMA_REQ, sizeof *req, VIRTQ_DESC_F_NEXT, 1};
    desc[1] = (struct virtq_desc){daddr + DMA_DATA + off, (uint32_t)len,
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
    uint64_t t_notify = now_ns();
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO)
        *(volatile uint32_t *)notify_reg = 0;
    else
        *(volatile uint16_t *)notify_reg = 0;
    if (use_irq) {
        // A wake is not a completion (a stale one from a polled request, a
        // config change): look at the ring after each.
        while (used->idx == last_used) {
            int64_t ts = 0;
            long r = zx_status(zx(ZX_SYS_interrupt_wait, irq_handle, (long)&ts, 0, 0));
            if (r != 0)
                die("interrupt_wait", r);
            uint64_t t_back = now_ns();
            irq_waits++;
            // Only a wake for this request says anything about it: a stale
            // one carries a timestamp from before the notify.
            if ((uint64_t)ts >= t_notify && (uint64_t)ts <= t_back) {
                slept++;
                to_irq_ns += (uint64_t)ts - t_notify;
                to_wake_ns += t_back - (uint64_t)ts;
            }
            ack_device();
        }
    } else {
        while (used->idx == last_used)
            ;
        // Nobody waits on the interrupt, but the line stays up until acked.
        ack_device();
    }
    mb();
    last_used++;
    return *status == VIRTIO_BLK_S_OK ? 0 : -1;
}

static int do_io(uint32_t type, uint64_t sector, size_t len) {
    return do_io_at(type, sector, len, 0);
}

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t transport_words[3];
    uint64_t interrupt;
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

// ---- Serving another process ------------------------------------------
//
// `zx-virtio-blk serve N` brings the disk up as above and then serves it
// to a client: a process of its own (this program again, exec'd with
// `client`) that holds no device, no BTI and none of the driver's memory,
// only two descriptors. One is the data window, a slice of the driver's
// DMA memory that the device reads and writes directly -- no copy on
// either side. The other is one end of a FIFO of 32-byte messages, the
// shape of Zircon's block FIFO: a request names an operation, a range of
// sectors and where in the window the data is; the same message comes
// back with a status.
//
// Each side either sleeps on the FIFO (zx_object_wait_one) or spins on
// zx_fifo_read; a MODE message from the client picks how the driver
// waits, on the FIFO and on the device, so that one run times every
// combination.

#define BLK_OP_READ 1
#define BLK_OP_WRITE 2
#define BLK_OP_MODE 3 // sector: BLK_MODE_* bits; no data
#define BLK_MODE_SPIN 1 // the driver spins on the FIFO instead of sleeping
#define BLK_MODE_IRQ 2  // the driver sleeps on the device's interrupt
#define FIFO_DEPTH 16

struct blk_msg {
    uint32_t op, reqid;
    uint32_t sectors, buf_off; // buf_off: bytes into the data window
    uint64_t sector;
    int64_t status; // 0 or a negative zx_status_t, on the way back
};
_Static_assert(sizeof(struct blk_msg) == 32, "a FIFO element is 32 bytes");

#define ZX_ERR_INVALID_ARGS (-10)
#define ZX_ERR_OUT_OF_RANGE (-14)
#define ZX_ERR_IO (-40)

static long fifo_read(zx_handle_t f, struct blk_msg *m, size_t n, size_t *got) {
    return zx_status(zx6(ZX_SYS_fifo_read, f, sizeof *m, (long)m, n, (long)got, 0));
}
static long fifo_write(zx_handle_t f, const struct blk_msg *m, size_t n, size_t *put) {
    return zx_status(zx6(ZX_SYS_fifo_write, f, sizeof *m, (long)m, n, (long)put, 0));
}

// Wait for something to read on `f`, sleeping or spinning; 0 once there is,
// -1 once the other side has gone.
static int fifo_wait(zx_handle_t f, int spin, struct blk_msg *m, size_t n, size_t *got) {
    for (;;) {
        long r = fifo_read(f, m, n, got);
        if (r == 0)
            return 0;
        if (r != ZX_ERR_SHOULD_WAIT)
            return -1;
        if (spin)
            continue;
        uint32_t observed = 0;
        r = zx_status(zx6(ZX_SYS_object_wait_one, f, ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED,
                          ZX_TIME_INFINITE, (long)&observed, 0, 0));
        if (r < 0 || !(observed & ZX_FIFO_READABLE))
            return -1;
    }
}

static int64_t serve_one(struct blk_msg *m, int *spin) {
    if (m->op == BLK_OP_MODE) {
        *spin = (m->sector & BLK_MODE_SPIN) != 0;
        use_irq = (m->sector & BLK_MODE_IRQ) && irq_handle;
        return 0;
    }
    if (m->op != BLK_OP_READ && m->op != BLK_OP_WRITE)
        return ZX_ERR_INVALID_ARGS;
    uint64_t len = (uint64_t)m->sectors * SECTOR;
    // The client's word, all of it: nothing reaches the device that does
    // not fit the window and the disk.
    if (m->sectors == 0 || m->buf_off % SECTOR || len > DATA_MAX ||
        m->buf_off > DATA_MAX - len)
        return ZX_ERR_OUT_OF_RANGE;
    if (m->sector > capacity || m->sectors > capacity - m->sector)
        return ZX_ERR_OUT_OF_RANGE;
    uint32_t type = m->op == BLK_OP_READ ? VIRTIO_BLK_T_IN : VIRTIO_BLK_T_OUT;
    return do_io_at(type, m->sector, len, m->buf_off) ? ZX_ERR_IO : 0;
}

// Serve `f` until the client closes it. Returns the requests served.
static uint64_t serve(zx_handle_t f) {
    struct blk_msg m[FIFO_DEPTH];
    int spin = 0;
    uint64_t served = 0;
    size_t got;
    use_irq = 0;
    while (fifo_wait(f, spin, m, FIFO_DEPTH, &got) == 0) {
        for (size_t i = 0; i < got; i++)
            m[i].status = serve_one(&m[i], &spin);
        served += got;
        size_t put = 0;
        // The client never has more than FIFO_DEPTH outstanding, so the
        // replies always fit.
        if (fifo_write(f, m, got, &put) != 0 || put != got)
            break;
    }
    return served;
}

// The client side: the FIFO and the window come as descriptors.
static zx_handle_t cfifo;
static uint8_t *window;
static uint32_t next_id;

static int64_t client_call(uint32_t op, uint64_t sector, uint32_t sectors, uint32_t off,
                           int spin) {
    struct blk_msg m = {op, ++next_id, sectors, off, sector, 0};
    size_t n = 0;
    if (fifo_write(cfifo, &m, 1, &n) != 0 || n != 1)
        die("fifo_write", 0);
    struct blk_msg back;
    if (fifo_wait(cfifo, spin, &back, 1, &n) != 0)
        die("the driver went away", 0);
    if (back.reqid != m.reqid)
        die("a reply to another request", back.reqid);
    return back.status;
}

static double client_time(uint32_t sectors, uint64_t span, int rounds, int spin) {
    uint64_t n = 0, t0 = now_ns();
    for (int r = 0; r < rounds; r++)
        for (uint64_t s = 0; s + sectors <= span; s += sectors, n++)
            if (client_call(BLK_OP_READ, s, sectors, 0, spin))
                die("served read failed at sector", (long)s);
    return (double)(now_ns() - t0) / n;
}

static int client_main(int fifo_fd, int data_fd, uint64_t disk_sectors, int rounds) {
    if (zx_status(zx(ECLIPSE_SYS_fd_to_handle, fifo_fd, ECLIPSE_FD_TAKE, (long)&cfifo, 0)) < 0)
        die("client: fd_to_handle", errno);
    close(fifo_fd);
    window = mmap(NULL, DATA_MAX, PROT_READ | PROT_WRITE, MAP_SHARED, data_fd, 0);
    close(data_fd);
    if (window == MAP_FAILED)
        die("client: mmap the window", errno);

    // What the driver wrote over the tail, read back through it: a sector
    // at a time into different places in the window, then in one piece.
    uint64_t tail = disk_sectors - 32;
    int bad = 0;
    memset(window, 0, DATA_MAX);
    for (int s = 0; s < 32; s++)
        if (client_call(BLK_OP_READ, tail + s, 1, (uint32_t)(s * SECTOR), 0))
            die("client: read failed", s);
    for (size_t i = 0; i < 32 * SECTOR; i++)
        if (window[i] != (uint8_t)(i * 7 + (i >> 9)))
            bad++;
    // A write from the window, and the read that brings it back.
    for (size_t i = 0; i < 8 * SECTOR; i++)
        window[DATA_MAX / 2 + i] = (uint8_t)(i * 13 + 5);
    if (client_call(BLK_OP_WRITE, tail, 8, DATA_MAX / 2, 0))
        die("client: write failed", 0);
    memset(window, 0, 8 * SECTOR);
    if (client_call(BLK_OP_READ, tail, 8, 0, 0))
        die("client: read failed", 0);
    for (size_t i = 0; i < 8 * SECTOR; i++)
        if (window[i] != (uint8_t)(i * 13 + 5))
            bad++;
    if (bad) {
        printf("zx-virtio-blk: FAIL: %d bytes served wrong\n", bad);
        return 1;
    }
    // And what the driver must refuse rather than hand to the device.
    if (client_call(BLK_OP_READ, disk_sectors - 1, 2, 0, 0) != ZX_ERR_OUT_OF_RANGE ||
        client_call(BLK_OP_READ, 0, 1, DATA_MAX, 0) != ZX_ERR_OUT_OF_RANGE ||
        client_call(BLK_OP_READ, 0, 1, 100, 0) != ZX_ERR_OUT_OF_RANGE ||
        client_call(BLK_OP_READ, 0, 0, 0, 0) != ZX_ERR_OUT_OF_RANGE ||
        client_call(9, 0, 1, 0, 0) != ZX_ERR_INVALID_ARGS)
        die("client: a bad request was not refused", 0);
    printf("zx-virtio-blk: served from another process: reads, a write, bad requests refused\n");

    uint64_t span = disk_sectors < 2048 ? disk_sectors : 2048;
    static const struct {
        uint32_t mode;
        int spin;
        const char *name;
    } modes[] = {
        {BLK_MODE_IRQ, 0, "all asleep (irq, FIFO)"},
        {0, 0, "device polled, FIFO asleep"},
        {BLK_MODE_SPIN, 1, "all spinning"},
    };
    for (size_t i = 0; i < sizeof modes / sizeof modes[0]; i++) {
        if (client_call(BLK_OP_MODE, modes[i].mode, 0, 0, 0))
            die("client: mode refused", (long)i);
        double one = client_time(1, span, rounds, modes[i].spin);
        double four = client_time(8, span, rounds, modes[i].spin);
        printf("zx-virtio-blk: served, %-28s read 512 B %7.1f us, 4 KiB %7.1f us\n",
               modes[i].name, one / 1000, four / 1000);
        // Back to sleeping before the next mode, or a spinning driver would
        // spin through the client's next MODE message's wake.
        if (modes[i].spin && client_call(BLK_OP_MODE, 0, 0, 0, 1))
            die("client: mode refused", (long)i);
    }
    return 0;
}

// Hand the disk to a client process and serve it until the client is done.
//
// `data_fd` is the data window, cut from the DMA memory before that was
// mapped (mapping it takes the handle).
static int serve_client(const char *self, int data_fd, int rounds) {
    zx_handle_t ends[2];
    long r = zx_status(zx6(ZX_SYS_fifo_create, FIFO_DEPTH, sizeof(struct blk_msg), 0, (long)&ends[0],
                      (long)&ends[1], 0));
    if (r != 0)
        die("fifo_create", r);
    long fifo_fd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, ends[1], 0, 0, 0));
    if (fifo_fd < 0)
        die("handle_to_fd (fifo)", fifo_fd);
    char a_fifo[16], a_data[16], a_cap[24], a_rounds[16];
    snprintf(a_fifo, sizeof a_fifo, "%ld", fifo_fd);
    snprintf(a_data, sizeof a_data, "%d", data_fd);
    snprintf(a_cap, sizeof a_cap, "%llu", (unsigned long long)capacity);
    snprintf(a_rounds, sizeof a_rounds, "%d", rounds);
    pid_t pid = fork();
    if (pid < 0)
        die("fork", errno);
    if (pid == 0) {
        // exec, not just fork: the client starts with none of the driver's
        // mappings -- registers, queue, the whole DMA area -- only the two
        // descriptors it was given.
        execl(self, self, "client", a_fifo, a_data, a_cap, a_rounds, (char *)NULL);
        printf("zx-virtio-blk: exec %s: %s\n", self, strerror(errno));
        _exit(1);
    }
    close((int)fifo_fd);
    close(data_fd);
    uint64_t served = serve(ends[0]);
    int st = 0;
    if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        printf("zx-virtio-blk: FAIL: the client failed (status %#x)\n", st);
        return 1;
    }
    printf("zx-virtio-blk: served %llu requests\n", (unsigned long long)served);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    const char *self = strchr(argv[0], '/') ? argv[0] : "/bin/zx-virtio-blk";
    if (argc > 1 && argv[1][0] == '/')
        return kernel_baseline(argv[1], argc > 2 ? atol(argv[2]) : 0);
    if (argc == 6 && !strcmp(argv[1], "client"))
        return client_main(atoi(argv[2]), atoi(argv[3]), strtoull(argv[4], NULL, 0),
                           atoi(argv[5]));
    int serving = argc > 1 && !strcmp(argv[1], "serve");
    if (serving) {
        argv++;
        argc--;
    }
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
    irq_handle = (zx_handle_t)c.interrupt;
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
    int slice = -1; // the data window, as a descriptor
    if (serving) {
        zx_handle_t h;
        r = zx_status(zx6(ZX_SYS_vmo_create_child, vmo, ZX_VMO_CHILD_SLICE, DMA_DATA, DATA_MAX,
                          (long)&h, 0));
        if (r != 0)
            die("vmo_create_child (slice)", r);
        // Into a descriptor now: that is how it travels.
        long fd = zx_status(zx(ECLIPSE_SYS_handle_to_fd, h, 0, 0, 0));
        if (fd < 0)
            die("handle_to_fd (slice)", fd);
        slice = (int)fd;
    }
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
    printf("zx-virtio-blk: disk #%ld up (%s), %llu sectors, DMA at %#llx, irq %d%s\n", index,
           transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO ? "virtio-mmio" : "virtio-pci",
           (unsigned long long)capacity, (unsigned long long)daddr, (int)c.irq,
           irq_handle ? "" : " (no interrupt object: polling only)");
    use_irq = irq_handle != 0;
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
    printf("zx-virtio-blk: 16 KiB written and read back right%s\n",
           use_irq ? ", waiting on the interrupt" : "");
    if (use_irq && irq_waits == 0)
        die("the interrupt never came", 0);

    // Let the boot settle first: as init, this runs while the shell is
    // still starting, and that time lands in whichever pass runs first.
    sleep(2);
    {
        // What a gateway syscall that does not block costs, and reading the
        // clock, to read the numbers below against: handle_close of no
        // handle.
        uint64_t t0 = now_ns();
        for (int i = 0; i < 1000; i++)
            zx(ZX_SYS_handle_close, 0, 0, 0, 0);
        uint64_t t1 = now_ns();
        for (int i = 0; i < 1000; i++)
            (void)now_ns();
        uint64_t t2 = now_ns();
        printf("zx-virtio-blk: a gateway syscall %.1f us, clock_gettime %.1f us\n",
               (t1 - t0) / 1000.0 / 1000, (t2 - t1) / 1000.0 / 1000);
    }
    uint64_t sectors = capacity < 2048 ? capacity : 2048; // the first MiB
    for (int mode = irq_handle ? 1 : 0; mode >= 0; mode--) {
        use_irq = mode;
        irq_waits = slept = to_irq_ns = to_wake_ns = 0;
        double one = time_reads(SECTOR, sectors, rounds);
        double four = time_reads(4096, sectors, rounds);
        const char *how = mode ? "interrupt" : "polling  ";
        printf("zx-virtio-blk: %s read 512 B  %8.1f us per request\n", how, one / 1000);
        printf("zx-virtio-blk: %s read 4 KiB  %8.1f us per request (%.1f MiB/s)\n", how,
               four / 1000, 4096.0 / four * 1e9 / (1 << 20));
        if (mode) {
            printf("zx-virtio-blk: %llu interrupt waits\n", (unsigned long long)irq_waits);
            if (slept)
                printf("zx-virtio-blk: %llu slept: notify to interrupt %.1f us, interrupt to "
                       "driver %.1f us\n",
                       (unsigned long long)slept, to_irq_ns / 1000.0 / slept,
                       to_wake_ns / 1000.0 / slept);
        }
    }
    if (serving && serve_client(self, slice, rounds))
        return 1;
    printf("zx-virtio-blk: all checks passed\n");
    return 0;
}
