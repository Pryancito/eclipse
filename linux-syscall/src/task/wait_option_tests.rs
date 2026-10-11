//! `wait4` and `waitid` read the same options word and Linux holds them
//! to two different masks. `waitid` checked its own, but inline, where no
//! test could reach it; `wait4` ran the word through
//! `WaitFlags::from_bits_truncate` and kept whatever was left.

use super::*;

const WNOHANG: u32 = 0x0000_0001;
const WUNTRACED: u32 = 0x0000_0002;
const WEXITED: u32 = 0x0000_0004;
const WCONTINUED: u32 = 0x0000_0008;
const WNOWAIT: u32 = 0x0100_0000;
const WNOTHREAD: u32 = 0x2000_0000;
const WALL: u32 = 0x4000_0000;
const WCLONE: u32 = 0x8000_0000;

/// `__WALL` is 0x40000000 and `__WCLONE` is 0x80000000. They were the
/// other way round in `waitid`'s own `bitflags`. Neither changes what
/// this kernel does yet, which is exactly why a swap sat there unnoticed
/// and would have been believed by the first caller to act on it.
#[test]
fn the_two_underscore_flags_are_the_bits_linux_gives_them() {
    assert_eq!(wait_opts::WALL, 0x4000_0000);
    assert_eq!(wait_opts::WCLONE, 0x8000_0000);
    assert_eq!(wait_opts::WNOTHREAD, 0x2000_0000);
    // `WSTOPPED` is `WUNTRACED` under another name, which is why one
    // field carries both syscalls' meaning.
    assert_eq!(wait_opts::WUNTRACED, 0x0000_0002);
    // And the two masks, written out, so a bit cannot move between them
    // without this failing.
    assert_eq!(wait_opts::WAIT4, 0xE000_000B);
    assert_eq!(wait_opts::WAITID, 0xE100_000F);
    assert_eq!(wait_opts::WAITID_REQUIRED, 0x0000_000E);
}

/// The interest is `(exited, stopped, continued)` **in that order**, and
/// the last two are separate bits asking for separate things.
///
/// Swapping the two in `wait4_options` passed green: the tests that ask
/// about them pass both at once, or neither, so the two fields were never
/// told apart. A `wait4(-1, &st, WUNTRACED)` that reports a child resumed
/// by `SIGCONT` instead of one stopped by `SIGTSTP` is a shell that
/// prints "Continued" when you press Ctrl-Z.
#[test]
fn stopped_and_continued_are_asked_for_one_at_a_time() {
    assert_eq!(
        wait4_options(WUNTRACED).unwrap().interest,
        (true, true, false),
        "WUNTRACED asked for the wrong state change"
    );
    assert_eq!(
        wait4_options(WCONTINUED).unwrap().interest,
        (true, false, true),
        "WCONTINUED asked for the wrong state change"
    );
    // And `waitid`, which reads the same two bits into the same two
    // places, with its own `exited` rather than an implied one.
    assert_eq!(
        waitid_options(WUNTRACED).unwrap().interest,
        (false, true, false)
    );
    assert_eq!(
        waitid_options(WCONTINUED).unwrap().interest,
        (false, false, true)
    );
}

/// `waitid(2)`'s `idtype_t`, from `include/uapi/linux/wait.h`.
///
/// `P_PIDFD` was **wrongly 5** here, so `waitid(P_PIDFD, fd, ...)` --
/// which is what glib's `g_child_watch_source_new()` does -- matched no
/// arm of the dispatch and came back `EINVAL`. The fix had no test, and
/// putting the 5 back passed green: the four numbers are read inside an
/// `async` method on `Syscall`, which no unit test can build, so the only
/// thing that can guard them is pinning them here.
#[test]
fn the_four_waitid_id_types_are_the_numbers_linux_gives_them() {
    assert_eq!(P_ALL, 0);
    assert_eq!(P_PID, 1);
    assert_eq!(P_PGID, 2);
    assert_eq!(P_PIDFD, 3, "P_PIDFD is back to the wrong number");
    // And no two of them are the same, which is what a dispatch on them
    // needs and what a copy-paste would break.
    let all = [P_ALL, P_PID, P_PGID, P_PIDFD];
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            assert_ne!(a, b, "two id types share a number");
        }
    }
}

