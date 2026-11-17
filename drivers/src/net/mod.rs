//! LAN drivers: Intel e1000/e1000e, Realtek, AMD PCnet and XGMAC, loopback.
#![allow(unused)]

use crate::sync::Mutex;
use alloc::{sync::Arc, vec};

pub mod e1000;
pub mod e1000e;
pub mod ether_hash;
pub mod ip_cfg;
/// A card of the kernel's lent to a stack that runs outside the kernel.
pub mod lent;
pub mod loopback;
// Los registros MII/MDIO genericos (el papel de `include/uapi/linux/mii.h`):
// ni el GMAC del D1 ni el GENET de la Raspberry Pi son sus dueños, los dos los
// usan para hablar con su PHY.
pub mod mii;
/// AMD PCnet-PCI II / PCnet-FAST III. Driven through PCI I/O ports, which only
/// x86 has, so the module is not built elsewhere.
#[cfg(target_arch = "x86_64")]
pub mod pcnet32;
/// The card of a stack that runs outside the kernel, to be looked at.
pub mod stack_card;
/// AMD XGMAC 10GbE (`amd-xgbe`). MMIO, so it builds everywhere; see the
/// module docs for what it does and does not do.
pub mod xgbe;
pub use isomorphic_drivers::provider::Provider;
pub use loopback::ShownLoopback;

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
        drop(q);
        finish_intx_registrations(&host);
    }
    Ok(())
}

/// Cards that have no MSI and raise the legacy line of their slot (INTx),
/// by the line: see [`pci_intx_line`].
static INTX_PENDING: Mutex<Vec<(usize, Arc<dyn Scheme>)>> = Mutex::new(Vec::new());
/// Set by the boot: see [`set_pci_intx_enabled`].
static INTX_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether a card with no MSI is given the legacy interrupt line of its
/// slot ([`pci_intx_line`]). It is under a hypervisor, and on a boot that
/// says `pci.intx=1` or `unetlend=any` (`kernel_hal::drivers::pci_intx_wanted`):
/// a card with no interrupt can only be looked at now and then, which is
/// enough for the kernel's own stack, that looks whenever a program waits
/// on a socket, and not for a borrower. Another boot leaves such a card
/// with no interrupt, as it always was: the line is the one the firmware wrote, which is
/// right where the lines of the PCI slots are steered to ISA interrupts
/// (QEMU does) and has not been seen on a real machine, where another
/// device may hold the same line with nobody to answer for it.
pub fn set_pci_intx_enabled(on: bool) {
    INTX_ON.store(on, Ordering::Release);
}

/// The interrupt line a PCI card with no MSI raises, as the number its
/// handler is registered with ([`pci_note_pending_intx`]) and called with.
///
/// It is the line the firmware wrote in the card's configuration space:
/// the ISA interrupt its slot's pin is steered to, which the I/O APIC has
/// on the pin of the same number. That is how QEMU's `e1000` and `pcnet`
/// interrupt, which have no MSI; without it nothing told their drivers of
/// a frame, and only the kernel's own stack, which looks at its cards
/// whenever a program waits on a socket, could use them. `None` for a card
/// with no interrupt pin, for a line the firmware did not give (0, 255) or
/// that is no ISA line, off x86_64, and on a boot that has not asked for
/// it ([`set_pci_intx_enabled`]).
pub fn pci_intx_line<Pin>(line: u8, pin: Option<Pin>) -> Option<usize> {
    if !cfg!(target_arch = "x86_64") || !INTX_ON.load(Ordering::Acquire) {
        return None;
    }
    pin?;
    // 2 is the cascade of the two 8259s and no line of a device's.
    matches!(line, 1 | 3..=15).then_some(line as usize)
}

/// `dev` raises the legacy line `line` ([`pci_intx_line`]): its handler is
/// registered, with any other card's on the same line, when the interrupt
/// controller is up ([`pci_finish_msi_registrations`]).
pub fn pci_note_pending_intx(line: usize, dev: Arc<dyn Scheme>) {
    let mut pending = INTX_PENDING.lock();
    if !pending
        .iter()
        .any(|(l, d)| *l == line && core::ptr::eq(Arc::as_ptr(d), Arc::as_ptr(&dev)))
    {
        pending.push((line, dev));
    }
}

