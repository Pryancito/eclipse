//! The `tty_ioctl` requests a getty, `stty`, a shell's job control and an
//! X server send to a VT, each answered the way Linux's `tty_ioctl.c` and
//! `vt_ioctl.c` answer them: what is set is read back, what is fixed on a
//! console reads as Linux's console, and a pointer that is not the
//! caller's is `EFAULT`.
//!
//! The expected numbers are written out from the headers.

use super::*;
use lock::Mutex as TestMutex;

/// A VT of this module's own: 3, 4, 5 and 6 belong to other tests.
const VT: usize = 2;

static SERIAL: TestMutex<()> = TestMutex::new(());

fn out<T: Default>(cmd: usize) -> T {
    let mut v = T::default();
    assert_eq!(
        tty_ioctl(VT, cmd as u32, &mut v as *mut T as usize),
        Ok(0),
        "{:#x}",
        cmd
    );
    v
}

/// `tcgetattr`, as the fields `==` can compare.
fn termios() -> (u32, u32, u32, u32, u8, [u8; 19]) {
    key(&tcgetattr())
}

fn tcgetattr() -> Termios {
    let mut t = Termios::default_tty();
    t.c_lflag = 0xdead;
    assert_eq!(set(TCGETS, &mut t), Ok(0));
    t
}

fn key(t: &Termios) -> (u32, u32, u32, u32, u8, [u8; 19]) {
    (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_line, t.c_cc)
}

fn set<T>(cmd: usize, v: &T) -> Result<usize> {
    tty_ioctl(VT, cmd as u32, v as *const T as usize)
}

/// `tcsetattr` then `tcgetattr` gives back what was set, through both the
/// old and the `termios2` layout, and `TCSETSF` also drops what was typed.
#[test]
fn what_tcsetattr_sets_tcgetattr_reads_back() {
    let _alone = SERIAL.lock();
    let saved = tcgetattr();
    // `stty raw -echo`: no ICANON, ISIG or ECHO, one byte at a time.
    let mut raw = saved;
    raw.c_lflag &= !(0o2 | 0o1 | 0o10); // ICANON, ISIG, ECHO
    raw.c_cc[6] = 1; // VMIN
    raw.c_cc[5] = 0; // VTIME
    assert_eq!(set(TCSETS, &raw), Ok(0));
    assert_eq!(termios(), key(&raw));
    // TCSETSW is TCSETS after the output drains: the same here.
    assert_eq!(set(TCSETSW, &saved), Ok(0));
    assert_eq!(termios(), key(&saved));

    // termios2 carries the same flags, and the speed of B38400.
    let t2: Termios2 = out(TCGETS2);
    assert_eq!(t2.c_lflag, saved.c_lflag);
    assert_eq!(t2.c_cflag & 0o10017, 0o17, "CBAUD is B38400");
    assert_eq!((t2.c_ispeed, t2.c_ospeed), (38400, 38400));
    let mut raw2 = t2;
    raw2.c_lflag = raw.c_lflag;
    assert_eq!(set(TCSETS2, &raw2), Ok(0));
    assert_eq!(tcgetattr().c_lflag, raw.c_lflag);
    assert_eq!(set(TCSETSW2, &t2), Ok(0));
    assert_eq!(termios(), key(&saved));

    // TCSETSF: "flush pending input" (tcsetattr(TCSAFLUSH)), which is what
    // a password prompt does so that nothing typed before it is read as
    // the password.
    for (cmd, how) in [(TCSETSF, 0usize), (TCSETSF2, 1)] {
        vt_stdin(VT).push_bytes(b"typed ahead");
        assert_eq!(out::<i32>(FIONREAD), 11, "FIONREAD before {}", how);
        let r = if how == 0 {
            set(cmd, &saved)
        } else {
            set(cmd, &t2)
        };
        assert_eq!(r, Ok(0));
        assert_eq!(out::<i32>(FIONREAD), 0, "FIONREAD after {}", how);
        assert_eq!(termios(), key(&saved));
    }
}

/// `tcsetpgrp` then `tcgetpgrp`; `tcgetsid` gives the foreground group
/// here, the one this kernel tracks.
#[test]
fn the_foreground_group_set_is_the_one_read() {
    let _alone = SERIAL.lock();
    let saved = tty_fg_pgrp(VT).load(Ordering::Relaxed);
    assert_eq!(set(TIOCSPGRP, &4242i32), Ok(0));
    assert_eq!(out::<i32>(TIOCGPGRP), 4242);
    assert_eq!(out::<i32>(TIOCGSID), 4242);
    assert_eq!(vt_foreground_pgrp(VT), 4242);
    set_vt_foreground_pgrp(VT, 4343);
    assert_eq!(out::<i32>(TIOCGPGRP), 4343);
    tty_fg_pgrp(VT).store(saved, Ordering::Relaxed);
}

