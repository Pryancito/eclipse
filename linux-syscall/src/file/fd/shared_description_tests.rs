//! One open file description and the descriptors that name it: the `dup`
//! family, `pipe2`, and the lock `flock` hangs on the description rather
//! than on the descriptor.
//!
//! `dup_limit_tests` next door covers `dup_target`, `dupfd_start` and
//! `dupfd_picked`, which are the three `RLIMIT_NOFILE` arithmetic steps, and
//! `fd_abi_number_tests` covers `dup3_args_ok` and the `close_range` flag
//! numbers. All of it is arithmetic on a number nobody opened. What no pure
//! test can see is the part that makes `dup` worth having:
//!
//! **That a copy is a second NAME, not a second file.** `fs/file.c` does
//! `get_file(file)` and stores that pointer, so both descriptors share the
//! offset and the status flags. That is what makes `prog >log 2>&1` write
//! one log instead of two streams overwriting each other from offset zero,
//! and it is what this kernel did not do: every `FileLike` carried a `dup`
//! of its own that built a NEW object, so the two descriptors had a cursor
//! each.
//!
//! **That `FD_CLOEXEC` is the one thing a descriptor owns.** It lives in the
//! per-process table and never in the shared object, so `dup` of a
//! close-on-exec descriptor gives one that is not, `dup3` and
//! `F_DUPFD_CLOEXEC` set it on the new descriptor only, and the original
//! keeps whatever it had.
//!
//! **That `dup2` is a replace.** Whatever was at the target is closed, in
//! the same table lock, and the answer is the target number -- and the
//! target arrives as the whole register, so a `dup2(fd, 1 << 32 | 1)` must
//! not fold onto descriptor 1 and clobber it.
//!
//! **That a pipe's two ends come back in order**, read end first, each one
//! open the one way it works, and that a `pipe2` that cannot hand them over
//! takes them back instead of leaving them in the table with nothing left
//! that can close them.
//!
//! **That `flock` takes a lock at all.** It used to validate the request and
//! answer 0 without taking anything, so two `LOCK_EX` on one file both
//! succeeded and every "is another instance running?" probe -- `flock(1)`,
//! dpkg's and apt's frontend locks -- found the file free. And since the
//! holder is the description, the same lock asked for again through a `dup`
//! of the same descriptor is not a conflict with itself.
//!
//! Fixed pids live in 47_38x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `open(2)` flags, as `<asm-generic/fcntl.h>` gives them.
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
/// `O_DIRECT` on x86_64. This is `pipe2`'s packet mode, which this kernel
/// does not have, so it is the bit that must not be masked away.
const O_DIRECT: usize = 0o40000;

/// `fcntl(2)` commands. `F_DUPFD_CLOEXEC` is `F_LINUX_SPECIFIC_BASE + 6`.
const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_DUPFD_CLOEXEC: usize = 1024 + 6;
/// `FD_CLOEXEC`, the per-descriptor flag `F_SETFD` sets. It is 1, and not
/// `O_CLOEXEC`: the two live in different words.
const FD_CLOEXEC: usize = 1;

/// `flock(2)` operations.
const LOCK_SH: usize = 1;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const LOCK_UN: usize = 8;

/// A flag `dup3` does not have. It has exactly one, `O_CLOEXEC`.
const A_FLAG_DUP3_DOES_NOT_HAVE: usize = O_RDWR;

/// `(int)-1` as it arrives in the register, and a target past 32 bits whose
/// low half is descriptor 1.
const MINUS_ONE: usize = usize::MAX;
const PAST_THE_REGISTER: usize = (1 << 32) | 1;

/// A process with a `/<top>` of its own, because a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A path as userspace writes one: NUL-terminated.
fn c(s: &str) -> Vec<u8> {
    let mut bytes = Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
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
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

fn open(sys: &Syscall<'_>, path: &[u8], flags: usize) -> SysResult {
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        flags,
        0o644,
    )
}

async fn fcntl(sys: &Syscall<'_>, fd: usize, cmd: usize, arg: usize) -> SysResult {
    sys.sys_fcntl(FileDesc::from(fd), cmd, arg).await
}

