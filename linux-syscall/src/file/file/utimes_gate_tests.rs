//! Who may set a file's times, asked through the syscall.
//!
//! `utimes_verdict` and `check_utimes` have tests of their own in
//! `linux-object`, and `utimensat_times` has `utimes_times_tests` beside
//! this file. What none of them can see is the **wiring**, which is where
//! the `chown` holes were: whether `explicit` is handed to the gate with
//! the value the caller's arguments mean, whether the refusal arrives
//! before the times are written, and whether the one branch that asks no
//! permission at all still asks none.
//!
//! That split is the whole subtlety here, and it is not symmetric. An
//! explicit time is the owner's and root's alone -- write access to the
//! file is NOT enough -- while a touch is allowed to anyone who may write
//! it. Get `explicit` backwards and a kernel either refuses every `touch`
//! on a file its caller may write, or lets anyone with write access forge
//! the timestamps on it: backdate `/etc/passwd`, hide a change from `make`,
//! or beat a backup's mtime comparison.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// Somebody who is not root.
const A_USER: u32 = 1000;

/// A second in 1970 that no clock here will produce by accident, so an
/// assertion on it cannot pass because something happened to be "now".
const AN_OLD_SECOND: usize = 1_000_000;
/// Another one, to tell the two times apart.
const A_DIFFERENT_OLD_SECOND: usize = 2_000_000;

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` called `name`, with exactly this owner and mode and
/// both its times at zero, and the descriptor it got.
fn a_file_named(lp: &LinuxProcess, dir_path: &str, name: &str, uid: u32, mode: u16) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    metadata.atime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    metadata.mtime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    inode.set_metadata(&metadata).unwrap();
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// The times the file really has, in seconds, asked of the filesystem and
/// not of the syscall under test.
fn times_of(lp: &LinuxProcess, dir_path: &str, name: &str) -> (i64, i64) {
    let metadata = lp
        .lookup_inode(&alloc::format!("{}/{}", dir_path, name))
        .unwrap()
        .metadata()
        .unwrap();
    (metadata.atime.sec, metadata.mtime.sec)
}

/// `utimensat(dirfd, pathname, times, flags)` by the syscall.
///
/// `sys_utimensat` wants `&mut self` and `on_a_thread` lends a shared one,
/// so this builds its own out of the same three parts, as the other
/// end-to-end modules do.
fn utimensat(
    sys: &Syscall<'_>,
    dirfd: FileDesc,
    pathname: UserInPtr<u8>,
    times: UserInOutPtr<[TimeSpec; 2]>,
    flags: usize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.sys_utimensat(dirfd, pathname, times, flags)
}

/// `futimens(fd, times)`, which glibc spells `utimensat(fd, NULL, times, 0)`
/// -- the form that reaches the file through a descriptor, with no path walk
/// in the way. The permission tests below use it on purpose: the gate is
/// then the only thing that can answer.
fn futimens(sys: &Syscall<'_>, fd: usize, times: &mut [TimeSpec; 2]) -> SysResult {
    utimensat(
        sys,
        FileDesc::from(fd),
        // Built here and not shared: `UserInPtr` is not `Copy`.
        UserInPtr::from(0),
        UserInOutPtr::from(times as *mut [TimeSpec; 2] as usize),
        0,
    )
}

/// A touch: `times` is a null pointer, which means "both to now".
fn touch(sys: &Syscall<'_>, fd: usize) -> SysResult {
    utimensat(
        sys,
        FileDesc::from(fd),
        UserInPtr::from(0),
        UserInOutPtr::from(0),
        0,
    )
}

fn at(sec: usize) -> TimeSpec {
    TimeSpec { sec, nsec: 0 }
}

/// `UTIME_OMIT` in a `timespec`: "leave this one as it is". The second is
/// ignored, so it is set to something recognizable to prove it is.
fn omit() -> TimeSpec {
    TimeSpec {
        sec: 777,
        nsec: UTIME_OMIT,
    }
}

#[test]
fn write_access_is_not_enough_to_set_an_explicit_time() {
    // `utimes_common`: an explicit time is the owner's and root's alone,
    // `EPERM` for anyone else, write access or not. The file here is
    // writable by everyone, so a kernel that asked for write permission
    // instead of ownership would allow it -- and then any user could
    // backdate a file they are merely allowed to append to.
    //
    // The refusal has to arrive before the times are written, which is what
    // the second assertion is for.
    let proc = a_process(45_600, "utimes-eperm", "utimes_eperm");
    let dir_path = String::from("/utimes_eperm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Root's file, and writable by everyone.
            let fd = a_file_named(&lp, &dir_path, "f", 0, 0o666);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            let mut explicit = [at(AN_OLD_SECOND), at(AN_OLD_SECOND)];
            assert_eq!(
                futimens(sys, fd, &mut explicit),
                Err(LxError::EPERM),
                "someone who is not the owner set an explicit time"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "a refused call wrote the times anyway"
            );
        })
    });
}

#[test]
fn a_touch_needs_write_access_and_nothing_more() {
    // The other half, and the one a wrong `explicit` breaks the opposite
    // way: a touch (`ATTR_TOUCH`) is allowed to anyone `MAY_WRITE` lets
    // write the file, which is what makes `touch somebody-elses-file` work
    // on a writable file. Asked of two files that differ only in whether
    // this caller may write them, so the answer turns on that and on
    // nothing else.
    let proc = a_process(45_601, "utimes-touch", "utimes_touch");
    let dir_path = String::from("/utimes_touch");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Both root's; the first is writable by everyone, the second is
            // not.
            let writable = a_file_named(&lp, &dir_path, "writable", 0, 0o666);
            let read_only = a_file_named(&lp, &dir_path, "read-only", 0, 0o644);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            assert_eq!(
                touch(sys, writable),
                Ok(0),
                "a touch was refused on a file this caller may write"
            );
            assert_ne!(
                times_of(&lp, &dir_path, "writable"),
                (0, 0),
                "the touch did not reach the inode"
            );

            assert_eq!(
                touch(sys, read_only),
                Err(LxError::EACCES),
                "a touch went through on a file this caller may not write"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "read-only"),
                (0, 0),
                "a refused touch wrote the times anyway"
            );
        })
    });
}

#[test]
fn omitting_both_times_asks_no_permission_at_all() {
    // `do_utimes`: both `UTIME_OMIT` is a request for nothing, and it
    // succeeds before any permission is looked at. That makes it an
    // ordering property in the opposite direction to the two above -- a
    // gate moved one step earlier would start refusing a call that asks for
    // nothing. Asked on a file this caller may neither own nor write, so
    // there is no other way for it to come back `Ok`.
    let proc = a_process(45_602, "utimes-omit", "utimes_omit");
    let dir_path = String::from("/utimes_omit");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Root's, and nobody else may even read it.
            let fd = a_file_named(&lp, &dir_path, "f", 0, 0o600);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            let mut nothing = [omit(), omit()];
            assert_eq!(
                futimens(sys, fd, &mut nothing),
                Ok(0),
                "a request for nothing was refused"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "a request for nothing changed a time"
            );
        })
    });
}

#[test]
fn futimens_takes_no_flags_whatever_they_hold() {
    // `do_utimes` with a null pathname is `futimens`, and it has no `flags`
    // argument to honour: a non-zero word there is `EINVAL` whatever it
    // holds, including flags that are perfectly good for `utimensat`. A
    // kernel that let `AT_SYMLINK_NOFOLLOW` through here would be accepting
    // an argument it then ignores.
    let proc = a_process(45_603, "utimes-flags", "utimes_flags");
    let dir_path = String::from("/utimes_flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let mut explicit = [at(AN_OLD_SECOND), at(AN_OLD_SECOND)];
            // The address is taken once, so the two calls below do not each
            // want their own borrow of it.
            let addr = &mut explicit as *mut [TimeSpec; 2] as usize;
            let times = || UserInOutPtr::from(addr);
            // `AT_SYMLINK_NOFOLLOW` (0x100) is a real `utimensat` flag.
            assert_eq!(
                utimensat(sys, FileDesc::from(fd), UserInPtr::from(0), times(), 0x100),
                Err(LxError::EINVAL),
                "futimens accepted a flag word"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "a refused call wrote the times anyway"
            );

            // And with no flags at all it goes through, so the EINVAL above
            // is the flag word and not the shape of the call.
            assert_eq!(
                utimensat(sys, FileDesc::from(fd), UserInPtr::from(0), times(), 0),
                Ok(0),
                "futimens with no flags was refused"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (AN_OLD_SECOND as i64, AN_OLD_SECOND as i64),
                "the explicit times did not reach the inode"
            );
        })
    });
}

#[test]
fn omitting_one_of_the_two_times_leaves_that_one_alone() {
    // The per-time `UTIME_OMIT`, which is how a program sets an mtime
    // without disturbing the atime it does not know. The two are written by
    // separate branches, so omitting each one in turn is the only way to
    // see that neither branch runs when it should not -- and the seconds
    // field beside an `UTIME_OMIT` is ignored, which the recognizable 777
    // above proves.
    let proc = a_process(45_604, "utimes-one", "utimes_one");
    let dir_path = String::from("/utimes_one");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_named(&lp, &dir_path, "f", 0, 0o644);

            let mut only_mtime = [omit(), at(AN_OLD_SECOND)];
            assert_eq!(futimens(sys, fd, &mut only_mtime), Ok(0));
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, AN_OLD_SECOND as i64),
                "omitting the atime did not leave it alone"
            );

            let mut only_atime = [at(A_DIFFERENT_OLD_SECOND), omit()];
            assert_eq!(futimens(sys, fd, &mut only_atime), Ok(0));
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (A_DIFFERENT_OLD_SECOND as i64, AN_OLD_SECOND as i64),
                "omitting the mtime did not leave it alone"
            );
        })
    });
}
