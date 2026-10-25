//! A file's length and its filesystem's numbers, asked through the syscalls.
//!
//! `truncate_type`, `ftruncate_type` and `grows_past_fsize` are pure and
//! have tests of their own beside this file. What none of them can see is
//! the **wiring**, which is where the `chown` holes were: whether each
//! syscall asks *its* gate (the two disagree about a directory, and about
//! permissions, on purpose), whether a refusal arrives before the length
//! changes, and whether the new length reaches the inode.
//!
//! The permission split is the sharp one. `truncate(2)` names a path, so it
//! asks for write permission on the file right then; `ftruncate(2)` names a
//! descriptor that was already opened for writing, so it asks for none --
//! which is what makes `>file` work in a shell after a `chmod 444`, and
//! what would hand out a free write to any file if `truncate` borrowed the
//! same answer.
//!
//! `statfs` and `fstatfs` do nothing but convert `FsInfo` and copy it out.
//! Eight numbers go into eight fields, and nothing in this tree had ever
//! read one of them back: `df` reporting a filesystem's free blocks as its
//! privileged-only reserve is a silent wrong answer, not a failure.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::{LinuxProcess, RLimit, RLIMIT_FSIZE};
use linux_object::signal::Signal;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// Somebody who is not root.
const A_USER: u32 = 1000;

/// A descriptor number this harness never hands out.
const A_CLOSED_FD: usize = 100;
/// What `statfs(2)` writes in `f_type` for the root of a test process: it is a
/// `RamFS` (`a_process`), and ramfs reports tmpfs's magic since `FsInfo` grew
/// `ftype`. A file and a directory of the same filesystem answer the same.
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// A length userspace wrote as `-1`. It is `EINVAL`, not an order to grow
/// the file to sixteen exabytes.
const A_NEGATIVE_LENGTH: usize = -1_i64 as usize;

/// Ten bytes whose every position is its own digit, so a length and the
/// bytes that survived a resize can be told apart at a glance.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A path as userspace passes one: NUL-terminated.
fn c(s: &str) -> alloc::vec::Vec<u8> {
    let mut bytes = alloc::vec::Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// A file of `dir_path` called `name` holding `content`, owned by `uid`
/// with exactly this mode, and the read-write descriptor it got.
fn a_file_named(
    lp: &LinuxProcess,
    dir_path: &str,
    name: &str,
    content: &[u8],
    uid: u32,
    mode: u16,
) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    inode.resize(content.len()).unwrap();
    inode.write_at(0, content).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// A descriptor open on a node of another kind, which both calls have to
/// refuse -- each with its own errno.
fn a_node_named(lp: &LinuxProcess, dir_path: &str, name: &str, type_: FileType) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, type_, 0o666).unwrap();
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// The length the file really has, asked of the filesystem and not of the
/// syscalls under test.
fn length_of(lp: &LinuxProcess, path: &str) -> usize {
    lp.lookup_inode(path).unwrap().metadata().unwrap().size
}

/// The first `n` bytes the file really holds, same source.
fn bytes_of(lp: &LinuxProcess, path: &str, n: usize) -> alloc::vec::Vec<u8> {
    let inode = lp.lookup_inode(path).unwrap();
    let mut buf = alloc::vec![0xffu8; n];
    let read = inode.read_at(0, &mut buf).unwrap();
    buf.truncate(read);
    buf
}

/// `truncate(path, len)` by the syscall.
fn truncate(sys: &Syscall<'_>, path: &[u8], len: usize) -> SysResult {
    // Built here and not shared: `UserInPtr` is not `Copy`.
    sys.sys_truncate(UserInPtr::from(path.as_ptr() as usize), len)
}

/// A `StatFs` nothing has written yet, filled with numbers no filesystem
/// here reports, so a buffer that was left alone can be told from one that
/// was written.
fn an_unwritten_statfs() -> StatFs {
    StatFs {
        f_type: 0x1234_5678,
        f_bsize: -1,
        f_blocks: 1,
        f_bfree: 1,
        f_bavail: 1,
        f_files: 1,
        f_ffree: 1,
        f_fsid: (-1, -1),
        f_namelen: -1,
        f_frsize: -1,
        f_flags: -1,
        f_spare: [-1; 4],
    }
}

