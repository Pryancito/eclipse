//! A physical-frame allocator for host tests.
//!
//! [`crate::KHANDLER`] is an `InitOnce`, so the whole test binary shares ONE
//! [`KernelHandler`]: whichever test calls `init_once_by` first decides what
//! every other test gets. Under `libos` the default handler answers
//! `frame_alloc` with `unimplemented!()`, so anything that allocates a page
//! table panics. This module is that one handler, and every test module that
//! needs `KHANDLER` installs it through [`install`].
//!
//! The frames come from the tail of the mock physical memory
//! (`libos::mem::PMEM_SIZE` is 1 GiB), well above the addresses the other
//! host tests hard-code, and are recycled through a free list so a long run
//! cannot exhaust the window.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::sync::Mutex;
use crate::{kernel_handler::KernelHandler, PhysAddr, PAGE_SIZE};

/// First frame handed out. The mock physical memory is 1 GiB; the half above
/// 512 MiB is not used by anything else in this crate's tests.
const ARENA_BASE: PhysAddr = 0x2000_0000;
/// One past the last frame.
const ARENA_END: PhysAddr = 0x4000_0000;

struct Arena {
    /// Next never-yet-used frame.
    next: PhysAddr,
    /// Frames handed back by `frame_dealloc`.
    free: Vec<PhysAddr>,
    /// Every frame currently out, and which test asked for it: a double free
    /// is caught here rather than as a mysterious aliased page table later,
    /// and [`live_frames`] can count one test's frames without seeing the
    /// rest of the binary's.
    live: BTreeMap<PhysAddr, u64>,
}

/// Identifies the test doing the allocating. The harness gives every `#[test]`
/// a thread of its own, so this is one test.
fn who() -> u64 {
    std::thread::current().id().as_u64().get()
}

static ARENA: Mutex<Arena> = Mutex::new(Arena {
    next: ARENA_BASE,
    free: Vec::new(),
    live: BTreeMap::new(),
});

pub(crate) struct TestKernelHandler;

impl KernelHandler for TestKernelHandler {
    fn frame_alloc(&self) -> Option<PhysAddr> {
        let mut arena = ARENA.lock();
        let paddr = match arena.free.pop() {
            Some(paddr) => paddr,
            None => {
                if arena.next >= ARENA_END {
                    return None;
                }
                let paddr = arena.next;
                arena.next += PAGE_SIZE;
                paddr
            }
        };
        assert!(
            arena.live.insert(paddr, who()).is_none(),
            "frame {:#x} handed out twice",
            paddr
        );
        Some(paddr)
    }

    fn frame_alloc_contiguous(&self, frame_count: usize, align_log2: usize) -> Option<PhysAddr> {
        let mut arena = ARENA.lock();
        // Only ever carved off the never-used end: the free list is not kept
        // sorted and tests that need a run of frames are rare.
        let align = 1usize << align_log2;
        let base = (arena.next + align - 1) & !(align - 1);
        if base + frame_count * PAGE_SIZE > ARENA_END {
            return None;
        }
        arena.next = base + frame_count * PAGE_SIZE;
        let me = who();
        for i in 0..frame_count {
            arena.live.insert(base + i * PAGE_SIZE, me);
        }
        Some(base)
    }

    fn frame_dealloc(&self, paddr: PhysAddr) {
        let mut arena = ARENA.lock();
        assert!(
            arena.live.remove(&paddr).is_some(),
            "frame {:#x} freed while not allocated",
            paddr
        );
        arena.free.push(paddr);
    }
}

/// Make [`crate::KHANDLER`] answer frame allocations. Idempotent, and safe to
/// call from every test: `init_once_by` is a `call_once`.
pub(crate) fn install() {
    crate::KHANDLER.init_once_by(&TestKernelHandler);
}

/// How many frames the *calling test* has outstanding right now. Tests use the
/// difference across an operation, never the absolute value.
///
/// Counted per thread on purpose. The arena is one static shared by the whole
/// test binary, so a global count meant a test measuring a delta of three
/// frames was also measuring whatever another test allocated in the same
/// instant: three flaky runs in eight, and rising with every test added to the
/// crate. `--test-threads=1` in CI is a property of the CI, not of the tests.
pub(crate) fn live_frames() -> usize {
    let me = who();
    ARENA
        .lock()
        .live
        .values()
        .filter(|owner| **owner == me)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arena is one static for the whole test binary, so `live_frames` has
    /// to answer for the calling test alone: a global count meant a test
    /// measuring a delta of three frames was also measuring whatever another
    /// test allocated in the same instant.
    #[test]
    fn a_test_counts_its_own_frames_and_nobody_else_s() {
        let mine = TestKernelHandler.frame_alloc().expect("arena exhausted");
        let before = live_frames();
        let theirs = std::thread::spawn(|| {
            let frame = TestKernelHandler.frame_alloc().expect("arena exhausted");
            assert_eq!(live_frames(), 1, "a fresh thread starts from its own zero");
            frame
        })
        .join()
        .unwrap();
        assert_eq!(live_frames(), before, "and its frame is not counted here");
        TestKernelHandler.frame_dealloc(theirs);
        TestKernelHandler.frame_dealloc(mine);
        assert_eq!(live_frames(), before - 1);
    }
}
