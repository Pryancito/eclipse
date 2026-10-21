//! The record locks of `fcntl`, asked for the way a program asks for them.
//!
//! `fcntl_record_lock` had 29 of its 61 lines at zero, and the suite's one
//! test for it (`setlkw_tests`, next door) asks a single question: that a
//! kill gets a process out of an `F_SETLKW` wait. Everything else about it
//! was unexamined -- which is a lot of behaviour to leave unexamined, because
//! this is the lock every package manager, every browser profile and every
//! database takes, and a kernel that gets it subtly wrong hands the same byte
//! range to two writers.
//!
//! Three things in it are invisible to a test that does not go through the
//! syscall:
//!
//! 1. **`l_whence`**, which decides what `l_start` is counted from. The three
//!    bases are 0, the descriptor's own position, and the file's size, and
//!    nothing downstream can tell them apart: `resolve_range` is handed the
//!    base already resolved, so its own tests cannot see which of the three
//!    was picked. Swap `SEEK_CUR` for `SEEK_END` and every pure test still
//!    passes.
//! 2. **`get`**, the flag that says whether this is `F_GETLK` or one of the
//!    `F_SETLK` family. It appears once, in `F_UNLCK if !get`, and the arm it
//!    guards is the one that releases a range. Drop the guard and `F_GETLK`
//!    silently unlocks what it was asked to report on.
//! 3. **What `F_GETLK` writes back**, which is five fields of the caller's
//!    own `struct flock`: the conflicting lock's type, start, length and pid,
//!    and a `l_whence` forced to zero because the numbers it just wrote are
//!    absolute. A caller that trusts a stale `l_whence` there re-reads the
//!    range against its own position.
//!
//! The lock table is global and keyed by `(dev, inode)`, so every test here
//! brings an inode with a **dev of its own** -- its process id -- and can
//! neither see nor be seen by the lock any other test in the binary takes.
//! The conflicting side is a second, live process, because
//! `record_lock::conflicts` is false for a lock the asker already owns: a
//! process cannot collide with itself, so a one-process test can never
//! observe a refusal.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use core::any::Any;
use linux_object::fs::record_lock::{self, LockRequest, F_RDLCK, F_UNLCK, F_WRLCK};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileSystem, FileType, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `SEEK_SET`, `SEEK_CUR` and `SEEK_END` as `l_whence` carries them, and one
/// more that is not a whence at all.
const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;
const A_WHENCE_OF_NOBODY: i16 = 3;

/// `SEEK_SET` again, for `lseek`, which takes it as a `usize`.
const LSEEK_SET: usize = 0;

/// An `l_type` that is none of `F_RDLCK`, `F_WRLCK` or `F_UNLCK`.
const A_LOCK_TYPE_OF_NOBODY: i16 = 7;

/// How big the file claims to be, so `SEEK_END` lands somewhere nameable,
/// and where the descriptor is left sitting, so `SEEK_CUR` lands somewhere
/// else nameable. Neither is the other and neither is zero.
const THE_SIZE: usize = 100;
const THE_POSITION: usize = 50;

/// An inode with a `dev` of its own, which is what keeps one test's locks
/// out of every other test's: the table is keyed by `(dev, inode)` and
/// shared by the whole binary.
struct Keyed {
    dev: usize,
    size: usize,
}

impl Keyed {
    /// A descriptor on a file whose key nobody else in this binary uses,
    /// and the `(dev, inode)` the lock table will file it under.
    fn open(lp: &LinuxProcess, dev: usize, size: usize) -> (usize, record_lock::FileKey) {
        let fd: i32 = lp
            .add_file(File::new(
                Arc::new(Keyed { dev, size }),
                OpenFlags::RDWR,
                String::from("/keyed"),
            ))
            .unwrap()
            .into();
        (fd as usize, (dev, 1))
    }
}

