mod consts;
mod ioapic;
mod lapic;

use self::consts::{X86_INT_BASE, X86_INT_LOCAL_APIC_BASE};
use self::ioapic::{IoApic, IoApicList};
use self::lapic::LocalApic;
use crate::prelude::{IrqHandler, IrqPolarity, IrqTriggerMode};
use crate::scheme::{IrqScheme, Scheme};
use crate::sync::Mutex;
use crate::utils::{run_irq_handler, IrqManager};
use crate::{DeviceError, DeviceResult, PhysAddr, VirtAddr};
use core::ops::Range;

const IOAPIC_IRQ_RANGE: Range<usize> = X86_INT_BASE..X86_INT_LOCAL_APIC_BASE;
const LAPIC_IRQ_RANGE: Range<usize> = 0..16;

type Phys2VirtFn = fn(paddr: PhysAddr) -> VirtAddr;

/// Advanced Programmable Interrupt Controller
pub struct Apic {
    ioapic_list: IoApicList,
    manager_ioapic: Mutex<IrqManager<256>>,
    manager_lapic: Mutex<IrqManager<16>>,
}

impl Apic {
    /// APIC id of the boot processor, as an MSI/IOAPIC physical destination
    /// (8 bits: the legacy, non-remapped format can address no more).
    pub fn bsp_apic_id() -> u8 {
        lapic::LocalApic::bsp_id()
    }

    /// Construct a new `Apic`.
    pub fn new(acpi_rsdp: usize, phys_to_virt: Phys2VirtFn) -> Self {
        Self {
            ioapic_list: IoApicList::new(acpi_rsdp, phys_to_virt),
            manager_ioapic: Mutex::new(IrqManager::new(IOAPIC_IRQ_RANGE)),
            manager_lapic: Mutex::new(IrqManager::new(LAPIC_IRQ_RANGE)),
        }
    }

    fn with_ioapic<F>(&self, gsi: u32, op: F) -> DeviceResult
    where
        F: FnOnce(&IoApic) -> DeviceResult,
    {
        if let Some(apic) = self.ioapic_list.find(gsi) {
            op(apic)
        } else {
            error!(
                "cannot find IOAPIC for global system interrupt number {}",
                gsi
            );
            Err(DeviceError::InvalidParam)
        }
    }

    pub fn send_init_ipi(dest: u32) {
        if LocalApic::is_initialized() {
            Self::local_apic().send_init_ipi(dest);
        }
    }

    pub fn send_sipi(vector: u8, dest: u32) {
        if LocalApic::is_initialized() {
            Self::local_apic().send_sipi(vector, dest);
        }
    }

    pub fn send_ipi_to(vector: u8, dest: u32) {
        if LocalApic::is_initialized() {
            Self::local_apic().send_ipi_to(vector, dest);
        }
    }

    /// [diag] Broadcast an NMI to every other CPU (reaches cores spinning with
    /// interrupts disabled).
    pub fn send_nmi_all_others() {
        if LocalApic::is_initialized() {
            Self::local_apic().send_nmi_all_others();
        }
    }

    pub fn init_local_apic_bsp(phys_to_virt: Phys2VirtFn) {
        unsafe { LocalApic::init_bsp(phys_to_virt) }
    }

    pub fn init_local_apic_ap() {
        if LocalApic::is_initialized() {
            unsafe { LocalApic::init_ap() }
        }
    }

    pub fn local_apic_ready() -> bool {
        LocalApic::is_initialized()
    }

    pub fn local_apic<'a>() -> &'a mut LocalApic {
        unsafe { LocalApic::get() }
    }

    pub fn register_local_apic_handler(&self, vector: usize, handler: IrqHandler) -> DeviceResult {
        // The spurious vector is the base itself, so its slot number is zero --
        // and zero is how `IrqManager::register_handler` is told to *allocate*
        // a slot instead of taking the one it was given. It would answer `Ok`
        // having put the handler somewhere else entirely, and `handle_irq`
        // returns before dispatching this vector in any case, because Intel
        // requires the spurious interrupt not to be acknowledged.
        if vector == consts::X86_INT_APIC_SPURIOUS {
            error!("the spurious interrupt vector takes no handler");
            return Err(DeviceError::InvalidParam);
        }
        if vector >= X86_INT_LOCAL_APIC_BASE {
            self.manager_lapic
                .lock()
                .register_handler(vector - X86_INT_LOCAL_APIC_BASE, handler)?;
            Ok(())
        } else {
            error!("invalid local APIC interrupt vector {}", vector);
            Err(DeviceError::InvalidParam)
        }
    }

