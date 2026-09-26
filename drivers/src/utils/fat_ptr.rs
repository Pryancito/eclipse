//! Liveness gate for `dyn` fat pointers reached from interrupt context.
//!
//! Every `Box<dyn Fn…>` / `Arc<dyn Fn…>` this crate calls from an IRQ (device
//! handlers, timer callbacks, event-listener wakers, deferred jobs) is two
//! words wide: `{ data, vtable }`. A heap smash — a coroutine stack overflow
//! spilling into neighbouring allocations, or a use-after-free of a one-shot
//! waker — leaves small garbage in one or both words. *Calling* such a pointer
//! is a jump through a null/garbage vtable slot, i.e. a null-range EXECUTE #PF
//! taken in IRQ context with no current thread, which is exactly the
//! unrecoverable cascade `docs/README-crash-repro.md` documents. **Dropping**
//! it is no better: `Drop` for a trait object also dispatches through the same
//! vtable.
//!
//! So the callers gate on [`dyn_fat_ptr_live`] and, when it says the pointer is
//! dead, `core::mem::forget` the value instead of calling or dropping it: leak
//! one closure, keep the machine.
//!
//! The check only ever rejects a pointer that **cannot** be live: a null word,
//! a vtable outside the kernel image, a misaligned vtable, or a vtable that
//! points into the kernel heap (see [`set_vtable_max`]). A genuinely live
//! kernel `dyn` pointer always passes, so a false positive cannot silently
//! disable a working handler. It is a corruption tripwire, not a validator:
//! garbage that happens to look like an aligned `.rodata` address is not caught
//! here.
//!
//! # The heap-range test
//!
//! Real vtables are emitted into `.rodata`, which every target links *below*
//! the `static mut HEAP` in `.bss`. So a vtable word at or above the heap base
//! cannot be genuine — it is a heap pointer that a use-after-free wrote over a
//! live `dyn` object's vtable slot. This is the exact signature observed when a
//! coalesced timer callback's `Box<dyn FnOnce>` had its vtable overwritten with
//! a freed BTree node during VMO teardown: dispatching through it jumps to a
//! data address and faults in IRQ context. Registering the heap base via
//! [`set_vtable_max`] turns that corruption into a caught-and-leaked closure
//! instead of a dead machine.
//!
//! # Why only the vtable gets an address test
//!
//! The `data` word is checked for null and nothing else, on purpose. A
//! *zero-sized* pointee — and a closure that captures nothing is zero-sized —
//! has no allocation, so `Box`/`Arc` store a **dangling** pointer for it:
//! `align_of::<T>()`, i.e. a value as small as 1. `linux_object`'s coalesced
//! DRM timer is exactly that shape
//! (`Box::new(move |_| deliver_pending_drm_timer())`), and rejecting it would
//! be catastrophic rather than merely wrong: the first refusal latches
//! [`heap_smash_suspected`], after which the IRQ and timer paths stop calling
//! *every* closure — keyboard, serial, xHCI and all timers go dead at once, and
//! the machine freezes with no fault to point at. Only the vtable can be
//! meaningfully bounded, because it always points into the kernel image's
//! `.rodata`.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Per-CPU sticky "the heap is no longer trustworthy" flags. A smash on one
/// core must not disable dyn dispatch on every other core.
///
/// Sized from `lock`'s own constant rather than a repeated `64`: it is the
/// single source of truth for every array indexed by the dense logical cpu id,
/// and the two drifting apart would not be a compile error -- it would silently
/// drop the reports of every core above this one, which is the one thing this
/// module cannot afford (see [`note_on`]).
const MAX_CPU: usize = lock::MAX_CORE_NUM;
static HEAP_SMASH_SUSPECTED: [AtomicBool; MAX_CPU] = [const { AtomicBool::new(false) }; MAX_CPU];

/// A smash reported by a CPU that could not name itself.
///
/// `lock::current_cpu_id` answers `NO_CPU` (255), *not* 0, when it cannot
/// resolve this core -- deliberately, and its own comment says why: "a wrong 0
/// does not lose the answer, it silently nests this CPU's interrupt-disable
/// depth in the BSP's slot". This module used to drop that answer on the floor:
/// `if cpu < MAX_CPU` made both halves **fail open**, so a core that cannot be
/// identified reported corruption to nobody and kept dispatching through dyn
/// pointers -- and the window where a core has no logical id yet (early AP
/// boot, or a bogus published id) is not a quiet one.
///
/// Both callers of [`note_heap_smash_suspected`] have already *proved* the
/// corruption: a fat pointer that cannot be live, or a `fn()` slot that is not
/// in `.text`. Losing that is the unrecoverable cascade this file exists to
/// prevent, so an unplaceable report counts for **every** CPU: we cannot rule
/// any of them out. That is the module's own stated policy for an ambiguous
/// shape -- "corruption that may be spreading, so stay conservative and latch".
static SMASH_ON_UNPLACEABLE_CPU: AtomicBool = AtomicBool::new(false);