impl rcore_fs::vfs::INode for Keyed {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(Metadata {
            dev: self.dev,
            inode: 1,
            size: self.size,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A process with a filesystem of its own.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A lock somebody else holds over `[start, end)`, seeded straight into the
/// table rather than through a syscall: what is under test is this side's
/// answer to it, not the other side's call.
fn somebody_else_holds(key: record_lock::FileKey, owner: KoID, exclusive: bool, range: (u64, u64)) {
    let req = LockRequest {
        exclusive,
        start: range.0,
        end: range.1,
        owner,
    };
    assert!(
        record_lock::setlk(key, &req, false),
        "the other process could not take [{}, {})",
        range.0,
        range.1
    );
}

/// Whether `owner` can take `[start, end)` right now, asked of the table and
/// not of the syscall under test.
fn somebody_else_could_take(key: record_lock::FileKey, owner: KoID, range: (u64, u64)) -> bool {
    record_lock::getlk(
        key,
        &LockRequest {
            exclusive: true,
            start: range.0,
            end: range.1,
            owner,
        },
    )
    .is_none()
}

/// A `struct flock` the way userspace fills one in.
fn a_lock(l_type: i16, l_whence: i16, l_start: i64, l_len: i64) -> Flock {
    Flock {
        l_type,
        l_whence,
        l_start,
        l_len,
        l_pid: 0,
    }
}

#[test]
fn the_base_of_a_range_is_the_whence_the_caller_sent() {
    // The three bases are zero, the descriptor's position and the file's
    // size, and this test puts a conflict on exactly one of them at a time
    // so that picking the wrong one changes the answer in BOTH directions:
    // a `SEEK_CUR` that read the size stops refusing, and a `SEEK_END` that
    // read the position starts refusing. Neither 50 nor 100 is zero and
    // neither is the other, so no two of the three can be confused.
    //
    // There is a lock one byte past each base too, so a base that is off by
    // one is caught as well.
    let proc = a_process(48_210, "whence");
    let holder = a_process(48_211, "whence-holder");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, key) = Keyed::open(&lp, 48_210, THE_SIZE);
            let fd = FileDesc::from(fd as i32);
            let who = holder.id();

            // One byte past zero, the position itself, and one byte past
            // the size. Nothing is seeded at [0, 1) or at [100, 101), which
            // is what the two successes below stand on.
            somebody_else_holds(key, who, true, (1, 2));
            somebody_else_holds(
                key,
                who,
                true,
                (THE_POSITION as u64, THE_POSITION as u64 + 1),
            );
            somebody_else_holds(key, who, true, (THE_SIZE as u64 + 1, THE_SIZE as u64 + 2));

            assert_eq!(sys.sys_lseek(fd, THE_POSITION as i64, LSEEK_SET), Ok(50));

            let set = a_lock(F_WRLCK, SEEK_SET, 0, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &set as *const Flock as usize)
                    .await,
                Ok(0),
                "SEEK_SET did not count from zero"
            );

            let cur = a_lock(F_WRLCK, SEEK_CUR, 0, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &cur as *const Flock as usize)
                    .await,
                Err(LxError::EAGAIN),
                "SEEK_CUR did not count from the descriptor's own position"
            );

            let end = a_lock(F_WRLCK, SEEK_END, 0, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &end as *const Flock as usize)
                    .await,
                Ok(0),
                "SEEK_END did not count from the size of the file"
            );

            let nowhere = a_lock(F_WRLCK, A_WHENCE_OF_NOBODY, 0, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &nowhere as *const Flock as usize)
                    .await,
                Err(LxError::EINVAL),
                "a whence that is not one of the three was accepted"
            );

            record_lock::release_owner(who);
        })
    });
}

