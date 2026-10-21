//! A syscall number reaches the handler that number names.
//!
//! Every other test in this crate calls a handler by its name -- `sys_read`,
//! `sys_getuid` -- which is the one thing userspace cannot do. What userspace
//! has is a number, and between the number and the handler sits a table of
//! 241 one-line arms typed out by hand.
//!
//! A mutation lot that gave each arm of that table the handler of the arm
//! below it, restricted to the 36 neighbouring pairs whose argument shapes
//! match so the swap compiles, left **all 36 alive** against the 1338 tests
//! of this crate. Nothing here would have noticed `Sys::READ` calling
//! `sys_write`, `Sys::GETUID` answering the group id, or `Sys::SETUID`
//! changing the group instead of the user. (The harness was not asleep: a
//! control mutation on `unknown_syscall_number` failed two tests by name.)
//!
//! So these tests make the call the way a program does, by number, and ask
//! for an answer only the right handler can give. Two handlers next to each
//! other in that table usually take the same arguments and fail the same way
//! on a bad one, so an errno does not tell them apart: what does is a piece
//! of state set up to be **different** on each side.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use linux_object::thread::ThreadExt;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, Thread, ROOT_JOB};

/// Make syscall `num` the way userspace makes it: by number, through the
/// table, rather than by calling a handler.
///
/// `Syscall::syscall` wants `&mut self` and `on_a_thread` lends a shared one,
/// so this builds its own out of the same three parts. The thread is the same
/// thread, so the call lands in the same process.
async fn by_number(sys: &Syscall<'_>, num: u32, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num, args).await
}

fn c_path(path: &str) -> String {
    alloc::format!("{}\0", path)
}

/// A process at `pid` whose root holds a `/<top>` of its own.
///
/// Every test gets its own top directory: a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

// ---------------------------------------------------------------------------
// Who am I
// ---------------------------------------------------------------------------

// Four DIFFERENT numbers, on purpose. With `ruid == euid` or `rgid == egid`
// -- which is every process in the rest of this suite -- `getuid` answering
// the effective id is the same answer, and there is nothing to see. That is
// exactly why the table went untested.
const RUID: u32 = 1001;
const EUID: u32 = 1002;
const RGID: u32 = 1003;
const EGID: u32 = 1004;

#[test]
fn each_identity_number_answers_its_own_identity() {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    // The groups first: moving the user ids takes the effective uid off root,
    // and after that nothing else may be set.
    lp.set_resgid(RGID, EGID, RGID).unwrap();
    lp.set_resuid(RUID, EUID, RUID).unwrap();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, 45_501, "dispatch-ids", lp).unwrap();
    // A thread-group leader is given a tid equal to the pid, on purpose and
    // as Linux does, so on a single-threaded process `getpid` and `gettid`
    // are the same answer and a swap between them is invisible. This leader
    // never runs; it is here so the thread the call is made on is the
    // process's SECOND, with a KoID of its own.
    let _leader = Thread::create_linux(&proc).unwrap();
    let (got, tid) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = Vec::new();
            for num in [
                Sys::GETPID as u32,
                Sys::GETTID as u32,
                Sys::GETPPID as u32,
                Sys::GETUID as u32,
                Sys::GETEUID as u32,
                Sys::GETGID as u32,
                Sys::GETEGID as u32,
            ] {
                got.push(by_number(sys, num, [0; 6]).await);
            }
            (got, sys.thread.id() as isize)
        })
    });

    // The process id is fixed; the thread's is a fresh KoID, which starts
    // above `1 << 32`, so the two can never collide by accident.
    assert_eq!(got[0], 45_501, "getpid did not answer the process id");
    assert_eq!(got[1], tid, "gettid did not answer the thread id");
    assert_ne!(
        got[0], got[1],
        "this test cannot tell getpid from gettid unless they differ"
    );
    // Nobody forked this process, so it has no parent. `setsid`, the arm
    // under `getppid`, would answer a session id instead.
    assert_eq!(
        got[2], 0,
        "getppid answered something for a process with no parent"
    );

    for (want, name, at) in [
        (RUID, "getuid", 3usize),
        (EUID, "geteuid", 4),
        (RGID, "getgid", 5),
        (EGID, "getegid", 6),
    ] {
        assert_eq!(got[at], want as isize, "{} answered the wrong id", name);
    }
}

