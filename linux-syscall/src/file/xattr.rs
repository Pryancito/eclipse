//! The extended-attribute syscalls: `getxattr`, `setxattr`, `listxattr`,
//! `removexattr` and their `l*` and `f*` forms.
//!
//! EclipseFS keeps extended attributes (`linux_object::fs::eclipsefs_xattr`);
//! every other filesystem here answers as one without them. The twelve used
//! to be answered from the dispatcher without reading a single argument: `listxattr` was 0, `getxattr` and `removexattr` `ENODATA`,
//! `setxattr` `EOPNOTSUPP`. That is the right answer for a file that exists,
//! asked with a name that is one. `fs/xattr.c` reads the name and the flags
//! first, then looks the file up, and only then asks the filesystem, so
//! `getxattr("/no/such/file", "user.x")` is `ENOENT`, `fsetxattr(-1, ...)`
//! is `EBADF`, a name over `XATTR_NAME_MAX` is `ERANGE`, and a value over
//! `XATTR_SIZE_MAX` is `E2BIG`. Here all four were the filesystem's answer.

use super::*;
use alloc::string::String;
use linux_object::fs::{XattrReply, XattrRequest};

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
    /// asks the filesystem, then the filesystem's answer. `buf` and `size`
    /// are the value (get, set) or the list (list) buffer.
    pub(crate) fn sys_xattr(
        &self,
        op: XattrOp,
        target: XattrTarget,
        name: UserInPtr<u8>,
        buf: usize,
        size: usize,
    ) -> SysResult {
        let name = if op == XattrOp::List {
            None
        } else {
            Some(name.as_c_str()?)
        };
        info!("xattr: op={:?}, target={:?}, name={:?}", op, target, name);
        xattr_precheck(op, name)?;
        let proc = self.linux_process();
        let (inode, path) = match target {
            XattrTarget::Path(path, follow) => {
                let path = path.as_c_str()?;
                let inode = proc.lookup_inode_at(FileDesc::CWD, path, follow)?;
                let path = if path.starts_with('/') {
                    String::from(path)
                } else {
                    format!("{}/{}", proc.current_working_directory(), path)
                };
                (Some(inode), path)
            }
            XattrTarget::Fd(fd) => {
                let file = proc.get_file_like(fd)?;
                match file.downcast_ref::<File>() {
                    Some(file) => (Some(file.inode()), file.path().clone()),
                    None => (None, String::new()),
                }
            }
        };
        let Some(inode) = inode else {
            return xattr_answer(op);
        };
        let value;
        let request = match (op, name) {
            (XattrOp::Get, Some(name)) => XattrRequest::Get(name),
            (XattrOp::Set { flags, .. }, Some(name)) => {
                value = UserInPtr::<u8>::from(buf).read_array(size)?;
                XattrRequest::Set {
                    name,
                    value: &value,
                    flags: flags as u32,
                }
            }
            (XattrOp::Remove, Some(name)) => XattrRequest::Remove(name),
            _ => XattrRequest::List,
        };
        let euid = proc.euid();
        let admin = proc.capable(CAP_SYS_ADMIN);
        match linux_object::fs::eclipsefs_xattr(&inode, &path, request, euid, admin) {
            None => xattr_answer(op),
            Some(reply) => match reply? {
                XattrReply::Value(bytes) | XattrReply::Names(bytes) => {
                    xattr_copy_out(&bytes, buf, size)
                }
                XattrReply::Done => Ok(0),
            },
        }
    }
}

/// `CAP_SYS_ADMIN`, which `trusted.*` attributes need.
const CAP_SYS_ADMIN: u32 = 21;

/// What `getxattr` and `listxattr` return: with `size` 0 only the length
/// the value or list needs, `ERANGE` when the buffer is too small for it,
/// else the bytes copied.
fn xattr_copy_out(bytes: &[u8], buf: usize, size: usize) -> SysResult {
    if size == 0 {
        return Ok(bytes.len());
    }
    if size < bytes.len() {
        return Err(LxError::ERANGE);
    }
    UserOutPtr::<u8>::from(buf).write_array(bytes)?;
    Ok(bytes.len())
}

