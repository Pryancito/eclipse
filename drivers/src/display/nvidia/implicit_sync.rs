//! Implicit synchronisation for buffers shared between processes: see the
//! `mod implicit_sync;` doc in `nvidia.rs`.

use super::NvidiaGpu;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// ON by default. `nvidia.implicit_sync=off` restores the old behaviour (a
/// submission waits only for the syncobjs its client named) without a
/// rebuild, because the wait this adds sits in the hottest path the driver
/// has and its granularity is coarser than Linux's.
static IMPLICIT_SYNC: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(v: bool) {
    IMPLICIT_SYNC.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    IMPLICIT_SYNC.load(Ordering::Relaxed)
}

/// EXECs that had at least one foreign buffer bound in their VA space.
static EXECS_WITH_DEPS: AtomicU64 = AtomicU64::new(0);
/// Dependencies this path actually blocked on.
static WAITED: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent blocked.
static WAIT_US: AtomicU64 = AtomicU64::new(0);
/// Dependencies that needed no wait: the producer's fence had already landed.
/// The normal case for a client that speaks `linux-drm-syncobj-v1`, whose
/// explicit wait this EXEC already honoured before reaching here.
static ALREADY_LANDED: AtomicU64 = AtomicU64::new(0);
/// Producers skipped because they are a dead process whose ring is still
/// draining. See the zombie note in [`wait_for_foreign_producers`].
static ZOMBIE_PRODUCER: AtomicU64 = AtomicU64::new(0);
/// Producers with no fence to wait on: no ready context, a wedged one, or a
/// ring that has never fenced. Nothing in flight, or nothing that will land.
static NO_FENCE: AtomicU64 = AtomicU64::new(0);
/// Dependencies given up on at the deadline. The submission goes ahead, so a
/// nonzero number here is a frame that MAY tear -- and a hang somewhere else,
/// because the budget is three frames' worth.
static TIMED_OUT: AtomicU64 = AtomicU64::new(0);

/// How long a submission will block for a producer it shares a buffer with.
///
/// 50 ms is three frames at 60 Hz: long enough that a producer merely late
/// with this frame is waited for, short enough that a producer that is not
/// coming back cannot hold the compositor. Past it the submission goes
/// ahead -- the old behaviour, counted.
const WAIT_BUDGET_US: u64 = 50_000;

/// Backstop for a clock that does not advance: with nothing but a deadline,
/// a timer that has stalled -- or a unit-test clock that only moves when a
/// test moves it -- turns this into an infinite spin. Whichever bound is hit
/// first ends the wait, and both count as the same give-up.
const MAX_SPINS: u32 = 1_000_000;

/// One line for `/proc/gpudbg`, next to the rest of the EXEC profile.
///
/// `timed-out` is the one to read: every other number can be healthy while it
/// grows, and a dependency given up on is exactly the tear this path exists
/// to stop.
pub(crate) fn profile_line() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = write!(
        s,
        "[gpudbg]  implicit sync: {} -- execs with foreign buffers={} waited={} ({}us) already-landed={} no-fence={} zombie-producer={} TIMED-OUT={}",
        if enabled() {
            "ON"
        } else {
            "OFF (nvidia.implicit_sync=off)"
        },
        EXECS_WITH_DEPS.load(Ordering::Relaxed),
        WAITED.load(Ordering::Relaxed),
        WAIT_US.load(Ordering::Relaxed),
        ALREADY_LANDED.load(Ordering::Relaxed),
        NO_FENCE.load(Ordering::Relaxed),
        ZOMBIE_PRODUCER.load(Ordering::Relaxed),
        TIMED_OUT.load(Ordering::Relaxed),
    );
    s
}

/// The distinct pids other than `me` that ALLOCATED a GEM object currently
/// bound in `me`'s GPU address space -- Linux's "external objects" of a VM,
/// the set `drm_gpuvm_exec_lock` walks to add a submission's implicit
/// dependencies.
///
/// `maps` is `(gem_handle, pid whose VA space holds the mapping)` and `gems`
/// is `(gem_handle, pid that allocated it)`. A mapping of a buffer whose
/// allocator is `me` is not external: its writes are already ordered by the
/// one ring that issues them. A mapping whose handle no longer has an object
/// is skipped rather than guessed at.
pub(super) fn foreign_owners(maps: &[(u32, u64)], gems: &[(u32, u64)], me: u64) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for &(handle, vas_pid) in maps {
        if vas_pid != me {
            continue;
        }
        let Some(&(_, allocator)) = gems.iter().find(|g| g.0 == handle) else {
            continue;
        };
        if allocator == me || out.contains(&allocator) {
            continue;
        }
        out.push(allocator);
    }
    out
}