/// Record that kernel memory corruption is suspected on **this** CPU.
/// Idempotent and lock-free: callable from any context, including a hard IRQ.
pub fn note_heap_smash_suspected() {
    note_on(lock::current_cpu_id());
}

/// Whether a heap smash has been observed on **this** CPU since boot.
pub fn heap_smash_suspected() -> bool {
    suspected_on(lock::current_cpu_id())
}

/// The half of [`note_heap_smash_suspected`] that does not ask the hardware,
/// so a test can hand it the id a real core would have answered.
fn note_on(cpu: u8) {
    // `get`, not `if cpu < MAX_CPU`: the same lookup, but with nowhere for an
    // out-of-range id to fall through unnoticed.
    match HEAP_SMASH_SUSPECTED.get(cpu as usize) {
        Some(flag) => flag.store(true, Ordering::SeqCst),
        None => SMASH_ON_UNPLACEABLE_CPU.store(true, Ordering::SeqCst),
    }
}

/// See [`note_on`].
fn suspected_on(cpu: u8) -> bool {
    if SMASH_ON_UNPLACEABLE_CPU.load(Ordering::Relaxed) {
        return true;
    }
    HEAP_SMASH_SUSPECTED
        .get(cpu as usize)
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// First address of the kernel heap, or `0` until the boot code registers it.
///
/// The kernel image links `.rodata` (where every real vtable lives) below
/// `.bss` (where the `static mut HEAP` sits), so **no genuine vtable is ever at
/// or above the heap base**. A fat pointer whose vtable word lands inside the
/// heap is therefore corruption by construction — most often a
/// use-after-free that has had a live `dyn` object's vtable slot overwritten
/// with a heap pointer (a freed BTree node, another allocation's payload, …).
///
/// Zero means "not registered" — hosted/libos builds and early boot leave the
/// heap-range test disabled and fall back to the address-half test alone.
static VTABLE_MAX: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel heap base so [`dyn_fat_ptr_live`] can reject vtables that
/// point into the heap. Call once, from bare-metal heap init, with the heap's
/// lowest address. Zero disables the test again. Idempotent; a later call
/// simply overwrites the bound.
///
/// **A bound that is too low is refused**, because it is not a degraded test,
/// it is a dead machine: every genuine vtable in the image would then be "at or
/// above the heap", so `dyn_fat_ptr_live` would refuse *every* dyn dispatch in
/// the system -- no keyboard, no serial, no xHCI, no timers -- and the
/// heap-resident case deliberately does not latch the sticky flag, so there
/// would be nothing to point at either. The caller's own comment in
/// `zCore/src/memory_x86_64.rs` describes exactly this trap for a *grown* heap
/// region, which lives in the linear map below the image; it is worth more than
/// a comment. The bound is compared against a vtable known to be genuine
/// because it was made here, in this image's `.rodata`.
pub fn set_vtable_max(heap_start: usize) {
    let known_good = a_genuine_vtable();
    if heap_start != 0 && known_good != 0 && heap_start <= known_good {
        warn!(
            "[fat-ptr] refusing a vtable ceiling of {:#x}: a real vtable lives at \
             {:#x}, so every dyn dispatch in the kernel would be read as a \
             use-after-free; leaving the heap-range test as it was",
            heap_start, known_good
        );
        return;
    }
    VTABLE_MAX.store(heap_start, Ordering::SeqCst);
}

/// The vtable word of a `dyn` pointer built right here, i.e. an address the
/// linker put in this image's read-only data alongside every other vtable.
fn a_genuine_vtable() -> usize {
    let probe: &dyn core::fmt::Debug = &();
    // SAFETY: `&dyn Debug` is exactly two words, `{ data, vtable }`, and
    // `probe` is a live local, so both words are in bounds and aligned.
    unsafe {
        let words = &probe as *const &dyn core::fmt::Debug as *const usize;
        core::ptr::read_volatile(words.add(1))
    }
}

/// Whether `vtable` falls in the heap (or above it), where no real vtable can
/// live. Returns `false` when the bound is unregistered so the test is inert.
#[inline]
fn vtable_in_heap(vtable: usize) -> bool {
    let max = VTABLE_MAX.load(Ordering::Relaxed);
    max != 0 && vtable >= max
}

/// Whether `addr` can be the address of a vtable in the kernel image.
///
/// Every bare-metal target links the kernel into the upper half (x86_64
/// `0xffff_8000_…`, riscv64 `0xffff_ffc0_…`, aarch64 TTBR1 `0xffff_…`), so a
/// sign-extended-negative value is the portable test, and a small or user-half
/// value in a vtable slot is corruption by construction.
///
/// On a hosted build (libos) the whole "kernel" is an ordinary userspace
/// process, so its vtables live at *low* addresses and this test would reject
/// every handler in the system. There is no kernel/user split to check there —
/// the test degrades to "not null", which is all that is knowable.
#[inline]
fn plausible_vtable(addr: usize) -> bool {
    if cfg!(target_os = "none") {
        (addr as isize) < 0
    } else {
        true
    }
}

/// Whether the `dyn` fat pointer stored in `p` still looks callable.
///
/// `P` must be a two-word pointer to an unsized value — `Box<dyn …>`,
/// `Arc<dyn …>`, `&dyn …`. A `P` of any other width returns `true`
/// (nothing to check), so a caller that passes a thin pointer degrades to the
/// old unguarded behaviour rather than rejecting everything.
///
/// Returns `false` — and latches [`heap_smash_suspected`] — when the pointer
/// cannot possibly be live. The caller must then `core::mem::forget` the value:
/// see the module docs for why dropping it is not an option.
pub fn dyn_fat_ptr_live<P>(p: &P) -> bool {
    const WORD: usize = core::mem::size_of::<usize>();
    if core::mem::size_of::<P>() != 2 * WORD {
        return true;
    }
    // SAFETY: `p` is a live `&P` of exactly two words, so both reads are in
    // bounds and naturally aligned (`P` contains pointers, hence is word
    // aligned). Volatile because the words being validated are precisely the
    // ones a concurrent smash may have rewritten — the compiler must not fold
    // these loads into an assumption about a well-formed fat pointer.
    let (data, vtable) = unsafe {
        let words = p as *const P as *const usize;
        (
            core::ptr::read_volatile(words),
            core::ptr::read_volatile(words.add(1)),
        )
    };
    // The vtable is the word that gets *dispatched through*, so it carries the
    // whole test: non-null, inside the kernel image, and word-aligned (a vtable
    // is an array of pointers and is never misaligned). `data` is only checked
    // for null — see the module docs: a zero-sized pointee legitimately stores
    // a tiny dangling pointer there, and rejecting it freezes the machine.
    let in_heap = vtable_in_heap(vtable);
    let live =
        data != 0 && vtable != 0 && plausible_vtable(vtable) && vtable % WORD == 0 && !in_heap;
    if !live {
        // Latching decision. A vtable that points INTO the heap is the
        // signature of a *localized* use-after-free — one freed object's slot
        // now holds a heap pointer (e.g. a recycled BTree node landing on a
        // one-shot timer callback). Skipping and leaking just that closure lets
        // the rest of the machine keep running, so we do NOT set the sticky
        // smash flag for it: latching would stop *all* IRQ and timer dyn
        // dispatch (keyboard, serial, xHCI, every timer) and freeze a machine
        // that only lost one callback. Every other dead shape — null,
        // misaligned, user-half — is ambiguous corruption that may be spreading,
        // so stay conservative and latch as before.
        if !in_heap {
            note_heap_smash_suspected();
        }
        warn!(
            "[fat-ptr] dead dyn pointer: data={:#x} vtable={:#x}{} — \
             skipping and leaking it instead of dispatching through the vtable",
            data,
            vtable,
            if in_heap {
                " (vtable points into the heap: use-after-free; contained, machine keeps running)"
            } else {
                ""
            },
        );
    }
    live
}

/// Clear every sticky flag and the vtable ceiling.
///
/// `#[cfg(test)]` only, and it is what makes this module testable at all. The
/// flags are sticky by design and the shipped build has no way to clear them, so
/// the first test that tripped the gate would stop every handler in every later
/// test of the same binary -- which is exactly what the gate does on purpose in a
/// kernel that is already corrupt, and is why this file had no tests at all
/// while sitting under every IRQ, timer and event dispatch in the system.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    for flag in HEAP_SMASH_SUSPECTED.iter() {
        flag.store(false, Ordering::SeqCst);
    }
    SMASH_ON_UNPLACEABLE_CPU.store(false, Ordering::SeqCst);
    VTABLE_MAX.store(0, Ordering::SeqCst);
}

