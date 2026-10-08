extern crate std;

use super::*;
use core::time::Duration;

/// Put the process-global latches back however the body leaves -- or panics
/// out of -- them, and serialise against every other test that presents.
struct Restored {
    _serialised: self::std::sync::MutexGuard<'static, ()>,
}

impl Drop for Restored {
    fn drop(&mut self) {
        reset_output_state_for_test();
    }
}

fn serialised() -> Restored {
    let g = Restored {
        _serialised: super::test_globals::lock(),
    };
    reset_output_state_for_test();
    g
}

// --- the decision itself, with no statics and no clock in the way ---

/// A pause with no watchdog is the permanent kind, and `0` is how the
/// deadline says so. It must not read as "a deadline in the distant past".
#[test]
fn a_pause_with_no_watchdog_holds_whatever_the_clock_says() {
    for now in [0u64, 1, 1_000_000_000, u64::MAX] {
        assert!(
            pause_in_force(true, 0, now),
            "a pause with no watchdog lifted itself at now={}",
            now
        );
    }
}

/// The watchdog's whole contract, at the boundary: in force up to the
/// deadline, over the moment it arrives. An off-by-one the wrong way is a
/// pause that outlives its window.
#[test]
fn a_watchdogged_pause_holds_up_to_its_deadline_and_not_past_it() {
    let deadline = 90_000_000_000;
    for now in [0u64, 1, deadline - 1] {
        assert!(
            pause_in_force(true, deadline, now),
            "expired early at now={}",
            now
        );
    }
    for now in [deadline, deadline + 1, u64::MAX] {
        assert!(
            !pause_in_force(true, deadline, now),
            "still paused at now={}, past its deadline",
            now
        );
    }
}

/// With the latch off nothing is paused, whatever the deadline still holds.
/// A resume clears both, but not in one instruction, so a reader can see
/// this combination.
#[test]
fn nothing_is_paused_while_the_latch_is_off() {
    for deadline in [0u64, 1, 90_000_000_000, u64::MAX] {
        assert!(!pause_in_force(false, deadline, 1_000));
    }
}

/// The sentinel collision, which is how the watchdog turns into the freeze
/// it exists to prevent: store a deadline of `0` and every reader reads
/// "no watchdog". `now + max` can land there -- a clock that starts at zero
/// with a zero window, or a sum truncated by the cast -- so the one thing
/// this may never return is `0`.
#[test]
fn a_watchdog_deadline_is_never_the_value_that_means_no_watchdog() {
    let cases = [
        (0u64, Duration::ZERO),
        (0, Duration::from_nanos(0)),
        (0, Duration::from_secs(90)),
        (1, Duration::ZERO),
        (u64::MAX, Duration::from_secs(90)),
        (u64::MAX / 2, Duration::from_secs(u64::MAX / 2)),
    ];
    for (now, max) in cases {
        let deadline = pause_deadline_ns(now, max);
        assert_ne!(
            deadline, 0,
            "now={} max={:?} gave the no-watchdog sentinel, i.e. a pause \
             that never lifts",
            now, max
        );
        // And the pause it describes is one a reader can get out of: either
        // the window is still open, or it has already closed.
        assert!(
            !pause_in_force(true, deadline, u64::MAX),
            "now={} max={:?} gave a deadline no clock can pass",
            now,
            max
        );
    }
}

/// And the window really is as long as it was asked for. `as_nanos` is a
/// `u128`; the cast to the stored `u64` truncates, which turns a long window
/// into a short one -- the same mistake as storing the sentinel, one step
/// milder, and equally invisible without a number to compare against.
#[test]
fn the_watchdog_window_is_as_long_as_it_was_asked_for() {
    assert_eq!(
        pause_deadline_ns(1_000, Duration::from_secs(90)),
        1_000 + 90_000_000_000
    );
    assert_eq!(pause_deadline_ns(0, Duration::from_nanos(1)), 1);
    // A window longer than the deadline can hold stops at the far end
    // rather than wrapping round to a moment that has already passed.
    assert_eq!(
        pause_deadline_ns(5, Duration::from_secs(u64::MAX)),
        u64::MAX,
        "an absurd window was truncated into a short one"
    );
}