/// The twelve, each one made by number through the dispatch table: which file
/// each number names (the `l*` forms differ by one `bool`, and only a dangling
/// symlink can tell them apart), the order the arguments are judged in, and
/// which of the two user buffers each call touches.
#[cfg(test)]
mod wiring_tests;

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

    /// The two limits are the numbers the uapi gives, and not whatever this
    /// file happens to say they are.
    ///
    /// Both tests above are written in terms of the constants
    /// (`"u".repeat(XATTR_NAME_MAX)`, `set(XATTR_SIZE_MAX + 1)`), so they
    /// hold for any value at all: 255 changed to 256, or 65536 to 65535,
    /// left the whole module green. The numbers are part of the ABI --
    /// userspace sizes its own buffers by them -- so they are pinned here by
    /// number, and the boundary is counted out by hand rather than named.
    #[test]
    fn the_two_limits_are_the_numbers_the_uapi_gives() {
        // `include/uapi/linux/limits.h`.
        assert_eq!(XATTR_NAME_MAX, 255);
        assert_eq!(XATTR_SIZE_MAX, 65536);
        // `XATTR_CREATE | XATTR_REPLACE`, the only two `setxattr` knows.
        assert_eq!(XATTR_FLAGS_KNOWN, 0b11);

        // The name's edge: 255 characters is a name, 256 is not.
        let name_255 = "u".repeat(255);
        let name_256 = "u".repeat(256);
        assert_eq!(xattr_precheck(XattrOp::Get, Some(&name_255)), Ok(()));
        assert_eq!(
            xattr_precheck(XattrOp::Get, Some(&name_256)).err(),
            Some(LxError::ERANGE)
        );

        // And the value's: 65536 bytes is a value, 65537 is `E2BIG`.
        let set = |size| XattrOp::Set { size, flags: 0 };
        assert_eq!(xattr_precheck(set(65536), Some("user.a")), Ok(()));
        assert_eq!(
            xattr_precheck(set(65537), Some("user.a")).err(),
            Some(LxError::E2BIG)
        );
    }
}

#[cfg(test)]
mod benches {
    //! The twelve xattr syscalls answer from a table in this kernel -- no
    //! filesystem here keeps extended attributes -- so what they cost IS
    //! `xattr_precheck` and `xattr_answer`, and nothing else. That makes
    //! these rows the whole of `getxattr`, minus the path lookup.
    //!
    //! It also makes the order observable: `fs/xattr.c` judges the flags,
    //! then the name, then the value, and these helpers keep that order
    //! because a caller that is wrong twice over must hear the same reason
    //! Linux gives.
    //!
    //! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
    //! own loop; a row there with an opaque input is a real compare and
    //! branch, a row there with a constant input is a call that was folded
    //! away and never ran.

    use super::*;
    use test::{black_box, Bencher};

    /// A `user.` name of the length a real attribute has.
    const NAME: &str = "user.selinux.context";

    /// `getxattr`'s check: a name to measure and nothing else.
    #[bench]
    fn precheck_get(b: &mut Bencher) {
        b.iter(|| {
            black_box(xattr_precheck(
                black_box(XattrOp::Get),
                black_box(Some(NAME)),
            ))
        })
    }

    /// `setxattr`'s: the flags word, the name, then the value length -- all
    /// three, which is the longest path through this function.
    #[bench]
    fn precheck_set(b: &mut Bencher) {
        b.iter(|| {
            black_box(xattr_precheck(
                black_box(XattrOp::Set { size: 64, flags: 1 }),
                black_box(Some(NAME)),
            ))
        })
    }

    /// `listxattr`, which has none of the three: the floor of this family,
    /// and what the dispatcher used to answer for all twelve.
    #[bench]
    fn precheck_list(b: &mut Bencher) {
        b.iter(|| black_box(xattr_precheck(black_box(XattrOp::List), black_box(None))))
    }

    /// A flags word with a bit neither `XATTR_CREATE` nor `XATTR_REPLACE`:
    /// refused first, before the name is looked at.
    #[bench]
    fn precheck_set_with_unknown_flags(b: &mut Bencher) {
        b.iter(|| {
            black_box(xattr_precheck(
                black_box(XattrOp::Set { size: 64, flags: 4 }),
                black_box(Some(NAME)),
            ))
        })
    }

    /// A name one byte over `XATTR_NAME_MAX`, which is `ERANGE`. The check is
    /// a length compare, not a walk, so it costs the same as a short name --
    /// the row exists to show that.
    #[bench]
    fn precheck_name_too_long(b: &mut Bencher) {
        let long = alloc::string::String::from_utf8(alloc::vec![b'x'; XATTR_NAME_MAX + 1]).unwrap();
        b.iter(|| {
            black_box(xattr_precheck(
                black_box(XattrOp::Get),
                black_box(Some(long.as_str())),
            ))
        })
    }

    /// The empty name, which is `ERANGE` too: `getxattr(path, "", ...)`.
    #[bench]
    fn precheck_empty_name(b: &mut Bencher) {
        b.iter(|| black_box(xattr_precheck(black_box(XattrOp::Get), black_box(Some("")))))
    }

    /// A value over `XATTR_SIZE_MAX`, which is `E2BIG` and is judged last.
    #[bench]
    fn precheck_value_too_big(b: &mut Bencher) {
        b.iter(|| {
            black_box(xattr_precheck(
                black_box(XattrOp::Set {
                    size: XATTR_SIZE_MAX + 1,
                    flags: 0,
                }),
                black_box(Some(NAME)),
            ))
        })
    }

    /// The answer itself, once the file is known to exist.
    #[bench]
    fn answer_get(b: &mut Bencher) {
        b.iter(|| black_box(xattr_answer(black_box(XattrOp::Get))))
    }

    #[bench]
    fn answer_list(b: &mut Bencher) {
        b.iter(|| black_box(xattr_answer(black_box(XattrOp::List))))
    }
}
