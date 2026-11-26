//! The calls a driver in a process makes of the kernel's table, as the
//! kernel's gateway (`ECLIPSE_GEM`) takes them: an op, four words, and
//! room for the answer's words; the call's value is a word, or an `errno`.

use super::DecRef;

pub const SLICE: u32 = 0;
pub const REGISTER: u32 = 1;
pub const REGISTER_DEVICE: u32 = 2;
pub const UNREGISTER: u32 = 3;
pub const ADD_REF: u32 = 4;
pub const DEC_REF: u32 = 5;
pub const HOLDS: u32 = 6;
pub const HOLDERS: u32 = 7;
pub const IS_DEVICE: u32 = 8;
pub const LOOKUP: u32 = 9;
pub const RELEASE_PID: u32 = 10;
pub const MARK_NO_SHARE: u32 = 11;
pub const NO_SHARE: u32 = 12;
pub const CLEAR_NO_SHARE: u32 = 13;
pub const REKEY_PID: u32 = 14;
pub const LOOKUP_BY_PHYS: u32 = 15;
pub const LOOKUP_BY_PHYS_WITHIN: u32 = 16;
pub const NOTE_HOST_PA: u32 = 17;
pub const HOST_PA_TOTALS: u32 = 18;
pub const REF_COUNT: u32 = 19;
pub const LOOKUP_FOR: u32 = 20;
/// Whether the kernel's own window onto `[a, a + b)` is mapped, once it
/// has tried: a range of a memory BAR of the driver's GPU. The kernel reads
/// an object placed there through that window, and takes none it has no
/// window onto ([`REGISTER_DEVICE`]); asked first, a driver places such an
/// object elsewhere.
pub const WINDOW: u32 = 21;

/// The most words a list answer carries ([`HOLDERS`], [`RELEASE_PID`]).
pub const LIST_MAX: usize = 256;

/// `errno`s of the gateway's answers.
pub const ENOENT: i32 = 2;
pub const EPERM: i32 = 1;
pub const EINVAL: i32 = 22;

/// [`DecRef`] as a word.
pub fn dec_ref_word(d: DecRef) -> u64 {
    match d {
        DecRef::NotTracked => 0,
        DecRef::NotHolder => 1,
        DecRef::Freed => 2,
        DecRef::StillReferenced(n) => 3 + n as u64,
    }
}

/// The [`DecRef`] of a word [`dec_ref_word`] made.
pub fn dec_ref_of(w: u64) -> DecRef {
    match w {
        0 => DecRef::NotTracked,
        1 => DecRef::NotHolder,
        2 => DecRef::Freed,
        n => DecRef::StillReferenced((n - 3) as u32),
    }
}

/// A [`RELEASE_PID`] entry as a word: the handle, and whether that was its
/// last reference.
pub fn released_word(handle: u32, freed: bool) -> u64 {
    handle as u64 | (freed as u64) << 32
}

pub fn released_of(w: u64) -> (u32, bool) {
    (w as u32, w >> 32 != 0)
}
