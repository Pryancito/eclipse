//! Where `sendfile(2)` and `copy_file_range(2)` put their arguments.
//!
//! `copy_range`, the loop both of them end in, has `copy_range_tests`
//! beside this file, and `copy_file_range_flags` and `user_offset_end` have
//! tests of their own. What none of them can see is the **wiring**, which
//! is where the `chown` holes were:
//!
//! - `sendfile` is nothing but a call to `copy_file_range` with its
//!   arguments moved around, and its whole contract is **which** of them
//!   goes where: the offset it is given belongs to the **input**, and the
//!   output is written at its own cursor. Swap the two descriptors and a
//!   web server writes its page back over the file it was serving.
//! - an explicit offset means "do not touch the cursor, tell me where you
//!   got to instead", so the number has to come back **in the caller's
//!   variable**. A copy that moved the bytes and wrote nothing back leaves
//!   the next call reading the same bytes again.
//! - both ends are checked for a window that walks off the end of `off_t`,
//!   each by its own call, and the two errnos are different (`EINVAL` for a
//!   negative position, `EOVERFLOW` for one that wraps) -- so a missing
//!   check on one end is invisible from the other.
//! - the output does not have to be a `File`. That is `sendfile`'s reason
//!   to exist, and asking `get_file` instead of `get_file_like` answered
//!   `EBADF` to every socket.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{EventFd, File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// A descriptor number this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// A position userspace wrote as `-1`: `EINVAL`, not a position sixteen
/// exabytes in.
const A_NEGATIVE_OFFSET: u64 = -1_i64 as u64;

/// Ten bytes whose every position is its own digit.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` called `name` holding `content`, and the descriptor
/// it got, opened as `flags` says.
fn a_file_with(
    lp: &LinuxProcess,
    dir_path: &str,
    name: &str,
    content: &[u8],
    flags: OpenFlags,
) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    if !content.is_empty() {
        inode.resize(content.len()).unwrap();
        inode.write_at(0, content).unwrap();
    }
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp.add_file(File::new(inode, flags, path)).unwrap().into();
    fd as usize
}

/// Where the descriptor's own cursor is, asked of the description and not
/// of the syscalls under test.
fn cursor_of(lp: &LinuxProcess, fd: usize) -> u64 {
    lp.get_file(FileDesc::from(fd))
        .unwrap()
        .seek(SeekFrom::Current(0))
        .unwrap()
}

/// What the file really holds, same source.
fn bytes_of(lp: &LinuxProcess, path: &str, n: usize) -> alloc::vec::Vec<u8> {
    let inode = lp.lookup_inode(path).unwrap();
    let mut buf = alloc::vec![0xffu8; n];
    let read = inode.read_at(0, &mut buf).unwrap();
    buf.truncate(read);
    buf
}

/// A pointer to one of this test's `u64`s, as userspace passes an
/// `off_t *`. Built per call, because `UserInOutPtr` is not `Copy`.
fn at(off: &mut u64) -> UserInOutPtr<u64> {
    UserInOutPtr::from(off as *mut u64 as usize)
}

/// The null `off_t *`, which means "use the descriptor's own cursor".
fn no_offset() -> UserInOutPtr<u64> {
    UserInOutPtr::from(0)
}

