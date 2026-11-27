//! POSIX message queues through the syscalls: a queue named in one process
//! and opened by name in another, the order the calls check things in, the
//! descriptor `mq_getsetattr` changes, the registration a `close` takes
//! away, and a receiver parked in one process woken by a sender in another.
//!
//! The queue logic has its own tests in `linux_object::ipc::mqueue`; these
//! are about what only the syscall can get wrong: copying in and out, which
//! error comes first, and which descriptor is which.
//!
//! Fixed pids live in 47_49x; queue names start with `w-`.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::time::Duration;
use linux_object::process::LinuxProcess;
use linux_object::time::TimeSpec;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_NONBLOCK: usize = 0o4000;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A name as libc hands it over: no slash, and a NUL at the end.
fn cname(name: &str) -> Vec<u8> {
    let mut v = Vec::from(name.as_bytes());
    v.push(0);
    v
}

fn attr(maxmsg: isize, msgsize: isize) -> MqAttr {
    MqAttr {
        maxmsg,
        msgsize,
        ..MqAttr::default()
    }
}

fn open(sys: &Syscall<'_>, name: &str, oflag: usize, a: Option<&MqAttr>) -> SysResult {
    let n = cname(name);
    let at = a.map_or(0, |a| a as *const MqAttr as usize);
    sys.sys_mq_open(n.as_ptr() as usize, oflag, 0o600, at)
}

fn unlink(sys: &Syscall<'_>, name: &str) -> SysResult {
    let n = cname(name);
    sys.sys_mq_unlink(n.as_ptr() as usize)
}

async fn send_at(sys: &Syscall<'_>, fd: usize, data: &[u8], prio: usize, ts: usize) -> SysResult {
    sys.sys_mq_timedsend(fd, data.as_ptr() as usize, data.len(), prio, ts, false)
        .await
}

async fn send(sys: &Syscall<'_>, fd: usize, data: &[u8], prio: usize) -> SysResult {
    send_at(sys, fd, data, prio, 0).await
}

/// Receive into a 64-byte buffer: the message and its priority.
async fn recv_at(sys: &Syscall<'_>, fd: usize, ts: usize) -> Result<(Vec<u8>, u32), LxError> {
    let mut buf = [0u8; 64];
    let mut prio = u32::MAX;
    let n = sys
        .sys_mq_timedreceive(
            fd,
            buf.as_mut_ptr() as usize,
            buf.len(),
            &mut prio as *mut u32 as usize,
            ts,
            false,
        )
        .await?;
    Ok((buf[..n].to_vec(), prio))
}

async fn recv(sys: &Syscall<'_>, fd: usize) -> Result<(Vec<u8>, u32), LxError> {
    recv_at(sys, fd, 0).await
}

fn getattr(sys: &Syscall<'_>, fd: usize) -> MqAttr {
    let mut old = MqAttr::default();
    sys.sys_mq_getsetattr(fd, 0, &mut old as *mut MqAttr as usize)
        .unwrap();
    old
}

/// The `Syscall` the harness lends, as a `&mut` one.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A queue named in one process is the same queue opened by name in
/// another, and its messages come out highest priority first.
#[test]
fn a_queue_named_in_one_process_is_opened_by_name_in_another() {
    let maker = a_process(47_490, "mq-maker");
    on_a_thread(&maker, |sys| {
        Box::pin(async move {
            assert_eq!(open(sys, "w-across", O_RDWR, None), Err(LxError::ENOENT));
            let a = attr(4, 32);
            let fd = open(sys, "w-across", O_CREAT | O_EXCL | O_WRONLY, Some(&a)).unwrap();
            assert_eq!(
                open(sys, "w-across", O_CREAT | O_EXCL | O_RDWR, Some(&a)),
                Err(LxError::EEXIST)
            );
            assert_eq!(send(sys, fd, b"low", 1).await, Ok(0));
            assert_eq!(send(sys, fd, b"high", 9).await, Ok(0));
            assert_eq!(send(sys, fd, b"low2", 1).await, Ok(0));
            // Opened for writing only.
            assert_eq!(recv(sys, fd).await, Err(LxError::EBADF));
        })
    });
    let taker = a_process(47_491, "mq-taker");
    on_a_thread(&taker, |sys| {
        Box::pin(async move {
            let fd = open(sys, "w-across", O_RDONLY | O_NONBLOCK, None).unwrap();
            let a = getattr(sys, fd);
            assert_eq!(
                (a.flags, a.maxmsg, a.msgsize, a.curmsgs),
                (O_NONBLOCK as isize, 4, 32, 3)
            );
            assert_eq!(recv(sys, fd).await, Ok((b"high".to_vec(), 9)));
            assert_eq!(recv(sys, fd).await, Ok((b"low".to_vec(), 1)));
            assert_eq!(recv(sys, fd).await, Ok((b"low2".to_vec(), 1)));
            assert_eq!(recv(sys, fd).await, Err(LxError::EAGAIN));
            assert_eq!(send(sys, fd, b"x", 0).await, Err(LxError::EBADF));
            assert_eq!(unlink(sys, "w-across"), Ok(0));
            assert_eq!(unlink(sys, "w-across"), Err(LxError::ENOENT));
        })
    });
}

