//! LAN driver, only for Realtek currently.
#![allow(unused)]

use crate::sync::Mutex;
use alloc::{sync::Arc, vec};
use smoltcp::socket::SocketSet;

pub mod e1000;
pub mod e1000e;
// pub mod ixgbe;
pub mod loopback;
pub use isomorphic_drivers::provider::Provider;
pub use loopback::{LoopbackDevice, LoopbackInterface};

use crate::scheme::{IrqScheme, Scheme};
use crate::DeviceResult;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Sequential `ethN` names (Linux-style). PCI slot numbering (`eth{bus}d{dev}f{fn}`)
/// made the first NIC `eth0d3f0` on QEMU/VirtualBox, so udhcpc/scripts looking
/// for `eth0` never bound to the real interface.
static ETH_IFACE_SEQ: AtomicUsize = AtomicUsize::new(0);

pub fn next_eth_ifname() -> String {
    let n = ETH_IFACE_SEQ.fetch_add(1, Ordering::Relaxed);
    alloc::format!("eth{}", n)
}

static MSI_IRQ_HOST: Mutex<Option<Arc<dyn IrqScheme>>> = Mutex::new(None);
static MSI_PENDING: Mutex<Vec<(usize, Arc<dyn Scheme>)>> = Mutex::new(Vec::new());
const MAX_MSI_PENDING: usize = 256;
/// Which of the two MSI queues this is, for the log.
const MSI_QUEUE_OWNER: &str = "net";

fn enqueue_pending_msi(
    pending: &mut Vec<(usize, Arc<dyn Scheme>)>,
    vector: usize,
    dev: &Arc<dyn Scheme>,
) {
    if pending
        .iter()
        .any(|(v, d)| *v == vector && core::ptr::eq(Arc::as_ptr(d), Arc::as_ptr(dev)))
    {
        return;
    }
    if pending.len() >= MAX_MSI_PENDING {
        // A dropped entry is a device that will never be told about its own
        // interrupt, so it does not go quietly. One note per device today
        // makes this unreachable, and that is exactly why it would be
        // unreachable to debug as well if it ever were reached.
        let (v, d) = pending.remove(0);
        crate::klog_warn!(
            "[{}] MSI queue full at {}: vector {} for {} dropped, that device will get no interrupt",
            MSI_QUEUE_OWNER,
            MAX_MSI_PENDING,
            v,
            d.name()
        );
    }
    pending.push((vector, dev.clone()));
}

pub fn pci_set_irq_host(irq: Arc<dyn IrqScheme>) {
    *MSI_IRQ_HOST.lock() = Some(irq);
}

pub fn pci_note_pending_msi(vector: usize, dev: Arc<dyn Scheme>) {
    enqueue_pending_msi(&mut MSI_PENDING.lock(), vector, &dev);
}

/// Register an ISR closure for MSI `vector` and unmask it, immediately (not
/// deferred like `pci_note_pending_msi`). Returns true on success. Used by the
/// NVIDIA console-GPU boot to bring the GPU's MSI delivery online for the
/// SEC2-resume window (the Linux-faithful interrupt path) rather than running
/// fully INTx-masked. Shared here because `net` owns the IRQ host handle.
pub fn msi_register_and_unmask(vector: usize, handler: crate::scheme::IrqHandler) -> bool {
    let host = MSI_IRQ_HOST.lock().clone();
    if let Some(host) = host {
        if host.register_handler(vector, handler).is_ok() {
            let _ = host.unmask(vector);
            return true;
        }
    }
    false
}

/// Mask + unregister an MSI `vector` previously brought online with
/// [`msi_register_and_unmask`].
pub fn msi_mask_and_unregister(vector: usize) {
    let host = MSI_IRQ_HOST.lock().clone();
    if let Some(host) = host {
        let _ = host.mask(vector);
        let _ = host.unregister(vector);
    }
}

/// Mask an MSI `vector` without unregistering (the storm self-limiter calls
/// this from inside the ISR).
pub fn msi_mask(vector: usize) {
    let host = MSI_IRQ_HOST.lock().clone();
    if let Some(host) = host {
        let _ = host.mask(vector);
    }
}

