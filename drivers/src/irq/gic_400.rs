use crate::prelude::IrqHandler;
use crate::scheme::{IrqScheme, Scheme};
use crate::sync::Mutex;
use crate::utils::gic_banked::{bitmap_slot, BankedEnables};
use crate::utils::{run_irq_handler, IrqManager};
use crate::DeviceResult;
use core::sync::atomic::{AtomicU32, Ordering};

pub static GICC_SIZE: usize = 0x1000;
pub static GICD_SIZE: usize = 0x1000;
static GICD_CTLR: u32 = 0x000;
static GICD_TYPER: u32 = 0x004;
static GICD_ISENABLER: u32 = 0x100;
static GICD_ICENABLER: u32 = 0x180;
static GICD_IPRIORITY: u32 = 0x400;
static GICD_ITARGETSR: u32 = 0x800;
static GICD_ICFGR: u32 = 0xc00;
static GICC_IAR: u32 = 0x000c;
static GICC_EOIR: u32 = 0x0010;
static GICC_CTLR: u32 = 0x0000;
static GICC_PMR: u32 = 0x0004;

/// Interrupt IDs a GICv2 distributor can route. 1020..=1023 are reserved
/// (1023 is "spurious"), and `pending_irq` already turns everything from
/// 0x3fe up into `usize::MAX`.
const GIC_IRQ_COUNT: usize = 1024;
const GIC_IRQ_RANGE: core::ops::Range<usize> = 0..1020;

pub struct IntController {
    gicc: GicCpuIf,
    gicd: GicDistIf,
    manager: Mutex<IrqManager<GIC_IRQ_COUNT>>,
    /// The private interrupt ids something has asked to have enabled, so a
    /// core coming online can enable them in its own bank -- see
    /// `utils::gic_banked`. Plain atomic rather than the `Mutex`: it is read
    /// from `init_hart`, which runs on a core that has no scheduler yet.
    banked: AtomicU32,
}

struct GicDistIf {
    pub address: usize,
    pub ncpus: u32,
    pub nirqs: u32,
}

struct GicCpuIf {
    address: usize,
}

impl IntController {
    pub fn new(gicc_base: usize, gicd_base: usize) -> Self {
        Self {
            gicc: GicCpuIf { address: gicc_base },
            gicd: GicDistIf {
                address: gicd_base,
                ncpus: 0,
                nirqs: 0,
            },
            // Was 0..50, which is neither the distributor's range nor
            // anything else: SPIs start at ID 32, so a board with more than
            // eighteen of them had devices whose IRQ could not be registered
            // at all, while `is_valid_irq` below went on saying every ID was
            // fine. riscv's PLIC already sizes its table to the controller.
            manager: Mutex::new(IrqManager::new(GIC_IRQ_RANGE)),
            banked: AtomicU32::new(BankedEnables::new().mask()),
        }
    }

    fn init(&mut self) {
        unsafe {
            // Disable IRQ Distribution
            self.gicd.write(GICD_CTLR, 0);

            let typer = self.gicd.read(GICD_TYPER);
            self.gicd.ncpus = ((typer & (0x7 << 5)) >> 5) + 1;
            self.gicd.nirqs = ((typer & 0x1f) + 1) * 32;

            // Set all SPIs to level triggered
            for irq in (32..self.gicd.nirqs).step_by(16) {
                self.gicd.write(GICD_ICFGR + ((irq / 16) * 4), 0);
            }

            // Disable all SPIs
            for irq in (32..self.gicd.nirqs).step_by(32) {
                self.gicd
                    .write(GICD_ICENABLER + ((irq / 32) * 4), 0xffff_ffff);
            }

            // Affine all SPIs to CPU0 and set priorities for all IRQs
            for irq in 0..self.gicd.nirqs {
                if irq > 31 {
                    let ext_offset = GICD_ITARGETSR + (4 * (irq / 4));
                    let int_offset = irq % 4;
                    let mut val = self.gicd.read(ext_offset);
                    val |= 0b0000_0001 << (8 * int_offset);
                    self.gicd.write(ext_offset, val);
                }

                let ext_offset = GICD_IPRIORITY + (4 * (irq / 4));
                let int_offset = irq % 4;
                let mut val = self.gicd.read(ext_offset);
                val |= 0b0000_0000 << (8 * int_offset);
                self.gicd.write(ext_offset, val);
            }

            // Enable IRQ distribution. Global, and the only part of this
            // that is: everything above touched either an SPI register or
            // this core's own bank.
            self.gicd.write(GICD_CTLR, 0x1);
        }
        // This core's CPU interface and private enables, through the same
        // function every other core will call -- so the boot core cannot
        // quietly end up with a setup none of the others get.
        self.init_this_cpu();
    }