/// Whether this DESCRIPTOR is close-on-exec, asked through the syscall that
/// a program asks with.
async fn is_cloexec(sys: &Syscall<'_>, fd: usize) -> Result<bool, LxError> {
    Ok(fcntl(sys, fd, F_GETFD, 0).await? & FD_CLOEXEC != 0)
}

async fn write_to(sys: &Syscall<'_>, fd: usize, bytes: &[u8]) -> SysResult {
    sys.sys_write(
        FileDesc::from(fd),
        UserInPtr::from(bytes.as_ptr() as usize),
        bytes.len(),
    )
    .await
}

/// Read `len` bytes and hand back what was read, which is where a shared
/// cursor shows.
async fn read_from(sys: &Syscall<'_>, fd: usize, len: usize) -> Result<Vec<u8>, LxError> {
    let mut buf = alloc::vec![0u8; len];
    let got = sys
        .sys_read(
            FileDesc::from(fd),
            UserOutPtr::from(buf.as_mut_ptr() as usize),
            len,
        )
        .await?;
    buf.truncate(got);
    Ok(buf)
}

/// The eight bytes an eventfd read hands back, as the number they are. Two
/// eventfds with different counts are how a replace is told from a no-op.
async fn read_a_count(sys: &Syscall<'_>, fd: usize) -> Result<u64, LxError> {
    let eight = read_from(sys, fd, 8).await?;
    assert_eq!(eight.len(), 8, "an eventfd read is eight bytes or nothing");
    let mut whole = [0u8; 8];
    whole.copy_from_slice(&eight);
    Ok(u64::from_ne_bytes(whole))
}

/// The two numbers `pipe2` wrote, read back as userspace reads them.
fn a_pair() -> [i32; 2] {
    [-1; 2]
}

fn pipe2(sys: &Syscall<'_>, fds: &mut [i32; 2], flags: usize) -> SysResult {
    sys.sys_pipe2(UserOutPtr::from(fds as *mut [i32; 2] as usize), flags)
}

async fn flock(sys: &Syscall<'_>, fd: usize, operation: usize) -> SysResult {
    sys.sys_flock(FileDesc::from(fd), operation).await
}

#[test]
fn a_copy_of_a_descriptor_is_a_second_name_for_one_open_file() {
    // The whole reason `dup` exists, and the bug that made `prog >log 2>&1`
    // write the file twice from offset zero: the copy has to share the
    // cursor. Asked of a file with ten known bytes, four at a time through
    // alternating descriptors, so a kernel that gave each its own cursor
    // hands back the same four twice.
    let proc = a_process(47_380, "dup-share", "dup_share");
    let dir = String::from("/dup_share");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = c(&alloc::format!("{dir}/f"));
            let one = open(sys, &path, O_RDWR | O_CREAT).unwrap();
            assert_eq!(write_to(sys, one, b"0123456789").await, Ok(10));
            // Back to the start, so the reads below are the test and not
            // what the write left behind.
            assert_eq!(sys.sys_lseek(FileDesc::from(one), 0, 0), Ok(0));

            let copy = sys.sys_dup(FileDesc::from(one)).unwrap();
            assert_eq!(read_from(sys, one, 4).await.as_deref(), Ok(&b"0123"[..]));
            assert_eq!(
                read_from(sys, copy, 4).await.as_deref(),
                Ok(&b"4567"[..]),
                "the copy has a cursor of its own, so it is a second file"
            );
            // And the status flags are shared too, which is the other half
            // of one description: `F_SETFL` through one is visible through
            // the other. `F_GETFL`'s answer carries the access mode, so it
            // is compared with itself rather than with a literal.
            const F_GETFL: usize = 3;
            const F_SETFL: usize = 4;
            const O_APPEND: usize = 0o2000;
            let before = fcntl(sys, copy, F_GETFL, 0).await.unwrap();
            assert_eq!(fcntl(sys, one, F_SETFL, O_APPEND).await, Ok(0));
            assert_eq!(
                fcntl(sys, copy, F_GETFL, 0).await,
                Ok(before | O_APPEND),
                "the status flags are not shared, so it is a second file"
            );

            // `dup2` and `F_DUPFD` are the same claim by another name.
            let at_twenty = sys.sys_dup2(FileDesc::from(one), 20).unwrap();
            assert_eq!(at_twenty, 20);
            assert_eq!(
                read_from(sys, at_twenty, 2).await.as_deref(),
                Ok(&b"89"[..])
            );
            let by_fcntl = fcntl(sys, one, F_DUPFD, 0).await.unwrap();
            assert_eq!(
                read_from(sys, by_fcntl, 4).await.as_deref(),
                Ok(&[][..]),
                "the file is at its end for every name it has"
            );
        })
    });
}

