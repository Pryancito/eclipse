use super::*;

/// A `drm_mode_modeinfo` that states its own refresh, which is the field
/// Linux fills in and the one every mode this driver advertises carries.
fn modeinfo_at(hz: u32) -> [u8; 68] {
    let mut m = [0u8; 68];
    m[24..28].copy_from_slice(&hz.to_ne_bytes());
    m
}

/// The lattice of a session that booted and has been at `hz` ever since:
/// anchored on the epoch at sequence 0, which is where `VBLANK_LATTICE`
/// starts and the state in which the counter is exactly the old
/// `now_ns / period_ns`. Anchoring a test's lattice at its own `now_ns`
/// instead would hide the whole bug, because a rescale of a zero-length
/// interval is a no-op.
///
/// These tests then evaluate it at an explicit `now_ns` rather than at
/// whatever the clock does between two calls: a modeset is sub-millisecond
/// and a vblank is 7 ms, so a test that merely read the counter twice would
/// be deciding its own outcome on scheduling.
fn booted_at(hz: u64) -> VblankLattice {
    VblankLattice {
        base_ns: 0,
        base_seq: 0,
        period_ns: 1_000_000_000 / hz,
    }
}

/// An hour of uptime, in nanoseconds.
const AN_HOUR: u64 = 3_600 * 1_000_000_000;

/// Install `lattice` as the live one, for the tests that go through the
/// public setters.
fn install(lattice: VblankLattice) {
    *VBLANK_LATTICE.lock() = lattice;
}

/// **The bug.** Dropping the refresh must not drop the counter.
///
/// The counter was `now_ns / period_ns`, so it was not a counter but a
/// scale of the clock: at 144 Hz an hour in it read 518400, and the instant
/// wlroots committed `MODE_ID = 0` and the period fell back to 60 Hz the
/// same clock read 216000 -- three hundred thousand vblanks backwards. Both
/// halves of `WAIT_VBLANK` are then waiting for a sequence that will not
/// come round again for an hour and a half, and the event form has no cap
/// at all.
#[test]
fn a_lower_refresh_does_not_take_the_counter_backwards() {
    let mut lattice = booted_at(144);
    let before = lattice.seq_full(AN_HOUR);
    assert_eq!(before, 518_400);

    lattice.set_period(AN_HOUR, 1_000_000_000 / 60);
    let now_ns = AN_HOUR;

    assert_eq!(
        lattice.seq_full(now_ns),
        before,
        "the counter moved across the modeset (the old scaling read 216000 here)"
    );
    // And it keeps counting from there, at the new rate.
    assert_eq!(lattice.seq_full(now_ns + 1_000_000_000), before + 60);
}

/// The other direction was just as wrong, and is what the FIRST modeset
/// does: the 60 Hz fallback -> the panel's 144 Hz used to multiply the
/// counter by 2.4 in one step, which wlroots hands to clients as
/// presentation feedback and Xorg Present reads as a target MSC it has
/// already missed.
#[test]
fn a_higher_refresh_does_not_take_the_counter_forwards() {
    let mut lattice = booted_at(60);
    assert_eq!(lattice.seq_full(AN_HOUR), 216_000);

    lattice.set_period(AN_HOUR, 1_000_000_000 / 144);
    let now_ns = AN_HOUR;

    assert_eq!(
        lattice.seq_full(now_ns),
        216_000,
        "the counter moved across the modeset (the old scaling read 518400 here)"
    );
    assert_eq!(lattice.seq_full(now_ns + 1_000_000_000), 216_000 + 144);
}

/// Re-anchoring may not shift the phase either: the boundary the counter
/// last crossed keeps both its instant and its sequence, so a flip
/// completion already paced to it is not pulled forward or pushed back by
/// the modeset.
#[test]
fn a_modeset_keeps_the_boundary_the_counter_last_crossed() {
    let mut lattice = booted_at(60);
    // Mid-period, so the re-anchor has a boundary to find rather than
    // landing on the one it started from.
    let now_ns = lattice.period_ns * 4_096 + lattice.period_ns / 3;
    let seq = lattice.seq_full(now_ns);
    let boundary = lattice.last_boundary_ns(now_ns);
    assert_eq!(seq, 4_096);

    lattice.set_period(now_ns, 1_000_000_000 / 144);

    assert_eq!(
        lattice.base_seq, seq,
        "the anchor took a sequence the counter had not reached"
    );
    assert_eq!(
        lattice.base_ns, boundary,
        "the anchor moved off the last boundary"
    );
    assert_eq!(lattice.seq_full(now_ns), seq, "the counter moved");
}

