use super::*;

/// Every type `rcore_fs` names that is not a regular file or a directory.
const OTHER_TYPES: [FileType; 5] = [
    FileType::SymLink,
    FileType::CharDevice,
    FileType::BlockDevice,
    FileType::NamedPipe,
    FileType::Socket,
];

#[test]
fn truncate_takes_a_regular_file_and_nothing_else() {
    assert_eq!(Syscall::truncate_type(FileType::File), Ok(()));
    // `do_sys_truncate`: a directory is EISDIR, ahead of the permission
    // check. It used to be EACCES for a directory the caller could not
    // write, and EISDIR only because the ramfs said so for the rest.
    assert_eq!(Syscall::truncate_type(FileType::Dir), Err(LxError::EISDIR));
    // Not a regular file: EINVAL. A character device in a ramfs used to
    // get the ramfs's own answer for "not a file", which is EISDIR.
    for type_ in OTHER_TYPES {
        assert_eq!(
            Syscall::truncate_type(type_),
            Err(LxError::EINVAL),
            "{:?}",
            type_
        );
    }
}

#[test]
fn ftruncate_is_einval_on_anything_but_a_regular_file_a_directory_included() {
    assert_eq!(Syscall::ftruncate_type(FileType::File), Ok(()));
    // `do_sys_ftruncate` has one errno for every non-file, and it is
    // not `truncate`'s EISDIR: `if (!S_ISREG(inode->i_mode) || ...)
    // goto out_putf;` with `error = -EINVAL`.
    assert_eq!(Syscall::ftruncate_type(FileType::Dir), Err(LxError::EINVAL));
    for type_ in OTHER_TYPES {
        assert_eq!(
            Syscall::ftruncate_type(type_),
            Err(LxError::EINVAL),
            "{:?}",
            type_
        );
    }
}
