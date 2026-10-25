//! The two places in `tty_ioctl` whose numbers come from the Linux headers
//! and where nothing had ever named one.
//!
//! Both were measured the same way: sixteen mutations against this crate's
//! whole suite, and **all sixteen survived**. Not one of the 3820 tests
//! reaches either the `TCFLSH` arm of [`super::tty_ioctl`] or
//! [`super::kdgkbent_value`].
//!
//! Neither failure is loud. A `tcflush` that answers `Ok` without flushing
//! leaves what was typed before a password prompt sitting in the queue, to be
//! read by whatever runs next; a keymap value from the wrong table makes an X
//! server bind the arrow keys to letters. The machine keeps running either
//! way.
//!
//! The expectations here are **written out from the headers**, never composed
//! from the constants the code uses. A test that builds its expectation the
//! same way the code does proves only that the two halves agree with each
//! other, and that is exactly what was missing: every one of those sixteen
//! mutations changed both halves at once.

extern crate std;

use super::*;
use lock::Mutex as TestMutex;

/// A VT of this module's own. 3 belongs to the line-discipline tests, 4 to
/// the stdout ones and 6 to the compositor's; this one is otherwise idle, and
/// the queues it touches are process-wide.
const VT: usize = 5;

static SERIAL: TestMutex<()> = TestMutex::new(());

/// `kb_value` is `KTYP << 8 | KVAL`, so each table below is the high byte.
///
/// `KT_LATIN` is 0, `KT_SPEC` 2, `KT_CUR` 6 and `KT_SHIFT` 7, which is what
/// makes every number here a `0x0?xx`. Those four now come from
/// `super::ioctl`, where [`super::ioctl`]'s own test names each one; what this
/// test adds is the other half, the low byte that picks one key out of its
/// table, and that a given key is answered from the table it belongs to.
#[test]
fn every_modifier_answers_from_the_shift_table_and_keeps_its_own_place() {
    use zcore_drivers::input::input_event_codes::key::*;
    // Both shifts are the same entry: a keymap has one K_SHIFT.
    assert_eq!(kdgkbent_value(KEY_LEFTSHIFT, 0), 0x0700, "K_SHIFT");
    assert_eq!(kdgkbent_value(KEY_RIGHTSHIFT, 0), 0x0700, "K_SHIFT, right");
    // AltGr is 1 and Ctrl is 2, so a swap of the two leaves a keyboard whose
    // Ctrl key composes accented characters.
    assert_eq!(kdgkbent_value(KEY_RIGHTALT, 0), 0x0701, "K_ALTGR");
    assert_eq!(kdgkbent_value(KEY_LEFTCTRL, 0), 0x0702, "K_CTRL");
    assert_eq!(kdgkbent_value(KEY_RIGHTCTRL, 0), 0x0702, "K_CTRL, right");
    assert_eq!(kdgkbent_value(KEY_LEFTALT, 0), 0x0703, "K_ALT");
}

/// The four cursor keys, each from `KT_CUR` and each with its own low byte.
///
/// Down is 0, left 1, right 2 and up 3 -- an order that looks arbitrary
/// because it is: it is the order the table has, and nothing but a written
/// number holds it. Any rotation of the four passes a test that only asks
/// that they differ.
#[test]
fn the_cursor_keys_are_the_four_entries_of_their_table_in_order() {
    use zcore_drivers::input::input_event_codes::key::*;
    assert_eq!(kdgkbent_value(KEY_DOWN, 0), 0x0600, "K_DOWN");
    assert_eq!(kdgkbent_value(KEY_LEFT, 0), 0x0601, "K_LEFT");
    assert_eq!(kdgkbent_value(KEY_RIGHT, 0), 0x0602, "K_RIGHT");
    assert_eq!(kdgkbent_value(KEY_UP, 0), 0x0603, "K_UP");
}

/// Enter is a `KT_SPEC` entry; Escape is a Latin one that happens to be a
/// control character.
///
/// The two sit in different tables for a reason a keymap loader acts on:
/// `KT_SPEC` entries are actions, and a `KT_LATIN` entry is a character to
/// insert. Escape coming back as a special would make it do something instead
/// of sending `0x1b`.
#[test]
fn enter_is_an_action_and_escape_is_a_character() {
    use zcore_drivers::input::input_event_codes::key::*;
    assert_eq!(kdgkbent_value(KEY_ENTER, 0), 0x0201, "K_ENTER");
    assert_eq!(kdgkbent_value(KEY_KPENTER, 0), 0x0201, "K_ENTER, keypad");
    assert_eq!(kdgkbent_value(KEY_ESC, 0), 0x001b, "Latin table, ESC");
}

