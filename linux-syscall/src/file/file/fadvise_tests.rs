use super::*;

#[test]
fn the_six_values_posix_fadvise_defines_are_the_ones_taken() {
    for advice in [
        POSIX_FADV_NORMAL,
        POSIX_FADV_RANDOM,
        POSIX_FADV_SEQUENTIAL,
        POSIX_FADV_WILLNEED,
        POSIX_FADV_DONTNEED,
        POSIX_FADV_NOREUSE,
    ] {
        assert!(fadvise_advice_known(advice), "{}", advice);
    }
    // The list is exactly 0..=5, so its own length is part of the claim.
    assert_eq!((0..=64).filter(|a| fadvise_advice_known(*a)).count(), 6);
}

#[test]
fn an_advice_value_from_another_word_is_not_a_fadvise_value() {
    // `MADV_FREE` is 8 and `MADV_HWPOISON` is 100: both are real advice
    // values, neither is one `fadvise` takes.
    for advice in [6, 7, 8, 100, 101, usize::MAX] {
        assert!(!fadvise_advice_known(advice), "{}", advice);
    }
}
