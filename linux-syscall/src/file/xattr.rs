//! The extended-attribute syscalls: `getxattr`, `setxattr`, `listxattr`,
//! `removexattr` and their `l*` and `f*` forms.
//!
//! None of this kernel's filesystems keeps extended attributes, so the
//! twelve used to be answered from the dispatcher without reading a single
//! argument: `listxattr` was 0, `getxattr` and `removexattr` `ENODATA`,
//! `setxattr` `EOPNOTSUPP`. That is the right answer for a file that exists,
//! asked with a name that is one. `fs/xattr.c` reads the name and the flags
//! first, then looks the file up, and only then asks the filesystem, so
//! `getxattr("/no/such/file", "user.x")` is `ENOENT`, `fsetxattr(-1, ...)`
//! is `EBADF`, a name over `XATTR_NAME_MAX` is `ERANGE`, and a value over
//! `XATTR_SIZE_MAX` is `E2BIG`. Here all four were the filesystem's answer.

use super::*;

/// `XATTR_NAME_MAX` (`include/uapi/linux/limits.h`): the longest attribute
/// name, without its NUL.
const XATTR_NAME_MAX: usize = 255;
/// `XATTR_SIZE_MAX`: the largest value `setxattr` takes.
const XATTR_SIZE_MAX: usize = 65536;
/// `XATTR_CREATE` and `XATTR_REPLACE`: the only two bits `setxattr` knows.
const XATTR_FLAGS_KNOWN: usize = 1 | 2;

/// Which of the four operations, with the arguments `fs/xattr.c` judges
/// before it looks the file up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XattrOp {
    /// `getxattr`: `size` is only a buffer length, and one over
    /// `XATTR_SIZE_MAX` is clamped, not refused.
    Get,
    /// `setxattr`, with its value length and its flags word.
    Set { size: usize, flags: usize },
    /// `listxattr`: no name to judge.
    List,
    /// `removexattr`.
    Remove,
}

/// The file an xattr syscall names: a path (followed or not) or a descriptor.
#[derive(Debug)]
pub(crate) enum XattrTarget {
    /// `getxattr`/`setxattr`/`listxattr`/`removexattr` follow a final
    /// symlink; the `l*` forms do not.
    Path(UserInPtr<u8>, bool),
    /// The `f*` forms.
    Fd(FileDesc),
}

/// What `setxattr_copy`, `getxattr` and `removexattr` check before the file
/// is looked up, in their order: the flags word (`EINVAL`), then the name
/// (`ERANGE` when empty or longer than `XATTR_NAME_MAX`), then, for a
/// `setxattr`, the value (`E2BIG` past `XATTR_SIZE_MAX`). `listxattr` has
/// none of the three.
pub(crate) fn xattr_precheck(op: XattrOp, name: Option<&str>) -> LxResult<()> {
    if let XattrOp::Set { flags, .. } = op {
        if flags & !XATTR_FLAGS_KNOWN != 0 {
            return Err(LxError::EINVAL);
        }
    }
    if let Some(name) = name {
        if name.is_empty() || name.len() > XATTR_NAME_MAX {
            return Err(LxError::ERANGE);
        }
    }
    if let XattrOp::Set { size, .. } = op {
        if size > XATTR_SIZE_MAX {
            return Err(LxError::E2BIG);
        }
    }
    Ok(())
}

/// The filesystem's answer, once the file is known to exist: an attribute
/// that is not there for `getxattr` and `removexattr`, no attributes for
/// `listxattr`, and no way to set one.
pub(crate) fn xattr_answer(op: XattrOp) -> SysResult {
    match op {
        XattrOp::Get | XattrOp::Remove => Err(LxError::ENODATA),
        XattrOp::List => Ok(0),
        XattrOp::Set { .. } => Err(LxError::EOPNOTSUPP),
    }
}

impl Syscall<'_> {
    /// The twelve `*xattr` syscalls: the checks `fs/xattr.c` makes before it
    /// asks the filesystem, then the answer of a filesystem that keeps none.
    pub(crate) fn sys_xattr(
        &self,
        op: XattrOp,
        target: XattrTarget,
        name: UserInPtr<u8>,
    ) -> SysResult {
        let name = if op == XattrOp::List {
            None
        } else {
            Some(name.as_c_str()?)
        };
        info!("xattr: op={:?}, target={:?}, name={:?}", op, target, name);
        xattr_precheck(op, name)?;
        let proc = self.linux_process();
        match target {
            XattrTarget::Path(path, follow) => {
                proc.lookup_inode_at(FileDesc::CWD, path.as_c_str()?, follow)?;
            }
            XattrTarget::Fd(fd) => {
                proc.get_file_like(fd)?;
            }
        }
        xattr_answer(op)
    }
}

