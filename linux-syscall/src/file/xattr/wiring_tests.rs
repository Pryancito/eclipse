//! The twelve extended-attribute syscalls, each one made the way userspace
//! makes it: **by number, through the dispatch table**.
//!
//! `xattr_tests` next door decides every verdict these twelve take, and it
//! calls the two helpers directly (`xattr_precheck`, `xattr_answer`). That
//! leaves the whole of the wiring unasked, and here the wiring is most of
//! the call: `sys_xattr` takes the operation and the target as *arguments*,
//! so which of the twelve numbers builds which pair is decided in the
//! table and nowhere else.
//!
//! Four things only a syscall can show:
//!
//! 1. **Which file each number names.** The `l*` forms differ from the
//!    plain ones by a single `bool`, and the only thing that can tell them
//!    apart is a symlink: with a dangling one, `getxattr` follows it to
//!    nothing (`ENOENT`) and `lgetxattr` stops at the link (the answer of
//!    a filesystem without attributes). A table with the two `bool`s
//!    swapped passes every pure test there is.
//! 2. **The order.** `fs/xattr.c` reads the name and the flags before it
//!    looks the file up, so a path that is not there, asked with a name
//!    that is too long, is `ERANGE` and not `ENOENT`. The twelve used to
//!    be answered from the dispatcher without reading an argument at all.
//! 3. **That `listxattr` has no name.** Its buffer is the SECOND machine
//!    word where `getxattr`'s is the third, which is the one place in this
//!    family where two calls of the same shape take their arguments in
//!    different places.
//! 4. **What a descriptor with no inode behind it gets.** An eventfd is a
//!    file-like and not a `File`, so the lookup yields nothing to ask, and
//!    the answer has to be the no-attributes one rather than `EBADF` or a
//!    panic.
//!
//! Fixed pids live in 48_10x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `XATTR_CREATE`, and a bit `setxattr` does not know.
const XATTR_CREATE: usize = 1;
const A_SETXATTR_FLAG_OF_NOBODY: usize = 4;

/// A descriptor nobody opened, and the one every `f*` form is asked with
/// first.
const A_FD_OF_NOBODY: usize = 90;
const NOT_A_FD: usize = -1i32 as usize;

/// A value small enough that nothing refuses it for its size.
const A_SHORT_VALUE: &[u8] = b"yes";

/// A process with a `/x` of its own, because a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create("x", FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A path or a name as userspace writes one: NUL-terminated.
fn c(s: &str) -> Vec<u8> {
    let mut bytes = Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// Where userspace would say that lives.
fn at(bytes: &[u8]) -> usize {
    bytes.as_ptr() as usize
}

/// A file of `/x`, which is all these calls need of it.
fn a_file_named(lp: &LinuxProcess, name: &str) {
    let dir = lp.lookup_inode("/x").unwrap();
    dir.create(name, FileType::File, 0o644).unwrap();
}

/// A symbolic link of `/x` pointing at `to`, read relative to `/x`.
fn a_link_named(lp: &LinuxProcess, name: &str, to: &str) {
    let dir = lp.lookup_inode("/x").unwrap();
    let inode = dir.create(name, FileType::SymLink, 0o777).unwrap();
    inode.write_at(0, to.as_bytes()).unwrap();
}

/// The same `Syscall` with a mutable borrow, which the dispatch table needs.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A syscall made the way userspace makes it: by number, through the table.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    by_raw(sys, which as u32, args).await
}

/// The same, with the number already in hand: `Sys` is neither `Copy` nor
/// `Clone`, so a test that makes the same call twice keeps the number.
async fn by_raw(sys: &Syscall<'_>, num: u32, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(num, args).await
}

/// What an errno looks like on the way back out of the table.
fn errno(e: LxError) -> isize {
    -(e as isize)
}

/// An open descriptor of a file of `/x`, which is what the `f*` forms want.
fn opened(sys: &Syscall<'_>, path: &[u8]) -> usize {
    sys.sys_openat(FileDesc::CWD, UserInPtr::from(at(path)), 0, 0)
        .expect("a file that is there opens read-only")
}