#[test]
fn setting_the_user_does_not_set_the_group() {
    // The dangerous half of the same table. This process is root, so `setuid`
    // is allowed and sets all three user ids; the group ids must be where
    // they were. `setgid`, the arm under it, would move those instead -- and
    // a process that asked to drop its user privilege and dropped its group
    // privilege instead is still root.
    const NEW: u32 = 1005;
    let proc = a_process(45_502, "dispatch-setuid", "dispatch_setuid");
    let (ret, uid, euid, gid, egid) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let ret = by_number(sys, Sys::SETUID as u32, [NEW as usize, 0, 0, 0, 0, 0]).await;
            let p = sys.linux_process();
            (ret, p.uid(), p.euid(), p.gid(), p.egid())
        })
    });
    assert_eq!(ret, 0, "setuid failed");
    assert_eq!(uid, NEW, "setuid did not move the real user id");
    assert_eq!(euid, NEW, "setuid did not move the effective user id");
    assert_eq!(
        gid, 0,
        "setuid moved the real group id, which is setgid's job"
    );
    assert_eq!(egid, 0, "setuid moved the effective group id");
}

#[test]
fn setting_the_three_user_ids_does_not_set_the_group_ids() {
    // `setresuid(ruid, euid, suid)` and `setresgid` have the same three
    // arguments, so the swap compiles and every argument still means
    // something. The three values are distinct so a swap cannot land on the
    // same answer.
    let proc = a_process(45_503, "dispatch-setresuid", "dispatch_setresuid");
    let (ret, uid, euid, suid, gid, egid) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let ret = by_number(sys, Sys::SETRESUID as u32, [1006, 1007, 1008, 0, 0, 0]).await;
            let p = sys.linux_process();
            (ret, p.uid(), p.euid(), p.suid(), p.gid(), p.egid())
        })
    });
    assert_eq!(ret, 0, "setresuid failed");
    assert_eq!((uid, euid, suid), (1006, 1007, 1008), "the three user ids");
    assert_eq!(
        (gid, egid),
        (0, 0),
        "setresuid moved the group ids, which is setresgid's job"
    );
}

// ---------------------------------------------------------------------------
// Moving bytes in the right direction
// ---------------------------------------------------------------------------

#[test]
fn a_read_is_not_a_write() {
    // `read` and `write` are the first two arms of the table and take the same
    // three arguments, so swapping them compiles and both answer EBADF on a
    // bad descriptor. What tells them apart is which side ends up holding the
    // bytes.
    let proc = a_process(45_504, "dispatch-rw", "dispatch_rw");
    let (read_ret, buf, after_read, write_ret, after_write) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let path = c_path("/dispatch_rw/f");
            let fd = sys
                .sys_openat(
                    FileDesc::CWD,
                    UserInPtr::from(path.as_ptr() as usize),
                    0o102, // O_RDWR | O_CREAT
                    0o600,
                )
                .unwrap();
            let inode = sys.linux_process().lookup_inode("/dispatch_rw/f").unwrap();
            inode.write_at(0, b"abc").unwrap();

            // The buffer starts as something the file does not hold, so
            // "nothing happened" and "the bytes arrived" are different.
            let mut buf = *b"...";
            let read_ret = by_number(
                sys,
                Sys::READ as u32,
                [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
            )
            .await;
            let mut after_read = [0u8; 3];
            inode.read_at(0, &mut after_read).unwrap();

            // And the other way: a write must land in the file.
            let out = *b"xyz";
            sys.sys_lseek(FileDesc::from(fd as i32), 0, 0).unwrap();
            let write_ret = by_number(
                sys,
                Sys::WRITE as u32,
                [fd, out.as_ptr() as usize, out.len(), 0, 0, 0],
            )
            .await;
            let mut after_write = [0u8; 3];
            inode.read_at(0, &mut after_write).unwrap();
            (read_ret, buf, after_read, write_ret, after_write)
        })
    });

    assert_eq!(read_ret, 3, "read did not answer three bytes");
    assert_eq!(&buf, b"abc", "read left the buffer alone");
    assert_eq!(
        &after_read, b"abc",
        "read changed the file, which is what write is for"
    );
    assert_eq!(write_ret, 3, "write did not answer three bytes");
    assert_eq!(&after_write, b"xyz", "write did not reach the file");
}

