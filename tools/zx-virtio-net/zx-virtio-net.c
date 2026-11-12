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
// on the wire as 10.0.3.15, on the user network `make UNET=1` gives it:
//
//   - asks who has 10.0.3.2 (the gateway QEMU emulates) and takes the
//     answer, which proves a frame out and a frame in;
//   - pings it, ROUNDS times with 56 bytes and ROUNDS times with 1400,
//     first polling the receive queue and then sleeping on the interrupt,
//     and reports the round trip of each.
//
//   zx-virtio-net [N [ROUNDS]]  (defaults: card 0, 200 pings)
//   zx-virtio-net export [N [ROUNDS]]
//
// With `export` the driver gives the card to the kernel instead
// (eclipse_net_serve): it becomes an interface of the kernel's, a new ethN,
// with frames passed over a FIFO and a window of the DMA memory that the
// card reads and writes directly (see "Serving the kernel"). A child then
// gives the interface an address and pings the gateway through the
// kernel's own stack.
//
//   zx-virtio-net service [N]
//
// The driver as what it is for: the card goes to the kernel and the
// process carries its frames until it is killed, checking nothing. This is
// what a supervisor runs (eclipse-init, `type = respawn`). When it dies
// the kernel resets the card and keeps the interface for ten seconds, with
// its addresses and the connections through it; the next `service` gets
// the same card and the same interface, and what was waiting goes out.
//
//   zx-virtio-net restart [N [ROUNDS [PORT [KILLS [c]]]]]
//
// The check of that: starts the driver as a child, gives the interface an
// address and pings; then KILLS times (default 2) kills the driver with
// SIGKILL -- in the middle of a download of http://10.0.3.2:PORT/blob when
// PORT is given -- and starts it again: the interface must still be there
// under the same name, the download must finish, and the gateway must
// answer pings with no address set again. At the end the driver is killed
// and not started again, and the interface must go.
//
// The driver `restart` starts is the one the system runs: the Rust one,
// /bin/udrv-virtio-net (tools/eclipse-udrivers), where the image has it,
// and this program's own `service` where it does not, or with `c`.
//
//   zx-virtio-net bench [N [ROUNDS [PORT [c]]]]
//
// Starts the same driver as a child and uses the interface as `export`'s
// check does: pings, and three downloads when PORT is given. It is how the
// two drivers are compared, one boot each.
//
//   zx-virtio-net ping IFACE ADDRESS GATEWAY [ROUNDS]
//
// The baseline: the same pings through an interface the kernel drives
// itself (on x86_64, `ping eth0 10.0.2.15 10.0.2.2` with the default
// e1000e). In a boot of its own: with two cards the kernel's sockets go
// out through whichever interface is polled first, so boot the served
// card alone (`NIC=none`).
//
// Run it on Eclipse booted with the udriver option, which turns the Zircon
// gateway on unless `zircon.gateway=0` says otherwise (`make ... UNET=1`
// adds the card). Exit status: 0 on success, 2 when the gateway is off, 1
// on error.
//
// A prototype: without `export`, no stack above it and one frame in flight.

#define _GNU_SOURCE
#include <errno.h>
#include <net/if.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ZX_GATEWAY_BASE 0x5a000000L
#define ZX_GATEWAY_INDIRECT 0x8000
#define ZX_SYS_bti_pin 1
#define ZX_SYS_interrupt_wait 54
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
#define ZX_ERR_ALREADY_BOUND (-27)
#define ECLIPSE_SYS_net_serve 0x4007
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
// is simply told about), then the window: the receive buffers and the
// transmit buffers, a slot each. With `export` the kernel reaches the
// window too, and that is all of the driver's memory it reaches.
#define DMA_Q(q) ((q) * 2 * PAGE)
// As many receive buffers as the queue has descriptors. The card fills what
// it is given in one go and the kernel takes the frames in one go, so the
// ring is the size of a burst: with 64 instead of 32 a 32 MiB download over
// TCP at one CPU takes 1.79 s instead of 2.08 s.
#define RX_BUFS 64
#define TX_BUFS 32
#define BUF_SIZE 2048 // a header and a 1514-byte frame, with room
#define TX_HEADROOM 16 // where a frame starts in a transmit slot
#define DMA_WIN (4 * PAGE)
#define DMA_RX DMA_WIN
#define DMA_TX(i) (DMA_WIN + (RX_BUFS + (i)) * BUF_SIZE)
#define WIN_SIZE ((RX_BUFS + TX_BUFS) * BUF_SIZE)
#define DMA_SIZE (DMA_WIN + WIN_SIZE)
_Static_assert(RX_BUFS <= QUEUE_SIZE, "a descriptor per receive buffer");
_Static_assert(TX_BUFS <= QUEUE_SIZE, "a descriptor per transmit buffer");
_Static_assert(WIN_SIZE % PAGE == 0, "the window is whole pages");

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
// With `export` the transmit queue interrupts too: its completions are
// what frees the kernel's transmit slots.
static int tx_irq;

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
        // claim's interrupt object listens on. Without `export` only the
        // receive queue raises it: a sent frame is reclaimed before the
        // next is sent.
        uint16_t vector = irq_handle && (q == RXQ || tx_irq) ? 0 : VIRTIO_MSI_NO_VECTOR;
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

