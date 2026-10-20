//! The death of a process, told to the layers above this crate that keep
//! state by pid (`register_process_exit_hook`): the POSIX timers of
//! `linux-syscall` outlived their process because nothing here could
//! reach their table.
//!
//! The hook is registered for the whole binary and every Linux process that
//! dies anywhere in it runs this one, so the question has to be about OUR
//! pid and never about a count. A global counter here read `before + 1` and
//! was green while nothing else in the binary happened to exit a process
//! next to it; it went red about three runs in ten the moment something
//! did.
use super::*;
use core::sync::atomic::{AtomicU64, AtomicUsize};
use rcore_fs_ramfs::RamFS;

/// The pid this test is asking about. Somebody else's process dying is not
/// this test's business, and at sixteen threads there is always somebody
/// else.
static WATCHED: AtomicU64 = AtomicU64::new(0);
static DEATHS_OF_THE_WATCHED: AtomicUsize = AtomicUsize::new(0);
static PID_THE_HOOK_WAS_TOLD: AtomicU64 = AtomicU64::new(0);

fn note_death(pid: KoID) {
    if pid != WATCHED.load(Ordering::SeqCst) {
        return;
    }
    DEATHS_OF_THE_WATCHED.fetch_add(1, Ordering::SeqCst);
    PID_THE_HOOK_WAS_TOLD.store(pid, Ordering::SeqCst);
}

#[test]
fn a_hook_runs_once_per_death_with_the_dead_pid() {
    let pid = 0x5e12;
    WATCHED.store(pid, Ordering::SeqCst);
    register_process_exit_hook(note_death);
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();

    proc.exit(3);

    assert_eq!(
        DEATHS_OF_THE_WATCHED.load(Ordering::SeqCst),
        1,
        "the hook ran a different number of times than the process died"
    );
    assert_eq!(
        PID_THE_HOOK_WAS_TOLD.load(Ordering::SeqCst),
        pid,
        "the hook was told a pid that was not the one that died"
    );
}
