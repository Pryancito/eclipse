//! Descriptors the wait cannot park on, cannot find, or has been told to drop
//! -- the last four lines of this file that anything in this bench can reach.
//!
//! After the blocking path and the refusal path were covered, `poll.rs` had 14
//! production lines left at zero, and ten of them are out of reach from here:
//! six are the `write_array` failures, which want memory that is readable and
//! not writable and libos has none; two are dead (`err_fds.set` hangs off
//! `ready.except`, and `select_ready` returns `except: false` literally); two
//! are the input-device branch, which belongs to another thread. The four
//! left are these:
//!
//! - **`None => covered = false`, in both futures.** Before parking, each
//!   wait tries to subscribe a waker to every watched descriptor's event bus.
//!   A descriptor with no bus -- a plain inode-backed `File`, for which
//!   `subscribe_readiness` answers `None` -- cannot take one, and the wait
//!   has to fall back to re-scanning on the backstop tick. Every descriptor
//!   used to park a wait in this bench so far (a pipe, a terminal) HAS a bus,
//!   so the fallback had never been taken. What it decides is only the length
//!   of that tick, so it cannot be asserted on -- see the note below -- but
//!   the test that walks it pins what does matter: a descriptor that nothing
//!   can ever wake must still come back when the timeout runs out, rather
//!   than wait for an event that cannot arrive.
//! - **`None => continue`, in `select`.** `select_core` rejects a closed
//!   descriptor with `EBADF` before the wait begins, so this arm can only be
//!   reached by a descriptor that is closed WHILE the wait is parked, on a
//!   later pass. The fd is then dropped from the scan rather than failing the
//!   call -- which is what a caller wants, because by then it is the caller's
//!   own other thread that closed it.
//! - **The zeroed event of `EPOLL_CTL_DEL`.** A delete has no event to copy
//!   in, and every libc passes a null pointer for it, so the copy has to be
//!   skipped on `op`. Three tests in this crate call `epoll_ctl` and all
//!   three call it with `EPOLL_CTL_ADD`, so a delete had never been made.
//!   The line above it records what went wrong when the lookup order was
//!   different: a delete of a closed descriptor answered `ENOENT` instead of
//!   `EBADF`, because `DEL` never looked the descriptor up at all.
//!
//! What cannot be asserted, and is declared rather than discovered: which of
//! the two backstop ticks a wait chose (4 ms or 100 ms) does not change any
//! answer, because the timer is armed at the minimum of the tick and what is
//! left of the timeout. The mutation that makes a bus-less descriptor count
//! as covered therefore SURVIVES on purpose, as it did in the batch that
//! found it. The lines run; the choice is not observable from a return value.
//!
//! Fixed pids live in 49_33x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::{LinuxProcess, ProcessExt};
use rcore_fs::vfs::{FileSystem, FileType, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `POLLIN`, what a reader asks for.
const POLLIN: u16 = 0x0001;

/// Long enough that a wait parks and goes round the loop several times, short
/// enough not to slow the suite down.
const A_SHORT_WAIT: isize = 200;

/// Long enough that the helper thread's close lands well inside the wait and
/// the scan goes round again afterwards, and still a fraction of a second.
/// That the arm it is aiming for really did run is proved by the mutation
/// lot, not by this number: a timer cannot witness its own ordering.
const A_LONGER_WAIT: isize = 500;

/// How long the helper thread leaves the call to park before acting on it.
const A_MOMENT: core::time::Duration = core::time::Duration::from_millis(10);

/// How many moments the helper waits for the call to reach its park before
/// giving up. It does NOTHING when it runs out: a helper that acts anyway
/// would be closing a descriptor out from under whatever parks next.
const A_WAIT_BUDGET: usize = 2_000;

/// `EPOLL_CTL_ADD` and `EPOLL_CTL_DEL` (`/usr/include/linux/eventpoll.h`).
const CTL_ADD: i32 = 1;
const CTL_DEL: i32 = 2;

/// A descriptor no process here holds, so it is closed by construction.
const A_CLOSED_FD: usize = 100;

/// The caller's cookie, which `epoll_wait` hands back untouched.
const A_COOKIE: u64 = 0x5ea1_ed;

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

/// A character device that is never ready either way, and that `File` has no
/// event bus for: the one shape that makes a wait park AND makes the
/// readiness subscription answer `None`. A pipe would do the first and not
/// the second.
struct Quiet;

impl rcore_fs::vfs::INode for Quiet {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: false,
            write: false,
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

/// A character device that is always ready to read, for the epoll that has to
/// report something before the delete and nothing after it.
struct Ready;

impl rcore_fs::vfs::INode for Ready {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
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
/// descriptor each one was handed.
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

/// `poll(ufds, nfds, timeout_msecs)` by the syscall. `sys_poll` wants
/// `&mut self` and `on_a_thread` lends a shared one, so this builds its own
/// out of the same three parts, as `dispatch_tests::by_number` does.
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

/// `select(nfds, &read, &write, &except, &timeout)` by the syscall, with the
/// timeout in milliseconds.
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

/// `epoll_wait(epfd, &out, out.len(), 0)`: no waiting, just what is ready now.
async fn epoll_now(sys: &Syscall<'_>, epfd: usize, out: &mut [EpollEvent]) -> SysResult {
    let n = out.len();
    sys.sys_epoll_wait(
        FileDesc::from(epfd),
        UserOutPtr::from(out.as_mut_ptr() as usize),
        n,
        0,
    )
    .await
}

/// A host thread that waits for the call to park and then closes `fd` in the
/// process's own table, as a sibling thread of the caller's would.
fn a_closer(proc: Arc<Process>, fd: usize, parked: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..A_WAIT_BUDGET {
            if parked.load(Ordering::SeqCst) {
                std::thread::sleep(A_MOMENT);
                proc.linux().close_file(FileDesc::from(fd)).unwrap();
                return;
            }
            std::thread::sleep(A_MOMENT);
        }
    })
}

#[test]
fn a_poll_on_a_descriptor_with_no_event_bus_still_comes_back_when_its_timeout_runs_out() {
    // A descriptor that is never ready and has no event bus is the only shape
    // that reaches the subscription fallback: the wait parks (nothing is
    // ready) and cannot park a waker (nothing to park it on), so the only
    // thing that can ever end the wait is its own timeout. What is asserted
    // is that it does end -- a wait with no event source that waited for an
    // event would hang the caller for ever.
    let quiet: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Quiet);
    let (proc, fds) = a_process_holding(49_330, "poll-no-bus", &[quiet]);
    let fd = fds[0] as i32;
    let (got, revents, took) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(fd, POLLIN)];
            let begin = mono_now();
            let got = poll_them(sys, &mut set, A_SHORT_WAIT).await;
            (got, set[0].revents, mono_now().saturating_sub(begin))
        })
    });
    assert_eq!(got, Ok(0), "a wait with no event source did not time out");
    assert!(
        revents.is_empty(),
        "nothing was ready, so nothing may be reported: {:?}",
        revents
    );
    assert!(
        took >= core::time::Duration::from_millis(A_SHORT_WAIT as u64),
        "it answered before its deadline, so it never parked: {:?}",
        took
    );
}

