//! `drm_master_check_perm`: who may `SET_MASTER` and `DROP_MASTER`.
//!
//! Linux keeps two things per `drm_file` for it: `pid`, the process that
//! opened the node, and `was_master`, set the first time the file holds the
//! device's master (`drm_new_set_master`). `SET_MASTER` and `DROP_MASTER`
//! are then allowed to a file that was master, from the process that opened
//! it, and to `CAP_SYS_ADMIN`; anyone else is EACCES before the EBUSY or
//! the EINVAL of the call itself. A compositor's fd passed to another
//! process cannot take the display from it, and an unprivileged client
//! that never had the master cannot take it from a compositor that dropped
//! it for a VT switch. Under `drm.master_check`, like the rest of the
//! master checks ([`super::master_check_enabled`]).
//!
//! A child of `drm_scheme` because both it and `drm.rs` sit at their size
//! ceilings (`xtask tamano`); the per-file record lives here, keyed by the
//! open's identity ([`super::DrmDev::file_owner`]), and goes when the open
//! does ([`super::drm::DrmFileState`]'s `Drop`).

use super::drm;
use alloc::vec::Vec;
use lock::Mutex;

/// One open: its identity, the pid that opened it, and whether it has ever
/// held the master.
struct FileRecord {
    file: usize,
    pid: u64,
    was_master: bool,
}

static FILES: Mutex<Vec<FileRecord>> = Mutex::new(Vec::new());

/// `drm_open_helper`: `file` was opened by `pid`, and holds the master
/// already when `drm_master_open` gave it to the first open of the node.
pub(crate) fn note_open(file: usize, pid: u64, master: bool) {
    let mut files = FILES.lock();
    files.retain(|r| r.file != file);
    files.push(FileRecord {
        file,
        pid,
        was_master: master,
    });
}

/// `drm_new_set_master`: `file` holds the master now, and `was_master`
/// stays set when it drops it.
pub(crate) fn note_master(file: usize) {
    if let Some(r) = FILES.lock().iter_mut().find(|r| r.file == file) {
        r.was_master = true;
    }
}

/// The open closed: its record goes with it, so a later open at the same
/// address starts over.
pub(crate) fn forget(file: usize) {
    FILES.lock().retain(|r| r.file != file);
}

/// `drm_master_check_perm`: `file` was master and the caller is the process
/// that opened it, or the caller is `CAP_SYS_ADMIN`.
pub(crate) fn check_perm(file: usize) -> bool {
    let pid = caller_pid();
    let from_its_master = FILES
        .lock()
        .iter()
        .any(|r| r.file == file && r.was_master && r.pid == pid);
    from_its_master || drm::current_is_root()
}

/// `task_pid(current)`: the caller's pid, which the tests, with no current
/// thread, set by hand so an fd handed to another process can be played.
pub(crate) fn caller_pid() -> u64 {
    #[cfg(test)]
    {
        let forced = TEST_PID.load(core::sync::atomic::Ordering::Relaxed);
        if forced != 0 {
            return forced;
        }
    }
    drm::current_pid()
}

#[cfg(test)]
static TEST_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The pid the next calls come from (0: the real one, which under test is
/// 0 too). Take [`drm::test_globals::lock`] around it.
#[cfg(test)]
pub(crate) fn set_test_pid(pid: u64) {
    TEST_PID.store(pid, core::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record on its own: an open that was never master is refused,
    /// one that was is allowed from its pid and from no other, a reopened
    /// address starts over, and a closed one is gone.
    #[test]
    fn the_record_follows_the_open_and_its_pid() {
        let _serialised = drm::test_globals::lock();
        set_test_pid(41);
        note_open(0xF11E, 41, false);
        assert!(!check_perm(0xF11E), "never master");
        note_master(0xF11E);
        assert!(check_perm(0xF11E), "was master, from its own pid");
        set_test_pid(42);
        assert!(!check_perm(0xF11E), "was master, from another pid");
        set_test_pid(41);
        note_open(0xF11E, 41, false);
        assert!(!check_perm(0xF11E), "reopened: the record starts over");
        note_open(0xF11E, 41, true);
        assert!(check_perm(0xF11E), "master from the open");
        forget(0xF11E);
        assert!(!check_perm(0xF11E), "closed");
        set_test_pid(0);
    }
}
