//! The calls of a CUDA program that fail, in dmesg.
//!
//! libcuda answers CUDA_ERROR_OPERATING_SYSTEM (304) when one of its calls
//! to the kernel fails, and names none. `BOOTTRACE=<comm>` names it in
//! `/proc/bootprofile`, but only on a kernel booted with that. So from the
//! first UVM call a process makes, which libcuda makes in `cuInit`, every
//! call of that process that fails is logged the first time it fails with
//! that errno:
//!
//! ```text
//! [nvidia-uvm] pid 1086: mbind failed: ENOSYS (a0=0x7f0000000000 ...)
//! ```
//!
//! That is a line `dmesg | grep nvidia-uvm` shows beside UVM's own, and the
//! last of them before a 304 is the call libcuda gave up on. A process that
//! never calls UVM costs one relaxed load per failed call until one does.

use alloc::collections::BTreeSet;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;
use zircon_object::object::KoID;

use linux_object::error::LxError;

/// Whether any process has made a UVM call yet.
static ANY: AtomicBool = AtomicBool::new(false);

/// The processes that made a UVM call. A KoID is never handed out twice, so
/// one that exited stays here naming nothing.
static CALLERS: Mutex<BTreeSet<KoID>> = Mutex::new(BTreeSet::new());

/// The failures logged so far: process, call, errno.
static LOGGED: Mutex<BTreeSet<(KoID, u32, i32)>> = Mutex::new(BTreeSet::new());

/// At most this many processes are followed, and this many lines logged, in
/// one boot: a CUDA program that keeps failing, or a host that keeps
/// starting them, cannot grow either set without end.
const MAX_CALLERS: usize = 64;
const MAX_LINES: usize = 256;

/// `pid` made a UVM call: its failures are logged from now on.
pub(crate) fn note_uvm_caller(pid: KoID) {
    note(&mut CALLERS.lock(), pid);
    ANY.store(true, Ordering::Relaxed);
}

fn note(callers: &mut BTreeSet<KoID>, pid: KoID) {
    if callers.len() < MAX_CALLERS {
        callers.insert(pid);
    }
}

/// Call `num` of `pid` failed with `err`. Logged, with what it was asked
/// (`detail`), when `pid` made a UVM call and the call has not failed with
/// that errno in it before.
pub(crate) fn record(pid: KoID, num: u32, err: LxError, detail: impl FnOnce() -> String) {
    if !ANY.load(Ordering::Relaxed) {
        return;
    }
    if !first_failure(&CALLERS.lock(), &mut LOGGED.lock(), pid, num, err) {
        return;
    }
    kernel_hal::klog_warn!(
        "[nvidia-uvm] pid {}: {} failed: {:?} ({})",
        pid,
        crate::perf_accounting::resolve(num).unwrap_or_default(),
        err,
        detail()
    );
}

/// Whether this failure is one to log, noting it in `logged` if it is.
fn first_failure(
    callers: &BTreeSet<KoID>,
    logged: &mut BTreeSet<(KoID, u32, i32)>,
    pid: KoID,
    num: u32,
    err: LxError,
) -> bool {
    callers.contains(&pid) && logged.len() < MAX_LINES && logged.insert((pid, num, err as i32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::SyscallType as Sys;

    const PID: KoID = 1086;

    fn callers(pids: &[KoID]) -> BTreeSet<KoID> {
        pids.iter().copied().collect()
    }

    #[test]
    fn only_a_uvm_caller_is_logged() {
        let mut logged = BTreeSet::new();
        let mbind = Sys::MBIND as u32;
        assert!(!first_failure(
            &callers(&[]),
            &mut logged,
            PID,
            mbind,
            LxError::ENOSYS
        ));
        assert!(!first_failure(
            &callers(&[PID + 1]),
            &mut logged,
            PID,
            mbind,
            LxError::ENOSYS
        ));
        assert!(logged.is_empty());
        assert!(first_failure(
            &callers(&[PID]),
            &mut logged,
            PID,
            mbind,
            LxError::ENOSYS
        ));
    }

    /// Once per process, call and errno: a call that keeps failing the same
    /// way is one line, another errno or another process is another.
    #[test]
    fn each_failure_is_logged_once() {
        let callers = callers(&[PID, PID + 1]);
        let mut logged = BTreeSet::new();
        let mbind = Sys::MBIND as u32;
        let mmap = Sys::MMAP as u32;
        let mut first = |pid, num, err| first_failure(&callers, &mut logged, pid, num, err);
        assert!(first(PID, mbind, LxError::ENOSYS));
        assert!(!first(PID, mbind, LxError::ENOSYS));
        assert!(first(PID, mbind, LxError::EINVAL));
        assert!(first(PID, mmap, LxError::ENOSYS));
        assert!(first(PID + 1, mbind, LxError::ENOSYS));
    }

    #[test]
    fn the_lines_and_the_callers_are_bounded() {
        let callers = callers(&[PID]);
        let mut logged = BTreeSet::new();
        for num in 0..MAX_LINES as u32 {
            assert!(first_failure(
                &callers,
                &mut logged,
                PID,
                num,
                LxError::EINVAL
            ));
        }
        assert!(!first_failure(
            &callers,
            &mut logged,
            PID,
            9999,
            LxError::EINVAL
        ));

        let mut set = BTreeSet::new();
        for pid in 0..MAX_CALLERS as KoID + 8 {
            note(&mut set, pid);
        }
        assert_eq!(set.len(), MAX_CALLERS);
        assert!(!set.contains(&(MAX_CALLERS as KoID)));
    }

    /// Through the dispatcher: a call that fails in a process that made a
    /// UVM call is handed over with its errno.
    #[test]
    fn the_dispatcher_hands_a_failure_over() {
        use crate::on_a_thread::on_a_thread;
        use crate::Syscall;
        use alloc::boxed::Box;
        use linux_object::process::LinuxProcess;
        use rcore_fs_ramfs::RamFS;
        use zircon_object::task::{Process, ROOT_JOB};

        const CUDA: KoID = 45_403;
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            CUDA,
            "cuda",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        note_uvm_caller(CUDA);
        let mbind = Sys::MBIND as u32;
        let set_mempolicy = Sys::SET_MEMPOLICY as u32;
        let ret = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut full = Syscall {
                    thread: sys.thread,
                    thread_fn: sys.thread_fn,
                    syscall_entry: 0,
                };
                // MPOL_BIND over an unaligned start, then with no nodes.
                (
                    full.syscall(mbind, [0x1001, 0x1000, 2, 0, 0, 0]).await,
                    full.syscall(set_mempolicy, [2, 0, 0, 0, 0, 0]).await,
                )
            })
        });
        let einval = -(LxError::EINVAL as isize);
        assert_eq!(ret, (einval, einval));
        let logged = LOGGED.lock();
        assert!(logged.contains(&(CUDA, mbind, LxError::EINVAL as i32)));
        assert!(logged.contains(&(CUDA, set_mempolicy, LxError::EINVAL as i32)));
    }
}
