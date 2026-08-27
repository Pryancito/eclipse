//! `DRM_IOCTL_SYNCOBJ_EVENTFD` (`0xCF`): arrange for an eventfd to be signaled
//! when a DRM syncobj reaches a timeline point. This is how a Wayland
//! compositor waits on a client's buffer fence without a blocking thread —
//! wlroots' `linux-drm-syncobj-v1` explicit-sync path registers the acquire
//! point here and drives its event loop off the eventfd.
//!
//! The ioctl needs the eventfd from the process fd table, so it is parsed in
//! `linux-syscall` (like `SYNCOBJ_HANDLE_TO_FD`); this module owns the part
//! that must live where [`FileLike`] does: the waiter table and the delivery.
//!
//! # Model
//!
//! [`zcore_drivers::scheme::syncobj`] is synchronous (a point is signaled by an
//! explicit call, from an ioctl or this driver's own `EXEC` completion), so
//! there is no `dma_fence` to attach a callback to. Instead every point advance
//! calls [`on_syncobj_signaled`] via a registered hook, and we re-check the
//! registered waiters and deliver the ones whose target point is now reached.
//! A waiter whose target is already reached at registration time is delivered
//! immediately. Signalling an eventfd is a plain `write` of 1 (its counter
//! increments and its eventbus wakes any poller), so a viewer blocked in
//! `poll()`/`read()` on the eventfd is released exactly as under Linux.

use super::FileLike;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lock::Mutex;

struct Waiter {
    handle: u32,
    /// Target timeline point (floored at 1: point 0 means the binary "signaled"
    /// state, i.e. the next signal, never "already true on an unsignaled obj").
    point: u64,
    ev: Arc<dyn FileLike>,
}

lazy_static::lazy_static! {
    static ref WAITERS: Mutex<Vec<Waiter>> = Mutex::new(Vec::new());
}
/// Fast-path gate: the point-advance hook fires on EVERY syncobj signal,
/// including per-frame `EXEC` completions, so skip taking the lock when nothing
/// is registered (the common case — explicit sync is off on the software path).
static WAITER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Deliver an eventfd: a `write` of `1` bumps its counter and wakes its
/// eventbus, releasing any poller/reader. Best-effort — an overflowing eventfd
/// (never, for a u64 counter driven one at a time) is the only failure.
fn deliver(ev: &Arc<dyn FileLike>) {
    let _ = ev.write(&1u64.to_ne_bytes());
}

/// `SYNCOBJ_EVENTFD`: signal `ev` when syncobj `handle` reaches `point`.
/// Delivered immediately if the point is already reached; otherwise recorded
/// and delivered on its own as the syncobj advances. Point 0 means the binary
/// signaled state (floored to 1), matching the rest of the syncobj layer.
pub fn register(handle: u32, point: u64, ev: Arc<dyn FileLike>) {
    let target = point.max(1);
    if let Some(cur) = zcore_drivers::scheme::syncobj::query(handle) {
        if cur >= target {
            deliver(&ev);
            return;
        }
    }
    let mut waiters = WAITERS.lock();
    waiters.push(Waiter { handle, point: target, ev });
    WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
}

/// Registered point-advance hook (see [`init`]). Deliver every waiter whose
/// target point is now reached, and drop any whose syncobj has been destroyed.
/// Runs on every signal, so it returns on a single relaxed load when the
/// registry is empty. Deliveries happen with the lock released — an eventfd
/// `write` takes the eventbus lock, and holding two locks across a wake is how
/// this codebase has deadlocked before.
fn on_syncobj_signaled(_handle: u32, _point: u64) {
    if WAITER_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut fire: Vec<Arc<dyn FileLike>> = Vec::new();
    {
        let mut waiters = WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() {
            match zcore_drivers::scheme::syncobj::query(waiters[i].handle) {
                Some(cur) if cur >= waiters[i].point => {
                    fire.push(waiters.swap_remove(i).ev);
                }
                // Syncobj destroyed out from under the waiter: it can never be
                // reached now, so drop it (the eventfd is simply never signaled,
                // same as a real syncobj fd whose object went away).
                None => {
                    waiters.swap_remove(i);
                }
                _ => i += 1,
            }
        }
        WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
    }
    for ev in fire {
        deliver(&ev);
    }
}

/// Wire [`on_syncobj_signaled`] into the syncobj layer's point-advance upcall.
/// Called once at boot.
pub fn init() {
    zcore_drivers::scheme::syncobj::set_signal_hook(on_syncobj_signaled);
}
