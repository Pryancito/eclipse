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
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE` as passed by the ioctl.
    /// Stored for matching / ABI, but EVENTFD delivery always waits for the
    /// point to be *signaled* (fence landed): see [`deliver_ready_waiters`].
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

/// `SYNCOBJ_EVENTFD`: signal `ev` when syncobj `handle` reaches `point`
/// (fence *landed*, engines idle after `RELEASE_WFI_EN`).
///
/// Delivered immediately if the point is already reached; otherwise recorded
/// and delivered as the syncobj advances. Point 0 means the binary signaled
/// state (floored to 1). The ioctl's `WAIT_AVAILABLE` flag is accepted (NVK
/// / wlroots set it) but does **not** change delivery: firing on submit let
/// labwc sample a client's dmabuf while GR was still writing (mild static
/// across the whole Firefox/GPU-client window). Over-waiting until signaled
/// is correct for compositor acquire.
pub fn register(handle: u32, point: u64, ev: Arc<dyn FileLike>, wait_available: bool) {
    let target = point.max(1);
    // Insert FIRST, then check -- never `query` with `WAITERS` held.
    //
    // The window this closes: reading the point first and taking the lock
    // afterwards let a signal land in between and run the hook against a
    // registry that did not yet hold this waiter, which was then inserted with
    // a target already reached. Nothing re-checks it -- `arm_poller` only fires
    // while a hardware fence is pending, and an explicit SIGNAL leaves none --
    // so the eventfd was never written and the compositor's frame never
    // completed. Linux closes it by adding the callback and fetching the fence
    // under `syncobj->lock` (`drm_syncobj_add_callback_locked`).
    //
    // Registering before asking closes it the other way round, and without the
    // lock held across `query`: a signal landing between the insert and the
    // pass below finds this waiter already in the registry and delivers it,
    // and the pass then finds it gone. Holding `WAITERS` across `query` is
    // what wedged the machine -- `query` resolves the table and the
    // point-advance upcall lands back in `on_syncobj_signaled`, which takes
    // this same non-re-entrant lock (see `deliver_ready_waiters`).
    {
        let mut waiters = WAITERS.lock();
        waiters.push(Waiter {
            handle,
            point: target,
            wait_available,
            ev,
        });
        WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
    }
    deliver_ready_waiters();
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
    // Keep the poller alive while *either* SYNCOBJ_EVENTFD waiters *or*
    // sync_file poll waiters (GLX/DRI3) care about hardware fences. Before
    // this, only the eventfd path armed it — so a lone `sync_wait` on a
    // sync_file never saw the GPU land its fence and Zink killed the
    // swapchain (`zink: swapchain killed` → `GLXBadCurrentWindow`).
    let sync_files = super::syncobj_file::pending_waiter_count() > 0;
    let eventfds = WAITER_COUNT.load(Ordering::Relaxed) > 0;
    if !(sync_files || eventfds) || POLLER_ARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    // Eventfd-only: stand down when nothing is in flight (cheap idle).
    // Sync_file waiters (GLX/DRI3) must keep ticking even before the fence
    // exists — Mesa often exports a SYNC_FD and only then submits, and
    // without a tick after that submit `sync_wait` parks forever (glxgears
    // window up, gears frozen, no FPS).
    if !sync_files && !zcore_drivers::scheme::syncobj::has_pending() {
        POLLER_ARMED.store(false, Ordering::Release);
        return;
    }
    kernel_hal::timer::timer_set(
        kernel_hal::timer::deadline_after(POLL_INTERVAL),
        alloc::boxed::Box::new(|_| {
            POLLER_ARMED.store(false, Ordering::Release);
            // Resolves landed fences and fires the signal hook (below) for
            // every point that advanced, which delivers the eventfds / wakes
            // sync_file EventBuses.
            zcore_drivers::scheme::syncobj::poll_pending();
            arm_poller();
        }),
    );
}

/// Arm the hardware-fence poller from the sync_file path (a GLX client that
/// exported a fence still in flight has no SYNCOBJ_EVENTFD waiter).
pub(super) fn ensure_hw_fence_poller() {
    arm_poller();
}

/// Test/diag: whether the periodic HW-fence poller is currently scheduled.
#[cfg(test)]
pub(super) fn poller_is_armed() -> bool {
    POLLER_ARMED.load(Ordering::Acquire)
}

/// Registered point-advance hook (see [`init`]). Deliver every waiter whose
/// target point is now reached, and drop any whose syncobj has been destroyed.
/// Runs on every signal, so it returns on a single relaxed load when the
/// registry is empty. Deliveries happen with the lock released — an eventfd
/// `write` takes the eventbus lock, and holding two locks across a wake is how
/// this codebase has deadlocked before.
/// Deliver every registered waiter whose target point is now reached, and drop
/// any whose syncobj has been destroyed.
///
/// `query` is asked with `WAITERS` RELEASED. It resolves the syncobj table,
/// and a point that advances while it does runs the point-advance upcall,
/// which is [`on_syncobj_signaled`] -- this function again. Asking under the
/// lock re-entered it on a cpu that already held it, and a ticket mutex is not
/// re-entrant; the same shape wedged the machine on `syncobj_file`'s own
/// registry. So: snapshot, ask unlocked, take the lock again to retire what is
/// ready. A nested call that delivered a waiter first leaves it gone from the
/// second pass, so nothing is delivered twice.
fn deliver_ready_waiters() {
    if WAITER_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let probe: Vec<(u32, u64, bool)> = {
        let waiters = WAITERS.lock();
        waiters
            .iter()
            .map(|w| (w.handle, w.point, w.wait_available))
            .collect()
    };
    // `(handle, point, wait_available, reached)`. `reached == false` is a
    // syncobj destroyed out from under the waiter: it can never be reached
    // now, so drop it (the eventfd is simply never signaled, same as a real
    // syncobj fd whose object went away).
    //
    // Always `query` (signaled), never `query_submitted`. wlroots arms
    // SYNCOBJ_EVENTFD with WAIT_AVAILABLE on the client's acquire point;
    // delivering on submit made the compositor texture-sample Firefox's
    // dmabuf mid-render (snow across the window). SYNCOBJ_WAIT still honours
    // WAIT_AVAILABLE for NVK's WAIT_PENDING path; only the eventfd half of
    // compositor acquire is forced to "landed".
    let mut done: Vec<(u32, u64, bool, bool)> = Vec::new();
    for (handle, point, wait_available) in probe {
        let cur = zcore_drivers::scheme::syncobj::query(handle);
        match cur {
            Some(cur) if cur >= point => done.push((handle, point, wait_available, true)),
            None => done.push((handle, point, wait_available, false)),
            _ => {}
        }
    }
    if done.is_empty() {
        return;
    }
    let mut fire: Vec<Arc<dyn FileLike>> = Vec::new();
    {
        let mut waiters = WAITERS.lock();
        let mut i = 0;
        while i < waiters.len() {
            match done.iter().find(|&&(h, p, wa, _)| {
                h == waiters[i].handle && p == waiters[i].point && wa == waiters[i].wait_available
            }) {
                Some(&(_, _, _, reached)) => {
                    let w = waiters.swap_remove(i);
                    if reached {
                        fire.push(w.ev);
                    }
                }
                None => i += 1,
            }
        }
        WAITER_COUNT.store(waiters.len(), Ordering::SeqCst);
    }
    for ev in fire {
        deliver(&ev);
    }
}

fn on_syncobj_signaled(_handle: u32, _point: u64) {
    // Always wake sync_file waiters first: they are independent of the
    // eventfd registry, and returning early when WAITER_COUNT==0 used to
    // leave GLX `sync_wait` parked forever after a hardware fence landed.
    super::syncobj_file::wake_ready_waiters();

    deliver_ready_waiters();
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
pub(crate) mod hardware_fence_tests {
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

    lazy_static::lazy_static! {
        /// Both tests below drive the same process-wide waiter table, and cargo
        /// runs them on two threads at once. Without this, one test's `reset()`
        /// lands between the other's `register` and the signal it is about to
        /// check for, wiping the waiter: the eventfd is never delivered and the
        /// assertion reads `0` where it wanted `1`. It failed 9 runs in 60 that
        /// way, and never once under `--test-threads=1`.
        ///
        /// Held for the whole body, not just around `reset`, because every step
        /// in between touches the same globals.
        ///
        /// `pub(crate)`: a test elsewhere in the crate that signals syncobjs
        /// fires `on_syncobj_signaled` too, once a test here has installed it
        /// (the hook is process-wide and never uninstalled), and that walk
        /// delivers a waiter of these tests outside the `WAITERS` lock -- so
        /// the assertion here could run between the other thread's
        /// `swap_remove` and its `deliver`. Such a test takes this lock.
        pub(crate) static ref TEST_SERIAL: Mutex<()> = Mutex::new(());
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
        let _serial = TEST_SERIAL.lock();
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
        let _serial = TEST_SERIAL.lock();
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

    /// Compositor acquire (wlroots) passes WAIT_AVAILABLE on SYNCOBJ_EVENTFD.
    /// Delivering on submit used to let labwc sample a dmabuf while GR was
    /// still writing — snow across the whole GPU-client window. EVENTFD must
    /// wait for the fence to land even when that flag is set.
    #[test]
    fn wait_available_eventfd_does_not_fire_on_submit_alone() {
        let _serial = TEST_SERIAL.lock();
        reset();
        syncobj::set_signal_hook(on_syncobj_signaled);
        let handle = syncobj::create(false);
        let ev = CountingEventfd::new();

        register(handle, 1, ev.clone(), true /* WAIT_AVAILABLE */);
        assert_eq!(ev.delivered(), 0);

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
            "WAIT_AVAILABLE must not release the compositor's eventfd on submit"
        );
        assert!(
            POLLER_ARMED.load(Ordering::Acquire),
            "poller must stay armed until the fence lands"
        );

        reset();
    }
}
