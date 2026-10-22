extern crate std;

use super::*;

/// [`pending_hw_fences`] where the caller expects at most one fence: the
/// handle's own, or the lone source behind its link.
fn pending_hw_fence(handle: u32, point: u64) -> Option<(usize, u64, u32, u32)> {
    let fences = pending_hw_fences(handle, point);
    assert!(
        fences.len() <= 1,
        "{handle:#x}@{point} has {} fences in flight; ask for the list",
        fences.len()
    );
    fences.first().copied()
}
use crate::nvme::nvme_queue::test_clock;
use core::cell::RefCell;

/// Every test here drives the one global [`TABLE`] and the two global
/// hook slots, and the fence clock is per-thread, so a test that winds
/// its own clock forward would time out another test's pending fences.
/// Serialise them. (CI runs with `--test-threads=1`, which hides exactly
/// this class of coupling, so the lock is not optional here.)
///
/// The lock is `crate::test_globals::LOCK`, shared with `resv` and the
/// nouveau bookkeeping tests, not a private one: those put fences in
/// this same `TABLE` under locks of their own, so with three locks the
/// families ran side by side and one wiped the table under another --
/// `links_now() == 0` here, `resv::count` of a closed buffer there,
/// five failures in a bundle one run in three. Each test takes the one
/// lock once, so there is no order to invert.
fn test_lock() -> crate::test_globals::Guard {
    crate::test_globals::LOCK.lock()
}

/// The CPU-side wait hands the table back between its looks.
///
/// `wait` is what `EXEC` falls back to for a fence it cannot hand the
/// hardware -- on a desktop, the compositor's release of the client's
/// buffer -- and it used to re-take `TABLE` on every turn of its spin,
/// for up to ten seconds, with one `spin_loop()` between turns. `TABLE`
/// is also the lock the SIGNALLING side takes, so that waiter was
/// starving the thread that would end its own wait.
///
/// What a test here can and cannot see: a `spin_loop()` turn costs
/// nothing on the test clock, so the number of looks over a fixed span
/// of virtual microseconds cannot go down here however hard the loop
/// backs off -- the saving is real time on real hardware, and this
/// binary has none. What it CAN pin is that the loop really applies the
/// backoff, per look and in order, and that it stops applying it the
/// moment there is an answer. Those are the two ways the wiring breaks.
#[test]
fn a_cpu_wait_hands_the_table_back_between_its_looks() {
    let _g = test_lock();
    let h = create(false);
    test_clock::set(1_000_000);
    test_clock::set_auto_advance(1);
    const SPAN_US: u64 = 20_000;

    let looks_before = WAIT_TABLE_PROBES.load(Ordering::Relaxed);
    let turns_before = WAIT_SPIN_TURNS.load(Ordering::Relaxed);
    let t0 = now_us();
    assert!(matches!(
        wait(&[h], None, true, t0 + SPAN_US),
        WaitOutcome::Timeout
    ));
    let looks = WAIT_TABLE_PROBES.load(Ordering::Relaxed) - looks_before;
    let turns = WAIT_SPIN_TURNS.load(Ordering::Relaxed) - turns_before;

    assert!(
        looks > 2,
        "the wait did not actually loop ({} looks)",
        looks
    );
    // Every look past the eager ones paused, and long enough to have
    // reached the cap: that pause is the lock handed back.
    assert!(
        turns >= (looks - WAIT_EAGER_PROBES as u64) * WAIT_SPIN_CAP as u64 / 2,
        "{} looks burned only {} spin turns between them",
        looks,
        turns
    );
    // And the wait still ends where it was told to, not later: the
    // backoff is a pause between looks, never an extension of the bound.
    assert!(
        now_us() >= t0 + SPAN_US && now_us() < t0 + SPAN_US * 2,
        "the wait ran to {} for a deadline of {}",
        now_us(),
        t0 + SPAN_US
    );

    // A syncobj already signalled is still seen on the first look and
    // pauses for nothing: the backoff must not cost the common case.
    assert!(signal(h));
    let looks_before = WAIT_TABLE_PROBES.load(Ordering::Relaxed);
    let turns_before = WAIT_SPIN_TURNS.load(Ordering::Relaxed);
    assert!(matches!(
        wait(&[h], None, true, now_us() + SPAN_US),
        WaitOutcome::Signaled { .. }
    ));
    assert_eq!(
        WAIT_TABLE_PROBES.load(Ordering::Relaxed) - looks_before,
        1,
        "a signalled syncobj cost more than one look"
    );
    assert_eq!(
        WAIT_SPIN_TURNS.load(Ordering::Relaxed) - turns_before,
        0,
        "a signalled syncobj paused before answering"
    );

    test_clock::set_auto_advance(0);
    assert!(destroy(h));
}

/// The backoff itself: no pause at all for the first looks, then growth,
/// then a cap. The cap matters as much as the floor -- this is a pause
/// measured in `spin_loop()` turns, and one that grew without bound
/// would turn "look less often" into "notice the fence late".
#[test]
fn the_cpu_waits_backoff_starts_at_zero_grows_and_settles() {
    assert!(WAIT_EAGER_PROBES > 0, "the first look already pauses");
    for probes in 0..WAIT_EAGER_PROBES {
        assert_eq!(
            wait_spin_backoff(probes),
            0,
            "look {} paused before it looked",
            probes
        );
    }
    assert_eq!(wait_spin_backoff(WAIT_EAGER_PROBES), WAIT_SPIN_FLOOR);
    let mut prev = 0;
    let mut seen_cap = false;
    for probes in WAIT_EAGER_PROBES..WAIT_EAGER_PROBES + 200 {
        let turns = wait_spin_backoff(probes);
        assert!(
            (WAIT_SPIN_FLOOR..=WAIT_SPIN_CAP).contains(&turns),
            "look {} pauses {} turns",
            probes,
            turns
        );
        assert!(
            turns >= prev,
            "look {} pauses less than the one before",
            probes
        );
        prev = turns;
        seen_cap |= turns == WAIT_SPIN_CAP;
    }
    assert!(seen_cap, "the backoff never reaches its cap");
    // A wait parked long enough to run the shift off the top of the `u32`
    // stays at the cap. Unclamped it comes back ZERO, which is the whole
    // spin back again and the lock hammered exactly as before.
    assert_eq!(wait_spin_backoff(u32::MAX), WAIT_SPIN_CAP);
}

/// The first probes cost no timer: a fence that lands in tens of
/// microseconds -- an ordinary glxgears frame -- is seen on a yield,
/// never a millisecond later.
#[test]
fn a_fence_poll_yields_before_it_ever_arms_a_timer() {
    // Spelled out, because a zero here makes the loop below vacuous and
    // the whole test green on a schedule that never yields at all.
    assert!(
        FENCE_POLL_YIELDS > 0,
        "the poll arms a timer on its first probe"
    );
    for probes in 0..FENCE_POLL_YIELDS {
        assert_eq!(
            fence_poll_step(probes),
            PollStep::Yield,
            "probe {} armed a timer",
            probes
        );
    }
    assert_eq!(
        fence_poll_step(FENCE_POLL_YIELDS),
        PollStep::Sleep {
            us: FENCE_POLL_FLOOR_US
        }
    );
}

/// No sleep is ever asked for below the timer's arming floor (it would
/// not wake any sooner) nor above the fixed tick this replaces.
#[test]
fn a_fence_poll_sleeps_between_the_timer_floor_and_the_old_tick() {
    let mut seen_cap = false;
    for probes in FENCE_POLL_YIELDS..FENCE_POLL_YIELDS + 200 {
        let PollStep::Sleep { us } = fence_poll_step(probes) else {
            panic!("probe {} still yielding", probes);
        };
        assert!(
            (FENCE_POLL_FLOOR_US..=FENCE_POLL_CAP_US).contains(&us),
            "probe {} sleeps {} us",
            probes,
            us
        );
        seen_cap |= us == FENCE_POLL_CAP_US;
    }
    assert!(seen_cap, "the backoff never reaches its cap");
    // And a wait parked long enough to overflow the shift stays at the
    // cap instead of panicking in a debug build.
    assert_eq!(
        fence_poll_step(u32::MAX),
        PollStep::Sleep {
            us: FENCE_POLL_CAP_US
        }
    );
}

/// The backoff only ever moves outward, and it reaches the cap in about
/// a millisecond and a half, so a genuinely late fence costs no more
/// wakeups than the fixed tick did.
#[test]
fn a_fence_poll_backs_off_monotonically_and_settles_at_the_cap() {
    let mut prev = 0;
    let mut elapsed = 0;
    let mut probes = FENCE_POLL_YIELDS;
    while let PollStep::Sleep { us } = fence_poll_step(probes) {
        assert!(
            us >= prev,
            "probe {} sleeps less than the one before",
            probes
        );
        if us == FENCE_POLL_CAP_US {
            break;
        }
        prev = us;
        elapsed += us;
        probes += 1;
    }
    assert!(
        elapsed <= 2_000,
        "{} us of short probes before the cap",
        elapsed
    );
    // A 100 ms bound (the pre-present fence's) costs about as many
    // wakeups as the old flat 1 ms tick did: 100, give or take the
    // handful of short ones at the front.
    let mut wakes = 0;
    let mut t = 0;
    let mut probes = FENCE_POLL_YIELDS;
    while t < 100_000 {
        let PollStep::Sleep { us } = fence_poll_step(probes) else {
            unreachable!()
        };
        t += us;
        wakes += 1;
        probes += 1;
    }
    assert!(wakes <= 110, "{} wakeups to wait out 100 ms", wakes);
}

std::thread_local! {
    static SIGNALS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    /// `(ctx_idx, handle, point)` of each fence the timeout hook was
    /// fired for.
    static TIMEOUTS: RefCell<Vec<(u32, u32, u64)>> = const { RefCell::new(Vec::new()) };
}

fn record_signal(handle: u32, point: u64) {
    SIGNALS.with(|s| s.borrow_mut().push((handle, point)));
}

fn record_timeout(ctx_idx: u32, _va: usize, _payload: u32, handle: u32, point: u64) {
    TIMEOUTS.with(|s| s.borrow_mut().push((ctx_idx, handle, point)));
}

/// Install the upcalls and start from a clean slate. The hooks are the
/// real registration path (`set_signal_hook` is what `linux-object` calls
/// at boot to drive `SYNCOBJ_EVENTFD`), so what these tests observe is
/// what an eventfd waiter would.
fn arm_hooks() {
    set_signal_hook(record_signal);
    set_fence_timeout_hook(record_timeout);
    SIGNALS.with(|s| s.borrow_mut().clear());
    TIMEOUTS.with(|s| s.borrow_mut().clear());
}

fn signals() -> Vec<(u32, u64)> {
    SIGNALS.with(|s| s.borrow().clone())
}

fn timeouts() -> Vec<(u32, u32, u64)> {
    TIMEOUTS.with(|s| s.borrow().clone())
}

fn pending_now() -> usize {
    PENDING_COUNT.load(Ordering::Relaxed)
}

/// A fence landing zone the test drives by hand, standing in for the
/// sysmem word the GPU's host-semaphore RELEASE writes.
struct Landing(alloc::boxed::Box<u32>);

impl Landing {
    fn new() -> Self {
        Self(alloc::boxed::Box::new(0))
    }
    fn va(&self) -> usize {
        &*self.0 as *const u32 as usize
    }
    fn land(&mut self, payload: u32) {
        *self.0 = payload;
    }
}

/// Two fences of the same channel share one landing zone and are told
/// apart only by their payload, so what [`resolve_locked`] remembers of a
/// zone it has already read has to be the WORD -- the payload decides
/// against it, every time.
///
/// This is how a channel really works: the semaphore word is per channel,
/// and each RELEASE writes a higher payload into it. Remember a
/// landed/not-landed verdict per address instead and the first fence's
/// "landed" answers for every later one on the same channel -- every
/// syncobj of that channel jumps to its last point the moment the first
/// frame lands, and a client goes on drawing into a buffer the GPU is
/// still reading.
#[test]
fn one_landing_zone_carries_two_payloads_and_they_resolve_apart() {
    let _g = test_lock();
    let early = create(false);
    let late = create(false);
    let mut zone = Landing::new();
    assert!(attach_hw_fence(early, 1, zone.va(), 0, 1, 0, true));
    assert!(attach_hw_fence(late, 1, zone.va(), 0, 5, 0, true));
    assert_eq!(pending_now(), 2);

    // The GPU has written 1: the first fence has landed, the second has
    // not, and one resolve must say exactly that.
    zone.land(1);
    poll_pending();
    assert_eq!(query(early), Some(1), "payload 1 has landed");
    assert_eq!(
        query(late),
        Some(0),
        "payload 5 answered with payload 1's landing"
    );

    zone.land(5);
    poll_pending();
    assert_eq!(query(late), Some(1));
    assert!(destroy(early) && destroy(late));
}

/// ...and it tells them apart having read the word ONCE, not once per
/// fence.
///
/// The zone is pinned sysmem the GPU writes and the CPU maps uncached, so
/// every read of it leaves the CPU, while the compare against a payload is
/// free. A channel with four frames in flight put four fences in the
/// pending list at one address; a resolve read that one word four times.
#[test]
fn a_resolve_reads_one_landing_zone_once_however_many_fences_sit_in_it() {
    let _g = test_lock();
    let mut zone = Landing::new();
    let objs: Vec<u32> = (0..4).map(|_| create(false)).collect();
    for (i, &h) in objs.iter().enumerate() {
        assert!(attach_hw_fence(h, 1, zone.va(), 0, 10 + i as u32, 0, true));
    }
    assert_eq!(pending_now(), 4, "four fences, one zone");

    // Nothing has landed: the resolve has to look, and one look answers
    // all four payloads.
    FENCE_READS.store(0, Ordering::Relaxed);
    poll_pending();
    let reads = FENCE_READS.load(Ordering::Relaxed);
    assert_eq!(
        reads,
        1,
        "one resolve read the one landing zone {} times for {} fences",
        reads,
        objs.len()
    );
    for &h in &objs {
        assert_eq!(query(h), Some(0), "nothing has landed yet");
    }

    // And the word still decides per payload: 11 releases the first two
    // and holds the last two.
    zone.land(11);
    poll_pending();
    let reached: Vec<Option<u64>> = objs.iter().map(|&h| query(h)).collect();
    assert_eq!(
        reached,
        alloc::vec![Some(1), Some(1), Some(0), Some(0)],
        "one word, four payloads, and the word only answers the two below it"
    );

    zone.land(13);
    poll_pending();
    for &h in &objs {
        assert_eq!(query(h), Some(1));
    }
    for &h in &objs {
        assert!(destroy(h));
    }
}