#[cfg(test)]
static GATE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());

/// Takes the gate's process-wide state for one test at a time, and resets it on
/// the way in and out. (The CI runs this suite with `--test-threads=1` anyway;
/// this makes it true without the flag.)
#[cfg(test)]
pub(crate) struct GateForTest(#[allow(dead_code)] crate::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl GateForTest {
    pub(crate) fn new() -> Self {
        let guard = GATE.lock();
        reset_for_tests();
        Self(guard)
    }
}

#[cfg(test)]
impl Drop for GateForTest {
    fn drop(&mut self) {
        reset_for_tests();
    }
}

/// This is the gate every `dyn` closure the kernel calls from an interrupt goes
/// through -- device handlers, timer callbacks, event-listener wakers, deferred
/// jobs -- and it had **no tests**, for a reason that is itself a defect: the
/// flag it latches is sticky, per CPU, and unclearable, so one test that tripped
/// it would leave every later test in the binary unable to dispatch anything.
/// `reset_for_tests` above is what unlocked the rest of this module.
///
/// What a host build cannot answer: `plausible_vtable` degrades to "not null"
/// off bare metal, because a hosted kernel's vtables live at low addresses, so
/// the user-half rule is not exercised here. Everything else is.
#[cfg(test)]
mod fat_ptr_tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::sync::Arc;

    /// A two-word value whose words the gate will read as `{ data, vtable }`.
    /// The gate reads whatever it is handed, so this is how a smashed pointer is
    /// put in front of it without smashing anything.
    fn pointer(data: usize, vtable: usize) -> [usize; 2] {
        [data, vtable]
    }

    #[test]
    fn every_shape_of_live_dyn_pointer_the_kernel_actually_dispatches_passes() {
        let _gate = GateForTest::new();
        let arc: Arc<dyn Fn()> = Arc::new(|| {});
        assert!(dyn_fat_ptr_live(&arc), "Arc<dyn Fn> (every IRQ handler)");

        let boxed: Box<dyn Fn()> = Box::new(|| {});
        assert!(dyn_fat_ptr_live(&boxed), "Box<dyn Fn>");

        // The one the module docs single out: a closure that captures nothing is
        // zero-sized, so `Box` stores a **dangling** data word as small as 1.
        // Rejecting it would latch the flag and take the whole machine down, so
        // `data` is only ever checked for null.
        let captureless: Box<dyn Fn()> = Box::new(|| {});
        assert_eq!(
            core::mem::size_of_val(&*captureless),
            0,
            "not a ZST any more"
        );
        assert!(dyn_fat_ptr_live(&captureless), "a zero-sized pointee");

        let reference: &dyn core::fmt::Debug = &();
        assert!(dyn_fat_ptr_live(&reference), "&dyn");
        assert!(!heap_smash_suspected(), "a live pointer latched the flag");
    }

    #[test]
    fn a_pointer_that_is_not_two_words_is_not_judged() {
        // A caller that hands over a thin pointer gets the old unguarded
        // behaviour rather than having everything rejected.
        let _gate = GateForTest::new();
        assert!(dyn_fat_ptr_live(&1usize));
        assert!(dyn_fat_ptr_live(&[0usize, 0, 0]));
        assert!(!heap_smash_suspected());
    }

    #[test]
    fn a_null_word_in_either_half_is_dead_and_latches() {
        let good = a_genuine_vtable();
        for (data, vtable, what) in [
            (0, good, "a null data word"),
            (1, 0, "a null vtable"),
            (0, 0, "both null"),
        ] {
            let _gate = GateForTest::new();
            assert!(!dyn_fat_ptr_live(&pointer(data, vtable)), "{}", what);
            assert!(
                heap_smash_suspected(),
                "{} is ambiguous corruption and has to latch",
                what
            );
        }
    }

    #[test]
    fn a_misaligned_vtable_is_dead() {
        // A vtable is an array of pointers and is never misaligned, so an
        // off-by-a-byte word is corruption however plausible its magnitude.
        let _gate = GateForTest::new();
        let good = a_genuine_vtable();
        assert!(!dyn_fat_ptr_live(&pointer(1, good + 1)));
        assert!(heap_smash_suspected());
    }

    #[test]
    fn a_vtable_in_the_heap_is_dead_but_does_not_latch() {
        // The distinction the whole latching comment is about, and nothing
        // checked it. A vtable pointing into the heap is one freed object's slot
        // holding a heap pointer: contained, so leak that closure and let the
        // machine keep running. Latching here would stop the keyboard, the
        // serial port, the xHCI and every timer over one lost callback.
        let _gate = GateForTest::new();
        let ceiling = a_genuine_vtable() + 0x1000;
        set_vtable_max(ceiling);
        assert!(!dyn_fat_ptr_live(&pointer(1, ceiling)), "at the ceiling");
        assert!(!dyn_fat_ptr_live(&pointer(1, ceiling + 0x800)), "above it");
        assert!(
            !heap_smash_suspected(),
            "a contained use-after-free must not disable every handler"
        );
        // And a real pointer still goes through with the ceiling registered.
        let arc: Arc<dyn Fn()> = Arc::new(|| {});
        assert!(dyn_fat_ptr_live(&arc));
    }

    #[test]
    fn a_ceiling_at_or_below_a_real_vtable_is_refused() {
        // Accepting it reads every genuine vtable in the image as a
        // use-after-free: no keyboard, no serial, no timers, and no latched flag
        // to point at it either, because the heap case does not latch.
        let _gate = GateForTest::new();
        let good = a_genuine_vtable();
        let arc: Arc<dyn Fn()> = Arc::new(|| {});
        for ceiling in [1, good / 2, good] {
            set_vtable_max(ceiling);
            assert!(
                dyn_fat_ptr_live(&arc),
                "a ceiling of {:#x} disabled dyn dispatch",
                ceiling
            );
        }
        assert_eq!(VTABLE_MAX.load(Ordering::SeqCst), 0, "the bound was taken");
    }

    #[test]
    fn a_ceiling_of_zero_turns_the_heap_test_off() {
        // Which is how every build that never registers one behaves, and it has
        // to be inert rather than "everything is in the heap".
        let _gate = GateForTest::new();
        let good = a_genuine_vtable();
        set_vtable_max(good + 0x1000);
        assert!(!dyn_fat_ptr_live(&pointer(1, good + 0x1000)));
        set_vtable_max(0);
        assert!(dyn_fat_ptr_live(&pointer(1, good + 0x1000)));
    }

    #[test]
    fn a_smash_on_one_core_does_not_stop_the_others() {
        let _gate = GateForTest::new();
        note_on(3);
        assert!(suspected_on(3));
        for other in [0u8, 1, 2, 4, (MAX_CPU - 1) as u8] {
            assert!(!suspected_on(other), "core {} was stopped too", other);
        }
    }

    #[test]
    fn a_report_from_a_core_that_cannot_name_itself_is_not_dropped() {
        // `lock::current_cpu_id` answers 255, not 0, when it cannot resolve this
        // core -- and the window where a core has no logical id yet is not a
        // quiet one. The old `if cpu < MAX_CPU` failed **open**: the report went
        // nowhere and dispatch carried on through a pointer already proved dead.
        let _gate = GateForTest::new();
        note_on(u8::MAX);
        for cpu in [0u8, 1, 17, (MAX_CPU - 1) as u8, u8::MAX] {
            assert!(
                suspected_on(cpu),
                "core {} kept dispatching after an unplaceable smash",
                cpu
            );
        }
    }

    #[test]
    fn the_table_covers_exactly_the_cores_the_kernel_can_have() {
        // One source of truth: `lock::MAX_CORE_NUM` sizes every array indexed by
        // the dense logical id. A second copy of `64` here drifting low would
        // silently drop the reports of every core above it.
        assert_eq!(MAX_CPU, lock::MAX_CORE_NUM);
        {
            let _gate = GateForTest::new();
            note_on((MAX_CPU - 1) as u8);
            assert!(suspected_on((MAX_CPU - 1) as u8), "the last slot is a core");
            assert!(
                !suspected_on(0),
                "the last slot spilled into an unplaceable report"
            );
        }
        // The guard is a lock, so the second half waits for the first to drop.
        {
            let _gate = GateForTest::new();
            note_on(MAX_CPU as u8);
            assert!(
                suspected_on(0),
                "one past the last slot has to be unplaceable, not silent"
            );
        }
    }
}