#[test]
fn a_vectored_read_is_not_a_vectored_write() {
    // The same pair one level up, where the buffer arrives as an `iovec`
    // array: `{ base, len }` per entry.
    let proc = a_process(45_505, "dispatch-rwv", "dispatch_rwv");
    let (ret, buf, after) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let path = c_path("/dispatch_rwv/f");
            let fd = sys
                .sys_openat(
                    FileDesc::CWD,
                    UserInPtr::from(path.as_ptr() as usize),
                    0o102,
                    0o600,
                )
                .unwrap();
            let inode = sys.linux_process().lookup_inode("/dispatch_rwv/f").unwrap();
            inode.write_at(0, b"hello").unwrap();

            let mut buf = *b".....";
            let iov = [[buf.as_mut_ptr() as usize, buf.len()]];
            let ret = by_number(
                sys,
                Sys::READV as u32,
                [fd, iov.as_ptr() as usize, iov.len(), 0, 0, 0],
            )
            .await;
            let mut after = [0u8; 5];
            inode.read_at(0, &mut after).unwrap();
            (ret, buf, after)
        })
    });

    assert_eq!(ret, 5, "readv did not answer five bytes");
    assert_eq!(&buf, b"hello", "readv left the buffer alone");
    assert_eq!(
        &after, b"hello",
        "readv changed the file, which is what writev is for"
    );
}

// ---------------------------------------------------------------------------
// Reading a value is not setting it
// ---------------------------------------------------------------------------

#[test]
fn reading_a_clock_is_not_setting_it() {
    // A getter writes the buffer and a setter reads it, so the buffer starts
    // holding a value the setter must REFUSE: a nanosecond field of two
    // thousand million is not a time, and `clock_settime` rejects it before
    // it asks who is calling. The getter overwrites it. Nothing here can set
    // the clock, so nothing here can disturb another test.
    const BAD_NSEC: usize = 2_000_000_000;
    const BAD_USEC: usize = 2_000_000;
    let proc = a_process(45_506, "dispatch-clock", "dispatch_clock");
    let (ts_ret, ts, tv_ret, tv) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut ts = [0usize, BAD_NSEC];
            let ts_ret = by_number(
                sys,
                Sys::CLOCK_GETTIME as u32,
                [0, ts.as_mut_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            let mut tv = [0usize, BAD_USEC];
            let tv_ret = by_number(
                sys,
                Sys::GETTIMEOFDAY as u32,
                [tv.as_mut_ptr() as usize, 0, 0, 0, 0, 0],
            )
            .await;
            (ts_ret, ts, tv_ret, tv)
        })
    });

    assert_eq!(ts_ret, 0, "clock_gettime failed");
    assert!(
        ts[1] < 1_000_000_000,
        "clock_gettime left the buffer as it was, so it read it instead of writing it: {:?}",
        ts
    );
    assert_eq!(tv_ret, 0, "gettimeofday failed");
    assert!(
        tv[1] < 1_000_000,
        "gettimeofday left the buffer as it was: {:?}",
        tv
    );
}

#[test]
fn reading_a_limit_is_not_setting_it() {
    // Same shape, same trick: a soft limit above the hard one is not a limit,
    // and `setrlimit` refuses it, so if the buffer comes back as it went the
    // call read it instead of writing it. RLIMIT_NOFILE is 7.
    const NOFILE: usize = 7;
    let proc = a_process(45_507, "dispatch-rlimit", "dispatch_rlimit");
    let (ret, lim) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut lim = [u64::MAX - 1, 0u64];
            let ret = by_number(
                sys,
                Sys::GETRLIMIT as u32,
                [NOFILE, lim.as_mut_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            (ret, lim)
        })
    });

    assert_eq!(ret, 0, "getrlimit failed");
    assert!(
        lim[0] <= lim[1],
        "getrlimit left the buffer as it was, so it read it instead of writing it: {:?}",
        lim
    );
}

// ---------------------------------------------------------------------------
// A pathname is not a descriptor
// ---------------------------------------------------------------------------

