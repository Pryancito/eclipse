use core::ops::Range;

use crate::io::{Io, Mmio};
use crate::prelude::IrqHandler;
use crate::scheme::{IrqScheme, Scheme};
use crate::sync::Mutex;
use crate::utils::{bounded_drain, run_irq_handler, DRAIN_BURST};
use crate::{utils::IrqManager, DeviceError, DeviceResult};
use cfg_if::cfg_if;
#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
use core::sync::atomic::{AtomicU8, Ordering};

const IRQ_RANGE: Range<usize> = 1..1024;

const PLIC_PRIORITY_BASE: usize = 0x0;
cfg_if! {
    if #[cfg(feature = "fu740")] {
        const PLIC_ENABLE_BASE: usize = 0x2000;
        const PLIC_CONTEXT_BASE: usize = 0x20_0000;
    } else {
        const PLIC_ENABLE_BASE: usize = 0x2080;
        const PLIC_CONTEXT_BASE: usize = 0x20_1000;
    }
}
const PLIC_CONTEXT_THRESHOLD: usize = 0x0;
const PLIC_CONTEXT_CLAIM: usize = 0x4 / core::mem::size_of::<u32>();

const PLIC_ENABLE_HART_OFFSET: usize = 0x100 / core::mem::size_of::<u32>();
// There is no per-hart stride for the priority array: a source's priority is one
// number for the whole machine. There used to be a `PLIC_PRIORITY_HART_OFFSET`
// here, and its only use was `set_threshold` reaching a *context* register with
// it -- right by accident, because the two numbers are equal. Gone, so the
// mistake cannot be made again.
const PLIC_CONTEXT_CLAIM_HART_OFFSET: usize = 0x2000 / core::mem::size_of::<u32>();

struct PlicUnlocked {
    priority_base: &'static mut Mmio<u32>,
    enable_base: &'static mut Mmio<u32>,
    context_base: &'static mut Mmio<u32>,
    manager: IrqManager<1024>,
}

pub struct Plic {
    inner: Mutex<PlicUnlocked>,
}

impl PlicUnlocked {
    /// Toggle irq enable on the current hart.
    fn toggle(&mut self, irq_num: usize, enable: bool) {
        // A real check, not a `debug_assert!`. Every one of these three methods
        // turns `irq_num` into an **address**, and the kernel is compiled in
        // release, where a `debug_assert!` is not there at all. The numbers do
        // not all come from us either: see `handle_irq`.
        if !IRQ_RANGE.contains(&irq_num) {
            return;
        }
        let hart_id = cpu_id() as usize;
        let mmio = self
            .enable_base
            .add(PLIC_ENABLE_HART_OFFSET * hart_id + irq_num / 32);

        let mask = 1 << (irq_num % 32);
        if enable {
            mmio.write(mmio.read() | mask);
        } else {
            mmio.write(mmio.read() & !mask);
        }
    }

    /// Ask the PLIC what type of interrupt is occurred on the current hart.
    fn pending_irq(&mut self) -> Option<usize> {
        let hart_id = cpu_id() as usize;
        let irq_num = self
            .context_base
            .add(PLIC_CONTEXT_CLAIM_HART_OFFSET * hart_id + PLIC_CONTEXT_CLAIM)
            .read() as usize;
        // For whoever mutates this next: reading a claim of zero as a pending
        // source survives every test, and it is equivalent in effect rather than
        // a gap. Source 0 has no handler, no priority the range check will let
        // anything write, and a completion of zero is what the hardware ignores;
        // the only difference is `DRAIN_BURST` laps of an empty loop.
        if irq_num == 0 {
            None
        } else {
            Some(irq_num)
        }
    }

    /// Tell the PLIC we've served this IRQ.
    ///
    /// The completion goes to a fixed address -- the hart's own claim register --
    /// so any value is safe to write, and a source the PLIC claimed has to be
    /// completed whatever its number, or the PLIC never stops offering it.
    fn eoi(&mut self, irq_num: usize) {
        let hart_id = cpu_id() as usize;
        self.context_base
            .add(PLIC_CONTEXT_CLAIM + PLIC_CONTEXT_CLAIM_HART_OFFSET * hart_id)
            .write(irq_num as _);
    }