#[cfg(test)]
mod xattr_tests {
    //! What the xattr syscalls judge before the filesystem gets a say. They
    //! used to judge nothing: the dispatcher answered them by name.

    use super::*;

    /// `setxattr_copy`: a flag bit past `XATTR_CREATE | XATTR_REPLACE` is
    /// `EINVAL`, before the name is read.
    #[test]
    fn a_setxattr_flag_past_create_and_replace_is_einval() {
        for flags in [0, 1, 2, 3] {
            assert_eq!(
                xattr_precheck(XattrOp::Set { size: 4, flags }, Some("user.a")),
                Ok(())
            );
        }
        // Alone or beside a known bit: `flags & ~(XATTR_CREATE | XATTR_REPLACE)`.
        for flags in [4, 1 | 4, 2 | 8, 1 << 31] {
            assert_eq!(
                xattr_precheck(XattrOp::Set { size: 4, flags }, Some("user.a")),
                Err(LxError::EINVAL),
                "{:#x}",
                flags
            );
        }
        // Before the name: a bad flag with a bad name is still EINVAL.
        assert_eq!(
            xattr_precheck(XattrOp::Set { size: 4, flags: 8 }, Some("")),
            Err(LxError::EINVAL)
        );
    }

    /// `strncpy_from_user` into `char kname[XATTR_NAME_MAX + 1]`: a length of
    /// 0 or of the whole buffer is `ERANGE`, for get, set and remove alike.
    #[test]
    fn an_empty_or_overlong_name_is_erange() {
        let longest = "u".repeat(XATTR_NAME_MAX);
        let too_long = "u".repeat(XATTR_NAME_MAX + 1);
        for op in [
            XattrOp::Get,
            XattrOp::Set { size: 0, flags: 0 },
            XattrOp::Remove,
        ] {
            assert_eq!(xattr_precheck(op, Some("user.a")), Ok(()), "{:?}", op);
            assert_eq!(xattr_precheck(op, Some(&longest)), Ok(()), "{:?}", op);
            assert_eq!(
                xattr_precheck(op, Some("")),
                Err(LxError::ERANGE),
                "{:?}",
                op
            );
            assert_eq!(
                xattr_precheck(op, Some(&too_long)),
                Err(LxError::ERANGE),
                "{:?}",
                op
            );
        }
        assert_eq!(xattr_precheck(XattrOp::List, None), Ok(()));
    }

    /// `setxattr`: a value over `XATTR_SIZE_MAX` is `E2BIG`, judged after
    /// the name. `getxattr` takes any buffer length.
    #[test]
    fn a_value_over_xattr_size_max_is_e2big_for_setxattr_only() {
        let set = |size| XattrOp::Set { size, flags: 0 };
        assert_eq!(xattr_precheck(set(XATTR_SIZE_MAX), Some("user.a")), Ok(()));
        assert_eq!(
            xattr_precheck(set(XATTR_SIZE_MAX + 1), Some("user.a")),
            Err(LxError::E2BIG)
        );
        assert_eq!(
            xattr_precheck(set(XATTR_SIZE_MAX + 1), Some("")),
            Err(LxError::ERANGE),
            "the name comes first"
        );
        assert_eq!(xattr_precheck(XattrOp::Get, Some("user.a")), Ok(()));
    }

    /// The four answers of a filesystem without attributes.
    #[test]
    fn the_answers_of_a_filesystem_without_attributes() {
        assert_eq!(xattr_answer(XattrOp::Get), Err(LxError::ENODATA));
        assert_eq!(xattr_answer(XattrOp::Remove), Err(LxError::ENODATA));
        assert_eq!(xattr_answer(XattrOp::List), Ok(0));
        assert_eq!(
            xattr_answer(XattrOp::Set { size: 1, flags: 0 }),
            Err(LxError::EOPNOTSUPP)
        );
    }
}