#[test]
fn a_call_that_takes_a_pathname_is_not_the_one_that_takes_a_descriptor() {
    // `truncate`/`ftruncate`, `chdir`/`fchdir` and `statfs`/`fstatfs` are
    // three pairs of neighbours where the only difference is what the first
    // argument means. Both halves take the same registers, so the swap
    // compiles; and the pathname pointer read as a descriptor is a huge
    // number, so the wrong half answers EBADF and the right one answers zero.
    let proc = a_process(45_508, "dispatch-paths", "dispatch_paths");
    let (truncate, size, chdir, cwd, statfs, wrote) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let f = sys
                .linux_process()
                .lookup_inode("/dispatch_paths")
                .unwrap()
                .create("f", FileType::File, 0o644)
                .unwrap();
            f.write_at(0, b"0123456789").unwrap();

            let path = c_path("/dispatch_paths/f");
            let truncate = by_number(
                sys,
                Sys::TRUNCATE as u32,
                [path.as_ptr() as usize, 4, 0, 0, 0, 0],
            )
            .await;
            let size = f.metadata().unwrap().size;

            let dir = c_path("/dispatch_paths");
            let chdir = by_number(
                sys,
                Sys::CHDIR as u32,
                [dir.as_ptr() as usize, 0, 0, 0, 0, 0],
            )
            .await;
            let cwd = sys.linux_process().current_working_directory();

            // `statfs` fills a 120-byte struct; the buffer is bigger and
            // starts zeroed, so "it was written" is visible.
            let mut buf = [0u64; 32];
            let statfs = by_number(
                sys,
                Sys::STATFS as u32,
                [dir.as_ptr() as usize, buf.as_mut_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            let wrote = buf.iter().any(|w| *w != 0);
            (truncate, size, chdir, cwd, statfs, wrote)
        })
    });

    assert_eq!(truncate, 0, "truncate by pathname failed");
    assert_eq!(size, 4, "truncate did not shorten the file");
    assert_eq!(chdir, 0, "chdir by pathname failed");
    assert_eq!(cwd, "/dispatch_paths", "chdir did not move the process");
    assert_eq!(statfs, 0, "statfs by pathname failed");
    assert!(wrote, "statfs wrote nothing");
}

/// `stat` follows a symbolic link and `lstat` reports the link itself; the
/// two are neighbours in the table with the same two arguments. Only x86_64
/// has them: the generic table has `fstatat` and nothing else.
#[cfg(target_arch = "x86_64")]
#[test]
fn stat_follows_the_link_and_lstat_reports_it() {
    // Linux's own numbers, not this kernel's names for them: `st_mode` is at
    // offset 24 of the x86_64 `struct stat` (dev, ino and nlink are eight
    // bytes each ahead of it), S_IFMT is 0o170000, a regular file 0o100000
    // and a symbolic link 0o120000. Reading the offset and the bits out of
    // the code under test would only prove it agrees with itself.
    const MODE_AT: usize = 24;
    const IFMT: u32 = 0o170_000;
    const IFREG: u32 = 0o100_000;
    const IFLNK: u32 = 0o120_000;
    let proc = a_process(45_509, "dispatch-stat", "dispatch_stat");
    let (stat_mode, lstat_mode) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let top = sys.linux_process().lookup_inode("/dispatch_stat").unwrap();
            top.create("f", FileType::File, 0o644).unwrap();
            let link = top.create("l", FileType::SymLink, 0o777).unwrap();
            link.write_at(0, b"f").unwrap();

            let mode_of = |num: u32, path: &str| {
                let path = c_path(path);
                async move {
                    let mut st = [0u64; 32];
                    let ret = by_number(
                        sys,
                        num,
                        [path.as_ptr() as usize, st.as_mut_ptr() as usize, 0, 0, 0, 0],
                    )
                    .await;
                    assert_eq!(ret, 0, "stat {:?} failed", path);
                    let bytes: Vec<u8> = st.iter().flat_map(|w| w.to_ne_bytes()).collect();
                    u32::from_ne_bytes([
                        bytes[MODE_AT],
                        bytes[MODE_AT + 1],
                        bytes[MODE_AT + 2],
                        bytes[MODE_AT + 3],
                    ])
                }
            };
            let stat_mode = mode_of(Sys::STAT as u32, "/dispatch_stat/l").await;
            let lstat_mode = mode_of(Sys::LSTAT as u32, "/dispatch_stat/l").await;
            (stat_mode, lstat_mode)
        })
    });

    assert_eq!(stat_mode & IFMT, IFREG, "stat did not follow the link");
    assert_eq!(
        lstat_mode & IFMT,
        IFLNK,
        "lstat followed the link, which is what stat is for"
    );
}

// ---------------------------------------------------------------------------
// The rest of the credential table
// ---------------------------------------------------------------------------