#[test]
fn a_getlk_reports_the_lock_in_the_way_and_leaves_the_rest_of_the_struct_alone() {
    // `F_GETLK` is the only one of the family that writes back, and it
    // writes five fields: the type, start, length and pid of what is in the
    // way, and an `l_whence` of zero because those numbers are absolute and
    // the caller's own whence no longer applies to them.
    //
    // The whence sent in is `SEEK_END` on a zero-length file, which is the
    // same base as `SEEK_SET` and so changes no range -- it is there only so
    // that a `l_whence` that came back unchanged is visible.
    let proc = a_process(48_212, "getlk");
    let holder = a_process(48_213, "getlk-holder");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, key) = Keyed::open(&lp, 48_212, 0);
            let fd = FileDesc::from(fd as i32);
            let who = holder.id();
            somebody_else_holds(key, who, true, (10, 25));

            let mut fl = a_lock(F_WRLCK, SEEK_END, 0, 0);
            fl.l_pid = -7;
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &mut fl as *mut Flock as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                (fl.l_type, fl.l_whence, fl.l_start, fl.l_len, fl.l_pid),
                (F_WRLCK, SEEK_SET, 10, 15, who as i32),
                "F_GETLK did not describe the lock that is in the way"
            );

            // Asking about a range nothing covers leaves every field as it
            // was sent except the type, which becomes F_UNLCK: there is no
            // conflict to describe, so there is nothing to overwrite with.
            let mut clear = a_lock(F_WRLCK, SEEK_SET, 30, 5);
            clear.l_pid = -7;
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &mut clear as *mut Flock as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                (
                    clear.l_type,
                    clear.l_whence,
                    clear.l_start,
                    clear.l_len,
                    clear.l_pid
                ),
                (F_UNLCK, SEEK_SET, 30, 5, -7),
                "a free range was not reported as free, or was overwritten"
            );

            // A shared lock is reported as shared, and only to a writer:
            // two readers of the same range do not collide, which is the
            // whole point of F_RDLCK.
            record_lock::release_owner(who);
            somebody_else_holds(key, who, false, (40, 50));

            let mut shared = a_lock(F_WRLCK, SEEK_SET, 40, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &mut shared as *mut Flock as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                (shared.l_type, shared.l_start, shared.l_len),
                (F_RDLCK, 40, 10),
                "a shared lock was reported as something else"
            );

            let mut reader = a_lock(F_RDLCK, SEEK_SET, 40, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &mut reader as *mut Flock as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                reader.l_type, F_UNLCK,
                "a second reader was told it could not read"
            );

            // A lock held to the end of the file reports a length of zero,
            // which is how `struct flock` says "and anything appended".
            record_lock::release_owner(who);
            somebody_else_holds(key, who, true, (60, u64::MAX));

            let mut to_eof = a_lock(F_WRLCK, SEEK_SET, 60, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &mut to_eof as *mut Flock as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                (to_eof.l_start, to_eof.l_len),
                (60, 0),
                "a lock to the end of the file was given a length"
            );

            record_lock::release_owner(who);
        })
    });
}

#[test]
fn an_unlock_frees_the_range_and_a_getlk_is_never_an_unlock() {
    // `F_UNLCK` is served by one arm guarded by `if !get`, and that guard is
    // the only thing standing between `F_GETLK` and silently releasing the
    // range it was asked to report on. The two halves here are the same call
    // with the same struct, one as F_SETLK and one as F_GETLK, and the
    // second must refuse: there is no "report what an unlock would do".
    let proc = a_process(48_214, "unlock");
    let holder = a_process(48_215, "unlock-holder");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, key) = Keyed::open(&lp, 48_214, THE_SIZE);
            let fd = FileDesc::from(fd as i32);
            let who = holder.id();

            let mine = a_lock(F_WRLCK, SEEK_SET, 0, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &mine as *const Flock as usize)
                    .await,
                Ok(0)
            );
            assert!(
                !somebody_else_could_take(key, who, (0, 10)),
                "the lock this process just took is not in the table"
            );

            // Asking to report on the unlock must not perform it.
            let release = a_lock(F_UNLCK, SEEK_SET, 0, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, &release as *const Flock as usize)
                    .await,
                Err(LxError::EINVAL),
                "F_GETLK accepted F_UNLCK"
            );
            assert!(
                !somebody_else_could_take(key, who, (0, 10)),
                "F_GETLK released the range it was asked about"
            );

            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &release as *const Flock as usize)
                    .await,
                Ok(0),
                "F_SETLK refused to release this process's own lock"
            );
            assert!(
                somebody_else_could_take(key, who, (0, 10)),
                "the range is still held after it was unlocked"
            );

            // An unlock takes out only what it names: half of a held range
            // stays held.
            let whole = a_lock(F_WRLCK, SEEK_SET, 0, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &whole as *const Flock as usize)
                    .await,
                Ok(0)
            );
            let half = a_lock(F_UNLCK, SEEK_SET, 0, 5);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &half as *const Flock as usize)
                    .await,
                Ok(0)
            );
            assert!(
                somebody_else_could_take(key, who, (0, 5)),
                "the half that was unlocked is still held"
            );
            assert!(
                !somebody_else_could_take(key, who, (5, 10)),
                "the half that was NOT unlocked was released as well"
            );

            record_lock::release_owner(who);
        })
    });
}