#[test]
fn what_a_new_descriptor_gets_of_its_own_is_close_on_exec() {
    // `FD_CLOEXEC` lives in the per-process table, never in the shared
    // object, so it is the one thing that is NOT shared: `dup` of a
    // close-on-exec descriptor gives one that is not, `dup3` and
    // `F_DUPFD_CLOEXEC` set it on the new descriptor alone, and the
    // original keeps what it had. A kernel that read the flag off the
    // object would give the same answer for every name of one file.
    let proc = a_process(47_381, "dup-cloexec", "dup_cloexec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let one = sys.sys_eventfd2(1, 0).unwrap();
            assert_eq!(fcntl(sys, one, F_SETFD, FD_CLOEXEC).await, Ok(0));
            assert_eq!(is_cloexec(sys, one).await, Ok(true));

            // POSIX: `dup` and `dup2` clear it on the new descriptor.
            let plain = sys.sys_dup(FileDesc::from(one)).unwrap();
            assert_eq!(
                is_cloexec(sys, plain).await,
                Ok(false),
                "dup carried the flag over, so it was read off the object"
            );
            let at_twenty = sys.sys_dup2(FileDesc::from(one), 20).unwrap();
            assert_eq!(is_cloexec(sys, at_twenty).await, Ok(false));
            // And `dup3` is the whole reason that syscall exists: the flag
            // is set in the same table lock that installs the descriptor.
            let with_flag = sys.sys_dup3(FileDesc::from(one), 21, ANON_CLOEXEC).unwrap();
            assert_eq!(
                is_cloexec(sys, with_flag).await,
                Ok(true),
                "dup3 did not set the flag it was asked for"
            );
            let by_fcntl = fcntl(sys, one, F_DUPFD_CLOEXEC, 0).await.unwrap();
            assert_eq!(
                is_cloexec(sys, by_fcntl).await,
                Ok(true),
                "F_DUPFD_CLOEXEC is F_DUPFD with the flag, and it was dropped"
            );
            // F_DUPFD is the same call without it.
            let without = fcntl(sys, one, F_DUPFD, 0).await.unwrap();
            assert_eq!(is_cloexec(sys, without).await, Ok(false));
            // Through all of that the original is untouched.
            assert_eq!(
                is_cloexec(sys, one).await,
                Ok(true),
                "a copy changed the flag of the descriptor it was copied from"
            );
        })
    });
}

#[test]
fn dup2_closes_whatever_was_at_its_target() {
    // A replace, not an insert: the description that was at the target is
    // dropped and the target names the source afterwards. Two eventfds with
    // different counts, so the answer cannot be right by accident, and the
    // count is read through the TARGET number.
    let proc = a_process(47_382, "dup2-replace", "dup2_replace");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // Non-blocking, because the last read of this test is of an
            // eventfd that the one before it emptied, and a blocking one
            // would park there for ever instead of saying so.
            let source = sys.sys_eventfd2(3, ANON_NONBLOCK).unwrap();
            let target = sys.sys_eventfd2(7, ANON_NONBLOCK).unwrap();
            assert_ne!(source, target);

            assert_eq!(
                sys.sys_dup2(FileDesc::from(source), target),
                Ok(target),
                "dup2 does not answer with its target"
            );
            assert_eq!(
                read_a_count(sys, target).await,
                Ok(3),
                "the target still names what was there, so nothing was replaced"
            );
            // And the source is the same description, so the read above
            // emptied it for both names.
            assert_eq!(read_a_count(sys, source).await, Err(LxError::EAGAIN));
        })
    });
}