#[test]
fn sendfile_gives_the_offset_to_the_input_and_the_cursor_to_the_output() {
    // All of `sendfile` is this: `copy_file_range(in, offset, out, NULL,
    // count, 0)`. So the test has to be able to tell each of those four
    // places apart from the others, and it is built so that it can:
    //
    // - the two files hold different bytes, so writing the wrong way round
    //   is visible in both of them;
    // - the input's cursor sits at 7, nowhere near the offset asked for or
    //   the output's cursor, so a kernel that read from the cursor instead
    //   of the offset reads different bytes;
    // - the offset asked for is 2 and the count 4, so the bytes that land
    //   name their own position.
    let proc = a_process(45_800, "sendfile-map", "sendfile_map");
    let dir_path = String::from("/sendfile_map");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[], OpenFlags::RDWR);
            lp.get_file(FileDesc::from(src))
                .unwrap()
                .seek(SeekFrom::Start(7))
                .unwrap();

            let mut off = 2u64;
            assert_eq!(
                sys.sys_sendfile(FileDesc::from(dst), FileDesc::from(src), at(&mut off), 4)
                    .await,
                Ok(4),
                "sendfile did not copy the four bytes it was asked for"
            );
            assert_eq!(
                bytes_of(&lp, &alloc::format!("{}/dst", dir_path), 10),
                b"2345".to_vec(),
                "sendfile read from somewhere other than the offset it was given"
            );
            assert_eq!(
                bytes_of(&lp, &alloc::format!("{}/src", dir_path), 10),
                TEN_DIGITS.to_vec(),
                "sendfile wrote into its input"
            );
            assert_eq!(
                off, 6,
                "the offset did not come back advanced by what moved"
            );
            assert_eq!(
                cursor_of(&lp, src),
                7,
                "an explicit offset moved the input's cursor"
            );
            assert_eq!(
                cursor_of(&lp, dst),
                4,
                "the output was not written at its own cursor"
            );
        })
    });
}

#[test]
fn an_explicit_offset_lands_in_the_callers_variable_and_leaves_the_cursor_alone() {
    // `copy_file_range` with both offsets given. Three numbers that cannot
    // be each other: read from 1, write at 5, copy 3. The destination is
    // ten bytes of `z`, so a write at the wrong position is visible, and
    // both variables have to come back advanced by exactly what moved.
    let proc = a_process(45_801, "copy-explicit", "copy_explicit");
    let dir_path = String::from("/copy_explicit");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[b'z'; 10], OpenFlags::RDWR);

            let mut in_off = 1u64;
            let mut out_off = 5u64;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut in_off),
                    FileDesc::from(dst),
                    at(&mut out_off),
                    3,
                    0,
                )
                .await,
                Ok(3)
            );
            assert_eq!(
                bytes_of(&lp, &alloc::format!("{}/dst", dir_path), 10),
                b"zzzzz123zz".to_vec(),
                "the bytes did not land where the output offset said"
            );
            assert_eq!(in_off, 4, "the input offset did not come back advanced");
            assert_eq!(out_off, 8, "the output offset did not come back advanced");
            assert_eq!(cursor_of(&lp, src), 0, "an explicit offset moved a cursor");
            assert_eq!(cursor_of(&lp, dst), 0, "an explicit offset moved a cursor");
        })
    });
}

#[test]
fn a_null_offset_walks_the_cursor_on_both_ends() {
    // The other half of the same decision, and the default for `sendfile`'s
    // output: a null `off_t *` means the descriptor's own cursor, which the
    // copy both starts from and advances. Asked after a first copy has
    // already moved them, so "starts from the cursor" is a claim about a
    // cursor that is not zero.
    let proc = a_process(45_802, "copy-cursor", "copy_cursor");
    let dir_path = String::from("/copy_cursor");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[], OpenFlags::RDWR);

            for (round, expected) in [(1usize, b"0123".to_vec()), (2, b"01234567".to_vec())] {
                assert_eq!(
                    sys.sys_copy_file_range(
                        FileDesc::from(src),
                        no_offset(),
                        FileDesc::from(dst),
                        no_offset(),
                        4,
                        0,
                    )
                    .await,
                    Ok(4),
                    "round {} of a copy from the cursor",
                    round
                );
                assert_eq!(
                    bytes_of(&lp, &alloc::format!("{}/dst", dir_path), 10),
                    expected,
                    "round {} did not resume where the last one stopped",
                    round
                );
                assert_eq!(cursor_of(&lp, src), (round * 4) as u64);
                assert_eq!(cursor_of(&lp, dst), (round * 4) as u64);
            }
        })
    });
}