pub fn pci_finish_msi_registrations() -> DeviceResult {
    let host = MSI_IRQ_HOST.lock().clone();
    if let Some(host) = host {
        let mut q = MSI_PENDING.lock();
        for (v, d) in q.drain(..) {
            match host.register_device(v, d) {
                Ok(_) => {
                    crate::klog_info!("[net] IRQ vector {} registered for NIC", v);
                    let _ = host.unmask(v);
                }
                Err(e) => crate::klog_warn!("[net] failed to register IRQ vector {}: {:?}", v, e),
            }
        }
    }
    Ok(())
}

// The RTL8211F PHY/GMAC pair only exists on the D1, so the glue that wires it
// into the kernel (`rtlx`) is riscv64-only. The driver itself is opened to the
// host test build as well: it is the largest file in the tree without a test,
// and behind a bare `target_arch` gate no `cargo test` ever compiled a line of
// it, so any test written for it would have been run by nobody.
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "riscv64", test))] {
mod realtek;
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "riscv64")] {
mod rtlx;

pub use rtlx::*;
    }
}

/*
/// External functions that drivers must use
pub trait Provider {
    /// Page size (usually 4K)
    const PAGE_SIZE: usize;

    /// Allocate consequent physical memory for DMA.
    /// Return (`virtual address`, `physical address`).
    /// The address is page aligned.
    fn alloc_dma(size: usize) -> (usize, usize);

    /// Deallocate DMA
    fn dealloc_dma(vaddr: usize, size: usize);
}
*/

pub struct ProviderImpl;

impl Provider for ProviderImpl {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn alloc_dma(size: usize) -> (usize, usize) {
        // `div_ceil`, not `/`: truncating division backs a 2048-byte request
        // with ZERO pages (the caller then writes into memory nobody owns) and
        // a 5000-byte request with one 4096-byte page. `DmaRegion::alloc_inner`
        // already rounds up the same way.
        let pages = size.div_ceil(PAGE_SIZE);
        let paddr = unsafe { drivers_dma_alloc(pages) };
        let vaddr = phys_to_virt(paddr);
        // Consumers of this trait (the ixgbe HAL) require zeroed DMA pages.
        if paddr != 0 {
            unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, pages * PAGE_SIZE) };
        }
        (vaddr, paddr)
    }

    fn dealloc_dma(vaddr: usize, size: usize) {
        let paddr = virt_to_phys(vaddr);
        unsafe { drivers_dma_dealloc(paddr, size.div_ceil(PAGE_SIZE)) };
    }
}

pub fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    unsafe { drivers_phys_to_virt(paddr) }
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    unsafe { drivers_virt_to_phys(vaddr) }
}

pub fn timer_now_as_micros() -> u64 {
    unsafe { drivers_timer_now_as_micros() }
}

unsafe extern "C" {
    fn drivers_dma_alloc(pages: usize) -> PhysAddr;
    fn drivers_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32;
    fn drivers_phys_to_virt(paddr: PhysAddr) -> VirtAddr;
    fn drivers_virt_to_phys(vaddr: VirtAddr) -> PhysAddr;
    fn drivers_timer_now_as_micros() -> u64;
    fn drivers_intr_on();
    fn drivers_intr_off();
    fn drivers_intr_get() -> bool;
    fn drivers_wake_net_rx_waiters();
}

/// Wake all tasks waiting for TCP/UDP RX data.
/// Call this after `iface.poll()` processes incoming packets.
pub fn wake_net_rx_waiters() {
    unsafe { drivers_wake_net_rx_waiters() }
}

pub const PAGE_SIZE: usize = 4096;

type VirtAddr = usize;
type PhysAddr = usize;

lazy_static::lazy_static! {
    pub static ref SOCKETS: Arc<Mutex<SocketSet<'static>>> =
    Arc::new(Mutex::new(SocketSet::new(vec![])));

    static ref PACKET_CALLBACK: Mutex<Option<fn(&[u8])>> = Mutex::new(None);

    /// AF_PACKET taps queued during smoltcp `poll` (SOCKETS held) and flushed after.
    pub static ref DEFERRED_PACKETS: Mutex<alloc::collections::VecDeque<Vec<u8>>> = Mutex::new(alloc::collections::VecDeque::new());
}