#[test]
fn truncate_and_ftruncate_both_resize_the_file_and_a_regrown_tail_reads_as_zeros() {
    // The plain job of both calls, and the one thing a resize must never do:
    // hand back the bytes that were there before. A file cut to four bytes
    // and grown again to eight reads as four digits and four zeros, not as
    // the original eight digits -- otherwise `ftruncate` is a way to read
    // what a file used to hold after its owner shortened it.
    let proc = a_process(45_700, "trunc-resize", "trunc_resize");
    let dir_path = String::from("/trunc_resize");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", TEN_DIGITS, 0, 0o644);
            let path = alloc::format!("{}/f", dir_path);
            let name = c(&path);

            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(fd), 4),
                Ok(0),
                "ftruncate refused a shrink of a regular file"
            );
            assert_eq!(
                length_of(&lp, &path),
                4,
                "the shrink did not reach the inode"
            );
            assert_eq!(bytes_of(&lp, &path, 10), b"0123".to_vec());

            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(fd), 8),
                Ok(0),
                "ftruncate refused a grow of a regular file"
            );
            assert_eq!(length_of(&lp, &path), 8, "the grow did not reach the inode");
            assert_eq!(
                bytes_of(&lp, &path, 10),
                b"0123\0\0\0\0".to_vec(),
                "the bytes cut away came back when the file was grown again"
            );

            // And the same two by path, which is a different gate and a
            // different resize call reaching the same inode.
            assert_eq!(truncate(sys, &name, 2), Ok(0));
            assert_eq!(length_of(&lp, &path), 2);
            assert_eq!(truncate(sys, &name, 6), Ok(0));
            assert_eq!(
                bytes_of(&lp, &path, 10),
                b"01\0\0\0\0".to_vec(),
                "a tail regrown by truncate did not read as zeros"
            );
        })
    });
}

#[test]
fn a_directory_is_eisdir_to_truncate_and_einval_to_ftruncate() {
    // The two gates are not the same function and must not be swapped:
    // `do_sys_truncate` tells a directory apart (`EISDIR`) while
    // `do_sys_ftruncate` has one errno for every non-file (`EINVAL`).
    // Both gates have pure tests beside this file; that they are each
    // asked by the right syscall is only visible from here.
    let proc = a_process(45_701, "trunc-type", "trunc_type");
    let dir_path = String::from("/trunc_type");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let dirfd = a_node_named(&lp, &dir_path, "d", FileType::Dir);
            let chrfd = a_node_named(&lp, &dir_path, "chr", FileType::CharDevice);

            assert_eq!(
                truncate(sys, &c(&alloc::format!("{}/d", dir_path)), 0),
                Err(LxError::EISDIR),
                "truncate of a directory did not answer EISDIR"
            );
            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(dirfd), 0),
                Err(LxError::EINVAL),
                "ftruncate of a directory did not answer EINVAL"
            );

            // The other half of each gate: a node that is neither.
            assert_eq!(
                truncate(sys, &c(&alloc::format!("{}/chr", dir_path)), 0),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(chrfd), 0),
                Err(LxError::EINVAL)
            );
        })
    });
}

#[test]
fn a_negative_length_is_einval_before_the_path_or_the_descriptor_is_even_looked_at() {
    // `user_len` runs ahead of the lookup in both calls, so a caller who
    // passed `-1` is told about the length and not about the name. Asked
    // against the two errnos the same arguments produce with a length of
    // zero, so an `EINVAL` here cannot be a bad path or a bad descriptor
    // answering by accident.
    let proc = a_process(45_702, "trunc-neg", "trunc_neg");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nowhere = c("/trunc_neg/not-there");

            assert_eq!(
                truncate(sys, &nowhere, 0),
                Err(LxError::ENOENT),
                "the path this test relies on being absent was found"
            );
            assert_eq!(
                truncate(sys, &nowhere, A_NEGATIVE_LENGTH),
                Err(LxError::EINVAL),
                "truncate looked up the path before judging the length"
            );

            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(A_CLOSED_FD), 0),
                Err(LxError::EBADF),
                "the descriptor this test relies on being closed was open"
            );
            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(A_CLOSED_FD), A_NEGATIVE_LENGTH),
                Err(LxError::EINVAL),
                "ftruncate looked up the descriptor before judging the length"
            );
        })
    });
}

#[test]
fn truncate_asks_for_write_permission_on_the_file_and_ftruncate_asks_for_none() {
    // The asymmetry, on one file so nothing else can account for the two
    // answers: root's, mode 0o444, already open read-write, and the caller
    // is not root. `truncate` must refuse -- it is deciding now whether
    // this caller may write this file -- and `ftruncate` must not, because
    // that decision was made when the descriptor was opened.
    //
    // Borrowing `truncate`'s gate for `ftruncate` breaks `>file` on a
    // read-only file the shell has already opened; borrowing `ftruncate`'s
    // for `truncate` hands any user a free write to any file in the tree.
    let proc = a_process(45_703, "trunc-perm", "trunc_perm");
    let dir_path = String::from("/trunc_perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", TEN_DIGITS, 0, 0o444);
            let path = alloc::format!("{}/f", dir_path);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            assert_eq!(
                truncate(sys, &c(&path), 0),
                Err(LxError::EACCES),
                "truncate of a file this caller may not write was allowed"
            );
            assert_eq!(
                length_of(&lp, &path),
                10,
                "a refused truncate shortened the file anyway"
            );

            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(fd), 0),
                Ok(0),
                "ftruncate asked about the mode instead of the descriptor"
            );
            assert_eq!(length_of(&lp, &path), 0);
        })
    });
}