#[test]
fn a_flag_this_kernel_does_not_define_is_einval_before_the_descriptors_are_looked_at() {
    // `copy_file_range(2)` defines no flags yet and Linux reserves the
    // word, so the day one means something a program can tell a kernel
    // that honoured it from a kernel that dropped it. Asked against the
    // same call with a flag of zero, which gets as far as the descriptors,
    // so the `EINVAL` is the word and not the call.
    let proc = a_process(45_803, "copy-flag", "copy_flag");
    let dir_path = String::from("/copy_flag");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[], OpenFlags::RDWR);

            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(A_CLOSED_FD),
                    no_offset(),
                    FileDesc::from(A_CLOSED_FD),
                    no_offset(),
                    1,
                    1,
                )
                .await,
                Err(LxError::EINVAL),
                "the flag word was read after the descriptors"
            );
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(A_CLOSED_FD),
                    no_offset(),
                    FileDesc::from(A_CLOSED_FD),
                    no_offset(),
                    1,
                    0,
                )
                .await,
                Err(LxError::EBADF),
                "the descriptors this test relies on being closed were open"
            );

            // And on a pair that would have worked: nothing moves.
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    no_offset(),
                    FileDesc::from(dst),
                    no_offset(),
                    4,
                    1,
                )
                .await,
                Err(LxError::EINVAL)
            );
            assert!(
                bytes_of(&lp, &alloc::format!("{}/dst", dir_path), 10).is_empty(),
                "a refused call copied something anyway"
            );
            assert_eq!(cursor_of(&lp, src), 0);
        })
    });
}

#[test]
fn a_negative_offset_is_einval_on_either_end_and_nothing_is_copied() {
    // Each end calls `user_offset_end` for itself, so a missing check on
    // one of them is invisible from the other: both are asked here, and
    // each with the other end's offset perfectly good.
    let proc = a_process(45_804, "copy-neg", "copy_neg");
    let dir_path = String::from("/copy_neg");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[], OpenFlags::RDWR);
            let dst_path = alloc::format!("{}/dst", dir_path);

            let mut bad = A_NEGATIVE_OFFSET;
            let mut good = 0u64;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut bad),
                    FileDesc::from(dst),
                    at(&mut good),
                    4,
                    0,
                )
                .await,
                Err(LxError::EINVAL),
                "a negative input offset was taken"
            );
            assert!(bytes_of(&lp, &dst_path, 10).is_empty());

            let mut good = 0u64;
            let mut bad = A_NEGATIVE_OFFSET;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut good),
                    FileDesc::from(dst),
                    at(&mut bad),
                    4,
                    0,
                )
                .await,
                Err(LxError::EINVAL),
                "a negative output offset was taken"
            );
            assert!(
                bytes_of(&lp, &dst_path, 10).is_empty(),
                "a refused call copied something anyway"
            );
        })
    });
}

#[test]
fn a_window_that_walks_past_off_t_is_eoverflow_and_copying_everything_from_zero_is_not() {
    // `user_offset_end` is not a ceiling on the offset: it is Linux's wrap
    // test on `offset + count`, and the two halves of that distinction are
    // both here. A count with its top bit set from position 1 wraps, which
    // is `EOVERFLOW` and not `EINVAL` -- a caller told `EINVAL` looks at
    // its offset, which is fine. The same count from position 0 does not
    // wrap, and `sendfile(out, in, NULL, SIZE_MAX)` is the "copy the lot"
    // idiom that Linux accepts and clamps, so rejecting it here would
    // break a program that works there.
    let proc = a_process(45_805, "copy-ovf", "copy_ovf");
    let dir_path = String::from("/copy_ovf");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            let dst = a_file_with(&lp, &dir_path, "dst", &[], OpenFlags::RDWR);

            let mut one = 1u64;
            let mut zero = 0u64;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut one),
                    FileDesc::from(dst),
                    at(&mut zero),
                    usize::MAX,
                    0,
                )
                .await,
                Err(LxError::EOVERFLOW),
                "an input window that wraps was taken"
            );

            let mut zero = 0u64;
            let mut one = 1u64;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut zero),
                    FileDesc::from(dst),
                    at(&mut one),
                    usize::MAX,
                    0,
                )
                .await,
                Err(LxError::EOVERFLOW),
                "an output window that wraps was taken"
            );

            let mut from_zero = 0u64;
            assert_eq!(
                sys.sys_sendfile(
                    FileDesc::from(dst),
                    FileDesc::from(src),
                    at(&mut from_zero),
                    usize::MAX,
                )
                .await,
                Ok(10),
                "the copy-everything idiom was refused"
            );
            assert_eq!(from_zero, 10);
            assert_eq!(
                bytes_of(&lp, &alloc::format!("{}/dst", dir_path), 16),
                TEN_DIGITS.to_vec()
            );
        })
    });
}

