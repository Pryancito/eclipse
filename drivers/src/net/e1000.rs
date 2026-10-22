//! Intel PRO/1000 Network Adapter i.e. e1000 network driver
//! Datasheet: <https://www.intel.ca/content/dam/doc/datasheet/82574l-gbe-controller-datasheet.pdf>

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use managed::ManagedSlice;
use smoltcp::iface::*;
use smoltcp::phy::{self, Checksum, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::{timer_now_as_micros, ProviderImpl};
use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::net::get_sockets;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme, SchemeUpcast};
use crate::utils::dma::DmaRegion;
use crate::{Device, DeviceError, DeviceResult};
use core::sync::atomic::{fence, AtomicBool, Ordering};
use lock::Mutex;
use pci::{PCIDevice, BAR};

const NUM_DESC: usize = 256;
/// Size in bytes of each RX/TX DMA buffer (one page).
const RX_BUF_SIZE: usize = 4096;
/// Stack scratch for [`E1000TxToken::consume`]; covers the advertised MTU
/// (1514) with headroom. Longer frames fall back to a heap buffer — they must
/// never be clipped, see the comment there.
const TX_SCRATCH_LEN: usize = 1536;

/// The reason the scratch exists is that the common case must not allocate,
/// so it has to cover the MTU this driver advertises to smoltcp
/// (`DeviceCapabilities::max_transmission_unit`, 1514). Checked here rather
/// than in a test: neither value can change at run time, so the build is the
/// right place to catch someone lowering one of them.
const _: () = assert!(TX_SCRATCH_LEN >= 1514);

/// The next descriptor index, wrapping at the end of the ring.
#[inline]
fn next_desc(idx: usize) -> usize {
    (idx + 1) % NUM_DESC
}

/// How many bytes of an RX buffer a completed descriptor really covers.
///
/// `len` is written by the *device*, so it is not ours to trust: the buffer it
/// describes is one page, and a length past that would have us build a slice
/// over memory the descriptor never owned.
#[inline]
fn rx_len_clamped(device_len: u16) -> usize {
    (device_len as usize).min(RX_BUF_SIZE)
}

/// Whether a frame of `len` bytes may be posted at all.
///
/// Rejecting is the point: clipping a frame to the buffer size put a corrupt,
/// half-length packet on the wire while `NetScheme::send` still reported the
/// full length as written, so the failure showed up as data loss on the peer
/// rather than as an error here.
#[inline]
fn tx_frame_postable(len: usize) -> bool {
    len != 0 && len <= RX_BUF_SIZE
}

/// Whether a frame can be posted at `next_to_use`, given a way to read a
/// descriptor's DD bit. The tail slot must be free **and the one after it**.
///
/// The NIC has no ownership bit of its own: it works descriptors from TDH up
/// to (but excluding) TDT and reads `TDH == TDT` as an EMPTY ring. Gating
/// purely per-slot on DD let software post all `NUM_DESC` slots -- every
/// descriptor starts pre-armed DD, so with the link still negotiating (or the
/// NIC otherwise stalled) 256 frames post in a row, the tail wraps onto the
/// head, and the `TDT` write hands the NIC a value equal to TDH. From its side
/// the ring is empty: the posted frames are never fetched, their DD bits never
/// come back, and every later `send` finds DD clear at slot 0 and fails
/// forever. TX is dead until reboot, with no watchdog to notice. Linux's
/// `e1000_desc_unused` bounds in-flight descriptors to `count - 1` for the
/// same reason, and `e1000e::tx_can_post` is the same guard.
#[inline]
fn tx_ring_has_room(next_to_use: usize, dd_at: impl Fn(usize) -> bool) -> bool {
    dd_at(next_to_use) && dd_at(next_desc(next_to_use))
}