    /// Set the priority for the irq_num. See [`toggle`](Self::toggle) on the
    /// guard.
    fn set_priority(&mut self, irq_num: usize, priority: u8) {
        if !IRQ_RANGE.contains(&irq_num) {
            return;
        }
        self.priority_base.add(irq_num).write(priority as _);
    }

    /// Set this hart's priority threshold: a source interrupts it only if its
    /// own priority is strictly greater.
    ///
    /// The stride is the **context** one, which is what a threshold lives in. It
    /// used to be spelled `PLIC_PRIORITY_HART_OFFSET`, a stride of the priority
    /// array that does not exist; the two happened to be the same number, so it
    /// worked, and changing either one would have sent every hart's threshold but
    /// hart 0's to the wrong address with nothing to say so.
    fn set_threshold(&mut self, threshold: u8) {
        let hart_id = cpu_id() as usize;
        self.context_base
            .add(PLIC_CONTEXT_CLAIM_HART_OFFSET * hart_id + PLIC_CONTEXT_THRESHOLD)
            .write(threshold as _);
    }

    fn init_hart(&mut self) {
        self.set_threshold(1);
    }
}

impl Plic {
    pub fn new(base: usize) -> Self {
        let mut inner = PlicUnlocked {
            priority_base: unsafe { Mmio::<u32>::from_base(base + PLIC_PRIORITY_BASE) },
            enable_base: unsafe { Mmio::<u32>::from_base(base + PLIC_ENABLE_BASE) },
            context_base: unsafe { Mmio::<u32>::from_base(base + PLIC_CONTEXT_BASE) },
            manager: IrqManager::new(IRQ_RANGE),
        };
        inner.init_hart();
        Self {
            inner: Mutex::new(inner),
        }
    }
}

impl Scheme for Plic {
    fn name(&self) -> &str {
        "riscv-plic"
    }

    fn handle_irq(&self, _unused: usize) {
        // Bounded: a level-triggered source whose handler does not quiesce it
        // is claimed again the instant it is acknowledged. The rest of the
        // queue arrives on the next interrupt, which is still asserted.
        let cut_short = bounded_drain(DRAIN_BURST, || {
            // CRITICAL: claim and look the handler up under the lock, then
            // RELEASE it before running the handler. `self.inner` is the one
            // lock every hart needs to service its own interrupts *and* the one
            // `mask`, `unmask` and `register_handler` take, so a handler that
            // touches its own line -- which is what quiescing a device looks
            // like -- deadlocked the hart while holding it, and every other
            // hart's next interrupt deadlocked behind it. Holding it across the
            // handler also serialised all harts on one lock for the whole
            // duration of every handler, which is not what a PLIC's per-hart
            // contexts are for.
            //
            // The x86 path fixed exactly this and wrote down why
            // (`x86_apic::handle_irq`: "a self-deadlock that pinned the CPU
            // ... and froze every other core. This never reproduced under 2
            // emulated CPUs"); this driver and the GIC-400 were still doing it.
            // `net::msi_mask` names the caller: "the storm self-limiter calls
            // this from inside the ISR".
            // Cloning the handler out keeps the closure alive even if another
            // hart unregisters it while it runs.
            let (irq_num, handler) = {
                let mut inner = self.inner.lock();
                match inner.pending_irq() {
                    Some(irq_num) => (irq_num, inner.manager.get(irq_num)),
                    None => return false,
                }
            };
            trace!("riscv plic handle irq: {}", irq_num);
            // The vtable and heap-smash gates live in `run_irq_handler`, once,
            // for all three architectures.
            // And for whoever mutates this next: widening the pattern to any
            // error survives the suite, because no test in this process can
            // reach the other two errors. Tripping either gate latches
            // `heap_smash_suspected`, which is per-CPU, has no reset, and on the
            // host resolves to slot 0 for the whole binary -- so the test that
            // tripped it would silently stop every later test's handler from
            // running. It is a real difference in the kernel: a handler that is
            // registered and healthy, refused because the heap is suspect, would
            // have its source dropped to priority 0 and be lost for the rest of
            // the boot.
            if let Err(DeviceError::InvalidParam) = run_irq_handler(irq_num, handler) {
                warn!("no registered handler for IRQ {}!", irq_num);
                // Silence it: priority 0 is "never interrupt". `irq_num` came
                // out of a device register, so `set_priority` checks it before
                // turning it into an address. Only for "nothing registered":
                // a handler that is registered and was refused for a suspect
                // vtable comes back as a different error, and dropping the
                // source's priority there would lose the device for good.
                self.inner.lock().set_priority(irq_num, 0);
            }
            self.inner.lock().eoi(irq_num);
            true
        });
        if cut_short {
            warn!(
                "[plic] {} interrupts in one pass; more pending",
                DRAIN_BURST
            );
        }
    }
}

