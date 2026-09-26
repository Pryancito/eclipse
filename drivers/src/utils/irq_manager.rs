use super::IdAllocator;
use crate::{prelude::IrqHandler, DeviceError, DeviceResult};
use core::ops::Range;

pub struct IrqManager<const IRQ_COUNT: usize> {
    irq_range: Range<usize>,
    table: [Option<IrqHandler>; IRQ_COUNT],
    allocator: IdAllocator,
}

impl<const IRQ_COUNT: usize> IrqManager<IRQ_COUNT> {
    pub fn new(irq_range: Range<usize>) -> Self {
        assert!(irq_range.end <= IRQ_COUNT);
        const EMPTY_HANDLER: Option<IrqHandler> = None;
        let allocator = IdAllocator::new(irq_range.clone()).unwrap();
        Self {
            irq_range,
            table: [EMPTY_HANDLER; IRQ_COUNT],
            allocator,
        }
    }

    #[allow(unused)]
    pub fn alloc_block(&mut self, count: usize) -> DeviceResult<usize> {
        info!("IRQ alloc_block {}", count);
        // `count == 0` makes `leading_zeros()` 32 and the subtraction below
        // underflow: a panic in debug, and in release an alignment of about
        // 2^32 that no allocation can ever satisfy. A `debug_assert` does not
        // catch it in the build that ships. MSI-X tables come straight from a
        // device's capability, so the count is not ours to trust.
        // Zero is not a power of two, so one check covers both; and a count
        // that does not fit in a `u32` wraps in the cast below to something
        // small (or to zero), silently reserving the wrong number of vectors.
        if !count.is_power_of_two() || count > u32::MAX as usize {
            return Err(DeviceError::InvalidParam);
        }
        let align_log2 = 31 - (count as u32).leading_zeros();
        self.allocator.alloc_contiguous(count, align_log2 as _)
    }

    #[allow(unused)]
    pub fn free_block(&mut self, start: usize, count: usize) -> DeviceResult {
        info!("IRQ free_block {:#x?}", start..start + count);
        self.allocator.free(start, count)
    }