#[test]
fn dup2_of_a_descriptor_onto_itself_changes_nothing_and_dup3_refuses_it() {
    // The one case where the two calls differ on purpose. `dup2(fd, fd)`
    // answers `fd` and does nothing at all -- not even clear the flag it
    // clears for every other target -- while `dup3(fd, fd, ..)` is EINVAL,
    // because the atomic `O_CLOEXEC` it exists for would have nowhere to go.
    // Both still answer EBADF for a descriptor nobody opened.
    let proc = a_process(47_383, "dup-self", "dup_self");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = sys.sys_eventfd2(1, 0).unwrap();
            assert_eq!(fcntl(sys, fd, F_SETFD, FD_CLOEXEC).await, Ok(0));

            assert_eq!(sys.sys_dup2(FileDesc::from(fd), fd), Ok(fd));
            assert_eq!(
                is_cloexec(sys, fd).await,
                Ok(true),
                "dup2 onto itself cleared the flag, which is not a no-op"
            );
            assert_eq!(
                sys.sys_dup3(FileDesc::from(fd), fd, 0),
                Err(LxError::EINVAL),
                "dup3 took a target equal to its source"
            );
            assert_eq!(
                sys.sys_dup3(FileDesc::from(fd), 20, A_FLAG_DUP3_DOES_NOT_HAVE),
                Err(LxError::EINVAL),
                "dup3 took a flag it does not have"
            );
            // A descriptor nobody opened, as both the source and the target.
            let free = usize::from(sys.linux_process().get_free_fd());
            assert_eq!(
                sys.sys_dup2(FileDesc::from(free), free),
                Err(LxError::EBADF),
                "dup2 onto itself did not check the descriptor is open"
            );
            assert_eq!(sys.sys_dup(FileDesc::from(free)), Err(LxError::EBADF));
        })
    });
}

#[test]
fn the_target_of_dup2_is_judged_as_the_whole_register() {
    // `ksys_dup3` answers EBADF to a target at or past `RLIMIT_NOFILE`
    // before it looks at anything else, and the check has to see the whole
    // word: narrowing to an `int` first turned `1 << 32 | 1` into
    // descriptor 1 -- which a shell has its stdout on -- and `-1` into a
    // table entry at -1. Descriptor 1 is given something recognisable here
    // so that a fold onto it is visible and not just EBADF.
    let proc = a_process(47_384, "dup2-register", "dup2_register");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let marker = sys.sys_eventfd2(5, 0).unwrap();
            assert_eq!(sys.sys_dup2(FileDesc::from(marker), 1), Ok(1));
            let source = sys.sys_eventfd2(9, 0).unwrap();

            assert_eq!(
                sys.sys_dup2(FileDesc::from(source), PAST_THE_REGISTER),
                Err(LxError::EBADF)
            );
            assert_eq!(
                sys.sys_dup2(FileDesc::from(source), MINUS_ONE),
                Err(LxError::EBADF)
            );
            assert_eq!(
                sys.sys_dup3(FileDesc::from(source), PAST_THE_REGISTER, 0),
                Err(LxError::EBADF)
            );
            assert_eq!(
                read_a_count(sys, 1).await,
                Ok(5),
                "a target past 32 bits folded onto descriptor 1 and clobbered it"
            );
        })
    });
}

#[test]
fn f_dupfd_takes_the_lowest_free_descriptor_at_or_above_its_start() {
    // What `F_DUPFD` is for: not a number of the caller's choosing, but the
    // lowest free one from a floor it names. The floor is checked against
    // `RLIMIT_NOFILE` the way `f_dupfd` does, and a start of `-1` -- which
    // is how every "no minimum" mistake spells it -- used to reach the
    // allocator as `usize::MAX`.
    let proc = a_process(47_385, "dupfd-start", "dupfd_start");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = sys.sys_eventfd2(1, 0).unwrap();
            let hole = sys.sys_dup(FileDesc::from(fd)).unwrap();
            let above = sys.sys_dup(FileDesc::from(fd)).unwrap();
            assert!(hole < above, "{} {}", hole, above);
            assert_eq!(sys.sys_close(FileDesc::from(hole)), Ok(0));

            assert_eq!(
                fcntl(sys, fd, F_DUPFD, 0).await,
                Ok(hole),
                "F_DUPFD did not take the lowest free descriptor"
            );
            assert_eq!(
                fcntl(sys, fd, F_DUPFD, above + 1).await,
                Ok(above + 1),
                "F_DUPFD did not start where it was told to"
            );
            // The floor itself, judged as the whole register.
            assert_eq!(
                fcntl(sys, fd, F_DUPFD, MINUS_ONE).await,
                Err(LxError::EINVAL)
            );
            assert_eq!(
                fcntl(sys, fd, F_DUPFD, 1024).await,
                Err(LxError::EINVAL),
                "a start at the limit is EINVAL, not a descriptor"
            );
        })
    });
}