#[test]
fn growing_past_rlimit_fsize_is_efbig_with_a_sigxfsz_and_shrinking_under_it_is_allowed() {
    // `RLIMIT_FSIZE` is asked of both calls, and only of the ones that
    // grow: a file that is already longer than the limit can still be cut
    // down to a length that is also longer than it, which is the shape the
    // check had wrong once. `grows_past_fsize` is pure and tested; that
    // both call sites ask it, that neither resizes afterwards, and that the
    // refusal comes with the `SIGXFSZ` the limit owes the caller are not.
    let proc = a_process(45_704, "trunc-fsize", "trunc_fsize");
    let dir_path = String::from("/trunc_fsize");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", &[b'x'; 40], 0, 0o644);
            let path = alloc::format!("{}/f", dir_path);
            let max = lp.rlimit(RLIMIT_FSIZE, None, false).unwrap().max;
            lp.rlimit(RLIMIT_FSIZE, Some(RLimit { cur: 16, max }), false)
                .unwrap();

            // Shorter than it was, longer than the limit: allowed.
            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(fd), 20),
                Ok(0),
                "a shrink was refused because the file stayed over the limit"
            );
            assert_eq!(length_of(&lp, &path), 20);

            assert!(
                !sys.thread.lock_linux().signals.contains(Signal::SIGXFSZ),
                "something raised SIGXFSZ before any call exceeded the limit"
            );
            assert_eq!(
                sys.sys_ftruncate(FileDesc::from(fd), 64),
                Err(LxError::EFBIG),
                "ftruncate grew a file past RLIMIT_FSIZE"
            );
            assert_eq!(
                length_of(&lp, &path),
                20,
                "a refused ftruncate resized the file anyway"
            );
            assert!(
                sys.thread.lock_linux().signals.contains(Signal::SIGXFSZ),
                "EFBIG came without the SIGXFSZ the limit owes the caller"
            );

            assert_eq!(
                truncate(sys, &c(&path), 64),
                Err(LxError::EFBIG),
                "truncate grew a file past RLIMIT_FSIZE"
            );
            assert_eq!(length_of(&lp, &path), 20);
        })
    });
}

#[test]
fn a_memfd_sealed_against_shrinking_keeps_its_length_and_may_still_grow() {
    // File seals are what makes a shared memory pool safe to map: a
    // compositor keeps a client's `memfd` only because the client can no
    // longer shrink it out from under the mapping (Wayland's
    // `os_create_anonymous_file`, Mesa's shm pools). The seal is read by
    // `memfd_resize_allowed`, which `ftruncate` is the only caller of, so
    // nothing in this tree had ever asked `ftruncate` about one.
    //
    // Both directions, because a seal that refused every resize would pass
    // a test that only tried to shrink.
    let proc = a_process(45_705, "trunc-seal", "trunc_seal");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let memfd = linux_object::fs::new_memfd("pool", linux_object::fs::MFD_ALLOW_SEALING)
                .expect("a sealable memfd");
            let inode = memfd.inode();
            let fd: i32 = lp.add_file(memfd).unwrap().into();
            let fd = FileDesc::from(fd as usize);

            assert_eq!(sys.sys_ftruncate(fd, 10), Ok(0));
            linux_object::fs::memfd_add_seals(&inode, linux_object::fs::F_SEAL_SHRINK).unwrap();

            assert_eq!(
                sys.sys_ftruncate(fd, 4),
                Err(LxError::EPERM),
                "a memfd sealed against shrinking was shrunk"
            );
            assert_eq!(
                inode.metadata().unwrap().size,
                10,
                "a refused resize shortened the sealed file anyway"
            );

            assert_eq!(
                sys.sys_ftruncate(fd, 20),
                Ok(0),
                "F_SEAL_SHRINK refused a grow as well"
            );
            assert_eq!(inode.metadata().unwrap().size, 20);
        })
    });
}