    /// Is `gsi` a vector delivered straight to the local APIC by the device --
    /// an MSI -- rather than a line an I/O APIC routes?
    ///
    /// The two live in one numbering space and one handler table, so telling
    /// them apart is a matter of asking whether any I/O APIC claims the number.
    fn is_msi_vector(&self, gsi: usize) -> bool {
        self.ioapic_list.find(gsi as _).is_none() && IOAPIC_IRQ_RANGE.contains(&gsi)
    }
}

impl Scheme for Apic {
    fn name(&self) -> &str {
        "x86-apic"
    }

    fn handle_irq(&self, vector: usize) {
        // Intel: the spurious-interrupt vector must NOT write EOI.
        if vector != self::consts::X86_INT_APIC_SPURIOUS && LocalApic::is_initialized() {
            Self::local_apic().eoi();
        }
        // CRITICAL: look the handler up under the manager lock, then RELEASE the
        // lock before invoking it. `manager_lapic`/`manager_ioapic` are single
        // global Mutexes taken on every interrupt (the LAPIC timer fires on
        // every CPU at 250 Hz), and the handlers re-enter this very path:
        // `timer_tick` runs a timer callback that can touch the IRQ subsystem,
        // and the old code (call under the lock) then re-acquired this same
        // global lock on the SAME CPU — a self-deadlock that pinned the CPU (and
        // the timer heap lock) forever and froze every other core. This never
        // reproduced under 2 emulated CPUs; it only bites on real multi-core
        // hardware. Cloning the `Arc` out keeps the closure alive even if
        // another CPU unregisters it while it runs.
        if vector == self::consts::X86_INT_APIC_SPURIOUS {
            return;
        }
        let handler = if vector >= X86_INT_LOCAL_APIC_BASE {
            self.manager_lapic
                .lock()
                .get(vector - X86_INT_LOCAL_APIC_BASE)
        } else {
            self.manager_ioapic.lock().get(vector)
        };
        // The two gates this file worked out -- a sticky heap smash (device
        // IRQs from PS/2, the UART and xHCI were still calling through after a
        // null-[rsp] soft-smash with in_timer_callback=false) and a null or
        // non-kernel vtable -- now live in `run_irq_handler`, which every
        // architecture's dispatcher calls once it has let its table lock go.
        // The PLIC had only the second and the GIC-400 had neither.
        if let Err(DeviceError::InvalidParam) = run_irq_handler(vector, handler) {
            warn!("no registered handler for interrupt vector {}!", vector);
        }
    }
}

impl IrqScheme for Apic {
    fn is_valid_irq(&self, gsi: usize) -> bool {
        self.ioapic_list.find(gsi as _).is_some()
            || (X86_INT_BASE..X86_INT_LOCAL_APIC_BASE).contains(&gsi)
    }

    fn mask(&self, gsi: usize) -> DeviceResult {
        if let Some(apic) = self.ioapic_list.find(gsi as _) {
            apic.toggle(gsi as _, false);
            Ok(())
        } else if (X86_INT_BASE..X86_INT_LOCAL_APIC_BASE).contains(&gsi) {
            // MSI vector: effectively always unmasked at the APIC level,
            // managed at the PCI device level.
            Ok(())
        } else {
            error!(
                "cannot find IOAPIC for global system interrupt number {}",
                gsi
            );
            Err(DeviceError::InvalidParam)
        }
    }

    fn unmask(&self, gsi: usize) -> DeviceResult {
        if let Some(apic) = self.ioapic_list.find(gsi as _) {
            apic.toggle(gsi as _, true);
            Ok(())
        } else if (X86_INT_BASE..X86_INT_LOCAL_APIC_BASE).contains(&gsi) {
            // MSI vector
            Ok(())
        } else {
            error!(
                "cannot find IOAPIC for global system interrupt number {}",
                gsi
            );
            Err(DeviceError::InvalidParam)
        }
    }

