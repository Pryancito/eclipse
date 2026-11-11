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
//   zx-virtio-blk export [N [IRQ]]  (IRQ 0: poll the device; 2: sleep on its
//                                    interrupt at once; default 1: look at
//                                    it for a while, then sleep)
//   zx-virtio-blk /dev/vdX [OFFSET_MIB [w]]  (w: time writes there too)
//   zx-virtio-blk fs /dev/vdXN  (mount the vfat there, read and write it)
//   zx-virtio-blk service [N]
//   zx-virtio-blk restart [N [KILLS]]
//
// `service` is the driver as what it is for: the disk goes to the kernel
// and the process serves it until it is killed, checking and writing
// nothing of its own. This is what a supervisor runs (eclipse-init, `type =
// respawn`). When it dies the kernel resets the device and keeps the disk:
// the node stays, with whatever is mounted on it, and requests wait up to
// ten seconds for the next `service`, which gets the same disk; what was
// out with the dead driver is asked again.
//
// `restart` is the check of that, on the image `export` wants (a vfat in
// the first partition holding `pattern.bin`): it starts the driver as a
// child, mounts the filesystem, and KILLS times (default 2) kills the
// driver with SIGKILL in the middle of a file being written and synced and
// starts it again -- the write must finish and read back right after a
// remount. Then the same under a process reading the node, then a kill
// with nobody started after it: a write must fail once the ten seconds are
// up, and work again when a driver does come.
//
// With `serve` the driver goes on to serve the disk to another process,
// over a FIFO and a window of its DMA memory, and that process checks and
// times it (see "Serving another process" below). With `export` it serves
// the kernel instead: the disk becomes a block device of the kernel's, a
// new /dev/vdX, and a child process checks and times it with pread (see
// "Serving the kernel"). The fourth form is the baseline: the same reads through the kernel's
// driver, with pread on its block device, starting OFFSET_MIB into it
// (default 0). The first pass is what reaches the device (the kernel
// caches what it reads, and reads ahead); the second is served from the
// cache.
//
// With `export`, a disk whose first partition is a vfat holding
// `pattern.bin` (see fs_test) also gets mounted, read and written through
// the served node; `fs` does the same on any node, the kernel's driver's
// included, to compare.
//
// Run it on Eclipse booted with the udriver option, which turns the Zircon
// gateway on unless `zircon.gateway=0` says otherwise. Exit status: 0 on
// success, 2 when the gateway is off, 1 on error.
//
// A prototype: one queue, one request in flight, one client.

#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
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
#define ZX_ERR_ALREADY_BOUND (-27)
#define ECLIPSE_SYS_fd_to_handle 0x4001
#define ECLIPSE_FD_TAKE 1

#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_device_claim 0x4005
#define ECLIPSE_SYS_block_serve 0x4006
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
#define VIRTIO_BLK_T_FLUSH 4
// Feature bit 9: the device has a write cache and takes VIRTIO_BLK_T_FLUSH.
// A driver that does not accept it gets a device that syncs every write
// (QEMU does exactly that), which makes each one cost a flush.
#define VIRTIO_BLK_F_FLUSH (1u << 9)
#define VIRTIO_BLK_S_OK 0

// Split virtqueue (virtio 1.1 §2.6).
// Room for MAX_INFLIGHT requests of three descriptors each.
#define QUEUE_SIZE 64
#define MAX_INFLIGHT 16
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
// used ring on the next page), a page of request headers and statuses (one
// REQ_SLOT per request in flight), then the data buffer.
#define DMA_QUEUE 0
#define DMA_USED PAGE
#define DMA_REQ (2 * PAGE)
#define DMA_DATA (3 * PAGE)
#define DATA_MAX (64 * 1024)
#define DMA_SIZE (DMA_DATA + DATA_MAX)
#define REQ_SLOT 32
_Static_assert(MAX_INFLIGHT * REQ_SLOT <= PAGE, "the headers fit their page");
_Static_assert(3 * MAX_INFLIGHT <= QUEUE_SIZE, "the descriptors fit the queue");

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

// Hybrid waiting, for serve(): look for a while before sleeping, because
// a sleep and the wake behind it cost more than a fast device, or a client
// with its next request ready, takes to answer. A look that runs out was
// wasted (and on one CPU it always is: nobody else ran meanwhile), so after
// one the next waits sleep straight away, twice as many after each miss in
// a row (up to HYBRID_SKIP_MAX), until a look is tried again and pays.
#define HYBRID_SKIP_MAX 64
#define DEVICE_LOOK_NS 200000ull // on the used ring, before the interrupt
#define FIFO_LOOK_NS 300000ull   // on the FIFO, before sleeping on it
struct hybrid {
    int on;
    unsigned skip, backoff; // waits left to sleep through; the next run of them
};
static struct hybrid device_look, fifo_look;

// Whether this wait starts with a look.
static int hybrid_try(struct hybrid *h) {
    if (!h->on)
        return 0;
    if (h->skip) {
        h->skip--;
        return 0;
    }
    return 1;
}

// How the look hybrid_try allowed went.
static void hybrid_done(struct hybrid *h, int hit) {
    if (hit) {
        h->backoff = 1;
        return;
    }
    h->skip = h->backoff ? h->backoff : 1;
    if (h->backoff < HYBRID_SKIP_MAX)
        h->backoff = h->backoff ? h->backoff * 2 : 2;
}

static inline uint32_t rd32(uint32_t off) { return *(volatile uint32_t *)(regs + off); }
static inline uint16_t rd16(uint32_t off) { return *(volatile uint16_t *)(regs + off); }
static inline uint8_t rd8(uint32_t off) { return *(volatile uint8_t *)(regs + off); }
static inline void wr32(uint32_t off, uint32_t v) { *(volatile uint32_t *)(regs + off) = v; }
static inline void wr16(uint32_t off, uint16_t v) { *(volatile uint16_t *)(regs + off) = v; }
static inline void wr8(uint32_t off, uint8_t v) { *(volatile uint8_t *)(regs + off) = v; }

static uint64_t capacity;
static int has_flush; // VIRTIO_BLK_F_FLUSH negotiated