    pub fn irq_enable(&self, irq: u32) {
        // Record it first if it is private, because this write only reaches
        // the bank of whichever core is running: a core that comes up later
        // replays the recorded set into its own bank from `init_hart`.
        self.record_banked(irq, true);
        let (offset, bit) = bitmap_slot(GICD_ISENABLER, irq);
        unsafe {
            self.gicd.write(offset, bit);
        }
    }

    pub fn irq_disable(&self, irq: u32) {
        self.record_banked(irq, false);
        let (offset, bit) = bitmap_slot(GICD_ICENABLER, irq);
        unsafe {
            self.gicd.write(offset, bit);
        }
    }

    /// Add or remove a private id from the replay set, leaving a shared one
    /// alone: an SPI lives in the distributor's one copy, so whoever enabled
    /// it enabled it for the whole machine.
    fn record_banked(&self, irq: u32, enable: bool) {
        let mut set = BankedEnables::from_mask(self.banked.load(Ordering::Relaxed));
        let changed = if enable {
            set.record(irq)
        } else {
            set.forget(irq)
        };
        if changed {
            self.banked.store(set.mask(), Ordering::Relaxed);
        }
    }

    /// Bring *this* core's half of the GIC up: its CPU interface, and its own
    /// bank of the private interrupt enables.
    ///
    /// The boot core does this from `init`. Every other core has to do it for
    /// itself, and until it does, the generic timer PPI and the shootdown SGI
    /// are pending at a core the distributor will not forward them to.
    pub fn init_this_cpu(&self) {
        let banked = BankedEnables::from_mask(self.banked.load(Ordering::Relaxed));
        unsafe {
            // The banked enables before the interface goes up, so nothing is
            // forwarded to a core that is not yet listening.
            self.gicd.write(GICD_ISENABLER, banked.mask());
            self.gicc.write(GICC_CTLR, 1);
            self.gicc.write(GICC_PMR, 0xff);
        }
    }

    pub fn irq_eoi(&self, irq: u32) {
        unsafe {
            self.gicc.write(GICC_EOIR, irq);
        }
    }

    /// The acknowledged interrupt, as the raw GICC_IAR word, or `usize::MAX`
    /// when there is none.
    ///
    /// The whole word, not the interrupt id: for a **software-generated
    /// interrupt** (an SGI, ids 0..=15) bits [12:10] carry the id of the CPU
    /// that sent it, and GICv2 requires the same word to come back to GICC_EOIR
    /// -- an EOI that does not match the active interrupt is UNPREDICTABLE, and
    /// on this part leaves it active, so the CPU's running priority never drops
    /// and it takes no further interrupt of that priority or below. Ever.
    ///
    /// The spuriousness test therefore has to look at the id alone. Comparing
    /// the whole word against 0x3fe called every SGI from a CPU other than 0
    /// spurious -- IAR is `0x400` for CPU 1 -- and the caller then wrote
    /// `0xffff_ffff` to EOIR. SPIs are unaffected either way: their CPUID
    /// field is zero, so the word *is* the id.
    pub fn pending_irq(&self) -> usize {
        let iar = unsafe { self.gicc.read(GICC_IAR) as usize };
        if iar & 0x3ff >= 0x3fe {
            usize::MAX
        } else {
            iar
        }
    }
}