/// A window that has not run out is worth more than the plain pause that
/// would replace it: one frees itself after 90 seconds, the other never
/// does. A second `set_scanout_paused(true)` must not make that trade.
#[test]
fn a_second_plain_pause_leaves_a_running_watchdog_alone() {
    let deadline = 90_000_000_000;
    for now in [0u64, 1, deadline - 1] {
        assert_eq!(
            deadline_kept_by_plain_pause(deadline, now),
            deadline,
            "the watchdog was thrown away at now={}, turning a 90-second \
             freeze into a permanent one",
            now
        );
    }
}

/// And the mirror image: a deadline already in the past is not a watchdog,
/// it is a pause that lifts on its first read. Inheriting it would make the
/// call not pause at all.
#[test]
fn a_plain_pause_does_not_inherit_a_watchdog_that_already_ran_out() {
    let deadline = 90_000_000_000;
    for now in [deadline, deadline + 1, u64::MAX] {
        assert_eq!(
            deadline_kept_by_plain_pause(deadline, now),
            0,
            "now={} kept an expired deadline, so the pause would lift \
             immediately",
            now
        );
    }
    // "No watchdog" is already the answer and stays it.
    assert_eq!(deadline_kept_by_plain_pause(0, 12_345), 0);
}

/// A damage rect that covers the whole display catches the panel up just as
/// a full-frame present does -- a `DIRTYFB` clip over the entire framebuffer
/// is a legal way to say "all of it changed". Anything short of that does
/// not, and neither does any rect when there is no display to catch up with.
#[test]
fn only_a_present_that_covers_the_whole_screen_catches_the_panel_up() {
    let screen = Some((1920, 1080));
    assert!(present_caught_the_panel_up(None, screen));
    assert!(
        present_caught_the_panel_up(None, None),
        "a full frame is one"
    );
    assert!(present_caught_the_panel_up(
        Some((0, 0, 1920, 1080)),
        screen
    ));
    assert!(
        present_caught_the_panel_up(Some((0, 0, 3840, 2160)), screen),
        "a rect larger than the display still covers it"
    );
    for short in [
        (0, 0, 1919, 1080),
        (0, 0, 1920, 1079),
        (1, 0, 1920, 1080),
        (0, 1, 1920, 1080),
        (48, 25, 180, 160),
    ] {
        assert!(
            !present_caught_the_panel_up(Some(short), screen),
            "{:?} leaves rows or columns behind and must keep the mark",
            short
        );
    }
    assert!(
        !present_caught_the_panel_up(Some((0, 0, 1920, 1080)), None),
        "with no display there is nothing to be caught up with"
    );
}

// --- and the same decisions through the real latches ---

/// The plain pause is the one that latches. It must survive being read, and
/// read, and read -- the present path asks once per frame.
#[test]
fn a_plain_pause_survives_every_read_until_someone_lifts_it() {
    let _restored = serialised();
    set_scanout_paused(true);
    for _ in 0..8 {
        assert!(scanout_paused(), "a pause with no watchdog lifted itself");
    }
    set_scanout_paused(false);
    assert!(!scanout_paused());
}

/// The watchdog, end to end: a window that has already closed is lifted by
/// the first reader, and stays lifted. A zero-length window is the same code
/// path as a 90-second one that ran out, without a test that sleeps.
#[test]
fn the_watchdog_lifts_a_pause_nobody_came_back_for() {
    let _restored = serialised();
    set_scanout_paused_for(Duration::ZERO);
    assert!(
        !scanout_paused(),
        "a window that has already closed still counted as paused"
    );
    assert!(!scanout_paused(), "and it must not come back");
    assert_eq!(
        SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst),
        0,
        "the expired deadline was left behind for the next pause to inherit"
    );
}

