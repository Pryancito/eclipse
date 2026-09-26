//! The kernel hooks this crate calls, doubled for the host test build.
//!
//! `zcore-drivers` reaches the kernel through a handful of `extern "C"` symbols
//! (`drivers_dma_alloc`, `drivers_phys_to_virt`, ...). They are defined by
//! `kernel-hal`, which is not linked into this crate's test binary, so a test
//! that reaches any of them needs a double.
//!
//! **There can be exactly one definition of each per binary**, so these cannot
//! live in whichever test module happened to need them first: a second module
//! that wants them gets `symbol ... is already defined` and no test. They lived
//! in `net/e1000e.rs`'s test module until `utils/dma.rs` needed them too, which
//! is how that came out. They are here now, once, for every module in the crate.
//!
//! By default every hook answers the way the e1000e's tests have always assumed:
//! allocation succeeds and is zeroed, deallocation is a no-op that only counts
//! (so a test can leak freely), and the cache-policy calls succeed. The switches
//! below make each one fail, which is the only way to reach the error paths --
//! and the error paths are where the interesting behaviour lives.

extern crate std;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::{alloc, alloc_zeroed, Layout};

/// `drivers_dma_alloc` answers 0 (out of memory).
pub static FAIL_ALLOC: AtomicBool = AtomicBool::new(false);
/// `drivers_dma_alloc` answers an address that is not page-aligned.
pub static MISALIGN_ALLOC: AtomicBool = AtomicBool::new(false);
/// `drivers_dma_alloc` hands back dirty memory instead of zeroes, which is what
/// recycled frames actually look like.
pub static POISON_ALLOC: AtomicBool = AtomicBool::new(false);
/// `drivers_dma_mark_uncached` fails.
pub static FAIL_MARK: AtomicBool = AtomicBool::new(false);
/// `drivers_dma_verify_uncached` fails -- the interesting one, because it fails
/// AFTER the pages have already been remapped.
pub static FAIL_VERIFY: AtomicBool = AtomicBool::new(false);

pub static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
pub static DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
pub static DEALLOC_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static MARK_CALLS: AtomicUsize = AtomicUsize::new(0);
pub static VERIFY_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Put every switch back to its default and zero every counter.
pub fn reset() {
    for flag in [
        &FAIL_ALLOC,
        &MISALIGN_ALLOC,
        &POISON_ALLOC,
        &FAIL_MARK,
        &FAIL_VERIFY,
    ] {
        flag.store(false, Ordering::SeqCst);
    }
    for counter in [
        &ALLOC_CALLS,
        &DEALLOC_CALLS,
        &DEALLOC_PAGES,
        &MARK_CALLS,
        &VERIFY_CALLS,
    ] {
        counter.store(0, Ordering::SeqCst);
    }
}

#[no_mangle]
extern "C" fn drivers_dma_alloc(pages: usize) -> usize {
    ALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
    if FAIL_ALLOC.load(Ordering::SeqCst) {
        return 0;
    }
    // One extra page of slack, so a deliberately misaligned answer still points
    // inside the allocation.
    let layout = Layout::from_size_align((pages + 1) * 4096, 4096).unwrap();
    let base = if POISON_ALLOC.load(Ordering::SeqCst) {
        let p = unsafe { alloc(layout) };
        unsafe { core::ptr::write_bytes(p, 0x5a, (pages + 1) * 4096) };
        p as usize
    } else {
        unsafe { alloc_zeroed(layout) as usize }
    };
    assert_ne!(base, 0, "the host allocator ran out");
    if MISALIGN_ALLOC.load(Ordering::SeqCst) {
        base + 8
    } else {
        base
    }
}

/// Counts and leaks. Freeing for real would pull the memory out from under the
/// tests that keep raw pointers into a ring on purpose, and every question these
/// tests ask about deallocation is about whether it was CALLED.
#[no_mangle]
extern "C" fn drivers_dma_dealloc(_paddr: usize, pages: usize) -> i32 {
    DEALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
    DEALLOC_PAGES.fetch_add(pages, Ordering::SeqCst);
    0
}

#[no_mangle]
extern "C" fn drivers_phys_to_virt(paddr: usize) -> usize {
    paddr
}

#[no_mangle]
extern "C" fn drivers_virt_to_phys(vaddr: usize) -> usize {
    vaddr
}

#[no_mangle]
extern "C" fn drivers_dma_mark_uncached(_paddr: usize, _pages: usize) -> i32 {
    MARK_CALLS.fetch_add(1, Ordering::SeqCst);
    if FAIL_MARK.load(Ordering::SeqCst) {
        -1
    } else {
        0
    }
}

#[no_mangle]
extern "C" fn drivers_dma_verify_uncached(_paddr: usize, _pages: usize) -> i32 {
    VERIFY_CALLS.fetch_add(1, Ordering::SeqCst);
    if FAIL_VERIFY.load(Ordering::SeqCst) {
        -1
    } else {
        0
    }
}

#[no_mangle]
extern "C" fn drivers_timer_now_as_micros() -> u64 {
    crate::nvme::nvme_queue::test_clock::now()
}

#[no_mangle]
extern "C" fn drivers_klog_emit(_priority: u8, _msg: *const u8, _len: usize) {}

#[no_mangle]
extern "C" fn drivers_intr_on() {}

#[no_mangle]
extern "C" fn drivers_intr_off() {}

#[no_mangle]
extern "C" fn drivers_intr_get() -> bool {
    false
}

#[no_mangle]
extern "C" fn drivers_wake_net_rx_waiters() {}

#[no_mangle]
extern "C" fn drivers_net_drain() {}
