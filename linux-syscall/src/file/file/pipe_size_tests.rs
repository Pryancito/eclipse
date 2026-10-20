use super::*;

#[test]
fn rounds_up_to_powers_of_two_with_page_floor() {
    assert_eq!(pipe_size_round(1), Ok(4096));
    assert_eq!(pipe_size_round(4096), Ok(4096));
    assert_eq!(pipe_size_round(4097), Ok(8192));
    assert_eq!(pipe_size_round(65536), Ok(65536));
    assert_eq!(pipe_size_round(1024 * 1024), Ok(1024 * 1024));
}

#[test]
fn rejects_zero_and_oversize() {
    assert_eq!(pipe_size_round(0), Err(LxError::EINVAL));
    assert_eq!(pipe_size_round(1024 * 1024 + 1), Err(LxError::EPERM));
    assert_eq!(pipe_size_round(usize::MAX), Err(LxError::EPERM));
}