/// [`hw_fences_landed`] is `fences.iter().all(hw_fence_landed)` with the
/// reads collapsed: same answer, one read per ADDRESS.
///
/// This is the list the present pre-wait and `GEM_CPU_PREP` hold, and
/// they take it again on every probe of a poll loop. A buffer two submits
/// of one ring wrote names that ring's one semaphore word twice.
#[test]
fn a_list_of_fences_reads_each_landing_zone_once_and_answers_like_all() {
    let _g = test_lock();
    let mut ring_a = Landing::new();
    let mut ring_b = Landing::new();
    // Three submits of ring A and two of ring B wrote this buffer.
    let fences = [
        (ring_a.va(), 7u32),
        (ring_b.va(), 3u32),
        (ring_a.va(), 8u32),
        (ring_b.va(), 4u32),
        (ring_a.va(), 9u32),
    ];
    let by_hand = |fences: &[(usize, u32)]| fences.iter().all(|&(va, p)| hw_fence_landed(va, p));

    // Nothing landed: the answer is no, and it cost two reads (it stops
    // at the first fence in flight, which here is the first one).
    FENCE_READS.store(0, Ordering::Relaxed);
    assert!(!hw_fences_landed(&fences));
    assert!(FENCE_READS.load(Ordering::Relaxed) <= 2);

    // Ring A all the way, ring B one short: still no, and now the whole
    // list is walked -- five fences, two addresses, two reads.
    ring_a.land(9);
    ring_b.land(3);
    assert!(!by_hand(&fences), "ring B is one submit short");
    FENCE_READS.store(0, Ordering::Relaxed);
    assert!(!hw_fences_landed(&fences));
    assert_eq!(
        FENCE_READS.load(Ordering::Relaxed),
        2,
        "five fences in two zones cost more than two reads"
    );

    ring_b.land(4);
    assert!(by_hand(&fences));
    FENCE_READS.store(0, Ordering::Relaxed);
    assert!(hw_fences_landed(&fences));
    assert_eq!(FENCE_READS.load(Ordering::Relaxed), 2);
}

/// The word runs past the top of a u32 and keeps counting, so the compare
/// is a signed difference and not `>=`. Collapsing the reads must not
/// quietly turn it into one.
#[test]
fn a_landing_zone_that_has_wrapped_still_answers_the_payloads_behind_it() {
    let _g = test_lock();
    let mut zone = Landing::new();
    zone.land(3);
    // 3 is four ticks past 0xffff_ffff.
    assert!(hw_fences_landed(&[(zone.va(), 0xffff_ffff)]));
    assert!(hw_fences_landed(&[
        (zone.va(), 0xffff_ffff),
        (zone.va(), 3)
    ]));
    assert!(!hw_fences_landed(&[(zone.va(), 4)]));
    // Half the range away is "not yet", not "long past".
    assert!(!hw_fences_landed(&[(zone.va(), 0x8000_0003)]));
}

/// A fence given up on after [`FENCE_TIMEOUT_US`] still advances the
/// point (QUERY and WAIT read it as reached, like a `dma_fence` signaled
/// with an error), but [`reached_by_timeout`] remembers which points got
/// there that way -- on the handle, and on every import, transfer and
/// merge that took the point from it -- until the CPU's own signal, a
/// re-arm or a reset replaces the dead fence.
#[test]
fn a_point_reached_by_a_timed_out_fence_is_remembered_and_carried_by_its_links() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let mut first = Landing::new();
    let dead = Landing::new();
    assert!(attach_hw_fence(src, 2, first.va(), 0, 1, 0, false));
    first.land(1);
    assert_eq!(query(src), Some(2), "point 2 is the GPU's word");
    assert!(attach_hw_fence(src, 5, dead.va(), 0, 2, 0, false));
    // Links made while the fence is still in flight: they materialise
    // when it is given up on.
    let importer = create(false);
    assert!(import_snapshot(importer, src, 5));
    let moved = create(false);
    assert!(transfer(moved, 9, src, 5));
    let merged = merge_fences(&[(src, 5)]);
    assert!(!reached_by_timeout(&[src], Some(&[5])), "still in flight");
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    assert_eq!(query(src), Some(5));
    assert_eq!(
        timeouts().len(),
        2,
        "the fence itself, and its copy the transfer put on `moved`"
    );
    assert!(
        !reached_by_timeout(&[src], Some(&[2])),
        "2 was reached for real"
    );
    assert!(reached_by_timeout(&[src], Some(&[3])));
    assert!(reached_by_timeout(&[src], Some(&[5])));
    assert!(
        !reached_by_timeout(&[src], Some(&[6])),
        "6 is not reached at all, by anything"
    );
    assert_eq!(query(importer), Some(1));
    assert!(reached_by_timeout(&[importer], None));
    assert_eq!(query(moved), Some(9));
    assert!(reached_by_timeout(&[moved], Some(&[9])));
    assert!(
        reached_by_timeout(&[moved], Some(&[0])),
        "0 is the binary fence"
    );
    assert_eq!(query(merged), Some(1));
    assert!(reached_by_timeout(&[merged], Some(&[1])));
    // Links made AFTER the fence died take the point, and the taint,
    // at once.
    let late_import = create(false);
    assert!(import_snapshot(late_import, src, 5));
    assert!(reached_by_timeout(&[late_import], Some(&[1])));
    let late_move = create(false);
    assert!(transfer(late_move, 3, src, 5));
    assert!(reached_by_timeout(&[late_move], Some(&[3])));
    let late_merge = merge_fences(&[(src, 5)]);
    assert!(reached_by_timeout(&[late_merge], Some(&[1])));
    // But not from the good point.
    let good_import = create(false);
    assert!(import_snapshot(good_import, src, 2));
    assert!(!reached_by_timeout(&[good_import], Some(&[1])));
    // The whole list: one dead handle among good ones is enough.
    assert!(reached_by_timeout(&[good_import, late_move], Some(&[1, 3])));
    // What replaces the dead fence clears it: the CPU's own signal at or
    // past the point...
    assert!(timeline_signal(src, 5));
    assert!(!reached_by_timeout(&[src], Some(&[5])));
    assert!(!reached_by_timeout(&[src], Some(&[3])));
    // ...a reset...
    assert!(reset(late_move));
    assert!(!reached_by_timeout(&[late_move], Some(&[3])));
    // ...and a binary re-arm on the slot, which the next landing then
    // makes good.
    let mut again = Landing::new();
    assert!(attach_hw_fence(importer, 1, again.va(), 0, 1, 0, true));
    assert_eq!(query(importer), Some(0));
    assert!(!reached_by_timeout(&[importer], None));
    again.land(1);
    assert_eq!(query(importer), Some(1));
    assert!(!reached_by_timeout(&[importer], None));
    // A signal below the dead point leaves it dead.
    assert!(timeline_signal(moved, 4));
    assert!(reached_by_timeout(&[moved], Some(&[9])));
    // Gone is gone: the mark leaves with the object.
    assert!(destroy(moved));
    assert!(!reached_by_timeout(&[moved], Some(&[9])));
    assert_eq!(
        TABLE
            .lock()
            .errored
            .iter()
            .filter(|e| e.handle == moved)
            .count(),
        0
    );
    for h in [
        src,
        importer,
        merged,
        late_import,
        late_merge,
        good_import,
        late_move,
    ] {
        assert!(destroy(h));
    }
    assert!(TABLE.lock().errored.is_empty(), "nothing left behind");
}

/// EXEC turns a wait into an ACQUIRE of the fence that delivers its
/// point, and a point the counter has already reached has no such
/// fence: the wait is over. The list looked only at what was still in
/// flight, so a timeline wait on a point already delivered came back
/// with the NEXT fence pending on the handle, and a wait on point 1,
/// read as binary, with the HIGHEST: a consumer that had waited for
/// frame N stalled behind frame N+1, which nobody asked it to wait for.
/// Linux drops a dependency on a signaled `dma_fence` before the job
/// is scheduled.
#[test]
fn a_wait_on_a_point_already_reached_has_nothing_to_acquire_whatever_is_still_in_flight() {
    let _g = test_lock();
    arm_hooks();
    let tl = create(false);
    let mut z = Landing::new();
    for point in 1..=3u64 {
        assert!(attach_hw_fence(
            tl,
            point,
            z.va(),
            0,
            point as u32,
            0,
            false
        ));
    }
    z.land(2);
    assert_eq!(query(tl), Some(2), "points 1 and 2 are the GPU's word");
    assert_eq!(
        pending_hw_fences(tl, 2),
        [],
        "point 2 is reached: nothing to acquire, not the fence of point 3"
    );
    assert_eq!(
        pending_hw_fences(tl, 1),
        [],
        "point 1 is reached too, and no binary wait on the highest fence in flight"
    );
    assert_eq!(
        pending_hw_fences(tl, 3),
        [(z.va(), 0, 3, 0)],
        "point 3 is the fence still in flight"
    );
    // A binary syncobj re-armed is at 0 with its fence in flight; one
    // signaled by the CPU has no fence at all. Neither changes.
    let b = create(true);
    assert_eq!(pending_hw_fences(b, 1), []);
    let mut zb = Landing::new();
    assert!(attach_hw_fence(b, 1, zb.va(), 0, 7, 1, true));
    assert_eq!(pending_hw_fences(b, 1), [(zb.va(), 0, 7, 1)]);
    zb.land(7);
    assert_eq!(query(b), Some(1));
    assert_eq!(pending_hw_fences(b, 1), []);
    z.land(3);
    assert_eq!(query(tl), Some(3));
    for h in [tl, b] {
        assert!(destroy(h));
    }
}

/// The bug this guards: a binary syncobj is a fence SLOT, not a counter.
/// Signalling it a second time used to hit `point <= obj.point` (its point
/// is always 1) and drop the new fence on the floor, leaving the object
/// reading "signaled" while the GPU was still writing — so the next waiter
/// went straight through and sampled a half-written buffer.
#[test]
fn a_second_binary_signal_rearms_the_syncobj_on_the_new_fence() {
    let _g = test_lock();
    let h = create(false);
    let mut first = Landing::new();
    let mut second = Landing::new();

    // First submit signals it; the fence lands; the object is signaled.
    assert!(attach_hw_fence(h, 1, first.va(), 0, 1, 0, true));
    assert_eq!(query(h), Some(0), "unsignaled until the fence lands");
    first.land(1);
    assert_eq!(query(h), Some(1), "signaled once the GPU wrote the fence");

    // Second submit signals the SAME handle. The object must go back to
    // unsignaled and wait for THIS fence, not report the old one.
    assert!(attach_hw_fence(h, 1, second.va(), 0, 1, 0, true));
    assert_eq!(
        query(h),
        Some(0),
        "a re-signalled binary syncobj must not still read as signaled"
    );
    // ...while LAST_SUBMITTED and WAIT_AVAILABLE still see the submit, and
    // EXEC can still find the fence to turn into a GPU-side ACQUIRE.
    assert_eq!(query_submitted(h), Some(1));
    assert_eq!(
        pending_hw_fence(h, 1).map(|f| f.0),
        Some(second.va()),
        "the pending fence must be the new one"
    );

    second.land(1);
    assert_eq!(query(h), Some(1));

    destroy(h);
}

/// The timeline flavour keeps its monotonic-counter semantics: a fence for
/// a point already reached is genuinely moot and stays dropped.
#[test]
fn a_timeline_fence_at_or_below_the_current_point_is_still_dropped() {
    let _g = test_lock();
    let h = create(false);
    let mut fence = Landing::new();

    assert!(attach_hw_fence(h, 5, fence.va(), 0, 1, 0, false));
    fence.land(1);
    assert_eq!(query(h), Some(5));

    // Point 3 is behind the timeline: nothing to wait for, and the object
    // must not be rewound.
    let stale = Landing::new();
    assert!(attach_hw_fence(h, 3, stale.va(), 0, 7, 0, false));
    assert_eq!(query(h), Some(5));
    assert!(pending_hw_fence(h, 3).is_none());

    destroy(h);
}

/// A transfer whose source has not landed yet has to remember WHICH point
/// it was asked to reach. Landing `dst` at 1 instead looks like a rounding
/// error and is a hang: wlroots' `linux-drm-syncobj-v1` moves a client's
/// acquire point into its own timeline at a point it picks, then waits on
/// that point, so a point that never arrives freezes the surface and, with
/// the commit waiting on it, the whole output.
#[test]
fn a_deferred_transfer_reaches_the_point_it_was_given() {
    let _g = test_lock();
    let src = create(false);
    let dst = create(false);

    // `src` is nowhere near point 5 yet, so the transfer is deferred.
    assert!(transfer(dst, 9, src, 5));
    assert_eq!(query(dst), Some(0), "nothing has landed yet");

    // `src` arrives. `dst` must now be at 9, not 1.
    assert!(timeline_signal(src, 5));
    assert_eq!(
        query(dst),
        Some(9),
        "a deferred transfer must reach its destination point"
    );
    // And a wait on that point is satisfied, which is the thing that hung.
    assert!(matches!(
        wait(&[dst], Some(&[9]), false, 0),
        WaitOutcome::Signaled { .. }
    ));

    destroy(dst);
    destroy(src);
}

/// Mesa's `vk_drm_syncobj_wait_many` uses the TIMELINE wait whenever any
/// entry in the batch carries a non-zero value, and passes wait_value 0 for
/// the BINARY syncs sitting in that same batch. Point 0 therefore has to
/// mean "wait for this syncobj's current fence", as `drm_syncobj_find_fence`
/// makes it in Linux -- never "already satisfied". It returning signaled is
/// silent: every caller gets VK_SUCCESS and races the GPU.
#[test]
fn a_timeline_wait_on_point_zero_still_waits() {
    let _g = test_lock();
    let h = create(false);
    assert_eq!(query(h), Some(0));

    // Unsignaled, no fence attached: point 0 must NOT report signaled.
    assert!(matches!(
        wait(&[h], Some(&[0]), false, 0),
        WaitOutcome::Timeout
    ));
    // And the readiness probe must agree: `None` is "sleep and retry",
    // i.e. not satisfied. (A zero deadline would report Timeout instead,
    // which is the same verdict by another name.)
    assert!(wait_ready(&[h], Some(&[0]), false, u64::MAX).is_none());

    // Once it IS signaled, point 0 is satisfied.
    assert!(signal(h));
    assert!(matches!(
        wait(&[h], Some(&[0]), false, 0),
        WaitOutcome::Signaled {
            first_signaled_index: 0
        }
    ));

    destroy(h);
}