impl Scheme for IntController {
    fn name(&self) -> &str {
        "ARM Generic Interrupt Controller"
    }

    fn handle_irq(&self, irq_num: usize) {
        if irq_num != usize::MAX {
            // Dispatch on the interrupt id, acknowledge with the whole word:
            // see `pending_irq`. For everything but an SGI the two are equal,
            // so a caller passing a bare id (every caller but the aarch64 trap
            // entry) is unaffected.
            let id = irq_num & 0x3ff;
            // CRITICAL: clone the handler out under the lock, then RELEASE the
            // lock before running it. This used to be one call through the
            // guard, which meant the handler ran with `self.manager` -- the one
            // lock every core needs to register or look up a handler -- held by
            // its own stack frame. A handler that registers or unregisters an
            // interrupt from inside itself then deadlocked the core, and every
            // other core's next interrupt piled up behind it.
            //
            // The x86 path fixed exactly this and wrote down why
            // (`x86_apic::handle_irq`: "a self-deadlock that pinned the CPU
            // ... and froze every other core. This never reproduced under 2
            // emulated CPUs"); this driver and the PLIC were still doing it.
            // Holding it across the handler also serialised every core's
            // interrupt dispatch on one lock for the whole duration of every
            // handler.
            let handler = self.manager.lock().get(id);
            if run_irq_handler(id, handler).is_err() {
                trace!("no registered handler for IRQ {}", id);
            }
        }
        self.irq_eoi(irq_num as u32);
    }
}

impl IrqScheme for IntController {
    fn is_valid_irq(&self, irq_num: usize) -> bool {
        irq_num != usize::MAX
    }

    fn mask(&self, irq_num: usize) -> DeviceResult {
        self.irq_disable(irq_num as u32);
        Ok(())
    }

    fn unmask(&self, irq_num: usize) -> DeviceResult {
        self.irq_enable(irq_num as u32);
        Ok(())
    }

    fn register_handler(&self, irq_num: usize, handler: IrqHandler) -> DeviceResult {
        // Two things, both about telling the caller the truth.
        //
        // The error was `.ok()`-ed and `Ok(())` returned whatever happened, so a
        // driver could not tell a handler that was installed from one that was
        // not: an id outside the distributor's range, or one another driver
        // already owns, came back as success with nothing registered and the
        // device's interrupts arriving at no one.
        //
        // And `register_fixed_handler`, because this controller's ids start at
        // zero: SGI 0 is the TLB-shootdown interrupt, and the other entry point
        // reads a zero as "allocate one".
        self.manager
            .lock()
            .register_fixed_handler(irq_num, handler)
            .map(|_| ())
    }

    fn unregister(&self, _irq_num: usize) -> DeviceResult {
        todo!()
    }

    fn init_hart(&self) {
        self.init_this_cpu();
    }
}

impl GicDistIf {
    unsafe fn read(&self, reg: u32) -> u32 {
        core::ptr::read_volatile((self.address + reg as usize) as *const u32)
    }

    unsafe fn write(&self, reg: u32, value: u32) {
        core::ptr::write_volatile((self.address + reg as usize) as *mut u32, value);
    }
}

impl GicCpuIf {
    unsafe fn read(&self, reg: u32) -> u32 {
        core::ptr::read_volatile((self.address + reg as usize) as *const u32)
    }

    unsafe fn write(&self, reg: u32, value: u32) {
        core::ptr::write_volatile((self.address + reg as usize) as *mut u32, value);
    }
}

pub fn init(gicc_base: usize, gicd_base: usize) -> IntController {
    let mut controller = IntController::new(gicc_base, gicd_base);
    controller.init();
    controller
}

