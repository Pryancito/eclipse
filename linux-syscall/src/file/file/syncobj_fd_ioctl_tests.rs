use super::{syncobj_fd_args_check, syncobj_fd_missing_handle_errno, LxError};

/// libdrm's two shapes go through; a pad, an unknown bit, and the
/// timeline bit (alone or with the sync_file one) are EINVAL.
#[test]
fn only_pad_zero_and_the_sync_file_bit_are_accepted() {
    assert_eq!(syncobj_fd_args_check(0, 0), Ok(()));
    assert_eq!(syncobj_fd_args_check(1, 0), Ok(()));
    assert_eq!(syncobj_fd_args_check(0, 1), Err(LxError::EINVAL));
    assert_eq!(syncobj_fd_args_check(1, 0xdead_beef), Err(LxError::EINVAL));
    // DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE_TIMELINE and the
    // import twin: bit 1.
    assert_eq!(syncobj_fd_args_check(1 << 1, 0), Err(LxError::EINVAL));
    assert_eq!(syncobj_fd_args_check(0x3, 0), Err(LxError::EINVAL));
    for bit in [2, 3, 8, 16, 31] {
        assert_eq!(syncobj_fd_args_check(1 << bit, 0), Err(LxError::EINVAL));
    }
}

/// `drm_syncobj_handle_to_fd` says EINVAL, `drm_syncobj_find_fence`
/// (the sync_file export) and the import's destination lookup ENOENT.
#[test]
fn a_missing_handle_is_einval_on_the_opaque_export_and_enoent_elsewhere() {
    assert_eq!(
        syncobj_fd_missing_handle_errno(true, false),
        LxError::EINVAL
    );
    assert_eq!(syncobj_fd_missing_handle_errno(true, true), LxError::ENOENT);
    assert_eq!(
        syncobj_fd_missing_handle_errno(false, true),
        LxError::ENOENT
    );
    assert_eq!(
        syncobj_fd_missing_handle_errno(false, false),
        LxError::ENOENT
    );
}
