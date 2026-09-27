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
extern "C" fn drivers_wake_net_rx_waiters() {}

#[no_mangle]
extern "C" fn drivers_net_drain() {}