#[test]
fn a_pipe_hands_back_its_two_ends_in_the_order_the_pair_is_written() {
    // `pipe(2)` writes `pipefd[0]` first, the READ end, and the two ends are
    // each open the one way they work: a read of the write end is EBADF, as
    // is a write of the read end. Swapping the two numbers passes every
    // test that only writes then reads.
    let proc = a_process(47_386, "pipe-order", "pipe_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut fds = a_pair();
            assert_eq!(pipe2(sys, &mut fds, 0), Ok(0));
            let (read_end, write_end) = (fds[0] as usize, fds[1] as usize);
            assert!(read_end != write_end && fds[0] >= 0 && fds[1] >= 0);

            assert_eq!(write_to(sys, write_end, b"one message").await, Ok(11));
            assert_eq!(
                read_from(sys, read_end, 11).await.as_deref(),
                Ok(&b"one message"[..]),
                "nothing came out of the end the pair says is the reader"
            );
            assert_eq!(
                read_from(sys, write_end, 1).await,
                Err(LxError::EBADF),
                "the write end is readable, so the two ends are not in order"
            );
            assert_eq!(
                write_to(sys, read_end, b"x").await,
                Err(LxError::EBADF),
                "the read end is writable, so the two ends are not in order"
            );
        })
    });
}

#[test]
fn pipe2_is_held_to_the_two_flags_it_has_and_they_reach_both_ends() {
    // The sixth caller of the same flag word, and the one the sweep that
    // wrote `anon_fd_flags` missed: it masked the word down to the two bits
    // it knows and kept going, so `O_DIRECT` -- packet mode, which this
    // kernel does not have -- came back as a working byte-stream pipe with
    // the message boundaries silently gone. And `O_CLOEXEC` has to reach
    // BOTH descriptors, since a half-marked pair leaks one end past
    // `execve`.
    let proc = a_process(47_387, "pipe-flags", "pipe_flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let mut fds = a_pair();
            let free_before = lp.get_free_fd();

            assert_eq!(
                pipe2(sys, &mut fds, O_DIRECT),
                Err(LxError::EINVAL),
                "a pipe was given byte-stream semantics for a packet-mode ask"
            );
            assert_eq!(
                pipe2(sys, &mut fds, 1 << 20),
                Err(LxError::EINVAL),
                "a flag pipe2 does not have was ignored"
            );
            assert_eq!(fds, a_pair(), "a refused pipe2 wrote descriptors anyway");
            assert_eq!(
                lp.get_free_fd(),
                free_before,
                "a refused pipe2 left descriptors behind"
            );

            assert_eq!(pipe2(sys, &mut fds, ANON_CLOEXEC | ANON_NONBLOCK), Ok(0));
            for end in [fds[0] as usize, fds[1] as usize] {
                assert_eq!(
                    is_cloexec(sys, end).await,
                    Ok(true),
                    "O_CLOEXEC did not reach both ends of the pair"
                );
            }
            // And `O_NONBLOCK` reached the objects: an empty pipe answers
            // at once instead of parking.
            assert_eq!(
                read_from(sys, fds[0] as usize, 1).await,
                Err(LxError::EAGAIN),
                "O_NONBLOCK never reached the read end"
            );
        })
    });
}

