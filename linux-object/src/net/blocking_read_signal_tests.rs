//! A blocking read of a netlink or uevent socket with nothing queued polls
//! every 5 ms, and used to poll for good: no signal ended it. `alarm(4)`
//! could not cut a `read(nl, ..)` short, and neither could ^C (found by
//! tools/sysfuzz, seeds 0xacaf40674e4392c4 and 0xb4b374f04e51df2c). Linux
//! sleeps there interruptibly and answers EINTR, as `packet.rs` already does.
extern crate std;

use super::*;
use crate::process::{send_signal_to_process, LinuxProcess, ProcessExt};
use crate::signal::{Signal as LinuxSignal, SignalAction, SignalActionFlags, Sigset};
use crate::thread::ThreadExt;
use core::time::Duration;
use rcore_fs_ramfs::RamFS;
use std::sync::mpsc;
use zircon_object::object::KernelObject;
use zircon_object::object::KoID;
use zircon_object::task::{Process, Thread, ROOT_JOB};

fn a_reader(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "reader",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    proc.linux().set_signal_action(
        LinuxSignal::SIGUSR1,
        SignalAction {
            handler: 0x1000,
            flags: SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// `read` on `socket` as `thread`, on a host thread of its own, so a read
/// that never ends fails the test instead of hanging the suite.
fn read_as(thread: &Arc<Thread>, socket: Arc<dyn Socket>) -> mpsc::Receiver<SysResult> {
    let thread = thread.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = async_std::task::block_on(async move {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            let mut buf = [0u8; 64];
            let r = socket.read(&mut buf).await.0;
            kernel_hal::thread::set_current_thread(None);
            r
        });
        let _ = tx.send(r);
    });
    rx
}

fn a_signal_ends_the_read(pid: KoID, socket: Arc<dyn Socket>) {
    let (proc, thread) = a_reader(pid);
    let read = read_as(&thread, socket);
    assert!(
        read.recv_timeout(Duration::from_millis(100)).is_err(),
        "nothing is queued: the read must wait"
    );
    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGUSR1).unwrap();
    assert_eq!(
        read.recv_timeout(Duration::from_secs(3)),
        Ok(Err(LxError::EINTR)),
        "a caught signal must end the read with EINTR"
    );
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "and stay pending, for delivery on the way out"
    );
    thread.terminate_abandoned();
}

#[test]
fn a_signal_ends_a_blocking_netlink_read() {
    a_signal_ends_the_read(
        49_701,
        Arc::new(NetlinkSocketState::new(SocketType::SOCK_RAW)),
    );
}

#[test]
fn a_signal_ends_a_blocking_uevent_read() {
    a_signal_ends_the_read(
        49_702,
        Arc::new(uevent::UeventSocket::new(SocketType::SOCK_RAW)),
    );
}