// Hand the `len`-byte frame in transmit slot `i` (descriptor `i`) to the
// device, its header in the headroom before it.
static void tx_post(uint16_t i, size_t len) {
    size_t at = DMA_TX(i) + TX_HEADROOM - net_hdr;
    memset(dma + at, 0, net_hdr);
    desc[TXQ][i].addr = daddr + at;
    desc[TXQ][i].len = (uint32_t)(net_hdr + len);
    desc[TXQ][i].flags = 0;
    desc[TXQ][i].next = 0;
    avail[TXQ]->ring[avail[TXQ]->idx % QUEUE_SIZE] = i;
    mb();
    avail[TXQ]->idx++;
    mb();
}

// Send the `len`-byte frame in transmit slot 0, and wait for the device to
// take it: one frame in flight.
static void tx_send(size_t len) {
    tx_post(0, len);
    notify(TXQ);
    uint16_t want = (uint16_t)(last_used[TXQ] + 1);
    uint64_t t0 = now_ns();
    for (unsigned k = 1; used[TXQ]->idx != want; k++)
        if (k % 4096 == 0 && now_ns() - t0 > 2000000000ull)
            die("the device did not take a frame within 2 s, length", (long)len);
    last_used[TXQ] = want;
}

static uint8_t *tx_frame(void) { return dma + DMA_TX(0) + TX_HEADROOM; }

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

static const uint8_t MY_IP[4] = {10, 0, 3, 15};
static const uint8_t GW_IP[4] = {10, 0, 3, 2};
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

// ---- Serving the kernel ----
//
// The card becomes an interface of the kernel's. Frames pass through the
// window, where the card's buffers already are, and a FIFO of 16-byte
// messages says which: `op, slot, len, off`.
//
//   NET_RX, to the kernel: a frame of `len` bytes is in receive slot
//     `slot`, `off` bytes in. The kernel sends the message back when it has
//     copied the frame out, and the slot goes back to the card.
//   NET_TX, from the kernel: a frame of `len` bytes is in transmit slot
//     `slot`, TX_HEADROOM bytes in. It goes back once the card has taken
//     the frame, and the kernel may use the slot again.
//
// Two threads, each with one side of both queues: the one that sleeps on
// the card's interrupt reads the used rings and writes to the FIFO, and
// the one that sleeps on the FIFO fills the avail rings. Neither can find
// the FIFO full: it holds as many messages as there are slots.

#define NET_RX 1
#define NET_TX 2
#define FIFO_DEPTH 128
_Static_assert(RX_BUFS + TX_BUFS <= FIFO_DEPTH, "a message per slot fits the FIFO");

struct net_msg {
    uint32_t op, slot, len, off;
};

static zx_handle_t fifo;

// `n` messages to the kernel, in as few calls as the FIFO takes them in.
// It has room for all of them: a message per slot, and no slot is in two.
static void fifo_put(const struct net_msg *m, size_t n) {
    while (n) {
        size_t put = 0;
        long r = zx_status(zx6(ZX_SYS_fifo_write, fifo, sizeof *m, (long)m, n, (long)&put, 0));
        if (r != 0 || put == 0 || put > n)
            die("fifo_write", r);
        m += put;
        n -= put;
    }
}

