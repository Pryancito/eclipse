//! A descriptor whose file will not say whether it is ready, and what
//! `poll(2)` and `select(2)` answer then.
//!
//! Both waits ask every watched file for its readiness once per pass, and
//! both keep one slot for the first file that answers with an error instead
//! of a readiness word:
//!
//! ```text
//!     let status = match file_like.poll(..) {
//!         Ok(ret) => ret,
//!         Err(err) => { early_err = Some(err); break; }
//!     };
//!     ...
//!     if let Some(err) = early_err { return Poll::Ready(Err(err)); }
//! ```
//!
//! That slot, the `break` that fills it and the check that reads it were 19
//! of the lines this file still had at zero -- the largest single piece of
//! `poll.rs` left after its blocking path was covered -- and the reason is
//! that nothing a test could put in a descriptor table refuses to answer:
//! ramfs is ready both ways, a pipe answers for its buffer, a terminal for
//! its queue. Real descriptors do refuse: a character device whose hardware
//! answered nothing (`EIO`), or one whose driver was torn down from under it
//! (`ENODEV`).
//!
//! Three things are decided in those lines and none of them had ever been
//! asked for:
//!
//! - **Whose error the caller gets.** It is the file's own, passed through,
//!   and not an errno of the wait's choosing. A caller handed `EIO` knows to
//!   drop that descriptor; one handed `EINVAL` or `ENOSYS` learns nothing.
//! - **That the scan stops there.** The `break` means the files after the
//!   refusing one are not asked at all, so the answer is the FIRST refusal's
//!   and a later, different one is never seen.
//! - **That the error beats a result.** `early_err` is checked BEFORE
//!   `events > 0`, so a set holding both a ready descriptor and a refusing
//!   one answers with the error and not with the count. Swap those two
//!   checks and the error is swallowed for as long as anything else in the
//!   set is ready -- which, for a set holding a terminal, is for ever.
//!
//! `select` has a fourth, and it is the one with a bug on record: its error
//! path does not commit the ready bitmaps, so the caller's `fd_set`s come
//! back as they were handed in. `FdSet::new` carries the note of what went
//! wrong when a failing select did clear them -- the usual `while
//! (select(...) < 0 && errno == EINTR) continue;` loop re-entered on an
//! emptied set and waited for an event it could no longer ask about. The
//! signal half of that bug has a test; this half had none, and it is the
//! half where the caller cannot even retry, because the error is the
//! descriptor's and not the wait's.
//!
//! None of these calls reaches a park: the refusal is found on the future's
//! first pass. So the only claim made here about time is a lower bound -- a
//! call that came back well inside its deadline did not wait for it.
//!
//! Fixed pids live in 49_32x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `POLLIN`, what a reader asks for.
const POLLIN: u16 = 0x0001;

/// A timeout no call here waits out: each one must come back on the future's
/// first pass, so a refusal that was swallowed would show up as a run that
/// takes this long rather than as a passing test.
const A_CEILING: isize = 5_000;

/// Half of `A_CEILING`. A call that came back inside this did not wait for
/// its deadline, which is the one thing this module claims about time.
const WELL_INSIDE_IT: Duration = Duration::from_millis(2_500);

/// What a device says when it is asked whether it is ready and cannot
/// answer. Named rather than held as an `FsError`, which is neither `Clone`
/// nor `Copy`; the errno each one has to reach the caller as is written out
/// at the assertion instead of taken from here, so the expectation does not
/// come from the same table as the input.
#[derive(Clone, Copy)]
enum Fault {
    /// The hardware answered nothing: `EIO`.
    Hardware,
    /// The driver is gone from under the descriptor: `ENODEV`.
    Gone,
}

impl Fault {
    fn as_fs_error(self) -> FsError {
        match self {
            Fault::Hardware => FsError::DeviceError,
            Fault::Gone => FsError::NoDevice,
        }
    }
}