/// The context `pid` submits on, or `None` when it has nothing in flight that
/// could land.
///
/// Deliberately the same rule [`NvidiaGpu::ring_idle_probe`] applies, minus
/// its zombie arm: a pid with no ready entry of its own is NOT routed to
/// context 0, because that is the compositor's ring and waiting on it for a
/// client that has queued nothing would be a wait on the wrong process
/// entirely. Only the owner of the RM channel legitimately answers 0.
fn producer_ctx(dev: &NvidiaGpu, pid: u64) -> Option<u32> {
    let entry = dev
        .nouveau_pid_ctx
        .lock()
        .iter()
        .find(|t| t.0 == pid)
        .map(|t| (t.1, t.4));
    match entry {
        Some((ctx, true)) => Some(ctx),
        Some((_, false)) => None,
        None if dev.nouveau_owns_rm_channel(pid) => Some(0),
        None => None,
    }
}

/// Block until every process that lent `owner_pid` a buffer has landed the
/// last fence its ring issued, so an EXEC by `owner_pid` cannot read a buffer
/// another process is still writing.
///
/// This is the dependency Linux takes from a buffer's `dma_resv`, which this
/// driver does not have: a `NouveauGemObject` carries no fence, and `EXEC`
/// carries only the syncobjs its client named. Without it a compositor
/// sampling a client's just-rendered frame waited for nothing at all unless
/// both sides spoke `linux-drm-syncobj-v1` -- and a frame read while the
/// client was still drawing it comes out with everything the client had not
/// got to yet missing, which on a text window is its text.
///
/// Three deliberate choices, each a limit worth knowing:
///
/// * The fence waited for is the LAST ONE THE PRODUCER'S RING ISSUED, read
///   where it already lies, not a fresh probe appended to that ring the way
///   `scanout_render_fence` and `GEM_CPU_PREP` do. A probe would be exact
///   about the tail but it writes to someone else's ring on every submit of
///   ours, and a consumer's bookkeeping has no business doing that. What this
///   misses is an unfenced tail; NVK fences essentially every submit, and a
///   frame a client hands over is fenced by construction -- that fence is how
///   it signals the syncobj it hands over with.
/// * The wait is on the CPU, before the submission, not an ACQUIRE in the
///   ring. An ACQUIRE would let the two GPUs pipeline, and is the better
///   version of this; it also takes a peer mapping on the producer's context,
///   which is what decides when a dead context can be reaped, so a consumer
///   arriving late would keep one alive. Latency first, lifecycle second:
///   this costs the submitting thread the wait and touches nothing else.
/// * It waits per PRODUCER, not per buffer -- the producer's whole ring, not
///   the work that touches this buffer. That is all the granularity this
///   driver publishes, and it is the same overshoot `GEM_CPU_PREP` has.
///
/// A ZOMBIE producer -- a process that exited with work still queued -- is
/// left alone. Its park landed its last payload from the CPU, so the fence
/// this reads says nothing about its ring, and the honest answer needs the
/// probe this deliberately does not append. What covers such a buffer is what
/// covered it before: `GEM_CLOSE` defers the free behind the ring, and a
/// CPU-side `GEM_CPU_PREP` waits for it.
pub(super) fn wait_for_foreign_producers(dev: &NvidiaGpu, ctx_idx: u32, owner_pid: u64) {
    if !enabled() {
        return;
    }
    // Both snapshots taken and the guards dropped before anything else: the
    // order documented in `nouveau_release_process` never holds `nouveau_gem`
    // across the locks below.
    let maps: Vec<(u32, u64)> = dev
        .nouveau_vm_mappings
        .lock()
        .iter()
        .map(|m| (m.gem_handle, m.owner_pid))
        .collect();
    let gems: Vec<(u32, u64)> = dev
        .nouveau_gem
        .lock()
        .iter()
        .map(|o| (o.handle, o.owner_pid))
        .collect();
    let foreign = foreign_owners(&maps, &gems, owner_pid);
    if foreign.is_empty() {
        return;
    }
    EXECS_WITH_DEPS.fetch_add(1, Ordering::Relaxed);
    for pid in foreign {
        if dev.nouveau_zombies.lock().iter().any(|z| z.pid == pid) {
            ZOMBIE_PRODUCER.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some(pctx) = producer_ctx(dev, pid) else {
            NO_FENCE.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if pctx == ctx_idx {
            // The producer submits on the very ring this EXEC goes to, so the
            // ring itself orders them.
            continue;
        }
        if pctx >= 1 && super::super::nouveau_uapi::ctx_is_wedged(pctx) {
            // Nothing on a jammed ring will ever land; nouveau answers the
            // same way, counting a fence signaled with an error as signaled.
            NO_FENCE.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some((fence_va, payload)) = last_issued_fence(dev, pctx) else {
            NO_FENCE.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if crate::scheme::syncobj::hw_fence_landed(fence_va, payload) {
            ALREADY_LANDED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        WAITED.fetch_add(1, Ordering::Relaxed);
        let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
        let mut spins = 0u32;
        loop {
            if crate::scheme::syncobj::hw_fence_landed(fence_va, payload) {
                break;
            }
            let waited = unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start);
            spins += 1;
            if waited >= WAIT_BUDGET_US || spins >= MAX_SPINS {
                TIMED_OUT.fetch_add(1, Ordering::Relaxed);
                crate::klog_warn!(
                    "[nouveau-uapi] implicit sync: pid={} submitting on ctx{} gave up after {}us \
                     ({} spins) on ctx{} (pid {}), which lent it a buffer and has not landed \
                     payload {} -- submitting anyway, so this frame may read a buffer still \
                     being written",
                    owner_pid,
                    ctx_idx,
                    waited,
                    spins,
                    pctx,
                    pid,
                    payload
                );
                break;
            }
            super::gpu_spin();
        }
        WAIT_US.fetch_add(
            unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start),
            Ordering::Relaxed,
        );
    }
}

/// `(fence landing zone, payload)` of the last fence context `ctx` issued, as
/// [`NvidiaGpu::ring_idle_probe`]'s own ring-full fallback reads it. `None`
/// when that context has never fenced.
fn last_issued_fence(dev: &NvidiaGpu, ctx: u32) -> Option<(usize, u32)> {
    match dev.nouveau_fast.lock().get(ctx as usize) {
        Some(super::FastSlot::Ready(f)) if f.fenced > 0 => {
            Some((f.fence_sem_va, f.next_payload.wrapping_sub(1)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_of_my_own_is_not_an_external_object() {
        let maps = [(7u32, 100u64)];
        let gems = [(7u32, 100u64)];
        assert!(foreign_owners(&maps, &gems, 100).is_empty());
    }

    #[test]
    fn a_buffer_another_process_allocated_is() {
        let maps = [(7u32, 100u64)];
        let gems = [(7u32, 42u64)];
        assert_eq!(foreign_owners(&maps, &gems, 100), alloc::vec![42]);
    }

    #[test]
    fn a_mapping_in_someone_elses_address_space_is_not_mine() {
        // The compositor's own mapping of its own buffer says nothing about
        // what THIS client's submission has to wait for.
        let maps = [(7u32, 42u64)];
        let gems = [(7u32, 42u64)];
        assert!(foreign_owners(&maps, &gems, 100).is_empty());
    }

    #[test]
    fn the_same_producer_is_named_once_however_many_buffers_it_lent() {
        let maps = [(7u32, 100u64), (8u32, 100u64), (9u32, 100u64)];
        let gems = [(7u32, 42u64), (8u32, 42u64), (9u32, 42u64)];
        assert_eq!(foreign_owners(&maps, &gems, 100), alloc::vec![42]);
    }

    #[test]
    fn two_producers_are_both_named_in_bind_order() {
        let maps = [(9u32, 100u64), (7u32, 100u64)];
        let gems = [(7u32, 42u64), (9u32, 43u64)];
        assert_eq!(foreign_owners(&maps, &gems, 100), alloc::vec![43, 42]);
    }

    #[test]
    fn a_mapping_whose_object_is_gone_is_skipped_not_guessed_at() {
        // GEM_CLOSE can defer the free behind a ring, so a mapping can
        // outlive the lookup; an unknown handle must not become a wait on
        // some other process.
        let maps = [(7u32, 100u64)];
        let gems = [(8u32, 42u64)];
        assert!(foreign_owners(&maps, &gems, 100).is_empty());
    }

    #[test]
    fn the_hatch_is_on_by_default() {
        assert!(enabled());
    }

    #[test]
    fn the_spin_backstop_is_there_for_a_clock_that_does_not_move() {
        // The unit-test clock only advances when a test advances it, so a
        // deadline alone would hang the suite rather than fail it -- which is
        // how this bound came to exist.
        assert!(MAX_SPINS > 0);
    }

    #[test]
    fn the_budget_is_three_frames_at_60_hz() {
        // Not a round number for its own sake: the comparison that matters is
        // against a frame, because the thing being waited for is one.
        assert_eq!(WAIT_BUDGET_US, 3 * 1_000_000 / 60 + 50_000 - 50_000);
        assert!(WAIT_BUDGET_US > 1_000_000 / 60);
    }

    /// The line `/proc/gpudbg` prints: the switch spelled out, then the
    /// seven counters in the order their doc comments above give them.
    #[test]
    fn the_profile_line_names_the_switch_and_every_counter_in_order() {
        let _serialised = crate::test_globals::LOCK.lock();
        let was = enabled();
        let counters: [(&AtomicU64, u64); 7] = [
            (&EXECS_WITH_DEPS, 7),
            (&WAITED, 6),
            (&WAIT_US, 5),
            (&ALREADY_LANDED, 4),
            (&NO_FENCE, 3),
            (&ZOMBIE_PRODUCER, 2),
            (&TIMED_OUT, 1),
        ];
        for (c, v) in counters.iter() {
            c.store(*v, Ordering::Relaxed);
        }
        set_enabled(true);
        assert_eq!(
            profile_line(),
            "[gpudbg]  implicit sync: ON -- execs with foreign buffers=7 waited=6 (5us) \
             already-landed=4 no-fence=3 zombie-producer=2 TIMED-OUT=1"
        );
        set_enabled(false);
        assert_eq!(
            profile_line(),
            "[gpudbg]  implicit sync: OFF (nvidia.implicit_sync=off) -- execs with foreign \
             buffers=7 waited=6 (5us) already-landed=4 no-fence=3 zombie-producer=2 TIMED-OUT=1"
        );
        set_enabled(was);
        for (c, _) in counters.iter() {
            c.store(0, Ordering::Relaxed);
        }
    }
}