/// A non-blocking pidfd without `WNOHANG` turns an empty wait into
/// `EAGAIN`; with `WNOHANG` it stays the usual empty success.
#[test]
fn a_nonblocking_pidfd_empty_wait_is_eagain_unless_wnohang() {
    assert_eq!(pidfd_waitid_empty(true), Ok(()));
    assert_eq!(pidfd_waitid_empty(false), Err(LxError::EAGAIN));
}

/// `WEXITED` and `WNOWAIT` belong to `waitid`. `kernel_wait4`'s mask
/// leaves both out, so naming either is EINVAL there.
#[test]
fn wait4_refuses_the_two_bits_that_are_waitids() {
    assert_eq!(wait4_options(WEXITED), Err(LxError::EINVAL));
    assert_eq!(wait4_options(WNOWAIT), Err(LxError::EINVAL));
    assert_eq!(wait4_options(WNOHANG | WNOWAIT), Err(LxError::EINVAL));
    // And they are fine on waitid, which is the point of the two masks.
    assert!(waitid_options(WEXITED).is_ok());
    assert!(waitid_options(WEXITED | WNOWAIT).is_ok());
}

/// `wait4(pid, &st, WNOWAIT, NULL)` used to leave the zombie in place --
/// an extension of this kernel's own, since the call is EINVAL in Linux.
/// A `wait4` always reaps.
#[test]
fn a_wait4_always_reaps_and_always_reports_an_exit() {
    let o = wait4_options(0).unwrap();
    assert!(o.reap);
    assert_eq!(o.interest, (true, false, false));
    assert!(!o.nohang);
    let o = wait4_options(WNOHANG | WUNTRACED | WCONTINUED).unwrap();
    assert!(o.nohang);
    assert!(o.reap);
    assert_eq!(o.interest, (true, true, true));
}

#[test]
fn wait4_takes_every_bit_on_its_own_mask_and_nothing_else() {
    assert!(wait4_options(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE).is_ok());
    for stray in [0x10u32, 0x20, 0x1_0000, 0x1000_0000] {
        assert_eq!(wait4_options(stray), Err(LxError::EINVAL), "{:#x}", stray);
    }
}

/// `waitid` has no default interest, so a word naming none of the three
/// is a wait for nothing. Linux refuses it rather than block forever.
#[test]
fn a_waitid_that_asks_for_nothing_is_refused() {
    assert_eq!(waitid_options(0), Err(LxError::EINVAL));
    assert_eq!(waitid_options(WNOHANG), Err(LxError::EINVAL));
    assert_eq!(waitid_options(WNOWAIT | WALL), Err(LxError::EINVAL));
    for one in [WEXITED, WUNTRACED, WCONTINUED] {
        assert!(waitid_options(one).is_ok(), "{:#x}", one);
    }
}

#[test]
fn waitid_reports_exactly_what_it_was_asked_for() {
    let o = waitid_options(WEXITED).unwrap();
    assert_eq!(o.interest, (true, false, false));
    assert!(o.reap);
    let o = waitid_options(WUNTRACED | WNOWAIT | WNOHANG).unwrap();
    assert_eq!(o.interest, (false, true, false));
    assert!(!o.reap);
    assert!(o.nohang);
    let o = waitid_options(WEXITED | WUNTRACED | WCONTINUED).unwrap();
    assert_eq!(o.interest, (true, true, true));
}

#[test]
fn waitid_refuses_a_bit_outside_its_mask_too() {
    for stray in [0x10u32, 0x0200_0000, 0x1000_0000] {
        assert_eq!(
            waitid_options(stray | WEXITED),
            Err(LxError::EINVAL),
            "{:#x}",
            stray
        );
    }
}