fn metadata_of(kind: FileType) -> Metadata {
    Metadata {
        dev: 0,
        inode: 1,
        size: 0,
        blk_size: 4096,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: kind,
        mode: 0o644,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

/// A character device that answers every readiness query with an error, and
/// counts how many times it was asked -- which is how "the scan stopped
/// before reaching this one" is checked without reading the kernel's own
/// state.
struct Refusing {
    says: Fault,
    asked: AtomicUsize,
}

impl Refusing {
    fn new(says: Fault) -> Arc<Self> {
        Arc::new(Refusing {
            says,
            asked: AtomicUsize::new(0),
        })
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl rcore_fs::vfs::INode for Refusing {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Err(self.says.as_fs_error())
    }

    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(metadata_of(FileType::CharDevice))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A character device that is always ready both ways, so a set can hold one
/// descriptor with an answer next to one that refuses.
struct Willing;

impl rcore_fs::vfs::INode for Willing {
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
        Ok(metadata_of(FileType::CharDevice))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A process whose table holds `inodes` in the order given, and the
/// descriptor each one was handed. The order matters: `add_file` hands out
/// the lowest free descriptor, and both scans go in ascending order, so the
/// first inode here is the first one asked.
fn a_process_holding(
    pid: KoID,
    name: &str,
    inodes: &[Arc<dyn rcore_fs::vfs::INode>],
) -> (Arc<Process>, Vec<usize>) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let mut fds = Vec::new();
    for inode in inodes {
        let fd: i32 = lp
            .add_file(File::new(
                inode.clone(),
                OpenFlags::RDWR,
                String::from("/dev/made-up"),
            ))
            .unwrap()
            .into();
        fds.push(fd as usize);
    }
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap();
    (proc, fds)
}

/// One `pollfd` of a caller's array.
fn an_entry(fd: i32, events: u16) -> PollFd {
    PollFd {
        fd: FileDesc::from(fd),
        events: PollEvents::from_bits_truncate(events),
        revents: PollEvents::empty(),
    }
}

/// `poll(ufds, nfds, timeout_msecs)` by the syscall.
///
/// `sys_poll` wants `&mut self` and `on_a_thread` lends a shared one, so this
/// builds its own out of the same three parts, as `dispatch_tests::by_number`
/// does.
async fn poll_them(sys: &Syscall<'_>, set: &mut [PollFd], timeout_msecs: isize) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let nfds = set.len();
    own.sys_poll(
        UserInOutPtr::from(set.as_mut_ptr() as usize),
        nfds,
        timeout_msecs,
    )
    .await
}

/// An `fd_set` in host memory: one `u32` per 32 descriptors.
fn a_set(nfds: usize, fds: &[usize]) -> Vec<u32> {
    let mut words = alloc::vec![0u32; nfds.div_ceil(FD_PER_ITEM).max(1)];
    for &fd in fds {
        words[fd / FD_PER_ITEM] |= 1 << (fd % FD_PER_ITEM);
    }
    words
}

/// `select(nfds, &read, NULL-ish, NULL-ish, &timeout)` by the syscall, with
/// the timeout in milliseconds for the sake of `A_CEILING`.
async fn select_them(
    sys: &Syscall<'_>,
    nfds: usize,
    read: &mut [u32],
    timeout_msecs: isize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let mut write = a_set(nfds, &[]);
    let mut except = a_set(nfds, &[]);
    let mut timeout = TimeVal {
        sec: (timeout_msecs / 1000) as usize,
        usec: (timeout_msecs % 1000) as usize * 1000,
    };
    own.sys_select(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(write.as_mut_ptr() as usize),
        UserInOutPtr::from(except.as_mut_ptr() as usize),
        UserInOutPtr::from(&mut timeout as *mut TimeVal as usize),
    )
    .await
}

#[test]
fn a_poll_whose_file_will_not_say_if_it_is_ready_hands_the_files_error_to_the_caller() {
    let device = Refusing::new(Fault::Hardware);
    let held: Arc<dyn rcore_fs::vfs::INode> = device.clone();
    let (proc, fds) = a_process_holding(49_320, "poll-refused", &[held]);
    let fd = fds[0] as i32;
    let (got, took) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(fd, POLLIN)];
            let begin = mono_now();
            let got = poll_them(sys, &mut set, A_CEILING).await;
            (got, mono_now().saturating_sub(begin))
        })
    });
    assert_eq!(
        got,
        Err(LxError::EIO),
        "the device's own error reaches the caller"
    );
    assert_eq!(device.asked(), 1, "asked once, on the future's first pass");
    assert!(
        took < WELL_INSIDE_IT,
        "a poll whose file refuses must not wait out its timeout, took {:?}",
        took
    );
}

#[test]
fn a_poll_hands_back_the_error_the_file_gave_and_not_an_errno_of_its_own() {
    let device = Refusing::new(Fault::Gone);
    let held: Arc<dyn rcore_fs::vfs::INode> = device.clone();
    let (proc, fds) = a_process_holding(49_321, "poll-gone", &[held]);
    let fd = fds[0] as i32;
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(fd, POLLIN)];
            poll_them(sys, &mut set, A_CEILING).await
        })
    });
    // The other refusing device in this module answers `EIO`; this one is a
    // driver that went away, and the caller has to be able to tell them
    // apart or it cannot know whether retrying is worth anything.
    assert_eq!(got, Err(LxError::ENODEV), "a device that is gone, verbatim");
    assert_eq!(device.asked(), 1);
}

#[test]
fn a_poll_stops_at_the_first_file_that_refuses_and_never_asks_the_rest() {
    let first = Refusing::new(Fault::Hardware);
    let second = Refusing::new(Fault::Gone);
    let held: [Arc<dyn rcore_fs::vfs::INode>; 2] = [first.clone(), second.clone()];
    let (proc, fds) = a_process_holding(49_322, "poll-break", &held);
    let (a, b) = (fds[0] as i32, fds[1] as i32);
    assert!(a < b, "the first inode must hold the lower descriptor");
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(a, POLLIN), an_entry(b, POLLIN)];
            poll_them(sys, &mut set, A_CEILING).await
        })
    });
    assert_eq!(
        got,
        Err(LxError::EIO),
        "the first refusal's error, not the second's"
    );
    assert_eq!(first.asked(), 1);
    assert_eq!(
        second.asked(),
        0,
        "the scan breaks at the first refusal instead of asking on"
    );
}

