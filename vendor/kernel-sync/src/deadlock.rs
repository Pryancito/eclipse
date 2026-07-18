//! Spinlock deadlock self-report, shared by every mutex flavor in this crate.
//!
//! A spinlock that spins "forever" (billions of PAUSEs, i.e. many seconds) is
//! a deadlock on an IRQ-off kernel: the machine freezes with no panic and no
//! console output — indistinguishable from a hard hang on a monitor-only box.
//! When a waiter crosses the threshold it calls the installed hook ONCE with
//! its `#[track_caller]` location, so the kernel can paint the stuck call site
//! somewhere lock-free (e.g. straight onto the framebuffer). The hook MUST NOT
//! take locks or allocate.

use core::sync::atomic::{AtomicUsize, Ordering};

static DEADLOCK_HOOK: AtomicUsize = AtomicUsize::new(0);

/// ~8s of PAUSE iterations on current hardware. Normal contention is orders of
/// magnitude below this; only a genuine deadlock/livelock crosses it.
pub(crate) const DEADLOCK_SPINS: u64 = 1_000_000_000;

/// Install the deadlock self-report hook (`file`, `line` of the stuck caller).
pub fn set_deadlock_hook(f: fn(&'static str, u32)) {
    DEADLOCK_HOOK.store(f as usize, Ordering::SeqCst);
}

#[inline(never)]
pub(crate) fn report_deadlock(file: &'static str, line: u32) {
    let h = DEADLOCK_HOOK.load(Ordering::Relaxed);
    if h != 0 {
        let f: fn(&'static str, u32) = unsafe { core::mem::transmute(h) };
        f(file, line);
    }
}