    fn configure(&self, gsi: usize, tm: IrqTriggerMode, pol: IrqPolarity) -> DeviceResult {
        let gsi = gsi as u32;
        self.with_ioapic(gsi, |apic| {
            apic.configure(gsi, tm, pol, LocalApic::bsp_id());
            Ok(())
        })
    }

    fn register_handler(&self, gsi: usize, handler: IrqHandler) -> DeviceResult {
        let gsi32 = gsi as u32;
        if self.ioapic_list.find(gsi32).is_some() {
            // Interrupción gestionada por IOAPIC (IRQ legacy/PCI-INTx).
            self.with_ioapic(gsi32, |apic| {
                let vector = apic.get_vector(gsi32) as _;
                let vector = self
                    .manager_ioapic
                    .lock()
                    .register_handler(vector, handler)? as u8;
                apic.map_vector(gsi32, vector);
                Ok(())
            })
        } else {
            // No hay IOAPIC para este GSI → es un vector MSI.
            // El hardware escribe el vector directamente en el LAPIC,
            // así que registramos el handler en manager_ioapic.table[gsi]
            // sin pasar por el IOAPIC.
            //
            // The number has to be checked here rather than left to the
            // manager: `register_handler(0)` means *allocate one*, so a caller
            // that asked for vector zero -- which is not a vector at all, the
            // space starts at `X86_INT_BASE` -- was told `Ok` and given a
            // different vector, with nothing to tell it which.
            if !IOAPIC_IRQ_RANGE.contains(&gsi) {
                error!("interrupt vector {gsi} is outside the MSI vector range");
                return Err(DeviceError::InvalidParam);
            }
            self.manager_ioapic
                .lock()
                .register_handler(gsi, handler)
                .map(|_| ())
        }
    }

    fn unregister(&self, gsi: usize) -> DeviceResult {
        // `register_handler` takes both kinds and this took only one: an MSI
        // vector has no I/O APIC, so every attempt to release one answered
        // `InvalidParam` and left the handler in the table with its vector
        // still allocated. Both callers throw that answer away -- the NIC and
        // GPU teardown path (`msi_mask_and_unregister`) with `let _ =`, and
        // `zx_interrupt_destroy` by turning it into `NOT_FOUND` -- so the leak
        // was silent, and the stale handler stays reachable if the vector is
        // raised again.
        if self.is_msi_vector(gsi) {
            return self.manager_ioapic.lock().unregister_handler(gsi);
        }
        let gsi = gsi as u32;
        self.with_ioapic(gsi, |apic| {
            let vector = apic.get_vector(gsi) as _;
            self.manager_ioapic.lock().unregister_handler(vector)?;
            apic.map_vector(gsi, 0);
            Ok(())
        })
    }

    fn msi_alloc_block(&self, requested_irqs: usize) -> DeviceResult<Range<usize>> {
        // A device's MSI capability is where this count comes from, so neither
        // end of it is ours to trust. Zero rounded up to one and handed back a
        // vector to a caller that had asked for none; and a count with no next
        // power of two panicked the kernel in a debug build.
        if requested_irqs == 0 {
            return Err(DeviceError::InvalidParam);
        }
        let alloc_size = requested_irqs
            .checked_next_power_of_two()
            .ok_or(DeviceError::InvalidParam)?;
        let start = self.manager_ioapic.lock().alloc_block(alloc_size)?;
        Ok(start..start + alloc_size)
    }

    fn msi_free_block(&self, block: Range<usize>) -> DeviceResult {
        // Must mirror `msi_alloc_block`, which allocates from `manager_ioapic`;
        // freeing to `manager_lapic` leaked the IOAPIC vectors and corrupted the
        // 16-entry LAPIC allocator.
        //
        // And mirror its *shape*: `alloc_block` only ever hands out a run whose
        // length is a power of two, starting at a vector that is a multiple of
        // that length. The allocator underneath keeps no record of where a block
        // began, so a free of any other run is a free it performs -- the vectors
        // named go back into the pool, to be handed to a second device while the
        // first still has an MSI aimed at them, and the rest of the block can
        // never be returned at all.
        //
        // The shape is not proof. A single vector out of a larger block has the
        // shape of a block of one, and only a record of every block's extent
        // would tell those apart. What it does catch is every free of an odd
        // length or from a misaligned vector, which is the shape a caller that
        // lost track of its block produces.
        let len = block.len();
        if !len.is_power_of_two() || !block.start.is_multiple_of(len) {
            error!(
                "{:#x?} is not a block msi_alloc_block could have given",
                block
            );
            return Err(DeviceError::InvalidParam);
        }
        self.manager_ioapic.lock().free_block(block.start, len)
    }