pub fn get_irq_num(gicc_base: usize, gicd_base: usize) -> usize {
    IntController::new(gicc_base, gicd_base).pending_irq()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;

    /// A distributor and a CPU interface backed by host memory. The driver
    /// reaches both through volatile `u32` accesses at an offset from a base
    /// address, and never asks the hardware a question whose answer it does
    /// not also accept as zero, so a zeroed buffer is a GIC that reports one
    /// CPU interface and 32 interrupt ids -- the private ones and no SPIs.
    struct FakeGic {
        gicd: Box<[u32; 1024]>,
        gicc: Box<[u32; 1024]>,
    }

    impl FakeGic {
        fn new() -> Self {
            Self {
                gicd: Box::new([0; 1024]),
                gicc: Box::new([0; 1024]),
            }
        }

        fn controller(&mut self) -> IntController {
            init(self.gicc.as_ptr() as usize, self.gicd.as_ptr() as usize)
        }

        /// What this core's bank of `GICD_ISENABLER0` holds.
        fn isenabler0(&self) -> u32 {
            self.gicd[GICD_ISENABLER as usize / 4]
        }

        fn gicc_reg(&self, reg: u32) -> u32 {
            self.gicc[reg as usize / 4]
        }

        /// Wipe the distributor's private-id words and the CPU interface, the
        /// way a core that has not been through `init_hart` sees them: the
        /// banked registers are per-core copies, and a core that just came out
        /// of reset has its own, at its own reset value.
        fn as_a_fresh_core_sees_it(&mut self) {
            self.gicd[GICD_ISENABLER as usize / 4] = 0;
            self.gicc.fill(0);
        }
    }

    /// The three ids the aarch64 port enables at boot.
    const TIMER_PPI: u32 = 30;
    const IPI_SGI: u32 = 0;
    const UART_SPI: u32 = 33;

    #[test]
    fn enabling_an_id_sets_its_own_bit_in_its_own_word() {
        // `GICD_ISENABLER` is a *set*-enable register: a written 1 enables, a
        // written 0 does nothing, so the hardware accumulates the one-bit
        // writes the driver sends it. The buffer behind this fake is plain
        // memory and keeps only the last write, so each id is checked as it
        // goes -- which is also what catches a bit computed from one id
        // landing in the word of another.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.irq_enable(TIMER_PPI);
        assert_eq!(gic.isenabler0(), 1 << TIMER_PPI);
        ctrl.irq_enable(IPI_SGI);
        assert_eq!(gic.isenabler0(), 1 << IPI_SGI);
        ctrl.irq_enable(UART_SPI);
        assert_eq!(
            gic.isenabler0(),
            1 << IPI_SGI,
            "an SPI does not touch the private word"
        );
        assert_eq!(
            gic.gicd[(GICD_ISENABLER as usize / 4) + 1],
            1 << 1,
            "it is id 33, so the second word, bit one"
        );
    }

    #[test]
    fn a_core_coming_online_enables_the_same_private_ids_for_itself() {
        // This is the bug: the boot core's writes above reached the boot
        // core's bank. A core that comes up later starts from its own, which
        // has nothing in it, and until it repeats those writes the distributor
        // forwards it neither the scheduler tick nor a shootdown IPI.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.irq_enable(TIMER_PPI);
        ctrl.irq_enable(UART_SPI);
        ctrl.irq_enable(IPI_SGI);

        gic.as_a_fresh_core_sees_it();
        assert_eq!(gic.isenabler0(), 0, "a fresh core has nothing enabled");

        ctrl.init_hart();
        assert_eq!(
            gic.isenabler0(),
            (1 << TIMER_PPI) | (1 << IPI_SGI),
            "and now it has exactly what the boot core enabled"
        );
    }

    #[test]
    fn a_core_coming_online_brings_up_its_cpu_interface_too() {
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        gic.as_a_fresh_core_sees_it();
        assert_eq!(gic.gicc_reg(GICC_CTLR), 0);

        ctrl.init_hart();
        assert_eq!(gic.gicc_reg(GICC_CTLR), 1, "the interface is listening");
        assert_eq!(
            gic.gicc_reg(GICC_PMR),
            0xff,
            "and its priority mask lets everything through"
        );
    }

    #[test]
    fn the_boot_core_goes_through_the_same_path_as_every_other_core() {
        // `init` used to do the CPU-interface writes itself, so the boot core
        // could be set up in a way no other core was. If these two diverge
        // again, the machine boots and the divergence is invisible until a
        // second core needs something the first one happened to have.
        let mut boot = FakeGic::new();
        let _ = boot.controller();

        let mut other = FakeGic::new();
        let ctrl = other.controller();
        other.as_a_fresh_core_sees_it();
        ctrl.init_hart();

        assert_eq!(boot.gicc_reg(GICC_CTLR), other.gicc_reg(GICC_CTLR));
        assert_eq!(boot.gicc_reg(GICC_PMR), other.gicc_reg(GICC_PMR));
    }

    #[test]
    fn an_id_that_gets_disabled_is_not_replayed_onto_the_next_core() {
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.irq_enable(TIMER_PPI);
        ctrl.irq_enable(IPI_SGI);
        ctrl.irq_disable(TIMER_PPI);

        gic.as_a_fresh_core_sees_it();
        ctrl.init_hart();
        assert_eq!(
            gic.isenabler0(),
            1 << IPI_SGI,
            "the one still wanted, and not the one turned off"
        );
    }

    #[test]
    fn a_core_that_wanted_nothing_private_writes_nothing() {
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.irq_enable(UART_SPI);

        gic.as_a_fresh_core_sees_it();
        ctrl.init_hart();
        assert_eq!(gic.isenabler0(), 0);
    }

    #[test]
    fn disabling_an_spi_leaves_the_private_set_alone() {
        // `GICD_ICENABLER` for a shared id is the distributor's one copy, so
        // it has nothing to do with what a core replays for itself.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.irq_enable(TIMER_PPI);
        ctrl.irq_disable(UART_SPI);

        gic.as_a_fresh_core_sees_it();
        ctrl.init_hart();
        assert_eq!(gic.isenabler0(), 1 << TIMER_PPI);
    }

    #[test]
    fn a_handler_runs_with_the_dispatch_table_unlocked() {
        // `self.manager` is the one lock every core needs to look up or register
        // a handler, and the handler used to run inside it: a handler that
        // registers or unregisters an interrupt from inside itself deadlocked
        // its own core, and every other core's next interrupt piled up behind
        // it. The x86 path fixed this and wrote down that it "never reproduced
        // under 2 emulated CPUs"; this driver was still doing it, and QEMU is
        // the only place the aarch64 port runs in CI.
        //
        // `try_lock`, not `lock`, so a regression fails this test in
        // microseconds instead of hanging the run.
        let mut gic = FakeGic::new();
        let ctrl = Arc::new(gic.controller());
        let got_in = Arc::new(AtomicUsize::new(0));
        let seen = got_in.clone();
        // `Weak`, because the closure lives inside the table it reaches back
        // into -- which is the situation being tested.
        let back = Arc::downgrade(&ctrl);
        ctrl.register_handler(
            UART_SPI as usize,
            Arc::new(move || {
                let ctrl = back.upgrade().unwrap();
                // Asked *before* the call below, and asserted on the spot: the
                // registration is what would spin here forever, so this is the
                // line that has to fail, not hang.
                assert!(
                    ctrl.manager.try_lock().is_some(),
                    "the handler is running with the dispatch table locked"
                );
                seen.fetch_add(1, Ordering::SeqCst);
                // And the real thing the lock was blocking: registering another
                // interrupt from inside a handler.
                ctrl.register_handler(40, Arc::new(|| {})).unwrap();
            }),
        )
        .unwrap();

        ctrl.handle_irq(UART_SPI as usize);
        assert_eq!(
            got_in.load(Ordering::SeqCst),
            1,
            "the handler ran with the dispatch table still locked"
        );
        assert!(
            ctrl.manager.lock().get(40).is_some(),
            "the handler could not register an interrupt"
        );
    }

    #[test]
    fn an_id_that_could_not_be_registered_is_not_reported_as_success() {
        // The error was thrown away and `Ok(())` returned whatever happened, so
        // a driver could not tell a handler that was installed from one that was
        // not: its device's interrupts then arrived at nobody, and the only
        // trace was a `trace!` line.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        // Past the top of what this distributor routes (1020..=1023 are
        // reserved).
        assert!(ctrl
            .register_handler(GIC_IRQ_RANGE.end, Arc::new(|| {}))
            .is_err());
        assert!(ctrl.register_handler(4096, Arc::new(|| {})).is_err());
        // And an id another driver already owns.
        ctrl.register_handler(UART_SPI as usize, Arc::new(|| {}))
            .unwrap();
        assert!(
            ctrl.register_handler(UART_SPI as usize, Arc::new(|| {}))
                .is_err(),
            "two drivers cannot share one line: the second handler replaces nothing"
        );
    }

    #[test]
    fn the_shootdown_sgi_is_claimed_by_name_and_only_once() {
        // Id 0 is SGI 0, the interrupt `send_ipi` rings for a TLB shootdown --
        // and `IrqManager::register_handler` reads a zero as "allocate one".
        // That returns 0 for the first caller, so the aarch64 port works by
        // luck; a second caller was handed id 1 and told `Ok`, which installs a
        // handler on an id the distributor is delivering to something else,
        // leaves the shootdown dispatched to nobody, and hangs the initiator in
        // a wait that has no timeout. That is the `[tlb-shootdown] slow ack
        // wait` this port already sees.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.register_handler(IPI_SGI as usize, Arc::new(|| {}))
            .unwrap();
        assert!(
            ctrl.register_handler(IPI_SGI as usize, Arc::new(|| {}))
                .is_err(),
            "SGI 0 was handed out twice"
        );
        assert!(
            ctrl.manager.lock().get(1).is_none(),
            "a handler for SGI 0 landed on SGI 1"
        );
    }

    #[test]
    fn an_sgi_dispatches_on_its_id_and_completes_with_the_whole_word() {
        // GICC_IAR carries the sending core's id in bits [12:10] for an SGI, and
        // GICv2 requires that same word back at GICC_EOIR: an EOI that does not
        // match the active interrupt leaves it active, so the core's running
        // priority never drops and it takes no further interrupt of that
        // priority or below. Ever.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        let (handler, hits) = counting();
        ctrl.register_handler(IPI_SGI as usize, handler).unwrap();

        // SGI 0 sent by core 1.
        let iar = 0x400 | IPI_SGI as usize;
        ctrl.handle_irq(iar);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an SGI from another core was dispatched on the whole word, not its id"
        );
        assert_eq!(
            gic.gicc_reg(GICC_EOIR),
            iar as u32,
            "the completion has to carry the CPUID field back"
        );
    }

    #[test]
    fn an_id_nobody_registered_is_still_completed() {
        // Skipping the EOI would leave the interrupt active at this core's CPU
        // interface for good. The dispatch says nothing about whether the
        // completion is owed.
        let mut gic = FakeGic::new();
        let ctrl = gic.controller();
        ctrl.handle_irq(UART_SPI as usize);
        assert_eq!(gic.gicc_reg(GICC_EOIR), UART_SPI);
    }

    /// A handler that counts its calls.
    fn counting() -> (IrqHandler, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        (
            Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
            hits,
        )
    }
}