    /// Add a handler to IRQ table. if `irq_num == 0`, we need to allocate one.
    /// Returns the specified IRQ number or an allocated IRQ on success.
    ///
    /// **The number that comes back is the one to program the controller with**,
    /// and it is not always the one that was asked for. See
    /// [`register_fixed_handler`](Self::register_fixed_handler) for a caller
    /// that means id zero when it says zero.
    // aarch64's only interrupt controller is the GIC-400, whose ids start at
    // zero, so it asks by name through the door below and never through this
    // one; `deny(warnings)` fails that build over the unused method.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub fn register_handler(&mut self, irq_num: usize, handler: IrqHandler) -> DeviceResult<usize> {
        info!("IRQ register handler {}", irq_num);
        if irq_num == 0 {
            // allocate a valid IRQ number
            let irq_num = self.allocator.alloc()?;
            self.table[irq_num] = Some(handler);
            Ok(irq_num)
        } else {
            self.register_fixed_handler(irq_num, handler)
        }
    }

    /// Claim `irq_num` **by name**, where zero is the id zero and not a request
    /// to allocate one.
    ///
    /// A controller whose id space starts at zero has no other way to ask:
    /// GICv2 SGI 0 is the interrupt `send_ipi` rings for a TLB shootdown, and
    /// registering it through the door above means "give me any free id". That
    /// happens to return 0 on the boot core, because 0 is the lowest free id at
    /// the point the aarch64 port registers it -- so anything registered before
    /// it would move the shootdown handler to another id, leave every shootdown
    /// dispatched to nobody, and hang the initiator in a wait that has no
    /// timeout.
    pub fn register_fixed_handler(
        &mut self,
        irq_num: usize,
        handler: IrqHandler,
    ) -> DeviceResult<usize> {
        // For whoever mutates this next: the range check is **redundant** and
        // weakening either end of it leaves every test green. `IrqManager::new`
        // builds the allocator from this very range and `alloc_fixed` rejects an
        // id outside it with the same error, so the two cannot disagree. What it
        // does earn is the compiler's help: take it out altogether and
        // `irq_range` has no reader left, which `deny(warnings)` refuses to
        // build. Belt and braces, not a gap.
        if !self.irq_range.contains(&irq_num) {
            return Err(DeviceError::InvalidParam);
        }
        self.allocator.alloc_fixed(irq_num)?;
        self.table[irq_num] = Some(handler);
        Ok(irq_num)
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn unregister_handler(&mut self, irq_num: usize) -> DeviceResult {
        info!("IRQ unregister handler {}", irq_num);
        if !self.allocator.is_alloced(irq_num) {
            Err(DeviceError::InvalidParam)
        } else {
            self.allocator.free(irq_num, 1)?;
            self.table[irq_num] = None;
            Ok(())
        }
    }

    #[allow(unused)]
    pub fn overwrite_handler(&mut self, irq_num: usize, handler: IrqHandler) -> DeviceResult {
        info!("IRQ overwrite handle {}", irq_num);
        if !self.allocator.is_alloced(irq_num) {
            Err(DeviceError::InvalidParam)
        } else {
            self.table[irq_num] = Some(handler);
            Ok(())
        }
    }

    /// Clone the handler registered for `irq_num`, if any.
    ///
    /// Lets a dispatcher release the table lock BEFORE invoking the handler
    /// (the clone is a cheap `Arc` bump). Running a handler while the table lock
    /// is held is a deadlock hazard: the handler may re-enter the same IRQ path
    /// on the same CPU (the x86 timer does) and try to re-acquire this very
    /// lock. The returned `Arc` also keeps the closure alive if another CPU
    /// unregisters it while it runs.
    // Indexing (`self.table[irq_num]`) is what this replaced, and the index
    // comes from the interrupt controller, not from us: the GIC-400 hands out
    // ids up to 1021 while its table held 50, so on aarch64 any SPI above 18
    // brought the machine down out of bounds, from nothing userspace did.
    //
    // Every architecture's dispatcher goes through here, but a target with no
    // interrupt controller at all compiles none of them, and `deny(warnings)`
    // would fail that build over the unused method.
    #[cfg_attr(
        not(any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv32",
            target_arch = "riscv64"
        )),
        allow(dead_code)
    )]
    pub fn get(&self, irq_num: usize) -> Option<IrqHandler> {
        self.table.get(irq_num).and_then(|h| h.clone())
    }
}

/// Run the handler a dispatcher cloned out of its table with
/// [`IrqManager::get`], **after** releasing the table lock.
///
/// It takes the closure by value and cannot reach an `IrqManager`, which is the
/// point: there is nothing here to hold a lock on. The method this replaced
/// (`handle`, which looked up and invoked in one go) could only be called
/// through the guard, so using it *was* running the handler under the lock --
/// and that is a deadlock, not a slow path: the handler may re-enter the very
/// IRQ path that is dispatching it (masking its own line is what quiescing a
/// device looks like; the x86 timer callback touches the IRQ subsystem) and
/// block on a lock its own stack frame holds. It also serialised every CPU's
/// interrupt dispatch on one lock for the whole duration of every handler.
///
/// The three checks below were three copies that had drifted: x86 refused to
/// call through after a heap smash, the PLIC did not, and the GIC-400 checked
/// neither.
///
/// `Err(InvalidParam)` means nothing was registered; the caller decides what to
/// do about it, because they do not agree (the PLIC drops the source's priority
/// to zero so a level-triggered line stops re-interrupting, the others log).
#[cfg_attr(
    not(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv32",
        target_arch = "riscv64"
    )),
    allow(dead_code)
)]
pub fn run_irq_handler(irq_num: usize, handler: Option<IrqHandler>) -> DeviceResult {
    let Some(f) = handler else {
        return Err(DeviceError::InvalidParam);
    };
    // For whoever mutates this next: **neither gate below can be tested in
    // process**, and taking either out leaves the whole suite green. Both latch
    // `fat_ptr::heap_smash_suspected`, which is a per-CPU flag with no reset
    // that resolves to slot 0 on a hosted build -- so a test that tripped one
    // would stop every handler in every later test of the same binary, which is
    // exactly the failure the gate exists to cause on purpose in a kernel that
    // is already corrupt. `fat_ptr.rs` has no tests of its own either; that is
    // a vein, not a licence to delete these.
    //
    // Leaked rather than dropped, here and below: dropping the `Arc` runs its
    // destructor through the same vtable we have just refused to call.
    if super::fat_ptr::heap_smash_suspected() {
        core::mem::forget(f);
        return Err(DeviceError::NotSupported);
    }
    // Null or non-kernel vtable (0x13446-class soft-smash). Same gate as
    // `EventListener::trigger` and the deferred-job queue.
    if !super::fat_ptr::dyn_fat_ptr_live(&f) {
        core::mem::forget(f);
        warn!(
            "IRQ {}: handler fat-pointer is dead (heap smash?); skipping to \
             avoid null-range EXECUTE #PF",
            irq_num
        );
        return Err(DeviceError::NotSupported);
    }
    f();
    Ok(())
}

