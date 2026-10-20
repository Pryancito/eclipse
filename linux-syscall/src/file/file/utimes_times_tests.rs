use super::*;
use linux_object::time::TimeVal;

const NOW: TimeSpec = TimeSpec { sec: 1000, nsec: 5 };

fn ts(sec: usize, nsec: usize) -> TimeSpec {
    TimeSpec { sec, nsec }
}

#[test]
fn a_null_times_and_a_pair_of_nows_are_the_same_touch() {
    assert_eq!(utimensat_times(None, NOW), Ok(TimesToSet::Touch(NOW)));
    assert_eq!(
        utimensat_times(Some([ts(7, UTIME_NOW), ts(9, UTIME_NOW)]), NOW),
        Ok(TimesToSet::Touch(NOW)),
        "the seconds next to UTIME_NOW mean nothing"
    );
}

#[test]
fn a_pair_of_omits_asks_for_nothing() {
    assert_eq!(
        utimensat_times(Some([ts(7, UTIME_OMIT), ts(9, UTIME_OMIT)]), NOW),
        Ok(TimesToSet::Nothing)
    );
}

#[test]
fn an_explicit_time_is_set_and_the_other_half_follows_its_own_marker() {
    // `touch -m -d @42`: mtime explicit, atime untouched.
    assert_eq!(
        utimensat_times(Some([ts(1, UTIME_OMIT), ts(42, 0)]), NOW),
        Ok(TimesToSet::Set {
            atime: None,
            mtime: Some(ts(42, 0))
        })
    );
    // `touch -a -d @42` with mtime "now": still an explicit set, so
    // still the owner's to do, and the NOW half is this instant.
    assert_eq!(
        utimensat_times(Some([ts(42, 0), ts(1, UTIME_NOW)]), NOW),
        Ok(TimesToSet::Set {
            atime: Some(ts(42, 0)),
            mtime: Some(NOW)
        })
    );
}

#[test]
fn a_nanosecond_field_out_of_range_is_einval_whatever_the_other_half() {
    assert_eq!(
        utimensat_times(Some([ts(1, 1_000_000_000), ts(1, UTIME_OMIT)]), NOW),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        utimensat_times(Some([ts(1, UTIME_NOW), ts(1, usize::MAX)]), NOW),
        Err(LxError::EINVAL),
        "a NOW half does not excuse the other"
    );
    // The two markers sit just above the valid range and are not it.
    assert_eq!(UTIME_NOW, 0x3fff_ffff);
    assert_eq!(UTIME_OMIT, 0x3fff_fffe);
    assert!(utimensat_times(Some([ts(1, 999_999_999), ts(1, 0)]), NOW).is_ok());
}

#[test]
fn utimes_is_always_explicit_and_checks_its_microseconds() {
    let tv = |sec, usec| TimeVal { sec, usec };
    assert_eq!(utimes_times(None, NOW), Ok(TimesToSet::Touch(NOW)));
    assert_eq!(
        utimes_times(Some([tv(1, 500), tv(2, 999_999)]), NOW),
        Ok(TimesToSet::Set {
            atime: Some(ts(1, 500_000)),
            mtime: Some(ts(2, 999_999_000))
        })
    );
    assert_eq!(
        utimes_times(Some([tv(1, 1_000_000), tv(2, 0)]), NOW),
        Err(LxError::EINVAL)
    );
}