/// A binary signal on a handle userspace has already driven past 1 as a
/// timeline must take the timeline branch, not the slot-replacement one.
/// Rewinding is wrong there (it would un-signal a real timeline), and so
/// is purging the handle's pending fences: the first cut of the binary
/// fix dropped them unconditionally, which threw away a timeline fence
/// still in flight. Nothing was then left to signal its point when the
/// GPU landed it, so `pending_hw_fence` returned `None`, `query` stayed
/// below the point, and a waiter on it hung.
#[test]
fn a_binary_signal_does_not_strand_a_timeline_fence_in_flight() {
    let _g = test_lock();
    let h = create(false);
    let mut reached = Landing::new();
    let inflight = Landing::new();

    // Drive the handle to 5 as a timeline.
    assert!(attach_hw_fence(h, 5, reached.va(), 0, 1, 0, false));
    reached.land(1);
    assert_eq!(query(h), Some(5));

    // Point 7 is submitted and still in flight.
    assert!(attach_hw_fence(h, 7, inflight.va(), 0, 3, 0, false));
    assert_eq!(query_submitted(h), Some(7));

    // A stray binary signal arrives on the same handle.
    let stray = Landing::new();
    assert!(attach_hw_fence(h, 1, stray.va(), 0, 1, 0, true));

    assert_eq!(
        query(h),
        Some(5),
        "a binary signal must not rewind a handle already past binary range"
    );
    assert_eq!(
        pending_hw_fence(h, 7).map(|f| f.0),
        Some(inflight.va()),
        "the in-flight timeline fence must survive the binary signal"
    );
    assert_eq!(query_submitted(h), Some(7));

    destroy(h);
}

/// The same stranding, one arm over: a handle still INSIDE binary range
/// (point 0 or 1) with a timeline fence in flight at a higher point. The
/// binary arm rewinds the counter, which is right, and used to purge every
/// pending fence on the handle, which is not -- the in-flight one is not
/// superseded by a binary signal, and with it gone nothing would ever
/// advance the counter to its point.
#[test]
fn a_binary_signal_in_binary_range_spares_a_higher_timeline_fence() {
    let _g = test_lock();
    let h = create(false);
    let inflight = Landing::new();

    // Point 7 submitted while the handle is still at 0.
    assert!(attach_hw_fence(h, 7, inflight.va(), 0, 3, 0, false));
    assert_eq!(query(h), Some(0));
    assert_eq!(query_submitted(h), Some(7));

    // A binary signal arrives on the same handle.
    let stray = Landing::new();
    assert!(attach_hw_fence(h, 1, stray.va(), 0, 1, 0, true));

    let vas: Vec<usize> = pending_hw_fences(h, 7).into_iter().map(|f| f.0).collect();
    assert!(
        vas.contains(&inflight.va()),
        "the in-flight timeline fence must survive a binary signal"
    );
    // ...and the slot's own fence is the node below it in the chain: 7
    // counts once both have landed.
    assert_eq!(vas.len(), 2);
    assert!(vas.contains(&stray.va()));
    assert_eq!(query_submitted(h), Some(7));

    destroy(h);
}

/// Closing a syncobj that still has a submit in flight is the ordinary
/// end of a frame's semaphore, and process teardown does it to every
/// handle a client owns (`drm_scheme.rs`'s release path). The fences it
/// left behind outlived it: the resolver could no longer find the object
/// to advance, nobody could wait on it, and 10 s later the timeout hook
/// fired and latched the GPU context WEDGED -- so the client's NEXT
/// submit (or another process sharing the channel) failed with
/// EIO/device-lost, for a syncobj that had simply been closed.
#[test]
fn destroying_a_syncobj_drops_the_timeline_fences_still_in_flight() {
    let _g = test_lock();
    arm_hooks();
    const CTX: u32 = 0x5e_71;

    let before = pending_now();
    let h = create(false);
    let fences = [Landing::new(), Landing::new(), Landing::new()];
    // Binary range, an ordinary timeline point, and the far end of the
    // counter: which fences go must not depend on the point at all. Only
    // the first of the three was dropped before, and the filter's blind
    // spot is everything above binary range.
    for (i, point) in [1u64, 7, u64::MAX].iter().enumerate() {
        assert!(attach_hw_fence(h, *point, fences[i].va(), 0, 3, CTX, false));
    }
    assert_eq!(pending_now(), before + 3, "three fences in flight");

    assert!(destroy(h));
    assert_eq!(
        pending_now(),
        before,
        "destroying a syncobj must drop every fence in flight on it, not \
         just the ones inside binary range"
    );

    // Past the hang deadline the orphan used to reach the timeout hook.
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    poll_pending();
    assert!(
        timeouts().iter().all(|&(ctx, ..)| ctx != CTX),
        "a closed syncobj must not wedge the context its fence was on"
    );
}

/// `SYNCOBJ_RESET` is `drm_syncobj_replace_fence(syncobj, NULL)`: the
/// object carries no fence afterwards. A timeline fence left in flight
/// across the reset landed later and drove the counter back up on its
/// own, so a syncobj Mesa had just reset for reuse re-signaled itself
/// with nobody having signaled it.
#[test]
fn resetting_a_syncobj_drops_the_timeline_fence_it_carried() {
    let _g = test_lock();

    let h = create(false);
    let mut fence = Landing::new();
    assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
    assert_eq!(query_submitted(h), Some(7));

    assert!(reset(h));
    assert_eq!(
        query_submitted(h),
        Some(0),
        "a reset syncobj carries no fence, in flight ones included"
    );

    // The GPU writes the landing zone anyway: the fence was real, it just
    // no longer belongs to anything.
    fence.land(3);
    assert_eq!(
        query(h),
        Some(0),
        "a reset syncobj must not re-signal itself from the fence it had"
    );

    destroy(h);
}

/// A deferred transfer names its source BY HANDLE, because there is no
/// refcounted `dma_fence` here to hand over. So destroying the source --
/// a client closing its acquire syncobj, or exiting -- left the
/// destination linked to a handle that no longer resolves: its point
/// could never advance again. That is wlroots' `linux-drm-syncobj-v1`
/// timeline, and NVK waits on it with an INT64_MAX deadline, so the
/// surface (and the output committing it) froze for good.
#[test]
fn destroying_a_transfer_source_releases_the_destination_it_promised() {
    let _g = test_lock();
    let src = create(false);
    let dst = create(false);

    // `src` is nowhere near point 5, so the transfer stays deferred.
    assert!(transfer(dst, 9, src, 5));
    assert_eq!(query(dst), Some(0), "nothing has landed yet");

    arm_hooks();
    assert!(destroy(src));
    assert!(
        signals().contains(&(dst, 9)),
        "and an eventfd armed on that point has to be woken -- reading the \
         table is not how a waiter finds out, got {:?}",
        signals()
    );
    assert_eq!(
        query(dst),
        Some(9),
        "the destination of a transfer whose source went away must be \
         released to the point it was promised, not parked forever"
    );
    assert!(matches!(
        wait(&[dst], Some(&[9]), false, 0),
        WaitOutcome::Signaled { .. }
    ));

    destroy(dst);
}

/// Every table access resolves the pending fences first, and that
/// resolution is DESTRUCTIVE: the fence is taken out of the table and the
/// counter advanced before the caller's own work begins. The upcalls it
/// produced are handed back to be made once the lock is released. An
/// ioctl that then bailed out on an unknown handle dropped that parcel on
/// the floor, and there was nothing left to retry it with: the
/// `SYNCOBJ_EVENTFD` waiter for the point that had just been reached was
/// never woken, which for a compositor is a surface that never repaints.
/// One `SYNCOBJ_FD_TO_HANDLE` with a stale handle -- a race any client
/// can lose at teardown -- was enough.
#[test]
fn a_failed_import_still_delivers_the_signals_it_resolved() {
    let _g = test_lock();
    const GONE: u32 = 0xdead_beef;

    // Bad DESTINATION: the source resolves, the destination does not.
    let h = create(false);
    let mut fence = Landing::new();
    assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
    arm_hooks();
    // Land it WITHOUT touching the table, so the resolution happens
    // inside the failing call.
    fence.land(3);
    assert!(!import_snapshot(GONE, h, 1), "unknown destination handle");
    assert!(
        signals().contains(&(h, 7)),
        "the fence that landed inside the failing import must still wake \
         its waiters, got {:?}",
        signals()
    );

    // Bad SOURCE: the call gives up before it even looks at the
    // destination, one return earlier.
    let h2 = create(false);
    let mut fence2 = Landing::new();
    assert!(attach_hw_fence(h2, 7, fence2.va(), 0, 3, 0, false));
    arm_hooks();
    fence2.land(3);
    assert!(!import_snapshot(h, GONE, 1), "unknown source handle");
    assert!(signals().contains(&(h2, 7)), "got {:?}", signals());

    destroy(h2);
    destroy(h);
}

/// [`transfer`] has the same two exits, and loses the same parcel.
#[test]
fn a_failed_transfer_still_delivers_the_signals_it_resolved() {
    let _g = test_lock();
    const GONE: u32 = 0xdead_beef;

    let h = create(false);
    let mut fence = Landing::new();
    assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, 0, false));
    arm_hooks();
    fence.land(3);
    assert!(!transfer(GONE, 1, h, 1), "unknown destination handle");
    assert!(signals().contains(&(h, 7)), "got {:?}", signals());

    let h2 = create(false);
    let mut fence2 = Landing::new();
    assert!(attach_hw_fence(h2, 7, fence2.va(), 0, 3, 0, false));
    arm_hooks();
    fence2.land(3);
    assert!(!transfer(h, 1, GONE, 1), "unknown source handle");
    assert!(signals().contains(&(h2, 7)), "got {:?}", signals());

    destroy(h2);
    destroy(h);
}

/// The other half of the dropped parcel: a fence that did NOT land within
/// [`FENCE_TIMEOUT_US`] is a hung ring, and the upcall is how the driver
/// learns of it -- it latches the context wedged so the next submit fails
/// honestly instead of the client waiting on a GPU that will never
/// answer. Losing it turns a reported hang into a silent one.
#[test]
fn a_failed_ioctl_still_reports_the_fence_that_timed_out() {
    let _g = test_lock();
    const CTX: u32 = 0x7a_11;
    const GONE: u32 = 0xdead_beef;

    let h = create(false);
    let fence = Landing::new();
    assert!(attach_hw_fence(h, 7, fence.va(), 0, 3, CTX, false));

    test_clock::advance(FENCE_TIMEOUT_US + 1);
    arm_hooks();
    assert!(!import_snapshot(GONE, h, 1), "unknown destination handle");
    assert!(
        timeouts().contains(&(CTX, h, 7)),
        "a hung fence resolved inside a failing ioctl must still reach the \
         timeout hook, got {:?}",
        timeouts()
    );

    destroy(h);
}

/// The two fixes have to stay independent: dropping a destroyed handle's
/// fences must not touch anybody else's, and the links it releases are
/// only the ones that named it.
#[test]
fn destroying_one_syncobj_leaves_the_others_alone() {
    let _g = test_lock();
    let keep = create(false);
    let doomed = create(false);
    let other_src = create(false);
    let linked = create(false);

    let kept_fence = Landing::new();
    let doomed_fence = Landing::new();
    assert!(attach_hw_fence(keep, 7, kept_fence.va(), 0, 3, 0, false));
    assert!(attach_hw_fence(
        doomed,
        7,
        doomed_fence.va(),
        0,
        3,
        0,
        false
    ));
    // `linked` waits on a source that is NOT the one being destroyed.
    assert!(transfer(linked, 9, other_src, 5));

    let before = pending_now();
    assert!(destroy(doomed));
    assert_eq!(before - 1, pending_now(), "only its own fence goes");
    assert_eq!(
        query_submitted(keep),
        Some(7),
        "another handle's fence must survive"
    );
    assert_eq!(
        query(linked),
        Some(0),
        "a link on a different source must not be released"
    );

    // And the link still works once its real source arrives.
    assert!(timeline_signal(other_src, 5));
    assert_eq!(query(linked), Some(9));

    destroy(linked);
    destroy(other_src);
    destroy(keep);
}

#[cfg(test)]
fn object_count() -> usize {
    TABLE.lock().objects.len()
}

/// `SYNC_IOC_MERGE`: the merged fence signals only once EVERY source has,
/// the way the `dma_fence_array` behind a merged sync_file does.
#[test]
fn a_merged_fence_waits_for_every_source() {
    let _g = test_lock();
    let a = create(false);
    let b = create(false);
    let m = merge_fences(&[(a, 3), (b, 1)]);
    assert_eq!(query(m), Some(0), "nothing has been reached yet");
    assert!(timeline_signal(a, 3));
    assert_eq!(query(m), Some(0), "one source of two is not enough");
    assert!(matches!(wait(&[m], None, false, 0), WaitOutcome::Timeout));
    assert!(timeline_signal(b, 1));
    assert_eq!(query(m), Some(1), "both sources reached: the merge signals");
    assert!(matches!(
        wait(&[m], None, false, 0),
        WaitOutcome::Signaled { .. }
    ));
    destroy(m);
    destroy(a);
    destroy(b);
}

#[test]
fn a_merge_of_fences_already_reached_is_signaled_at_once() {
    let _g = test_lock();
    let a = create(true);
    let b = create(false);
    assert!(timeline_signal(b, 5));
    let m = merge_fences(&[(a, 1), (b, 5)]);
    assert_eq!(query(m), Some(1));
    destroy(m);
    destroy(a);
    destroy(b);
}

/// Mesa's per-frame pattern under X11: a surrogate syncobj receives a
/// transfer of a point still in flight, is exported as a sync_file, the
/// sync_file is imported into the acquire semaphore, and the surrogate
/// is destroyed at once. The semaphore must NOT signal until the fence
/// lands -- releasing it on the surrogate's destroy handed the GPU an
/// image the compositor was still reading.
#[test]
fn a_link_survives_the_destroy_of_its_source() {
    let _g = test_lock();
    arm_hooks();
    let release = create(false);
    let mut landing = Landing::new();
    assert!(attach_hw_fence(release, 4, landing.va(), 0, 7, 0, false));
    let surrogate = create(false);
    assert!(transfer(surrogate, 0, release, 4));
    let sem = create(false);
    assert!(import_snapshot(sem, surrogate, 1));
    assert!(destroy(surrogate), "destroyed right after the export");
    assert_eq!(
        query(sem),
        Some(0),
        "the fence has not landed: the semaphore must stay unsignaled"
    );
    // The surrogate may get a *submitted* announce from the HW-fence
    // transfer (same as `attach_hw_fence`); that is not a landed signal.
    // The acquire semaphore itself must stay quiet until the GPU writes.
    assert!(
        !signals().iter().any(|&(h, _)| h == sem),
        "and the semaphore was not woken early, got {:?}",
        signals()
    );
    landing.land(7);
    assert_eq!(
        query(sem),
        Some(1),
        "once the GPU writes the fence the link resolves through the orphan"
    );
    assert!(
        !signals().is_empty(),
        "and the landing is announced, so an eventfd waiter re-checks"
    );
    destroy(sem);
    destroy(release);
}

