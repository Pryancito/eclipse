use super::*;

#[test]
fn the_flags_word_is_reserved_and_has_to_stay_empty() {
    assert_eq!(copy_file_range_flags(0), Ok(()));
    for flags in [1, 2, 4, 1 << 31, usize::MAX] {
        assert_eq!(
            copy_file_range_flags(flags),
            Err(LxError::EINVAL),
            "{flags}"
        );
    }
}
