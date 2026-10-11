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
use core::cell::Cell;
use core::sync::atomic::Ordering;
use std::alloc::{alloc, alloc_zeroed, Layout};

// ─── Per-thread, not process-wide ───────────────────────────────────────────
//
// These were `AtomicBool`/`AtomicUsize` statics, which made every switch and
// every counter shared by the WHOLE test binary. A test asserting that it took
// exactly four regions was asserting something about every other test that
// happened to be running: at `--test-threads=16` this suite failed about two
// runs in three, and the modules that use these hooks each kept a private
// `TURNSTILE` mutex that could not possibly help, since the tests perturbing the
// counters are in modules that never take any lock at all.
//
// A test's allocations happen on the thread running it, so the state belongs to
// that thread. Each one now sees its own switches and its own counts, whatever
// `--test-threads` says and whatever the rest of the binary is doing. The
// wrappers keep the `.load(Ordering)` / `.store(v, Ordering)` shape of the
// atomics they replace, so every call site reads as it did; the `Ordering` is
// accepted and ignored, because there is nothing to order against.

/// A `bool` that is private to the thread reading it, shaped like `AtomicBool`.
pub struct ThreadFlag(&'static std::thread::LocalKey<Cell<bool>>);

impl ThreadFlag {
    pub fn store(&self, value: bool, _: Ordering) {
        self.0.with(|c| c.set(value));
    }
    pub fn load(&self, _: Ordering) -> bool {
        self.0.with(|c| c.get())
    }
}

/// A `usize` that is private to the thread reading it, shaped like
/// `AtomicUsize`.
pub struct ThreadCount(&'static std::thread::LocalKey<Cell<usize>>);

impl ThreadCount {
    pub fn store(&self, value: usize, _: Ordering) {
        self.0.with(|c| c.set(value));
    }
    pub fn load(&self, _: Ordering) -> usize {
        self.0.with(|c| c.get())
    }
    /// Returns the value from *before* the add, like `AtomicUsize::fetch_add`.
    pub fn fetch_add(&self, delta: usize, _: Ordering) -> usize {
        self.0.with(|c| {
            let before = c.get();
            c.set(before + delta);
            before
        })
    }
}

std::thread_local! {
    static FAIL_ALLOC_CELL: Cell<bool> = const { Cell::new(false) };
    static MISALIGN_ALLOC_CELL: Cell<bool> = const { Cell::new(false) };
    static POISON_ALLOC_CELL: Cell<bool> = const { Cell::new(false) };
    static FAIL_MARK_CELL: Cell<bool> = const { Cell::new(false) };
    static FAIL_VERIFY_CELL: Cell<bool> = const { Cell::new(false) };
    static FAIL_ALLOC_AFTER_CELL: Cell<usize> = const { Cell::new(usize::MAX) };
    static ALLOC_CALLS_CELL: Cell<usize> = const { Cell::new(0) };
    static ALLOC_PAGES_CELL: Cell<usize> = const { Cell::new(0) };
    static DEALLOC_CALLS_CELL: Cell<usize> = const { Cell::new(0) };
    static DEALLOC_PAGES_CELL: Cell<usize> = const { Cell::new(0) };
    static MARK_CALLS_CELL: Cell<usize> = const { Cell::new(0) };
    static VERIFY_CALLS_CELL: Cell<usize> = const { Cell::new(0) };
    static WAKE_CALLS_CELL: Cell<usize> = const { Cell::new(0) };
}

/// `drivers_dma_alloc` answers 0 (out of memory).
pub static FAIL_ALLOC: ThreadFlag = ThreadFlag(&FAIL_ALLOC_CELL);
/// `drivers_dma_alloc` answers an address that is not page-aligned.
pub static MISALIGN_ALLOC: ThreadFlag = ThreadFlag(&MISALIGN_ALLOC_CELL);
/// `drivers_dma_alloc` hands back dirty memory instead of zeroes, which is what
/// recycled frames actually look like.
pub static POISON_ALLOC: ThreadFlag = ThreadFlag(&POISON_ALLOC_CELL);
/// `drivers_dma_mark_uncached` fails.
pub static FAIL_MARK: ThreadFlag = ThreadFlag(&FAIL_MARK_CELL);
/// `drivers_dma_verify_uncached` fails -- the interesting one, because it fails
/// AFTER the pages have already been remapped.
pub static FAIL_VERIFY: ThreadFlag = ThreadFlag(&FAIL_VERIFY_CELL);

/// `drivers_dma_alloc` answers 0 once this many calls have already been made,
/// so a caller that takes several regions can be failed on the *third* one --
/// which is the only way to reach the path that gives the first two back.
/// `usize::MAX` (the default) never fails.
pub static FAIL_ALLOC_AFTER: ThreadCount = ThreadCount(&FAIL_ALLOC_AFTER_CELL);

pub static ALLOC_CALLS: ThreadCount = ThreadCount(&ALLOC_CALLS_CELL);
/// Pages asked for, summed. `ALLOC_CALLS` counts the calls; this is what they
/// asked for, which is where a byte length rounded the wrong way shows up.
pub static ALLOC_PAGES: ThreadCount = ThreadCount(&ALLOC_PAGES_CELL);
pub static DEALLOC_CALLS: ThreadCount = ThreadCount(&DEALLOC_CALLS_CELL);
pub static DEALLOC_PAGES: ThreadCount = ThreadCount(&DEALLOC_PAGES_CELL);
pub static MARK_CALLS: ThreadCount = ThreadCount(&MARK_CALLS_CELL);
pub static VERIFY_CALLS: ThreadCount = ThreadCount(&VERIFY_CALLS_CELL);
/// How many times the kernel was asked to wake the tasks waiting for RX
/// data. A driver that polls and does not wake leaves a blocked reader on
/// the fallback park timer, which is the slowest way for a frame to arrive
/// and looks exactly like a network that is merely slow.
pub static WAKE_CALLS: ThreadCount = ThreadCount(&WAKE_CALLS_CELL);

/// Run `body` with every switch and counter of *this thread* reset on both
/// sides of it.
///
/// There is no lock, and that is the point. Each module that needed this used to
/// keep its own `static TURNSTILE` inside its own test module, and three private
/// mutexes over one set of process-wide counters guard nothing: the tests that
/// perturbed the counters were in modules holding a different lock, or no lock
/// at all. At `--test-threads=16` this suite failed about two runs in three,
/// and CI passing `--test-threads=1` is what hid it -- while all three copies
/// carried a comment claiming they did not rely on that.
///
/// The state is per-thread now, so the isolation is real and costs no
/// serialisation. One definition, here, beside what it resets.
pub fn alone_with_the_allocator<R>(body: impl FnOnce() -> R) -> R {
    reset();
    let out = body();
    reset();
    out
}

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
        &ALLOC_PAGES,
        &DEALLOC_CALLS,
        &DEALLOC_PAGES,
        &MARK_CALLS,
        &VERIFY_CALLS,
        &WAKE_CALLS,
    ] {
        counter.store(0, Ordering::SeqCst);
    }
    FAIL_ALLOC_AFTER.store(usize::MAX, Ordering::SeqCst);
}

