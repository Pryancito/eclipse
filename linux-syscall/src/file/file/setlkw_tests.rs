//! A process killed while it waits in `F_SETLKW` gets out of the wait.
//!
//! The wait polled the lock table every 10 ms and looked at nothing else, so
//! a process killed in it was reaped and then stayed, with its memory and the
//! robust futexes it held, for as long as somebody else held the lock it was
//! waiting for.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::time::Duration;
use linux_object::fs::record_lock::{self, LockRequest, F_WRLCK};
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;

fn a_process(pid: KoID) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "setlkw", lp).unwrap()
}

#[test]
fn a_kill_ends_an_f_setlkw_wait() {
    let proc = a_process(46_311);
    // Alive, and holding the whole file for as long as the test runs.
    let holder = a_process(46_312);
    let killer = proc.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        killer.exit(137);
    });
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let path = b"/setlkw\0";
            let fd = sys.sys_openat(
                FileDesc::CWD,
                UserInPtr::from(path.as_ptr() as usize),
                O_RDWR | O_CREAT,
                0o600,
            )?;
            let fd = FileDesc::from(fd as i32);
            let meta = sys.linux_process().get_file(fd)?.metadata()?;
            let whole = LockRequest {
                exclusive: true,
                start: 0,
                end: u64::MAX,
                owner: holder.id(),
            };
            assert!(record_lock::setlk((meta.dev, meta.inode), &whole, false));
            let fl = Flock {
                l_type: F_WRLCK,
                ..Flock::default()
            };
            sys.sys_fcntl(fd, F_SETLKW, &fl as *const Flock as usize)
                .await
        })
    });
    assert_eq!(got, Err(LxError::EINTR));
}
