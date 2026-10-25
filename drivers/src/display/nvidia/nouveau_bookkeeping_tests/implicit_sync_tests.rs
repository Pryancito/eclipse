//! `implicit_sync::wait_for_foreign_producers` arm by arm, on state built
//! by hand. The EXEC tests reach the common arms (a fence already landed,
//! a deadline given up on); these are the ones they do not -- the hatch, a
//! dead producer, one with no ring to wait on, the same ring, a wedged
//! ring, and a fence that lands WHILE the wait is on. A child of
//! `nouveau_bookkeeping_tests` for its harness (`LOCK`, `gpu`, `object`,
//! `mapping`); a file of its own because the parent is at its ceiling.

use super::*;
use core::sync::atomic::AtomicU32;

/// The seven counters of the profile line, in print order: execs with
/// foreign buffers, waited, waited microseconds, already landed, no fence,
/// zombie producer, timed out.
fn counters() -> [u64; 7] {
    let line = implicit_sync::profile_line();
    let nums: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    assert!(nums.len() >= 7, "{}", line);
    let mut out = [0u64; 7];
    out.copy_from_slice(&nums[nums.len() - 7..]);
    out
}

/// What the counters moved since `before`; the microseconds are not
/// compared, the clock is the test's and says nothing about the arms.
fn moved(before: [u64; 7]) -> [u64; 7] {
    let mut now = counters();
    for (n, b) in now.iter_mut().zip(before) {
        *n -= b;
    }
    now[2] = 0;
    now
}

/// A buffer B allocated and A mapped into its own VA space: the one
/// external object that makes A's submission depend on B.
fn lend(gpu: &NvidiaGpu) {
    let h = object(gpu, B, 4096, None);
    mapping(gpu, A, h, 0x1_0000, 4096);
}

/// `pid` submits on context `ctx`; `ready` is the entry's last field.
fn entry(gpu: &NvidiaGpu, pid: u64, ctx: u32, ready: bool) {
    gpu.nouveau_pid_ctx.lock().push((pid, ctx, 0, 0, ready));
}

/// Context `ctx` has fenced once, releasing `payload` into a word of ours
/// that starts at 0 (not landed). The slot's other fields are no part of
/// this path.
fn fenced(gpu: &NvidiaGpu, ctx: u32, payload: u32) -> &'static AtomicU32 {
    let cell: &'static AtomicU32 =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicU32::new(0)));
    gpu.nouveau_fast.lock()[ctx as usize] = FastSlot::Ready(nv::FastCtx {
        userd_gpget: 0,
        userd_gpput: 0,
        gpfifo_va: 0,
        fence_pb_va: 0,
        fence_sem_va: cell.as_ptr() as usize,
        buf_gpu_va: 0,
        fence_pb_off: 0,
        fence_sem_off: 0,
        slot_bytes: 0,
        entries: 0,
        work_token: 0,
        runlist_id: 0,
        doorbell_va: 0,
        next_payload: payload + 1,
        submits: 1,
        fenced: 1,
        last_fence: Some((1, payload)),
        entries_put: 2,
    });
    cell
}

fn wait(gpu: &NvidiaGpu, ctx: u32, pid: u64) -> [u64; 7] {
    let before = counters();
    implicit_sync::wait_for_foreign_producers(gpu, ctx, pid);
    moved(before)
}

/// Puts back what a test turns: the hatch, the wedged flag of context 2
/// and the spin hook, whether the test passed or not.
struct Scene;
impl Drop for Scene {
    fn drop(&mut self) {
        implicit_sync::set_enabled(true);
        nv::ctx_clear_wedged(2);
        *GPU_SPIN_HOOK.lock() = None;
    }
}

const EXECS: [u64; 7] = [1, 0, 0, 0, 0, 0, 0];
const NO_FENCE: [u64; 7] = [1, 0, 0, 0, 1, 0, 0];

/// `nvidia.implicit_sync=off` is the old behaviour entire: not a counter
/// moves, however unlanded the producer's fence.
#[test]
fn with_the_hatch_off_nothing_is_looked_at() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    entry(&gpu, B, 2, true);
    let _unlanded = fenced(&gpu, 2, 5);
    implicit_sync::set_enabled(false);
    assert_eq!(wait(&gpu, 1, A), [0; 7]);
    implicit_sync::set_enabled(true);
    assert_eq!(
        wait(&gpu, 1, A),
        [1, 1, 0, 0, 0, 0, 1],
        "on, the same state is a wait given up on"
    );
}