    fn msi_register_handler(
        &self,
        block: Range<usize>,
        msi_id: usize,
        handler: IrqHandler,
    ) -> DeviceResult {
        if msi_id < block.len() {
            self.manager_ioapic
                .lock()
                .overwrite_handler(block.start + msi_id, handler)
        } else {
            Err(DeviceError::InvalidParam)
        }
    }

    fn apic_timer_enable(&self) {
        if LocalApic::is_initialized() {
            // SAFETY: this will called only once for every core
            Apic::local_apic().enable_timer();
        }
    }
}

/// Host tests for the x86 interrupt controller.
///
/// What they can reach, and what they cannot. Everything below runs against an
/// `Apic` whose I/O APIC list is empty, because the only way to build one is to
/// let the ACPI tables name its MMIO window, and an I/O APIC's window is an
/// index register and a data register: every one of its registers is read and
/// written through the same four bytes. A buffer of ordinary memory cannot
/// stand in for that -- reading back the redirection table gives whatever was
/// written last, whichever entry it belonged to -- so the branches that program
/// one stay uncovered here and only run on a machine that has one.
///
/// What is left is not the leftovers. An MSI never touches an I/O APIC at all:
/// the device writes the vector straight to the local APIC, which is how every
/// modern disk, network and graphics device on this architecture delivers its
/// interrupts. The vector space they are allocated from, the table their
/// handlers live in, and the dispatch that picks one when an interrupt arrives
/// are exactly what is measured below.
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, sync::Arc};
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// The first vector `msi_alloc_block` can hand out.
    const FIRST: usize = X86_INT_BASE;

    /// A page of zeros wherever the ACPI parser looks: the RSDP signature check
    /// fails there, so the list of I/O APICs comes back empty.
    fn no_acpi(_paddr: PhysAddr) -> VirtAddr {
        static PAGE: spin::Once<usize> = spin::Once::new();
        *PAGE.call_once(|| {
            let page: Box<[u8; 4096]> = Box::new([0; 4096]);
            Box::leak(page).as_ptr() as usize
        })
    }

    fn apic() -> Apic {
        Apic::new(0x1234, no_acpi)
    }

    /// A handler that counts its calls. The counter is leaked so the handler
    /// can outlive the test's frame, as a real one outlives its registration.
    fn counter() -> (IrqHandler, &'static AtomicUsize) {
        let hits: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
        (
            Arc::new(move || {
                hits.fetch_add(1, Ordering::SeqCst);
            }),
            hits,
        )
    }

    fn nop() -> IrqHandler {
        Arc::new(|| {})
    }

    // ---- taking a vector and giving it back -------------------------------

    #[test]
    fn a_vector_comes_back_when_its_handler_is_unregistered() {
        // The sequence a driver actually performs: `msi_register_and_unmask`
        // then `msi_mask_and_unregister`. The second half used to answer
        // `InvalidParam` for every MSI vector -- `unregister` only knew how to
        // release a line behind an I/O APIC -- and both callers discard the
        // answer, so the handler stayed in the table with its vector still
        // taken. The GPU boot path runs this on every attempt.
        let a = apic();
        assert_eq!(a.register_handler(FIRST + 0x10, nop()), Ok(()));
        assert_eq!(a.unmask(FIRST + 0x10), Ok(()));
        assert_eq!(a.mask(FIRST + 0x10), Ok(()));
        assert_eq!(a.unregister(FIRST + 0x10), Ok(()));
        // Taken back for real, not just forgotten: the same vector can be had
        // again, which it could not while the allocator still held it.
        assert_eq!(a.register_handler(FIRST + 0x10, nop()), Ok(()));
    }

    #[test]
    fn an_unregistered_vector_stops_being_dispatched() {
        let a = apic();
        let (handler, hits) = counter();
        a.register_handler(FIRST + 1, handler).unwrap();
        a.handle_irq(FIRST + 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        a.unregister(FIRST + 1).unwrap();
        a.handle_irq(FIRST + 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn giving_back_a_vector_nobody_took_is_an_error() {
        let a = apic();
        assert_eq!(a.unregister(FIRST + 2), Err(DeviceError::InvalidParam));
        // Twice over is the same thing: the second is not a free.
        a.register_handler(FIRST + 2, nop()).unwrap();
        assert_eq!(a.unregister(FIRST + 2), Ok(()));
        assert_eq!(a.unregister(FIRST + 2), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn the_same_vector_cannot_go_to_two_devices() {
        let a = apic();
        assert_eq!(a.register_handler(FIRST + 3, nop()), Ok(()));
        assert_eq!(
            a.register_handler(FIRST + 3, nop()),
            Err(DeviceError::AlreadyExists)
        );
    }

    #[test]
    fn a_number_outside_the_vector_space_is_not_a_vector() {
        let a = apic();
        // Zero is the one that mattered: it is how `IrqManager` is told to pick
        // a vector itself, so it used to answer `Ok` having registered the
        // handler somewhere else, with nothing to tell the caller where.
        assert_eq!(a.register_handler(0, nop()), Err(DeviceError::InvalidParam));
        assert_eq!(a.register_handler(5, nop()), Err(DeviceError::InvalidParam));
        assert_eq!(
            a.register_handler(X86_INT_LOCAL_APIC_BASE, nop()),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            a.register_handler(usize::MAX, nop()),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn without_an_io_apic_only_a_vector_is_a_valid_interrupt() {
        let a = apic();
        assert!(a.is_valid_irq(FIRST));
        assert!(a.is_valid_irq(X86_INT_LOCAL_APIC_BASE - 1));
        // The top of the range is the local APIC's own first vector, not a
        // device's.
        assert!(!a.is_valid_irq(X86_INT_LOCAL_APIC_BASE));
        // A legacy ISA line, which needs an I/O APIC to go anywhere.
        assert!(!a.is_valid_irq(5));
    }

    #[test]
    fn masking_an_msi_vector_is_the_devices_job_not_this_ones() {
        let a = apic();
        // Nothing to mask at this end -- the device holds the mask bit -- but
        // saying so is not the same as refusing.
        assert_eq!(a.mask(FIRST + 4), Ok(()));
        assert_eq!(a.unmask(FIRST + 4), Ok(()));
        // A number that is not a vector and has no I/O APIC is still refused.
        assert_eq!(a.mask(5), Err(DeviceError::InvalidParam));
        assert_eq!(a.unmask(5), Err(DeviceError::InvalidParam));
        assert_eq!(
            a.configure(5, IrqTriggerMode::Edge, IrqPolarity::ActiveHigh),
            Err(DeviceError::InvalidParam)
        );
    }

    // ---- blocks of vectors ------------------------------------------------

    #[test]
    fn a_block_is_rounded_up_to_a_power_of_two_and_aligned_to_itself() {
        let a = apic();
        let block = a.msi_alloc_block(3).unwrap();
        assert_eq!(block.len(), 4);
        assert_eq!(block.start % 4, 0);
        assert!(block.start >= FIRST);
        // MSI addresses a block by its base vector plus a small index, so the
        // vectors have to be next to each other.
        let second = a.msi_alloc_block(2).unwrap();
        assert_eq!(second.len(), 2);
        assert!(second.start >= block.end || second.end <= block.start);
    }

    #[test]
    fn a_block_of_no_vectors_is_not_a_block() {
        let a = apic();
        // Rounding zero up gave one, so a device whose capability asked for no
        // interrupts was handed a vector anyway.
        assert_eq!(a.msi_alloc_block(0), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn a_count_with_no_next_power_of_two_is_an_error_not_a_panic() {
        let a = apic();
        // Straight out of a device's MSI-X capability. `next_power_of_two`
        // overflows here, which is a panic in a debug kernel.
        assert_eq!(
            a.msi_alloc_block(usize::MAX),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            a.msi_alloc_block((usize::MAX >> 1) + 2),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_block_given_back_can_be_had_again() {
        let a = apic();
        let block = a.msi_alloc_block(4).unwrap();
        assert_eq!(a.msi_free_block(block.clone()), Ok(()));
        assert_eq!(a.msi_alloc_block(4).unwrap(), block);
    }

    #[test]
    fn a_free_that_is_not_the_shape_of_a_block_is_refused() {
        let a = apic();
        let block = a.msi_alloc_block(4).unwrap();
        // Three vectors is not a length any block has, and a pair starting one
        // past the base is not where any pair starts. Both used to go straight
        // through to the allocator.
        assert_eq!(
            a.msi_free_block(block.start..block.start + 3),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            a.msi_free_block(block.start + 1..block.start + 3),
            Err(DeviceError::InvalidParam)
        );
        // An empty range is not a block either, nor a backwards one, which
        // measures as empty.
        assert_eq!(
            a.msi_free_block(block.start..block.start),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            a.msi_free_block(block.end..block.start),
            Err(DeviceError::InvalidParam)
        );
        // And none of that disturbed the block itself.
        assert_eq!(a.msi_free_block(block.clone()), Ok(()));
        assert_eq!(a.msi_alloc_block(4).unwrap(), block);
    }

    #[test]
    fn a_run_that_was_never_one_block_is_refused() {
        // The length on its own is what refuses this one. Six vectors starting
        // at a multiple of six look aligned, and every one of them is taken, so
        // the allocator underneath would hand all six back -- two devices'
        // blocks released by a caller that owned one of them, and the vectors
        // of the other free to be given away while its MSIs still point here.
        let a = apic();
        let first = a.msi_alloc_block(4).unwrap();
        let second = a.msi_alloc_block(4).unwrap();
        let third = a.msi_alloc_block(4).unwrap();
        assert_eq!((second.start, third.start), (first.end, second.end));
        let run = second.start..second.start + 6;
        assert_eq!(run.start % run.len(), 0, "the run has to look aligned");
        assert_eq!(a.msi_free_block(run), Err(DeviceError::InvalidParam));
        // Both blocks came through it untouched.
        assert_eq!(a.msi_free_block(second.clone()), Ok(()));
        assert_eq!(a.msi_free_block(third), Ok(()));
        assert_eq!(a.msi_alloc_block(4).unwrap(), second);
    }

    #[test]
    fn one_vector_out_of_a_block_still_slips_through() {
        // Recorded, not endorsed. A single vector has the shape of a block of
        // one, so the check cannot see that this one came out of the middle of
        // a live block of four. Telling them apart needs a record of every
        // block's extent, which nothing here keeps. No caller does it today --
        // the PCI layer frees exactly the range it was handed -- and this test
        // is here so that whoever closes the hole finds out that they did.
        let a = apic();
        let block = a.msi_alloc_block(4).unwrap();
        assert_eq!(a.msi_free_block(block.start + 1..block.start + 2), Ok(()));
        // Here is what it costs, and why it is worth closing: that vector is
        // back in the pool while the device still has an MSI aimed at it, and
        // the block it came from can no longer be given back.
        assert_eq!(
            a.msi_free_block(block.clone()),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_block_that_was_never_allocated_is_not_freed() {
        let a = apic();
        assert_eq!(
            a.msi_free_block(FIRST + 0x40..FIRST + 0x42),
            Err(DeviceError::InvalidParam)
        );
        let block = a.msi_alloc_block(2).unwrap();
        a.msi_free_block(block.clone()).unwrap();
        assert_eq!(a.msi_free_block(block), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn the_vector_space_runs_out_rather_than_handing_one_out_twice() {
        let a = apic();
        let mut blocks = alloc::vec::Vec::new();
        while let Ok(block) = a.msi_alloc_block(8) {
            for other in &blocks {
                let other: &Range<usize> = other;
                assert!(block.start >= other.end || block.end <= other.start);
            }
            assert!(block.start >= FIRST && block.end <= X86_INT_LOCAL_APIC_BASE);
            blocks.push(block);
        }
        // 0x20..0xf0 is 208 vectors, so 26 blocks of eight.
        assert_eq!(blocks.len(), (X86_INT_LOCAL_APIC_BASE - FIRST) / 8);
    }

    #[test]
    fn an_msi_handler_answers_only_its_own_vector() {
        let a = apic();
        let block = a.msi_alloc_block(4).unwrap();
        let (handler, hits) = counter();
        assert_eq!(a.msi_register_handler(block.clone(), 2, handler), Ok(()));
        a.handle_irq(block.start + 2);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        for other in [block.start, block.start + 1, block.start + 3] {
            a.handle_irq(other);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_msi_id_past_the_end_of_its_block_is_an_error() {
        let a = apic();
        let block = a.msi_alloc_block(2).unwrap();
        let next = a.msi_alloc_block(2).unwrap();
        assert_eq!(next.start, block.end);
        assert_eq!(a.msi_register_handler(block.clone(), 1, nop()), Ok(()));
        // One past the end is the first vector of the block next door, which
        // belongs to another device. Nothing further down would object: that
        // vector is allocated, so overwriting its handler is a thing the table
        // will do. This guard is the whole of what stops it.
        let (handler, hits) = counter();
        assert_eq!(
            a.msi_register_handler(block, 2, handler),
            Err(DeviceError::InvalidParam)
        );
        a.handle_irq(next.start);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    // ---- dispatch ---------------------------------------------------------

    #[test]
    fn a_device_vector_and_a_local_apic_vector_do_not_share_a_slot() {
        // Two tables, one numbering space: the local APIC's vectors start where
        // the device vectors stop, and the slot inside the second table is the
        // vector minus that base. Confusing the two would deliver the timer's
        // interrupt to a device driver.
        let a = apic();
        let (device, device_hits) = counter();
        let (timer, timer_hits) = counter();
        a.register_handler(FIRST + 1, device).unwrap();
        a.register_local_apic_handler(consts::X86_INT_APIC_TIMER, timer)
            .unwrap();

        a.handle_irq(FIRST + 1);
        assert_eq!(
            (
                device_hits.load(Ordering::SeqCst),
                timer_hits.load(Ordering::SeqCst)
            ),
            (1, 0)
        );
        a.handle_irq(consts::X86_INT_APIC_TIMER);
        assert_eq!(
            (
                device_hits.load(Ordering::SeqCst),
                timer_hits.load(Ordering::SeqCst)
            ),
            (1, 1)
        );
    }

    #[test]
    fn the_spurious_vector_takes_no_handler() {
        let a = apic();
        // Its slot number is zero, which is how `IrqManager` is told to choose
        // a slot itself: the handler went somewhere else and the caller was
        // told `Ok`. And `handle_irq` does not dispatch this vector in any
        // case, because Intel requires it not to be acknowledged.
        assert_eq!(
            a.register_local_apic_handler(consts::X86_INT_APIC_SPURIOUS, nop()),
            Err(DeviceError::InvalidParam)
        );
        // The slot it would have taken is still free for the vector that owns it.
        let (timer, hits) = counter();
        a.register_local_apic_handler(consts::X86_INT_APIC_TIMER, timer)
            .unwrap();
        a.handle_irq(consts::X86_INT_APIC_SPURIOUS);
        a.handle_irq(consts::X86_INT_APIC_TIMER);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_vector_below_the_local_apic_base_is_not_a_local_apic_vector() {
        let a = apic();
        assert_eq!(
            a.register_local_apic_handler(0x10, nop()),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            a.register_local_apic_handler(X86_INT_LOCAL_APIC_BASE - 1, nop()),
            Err(DeviceError::InvalidParam)
        );
        // The table holds sixteen; past its end there is no slot to take.
        assert_eq!(
            a.register_local_apic_handler(X86_INT_LOCAL_APIC_BASE + 16, nop()),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn an_interrupt_with_no_handler_is_reported_rather_than_fatal() {
        // The vector comes from the hardware, not from us, so nothing here is
        // in a position to insist it is one we know.
        let a = apic();
        a.handle_irq(FIRST);
        a.handle_irq(consts::X86_INT_APIC_SPURIOUS);
        a.handle_irq(consts::X86_INT_APIC_ERROR);
        a.handle_irq(X86_INT_LOCAL_APIC_BASE + 15);
        a.handle_irq(0x1ff);
        a.handle_irq(usize::MAX);
    }

    #[test]
    fn the_local_apic_is_not_touched_when_it_was_never_brought_up() {
        // Every entry point that would write the local APIC's registers asks
        // first, which is what makes the rest of this module measurable on a
        // machine whose local APIC belongs to another operating system.
        assert!(!Apic::local_apic_ready());
        let a = apic();
        a.apic_timer_enable();
        Apic::send_ipi_to(0x40, 1);
        Apic::send_init_ipi(1);
        Apic::send_sipi(0x8, 1);
        Apic::send_nmi_all_others();
        a.handle_irq(FIRST);
    }
}