#[test]
fn a_non_blocking_set_takes_a_free_range_and_answers_eagain_for_a_held_one() {
    // `F_SETLK` is one attempt and no wait, so its two answers are the range
    // and `EAGAIN`. Which one comes back depends on fcntl(2)'s compatibility
    // rule -- two readers share, a writer excludes everyone -- and the four
    // combinations are asked here in both directions.
    let proc = a_process(48_216, "setlk");
    let holder = a_process(48_217, "setlk-holder");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, key) = Keyed::open(&lp, 48_216, THE_SIZE);
            let fd = FileDesc::from(fd as i32);
            let who = holder.id();
            somebody_else_holds(key, who, true, (0, 10));
            somebody_else_holds(key, who, false, (20, 30));

            // Somebody else's exclusive lock refuses both kinds of request.
            for kind in [F_RDLCK, F_WRLCK] {
                let fl = a_lock(kind, SEEK_SET, 0, 10);
                assert_eq!(
                    sys.sys_fcntl(fd, F_SETLK, &fl as *const Flock as usize)
                        .await,
                    Err(LxError::EAGAIN),
                    "an exclusive lock let a {} through",
                    if kind == F_RDLCK { "reader" } else { "writer" }
                );
            }

            // Somebody else's shared lock admits a reader and refuses a
            // writer, which is the one pair that says the exclusivity of the
            // REQUEST is read and not just the held side's.
            let reader = a_lock(F_RDLCK, SEEK_SET, 20, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &reader as *const Flock as usize)
                    .await,
                Ok(0),
                "a shared lock refused a second reader"
            );
            let writer = a_lock(F_WRLCK, SEEK_SET, 20, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &writer as *const Flock as usize)
                    .await,
                Err(LxError::EAGAIN),
                "a shared lock let a writer in"
            );

            // And a range nobody is near is simply taken.
            let free = a_lock(F_WRLCK, SEEK_SET, 50, 10);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &free as *const Flock as usize)
                    .await,
                Ok(0),
                "a free range was refused"
            );
            assert!(
                !somebody_else_could_take(key, who, (50, 60)),
                "the range came back Ok and was not taken"
            );

            record_lock::release_owner(who);
        })
    });
}

#[test]
fn the_type_the_range_the_descriptor_and_the_pointer_each_refuse_in_their_own_way() {
    // Four refusals of one call, each from a different place, so that no two
    // of them can be the same check: an `l_type` the match has no arm for,
    // a range that resolves below zero or past the end of the number, a
    // descriptor that is not a file at all, and no struct to read.
    let proc = a_process(48_218, "refusals");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, _key) = Keyed::open(&lp, 48_218, THE_SIZE);
            let fd = FileDesc::from(fd as i32);

            let odd = a_lock(A_LOCK_TYPE_OF_NOBODY, SEEK_SET, 0, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &odd as *const Flock as usize)
                    .await,
                Err(LxError::EINVAL),
                "a lock type with no arm was accepted"
            );

            // A negative length counts the bytes BEFORE l_start, so from
            // zero it reaches below the start of the file.
            let behind = a_lock(F_WRLCK, SEEK_SET, 0, -1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &behind as *const Flock as usize)
                    .await,
                Err(LxError::EINVAL),
                "a range that starts before the file was accepted"
            );

            let too_far = a_lock(F_WRLCK, SEEK_SET, i64::MAX, 1);
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, &too_far as *const Flock as usize)
                    .await,
                Err(LxError::EOVERFLOW),
                "a range that runs off the end of the number was accepted"
            );

            // Record locks are for files: an eventfd is a descriptor and is
            // not a file, and Linux answers EBADF rather than pretending.
            let evt = FileDesc::from(sys.sys_eventfd2(0, 0).unwrap() as i32);
            let fl = a_lock(F_WRLCK, SEEK_SET, 0, 1);
            assert_eq!(
                sys.sys_fcntl(evt, F_SETLK, &fl as *const Flock as usize)
                    .await,
                Err(LxError::EBADF),
                "an eventfd was given a record lock"
            );

            // The struct is read before any of that, so with nowhere to read
            // it from there is nothing to refuse about its contents.
            assert_eq!(
                sys.sys_fcntl(fd, F_SETLK, 0).await,
                Err(LxError::EFAULT),
                "the lock struct was never read"
            );
        })
    });
}