/// A producer with nothing that could land is a dependency with no fence:
/// no entry at all, an entry whose ring is not ready, or a ready ring
/// that has never fenced. None of them is routed to context 0.
#[test]
fn a_producer_with_no_ring_to_wait_on_is_a_dependency_with_no_fence() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    assert_eq!(wait(&gpu, 1, A), NO_FENCE, "no entry, no RM channel");
    entry(&gpu, B, 2, false);
    assert_eq!(wait(&gpu, 1, A), NO_FENCE, "an entry that is not ready");
    gpu.nouveau_pid_ctx.lock().clear();
    entry(&gpu, B, 2, true);
    assert_eq!(wait(&gpu, 1, A), NO_FENCE, "a ring that never fenced");
}

/// A producer on the ring this EXEC goes to is ordered by the ring: the
/// dependency is counted and nothing is waited for. The owner of the RM
/// channel with no entry of its own submits on ring 0, so on ring 0 that
/// is the same ring, and from ring 1 it is a producer whose slot has
/// nothing to wait on.
#[test]
fn a_producer_on_the_same_ring_is_ordered_by_the_ring() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    entry(&gpu, B, 1, true);
    let _unlanded = fenced(&gpu, 1, 5);
    assert_eq!(wait(&gpu, 1, A), EXECS, "B and A share ring 1");
    gpu.nouveau_pid_ctx.lock().clear();
    gpu.nouveau_channels.lock().push(nv::NouveauChannelState {
        id: 7,
        h_vas: 0,
        notifier_handle: 0,
        rm_backed: true,
        ctx_idx: 0,
        owner_pid: B,
    });
    assert_eq!(wait(&gpu, 0, A), EXECS, "the RM channel's owner is ring 0");
    assert_eq!(wait(&gpu, 1, A), NO_FENCE, "and ring 0 has no slot here");
}

/// A dead producer whose ring is still draining is left alone, before its
/// fence is even looked at: the park landed its payload from the CPU, so
/// the fence says nothing, and what covers its buffer is the deferred free.
#[test]
fn a_zombie_producer_is_skipped_before_its_fence_is_read() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    entry(&gpu, B, 2, true);
    let _unlanded = fenced(&gpu, 2, 5);
    gpu.nouveau_zombies.lock().push(ZombieCtx {
        pid: B,
        ctx_idx: 2,
        born_us: 0,
        waiters: Vec::new(),
        submits_at_park: 0,
    });
    assert_eq!(wait(&gpu, 1, A), [1, 0, 0, 0, 0, 1, 0]);
}

/// Nothing on a jammed ring will ever land: a wedged producer is a
/// dependency with no fence, however unlanded the fence it last issued.
#[test]
fn a_wedged_producer_is_not_waited_for() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    entry(&gpu, B, 2, true);
    let cell = fenced(&gpu, 2, 5);
    nv::ctx_set_wedged(2);
    assert_eq!(wait(&gpu, 1, A), NO_FENCE);
    nv::ctx_clear_wedged(2);
    cell.store(5, Ordering::SeqCst);
    assert_eq!(
        wait(&gpu, 1, A),
        [1, 0, 0, 1, 0, 0, 0],
        "unwedged with the fence landed, nothing to wait for either"
    );
}

/// The wait ends when the fence lands, not at the deadline: the producer
/// writes its payload while the consumer spins (the spin hook is the GPU),
/// and the dependency counts as waited, not timed out.
#[test]
fn a_fence_that_lands_during_the_wait_ends_it() {
    let _g = LOCK.lock();
    let _s = Scene;
    let gpu = gpu();
    lend(&gpu);
    entry(&gpu, B, 2, true);
    let cell = fenced(&gpu, 2, 5);
    let spins = std::sync::Arc::new(AtomicU32::new(0));
    let seen = spins.clone();
    *GPU_SPIN_HOOK.lock() = Some(alloc::boxed::Box::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        cell.store(5, Ordering::SeqCst);
    }));
    assert_eq!(wait(&gpu, 1, A), [1, 1, 0, 0, 0, 0, 0]);
    *GPU_SPIN_HOOK.lock() = None;
    assert_eq!(
        spins.load(Ordering::SeqCst),
        1,
        "one spin, then it had landed"
    );
    assert_eq!(
        wait(&gpu, 1, A),
        [1, 0, 0, 1, 0, 0, 0],
        "and now it is already landed"
    );
}
