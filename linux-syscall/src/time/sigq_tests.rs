//! A POSIX timer's signal entry, as Linux 6.18 accounts for it (`tmrq`
//! probe): counted against the creator's user from `timer_create` to
//! `timer_delete`, refused past `RLIMIT_SIGPENDING` (`EAGAIN`), and its
//! expiries delivered even at the limit, since they add nothing to it.
//!
//! Fixed pids live in 51_2xx; each test's process has the same number as
//! its uid, because the count is one table for the machine.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::sync::Arc;
use linux_object::process::{send_signal_to_process_with_info, LinuxProcess, RLIMIT_SIGPENDING};
use linux_object::signal::queue::queued_for_user;
use linux_object::signal::Sigset;
use rcore_fs_ramfs::RamFS;

const CLOCK_MONOTONIC: usize = 1;

/// A process of real user `pid`, allowed `limit` queued signals.
fn a_process(pid: KoID, limit: u64) -> Arc<Process> {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let uid = pid as u32;
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    let limit = RLimit {
        cur: limit,
        max: limit,
    };
    proc.linux()
        .rlimit(RLIMIT_SIGPENDING, Some(limit), false)
        .unwrap();
    proc
}

/// `timer_create(CLOCK_MONOTONIC, NULL, &id)`: a `SIGALRM` to the process.
fn create(sys: &Syscall<'_>) -> Result<usize, LxError> {
    let mut id = [0i32];
    sys.sys_timer_create(CLOCK_MONOTONIC, 0, id.as_mut_ptr() as usize)?;
    Ok(id[0] as usize)
}

#[test]
fn a_timer_is_counted_while_it_lives_and_refused_past_the_limit() {
    let proc = a_process(51_201, 1);
    let (first, second, during, after, again) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let first = create(sys);
            let second = create(sys);
            let during = queued_for_user(51_201);
            sys.sys_timer_delete(first.unwrap()).unwrap();
            let after = queued_for_user(51_201);
            let again = create(sys);
            sys.sys_timer_delete(again.unwrap()).unwrap();
            (first, second, during, after, again)
        })
    });
    assert!(first.is_ok());
    assert_eq!(
        second,
        Err(LxError::EAGAIN),
        "a second timer past the limit"
    );
    assert_eq!(during, 1, "SigQ counts the timer");
    assert_eq!(after, 0, "timer_delete gives it back");
    assert!(again.is_ok(), "room again");
}

#[test]
fn a_timer_s_expiry_is_delivered_at_the_limit_and_a_sigqueue_is_not() {
    let proc = a_process(51_202, 1);
    let (pending, count, sigqueue) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = create(sys).unwrap();
            let mut blocked = Sigset::empty();
            blocked.insert(Signal::SIGALRM);
            blocked.insert(Signal::SIGRT34);
            sys.thread.lock_linux().block_signals(&blocked);
            let deliver = posix_timer_expiry_signal(id, TimerNotify::SIGALRM_TO_PROCESS, 0);
            let (signal, info) = deliver.unwrap();
            deliver_timer_signal(sys.zircon_process().id(), None, signal, info);
            let pending = sys
                .linux_process()
                .shared_pending()
                .contains(Signal::SIGALRM);
            let count = queued_for_user(51_202);
            let pid = sys.zircon_process().id() as usize;
            let rt = Signal::SIGRT34;
            let info = SigInfo::queued(rt, pid as i32, 51_202, 1);
            let sigqueue = send_signal_to_process_with_info(pid, rt, Some(info));
            sys.sys_timer_delete(id).unwrap();
            (pending, count, sigqueue)
        })
    });
    assert!(pending, "the expiry was refused at the limit");
    assert_eq!(count, 1, "the expiry was counted again");
    assert_eq!(sigqueue, Err(LxError::EAGAIN));
}