#[test]
fn the_name_and_the_flags_are_judged_before_the_file_is_looked_up() {
    // `fs/xattr.c` reads the name and the flags word first, looks the file
    // up second, and only then asks the filesystem. The order is visible
    // because each stage has an errno of its own: with a path that is not
    // there, a refusal that is not ENOENT can only have come from before
    // the lookup.
    let proc = a_process(48_100, "xattr-order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nowhere = c("/x/nothing-here");
            let good = c("user.one");
            let empty = c("");
            let mut long = Vec::from(&b"user."[..]);
            long.resize(256 + 5, b'a');
            long.push(0);

            // A name that is empty or past XATTR_NAME_MAX, on a path that
            // does not exist: ERANGE, from before the lookup.
            for name in [&empty, &long] {
                assert_eq!(
                    by_number(
                        sys,
                        crate::Sys::GETXATTR,
                        [at(&nowhere), at(name), 0, 0, 0, 0]
                    )
                    .await,
                    errno(LxError::ERANGE),
                    "the file was looked up before the name was read"
                );
            }

            // A flags word with a bit nobody defines: EINVAL, also from
            // before the lookup.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SETXATTR,
                    [
                        at(&nowhere),
                        at(&good),
                        at(A_SHORT_VALUE),
                        A_SHORT_VALUE.len(),
                        A_SETXATTR_FLAG_OF_NOBODY,
                        0
                    ]
                )
                .await,
                errno(LxError::EINVAL),
                "the file was looked up before the flags were read"
            );

            // A value past XATTR_SIZE_MAX: E2BIG, and the value itself is
            // never read, so no buffer has to exist for it.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SETXATTR,
                    [at(&nowhere), at(&good), 0, 65537, 0, 0]
                )
                .await,
                errno(LxError::E2BIG),
                "a value too big to take was read anyway"
            );

            // And with all three in order, the lookup is reached and it is
            // the lookup that answers.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::GETXATTR,
                    [at(&nowhere), at(&good), 0, 0, 0, 0]
                )
                .await,
                errno(LxError::ENOENT),
                "a path that is not there was answered by the filesystem"
            );
        })
    });
}

#[test]
fn a_path_that_is_not_there_is_enoent_in_every_path_form() {
    // What this family answered before it read its arguments: `listxattr`
    // was 0, `getxattr` and `removexattr` ENODATA, `setxattr` EOPNOTSUPP
    // -- the answers of a file that EXISTS on a filesystem without
    // attributes. A program that uses getxattr to probe for a file (and
    // `cp -a`, `rsync -X` and `tar --xattrs` all walk paths that can go
    // away underneath them) could not tell the two apart.
    let proc = a_process(48_101, "xattr-enoent");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nowhere = c("/x/nothing-here");
            let name = c("user.one");
            let gone = at(&nowhere);
            let named = at(&name);

            for which in [
                crate::Sys::GETXATTR,
                crate::Sys::LGETXATTR,
                crate::Sys::REMOVEXATTR,
                crate::Sys::LREMOVEXATTR,
            ] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(sys, which, [gone, named, 0, 0, 0, 0]).await,
                    errno(LxError::ENOENT),
                    "{} answered for a file that is not there",
                    named_it
                );
            }
            for which in [crate::Sys::SETXATTR, crate::Sys::LSETXATTR] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(
                        sys,
                        which,
                        [gone, named, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                    )
                    .await,
                    errno(LxError::ENOENT),
                    "{} answered for a file that is not there",
                    named_it
                );
            }
            // `listxattr` has no name: its buffer is the second word.
            for which in [crate::Sys::LISTXATTR, crate::Sys::LLISTXATTR] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(sys, which, [gone, 0, 0, 0, 0, 0]).await,
                    errno(LxError::ENOENT),
                    "{} answered for a file that is not there",
                    named_it
                );
            }
        })
    });
}