#[test]
fn a_select_on_a_descriptor_with_no_event_bus_still_comes_back_when_its_timeout_runs_out() {
    // The same fallback in `select_core`, which keeps its own copy of it.
    let quiet: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Quiet);
    let (proc, fds) = a_process_holding(49_331, "select-no-bus", &[quiet]);
    let fd = fds[0];
    let nfds = fd + 1;
    let (got, left, took) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(nfds, &[fd]);
            let begin = mono_now();
            let got = select_them(sys, nfds, &mut read, A_SHORT_WAIT).await;
            (got, read, mono_now().saturating_sub(begin))
        })
    });
    assert_eq!(got, Ok(0), "a wait with no event source did not time out");
    assert_eq!(
        left,
        alloc::vec![0u32; left.len()],
        "a select that timed out must hand back cleared sets"
    );
    assert!(
        took >= core::time::Duration::from_millis(A_SHORT_WAIT as u64),
        "it answered before its deadline, so it never parked: {:?}",
        took
    );
}

#[test]
fn a_descriptor_closed_while_a_select_waits_is_dropped_from_the_set_instead_of_failing_it() {
    // `select_core` answers EBADF for a closed descriptor BEFORE the wait
    // begins, so the only way to reach the arm that skips one is to close it
    // while the wait is parked -- which is the realistic case, because by
    // then it is the caller's own sibling thread doing the closing. The call
    // must come back on its timeout with nothing to report, not with EBADF
    // for a descriptor that was perfectly good when it was asked for.
    let quiet: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Quiet);
    let (proc, fds) = a_process_holding(49_332, "select-closed", &[quiet]);
    let fd = fds[0];
    let nfds = fd + 1;
    let parked = Arc::new(AtomicBool::new(false));
    let closer = a_closer(proc.clone(), fd, parked.clone());
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(nfds, &[fd]);
            parked.store(true, Ordering::SeqCst);
            select_them(sys, nfds, &mut read, A_LONGER_WAIT).await
        })
    });
    closer.join().unwrap();
    assert_eq!(
        got,
        Ok(0),
        "a descriptor closed mid-wait must be dropped from the scan, not fail the call"
    );
    assert!(
        proc.linux()
            .get_files()
            .unwrap()
            .get(&FileDesc::from(fd))
            .is_none(),
        "the helper never managed to close the descriptor, so nothing was tested"
    );
}