/// The counter must stay continuous over a whole run of modesets, through
/// the setters the ioctls actually call: wlroots turns an output off and on
/// again (DPMS, VT switch, output enable) and every commit lands there.
/// 75, 59 and 50 Hz are what an ordinary monitor offers -- it takes no
/// exotic refresh to move the lattice.
#[test]
fn the_counter_never_goes_backwards_over_a_run_of_modesets() {
    let _serialised = super::test_globals::lock();
    install(booted_at(60));
    let mut last = vblank_seq_full();
    assert!(
        last > 0,
        "this test needs some uptime to rescale: the clock read {} ns",
        monotonic_ns()
    );
    for hz in [144u32, 60, 75, 59, 144, 50, 60] {
        set_vblank_period_from_modeinfo(&modeinfo_at(hz));
        assert_eq!(
            vblank_period_ns(),
            1_000_000_000 / u64::from(hz),
            "{} Hz was not applied",
            hz
        );
        let now = vblank_seq_full();
        assert!(
            now >= last,
            "{} Hz took the counter {} -> {}",
            hz,
            last,
            now
        );
        last = now;
        // `MODE_ID = 0` / a null-fb SETCRTC: back to the fallback.
        reset_vblank_period();
        assert_eq!(
            vblank_period_ns(),
            1_000_000_000 / FALLBACK_VBLANK_HZ,
            "the output going off after {} Hz left the mode's pacing",
            hz
        );
        let now = vblank_seq_full();
        assert!(
            now >= last,
            "the output going off after {} Hz took the counter {} -> {}",
            hz,
            last,
            now
        );
        last = now;
    }
    reset_vblank_period();
}

/// The deadline for a target has to be measured on the lattice the counter
/// is actually on. It used to be `target * period`, which after a modeset
/// pointed at an instant with no relation to the counter: a target one
/// vblank out came back as a sleep of hours, so `wait_vblank_sleep` sat on
/// its 3 s cap waiting for a frame that was 16 ms away.
#[test]
fn a_target_one_vblank_out_is_one_vblank_away_after_a_modeset() {
    let _serialised = super::test_globals::lock();
    install(booted_at(144));
    set_vblank_period_from_modeinfo(&modeinfo_at(60));
    let period = vblank_period_ns();

    let now = kernel_hal::timer::timer_now();
    let target = vblank_seq_now().wrapping_add(1);
    let deadline = vblank_deadline_for_seq(target).expect("a target ahead has a deadline");

    assert!(
        deadline > now && deadline <= now + Duration::from_nanos(period),
        "a target one vblank out is {:?} away, not within one {} ns period",
        deadline.saturating_sub(now),
        period
    );
    // A target already reached is reported as reached, not as a deadline in
    // the past.
    assert_eq!(vblank_deadline_for_seq(vblank_seq_now()), None);
    reset_vblank_period();
}

/// The period is never 0, whatever the mode says -- a zero period is an
/// immediately-and-forever-due timer, and `ticks_since_base` divides by it.
#[test]
fn the_period_is_never_zero() {
    let _serialised = super::test_globals::lock();
    set_vblank_hz(0);
    assert!(vblank_period_ns() > 0);
    let _no_division_by_zero = vblank_seq_full();
    set_vblank_period_from_modeinfo(&[0u8; 68]);
    assert_eq!(vblank_period_ns(), 1_000_000_000 / FALLBACK_VBLANK_HZ);
    // `vrefresh` is a `u32` the client fills in and nothing caps it, so a
    // mode claiming four billion Hz reaches here and divides to zero. The
    // period used to be clamped on every read, which hid that; it is
    // clamped once on the way in now, and this is what holds the clamp.
    set_vblank_period_from_modeinfo(&modeinfo_at(u32::MAX));
    assert!(
        vblank_period_ns() > 0,
        "a mode claiming {} Hz left a zero period",
        u32::MAX
    );
    let _still_no_division_by_zero = vblank_seq_full();
    reset_vblank_period();
}