// What the card has finished, to the kernel. Returns how much there was.
static int drain_used(void) {
    struct net_msg m[RX_BUFS + TX_BUFS];
    int n = 0;
    mb();
    while (used[RXQ]->idx != last_used[RXQ] && n < RX_BUFS) {
        volatile struct virtq_used_elem *e = &used[RXQ]->ring[last_used[RXQ] % QUEUE_SIZE];
        last_used[RXQ]++;
        if (e->id >= RX_BUFS || e->len < net_hdr || e->len > BUF_SIZE)
            die("the device returned a receive buffer that is not one of ours, id", e->id);
        m[n++] = (struct net_msg){NET_RX, e->id, e->len - (uint32_t)net_hdr, (uint32_t)net_hdr};
    }
    for (int tx = 0; used[TXQ]->idx != last_used[TXQ] && tx < TX_BUFS; tx++) {
        volatile struct virtq_used_elem *e = &used[TXQ]->ring[last_used[TXQ] % QUEUE_SIZE];
        last_used[TXQ]++;
        m[n++] = (struct net_msg){NET_TX, e->id, 0, 0};
    }
    fifo_put(m, (size_t)n);
    return n;
}

static void *card_thread(void *arg) {
    (void)arg;
    for (;;) {
        if (irq_handle) {
            int64_t ts = 0;
            long r = zx_status(zx(ZX_SYS_interrupt_wait, irq_handle, (long)&ts, 0, 0));
            if (r != 0)
                die("interrupt_wait", r);
            ack_device();
            while (drain_used()) {
            }
        } else if (!drain_used()) {
            ack_device();
            usleep(200); // no interrupt to sleep on
        }
    }
    return NULL;
}

// A read, and a wait when it finds nothing: not eclipse_fifo_reply_wait,
// the one call the disk driver makes of the two. It was tried here and
// measured (32 MiB over TCP, one CPU): a thread that reads the moment it is
// woken gives the card its buffers back two or three at a time instead of
// the whole ring, the card then delivers two or three frames at a time, and
// the stack acknowledges each of those bursts -- ten thousand ACKs for a
// download instead of eight hundred, and 3.4 s instead of 2.0 s on the
// boots where it settles that way. The trip back through user mode before
// the read is what lets the kernel finish what it is sending.
static void *fifo_thread(void *arg) {
    (void)arg;
    for (;;) {
        struct net_msg m[RX_BUFS + TX_BUFS];
        size_t got = 0;
        long r = zx_status(
            zx6(ZX_SYS_fifo_read, fifo, sizeof m[0], (long)m, sizeof m / sizeof m[0], (long)&got, 0));
        if (r == ZX_ERR_SHOULD_WAIT) {
            uint32_t observed = 0;
            r = zx_status(zx6(ZX_SYS_object_wait_one, fifo, ZX_FIFO_READABLE | ZX_FIFO_PEER_CLOSED,
                              ZX_TIME_INFINITE, (long)&observed, 0, 0));
            if (r < 0 || !(observed & ZX_FIFO_READABLE))
                die("the kernel closed the FIFO", r);
            continue;
        }
        if (r != 0)
            die("fifo_read", r);
        int rx = 0, tx = 0;
        for (size_t i = 0; i < got; i++) {
            if (m[i].op == NET_RX && m[i].slot < RX_BUFS) {
                rx_post((uint16_t)m[i].slot);
                rx = 1;
            } else if (m[i].op == NET_TX && m[i].slot < TX_BUFS && m[i].len <= 1514 &&
                       m[i].off == TX_HEADROOM) {
                tx_post((uint16_t)m[i].slot, m[i].len);
                tx = 1;
            } else {
                die("a message from the kernel that means nothing, op", m[i].op);
            }
        }
        if (rx)
            notify(RXQ);
        if (tx)
            notify(TXQ);
    }
    return NULL;
}

// The interface whose address is `mac`, by asking each ethN for its own.
static int find_ifname(char *name, size_t len) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0)
        return -1;
    for (int i = 0; i < 8; i++) {
        struct ifreq ifr;
        memset(&ifr, 0, sizeof ifr);
        snprintf(ifr.ifr_name, sizeof ifr.ifr_name, "eth%d", i);
        if (ioctl(s, SIOCGIFHWADDR, &ifr) == 0 && !memcmp(ifr.ifr_hwaddr.sa_data, mac, 6)) {
            snprintf(name, len, "eth%d", i);
            close(s);
            return 0;
        }
    }
    close(s);
    return -1;
}