#[test]
fn taking_a_descriptor_out_of_an_epoll_needs_no_event_to_copy_in() {
    // Every libc passes a null pointer as the event of an `EPOLL_CTL_DEL`,
    // because a delete has nothing to say about what to watch for. The copy
    // is therefore skipped on the operation, and a delete that tried to read
    // it would answer EFAULT to a perfectly ordinary call.
    let ready: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Ready);
    let (proc, fds) = a_process_holding(49_333, "epoll-del", &[ready]);
    let fd = fds[0];
    let (added, before, deleted, after, closed) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let epfd = sys.sys_epoll_create1(0).unwrap();
            let watch = EpollEvent {
                events: u32::from(POLLIN),
                data: A_COOKIE,
            };
            let added = sys.sys_epoll_ctl(
                FileDesc::from(epfd),
                CTL_ADD,
                FileDesc::from(fd),
                UserInPtr::from(&watch as *const EpollEvent as usize),
            );
            let mut out = alloc::vec![EpollEvent { events: 0, data: 0 }; 4];
            let before = epoll_now(sys, epfd, &mut out).await;
            // The null event pointer: what a delete is always given.
            let deleted = sys.sys_epoll_ctl(
                FileDesc::from(epfd),
                CTL_DEL,
                FileDesc::from(fd),
                UserInPtr::from(0usize),
            );
            let after = epoll_now(sys, epfd, &mut out).await;
            // A delete of a closed descriptor: EBADF, which is the answer for
            // a closed fd, and not the ENOENT it gave when a delete did not
            // look the descriptor up at all.
            let closed = sys.sys_epoll_ctl(
                FileDesc::from(epfd),
                CTL_DEL,
                FileDesc::from(A_CLOSED_FD),
                UserInPtr::from(0usize),
            );
            (added, before, deleted, after, closed)
        })
    });
    assert_eq!(added, Ok(0), "the watch was not accepted");
    assert_eq!(before, Ok(1), "a ready watched descriptor was not reported");
    assert_eq!(
        deleted,
        Ok(0),
        "a delete with a null event pointer was refused"
    );
    assert_eq!(
        after,
        Ok(0),
        "the descriptor was still reported after being deleted"
    );
    assert_eq!(
        closed,
        Err(LxError::EBADF),
        "a delete of a closed descriptor must be EBADF, not ENOENT"
    );
}