/// Somewhere to build a `len`-byte frame: the stack scratch when it fits, a
/// heap buffer when it does not.
///
/// Do NOT index a fixed-size stack array with `len`: it is whatever the IP
/// layer computed for this datagram, and this smoltcp has neither IP
/// fragmentation nor an egress MTU clamp (`max_transmission_unit` only feeds
/// the TCP MSS). A single `sendto()` of a 1495-byte UDP payload -- never mind
/// a 64 KiB one, or a raw socket -- makes `len` exceed the buffer, and
/// `&mut scratch[..len]` panics inside the kernel.
fn tx_stage<'a>(
    len: usize,
    scratch: &'a mut [u8; TX_SCRATCH_LEN],
    heap: &'a mut Vec<u8>,
) -> &'a mut [u8] {
    if len <= TX_SCRATCH_LEN {
        &mut scratch[..len]
    } else {
        heap.resize(len, 0);
        heap.as_mut_slice()
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct E1000SendDesc {
    addr: u64,
    len: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct E1000RecvDesc {
    addr: u64,
    len: u16,
    chksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

pub struct E1000 {
    base: usize,
    size: usize,
    mac: EthernetAddress,

    // RX ring & buffers
    rx_ring: DmaRegion,
    rx_bufs: Vec<DmaRegion>,
    rx_next_to_clean: usize,

    // TX ring & buffers
    tx_ring: DmaRegion,
    tx_bufs: Vec<DmaRegion>,
    tx_next_to_use: usize,
}

#[inline]
unsafe fn mmio_read(base: usize, reg: usize) -> u32 {
    core::ptr::read_volatile((base + reg * 4) as *const u32)
}

#[inline]
unsafe fn mmio_write(base: usize, reg: usize, val: u32) {
    core::ptr::write_volatile((base + reg * 4) as *mut u32, val);
}

const E1000_CTRL: usize = 0x0000 / 4;
const E1000_STATUS: usize = 0x0008 / 4;
const E1000_ICR: usize = 0x00C0 / 4;
const E1000_IMS: usize = 0x00D0 / 4;
const E1000_IMC: usize = 0x00D8 / 4;
const E1000_RCTL: usize = 0x0100 / 4;
const E1000_TCTL: usize = 0x0400 / 4;
const E1000_TIPG: usize = 0x0410 / 4;
const E1000_RDBAL: usize = 0x2800 / 4;
const E1000_RDBAH: usize = 0x2804 / 4;
const E1000_RDLEN: usize = 0x2808 / 4;
const E1000_RDH: usize = 0x2810 / 4;
const E1000_RDT: usize = 0x2818 / 4;
const E1000_TDBAL: usize = 0x3800 / 4;
const E1000_TDBAH: usize = 0x3804 / 4;
const E1000_TDLEN: usize = 0x3808 / 4;
const E1000_TDH: usize = 0x3810 / 4;
const E1000_TDT: usize = 0x3818 / 4;
const E1000_MTA: usize = 0x5200 / 4;
const E1000_RAL: usize = 0x5400 / 4;
const E1000_RAH: usize = 0x5404 / 4;

impl E1000 {
    fn udelay(us: u64) {
        let t0 = timer_now_as_micros();
        const MAX_SPINS: u64 = 10_000_000;
        let mut spins = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins >= MAX_SPINS {
                break;
            }
        }
    }

    pub fn new(base: usize, size: usize, mac: EthernetAddress) -> DeviceResult<Self> {
        let rx_ring = DmaRegion::alloc(4096).ok_or(DeviceError::IoError)?;
        let tx_ring = DmaRegion::alloc(4096).ok_or(DeviceError::IoError)?;

        let mut rx_bufs = Vec::with_capacity(NUM_DESC);
        let mut tx_bufs = Vec::with_capacity(NUM_DESC);

        for _ in 0..NUM_DESC {
            let buf = DmaRegion::alloc_uninit(RX_BUF_SIZE).ok_or(DeviceError::IoError)?;
            rx_bufs.push(buf);
        }

        for _ in 0..NUM_DESC {
            let buf = DmaRegion::alloc_uninit(RX_BUF_SIZE).ok_or(DeviceError::IoError)?;
            tx_bufs.push(buf);
        }

        // Initialize descriptors
        let rx_desc_slice =
            unsafe { core::slice::from_raw_parts_mut(rx_ring.as_ptr::<E1000RecvDesc>(), NUM_DESC) };
        for (i, desc) in rx_desc_slice.iter_mut().enumerate() {
            desc.addr = rx_bufs[i].paddr() as u64;
            desc.status = 0;
            desc.errors = 0;
            desc.len = 0;
            desc.chksum = 0;
            desc.special = 0;
        }

        let tx_desc_slice =
            unsafe { core::slice::from_raw_parts_mut(tx_ring.as_ptr::<E1000SendDesc>(), NUM_DESC) };
        for (i, desc) in tx_desc_slice.iter_mut().enumerate() {
            desc.addr = tx_bufs[i].paddr() as u64;
            desc.len = 0;
            desc.cso = 0;
            desc.cmd = 0;
            // DD (Descriptor Done) set: a fresh, never-used TX slot must read as
            // "done/free" so `can_send`/`send` can gate on the real ownership bit
            // from the very first transmit. Initializing it clear forced a
            // `first_trans` flag that blanket-skipped the ownership check for the
            // first ring lap, which could overwrite an in-flight descriptor.
            desc.status = 1;
            desc.css = 0;
            desc.special = 0;
        }

        unsafe {
            // 1. Disable interrupts
            mmio_write(base, E1000_IMC, 0xffffffff);
            let _ = mmio_read(base, E1000_IMC);

            // 2. Device Reset
            let ctrl = mmio_read(base, E1000_CTRL);
            mmio_write(base, E1000_CTRL, ctrl | (1 << 26)); // Device Reset (RST)

            // Wait for reset to complete
            Self::udelay(10_000); // 10ms

            // 3. Disable interrupts again just in case
            mmio_write(base, E1000_IMC, 0xffffffff);
            let _ = mmio_read(base, E1000_IMC);

            // 4. Configure link (Set Link Up, Auto-Speed, Full Duplex)
            let ctrl = mmio_read(base, E1000_CTRL);
            mmio_write(base, E1000_CTRL, ctrl | (1 << 6) | (1 << 5) | (1 << 0)); // SLU | ASDE | FD

            // Program transmit descriptor base and length
            mmio_write(base, E1000_TDBAL, tx_ring.paddr() as u32);
            mmio_write(base, E1000_TDBAH, (tx_ring.paddr() >> 32) as u32);
            mmio_write(base, E1000_TDLEN, 4096);

            // Initialize head and tail to 0
            mmio_write(base, E1000_TDH, 0);
            mmio_write(base, E1000_TDT, 0);

            // TCTL: EN | PSP | CT=0x10 | COLD=0x40
            mmio_write(
                base,
                E1000_TCTL,
                (1 << 1) | (1 << 3) | (0x10 << 4) | (0x40 << 12),
            );
            // TIPG: IPGT=0xa | IPGR1=0x8 | IPGR2=0xc
            mmio_write(base, E1000_TIPG, 0xa | (0x8 << 10) | (0xc << 20));

            // Write MAC address to RAL / RAH
            let mut ral: u32 = 0;
            let mut rah: u32 = 0;
            for i in 0..4 {
                ral |= (mac.as_bytes()[i] as u32) << (i * 8);
            }
            for i in 0..2 {
                rah |= (mac.as_bytes()[i + 4] as u32) << (i * 8);
            }
            mmio_write(base, E1000_RAL, ral);
            mmio_write(base, E1000_RAH, rah | (1 << 31)); // AV | AS=DA

            // Clear MTA (Multicast Table Array)
            for i in E1000_MTA..E1000_RAL {
                mmio_write(base, i, 0);
            }

            // Program receive descriptor base and length
            mmio_write(base, E1000_RDBAL, rx_ring.paddr() as u32);
            mmio_write(base, E1000_RDBAH, (rx_ring.paddr() >> 32) as u32);
            mmio_write(base, E1000_RDLEN, 4096);

            // Initialize head and tail
            mmio_write(base, E1000_RDH, 0);
            mmio_write(base, E1000_RDT, (NUM_DESC - 1) as u32);

            // RCTL: EN | UPE | MPE | BAM | SECRC | BSIZE=0 (2048)
            // UPE/MPE: DHCP OFFER is often unicast to our MAC, but QEMU/VBox and
            // some bridges rewrite the dest; without promiscuous the frame never
            // leaves the NIC. BAM already accepts broadcast DISCOVER replies.
            mmio_write(
                base,
                E1000_RCTL,
                (1 << 1) | (1 << 3) | (1 << 4) | (1 << 15) | (1 << 26),
            );

            // Clear pending interrupts
            let _icr = mmio_read(base, E1000_ICR);

            // Enable RXT0 and LSC interrupts
            mmio_write(base, E1000_IMS, (1 << 7) | (1 << 2)); // RXT0 | LSC
            let _ = mmio_read(base, E1000_IMS);
        }

        Ok(E1000 {
            base,
            size,
            mac,
            rx_ring,
            rx_bufs,
            rx_next_to_clean: 0,
            tx_ring,
            // (descriptors pre-initialized with DD set above)
            tx_bufs,
            tx_next_to_use: 0,
        })
    }

    pub fn handle_interrupt(&mut self) -> bool {
        unsafe {
            let icr = mmio_read(self.base, E1000_ICR);
            if icr != 0 {
                if (icr & (1 << 2)) != 0 {
                    let status = mmio_read(self.base, E1000_STATUS);
                    let link_up = (status & (1 << 1)) != 0;
                    info!(
                        "[e1000] Link status changed. Link is {}",
                        if link_up { "UP" } else { "DOWN" }
                    );
                }
                true
            } else {
                false
            }
        }
    }

    pub fn receive(&mut self) -> Option<Vec<u8>> {
        let ring = self.rx_ring.as_ptr::<E1000RecvDesc>();
        let desc_addr = unsafe { ring.add(self.rx_next_to_clean) };
        let status = unsafe { core::ptr::read_volatile(&((*desc_addr).status)) };

        if (status & 1) == 0 {
            return None;
        }

        fence(Ordering::Acquire);

        let len = rx_len_clamped(unsafe { core::ptr::read_volatile(&((*desc_addr).len)) });

        let buf_vaddr = self.rx_bufs[self.rx_next_to_clean].vaddr();
        let buffer = unsafe { core::slice::from_raw_parts(buf_vaddr as *const u8, len) };
        let pkt = buffer.to_vec();

        unsafe {
            core::ptr::write_volatile(&mut (*desc_addr).status, 0);
            core::ptr::write_volatile(&mut (*desc_addr).errors, 0);
        }

        fence(Ordering::Release);

        unsafe {
            mmio_write(self.base, E1000_RDT, self.rx_next_to_clean as u32);
        }

        self.rx_next_to_clean = next_desc(self.rx_next_to_clean);

        Some(pkt)
    }

    /// Is the descriptor at `idx` free (hardware wrote DD back)?
    #[inline]
    fn tx_dd_at(&self, idx: usize) -> bool {
        let ring = self.tx_ring.as_ptr::<E1000SendDesc>();
        let desc_addr = unsafe { ring.add(idx) };
        let status = unsafe { core::ptr::read_volatile(&((*desc_addr).status)) };
        (status & 1) != 0
    }

    /// Can a frame be posted at `tx_next_to_use`? The tail slot must be free
    /// AND the slot after it too — the latter is the guard slot the hardware
    /// needs.
    ///
    /// The NIC has no ownership bit of its own: it works descriptors from TDH
    /// up to (but excluding) TDT and reads `TDH == TDT` as an EMPTY ring.
    /// Gating purely per-slot on DD let software post all `NUM_DESC` slots —
    /// every descriptor starts pre-armed DD, so with the link still
    /// negotiating (or the NIC otherwise stalled) 256 frames post in a row,
    /// the tail wraps onto the head, and the `TDT` write hands the NIC a value
    /// equal to TDH. From its side the ring is empty: the posted frames are
    /// never fetched, their DD bits never come back, and every later `send`
    /// finds DD clear at slot 0 and fails forever. TX is dead until reboot,
    /// with no watchdog to notice. Linux's `e1000_desc_unused` bounds
    /// in-flight descriptors to `count - 1` for the same reason, and
    /// `e1000e::tx_can_post` is the same guard.
    pub fn can_send(&self) -> bool {
        tx_ring_has_room(self.tx_next_to_use, |i| self.tx_dd_at(i))
    }

    /// Post a frame to the TX ring. Returns false (dropping the frame) if the
    /// target descriptor is still owned by hardware or the frame is too long
    /// for a TX buffer.
    pub fn send(&mut self, buffer: &[u8]) -> bool {
        // Reject instead of truncating: silently clipping a frame to the
        // buffer size put a corrupt, half-length packet on the wire while
        // `NetScheme::send` still reported the full length as written.
        if !tx_frame_postable(buffer.len()) {
            return false;
        }

        let index = self.tx_next_to_use;
        let ring = self.tx_ring.as_ptr::<E1000SendDesc>();
        let desc_addr = unsafe { ring.add(index) };

        // Re-check hardware ownership (DD bit) under the CURRENT lock, tail
        // slot and guard slot both. The TxToken path checks can_send() under a
        // different lock acquisition than this send, so on SMP another CPU can
        // consume the slot in between; the old debug_assert! was compiled out
        // in release and let that race silently overwrite a descriptor the NIC
        // was still DMA-reading.
        if !self.can_send() {
            return false;
        }

        let len = buffer.len();
        let buf_vaddr = self.tx_bufs[index].vaddr();
        let target = unsafe { core::slice::from_raw_parts_mut(buf_vaddr as *mut u8, len) };
        target.copy_from_slice(buffer);

        unsafe {
            core::ptr::write_volatile(&mut (*desc_addr).len, buffer.len() as u16);
            core::ptr::write_volatile(&mut (*desc_addr).cmd, (1 << 3) | (1 << 1) | (1 << 0)); // RS | IFCS | EOP
            core::ptr::write_volatile(&mut (*desc_addr).status, 0);
            core::ptr::write_volatile(&mut (*desc_addr).cso, 0);
            core::ptr::write_volatile(&mut (*desc_addr).css, 0);
            core::ptr::write_volatile(&mut (*desc_addr).special, 0);
        }

        fence(Ordering::SeqCst);

        self.tx_next_to_use = next_desc(self.tx_next_to_use);

        unsafe {
            mmio_write(self.base, E1000_TDT, self.tx_next_to_use as u32);
        }

        fence(Ordering::SeqCst);
        true
    }
}

#[derive(Clone)]
pub struct E1000Driver {
    pub hw: Arc<Mutex<E1000>>,
    pub stats: Arc<Mutex<NetStats>>,
}

#[derive(Clone)]
pub struct E1000Interface {
    iface: Arc<Mutex<Interface<'static, E1000Driver>>>,
    driver: E1000Driver,
    name: String,
    irq: usize,
    base: usize,
    poll_pending: Arc<core::sync::atomic::AtomicBool>,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000Interface {
    fn ims_rearm(&self) {
        unsafe {
            mmio_write(self.base, E1000_IMS, (1 << 7) | (1 << 2)); // RXT0 | LSC
            let _ = mmio_read(self.base, E1000_IMS);
        }
    }

    /// Poll the interface WITHOUT re-arming IMS. The IRQ deferred job re-arms
    /// only AFTER clearing `poll_pending` (see `handle_irq`); re-arming inside
    /// the poll would unmask IMS while `poll_pending` is still true, so a packet
    /// arriving in that window fires an IRQ that hits the `else { ims_rearm() }`
    /// branch — which read-clears ICR without queuing a poll, dropping the RX
    /// frame until the next interrupt. The periodic `poll()` path re-arms itself.
    fn poll_inner(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        // Mutex::lock() uses push_off/pop_off which already disables interrupts
        // for the duration of the critical section. Manual intr_off/on bypasses
        // the noff accounting and panics ("RefCell already borrowed") under SMP.
        let sockets = get_sockets();
        // `try_lock`, NOT a blocking `lock()`. This poll runs from a deferred
        // job, and a socket syscall drains that queue WHILE it holds the
        // socket set (a read/write pumping the NIC to make progress). On this
        // IRQ-off kernel the blocking acquire then spins forever on a lock the
        // same CPU already owns: reproduced in QEMU as
        //   [DEADLOCK: spinlock(s) stuck >8s] cpu=0 at drivers/src/net/e1000.rs
        //   HOLDER cpu=0 at linux-object/src/net/tcp.rs
        // during a 64 MB HTTP download. If either lock is held, skip the
        // smoltcp poll: the frame that owns the socket set drives it on
        // release, so nothing is lost. `e1000e::poll_with_irq_hint` documents
        // and fixes exactly this; e1000 never got the same treatment.
        let res = {
            match (sockets.try_lock(), self.iface.try_lock()) {
                (Some(mut sockets), Some(mut iface)) => match iface.poll(&mut sockets, timestamp) {
                    Ok(p) => {
                        trace!("e1000 NetScheme poll: {:?}", p);
                        Ok(())
                    }
                    Err(err) => {
                        warn!("poll got err {}", err);
                        Err(DeviceError::IoError)
                    }
                },
                _ => Ok(()),
            }
        };
        super::net_flush_deferred_packets();
        res
    }
}

impl Scheme for E1000Interface {
    fn name(&self) -> &str {
        "e1000"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let icr = unsafe { mmio_read(self.base, E1000_ICR) };
        if icr == 0 {
            if !self.poll_pending.load(core::sync::atomic::Ordering::SeqCst) {
                self.ims_rearm();
            }
            return;
        }

        if !self.poll_pending.load(core::sync::atomic::Ordering::SeqCst) {
            self.poll_pending
                .store(true, core::sync::atomic::Ordering::SeqCst);
            unsafe {
                mmio_write(self.base, E1000_IMC, 0xffffffff);
                let _ = mmio_read(self.base, E1000_IMC);
            }
            // The guard is built OUTSIDE the closure and moved in, so
            // `poll_pending` is cleared and IMS re-armed whether the job runs
            // or is dropped unexecuted: `deferred_job` caps its queue at 256
            // and evicts entries under pressure. Clearing it in the closure
            // BODY meant an evicted bottom-half left the NIC masked and
            // `poll_pending` stuck true forever — every later IRQ then took
            // the `else` branch below, read-clearing ICR without ever queuing
            // a poll, and the interface was demoted to the periodic
            // `poll_ifaces` path for the life of the kernel. This is the same
            // `PollPendingGuard` shape `e1000e.rs` uses.
            struct PollPendingGuard {
                pending: Arc<AtomicBool>,
                iface: E1000Interface,
            }
            impl Drop for PollPendingGuard {
                fn drop(&mut self) {
                    // Clear BEFORE re-arming, so an IRQ firing after the
                    // unmask finds poll_pending=false and queues a fresh poll
                    // instead of dropping the RX cause.
                    self.pending
                        .store(false, core::sync::atomic::Ordering::SeqCst);
                    self.iface.ims_rearm();
                }
            }
            let guard = PollPendingGuard {
                pending: self.poll_pending.clone(),
                iface: self.clone(),
            };
            let self_clone = self.clone();
            // Front of the queue: the NIC bottom-half must run before a
            // backlog of lower-urgency jobs can age it out of the FIFO.
            crate::utils::deferred_job::push_deferred_job_front(move || {
                let guard = guard;
                // Drain WITHOUT re-arming; the guard re-arms on drop.
                let _ = self_clone.poll_inner();
                drop(guard);
            });
        } else {
            // Bottom-half already pending and IMC held. ICR was cleared by the
            // read above; stashing is not available on this older path, but
            // rearming IMS would let further edges clear causes the BH never
            // sees. Leave the NIC masked until the queued poll's Drop rearms.
        }
    }
}

impl NetScheme for E1000Interface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    // get ip addresses
    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: smoltcp::wire::EthernetAddress,
    ) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        self.iface
            .lock()
            .seed_neighbor(protocol, hardware, timestamp);
        Ok(())
    }

    fn poll(&self) -> DeviceResult {
        // Periodic (non-IRQ) poll path: drain, then re-arm IMS. The IRQ path
        // calls poll_inner() directly and re-arms only after clearing
        // poll_pending (see handle_irq).
        let res = self.poll_inner();
        self.ims_rearm();
        res
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        // Try to read directly from hardware.
        if let Some(pkt) = self.driver.hw.lock().receive() {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            Ok(n)
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        // A frame longer than a TX buffer is rejected outright rather than
        // clipped, so the caller is never told that a corrupt, truncated
        // packet went out in full.
        if data.is_empty() || data.len() > RX_BUF_SIZE {
            return Err(DeviceError::InvalidParam);
        }
        // send() re-checks descriptor ownership under this same lock, so a
        // full/in-flight ring returns NotReady instead of corrupting a slot.
        let mut driver = self.driver.hw.lock();
        if driver.send(data) {
            Ok(data.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        // Return true so callers always attempt recv(); actual receive will return NotReady if nothing.
        true
    }

    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            let mut set_primary = false;
            for slot in addrs.iter_mut() {
                if let IpCidr::Ipv4(_) = slot {
                    if !set_primary {
                        *slot = IpCidr::Ipv4(cidr);
                        set_primary = true;
                    } else {
                        *slot = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0));
                    }
                }
            }
            if !set_primary {
                if let Some(slot) = addrs.iter_mut().next() {
                    *slot = IpCidr::Ipv4(cidr);
                }
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            if addrs.contains(&cidr) {
                return;
            }
            for slot in addrs.iter_mut() {
                if (slot.address().is_unspecified() && slot.prefix_len() == 0)
                    || (slot.address() == IpAddress::v4(240, 0, 0, 0) && slot.prefix_len() == 32)
                {
                    *slot = cidr;
                    return;
                }
            }
            // Every slot is taken. Growing the list is the only way to keep
            // the addresses that are already there; overwriting the newest one
            // silently takes an address the interface still answers on.
            if let ManagedSlice::Owned(addrs) = addrs {
                addrs.push(cidr);
            } else if let Some(slot) = addrs.iter_mut().last() {
                // Borrowed storage cannot grow, so the newest slot is the best
                // there is.
                *slot = cidr;
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            for slot in addrs.iter_mut() {
                if *slot == cidr {
                    *slot = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
                    return;
                }
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        match gateway {
            Some(IpAddress::Ipv4(gw)) => {
                let mut routes = self.routes.lock();
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                    // Only one default route can be in force, so a new one
                    // replaces the one already there. A route to a named
                    // network replaces nothing -- and this purge used to run
                    // for every gatewayed route, so the static route in a DHCP
                    // lease took 0.0.0.0/0 out of the table the kernel reports
                    // while smoltcp went on routing through it.
                    routes
                        .retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0));
                }
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv4(gw)),
                });
            }
            Some(IpAddress::Ipv6(gw)) => {
                let mut routes = self.routes.lock();
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                    // Only one default route can be in force, so a new one
                    // replaces the one already there. A route to a named
                    // network replaces nothing -- and this purge used to run
                    // for every gatewayed route, so the static route in a DHCP
                    // lease took ::/0 out of the table the kernel reports
                    // while smoltcp went on routing through it.
                    routes
                        .retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0));
                }
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv6(gw)),
                });
            }
            None => {
                self.routes.lock().push(RouteInfo { dst: cidr, gateway });
            }
            _ => {}
        }
        Ok(())
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        if cidr.prefix_len() == 0 {
            match cidr {
                IpCidr::Ipv4(_) => {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                }
                IpCidr::Ipv6(_) => {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                }
                _ => {}
            }
        }
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        let mut res = Vec::new();

        // 1. Add tracked routes
        res.extend(self.routes.lock().clone());

        // 2. Add direct routes
        for cidr in iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(v4) if v4.prefix_len() > 0 => {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv4(v4.network()),
                        gateway: None,
                    });
                }
                IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv6(v6.network()),
                        gateway: None,
                    });
                }
                _ => {}
            }
        }
        res
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
    }
    fn get_mtu(&self) -> usize {
        1500
    }
}