/// Same, one level deeper: the dying source is itself only linked.
#[test]
fn a_link_survives_the_destroy_of_a_source_that_is_itself_linked() {
    let _g = test_lock();
    let release = create(false);
    let surrogate = create(false);
    assert!(
        transfer(surrogate, 0, release, 2),
        "deferred: release is at 0"
    );
    let sem = create(false);
    assert!(import_snapshot(sem, surrogate, 1));
    assert!(destroy(surrogate));
    assert_eq!(query(sem), Some(0), "release has not been signaled");
    assert!(timeline_signal(release, 2));
    assert_eq!(query(sem), Some(1));
    destroy(sem);
    destroy(release);
}

/// An orphan lives exactly as long as something depends on it.
#[test]
fn an_orphaned_source_is_collected_once_nothing_depends_on_it() {
    let _g = test_lock();
    let src = create(false);
    let mut landing = Landing::new();
    assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, 1));
    let objects = object_count();
    let pending = pending_now();
    assert!(destroy(src));
    assert_eq!(
        object_count(),
        objects,
        "still named by dst's link: kept as an orphan"
    );
    assert!(!add_ref(src), "an orphan cannot be revived");
    assert!(!destroy(src), "nor destroyed twice");
    // A direct signal replaces dst's fence, so nothing names the orphan.
    assert!(timeline_signal(dst, 1));
    assert_eq!(object_count(), objects - 1, "the orphan is collected");
    assert_eq!(pending_now(), pending - 1, "and its fence with it");
    landing.land(1);
    destroy(dst);
}

/// A merge whose source is gone for good (no fence, no link) keeps
/// waiting for the sources that are still alive.
#[test]
fn a_merge_outlives_a_source_that_can_never_signal() {
    let _g = test_lock();
    let a = create(false);
    let b = create(false);
    let m = merge_fences(&[(a, 1), (b, 1)]);
    assert!(destroy(a));
    assert_eq!(query(m), Some(0), "b has not signaled");
    assert!(timeline_signal(b, 1));
    assert_eq!(query(m), Some(1));
    destroy(m);
    destroy(b);
}

// ----- The wait family, abandoned landing zones, exported snapshots -----

