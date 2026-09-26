use alloc::boxed::Box;
use core::ops::{Deref, DerefMut, Range};

use bitmap_allocator::{BitAlloc, BitAlloc16, BitAlloc256, BitAlloc4K, BitAlloc64K};

use crate::{DeviceError, DeviceResult};

pub trait IdAllocatorWrapper: Send + Sync {
    fn new(range: Range<usize>) -> Self
    where
        Self: Sized;
    // Only `IrqManager::register_handler`'s "allocate one" branch asks for an
    // id without naming it, and aarch64's GIC-400 does not use that door: see
    // the comment on it.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    fn alloc(&mut self) -> DeviceResult<usize>;
    fn alloc_fixed(&mut self, id: usize) -> DeviceResult;
    fn alloc_contiguous(&mut self, count: usize, align_log2: usize) -> DeviceResult<usize>;
    fn free(&mut self, start_id: usize, count: usize) -> DeviceResult;
    fn is_alloced(&self, id: usize) -> bool;
}

pub struct IdAllocator(Box<dyn IdAllocatorWrapper>);

impl IdAllocator {
    pub fn new(range: Range<usize>) -> DeviceResult<Self> {
        Ok(match range.end {
            0..=0x10 => Self(Box::new(IdAllocator16::new(range))),
            0x11..=0x100 => Self(Box::new(IdAllocator256::new(range))),
            0x101..=0x1000 => Self(Box::new(IdAllocator4K::new(range))),
            0x1001..=0x10000 => Self(Box::new(IdAllocator64K::new(range))),
            _ => {
                warn!("out of range in IdAllocator::new(): {:#x?}", range);
                return Err(DeviceError::InvalidParam);
            }
        })
    }
}

