// zx-virtio-net: a virtio network driver in userspace.
//
// The card is one the kernel left alone (`udriver=virtio-net@N`; the
// kernel has no virtio-net driver of its own), claimed with
// eclipse_device_claim like the disk of zx-virtio-blk: registers as a
// physical VMO mapped through an fd, DMA memory as a contiguous VMO pinned
// through the claim's BTI, the interrupt as an interrupt object. It brings
// the device up on either transport -- legacy virtio-mmio (aarch64,
// riscv64 under QEMU virt) or modern virtio-pci (x86_64 q35, MSI-X vector
// 0) -- with a receive and a transmit queue, and then talks to whatever is
// on the wire as 10.0.2.15, the address QEMU's user network expects:
//
//   - asks who has 10.0.2.2 (the gateway QEMU emulates) and takes the
//     answer, which proves a frame out and a frame in;
//   - pings it, ROUNDS times with 56 bytes and ROUNDS times with 1400,
//     first polling the receive queue and then sleeping on the interrupt,
//     and reports the round trip of each.
//
//   zx-virtio-net [N [ROUNDS]]  (defaults: card 0, 200 pings)
//
// Run it on Eclipse booted with `zircon.gateway=1` and the udriver option
// (`make ... UNET=1` adds the card). Exit status: 0 on success, 2 when the
// gateway is off, 1 on error.
//
// A prototype: no stack above it yet, one frame in flight.

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
#define ZX_SYS_interrupt_wait 54
#define ZX_SYS_vmo_create_contiguous 193
#define ECLIPSE_SYS_handle_to_fd 0x4000
#define ECLIPSE_SYS_device_claim 0x4005
#define ECLIPSE_DEVICE_VIRTIO_NET 2
#define ECLIPSE_TRANSPORT_VIRTIO_MMIO 1
#define ECLIPSE_TRANSPORT_VIRTIO_PCI 2
#define ZX_BTI_PERM_READ (1u << 0)
#define ZX_BTI_PERM_WRITE (1u << 1)
#define ZX_BTI_CONTIGUOUS (1u << 4)

#define PAGE 4096

#define STATUS_ACKNOWLEDGE 1
#define STATUS_DRIVER 2
#define STATUS_DRIVER_OK 4
#define STATUS_FEATURES_OK 8

// Legacy virtio-mmio registers (spec 1.1 §4.2.4).
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

// Modern virtio-pci common configuration (spec 1.1 §4.1.4.3).
#define PCI_DEVICE_FEATURE_SELECT 0x00
#define PCI_DEVICE_FEATURE 0x04
#define PCI_DRIVER_FEATURE_SELECT 0x08
#define PCI_DRIVER_FEATURE 0x0c
#define PCI_CONFIG_MSIX_VECTOR 0x10
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

#define VIRTIO_NET_F_MAC (1u << 5)
#define VIRTIO_ID_NET 1

#define QUEUE_SIZE 64
#define RXQ 0
#define TXQ 1
#define VIRTQ_DESC_F_WRITE 2

struct virtq_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags, next;
};
struct virtq_avail {
    uint16_t flags, idx, ring[QUEUE_SIZE];
};
struct virtq_used_elem {
    uint32_t id, len;
};
struct virtq_used {
    uint16_t flags, idx;
    struct virtq_used_elem ring[QUEUE_SIZE];
};

// The DMA area: each queue's descriptors and avail ring in one page and
// its used ring in the next (the legacy layout, which the modern transport
// is simply told about), then the receive buffers and the transmit buffer.
#define DMA_Q(q) ((q) * 2 * PAGE)
#define RX_BUFS 32
#define BUF_SIZE 2048 // a header and a 1514-byte frame, with room
#define DMA_RX (4 * PAGE)
#define DMA_TX (DMA_RX + RX_BUFS * BUF_SIZE)
#define DMA_SIZE (DMA_TX + PAGE)
_Static_assert(RX_BUFS <= QUEUE_SIZE, "a descriptor per receive buffer");

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
    printf("zx-virtio-net: %s: %ld\n", what, v);
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
static volatile uint8_t *notify_reg[2];