#[no_mangle]
extern "C" fn drivers_dma_alloc(pages: usize) -> usize {
    let before = ALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
    ALLOC_PAGES.fetch_add(pages, Ordering::SeqCst);
    if FAIL_ALLOC.load(Ordering::SeqCst) || before >= FAIL_ALLOC_AFTER.load(Ordering::SeqCst) {
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
extern "C" fn drivers_wake_net_rx_waiters() {
    WAKE_CALLS.fetch_add(1, Ordering::SeqCst);
}

#[no_mangle]
extern "C" fn drivers_net_drain() {}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn drivers_dma_alloc(pages: usize) -> usize;
        fn drivers_dma_dealloc(paddr: usize, pages: usize) -> i32;
        fn drivers_dma_mark_uncached(paddr: usize, pages: usize) -> i32;
        fn drivers_dma_verify_uncached(paddr: usize, pages: usize) -> i32;
        fn drivers_phys_to_virt(paddr: usize) -> usize;
        fn drivers_virt_to_phys(vaddr: usize) -> usize;
        fn drivers_wake_net_rx_waiters();
    }

    /// Every switch here is the only way a test elsewhere in the crate can
    /// reach an error path, and every counter is the only evidence those
    /// tests have that a call happened at all. A switch that silently stopped
    /// working would not fail anything: the tests using it would simply stop
    /// asserting what they claim to, and pass. So the doubles get their own
    /// tests.
    ///
    /// Allocation succeeds and is ZEROED by default, which is what every
    /// caller in the crate assumes about a fresh DMA region.
    #[test]
    fn allocation_succeeds_zeroed_and_page_aligned_by_default() {
        alone_with_the_allocator(|| {
            let base = unsafe { drivers_dma_alloc(2) };
            assert_ne!(base, 0);
            assert_eq!(base % 4096, 0, "the default answer is not page-aligned");
            let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, 2 * 4096) };
            assert!(bytes.iter().all(|b| *b == 0), "the region came back dirty");
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(ALLOC_PAGES.load(Ordering::SeqCst), 2);
        });
    }

    /// `ALLOC_PAGES` counts what was ASKED for, which is where a byte length
    /// rounded the wrong way shows up; `ALLOC_CALLS` counts the calls. The
    /// two must not be the same number by accident.
    #[test]
    fn the_pages_asked_for_are_counted_apart_from_the_calls() {
        alone_with_the_allocator(|| {
            for pages in [1, 4, 16] {
                assert_ne!(unsafe { drivers_dma_alloc(pages) }, 0);
            }
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 3);
            assert_eq!(ALLOC_PAGES.load(Ordering::SeqCst), 21);
        });
    }

    /// `FAIL_ALLOC` is out of memory, which the kernel answers with 0.
    #[test]
    fn the_failure_switch_answers_out_of_memory() {
        alone_with_the_allocator(|| {
            FAIL_ALLOC.store(true, Ordering::SeqCst);
            assert_eq!(unsafe { drivers_dma_alloc(1) }, 0);
            // A failed allocation is still a call: a caller that asked and
            // got nothing is exactly what the counters are for.
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(ALLOC_PAGES.load(Ordering::SeqCst), 1);
        });
    }

    /// `FAIL_ALLOC_AFTER` is the interesting one: it must let the first N
    /// calls through and fail from the N+1-th, which is the only way to
    /// reach the path that gives the earlier regions back. An off-by-one here
    /// would fail the wrong allocation and quietly test a different path.
    #[test]
    fn failing_after_n_lets_exactly_n_through() {
        alone_with_the_allocator(|| {
            FAIL_ALLOC_AFTER.store(2, Ordering::SeqCst);
            assert_ne!(unsafe { drivers_dma_alloc(1) }, 0, "the 1st was refused");
            assert_ne!(unsafe { drivers_dma_alloc(1) }, 0, "the 2nd was refused");
            assert_eq!(unsafe { drivers_dma_alloc(1) }, 0, "the 3rd was allowed");
            assert_eq!(unsafe { drivers_dma_alloc(1) }, 0);
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 4);
        });
    }

    /// Zero means fail from the very first call, with nothing let through.
    #[test]
    fn failing_after_zero_refuses_everything() {
        alone_with_the_allocator(|| {
            FAIL_ALLOC_AFTER.store(0, Ordering::SeqCst);
            assert_eq!(unsafe { drivers_dma_alloc(1) }, 0);
        });
    }

    /// A misaligned answer must still point INSIDE the allocation, or a test
    /// that rounds it up and writes there corrupts the heap instead of
    /// exercising the alignment check.
    #[test]
    fn a_misaligned_answer_still_points_inside_the_allocation() {
        alone_with_the_allocator(|| {
            MISALIGN_ALLOC.store(true, Ordering::SeqCst);
            let base = unsafe { drivers_dma_alloc(1) };
            assert_ne!(base, 0);
            assert_ne!(base % 4096, 0, "the answer is aligned after all");
            // Rounded up to the next page it is still within the slack page
            // the hook allocates on top of what was asked for.
            let rounded = (base + 4095) & !4095;
            let bytes = unsafe { core::slice::from_raw_parts(rounded as *const u8, 4096) };
            assert!(bytes.iter().all(|b| *b == 0));
        });
    }

    /// `POISON_ALLOC` is what a recycled frame actually looks like, so it has
    /// to be non-zero everywhere -- a poison that left any part zeroed would
    /// let a caller that forgets to clear its ring pass anyway.
    #[test]
    fn the_poison_switch_hands_back_dirty_memory_throughout() {
        alone_with_the_allocator(|| {
            POISON_ALLOC.store(true, Ordering::SeqCst);
            let base = unsafe { drivers_dma_alloc(1) };
            assert_ne!(base, 0);
            let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, 2 * 4096) };
            assert!(bytes.iter().all(|b| *b == 0x5a), "the poison left holes");
        });
    }

    /// Deallocation counts and leaks on purpose: freeing for real would pull
    /// the memory out from under a test that keeps a raw pointer into a ring.
    /// Every question asked about it is whether it was CALLED.
    #[test]
    fn deallocation_is_counted_and_never_fails() {
        alone_with_the_allocator(|| {
            let base = unsafe { drivers_dma_alloc(3) };
            assert_eq!(unsafe { drivers_dma_dealloc(base, 3) }, 0);
            assert_eq!(unsafe { drivers_dma_dealloc(base, 1) }, 0);
            assert_eq!(DEALLOC_CALLS.load(Ordering::SeqCst), 2);
            assert_eq!(DEALLOC_PAGES.load(Ordering::SeqCst), 4);
            // And the memory is still readable afterwards, which is the whole
            // reason it leaks.
            let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, 4096) };
            assert!(bytes.iter().all(|b| *b == 0));
        });
    }

    /// The cache-policy calls succeed by default and fail on their switch,
    /// with the kernel's own convention: 0 is success, negative is failure.
    #[test]
    fn the_cache_policy_calls_follow_the_kernels_sign_convention() {
        alone_with_the_allocator(|| {
            assert_eq!(unsafe { drivers_dma_mark_uncached(0x1000, 1) }, 0);
            assert_eq!(unsafe { drivers_dma_verify_uncached(0x1000, 1) }, 0);
            assert_eq!(MARK_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(VERIFY_CALLS.load(Ordering::SeqCst), 1);

            FAIL_MARK.store(true, Ordering::SeqCst);
            FAIL_VERIFY.store(true, Ordering::SeqCst);
            assert!(unsafe { drivers_dma_mark_uncached(0x1000, 1) } < 0);
            assert!(unsafe { drivers_dma_verify_uncached(0x1000, 1) } < 0);
            assert_eq!(MARK_CALLS.load(Ordering::SeqCst), 2);
            assert_eq!(VERIFY_CALLS.load(Ordering::SeqCst), 2);
        });
    }

    /// The address translations are the identity in a host build, and they
    /// have to be each other's inverse: a driver that converts a ring
    /// descriptor one way and reads it back the other must land where it
    /// started.
    #[test]
    fn the_address_translations_are_the_identity_and_inverse() {
        for a in [0usize, 1, 4096, 0xdead_beef, usize::MAX] {
            assert_eq!(unsafe { drivers_phys_to_virt(a) }, a);
            assert_eq!(unsafe { drivers_virt_to_phys(a) }, a);
            assert_eq!(unsafe { drivers_phys_to_virt(drivers_virt_to_phys(a)) }, a);
        }
    }

    /// The wake counter is how a test tells a driver that polls and wakes
    /// from one that polls and leaves a blocked reader on the fallback park
    /// timer -- which looks exactly like a slow network.
    #[test]
    fn every_wake_of_the_rx_waiters_is_counted() {
        alone_with_the_allocator(|| {
            assert_eq!(WAKE_CALLS.load(Ordering::SeqCst), 0);
            unsafe { drivers_wake_net_rx_waiters() };
            unsafe { drivers_wake_net_rx_waiters() };
            assert_eq!(WAKE_CALLS.load(Ordering::SeqCst), 2);
        });
    }

    /// `fetch_add` returns the value from BEFORE the add, like the
    /// `AtomicUsize` it stands in for. `drivers_dma_alloc` compares that
    /// return against `FAIL_ALLOC_AFTER`, so a `fetch_add` that returned the
    /// new value would shift the failure by one call.
    #[test]
    fn the_thread_counter_returns_the_value_from_before_the_add() {
        alone_with_the_allocator(|| {
            assert_eq!(ALLOC_CALLS.fetch_add(1, Ordering::SeqCst), 0);
            assert_eq!(ALLOC_CALLS.fetch_add(5, Ordering::SeqCst), 1);
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 6);
        });
    }

    /// `reset` is what makes the isolation real; it must clear every switch
    /// and zero every counter, including `FAIL_ALLOC_AFTER`, whose default is
    /// `usize::MAX` and not zero -- resetting that one to zero would refuse
    /// every allocation in the rest of the suite.
    #[test]
    fn reset_restores_every_default_including_the_one_that_is_not_zero() {
        alone_with_the_allocator(|| {
            for flag in [
                &FAIL_ALLOC,
                &MISALIGN_ALLOC,
                &POISON_ALLOC,
                &FAIL_MARK,
                &FAIL_VERIFY,
            ] {
                flag.store(true, Ordering::SeqCst);
            }
            for counter in [
                &ALLOC_CALLS,
                &ALLOC_PAGES,
                &DEALLOC_CALLS,
                &DEALLOC_PAGES,
                &MARK_CALLS,
                &VERIFY_CALLS,
                &WAKE_CALLS,
            ] {
                counter.store(7, Ordering::SeqCst);
            }
            FAIL_ALLOC_AFTER.store(0, Ordering::SeqCst);

            reset();

            for flag in [
                &FAIL_ALLOC,
                &MISALIGN_ALLOC,
                &POISON_ALLOC,
                &FAIL_MARK,
                &FAIL_VERIFY,
            ] {
                assert!(!flag.load(Ordering::SeqCst));
            }
            for counter in [
                &ALLOC_CALLS,
                &ALLOC_PAGES,
                &DEALLOC_CALLS,
                &DEALLOC_PAGES,
                &MARK_CALLS,
                &VERIFY_CALLS,
                &WAKE_CALLS,
            ] {
                assert_eq!(counter.load(Ordering::SeqCst), 0);
            }
            assert_eq!(FAIL_ALLOC_AFTER.load(Ordering::SeqCst), usize::MAX);
            assert_ne!(unsafe { drivers_dma_alloc(1) }, 0, "allocation still fails");
        });
    }

    /// `alone_with_the_allocator` resets on BOTH sides, so a test that leaves
    /// a switch set cannot leak it to the next one on the same thread.
    #[test]
    fn the_guard_resets_on_the_way_out_as_well_as_in() {
        alone_with_the_allocator(|| {
            FAIL_ALLOC.store(true, Ordering::SeqCst);
            ALLOC_CALLS.store(99, Ordering::SeqCst);
        });
        assert!(!FAIL_ALLOC.load(Ordering::SeqCst));
        assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 0);
    }

    /// The state is PER THREAD, which is the whole reason these are
    /// `thread_local!` and not statics: the suite ran at `--test-threads=16`
    /// and failed about two runs in three while they were process-wide. A
    /// switch set here must not be visible to another thread, and a counter
    /// bumped there must not be visible here.
    #[test]
    fn one_threads_switches_and_counters_are_invisible_to_another() {
        alone_with_the_allocator(|| {
            FAIL_ALLOC.store(true, Ordering::SeqCst);
            ALLOC_CALLS.store(42, Ordering::SeqCst);

            let seen = std::thread::spawn(|| {
                let flag = FAIL_ALLOC.load(Ordering::SeqCst);
                let count = ALLOC_CALLS.load(Ordering::SeqCst);
                // And this thread allocating must not touch ours.
                assert_ne!(unsafe { drivers_dma_alloc(1) }, 0);
                (flag, count, ALLOC_CALLS.load(Ordering::SeqCst))
            })
            .join()
            .unwrap();

            assert!(!seen.0, "the other thread saw our failure switch");
            assert_eq!(seen.1, 0, "the other thread saw our call count");
            assert_eq!(seen.2, 1, "the other thread did not count its own call");
            assert!(FAIL_ALLOC.load(Ordering::SeqCst));
            assert_eq!(ALLOC_CALLS.load(Ordering::SeqCst), 42);
        });
    }
}