/// A process that is root, with user and group ids set to four different
/// numbers so no pair of them can answer for another.
///
/// The groups go first on purpose: moving the user ids takes the effective
/// uid off root, and after that nothing else may be set.
fn a_process_with_four_ids(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_resgid(RGID, EGID, RGID).unwrap();
    lp.set_resuid(RUID, EUID, RUID).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

#[test]
fn setting_two_user_ids_does_not_set_two_group_ids() {
    // `setreuid(ruid, euid)` and `setregid(rgid, egid)`: two registers each,
    // so the swap compiles, and both answer zero on success, so the return
    // value says nothing. Only where the numbers landed does.
    let proc = a_process(45_510, "dispatch-setreuid", "dispatch_setreuid");
    let (ret, ids) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let ret = by_number(sys, Sys::SETREUID as u32, [1010, 1011, 0, 0, 0, 0]).await;
            let p = sys.linux_process();
            (ret, (p.uid(), p.euid(), p.gid(), p.egid()))
        })
    });
    assert_eq!(ret, 0, "setreuid failed");
    assert_eq!(
        ids,
        (1010, 1011, 0, 0),
        "setreuid did not move the two user ids and leave the group ids alone"
    );
}

#[test]
fn the_three_user_ids_are_read_back_and_not_the_group_ids() {
    // `getresuid` and `getresgid` each write three out pointers. The buffer
    // starts holding a number no id here is, so "it was written" is visible,
    // and the four ids differ, so writing the group ones is visible too.
    const UNSET: u32 = 9_999;
    let proc = a_process_with_four_ids(45_511, "dispatch-getresuid");
    let (ret, got) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut ids = [UNSET; 3];
            let ret = by_number(
                sys,
                Sys::GETRESUID as u32,
                [
                    &mut ids[0] as *mut u32 as usize,
                    &mut ids[1] as *mut u32 as usize,
                    &mut ids[2] as *mut u32 as usize,
                    0,
                    0,
                    0,
                ],
            )
            .await;
            (ret, ids)
        })
    });
    assert_eq!(ret, 0, "getresuid failed");
    assert_eq!(
        got,
        [RUID, EUID, RUID],
        "getresuid answered the group ids, or wrote nothing at all"
    );
}

#[test]
fn asking_for_the_filesystem_user_id_is_not_asking_for_the_group_one() {
    // `setfsuid(-1)` is the pure query: `(uid_t)-1` is not a user id, so the
    // call changes nothing and answers the id it found. The filesystem ids
    // track the effective ones, which here are two different numbers, so the
    // answer alone says which of the two calls ran.
    let proc = a_process_with_four_ids(45_512, "dispatch-setfsuid");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(
            async move { by_number(sys, Sys::SETFSUID as u32, [usize::MAX, 0, 0, 0, 0, 0]).await },
        )
    });
    assert_eq!(
        got, EUID as isize,
        "setfsuid answered {}, and the filesystem GROUP id is {}",
        got, EGID
    );
}

#[test]
fn getting_the_supplementary_groups_does_not_set_them() {
    // `getgroups(size, list)` writes the list and answers how many; its
    // neighbour `setgroups(size, list)` reads the list and answers zero. The
    // buffer starts holding groups this process is not in, so a swap would
    // put it in them -- and that is a privilege change, which is why this
    // pair is worth a test of its own.
    const SEEDED: [u32; 2] = [2_001, 2_002];
    const JUNK: u32 = 3_003;
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_groups(SEEDED.to_vec());
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, 45_513, "dispatch-groups", lp).unwrap();
    let (ret, buf, after) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut buf = [JUNK; 4];
            let ret = by_number(
                sys,
                Sys::GETGROUPS as u32,
                [buf.len(), buf.as_mut_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            (ret, buf, sys.linux_process().groups())
        })
    });
    assert_eq!(ret, 2, "getgroups did not answer how many there are");
    assert_eq!(
        &buf[..2],
        &SEEDED,
        "getgroups did not write the list it was asked for"
    );
    assert_eq!(
        after,
        SEEDED.to_vec(),
        "getgroups changed which groups the process is in, which is setgroups' job"
    );
}

// ---------------------------------------------------------------------------
// Four more neighbours, four more ways to tell them apart
// ---------------------------------------------------------------------------