/// What a console says of its keyboard: a PC 101-key one (`KB_101` is 2),
/// in `K_XLATE` (1) until told otherwise, each mode `kbd.h` defines taken
/// and anything else `EINVAL`; the LEDs set are the LEDs read.
#[test]
fn the_keyboard_is_a_pc_keyboard_in_the_mode_it_was_given() {
    let _alone = SERIAL.lock();
    assert_eq!(out::<u8>(KDGKBTYPE), 2, "KB_101");
    assert_eq!(out::<i32>(KDGKBMODE), 1, "K_XLATE");
    // K_RAW, K_XLATE, K_MEDIUMRAW, K_UNICODE, K_OFF.
    for mode in [0usize, 2, 3, 4, 1] {
        assert_eq!(tty_ioctl(VT, KDSKBMODE as u32, mode), Ok(0), "{}", mode);
        assert_eq!(out::<i32>(KDGKBMODE), mode as i32);
    }
    assert_eq!(
        tty_ioctl(VT, KDSKBMODE as u32, 5),
        Err(FsError::InvalidParam)
    );
    assert_eq!(out::<i32>(KDGKBMODE), 1, "a refused mode changes nothing");

    // LED_SCR 1, LED_NUM 2, LED_CAP 4.
    assert_eq!(tty_ioctl(VT, KDSETLED as u32, 0b110), Ok(0));
    assert_eq!(out::<i32>(KDGETLED), 0b110);
    assert_eq!(tty_ioctl(VT, KDSETLED as u32, 0), Ok(0));
    assert_eq!(out::<i32>(KDGETLED), 0);
    assert_eq!(tty_ioctl(VT, KDMKTONE as u32, 0x1234), Ok(0));
}

/// `KDGKBENT` fills `kb_value` from the table and index the caller put in
/// `struct kbentry`; a null one is refused.
#[test]
fn kdgkbent_answers_in_the_entry_it_was_given() {
    let _alone = SERIAL.lock();
    // kb_table, kb_index, kb_value. Keycode 1 is Escape, in the plain
    // table: KT_LATIN, 0x1b.
    let mut entry = [0u8, 1, 0xff, 0xff];
    assert_eq!(
        tty_ioctl(VT, KDGKBENT as u32, entry.as_mut_ptr() as usize),
        Ok(0)
    );
    assert_eq!(u16::from_ne_bytes([entry[2], entry[3]]), 0x001b);
    assert_eq!(&entry[..2], [0, 1], "the caller's half is left alone");
    assert_eq!(
        tty_ioctl(VT, KDGKBENT as u32, 0),
        Err(FsError::InvalidParam)
    );
}

/// What the VT layer says of itself: the VT the caller is on is the one
/// `VT_OPENQRY` hands out, `VT_GETSTATE` counts VTs from 1 with bit 0
/// unused, and a mode set with `VT_SETMODE` is the one `VT_GETMODE` reads.
#[test]
fn the_vt_layer_answers_in_vt_numbers_from_one() {
    let _alone = SERIAL.lock();
    let active = kernel_hal::console::active_vt() as i32;
    assert_eq!(out::<i32>(VT_OPENQRY), active + 1);
    let st: VtStat = {
        let mut s = VtStat {
            v_active: 0,
            v_signal: 0xffff,
            v_state: 0,
        };
        assert_eq!(
            tty_ioctl(VT, VT_GETSTATE as u32, &mut s as *mut _ as usize),
            Ok(0)
        );
        s
    };
    assert_eq!(st.v_active as i32, active + 1);
    assert_eq!(st.v_signal, 0);
    assert_eq!(st.v_state & 1, 0, "bit 0 is no VT");
    assert_ne!(st.v_state & (1 << (VT + 1)), 0, "this VT is in use");

    let mode = VtMode {
        mode: VT_AUTO,
        waitv: 1,
        relsig: 0,
        acqsig: 0,
        frsig: 0,
    };
    assert_eq!(set(VT_SETMODE, &mode), Ok(0));
    let mut back = VtMode::auto();
    back.waitv = 0;
    assert_eq!(
        tty_ioctl(VT, VT_GETMODE as u32, &mut back as *mut _ as usize),
        Ok(0)
    );
    assert_eq!((back.mode, back.waitv), (VT_AUTO, 1));
    assert_eq!(vt_owner(VT), 0, "VT_AUTO has no owner");
    assert_eq!(
        tty_ioctl(VT, VT_SETMODE as u32, 0),
        Err(FsError::InvalidParam)
    );
    *tty_vt_mode(VT).lock() = VtMode::auto();

    // Nothing to wait for, nothing pending to release, and VTs are never
    // freed: all three are accepted.
    assert_eq!(tty_ioctl(VT, VT_WAITACTIVE as u32, 1), Ok(0));
    assert_eq!(tty_ioctl(VT, VT_DISALLOCATE as u32, 0), Ok(0));
    assert_eq!(tty_ioctl(VT, TIOCSCTTY as u32, 0), Ok(0));
    assert_eq!(tty_ioctl(VT, TIOCNOTTY as u32, 0), Ok(0));
}

