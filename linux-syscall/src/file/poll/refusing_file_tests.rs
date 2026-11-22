//! A descriptor whose file will not say whether it is ready, and what
//! `poll(2)` and `select(2)` answer then.
//!
//! In Linux `->poll` returns a mask and has no error to give, so a device
//! that cannot answer reports `EPOLLERR` and the wait goes on: `poll` puts
//! `POLLERR` in that entry's `revents` (whether or not it was asked for) and
//! counts it, and `select` reports the descriptor ready for reading and
//! writing (`POLLIN_SET` and `POLLOUT_SET` both hold `EPOLLERR`). Here the
//! whole call used to fail with the file's errno, which a caller with a
//! terminal and a broken device in one set could never get past.
//!
//! So what is asked below:
//!
//! - the call succeeds and the refusing descriptor is reported as an error;
//! - the scan does not stop there: every file is asked, and the ready ones
//!   are reported beside it;
//! - none of these calls waits out its timeout, because an error is an
//!   event.
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
/// `POLLERR`, which `revents` carries whether or not it was asked for.
const POLLERR: u16 = 0x0008;

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
fn a_poll_whose_file_will_not_say_if_it_is_ready_reports_pollerr_for_it() {
    for (pid, fault) in [(49_320, Fault::Hardware), (49_321, Fault::Gone)] {
        let device = Refusing::new(fault);
        let held: Arc<dyn rcore_fs::vfs::INode> = device.clone();
        let (proc, fds) = a_process_holding(pid, "poll-refused", &[held]);
        let fd = fds[0] as i32;
        let (got, revents, took) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut set = [an_entry(fd, POLLIN)];
                let begin = mono_now();
                let got = poll_them(sys, &mut set, A_CEILING).await;
                (got, set[0].revents.bits(), mono_now().saturating_sub(begin))
            })
        });
        assert_eq!(
            got,
            Ok(1),
            "the refusing entry is counted, not the call failed"
        );
        assert_eq!(revents, POLLERR);
        assert_eq!(device.asked(), 1, "asked once, on the future's first pass");
        assert!(
            took < WELL_INSIDE_IT,
            "an error is an event, took {:?}",
            took
        );
    }
}

#[test]
fn a_poll_asks_every_file_and_reports_the_ready_ones_beside_the_refusing_one() {
    let ready: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Willing);
    let first = Refusing::new(Fault::Hardware);
    let second = Refusing::new(Fault::Gone);
    let held: [Arc<dyn rcore_fs::vfs::INode>; 3] = [first.clone(), ready, second.clone()];
    let (proc, fds) = a_process_holding(49_322, "poll-all", &held);
    let (a, good, b) = (fds[0] as i32, fds[1] as i32, fds[2] as i32);
    let (got, revents) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [
                an_entry(a, POLLIN),
                an_entry(good, POLLIN),
                an_entry(b, POLLIN),
            ];
            let got = poll_them(sys, &mut set, A_CEILING).await;
            (
                got,
                [
                    set[0].revents.bits(),
                    set[1].revents.bits(),
                    set[2].revents.bits(),
                ],
            )
        })
    });
    assert_eq!(got, Ok(3));
    assert_eq!(revents[0], POLLERR);
    assert_eq!(revents[1] & POLLIN, POLLIN, "the ready one is reported too");
    assert_eq!(revents[2], POLLERR);
    assert_eq!((first.asked(), second.asked()), (1, 1), "the scan goes on");
}

#[test]
fn a_select_reports_a_refusing_file_ready_and_goes_on_with_the_set() {
    let ready: Arc<dyn rcore_fs::vfs::INode> = Arc::new(Willing);
    let first = Refusing::new(Fault::Hardware);
    let second = Refusing::new(Fault::Gone);
    let held: [Arc<dyn rcore_fs::vfs::INode>; 3] = [first.clone(), ready, second.clone()];
    let (proc, fds) = a_process_holding(49_324, "select-refused", &held);
    let (a, good, b) = (fds[0], fds[1], fds[2]);
    let nfds = b + 1;
    let (got, read, took) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(nfds, &[a, good, b]);
            let begin = mono_now();
            let got = select_them(sys, nfds, &mut read, A_CEILING).await;
            (got, read, mono_now().saturating_sub(begin))
        })
    });
    assert_eq!(got, Ok(3), "EPOLLERR is in POLLIN_SET");
    assert_eq!(read, a_set(nfds, &[a, good, b]));
    assert_eq!((first.asked(), second.asked()), (1, 1));
    assert!(took < WELL_INSIDE_IT, "took {:?}", took);
}
