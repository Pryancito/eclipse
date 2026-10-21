//! Who owns the files of `/proc/<pid>`: `task_dump_owner`, measured on Linux
//! 6.18 with glibc and musl. Split out of `procfs.rs` for the size ratchet.

use rcore_fs::vfs::{FileType, Metadata};
use zircon_object::task::{Process, Status};

use crate::process::{ProcessExt, SUID_DUMP_USER};

/// `metadata` of a file of `proc`'s, owned as `task_dump_owner` says: by the
/// effective ids of the process, but by root for anything other than a
/// directory of mode 0555 while the process is not dumpable or, as a zombie,
/// has no memory left.
///
/// Everything was root's. busybox `ps` and `top` take the user of a process
/// from `stat` of `/proc/<pid>`, so they named root as the user of every
/// process, and a process that was not root could not write the `comm` of
/// its own threads (0644), which is what `pthread_setname_np` does for a
/// thread other than the caller.
pub(crate) fn of(proc: &Process, metadata: Metadata) -> Metadata {
    let Some(linux) = proc.try_linux() else {
        return metadata;
    };
    // "To keep the rules in /proc simple I have made this apply to all per
    // process world readable and executable directories."
    let its_own = (metadata.type_ == FileType::Dir && metadata.mode == 0o555)
        || (linux.dumpable() == SUID_DUMP_USER && !matches!(proc.status(), Status::Exited(_)));
    if !its_own {
        return metadata;
    }
    Metadata {
        uid: linux.euid() as usize,
        gid: linux.egid() as usize,
        ..metadata
    }
}

/// [`of`] for the process `pid`, or root's if it is gone, as `pid_getattr`
/// leaves a file of a process that has been reaped.
pub(crate) fn of_pid(pid: u64, metadata: Metadata) -> Metadata {
    match crate::process::find_task(pid as _) {
        Some(proc) => of(&proc, metadata),
        None => metadata,
    }
}
