//! The `times` of `utimensat(2)` and `utimes(2)`: an ARRAY of two of these
//! structures, where the thing that differs between the ABIs is the STRIDE.
//!
//! A pair of `struct timespec` is 32 bytes here and **16** to a 32-bit
//! process. That is not a rounding difference: the second entry starts at
//! offset 8 there and at offset 16 here, so a handler that read the native
//! array and halved each entry afterwards would read sixteen bytes past the
//! end of the caller's buffer and take the modification time out of whatever
//! followed it.

use super::*;
use core::mem::size_of;
use kernel_hal::user::{UserInOutPtr, UserInPtr};
use linux_object::time::{TimeSpec, TimeVal};

/// A buffer with room for a native pair plus the slack to pick an address
/// inside it, handed out as an address a user pointer can be made from.
struct Room([u32; 10]);

impl Room {
    fn new() -> Self {
        // `0xaa` everywhere, as the i386 probe poisons its buffers: a word
        // this reads that it should not have read comes back as a number
        // nobody could have written by accident.
        Self([0xaaaa_aaaa; 10])
    }

    /// The base, and an address four bytes in if the base happens to be
    /// 8-aligned: the awkward one, which is what a 32-bit program's
    /// `struct timespec *` actually is.
    fn four_aligned(&mut self) -> usize {
        let base = self.0.as_mut_ptr() as usize;
        let at = if base.is_multiple_of(8) {
            base + 4
        } else {
            base
        };
        assert!(
            !at.is_multiple_of(8) && at.is_multiple_of(4),
            "the address under test has to be the awkward one: {:#x}",
            at
        );
        at
    }

    fn aligned(&mut self) -> usize {
        let base = self.0.as_mut_ptr() as usize;
        if base.is_multiple_of(8) {
            base
        } else {
            base + 4
        }
    }
}

/// Put `words` down at `at` as little-endian 32-bit values, through a user
/// pointer, so the test needs no `unsafe` of its own --- which this crate
/// denies.
fn put_words(at: usize, words: &[u32]) {
    let mut bytes = alloc::vec::Vec::new();
    for w in words {
        bytes.extend_from_slice(&w.to_ne_bytes());
    }
    let mut out: kernel_hal::user::UserOutPtr<u8> = at.into();
    out.write_array(&bytes).expect("the buffer under test");
}

/// The claim the stride rests on: an array of one of these structures is a
/// run of machine words too, with nothing between the entries at either
/// width. If a `#[repr]` ever put padding after one of them, every offset
/// below would move and this fails first.
#[test]
fn the_entries_of_an_array_sit_back_to_back() {
    assert_eq!(size_of::<[TimeSpec; 2]>(), 2 * size_of::<TimeSpec>());
    assert_eq!(size_of::<[TimeVal; 2]>(), 2 * size_of::<TimeVal>());
    // And the two widths of the pair, which are the numbers the handlers
    // depend on.
    assert_eq!(size_of::<[TimeSpec; 2]>(), 32, "the native pair");
    assert_eq!(<TimeSpec as Narrow>::WORDS * 4 * 2, 16, "the i386 pair");
}

/// The second entry of a 32-bit caller's pair starts at offset **8**. Read at
/// the native stride it would start at 16, which here is poison.
#[test]
fn the_second_entry_of_a_narrow_pair_comes_from_offset_eight() {
    let mut room = Room::new();
    let at = room.aligned();
    // Only the sixteen bytes a 32-bit caller owns; everything after them is
    // still `0xaa`.
    put_words(at, &[11, 22, 33, 44]);

    let ptr: UserInPtr<[TimeSpec; 2]> = at.into();
    let [a, m] = narrow_get_array(true, &ptr).expect("the pair the caller wrote");
    assert_eq!((a.sec, a.nsec), (11, 22), "atime from offsets 0 and 4");
    assert_eq!((m.sec, m.nsec), (33, 44), "mtime from offsets 8 and 12");
}

/// And the native stride is 16, which is the other half of the same claim:
/// the same function at `abi32 == false` reads the entry at 16 and not the
/// one at 8.
#[test]
fn the_second_entry_of_a_native_pair_comes_from_offset_sixteen() {
    let mut room = Room::new();
    let at = room.aligned();
    let mut out: kernel_hal::user::UserOutPtr<[TimeSpec; 2]> = at.into();
    out.write([TimeSpec { sec: 7, nsec: 8 }, TimeSpec { sec: 9, nsec: 10 }])
        .expect("the buffer under test");

    let ptr: UserInPtr<[TimeSpec; 2]> = at.into();
    let [a, m] = narrow_get_array(false, &ptr).expect("the native pair");
    assert_eq!((a.sec, a.nsec), (7, 8));
    assert_eq!((m.sec, m.nsec), (9, 10));

    // What the bytes actually are, so that the offset and not just the
    // round trip is pinned: the second entry's seconds at byte 16.
    let image: UserInPtr<u8> = at.into();
    let bytes = image.read_array(32).expect("the image just written");
    let word = |off: usize| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[off..off + 8]);
        u64::from_ne_bytes(buf)
    };
    assert_eq!(word(0), 7, "atime.sec at 0");
    assert_eq!(word(16), 9, "mtime.sec at 16, not at 8");
}