static int sh(const char *cmd) {
    fflush(stdout);
    int st = system(cmd);
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}

// The larger ping: the kernel's ICMP path keeps replies of up to 1280 bytes.
#define PING_BIG 1200
// The summary of a run of pings, and the status of the ping itself: 0 only
// when replies came back.
#define PING_CMD "ping -c %d -i 0.01 -s %d -W 2 %s > /tmp/ping.out; rc=$?; tail -2 /tmp/ping.out; exit $rc"

// Fetch http://GW:PORT/blob three times over TCP and say how fast it came:
// what a download through the interface is worth. The server is whatever
// listens on that port of the host (QEMU's user network hands the
// gateway's ports to it).
static int fetch_through(const char *name, const char *what, const char *gw, int port) {
    char cmd[256];
    snprintf(cmd, sizeof cmd, "wget -q -O /tmp/blob http://%s:%d/blob", gw, port);
    for (int i = 0; i < 3; i++) {
        struct timespec a, b;
        struct stat st;
        clock_gettime(CLOCK_MONOTONIC, &a);
        int rc = sh(cmd);
        clock_gettime(CLOCK_MONOTONIC, &b);
        if (rc != 0 || stat("/tmp/blob", &st) != 0 || st.st_size == 0) {
            printf("zx-virtio-net: FAIL: %s\n", cmd);
            return 1;
        }
        double ms = (b.tv_sec - a.tv_sec) * 1e3 + (b.tv_nsec - a.tv_nsec) / 1e6;
        printf("zx-virtio-net: %s (%s), tcp: %ld KiB in %.0f ms, %.1f MiB/s\n", name, what,
               (long)(st.st_size / 1024), ms, st.st_size / 1048576.0 / (ms / 1e3));
        unlink("/tmp/blob");
    }
    return 0;
}

// Give `name` the address `ip` and ping `gw` through the kernel's stack,
// with 56 bytes and with PING_BIG. 0 when replies came back to both.
static int ping_through(const char *name, const char *what, const char *ip, const char *gw,
                        int rounds, int port) {
    char cmd[256];
    snprintf(cmd, sizeof cmd, "ifconfig %s %s netmask 255.255.255.0 up", name, ip);
    if (sh(cmd) != 0) {
        printf("zx-virtio-net: FAIL: %s\n", cmd);
        return 1;
    }
    for (int size = 56; size <= PING_BIG; size += PING_BIG - 56) {
        printf("zx-virtio-net: %s (%s), ping %d bytes x%d:\n", name, what, size, rounds);
        snprintf(cmd, sizeof cmd, PING_CMD, rounds, size, gw);
        if (sh(cmd) != 0) {
            printf("zx-virtio-net: FAIL: %s\n", cmd);
            return 1;
        }
    }
    return port ? fetch_through(name, what, gw, port) : 0;
}

// The check, in a child: the interface is the kernel's now, so it is used
// the way anything else uses the network.
static int export_check(int rounds, int port) {
    char name[16];
    if (find_ifname(name, sizeof name) != 0) {
        printf("zx-virtio-net: FAIL: no interface has the card's address\n");
        return 1;
    }
    printf("zx-virtio-net: the card is %s\n", name);
    return ping_through(name, "driver in userspace", "10.0.3.15", "10.0.3.2", rounds, port);
}

// How long the kernel keeps an interface whose driver died, for the next
// driver to pick up (DRIVER_GRACE in loader/src/linux/net_serve.rs), and a
// margin to look for it gone.
#define DRIVER_GRACE_MS 10000
#define GONE_MARGIN_MS 5000