#[test]
fn a_pipe_that_cannot_hand_over_its_pair_takes_both_ends_back() {
    // The descriptors belong to the caller only once it has their numbers.
    // If the write of the pair fails -- a bad pointer is the way that
    // happens -- they have to come back out of the table, or they sit there
    // with nothing left that can close them, and the process leaks two
    // descriptors per call until it hits `RLIMIT_NOFILE`.
    let proc = a_process(47_388, "pipe-rollback", "pipe_rollback");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let free_before = lp.get_free_fd();
            assert_eq!(
                sys.sys_pipe2(UserOutPtr::from(0), 0),
                Err(LxError::EFAULT),
                "a pair that cannot be written was not EFAULT"
            );
            assert_eq!(
                lp.get_free_fd(),
                free_before,
                "the two ends were left in the table with no way to close them"
            );
            // Twice, because the leak the rollback prevents is cumulative.
            assert_eq!(sys.sys_pipe2(UserOutPtr::from(0), 0), Err(LxError::EFAULT));
            assert_eq!(lp.get_free_fd(), free_before);
        })
    });
}

#[test]
fn two_exclusive_flocks_on_one_file_do_not_both_succeed() {
    // It used to validate the request and answer 0 without taking
    // anything, so `flock(1)`, dpkg's and apt's frontend locks and every
    // `LOCK_EX | LOCK_NB` "is another instance running?" probe found the
    // file free every time. Two separate opens of one path are two
    // descriptions, which is what a second process would have.
    let proc = a_process(47_389, "flock-conflict", "flock_conflict");
    let dir = String::from("/flock_conflict");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = c(&alloc::format!("{dir}/lockfile"));
            let mine = open(sys, &path, O_RDWR | O_CREAT).unwrap();
            let theirs = open(sys, &path, O_RDWR).unwrap();

            assert_eq!(flock(sys, mine, LOCK_EX).await, Ok(0));
            assert_eq!(
                flock(sys, theirs, LOCK_EX | LOCK_NB).await,
                Err(LxError::EAGAIN),
                "the file was free although it was locked"
            );
            // A shared lock is refused by an exclusive one too.
            assert_eq!(
                flock(sys, theirs, LOCK_SH | LOCK_NB).await,
                Err(LxError::EAGAIN)
            );
            // And once it is released the next one gets it.
            assert_eq!(flock(sys, mine, LOCK_UN).await, Ok(0));
            assert_eq!(flock(sys, theirs, LOCK_EX | LOCK_NB).await, Ok(0));
            // Two shared locks are not a conflict, and an exclusive one on
            // top of a shared one is.
            assert_eq!(flock(sys, theirs, LOCK_SH).await, Ok(0));
            assert_eq!(flock(sys, mine, LOCK_SH | LOCK_NB).await, Ok(0));
            assert_eq!(
                flock(sys, theirs, LOCK_EX | LOCK_NB).await,
                Err(LxError::EAGAIN),
                "an exclusive lock was given over somebody else's shared one"
            );
        })
    });
}

#[test]
fn a_flock_is_held_by_the_open_file_description_and_not_by_the_descriptor() {
    // Which is why the `dup` family and `flock` belong in one file: the
    // holder is the `Arc<File>` behind the descriptor, so a copy of the
    // descriptor is the SAME holder. Asking again through the copy is not a
    // conflict with itself, and a release through the copy releases the
    // one lock there is. A kernel that keyed the lock by descriptor number
    // would deadlock a program that locks through one name and unlocks
    // through another.
    let proc = a_process(47_390, "flock-owner", "flock_owner");
    let dir = String::from("/flock_owner");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = c(&alloc::format!("{dir}/lockfile"));
            let mine = open(sys, &path, O_RDWR | O_CREAT).unwrap();
            let copy = sys.sys_dup(FileDesc::from(mine)).unwrap();
            let theirs = open(sys, &path, O_RDWR).unwrap();

            assert_eq!(flock(sys, mine, LOCK_EX).await, Ok(0));
            assert_eq!(
                flock(sys, copy, LOCK_EX | LOCK_NB).await,
                Ok(0),
                "a descriptor conflicted with its own copy, which is one holder"
            );
            assert_eq!(
                flock(sys, theirs, LOCK_EX | LOCK_NB).await,
                Err(LxError::EAGAIN)
            );
            assert_eq!(
                flock(sys, copy, LOCK_UN).await,
                Ok(0),
                "a release through a copy is a release of the one lock"
            );
            assert_eq!(flock(sys, theirs, LOCK_EX | LOCK_NB).await, Ok(0));
        })
    });
}

