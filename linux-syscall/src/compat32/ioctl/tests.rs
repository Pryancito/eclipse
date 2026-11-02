//! What this file decides is which requests a 32-bit process may make at all,
//! so what is worth pinning is the **shape of the list** and not its entries.
//!
//! A test that restated the forty numbers would pass whatever the table said.
//! These ask instead for the properties that made each entry legal: that no
//! vouched number carries a size that an i386 `sizeof` would measure
//! differently, that every number this kernel's terminal layer answers is
//! either in the table or deliberately out of it, and that the three requests
//! the measurements proved dangerous are **not** in it.

use super::*;

/// `_IOC`'s size field, bits 16..30 --- the `sizeof` the CALLER's compiler
/// measured. A request whose size field holds the size of something that
/// changes width between the ABIs arrives as a different number from a
/// 32-bit program, which is the second form in the module documentation.
fn ioc_size(request: u32) -> u32 {
    (request >> 16) & 0x3fff
}

/// Every vouched request either encodes no size at all (a legacy `_IO`
/// number, which is what the whole `0x54xx` family is) or encodes a size that
/// is the same number in both ABIs.
///
/// The sizes that are allowed to appear are listed, rather than merely
/// bounded, because the danger is not a big size --- it is a size measured on
/// a type whose width changes. 4 is an `int` or a `u32`, 44 is `termios2`;
/// both are the same in both ABIs. A 4 that came from a `long` or a pointer
/// would be the bug, and it cannot appear here because nothing in the table
/// passes one.
#[test]
fn no_vouched_request_carries_a_size_that_the_two_abis_measure_differently() {
    for &(request, name) in SAME {
        let size = ioc_size(request);
        assert!(
            matches!(size, 0 | 4 | 44),
            "{} ({:#x}) encodes a size of {}, which is not one of the widths \
             this table has checked",
            name,
            request,
            size
        );
    }
}

/// The three requests the measurements singled out are **not** vouched, and
/// each for its own reason:
///
/// - `BLKGETSIZE64` as x86_64 spells it (`0x80081272`) is a number a 32-bit
///   program never sends, and the i386 spelling (`0x80041272`) is a number no
///   native handler compares against.
/// - `BLKGETSIZE` (`0x1260`) is the same number in both ABIs and writes a
///   `long`, so passing it through would have the kernel write eight bytes
///   where the caller gave four.
/// - `SIOCGIFCONF` (`0x8912`) passes a structure that holds a pointer.
///
/// Pinned by number and not by name because the point is that the dispatch
/// refuses them, and the dispatch only sees numbers.
#[test]
fn the_three_requests_the_measurements_refused_are_not_vouched() {
    for &(request, why) in &[
        (0x80081272u32, "BLKGETSIZE64 as x86_64 spells it"),
        (0x80041272, "BLKGETSIZE64 as i386 spells it"),
        (0x1260, "BLKGETSIZE, whose payload is a long"),
        (0x8912, "SIOCGIFCONF, whose ifconf holds a pointer"),
        (0x8915, "SIOCGIFADDR, whose ifreq is 32 bytes and not 40"),
        (0x5460, "FIOQSIZE, whose loff_t may be 4-aligned"),
    ] {
        assert_eq!(
            vouched(request),
            None,
            "{:#x} ({}) must not be vouched",
            request,
            why
        );
    }
}

/// The table has no duplicate number and no empty name: a second entry for a
/// number would be dead weight that the linear lookup never reaches, and a
/// nameless one would make the log line useless, which is the only reason the
/// names are carried at all.
#[test]
fn the_table_names_each_request_exactly_once() {
    for (at, &(request, name)) in SAME.iter().enumerate() {
        assert!(!name.is_empty(), "entry {} has no name", at);
        assert_eq!(
            SAME.iter().filter(|&&(n, _)| n == request).count(),
            1,
            "{} ({:#x}) appears more than once",
            name,
            request
        );
        assert_eq!(vouched(request), Some(name));
    }
}

/// The request numbers in the table are the ones this kernel's terminal layer
/// actually answers, taken from the one place it declares them
/// (`linux_object::fs::ioctl`) rather than written out a second time here.
///
/// A second copy of an ABI number is a copy that can drift, and this is the
/// test that would notice: if `TCGETS` ever moved, the constant and the table
/// would disagree and this fails, instead of a 32-bit program silently losing
/// a request the kernel still answers.
#[test]
fn the_vouched_numbers_are_the_kernels_own_constants() {
    use linux_object::fs::ioctl as k;
    for &(constant, name) in &[
        (k::TCGETS, "TCGETS"),
        (k::TCSETS, "TCSETS"),
        (k::TCSETSW, "TCSETSW"),
        (k::TCSETSF, "TCSETSF"),
        (k::TCGETS2, "TCGETS2"),
        (k::TCSETS2, "TCSETS2"),
        (k::TCFLSH, "TCFLSH"),
        (k::TIOCSCTTY, "TIOCSCTTY"),
        (k::TIOCNOTTY, "TIOCNOTTY"),
        (k::TIOCGPGRP, "TIOCGPGRP"),
        (k::TIOCSPGRP, "TIOCSPGRP"),
        (k::TIOCGWINSZ, "TIOCGWINSZ"),
        (k::TIOCSWINSZ, "TIOCSWINSZ"),
        (k::TIOCOUTQ, "TIOCOUTQ"),
        (k::TIOCGSID, "TIOCGSID"),
        (k::FIONREAD, "FIONREAD"),
        (k::FIONBIO, "FIONBIO"),
        (k::FIOASYNC, "FIOASYNC"),
        (k::FIOCLEX, "FIOCLEX"),
        (k::FIONCLEX, "FIONCLEX"),
        (k::TIOCGPTN, "TIOCGPTN"),
        (k::TIOCSPTLCK, "TIOCSPTLCK"),
        (k::KDGETMODE, "KDGETMODE"),
        (k::KDSETMODE, "KDSETMODE"),
        (k::VT_GETMODE, "VT_GETMODE"),
        (k::VT_SETMODE, "VT_SETMODE"),
        (k::VT_GETSTATE, "VT_GETSTATE"),
        (k::VT_ACTIVATE, "VT_ACTIVATE"),
    ] {
        assert_eq!(
            vouched(constant as u32),
            Some(name),
            "{} is {:#x} in the kernel and the table does not vouch for it \
             under that number",
            name,
            constant
        );
    }
    // And `FIOQSIZE`, which the kernel declares and this table deliberately
    // leaves out: the same source of truth, the opposite answer.
    assert_eq!(vouched(k::FIOQSIZE as u32), None);
}

/// `ioctl` is answered here and NOT by the translation table. It is in
/// neither allowlist on purpose: those vouch for a call, and `ioctl` can only
/// be vouched for one request at a time.
#[test]
fn ioctl_is_answered_here_and_not_by_the_table() {
    use super::super::{name, native_number};
    assert_eq!(name(54), Some("ioctl"));
    assert_eq!(
        native_number(54),
        None,
        "ioctl must not translate straight to the native number"
    );
}
