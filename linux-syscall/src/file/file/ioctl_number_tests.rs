//! The ioctl numbers this layer dispatches on, written out from the headers.
//!
//! `sys_ioctl` serves a handful of requests before the file's own handler
//! ever sees them, and it decides from the request number alone which of
//! them those are. Nothing inside this kernel can check one of those
//! numbers: the comparison and the number compared are the same constant,
//! so they agree whatever it says, and userspace is the only party that
//! disagrees. Measured by mutation over the whole suite, every one of the
//! eleven numbers and bounds in that dispatcher could be changed without a
//! single test noticing.
//!
//! They used to be declared inside the dispatcher's body, which is why: a
//! `const` in a function body is invisible to every test, so it was not
//! that the test was missing, it was that there was no way to write it.
//! They are now taken from `linux_object::fs::ioctl`, the one place this
//! kernel writes them, and what the tests here pin is that place against
//! the header.

use super::*;

/// `_IO(ty, nr)` from `<asm-generic/ioctl.h>`: direction `_IOC_NONE`, no
/// size, so just the magic letter and the command number.
fn io(ty: u8, nr: u8) -> usize {
    ((ty as usize) << 8) | nr as usize
}

/// The six file ioctls of `<asm-generic/ioctls.h>`, which `do_vfs_ioctl`
/// answers for every kind of fd. They sit inside the `0x54xx` terminal
/// block but are not terminal questions.
const FILE_IOCTLS: &[(&str, usize)] = &[
    ("FIONREAD", FIONREAD),
    ("FIONBIO", FIONBIO),
    ("FIONCLEX", FIONCLEX),
    ("FIOCLEX", FIOCLEX),
    ("FIOASYNC", FIOASYNC),
    ("FIOQSIZE", FIOQSIZE),
];

#[test]
fn the_file_ioctls_this_layer_serves_are_the_numbers_asm_generic_ioctls_h_gives() {
    // Hand-written from the header, compared against nothing in the tree.
    assert_eq!(FIONREAD, 0x541B, "bytes waiting to be read");
    assert_eq!(FIONBIO, 0x5421, "set or clear O_NONBLOCK");
    assert_eq!(FIONCLEX, 0x5450, "clear FD_CLOEXEC");
    assert_eq!(FIOCLEX, 0x5451, "set FD_CLOEXEC");
    assert_eq!(FIOASYNC, 0x5452, "set or clear O_ASYNC");
    assert_eq!(FIOQSIZE, 0x5460, "bytes still queued for this file");
    // FIONCLEX and FIOCLEX are adjacent and in that order: swapped, every
    // `fcntl(FD_CLOEXEC)` done through the ioctl spelling does the opposite
    // of what it was asked, and an fd meant to be closed on exec is
    // inherited by every child.
    assert_eq!(FIOCLEX, FIONCLEX + 1);
}

#[test]
fn the_pty_and_window_requests_are_the_numbers_the_uapi_macros_build() {
    // Rebuilt from the magic letter and the command number rather than
    // copied, which is how they read against `<asm-generic/ioctls.h>`.
    assert_eq!(TIOCSCTTY, io(b'T', 0x0E), "adopt a controlling terminal");
    assert_eq!(TIOCGWINSZ, io(b'T', 0x13), "get the window size");
    assert_eq!(TIOCGPTPEER, io(b'T', 0x41), "open a pty master's slave");
    // The magic letter is what makes them terminal requests at all.
    for (name, request) in [
        ("TIOCSCTTY", TIOCSCTTY),
        ("TIOCGWINSZ", TIOCGWINSZ),
        ("TIOCGPTPEER", TIOCGPTPEER),
    ] {
        assert_eq!((request >> 8) & 0xff, b'T' as usize, "{}", name);
    }
}

#[test]
fn no_two_requests_this_layer_dispatches_on_share_a_number() {
    let all: &[(&str, usize)] = &[
        ("FIONREAD", FIONREAD),
        ("FIONBIO", FIONBIO),
        ("FIONCLEX", FIONCLEX),
        ("FIOCLEX", FIOCLEX),
        ("FIOASYNC", FIOASYNC),
        ("FIOQSIZE", FIOQSIZE),
        ("TIOCSCTTY", TIOCSCTTY),
        ("TIOCGWINSZ", TIOCGWINSZ),
        ("TIOCGPTPEER", TIOCGPTPEER),
    ];
    for (i, (an, a)) in all.iter().enumerate() {
        for (bn, b) in &all[i + 1..] {
            assert_ne!(a, b, "{} and {} are the same number", an, bn);
        }
    }
}

#[test]
fn a_file_ioctl_inside_the_terminal_block_keeps_its_own_errno() {
    // These six are answered above the file's handler and are legal on
    // pipes, sockets and regular files, so an EINVAL from one of them is
    // the file's real answer. Turned into ENOTTY, a caller asking an
    // ordinary question about a pipe is told the pipe is not a terminal.
    for (name, request) in FILE_IOCTLS {
        assert!(
            !tty_block_wants_enotty(*request),
            "{} must keep its own errno",
            name
        );
    }
}

#[test]
fn a_terminal_request_inside_the_block_is_normalised_to_enotty() {
    // The whole point of the arm: musl's `isatty()` probes with TIOCGWINSZ
    // and an inode impl that answers EINVAL instead of ENOTTY makes
    // errno-checking callers fail on a plain log file.
    for (name, request) in [
        ("TCGETS", 0x5401usize),
        ("TIOCOUTQ", 0x5411),
        ("TIOCSCTTY", TIOCSCTTY),
        ("TIOCGWINSZ", TIOCGWINSZ),
        ("TIOCGPTPEER", TIOCGPTPEER),
    ] {
        assert!(tty_block_wants_enotty(request), "{}", name);
    }
    // TIOCOUTQ is deliberately in the block: it asks about a terminal's
    // output queue, unlike FIONREAD, which asks about any file.
    assert!(tty_block_wants_enotty(0x5411));
    assert!(!tty_block_wants_enotty(FIONREAD));
}

#[test]
fn nothing_outside_the_terminal_block_is_normalised() {
    // One below the first number and one above the last, plus the two
    // families that share the low byte but not the magic letter.
    for request in [
        0x53FFusize,
        FIOQSIZE + 1,
        0x5461,
        0x4B3A,      // KDSETMODE, the console family
        0x5601,      // VT_GETMODE, the VT family
        0xC004_6440, // a DRM request, which must never be touched here
        0,
    ] {
        assert!(
            !tty_block_wants_enotty(request),
            "{:#x} is outside the block",
            request
        );
    }
    // And the bound is the block's last number, not one past it.
    assert!(tty_block_wants_enotty(FIOQSIZE - 1));
}

#[test]
fn the_number_the_exclusion_list_used_to_name_is_not_fioqsize() {
    // The list this arm is built from used to end in `0x545E` with a
    // comment calling it FIOQSIZE. It is not: `<asm-generic/ioctls.h>`
    // runs TIOCMIWAIT 0x545C, TIOCGICOUNT 0x545D, and then jumps to
    // FIOQSIZE 0x5460, leaving 0x545E and 0x545F assigned to nothing.
    assert_eq!(FIOQSIZE, 0x5460);
    assert_ne!(FIOQSIZE, 0x545E);
    // So 0x545E is an unknown request in the terminal block, and an
    // unknown request on something that is not a terminal is ENOTTY.
    assert!(tty_block_wants_enotty(0x545E));
    assert!(tty_block_wants_enotty(0x545F));
}