#[test]
fn a_file_that_refuses_beats_the_descriptors_that_were_already_ready() {
    let ready: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Willing);
    let device = Refusing::new(Fault::Hardware);
    let refusing: Arc<dyn rcore_fs::vfs::INode> = device.clone();
    let (proc, fds) = a_process_holding(49_323, "poll-beats", &[ready, refusing]);
    let (good, bad) = (fds[0] as i32, fds[1] as i32);
    assert!(good < bad, "the ready descriptor must be scanned first");
    let (alone, together) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // The control: the ready descriptor on its own, so a failing
            // assertion below cannot be a ready device that was never ready.
            let mut one = [an_entry(good, POLLIN)];
            let alone = poll_them(sys, &mut one, A_CEILING).await;
            let mut both = [an_entry(good, POLLIN), an_entry(bad, POLLIN)];
            let together = poll_them(sys, &mut both, A_CEILING).await;
            (alone, together)
        })
    });
    assert_eq!(alone, Ok(1), "the willing device really does report ready");
    assert_eq!(
        together,
        Err(LxError::EIO),
        "the error is answered even though one descriptor had an event"
    );
}

#[test]
fn a_select_whose_file_will_not_say_if_it_is_ready_hands_the_files_error_to_the_caller() {
    let device = Refusing::new(Fault::Hardware);
    let held: Arc<dyn rcore_fs::vfs::INode> = device.clone();
    let (proc, fds) = a_process_holding(49_324, "select-refused", &[held]);
    let fd = fds[0];
    let nfds = fd + 1;
    let (got, took) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(nfds, &[fd]);
            let begin = mono_now();
            let got = select_them(sys, nfds, &mut read, A_CEILING).await;
            (got, mono_now().saturating_sub(begin))
        })
    });
    assert_eq!(
        got,
        Err(LxError::EIO),
        "the device's own error reaches the caller"
    );
    assert_eq!(device.asked(), 1, "asked once, on the future's first pass");
    assert!(
        took < WELL_INSIDE_IT,
        "a select whose file refuses must not wait out its timeout, took {:?}",
        took
    );
}

#[test]
fn a_select_that_fails_leaves_the_callers_set_exactly_as_it_was() {
    let ready: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Willing);
    let device = Refusing::new(Fault::Hardware);
    let refusing: Arc<dyn rcore_fs::vfs::INode> = device.clone();
    let (proc, fds) = a_process_holding(49_325, "select-set", &[ready, refusing]);
    let (good, bad) = (fds[0], fds[1]);
    assert!(good < bad, "the ready descriptor must be scanned first");
    let nfds = bad + 1;
    let (narrowed, narrowed_set, got, asked_for, handed_back) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // The control: a select that succeeds DOES rewrite the set,
            // down to the descriptors that are ready. Without this, the
            // assertion below would also pass if `commit` never wrote
            // anything at all.
            let mut only_good = a_set(nfds, &[good]);
            let before = only_good.clone();
            let narrowed = select_them(sys, nfds, &mut only_good, A_CEILING).await;
            let asked_for = a_set(nfds, &[good, bad]);
            let mut read = asked_for.clone();
            let got = select_them(sys, nfds, &mut read, A_CEILING).await;
            (narrowed, (before, only_good), got, asked_for, read)
        })
    });
    assert_eq!(
        narrowed,
        Ok(1),
        "the willing device really does report ready"
    );
    assert_eq!(
        narrowed_set.1, narrowed_set.0,
        "a select that found that one descriptor hands back the bit it found"
    );
    assert_eq!(
        got,
        Err(LxError::EIO),
        "the error is answered even though one descriptor was ready"
    );
    assert_eq!(
        handed_back, asked_for,
        "a select that failed must hand the set back untouched, or the \
         caller's retry loop re-enters on a set with nothing in it"
    );
}

#[test]
fn a_select_stops_at_the_first_file_that_refuses_and_never_asks_the_rest() {
    let first = Refusing::new(Fault::Hardware);
    let second = Refusing::new(Fault::Gone);
    let held: [Arc<dyn rcore_fs::vfs::INode>; 2] = [first.clone(), second.clone()];
    let (proc, fds) = a_process_holding(49_326, "select-break", &held);
    let (a, b) = (fds[0], fds[1]);
    assert!(a < b, "select scans its set in ascending descriptor order");
    let nfds = b + 1;
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(nfds, &[a, b]);
            select_them(sys, nfds, &mut read, A_CEILING).await
        })
    });
    assert_eq!(
        got,
        Err(LxError::EIO),
        "the first refusal's error, not the second's"
    );
    assert_eq!(first.asked(), 1);
    assert_eq!(
        second.asked(),
        0,
        "the scan breaks at the first refusal instead of asking on"
    );
}