/// A VT has no serial line behind it: Linux's console reports DTR, RTS,
/// CTS, carrier and DSR up, nothing queued for output and every line
/// counter at zero, and takes the line setters without complaint.
#[test]
fn a_vt_is_a_serial_line_that_is_always_up() {
    let _alone = SERIAL.lock();
    // TIOCM_DTR 0x002, TIOCM_RTS 0x004, TIOCM_CTS 0x020, TIOCM_CAR 0x040,
    // TIOCM_DSR 0x100.
    assert_eq!(out::<i32>(TIOCMGET), 0x002 | 0x004 | 0x020 | 0x040 | 0x100);
    for cmd in [TIOCMSET, TIOCMBIS, TIOCMBIC] {
        assert_eq!(set(cmd, &0i32), Ok(0));
    }
    assert_eq!(out::<i32>(TIOCOUTQ), 0);
    let mut counts = SerialIcounter {
        rx: 7,
        ..Default::default()
    };
    assert_eq!(
        tty_ioctl(VT, TIOCGICOUNT as u32, &mut counts as *mut _ as usize),
        Ok(0)
    );
    assert_eq!(counts.rx, 0);
    assert_eq!(counts.tx, 0);
    for cmd in [TCSBRK, TCSBRKP, TIOCSBRK, TIOCCBRK] {
        assert_eq!(tty_ioctl(VT, cmd as u32, 0), Ok(0), "{:#x}", cmd);
    }
}

/// `TIOCLINUX` answers `TIOCL_GETSHIFTSTATE` (6) in the byte it was given,
/// with no modifier held, refuses another subcommand with `EINVAL` like
/// Linux, and a null argument.
#[test]
fn tioclinux_answers_the_shift_state_in_place() {
    let _alone = SERIAL.lock();
    let mut arg = [6u8, 0xaa];
    assert_eq!(
        tty_ioctl(VT, TIOCLINUX as u32, arg.as_mut_ptr() as usize),
        Ok(0)
    );
    if !(SHIFT_DOWN.load(Ordering::SeqCst)
        || CTRL_DOWN.load(Ordering::SeqCst)
        || ALTGR_DOWN.load(Ordering::SeqCst)
        || LEFT_ALT_DOWN.load(Ordering::SeqCst))
    {
        assert_eq!(arg[0], 0);
    }
    assert_eq!(arg[1], 0xaa, "one byte answered");
    let mut other = [3u8];
    assert_eq!(
        tty_ioctl(VT, TIOCLINUX as u32, other.as_mut_ptr() as usize),
        Err(FsError::InvalidParam)
    );
    assert_eq!(
        tty_ioctl(VT, TIOCLINUX as u32, 0),
        Err(FsError::InvalidParam)
    );
}

/// A request no terminal knows is `ENOTTY` (`NotSupported` here).
#[test]
fn a_request_that_is_not_a_terminal_one_is_enotty() {
    assert_eq!(tty_ioctl(VT, 0x1234_5678, 0), Err(FsError::NotSupported));
}

/// The window size set with `TIOCSWINSZ` is the one `TIOCGWINSZ` reads,
/// and `0x0` gives back the console's own.
#[test]
fn the_window_size_set_is_the_one_read_until_cleared() {
    let _alone = SERIAL.lock();
    let own: ConsoleWinSize = out(TIOCGWINSZ);
    let small = ConsoleWinSize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(set(TIOCSWINSZ, &small), Ok(0));
    let back: ConsoleWinSize = out(TIOCGWINSZ);
    assert_eq!((back.ws_row, back.ws_col), (24, 80));
    assert_eq!(set(TIOCSWINSZ, &ConsoleWinSize::default()), Ok(0));
    let again: ConsoleWinSize = out(TIOCGWINSZ);
    assert_eq!((again.ws_row, again.ws_col), (own.ws_row, own.ws_col));
}

/// A pointer outside the caller's half is `EFAULT` for every request that
/// reads or writes one.
#[test]
fn a_pointer_that_is_not_the_callers_is_efault() {
    let _alone = SERIAL.lock();
    struct Strict;
    impl Drop for Strict {
        fn drop(&mut self) {
            kernel_hal::user::set_strict_user_half(false);
        }
    }
    kernel_hal::user::set_strict_user_half(true);
    let _strict = Strict;
    const KERNEL: usize = 0xffff_8000_0000_0000;
    for cmd in [
        TCGETS,
        TCGETS2,
        TCSETS,
        TCSETS2,
        TCSETSF,
        TCSETSF2,
        TIOCGPGRP,
        TIOCSPGRP,
        TIOCGWINSZ,
        TIOCSWINSZ,
        KDGKBTYPE,
        KDGKBMODE,
        KDGETLED,
        KDGKBENT,
        VT_OPENQRY,
        VT_GETMODE,
        VT_SETMODE,
        VT_GETSTATE,
        FIONREAD,
        TIOCOUTQ,
        TIOCGSID,
        TIOCMGET,
        TIOCGICOUNT,
        TIOCLINUX,
    ] {
        assert_eq!(
            tty_ioctl(VT, cmd as u32, KERNEL),
            Err(FsError::BadAddress),
            "{:#x}",
            cmd
        );
    }
}