impl IrqScheme for Plic {
    fn is_valid_irq(&self, irq_num: usize) -> bool {
        IRQ_RANGE.contains(&irq_num)
    }

    fn mask(&self, irq_num: usize) -> DeviceResult {
        if self.is_valid_irq(irq_num) {
            self.inner.lock().toggle(irq_num, false);
            Ok(())
        } else {
            Err(DeviceError::InvalidParam)
        }
    }

    fn unmask(&self, irq_num: usize) -> DeviceResult {
        if self.is_valid_irq(irq_num) {
            self.inner.lock().toggle(irq_num, true);
            Ok(())
        } else {
            Err(DeviceError::InvalidParam)
        }
    }

    fn register_handler(&self, irq_num: usize, handler: IrqHandler) -> DeviceResult {
        let mut inner = self.inner.lock();
        // The id the manager *assigned*, not the one that was asked for:
        // `register_handler(0, ..)` means "allocate one", and this used to raise
        // the priority of source 0 -- which is "no interrupt" and does not
        // exist, so the write was dropped by the range check -- while the
        // handler went in at the allocated id, whose priority stayed at 0.
        // Priority 0 is "never interrupt", so the device was registered,
        // enabled, and silent.
        let assigned = inner.manager.register_handler(irq_num, handler)?;
        // Above this hart's threshold, which `init_hart` sets to 1: a source
        // interrupts only if its priority is strictly greater.
        inner.set_priority(assigned, 7);
        Ok(())
    }

    fn unregister(&self, irq_num: usize) -> DeviceResult {
        let mut inner = self.inner.lock();
        inner.manager.unregister_handler(irq_num).map(|_| {
            // The mirror of `register_handler`, which raises the priority to 7.
            // Without this the source keeps its priority and, if it is still
            // enabled, keeps interrupting: every one of them then arrives at
            // `handle_irq` with no handler, which logs a line and silences it
            // there -- so the line was printed once per spurious interrupt on a
            // level-triggered source, from a driver having done nothing wrong.
            inner.set_priority(irq_num, 0);
        })
    }

    fn init_hart(&self) {
        self.inner.lock().init_hart();
    }
}

/// The hart this code is running on, which every per-hart register address in
/// this file is computed from.
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
fn cpu_id() -> u8 {
    let mut cpu_id;
    unsafe {
        core::arch::asm!("mv {0}, tp", out(reg) cpu_id);
    }
    cpu_id
}

/// The host copy of this driver (see `irq/mod.rs`) has no `tp` to read, so the
/// hart it is running on is whatever a test says it is.
///
/// This matters more here than it would for a GIC, whose per-core registers are
/// *banked* -- the same address, answering differently per core -- so a host
/// buffer stands in for one core and that is that. Every per-hart register of a
/// PLIC is a different **address**, computed from this number, so a test that
/// cannot choose the hart cannot see the arithmetic that picks the address.
#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
fn cpu_id() -> u8 {
    HOST_HART.load(Ordering::Relaxed)
}

#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
static HOST_HART: AtomicU8 = AtomicU8::new(0);