/// How many frames one `poll` may hand to an AF_PACKET tap.
///
/// It was 64. The e1000e has **256 RX descriptors** (its own comment says so),
/// every frame of a poll goes through [`net_defer_packet`] while smoltcp holds
/// `SOCKETS`, and the queue is only drained afterwards by
/// [`net_flush_deferred_packets`] -- so one poll of a full ring offered 256
/// frames to a queue that held 64 and dropped the oldest 192 without a word.
/// With a tap running, that is not an edge case: it is every burst that fills
/// the ring, which is what a download does.
///
/// A poll's worth of the largest ring in the tree, then. The frames are only
/// copied at all when a tap is registered, and the queue is emptied on every
/// flush, so the cost is borne by whoever is capturing.
const DEFERRED_PACKET_MAX: usize = 256;

/// Frames dropped because the tap queue was full, since boot.
static DEFERRED_PACKETS_DROPPED: AtomicUsize = AtomicUsize::new(0);

/// How many frames an AF_PACKET tap has lost to a full queue.
///
/// A cap that drops is invisible without this: the tap simply shows fewer
/// frames than went past, and nothing anywhere says so.
pub fn dropped_deferred_packets() -> usize {
    DEFERRED_PACKETS_DROPPED.load(Ordering::Relaxed)
}

/// Sets a callback for every received packet (raw).
pub fn set_packet_callback(callback: fn(&[u8])) {
    // Mutex::lock() uses push_off/pop_off which handles interrupt disabling.
    // Manual intr_off/on here bypasses noff accounting and causes
    // "RefCell already borrowed" panics under SMP.
    *PACKET_CALLBACK.lock() = Some(callback);
}

/// Dispatches a received packet to the registered callback.
pub fn net_dispatch_packet(data: &[u8]) {
    // Hot path for AF_PACKET / edhcpc — keep quiet (was warn! per packet).
    // Copy the fn pointer and drop the mutex before invoking — `push_packet` locks
    // more mutexes; holding PACKET_CALLBACK across the call nests push_off/pop_off
    // badly with manual intr_on/intr_off and panics with "RefCell already borrowed".
    let callback = *PACKET_CALLBACK.lock();
    if let Some(cb) = callback {
        cb(data);
    }
}

/// Queue a frame for AF_PACKET while smoltcp holds `SOCKETS` (see [`net_flush_deferred_packets`]).
///
/// No-op (and no allocation) when no packet callback is registered — the common
/// case for normal TCP/UDP traffic without an AF_PACKET tap.
pub fn net_defer_packet(data: &[u8]) {
    if PACKET_CALLBACK.lock().is_none() {
        return;
    }
    queue_for_the_tap(data.to_vec());
}