/// Linux's order: the deadline is read and checked first, then the
/// priority, then the descriptor, then the size.
#[test]
fn the_checks_come_in_linux_order() {
    let proc = a_process(47_492, "mq-order");
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let a = attr(1, 8);
            let fd = open(sys, "w-order", O_CREAT | O_RDWR, Some(&a)).unwrap();
            let bad = TimeSpec {
                sec: 0,
                nsec: 1_000_000_000,
            };
            let bad = &bad as *const TimeSpec as usize;
            // A bad deadline before a bad descriptor and a bad priority.
            assert_eq!(
                send_at(sys, 9999, b"x", 1 << 20, bad).await,
                Err(LxError::EINVAL)
            );
            assert_eq!(recv_at(sys, 9999, bad).await, Err(LxError::EINVAL));
            // The priority before the descriptor.
            assert_eq!(send(sys, 9999, b"x", 32768).await, Err(LxError::EINVAL));
            assert_eq!(send(sys, 9999, b"x", 32767).await, Err(LxError::EBADF));
            // A descriptor that is not a queue's.
            assert_eq!(send(sys, 0, b"x", 0).await, Err(LxError::EBADF));
            // The size.
            assert_eq!(send(sys, fd, &[0u8; 9], 0).await, Err(LxError::EMSGSIZE));
            let mut small = [0u8; 7];
            assert_eq!(
                sys.sys_mq_timedreceive(fd, small.as_mut_ptr() as usize, 7, 0, 0, false)
                    .await,
                Err(LxError::EMSGSIZE)
            );
            // `mq_getsetattr` takes nothing but O_NONBLOCK, and says so
            // before it looks at the descriptor.
            let odd = MqAttr {
                flags: 1,
                ..MqAttr::default()
            };
            assert_eq!(
                sys.sys_mq_getsetattr(9999, &odd as *const MqAttr as usize, 0),
                Err(LxError::EINVAL)
            );
            assert_eq!(sys.sys_mq_getsetattr(9999, 0, 0), Err(LxError::EBADF));
            // A deadline already past: ETIMEDOUT on a full queue, nothing on
            // one with room.
            let past = TimeSpec { sec: 1, nsec: 0 };
            let past = &past as *const TimeSpec as usize;
            assert_eq!(send_at(sys, fd, b"1", 0, past).await, Ok(0));
            assert_eq!(
                send_at(sys, fd, b"2", 0, past).await,
                Err(LxError::ETIMEDOUT)
            );
            assert_eq!(recv_at(sys, fd, past).await, Ok((b"1".to_vec(), 0)));
            assert_eq!(recv_at(sys, fd, past).await, Err(LxError::ETIMEDOUT));
            // Names a libc would never pass.
            assert_eq!(open(sys, "a/b", O_RDWR, None), Err(LxError::EACCES));
            assert_eq!(open(sys, "", O_RDWR, None), Err(LxError::ENOENT));
            unlink(sys, "w-order").unwrap();
        })
    });
}

/// `mq_setattr` changes the description, so a second `mq_open` of the same
/// queue is not affected; the descriptor is close-on-exec.
#[test]
fn setattr_changes_one_description_and_the_descriptor_is_cloexec() {
    let proc = a_process(47_493, "mq-setattr");
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let a = attr(4, 32);
            let one = open(sys, "w-setattr", O_CREAT | O_RDWR, Some(&a)).unwrap();
            let two = open(sys, "w-setattr", O_RDWR, None).unwrap();
            let nb = MqAttr {
                flags: O_NONBLOCK as isize,
                ..MqAttr::default()
            };
            let mut old = MqAttr::default();
            sys.sys_mq_getsetattr(
                one,
                &nb as *const MqAttr as usize,
                &mut old as *mut MqAttr as usize,
            )
            .unwrap();
            assert_eq!(old.flags, 0);
            assert_eq!(getattr(sys, one).flags, O_NONBLOCK as isize);
            assert_eq!(getattr(sys, two).flags, 0);
            assert_eq!(recv(sys, one).await, Err(LxError::EAGAIN));
            assert_eq!(sys.sys_fcntl(one.into(), 1 /* F_GETFD */, 0).await, Ok(1));
            unlink(sys, "w-setattr").unwrap();
        })
    });
}

