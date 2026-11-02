//! What the six structures look like to a 32-bit process, against the sizes
//! measured on real Linux rather than against a header read from memory.

use super::*;
use core::mem::size_of;

/// The narrow image, as a vector of its bytes.
fn narrow<T: Narrow>(value: T) -> alloc::vec::Vec<u8> {
    let (bytes, len) = to_narrow_bytes(value);
    bytes[..len].to_vec()
}

/// The words of a narrow image, read back as unsigned 32-bit values.
fn words_of(bytes: &[u8]) -> alloc::vec::Vec<u32> {
    bytes
        .chunks(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// The sizes measured on this container's Linux with an i386 program that
/// poisons a buffer with `0xaa`, makes the call and looks at where the poison
/// starts again. A `#[repr(C)]` of `u64` fields would put `rusage` at 144 and
/// write twice as far into the caller's buffer as it owns.
#[test]
fn the_narrow_sizes_are_the_ones_measured_on_real_linux() {
    assert_eq!(narrow(TimeSpec::default()).len(), 8, "struct timespec");
    assert_eq!(narrow(TimeVal::default()).len(), 8, "struct timeval");
    assert_eq!(narrow(ITimerVal::default()).len(), 16, "struct itimerval");
    assert_eq!(narrow(ITimerSpec::default()).len(), 16, "struct itimerspec");
    assert_eq!(
        narrow(Tms {
            tms_utime: 0,
            tms_stime: 0,
            tms_cutime: 0,
            tms_cstime: 0,
        })
        .len(),
        16,
        "struct tms"
    );
    assert_eq!(narrow(RUsage::default()).len(), 72, "struct rusage");
}

/// The claim the trait makes about each structure: it is a run of words and
/// nothing else, so the native image is exactly twice the narrow one. A field
/// added to one of these without updating `WORDS` fails here instead of
/// writing the caller's buffer short.
#[test]
fn the_word_count_is_half_the_native_size_for_every_one_of_them() {
    macro_rules! check {
        ($($t:ty),* $(,)?) => {$({
            assert_eq!(
                <$t as Narrow>::WORDS * 8,
                size_of::<$t>(),
                concat!(stringify!($t), ": WORDS does not match the native size"),
            );
        })*};
    }
    check!(TimeSpec, TimeVal, ITimerVal, ITimerSpec, Tms, RUsage);
}

/// `ru_maxrss` at 16, `ru_minflt` at 32, `ru_nvcsw` at 64: the three offsets
/// the measurement pinned down, and the three a doubled layout would move.
#[test]
fn the_rusage_fields_land_where_the_i386_call_put_them() {
    let usage = RUsage {
        utime: TimeVal { sec: 1, usec: 2 },
        stime: TimeVal { sec: 3, usec: 4 },
        maxrss: 1572,
        minflt: 50,
        nvcsw: 7,
        nivcsw: 9,
        ..RUsage::default()
    };
    let words = words_of(&narrow(usage));
    assert_eq!(words.len(), 18);
    assert_eq!(
        &words[0..4],
        &[1, 2, 3, 4],
        "the two timevals, 8 bytes each"
    );
    assert_eq!(words[4], 1572, "ru_maxrss, at byte 16");
    assert_eq!(words[8], 50, "ru_minflt, at byte 32");
    assert_eq!(words[16], 7, "ru_nvcsw, at byte 64");
    assert_eq!(words[17], 9, "ru_nivcsw, at byte 68");
}

/// The nested pairs are in ABI order and not swapped: `it_interval` first,
/// which is the order `setitimer(2)` and `timer_settime(2)` both document.
#[test]
fn the_interval_comes_before_the_value_in_both_of_the_pairs() {
    let words = words_of(&narrow(ITimerVal {
        interval: TimeVal { sec: 11, usec: 12 },
        value: TimeVal { sec: 13, usec: 14 },
    }));
    assert_eq!(words, [11, 12, 13, 14]);

    let words = words_of(&narrow(ITimerSpec {
        interval: TimeSpec { sec: 21, nsec: 22 },
        value: TimeSpec { sec: 23, nsec: 24 },
    }));
    assert_eq!(words, [21, 22, 23, 24]);
}

/// Writing narrows by truncation, as `put_old_timespec32` does: a `tv_sec`
/// past 2106 comes back wrapped, not clamped and not refused. The year 2106
/// is where the field runs out of bits; a program reading it into a signed
/// `time_t` breaks in 2038, which is the `*_time64` calls' whole reason to
/// exist.
#[test]
fn a_second_too_big_for_the_field_is_truncated_and_not_clamped() {
    let past_2106 = 0x1_0000_0003_usize;
    let words = words_of(&narrow(TimeSpec {
        sec: past_2106,
        nsec: 5,
    }));
    assert_eq!(words, [3, 5], "the top bits are dropped, not saturated");
}

/// Reading widens by **sign** extension, because `old_time32_t` is signed.
/// Zero-extending instead would turn the `-1` a program passes to mean "no
/// timeout" into 4294967295, a number that passes every validity check.
#[test]
fn a_negative_word_is_sign_extended_and_not_zero_extended() {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&(-1i32).to_ne_bytes());
    bytes[4..8].copy_from_slice(&(-2i32).to_ne_bytes());
    let ts: TimeSpec = from_narrow_bytes(&bytes);
    assert_eq!(ts.sec, usize::MAX, "-1 sign-extended");
    assert_eq!(ts.nsec, usize::MAX - 1, "-2 sign-extended");
    assert_ne!(ts.sec, u32::MAX as usize, "not zero-extended");
}

/// Narrow and back is the identity for anything that fits, which is what
/// makes the write-back of an `InOut` timeout (`select`, `pselect6`) safe to
/// route through the same pair.
#[test]
fn narrowing_and_widening_round_trips_everything_that_fits() {
    let ts = TimeSpec {
        sec: 1_700_000_000,
        nsec: 999_999_999,
    };
    let (bytes, len) = to_narrow_bytes(ts);
    assert_eq!(from_narrow_bytes::<TimeSpec>(&bytes[..len]), ts);

    let usage = RUsage {
        utime: TimeVal { sec: 5, usec: 6 },
        minflt: 123,
        nvcsw: 456,
        ..RUsage::default()
    };
    let (bytes, len) = to_narrow_bytes(usage);
    let back: RUsage = from_narrow_bytes(&bytes[..len]);
    assert_eq!(back.utime.sec, 5);
    assert_eq!(back.utime.usec, 6);
    assert_eq!(back.minflt, 123);
    assert_eq!(back.nvcsw, 456);
}