/// `WAIT_AVAILABLE`: a fence that is submitted but has not landed
/// satisfies the wait (Mesa's "wait for the point to be queued"); a
/// plain wait still parks on it, and neither flavour signals anything.
#[test]
fn wait_available_is_satisfied_by_a_fence_in_flight_and_a_plain_wait_is_not() {
    let _g = test_lock();
    arm_hooks();
    let h = create(false);
    let t = create(false);
    let mut zone = Landing::new();
    // Nothing submitted yet: nothing is available either.
    assert!(matches!(
        wait_available(&[h], None, true, 0),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_available_ready(&[h], None, true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    assert!(attach_hw_fence(h, 1, zone.va(), 0, 1, 0, true));
    assert!(attach_hw_fence(t, 5, zone.va(), 0, 2, 0, false));
    assert!(
        matches!(wait(&[h], None, true, 0), WaitOutcome::Timeout),
        "submitted is not landed"
    );
    assert!(matches!(
        wait_available(&[h], None, true, 0),
        WaitOutcome::Signaled {
            first_signaled_index: 0
        }
    ));
    assert!(matches!(
        wait_ready(&[h], None, true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    assert!(matches!(
        wait_available_ready(&[h], None, true, 0),
        Some(Ok(0))
    ));
    // The point in flight is the bound: at it available, past it not.
    assert!(matches!(
        wait_available(&[t], Some(&[5]), true, 0),
        WaitOutcome::Signaled { .. }
    ));
    assert!(matches!(
        wait_available(&[t], Some(&[6]), true, 0),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_available_ready(&[t], Some(&[6]), true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    // Any-of names the first satisfied index; all-of needs every one.
    assert!(matches!(
        wait_available(&[t, h], Some(&[6, 1]), false, 0),
        WaitOutcome::Signaled {
            first_signaled_index: 1
        }
    ));
    assert!(matches!(
        wait_available_ready(&[t, h], Some(&[6, 1]), false, 0),
        Some(Ok(1))
    ));
    assert!(matches!(
        wait_available(&[t, h], Some(&[6, 1]), true, 0),
        WaitOutcome::Timeout
    ));
    // Available is not signaled: the counter reads 0 and the fence stays.
    assert_eq!(query(h), Some(0));
    assert_eq!(query_submitted(h), Some(1));
    assert_eq!(pending_now(), 2);
    assert!(
        signals().iter().all(|&(_, p)| p >= 1),
        "only submits announced"
    );
    // A probe with the deadline ahead says sleep...
    let ahead = now_us() + 1_000_000;
    assert!(wait_ready(&[h], None, true, ahead).is_none());
    // ...until the fence lands, when the plain wait goes through.
    zone.land(2);
    assert!(matches!(wait_ready(&[h], None, true, ahead), Some(Ok(0))));
    assert!(matches!(
        wait(&[h, t], Some(&[1, 5]), true, 0),
        WaitOutcome::Signaled {
            first_signaled_index: 0
        }
    ));
    assert_eq!(pending_now(), 0);
    // An unknown handle is Invalid on every flavour, whatever the others.
    assert!(matches!(
        wait_available(&[h, 0xdead_0000], None, false, 0),
        WaitOutcome::Invalid
    ));
    assert!(matches!(
        wait_available_ready(&[h, 0xdead_0000], None, false, 0),
        Some(Err(WaitOutcome::Invalid))
    ));
    destroy(h);
    destroy(t);
}

/// A driver about to submit on a channel may take a fence pending on
/// THAT channel as already ordered: the ring executes in order, so the
/// submission cannot overtake it. A fence on any other channel is a real
/// wait, and an ordered fence is not a signaled one.
#[test]
fn wait_ordered_trusts_a_fence_on_its_own_channel_and_waits_for_every_other() {
    let _g = test_lock();
    arm_hooks();
    let same = create(false);
    let other = create(false);
    let plain = create(false);
    let mut zone_a = Landing::new();
    let mut zone_b = Landing::new();
    assert!(attach_hw_fence(same, 3, zone_a.va(), 0, 1, 1, false));
    assert!(attach_hw_fence(other, 1, zone_b.va(), 0, 1, 2, true));
    assert!(timeline_signal(plain, 7));
    assert!(matches!(
        wait_ordered(&[same], Some(&[3]), 0, 1),
        WaitOutcome::Signaled {
            first_signaled_index: 0
        }
    ));
    assert!(
        matches!(
            wait_ordered(&[same], Some(&[4]), 0, 1),
            WaitOutcome::Timeout
        ),
        "only up to the point the ring will land"
    );
    assert!(
        matches!(
            wait_ordered(&[same], Some(&[3]), 0, 2),
            WaitOutcome::Timeout
        ),
        "from another channel the same fence is a wait"
    );
    // Every handle must pass: the other channel's fence holds it up...
    assert!(matches!(
        wait_ordered(&[plain, same, other], Some(&[7, 3, 1]), 0, 1),
        WaitOutcome::Timeout
    ));
    // ...until it lands.
    zone_b.land(1);
    assert!(matches!(
        wait_ordered(&[plain, same, other], Some(&[7, 3, 1]), 0, 1),
        WaitOutcome::Signaled {
            first_signaled_index: 0
        }
    ));
    // Ordered is not signaled: `same` still waits for its own landing.
    assert_eq!(query(same), Some(0));
    assert_eq!(pending_now(), 1);
    assert!(matches!(
        wait(&[same], Some(&[3]), true, 0),
        WaitOutcome::Timeout
    ));
    zone_a.land(1);
    assert_eq!(query(same), Some(3));
    assert!(matches!(
        wait_ordered(&[same, 0xdead_0000], None, 0, 1),
        WaitOutcome::Invalid
    ));
    destroy(same);
    destroy(other);
    destroy(plain);
}

/// A channel going away takes its landing zone with it. Every fence
/// pending on that zone is released as signaled -- its waiters move on,
/// the eventfd side hears of it, an import of it resolves -- and none is
/// reported as a timeout; a fence on any other zone is untouched.
#[test]
fn abandoning_a_landing_zone_releases_its_fences_as_signaled_and_no_others() {
    let _g = test_lock();
    arm_hooks();
    let a = create(false);
    let b = create(false);
    let c = create(false);
    let dst = create(false);
    let dead = Landing::new();
    let live = Landing::new();
    assert!(attach_hw_fence(a, 3, dead.va(), 0, 1, 1, false));
    assert!(attach_hw_fence(a, 5, dead.va(), 0, 2, 1, false));
    assert!(attach_hw_fence(b, 1, dead.va(), 0, 3, 1, true));
    assert!(attach_hw_fence(c, 1, live.va(), 0, 1, 2, true));
    // The compositor's acquire: a's point 3, imported as a sync_file.
    assert!(import_snapshot(dst, a, 3));
    assert_eq!(query(dst), Some(0));
    assert_eq!(pending_now(), 4);
    assert_eq!(abandon_fences(0xdead_0000), 0, "a zone nobody uses");
    assert_eq!(pending_now(), 4);
    SIGNALS.with(|s| s.borrow_mut().clear());
    assert_eq!(abandon_fences(dead.va()), 3);
    assert_eq!(pending_now(), 1, "the live zone's fence stays");
    assert_eq!(query(a), Some(5));
    assert_eq!(query(b), Some(1));
    assert_eq!(query(c), Some(0));
    assert_eq!(query(dst), Some(1), "a reached 3, so the import is in");
    assert!(matches!(
        wait(&[a, b], Some(&[5, 1]), true, 0),
        WaitOutcome::Signaled { .. }
    ));
    assert!(matches!(wait(&[c], None, true, 0), WaitOutcome::Timeout));
    let heard = signals();
    assert!(heard.contains(&(a, 5)), "announced: {:?}", heard);
    assert!(heard.contains(&(b, 1)), "announced: {:?}", heard);
    assert!(heard.iter().all(|&(h, _)| h != c), "not c: {:?}", heard);
    // Released, not timed out: the ring is not latched wedged for them.
    // The live zone's fence does time out, on its own clock.
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    assert_eq!(query(c), Some(1), "timed out: released too, but reported");
    assert_eq!(timeouts(), [(2, c, 1)]);
    destroy(a);
    destroy(b);
    destroy(c);
    destroy(dst);
}

/// `HANDLE_TO_FD` with `EXPORT_SYNC_FILE`: the snapshot names the fence
/// current at export time -- the highest submission in flight, never
/// point 0 (an unsignaled binary syncobj exports its next signal), so an
/// importer waits for exactly that submission and not one less.
#[test]
fn an_exported_snapshot_names_the_fence_in_flight_and_never_point_zero() {
    let _g = test_lock();
    arm_hooks();
    assert_eq!(export_snapshot(0xdead_0000), None);
    let h = create(false);
    assert_eq!(export_snapshot(h), Some(1), "unsignaled: its next signal");
    let s = create(true);
    assert_eq!(export_snapshot(s), Some(1));
    let t = create(false);
    assert!(timeline_signal(t, 4));
    assert_eq!(export_snapshot(t), Some(4));
    let mut zone = Landing::new();
    assert!(attach_hw_fence(t, 6, zone.va(), 0, 1, 0, false));
    assert!(attach_hw_fence(t, 9, zone.va(), 0, 2, 0, false));
    assert_eq!(
        export_snapshot(t),
        Some(9),
        "the highest fence in flight, not the counter"
    );
    let dst = create(false);
    assert!(import_snapshot(dst, t, export_snapshot(t).unwrap()));
    zone.land(1);
    assert_eq!(query(t), Some(6));
    assert_eq!(query(dst), Some(0), "the second submission has not landed");
    zone.land(2);
    assert_eq!(query(dst), Some(1));
    assert_eq!(export_snapshot(t), Some(9), "landed: the counter itself");
    // An object that is itself waiting on an import exports its next
    // signal, and a snapshot of it resolves when that import does.
    let src = create(false);
    let linked = create(false);
    assert!(import_snapshot(linked, src, 1));
    assert_eq!(export_snapshot(linked), Some(1));
    let far = create(false);
    assert!(import_snapshot(far, linked, 1));
    assert_eq!(query(far), Some(0));
    assert!(signal(src));
    assert_eq!(query(far), Some(1));
    for x in [h, s, t, dst, src, linked, far] {
        destroy(x);
    }
}

/// The one line every timed-out wait prints: each handle with its
/// target, its current point (-1 for one that does not exist) and the
/// fences still in flight for it, with the channel they ride.
#[test]
fn describe_names_each_handle_with_target_current_and_the_fences_in_flight() {
    let _g = test_lock();
    let h = create(false);
    let t = create(false);
    assert!(timeline_signal(t, 2));
    let zone = Landing::new();
    assert!(attach_hw_fence(t, 7, zone.va(), 0, 1, 3, false));
    assert!(attach_hw_fence(t, 8, zone.va(), 0, 2, 3, false));
    assert_eq!(
        describe(&[h, t, 0xdead_0000], Some(&[1, 7, 1])),
        alloc::format!(" {:#x}:1/0 {:#x}:7/2+7(ctx3)+8(ctx3) 0xdead0000:1/-1", h, t)
    );
    assert_eq!(
        describe(&[t], None),
        alloc::format!(" {:#x}:1/2+7(ctx3)+8(ctx3)", t),
        "no points: binary targets"
    );
    assert_eq!(describe(&[], None), "");
    destroy(h);
    destroy(t);
}

// ----- A fence, once signaled, stays signaled -----

/// A `sync_file` holds the `dma_fence` that was current at export time,
/// and a signaled `dma_fence` never un-signals. Here an import is a link
/// to its SOURCE, and the source is a binary slot that the next frame's
/// submit rewinds to 0 -- so an importer that had read "signaled" read
/// "unsignaled" again the moment its source was re-armed.
#[test]
fn an_importer_stays_signaled_when_its_source_is_rearmed() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let mut first = Landing::new();
    assert!(attach_hw_fence(src, 1, first.va(), 0, 1, 0, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
    first.land(1);
    assert_eq!(
        query(dst),
        Some(1),
        "the fence landed: the importer is signaled"
    );
    // Next frame: the producer re-arms its binary syncobj on a new fence.
    let mut second = Landing::new();
    assert!(attach_hw_fence(src, 1, second.va(), 0, 2, 0, true));
    assert_eq!(query(src), Some(0), "the slot is armed on the new fence");
    assert_eq!(
        query(dst),
        Some(1),
        "but the fence the importer holds has signaled, and stays signaled"
    );
    second.land(2);
    destroy(dst);
    destroy(src);
}

/// Same source re-arm, with the first fence landed but not yet seen by
/// anyone (nobody polled between the GPU's write and the next EXEC): the
/// re-arm discarded that fence as "superseded", and with it the only
/// thing that could ever signal the importer.
#[test]
fn a_landed_fence_the_source_replaces_before_anyone_looked_still_signals_the_importer() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let mut first = Landing::new();
    assert!(attach_hw_fence(src, 1, first.va(), 0, 1, 0, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
    first.land(1);
    let mut second = Landing::new();
    assert!(attach_hw_fence(src, 1, second.va(), 0, 2, 0, true));
    assert_eq!(query(src), Some(0));
    assert_eq!(
        query(dst),
        Some(1),
        "the fence it imported landed before the replacement"
    );
    second.land(2);
    destroy(dst);
    destroy(src);
}

/// `EXPORT_SYNC_FILE` hands out the fence the syncobj carries NOW, and a
/// `sync_file` holds its own reference to it: resetting or re-arming the
/// syncobj afterwards does not touch the file. Mesa exports a `SYNC_FD`
/// and resets the binary semaphore in the same call
/// (`vk_common_GetSemaphoreFdKHR`, copy transference) while the GPU is
/// still on its way to the fence. Here the file was `(handle, point)`
/// read live against the source: the reset dropped the pending fence and
/// zeroed the point, and the file, its merges and its imports waited for
/// the source's next signal, or for ever.
#[test]
fn an_exported_sync_file_keeps_its_fence_through_a_reset_or_a_re_arm_of_the_source() {
    let _g = test_lock();
    arm_hooks();
    // In flight at export, reset before it lands.
    let src = create(false);
    let mut zone = Landing::new();
    assert!(attach_hw_fence(src, 1, zone.va(), 0x100, 7, 3, true));
    let fd = export_fence(src).expect("a live source");
    assert_ne!(fd, src, "a syncobj of its own");
    assert_eq!(
        pending_hw_fences(fd, 1),
        alloc::vec![(zone.va(), 0x100u64, 7u32, 3u32)],
        "the file carries the same hardware fence"
    );
    assert!(reset(src));
    assert_eq!(query(src), Some(0));
    assert!(
        pending_hw_fences(src, 1).is_empty(),
        "the reset dropped the source's fence"
    );
    assert_eq!(query(fd), Some(0), "not landed yet");
    let dst = create(false);
    assert!(import_snapshot(dst, fd, 1));
    assert!(matches!(
        wait_ready(&[dst], None, true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    zone.land(7);
    assert_eq!(query(fd), Some(1), "the fence landed: the file signals");
    assert_eq!(query(dst), Some(1), "and so does its import");
    assert_eq!(query(src), Some(0), "the reset source does not");
    destroy(dst);
    destroy(fd);
    // Re-armed by the next submit before the exported fence lands.
    let mut first = Landing::new();
    assert!(attach_hw_fence(src, 1, first.va(), 0, 8, 3, true));
    let fd = export_fence(src).unwrap();
    let mut second = Landing::new();
    assert!(attach_hw_fence(src, 1, second.va(), 0, 9, 3, true));
    assert_eq!(query(src), Some(0));
    first.land(8);
    assert_eq!(query(fd), Some(1), "the first submission's fence");
    assert_eq!(query(src), Some(0), "the source waits for the second");
    second.land(9);
    assert_eq!(query(src), Some(1));
    destroy(fd);
    // Landed before the export: reached at once, and a reset changes
    // nothing about it.
    let fd = export_fence(src).unwrap();
    assert_eq!(query(fd), Some(1));
    assert!(reset(src));
    assert_eq!(query(fd), Some(1));
    assert!(!reached_by_timeout(&[fd], Some(&[1])));
    destroy(fd);
    // Given up on before the export: the file carries the timeout mark.
    let dead = Landing::new();
    assert!(attach_hw_fence(src, 1, dead.va(), 0, 10, 3, true));
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    assert_eq!(query(src), Some(1), "reached, by the timeout");
    let fd = export_fence(src).unwrap();
    assert_eq!(query(fd), Some(1));
    assert!(reached_by_timeout(&[fd], Some(&[1])), "and marked as such");
    destroy(fd);
    assert!(reset(src));
    // No fence at all: the file is the source's next signal.
    let fd = export_fence(src).unwrap();
    assert_eq!(query(fd), Some(0));
    assert!(signal(src));
    assert_eq!(query(fd), Some(1));
    destroy(fd);
    destroy(src);
    // A timeline: the file is the point in flight, not the counter.
    let tl = create(false);
    assert!(timeline_signal(tl, 3));
    let mut z = Landing::new();
    assert!(attach_hw_fence(tl, 5, z.va(), 0, 11, 3, false));
    let fd = export_fence(tl).unwrap();
    assert_eq!(query(fd), Some(0), "point 5 is still in flight");
    assert!(reset(tl));
    z.land(11);
    assert_eq!(query(fd), Some(1));
    destroy(fd);
    destroy(tl);
    // An unknown handle exports nothing, and leaves nothing behind.
    let before = TABLE.lock().objects.len();
    assert_eq!(export_fence(0xdead_0000), None);
    assert_eq!(TABLE.lock().objects.len(), before);
}

/// `IMPORT_SYNC_FILE` and a binary `TRANSFER` REPLACE the destination's
/// fence (`drm_syncobj_replace_fence` in Linux: the old `dma_fence` may
/// still signal, but the syncobj no longer carries it). The destination
/// here kept its previous hardware fence in `pending` next to the new
/// link, so when that superseded fence landed it advanced the object,
/// and a waiter went through before the fence the import stood for had
/// been reached; `pending_hw_fences` handed EXEC the same stale fence to
/// ACQUIRE. And a slot whose fence had already landed stayed signaled
/// through the import, so the next wait on it passed at once (a
/// `VkSemaphore` imported into again: signal, wait, import, wait).
/// Three shapes, each over a fence in flight and over one landed: an
/// import, a transfer whose source has nothing submitted (software
/// link) and a transfer whose source has its own fence in flight (which
/// moves onto the destination, alone). A timeline transfer ADDS to the
/// chain and keeps the binary fence; a slot already driven past binary
/// range is not a slot (as for a stray binary signal).
#[test]
fn an_import_or_a_binary_transfer_replaces_the_fence_the_object_was_carrying() {
    let _g = test_lock();
    arm_hooks();
    for (shape, landed_first) in [
        ("import", false),
        ("transfer-link", false),
        ("transfer-hw", false),
        ("import", true),
        ("transfer-link", true),
        ("transfer-hw", true),
    ] {
        let sem = create(false);
        let mut old = Landing::new();
        assert!(attach_hw_fence(sem, 1, old.va(), 0, 7, 3, true));
        if landed_first {
            old.land(7);
            assert_eq!(query(sem), Some(1), "{}: signaled, and seen", shape);
        }
        let src = create(false);
        let mut theirs = Landing::new();
        match shape {
            "import" => assert!(import_snapshot(sem, src, 1)),
            "transfer-link" => assert!(transfer(sem, 0, src, 0)),
            _ => {
                assert!(attach_hw_fence(src, 1, theirs.va(), 0, 9, 4, true));
                assert!(transfer(sem, 0, src, 0));
            }
        }
        // (`attach_hw_fence` itself notifies the submitted point, so the
        // eventfd check below starts counting here.)
        let notified = signals().len();
        let fences = pending_hw_fences(sem, 1);
        assert!(
            !fences.iter().any(|f| f.0 == old.va()),
            "{}: EXEC must not acquire the fence the import replaced: {:?}",
            shape,
            fences
        );
        if shape == "transfer-hw" {
            assert_eq!(
                fences,
                alloc::vec![(theirs.va(), 0u64, 9u32, 4u32)],
                "{}: the source's fence, alone",
                shape
            );
        }
        old.land(7);
        assert_eq!(
            query(sem),
            Some(0),
            "{}: the replaced fence landed ({}), the object waits for its source",
            shape,
            if landed_first { "before" } else { "after" }
        );
        assert!(
            matches!(
                wait_ready(&[sem], None, true, 0),
                Some(Err(WaitOutcome::Timeout))
            ),
            "{}: a waiter must not go through",
            shape
        );
        assert!(
            signals()[notified..].iter().all(|&(h, _)| h != sem),
            "{}: no eventfd for the replaced fence",
            shape
        );
        if shape == "transfer-hw" {
            theirs.land(9);
        } else {
            assert!(signal(src));
        }
        assert_eq!(query(sem), Some(1), "{}: reached through the source", shape);
        destroy(sem);
        destroy(src);
    }
    // A fence given up on leaves its timeout mark; the import replaces
    // that too, so the point the source then delivers is a clean one.
    let sem = create(false);
    let dead = Landing::new();
    assert!(attach_hw_fence(sem, 1, dead.va(), 0, 7, 3, true));
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    assert!(reached_by_timeout(&[sem], None));
    let src = create(false);
    assert!(import_snapshot(sem, src, 1));
    assert_eq!(query(sem), Some(0));
    assert!(signal(src));
    assert_eq!(query(sem), Some(1));
    assert!(
        !reached_by_timeout(&[sem], None),
        "the dead fence went with the import"
    );
    destroy(sem);
    destroy(src);
    // A timeline transfer adds a point to the chain: the binary fence in
    // flight on the destination stays.
    let tl = create(false);
    let mut own = Landing::new();
    assert!(attach_hw_fence(tl, 1, own.va(), 0, 7, 3, true));
    let src = create(false);
    assert!(transfer(tl, 5, src, 0));
    assert_eq!(
        pending_hw_fences(tl, 1),
        alloc::vec![(own.va(), 0u64, 7u32, 3u32)],
        "a transfer at point 5 replaces nothing at point 1"
    );
    own.land(7);
    assert_eq!(query(tl), Some(1));
    // A handle already driven past binary range is not a slot: an import
    // does not rewind it (as a stray binary signal does not).
    assert!(timeline_signal(tl, 9));
    assert!(import_snapshot(tl, src, 1));
    assert_eq!(query(tl), Some(9), "a timeline keeps its point");
    assert!(transfer(tl, 0, src, 0));
    assert_eq!(query(tl), Some(9), "through a binary transfer too");
    destroy(tl);
    destroy(src);
}

/// Objects still carrying a link (an import or transfer not yet
/// materialised into their own counter).
#[cfg(test)]
fn links_now() -> usize {
    TABLE
        .lock()
        .objects
        .iter()
        .filter(|o| !o.links.is_empty())
        .count()
}

/// `SYNCOBJ_RESET` empties the object it names, and nothing else: an
/// importer that already received the fence keeps it, whether the
/// landing was seen before the reset or not.
#[test]
fn resetting_a_source_does_not_unsignal_the_fence_it_already_delivered() {
    let _g = test_lock();
    arm_hooks();
    for seen_before_reset in [true, false] {
        let src = create(false);
        let mut landing = Landing::new();
        assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
        let dst = create(false);
        assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
        landing.land(1);
        if seen_before_reset {
            assert_eq!(query(dst), Some(1));
        }
        assert!(reset(src));
        assert_eq!(query(src), Some(0), "the reset object carries nothing");
        assert_eq!(
            query(dst),
            Some(1),
            "the importer keeps the fence (landing seen first: {})",
            seen_before_reset
        );
        destroy(dst);
        destroy(src);
    }
}

/// The X11 acquire path: the acquire semaphore's sync_file is a MERGE of
/// the compositor's release and the previous present, and both sources
/// are re-armed every frame. Once merged and signaled it must stay so,
/// or the acquire waits for a release that already happened.
#[test]
fn a_merged_fence_stays_signaled_when_its_sources_are_rearmed() {
    let _g = test_lock();
    arm_hooks();
    let release = create(false);
    let mut first = Landing::new();
    assert!(attach_hw_fence(release, 1, first.va(), 0, 1, 0, true));
    let present = create(false);
    assert!(timeline_signal(present, 6));
    let m = merge_fences(&[(release, 1), (present, 6)]);
    assert_eq!(query(m), Some(0), "the release is still in flight");
    first.land(1);
    assert_eq!(query(m), Some(1), "both sources reached");
    // Next frame: the release slot is re-armed and the present timeline
    // is reset for reuse.
    let mut second = Landing::new();
    assert!(attach_hw_fence(release, 1, second.va(), 0, 2, 0, true));
    assert!(reset(present));
    assert_eq!(query(release), Some(0));
    assert_eq!(query(present), Some(0));
    assert_eq!(
        query(m),
        Some(1),
        "the merged fence signaled once, and that is for good"
    );
    assert!(matches!(
        wait(&[m], None, true, 0),
        WaitOutcome::Signaled { .. }
    ));
    second.land(2);
    destroy(m);
    destroy(present);
    destroy(release);
}

/// A chain of links (an import of a transfer of a timeline point)
/// collapses into plain counters in one resolve once its root is
/// reached: no object keeps a link to re-derive, and the destination
/// reached the point the transfer asked for, not just 1.
#[test]
fn a_chain_of_links_collapses_into_counters_once_its_root_is_reached() {
    let _g = test_lock();
    arm_hooks();
    let root = create(false);
    let mid = create(false);
    assert!(transfer(mid, 9, root, 3), "deferred: root is at 0");
    let leaf = create(false);
    assert!(import_snapshot(leaf, mid, 9));
    let links = links_now();
    assert!(links >= 2, "two links recorded, got {}", links);
    assert!(timeline_signal(root, 3));
    assert_eq!(links_now(), links - 2, "both links materialised");
    assert_eq!(query(mid), Some(9), "the transfer's own point, kept");
    assert_eq!(query(leaf), Some(1));
    assert!(
        signals().contains(&(mid, 9)) && signals().contains(&(leaf, 1)),
        "each materialisation is announced for its eventfd waiters, got {:?}",
        signals()
    );
    // The root can now do anything without touching them.
    assert!(reset(root));
    assert_eq!(query(mid), Some(9));
    assert_eq!(query(leaf), Some(1));
    destroy(leaf);
    destroy(mid);
    destroy(root);
}

/// An orphan (a destroyed source something still links to) is let go as
/// soon as the link that named it materialises, not only when that link
/// is replaced by a direct signal.
#[test]
fn a_materialised_link_lets_its_orphaned_source_go() {
    let _g = test_lock();
    let src = create(false);
    let mut landing = Landing::new();
    assert!(attach_hw_fence(src, 1, landing.va(), 0, 1, 0, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, 1));
    let objects = object_count();
    assert!(destroy(src));
    assert_eq!(
        object_count(),
        objects,
        "kept as an orphan while dst links to it"
    );
    landing.land(1);
    assert_eq!(query(dst), Some(1));
    assert_eq!(
        object_count(),
        objects - 1,
        "dst holds its point now: nothing names the orphan"
    );
    assert_eq!(links_now(), 0);
    destroy(dst);
}

/// EXEC asks [`pending_hw_fence`] for the hardware fence behind each
/// wait so it can emit a GPU ACQUIRE instead of parking the ioctl on the
/// CPU. The acquire semaphore of an X11 swapchain image is always an
/// import or a merge, whose fence sits in the SOURCE's row: looked up by
/// the importer's handle it came back empty, so every such wait spun on
/// the CPU inside EXEC. Seen through the link, it is the source's fence.
#[test]
fn an_exec_wait_on_an_importer_finds_the_fence_its_source_waits_for() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let mut zone = Landing::new();
    assert!(attach_hw_fence(src, 1, zone.va(), 0x4000, 7, 3, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, export_snapshot(src).unwrap()));
    let fence = Some((zone.va(), 0x4000u64, 7u32, 3u32));
    assert_eq!(pending_hw_fence(dst, 1), fence, "the source's fence");
    assert_eq!(
        pending_hw_fence(dst, 0),
        fence,
        "point 0 is the binary fence"
    );
    assert_eq!(
        pending_hw_fence(dst, 2),
        None,
        "a binary import delivers point 1, nothing higher"
    );
    assert_eq!(
        query_submitted(dst),
        Some(1),
        "submitted as far as its source"
    );
    assert_eq!(query(dst), Some(0));
    // An import of an import: two links deep, same fence.
    let leaf = create(false);
    assert!(import_snapshot(leaf, dst, 1));
    assert_eq!(pending_hw_fence(leaf, 1), fence);
    // A source with nothing submitted has nothing to acquire.
    let idle = create(false);
    let waiter = create(false);
    assert!(import_snapshot(waiter, idle, 1));
    assert_eq!(pending_hw_fence(waiter, 1), None);
    assert_eq!(query_submitted(waiter), Some(0));
    zone.land(7);
    assert_eq!(query(leaf), Some(1));
    assert_eq!(pending_hw_fence(dst, 1), None, "landed: nothing in flight");
    for h in [leaf, dst, src, waiter, idle] {
        assert!(destroy(h));
    }
}

/// A transfer recorded before its source was submitted is a link; once
/// the source's work is in flight, the link shows its fences, and only
/// up to the point the transfer promised: among several fences on the
/// source, the lowest one that covers the transferred point and every
/// one below it (the chain), never the ones above.
#[test]
fn a_pending_transfer_shows_its_source_fence_up_to_the_point_it_promises() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let dst = create(false);
    assert!(
        transfer(dst, 6, src, 4),
        "deferred: nothing submitted on src"
    );
    assert_eq!(pending_hw_fence(dst, 6), None);
    assert!(matches!(
        wait_available(&[dst], Some(&[6]), true, 0),
        WaitOutcome::Timeout
    ));
    assert_eq!(query_submitted(dst), Some(0));
    let mut low = Landing::new();
    let mut high = Landing::new();
    let top = Landing::new();
    assert!(attach_hw_fence(src, 2, low.va(), 0, 5, 1, false));
    assert!(attach_hw_fence(src, 4, high.va(), 0, 11, 2, false));
    assert!(attach_hw_fence(src, 6, top.va(), 0, 13, 2, false));
    let chain = alloc::vec![(low.va(), 0u64, 5u32, 1u32), (high.va(), 0, 11, 2)];
    assert_eq!(
        pending_hw_fences(dst, 6),
        chain,
        "the fence that reaches 4, and the one below it"
    );
    assert_eq!(
        pending_hw_fences(dst, 3),
        chain,
        "any point the transfer covers"
    );
    assert_eq!(pending_hw_fence(dst, 7), None, "past what it promises");
    assert!(matches!(
        wait_available(&[dst], Some(&[6]), true, 0),
        WaitOutcome::Signaled { .. }
    ));
    assert!(matches!(
        wait_available_ready(&[dst], Some(&[6]), true, 0),
        Some(Ok(0))
    ));
    assert!(matches!(
        wait_available(&[dst], Some(&[7]), true, 0),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_available_ready(&[dst], Some(&[7]), true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    assert_eq!(query_submitted(dst), Some(6));
    assert_eq!(query(dst), Some(0), "available is not signaled");
    assert_eq!(export_snapshot(dst), Some(6));
    high.land(11);
    assert_eq!(query(dst), Some(0), "4 landed out of order: it waits for 2");
    low.land(5);
    assert_eq!(query(dst), Some(6));
    assert!(destroy(dst));
    assert!(destroy(src));
    assert_eq!(pending_now(), 0, "src's fence at 6 went with it");
}

/// A merged fence is submitted once EVERY source is, and is one GPU
/// ACQUIRE per source still in flight: with both halves running, the
/// two fences, in the link's order; once one lands, the other alone.
#[test]
fn a_merge_is_available_once_every_source_is_submitted_and_acquires_every_fence_in_flight() {
    let _g = test_lock();
    arm_hooks();
    let a = create(false);
    let b = create(false);
    let mut za = Landing::new();
    let mut zb = Landing::new();
    let m = merge_fences(&[(a, 1), (b, 1)]);
    assert!(matches!(
        wait_available(&[m], None, true, 0),
        WaitOutcome::Timeout
    ));
    assert_eq!(pending_hw_fence(m, 1), None);
    assert!(attach_hw_fence(a, 1, za.va(), 0, 1, 0, true));
    assert!(
        matches!(wait_available(&[m], None, true, 0), WaitOutcome::Timeout),
        "one source not submitted"
    );
    assert_eq!(pending_hw_fence(m, 1), None, "b has nothing to acquire yet");
    assert!(attach_hw_fence(b, 1, zb.va(), 0, 2, 1, true));
    assert!(matches!(
        wait_available(&[m], None, true, 0),
        WaitOutcome::Signaled { .. }
    ));
    assert_eq!(query_submitted(m), Some(1));
    assert_eq!(query(m), Some(0));
    assert_eq!(
        pending_hw_fences(m, 1),
        [(za.va(), 0, 1, 0), (zb.va(), 0, 2, 1)],
        "two fences in flight are two ACQUIREs, a's then b's"
    );
    za.land(1);
    assert_eq!(
        pending_hw_fence(m, 1),
        Some((zb.va(), 0, 2, 1)),
        "a reached: b's fence is the one left"
    );
    zb.land(2);
    assert_eq!(query(m), Some(1));
    for h in [m, a, b] {
        assert!(destroy(h));
    }
}

/// The list follows every source of a merge to its own fence, through a
/// source that is itself a link; a source still short of its point with
/// nothing submitted empties it, whatever the others hold: half a wait
/// on the GPU and half nowhere would let the submit run early.
#[test]
fn a_merge_lists_the_fence_behind_each_source_or_nothing_when_one_has_none() {
    let _g = test_lock();
    arm_hooks();
    let a = create(false);
    let b = create(false);
    let c = create(false);
    let mut za = Landing::new();
    let mut zb = Landing::new();
    let mut zc = Landing::new();
    assert!(attach_hw_fence(a, 1, za.va(), 0x1000, 1, 1, true));
    assert!(attach_hw_fence(b, 1, zb.va(), 0x2000, 2, 2, true));
    // Mesa's surrogate: a's fence transferred into a binary of its own.
    let via_a = create(false);
    assert!(transfer(via_a, 0, a, 0));
    // An importer of b: a link, not a fence.
    let via_b = create(false);
    assert!(import_snapshot(via_b, b, 1));
    let m = merge_fences(&[(via_a, 1), (via_b, 1), (c, 1)]);
    assert_eq!(
        pending_hw_fences(m, 1),
        [],
        "c has nothing submitted: nothing to acquire"
    );
    assert!(attach_hw_fence(c, 1, zc.va(), 0x3000, 3, 1, true));
    assert_eq!(
        pending_hw_fences(m, 1),
        [
            (za.va(), 0x1000, 1, 1),
            (zb.va(), 0x2000, 2, 2),
            (zc.va(), 0x3000, 3, 1)
        ],
        "one fence per source, in the link's order, through the surrogate and the import"
    );
    assert_eq!(
        pending_hw_fences(m, 2),
        [],
        "a binary merge promises point 1 only"
    );
    zb.land(2);
    assert_eq!(
        pending_hw_fences(m, 1),
        [(za.va(), 0x1000, 1, 1), (zc.va(), 0x3000, 3, 1)],
        "b reached: its fence leaves the list"
    );
    assert!(destroy(via_a));
    assert!(destroy(via_b));
    assert_eq!(
        pending_hw_fences(m, 1),
        [(za.va(), 0x1000, 1, 1), (zc.va(), 0x3000, 3, 1)],
        "the surrogates gone, as Mesa leaves them: the fences are still theirs"
    );
    za.land(1);
    zc.land(3);
    assert_eq!(query(m), Some(1));
    assert_eq!(pending_hw_fences(m, 1), []);
    for h in [m, a, b, c] {
        assert!(destroy(h));
    }
    assert_eq!(pending_now(), 0);
}

/// A merge is ordered for a channel only when EVERY fence behind it is
/// that channel's: two of its own, yes; one of its own and the
/// compositor's, a wait from either side.
#[test]
fn a_merge_is_ordered_only_when_every_fence_behind_it_is_the_channels_own() {
    let _g = test_lock();
    arm_hooks();
    let a = create(false);
    let b = create(false);
    let c = create(false);
    let mut za = Landing::new();
    let mut zb = Landing::new();
    let mut zc = Landing::new();
    assert!(attach_hw_fence(a, 1, za.va(), 0, 1, 1, true));
    assert!(attach_hw_fence(b, 1, zb.va(), 0, 2, 1, true));
    assert!(attach_hw_fence(c, 1, zc.va(), 0, 3, 2, true));
    let ours = merge_fences(&[(a, 1), (b, 1)]);
    let mixed = merge_fences(&[(a, 1), (c, 1)]);
    assert!(matches!(
        wait_ordered(&[ours], None, 0, 1),
        WaitOutcome::Signaled { .. }
    ));
    assert!(matches!(
        wait_ordered(&[ours], None, 0, 2),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_ordered(&[mixed], None, 0, 1),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_ordered(&[mixed], None, 0, 2),
        WaitOutcome::Timeout
    ));
    zc.land(3);
    assert!(
        matches!(
            wait_ordered(&[mixed], None, 0, 1),
            WaitOutcome::Signaled { .. }
        ),
        "the compositor's half landed: what is left is ours"
    );
    assert_eq!(query(mixed), Some(0), "ordered is not signaled");
    za.land(1);
    zb.land(2);
    for h in [ours, mixed, a, b, c] {
        assert!(destroy(h));
    }
    assert_eq!(pending_now(), 0);
}

/// The in-order guarantee of a channel holds through a link as well: a
/// submit on the channel whose fence an importer waits for cannot
/// overtake it.
#[test]
fn a_wait_on_its_own_channel_is_ordered_through_a_link_too() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let zone = Landing::new();
    assert!(attach_hw_fence(src, 1, zone.va(), 0, 1, 5, true));
    let dst = create(false);
    assert!(import_snapshot(dst, src, 1));
    assert!(matches!(
        wait_ordered(&[dst], None, 0, 5),
        WaitOutcome::Signaled { .. }
    ));
    assert!(matches!(
        wait_ordered(&[dst], None, 0, 6),
        WaitOutcome::Timeout
    ));
    assert!(
        matches!(wait(&[dst], None, true, 0), WaitOutcome::Timeout),
        "ordered is not signaled"
    );
    assert_eq!(query(dst), Some(0));
    assert!(destroy(dst));
    assert!(destroy(src));
}

/// `EXPORT_SYNC_FILE` takes the fence attached LAST. On a timeline that
/// still carries a transfer, that is the point the transfer promised,
/// not the counter: exporting the counter made the importer signaled
/// before the transferred work had run.
#[test]
fn exporting_a_timeline_with_a_transfer_pending_names_the_point_it_promises() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let t = create(false);
    assert!(timeline_signal(t, 3));
    assert!(transfer(t, 5, src, 2), "deferred: src is at 0");
    assert_eq!(query(t), Some(3));
    assert_eq!(export_snapshot(t), Some(5));
    let imp = create(false);
    assert!(import_snapshot(imp, t, export_snapshot(t).unwrap()));
    assert_eq!(query(imp), Some(0), "not before the transfer lands");
    assert!(matches!(wait(&[imp], None, true, 0), WaitOutcome::Timeout));
    assert!(timeline_signal(src, 2));
    assert_eq!(query(t), Some(5));
    assert_eq!(query(imp), Some(1));
    for h in [imp, t, src] {
        assert!(destroy(h));
    }
}

/// Two syncobjs importing each other can never resolve; looking through
/// their links must give up, not recurse until the stack runs out.
#[test]
fn a_cycle_of_links_is_neither_acquirable_nor_available() {
    let _g = test_lock();
    arm_hooks();
    let a = create(false);
    let b = create(false);
    assert!(import_snapshot(a, b, 1));
    assert!(import_snapshot(b, a, 1));
    assert_eq!(pending_hw_fence(a, 1), None);
    assert!(matches!(
        wait_available(&[a, b], None, true, 0),
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        wait_available_ready(&[a], None, true, 0),
        Some(Err(WaitOutcome::Timeout))
    ));
    assert_eq!(query_submitted(a), Some(0));
    assert_eq!(export_snapshot(a), Some(1));
    assert!(matches!(
        wait_ordered(&[a], None, 0, 0),
        WaitOutcome::Timeout
    ));
    // A direct signal breaks the cycle: b's link is then reached.
    assert!(signal(a));
    assert_eq!(query(b), Some(1));
    assert_eq!(links_now(), 0);
    assert!(destroy(a));
    assert!(destroy(b));
}

/// `SYNCOBJ_TRANSFER` to a timeline point ADDS a node to the chain
/// (`drm_syncobj_add_point`): the point counts once its own source and
/// every node before it have -- and a transfer FROM point 0 takes the
/// chain the source carries now (`drm_syncobj_find_fence` at 0), not its
/// point 1. The pair is Mesa's many-to-many
/// `vk_drm_syncobj_transfer_payloads` (a `vkQueueSubmit` that only
/// forwards N waits into M signals: NVK's `copy_sync_payloads`): each
/// wait goes into a temporary timeline at points 1..N, then the
/// temporary goes at point 0 into every signal, "relying on waiting on
/// a whole chain waiting on everything". With one link per object,
/// point 2 threw away what point 1 waited on and point 0 read as point
/// 1, so every signal semaphore followed ONE wait of N, whichever
/// order they came in: a semaphore signaled before its producer had
/// finished, and a buffer reused under the GPU.
#[test]
fn a_transfer_to_a_timeline_point_waits_for_the_nodes_before_it_like_a_chain() {
    let _g = test_lock();
    arm_hooks();
    let reached = |h: u32, p: u64| matches!(wait_ready(&[h], Some(&[p]), true, 0), Some(Ok(_)));

    // Mesa's shape, both binary waits still pending when forwarded,
    // signaled in either order.
    for first in 0..2usize {
        let w = [create(false), create(false)];
        let tmp = create(false);
        let (s_bin, s_tl) = (create(false), create(false));
        assert!(transfer(tmp, 1, w[0], 0));
        assert!(transfer(tmp, 2, w[1], 0));
        assert!(transfer(s_bin, 0, tmp, 0));
        assert!(transfer(s_tl, 7, tmp, 0));
        assert!(
            !reached(s_bin, 1) && !reached(s_tl, 7),
            "nothing signaled yet"
        );
        assert!(signal(w[first]));
        assert_eq!(
            query(tmp),
            Some(if first == 0 { 1 } else { 0 }),
            "point 1 is w0's alone; point 2 needs both (w{first} signaled first)"
        );
        assert!(
            !reached(s_bin, 1),
            "one wait of two is not the chain (w{} first)",
            first
        );
        assert!(!reached(s_tl, 7));
        assert!(signal(w[1 - first]));
        assert_eq!(query(tmp), Some(2));
        assert!(reached(s_bin, 1) && reached(s_tl, 7));
        assert_eq!(query(s_tl), Some(7));
        for h in [w[0], w[1], tmp, s_bin, s_tl] {
            assert!(destroy(h));
        }
    }

    // A wait already done when forwarded: the temporary reaches 1 at
    // once, and point 0 is still the whole chain, not that 1.
    {
        let (w0, w1, tmp, s) = (create(true), create(false), create(false), create(false));
        assert!(transfer(tmp, 1, w0, 0));
        assert_eq!(query(tmp), Some(1));
        assert!(transfer(tmp, 2, w1, 0));
        assert!(transfer(s, 0, tmp, 0));
        assert!(
            !reached(s, 1),
            "point 0 of the temporary is point 2, still pending"
        );
        assert!(signal(w1));
        assert!(reached(s, 1));
        for h in [w0, w1, tmp, s] {
            assert!(destroy(h));
        }
    }

    // Both waits in flight on two channels: EXEC behind the signal sees
    // both fences (one ACQUIRE each), and the signal counts only once
    // both have landed, whichever lands first.
    {
        let (w0, w1, tmp, s) = (create(false), create(false), create(false), create(false));
        let (mut l0, mut l1) = (Landing::new(), Landing::new());
        assert!(attach_hw_fence(w0, 1, l0.va(), 0, 1, 0, true));
        assert!(attach_hw_fence(w1, 1, l1.va(), 0, 1, 1, true));
        assert!(transfer(tmp, 1, w0, 0));
        assert!(transfer(tmp, 2, w1, 0));
        assert!(transfer(s, 0, tmp, 0));
        let vas: Vec<usize> = pending_hw_fences(s, 1).into_iter().map(|f| f.0).collect();
        assert_eq!(vas.len(), 2, "both fences of the chain: {:?}", vas);
        assert!(vas.contains(&l0.va()) && vas.contains(&l1.va()));
        l1.land(1);
        assert!(
            !reached(s, 1),
            "w1 landed first: w0 is still the node before it"
        );
        assert_eq!(query(tmp), Some(0));
        l0.land(1);
        assert!(reached(s, 1));
        assert_eq!(query(tmp), Some(2));
        for h in [w0, w1, tmp, s] {
            assert!(destroy(h));
        }
    }

    // The nodes below stay independent: point 1 is reached when its own
    // source is, whatever point 2 still waits on (no over-sync).
    {
        let (a, b, t) = (create(false), create(false), create(false));
        assert!(transfer(t, 1, a, 0));
        assert!(transfer(t, 2, b, 0));
        assert!(signal(a));
        assert_eq!(query(t), Some(1));
        assert!(reached(t, 1) && !reached(t, 2));
        assert!(signal(b));
        assert_eq!(query(t), Some(2));
        for h in [a, b, t] {
            assert!(destroy(h));
        }
    }

    // A transfer already reached at a point above a node still waiting
    // queues behind it too: behind a link (a wait done, forwarded after
    // one that is not) and behind the object's own fence in flight (an
    // EXEC signal at 3, then a done wait forwarded at 5).
    {
        let (a, b, t) = (create(false), create(true), create(false));
        assert!(transfer(t, 1, a, 0));
        assert!(transfer(t, 2, b, 0));
        assert_eq!(
            query(t),
            Some(0),
            "2 is done, 1 is not: the chain stops at 1"
        );
        assert!(!reached(t, 2));
        assert!(signal(a));
        assert_eq!(query(t), Some(2));
        let u = create(false);
        let mut land = Landing::new();
        assert!(attach_hw_fence(u, 3, land.va(), 0, 1, 0, false));
        assert!(transfer(u, 5, b, 0));
        assert_eq!(query(u), Some(0), "5 is done, 3 has not landed");
        assert!(!reached(u, 5));
        land.land(1);
        assert_eq!(query(u), Some(5));
        for h in [a, b, t, u] {
            assert!(destroy(h));
        }
    }

    // A hardware fence landing at a point above a node still waiting
    // queues behind it (EXEC signals 4 while a transfer at 3 is
    // pending), and a CPU signal below a pending transfer leaves the
    // transfer waiting instead of dropping it.
    {
        let (w, t) = (create(false), create(false));
        assert!(transfer(t, 3, w, 0));
        let mut land = Landing::new();
        assert!(attach_hw_fence(t, 4, land.va(), 0, 1, 0, false));
        land.land(1);
        assert_eq!(poll_pending(), 1, "landed, and held behind the node at 3");
        assert_eq!(
            query(t),
            Some(0),
            "4 landed, 3 has not: the chain stops at 3"
        );
        assert!(!reached(t, 3) && !reached(t, 4));
        assert!(signal(w));
        assert_eq!(query(t), Some(4));
        assert_eq!(pending_now(), 0);
        let (a, u) = (create(false), create(false));
        assert!(transfer(u, 5, a, 0));
        assert!(timeline_signal(u, 3));
        assert_eq!(query(u), Some(3));
        assert!(signal(a));
        assert_eq!(
            query(u),
            Some(5),
            "the transfer to 5 survived the signal to 3"
        );
        for h in [w, t, a, u] {
            assert!(destroy(h));
        }
    }
    assert_eq!(links_now(), 0);
}

/// Linux finds a syncobj handle in the calling file's table and nowhere
/// else: another process's handle is ENOENT for every syncobj ioctl.
/// Here the handle space is global, and with DESTROY the only ioctl
/// that asked who held the handle, a client guessing the compositor's
/// (consecutive) handle numbers could signal its release timelines,
/// reset its acquire fences or wait on them. `usable_by` is the one
/// answer every arm now asks for, holders standing in for the file.
#[test]
fn a_syncobj_is_named_only_by_the_processes_that_hold_it() {
    let _g = test_lock();
    arm_hooks();
    const A: u64 = 92_001;
    const B: u64 = 92_002;
    let a1 = create_for(A, false);
    let b1 = create_for(B, true);
    let kernel = create(false);
    assert!(usable_by(A, a1) && usable_by(B, b1));
    assert!(!usable_by(B, a1), "A's is not B's");
    assert!(!usable_by(A, b1), "B's is not A's");
    assert!(
        usable_by(A, kernel) && usable_by(B, kernel),
        "no file owns a handle nobody holds"
    );
    assert!(
        usable_by(0, a1) && usable_by(0, b1) && usable_by(0, kernel),
        "the kernel itself names any live handle"
    );
    assert!(!usable_by(A, 0xdead_0007) && !usable_by(0, 0xdead_0007));
    // An import (an opaque FD_TO_HANDLE) makes B a holder of a1.
    assert!(add_ref_for(B, a1));
    assert!(usable_by(B, a1) && usable_by(A, a1));
    // The array form is all-or-nothing, like `drm_syncobj_array_find`.
    assert!(all_usable_by(A, &[a1, kernel]));
    assert!(!all_usable_by(A, &[a1, b1]), "one of them is B's alone");
    assert!(!all_usable_by(A, &[a1, 0xdead_0008]));
    assert!(all_usable_by(A, &[]), "nothing to refuse");
    // B gives its reference back: a1 is A's alone again.
    assert!(destroy_for(B, a1));
    assert!(!usable_by(B, a1) && usable_by(A, a1));
    // A destroyed handle is nobody's, not even the kernel's.
    assert!(destroy_for(A, a1));
    assert!(!usable_by(A, a1) && !usable_by(0, a1));
    assert!(destroy_for(B, b1) && destroy(kernel));
}

/// A syncobj handle is a `drm_file` thing in Linux: `SYNCOBJ_DESTROY`
/// frees the caller's reference and nobody else's, and the file's close
/// (`drm_syncobj_release`) gives back every handle the process still
/// had. Here the handle space is global and, before this, `destroy` took
/// whichever reference was there, so a client guessing the compositor's
/// handle numbers could free its timelines, and a client that crashed
/// mid-frame left its syncobjs (and any fence in flight on them) in the
/// table for the rest of the boot.
#[test]
fn a_process_that_dies_gives_back_its_syncobjs_and_only_its_own() {
    let _g = test_lock();
    arm_hooks();
    const A: u64 = 91_001;
    const B: u64 = 91_002;
    let live = || TABLE.lock().objects.iter().filter(|o| o.refs > 0).count();
    let before = live();
    let a1 = create_for(A, false);
    let a2 = create_for(A, true);
    let b1 = create_for(B, false);
    let kernel = create(false);
    assert!(held_by(A, a1) && held_by(A, a2) && held_by(B, b1));
    assert!(!held_by(A, b1) && !held_by(B, a1) && !held_by(A, kernel));
    // B imports a1 (an opaque FD_TO_HANDLE): a reference of its own.
    assert!(add_ref_for(B, a1));
    assert!(held_by(B, a1) && held_by(A, a1));
    assert!(!add_ref_for(B, 0xdead_0001), "unknown handle");
    // A imports its own export of a2: two references of A's on it, and
    // its death gives back both.
    assert!(add_ref_for(A, a2));
    // DESTROY from the wrong process is EINVAL and changes nothing.
    assert!(!destroy_for(B, a2), "a2 is A's alone");
    assert!(!destroy_for(A, b1), "b1 is B's alone");
    assert!(!destroy_for(A, kernel), "no process holds the kernel's");
    assert!(exists(a2) && exists(b1) && exists(kernel));
    assert_eq!(query(a2), Some(1));
    // A's fence in flight on a1: B's EXEC acquires it, so it must
    // survive A's death (B holds the object) and still land.
    let mut landing = Landing::new();
    assert!(attach_hw_fence(a1, 1, landing.va(), 0, 7, 0, true));
    assert_eq!(pending_now(), 1);
    // A dies: a1 (its reference; B keeps the object) and a2 (both
    // references, so freed).
    assert_eq!(release_owner(A), 3);
    assert!(exists(a1), "B still holds a1");
    assert!(!held_by(A, a1) && held_by(B, a1));
    assert!(!exists(a2), "nobody held a2 but A");
    assert!(exists(b1) && exists(kernel));
    assert_eq!(pending_now(), 1, "the fence B waits for is still armed");
    assert_eq!(release_owner(A), 0, "a second teardown finds nothing");
    // (The arm itself already announced the fence to EXEC waiters.)
    let announced = signals().iter().filter(|&&s| s == (a1, 1)).count();
    landing.land(7);
    assert_eq!(query(a1), Some(1), "and it lands for B");
    assert_eq!(
        signals().iter().filter(|&&s| s == (a1, 1)).count(),
        announced + 1,
        "the landing is delivered"
    );
    // B's own DESTROY of the imported reference is the last one: gone.
    assert!(destroy_for(B, a1));
    assert!(!exists(a1));
    assert!(!destroy_for(B, a1), "already gone");
    // A fence in flight on a syncobj nobody else holds goes with its
    // process (no waiter, no WEDGE from the timeout hook later).
    let mut orphan = Landing::new();
    assert!(attach_hw_fence(b1, 1, orphan.va(), 0, 3, 0, true));
    assert_eq!(pending_now(), 1);
    assert_eq!(release_owner(B), 1);
    assert!(!exists(b1));
    assert_eq!(pending_now(), 0);
    let announced = signals().iter().filter(|&&s| s == (b1, 1)).count();
    orphan.land(3);
    assert_eq!(poll_pending(), 0);
    assert_eq!(
        signals().iter().filter(|&&s| s == (b1, 1)).count(),
        announced,
        "nothing is delivered for a dead process's fence"
    );
    // pid 0 is the kernel: nothing recorded, nothing released, and
    // `destroy_for(0, ..)` is plain `destroy`.
    let k2 = create_for(0, false);
    assert!(!held_by(0, k2));
    assert!(add_ref_for(0, k2));
    assert_eq!(release_owner(0), 0);
    assert!(exists(k2) && exists(kernel));
    assert!(destroy_for(0, k2), "the extra reference");
    assert!(destroy_for(0, k2), "the last one");
    assert!(!exists(k2));
    assert!(destroy(kernel));
    assert_eq!(live(), before, "nothing of this test is left in the table");
}

/// A resolve over a table with no chain link must not walk the object
/// list, and the walk it skips is the one a real Vulkan client pays
/// hundreds of times per submit.
///
/// Nothing in Mesa's ordinary path makes a link: they come from
/// `SYNCOBJ_TRANSFER` and sync_file import. So the table a client
/// actually signals against has none, and every `attach_hw_fence` of an
/// `EXEC` carrying 256 signals used to ask all of its objects whether
/// their lowest link was ready.
#[test]
fn a_table_with_no_links_is_resolved_without_walking_its_objects() {
    let _g = test_lock();
    // Retire any link an earlier test left behind: the flag is sticky, so
    // one full walk is what clears it.
    poll_pending();
    let handles: Vec<u32> = (0..8).map(|_| create(false)).collect();
    let before = LINK_WALKS.load(Ordering::Relaxed);
    for _ in 0..16 {
        let mut t = TABLE.lock();
        let d = resolve_locked(&mut t);
        drop(t);
        d.run();
    }
    assert_eq!(
        LINK_WALKS.load(Ordering::Relaxed),
        before,
        "a link-free table should be resolved without a pass over its objects"
    );
    for h in handles {
        assert!(destroy(h));
    }
}

/// ...and the moment a link DOES exist the walk comes back, so the fast
/// path cannot be hiding a link from the fixed point. A link created by
/// any of the four paths that make one has to set the flag; this covers
/// `transfer`, which is the one wlroots' `linux-drm-syncobj-v1` drives.
#[test]
fn a_link_brings_the_walk_back_and_still_resolves() {
    let _g = test_lock();
    poll_pending();
    let (src, dst) = (create(false), create(false));
    // `src` has not reached 5, so the transfer is recorded as a link that
    // waits for it rather than a point already delivered.
    assert!(transfer(dst, 7, src, 5));
    let before = LINK_WALKS.load(Ordering::Relaxed);
    {
        let mut t = TABLE.lock();
        let d = resolve_locked(&mut t);
        drop(t);
        d.run();
    }
    assert!(
        LINK_WALKS.load(Ordering::Relaxed) > before,
        "a table that holds a link must be walked"
    );
    assert_eq!(query(dst), Some(0), "the link has not been satisfied yet");
    // Satisfy it the ordinary way and the link must still resolve.
    assert!(timeline_signal(src, 5));
    assert_eq!(
        query(dst),
        Some(7),
        "the transfer should have landed dst at the point it asked for"
    );
    assert!(destroy(src));
    assert!(destroy(dst));
}

/// A merge is the other path that creates a link, and it is the one on
/// Mesa's swapchain acquire (`merge_fences`): the acquire semaphore is the
/// compositor's release AND the previous present, merged. Its link has to
/// resolve too, so the flag has to be set where the merged object is
/// pushed -- a missed store there would leave a window waiting forever on
/// an acquire whose sources have both signaled.
#[test]
fn a_merge_created_before_its_sources_signal_still_resolves() {
    let _g = test_lock();
    poll_pending();
    let (a, b) = (create(false), create(false));
    let m = merge_fences(&[(a, 1), (b, 1)]);
    assert_eq!(query(m), Some(0), "nothing has signaled yet");
    assert!(signal(a));
    assert_eq!(query(m), Some(0), "one source is still in flight");
    assert!(signal(b));
    assert_eq!(
        query(m),
        Some(1),
        "the merge should be signaled once every source is"
    );
    assert!(destroy(a));
    assert!(destroy(b));
    assert!(destroy(m));
}

/// The batched check answers exactly what the per-handle one did, and
/// names the SAME handle -- the first bad one in the order given -- so the
/// line `EXEC`/`VM_BIND` logs is unchanged.
#[test]
fn the_batched_handle_check_names_the_first_bad_handle() {
    let _g = test_lock();
    let mine = create_for(4242, false);
    let theirs = create_for(4343, false);
    let good = [mine];
    assert_eq!(first_unusable_by(4242, good.iter().copied()), None);
    assert!(all_usable_by(4242, &good));
    // Another process's handle, and one that never existed: whichever
    // comes first is the one named.
    assert_eq!(
        first_unusable_by(4242, [mine, theirs, 0].iter().copied()),
        Some(theirs)
    );
    assert_eq!(
        first_unusable_by(4242, [mine, 0, theirs].iter().copied()),
        Some(0)
    );
    assert!(!all_usable_by(4242, &[mine, theirs]));
    // Agreement with the per-handle predicate it replaced, handle by
    // handle, which is the property the submit paths rely on.
    for h in [mine, theirs, 0, u32::MAX] {
        assert_eq!(
            usable_by(4242, h),
            first_unusable_by(4242, [h].iter().copied()).is_none(),
            "handle {h} disagrees between the batched and per-handle check"
        );
    }
    assert!(destroy_for(4242, mine));
    assert!(destroy_for(4343, theirs));
}
/// [`bundle_fence`] is the `dma_fence_array` of an
/// `EXPORT_SYNC_FILE`: signaled at once with nothing to wait for, and
/// otherwise only once every hardware fence has landed and every
/// imported fence is reached, in any order. The per-fence carriers are
/// nobody's: they live as orphans while the merge names them and go
/// once it is done with them.
#[test]
fn a_bundle_is_reached_once_every_hardware_fence_and_every_import_is() {
    let _g = test_lock();
    arm_hooks();
    assert_eq!(
        query(bundle_fence(&[], &[])),
        Some(1),
        "nothing to wait for"
    );
    let mut first = Landing::new();
    let mut second = Landing::new();
    let landed = Landing::new();
    let import = create(false);
    let hw = |l: &Landing, payload: u32, ctx_idx: u32| HwFence {
        fence_va: l.va(),
        fence_gpu_va: 0,
        payload,
        ctx_idx,
    };
    // A fence that has landed already is not a dependency.
    let past = bundle_fence(&[hw(&landed, 0, 0)], &[]);
    assert_eq!(query(past), Some(1));
    let objects_before = TABLE.lock().objects.len();
    let b = bundle_fence(&[hw(&first, 1, 1), hw(&second, 7, 2)], &[(import, 1)]);
    assert_eq!(query(b), Some(0));
    assert_eq!(
        TABLE.lock().objects.len(),
        objects_before + 3,
        "the merge and its two orphaned carriers"
    );
    assert_eq!(pending_now(), 2, "both hardware fences in flight");
    // The second ring passes first, then the first, then the import:
    // the order does not matter and nothing is reached early.
    second.land(7);
    assert_eq!(query(b), Some(0), "one ring still busy");
    first.land(1);
    assert_eq!(query(b), Some(0), "both rings passed, the import has not");
    assert!(signal(import));
    assert_eq!(query(b), Some(1));
    assert_eq!(pending_now(), 0);
    assert_eq!(
        TABLE.lock().objects.len(),
        objects_before + 1,
        "the carriers were collected; only the merge is left"
    );
    // And the other order, on a bundle of its own: the import reached
    // while the ring has not passed its fence.
    let mut third = Landing::new();
    let import2 = create(false);
    let b2 = bundle_fence(&[hw(&third, 1, 3)], &[(import2, 1)]);
    assert!(signal(import2));
    assert_eq!(query(b2), Some(0), "the ring has not passed");
    third.land(1);
    assert_eq!(query(b2), Some(1));
    destroy(b2);
    destroy(import2);
    destroy(b);
    destroy(past);
    destroy(import);
}

// --- The edges of the table: unknown handles, orphans and marks ------------

/// Every entry point answers an unknown handle with "no" -- and still
/// delivers the signals its resolve pass produced on the way out, the
/// contract `a_failed_ioctl_still_delivers_the_signals_it_resolved` pins
/// for the ioctls that take a destination.
#[test]
fn an_unknown_handle_is_refused_everywhere_and_the_resolve_still_counts() {
    let _g = test_lock();
    const NOBODY: u32 = 0xdead_0042;
    assert!(!exists(NOBODY));
    let other = create(false);
    let mut landed = Landing::new();
    assert!(attach_hw_fence(other, 1, landed.va(), 0, 1, 0, true));
    landed.land(1);
    // From here: the arm itself announced the submit, the landing has not
    // been seen yet.
    arm_hooks();
    let mut zone = Landing::new();
    // Not landed: the slow arm, which looks the handle up under the lock.
    assert!(!attach_hw_fence(NOBODY, 1, zone.va(), 0, 1, 0, true));
    assert!(
        signals().contains(&(other, 1)),
        "the fence that had landed on `other` was resolved on the way, {:?}",
        signals()
    );
    assert_eq!(query(other), Some(1));
    assert_eq!(pending_now(), 0, "nothing was attached to nobody");
    // Landed already: the fast arm, a plain signal of an unknown handle.
    zone.land(1);
    assert!(!attach_hw_fence(NOBODY, 1, zone.va(), 0, 1, 0, true));
    assert!(!timeline_signal(NOBODY, 3));
    assert!(!signal(NOBODY));
    assert!(!add_ref(NOBODY));
    assert!(!reset(NOBODY));
    assert_eq!(query(NOBODY), None);
    assert!(
        pending_hw_fences(NOBODY, 1).is_empty() && pending_hw_fences(NOBODY, 7).is_empty(),
        "no fence to acquire behind a handle that is not there"
    );
    destroy(other);
}

/// `drm_syncobj_get`: a second reference keeps the object through one
/// `DESTROY`, and the last one takes it.
#[test]
fn a_second_reference_keeps_the_object_through_one_destroy() {
    let _g = test_lock();
    let h = create(true);
    assert!(add_ref(h));
    assert!(destroy(h));
    assert!(exists(h), "one reference left");
    assert_eq!(query(h), Some(1), "and the object untouched");
    assert!(destroy(h));
    assert!(!exists(h));
    assert!(!destroy(h), "nothing left to destroy");
}

/// An orphan (`destroy`ed while a link still named it) linked into `dst`:
/// the source handle, its fence still in flight so it stays in the table.
fn orphan_linked_into(dst: u32, zone: &Landing) -> u32 {
    let src = create(false);
    assert!(attach_hw_fence(src, 1, zone.va(), 0, 1, 0, true));
    assert!(import_snapshot(dst, src, 1));
    let objects = object_count();
    assert!(destroy(src));
    assert_eq!(
        object_count(),
        objects,
        "kept as an orphan: dst's link names it"
    );
    src
}

/// Whatever replaces the link that named an orphan lets the orphan go, in
/// the same call: a binary re-arm of the destination, a reset of it, and a
/// new import into it. (`timeline_signal` is the case
/// `an_orphaned_source_is_collected_once_nothing_depends_on_it` pins.)
#[test]
fn replacing_the_link_that_named_an_orphan_collects_it() {
    let _g = test_lock();
    let dst = create(false);
    let zone = Landing::new();

    // A binary re-arm: the slot's links go with its fence.
    let src = orphan_linked_into(dst, &zone);
    let objects = object_count();
    let mut mine = Landing::new();
    assert!(attach_hw_fence(dst, 1, mine.va(), 0, 1, 0, true));
    assert_eq!(object_count(), objects - 1, "the orphan is collected");
    assert!(!exists(src));
    assert_eq!(pending_now(), 1, "its fence went with it; dst's own stays");
    mine.land(1);
    assert_eq!(query(dst), Some(1), "which lands as usual");

    // A reset: "this object carries no fence at all".
    let src = orphan_linked_into(dst, &zone);
    let objects = object_count();
    assert!(reset(dst));
    assert_eq!(object_count(), objects - 1);
    assert!(!exists(src));
    assert_eq!(pending_now(), 0);
    assert_eq!(query(dst), Some(0));

    // A new import: the import REPLACES the destination's fence.
    let src = orphan_linked_into(dst, &zone);
    let next = create(false);
    let objects = object_count();
    assert!(import_snapshot(dst, next, 1));
    assert_eq!(object_count(), objects - 1);
    assert!(!exists(src));
    assert_eq!(query(dst), Some(0), "and dst now waits for `next`");
    assert!(signal(next));
    assert_eq!(query(dst), Some(1));
    destroy(next);
    destroy(dst);
}

/// The process-side destroys deliver the same release a kernel `destroy`
/// does: a transfer whose source went with its process, by one `DESTROY`
/// or by the process dying, still wakes the waiter on the point it
/// promised.
#[test]
fn a_process_taking_its_transfer_source_away_releases_what_it_promised() {
    let _g = test_lock();
    arm_hooks();
    const A: u64 = 92_001;
    let src = create_for(A, false);
    let dst = create(false);
    assert!(transfer(dst, 9, src, 5));
    assert_eq!(query(dst), Some(0));
    assert!(destroy_for(A, src));
    assert!(
        signals().contains(&(dst, 9)),
        "the eventfd on point 9 was woken, got {:?}",
        signals()
    );
    assert_eq!(query(dst), Some(9));

    let src = create_for(A, false);
    let later = create(false);
    assert!(transfer(later, 4, src, 2));
    assert_eq!(release_owner(A), 1);
    assert!(
        signals().contains(&(later, 4)),
        "a process dying releases it too, got {:?}",
        signals()
    );
    assert_eq!(query(later), Some(4));
    assert!(!exists(src));
    destroy(dst);
    destroy(later);
}

/// A tainted point transferred BEHIND a node of the destination's chain
/// carries its mark from that node, not from the counter: the points up
/// to the node are the GPU's own word, and an EXEC that waited on one of
/// them must not answer EIO for a timeout it never waited on.
#[test]
fn a_tainted_point_queued_behind_a_node_marks_only_what_it_delivers() {
    let _g = test_lock();
    arm_hooks();
    let src = create(false);
    let dead = Landing::new();
    assert!(attach_hw_fence(src, 5, dead.va(), 0, 2, 0, false));
    test_clock::advance(FENCE_TIMEOUT_US + 1);
    assert_eq!(query(src), Some(5), "given up on: reached, and tainted");
    assert!(reached_by_timeout(&[src], Some(&[5])));

    // The destination waits at point 2 on a fence of its own in flight.
    let dst = create(false);
    let mut node = Landing::new();
    assert!(attach_hw_fence(dst, 2, node.va(), 0, 1, 0, false));
    // Point 5 of `src` is reached already, but at 4 on `dst` it queues
    // behind the node at 2, as a chain would.
    assert!(transfer(dst, 4, src, 5));
    assert_eq!(query(dst), Some(0), "queued behind the node");
    assert!(
        reached_by_timeout(&[dst], Some(&[4])),
        "the mark is on from the start"
    );
    assert!(
        reached_by_timeout(&[dst], Some(&[3])),
        "3 is delivered by the same transfer"
    );
    assert!(
        !reached_by_timeout(&[dst], Some(&[2])) && !reached_by_timeout(&[dst], Some(&[1])),
        "1 and 2 are the node's: the GPU's own word"
    );
    node.land(1);
    assert_eq!(
        query(dst),
        Some(4),
        "the node landed and the point behind it followed"
    );
    assert!(reached_by_timeout(&[dst], Some(&[4])));
    assert!(!reached_by_timeout(&[dst], Some(&[2])));
    destroy(src);
    destroy(dst);
}

/// The stall reporter's budget: a boot gets [`MAX_STALL_REPORTS`] lines
/// and then nothing, however many waits park -- each report is a
/// synchronous UART write on a path that may be re-entered.
#[test]
fn the_stall_reporter_writes_its_budget_and_then_falls_silent() {
    let budget = AtomicU32::new(0);
    let numbers: Vec<_> = (0..MAX_STALL_REPORTS + 3)
        .map(|_| stall_report_number(&budget))
        .collect();
    let expected: Vec<_> = (0..MAX_STALL_REPORTS)
        .map(Some)
        .chain(core::iter::repeat_n(None, 3))
        .collect();
    assert_eq!(numbers, expected);
    assert!(
        stall_report_number(&budget).is_none(),
        "spent for the rest of the boot"
    );
}
