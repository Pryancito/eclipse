//! Pluggable, tamper-resistant monotonic clock for timestamping security events.
//!
//! `hunter` is a `no_std` crate that must stay free of a hard dependency on
//! `kernel-hal` (it sits *below* the syscall layer in the build graph). The
//! kernel registers a time source at boot via [`set_time_source`]; until then
//! timestamps read as `0`, which the renderer prints harmlessly.
//!
//! Hardening (P12): the time source gates both the forensic log and the IDS
//! sliding windows, so it is a security-sensitive input. Registration is
//! therefore **sealed** — only the first call wins — preventing later code (or
//! an attacker who reaches a mutator) from swapping in a frozen / lying clock
//! to silence detection. The heuristics additionally apply a count-based window
//! backstop so a stuck clock cannot disable rate detection outright.

use core::sync::atomic::{AtomicPtr, Ordering};

/// The kernel's monotonic clock (nanoseconds since boot), stored as an erased
/// pointer. `fn` pointers are guaranteed to fit in a data pointer here.
///
/// This pointer **is** the seal, and that is the whole reason there is no
/// second flag beside it. There used to be one, set before the pointer was
/// published, which left a window in which [`is_sealed`] already answered yes
/// and [`now_ns`] still answered 0 — and `is_sealed` is the answer to *may the
/// clock be trusted*, asked about the one input that gates detection. Deriving
/// it from the pointer makes the two impossible to disagree.
static TIME_SOURCE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Registers the monotonic clock used to timestamp events, in nanoseconds
/// since boot. Only the first registration takes effect; later calls are
/// no-ops, so the clock cannot be swapped out at runtime.
pub fn set_time_source(f: fn() -> u64) {
    // One compare-exchange is the whole registration: claim the slot and
    // publish the pointer in the same step, so there is nothing in between for
    // a second registration -- or a reader -- to land in.
    let _ = TIME_SOURCE.compare_exchange(
        core::ptr::null_mut(),
        f as *mut (),
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
}

/// Returns `true` once a time source has been sealed in.
pub fn is_sealed() -> bool {
    !TIME_SOURCE.load(Ordering::Acquire).is_null()
}

/// Forgets the registered clock.
///
/// The seal is one-way by design, so a test that wants a clock of its own is
/// the only thing that may break it -- which is why this exists and why it is
/// test-only. Callers hold [`crate::test_globals::lock`].
#[cfg(test)]
pub(crate) fn reset_for_test() {
    TIME_SOURCE.store(core::ptr::null_mut(), Ordering::SeqCst);
}

/// Current monotonic time in nanoseconds, or `0` if no source is registered.
pub fn now_ns() -> u64 {
    let p = TIME_SOURCE.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: `p` is non-null only after `set_time_source` sealed a valid
    // `fn() -> u64` pointer, and the slot only ever goes back to null (never
    // to another value), which the check above has already ruled out.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}
