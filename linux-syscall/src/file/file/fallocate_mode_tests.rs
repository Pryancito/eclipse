use super::*;

#[test]
fn the_plain_mode_may_grow_the_file_and_keep_size_may_not() {
    assert_eq!(
        fallocate_op(0),
        Ok(FallocateOp::Allocate { keep_size: false })
    );
    assert_eq!(
        fallocate_op(FALLOC_FL_KEEP_SIZE),
        Ok(FallocateOp::Allocate { keep_size: true })
    );
}

#[test]
fn a_bit_fallocate_does_not_define_is_not_supported() {
    // 0x04 is `FALLOC_FL_NO_HIDE_STALE`, which is a named flag and still
    // outside the supported mask.
    for mode in [0x04, 0x80, 0x100, usize::MAX] {
        assert_eq!(fallocate_op(mode), Err(LxError::EOPNOTSUPP), "{mode:#x}");
    }
}

#[test]
fn a_hole_punch_must_promise_not_to_change_the_size() {
    assert_eq!(fallocate_op(FALLOC_FL_PUNCH_HOLE), Err(LxError::EOPNOTSUPP));
    assert_eq!(
        fallocate_op(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE),
        Ok(FallocateOp::ZeroRange { keep_size: true })
    );
}

#[test]
fn punching_and_zeroing_the_same_range_are_two_different_orders() {
    assert_eq!(
        fallocate_op(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE),
        Err(LxError::EOPNOTSUPP)
    );
}

#[test]
fn zero_range_is_the_one_of_the_two_that_may_grow_the_file() {
    assert_eq!(
        fallocate_op(FALLOC_FL_ZERO_RANGE),
        Ok(FallocateOp::ZeroRange { keep_size: false })
    );
    assert_eq!(
        fallocate_op(FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE),
        Ok(FallocateOp::ZeroRange { keep_size: true })
    );
}

#[test]
fn collapse_and_insert_must_travel_alone() {
    for mode in [FALLOC_FL_COLLAPSE_RANGE, FALLOC_FL_INSERT_RANGE] {
        // Alone they are understood, and refused for want of an extent map.
        assert_eq!(fallocate_op(mode), Err(LxError::EOPNOTSUPP), "{mode:#x}");
        // With any company at all they are not even understood.
        assert_eq!(
            fallocate_op(mode | FALLOC_FL_KEEP_SIZE),
            Err(LxError::EINVAL),
            "{mode:#x}"
        );
    }
    assert_eq!(
        fallocate_op(FALLOC_FL_COLLAPSE_RANGE | FALLOC_FL_INSERT_RANGE),
        Err(LxError::EINVAL)
    );
}

#[test]
fn unshare_travels_with_keep_size_and_with_nothing_else() {
    assert_eq!(
        fallocate_op(FALLOC_FL_UNSHARE_RANGE),
        Ok(FallocateOp::Allocate { keep_size: false })
    );
    assert_eq!(
        fallocate_op(FALLOC_FL_UNSHARE_RANGE | FALLOC_FL_KEEP_SIZE),
        Ok(FallocateOp::Allocate { keep_size: true })
    );
    assert_eq!(
        fallocate_op(FALLOC_FL_UNSHARE_RANGE | FALLOC_FL_ZERO_RANGE),
        Err(LxError::EINVAL)
    );
}