/// The lines of [`INTX_PENDING`], each with the cards that share it.
fn intx_lines(pending: Vec<(usize, Arc<dyn Scheme>)>) -> Vec<(usize, Vec<Arc<dyn Scheme>>)> {
    let mut lines: Vec<(usize, Vec<Arc<dyn Scheme>>)> = Vec::new();
    for (line, dev) in pending {
        match lines.iter_mut().find(|(l, _)| *l == line) {
            Some((_, devs)) => devs.push(dev),
            None => lines.push((line, alloc::vec![dev])),
        }
    }
    lines
}

fn finish_intx_registrations(host: &Arc<dyn IrqScheme>) {
    use crate::prelude::{IrqPolarity, IrqTriggerMode};
    let pending = core::mem::take(&mut *INTX_PENDING.lock());
    for (line, devs) in intx_lines(pending) {
        // A PCI line is held down until its card is answered, and shared:
        // every card on it is asked, and each knows by its own registers
        // whether it was the one.
        let names: Vec<&str> = devs.iter().map(|d| d.name()).collect();
        let names = names.join(",");
        let handler: crate::scheme::IrqHandler = Arc::new(move || {
            for dev in &devs {
                dev.handle_irq(line);
            }
        });
        let first = handler.clone();
        let done = host
            .configure(line, IrqTriggerMode::Level, IrqPolarity::ActiveLow)
            .and_then(|_| host.register_handler(line, handler))
            .and_then(|_| host.unmask(line));
        if done.is_ok() {
            // A card that raised its line before this (a PCnet does, the
            // moment it is started) raised it at an entry that was still
            // masked and edge, where a controller may drop it: the line is
            // then held with nothing left to announce it, and the card is
            // never heard again. Asking the cards once here answers it.
            first();
        }
        match done {
            Ok(_) => crate::klog_info!(
                "[net] legacy interrupt line {} registered for {}",
                line,
                names
            ),
            Err(e) => crate::klog_warn!(
                "[net] failed to register legacy interrupt line {} for {}: {:?}",
                line,
                names,
                e
            ),
        }
    }
}

// The RTL8211F PHY/GMAC pair only exists on the D1, so neither the driver nor
// the `rtlx` glue above it is built into any other kernel. Both are opened to
// the host test build all the same: behind a bare `target_arch` gate no
// `cargo test` ever compiled a line of either, so any test written for them
// would have been run by nobody.
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "riscv64", test))] {
mod realtek;
    }
}

// The glue itself comes with the host test build too, for the same reason the
// driver below it does: behind a bare `target_arch` no `cargo test` compiles a
// line of it, so nobody could write a test for it and nobody would have run one.
// Only the module, not the re-export: the public surface on every other target
// stays exactly what it was.
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "riscv64", test))] {
mod rtlx;
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "riscv64")] {
pub use rtlx::*;
    }
}

// El GENET v5 del BCM2711 (la Raspberry Pi 4). Como el driver del D1, se abre
// al build de test: el fichero no lleva ni una instruccion de ARM --solo lee y
// escribe una ventana de registros-- asi que detras de un `target_arch` pelado
// ningun `cargo test` compilaria una linea y nadie podria probarlo.
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "aarch64", test))] {
mod genet;
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "aarch64")] {
pub use genet::*;
    }
}

