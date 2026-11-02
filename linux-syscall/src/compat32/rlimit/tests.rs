//! The two directions of a narrow `struct rlimit`, against the table in the
//! module documentation --- which was measured on this container's Linux, not
//! read off a header.

use super::*;

/// Reading out clamps. The row that matters is `1 << 33`: truncating it
/// gives 0, which tells a process whose limit is eight gigabytes that it has
/// no limit at all, in the direction that forbids rather than allows.
#[test]
fn reading_a_limit_out_clamps_it_and_never_truncates() {
    for (wide, expected) in [
        (1000u64, 1000u32),
        (u64::from(u32::MAX) - 1, u32::MAX - 1),
        // The measured row.
        (1 << 33, COMPAT_RLIM_INFINITY),
        (RLIM_INFINITY, COMPAT_RLIM_INFINITY),
    ] {
        assert_eq!(
            narrow_word(wide, COMPAT_RLIM_INFINITY),
            expected,
            "{:#x} through ugetrlimit",
            wide
        );
    }
    // And the two rows where a clamp and a truncation differ, which is the
    // whole point: `1 << 33` truncates to 0 and infinity truncates to
    // 0xffffffff by accident. Only the first distinguishes the two, so it is
    // asserted on its own rather than over the loop above --- for a value
    // that fits, the clamp and the truncation agree and an `assert_ne!`
    // would be claiming otherwise.
    assert_eq!((1u64 << 33) as u32, 0, "what truncating would have given");
    assert_eq!(narrow_word(1 << 33, COMPAT_RLIM_INFINITY), u32::MAX);
}

/// And the two narrow calls clamp at different numbers: 191 at `0xffffffff`,
/// 76 at `0x7fffffff`. That is the ONLY difference between them, which is
/// why one helper takes the ceiling as an argument.
#[test]
fn the_old_getrlimit_clamps_lower_than_ugetrlimit_does() {
    assert_eq!(OLD_RLIM_INFINITY, 0x7fff_ffff);
    assert_eq!(COMPAT_RLIM_INFINITY, 0xffff_ffff);
    for wide in [1u64 << 33, RLIM_INFINITY] {
        assert_eq!(narrow_word(wide, OLD_RLIM_INFINITY), 0x7fff_ffff);
        assert_eq!(narrow_word(wide, COMPAT_RLIM_INFINITY), 0xffff_ffff);
    }
    // A value that fits in a signed long is the same through both.
    assert_eq!(narrow_word(2000, OLD_RLIM_INFINITY), 2000);
    assert_eq!(narrow_word(2000, COMPAT_RLIM_INFINITY), 2000);
}

/// Writing in widens, and `0xffffffff` is "infinity" rather than the number
/// it spells. A process that asks for no limit and gets four gigabytes is
/// the kind of thing found years later.
#[test]
fn the_narrow_infinity_widens_to_the_wide_infinity_and_not_to_four_gigabytes() {
    assert_eq!(widen(COMPAT_RLIM_INFINITY), RLIM_INFINITY);
    assert_ne!(
        widen(COMPAT_RLIM_INFINITY),
        u64::from(u32::MAX),
        "0xffffffff is not the number 4294967295 here"
    );
    assert_eq!(widen(1234), 1234);
    assert_eq!(widen(0), 0);
    // One below infinity is still a number.
    assert_eq!(widen(COMPAT_RLIM_INFINITY - 1), u64::from(u32::MAX) - 1);
}

/// `0x7fffffff` is NOT infinity on the way in, even though `getrlimit` (76)
/// reports it for one. The clamp is a report, not a meaning: nothing in
/// `setrlimit` reads that number specially, and the measurement only ever
/// showed `0xffffffff` widening.
#[test]
fn the_lower_ceiling_is_a_report_and_not_a_second_meaning_of_infinity() {
    assert_eq!(widen(OLD_RLIM_INFINITY), u64::from(OLD_RLIM_INFINITY));
    assert_ne!(widen(OLD_RLIM_INFINITY), RLIM_INFINITY);
}

/// The narrow structure is eight bytes, which is what makes it a conversion
/// at all: writing sixteen would put eight bytes of the native layout into
/// whatever the caller keeps after its `struct rlimit`.
#[test]
fn the_narrow_structure_is_half_the_native_one() {
    assert_eq!(RLIMIT32_SIZE, 8);
    assert_eq!(core::mem::size_of::<RLimit>(), 16);
}

/// `prlimit64` (340) is deliberately not one of these: its structure is
/// `struct rlimit64` in both ABIs, so it is in `i386_abi_same.in` and the
/// table maps it. Pinned here because it is the call musl's `getrlimit(3)`
/// really makes, so getting it wrong would be the common path and not the
/// compatibility one.
#[test]
fn prlimit64_is_mapped_and_the_two_narrow_calls_are_not() {
    use super::super::{name, native_number};
    assert_eq!(name(340), Some("prlimit64"));
    assert!(
        native_number(340).is_some(),
        "prlimit64 passes struct rlimit64 in both ABIs"
    );
    for (num, called) in [(75, "setrlimit"), (76, "getrlimit"), (191, "ugetrlimit")] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) is answered here, not by the table",
            num,
            called
        );
    }
}