#[test]
fn the_l_forms_stop_at_the_final_symlink_and_the_others_follow_it() {
    // The only difference between `getxattr` and `lgetxattr` here is one
    // `bool` in the table, and a filesystem without attributes gives the
    // same answer for every file, so the ONLY input that can tell the six
    // pairs apart is a link that points at nothing: following it is
    // ENOENT, stopping at it is the no-attributes answer. With the two
    // `bool`s swapped, `cp -a` would read the attributes of the link's
    // target where it meant the link, and every pure test would pass.
    let proc = a_process(48_102, "xattr-links");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_link_named(sys.linux_process(), "dangling", "nothing-here");
            let link = c("/x/dangling");
            let name = c("user.one");
            let l = at(&link);
            let n = at(&name);

            // Following the link reaches nothing.
            assert_eq!(
                by_number(sys, crate::Sys::GETXATTR, [l, n, 0, 0, 0, 0]).await,
                errno(LxError::ENOENT)
            );
            assert_eq!(
                by_number(sys, crate::Sys::REMOVEXATTR, [l, n, 0, 0, 0, 0]).await,
                errno(LxError::ENOENT)
            );
            assert_eq!(
                by_number(sys, crate::Sys::LISTXATTR, [l, 0, 0, 0, 0, 0]).await,
                errno(LxError::ENOENT)
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SETXATTR,
                    [l, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                )
                .await,
                errno(LxError::ENOENT)
            );

            // Stopping at the link reaches the link, which is a file like
            // any other to a filesystem with no attributes to give.
            assert_eq!(
                by_number(sys, crate::Sys::LGETXATTR, [l, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA),
                "lgetxattr followed the link"
            );
            assert_eq!(
                by_number(sys, crate::Sys::LREMOVEXATTR, [l, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA),
                "lremovexattr followed the link"
            );
            assert_eq!(
                by_number(sys, crate::Sys::LLISTXATTR, [l, 0, 0, 0, 0, 0]).await,
                0,
                "llistxattr followed the link"
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::LSETXATTR,
                    [l, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                )
                .await,
                errno(LxError::EOPNOTSUPP),
                "lsetxattr followed the link"
            );

            // A link that points at something real answers the same either
            // way, which is why the dangling one is the test.
            a_file_named(sys.linux_process(), "real");
            a_link_named(sys.linux_process(), "good", "real");
            let good = c("/x/good");
            for which in [crate::Sys::GETXATTR, crate::Sys::LGETXATTR] {
                assert_eq!(
                    by_number(sys, which, [at(&good), n, 0, 0, 0, 0]).await,
                    errno(LxError::ENODATA)
                );
            }
        })
    });
}

#[test]
fn a_descriptor_that_is_not_one_is_ebadf_in_every_f_form() {
    // The `f*` forms take the descriptor in the FIRST machine word, where
    // the path forms take a pointer, and a descriptor is looked up after
    // the name and the flags, like a path.
    let proc = a_process(48_103, "xattr-badfd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let name = c("user.one");
            let n = at(&name);

            for fd in [NOT_A_FD, A_FD_OF_NOBODY] {
                assert_eq!(
                    by_number(sys, crate::Sys::FGETXATTR, [fd, n, 0, 0, 0, 0]).await,
                    errno(LxError::EBADF)
                );
                assert_eq!(
                    by_number(sys, crate::Sys::FREMOVEXATTR, [fd, n, 0, 0, 0, 0]).await,
                    errno(LxError::EBADF)
                );
                assert_eq!(
                    by_number(sys, crate::Sys::FLISTXATTR, [fd, 0, 0, 0, 0, 0]).await,
                    errno(LxError::EBADF)
                );
                assert_eq!(
                    by_number(
                        sys,
                        crate::Sys::FSETXATTR,
                        [fd, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                    )
                    .await,
                    errno(LxError::EBADF)
                );
            }

            // And a descriptor that is not one is still judged after the
            // name, as a path is.
            let empty = c("");
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::FGETXATTR,
                    [NOT_A_FD, at(&empty), 0, 0, 0, 0]
                )
                .await,
                errno(LxError::ERANGE),
                "the descriptor was looked up before the name was read"
            );
        })
    });
}