#[test]
fn the_offsets_are_judged_before_a_descriptor_opened_the_wrong_way_round() {
    // Both answers are about arguments the caller passed, so which one it
    // gets is an order, and the order is the one `copy_file_range` has:
    // the window first, the access mode of the descriptions after. The
    // same pair gives both answers here, so nothing but the offset can
    // account for the difference.
    let proc = a_process(45_806, "copy-order", "copy_order");
    let dir_path = String::from("/copy_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let src = a_file_with(&lp, &dir_path, "src", TEN_DIGITS, OpenFlags::RDWR);
            // Open for reading only: a perfectly good descriptor that may
            // not be written through.
            let read_only = a_file_with(&lp, &dir_path, "ro", &[], OpenFlags::RDONLY);

            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    no_offset(),
                    FileDesc::from(read_only),
                    no_offset(),
                    4,
                    0,
                )
                .await,
                Err(LxError::EBADF),
                "a copy into a read-only description was allowed"
            );

            let mut bad = A_NEGATIVE_OFFSET;
            assert_eq!(
                sys.sys_copy_file_range(
                    FileDesc::from(src),
                    at(&mut bad),
                    FileDesc::from(read_only),
                    no_offset(),
                    4,
                    0,
                )
                .await,
                Err(LxError::EINVAL),
                "the access mode was judged before the window"
            );
        })
    });
}

#[test]
fn an_output_that_is_not_a_file_is_the_whole_point_of_sendfile() {
    // `sendfile(2)` exists so that a server can push a file at a socket
    // without the bytes going through its own address space, and a socket
    // is not a `File`. Asking the process for a `File` here answered
    // `EBADF` to every one of those calls.
    //
    // An eventfd stands in for the socket: it is a `FileLike` that is not
    // a `File`, it is open for reading and writing, and what it does with
    // eight bytes is something this test can read back -- they are a
    // counter, so the number that comes out proves the bytes arrived and
    // arrived in order.
    let proc = a_process(45_807, "sendfile-sock", "sendfile_sock");
    let dir_path = String::from("/sendfile_sock");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let five = 5u64.to_ne_bytes();
            let src = a_file_with(&lp, &dir_path, "src", &five, OpenFlags::RDWR);
            let not_a_file: i32 = lp
                .add_file(EventFd::new(0, OpenFlags::RDWR))
                .unwrap()
                .into();
            let not_a_file = not_a_file as usize;
            assert!(
                lp.get_file(FileDesc::from(not_a_file)).is_err(),
                "the stand-in for a socket turned out to be a File"
            );

            let mut off = 0u64;
            assert_eq!(
                sys.sys_sendfile(
                    FileDesc::from(not_a_file),
                    FileDesc::from(src),
                    at(&mut off),
                    8,
                )
                .await,
                Ok(8),
                "sendfile refused an output that is not a File"
            );
            assert_eq!(off, 8);

            let mut back = [0u8; 8];
            let addr = back.as_mut_ptr() as usize;
            assert_eq!(
                sys.sys_read(FileDesc::from(not_a_file), addr.into(), 8)
                    .await,
                Ok(8)
            );
            assert_eq!(
                u64::from_ne_bytes(back),
                5,
                "the bytes that arrived were not the bytes that were sent"
            );
        })
    });
}