static uint8_t *dma;   // CPU view
static uint64_t daddr; // device view
static struct virtq_desc *desc[2];
static struct virtq_avail *avail[2];
static volatile struct virtq_used *used[2];
static uint16_t last_used[2];
static zx_handle_t irq_handle; // 0: none, poll
static uint8_t mac[6];
// The header in front of every frame: 10 bytes on a legacy device that was
// not offered mergeable buffers, 12 on a modern one (spec 1.1 §5.1.6).
static size_t net_hdr;

static inline uint32_t rd32(uint32_t off) { return *(volatile uint32_t *)(regs + off); }
static inline uint16_t rd16(uint32_t off) { return *(volatile uint16_t *)(regs + off); }
static inline uint8_t rd8(uint32_t off) { return *(volatile uint8_t *)(regs + off); }
static inline void wr32(uint32_t off, uint32_t v) { *(volatile uint32_t *)(regs + off) = v; }
static inline void wr16(uint32_t off, uint16_t v) { *(volatile uint16_t *)(regs + off) = v; }
static inline void wr8(uint32_t off, uint8_t v) { *(volatile uint8_t *)(regs + off) = v; }

static void init_mmio(void) {
    if (rd32(MMIO_MAGIC) != 0x74726976 || rd32(MMIO_VERSION) != 1 ||
        rd32(MMIO_DEVICE_ID) != VIRTIO_ID_NET)
        die("not a legacy virtio-mmio network card, device id", rd32(MMIO_DEVICE_ID));
    wr32(MMIO_STATUS, 0);
    wr32(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    if (!(rd32(MMIO_HOST_FEATURES) & VIRTIO_NET_F_MAC))
        die("the card has no MAC address of its own", 0);
    wr32(MMIO_GUEST_FEATURES, VIRTIO_NET_F_MAC);
    wr32(MMIO_GUEST_PAGE_SIZE, PAGE);
    for (int q = 0; q < 2; q++) {
        wr32(MMIO_QUEUE_SEL, q);
        uint32_t max = rd32(MMIO_QUEUE_NUM_MAX);
        if (max < QUEUE_SIZE)
            die("queue too small", max);
        wr32(MMIO_QUEUE_NUM, QUEUE_SIZE);
        wr32(MMIO_QUEUE_ALIGN, PAGE);
        wr32(MMIO_QUEUE_PFN, (uint32_t)((daddr + DMA_Q(q)) / PAGE));
        notify_reg[q] = regs + MMIO_QUEUE_NOTIFY;
    }
    for (int i = 0; i < 6; i++)
        mac[i] = rd8(MMIO_CONFIG + i);
    net_hdr = 10;
    wr32(MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
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
    if (!(rd32(c + PCI_DEVICE_FEATURE) & VIRTIO_NET_F_MAC))
        die("the card has no MAC address of its own", 0);
    wr32(c + PCI_DRIVER_FEATURE_SELECT, 0);
    wr32(c + PCI_DRIVER_FEATURE, VIRTIO_NET_F_MAC);
    wr32(c + PCI_DRIVER_FEATURE_SELECT, 1);
    wr32(c + PCI_DRIVER_FEATURE, VIRTIO_F_VERSION_1_HI);
    wr8(c + PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if (!(rd8(c + PCI_DEVICE_STATUS) & STATUS_FEATURES_OK))
        die("device refused the features", rd8(c + PCI_DEVICE_STATUS));
    // No interrupt for configuration changes: nothing here reads them.
    wr16(c + PCI_CONFIG_MSIX_VECTOR, VIRTIO_MSI_NO_VECTOR);
    for (int q = 0; q < 2; q++) {
        wr16(c + PCI_QUEUE_SELECT, q);
        uint16_t max = rd16(c + PCI_QUEUE_SIZE);
        if (max < QUEUE_SIZE)
            die("queue too small", max);
        wr16(c + PCI_QUEUE_SIZE, QUEUE_SIZE);
        // MSI-X vector 0 is the one the kernel armed, and the one the
        // claim's interrupt object listens on. Only the receive queue
        // raises it: a sent frame is reclaimed before the next is sent.
        uint16_t vector = irq_handle && q == RXQ ? 0 : VIRTIO_MSI_NO_VECTOR;
        wr16(c + PCI_QUEUE_MSIX_VECTOR, vector);
        if (vector == 0 && rd16(c + PCI_QUEUE_MSIX_VECTOR) != 0) {
            printf("zx-virtio-net: the device refused MSI-X vector 0; polling\n");
            irq_handle = 0;
        }
        uint64_t d = daddr + DMA_Q(q);
        uint64_t a = d + QUEUE_SIZE * sizeof(struct virtq_desc);
        uint64_t u = d + PAGE;
        wr32(c + PCI_QUEUE_DESC, (uint32_t)d);
        wr32(c + PCI_QUEUE_DESC + 4, (uint32_t)(d >> 32));
        wr32(c + PCI_QUEUE_DRIVER, (uint32_t)a);
        wr32(c + PCI_QUEUE_DRIVER + 4, (uint32_t)(a >> 32));
        wr32(c + PCI_QUEUE_DEVICE, (uint32_t)u);
        wr32(c + PCI_QUEUE_DEVICE + 4, (uint32_t)(u >> 32));
        uint16_t notify_off = rd16(c + PCI_QUEUE_NOTIFY_OFF);
        wr16(c + PCI_QUEUE_ENABLE, 1);
        notify_reg[q] = regs + pci_notify + (uint32_t)notify_off * pci_mult;
    }
    for (int i = 0; i < 6; i++)
        mac[i] = rd8(pci_device + i);
    net_hdr = 12;
    wr8(c + PCI_DEVICE_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
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

static void notify(int q) {
    // 32 bits wide on virtio-mmio; the modern transport takes 16.
    if (transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO)
        *(volatile uint32_t *)notify_reg[q] = q;
    else
        *(volatile uint16_t *)notify_reg[q] = q;
}

// Hand receive buffer `i` (descriptor `i`) to the device.
static void rx_post(uint16_t i) {
    desc[RXQ][i].addr = daddr + DMA_RX + (uint64_t)i * BUF_SIZE;
    desc[RXQ][i].len = BUF_SIZE;
    desc[RXQ][i].flags = VIRTQ_DESC_F_WRITE;
    desc[RXQ][i].next = 0;
    avail[RXQ]->ring[avail[RXQ]->idx % QUEUE_SIZE] = i;
    mb();
    avail[RXQ]->idx++;
    mb();
}

// Send the `len`-byte frame at the transmit buffer (after its header),
// and wait for the device to take it: one frame in flight.
static void tx_send(size_t len) {
    memset(dma + DMA_TX, 0, net_hdr);
    desc[TXQ][0].addr = daddr + DMA_TX;
    desc[TXQ][0].len = (uint32_t)(net_hdr + len);
    desc[TXQ][0].flags = 0;
    desc[TXQ][0].next = 0;
    avail[TXQ]->ring[avail[TXQ]->idx % QUEUE_SIZE] = 0;
    mb();
    avail[TXQ]->idx++;
    mb();
    notify(TXQ);
    uint16_t want = (uint16_t)(last_used[TXQ] + 1);
    uint64_t t0 = now_ns();
    for (unsigned k = 1; used[TXQ]->idx != want; k++)
        if (k % 4096 == 0 && now_ns() - t0 > 2000000000ull)
            die("the device did not take a frame within 2 s, length", (long)len);
    last_used[TXQ] = want;
}

static uint8_t *tx_frame(void) { return dma + DMA_TX + net_hdr; }

// The next received frame, or NULL after `timeout_ns` without one. With
// `sleep` the wait is on the interrupt (and then has no timeout: the
// caller has seen an answer come back by polling first). The buffer goes
// back to the device on the next call.
static const uint8_t *rx_next(size_t *len, int sleep, uint64_t timeout_ns) {
    static int pending = -1;
    if (pending >= 0) {
        rx_post((uint16_t)pending);
        notify(RXQ);
        pending = -1;
    }
    uint64_t t0 = now_ns();
    for (unsigned k = 1; used[RXQ]->idx == last_used[RXQ]; k++) {
        if (sleep) {
            int64_t ts = 0;
            long r = zx_status(zx(ZX_SYS_interrupt_wait, irq_handle, (long)&ts, 0, 0));
            if (r != 0)
                die("interrupt_wait", r);
            ack_device();
        } else if (k % 4096 == 0 && now_ns() - t0 > timeout_ns) {
            return NULL;
        }
    }
    if (!sleep)
        ack_device(); // nobody waits on it, but the line stays up until acked
    mb();
    volatile struct virtq_used_elem *e = &used[RXQ]->ring[last_used[RXQ] % QUEUE_SIZE];
    last_used[RXQ]++;
    pending = (int)e->id;
    if (e->id >= RX_BUFS || e->len < net_hdr || e->len > BUF_SIZE)
        die("the device returned a receive buffer that is not one of ours, id", e->id);
    *len = e->len - net_hdr;
    return dma + DMA_RX + (size_t)e->id * BUF_SIZE + net_hdr;
}

// ---- The little that is said on the wire ----

static const uint8_t MY_IP[4] = {10, 0, 2, 15};
static const uint8_t GW_IP[4] = {10, 0, 2, 2};
static uint8_t gw_mac[6];

static uint16_t checksum(const uint8_t *p, size_t n) {
    uint32_t sum = 0;
    for (size_t i = 0; i + 1 < n; i += 2)
        sum += (uint32_t)p[i] << 8 | p[i + 1];
    if (n & 1)
        sum += (uint32_t)p[n - 1] << 8;
    while (sum >> 16)
        sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)~sum;
}

static void put16(uint8_t *p, uint16_t v) {
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}

// Ask who has the gateway's address and take the answer.
static int arp_gateway(void) {
    uint8_t *f = tx_frame();
    memset(f, 0xff, 6);
    memcpy(f + 6, mac, 6);
    put16(f + 12, 0x0806);
    uint8_t *a = f + 14;
    put16(a, 1);          // Ethernet
    put16(a + 2, 0x0800); // IPv4
    a[4] = 6;
    a[5] = 4;
    put16(a + 6, 1); // request
    memcpy(a + 8, mac, 6);
    memcpy(a + 14, MY_IP, 4);
    memset(a + 18, 0, 6);
    memcpy(a + 24, GW_IP, 4);
    for (int attempt = 0; attempt < 3; attempt++) {
        tx_send(60); // the shortest frame Ethernet carries
        uint64_t t0 = now_ns();
        while (now_ns() - t0 < 1000000000ull) {
            size_t len;
            const uint8_t *r = rx_next(&len, 0, 1000000000ull);
            if (!r)
                break;
            if (len >= 42 && r[12] == 0x08 && r[13] == 0x06 && r[21] == 2 &&
                !memcmp(r + 28, GW_IP, 4)) {
                memcpy(gw_mac, r + 22, 6);
                return 0;
            }
        }
    }
    return -1;
}

// One echo request with `payload` bytes and its reply. Returns the round
// trip in nanoseconds, or 0 when no reply came (polling only).
static uint64_t ping(uint16_t seq, size_t payload, int sleep) {
    uint8_t *f = tx_frame();
    memcpy(f, gw_mac, 6);
    memcpy(f + 6, mac, 6);
    put16(f + 12, 0x0800);
    uint8_t *ip = f + 14;
    size_t ip_len = 20 + 8 + payload;
    memset(ip, 0, 20);
    ip[0] = 0x45;
    put16(ip + 2, (uint16_t)ip_len);
    put16(ip + 4, seq);
    ip[8] = 64; // TTL
    ip[9] = 1;  // ICMP
    memcpy(ip + 12, MY_IP, 4);
    memcpy(ip + 16, GW_IP, 4);
    put16(ip + 10, checksum(ip, 20));
    uint8_t *icmp = ip + 20;
    memset(icmp, 0, 8);
    icmp[0] = 8; // echo request
    put16(icmp + 4, 0x5a58);
    put16(icmp + 6, seq);
    for (size_t i = 0; i < payload; i++)
        icmp[8 + i] = (uint8_t)(i + seq);
    put16(icmp + 2, checksum(icmp, 8 + payload));
    size_t frame = 14 + ip_len;
    uint64_t t0 = now_ns();
    tx_send(frame < 60 ? 60 : frame);
    for (;;) {
        size_t len;
        const uint8_t *r = rx_next(&len, sleep, 1000000000ull);
        if (!r)
            return 0;
        if (len < 14 + ip_len || r[12] != 0x08 || r[13] != 0x00 || r[23] != 1)
            continue; // not ICMP over IPv4, or too short to be the answer
        const uint8_t *ricmp = r + 14 + (r[14] & 0xf) * 4;
        if (ricmp[0] != 0 || ricmp[6] != (uint8_t)(seq >> 8) || ricmp[7] != (uint8_t)seq)
            continue; // not the reply to this request
        uint64_t t1 = now_ns();
        for (size_t i = 0; i < payload; i++)
            if (ricmp[8 + i] != (uint8_t)(i + seq))
                die("the echo came back changed, at byte", (long)i);
        return t1 - t0;
    }
}

static int ping_series(const char *how, size_t payload, int rounds, int sleep) {
    static uint16_t seq;
    uint64_t sum = 0, best = UINT64_MAX;
    for (int i = 0; i < rounds; i++) {
        uint64_t ns = ping(++seq, payload, sleep);
        if (ns == 0) {
            printf("zx-virtio-net: FAIL: no echo reply for %zu bytes (%s), ping %d\n", payload,
                   how, i);
            return -1;
        }
        sum += ns;
        if (ns < best)
            best = ns;
    }
    printf("zx-virtio-net: ping %4zu bytes x%d, %s: %8.1f us mean, %8.1f us best\n", payload,
           rounds, how, sum / 1e3 / rounds, best / 1e3);
    return 0;
}

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t transport_words[3];
    uint64_t interrupt;
};

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    long index = argc > 1 ? atol(argv[1]) : 0;
    int rounds = argc > 2 ? atoi(argv[2]) : 200;
    struct claim c;
    long r = zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_VIRTIO_NET, index, (long)&c, 0));
    if (r == -ENOSYS) {
        printf("zx-virtio-net: the gateway is off (boot with zircon.gateway=1)\n");
        return 2;
    }
    if (r != 0)
        die("claim (boot with udriver=virtio-net@N and a card there)", r);
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
    dma = map_handle(vmo, DMA_SIZE);
    memset(dma, 0, DMA_SIZE);
    for (int q = 0; q < 2; q++) {
        desc[q] = (struct virtq_desc *)(dma + DMA_Q(q));
        avail[q] = (struct virtq_avail *)(dma + DMA_Q(q) + QUEUE_SIZE * sizeof(struct virtq_desc));
        used[q] = (volatile struct virtq_used *)(dma + DMA_Q(q) + PAGE);
    }

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
    for (uint16_t i = 0; i < RX_BUFS; i++)
        rx_post(i);
    notify(RXQ);
    printf("zx-virtio-net: card %ld up (%s), mac %02x:%02x:%02x:%02x:%02x:%02x, interrupt %s\n",
           index, transport == ECLIPSE_TRANSPORT_VIRTIO_MMIO ? "virtio-mmio" : "virtio-pci",
           mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], irq_handle ? "yes" : "no");

    if (arp_gateway() != 0) {
        printf("zx-virtio-net: FAIL: nobody answered for 10.0.2.2\n");
        return 1;
    }
    printf("zx-virtio-net: 10.0.2.2 is at %02x:%02x:%02x:%02x:%02x:%02x\n", gw_mac[0], gw_mac[1],
           gw_mac[2], gw_mac[3], gw_mac[4], gw_mac[5]);
    int bad = 0;
    bad |= ping_series("polling", 56, rounds, 0);
    bad |= ping_series("polling", 1400, rounds, 0);
    if (irq_handle && !bad) {
        bad |= ping_series("interrupt", 56, rounds, 1);
        bad |= ping_series("interrupt", 1400, rounds, 1);
    }
    if (bad)
        return 1;
    printf("zx-virtio-net: all checks passed\n");
    return 0;
}
