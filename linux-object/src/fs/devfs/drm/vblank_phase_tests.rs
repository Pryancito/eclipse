use super::*;

/// The first frame of a boot has had no vblank, so there is no "time since
/// the last vblank" to report. `next_vblank` starts at zero, and measuring
/// from the epoch printed `missed refresh slot (12430386us since last
/// vblank)` at the very first present on real hardware -- twelve seconds,
/// which was the uptime. The message is one-shot, so that false alarm also
/// spent the one report a REAL catch-up would ever get.
///
/// What the first call must do instead: seed the lattice and deliver now,
/// silently. This test pins the seeding, which is the observable half --
/// the phase has to land on a period boundary at or just below `now`, not
/// stay at zero and not jump ahead of `now`.
#[test]
fn the_first_frame_of_a_boot_seeds_the_phase_instead_of_reporting_a_missed_slot() {
    let _serialised = super::test_globals::lock();
    reset_vblank_period();
    let period = vblank_period_ns();
    DRM_STATE.lock().next_vblank = Duration::ZERO;

    let before = kernel_hal::timer::timer_now();
    let deadline = next_vblank_deadline();
    let after = kernel_hal::timer::timer_now();

    // Delivered immediately: the deadline is the `now` it read, which lies
    // in the window this test bracketed.
    assert!(
        deadline >= before && deadline <= after,
        "first frame must deliver at once, got {:?} outside {:?}..={:?}",
        deadline,
        before,
        after
    );

    // And the phase is now on the lattice, below `now`, not still zero.
    let seeded = u64::try_from(DRM_STATE.lock().next_vblank.as_nanos()).unwrap();
    assert_ne!(seeded, 0, "the phase was left unseeded");
    // On the lattice, which is what "on-grid" means once a modeset can
    // re-anchor it -- `seeded % period == 0` only held while the lattice
    // was pinned to the boot epoch.
    assert_eq!(
        seeded % period,
        VBLANK_LATTICE.lock().base_ns % period,
        "the phase sits off-grid"
    );
    let now_ns = u64::try_from(deadline.as_nanos()).unwrap();
    assert!(seeded <= now_ns && now_ns - seeded < period);

    // A second call, one period on, is an ordinary on-time frame: it waits
    // for the slot rather than taking the catch-up path again.
    let next = next_vblank_deadline();
    assert!(
        next > deadline,
        "a seeded phase must pace the next frame, got {:?} <= {:?}",
        next,
        deadline
    );
    DRM_STATE.lock().next_vblank = Duration::ZERO;
}