pub struct E1000RxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct E1000TxToken {
    driver: E1000Driver,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::Device<'_> for E1000Driver {
    type RxToken = E1000RxToken;
    type TxToken = E1000TxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.hw.lock().receive().map(|pkt| {
            (
                E1000RxToken {
                    data: pkt,
                    stats: self.stats.clone(),
                },
                E1000TxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(E1000TxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        // Do NOT set max_burst_size. smoltcp clamps the TCP window to
        // `burst * MSS` and stores it in a u16: burst=64 → 64*1474=94336,
        // which wraps to 28800 and caps throughput at a few Mbps.
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for E1000RxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut stats = self.stats.lock();
        stats.rx_packets += 1;
        stats.rx_bytes += self.data.len() as u64;
        drop(stats);

        // Dispatch to global packet tapping (AF_PACKET sockets) after smoltcp unlocks SOCKETS.
        super::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for E1000TxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        // Do NOT index a fixed-size stack array with `len`: it is whatever the
        // IP layer computed for this datagram, and this smoltcp has neither IP
        // fragmentation nor an egress MTU clamp (`max_transmission_unit` only
        // feeds the TCP MSS). A single `sendto()` of a 1495-byte UDP payload
        // — never mind a 64 KiB one, or a raw socket — makes `len` exceed the
        // buffer and `&mut buffer[..len]` panics inside the kernel.
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap_buf = Vec::new();
        let buf: &mut [u8] = tx_stage(len, &mut scratch, &mut heap_buf);
        let result = f(buf)?;

        let mut driver = self.driver.hw.lock();
        let sent = driver.send(buf);
        drop(driver);

        if sent {
            let mut stats = self.stats.lock();
            stats.tx_packets += 1;
            stats.tx_bytes += len as u64;
        } else {
            // NEVER report a dropped frame as sent. smoltcp's `socket_ingress`
            // emits the ACK / window-update for a received segment through the
            // TxToken paired with that RxToken, and it has ALREADY advanced
            // `remote_last_ack` / `remote_last_win` by the time we are called.
            // Returning `Ok` there loses the ACK for good: the peer waits on a
            // window it believes is closed and the transfer stalls mid-stream.
            // `Err(Exhausted)` keeps smoltcp's state consistent so the segment
            // is re-emitted. (`e1000e::E1000eTxToken::consume` documents the
            // same failure; it additionally spins briefly for a free slot.)
            return Err(smoltcp::Error::Exhausted);
        }

        Ok(result)
    }
}

pub fn init(
    name: String,
    irq: usize,
    header: usize,
    size: usize,
    index: usize,
) -> DeviceResult<E1000Interface> {
    info!("Probing e1000 {}", name);

    let mac: [u8; 6] = [0x54, 0x51, 0x9F, 0x71, 0xC0, index as u8];
    let ethernet_addr = EthernetAddress::from_bytes(&mac);
    let e1000 = E1000::new(header, size, ethernet_addr)?;
    let hw = Arc::new(Mutex::new(e1000));
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let net_driver = E1000Driver {
        hw: hw.clone(),
        stats: stats.clone(),
    };

    let mut eui64 = [0u8; 8];
    eui64[0] = mac[0] ^ 2;
    eui64[1] = mac[1];
    eui64[2] = mac[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = mac[3];
    eui64[6] = mac[4];
    eui64[7] = mac[5];
    let link_local = Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        (eui64[0] as u16) << 8 | eui64[1] as u16,
        (eui64[2] as u16) << 8 | eui64[3] as u16,
        (eui64[4] as u16) << 8 | eui64[5] as u16,
        (eui64[6] as u16) << 8 | eui64[7] as u16,
    );

    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    // Per-NIC storage (same pattern as e1000e): a shared `static mut` would
    // alias if two e1000 probes ran, corrupting both route tables.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let routes = Routes::new(routes_storage);
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    let iface = InterfaceBuilder::new(net_driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs.clone())
        .routes(routes)
        .finalize();

    crate::klog_info!("e1000 interface {} discovered", name);
    let e1000_iface = E1000Interface {
        iface: Arc::new(Mutex::new(iface)),
        driver: net_driver,
        name,
        irq,
        base: header,
        poll_pending: Arc::new(core::sync::atomic::AtomicBool::new(false)),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    };

    Ok(e1000_iface)
}

pub struct E1000DriverPci;

impl PciDriver for E1000DriverPci {
    fn name(&self) -> &str {
        "e1000"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        vendor_id == 0x8086 && (device_id == 0x100e || device_id == 0x100f)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[0] {
            // Through the base the mapper returns: see `bus::resolve_window`.
            let vaddr = crate::bus::resolve_window(mapper, addr as usize, 4096 * 8, 0);
            let name = crate::net::next_eth_ifname();
            let vector = irq.map(|idx| idx + 32).unwrap_or(0);
            let iface = init(name, vector, vaddr, len as usize, 0)?;
            Ok(Device::Net(Arc::new(iface)))
        } else {
            Err(crate::DeviceError::NotSupported)
        }
    }
}

#[cfg(test)]
mod ring_tests {
    //! The e1000 is QEMU's default NIC, so this driver is the one every guest
    //! boots with -- and until recently it had no tests at all. Three of the
    //! bugs fixed in it were the kind that leave no error behind: a frame
    //! clipped to the buffer size and reported as fully sent, a TX ring that
    //! wrapped its tail onto its head and died silently until reboot, and a
    //! `len` from the IP layer used to index a fixed stack array, which
    //! panics the kernel from one `sendto`. These are the tests those fixes
    //! never got.

    use super::*;

    #[test]
    fn the_ring_index_wraps_at_the_end_and_nowhere_else() {
        assert_eq!(next_desc(0), 1);
        assert_eq!(next_desc(NUM_DESC - 2), NUM_DESC - 1);
        assert_eq!(
            next_desc(NUM_DESC - 1),
            0,
            "the last slot wraps to the first"
        );
    }

    #[test]
    fn a_device_length_past_the_buffer_is_clamped() {
        // `len` comes from the device. A slice built over more than the
        // buffer holds reads memory the descriptor never owned, and the
        // contents go straight up the network stack.
        assert_eq!(rx_len_clamped(0), 0);
        assert_eq!(rx_len_clamped(60), 60);
        assert_eq!(rx_len_clamped(RX_BUF_SIZE as u16), RX_BUF_SIZE);
        assert_eq!(rx_len_clamped(u16::MAX), RX_BUF_SIZE);
        // The boundary itself: one past the buffer must not survive.
        assert_eq!(rx_len_clamped(RX_BUF_SIZE as u16 + 1), RX_BUF_SIZE);
    }

    #[test]
    fn an_empty_or_oversized_frame_is_refused_rather_than_clipped() {
        // Clipping put a corrupt half-frame on the wire while the caller was
        // told the whole thing went out, so the loss surfaced on the peer.
        assert!(!tx_frame_postable(0), "a zero-length frame is not a frame");
        assert!(tx_frame_postable(1));
        assert!(tx_frame_postable(60));
        assert!(tx_frame_postable(1514), "the advertised MTU must fit");
        assert!(tx_frame_postable(RX_BUF_SIZE), "a full buffer still fits");
        assert!(
            !tx_frame_postable(RX_BUF_SIZE + 1),
            "one byte past the DMA buffer must be refused, not truncated"
        );
        assert!(!tx_frame_postable(65536));
        assert!(!tx_frame_postable(usize::MAX));
    }

    #[test]
    fn the_tail_slot_alone_is_not_enough_to_post() {
        // The guard slot is the whole point: the NIC reads `TDH == TDT` as an
        // EMPTY ring, so filling the last free descriptor hands it a tail
        // equal to its head and it fetches nothing at all.
        let free_tail_only = |i: usize| i == 10;
        assert!(
            !tx_ring_has_room(10, free_tail_only),
            "a free tail with a busy guard slot must NOT be postable"
        );
        let free_both = |i: usize| i == 10 || i == 11;
        assert!(tx_ring_has_room(10, free_both));
    }

    #[test]
    fn the_guard_slot_is_looked_up_with_the_ring_wrapped() {
        // At the end of the ring the guard slot is index 0, not index 256 --
        // which would be off the end of the descriptor array.
        let last = NUM_DESC - 1;
        let free_last_and_first = |i: usize| i == last || i == 0;
        assert!(tx_ring_has_room(last, free_last_and_first));
        let free_last_only = |i: usize| i == last;
        assert!(
            !tx_ring_has_room(last, free_last_only),
            "wrapping must consult slot 0, and a busy slot 0 must block"
        );
        // And the index it asks about never leaves the ring.
        for start in [0usize, 1, NUM_DESC / 2, NUM_DESC - 2, NUM_DESC - 1] {
            tx_ring_has_room(start, |i| {
                assert!(i < NUM_DESC, "asked about descriptor {} of {}", i, NUM_DESC);
                true
            });
        }
    }

    #[test]
    fn a_ring_that_is_entirely_busy_never_posts() {
        assert!(!tx_ring_has_room(0, |_| false));
        assert!(!tx_ring_has_room(NUM_DESC - 1, |_| false));
    }

    #[test]
    fn a_frame_that_fits_the_scratch_uses_it_and_is_exactly_its_length() {
        let mut scratch = [0xAAu8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        for len in [0usize, 1, 60, 1514, TX_SCRATCH_LEN] {
            let buf = tx_stage(len, &mut scratch, &mut heap);
            assert_eq!(buf.len(), len, "staged buffer must be exactly `len`");
            assert!(heap.is_empty(), "len {} should not have hit the heap", len);
        }
    }

    #[test]
    fn a_frame_longer_than_the_scratch_goes_to_the_heap_instead_of_panicking() {
        // This is the one that panicked the kernel. `len` is whatever the IP
        // layer computed, and a single `sendto()` of a 1495-byte UDP payload
        // takes it past the 1536-byte scratch: 1495 + 20 (IP) + 8 (UDP) + 14
        // (Ethernet) = 1537.
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        let buf = tx_stage(1537, &mut scratch, &mut heap);
        assert_eq!(buf.len(), 1537);

        // And the extreme a raw socket can ask for.
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        let buf = tx_stage(65535, &mut scratch, &mut heap);
        assert_eq!(buf.len(), 65535);
    }

    #[test]
    fn the_staged_buffer_is_zeroed_whichever_side_of_the_boundary_it_is_on() {
        // smoltcp fills only the headers it knows about; whatever is left is
        // what goes on the wire. A dirty scratch leaks the previous frame.
        let mut scratch = [0xFFu8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        let buf = tx_stage(64, &mut scratch, &mut heap);
        buf.fill(0);
        assert!(buf.iter().all(|&b| b == 0));

        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = alloc::vec![0xFFu8; 8];
        let buf = tx_stage(4000, &mut scratch, &mut heap);
        assert_eq!(buf.len(), 4000);
        assert!(
            buf[8..].iter().all(|&b| b == 0),
            "the grown tail of the heap buffer must be zero, not stale bytes"
        );
    }

    #[test]
    fn the_scratch_boundary_is_exact() {
        // One byte either side of `TX_SCRATCH_LEN`: `<=` uses the scratch, and
        // `>` must not, because `&mut scratch[..len]` past its length is the
        // panic.
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        assert_eq!(
            tx_stage(TX_SCRATCH_LEN, &mut scratch, &mut heap).len(),
            TX_SCRATCH_LEN
        );
        assert!(heap.is_empty());

        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        assert_eq!(
            tx_stage(TX_SCRATCH_LEN + 1, &mut scratch, &mut heap).len(),
            TX_SCRATCH_LEN + 1
        );
        assert_eq!(
            heap.len(),
            TX_SCRATCH_LEN + 1,
            "it must have gone to the heap"
        );
    }

    #[test]
    fn the_scratch_covers_the_advertised_mtu() {
        // The reason the scratch exists at all: the common case must not
        // allocate. 1514 is the MTU this driver advertises to smoltcp.
        // (That the scratch covers the advertised MTU is asserted at compile
        // time next to the constant itself -- a build error beats a test
        // failure for something neither value can change at run time.)
        //
        // A postable frame is allowed to be longer than the scratch, so the
        // two limits are not the same number and must not be conflated.
        assert!(tx_frame_postable(TX_SCRATCH_LEN + 1));
    }
}

#[cfg(test)]
mod e1000_desc_layout_tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    /// Both descriptor rings are read and written by the card itself over
    /// DMA, so these sixteen bytes are the hardware ABI and not ours to
    /// choose. Intel's own layout, as Linux spells it in
    /// `drivers/net/ethernet/intel/e1000/e1000_hw.h`, is
    ///
    /// ```text
    /// struct e1000_tx_desc {           struct e1000_rx_desc {
    ///     __le64 buffer_addr;              __le64 buffer_addr;
    ///     union { __le32 data;             __le16 length;
    ///         struct {                     __le16 csum;
    ///             __le16 length;           u8 status;
    ///             u8 cso;                  u8 errors;
    ///             u8 cmd;                  __le16 special;
    ///         } flags;                 };
    ///     } lower;
    ///     union { __le32 data;
    ///         struct {
    ///             u8 status;
    ///             u8 css;
    ///             __le16 special;
    ///         } fields;
    ///     } upper;
    /// };
    /// ```
    ///
    /// Nothing pinned it, and no behaviour test can: every read and write
    /// here goes through a field name, so swapping two adjacent single-byte
    /// fields compiles, leaves the size at sixteen, and passes the whole
    /// suite while the card reads the checksum offset as the descriptor
    /// command. Five of five such swaps survived the crate's 1513 tests.
    #[test]
    fn a_transmit_descriptor_is_the_sixteen_bytes_the_card_reads() {
        assert_eq!(size_of::<E1000SendDesc>(), 16);
        assert_eq!(align_of::<E1000SendDesc>(), 8);
        assert_eq!(offset_of!(E1000SendDesc, addr), 0);
        assert_eq!(offset_of!(E1000SendDesc, len), 8);
        assert_eq!(offset_of!(E1000SendDesc, cso), 10);
        assert_eq!(offset_of!(E1000SendDesc, cmd), 11);
        assert_eq!(offset_of!(E1000SendDesc, status), 12);
        assert_eq!(offset_of!(E1000SendDesc, css), 13);
        assert_eq!(offset_of!(E1000SendDesc, special), 14);
    }

    /// `length` at offset eight is what the driver trusts to know how much of
    /// the buffer the card filled, and `status` at twelve carries the
    /// descriptor-done bit it polls. The checksum sits between them and the
    /// error bits right after, both adjacent to their neighbour and the same
    /// width as it.
    #[test]
    fn a_receive_descriptor_is_the_sixteen_bytes_the_card_writes() {
        assert_eq!(size_of::<E1000RecvDesc>(), 16);
        assert_eq!(align_of::<E1000RecvDesc>(), 8);
        assert_eq!(offset_of!(E1000RecvDesc, addr), 0);
        assert_eq!(offset_of!(E1000RecvDesc, len), 8);
        assert_eq!(offset_of!(E1000RecvDesc, chksum), 10);
        assert_eq!(offset_of!(E1000RecvDesc, status), 12);
        assert_eq!(offset_of!(E1000RecvDesc, errors), 13);
        assert_eq!(offset_of!(E1000RecvDesc, special), 14);
    }
}

/// The driver over a register window in ordinary memory.
///
/// `ring_tests` and `e1000_desc_layout_tests` above cover the pure helpers --
/// `next_desc`, `rx_len_clamped`, `tx_frame_postable`, `tx_ring_has_room`,
/// `tx_stage` -- and the descriptor layout, and they are good tests. What
/// nothing covered is **the bodies that use them**: `E1000::new`, `receive`,
/// `send`, `can_send` and `handle_interrupt` were at zero lines, 568 of the
/// file's 590 lines of production. So the ring guard that keeps TX from going
/// dead until reboot was decided by a tested helper and **asked for by
/// nothing**, and the bring-up sequence -- the MAC split across two registers,
/// the ring addresses split into halves, the words that enable transmit and
/// receive -- had no test at all.
///
/// The seam is that the driver reaches its registers through `mmio_read` and
/// `mmio_write`, which are `read_volatile`/`write_volatile` at
/// `base + reg * 4` and nothing else: **a leaked window of zeroes IS the
/// card** as far as they can tell, the same bench the UART and the PL011 use.
/// The rings are real `DmaRegion`s, which the host shim serves.
///
/// What this bench cannot show: a register the driver only READS (the window
/// does not count reads, and a real card self-clears its reset bit where the
/// window keeps it), and the interior of the link-status branch, which only
/// logs.
#[cfg(test)]
mod card_tests {
    use super::*;
    use crate::utils::host_hooks::alone_with_the_allocator;

    /// One register past the highest index the driver touches.
    const REGISTERS: usize = E1000_RAH + 1;

    // The bits, by the name the 82540 manual gives them.
    const RST: u32 = 1 << 26;
    const SLU: u32 = 1 << 6;
    const ASDE: u32 = 1 << 5;
    const FD: u32 = 1 << 0;
    const AV: u32 = 1 << 31;
    const RXT0: u32 = 1 << 7;
    const LSC: u32 = 1 << 2;
    /// Descriptor Done, the ownership bit both rings are gated on.
    const DD: u8 = 1;
    /// A MAC with all six bytes different, so an endianness mistake shows.
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    /// The address of a leaked register window, which is all `mmio_read` and
    /// `mmio_write` ever ask of a card. Only the address comes back, so
    /// nothing here holds a reference to it.
    fn a_window() -> usize {
        let window: &'static mut [u32] = alloc::vec![0u32; REGISTERS].leak();
        window.as_mut_ptr() as usize
    }

    fn reg(base: usize, at: usize) -> u32 {
        // SAFETY: `base` came from `a_window`, which leaked REGISTERS words.
        unsafe { core::ptr::read_volatile((base + at * 4) as *const u32) }
    }

    fn set_reg(base: usize, at: usize, value: u32) {
        // SAFETY: as `reg`.
        unsafe { core::ptr::write_volatile((base + at * 4) as *mut u32, value) }
    }

    /// The virtual clock has to move on its own. `E1000::new` waits ten
    /// milliseconds for the reset to settle, and with the clock standing at
    /// zero that wait runs to its ten-million-spin cap every time instead.
    fn a_moving_clock() {
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
    }

    fn a_card(base: usize) -> E1000 {
        a_moving_clock();
        E1000::new(base, REGISTERS * 4, EthernetAddress(MAC)).expect("the shim serves the rings")
    }

    fn rx_desc(nic: &E1000, at: usize) -> E1000RecvDesc {
        // SAFETY: the ring holds NUM_DESC descriptors and `at` is below it.
        unsafe { core::ptr::read_volatile(nic.rx_ring.as_ptr::<E1000RecvDesc>().add(at)) }
    }

    fn tx_desc(nic: &E1000, at: usize) -> E1000SendDesc {
        // SAFETY: as `rx_desc`.
        unsafe { core::ptr::read_volatile(nic.tx_ring.as_ptr::<E1000SendDesc>().add(at)) }
    }

    /// What the card does when it has filled a receive descriptor: the bytes
    /// in the buffer, the length and the done bit in the descriptor.
    fn the_card_delivers(nic: &E1000, at: usize, frame: &[u8], reports: u16) {
        let into = nic.rx_bufs[at].vaddr() as *mut u8;
        // SAFETY: the buffer is RX_BUF_SIZE bytes and `frame` is shorter.
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), into, frame.len()) };
        let mut desc = rx_desc(nic, at);
        desc.len = reports;
        desc.status = DD;
        // SAFETY: as `rx_desc`.
        unsafe { core::ptr::write_volatile(nic.rx_ring.as_ptr::<E1000RecvDesc>().add(at), desc) };
    }

    /// Hand a transmit descriptor back to the card, or take it back.
    fn the_card_owns_transmit_slot(nic: &E1000, at: usize, owns: bool) {
        let mut desc = tx_desc(nic, at);
        desc.status = if owns { 0 } else { DD };
        // SAFETY: as `tx_desc`.
        unsafe { core::ptr::write_volatile(nic.tx_ring.as_ptr::<E1000SendDesc>().add(at), desc) };
    }

    // --- bringing the card up ---------------------------------------------

    /// The MAC goes out as two words, low four bytes then high two, each
    /// little-endian. Get the order wrong and the card filters on an address
    /// that is not ours: every unicast frame is dropped by the hardware, and
    /// the only symptom is a link that is up and a stack that hears nothing.
    #[test]
    fn the_mac_goes_out_little_endian_in_two_registers_with_the_valid_bit() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            assert_eq!(
                reg(base, E1000_RAL),
                0x1200_5452,
                "RAL must be bytes 0..4 of the MAC, least significant first"
            );
            assert_eq!(
                reg(base, E1000_RAH) & !AV,
                0x0000_5634,
                "RAH must be bytes 4..6 of the MAC, least significant first"
            );
            assert_ne!(
                reg(base, E1000_RAH) & AV,
                0,
                "without AV the card ignores the address it was just given"
            );
        });
    }

    /// The reset is asked for, and the link bits are set AFTER it: a card still
    /// in reset forgets whatever it was told. The window does not self-clear
    /// RST the way a card does, so finding all four bits set is what says the
    /// link word was written on top of the reset word and not before it.
    #[test]
    fn the_reset_is_asked_for_and_the_link_is_brought_up_on_top_of_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            let ctrl = reg(base, E1000_CTRL);
            assert_ne!(ctrl & RST, 0, "the card was never reset");
            assert_ne!(
                ctrl & SLU,
                0,
                "Set Link Up is clear, so the link stays down"
            );
            assert_ne!(ctrl & ASDE, 0, "Auto-Speed Detection is clear");
            assert_ne!(ctrl & FD, 0, "Full Duplex is clear");
        });
    }

    /// Interrupts are masked before anything else is touched, and the two the
    /// driver actually handles are armed at the end. Arming more than those
    /// two wakes the handler for causes it does not clear, which on a shared
    /// line is an interrupt storm.
    #[test]
    fn interrupts_are_masked_first_and_only_the_two_wanted_are_armed() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            assert_eq!(
                reg(base, E1000_IMC),
                0xffff_ffff,
                "the mask-clear register must have been written with every cause"
            );
            assert_eq!(
                reg(base, E1000_IMS),
                RXT0 | LSC,
                "only receive-timer and link-status-change may be armed"
            );
        });
    }

    /// Each ring is handed over as a physical address split into halves, plus
    /// its length in BYTES. A high half dropped works on any machine whose DMA
    /// memory happens to sit below 4 GiB and fails on the one that does not.
    #[test]
    fn each_ring_is_handed_over_as_a_split_physical_address_and_a_byte_length() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let nic = a_card(base);
            let tx = nic.tx_ring.paddr() as u64;
            let rx = nic.rx_ring.paddr() as u64;
            assert_eq!(reg(base, E1000_TDBAL), tx as u32, "TDBAL");
            assert_eq!(reg(base, E1000_TDBAH), (tx >> 32) as u32, "TDBAH");
            assert_eq!(reg(base, E1000_RDBAL), rx as u32, "RDBAL");
            assert_eq!(reg(base, E1000_RDBAH), (rx >> 32) as u32, "RDBAH");
            assert_eq!(reg(base, E1000_TDLEN), 4096, "TDLEN is a byte count");
            assert_eq!(reg(base, E1000_RDLEN), 4096, "RDLEN is a byte count");
            assert_ne!(tx, rx, "the two rings cannot be the same memory");
        });
    }

    /// The two rings start in OPPOSITE states, and both are right. Transmit is
    /// empty, so head and tail agree. Receive is full: the tail is the last
    /// descriptor the card may fill, so `NUM_DESC - 1` hands it every
    /// descriptor but one -- a tail equal to the head would mean an empty ring
    /// and the card would never receive anything.
    #[test]
    fn the_transmit_ring_starts_empty_and_the_receive_ring_starts_handed_over() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            assert_eq!(reg(base, E1000_TDH), 0, "TDH");
            assert_eq!(reg(base, E1000_TDT), 0, "TDT: an empty transmit ring");
            assert_eq!(reg(base, E1000_RDH), 0, "RDH");
            assert_eq!(
                reg(base, E1000_RDT),
                (NUM_DESC - 1) as u32,
                "RDT: the card must own every receive descriptor but one"
            );
        });
    }

    /// The transmit control word, field by field. `EN` off sends nothing;
    /// `PSP` off puts a runt on the wire that the peer discards; and the two
    /// timings are what a half-duplex segment needs to back off correctly.
    #[test]
    fn the_transmit_control_word_enables_transmit_and_pads_short_frames() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            let tctl = reg(base, E1000_TCTL);
            assert_ne!(tctl & (1 << 1), 0, "TCTL: EN is clear, nothing is sent");
            assert_ne!(tctl & (1 << 3), 0, "TCTL: PSP is clear, runts go out short");
            assert_eq!((tctl >> 4) & 0x3FF, 0x10, "TCTL: CT is not the 802.3 16");
            assert_eq!(
                (tctl >> 12) & 0x3FF,
                0x40,
                "TCTL: COLD is not 64 byte times"
            );
            let tipg = reg(base, E1000_TIPG);
            assert_eq!(tipg & 0x3FF, 0xa, "TIPG: IPGT");
            assert_eq!((tipg >> 10) & 0x3FF, 0x8, "TIPG: IPGR1");
            assert_eq!((tipg >> 20) & 0x3FF, 0xc, "TIPG: IPGR2");
        });
    }

    /// The receive control word, the same way. The two promiscuous bits are
    /// deliberate and the comment in `new` says why: a DHCP OFFER unicast to
    /// our MAC gets its destination rewritten by some bridges, and without
    /// them the frame never leaves the card. `SECRC` strips the CRC, which the
    /// stack above does not expect to see.
    #[test]
    fn the_receive_control_word_enables_receive_and_keeps_the_promiscuous_bits() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let _nic = a_card(base);
            let rctl = reg(base, E1000_RCTL);
            assert_ne!(rctl & (1 << 1), 0, "RCTL: EN is clear, nothing is received");
            assert_ne!(rctl & (1 << 3), 0, "RCTL: UPE, unicast promiscuous");
            assert_ne!(rctl & (1 << 4), 0, "RCTL: MPE, multicast promiscuous");
            assert_ne!(rctl & (1 << 15), 0, "RCTL: BAM, accept broadcast");
            assert_ne!(rctl & (1 << 26), 0, "RCTL: SECRC, strip the ethernet CRC");
            assert_eq!(
                (rctl >> 16) & 0x3,
                0,
                "RCTL: BSIZE must stay 0, which with BSEX clear is 2048 bytes"
            );
        });
    }

    /// The multicast table is cleared, and the loop that clears it stops
    /// BEFORE the address registers -- which were written just above it. An
    /// inclusive end zeroes the MAC the driver had already programmed, and the
    /// card then filters on nothing.
    #[test]
    fn the_multicast_table_is_cleared_without_running_into_the_address_registers() {
        alone_with_the_allocator(|| {
            let base = a_window();
            for at in E1000_MTA..REGISTERS {
                set_reg(base, at, 0xffff_ffff);
            }
            let _nic = a_card(base);
            for at in E1000_MTA..E1000_RAL {
                assert_eq!(
                    reg(base, at),
                    0,
                    "the multicast entry at {} was left set",
                    at
                );
            }
            assert_eq!(
                reg(base, E1000_RAL),
                0x1200_5452,
                "the clearing loop ran over the address register"
            );
        });
    }

    /// Every receive descriptor points at ITS OWN buffer and starts not-done.
    /// One index off and two descriptors share a buffer: the second frame
    /// overwrites the first while the stack is still reading it.
    #[test]
    fn every_receive_descriptor_points_at_its_own_buffer() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let nic = a_card(base);
            for at in 0..NUM_DESC {
                let desc = rx_desc(&nic, at);
                assert_eq!(
                    desc.addr,
                    nic.rx_bufs[at].paddr() as u64,
                    "receive descriptor {} does not point at its own buffer",
                    at
                );
                assert_eq!(desc.status, 0, "descriptor {} starts done", at);
                assert_eq!(desc.len, 0, "descriptor {} starts with a length", at);
            }
        });
    }

    /// Every transmit descriptor starts marked DONE, which is what lets the
    /// very first frame be posted against the real ownership bit. Starting
    /// them clear forced a `first_trans` flag that skipped the ownership check
    /// for the whole first lap of the ring, and that check is the only thing
    /// standing between a frame and a descriptor the card is still reading.
    #[test]
    fn every_transmit_descriptor_starts_marked_done_so_the_first_frame_can_go() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let nic = a_card(base);
            for at in 0..NUM_DESC {
                let desc = tx_desc(&nic, at);
                assert_eq!(
                    desc.addr,
                    nic.tx_bufs[at].paddr() as u64,
                    "transmit descriptor {} does not point at its own buffer",
                    at
                );
                assert_ne!(
                    desc.status & DD,
                    0,
                    "transmit descriptor {} starts owned by the card",
                    at
                );
                assert_eq!(desc.cmd, 0, "descriptor {} starts with a command", at);
            }
            assert!(nic.can_send(), "a card that has just come up cannot send");
        });
    }

    // --- the interrupt ----------------------------------------------------

    /// An interrupt with no cause is NOT claimed. The line is shared, so
    /// claiming it means the device that really raised it never gets its
    /// handler called.
    #[test]
    fn an_interrupt_with_no_cause_is_not_claimed() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            set_reg(base, E1000_ICR, 0);
            assert!(!nic.handle_interrupt());
        });
    }

    /// And one with any cause is claimed, link-status change included.
    #[test]
    fn an_interrupt_with_a_cause_is_claimed_whichever_cause_it_is() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            for cause in [LSC, RXT0, LSC | RXT0, 1 << 31] {
                set_reg(base, E1000_ICR, cause);
                set_reg(base, E1000_STATUS, 1 << 1);
                assert!(nic.handle_interrupt(), "cause {:#x} was not claimed", cause);
            }
        });
    }

    // --- receiving --------------------------------------------------------

    /// Nothing is received until the card marks the descriptor done, and the
    /// tail is NOT moved: handing a descriptor back that the card never filled
    /// loses the frame it is about to put there.
    #[test]
    fn nothing_is_received_until_the_card_marks_the_descriptor_done() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            assert!(nic.receive().is_none());
            assert_eq!(
                reg(base, E1000_RDT),
                (NUM_DESC - 1) as u32,
                "the tail moved for a descriptor the card had not filled"
            );
            assert_eq!(
                nic.rx_next_to_clean, 0,
                "the ring advanced over a live slot"
            );
        });
    }

    /// A delivered frame comes back byte for byte, and the descriptor goes
    /// straight back to the card: status and errors cleared, and the tail
    /// written with the slot just freed. A tail that is never written starves
    /// the card of descriptors one frame at a time until it drops everything.
    #[test]
    fn a_received_frame_comes_back_and_its_descriptor_goes_back_to_the_card() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            let frame = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03];
            the_card_delivers(&nic, 0, &frame, frame.len() as u16);
            assert_eq!(
                nic.receive().expect("a frame was waiting").as_slice(),
                &frame[..],
                "the frame did not come back as the card left it"
            );
            let desc = rx_desc(&nic, 0);
            assert_eq!(desc.status, 0, "the descriptor was not handed back");
            assert_eq!(desc.errors, 0, "the error byte was left set");
            assert_eq!(
                reg(base, E1000_RDT),
                0,
                "the tail was not given the free slot"
            );
            assert_eq!(nic.rx_next_to_clean, 1, "the ring did not advance");
        });
    }

    /// The length is the CARD's word, so it is not ours to trust: a length past
    /// the one-page buffer would have us hand the stack a slice over memory the
    /// descriptor never owned. The helper that clamps it is tested; that the
    /// body USES it is what this asks.
    #[test]
    fn a_length_the_card_reports_past_the_buffer_is_clamped_to_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            the_card_delivers(&nic, 0, &[0xa5], u16::MAX);
            let frame = nic.receive().expect("a frame was waiting");
            assert_eq!(
                frame.len(),
                RX_BUF_SIZE,
                "a frame longer than the buffer it came in"
            );
            assert_eq!(frame[0], 0xa5, "and it still starts where the buffer does");
        });
    }

    /// The ring wraps instead of running off the end. Without the wrap the
    /// next read is one descriptor past the ring, which is whatever the
    /// allocator put there.
    #[test]
    fn the_receive_ring_wraps_at_the_end_instead_of_running_off_it() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            nic.rx_next_to_clean = NUM_DESC - 1;
            the_card_delivers(&nic, NUM_DESC - 1, &[0x7f], 1);
            // So that finding the last slot in the tail says it was written.
            set_reg(base, E1000_RDT, 0);
            assert_eq!(
                nic.receive().expect("a frame was waiting").as_slice(),
                &[0x7f][..]
            );
            assert_eq!(nic.rx_next_to_clean, 0, "the ring did not wrap");
            assert_eq!(
                reg(base, E1000_RDT),
                (NUM_DESC - 1) as u32,
                "the freed slot is the last one"
            );
        });
    }

    // --- sending ----------------------------------------------------------

    /// A posted frame lands in the slot's own buffer with the three command
    /// bits the card needs, its status cleared so DD means something, and the
    /// tail moved on. The tail write is the doorbell: without it the frame
    /// sits in the ring and the card never looks.
    #[test]
    fn a_posted_frame_lands_in_its_buffer_with_the_command_bits_and_rings_the_bell() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            let frame = [0x11u8; 60];
            assert!(nic.send(&frame), "a card that just came up refused a frame");
            let desc = tx_desc(&nic, 0);
            assert_eq!(desc.len, 60, "the descriptor does not carry the length");
            assert_ne!(desc.cmd & (1 << 0), 0, "EOP is clear: the frame never ends");
            assert_ne!(desc.cmd & (1 << 1), 0, "IFCS is clear: no CRC is appended");
            assert_ne!(desc.cmd & (1 << 3), 0, "RS is clear: DD never comes back");
            assert_eq!(desc.status & DD, 0, "the done bit was left set");
            // SAFETY: the buffer is RX_BUF_SIZE bytes and 60 is less.
            let sent =
                unsafe { core::slice::from_raw_parts(nic.tx_bufs[0].vaddr() as *const u8, 60) };
            assert_eq!(sent, &frame[..], "the bytes did not reach the buffer");
            assert_eq!(nic.tx_next_to_use, 1, "the ring did not advance");
            assert_eq!(reg(base, E1000_TDT), 1, "the doorbell was not rung");
        });
    }

    /// A frame longer than a buffer is REFUSED, not clipped. Clipping put a
    /// corrupt half-length packet on the wire while the caller was told the
    /// whole length had been written, so it looked like data loss at the peer
    /// rather than an error here.
    #[test]
    fn a_frame_longer_than_a_buffer_is_refused_instead_of_clipped() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            assert!(!nic.send(&[0x22u8; RX_BUF_SIZE + 1]), "it was accepted");
            assert_eq!(tx_desc(&nic, 0).len, 0, "a descriptor was written anyway");
            assert_eq!(reg(base, E1000_TDT), 0, "the doorbell was rung anyway");
            assert_eq!(nic.tx_next_to_use, 0, "the ring advanced anyway");
        });
    }

    /// And so is an empty one: a zero-length descriptor is not a frame, and
    /// the card's behaviour on one is not defined.
    #[test]
    fn an_empty_frame_is_refused() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            assert!(!nic.send(&[]), "an empty frame was accepted");
            assert_eq!(reg(base, E1000_TDT), 0, "the doorbell was rung anyway");
        });
    }

    /// The tail slot has to be free, and so does THE ONE AFTER IT. That second
    /// slot is the guard the hardware needs: the card works descriptors from
    /// head up to -- but excluding -- tail, so a tail that catches up with the
    /// head reads as an EMPTY ring. Post all 256 and the posted frames are
    /// never fetched, their done bits never come back, and every later send
    /// fails forever. Transmit is dead until reboot, with nothing to notice.
    #[test]
    fn a_frame_is_refused_when_the_guard_slot_after_the_tail_is_still_the_cards() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            the_card_owns_transmit_slot(&nic, 1, true);
            assert!(!nic.can_send(), "the guard slot was not looked at");
            assert!(
                !nic.send(&[0x33u8; 60]),
                "a frame went out over the guard slot"
            );
            assert_eq!(reg(base, E1000_TDT), 0, "the doorbell was rung anyway");
            assert_eq!(tx_desc(&nic, 0).len, 0, "the descriptor was written anyway");
        });
    }

    /// And the tail slot itself, which is the obvious half: a descriptor the
    /// card still owns is one it may be DMA-reading right now.
    #[test]
    fn a_frame_is_refused_when_the_tail_slot_itself_is_still_the_cards() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            the_card_owns_transmit_slot(&nic, 0, true);
            assert!(!nic.can_send(), "the tail slot was not looked at");
            assert!(
                !nic.send(&[0x44u8; 60]),
                "a frame overwrote a live descriptor"
            );
            assert_eq!(reg(base, E1000_TDT), 0, "the doorbell was rung anyway");
        });
    }

    /// The transmit ring wraps too, and the doorbell carries the WRAPPED tail:
    /// a tail of 256 is outside a 256-descriptor ring, and the card would
    /// reject it or fetch past the end.
    #[test]
    fn the_transmit_ring_wraps_and_the_doorbell_carries_the_wrapped_tail() {
        alone_with_the_allocator(|| {
            let base = a_window();
            let mut nic = a_card(base);
            // One ordinary frame first, so the tail is NOT already zero when
            // the wrapping one is posted.
            assert!(nic.send(&[0x55u8; 60]), "the first frame was refused");
            assert_eq!(reg(base, E1000_TDT), 1, "the tail did not move");
            // Slot 0 is the GUARD slot for a tail at the end of the ring, and
            // the frame just posted left it owned by the card: until the card
            // reports it done, the ring cannot wrap. That is the guard doing
            // its job, so the card finishes it here.
            the_card_owns_transmit_slot(&nic, 0, false);
            nic.tx_next_to_use = NUM_DESC - 1;
            assert!(
                nic.send(&[0x66u8; 60]),
                "the last slot of the ring was refused"
            );
            assert_eq!(nic.tx_next_to_use, 0, "the ring did not wrap");
            assert_eq!(
                reg(base, E1000_TDT),
                0,
                "the doorbell carries an index off the ring"
            );
            assert_eq!(
                tx_desc(&nic, NUM_DESC - 1).len,
                60,
                "the frame went to another slot"
            );
        });
    }
}