#[test]
fn a_descriptor_with_no_inode_behind_it_takes_the_no_attribute_answers() {
    // An eventfd is a file-like and not a `File`, so there is no inode to
    // ask and no path to name. That is not a reason to refuse: the call
    // falls back to the answer a filesystem without attributes gives, which
    // is what `fgetxattr` on a pipe answers on Linux.
    let proc = a_process(48_104, "xattr-anon");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = sys.sys_eventfd2(0, 0).expect("an eventfd always opens");
            let name = c("user.one");
            let n = at(&name);

            assert_eq!(
                by_number(sys, crate::Sys::FGETXATTR, [fd, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(sys, crate::Sys::FREMOVEXATTR, [fd, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(sys, crate::Sys::FLISTXATTR, [fd, 0, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::FSETXATTR,
                    [fd, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                )
                .await,
                errno(LxError::EOPNOTSUPP)
            );
        })
    });
}

#[test]
fn a_file_that_is_there_gets_the_answers_of_a_filesystem_without_attributes() {
    // RamFS keeps no attributes, so every one of the twelve reaches the
    // same four answers once the file is known to exist -- and reaching
    // them is the point: before, they were given without the file being
    // looked up at all.
    let proc = a_process(48_105, "xattr-plain");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "file");
            let path = c("/x/file");
            let name = c("user.one");
            let p = at(&path);
            let n = at(&name);
            let fd = opened(sys, &path);

            for (which, target) in [
                (crate::Sys::GETXATTR, p),
                (crate::Sys::LGETXATTR, p),
                (crate::Sys::FGETXATTR, fd),
            ] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(sys, which, [target, n, 0, 0, 0, 0]).await,
                    errno(LxError::ENODATA),
                    "{}",
                    named_it
                );
            }
            for (which, target) in [
                (crate::Sys::REMOVEXATTR, p),
                (crate::Sys::LREMOVEXATTR, p),
                (crate::Sys::FREMOVEXATTR, fd),
            ] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(sys, which, [target, n, 0, 0, 0, 0]).await,
                    errno(LxError::ENODATA),
                    "{}",
                    named_it
                );
            }
            // A list of no attributes is zero bytes long, with a buffer or
            // without one: nothing is copied, so nothing can be copied
            // wrong.
            let mut list = [0xaau8; 8];
            for (which, target) in [
                (crate::Sys::LISTXATTR, p),
                (crate::Sys::LLISTXATTR, p),
                (crate::Sys::FLISTXATTR, fd),
            ] {
                let named_it = format!("{:?}", which);
                let num = which as u32;
                assert_eq!(by_raw(sys, num, [target, 0, 0, 0, 0, 0]).await, 0);
                assert_eq!(
                    by_raw(
                        sys,
                        num,
                        [target, list.as_mut_ptr() as usize, list.len(), 0, 0, 0]
                    )
                    .await,
                    0,
                    "{} with a buffer",
                    named_it
                );
            }
            assert_eq!(list, [0xaa; 8], "an empty list wrote into the buffer");

            for (which, target) in [
                (crate::Sys::SETXATTR, p),
                (crate::Sys::LSETXATTR, p),
                (crate::Sys::FSETXATTR, fd),
            ] {
                let named_it = format!("{:?}", which);
                assert_eq!(
                    by_number(
                        sys,
                        which,
                        [
                            target,
                            n,
                            at(A_SHORT_VALUE),
                            A_SHORT_VALUE.len(),
                            XATTR_CREATE,
                            0
                        ]
                    )
                    .await,
                    errno(LxError::EOPNOTSUPP),
                    "{}",
                    named_it
                );
            }
        })
    });
}