/// The one place the cap is applied, so there is one cap.
///
/// It used to be written out twice, once here and once in
/// [`net_defer_packet_owned`], which is two places to fix and one to forget.
fn queue_for_the_tap(frame: Vec<u8>) {
    let mut q = DEFERRED_PACKETS.lock();
    if q.len() >= DEFERRED_PACKET_MAX {
        q.pop_front();
        DEFERRED_PACKETS_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    q.push_back(frame);
}

/// Like [`net_defer_packet`], but takes ownership so the caller can avoid a
/// second heap copy when it already holds a `Vec<u8>`.
pub fn net_defer_packet_owned(data: Vec<u8>) {
    if PACKET_CALLBACK.lock().is_none() {
        return;
    }
    queue_for_the_tap(data);
}

/// Flush frames queued by [`net_defer_packet`]; call only after releasing smoltcp locks.
pub fn net_flush_deferred_packets() {
    let batch: alloc::collections::VecDeque<Vec<u8>> = {
        let mut q = DEFERRED_PACKETS.lock();
        core::mem::take(&mut *q)
    };
    for pkt in batch {
        net_dispatch_packet(&pkt);
    }
}

// 注意！这个容易出现死锁
pub fn get_sockets() -> Arc<Mutex<SocketSet<'static>>> {
    SOCKETS.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::IrqScheme;
    extern crate std;

    /// Everything here lives in process-wide statics -- the tap callback, the
    /// deferred queue, the drop counter, the MSI queue -- so the tests take
    /// turns, and each leaves them as it found them.
    fn alone_with_the_statics<R>(body: impl FnOnce() -> R) -> R {
        static TURNSTILE: Mutex<()> = Mutex::new(());
        let _guard = TURNSTILE.lock();
        clear_the_statics();
        let out = body();
        clear_the_statics();
        out
    }

    fn clear_the_statics() {
        *PACKET_CALLBACK.lock() = None;
        DEFERRED_PACKETS.lock().clear();
        DEFERRED_PACKETS_DROPPED.store(0, Ordering::SeqCst);
        MSI_PENDING.lock().clear();
        *MSI_IRQ_HOST.lock() = None;
        SEEN.lock().clear();
    }

    /// The frames the tap was handed, in the order it got them.
    static SEEN: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

    fn tap(data: &[u8]) {
        SEEN.lock().push(data.to_vec());
    }

    fn seen() -> Vec<Vec<u8>> {
        SEEN.lock().clone()
    }

    fn frame(n: u32) -> Vec<u8> {
        // Each frame carries its own number, so a frame that arrives out of
        // order or in place of another is a wrong number rather than one
        // indistinguishable buffer among many.
        n.to_le_bytes().to_vec()
    }

    #[test]
    fn nothing_is_copied_when_nobody_is_listening() {
        // The common case: no AF_PACKET tap, and the receive path must not pay
        // an allocation and a copy per frame for a queue nobody drains.
        alone_with_the_statics(|| {
            for i in 0..10 {
                net_defer_packet(&frame(i));
                net_defer_packet_owned(frame(i));
            }
            assert!(DEFERRED_PACKETS.lock().is_empty());
            assert_eq!(dropped_deferred_packets(), 0);
        })
    }

    #[test]
    fn a_frame_reaches_the_tap_only_once_the_queue_is_flushed() {
        // The whole point of deferring: `net_defer_packet` is called with
        // smoltcp's `SOCKETS` held, and the callback takes more locks.
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            net_defer_packet(&frame(7));
            assert!(seen().is_empty(), "the tap was called with SOCKETS held");
            net_flush_deferred_packets();
            assert_eq!(seen(), std::vec![frame(7)]);
        })
    }

    #[test]
    fn the_frames_reach_the_tap_in_the_order_they_arrived() {
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            for i in 0..8 {
                net_defer_packet(&frame(i));
            }
            net_flush_deferred_packets();
            assert_eq!(seen(), (0..8).map(frame).collect::<Vec<_>>());
        })
    }

    #[test]
    fn a_whole_ring_of_frames_reaches_the_tap() {
        // The e1000e has 256 RX descriptors, and one poll can drain all of
        // them before anything is flushed. The cap was 64, so 192 of those 256
        // frames were dropped -- silently, and the oldest first, which for a
        // capture means losing the beginning of the burst.
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            for i in 0..256 {
                net_defer_packet(&frame(i));
            }
            net_flush_deferred_packets();
            assert_eq!(seen().len(), 256, "a poll of a full ring lost frames");
            assert_eq!(seen(), (0..256).map(frame).collect::<Vec<_>>());
            assert_eq!(dropped_deferred_packets(), 0);
        })
    }

    #[test]
    fn the_frames_that_do_not_fit_are_counted_instead_of_vanishing() {
        // Past the cap frames still have to go, but a cap that drops without
        // counting is a cap nobody can see: the tap just shows fewer frames
        // than went past and nothing anywhere says so.
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            for i in 0..(DEFERRED_PACKET_MAX + 10) as u32 {
                net_defer_packet(&frame(i));
            }
            assert_eq!(dropped_deferred_packets(), 10);
            net_flush_deferred_packets();
            assert_eq!(seen().len(), DEFERRED_PACKET_MAX);
            // The oldest went, so the newest are the ones still here.
            assert_eq!(seen()[0], frame(10));
        })
    }

    #[test]
    fn the_cap_is_the_same_whether_the_frame_is_borrowed_or_owned() {
        // It was written out twice, which is two places to fix and one to
        // forget. This is the test that would have noticed.
        for owned in [false, true] {
            alone_with_the_statics(|| {
                set_packet_callback(tap);
                for i in 0..(DEFERRED_PACKET_MAX + 3) as u32 {
                    if owned {
                        net_defer_packet_owned(frame(i));
                    } else {
                        net_defer_packet(&frame(i));
                    }
                }
                assert_eq!(
                    (DEFERRED_PACKETS.lock().len(), dropped_deferred_packets()),
                    (DEFERRED_PACKET_MAX, 3),
                    "owned = {}",
                    owned
                );
            })
        }
    }

    #[test]
    fn a_flush_leaves_the_queue_empty_even_if_the_tap_goes_away() {
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            net_defer_packet(&frame(1));
            *PACKET_CALLBACK.lock() = None;
            net_flush_deferred_packets();
            assert!(
                DEFERRED_PACKETS.lock().is_empty(),
                "the frames of a tap that unregistered stay queued for ever"
            );
            assert!(seen().is_empty());
        })
    }

    #[test]
    fn a_flush_with_nothing_queued_does_nothing() {
        alone_with_the_statics(|| {
            set_packet_callback(tap);
            net_flush_deferred_packets();
            assert!(seen().is_empty());
        })
    }

    #[test]
    fn the_interfaces_are_named_the_way_the_scripts_look_for_them() {
        // `eth{bus}d{dev}f{fn}` made the first NIC `eth0d3f0`, and udhcpc never
        // bound to it. The names are sequential and start where the counter is,
        // so the test asserts the shape and the step, not an absolute number.
        let first = next_eth_ifname();
        let second = next_eth_ifname();
        assert!(first.starts_with("eth"), "{}", first);
        let n: usize = first[3..].parse().expect("the name is not ethN");
        assert_eq!(second, alloc::format!("eth{}", n + 1));
    }

    /// An interrupt controller that refuses the vectors it was told to refuse.
    struct PickyIntc {
        refuse: Vec<usize>,
        unmask_refuse: Vec<usize>,
        registered: Mutex<Vec<usize>>,
        unmasked: Mutex<Vec<usize>>,
    }

    impl PickyIntc {
        fn refusing(refuse: &[usize]) -> Arc<Self> {
            Arc::new(PickyIntc {
                refuse: refuse.to_vec(),
                unmask_refuse: Vec::new(),
                registered: Mutex::new(Vec::new()),
                unmasked: Mutex::new(Vec::new()),
            })
        }
    }

    impl Scheme for PickyIntc {
        fn name(&self) -> &str {
            "picky-intc"
        }
    }

    impl IrqScheme for PickyIntc {
        fn is_valid_irq(&self, _irq: usize) -> bool {
            true
        }
        fn mask(&self, _irq: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, irq: usize) -> DeviceResult {
            if self.unmask_refuse.contains(&irq) {
                return Err(crate::DeviceError::InvalidParam);
            }
            self.unmasked.lock().push(irq);
            Ok(())
        }
        fn register_handler(&self, _irq: usize, _h: crate::scheme::IrqHandler) -> DeviceResult {
            Ok(())
        }
        fn register_device(&self, irq: usize, _dev: Arc<dyn Scheme>) -> DeviceResult {
            if self.refuse.contains(&irq) {
                return Err(crate::DeviceError::InvalidParam);
            }
            self.registered.lock().push(irq);
            Ok(())
        }
        fn unregister(&self, _irq: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// A NIC, as far as this queue is concerned.
    struct Nic(&'static str);
    impl Scheme for Nic {
        fn name(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn a_noted_vector_is_registered_and_unmasked_when_the_walk_finishes() {
        alone_with_the_statics(|| {
            let intc = PickyIntc::refusing(&[]);
            pci_set_irq_host(intc.clone());
            pci_note_pending_msi(11, Arc::new(Nic("eth0")));
            pci_finish_msi_registrations().expect("the walk failed");
            assert_eq!(intc.registered.lock().clone(), std::vec![11]);
            assert_eq!(intc.unmasked.lock().clone(), std::vec![11]);
            assert!(
                MSI_PENDING.lock().is_empty(),
                "a registered vector is still pending"
            );
        })
    }

    #[test]
    fn two_devices_sharing_a_vector_are_both_kept() {
        // MSI vectors get shared, and the dedup is about a repeated *note* of
        // the same device, not about the vector. Matching on the vector alone
        // would drop the second card and leave it without interrupts, which is
        // the same silence as every other bug in this file.
        alone_with_the_statics(|| {
            let intc = PickyIntc::refusing(&[]);
            pci_set_irq_host(intc.clone());
            pci_note_pending_msi(11, Arc::new(Nic("eth0")));
            pci_note_pending_msi(11, Arc::new(Nic("eth1")));
            assert_eq!(
                MSI_PENDING.lock().len(),
                2,
                "a device sharing a vector was dropped"
            );
            pci_finish_msi_registrations().expect("the walk failed");
            assert_eq!(intc.registered.lock().clone(), std::vec![11, 11]);
        })
    }

    #[test]
    fn the_same_device_and_vector_is_not_queued_twice() {
        alone_with_the_statics(|| {
            let dev: Arc<dyn Scheme> = Arc::new(Nic("eth0"));
            pci_note_pending_msi(11, dev.clone());
            pci_note_pending_msi(11, dev.clone());
            assert_eq!(MSI_PENDING.lock().len(), 1);
            // A different vector for the same device is a different thing.
            pci_note_pending_msi(12, dev);
            assert_eq!(MSI_PENDING.lock().len(), 2);
        })
    }

    #[test]
    fn a_vector_the_controller_refuses_does_not_cost_the_devices_behind_it() {
        // This is the one that matters, and the twin of this function in
        // `usb::xhci_hid` used to fail it: with `?` inside the `drain`, the
        // first refusal returned from the function and `Drain::drop` threw away
        // every entry behind it, so a device queued after a failing one never
        // got its interrupt -- and the caller discards the error, so nothing
        // was logged either.
        alone_with_the_statics(|| {
            let intc = PickyIntc::refusing(&[7]);
            pci_set_irq_host(intc.clone());
            pci_note_pending_msi(7, Arc::new(Nic("eth0")));
            pci_note_pending_msi(9, Arc::new(Nic("eth1")));
            pci_finish_msi_registrations().expect("the walk failed");
            assert_eq!(
                intc.registered.lock().clone(),
                std::vec![9],
                "the device behind the refused one lost its interrupt"
            );
            assert!(MSI_PENDING.lock().is_empty());
        })
    }

    #[test]
    fn a_walk_with_no_interrupt_controller_leaves_the_queue_alone() {
        // The NICs are enumerated before the controller is handed over, so the
        // queue has to survive a walk that comes too early -- otherwise every
        // vector noted before then is lost.
        alone_with_the_statics(|| {
            pci_note_pending_msi(11, Arc::new(Nic("eth0")));
            pci_finish_msi_registrations().expect("the walk failed");
            assert_eq!(
                MSI_PENDING.lock().len(),
                1,
                "the queue was emptied with nowhere to register"
            );
        })
    }

    #[test]
    fn a_full_queue_says_which_device_it_is_dropping() {
        // Unreachable with one note per device, and that is the reason it is
        // pinned: if it ever is reached, the note in the log is the only thing
        // that could explain a NIC that never interrupts.
        alone_with_the_statics(|| {
            for v in 0..MAX_MSI_PENDING + 5 {
                pci_note_pending_msi(v, Arc::new(Nic("eth0")));
            }
            let q = MSI_PENDING.lock();
            assert_eq!(q.len(), MAX_MSI_PENDING);
            assert_eq!(q[0].0, 5, "the oldest entries were not the ones dropped");
        })
    }
}
