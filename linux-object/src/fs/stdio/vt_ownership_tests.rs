//! Host tests for who owns a VT and what happens when that owner is gone.
//!
//! This is the code between "the compositor died" and "the screen comes
//! back". Nothing here fails loudly: a VT left in `KD_GRAPHICS` with no
//! owner is a black screen, and the machine is still running perfectly
//! well behind it.
//!
//! The KD mode and the per-VT owner are process-wide statics and cargo
//! runs a crate's tests in threads, so every test here takes
//! [`test_lock`] first.

extern crate std;

use super::*;
use kernel_hal::console::{
    kd_mode_vt, set_kd_mode_vt, GRAPHICS_VT, KD_GRAPHICS, KD_TEXT, KD_TEXT0, KD_TEXT1,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Put `vt` in the state a compositor leaves it in: taken with
/// `VT_SETMODE(VT_PROCESS)` by `owner`, and in graphics mode.
fn a_compositor_owns(vt: usize, owner: u64) {
    *tty_vt_mode(vt).lock() = VtMode {
        mode: VT_PROCESS,
        waitv: 0,
        relsig: 10,
        acqsig: 12,
        frsig: 0,
    };
    TTY_STATES[vt_clamp(vt)]
        .vt_owner
        .store(owner, Ordering::Relaxed);
    set_kd_mode_vt(vt, KD_GRAPHICS);
}

fn release(vt: usize) {
    *tty_vt_mode(vt).lock() = VtMode::auto();
    TTY_STATES[vt_clamp(vt)]
        .vt_owner
        .store(0, Ordering::Relaxed);
    set_kd_mode_vt(vt, KD_TEXT);
}

// ---- the one that matters ---------------------------------------------

#[test]
fn giving_a_vt_back_returns_it_to_text() {
    // Linux's `reset_vc()` does this, and the comment at its call site
    // says why: "it saves the agony when the X server dies and the screen
    // remains blanked due to KD_GRAPHICS". Reverting the handshake and
    // leaving the VT in graphics mode is a black screen on a machine that
    // is otherwise fine.
    let _g = test_lock();
    a_compositor_owns(GRAPHICS_VT, 4321);
    assert_eq!(kd_mode_vt(GRAPHICS_VT), KD_GRAPHICS, "arrange failed");

    reset_vc(GRAPHICS_VT);

    assert_eq!(
        kd_mode_vt(GRAPHICS_VT),
        KD_TEXT,
        "the VT stayed in graphics mode with nobody drawing on it"
    );
    release(GRAPHICS_VT);
}

#[test]
fn giving_a_vt_back_drops_the_handshake_and_the_owner() {
    let _g = test_lock();
    a_compositor_owns(GRAPHICS_VT, 4321);

    reset_vc(GRAPHICS_VT);

    assert_eq!(tty_vt_mode(GRAPHICS_VT).lock().mode, VT_AUTO);
    assert_eq!(vt_owner(GRAPHICS_VT), 0);
    release(GRAPHICS_VT);
}

#[test]
fn giving_back_a_vt_that_does_not_exist_lands_on_the_last_one() {
    // `reset_vc` is reached from a switch request, whose VT number comes
    // from userspace. Clamping rather than indexing is what keeps that
    // from being a panic.
    let _g = test_lock();
    reset_vc(usize::MAX);
    assert_eq!(vt_owner(kernel_hal::console::NUM_VTS - 1), 0);
}

// ---- a corpse is not a seat -------------------------------------------

#[test]
fn a_dead_owner_does_not_count_as_a_graphics_session() {
    // The only consumer is the `DROP_MASTER` console restore, which stands
    // down when a seat owns the VT because the seat will restore text
    // itself. When the compositor is what died, the seat IS the corpse:
    // the restore that exists for exactly this case was suppressed by it.
    // No process has this KoID, so the owner is not there.
    let _g = test_lock();
    TTY_STATES[GRAPHICS_VT]
        .vt_owner
        .store(0xdead_beef, Ordering::Relaxed);

    assert!(
        !graphics_vt_seat_owned(),
        "a KoID that names no process was taken for a live seat"
    );
    release(GRAPHICS_VT);
}

#[test]
fn an_unowned_graphics_vt_is_not_a_graphics_session() {
    let _g = test_lock();
    TTY_STATES[GRAPHICS_VT].vt_owner.store(0, Ordering::Relaxed);
    assert!(!graphics_vt_seat_owned());
}

#[test]
fn a_process_that_was_never_there_does_not_exist() {
    // What `vt_owner_alive` leans on. If this ever started answering true
    // for an arbitrary number, every owner would look alive for ever.
    assert!(!crate::process::process_exists(0xdead_beef));
    assert!(!crate::process::process_exists(0));
}

// ---- the signal the owner asked to be told with ------------------------

#[test]
fn a_relsig_of_zero_names_no_signal() {
    // `vt_mode.relsig` is an `i16` straight from userspace, and 0 is the
    // "do not signal me" of a `struct vt_mode` left zeroed. An owner that
    // cannot be signalled is as unreachable as a dead one, which is why
    // the switch path now gives the VT back instead of skipping the check.
    assert!(vt_signal(0).is_none());
}

#[test]
fn a_signal_number_out_of_range_names_no_signal() {
    for n in [-1i16, -32, 65, 100, i16::MIN, i16::MAX] {
        assert!(vt_signal(n).is_none(), "{} is not a signal", n);
    }
}

#[test]
fn a_signal_number_that_wraps_into_range_is_refused() {
    // This is what the range check is actually for. `vt_signal` narrows
    // with `n as u8`, which TRUNCATES: 257 would come out as SIGHUP and
    // 265 as SIGKILL, and `Signal::try_from` cannot tell the difference
    // because it only ever sees the low byte.
    //
    // It is also why moving either end of the range changes nothing on
    // its own -- `try_from` already refuses 0 and 65..=255. The numbers
    // that need the guard are the ones above a byte.
    for n in [256i16, 257, 265, 320, 521] {
        assert!(
            vt_signal(n).is_none(),
            "relsig {} wrapped into signal {:?}",
            n,
            vt_signal(n)
        );
    }
}

#[test]
fn the_signals_a_compositor_actually_asks_for_resolve() {
    // wlroots/seatd use SIGUSR1/SIGUSR2 (10 and 12) for relsig/acqsig.
    assert_eq!(vt_signal(10), Some(crate::signal::Signal::SIGUSR1));
    assert_eq!(vt_signal(12), Some(crate::signal::Signal::SIGUSR2));
}

#[test]
fn every_signal_number_resolves_or_is_refused() {
    // No number from userspace may panic on the way in.
    for n in i16::MIN..=i16::MAX {
        let got = vt_signal(n);
        assert_eq!(got.is_some(), (1..=64).contains(&n), "relsig {}", n);
    }
}

// ---- VT_SETMODE takes two modes, not any byte --------------------------

#[test]
fn vt_setmode_takes_auto_and_process() {
    assert!(vt_mode_accepted(VT_AUTO));
    assert!(vt_mode_accepted(VT_PROCESS));
}

#[test]
fn vt_setmode_refuses_a_mode_it_does_not_know() {
    // Linux answers EINVAL. Storing it meant every later
    // `mode == VT_PROCESS` read false, so the VT looked unowned to the
    // switch handshake while the caller believed it had taken it.
    for mode in [2u8, 3, 0x10, 0xff] {
        assert!(!vt_mode_accepted(mode), "mode {:#x} was accepted", mode);
    }
}

#[test]
fn the_two_vt_modes_are_the_numbers_linux_uses() {
    // Against the literals, not against themselves: these are the ABI's.
    assert_eq!(VT_AUTO, 0);
    assert_eq!(VT_PROCESS, 1);
}

// ---- the VT number from userspace --------------------------------------

#[test]
fn a_vt_number_past_the_last_one_is_clamped() {
    assert_eq!(vt_clamp(usize::MAX), kernel_hal::console::NUM_VTS - 1);
    assert_eq!(
        vt_clamp(kernel_hal::console::NUM_VTS),
        kernel_hal::console::NUM_VTS - 1
    );
}

#[test]
fn a_real_vt_number_is_left_alone() {
    for vt in 0..kernel_hal::console::NUM_VTS {
        assert_eq!(vt_clamp(vt), vt);
    }
}

#[test]
fn the_graphics_vt_is_the_last_one() {
    // `GRAPHICS_VT` is the reserved one with no login shell, and several
    // decisions here compare against it. If it stopped being the last VT
    // the reservation in `zCore/src/main.rs` would move without them.
    assert_eq!(GRAPHICS_VT, kernel_hal::console::NUM_VTS - 1);
}

// ---- KDSETMODE / KDGETMODE -------------------------------------------

/// `KDSETMODE` handed its argument straight to the console with no
/// validation. Linux's `vt_ioctl.c` accepts `KD_TEXT`, its two obsolete
/// aliases and `KD_GRAPHICS`, and answers everything else with `-EINVAL`;
/// here anything at all was accepted and stored, and since every decision
/// downstream compares against `KD_TEXT`, a VT in a value that is neither
/// constant stops being drawn AND matches no arm of `set_kd_mode_vt`, so
/// nothing repaints it either. A blank screen, reported as success.
///
/// The two halves of the fix are deliberately redundant: this arm is what
/// REPORTS the refusal, and `set_kd_mode_vt` refuses the store on its own,
/// so no caller can put a VT into a mode that is not one of the two. A
/// mutation that stores the bad mode here anyway is therefore a no-op, and
/// that is the point -- the check is not load-bearing for the console's
/// state, only for the errno.
#[test]
fn kdsetmode_refuses_a_mode_that_is_not_a_mode() {
    let _g = test_lock();
    set_kd_mode_vt(0, KD_TEXT);

    assert!(
        tty_ioctl(0, KDSETMODE as u32, 0x2a).is_err(),
        "an unknown mode has to be EINVAL, not a silently blanked console"
    );
    assert_eq!(kd_mode_vt(0), KD_TEXT, "and the VT must be left as it was");

    // The two real modes still go through.
    assert!(tty_ioctl(0, KDSETMODE as u32, KD_GRAPHICS as usize).is_ok());
    assert_eq!(kd_mode_vt(0), KD_GRAPHICS);
    assert!(tty_ioctl(0, KDSETMODE as u32, KD_TEXT as usize).is_ok());
    assert_eq!(kd_mode_vt(0), KD_TEXT);
}

/// The obsolete text aliases are a request for text mode, and
/// `KD_GETMODE` has to answer with the canonical value Linux stores
/// (`vc_mode = KD_TEXT`), not with the alias that was sent -- a caller
/// comparing the reply against `KD_TEXT` would otherwise conclude the
/// console is in graphics mode.
#[test]
fn kdsetmode_folds_the_obsolete_text_aliases() {
    let _g = test_lock();

    for alias in [KD_TEXT0, KD_TEXT1] {
        set_kd_mode_vt(0, KD_GRAPHICS);
        assert!(tty_ioctl(0, KDSETMODE as u32, alias as usize).is_ok());
        assert_eq!(kd_mode_vt(0), KD_TEXT, "alias {:#x}", alias);

        let mut out: i32 = -1;
        assert!(tty_ioctl(0, KDGETMODE as u32, &mut out as *mut i32 as usize).is_ok());
        assert_eq!(out, KD_TEXT as i32, "KD_GETMODE after alias {:#x}", alias);
    }

    set_kd_mode_vt(0, KD_TEXT);
}

// ---- KDSKBMODE / KDGKBMODE -------------------------------------------

/// `vt_do_kdskbmode` accepts five modes and refuses the rest with EINVAL.
/// Storing anything else used to switch the VT's line discipline off:
/// "cooked" is decided by comparing the stored value against `K_XLATE`
/// and `K_UNICODE`, so a number that is neither stopped every key press
/// from becoming a character. A dead keyboard, reported as success.
#[test]
fn kdskbmode_refuses_a_mode_that_is_not_a_mode() {
    let _g = test_lock();
    TTY_STATES[0].kbd_mode.store(K_XLATE, Ordering::Relaxed);

    for bogus in [5usize, 0x2a, usize::MAX] {
        assert!(
            tty_ioctl(0, KDSKBMODE as u32, bogus).is_err(),
            "mode {:#x} has to be EINVAL",
            bogus
        );
        assert_eq!(
            tty_kbd_mode(0),
            K_XLATE,
            "and the VT must be left as it was"
        );
        assert!(tty_kbd_cooked(0), "the keyboard keeps cooking");
    }
}

/// The five modes Linux knows all go through, and `KDGKBMODE` reads back
/// exactly what was set.
#[test]
fn kdskbmode_takes_the_five_modes_linux_knows() {
    let _g = test_lock();
    for mode in [K_RAW, K_MEDIUMRAW, K_XLATE, K_UNICODE, K_OFF] {
        assert!(
            tty_ioctl(0, KDSKBMODE as u32, mode as usize).is_ok(),
            "{:#x}",
            mode
        );
        let mut out: i32 = -1;
        assert!(tty_ioctl(0, KDGKBMODE as u32, &mut out as *mut i32 as usize).is_ok());
        assert_eq!(out, mode);
    }
    // Only the two translating modes cook; the X-server modes do not.
    TTY_STATES[0].kbd_mode.store(K_RAW, Ordering::Relaxed);
    assert!(!tty_kbd_cooked(0));
    TTY_STATES[0].kbd_mode.store(K_XLATE, Ordering::Relaxed);
    assert!(tty_kbd_cooked(0));
}
