//! Pluggable monotonic clock for timestamping security events.
//!
//! `hunter` is a `no_std` crate that must stay free of a hard dependency on
//! `kernel-hal` (it sits *below* the syscall layer in the build graph). Instead
//! the kernel registers a time source at boot via [`set_time_source`]; until
//! then timestamps read as `0`, which the renderer prints harmlessly.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Function pointer to the kernel's monotonic clock, stored as a `usize`
/// (0 = unset). `fn` pointers are guaranteed to fit in a `usize`.
static TIME_SOURCE: AtomicUsize = AtomicUsize::new(0);

/// Registers the monotonic clock used to timestamp events, in nanoseconds
/// since boot. Called once from kernel init.
pub fn set_time_source(f: fn() -> u64) {
    TIME_SOURCE.store(f as usize, Ordering::Relaxed);
}

/// Current monotonic time in nanoseconds, or `0` if no source is registered.
pub fn now_ns() -> u64 {
    let raw = TIME_SOURCE.load(Ordering::Relaxed);
    if raw == 0 {
        return 0;
    }
    // SAFETY: `raw` is non-zero only after `set_time_source` stored a valid
    // `fn() -> u64` pointer, and such pointers are never unloaded.
    let f: fn() -> u64 = unsafe { core::mem::transmute(raw) };
    f()
}