/// A window that is still open is not cut short by a reader.
#[test]
fn a_watchdog_that_has_not_run_out_keeps_the_pause() {
    let _restored = serialised();
    set_scanout_paused_for(SCANOUT_PAUSE_MAX);
    assert!(scanout_paused());
    assert!(scanout_paused());
}

/// The clobber, through the real latches: pausing again while the bring-up
/// window is open used to store the no-watchdog sentinel first, so a wedge
/// after it froze the desktop for good instead of for 90 seconds.
#[test]
fn pausing_again_does_not_turn_a_bounded_freeze_into_a_permanent_one() {
    let _restored = serialised();
    set_scanout_paused_for(SCANOUT_PAUSE_MAX);
    let armed = SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst);
    assert_ne!(armed, 0);

    set_scanout_paused(true);

    assert!(scanout_paused());
    assert_eq!(
        SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst),
        armed,
        "the second pause cancelled the watchdog"
    );
}

/// A present taken during the pause is reported complete -- the compositor's
/// frame loop must not stop -- and records what it bound, so `GETCRTC` stays
/// truthful. What it must ALSO do is admit that the panel no longer shows
/// that framebuffer, because nothing else in the tree can tell afterwards.
#[test]
fn a_present_dropped_by_the_pause_is_acknowledged_and_marks_the_panel_stale() {
    let _restored = serialised();
    set_scanout_paused(true);

    assert!(
        present_now_checked(0x5151, SYNTH_CRTC_ID, None).is_ok(),
        "a paused present must still be acknowledged, or the compositor \
         blocks in poll() for a frame nobody will report"
    );
    assert_eq!(crtc_fb(), 0x5151, "GETCRTC must still report what it bound");
    assert!(
        SCANOUT_STALE.load(Ordering::SeqCst),
        "the panel is a frame behind crtc_fb and nothing recorded it"
    );
}

/// Resuming with nothing ever bound has no frame to put back, so it clears
/// the mark rather than leaving it set for the next pointer move to act on.
#[test]
fn a_resume_with_nothing_bound_clears_the_mark_instead_of_presenting() {
    let _restored = serialised();
    set_scanout_paused(true);
    assert!(present_now_checked(0, SYNTH_CRTC_ID, None).is_ok());
    assert!(SCANOUT_STALE.load(Ordering::SeqCst));

    set_scanout_paused(false);

    assert!(!scanout_paused());
    assert!(
        !SCANOUT_STALE.load(Ordering::SeqCst),
        "nothing was ever bound, so there is no stale frame to chase"
    );
}

/// A client that turned the CRTC off during the pause meant it. The resume
/// must not light the panel behind its back -- and must not drop the mark
/// either, or the present that does un-blank would restore rects from a
/// frame the panel never showed.
#[test]
fn a_resume_does_not_light_a_panel_the_client_turned_off() {
    let _restored = serialised();
    set_scanout_paused(true);
    assert!(present_now_checked(0x6262, SYNTH_CRTC_ID, None).is_ok());
    set_crtc_blanked(true);

    set_scanout_paused(false);

    assert!(crtc_blanked(), "the resume turned the screen back on");
    assert!(
        SCANOUT_STALE.load(Ordering::SeqCst),
        "the mark was dropped while the panel was still a frame behind"
    );
}

/// The harness itself. `reset_output_state_for_test` is what stands between
/// a test that leaves a pause behind and every present test that follows
/// passing while touching nothing at all.
#[test]
fn the_reset_between_tests_lifts_a_leaked_pause() {
    let _restored = serialised();
    set_scanout_paused_for(SCANOUT_PAUSE_MAX);
    assert!(present_now_checked(0x7373, SYNTH_CRTC_ID, None).is_ok());
    assert!(scanout_paused());

    reset_output_state_for_test();

    assert!(!scanout_paused(), "a pause survived the reset");
    assert_eq!(SCANOUT_PAUSE_DEADLINE_NS.load(Ordering::SeqCst), 0);
    assert!(!SCANOUT_STALE.load(Ordering::SeqCst));
}
