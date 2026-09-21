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
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`: fire when a fence covering
    /// `point` has been *submitted* (EXEC attached a hardware fence), not
    /// necessarily signaled.
    wait_available: bool,
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
/// `wait_available` uses the last-submitted point (in-flight EXEC fences).
pub fn register(handle: u32, point: u64, ev: Arc<dyn FileLike>, wait_available: bool) {
    let target = point.max(1);
    // The check and the insertion happen under ONE hold of `WAITERS`, which is
    // what makes them atomic against `on_syncobj_signaled`. Reading the point
    // first and taking the lock afterwards left a window: a signal landing in
    // it ran the hook against a registry that did not yet hold this waiter, and
    // the waiter was then inserted with a target already reached. Nothing
    // re-checks it -- `arm_poller` only fires while a hardware fence is
    // pending, and an explicit SIGNAL leaves none -- so the eventfd was never
    // written and the compositor's frame never completed. Linux closes the same
    // window by adding the callback and fetching the fence under `syncobj->lock`
    // (`drm_syncobj_add_callback_locked`).
    //
    // Lock order is WAITERS then the syncobj TABLE (via `query`), the same way
    // round as the hook; the delivery is done with the lock released, because
    // writing an eventfd takes the eventbus lock and holding two across a wake
    // is how this codebase has deadlocked before.
    let already_reached = {
        let mut waiters = WAITERS.lock();
        let cur = if wait_available {
            zcore_drivers::scheme::syncobj::query_submitted(handle)
        } else {
            zcore_drivers::scheme::syncobj::query(handle)
        };
        match cur {
            Some(cur) if cur >= target => true,
            _ => {
                waiters.push(Waiter {
                    handle,
                    point: target,
                    wait_available,
                    ev: ev.clone(),
                });
                WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
                false
            }
        }
    };
    if already_reached {
        deliver(&ev);
        return;
    }
    arm_poller();
}

/// How often the poller re-checks pending hardware fences while eventfd
/// waiters are armed. A fence that lands is otherwise only noticed on the
/// next syncobj ioctl from SOME process, which for a compositor blocked in
/// `poll()` on the eventfd may be a whole frame away.
const POLL_INTERVAL: core::time::Duration = core::time::Duration::from_micros(250);

static POLLER_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Hardware fences (`syncobj::attach_hw_fence`, the GPU driver's direct-submit
/// path) are resolved lazily -- nobody is notified when the GPU writes the
/// landing zone. An eventfd waiter is the one client that does NOT come back
/// to ask, so while any is armed and a hardware fence is pending, a short
/// periodic timer polls the fences on their behalf; the signal hook then
/// delivers the eventfd exactly as an explicit signal would. Idle otherwise.
fn arm_poller() {
    if WAITER_COUNT.load(Ordering::Relaxed) == 0
        || !zcore_drivers::scheme::syncobj::has_pending()
        || POLLER_ARMED.swap(true, Ordering::AcqRel)
    {
        return;
    }
    kernel_hal::timer::timer_set(
        kernel_hal::timer::deadline_after(POLL_INTERVAL),
        alloc::boxed::Box::new(|_| {
            POLLER_ARMED.store(false, Ordering::Release);
            // Resolves landed fences and fires the signal hook (below) for
            // every point that advanced, which delivers the eventfds.
            zcore_drivers::scheme::syncobj::poll_pending();
            arm_poller();
        }),
    );
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
            match if waiters[i].wait_available {
                zcore_drivers::scheme::syncobj::query_submitted(waiters[i].handle)
            } else {
                zcore_drivers::scheme::syncobj::query(waiters[i].handle)
            } {
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
    // Re-arm for the waiters that are still here. `register` is the only other
    // place that arms the poller, and it gives up when nothing is pending --
    // which is the normal case, because a compositor registers the acquire
    // point BEFORE the client submits the work that will reach it.
    //
    // The submit is what creates the hardware fence, and `attach_hw_fence`
    // announces it through this very hook, with the point it just *submitted*.
    // A plain waiter (no WAIT_AVAILABLE) does not accept a submitted point, so
    // it stays registered -- and without re-arming here, nothing is watching
    // when the GPU finally lands that fence. Hardware fences are resolved
    // lazily, only inside some other syncobj call, so "nothing is watching"
    // means the eventfd is never written and the frame never completes.
    //
    // Only reachable on hardware: a fence exists only when a real EXEC put one
    // there, so the software path never walks this at all.
    arm_poller();
}

/// Wire [`on_syncobj_signaled`] into the syncobj layer's point-advance upcall.
/// Called once at boot.
pub fn init() {
    zcore_drivers::scheme::syncobj::set_signal_hook(on_syncobj_signaled);
}

/// The half of explicit sync that only real hardware walks.
///
/// A fence here is either explicit — some ioctl sets the point, and the signal
/// hook runs right then — or a *hardware* fence, which the GPU lands on its
/// own with nobody notified. The second kind exists only when a real `EXEC`
/// put one there, so QEMU, where there is no nouveau at all, never reaches it.
/// That is why this path could be broken for a day without a single test or
/// boot noticing.
#[cfg(test)]
mod hardware_fence_tests {
    use super::*;
    use crate::error::LxResult;
    use crate::fs::{OpenFlags, PollEvents, PollStatus};
    use alloc::boxed::Box;
    use core::sync::atomic::AtomicU32;
    use zcore_drivers::scheme::syncobj;
    use zircon_object::object::*;

    /// An eventfd stand-in that counts what was written to it. Delivery is a
    /// plain `write` of 1, so a non-zero count is the compositor being told
    /// its frame is ready.
    struct CountingEventfd {
        base: KObjectBase,
        writes: AtomicU32,
    }

    impl_kobject!(CountingEventfd);

    impl CountingEventfd {
        fn new() -> Arc<Self> {
            Arc::new(CountingEventfd {
                base: KObjectBase::default(),
                writes: AtomicU32::new(0),
            })
        }
        fn delivered(&self) -> u32 {
            self.writes.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl FileLike for CountingEventfd {
        fn flags(&self) -> OpenFlags {
            OpenFlags::RDWR
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Ok(0)
        }
        fn write(&self, buf: &[u8]) -> LxResult<usize> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(buf.len())
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Ok(0)
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(PollStatus::default())
        }
        async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(PollStatus::default())
        }
    }

    fn reset() {
        WAITERS.lock().clear();
        WAITER_COUNT.store(0, Ordering::SeqCst);
        POLLER_ARMED.store(false, Ordering::Release);
    }

    /// An explicit signal delivers, which is the path that always worked and
    /// the baseline for the one below.
    #[test]
    fn an_explicit_signal_delivers_the_eventfd() {
        reset();
        syncobj::set_signal_hook(on_syncobj_signaled);
        let handle = syncobj::create(false);
        let ev = CountingEventfd::new();

        register(handle, 1, ev.clone(), false);
        assert_eq!(ev.delivered(), 0, "nothing has reached the point yet");

        syncobj::timeline_signal(handle, 1);
        assert_eq!(ev.delivered(), 1, "the signal releases the waiter");
    }

    /// The regression. A compositor registers the acquire point BEFORE the
    /// client submits the work that will reach it, so at registration there is
    /// no hardware fence to poll and the poller stands down. The submit then
    /// creates one and announces it through the signal hook — with the point
    /// it just *submitted*, which a plain waiter does not accept, so the
    /// waiter stays.
    ///
    /// If nothing re-arms the poller at that moment, the GPU lands the fence
    /// with nobody watching: hardware fences are only resolved inside some
    /// other syncobj call, so the eventfd is never written, the frame never
    /// completes, and the client sits in its next swap for ever — a window
    /// that is up and frozen, with no error anywhere.
    #[test]
    fn a_fence_submitted_after_the_waiter_still_gets_polled() {
        reset();
        syncobj::set_signal_hook(on_syncobj_signaled);
        let handle = syncobj::create(false);
        let ev = CountingEventfd::new();

        // Registration first, with nothing in flight — the poller has nothing
        // to watch and correctly does not arm.
        register(handle, 1, ev.clone(), false);
        assert_eq!(ev.delivered(), 0);

        // Now the client submits. The fence has NOT landed: the GPU writes the
        // payload into this word later, and it still reads zero.
        let landing_zone: u32 = 0;
        syncobj::attach_hw_fence(
            handle,
            1,
            &landing_zone as *const u32 as usize,
            0,
            1,
            0,
            false,
        );

        assert_eq!(
            ev.delivered(),
            0,
            "submitting is not landing: a plain waiter must not fire yet"
        );
        assert!(
            syncobj::has_pending(),
            "the fence is in flight, so there is something to poll"
        );
        assert!(
            POLLER_ARMED.load(Ordering::Acquire),
            "a waiter with an in-flight fence must leave the poller armed, or \
             nothing will ever notice the GPU landing it"
        );

        reset();
    }
}