/// The PLIC had no tests, because it is `cfg`-ed to riscv and nothing that runs
/// tests builds it (see `irq/mod.rs`). What that hid is the two kinds of thing a
/// host buffer answers for *exactly*: an address computed from a hart number,
/// and a number the driver reads out of a device register and uses as one.
///
/// The claim register is the second. It is the PLIC telling the hart which
/// source to service, and the driver turned that number into an MMIO address
/// with a `debug_assert!` in front of it -- which the kernel, compiled in
/// release, does not have at all.
#[cfg(all(test, not(any(target_arch = "riscv32", target_arch = "riscv64"))))]
mod plic_tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicUsize;

    /// A PLIC's register file in host memory: 4 MiB covers the priority array,
    /// every hart's enable bits and the contexts of 255 harts.
    struct Fake {
        words: &'static mut [u32],
        base: usize,
    }

    impl Fake {
        fn new() -> Self {
            // `vec!`, not `Box::new([0; N])`: the array form is built on the
            // stack first in a debug build, and 4 MiB there is a stack
            // overflow before `main`.
            let words: &'static mut [u32] = alloc::vec![0u32; 0x10_0000].leak();
            let base = words.as_ptr() as usize;
            Self { words, base }
        }

        fn plic(&self) -> Plic {
            Plic::new(self.base)
        }

        fn priority(&self, irq: usize) -> u32 {
            self.words[PLIC_PRIORITY_BASE / 4 + irq]
        }

        fn set_priority(&mut self, irq: usize, value: u32) {
            self.words[PLIC_PRIORITY_BASE / 4 + irq] = value;
        }

        fn enables(&self, hart: usize, irq: usize) -> u32 {
            self.words[PLIC_ENABLE_BASE / 4 + PLIC_ENABLE_HART_OFFSET * hart + irq / 32]
        }

        fn threshold(&self, hart: usize) -> u32 {
            self.words[Self::context(hart) + PLIC_CONTEXT_THRESHOLD]
        }

        fn claim(&self, hart: usize) -> u32 {
            self.words[Self::context(hart) + PLIC_CONTEXT_CLAIM]
        }

        /// Offer `irq` to `hart`, the way the PLIC does: its claim register
        /// reads as the source id until something completes it.
        ///
        /// The one thing host memory cannot model: on a real PLIC *reading* the
        /// claim register claims the source and clears its pending bit, while
        /// *writing* the same address completes it. Here both are plain memory,
        /// and `eoi` writes the id back after the handler returns, so an offered
        /// source stays offered -- exactly like a level-triggered line whose
        /// handler never quiesced the device. Every pass therefore runs to
        /// `DRAIN_BURST`, and a test that wants to look at one interrupt gates
        /// its handler on the first call.
        fn offer(&mut self, hart: usize, irq: u32) {
            let at = Self::context(hart) + PLIC_CONTEXT_CLAIM;
            self.words[at] = irq;
        }

        /// A raw word of the register file, for checking that a write the
        /// driver should not have made was not made.
        fn word(&self, index: usize) -> u32 {
            self.words[index]
        }

        fn poke(&mut self, index: usize, value: u32) {
            self.words[index] = value;
        }

        fn context(hart: usize) -> usize {
            PLIC_CONTEXT_BASE / 4 + PLIC_CONTEXT_CLAIM_HART_OFFSET * hart
        }
    }

    /// `cpu_id()` reads one process-wide static on the host, so the tests that
    /// care which hart they are on take turns. (The CI runs this suite with
    /// `--test-threads=1` anyway; this makes it true without the flag.)
    static HART: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

    struct OnHart(#[allow(dead_code)] crate::sync::MutexGuard<'static, ()>);

    impl OnHart {
        fn new(hart: u8) -> Self {
            let guard = HART.lock();
            HOST_HART.store(hart, Ordering::Relaxed);
            Self(guard)
        }

        /// Move to another hart, the way the same driver is entered from a
        /// different core.
        fn now(&self, hart: u8) {
            HOST_HART.store(hart, Ordering::Relaxed);
        }
    }

    impl Drop for OnHart {
        fn drop(&mut self) {
            HOST_HART.store(0, Ordering::Relaxed);
        }
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

    #[test]
    fn a_handler_runs_with_the_plics_lock_released() {
        // THE one. `self.inner` is the single lock a hart needs to claim an
        // interrupt *and* the one `mask`, `unmask` and `register_handler` take,
        // and the handler used to run inside it. Quiescing a device is exactly
        // what masking your own line looks like, so the first driver to do it
        // deadlocked its own hart while holding the lock every other hart
        // needs -- and `net::msi_mask` says in so many words that this happens:
        // "the storm self-limiter calls this from inside the ISR".
        //
        // `try_lock`, not `lock`, so a regression fails this test in
        // microseconds instead of hanging the run.
        let _hart = OnHart::new(0);
        let mut fake = Fake::new();
        let plic = Arc::new(fake.plic());
        let calls = Arc::new(AtomicUsize::new(0));
        let nth = calls.clone();
        let back = Arc::downgrade(&plic);
        plic.register_handler(
            7,
            Arc::new(move || {
                // First call only: the line stays asserted in a memory fake
                // (see `offer`), so this handler is entered `DRAIN_BURST` times.
                if nth.fetch_add(1, Ordering::SeqCst) > 0 {
                    return;
                }
                let plic = back.upgrade().unwrap();
                // Asked *before* the call below, and asserted on the spot: the
                // mask is what would spin here forever, so this is the line
                // that has to fail, not hang.
                assert!(
                    plic.inner.try_lock().is_some(),
                    "the handler is running with the PLIC locked"
                );
                // And the real thing the lock was blocking: a handler that
                // quiesces its own line.
                plic.mask(7).unwrap();
            }),
        )
        .unwrap();

        fake.offer(0, 7);
        plic.handle_irq(0);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            DRAIN_BURST,
            "the handler was never entered"
        );
        assert_eq!(
            fake.enables(0, 7) & (1 << 7),
            0,
            "the handler's own mask did not reach the enable bits"
        );
    }

    #[test]
    fn a_number_the_claim_register_invents_is_never_turned_into_an_address() {
        // The claim register is a device register: firmware, a misprogrammed
        // PLIC or an unmapped window can put anything in it, and the driver
        // used it as a word index into the priority array behind a
        // `debug_assert!` -- which the kernel, built in release, does not
        // have. The two below are picked to land inside this fake's 4 MiB so
        // the test can see the write; the real one segfaults a host and
        // corrupts a board.
        let _hart = OnHart::new(0);
        let mut fake = Fake::new();
        for invented in [IRQ_RANGE.end, 0x8_0000, 0xF_FFFF] {
            let plic = fake.plic();
            fake.poke(invented, 0xA5A5_A5A5);
            fake.offer(0, invented as u32);
            plic.handle_irq(0);
            assert_eq!(
                fake.word(invented),
                0xA5A5_A5A5,
                "source {} was turned into an address",
                invented
            );
        }
    }

    #[test]
    fn a_source_with_no_handler_is_silenced_instead_of_interrupting_forever() {
        // A level-triggered source nobody claimed is asserted again the instant
        // it is completed. Priority 0 is "never interrupt", and it is the only
        // thing that stops the hart taking that interrupt for the rest of the
        // boot.
        let _hart = OnHart::new(0);
        let mut fake = Fake::new();
        let plic = fake.plic();
        fake.set_priority(5, 7);
        fake.offer(0, 5);
        plic.handle_irq(0);
        assert_eq!(fake.priority(5), 0, "the source was left interrupting");
    }

    #[test]
    fn registering_a_handler_raises_the_source_above_this_harts_threshold() {
        // A source interrupts only if its priority is strictly greater than the
        // hart's threshold, which `init_hart` sets to 1. A registered handler
        // that leaves the priority at 0 is a device that is enabled, has an
        // ISR, and never fires.
        let _hart = OnHart::new(0);
        let fake = Fake::new();
        let plic = fake.plic();
        plic.register_handler(9, Arc::new(|| {})).unwrap();
        assert!(
            fake.priority(9) > fake.threshold(0),
            "priority {} does not clear threshold {}",
            fake.priority(9),
            fake.threshold(0)
        );
    }

    #[test]
    fn asking_for_any_free_source_raises_the_priority_of_the_one_that_was_given() {
        // `register_handler(0, ..)` means "allocate one". The priority went to
        // source 0 -- which is "nothing pending", not a source, so the range
        // check dropped the write -- while the handler went in at the allocated
        // id with its priority still 0. Enabled, handled, and silent.
        let _hart = OnHart::new(0);
        let fake = Fake::new();
        let plic = fake.plic();
        plic.register_handler(0, Arc::new(|| {})).unwrap();
        let raised: Vec<usize> = IRQ_RANGE.filter(|&irq| fake.priority(irq) > 0).collect();
        assert_eq!(
            raised.len(),
            1,
            "exactly one source should have been raised, not {:?}",
            raised
        );
    }

    #[test]
    fn unregistering_drops_the_sources_priority_back_to_zero() {
        // The mirror of registering. Without it the source keeps priority 7 and,
        // if it is still enabled, keeps interrupting: every one of those arrives
        // with no handler and is logged, once per interrupt on a level-triggered
        // line, from a driver having done nothing wrong.
        let _hart = OnHart::new(0);
        let fake = Fake::new();
        let plic = fake.plic();
        plic.register_handler(9, Arc::new(|| {})).unwrap();
        assert_eq!(fake.priority(9), 7);
        plic.unregister(9).unwrap();
        assert_eq!(fake.priority(9), 0, "the source was left at priority 7");
    }

    #[test]
    fn each_hart_claims_and_completes_through_its_own_context() {
        // Every per-hart register of a PLIC is a different address, computed
        // from `cpu_id()`. Drop the hart from any one of those sums and hart 0's
        // registers answer for the whole machine.
        let _hart = OnHart::new(0);
        let mut fake = Fake::new();
        let plic = fake.plic();
        let (handler, hits) = counting();
        plic.register_handler(11, handler).unwrap();

        // Offered to hart 3 only. Hart 0 sees nothing.
        fake.offer(3, 11);
        plic.handle_irq(0);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "hart 0 serviced an interrupt offered to hart 3"
        );

        _hart.now(3);
        plic.handle_irq(0);
        assert_eq!(hits.load(Ordering::SeqCst), DRAIN_BURST);
        // The completion went to hart 3's claim register, which is where the
        // source was offered; no other hart's context was touched. Both
        // neighbours, because an address computed from the hart is as easy to
        // get wrong by one as to forget entirely.
        assert_eq!(fake.claim(3), 11);
        assert_eq!(fake.claim(2), 0, "the completion went to hart 2");
        assert_eq!(fake.claim(4), 0, "the completion went to hart 4");
        assert_eq!(fake.claim(0), 0, "the completion went to hart 0");
    }

    #[test]
    fn enabling_a_source_sets_one_bit_in_this_harts_own_enable_word() {
        let _hart = OnHart::new(2);
        let fake = Fake::new();
        let plic = fake.plic();
        plic.unmask(40).unwrap();
        assert_eq!(
            fake.enables(2, 40),
            1 << 8,
            "40 is bit 8 of the second word"
        );
        assert_eq!(fake.enables(0, 40), 0, "hart 0's bank was touched");
        plic.mask(40).unwrap();
        assert_eq!(fake.enables(2, 40), 0);
    }

    #[test]
    fn this_harts_threshold_goes_in_this_harts_context() {
        // `set_threshold` used the stride of the *priority* array. The two
        // happen to be the same number, so it worked; changing either would
        // have sent every hart's threshold but hart 0's to the wrong address
        // with nothing to say so. What a test can still catch is the hart
        // dropping out of the sum altogether.
        let _hart = OnHart::new(5);
        let fake = Fake::new();
        let _plic = fake.plic();
        assert_eq!(fake.threshold(5), 1, "hart 5's threshold was not set");
        assert_eq!(fake.threshold(0), 0, "hart 0's threshold was set instead");
    }

    #[test]
    fn a_source_that_never_quiesces_does_not_spin_the_hart_forever() {
        // A level-triggered line whose handler does not shut the device up is
        // offered again the instant it is completed. The pass is bounded and the
        // rest arrives on the next interrupt, which is still asserted.
        let _hart = OnHart::new(0);
        let mut fake = Fake::new();
        let plic = fake.plic();
        let (handler, hits) = counting();
        plic.register_handler(13, handler).unwrap();
        fake.offer(0, 13);
        plic.handle_irq(0);
        assert_eq!(hits.load(Ordering::SeqCst), DRAIN_BURST);
        // And it came back.
    }

    #[test]
    fn a_claim_of_zero_ends_the_pass_without_running_anything() {
        let _hart = OnHart::new(0);
        let fake = Fake::new();
        let plic = fake.plic();
        let (handler, hits) = counting();
        plic.register_handler(1, handler).unwrap();
        // Nothing offered: the claim register reads 0.
        plic.handle_irq(0);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_source_outside_the_controllers_range_is_refused_by_name() {
        let _hart = OnHart::new(0);
        let fake = Fake::new();
        let plic = fake.plic();
        // 0 is "nothing pending" and 1024 is past the top of the id space.
        assert!(!plic.is_valid_irq(0));
        assert!(!plic.is_valid_irq(IRQ_RANGE.end));
        assert!(plic.mask(IRQ_RANGE.end).is_err());
        assert!(plic.unmask(IRQ_RANGE.end).is_err());
        assert!(plic.is_valid_irq(IRQ_RANGE.end - 1));
        assert!(plic.unmask(IRQ_RANGE.end - 1).is_ok());
    }
}