/// An ordinary key answers with its character, and the table byte the caller
/// asked for is what picks which character.
///
/// `kb_table` is a bitmask here -- bit 0 is shift, bit 1 is AltGr -- which is
/// the whole reason `KDGKBENT` is called once per key *per table*: a loader
/// walks the tables to find out what each key does with each modifier held.
/// A table byte that is ignored gives every entry the unshifted character and
/// an X server with no capital letters.
#[test]
fn the_table_byte_is_the_modifier_mask_the_key_is_asked_about() {
    use zcore_drivers::input::input_event_codes::key::*;
    assert_eq!(kdgkbent_value(KEY_A, 0), 0x61, "'a'");
    assert_eq!(kdgkbent_value(KEY_A, 1), 0x41, "shift: 'A'");
    assert_eq!(kdgkbent_value(KEY_1, 0), 0x31, "'1'");
}

/// A key this keymap says nothing about answers 0, and that is a value, not
/// an absence.
///
/// 0 is `KTYP` 0 (`KT_LATIN`) and `KVAL` 0, which spells "this key types
/// NUL". It is what the function has always answered and the test pins it so
/// the answer stops drifting silently; whether a loader should instead be
/// told the entry is unbound is a separate question, noted where the constant
/// is declared and not decided here.
#[test]
fn a_key_with_nothing_bound_to_it_answers_a_zero_on_purpose() {
    use zcore_drivers::input::input_event_codes::key::*;
    assert_eq!(kdgkbent_value(KEY_F13, 0), 0, "NO_SYMBOL");
    assert_eq!(kdgkbent_value(KEY_SYSRQ, 0), 0, "NO_SYMBOL");
}

/// `TCFLSH` takes its selector **by value**, and the three it accepts decide
/// which queue is thrown away.
///
/// `pty.rs` carries the same three constants in the same shape and its tests
/// name every one of them by number. This arm, the one a real terminal goes
/// through, had none: the selector could have been any three numbers at all.
/// 0 and 2 drop what was typed, 1 is the output queue and leaves the input
/// alone -- and a swap of 0 and 1 is the dangerous direction, because then
/// the `tcflush(fd, TCIFLUSH)` that every password prompt makes before
/// reading leaves the keystrokes in place.
#[test]
fn only_the_input_selectors_throw_away_what_was_typed() {
    let _guard = SERIAL.lock();
    let sin = vt_stdin(VT);
    for (selector, survives) in [(0usize, false), (1, true), (2, false)] {
        sin.buf.lock().clear();
        sin.buf.lock().extend(b"secret".iter().copied());
        assert_eq!(
            tty_ioctl(VT, TCFLSH as u32, selector),
            Ok(0),
            "selector {selector} was refused"
        );
        assert_eq!(
            !sin.buf.lock().is_empty(),
            survives,
            "selector {selector} left the input queue wrong"
        );
    }
    sin.buf.lock().clear();
}

/// A fourth selector is `EINVAL`, not `Ok(0)`.
///
/// Answering 0 to a selector nobody defines tells a caller that got it wrong
/// -- and it is easy to get wrong, since `tcflush(3)` takes the selector as
/// its own argument while the ioctl takes it by value -- that the queue was
/// thrown away when it was not.
#[test]
fn a_selector_outside_the_three_is_refused_rather_than_answered() {
    let _guard = SERIAL.lock();
    let sin = vt_stdin(VT);
    sin.buf.lock().clear();
    sin.buf.lock().extend(b"kept".iter().copied());
    for selector in [3usize, 4, usize::MAX] {
        assert_eq!(
            tty_ioctl(VT, TCFLSH as u32, selector),
            Err(FsError::InvalidParam),
            "selector {selector} was accepted"
        );
    }
    assert_eq!(sin.buf.lock().len(), 4, "a refused flush still flushed");
    sin.buf.lock().clear();
}
