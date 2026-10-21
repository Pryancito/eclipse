//! Calls libc makes by number that the table did not have, made the way
//! libc makes them: `epoll_pwait2` on every architecture, and on x86_64
//! `creat` (glibc's `creat`) and `lchown` (both libcs' `lchown`). Each
//! answered `ENOSYS`.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::EpollEvent;
use linux_object::process::LinuxProcess;
use linux_object::time::TimeSpec;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

extern crate std;
use std::time::Instant;

/// Make syscall `num` the way userspace makes it, through the table, as
/// `dispatch_tests` does.
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

/// A process at `pid` whose root holds a `/<top>` of its own: a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "table", lp).unwrap()
}

const EPOLLIN: u32 = 1;
const EPOLL_CTL_ADD: usize = 1;
const SIGUSR1: usize = 10;
const SIG_BLOCK: usize = 0;
const EINTR: isize = -(LxError::EINTR as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);

/// A pipe and an epoll watching its read end for input, with `7` for data.
async fn a_watched_pipe(sys: &Syscall<'_>) -> ([i32; 2], usize) {
    let mut fds = [0i32; 2];
    let ret = by_number(
        sys,
        Sys::PIPE2 as u32,
        [fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0],
    )
    .await;
    assert_eq!(ret, 0);
    let ep = by_number(sys, Sys::EPOLL_CREATE1 as u32, [0; 6]).await;
    assert!(ep >= 0);
    let event = EpollEvent {
        events: EPOLLIN,
        data: 7,
    };
    let args = [
        ep as usize,
        EPOLL_CTL_ADD,
        fds[0] as usize,
        &event as *const _ as usize,
        0,
        0,
    ];
    assert_eq!(by_number(sys, Sys::EPOLL_CTL as u32, args).await, 0);
    (fds, ep as usize)
}

/// `epoll_pwait2(ep, out, 4, timeout, mask, 8)` by number.
async fn pwait2(
    sys: &Syscall<'_>,
    ep: usize,
    out: &mut [EpollEvent; 4],
    timeout: Option<&TimeSpec>,
    mask: Option<&u64>,
) -> isize {
    let timeout = timeout.map_or(0, |t| t as *const _ as usize);
    let mask = mask.map_or(0, |m| m as *const _ as usize);
    let args = [ep, out.as_mut_ptr() as usize, 4, timeout, mask, 8];
    by_number(sys, Sys::EPOLL_PWAIT2 as u32, args).await
}

fn no_events() -> [EpollEvent; 4] {
    [EpollEvent { events: 0, data: 0 }; 4]
}

/// A null timeout waits until there is something to report, and reports it.
#[test]
fn epoll_pwait2_with_no_timeout_waits_for_an_event() {
    let proc = a_process(46_511, "t1");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (fds, ep) = a_watched_pipe(sys).await;
            let mut out = no_events();
            let ret = {
                let wait = pwait2(sys, ep, &mut out, None, None);
                futures::pin_mut!(wait);
                let waited = futures::poll!(wait.as_mut()).is_pending();
                let byte = b"x";
                let args = [fds[1] as usize, byte.as_ptr() as usize, 1, 0, 0, 0];
                assert_eq!(by_number(sys, Sys::WRITE as u32, args).await, 1);
                (waited, wait.await)
            };
            let (events, data) = (out[0].events, out[0].data);
            (ret, events, data)
        })
    });
    assert_eq!(got, ((true, 1), EPOLLIN, 7));
}

/// `{0, 0}` is a look, not a wait; a timeout is waited out, and comes back
/// with nothing.
#[test]
fn epoll_pwait2_looks_with_a_zero_timeout_and_waits_out_another() {
    let proc = a_process(46_512, "t2");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_fds, ep) = a_watched_pipe(sys).await;
            let mut out = no_events();
            let zero = TimeSpec { sec: 0, nsec: 0 };
            let look = pwait2(sys, ep, &mut out, Some(&zero), None);
            futures::pin_mut!(look);
            let at_once = futures::poll!(look.as_mut());
            if at_once.is_pending() {
                // A look that waits fails here, before the wait below.
                return (at_once, 0, 0);
            }
            let thirty_ms = TimeSpec {
                sec: 0,
                nsec: 30_000_000,
            };
            let begin = Instant::now();
            let ret = pwait2(sys, ep, &mut no_events(), Some(&thirty_ms), None).await;
            (at_once, ret, begin.elapsed().as_millis())
        })
    });
    assert_eq!(got.0, core::task::Poll::Ready(0), "{{0, 0}} waited");
    assert_eq!(got.1, 0);
    assert!(got.2 >= 30, "a 30 ms timeout came back after {} ms", got.2);
}