// Wait for the card's interface to go once its driver is dead for good.
// 0 when it went, and not before the kernel's grace was nearly up.
static int interface_goes(void) {
    char name[16] = "";
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    for (int i = 0; i < (DRIVER_GRACE_MS + GONE_MARGIN_MS) / 10; i++) {
        if (find_ifname(name, sizeof name) != 0) {
            clock_gettime(CLOCK_MONOTONIC, &b);
            double s = (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9;
            printf("zx-virtio-net: no driver came back and the interface went, after %.1f s\n", s);
            return 0;
        }
        usleep(10000);
    }
    printf("zx-virtio-net: FAIL: the driver is gone for good and %s is still there\n", name);
    return 1;
}

// What outlives the driver: it sleeps until the driver is gone (the pipe
// closes), and then the interface must go too.
static void watch_the_end(int pipe_rd) {
    char c;
    while (read(pipe_rd, &c, 1) > 0) {
    }
    if (interface_goes() != 0)
        _exit(1);
    printf("zx-virtio-net: all checks passed\n");
    _exit(0);
}

// Hand the card to the kernel and run the two threads that carry its
// frames. From here on it is an interface of the kernel's.
static void serve_card(zx_handle_t window) {
    zx_handle_t ends[2];
    long r = zx_status(zx6(ZX_SYS_fifo_create, FIFO_DEPTH, sizeof(struct net_msg), 0,
                           (long)&ends[0], (long)&ends[1], 0));
    if (r != 0)
        die("fifo_create", r);
    fifo = ends[0];
    uint64_t mac64 = 0;
    for (int i = 0; i < 6; i++)
        mac64 |= (uint64_t)mac[i] << (8 * i);
    r = zx_status(zx(ECLIPSE_SYS_net_serve, ends[1], window, (long)mac64, RX_BUFS));
    if (r != 0)
        die("net_serve", r);
    printf("zx-virtio-net: card handed to the kernel\n");
    pthread_t card, msgs;
    if (pthread_create(&card, NULL, card_thread, NULL) || pthread_create(&msgs, NULL, fifo_thread, NULL))
        die("pthread_create", errno);
}

static int export_card(zx_handle_t window, int rounds, int port) {
    serve_card(window);
    int end[2];
    if (pipe(end) != 0)
        die("pipe", errno);
    pid_t pid = fork();
    if (pid < 0)
        die("fork", errno);
    if (pid == 0) {
        close(end[1]);
        watch_the_end(end[0]);
    }
    close(end[0]);
    pid = fork();
    if (pid < 0)
        die("fork", errno);
    if (pid == 0) {
        close(end[1]);
        _exit(export_check(rounds, port));
    }
    int st = 0;
    waitpid(pid, &st, 0);
    // Leaving closes the pipe and, with the process, its end of the FIFO.
    return !WIFEXITED(st) || WEXITSTATUS(st) != 0;
}

struct claim {
    uint32_t regs, bti;
    uint32_t transport, irq;
    uint64_t offset, size;
    uint64_t transport_words[3];
    uint64_t interrupt;
};

// Claim card `index` and set it up: queues, features, receive buffers
// posted. Returns the window of slots the kernel is to reach (0 unless
// `exporting`). `patient`: a claim refused because the card has a driver
// is tried again for a while -- the driver before this one may have died
// a moment ago, and the kernel resets the card before it gives it out.
static zx_handle_t card_up(long index, int exporting, int patient) {
    struct claim c;
    long r;
    for (int tries = patient ? 300 : 1;; usleep(10000)) {
        r = zx_status(zx(ECLIPSE_SYS_device_claim, ECLIPSE_DEVICE_VIRTIO_NET, index, (long)&c, 0));
        if (r != ZX_ERR_ALREADY_BOUND || --tries <= 0)
            break;
    }
    if (r == -ENOSYS) {
        printf("zx-virtio-net: the gateway is off (zircon.gateway=0 on the command line?)\n");
        exit(2);
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
    zx_handle_t window = 0; // the slots, as the kernel will reach them
    if (exporting) {
        r = zx_status(zx6(ZX_SYS_vmo_create_child, vmo, ZX_VMO_CHILD_SLICE, DMA_WIN, WIN_SIZE,
                          (long)&window, 0));
        if (r != 0)
            die("vmo_create_child (slice)", r);
    }
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
    return window;
}

// The driver as a service: the card goes to the kernel and this process
// carries its frames until it is killed. Whoever started it starts it
// again if it dies (eclipse-init, with `type = respawn`), and the kernel
// gives the next one the same card and the same interface. `report` >= 0
// is a pipe that is told the card's address once it is up.
static int service(long index, int report) {
    tx_irq = 1;
    serve_card(card_up(index, 1, 1));
    if (report >= 0) {
        if (write(report, mac, sizeof mac) != sizeof mac)
            die("write (report)", errno);
        close(report);
    }
    // Until a signal ends the process: there is nothing to return to.
    while (pause() != 0) {
    }
    return 0;
}

// The driver the system runs, when the image has it.
#define SYSTEM_DRIVER "/bin/udrv-virtio-net"
// Set by `c`: start this program's own `service` whatever the image has.
static int own_driver;

// Which driver start_driver() starts, for the log.
static const char *driver_name(void) {
    return !own_driver && access(SYSTEM_DRIVER, X_OK) == 0 ? SYSTEM_DRIVER : "this program's";
}

// Start the driver as a child, the way a supervisor would, and wait until
// it says the card is up. Its pid, or -1.
static pid_t start_driver(long index) {
    int up[2];
    if (pipe(up) != 0)
        die("pipe", errno);
    pid_t pid = fork();
    if (pid < 0)
        die("fork", errno);
    if (pid == 0) {
        close(up[0]);
        if (!own_driver && access(SYSTEM_DRIVER, X_OK) == 0) {
            char a_index[24], a_report[24];
            snprintf(a_index, sizeof a_index, "%ld", index);
            snprintf(a_report, sizeof a_report, "%d", up[1]);
            execl(SYSTEM_DRIVER, SYSTEM_DRIVER, a_index, a_report, (char *)NULL);
            printf("zx-virtio-net: exec %s: %s\n", SYSTEM_DRIVER, strerror(errno));
            _exit(127);
        }
        _exit(service(index, up[1]));
    }
    close(up[1]);
    ssize_t n = read(up[0], mac, sizeof mac);
    close(up[0]);
    if (n != sizeof mac) {
        printf("zx-virtio-net: FAIL: the driver did not come up\n");
        return -1;
    }
    return pid;
}

static void kill_driver(pid_t pid) {
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
}

static int ping_once(const char *what, const char *gw, int rounds, int size) {
    char cmd[256];
    printf("zx-virtio-net: %s, ping %d bytes x%d:\n", what, size, rounds);
    snprintf(cmd, sizeof cmd, PING_CMD, rounds, size, gw);
    if (sh(cmd) != 0) {
        printf("zx-virtio-net: FAIL: %s\n", cmd);
        return 1;
    }
    return 0;
}

// The check of what a supervisor counts on: the driver is killed `kills`
// times, each time in the middle of a download when there is a server to
// download from, and started again. The interface must keep its name and
// its address, the download must finish, and when the driver is killed and
// not started again the interface must go.
// The driver started as a child, and its interface used: pings, and
// downloads when there is a `port` to fetch from.
static int bench(long index, int rounds, int port) {
    char name[16];
    pid_t driver = start_driver(index);
    if (driver < 0)
        return 1;
    if (find_ifname(name, sizeof name) != 0) {
        printf("zx-virtio-net: FAIL: no interface has the card's address\n");
        return 1;
    }
    printf("zx-virtio-net: the card is %s, its driver (%s) pid %d\n", name, driver_name(),
           (int)driver);
    int bad = ping_through(name, "driver in userspace", "10.0.3.15", "10.0.3.2", rounds, port);
    kill_driver(driver);
    if (!bad)
        printf("zx-virtio-net: all checks passed\n");
    return bad;
}

static int restart_check(long index, int rounds, int port, int kills) {
    const char *ip = "10.0.3.15", *gw = "10.0.3.2";
    char name[16], now[16], cmd[256];
    pid_t driver = start_driver(index);
    if (driver < 0)
        return 1;
    if (find_ifname(name, sizeof name) != 0) {
        printf("zx-virtio-net: FAIL: no interface has the card's address\n");
        return 1;
    }
    printf("zx-virtio-net: the card is %s, its driver (%s) pid %d\n", name, driver_name(),
           (int)driver);
    snprintf(cmd, sizeof cmd, "ifconfig %s %s netmask 255.255.255.0 up", name, ip);
    if (sh(cmd) != 0 || ping_once("first driver", gw, rounds, 56) != 0)
        return 1;
    for (int k = 1; k <= kills; k++) {
        pid_t wget = -1;
        struct timespec a, b;
        clock_gettime(CLOCK_MONOTONIC, &a);
        if (port) {
            snprintf(cmd, sizeof cmd, "http://%s:%d/blob", gw, port);
            wget = fork();
            if (wget < 0)
                die("fork", errno);
            if (wget == 0) {
                execlp("wget", "wget", "-q", "-O", "/tmp/blob", cmd, (char *)NULL);
                _exit(127);
            }
            usleep(300000); // into the download
        }
        kill_driver(driver);
        // Nothing above the driver went with it.
        if (find_ifname(now, sizeof now) != 0 || strcmp(now, name) != 0) {
            printf("zx-virtio-net: FAIL: %s went with its driver\n", name);
            return 1;
        }
        driver = start_driver(index);
        if (driver < 0)
            return 1;
        if (find_ifname(now, sizeof now) != 0 || strcmp(now, name) != 0) {
            printf("zx-virtio-net: FAIL: the new driver's card is not %s\n", name);
            return 1;
        }
        printf("zx-virtio-net: driver killed and started again (%d of %d), still %s\n", k, kills,
               name);
        if (wget > 0) {
            int st = 0;
            struct stat sb;
            waitpid(wget, &st, 0);
            clock_gettime(CLOCK_MONOTONIC, &b);
            if (!WIFEXITED(st) || WEXITSTATUS(st) != 0 || stat("/tmp/blob", &sb) != 0) {
                printf("zx-virtio-net: FAIL: the download did not survive the driver\n");
                return 1;
            }
            double ms = (b.tv_sec - a.tv_sec) * 1e3 + (b.tv_nsec - a.tv_nsec) / 1e6;
            printf("zx-virtio-net: the download across it finished: %ld KiB in %.0f ms\n",
                   (long)(sb.st_size / 1024), ms);
            unlink("/tmp/blob");
        }
        // The address was the interface's, not the driver's: no ifconfig.
        if (ping_once("driver started again", gw, rounds, 56) != 0)
            return 1;
    }
    kill_driver(driver);
    if (interface_goes() != 0)
        return 1;
    printf("zx-virtio-net: all checks passed\n");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc >= 5 && !strcmp(argv[1], "ping")) {
        int bad = ping_through(argv[2], "the kernel's driver", argv[3], argv[4],
                               argc > 5 ? atoi(argv[5]) : 200, argc > 6 ? atoi(argv[6]) : 0);
        if (!bad)
            printf("zx-virtio-net: all checks passed\n");
        return bad;
    }
    if (argc > 1 && !strcmp(argv[1], "service"))
        return service(argc > 2 ? atol(argv[2]) : 0, -1);
    if (argc > 1 && !strcmp(argv[1], "restart")) {
        own_driver = argc > 6 && !strcmp(argv[6], "c");
        return restart_check(argc > 2 ? atol(argv[2]) : 0, argc > 3 ? atoi(argv[3]) : 20,
                             argc > 4 ? atoi(argv[4]) : 0, argc > 5 ? atoi(argv[5]) : 2);
    }
    if (argc > 1 && !strcmp(argv[1], "bench")) {
        own_driver = argc > 5 && !strcmp(argv[5], "c");
        return bench(argc > 2 ? atol(argv[2]) : 0, argc > 3 ? atoi(argv[3]) : 200,
                     argc > 4 ? atoi(argv[4]) : 0);
    }
    int exporting = argc > 1 && !strcmp(argv[1], "export");
    if (exporting) {
        argv++;
        argc--;
    }
    tx_irq = exporting;
    long index = argc > 1 ? atol(argv[1]) : 0;
    int rounds = argc > 2 ? atoi(argv[2]) : 200;
    zx_handle_t window = card_up(index, exporting, 0);

    if (exporting)
        return export_card(window, rounds, argc > 3 ? atoi(argv[3]) : 0);

    if (arp_gateway() != 0) {
        printf("zx-virtio-net: FAIL: nobody answered for 10.0.3.2\n");
        return 1;
    }
    printf("zx-virtio-net: 10.0.3.2 is at %02x:%02x:%02x:%02x:%02x:%02x\n", gw_mac[0], gw_mac[1],
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