/// The native branch reads **unaligned** too, and this is the test that
/// measures it: a `[TimeSpec; 2]` at a 4-aligned address, read with
/// `abi32 == false`.
///
/// It is not a theoretical case. `utimensat_time64` (i386's 412) is a 32-bit
/// process passing this pair at the NATIVE width --- thirty-two bytes, at a
/// 4-aligned address, because on i386 an `__s64` aligns to 4. That is the
/// fifth form of the trap: `abi32 == false` is a statement about the
/// structure and not about the process. Without this test, replacing the
/// native branch's `read_unaligned()` with `read()` leaves every other test
/// in this file green and answers `EFAULT` for a pointer Linux accepts.
///
/// Pointed out by Copilot on the #2410, which had exactly this gap.
#[test]
fn a_four_aligned_native_pair_is_read_rather_than_refused() {
    let mut room = Room::new();
    let at = room.four_aligned();
    // Written as WORDS and not with the typed `write`, which is the whole
    // point: at this address the typed write refuses too.
    put_words(at, &[21, 0, 0, 0, 22, 0, 0, 0]);

    let ptr: UserInOutPtr<[TimeSpec; 2]> = at.into();
    assert!(
        ptr.read().is_err(),
        "a 4-aligned native pair is exactly what the typed read refuses"
    );

    let [a, m] = narrow_get_array(false, &ptr).expect("bytes have no alignment to respect");
    assert_eq!((a.sec, a.nsec), (21, 0), "atime.sec at 0, nsec at 8");
    assert_eq!((m.sec, m.nsec), (22, 0), "mtime.sec at 16, nsec at 24");
}

/// A 32-bit caller's pair is 4-aligned, because on i386 every field of these
/// structures is a `long`. The typed read of a `[TimeSpec; 2]` refuses such
/// an address --- it stores through a `*[TimeSpec; 2]`, so it has to insist
/// on 8 --- and `copy_from_user` insists on nothing. That is the fifth form
/// of the alignment trap, and it reaches arrays too.
#[test]
fn a_four_aligned_pair_is_read_rather_than_refused() {
    let mut room = Room::new();
    let at = room.four_aligned();
    put_words(at, &[1, 2, 3, 4]);

    let ptr: UserInOutPtr<[TimeSpec; 2]> = at.into();
    assert!(
        ptr.read().is_err(),
        "a 4-aligned pair is exactly what the typed read refuses"
    );

    let [a, m] = narrow_get_array(true, &ptr).expect("bytes have no alignment to respect");
    assert_eq!((a.sec, a.nsec), (1, 2));
    assert_eq!((m.sec, m.nsec), (3, 4));
}

/// `UTIME_OMIT` is `(1 << 30) - 2` and `UTIME_NOW` is `(1 << 30) - 1`:
/// positive, and so unremarkable. A negative SECOND is the one that matters,
/// because the narrow words are sign-extended and a file's time before 1970
/// read zero-extended would land in the year 2106.
#[test]
fn a_time_before_the_epoch_is_sign_extended() {
    let mut room = Room::new();
    let at = room.aligned();
    put_words(at, &[(-2i32) as u32, 0, (-1i32) as u32, 0]);

    let ptr: UserInPtr<[TimeSpec; 2]> = at.into();
    let [a, m] = narrow_get_array(true, &ptr).expect("the pair the caller wrote");
    assert_eq!(a.sec as isize, -2, "two seconds before the epoch");
    assert_eq!(m.sec as isize, -1);
}

/// A null `times` is "now" in both calls, and that is the helper's job rather
/// than each handler's: `utimensat(fd, NULL, NULL, 0)` is how `futimens(fd,
/// NULL)` touches a file.
#[test]
fn a_null_pair_is_not_asked_for_rather_than_a_bad_address() {
    let ptr: UserInPtr<[TimeSpec; 2]> = 0usize.into();
    assert_eq!(
        narrow_get_array_if_not_null(true, &ptr),
        Ok(None),
        "a null times is a touch, at either width"
    );
    assert_eq!(narrow_get_array_if_not_null(false, &ptr), Ok(None));
    // And the plain getter still says what a null pointer is.
    assert_eq!(
        narrow_get_array::<TimeSpec, _, 2>(true, &ptr),
        Err(linux_object::error::LxError::EFAULT)
    );
}

/// The `timeval` pair of `utimes(2)` is the same shape, which is the point of
/// the helper being generic over the structure rather than written twice.
#[test]
fn the_timeval_pair_of_utimes_narrows_the_same_way() {
    let mut room = Room::new();
    let at = room.four_aligned();
    put_words(at, &[100, 999_999, 200, 1]);

    let ptr: UserInPtr<[TimeVal; 2]> = at.into();
    let [a, m] = narrow_get_array(true, &ptr).expect("the pair the caller wrote");
    assert_eq!((a.sec, a.usec), (100, 999_999));
    assert_eq!((m.sec, m.usec), (200, 1));
}