/// A `timespec` out of range is `EINVAL`, as `poll_select_set_timeout`
/// makes it, and is not read as a very long wait.
#[test]
fn epoll_pwait2_refuses_a_timespec_out_of_range() {
    let proc = a_process(46_513, "t3");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_fds, ep) = a_watched_pipe(sys).await;
            let nsec = TimeSpec {
                sec: 0,
                nsec: 1_000_000_000,
            };
            let negative = TimeSpec {
                sec: -1isize as usize,
                nsec: 0,
            };
            // Polled once each: read as a wait, either would never end.
            let mut answers = alloc::vec::Vec::new();
            for timeout in [&nsec, &negative] {
                let mut out = no_events();
                let call = pwait2(sys, ep, &mut out, Some(timeout), None);
                futures::pin_mut!(call);
                answers.push(futures::poll!(call.as_mut()));
            }
            answers
        })
    });
    let refused = core::task::Poll::Ready(EINVAL);
    assert_eq!(got, [refused, refused]);
}

/// The mask the call is given is the one it waits under: a signal blocked
/// before the call and pending, that the call's mask lets through, ends
/// the wait with `EINTR` at once. Without that mask the same call waits.
#[test]
fn epoll_pwait2_waits_under_the_mask_it_is_given() {
    let proc = a_process(46_514, "t4");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (_fds, ep) = a_watched_pipe(sys).await;
            let usr1: u64 = 1 << (SIGUSR1 - 1);
            let args = [SIG_BLOCK, &usr1 as *const _ as usize, 0, 8, 0, 0];
            assert_eq!(by_number(sys, Sys::RT_SIGPROCMASK as u32, args).await, 0);
            let tid = by_number(sys, Sys::GETTID as u32, [0; 6]).await as usize;
            let args = [tid, SIGUSR1, 0, 0, 0, 0];
            assert_eq!(by_number(sys, Sys::TKILL as u32, args).await, 0);
            let mut out = no_events();
            let blocked = {
                let wait = pwait2(sys, ep, &mut out, None, None);
                futures::pin_mut!(wait);
                futures::poll!(wait.as_mut()).is_pending()
            };
            let none: u64 = 0;
            let wait = pwait2(sys, ep, &mut out, None, Some(&none));
            futures::pin_mut!(wait);
            (blocked, futures::poll!(wait.as_mut()))
        })
    });
    assert_eq!(got, (true, core::task::Poll::Ready(EINTR)));
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    use super::*;
    use rcore_fs::vfs::INode;

    const O_ACCMODE: usize = 3;
    const O_WRONLY: usize = 1;
    const F_GETFL: usize = 3;

    fn node(sys: &Syscall<'_>, path: &str) -> Arc<dyn INode> {
        sys.linux_process().lookup_inode(path).unwrap()
    }

    /// `creat` is `open(O_CREAT | O_WRONLY | O_TRUNC)`: a new file with the
    /// mode asked for less the umask, an old one emptied, write-only both.
    #[test]
    fn creat_makes_a_write_only_file_and_empties_an_old_one() {
        let proc = a_process(46_515, "t5");
        let got = on_a_thread(&proc, |sys| {
            Box::pin(async move {
                by_number(sys, Sys::UMASK as u32, [0o022, 0, 0, 0, 0, 0]).await;
                let old = node(sys, "/t5")
                    .create("old", FileType::File, 0o600)
                    .unwrap();
                old.write_at(0, b"hello").unwrap();
                let mut got = alloc::vec::Vec::new();
                for (path, mode) in [("/t5/new", 0o660), ("/t5/old", 0o600)] {
                    let p = c_path(path);
                    let fd = by_number(
                        sys,
                        Sys::CREAT as u32,
                        [p.as_ptr() as usize, mode, 0, 0, 0, 0],
                    )
                    .await;
                    assert!(fd >= 0, "creat {} answered {}", path, fd);
                    let fl =
                        by_number(sys, Sys::FCNTL as u32, [fd as usize, F_GETFL, 0, 0, 0, 0]).await;
                    let meta = node(sys, path).metadata().unwrap();
                    got.push((fl as usize & O_ACCMODE, meta.mode & 0o777, meta.size));
                }
                got
            })
        });
        assert_eq!(got, [(O_WRONLY, 0o640, 0), (O_WRONLY, 0o600, 0)]);
    }

    /// `lchown` changes the owner of the link, and not of what it names; `-1`
    /// leaves that id as it is.
    #[test]
    fn lchown_changes_the_link_and_not_its_target() {
        let proc = a_process(46_516, "t6");
        let got = on_a_thread(&proc, |sys| {
            Box::pin(async move {
                let top = node(sys, "/t6");
                top.create("f", FileType::File, 0o644).unwrap();
                let link = top.create("l", FileType::SymLink, 0o777).unwrap();
                link.write_at(0, b"f").unwrap();
                let l = c_path("/t6/l");
                let args = [l.as_ptr() as usize, 1234, 5678, 0, 0, 0];
                assert_eq!(by_number(sys, Sys::LCHOWN as u32, args).await, 0);
                let args = [l.as_ptr() as usize, -1isize as usize, 99, 0, 0, 0];
                assert_eq!(by_number(sys, Sys::LCHOWN as u32, args).await, 0);
                let link = link.metadata().unwrap();
                let file = node(sys, "/t6/f").metadata().unwrap();
                ((link.uid, link.gid), (file.uid, file.gid))
            })
        });
        assert_eq!(got, ((1234, 99), (0, 0)));
    }
}