impl Deref for IdAllocator {
    type Target = Box<dyn IdAllocatorWrapper>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for IdAllocator {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

macro_rules! define_allocator {
    ($name: ident, $inner: ty) => {
        /// The bitmap plus the range it was built for. The range is not
        /// redundant: the bitmap's `test(id)` answers "is this id free", and
        /// an id that was never inserted is not free — so without the range,
        /// `is_alloced` answered **true** for every id outside the pool. That
        /// let `free()` `insert()` ids the allocator never owned, silently
        /// growing it past its own range, and made
        /// `IrqManager::unregister_handler` pass its `is_alloced` guard with
        /// an out-of-range vector and then panic indexing its IRQ table.
        struct $name($inner, Range<usize>);

        impl IdAllocatorWrapper for $name {
            fn new(range: Range<usize>) -> Self {
                let mut inner = <$inner>::DEFAULT;
                inner.insert(range.clone());
                Self(inner, range)
            }

            fn alloc(&mut self) -> DeviceResult<usize> {
                self.0.alloc().ok_or(DeviceError::NoResources)
            }

            fn alloc_fixed(&mut self, id: usize) -> DeviceResult {
                if !self.1.contains(&id) {
                    return Err(DeviceError::InvalidParam);
                }
                if self.0.test(id) {
                    self.0.remove(id..id + 1);
                    Ok(())
                } else {
                    Err(DeviceError::AlreadyExists)
                }
            }

            fn alloc_contiguous(&mut self, count: usize, align_log2: usize) -> DeviceResult<usize> {
                self.0
                    .alloc_contiguous(count, align_log2)
                    .ok_or(DeviceError::InvalidParam)
            }

            fn free(&mut self, start_id: usize, count: usize) -> DeviceResult {
                // The whole block must be inside the managed range, or the
                // `insert` below would hand the allocator ids it never owned.
                let end = start_id.checked_add(count);
                if end.is_none_or(|end| start_id < self.1.start || end > self.1.end) {
                    return Err(DeviceError::InvalidParam);
                }
                if count == 0 {
                    Err(DeviceError::InvalidParam)
                } else if count == 1 {
                    if !self.is_alloced(start_id) {
                        Err(DeviceError::InvalidParam)
                    } else {
                        self.0.dealloc(start_id);
                        Ok(())
                    }
                } else {
                    // Reject a double free: every id in the block must currently
                    // be allocated. Otherwise re-inserting an already-free id
                    // marks it free again and it can be handed out to two owners
                    // (e.g. the same IRQ vector to two devices).
                    if (start_id..start_id + count).any(|id| !self.is_alloced(id)) {
                        return Err(DeviceError::InvalidParam);
                    }
                    self.0.insert(start_id..start_id + count);
                    Ok(())
                }
            }

            fn is_alloced(&self, id: usize) -> bool {
                // An id outside the range was never handed out BY US, so the
                // honest answer is `false` -- not `!test(id)`, which is `true`
                // for every id the bitmap never had.
                self.1.contains(&id) && !self.0.test(id)
            }
        }
    };
}

define_allocator!(IdAllocator16, BitAlloc16);
define_allocator!(IdAllocator256, BitAlloc256);
define_allocator!(IdAllocator4K, BitAlloc4K);
define_allocator!(IdAllocator64K, BitAlloc64K);

#[cfg(test)]
mod tests {
    //! Host tests for the shared id allocator.
    //!
    //! This hands out IRQ vectors and device minor numbers. The failure that
    //! matters is not "allocation fails" — it is the same id being handed to
    //! two owners after a double free, which shows up much later as one
    //! device eating another's interrupts.

    use super::*;
    use crate::DeviceError;
    use alloc::vec::Vec;

    fn alloc_n(a: &mut IdAllocator, n: usize) -> Vec<usize> {
        (0..n).map(|_| a.alloc().unwrap()).collect()
    }

    #[test]
    fn allocated_ids_are_distinct_and_inside_the_range() {
        let mut a = IdAllocator::new(4..12).unwrap();
        let mut ids = alloc_n(&mut a, 8);
        ids.sort_unstable();
        assert_eq!(ids, (4..12).collect::<Vec<_>>());
        // The range is exhausted, and that is reported rather than wrapping.
        assert!(matches!(a.alloc(), Err(DeviceError::NoResources)));
    }

    #[test]
    fn a_freed_id_becomes_available_again() {
        let mut a = IdAllocator::new(0..8).unwrap();
        let ids = alloc_n(&mut a, 8);
        assert!(a.is_alloced(ids[3]));
        a.free(ids[3], 1).unwrap();
        assert!(!a.is_alloced(ids[3]));
        assert_eq!(a.alloc().unwrap(), ids[3]);
    }

    #[test]
    fn a_double_free_is_refused_so_one_id_never_gets_two_owners() {
        let mut a = IdAllocator::new(0..8).unwrap();
        let id = a.alloc().unwrap();
        a.free(id, 1).unwrap();
        // Second free of the same id: rejected. Accepting it would mark an
        // already-free id free again, and a later `alloc` could hand it out
        // while the first owner still believes it holds it.
        assert!(matches!(a.free(id, 1), Err(DeviceError::InvalidParam)));
        // Freeing an id that was never allocated is the same error.
        assert!(matches!(a.free(5, 1), Err(DeviceError::InvalidParam)));
        // A zero-length free is a caller bug, not a no-op.
        assert!(matches!(a.free(0, 0), Err(DeviceError::InvalidParam)));
    }

    #[test]
    fn a_block_free_requires_every_id_in_it_to_be_allocated() {
        let mut a = IdAllocator::new(0..16).unwrap();
        let base = a.alloc_contiguous(4, 0).unwrap();
        assert!((base..base + 4).all(|id| a.is_alloced(id)));
        // Punch a hole, then try to free the whole block: refused, because
        // re-inserting the hole would free it twice.
        a.free(base + 1, 1).unwrap();
        assert!(matches!(a.free(base, 4), Err(DeviceError::InvalidParam)));
        // With the hole filled again the block frees cleanly.
        a.alloc_fixed(base + 1).unwrap();
        a.free(base, 4).unwrap();
        assert!((base..base + 4).all(|id| !a.is_alloced(id)));
    }

    #[test]
    fn alloc_fixed_reserves_an_exact_id_once() {
        let mut a = IdAllocator::new(0..16).unwrap();
        a.alloc_fixed(7).unwrap();
        assert!(a.is_alloced(7));
        // Claiming it a second time must fail: that is how two drivers would
        // end up sharing one vector.
        assert!(matches!(a.alloc_fixed(7), Err(DeviceError::AlreadyExists)));
        // And the general allocator must not hand it out either.
        let ids = alloc_n(&mut a, 15);
        assert!(!ids.contains(&7));
    }

    #[test]
    fn alloc_contiguous_honours_its_alignment() {
        let mut a = IdAllocator::new(0..64).unwrap();
        // 8 ids aligned to 2^3: the base must be a multiple of 8.
        let base = a.alloc_contiguous(8, 3).unwrap();
        assert_eq!(base % 8, 0);
        assert!((base..base + 8).all(|id| a.is_alloced(id)));
        // The block is really reserved: nothing inside it comes back.
        let ids = alloc_n(&mut a, 56);
        assert!(ids.iter().all(|id| !(base..base + 8).contains(id)));
        assert!(matches!(a.alloc(), Err(DeviceError::NoResources)));
    }

    #[test]
    fn the_backing_bitmap_grows_with_the_range_and_stops_at_64k() {
        // Each size class must still allocate its top id.
        for end in [0x10usize, 0x100, 0x1000, 0x10000] {
            let mut a = IdAllocator::new(end - 1..end).unwrap();
            assert_eq!(a.alloc().unwrap(), end - 1);
            assert!(matches!(a.alloc(), Err(DeviceError::NoResources)));
        }
        // Past the largest bitmap the constructor refuses rather than
        // silently truncating the range.
        assert!(matches!(
            IdAllocator::new(0..0x10001),
            Err(DeviceError::InvalidParam)
        ));
    }

    #[test]
    fn an_empty_range_allocates_nothing() {
        let mut a = IdAllocator::new(0..0).unwrap();
        assert!(matches!(a.alloc(), Err(DeviceError::NoResources)));
        assert!(!a.is_alloced(0));
    }

    #[test]
    fn ids_outside_the_range_are_not_ours_to_hand_out_or_free() {
        let mut a = IdAllocator::new(4..8).unwrap();
        // Never allocated by us, whichever side of the range it sits on.
        assert!(!a.is_alloced(0));
        assert!(!a.is_alloced(3));
        assert!(!a.is_alloced(8));
        assert!(!a.is_alloced(usize::MAX));
        // Nor claimable or freeable. Accepting the free would `insert` it
        // into the bitmap and grow the pool past the range it was built for,
        // so the next `alloc` would return an id the caller never reserved.
        assert!(matches!(a.alloc_fixed(8), Err(DeviceError::InvalidParam)));
        assert!(matches!(a.free(8, 1), Err(DeviceError::InvalidParam)));
        assert!(matches!(a.free(3, 1), Err(DeviceError::InvalidParam)));
        // A block that starts inside the range but runs past its end.
        assert!(matches!(a.free(6, 4), Err(DeviceError::InvalidParam)));
        // And one whose end overflows rather than wrapping into range.
        assert!(matches!(
            a.free(usize::MAX, 2),
            Err(DeviceError::InvalidParam)
        ));
        // The pool is untouched by all of that.
        let mut ids = alloc_n(&mut a, 4);
        ids.sort_unstable();
        assert_eq!(ids, alloc::vec![4, 5, 6, 7]);
        assert!(matches!(a.alloc(), Err(DeviceError::NoResources)));
    }
}
