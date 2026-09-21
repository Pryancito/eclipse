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
    pub fn register_handler(&mut self, irq_num: usize, handler: IrqHandler) -> DeviceResult<usize> {
        info!("IRQ register handler {}", irq_num);
        let irq_num = if irq_num == 0 {
            // allocate a valid IRQ number
            self.allocator.alloc()?
        } else if self.irq_range.contains(&irq_num) {
            self.allocator.alloc_fixed(irq_num)?;
            irq_num
        } else {
            return Err(DeviceError::InvalidParam);
        };
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

    /// Look up and invoke the handler while holding the caller's lock. Still
    /// used by the riscv/aarch64 IRQ dispatchers; the x86 APIC path uses `get`
    /// instead to run the handler with the dispatch lock released (see
    /// `x86_apic::handle_irq`), so on x86 this is dead code.
    #[allow(dead_code)]
    pub fn handle(&self, irq_num: usize) -> DeviceResult {
        // Indexed, not `get`, this panicked the kernel on an IRQ number past
        // the end of the table -- and the number comes from the interrupt
        // controller, not from us. The GIC-400 hands out IDs up to 1021 while
        // its table held 50, so on aarch64 any SPI above 18 brought the
        // machine down with an out-of-bounds index, from nothing userspace
        // did. `get` right below does the same lookup safely; there is no
        // reason for the two to disagree.
        if let Some(Some(f)) = self.table.get(irq_num) {
            // Null OR non-kernel vtable (0x13446-class soft-smash). Same
            // gate as `x86_apic::handle_irq` / EventListener::trigger.
            if !super::fat_ptr::dyn_fat_ptr_live(f) {
                return Err(DeviceError::InvalidParam);
            }
            f();
            Ok(())
        } else {
            Err(DeviceError::InvalidParam)
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
    // Only the x86 APIC dispatch path clones handlers out before invoking
    // them; the other architectures' IRQ paths never call `get`, and under
    // `deny(warnings)` that dead code fails their whole bare-metal build.
    #[cfg_attr(
        not(any(target_arch = "x86", target_arch = "x86_64")),
        allow(dead_code)
    )]
    pub fn get(&self, irq_num: usize) -> Option<IrqHandler> {
        self.table.get(irq_num).and_then(|h| h.clone())
    }
}

/// `IrqManager` sits under every device in the system and had no tests.
///
/// The one that mattered: `handle` indexed the table where `get`, five lines
/// below, looks it up safely. The index comes from the interrupt controller —
/// the GIC-400 acknowledges IDs up to 1021 — so an interrupt the table was too
/// small for panicked the kernel outright, and nothing userspace did could
/// prevent it.
#[cfg(test)]
mod irq_manager_tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

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
                matches!(mgr.handle(past_the_end), Err(DeviceError::InvalidParam)),
                "IRQ {} should be refused",
                past_the_end
            );
        }
    }

    #[test]
    fn handle_and_get_answer_the_same_for_every_index() {
        // They are the same lookup written twice, and they disagreed: one
        // returned `None`, the other brought the machine down. Whatever else
        // changes, they have to keep agreeing.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, _) = counting();
        let irq = mgr.register_handler(5, handler).unwrap();
        assert_eq!(irq, 5);
        for i in 0..32usize {
            let registered = mgr.get(i).is_some();
            let handled = mgr.handle(i).is_ok();
            assert_eq!(registered, handled, "index {} disagrees", i);
        }
    }

    #[test]
    fn a_registered_handler_runs_and_an_empty_slot_does_not() {
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        mgr.register_handler(3, handler).unwrap();
        assert!(mgr.handle(3).is_ok());
        assert!(mgr.handle(3).is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // An in-range slot nobody claimed is an error, not a silent no-op:
        // the PLIC path uses it to mask an IRQ that keeps firing with no
        // handler, which is the difference between a warning and a live-lock.
        assert!(matches!(mgr.handle(4), Err(DeviceError::InvalidParam)));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_last_number_in_range_is_usable() {
        // Off by one at the top is how a bounds check gets written wrong in
        // the other direction, and it would silently drop one device's IRQ.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (handler, hits) = counting();
        assert_eq!(mgr.register_handler(15, handler).unwrap(), 15);
        assert!(mgr.handle(15).is_ok());
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
        assert!(mgr.handle(first).is_ok());
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
    fn the_same_fixed_irq_cannot_be_claimed_twice() {
        // Two drivers sharing a line would otherwise leave only the second
        // handler installed, and the first device would stop being serviced
        // with nothing in the log.
        let mut mgr = IrqManager::<16>::new(0..16);
        let (first, first_hits) = counting();
        let (second, second_hits) = counting();
        mgr.register_handler(9, first).unwrap();
        assert!(mgr.register_handler(9, second).is_err());
        mgr.handle(9).unwrap();
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
        assert!(matches!(mgr.handle(9), Err(DeviceError::InvalidParam)));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // Freeing the table slot but not the id (or the other way round)
        // would make the number unusable for the rest of the boot.
        let (again, again_hits) = counting();
        assert_eq!(mgr.register_handler(9, again).unwrap(), 9);
        mgr.handle(9).unwrap();
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
        mgr.handle(6).unwrap();
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
        assert!(matches!(mgr.handle(6), Err(DeviceError::InvalidParam)));
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
