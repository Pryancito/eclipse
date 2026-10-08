//! The death of a process, told to the layers above this crate that keep
//! state by pid (`register_process_exit_hook`): the POSIX timers of
//! `linux-syscall` outlived their process because nothing here could
//! reach their table.
use super::*;
use core::sync::atomic::{AtomicU64, AtomicUsize};
use rcore_fs_ramfs::RamFS;

static DEATHS: AtomicUsize = AtomicUsize::new(0);
static LAST_DEAD: AtomicU64 = AtomicU64::new(0);

fn note_death(pid: KoID) {
    DEATHS.fetch_add(1, Ordering::SeqCst);
    LAST_DEAD.store(pid, Ordering::SeqCst);
}

#[test]
fn a_hook_runs_once_per_death_with_the_dead_pid() {
    register_process_exit_hook(note_death);
    let pid = 0x5e12;
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    let before = DEATHS.load(Ordering::SeqCst);

    proc.exit(3);

    assert_eq!(
        DEATHS.load(Ordering::SeqCst),
        before + 1,
        "the hook ran a different number of times than the process died"
    );
    assert_eq!(LAST_DEAD.load(Ordering::SeqCst), pid);
}
