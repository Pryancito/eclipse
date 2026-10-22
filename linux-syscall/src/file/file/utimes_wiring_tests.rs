//! What `utimes(2)` does to the file, asked through the syscall.
//!
//! This is the last of the file's syscalls to get a test that calls it, and
//! it is the same shape as the rest: `utimes_times` decodes the pair of
//! `timeval`s and has `utimes_times_tests` beside this file, while
//! `check_utimes` has tests of its own in `linux-object`. What neither can
//! see is the wiring, and `utimes` has one piece of wiring that is all its
//! own.
//!
//! Every other call in the family carries `timespec`s, which go into the
//! inode as they are. `utimes` carries `timeval`s, so its times go through
//! a **multiplication by a thousand** on the way in. A kernel that forgets
//! it does not fail: it writes a time that is a thousand times too early in
//! the sub-second field, which is exactly the kind of error that comes back
//! as a build system rebuilding the world, or not rebuilding it at all.
//!
//! The other difference is the one that a refactor would quietly remove. In
//! `utimensat` a null pathname is not a mistake -- it is how glibc spells
//! `futimens`, and the call then names the file `dirfd` is open on. `utimes`
//! has no descriptor argument at all, so a null pathname there is a bad
//! pointer and nothing else.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::process::LinuxProcess;
use linux_object::time::TimeVal;
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