/// Only one process may be registered; its `close` of any descriptor of the
/// queue takes the registration away, and a registration a null `sevp` or
/// a bad `sigevent` cannot make.
///
/// One process only: a process of this harness has exited once its call is
/// over, and the registration of a process that has exited is nobody's.
#[test]
fn a_close_by_the_registered_process_frees_the_registration() {
    // `struct sigevent` with SIGEV_SIGNAL and SIGUSR1.
    #[repr(C)]
    struct SigEvent {
        value: usize,
        signo: i32,
        notify: i32,
        pad: [u8; 48],
    }
    let holder = a_process(47_494, "mq-notify-holder");
    on_a_thread(&holder, move |sys| {
        Box::pin(async move {
            let ev = SigEvent {
                value: 7,
                signo: 10,
                notify: 0,
                pad: [0; 48],
            };
            let ev_at = &ev as *const SigEvent as usize;
            let fd = open(sys, "w-notify", O_CREAT | O_RDWR, None).unwrap();
            let dup = open(sys, "w-notify", O_RDWR, None).unwrap();
            assert_eq!(sys.sys_mq_notify(fd, ev_at), Ok(0));
            assert_eq!(sys.sys_mq_notify(dup, ev_at), Err(LxError::EBUSY));
            // The close of the OTHER descriptor takes it away too.
            assert_eq!(sys.sys_close(dup.into()), Ok(0));
            assert_eq!(sys.sys_mq_notify(fd, ev_at), Ok(0));
            // A null sevp takes it away.
            assert_eq!(sys.sys_mq_notify(fd, 0), Ok(0));
            assert_eq!(sys.sys_mq_notify(fd, ev_at), Ok(0));
            assert_eq!(sys.sys_mq_notify(fd, 0), Ok(0));
            // A bad signal is EINVAL, a bad notify too, and so is SIGEV_THREAD
            // without a netlink socket behind `sigev_signo`.
            let mut bad = SigEvent {
                value: 0,
                signo: 65,
                notify: 0,
                pad: [0; 48],
            };
            assert_eq!(
                sys.sys_mq_notify(fd, &bad as *const SigEvent as usize),
                Err(LxError::EINVAL)
            );
            bad.signo = 10;
            bad.notify = 9;
            assert_eq!(
                sys.sys_mq_notify(fd, &bad as *const SigEvent as usize),
                Err(LxError::EINVAL)
            );
            let cookie = [0u8; 32];
            let thread = SigEvent {
                value: cookie.as_ptr() as usize,
                signo: fd as i32,
                notify: 2,
                pad: [0; 48],
            };
            assert_eq!(
                sys.sys_mq_notify(fd, &thread as *const SigEvent as usize),
                Err(LxError::ENOTSOCK)
            );
            unlink(sys, "w-notify").unwrap();
        })
    });
}

/// A receiver parked on an empty queue in one process is woken by a send in
/// another, and gets that message.
#[test]
fn a_parked_receiver_is_woken_by_a_sender_elsewhere() {
    let maker = a_process(47_495, "mq-park-maker");
    let receiver = a_process(47_496, "mq-parked-receiver");
    let sender = a_process(47_497, "mq-sender");
    on_a_thread(&maker, |sys| {
        Box::pin(async move {
            let a = attr(4, 32);
            open(sys, "w-park", O_CREAT | O_RDWR, Some(&a)).unwrap();
        })
    });
    let r = receiver.clone();
    let parked = std::thread::spawn(move || {
        on_a_thread(&r, |sys| {
            Box::pin(async move {
                let fd = open(sys, "w-park", O_RDONLY, None).unwrap();
                recv(sys, fd).await
            })
        })
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !parked.is_finished(),
        "a receive on an empty queue did not park"
    );
    on_a_thread(&sender, |sys| {
        Box::pin(async move {
            let fd = open(sys, "w-park", O_WRONLY, None).unwrap();
            assert_eq!(send(sys, fd, b"wake", 3).await, Ok(0));
            unlink(sys, "w-park").unwrap();
        })
    });
    assert_eq!(parked.join().unwrap(), Ok((b"wake".to_vec(), 3)));
}

/// The six calls are in the table: reached by number, none of them is
/// `ENOSYS` any more.
#[test]
fn the_calls_are_in_the_syscall_table() {
    let proc = a_process(47_498, "mq-table");
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = as_mut(sys);
            let enosys = -(LxError::ENOSYS as isize);
            let n = cname("w-table");
            let fd = own
                .syscall(
                    crate::Sys::MQ_OPEN as u32,
                    [n.as_ptr() as usize, O_CREAT | O_RDWR, 0o600, 0, 0, 0],
                )
                .await;
            assert!(fd >= 0, "mq_open by number answered {}", fd);
            let fd = fd as usize;
            let msg = *b"hi";
            let mut buf = [0u8; 8192];
            for (sys_no, args) in [
                (
                    crate::Sys::MQ_TIMEDSEND,
                    [fd, msg.as_ptr() as usize, 2, 0, 0, 0],
                ),
                (
                    crate::Sys::MQ_TIMEDRECEIVE,
                    [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
                ),
                (crate::Sys::MQ_NOTIFY, [fd, 0, 0, 0, 0, 0]),
                (crate::Sys::MQ_GETSETATTR, [fd, 0, 0, 0, 0, 0]),
                (crate::Sys::MQ_UNLINK, [n.as_ptr() as usize, 0, 0, 0, 0, 0]),
            ] {
                let got = own.syscall(sys_no as u32, args).await;
                assert_ne!(got, enosys);
                assert!(got >= 0, "answered {}", got);
            }
        })
    });
}
