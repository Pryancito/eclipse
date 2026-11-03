//! `tkill`, `tgkill` and `rt_tgsigqueueinfo` queue a real-time signal once
//! per call and are refused past the target user's `RLIMIT_SIGPENDING`
//! (`EAGAIN`, which tgkill(2) documents), all three through
//! [`queue_signal_to_thread`]. Only `kill(2)`'s `SI_USER` is never refused.
//!
//! Fixed pids live in 51_25x, each also the process's uid.

use super::*;
use linux_object::process::{LinuxProcess, RLimit, RLIMIT_SIGPENDING};
use linux_object::signal::queue::queued_for_user;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

#[test]
fn a_thread_directed_real_time_signal_past_the_limit_is_eagain() {
    let pid = 51_251;
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "q", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    proc.linux().set_resuid(51_251, 51_251, 51_251).unwrap();
    let two = RLimit { cur: 2, max: 2 };
    proc.linux()
        .rlimit(RLIMIT_SIGPENDING, Some(two), false)
        .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    let rt = Signal::SIGRT34;
    let tkill = SigInfo::from_user(rt, 3, 0, SignalCode::TKILL);
    let queued = SigInfo::queued(rt, 3, 0, 9);
    assert_eq!(queue_signal_to_thread(&thread, rt, tkill), Ok(()));
    assert_eq!(queue_signal_to_thread(&thread, rt, queued), Ok(()));
    assert_eq!(
        queue_signal_to_thread(&thread, rt, tkill),
        Err(LxError::EAGAIN)
    );
    assert_eq!(
        queue_signal_to_thread(&thread, rt, queued),
        Err(LxError::EAGAIN)
    );
    assert_eq!(
        thread.lock_linux().queued_entries_of(rt),
        2,
        "nothing refused stayed"
    );
    assert_eq!(queued_for_user(51_251), 2);
    let kill = SigInfo::from_user(rt, 3, 0, SignalCode::USER);
    assert_eq!(queue_signal_to_thread(&thread, rt, kill), Ok(()), "SI_USER");
    assert_eq!(thread.lock_linux().take_siginfo(rt).code, SignalCode::TKILL);
    assert_eq!(thread.lock_linux().take_siginfo(rt).code, SignalCode::QUEUE);
    assert!(!thread.lock_linux().signals.contains(rt));
    assert_eq!(queued_for_user(51_251), 0);
}