/// `IrqManager` sits under every device in the system and had no tests.
///
/// The one that mattered: `handle` indexed the table where `get` looks it up
/// safely. The index comes from the interrupt controller — the GIC-400
/// acknowledges IDs up to 1021 — so an interrupt the table was too small for
/// panicked the kernel outright, and nothing userspace did could prevent it.
#[cfg(test)]
mod irq_manager_tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// What a dispatcher does, in the order it has to do it in: look the
    /// handler up, let the table lock go, then run it. Spelled out here
    /// because it used to be one method (`handle`) that could only be called
    /// with the lock still held.
    fn dispatch<const N: usize>(mgr: &IrqManager<N>, irq_num: usize) -> DeviceResult {
        run_irq_handler(irq_num, mgr.get(irq_num))
    }

    /// A handler that counts its own calls.
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
    fn an_irq_past_the_end_of_the_table_is_refused_not_a_panic() {
        // This is the bug. The old `self.table[irq_num]` panicked here, and
        // both callers of `handle` pass a number straight from the interrupt
        // controller: the PLIC's claim register on riscv, the GIC's IAR on
        // aarch64.
        let mgr = IrqManager::<16>::new(0..16);
        for past_the_end in [16usize, 17, 50, 1021, usize::MAX] {
            assert!(
                matches!(dispatch(&mgr, past_the_end), Err(DeviceError::InvalidParam)),
                "IRQ {} should be refused",
                past_the_end
            );
        }
    }

    #[test]
    fn a_lookup_that_finds_nothing_and_one_that_finds_a_handler_are_the_only_two_answers() {
        // There used to be two lookups: `handle` indexed and `get` did not,
        // and they disagreed -- one returned `None`, the other brought the
        // machine down. There is one now, and every index in or out of the
        // table has to come back through it as `Some` or `None`, never a
        // panic.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, _) = counting();
        let irq = mgr.register_handler(5, handler).unwrap();
        assert_eq!(irq, 5);
        for i in 0..32usize {
            assert_eq!(
                mgr.get(i).is_some(),
                dispatch(&mgr, i).is_ok(),
                "index {} disagrees",
                i
            );
        }
    }

    #[test]
    fn the_two_dispatch_shapes_do_not_give_the_same_answer() {
        // Why `run_irq_handler` takes the closure by value instead of being a
        // method. A dispatcher holds one lock over the table that every CPU
        // needs to service its own interrupts, and a handler that re-enters
        // the IRQ path -- masking its own line, which is what quiescing a
        // device looks like -- blocks on a lock its own stack frame holds.
        // `handle` could only be called through the guard, so using it *was*
        // taking that risk; this one cannot reach the table at all.
        //
        // The handler asks with `try_lock`, not `lock`, so this test answers
        // the question in a few microseconds rather than hanging the run.
        let table = Arc::new(crate::sync::Mutex::new(IrqManager::<16>::new(0..16)));
        let got_in = Arc::new(AtomicUsize::new(0));
        {
            let seen = got_in.clone();
            // `Weak`, because the closure lives inside the very table it
            // reaches back into -- which is the whole situation being tested.
            let back = Arc::downgrade(&table);
            let handler: IrqHandler = Arc::new(move || {
                if let Some(table) = back.upgrade() {
                    if table.try_lock().is_some() {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            table.lock().register_handler(4, handler).unwrap();
        }

        // The old shape: look up and invoke through the guard.
        let guard = table.lock();
        let under_the_lock = guard.get(4).unwrap();
        under_the_lock();
        drop(guard);
        assert_eq!(
            got_in.load(Ordering::SeqCst),
            0,
            "a handler invoked through the guard should not have got the lock"
        );

        // The shape every dispatcher uses now: look up, release, run.
        let cloned_out = table.lock().get(4);
        run_irq_handler(4, cloned_out).unwrap();
        assert_eq!(
            got_in.load(Ordering::SeqCst),
            1,
            "the handler ran with the table still locked"
        );
    }

    #[test]
    fn nothing_registered_is_told_apart_from_a_handler_that_was_refused() {
        // The PLIC acts on the first (it drops the source's priority so a
        // level-triggered line stops re-interrupting) and must not act on the
        // second, where a handler is registered and healthy but the heap is
        // suspect: silencing the line there loses the device for good.
        assert!(matches!(
            run_irq_handler(1, None),
            Err(DeviceError::InvalidParam)
        ));
        let (handler, hits) = counting();
        assert!(run_irq_handler(1, Some(handler)).is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_registered_handler_runs_and_an_empty_slot_does_not() {
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        mgr.register_handler(3, handler).unwrap();
        assert!(dispatch(&mgr, 3).is_ok());
        assert!(dispatch(&mgr, 3).is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // An in-range slot nobody claimed is an error, not a silent no-op:
        // the PLIC path uses it to mask an IRQ that keeps firing with no
        // handler, which is the difference between a warning and a live-lock.
        assert!(matches!(dispatch(&mgr, 4), Err(DeviceError::InvalidParam)));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_last_number_in_range_is_usable() {
        // Off by one at the top is how a bounds check gets written wrong in
        // the other direction, and it would silently drop one device's IRQ.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        assert_eq!(mgr.register_handler(15, handler).unwrap(), 15);
        assert!(dispatch(&mgr, 15).is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Note for whoever mutates this next: `register_handler`'s own
    /// `irq_range.contains` check is **redundant**, and dropping either half
    /// of it leaves this test green. `IrqManager::new` builds the allocator
    /// from the very same range and `alloc_fixed` rejects an id outside it,
    /// so the two guards cannot disagree. It is belt and braces, not a gap.
    #[test]
    fn a_number_outside_the_range_cannot_be_registered() {
        let mut mgr = IrqManager::<64>::new(8..40);
        let (handler, _) = counting();
        assert!(matches!(
            mgr.register_handler(40, handler.clone()),
            Err(DeviceError::InvalidParam)
        ));
        // Below the range too, even though the table is big enough for it.
        assert!(matches!(
            mgr.register_handler(7, handler),
            Err(DeviceError::InvalidParam)
        ));
    }

    #[test]
    fn zero_means_allocate_one() {
        // `register_handler(0, ..)` is the "give me any free IRQ" call, which
        // means a range that contains 0 cannot ask for IRQ 0 by name: the
        // request is indistinguishable from "anything will do", and it
        // succeeds even when IRQ 0 is already taken. The GIC range starts at
        // 0, so this is worth knowing rather than discovering.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        let first = mgr.register_handler(0, handler).unwrap();
        assert!((0..16).contains(&first));
        assert!(dispatch(&mgr, first).is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // And the next allocation must not hand out the same number again.
        let (other, _) = counting();
        let second = mgr.register_handler(0, other).unwrap();
        assert_ne!(first, second);

        // The ambiguity itself: with IRQ 0 already claimed, asking for it by
        // name still succeeds, because it is read as "allocate".
        if first == 0 || second == 0 {
            let (third, _) = counting();
            assert!(mgr.register_handler(0, third).is_ok());
        }
    }

    #[test]
    fn asking_for_id_zero_by_name_is_not_a_request_to_allocate_one() {
        // The GIC-400's id space starts at zero and SGI 0 is the TLB-shootdown
        // interrupt, so "zero means allocate" leaves that one id unaskable: the
        // first caller gets it by luck (0 is the lowest free id) and a second is
        // handed id 1 and told it succeeded.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (first, first_hits) = counting();
        assert_eq!(mgr.register_fixed_handler(0, first).unwrap(), 0);

        let (second, second_hits) = counting();
        assert!(matches!(
            mgr.register_fixed_handler(0, second),
            Err(DeviceError::AlreadyExists)
        ));
        assert!(mgr.get(1).is_none(), "the second handler landed on id 1");
        dispatch(&mgr, 0).unwrap();
        assert_eq!(first_hits.load(Ordering::SeqCst), 1);
        assert_eq!(second_hits.load(Ordering::SeqCst), 0);

        // The other door still means "allocate one", and it must not hand out
        // the id already claimed above.
        let (third, _) = counting();
        assert_ne!(mgr.register_handler(0, third).unwrap(), 0);
    }

    #[test]
    fn a_fixed_id_outside_the_range_is_refused_at_either_end() {
        let mut mgr = IrqManager::<64>::new(8..40);
        let (handler, _) = counting();
        assert!(matches!(
            mgr.register_fixed_handler(7, handler.clone()),
            Err(DeviceError::InvalidParam)
        ));
        assert!(matches!(
            mgr.register_fixed_handler(40, handler.clone()),
            Err(DeviceError::InvalidParam)
        ));
        // And the table is bigger than the range, so the check is the range's,
        // not the table's.
        assert!(matches!(
            mgr.register_fixed_handler(63, handler),
            Err(DeviceError::InvalidParam)
        ));
    }

    #[test]
    fn the_same_fixed_irq_cannot_be_claimed_twice() {
        // Two drivers sharing a line would otherwise leave only the second
        // handler installed, and the first device would stop being serviced
        // with nothing in the log.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (first, first_hits) = counting();
        let (second, second_hits) = counting();
        mgr.register_handler(9, first).unwrap();
        assert!(mgr.register_handler(9, second).is_err());
        dispatch(&mgr, 9).unwrap();
        assert_eq!(first_hits.load(Ordering::SeqCst), 1);
        assert_eq!(second_hits.load(Ordering::SeqCst), 0);
    }

    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn unregistering_frees_both_the_slot_and_the_number() {
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        mgr.register_handler(9, handler).unwrap();
        mgr.unregister_handler(9).unwrap();
        assert!(matches!(dispatch(&mgr, 9), Err(DeviceError::InvalidParam)));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // Freeing the table slot but not the id (or the other way round)
        // would make the number unusable for the rest of the boot.
        let (again, again_hits) = counting();
        assert_eq!(mgr.register_handler(9, again).unwrap(), 9);
        dispatch(&mgr, 9).unwrap();
        assert_eq!(again_hits.load(Ordering::SeqCst), 1);
    }

    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn unregistering_what_was_never_registered_is_refused() {
        let mut mgr = IrqManager::<16>::new(0..16);
        assert!(matches!(
            mgr.unregister_handler(9),
            Err(DeviceError::InvalidParam)
        ));
        // Out of range is refused by the id allocator, not by an index panic.
        assert!(matches!(
            mgr.unregister_handler(9999),
            Err(DeviceError::InvalidParam)
        ));
    }

    #[test]
    fn overwriting_replaces_the_handler_without_releasing_the_number() {
        let mut mgr = IrqManager::<16>::new(0..16);
        let (first, first_hits) = counting();
        let (second, second_hits) = counting();
        mgr.register_handler(6, first).unwrap();
        mgr.overwrite_handler(6, second).unwrap();
        dispatch(&mgr, 6).unwrap();
        assert_eq!(first_hits.load(Ordering::SeqCst), 0);
        assert_eq!(second_hits.load(Ordering::SeqCst), 1);
        // The number stays taken, so nobody else can claim it behind us.
        let (third, _) = counting();
        assert!(mgr.register_handler(6, third).is_err());
    }

    #[test]
    fn overwriting_an_unclaimed_number_is_refused() {
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        assert!(matches!(
            mgr.overwrite_handler(6, handler),
            Err(DeviceError::InvalidParam)
        ));
        assert!(matches!(dispatch(&mgr, 6), Err(DeviceError::InvalidParam)));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_block_of_zero_is_refused_instead_of_underflowing() {
        // `31 - 0u32.leading_zeros()` is `31 - 32`: a panic in debug, and in
        // release an alignment of about 2^32 that nothing can satisfy. The
        // count comes from a device's MSI-X capability, so it is not ours.
        let mut mgr = IrqManager::<64>::new(0..64);
        assert!(matches!(mgr.alloc_block(0), Err(DeviceError::InvalidParam)));
    }

    #[test]
    fn a_block_too_big_for_the_cast_is_refused() {
        // `count as u32` wraps: 2^32 becomes 0, and 2^33 becomes 0 too, so
        // the alignment is computed from a number that is not the one asked
        // for. A device claiming a preposterous MSI-X count must be told no,
        // not quietly given a block of the wrong size.
        let mut mgr = IrqManager::<64>::new(0..64);
        for count in [1usize << 32, 1 << 33, 1 << 40] {
            assert!(
                matches!(mgr.alloc_block(count), Err(DeviceError::InvalidParam)),
                "{:#x} vectors should be refused",
                count
            );
        }
    }

    #[test]
    fn a_block_that_is_not_a_power_of_two_is_refused() {
        // MSI-X requires a power-of-two, naturally aligned block. The old
        // `debug_assert` said so and then did it anyway in release.
        let mut mgr = IrqManager::<64>::new(0..64);
        for count in [3usize, 5, 6, 7, 9] {
            assert!(
                matches!(mgr.alloc_block(count), Err(DeviceError::InvalidParam)),
                "{} is not a power of two",
                count
            );
        }
    }

    #[test]
    fn a_block_is_allocated_aligned_to_its_own_size() {
        // The range starts at 9 on purpose, for two reasons. Based at 0 the
        // first block lands on IRQ 0, and `register_handler(0, ..)` means
        // "allocate one" rather than "claim number 0", so the check below
        // would pass for the wrong reason — see `zero_means_allocate_one`.
        // And 9 is not a multiple of 4, so an allocator that ignored the
        // alignment would hand back 9 and be caught; a range based at 8 is
        // already aligned and would let that through.
        let mut mgr = IrqManager::<64>::new(9..64);
        let start = mgr.alloc_block(4).unwrap();
        assert_ne!(start, 0);
        assert_eq!(start % 4, 0, "block of 4 landed at {}", start);
        assert!(start + 4 <= 64);
        // MSI-X needs the base aligned to the block size because the vector
        // number is the base with the low bits substituted, not added.
        assert_eq!(start, 12, "first aligned block above 9 is 12");
        // And it is really taken: a fixed registration inside it must fail,
        // or an MSI-X vector and a legacy line end up on the same number.
        let (handler, _) = counting();
        assert!(mgr.register_handler(start, handler).is_err());
        assert!(mgr.register_handler(start + 3, counting().0).is_err());
        // Freeing it hands the numbers back.
        mgr.free_block(start, 4).unwrap();
        let (again, _) = counting();
        assert_eq!(mgr.register_handler(start, again).unwrap(), start);
    }
}
