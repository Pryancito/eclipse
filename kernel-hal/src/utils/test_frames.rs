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

use alloc::collections::BTreeSet;
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
    /// Every frame currently owned by a caller, so a double free is caught
    /// here rather than as a mysterious aliased page table later.
    live: BTreeSet<PhysAddr>,
}

static ARENA: Mutex<Arena> = Mutex::new(Arena {
    next: ARENA_BASE,
    free: Vec::new(),
    live: BTreeSet::new(),
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
            arena.live.insert(paddr),
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
        for i in 0..frame_count {
            arena.live.insert(base + i * PAGE_SIZE);
        }
        Some(base)
    }

    fn frame_dealloc(&self, paddr: PhysAddr) {
        let mut arena = ARENA.lock();
        assert!(
            arena.live.remove(&paddr),
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

/// How many frames are outstanding right now. Tests use the difference across
/// an operation, never the absolute value: the arena is shared by the whole
/// binary and `--test-threads=1` is the only thing keeping it quiet.
pub(crate) fn live_frames() -> usize {
    ARENA.lock().live.len()
}