#[test]
fn setting_the_host_name_does_not_set_the_domain_name() {
    // `sethostname` and `setdomainname` take the same pointer and length and
    // both answer zero, so only which of the two names moved says which ran.
    // Both are machine-wide, so this test puts them back.
    let was_host = linux_object::uname::hostname();
    let was_domain = linux_object::uname::domainname();
    let proc = a_process(45_514, "dispatch-hostname", "dispatch_hostname");
    let (ret, host, domain) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let name = b"dispatch-host";
            let ret = by_number(
                sys,
                Sys::SETHOSTNAME as u32,
                [name.as_ptr() as usize, name.len(), 0, 0, 0, 0],
            )
            .await;
            (
                ret,
                linux_object::uname::hostname(),
                linux_object::uname::domainname(),
            )
        })
    });
    linux_object::uname::set_hostname(&was_host);
    linux_object::uname::set_domainname(&was_domain);

    assert_eq!(ret, 0, "sethostname failed");
    assert_eq!(
        host, "dispatch-host",
        "sethostname did not move the host name"
    );
    assert_eq!(
        domain, was_domain,
        "sethostname moved the domain name, which is setdomainname's job"
    );
}

#[test]
fn the_highest_real_time_priority_is_not_the_lowest() {
    // `sched_get_priority_max` and `_min` take a policy and answer a number,
    // and for the fair policies both answer zero -- so asked about
    // SCHED_OTHER the two are indistinguishable, which is how the pair
    // survived. SCHED_FIFO is where they differ: 99 and 1.
    const SCHED_FIFO: usize = 1;
    const SCHED_OTHER: usize = 0;
    let proc = a_process(45_515, "dispatch-sched", "dispatch_sched");
    let (max, min, other_max) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                by_number(
                    sys,
                    Sys::SCHED_GET_PRIORITY_MAX as u32,
                    [SCHED_FIFO, 0, 0, 0, 0, 0],
                )
                .await,
                by_number(
                    sys,
                    Sys::SCHED_GET_PRIORITY_MIN as u32,
                    [SCHED_FIFO, 0, 0, 0, 0, 0],
                )
                .await,
                by_number(
                    sys,
                    Sys::SCHED_GET_PRIORITY_MAX as u32,
                    [SCHED_OTHER, 0, 0, 0, 0, 0],
                )
                .await,
            )
        })
    });
    assert_eq!(max, 99, "the highest SCHED_FIFO priority");
    assert_eq!(min, 1, "the lowest SCHED_FIFO priority");
    assert_eq!(other_max, 0, "a fair policy has no real-time priority band");
}

#[test]
fn setting_a_limit_writes_the_limit_and_not_the_buffer() {
    // The other half of the pair above `getrlimit`: `setrlimit(resource,
    // rlim)` moves the limit, and `getrusage(who, usage)`, the arm under it,
    // only fills a buffer and leaves every limit where it was. So what says
    // which one ran is the limit afterwards, read back through the table as
    // well.
    const NOFILE: usize = 7;
    let proc = a_process(45_516, "dispatch-setrlimit", "dispatch_setrlimit");
    let (set, got) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let want = [100u64, 200];
            let set = by_number(
                sys,
                Sys::SETRLIMIT as u32,
                [NOFILE, want.as_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            let mut got = [0u64; 2];
            by_number(
                sys,
                Sys::GETRLIMIT as u32,
                [NOFILE, got.as_mut_ptr() as usize, 0, 0, 0, 0],
            )
            .await;
            (set, got)
        })
    });
    assert_eq!(set, 0, "setrlimit failed");
    assert_eq!(got, [100, 200], "setrlimit did not move the limit");
}

#[test]
fn asking_for_the_capabilities_does_not_drop_them() {
    // `capget(hdr, data)` writes `data`; `capset(hdr, data)`, the arm under
    // it, reads it and answers zero without writing anything. The buffer
    // starts empty, which as an argument to `capset` means "drop everything"
    // and is accepted, so a swap answers zero and leaves the buffer empty --
    // a program asking what it may do would be told "nothing".
    //
    // The header is Linux's own: `_LINUX_CAPABILITY_VERSION_3` is 0x20080522
    // and pid 0 means the caller.
    const V3: u32 = 0x2008_0522;
    let proc = a_process(45_517, "dispatch-capget", "dispatch_capget");
    let (ret, data) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // `{ version: u32, pid: i32 }` and then two sets of
            // `{ effective, permitted, inheritable }`.
            let mut hdr = [V3, 0];
            let mut data = [0u32; 6];
            let ret = by_number(
                sys,
                Sys::CAPGET as u32,
                [
                    hdr.as_mut_ptr() as usize,
                    data.as_mut_ptr() as usize,
                    0,
                    0,
                    0,
                    0,
                ],
            )
            .await;
            (ret, data)
        })
    });
    assert_eq!(ret, 0, "capget failed");
    assert!(
        data[0] != 0 || data[3] != 0,
        "capget left the buffer empty, so this root process was told it may do nothing: {:?}",
        data
    );
}