static void init_mmio(void) {
    if (rd32(MMIO_MAGIC) != 0x74726976 || rd32(MMIO_VERSION) != 1 || rd32(MMIO_DEVICE_ID) != 2)
        die("not a legacy virtio-mmio block device, version", rd32(MMIO_VERSION));
    wr32(MMIO_STATUS, 0);
    wr32(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    uint32_t take = rd32(MMIO_HOST_FEATURES) & VIRTIO_BLK_F_FLUSH;
    wr32(MMIO_GUEST_FEATURES, take);
    has_flush = take != 0;
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
    wr32(c + PCI_DEVICE_FEATURE_SELECT, 0);
    uint32_t take = rd32(c + PCI_DEVICE_FEATURE) & VIRTIO_BLK_F_FLUSH;
    wr32(c + PCI_DRIVER_FEATURE_SELECT, 0);
    wr32(c + PCI_DRIVER_FEATURE, take);
    has_flush = take != 0;
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

// One request for the device: `off` is where its data is in the data buffer.
struct io {
    uint32_t type;
    uint64_t sector;
    size_t len, off;
    int status; // 0 once the device says so
};

// Up to MAX_INFLIGHT requests at once: three descriptors each (header,
// data, status), all on the available ring before one notify, then either
// waits on the interrupt or a spin on the used ring until the device has
// put every one of them there.
static void do_batch(struct io *ios, int n) {
    for (int i = 0; i < n; i++) {
        struct blk_req *req = (struct blk_req *)(dma + DMA_REQ + i * REQ_SLOT);
        uint64_t hdr = daddr + DMA_REQ + (uint64_t)i * REQ_SLOT;
        req->type = ios[i].type;
        req->reserved = 0;
        req->sector = ios[i].sector;
        dma[DMA_REQ + i * REQ_SLOT + sizeof *req] = 0xff;
        uint16_t d = (uint16_t)(3 * i);
        // A flush carries no data: its header chains straight to its status.
        desc[d] = (struct virtq_desc){
            hdr, sizeof *req, VIRTQ_DESC_F_NEXT,
            (uint16_t)(ios[i].type == VIRTIO_BLK_T_FLUSH ? d + 2 : d + 1)};
        desc[d + 1] = (struct virtq_desc){
            daddr + DMA_DATA + ios[i].off, (uint32_t)ios[i].len,
            VIRTQ_DESC_F_NEXT | (ios[i].type == VIRTIO_BLK_T_IN ? VIRTQ_DESC_F_WRITE : 0),
            (uint16_t)(d + 2)};
        desc[d + 2] = (struct virtq_desc){hdr + sizeof *req, 1, VIRTQ_DESC_F_WRITE, 0};
        avail->ring[(uint16_t)(avail->idx + i) % QUEUE_SIZE] = d;
    }
    mb();
    avail->idx += (uint16_t)n;
    mb();
    uint16_t want = (uint16_t)(last_used + n);
    // virtio-mmio registers take 32-bit accesses only; QEMU drops a 16-bit
    // store to QueueNotify. The PCI notify register is 16 bits wide.
    uint64_t t_notify = now_ns();
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO)
        *(volatile uint32_t *)notify_reg = 0;
    else
        *(volatile uint16_t *)notify_reg = 0;
    // A flush takes as long as the disk under the device does: no look for
    // it, and nothing learnt from it about how fast the device answers.
    int slow = ios[0].type == VIRTIO_BLK_T_FLUSH;
    if (use_irq && !slow && hybrid_try(&device_look)) {
        // The clock is a system call: look at it now and then, not on
        // every turn, or the look is mostly trips into the kernel.
        for (unsigned k = 1; used->idx != want; k++)
            if (k % 4096 == 0 && now_ns() - t_notify >= DEVICE_LOOK_NS)
                break;
        hybrid_done(&device_look, used->idx == want);
    }
    if (use_irq) {
        // A wake is not a completion (a stale one from a polled request, a
        // config change, one interrupt for part of the batch): look at the
        // ring after each.
        int first = 1;
        while (used->idx != want) {
            int64_t ts = 0;
            long r = zx_status(zx(ZX_SYS_interrupt_wait, irq_handle, (long)&ts, 0, 0));
            if (r != 0)
                die("interrupt_wait", r);
            uint64_t t_back = now_ns();
            irq_waits++;
            // Only the first wake for this batch says anything about it: a
            // stale one carries a timestamp from before the notify.
            if (first && (uint64_t)ts >= t_notify && (uint64_t)ts <= t_back) {
                slept++;
                to_irq_ns += (uint64_t)ts - t_notify;
                to_wake_ns += t_back - (uint64_t)ts;
                first = 0;
            }
            ack_device();
        }
    } else {
        while (used->idx != want)
            ;
        // Nobody waits on the interrupt, but the line stays up until acked.
        ack_device();
    }
    mb();
    last_used = want;
    for (int i = 0; i < n; i++)
        ios[i].status =
            dma[DMA_REQ + i * REQ_SLOT + sizeof(struct blk_req)] == VIRTIO_BLK_S_OK ? 0 : -1;
}

static int do_io_at(uint32_t type, uint64_t sector, size_t len, size_t off) {
    struct io one = {type, sector, len, off, 0};
    do_batch(&one, 1);
    return one.status;
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

// `depth` requests of `chunk` bytes at a time, each into its own part of
// the data buffer, over the same span.
static double time_reads_batch(size_t chunk, uint64_t sectors, int rounds, int depth) {
    uint64_t per = chunk / SECTOR, n = 0;
    struct io ios[MAX_INFLIGHT];
    uint64_t t0 = now_ns();
    for (int r = 0; r < rounds; r++)
        for (uint64_t s = 0; s + per * depth <= sectors; s += per * depth, n += depth) {
            for (int i = 0; i < depth; i++)
                ios[i] = (struct io){VIRTIO_BLK_T_IN, s + per * i, chunk, chunk * i, 0};
            do_batch(ios, depth);
            for (int i = 0; i < depth; i++)
                if (ios[i].status)
                    die("read failed at sector", (long)(s + per * i));
        }
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

// The same, writing: every write goes to the disk, nothing is cached.
static double time_pwrites(int fd, size_t chunk, off_t base, size_t span) {
    static char buf[4096];
    uint64_t n = 0, t0 = now_ns();
    for (off_t off = 0; off + (off_t)chunk <= (off_t)span; off += chunk, n++) {
        memset(buf, (int)n, chunk);
        if (pwrite(fd, buf, chunk, base + off) != (ssize_t)chunk)
            die("pwrite failed, errno", errno);
    }
    return (double)(now_ns() - t0) / n;
}

#include <pthread.h>

// MAX_INFLIGHT threads reading 4 KiB at a time, each over its own part of
// a MiB: what the kernel's driver does with as many requests outstanding
// as the served one is given.
struct pread_job {
    int fd;
    off_t base;
    size_t span;
};
static void *pread_thread(void *arg) {
    struct pread_job *j = arg;
    char buf[4096];
    for (off_t off = 0; off + 4096 <= (off_t)j->span; off += 4096)
        if (pread(j->fd, buf, 4096, j->base + off) != 4096)
            die("pread failed, errno", errno);
    return NULL;
}
static double time_preads_parallel(int fd, off_t base) {
    pthread_t t[MAX_INFLIGHT];
    struct pread_job jobs[MAX_INFLIGHT];
    size_t each = (1 << 20) / MAX_INFLIGHT;
    uint64_t t0 = now_ns();
    for (int i = 0; i < MAX_INFLIGHT; i++) {
        jobs[i] = (struct pread_job){fd, base + (off_t)(i * each), each};
        if (pthread_create(&t[i], NULL, pread_thread, &jobs[i]))
            die("pthread_create", i);
    }
    for (int i = 0; i < MAX_INFLIGHT; i++)
        pthread_join(t[i], NULL);
    return (double)(now_ns() - t0) / ((1 << 20) / 4096);
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
    // Off the sector grid: the node reads the sectors around the request
    // one call at a time, the way a filesystem reads its disk.
    double odd = time_preads(fd, 4096, base + (1 << 20) + 256, (1 << 20) - 4096);
    printf("zx-virtio-blk: kernel %s read 4 KiB off the grid %6.1f us\n", path, odd / 1000);
    double deep = time_preads_parallel(fd, base + (2 << 20));
    printf("zx-virtio-blk: kernel %s read 4 KiB x%d threads %6.1f us per request cold "
           "(%.1f MiB/s)\n",
           path, MAX_INFLIGHT, deep / 1000, 4096.0 / deep * 1e9 / (1 << 20));
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
#define BLK_OP_FLUSH 4 // what was written, onto the disk; no sectors, no data
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

// Wait for something to read on `f`, sleeping or spinning (or, with `look`,
// spinning for a while first); 0 once there is, -1 once the other side has
// gone.
static int fifo_wait_look(zx_handle_t f, int spin, struct hybrid *look, struct blk_msg *m,
                          size_t n, size_t *got) {
    int looking = look && !spin && hybrid_try(look);
    uint64_t t0 = looking ? now_ns() : 0;
    for (unsigned k = 1;; k++) {
        long r = fifo_read(f, m, n, got);
        if (r == 0) {
            if (looking)
                hybrid_done(look, 1);
            return 0;
        }
        if (r != ZX_ERR_SHOULD_WAIT)
            return -1;
        if (spin)
            continue;
        if (looking) {
            if (k % 16 || now_ns() - t0 < FIFO_LOOK_NS)
                continue;
            hybrid_done(look, 0);
            looking = 0;
        }
        uint32_t observed = 0;
        r = zx_status(zx6(ZX_SYS_object_wait_one, f, ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED,
                          ZX_TIME_INFINITE, (long)&observed, 0, 0));
        if (r < 0 || !(observed & ZX_FIFO_READABLE))
            return -1;
    }
}

static int fifo_wait(zx_handle_t f, int spin, struct blk_msg *m, size_t n, size_t *got) {
    return fifo_wait_look(f, spin, NULL, m, n, got);
}

// A MODE message: how the driver waits from now on, on the FIFO and on the
// device.
static void serve_mode(struct blk_msg *m, int *spin) {
    // Where the requests of the mode that just ended spent their time
    // waiting on the device, as the driver's own timing above says it.
    if (use_irq && slept)
        printf("zx-virtio-blk: served, %llu slept on the device: notify to interrupt "
               "%.1f us, interrupt to driver %.1f us\n",
               (unsigned long long)slept, to_irq_ns / 1000.0 / slept,
               to_wake_ns / 1000.0 / slept);
    slept = to_irq_ns = to_wake_ns = 0;
    *spin = (m->sector & BLK_MODE_SPIN) != 0;
    use_irq = (m->sector & BLK_MODE_IRQ) && irq_handle;
    m->status = 0;
}

// A read or a write, checked: 0 and the request for the device, or the
// status to send back without going near it.
static int64_t serve_check(const struct blk_msg *m, struct io *io) {
    if (m->op == BLK_OP_FLUSH) {
        if (m->sectors != 0)
            return ZX_ERR_INVALID_ARGS;
        *io = (struct io){VIRTIO_BLK_T_FLUSH, 0, 0, 0, 0};
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
    *io = (struct io){m->op == BLK_OP_READ ? VIRTIO_BLK_T_IN : VIRTIO_BLK_T_OUT, m->sector,
                      len, m->buf_off, 0};
    return 0;
}

// Serve `f` until the client closes it. Returns the requests served.
//
// Everything one read of the FIFO brings is on the device at once (up to
// MAX_INFLIGHT, in order), so a client with several requests outstanding
// pays one notify, one interrupt and one wake for all of them. A MODE
// message takes effect between the requests before it and after it.
//
// With `hybrid`, both waits (for a request, for the device) start with a
// look: see `struct hybrid`.
static uint64_t serve(zx_handle_t f, int irq, int hybrid) {
    struct blk_msg m[FIFO_DEPTH];
    struct io ios[MAX_INFLIGHT];
    int at[MAX_INFLIGHT]; // which message each of `ios` answers
    int spin = 0;
    uint64_t served = 0;
    size_t got;
    use_irq = irq && irq_handle;
    device_look = fifo_look = (struct hybrid){hybrid, 0, 1};
    while (fifo_wait_look(f, spin, &fifo_look, m, FIFO_DEPTH, &got) == 0) {
        int n = 0;
        for (size_t i = 0; i <= got; i++) {
            // A flush goes to the device alone, after everything before it
            // has come back: it is those writes it is asked to put on disk.
            int alone = n == 1 && ios[0].type == VIRTIO_BLK_T_FLUSH;
            int cut = i == got || m[i].op == BLK_OP_MODE || m[i].op == BLK_OP_FLUSH ||
                      alone || n == MAX_INFLIGHT;
            if (cut && n) {
                do_batch(ios, n);
                for (int k = 0; k < n; k++)
                    m[at[k]].status = ios[k].status ? ZX_ERR_IO : 0;
                n = 0;
            }
            if (i == got)
                break;
            if (m[i].op == BLK_OP_MODE) {
                serve_mode(&m[i], &spin);
                continue;
            }
            m[i].status = serve_check(&m[i], &ios[n]);
            // A device without a write cache has nothing to flush: every
            // write it answered is already on the disk.
            if (m[i].status == 0 && !(ios[n].type == VIRTIO_BLK_T_FLUSH && !has_flush))
                at[n++] = (int)i;
        }
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

// `depth` requests of `sectors` each outstanding at a time, each into its
// own part of the window; all sent, then every reply waited for.
static double client_time_depth(uint32_t sectors, uint64_t span, int rounds, int spin,
                                int depth) {
    struct blk_msg m[FIFO_DEPTH], back[FIFO_DEPTH];
    uint64_t n = 0, t0 = now_ns();
    for (int r = 0; r < rounds; r++)
        for (uint64_t s = 0; s + (uint64_t)sectors * depth <= span;
             s += (uint64_t)sectors * depth) {
            for (int i = 0; i < depth; i++)
                m[i] = (struct blk_msg){BLK_OP_READ, ++next_id, sectors,
                                        (uint32_t)(i * sectors * SECTOR),
                                        s + (uint64_t)i * sectors, 0};
            size_t put = 0;
            if (fifo_write(cfifo, m, depth, &put) != 0 || put != (size_t)depth)
                die("fifo_write", (long)put);
            for (int have = 0; have < depth;) {
                size_t got = 0;
                if (fifo_wait(cfifo, spin, back, depth - have, &got) != 0)
                    die("the driver went away", 0);
                for (size_t k = 0; k < got; k++)
                    if (back[k].status)
                        die("served read failed, status", (long)back[k].status);
                have += (int)got;
            }
            n += depth;
        }
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
    // Several at once, each into its own place, in the reverse of the
    // order they lie on the disk: sectors 8..31 of the tail, which still
    // hold the driver's pattern.
    {
        struct blk_msg m[3], back[3];
        memset(window, 0, DATA_MAX);
        for (int i = 0; i < 3; i++)
            m[i] = (struct blk_msg){BLK_OP_READ, ++next_id, 8, (uint32_t)((2 - i) * 8 * SECTOR),
                                    tail + 8 + 8 * (uint64_t)i, 0};
        size_t put = 0, have = 0;
        if (fifo_write(cfifo, m, 3, &put) != 0 || put != 3)
            die("client: fifo_write", (long)put);
        while (have < 3) {
            size_t got = 0;
            if (fifo_wait(cfifo, 0, back + have, 3 - have, &got) != 0)
                die("the driver went away", 0);
            have += got;
        }
        for (int i = 0; i < 3; i++)
            if (back[i].status || back[i].reqid != m[i].reqid)
                die("client: a batched read failed", i);
        for (int i = 0; i < 3; i++)
            for (size_t k = 0; k < 8 * SECTOR; k++) {
                size_t at = (size_t)(8 + 8 * i) * SECTOR + k; // into the pattern
                if (window[(size_t)(2 - i) * 8 * SECTOR + k] != (uint8_t)(at * 7 + (at >> 9)))
                    bad++;
            }
    }
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
    printf("zx-virtio-blk: served from another process: reads, a write, three at once, bad requests refused\n");

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
        double deep = client_time_depth(8, span, rounds, modes[i].spin, MAX_INFLIGHT);
        printf("zx-virtio-blk: served, %-28s 4 KiB x%d in flight %7.1f us per request "
               "(%.1f MiB/s)\n",
               modes[i].name, MAX_INFLIGHT, deep / 1000, 4096.0 / deep * 1e9 / (1 << 20));
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
    uint64_t served = serve(ends[0], 0, 0);
    int st = 0;
    if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        printf("zx-virtio-blk: FAIL: the client failed (status %#x)\n", st);
        return 1;
    }
    printf("zx-virtio-blk: served %llu requests\n", (unsigned long long)served);
    return 0;
}

// ---- Serving the kernel -----------------------------------------------
//
// `zx-virtio-blk export N` hands the kernel one end of the same FIFO and a
// window of the DMA memory (eclipse_block_serve), and the kernel puts a
// block device of its own on it: a new /dev/vdX, with the page cache and
// everything else above it. The kernel copies between its buffers and the
// window, which it cuts into 4 KiB slots, one request in flight on each:
// several threads at once, or one long read, reach the driver as one batch.
// The driver then serves the other end forever, asleep on the FIFO and on
// the device, while a child (this program again, exec'd with `kclient`)
// finds the new node and reads it as any process would.

#include <dirent.h>

// The vdX nodes in /dev, as " vda vdb ".
static void list_vd(char *out, size_t len) {
    size_t at = 0;
    out[at++] = ' ';
    out[at] = 0;
    DIR *d = opendir("/dev");
    if (!d)
        die("opendir /dev, errno", errno);
    struct dirent *e;
    while ((e = readdir(d)))
        if (!strncmp(e->d_name, "vd", 2) && strlen(e->d_name) == 3 && at + 5 < len) {
            memcpy(out + at, e->d_name, 3);
            at += 3;
            out[at++] = ' ';
            out[at] = 0;
        }
    closedir(d);
}

static int export_disk(const char *self, zx_handle_t window, int irq) {
    char before[128];
    list_vd(before, sizeof before);
    zx_handle_t ends[2];
    long r = zx_status(zx6(ZX_SYS_fifo_create, FIFO_DEPTH, sizeof(struct blk_msg), 0,
                           (long)&ends[0], (long)&ends[1], 0));
    if (r != 0)
        die("fifo_create", r);
    r = zx_status(zx(ECLIPSE_SYS_block_serve, ends[1], window, (long)capacity, 0));
    if (r != 0)
        die("block_serve", r);
    printf("zx-virtio-blk: disk handed to the kernel, %llu sectors\n",
           (unsigned long long)capacity);
    char a_cap[24];
    snprintf(a_cap, sizeof a_cap, "%llu", (unsigned long long)capacity);
    pid_t pid = fork();
    if (pid < 0)
        die("fork", errno);
    if (pid == 0) {
        execl(self, self, "kclient", before, a_cap, (char *)NULL);
        printf("zx-virtio-blk: exec %s: %s\n", self, strerror(errno));
        _exit(1);
    }
    // The kernel never closes its end: serve for as long as the system runs.
    serve(ends[0], irq, irq != 2);
    printf("zx-virtio-blk: FAIL: the kernel's end of the FIFO went away\n");
    return 1;
}

// The writes /proc/diskstats has seen on `name`, or -1.
static long disk_writes(const char *name) {
    FILE *f = fopen("/proc/diskstats", "r");
    if (!f)
        return -1;
    char line[256], dev[32];
    long writes = -1;
    unsigned maj, min;
    unsigned long rd, rdm, rds, rdt, wr;
    while (fgets(line, sizeof line, f))
        if (sscanf(line, "%u %u %31s %lu %lu %lu %lu %lu", &maj, &min, dev, &rd, &rdm, &rds,
                   &rdt, &wr) == 8 &&
            !strcmp(dev, name))
            writes = (long)wr;
    fclose(f);
    return writes;
}

#include <sys/mount.h>
#include <sys/stat.h>

// One pass over `path` 4 KiB at a time, each byte checked against the
// pattern `byte(i)`: ns per read, or a negative count of wrong bytes.
static double fs_read_pass(const char *path, size_t size, uint8_t (*byte)(size_t)) {
    static uint8_t buf[4096];
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        die("fs: open for reading, errno", errno);
    long bad = 0;
    uint64_t t0 = now_ns();
    for (size_t off = 0; off < size; off += sizeof buf) {
        if (read(fd, buf, sizeof buf) != (ssize_t)sizeof buf)
            die("fs: read, errno", errno);
        for (size_t i = 0; i < sizeof buf; i++)
            bad += buf[i] != byte(off + i);
    }
    uint64_t t1 = now_ns();
    close(fd);
    return bad ? -(double)bad : (double)(t1 - t0) / (size / sizeof buf);
}

static uint8_t fs_given(size_t i) { return (uint8_t)(i * 31 + (i >> 12)); }
static uint8_t fs_made(size_t i) { return (uint8_t)(i * 17 + (i >> 9) + 3); }

// A filesystem on `part`, the way anything else would use the disk: mount
// it (vfat), read a file that came with the image and check every byte,
// write a new one, sync it, and read that back after a remount, so that
// what is checked came off the disk and not out of a cache.
//
// The image: a vfat partition holding `pattern.bin`, 1 MiB of fs_given().
#define FS_GIVEN_SIZE ((size_t)1 << 20)
#define FS_MADE_SIZE ((size_t)256 << 10)
static int fs_test(const char *part) {
    const char *dir = "/mnt/served";
    mkdir("/mnt", 0755);
    mkdir(dir, 0755);
    if (mount(part, dir, "vfat", 0, NULL) != 0) {
        printf("zx-virtio-blk: FAIL: mount %s: %s\n", part, strerror(errno));
        return 1;
    }
    char given[64], made[64];
    snprintf(given, sizeof given, "%s/pattern.bin", dir);
    snprintf(made, sizeof made, "%s/made.bin", dir);
    double cold = fs_read_pass(given, FS_GIVEN_SIZE, fs_given);
    double warm = fs_read_pass(given, FS_GIVEN_SIZE, fs_given);
    if (cold < 0 || warm < 0) {
        printf("zx-virtio-blk: FAIL: %s reads wrong through the filesystem\n", given);
        return 1;
    }
    static uint8_t buf[4096];
    int fd = open(made, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        die("fs: create, errno", errno);
    uint64_t t0 = now_ns();
    for (size_t off = 0; off < FS_MADE_SIZE; off += sizeof buf) {
        for (size_t i = 0; i < sizeof buf; i++)
            buf[i] = fs_made(off + i);
        if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf)
            die("fs: write, errno", errno);
    }
    if (fsync(fd) != 0)
        die("fs: fsync, errno", errno);
    double wrote = (double)(now_ns() - t0) / (FS_MADE_SIZE / sizeof buf);
    close(fd);
    if (umount(dir) != 0 || mount(part, dir, "vfat", 0, NULL) != 0) {
        printf("zx-virtio-blk: FAIL: remount %s: %s\n", part, strerror(errno));
        return 1;
    }
    double back = fs_read_pass(made, FS_MADE_SIZE, fs_made);
    if (back < 0 || unlink(made) != 0 || umount(dir) != 0) {
        printf("zx-virtio-blk: FAIL: the file written to %s did not come back right\n", part);
        return 1;
    }
    printf("zx-virtio-blk: vfat on %s read 4 KiB %8.1f us cold, %8.1f us cached\n", part,
           cold / 1000, warm / 1000);
    printf("zx-virtio-blk: vfat on %s write 4 KiB %7.1f us with the sync, %8.1f us read "
           "back\n",
           part, wrote / 1000, back / 1000);
    return 0;
}

// The vdX that was not in `before`, waiting for it: 0 when there is one.
static int new_vd(const char *before, char name[4]) {
    char now[128];
    name[0] = 0;
    // The node appears once the kernel's hot-plug task has run, which also
    // reads the partition table through the driver.
    for (int i = 0; i < 200 && !name[0]; i++) {
        list_vd(now, sizeof now);
        for (char *p = now + 1; *p; p += 4) {
            char probe[6] = {' ', p[0], p[1], p[2], ' ', 0};
            if (!strstr(before, probe)) {
                memcpy(name, p, 3);
                name[3] = 0;
                break;
            }
        }
        if (!name[0])
            usleep(50000);
    }
    return name[0] ? 0 : -1;
}

static int kclient_main(const char *before, uint64_t disk_sectors) {
    char name[4], path[16];
    if (new_vd(before, name) != 0)
        die("kclient: no new /dev/vdX appeared", 0);
    snprintf(path, sizeof path, "/dev/%s", name);
    printf("zx-virtio-blk: the kernel put the served disk at %s\n", path);
    int fd = open(path, O_RDWR);
    if (fd < 0)
        die("kclient: open, errno", errno);

    // The pattern the driver wrote over the tail, through the kernel.
    static uint8_t buf[32 * SECTOR];
    off_t tail = (off_t)(disk_sectors - 32) * SECTOR;
    if (pread(fd, buf, sizeof buf, tail) != (ssize_t)sizeof buf)
        die("kclient: pread the tail, errno", errno);
    int bad = 0;
    for (size_t i = 0; i < sizeof buf; i++)
        if (buf[i] != (uint8_t)(i * 7 + (i >> 9)))
            bad++;
    if (bad) {
        printf("zx-virtio-blk: FAIL: %d bytes of the tail read wrong through %s\n", bad, path);
        return 1;
    }
    // A write, which must reach the driver: the disk's own count says so.
    long w0 = disk_writes(name);
    for (size_t i = 0; i < 4096; i++)
        buf[i] = (uint8_t)(i * 13 + 5);
    if (pwrite(fd, buf, 4096, tail) != 4096 || fsync(fd) != 0)
        die("kclient: pwrite, errno", errno);
    long w1 = disk_writes(name);
    if (w0 < 0 || w1 <= w0)
        die("kclient: the write never reached the disk, writes", w1);
    memset(buf, 0, 4096);
    if (pread(fd, buf, 4096, tail) != 4096)
        die("kclient: pread, errno", errno);
    for (size_t i = 0; i < 4096; i++)
        if (buf[i] != (uint8_t)(i * 13 + 5))
            bad++;
    // Past the end, refused by the kernel before the driver sees it.
    if (bad || pread(fd, buf, 4096, (off_t)disk_sectors * SECTOR) > 0) {
        printf("zx-virtio-blk: FAIL: the write or the end of %s is wrong\n", path);
        return 1;
    }
    printf("zx-virtio-blk: %s: the driver's pattern, a write that reached it, the end\n", path);
    // Writes, timed, on a part of the disk nothing else here reads: the
    // fourth MiB, short of the pattern at the tail.
    if (disk_sectors >= 4 * 2048) {
        double w = time_pwrites(fd, 4096, (off_t)3 << 20, 1 << 19);
        printf("zx-virtio-blk: kernel %s write 4 KiB %8.1f us\n", path, w / 1000);
    }
    close(fd);
    // A filesystem, when the disk carries one in its first partition.
    char part[20];
    snprintf(part, sizeof part, "%s1", path);
    if (access(part, F_OK) == 0 && fs_test(part) != 0)
        return 1;
    return kernel_baseline(path, 0);
}

// The driver and nothing else: the disk to the kernel, a byte down
// `report` (when there is one) once it is the kernel's, and its requests
// until a signal ends the process.
static int service_disk(zx_handle_t window, int report) {
    zx_handle_t ends[2];
    long r = zx_status(zx6(ZX_SYS_fifo_create, FIFO_DEPTH, sizeof(struct blk_msg), 0,
                           (long)&ends[0], (long)&ends[1], 0));
    if (r != 0)
        die("fifo_create", r);
    r = zx_status(zx(ECLIPSE_SYS_block_serve, ends[1], window, (long)capacity, 0));
    if (r != 0)
        die("block_serve", r);
    if (report >= 0) {
        if (write(report, "u", 1) != 1)
            die("write (report), errno", errno);
        close(report);
    }
    serve(ends[0], 1, 1);
    printf("zx-virtio-blk: the kernel's end of the FIFO went away\n");
    return 1;
}

// Start the driver as a child, the way a supervisor would, and wait until
// it says the disk is the kernel's. Its pid, or -1.
static pid_t start_service(const char *self, long index) {
    int up[2];
    if (pipe(up) != 0)
        die("pipe, errno", errno);
    pid_t pid = fork();
    if (pid < 0)
        die("fork, errno", errno);
    if (pid == 0) {
        char a_index[24], a_report[24];
        close(up[0]);
        snprintf(a_index, sizeof a_index, "%ld", index);
        snprintf(a_report, sizeof a_report, "%d", up[1]);
        execl(self, self, "service", a_index, a_report, (char *)NULL);
        printf("zx-virtio-blk: exec %s: %s\n", self, strerror(errno));
        _exit(127);
    }
    close(up[1]);
    char c;
    ssize_t n = read(up[0], &c, 1);
    close(up[0]);
    if (n != 1) {
        printf("zx-virtio-blk: FAIL: the driver did not come up\n");
        return -1;
    }
    return pid;
}

static void kill_service(pid_t pid) {
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
}

// What `restart` writes while the driver is killed: long enough that the
// kill lands in the middle of it.
#define RESTART_SIZE ((size_t)512 << 10)
#define RESTART_KILL_US 100000
// The kernel's wait for the next driver, and how far past it a request may
// take to fail.
#define DRIVER_GRACE_MS 10000
#define GRACE_MARGIN_MS 5000

// `path` created, filled with fs_made() and synced: 0 when all of it went.
static int write_made(const char *path, size_t size) {
    static uint8_t buf[4096];
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return 1;
    for (size_t off = 0; off < size; off += sizeof buf) {
        for (size_t i = 0; i < sizeof buf; i++)
            buf[i] = fs_made(off + i);
        if (write(fd, buf, sizeof buf) != (ssize_t)sizeof buf)
            return 2;
    }
    if (fsync(fd) != 0)
        return 3;
    return close(fd) != 0 ? 4 : 0;
}

// Read `span` bytes of `path` from `base` over and over for `ms`, each pass
// against the first: 0 when every pass read the same. A byte goes down
// `ready` once the first pass is in.
static int read_same(const char *path, off_t base, size_t span, int ms, int ready) {
    uint8_t *first = malloc(span), *again = malloc(span);
    int fd = open(path, O_RDONLY);
    if (!first || !again || fd < 0)
        return 1;
    if (pread(fd, first, span, base) != (ssize_t)span)
        return 2;
    if (write(ready, "r", 1) != 1)
        return 3;
    uint64_t until = now_ns() + (uint64_t)ms * 1000000;
    while (now_ns() < until) {
        for (size_t off = 0; off < span; off += 4096)
            if (pread(fd, again + off, 4096, base + (off_t)off) != 4096)
                return 4;
        if (memcmp(first, again, span) != 0)
            return 5;
    }
    return 0;
}

static int restart_check(const char *self, long index, int kills) {
    char before[128], name[4], node[16], part[20];
    list_vd(before, sizeof before);
    pid_t driver = start_service(self, index);
    if (driver < 0)
        return 1;
    if (new_vd(before, name) != 0)
        die("restart: no new /dev/vdX appeared", 0);
    snprintf(node, sizeof node, "/dev/%s", name);
    snprintf(part, sizeof part, "%s1", node);
    printf("zx-virtio-blk: the driver is pid %d and its disk is %s\n", (int)driver, node);
    // The partitions' nodes come a moment after the disk's, once the
    // kernel has read the table through the driver.
    for (int i = 0; i < 100 && access(part, F_OK) != 0; i++)
        usleep(50000);
    const char *dir = "/mnt/served";
    mkdir("/mnt", 0755);
    mkdir(dir, 0755);
    if (mount(part, dir, "vfat", 0, NULL) != 0) {
        printf("zx-virtio-blk: FAIL: mount %s: %s\n", part, strerror(errno));
        return 1;
    }
    char given[64], made[64];
    snprintf(given, sizeof given, "%s/pattern.bin", dir);
    snprintf(made, sizeof made, "%s/made.bin", dir);
    if (fs_read_pass(given, FS_GIVEN_SIZE, fs_given) < 0) {
        printf("zx-virtio-blk: FAIL: %s reads wrong before any kill\n", given);
        return 1;
    }

    // A file being written and synced, and the driver killed under it.
    for (int k = 1; k <= kills; k++) {
        pid_t writer = fork();
        if (writer < 0)
            die("fork, errno", errno);
        if (writer == 0)
            _exit(write_made(made, RESTART_SIZE));
        usleep(RESTART_KILL_US);
        int status;
        if (waitpid(writer, &status, WNOHANG) != 0) {
            printf("zx-virtio-blk: FAIL: the write was over before the kill\n");
            return 1;
        }
        uint64_t t0 = now_ns();
        kill_service(driver);
        driver = start_service(self, index);
        if (driver < 0)
            return 1;
        double back_ms = (now_ns() - t0) / 1e6;
        if (waitpid(writer, &status, 0) != writer || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            printf("zx-virtio-blk: FAIL: the write the driver died under did not finish "
                   "(status %#x)\n",
                   status);
            return 1;
        }
        double done_ms = (now_ns() - t0) / 1e6;
        // Off the disk, not out of a cache.
        if (umount(dir) != 0 || mount(part, dir, "vfat", 0, NULL) != 0) {
            printf("zx-virtio-blk: FAIL: remount %s: %s\n", part, strerror(errno));
            return 1;
        }
        if (fs_read_pass(made, RESTART_SIZE, fs_made) < 0 ||
            fs_read_pass(given, FS_GIVEN_SIZE, fs_given) < 0 || unlink(made) != 0) {
            printf("zx-virtio-blk: FAIL: kill %d: the files do not read back right\n", k);
            return 1;
        }
        printf("zx-virtio-blk: kill %d, %d ms into a %zu KiB write: driver back in %.0f ms "
               "(pid %d), the write done %.0f ms after the kill and right on the disk\n",
               k, RESTART_KILL_US / 1000, RESTART_SIZE >> 10, back_ms, (int)driver, done_ms);
    }

    // The same under a process reading the node, whose reads sleep on the
    // driver's answers instead of looking for them.
    {
        int ready[2];
        if (pipe(ready) != 0)
            die("pipe, errno", errno);
        pid_t reader = fork();
        if (reader < 0)
            die("fork, errno", errno);
        if (reader == 0) {
            close(ready[0]);
            _exit(read_same(node, (off_t)8 << 20, 1 << 20, 1500, ready[1]));
        }
        close(ready[1]);
        char c;
        if (read(ready[0], &c, 1) != 1)
            die("restart: the reader did not start", 0);
        close(ready[0]);
        usleep(RESTART_KILL_US);
        kill_service(driver);
        driver = start_service(self, index);
        if (driver < 0)
            return 1;
        int status;
        if (waitpid(reader, &status, 0) != reader || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            printf("zx-virtio-blk: FAIL: reads of %s across a kill went wrong (status %#x)\n",
                   node, status);
            return 1;
        }
        printf("zx-virtio-blk: reads of %s went on across a kill, every pass the same\n", node);
    }

    // Nobody after this one: a write waits for a driver, and then fails.
    int fd = open(node, O_RDWR);
    if (fd < 0)
        die("restart: open the node, errno", errno);
    static uint8_t buf[4096], back[4096];
    off_t spare = (off_t)3 << 20; // the fourth MiB: before the partition
    for (size_t i = 0; i < sizeof buf; i++)
        buf[i] = (uint8_t)(i * 13 + 5);
    kill_service(driver);
    uint64_t t0 = now_ns();
    int wrote = pwrite(fd, buf, sizeof buf, spare) == (ssize_t)sizeof buf && fsync(fd) == 0;
    double waited_ms = (now_ns() - t0) / 1e6;
    if (wrote || waited_ms < DRIVER_GRACE_MS - 1000 ||
        waited_ms > DRIVER_GRACE_MS + GRACE_MARGIN_MS) {
        printf("zx-virtio-blk: FAIL: with no driver a write %s after %.1f s\n",
               wrote ? "went through" : "failed", waited_ms / 1000);
        return 1;
    }
    printf("zx-virtio-blk: no driver came back and a write failed, after %.1f s\n",
           waited_ms / 1000);
    // A driver that comes late still finds its disk.
    driver = start_service(self, index);
    if (driver < 0)
        return 1;
    if (pwrite(fd, buf, sizeof buf, spare) != (ssize_t)sizeof buf || fsync(fd) != 0 ||
        pread(fd, back, sizeof back, spare) != (ssize_t)sizeof back ||
        memcmp(buf, back, sizeof buf) != 0) {
        printf("zx-virtio-blk: FAIL: %s does not work again with a late driver: %s\n", node,
               strerror(errno));
        return 1;
    }
    close(fd);
    if (fs_read_pass(given, FS_GIVEN_SIZE, fs_given) < 0 || umount(dir) != 0) {
        printf("zx-virtio-blk: FAIL: the filesystem is not right after the late driver\n");
        return 1;
    }
    printf("zx-virtio-blk: a driver that came late has the same %s, and it works\n", node);
    kill_service(driver);
    printf("zx-virtio-blk: all checks passed\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    const char *self = strchr(argv[0], '/') ? argv[0] : "/bin/zx-virtio-blk";
    if (argc > 1 && !strcmp(argv[1], "restart"))
        return restart_check(self, argc > 2 ? atol(argv[2]) : 0, argc > 3 ? atoi(argv[3]) : 2);
    if (argc == 3 && !strcmp(argv[1], "fs")) {
        if (fs_test(argv[2]) != 0) {
            char have[128];
            list_vd(have, sizeof have);
            printf("zx-virtio-blk: the disks here:%s\n", have);
            return 1;
        }
        printf("zx-virtio-blk: all checks passed\n");
        return 0;
    }
    if (argc > 1 && argv[1][0] == '/')
    {
        // `w` after the offset: time writes too, over the fourth MiB from
        // it. Only for a disk whose contents there nobody wants.
        if (argc > 3 && !strcmp(argv[3], "w")) {
            int fd = open(argv[1], O_RDWR);
            if (fd < 0)
                die("open the block device to write, errno", errno);
            double w = time_pwrites(fd, 4096, ((off_t)atol(argv[2]) + 3) << 20, 1 << 19);
            printf("zx-virtio-blk: kernel %s write 4 KiB %8.1f us\n", argv[1], w / 1000);
            close(fd);
        }
        return kernel_baseline(argv[1], argc > 2 ? atol(argv[2]) : 0);
    }
    if (argc == 6 && !strcmp(argv[1], "client"))
        return client_main(atoi(argv[2]), atoi(argv[3]), strtoull(argv[4], NULL, 0),
                           atoi(argv[5]));
    if (argc == 4 && !strcmp(argv[1], "kclient"))
        return kclient_main(argv[2], strtoull(argv[3], NULL, 0));
    int service = argc > 1 && !strcmp(argv[1], "service");
    int exporting = service || (argc > 1 && !strcmp(argv[1], "export"));
    int serving = exporting || (argc > 1 && !strcmp(argv[1], "serve"));
    if (serving) {
        argv++;
        argc--;
    }
    long index = argc > 1 ? atol(argv[1]) : 0;
    int rounds = argc > 2 ? atoi(argv[2]) : 2;
    struct claim c;
    long r;
    // As a service, a claim refused because the disk has a driver is tried
    // again for a while: the driver before this one may have died a moment
    // ago, and the kernel resets the device before it gives it out.
    for (int tries = service ? 300 : 1;; usleep(10000)) {
        r = zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_VIRTIO_BLK, index, (long)&c, 0));
        if (r != ZX_ERR_ALREADY_BOUND || --tries <= 0)
            break;
    }
    if (r == -ENOSYS) {
        printf("zx-virtio-blk: the gateway is off (zircon.gateway=0 on the command line?)\n");
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
    zx_handle_t window = 0; // or as a handle, for the kernel
    if (serving) {
        zx_handle_t h;
        r = zx_status(zx6(ZX_SYS_vmo_create_child, vmo, ZX_VMO_CHILD_SLICE, DMA_DATA, DATA_MAX,
                          (long)&h, 0));
        if (r != 0)
            die("vmo_create_child (slice)", r);
        window = h;
    }
    if (serving && !exporting) {
        zx_handle_t h = window;
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
    // A service says nothing and touches nothing: the disk is somebody's.
    if (service) {
        if (capacity < 64)
            die("disk too small, sectors", (long)capacity);
        return service_disk(window, argc > 2 ? atoi(argv[2]) : -1);
    }
    printf("zx-virtio-blk: disk #%ld up (%s), %llu sectors, DMA at %#llx, irq %d%s\n", index,
           transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO ? "virtio-mmio" : "virtio-pci",
           (unsigned long long)capacity, (unsigned long long)daddr, (int)c.irq,
           irq_handle ? "" : " (no interrupt object: polling only)");
    printf("zx-virtio-blk: the device %s\n",
           has_flush ? "has a write cache: writes stay in it until a flush"
                     : "has no write cache: every write is on the disk when answered");
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
    // The driver's own numbers are the serve runs'; this one is about the
    // kernel's side.
    if (exporting)
        return export_disk(self, window, rounds);
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
        double deep = time_reads_batch(4096, sectors, rounds, MAX_INFLIGHT);
        printf("zx-virtio-blk: %s read 4 KiB x%d in flight %5.1f us per request (%.1f MiB/s)\n",
               how, MAX_INFLIGHT, deep / 1000, 4096.0 / deep * 1e9 / (1 << 20));
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