#[test]
fn the_value_is_read_before_the_filesystem_answers_and_the_buffer_is_not() {
    // Which of the two user buffers each call touches, and when. `setxattr`
    // reads its value before the filesystem gets a say, so a value the
    // kernel cannot read is EFAULT and not EOPNOTSUPP. `getxattr` never
    // touches its buffer unless there is something to put in it, so the
    // same unreadable address is no error at all.
    let proc = a_process(48_106, "xattr-buffers");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "file");
            let path = c("/x/file");
            let name = c("user.one");
            let p = at(&path);
            let n = at(&name);

            assert_eq!(
                by_number(sys, crate::Sys::SETXATTR, [p, n, 0, 8, 0, 0]).await,
                errno(LxError::EFAULT),
                "setxattr answered without reading the value it was given"
            );
            assert_eq!(
                by_number(sys, crate::Sys::GETXATTR, [p, n, 0, 8, 0, 0]).await,
                errno(LxError::ENODATA),
                "getxattr wrote to a buffer it had nothing for"
            );
            assert_eq!(
                by_number(sys, crate::Sys::LISTXATTR, [p, 0, 8, 0, 0, 0]).await,
                0,
                "listxattr wrote to a buffer it had nothing for"
            );

            // A name the kernel cannot read is the caller's fault, and it
            // is read before anything else happens.
            assert_eq!(
                by_number(sys, crate::Sys::GETXATTR, [p, 0, 0, 0, 0, 0]).await,
                errno(LxError::EFAULT)
            );
            assert_eq!(
                by_number(sys, crate::Sys::SETXATTR, [p, 0, 0, 0, 0, 0]).await,
                errno(LxError::EFAULT)
            );
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // Twelve numbers, and the pairs that would be invisible to a test that
    // only read one errno at a time: a path swapped with a name, a
    // descriptor swapped with a name, a size swapped with a flags word,
    // and `listxattr`'s buffer, which sits one word earlier than
    // `getxattr`'s because there is no name in front of it.
    //
    // Each one is asked in two shapes whose answers differ, so a swap
    // turns one of them into the other's.
    let proc = a_process(48_107, "xattr-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "file");
            let path = c("/x/file");
            let name = c("user.one");
            let empty = c("");
            let p = at(&path);
            let n = at(&name);
            let fd = opened(sys, &path);

            // path, then name: with the name empty it is ERANGE, with the
            // two swapped the path would be read as a name (and would be
            // neither empty nor too long) and the answer would be the
            // file's.
            assert_eq!(
                by_number(sys, crate::Sys::GETXATTR, [p, at(&empty), 0, 0, 0, 0]).await,
                errno(LxError::ERANGE)
            );
            assert_eq!(
                by_number(sys, crate::Sys::GETXATTR, [p, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );

            // descriptor, then name: swapped, the name's address would be
            // the descriptor and the answer EBADF.
            assert_eq!(
                by_number(sys, crate::Sys::FGETXATTR, [fd, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(sys, crate::Sys::FLISTXATTR, [fd, 0, 0, 0, 0, 0]).await,
                0
            );

            // size, then flags: a flags word of 1 is XATTR_CREATE and a
            // size of 1 is an ordinary length, so with the two swapped the
            // unknown flag below would be read as a length and the EINVAL
            // would not come.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SETXATTR,
                    [p, n, at(A_SHORT_VALUE), 1, A_SETXATTR_FLAG_OF_NOBODY, 0]
                )
                .await,
                errno(LxError::EINVAL)
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SETXATTR,
                    [
                        p,
                        n,
                        at(A_SHORT_VALUE),
                        A_SHORT_VALUE.len(),
                        XATTR_CREATE,
                        0
                    ]
                )
                .await,
                errno(LxError::EOPNOTSUPP)
            );

            // `listxattr(path, list, size)`: the buffer is the SECOND word.
            // Asked with the path in the second, the first would be read as
            // a path and the call would answer for whatever is there -- so
            // the shape that must SUCCEED is the one that pins it.
            assert_eq!(
                by_number(sys, crate::Sys::LISTXATTR, [p, 0, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(sys, crate::Sys::LLISTXATTR, [p, 0, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(sys, crate::Sys::LISTXATTR, [0, p, 0, 0, 0, 0]).await,
                errno(LxError::EFAULT),
                "listxattr took its path from somewhere else"
            );

            // And the three `l*` path forms take their path in the first
            // word like the others.
            assert_eq!(
                by_number(sys, crate::Sys::LGETXATTR, [p, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(sys, crate::Sys::LREMOVEXATTR, [p, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::LSETXATTR,
                    [p, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                )
                .await,
                errno(LxError::EOPNOTSUPP)
            );
            assert_eq!(
                by_number(sys, crate::Sys::REMOVEXATTR, [p, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(sys, crate::Sys::FREMOVEXATTR, [fd, n, 0, 0, 0, 0]).await,
                errno(LxError::ENODATA)
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::FSETXATTR,
                    [fd, n, at(A_SHORT_VALUE), A_SHORT_VALUE.len(), 0, 0]
                )
                .await,
                errno(LxError::EOPNOTSUPP)
            );
        })
    });
}