#[test]
fn every_number_the_filesystem_reports_lands_in_its_own_field_of_statfs() {
    // The whole of `statfs` and `fstatfs` is this conversion, and every one
    // of these numbers is a different question: `f_bfree` is what is free
    // and `f_bavail` what is free to an unprivileged caller, `f_blocks`
    // counts blocks of `f_frsize` while `f_bsize` is the transfer size.
    // Eight distinct values, so no two of them can be swapped unnoticed.
    let statfs: StatFs = FsInfo {
        bsize: 4096,
        frsize: 512,
        blocks: 1000,
        bfree: 600,
        bavail: 400,
        files: 300,
        ffree: 200,
        namemax: 255,
        ftype: 0xEF53,
    }
    .into();

    assert_eq!(statfs.f_type, 0xEF53);
    assert_eq!(statfs.f_bsize, 4096);
    assert_eq!(statfs.f_frsize, 512);
    assert_eq!(statfs.f_blocks, 1000);
    assert_eq!(statfs.f_bfree, 600);
    assert_eq!(statfs.f_bavail, 400);
    assert_eq!(statfs.f_files, 300);
    assert_eq!(statfs.f_ffree, 200);
    assert_eq!(statfs.f_namelen, 255);
    // Not served yet, and a caller may read them: they have to be the
    // values that say so rather than whatever the stack held.
    assert_eq!(statfs.f_fsid, (0, 0));
    assert_eq!(statfs.f_flags, 0);
    assert_eq!(statfs.f_spare, [0; 4]);
}

#[test]
fn statfs_answers_for_any_file_in_the_filesystem_and_fstatfs_for_the_descriptor() {
    // `statfs(2)` takes "the pathname of any file within the mounted
    // filesystem", a directory included -- `df /` is the common case, and a
    // kernel that wanted a regular file there would break it. The buffer
    // starts out full of numbers no filesystem here reports, so what is
    // asserted is that the answer was written and not that it happened to
    // match.
    let proc = a_process(45_706, "statfs-ok", "statfs_ok");
    let dir_path = String::from("/statfs_ok");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", TEN_DIGITS, 0, 0o644);
            let dir_name = c(&dir_path);
            let file_name = c(&alloc::format!("{}/f", dir_path));
            // The numbers the filesystem itself reports, through the same
            // conversion: the test is that they reach the caller, whatever
            // ramfs decides its block size or name limit is.
            let want: StatFs = lp.root_inode().fs().info().into();

            for (what, name) in [("a file", &file_name), ("a directory", &dir_name)] {
                let mut buf = an_unwritten_statfs();
                let addr = &mut buf as *mut StatFs as usize;
                assert_eq!(
                    sys.sys_statfs(UserInPtr::from(name.as_ptr() as usize), addr.into()),
                    Ok(0),
                    "statfs refused {}",
                    what
                );
                assert_eq!(buf.f_type, TMPFS_MAGIC, "statfs of {} wrote no type", what);
                assert_eq!(
                    (buf.f_bsize, buf.f_namelen),
                    (want.f_bsize, want.f_namelen),
                    "statfs of {} left a field of the buffer alone",
                    what
                );
            }

            let mut buf = an_unwritten_statfs();
            let addr = &mut buf as *mut StatFs as usize;
            assert_eq!(sys.sys_fstatfs(FileDesc::from(fd), addr.into()), Ok(0));
            assert_eq!(
                buf.f_type, TMPFS_MAGIC,
                "fstatfs answered for a descriptor on the same filesystem with a different type"
            );
            assert_eq!(buf.f_bsize, want.f_bsize);
        })
    });
}

#[test]
fn a_statfs_that_cannot_be_answered_writes_nothing_into_the_buffer() {
    // The copy out is the last thing either call does, which is the only
    // reason a caller may read the buffer after a failure and find what it
    // put there. A write moved ahead of the lookup would fill it with the
    // numbers of whatever filesystem answered last.
    let proc = a_process(45_707, "statfs-bad", "statfs_bad");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nowhere = c("/statfs_bad/not-there");

            let mut buf = an_unwritten_statfs();
            let addr = &mut buf as *mut StatFs as usize;
            assert_eq!(
                sys.sys_statfs(UserInPtr::from(nowhere.as_ptr() as usize), addr.into()),
                Err(LxError::ENOENT)
            );
            assert_eq!(
                (buf.f_type, buf.f_bsize),
                (0x1234_5678, -1),
                "a statfs that found nothing wrote into the buffer"
            );

            let mut buf = an_unwritten_statfs();
            let addr = &mut buf as *mut StatFs as usize;
            assert_eq!(
                sys.sys_fstatfs(FileDesc::from(A_CLOSED_FD), addr.into()),
                Err(LxError::EBADF)
            );
            assert_eq!(
                (buf.f_type, buf.f_bsize),
                (0x1234_5678, -1),
                "an fstatfs of a closed descriptor wrote into the buffer"
            );
        })
    });
}