#[test]
fn flock_wants_a_descriptor_with_a_file_behind_it() {
    // The lock is keyed by the file's device and inode, so a descriptor
    // with no file behind it has nothing to key on: an eventfd is EBADF,
    // which is also what Linux answers for an anonymous inode nobody can
    // lock. A descriptor nobody opened is the same answer.
    let proc = a_process(47_391, "flock-what", "flock_what");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let anonymous = sys.sys_eventfd2(1, 0).unwrap();
            assert_eq!(flock(sys, anonymous, LOCK_EX).await, Err(LxError::EBADF));
            let free = usize::from(sys.linux_process().get_free_fd());
            assert_eq!(flock(sys, free, LOCK_EX).await, Err(LxError::EBADF));
            // And an operation that names no command, or two at once, is
            // EINVAL before any of that: the flag word is judged first.
            assert_eq!(flock(sys, free, 0).await, Err(LxError::EINVAL));
            assert_eq!(
                flock(sys, free, LOCK_EX | LOCK_SH).await,
                Err(LxError::EINVAL)
            );
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // All of them by number, through the table, which is the only thing
    // userspace does. `pipe` is `pipe2` with no flags and takes one
    // register; `dup3` takes three and `flock` two.
    let proc = a_process(47_392, "dup-by-number", "dup_by_number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let source = sys.sys_eventfd2(4, 0).unwrap();

            // `dup(oldfd)`.
            let copy = by_number(sys, crate::Sys::DUP, [source, 0, 0, 0, 0, 0]).await;
            assert!(copy >= 0, "dup by number: {}", copy);
            // `dup2(oldfd, newfd)`: the target is the second register, and
            // the answer is the target.
            assert_eq!(
                by_number(sys, crate::Sys::DUP2, [source, 30, 0, 0, 0, 0]).await,
                30
            );
            // `dup3(oldfd, newfd, flags)`: three registers, and the flag
            // word is the third.
            assert_eq!(
                by_number(sys, crate::Sys::DUP3, [source, 31, ANON_CLOEXEC, 0, 0, 0]).await,
                31
            );
            assert_eq!(is_cloexec(sys, 31).await, Ok(true));
            assert_eq!(
                read_a_count(sys, 31).await,
                Ok(4),
                "the three descriptors do not name the eventfd that was dup'ed"
            );

            // `pipe(fds)` and `pipe2(fds, flags)`.
            let mut fds = a_pair();
            let at = &mut fds as *mut [i32; 2] as usize;
            assert_eq!(
                by_number(sys, crate::Sys::PIPE, [at, 0, 0, 0, 0, 0]).await,
                0
            );
            let plain = [fds[0] as usize, fds[1] as usize];
            assert_eq!(is_cloexec(sys, plain[0]).await, Ok(false));
            assert_eq!(
                by_number(sys, crate::Sys::PIPE2, [at, ANON_CLOEXEC, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(is_cloexec(sys, fds[0] as usize).await, Ok(true));
            assert_ne!(
                fds[0] as usize, plain[0],
                "the second pipe2 wrote the same pair as the first"
            );

            // `flock(fd, operation)`: the operation is the second register,
            // and reading it as a descriptor would lock the wrong thing.
            assert_eq!(
                by_number(sys, crate::Sys::FLOCK, [plain[0], LOCK_EX, 0, 0, 0, 0]).await,
                0
            );
            // `fcntl(fd, cmd, arg)`: three registers, and `F_DUPFD`'s floor
            // is the third.
            let picked = by_number(
                sys,
                crate::Sys::FCNTL,
                [source, F_DUPFD_CLOEXEC, 40, 0, 0, 0],
            )
            .await;
            assert_eq!(picked, 40, "F_DUPFD_CLOEXEC by number: {}", picked);
            assert_eq!(is_cloexec(sys, 40).await, Ok(true));
        })
    });
}