/// A `tv_usec` that is not a microsecond count. A million of them is a
/// whole second, which belongs in `tv_sec`, so `utimes(2)` refuses it.
const A_MICROSECOND_TOO_MANY: usize = 1_000_000;

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` called `name`, with exactly this owner and mode and
/// both its times at zero. No descriptor comes back: `utimes` only ever
/// names a path.
fn a_file_named(lp: &LinuxProcess, dir_path: &str, name: &str, uid: u32, mode: u16) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    metadata.atime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    metadata.mtime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    inode.set_metadata(&metadata).unwrap();
}

/// The seconds the file really has, asked of the filesystem and not of the
/// syscall under test.
fn times_of(lp: &LinuxProcess, dir_path: &str, name: &str) -> (i64, i64) {
    let metadata = lp
        .lookup_inode(&alloc::format!("{}/{}", dir_path, name))
        .unwrap()
        .metadata()
        .unwrap();
    (metadata.atime.sec, metadata.mtime.sec)
}

/// And the sub-second halves, which are the whole point of this module:
/// they are the only place the microsecond-to-nanosecond step shows.
fn nanos_of(lp: &LinuxProcess, dir_path: &str, name: &str) -> (i32, i32) {
    let metadata = lp
        .lookup_inode(&alloc::format!("{}/{}", dir_path, name))
        .unwrap()
        .metadata()
        .unwrap();
    (metadata.atime.nsec, metadata.mtime.nsec)
}

/// A path as userspace writes one: NUL-terminated.
fn c(s: &str) -> alloc::vec::Vec<u8> {
    let mut bytes = alloc::vec::Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

fn tv(sec: usize, usec: usize) -> TimeVal {
    TimeVal { sec, usec }
}

/// `utimes(pathname, times)` by the syscall.
///
/// `sys_utimes` wants `&mut self` and `on_a_thread` lends a shared one, so
/// this builds its own out of the same three parts, as the other end-to-end
/// modules do. The two pointers are built here rather than passed in:
/// `UserInPtr` is not `Copy`, so each call needs its own.
fn call(sys: &Syscall<'_>, pathname: usize, times: usize) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.sys_utimes(UserInPtr::from(pathname), UserInPtr::from(times))
}

/// Two explicit times for the file this path names.
fn utimes(sys: &Syscall<'_>, pathname: &[u8], times: &[TimeVal; 2]) -> SysResult {
    call(
        sys,
        pathname.as_ptr() as usize,
        times as *const [TimeVal; 2] as usize,
    )
}

/// A touch: `times` is a null pointer, which means "both to now".
fn utimes_now(sys: &Syscall<'_>, pathname: &[u8]) -> SysResult {
    call(sys, pathname.as_ptr() as usize, 0)
}

/// The form that has no meaning here, however much it has in `utimensat`.
fn utimes_at_a_null_path(sys: &Syscall<'_>, times: &[TimeVal; 2]) -> SysResult {
    call(sys, 0, times as *const [TimeVal; 2] as usize)
}

#[test]
fn the_microseconds_of_both_timevals_reach_the_file_as_nanoseconds() {
    // The step that is `utimes` and nothing else. Both halves of both
    // times are asked for at once, with four different numbers, so a
    // kernel that dropped the multiplication, wrote one time into both
    // fields or swapped the pair cannot come back right by accident.
    let proc = a_process(46_100, "utimes-wire", "utimes_wire");
    let dir_path = String::from("/utimes_wire");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let path = c(&alloc::format!("{}/f", dir_path));

            let times = [tv(AN_OLD_SECOND, 250_000), tv(A_DIFFERENT_OLD_SECOND, 1)];
            assert_eq!(utimes(sys, &path, &times), Ok(0));

            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
                "the two times were swapped, shared or never written"
            );
            assert_eq!(
                nanos_of(&lp, &dir_path, "f"),
                (250_000_000, 1_000),
                "the microseconds did not become nanoseconds"
            );
        })
    });
}

#[test]
fn a_null_times_is_a_touch_and_it_replaces_the_times_that_were_there() {
    // A null `times` means "both to now", and the file is given an old
    // time first so that "now" is not what it already had. Both have to
    // end up the same, because a touch is one clock reading handed to
    // both fields and not two.
    let proc = a_process(46_101, "utimes-touch", "utimes_touch");
    let dir_path = String::from("/utimes_touch");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let path = c(&alloc::format!("{}/f", dir_path));

            let old = [tv(AN_OLD_SECOND, 0), tv(A_DIFFERENT_OLD_SECOND, 0)];
            assert_eq!(utimes(sys, &path, &old), Ok(0));
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64)
            );

            assert_eq!(utimes_now(sys, &path), Ok(0));
            let (atime, mtime) = times_of(&lp, &dir_path, "f");
            assert_ne!(
                atime, AN_OLD_SECOND as i64,
                "a null times left the old atime in place"
            );
            assert_ne!(
                mtime, A_DIFFERENT_OLD_SECOND as i64,
                "a null times left the old mtime in place"
            );
            assert_eq!(atime, mtime, "a touch gave the two times different values");
        })
    });
}

#[test]
fn a_microsecond_count_that_is_not_one_is_einval_through_either_of_the_two() {
    // `tv_usec` has to be under a million in BOTH timevals, and the
    // second one is the one that is easy to leave unchecked: every
    // assertion that only ever gets the first one wrong passes without
    // it. Nothing may be written either -- the refusal arrives before the
    // metadata is touched, so a half-applied pair is not a thing.
    let proc = a_process(46_102, "utimes-usec", "utimes_usec");
    let dir_path = String::from("/utimes_usec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let path = c(&alloc::format!("{}/f", dir_path));

            let first_bad = [
                tv(AN_OLD_SECOND, A_MICROSECOND_TOO_MANY),
                tv(AN_OLD_SECOND, 0),
            ];
            assert_eq!(
                utimes(sys, &path, &first_bad),
                Err(LxError::EINVAL),
                "a tv_usec of a million was accepted in the atime"
            );

            let second_bad = [
                tv(AN_OLD_SECOND, 0),
                tv(AN_OLD_SECOND, A_MICROSECOND_TOO_MANY),
            ];
            assert_eq!(
                utimes(sys, &path, &second_bad),
                Err(LxError::EINVAL),
                "only the first of the two timevals was checked"
            );

            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "a refused utimes wrote a time anyway"
            );
        })
    });
}

#[test]
fn the_path_is_walked_before_the_times_are_read() {
    // `do_utimes`: the path first, the times after. The order is visible
    // at the one call where both are wrong, and it is the opposite way
    // round from `fallocate`, where the mode comes before the descriptor.
    // Getting it backwards tells a caller its times are bad when the file
    // it named is not there, which is the answer that sends someone
    // looking in the wrong place.
    let proc = a_process(46_103, "utimes-order", "utimes_order");
    let dir_path = String::from("/utimes_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let there = c(&alloc::format!("{}/f", dir_path));
            let not_there = c(&alloc::format!("{}/not-here", dir_path));

            let bad = [
                tv(AN_OLD_SECOND, A_MICROSECOND_TOO_MANY),
                tv(AN_OLD_SECOND, 0),
            ];
            assert_eq!(
                utimes(sys, &not_there, &bad),
                Err(LxError::ENOENT),
                "the times were read before the path was walked"
            );
            // The same times, on a path that is there, to show that they
            // really are the other thing wrong with that call.
            assert_eq!(utimes(sys, &there, &bad), Err(LxError::EINVAL));
        })
    });
}

#[test]
fn a_null_pathname_is_efault_and_not_the_working_directory() {
    // The difference from `utimensat` that a refactor sharing its path
    // handling would remove without a word: there, a null pathname names
    // the file `dirfd` is open on. Here there is no `dirfd`, so a null
    // pathname is a bad pointer -- and reading it as "the current
    // directory" instead would set the times on a directory the caller
    // never named.
    let proc = a_process(46_104, "utimes-null", "utimes_null");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let times = [tv(AN_OLD_SECOND, 0), tv(AN_OLD_SECOND, 0)];
            assert_eq!(
                utimes_at_a_null_path(sys, &times),
                Err(LxError::EFAULT),
                "a null pathname was taken for a file"
            );
        })
    });
}

#[test]
fn only_the_owner_may_set_an_explicit_time_through_this_call_too() {
    // The asymmetric gate of `utimes_common`, asked once more because it
    // is reached through a different function here: an explicit time is
    // the owner's and root's alone, write access or not, while a touch is
    // allowed to anyone who may write the file. Both halves are asked of
    // the SAME file by the SAME caller, so nothing but `explicit` can
    // account for the two answers.
    let proc = a_process(46_105, "utimes-perm", "utimes_perm");
    let dir_path = String::from("/utimes_perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Root's file, and writable by everyone.
            a_file_named(&lp, &dir_path, "f", 0, 0o666);
            let path = c(&alloc::format!("{}/f", dir_path));
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            let explicit = [tv(AN_OLD_SECOND, 0), tv(AN_OLD_SECOND, 0)];
            assert_eq!(
                utimes(sys, &path, &explicit),
                Err(LxError::EPERM),
                "someone who is not the owner set an explicit time"
            );
            assert_eq!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "a refused call wrote the times anyway"
            );

            assert_eq!(
                utimes_now(sys, &path),
                Ok(0),
                "a touch was refused on a file this caller may write"
            );
            assert_ne!(
                times_of(&lp, &dir_path, "f"),
                (0, 0),
                "the touch did not reach the inode"
            );
        })
    });
}

// ---------------------------------------------------------------------------
// `utimensat(2)` with a PATH, which is the form nothing had ever called.
//
// The module above asks `utimes` for everything it does, and the null-path
// form of `utimensat` -- `futimens(3)` -- is asked for elsewhere. What was
// never asked for is `utimensat` with a pathname in it: seven lines, the
// whole `else` of the one `if` that chooses between the two forms. That is
// the form every tool actually calls (`touch -a`, `cp -p`, `tar -x`, every
// build system stamping an output), and it is the only one that reads the
// flag word, so `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` had never reached
// the syscall either.
//
// `AT_EMPTY_PATH` is the piece worth the most: with it, an EMPTY pathname
// names the file `dirfd` is open on, which is a third way to spell the same
// call. Without it an empty path is `ENOENT`, and the two are one `contains`
// apart.
//
// Fixed pids live in 49_35x.

/// `AT_FDCWD`, which an absolute path makes irrelevant and the syscall still
/// has to accept.
const AT_CWD: FileDesc = FileDesc::CWD;

/// A nanosecond count no clock here will produce by accident.
const AN_ODD_NANOSECOND: usize = 123_456_789;

/// A flag `utimensat(2)` has no business with (`AT_EACCESS`, which is
/// `faccessat`'s), to ask whether the flag word is checked at all.
const A_FLAG_THAT_IS_NOT_OURS: usize = 0x200;

fn ts(sec: usize, nsec: usize) -> TimeSpec {
    TimeSpec { sec, nsec }
}

/// `utimensat(dirfd, pathname, times, flags)` by the syscall. A null
/// `pathname` is the `futimens` form and is not what this half is about.
fn utimensat(
    sys: &Syscall<'_>,
    dirfd: FileDesc,
    pathname: usize,
    times: &[TimeSpec; 2],
    flags: usize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.sys_utimensat(
        dirfd,
        UserInPtr::from(pathname),
        UserInOutPtr::from(times as *const [TimeSpec; 2] as usize),
        flags,
    )
}

/// A descriptor open on `dir_path/name`, for the `AT_EMPTY_PATH` form.
fn an_open_file(lp: &LinuxProcess, dir_path: &str, name: &str) -> FileDesc {
    let path = alloc::format!("{}/{}", dir_path, name);
    let inode = lp.lookup_inode(&path).unwrap();
    lp.add_file(linux_object::fs::File::new(
        inode,
        linux_object::fs::OpenFlags::RDWR,
        path,
    ))
    .unwrap()
}

/// The sub-second halves as the file really has them.
fn file_nanos(lp: &LinuxProcess, dir_path: &str, name: &str) -> (i32, i32) {
    nanos_of(lp, dir_path, name)
}

#[test]
fn a_utimensat_that_names_a_path_sets_that_files_times() {
    // The form every tool calls, through the syscall, with four different
    // numbers so a kernel that wrote one time into both, or lost the
    // sub-second half, could not pass.
    let proc = a_process(49_350, "utimensat-path", "named");
    let lp = proc.linux();
    a_file_named(lp, "/named", "stamped", 0, 0o644);
    // A second file, untouched, so "it set the times" cannot be "it set
    // everybody's times".
    a_file_named(lp, "/named", "untouched", 0, 0o644);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let times = [
                ts(AN_OLD_SECOND, AN_ODD_NANOSECOND),
                ts(A_DIFFERENT_OLD_SECOND, 0),
            ];
            utimensat(
                sys,
                AT_CWD,
                c("/named/stamped").as_ptr() as usize,
                &times,
                0,
            )
        })
    });
    assert_eq!(got, Ok(0));
    assert_eq!(
        times_of(proc.linux(), "/named", "stamped"),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the two times went in, and not the same one twice"
    );
    assert_eq!(
        file_nanos(proc.linux(), "/named", "stamped"),
        (AN_ODD_NANOSECOND as i32, 0),
        "a timespec's nanoseconds go in as they are, with no scaling"
    );
    assert_eq!(
        times_of(proc.linux(), "/named", "untouched"),
        (0, 0),
        "the call named one file and touched another"
    );
}

#[test]
fn an_empty_path_with_at_empty_path_names_the_file_the_descriptor_is_open_on() {
    // The third spelling of the same call, and the one `contains` that tells
    // it from the mistake below.
    let proc = a_process(49_351, "utimensat-empty", "named");
    let lp = proc.linux();
    a_file_named(lp, "/named", "by-fd", 0, 0o644);
    let fd = an_open_file(lp, "/named", "by-fd");
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let times = [ts(AN_OLD_SECOND, 0), ts(A_DIFFERENT_OLD_SECOND, 0)];
            utimensat(sys, fd, c("").as_ptr() as usize, &times, 0x1000)
        })
    });
    assert_eq!(got, Ok(0));
    assert_eq!(
        times_of(proc.linux(), "/named", "by-fd"),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "AT_EMPTY_PATH did not reach the file the descriptor is open on"
    );
}

#[test]
fn an_empty_path_without_at_empty_path_names_nothing() {
    // Not the `futimens` form: the pathname is a valid pointer to an empty
    // string, not a null pointer, so the call takes the path branch and
    // looks up a name that does not exist.
    let proc = a_process(49_352, "utimensat-noname", "named");
    let lp = proc.linux();
    a_file_named(lp, "/named", "by-fd", 0, 0o644);
    let fd = an_open_file(lp, "/named", "by-fd");
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let times = [ts(AN_OLD_SECOND, 0), ts(A_DIFFERENT_OLD_SECOND, 0)];
            utimensat(sys, fd, c("").as_ptr() as usize, &times, 0)
        })
    });
    assert_eq!(got, Err(LxError::ENOENT));
    assert_eq!(
        times_of(proc.linux(), "/named", "by-fd"),
        (0, 0),
        "a call that named nothing stamped something anyway"
    );
}

#[test]
fn the_flag_word_of_the_path_form_is_checked_and_not_ignored() {
    // Only this form reads the flags at all, so this is where a flag that
    // belongs to another syscall has to be refused -- and where the one flag
    // that does belong here has to be accepted. `AT_SYMLINK_NOFOLLOW` on a
    // file that is not a symlink changes nothing about the answer, which is
    // the point: it must not be mistaken for a flag to refuse.
    let proc = a_process(49_353, "utimensat-flags", "named");
    let lp = proc.linux();
    a_file_named(lp, "/named", "flagged", 0, 0o644);
    let (refused, accepted) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let times = [ts(AN_OLD_SECOND, 0), ts(A_DIFFERENT_OLD_SECOND, 0)];
            let path = c("/named/flagged");
            let refused = utimensat(
                sys,
                AT_CWD,
                path.as_ptr() as usize,
                &times,
                A_FLAG_THAT_IS_NOT_OURS,
            );
            let accepted = utimensat(sys, AT_CWD, path.as_ptr() as usize, &times, 0x100);
            (refused, accepted)
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EINVAL),
        "a flag from another syscall was accepted"
    );
    assert_eq!(accepted, Ok(0), "AT_SYMLINK_NOFOLLOW was refused");
    assert_eq!(
        times_of(proc.linux(), "/named", "flagged"),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the accepted call did not reach the file"
    );
}