// El Cadence GEM (`cdns,macb`), que es el Ethernet de la Raspberry Pi 5: el
// RP1 lo lleva dentro. Se abre al build de test por lo mismo que el GENET.
cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "aarch64", test))] {
mod macb;
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "aarch64")] {
pub use macb::*;
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

/// Wake the tasks that wait for frames of a card.
pub fn wake_net_rx_waiters() {
    unsafe { drivers_wake_net_rx_waiters() }
}

pub const PAGE_SIZE: usize = 4096;

type VirtAddr = usize;
type PhysAddr = usize;

static PACKET_CALLBACK: Mutex<Option<fn(&[u8])>> = Mutex::new(None);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::IrqScheme;
    extern crate std;

    /// Everything here lives in process-wide statics -- the tap callback,
    /// the MSI queue -- so the tests take
    /// turns, and each leaves them as it found them.
    pub(crate) fn alone_with_the_statics<R>(body: impl FnOnce() -> R) -> R {
        static TURNSTILE: Mutex<()> = Mutex::new(());
        let _guard = TURNSTILE.lock();
        clear_the_statics();
        let out = body();
        clear_the_statics();
        out
    }

    fn clear_the_statics() {
        *PACKET_CALLBACK.lock() = None;
        MSI_PENDING.lock().clear();
        INTX_PENDING.lock().clear();
        INTX_ON.store(false, Ordering::SeqCst);
        *MSI_IRQ_HOST.lock() = None;
        SEEN.lock().clear();
    }

    /// The frames the tap was handed, in the order it got them.
    static SEEN: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

    fn tap(data: &[u8]) {
        SEEN.lock().push(data.to_vec());
    }

    /// The frames of this module's tests: the cards of other tests show
    /// theirs to the same tap meanwhile, and none of those is four bytes.
    fn seen() -> Vec<Vec<u8>> {
        let mut seen = SEEN.lock().clone();
        seen.retain(|frame| frame.len() == 4);
        seen
    }

    fn frame(n: u32) -> Vec<u8> {
        // Each frame carries its own number, so a frame that arrives out of
        // order or in place of another is a wrong number rather than one
        // indistinguishable buffer among many.
        n.to_le_bytes().to_vec()
    }

    #[test]
    fn a_frame_reaches_the_tap_that_listens_and_nobody_when_none_does() {
        alone_with_the_statics(|| {
            net_dispatch_packet(&frame(1));
            assert!(seen().is_empty());
            set_packet_callback(tap);
            for i in 2..5 {
                net_dispatch_packet(&frame(i));
            }
            assert_eq!(seen(), (2..5).map(frame).collect::<Vec<_>>());
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
        /// The vectors a handler was registered for by hand, through
        /// `msi_register_and_unmask` rather than the deferred walk.
        handlers: Mutex<Vec<usize>>,
        masked: Mutex<Vec<usize>>,
        unregistered: Mutex<Vec<usize>>,
    }

    impl PickyIntc {
        fn refusing(refuse: &[usize]) -> Arc<Self> {
            Arc::new(PickyIntc {
                refuse: refuse.to_vec(),
                unmask_refuse: Vec::new(),
                registered: Mutex::new(Vec::new()),
                unmasked: Mutex::new(Vec::new()),
                handlers: Mutex::new(Vec::new()),
                masked: Mutex::new(Vec::new()),
                unregistered: Mutex::new(Vec::new()),
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
        fn mask(&self, irq: usize) -> DeviceResult {
            self.masked.lock().push(irq);
            Ok(())
        }
        fn unmask(&self, irq: usize) -> DeviceResult {
            if self.unmask_refuse.contains(&irq) {
                return Err(crate::DeviceError::InvalidParam);
            }
            self.unmasked.lock().push(irq);
            Ok(())
        }
        fn register_handler(&self, irq: usize, _h: crate::scheme::IrqHandler) -> DeviceResult {
            self.handlers.lock().push(irq);
            Ok(())
        }
        fn register_device(&self, irq: usize, _dev: Arc<dyn Scheme>) -> DeviceResult {
            if self.refuse.contains(&irq) {
                return Err(crate::DeviceError::InvalidParam);
            }
            self.registered.lock().push(irq);
            Ok(())
        }
        fn unregister(&self, irq: usize) -> DeviceResult {
            self.unregistered.lock().push(irq);
            Ok(())
        }
    }

    /// An interrupt controller with lines: it keeps how each was set up and
    /// the handler it was given, to raise the line with.
    #[derive(Default)]
    struct LineIntc {
        configured: Mutex<Vec<(usize, alloc::string::String)>>,
        handlers: Mutex<Vec<(usize, crate::scheme::IrqHandler)>>,
        unmasked: Mutex<Vec<usize>>,
    }

    impl Scheme for LineIntc {
        fn name(&self) -> &str {
            "line-intc"
        }
    }

    impl IrqScheme for LineIntc {
        fn is_valid_irq(&self, _irq: usize) -> bool {
            true
        }
        fn mask(&self, _irq: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, irq: usize) -> DeviceResult {
            self.unmasked.lock().push(irq);
            Ok(())
        }
        fn configure(
            &self,
            irq: usize,
            tm: crate::prelude::IrqTriggerMode,
            pol: crate::prelude::IrqPolarity,
        ) -> DeviceResult {
            self.configured
                .lock()
                .push((irq, alloc::format!("{:?}/{:?}", tm, pol)));
            Ok(())
        }
        fn register_handler(&self, irq: usize, h: crate::scheme::IrqHandler) -> DeviceResult {
            self.handlers.lock().push((irq, h));
            Ok(())
        }
        fn unregister(&self, _irq: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// A card that counts the interrupts it is asked about, by line.
    struct Asked(&'static str, Mutex<Vec<usize>>);
    impl Scheme for Asked {
        fn name(&self) -> &str {
            self.0
        }
        fn handle_irq(&self, irq: usize) {
            self.1.lock().push(irq);
        }
    }

    #[test]
    fn the_line_of_a_card_with_no_msi_is_the_one_the_firmware_wrote() {
        alone_with_the_statics(|| {
            if !cfg!(target_arch = "x86_64") {
                set_pci_intx_enabled(true);
                assert_eq!(pci_intx_line(11, Some(1u8)), None);
                return;
            }
            // A boot that has not asked gives no card a line.
            assert_eq!(pci_intx_line(11, Some(1u8)), None);
            set_pci_intx_enabled(true);
            assert_eq!(pci_intx_line(11, Some(1u8)), Some(11));
            assert_eq!(pci_intx_line(5, Some(4u8)), Some(5));
            // No pin, no line: the card raises nothing.
            assert_eq!(pci_intx_line::<u8>(11, None), None);
            // A line the firmware did not give, the cascade, and what is
            // no ISA line.
            for line in [0u8, 2, 16, 33, 255] {
                assert_eq!(pci_intx_line(line, Some(1u8)), None, "line {}", line);
            }
            // And back.
            set_pci_intx_enabled(false);
            assert_eq!(pci_intx_line(11, Some(1u8)), None);
            set_pci_intx_enabled(true);
            assert_eq!(pci_intx_line(11, Some(1u8)), Some(11));
        })
    }

    #[test]
    fn the_cards_of_a_shared_line_are_all_asked_when_it_is_raised() {
        alone_with_the_statics(|| {
            let intc = Arc::new(LineIntc::default());
            pci_set_irq_host(intc.clone());
            let a = Arc::new(Asked("eth0", Mutex::new(Vec::new())));
            let b = Arc::new(Asked("eth1", Mutex::new(Vec::new())));
            let c = Arc::new(Asked("eth2", Mutex::new(Vec::new())));
            pci_note_pending_intx(11, a.clone());
            pci_note_pending_intx(11, b.clone());
            // Noted twice, asked once.
            pci_note_pending_intx(11, a.clone());
            pci_note_pending_intx(10, c.clone());
            pci_finish_msi_registrations().expect("the walk failed");

            // One handler a line, level and active low as a PCI line is,
            // set up before the handler is there, and then unmasked.
            assert_eq!(
                intc.configured.lock().clone(),
                std::vec![
                    (11, alloc::string::String::from("Level/ActiveLow")),
                    (10, alloc::string::String::from("Level/ActiveLow")),
                ]
            );
            assert_eq!(intc.unmasked.lock().clone(), std::vec![11, 10]);
            // Each card was asked once as its line was registered: one
            // that raised it before then is answered, and heard again.
            assert_eq!(a.1.lock().clone(), std::vec![11]);
            assert_eq!(b.1.lock().clone(), std::vec![11]);
            assert_eq!(c.1.lock().clone(), std::vec![10]);
            let handlers = intc.handlers.lock();
            assert_eq!(handlers.len(), 2);
            (handlers[0].1)();
            assert_eq!(a.1.lock().clone(), std::vec![11, 11]);
            assert_eq!(b.1.lock().clone(), std::vec![11, 11]);
            assert_eq!(c.1.lock().clone(), std::vec![10]);
            (handlers[1].1)();
            assert_eq!(c.1.lock().clone(), std::vec![10, 10]);
            // The walk took them: a second one registers nothing more.
            drop(handlers);
            pci_finish_msi_registrations().expect("the walk failed");
            assert_eq!(intc.handlers.lock().len(), 2);
        })
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

    /// A vector brought online by hand is registered and unmasked, in that
    /// order. This is the path the NVIDIA console GPU takes to get its MSI
    /// delivery up for the SEC2-resume window, rather than the deferred walk
    /// the NICs use.
    #[test]
    fn a_vector_brought_online_by_hand_is_registered_and_unmasked() {
        alone_with_the_statics(|| {
            let intc = PickyIntc::refusing(&[]);
            pci_set_irq_host(intc.clone());

            assert!(msi_register_and_unmask(42, Arc::new(|| {})));
            assert_eq!(intc.handlers.lock().clone(), std::vec![42]);
            assert_eq!(
                intc.unmasked.lock().clone(),
                std::vec![42],
                "a handler nobody unmasked is a handler that never runs"
            );
        })
    }

    /// With no interrupt controller there is nothing to register with, and the
    /// caller is told so rather than being left to believe the vector is live.
    #[test]
    fn a_vector_cannot_be_brought_online_without_a_controller() {
        alone_with_the_statics(|| {
            assert!(!msi_register_and_unmask(42, Arc::new(|| {})));
        })
    }

    /// Taking a vector offline masks it and gives it back. `msi_mask` on its
    /// own is what the storm self-limiter calls from inside the ISR, and it
    /// must NOT unregister: the handler has to still be there when the vector
    /// is unmasked again.
    #[test]
    fn taking_a_vector_offline_masks_it_and_only_then_gives_it_back() {
        alone_with_the_statics(|| {
            let intc = PickyIntc::refusing(&[]);
            pci_set_irq_host(intc.clone());

            msi_mask(7);
            assert_eq!(intc.masked.lock().clone(), std::vec![7]);
            assert!(
                intc.unregistered.lock().is_empty(),
                "masking from inside the ISR unregistered the handler"
            );

            msi_mask_and_unregister(9);
            assert_eq!(intc.masked.lock().clone(), std::vec![7, 9]);
            assert_eq!(intc.unregistered.lock().clone(), std::vec![9]);
        })
    }

    // ------------------------------------------------------------------- DMA

    /// A request that is not a whole number of pages still gets whole pages.
    /// Truncating division backs a 2048-byte request with ZERO pages -- the
    /// caller then writes into memory nobody owns -- and a 5000-byte request
    /// with one 4096-byte page.
    #[test]
    fn a_dma_request_is_rounded_up_to_whole_pages() {
        use crate::utils::host_hooks::{alone_with_the_allocator, ALLOC_PAGES, DEALLOC_PAGES};

        for (bytes, pages) in [
            (1usize, 1usize),
            (PAGE_SIZE / 2, 1),
            (PAGE_SIZE, 1),
            (PAGE_SIZE + 1, 2),
            (PAGE_SIZE * 2 - 1, 2),
        ] {
            alone_with_the_allocator(|| {
                let (vaddr, _paddr) = ProviderImpl::alloc_dma(bytes);
                assert_eq!(
                    ALLOC_PAGES.load(Ordering::SeqCst),
                    pages,
                    "{} bytes asked for {} pages",
                    bytes,
                    ALLOC_PAGES.load(Ordering::SeqCst)
                );
                ProviderImpl::dealloc_dma(vaddr, bytes);
                assert_eq!(
                    DEALLOC_PAGES.load(Ordering::SeqCst),
                    pages,
                    "{} bytes gave back a different number of pages than it took",
                    bytes
                );
            })
        }
    }

    /// The pages come back zeroed however dirty they were. The ixgbe HAL
    /// requires it, and a recycled frame is what the kernel allocator actually
    /// hands out -- all of it, not just the first page.
    #[test]
    fn dma_pages_come_back_zeroed_even_when_they_were_recycled() {
        use crate::utils::host_hooks::{alone_with_the_allocator, POISON_ALLOC};

        alone_with_the_allocator(|| {
            POISON_ALLOC.store(true, Ordering::SeqCst);
            let size = PAGE_SIZE * 3;
            let (vaddr, paddr) = ProviderImpl::alloc_dma(size);
            assert_ne!(paddr, 0);
            let region = unsafe { core::slice::from_raw_parts(vaddr as *const u8, size) };
            assert!(
                region.iter().all(|b| *b == 0),
                "{} of {} bytes came back dirty",
                region.iter().filter(|b| **b != 0).count(),
                size
            );
        })
    }

    /// An allocation that failed answers zero, and nothing is written through
    /// it. The zeroing walks `pages * PAGE_SIZE` bytes from the virtual address
    /// of whatever came back, so doing it unconditionally writes three pages of
    /// zeroes starting at the null pointer.
    #[test]
    fn a_dma_allocation_that_failed_is_not_zeroed_through_its_null_pointer() {
        use crate::utils::host_hooks::{alone_with_the_allocator, FAIL_ALLOC};

        alone_with_the_allocator(|| {
            FAIL_ALLOC.store(true, Ordering::SeqCst);
            let (_vaddr, paddr) = ProviderImpl::alloc_dma(PAGE_SIZE * 3);
            assert_eq!(paddr, 0, "the allocator was told to fail");
        })
    }
}
