//! Where `chdir` and `fchdir` leave a process: the directory from the root
//! and through no symbolic link, which is what `getcwd` reports.
//!
//! It was the path folded as the caller wrote it. After `cd /var/run`, a
//! link to `/run`, `getcwd` said `/var/run` where Linux says `/run`; and
//! every relative lookup failed from there, because the walk to the working
//! directory does not follow links: it stopped at the link itself and went
//! no further, so `ls` in a linked directory was `ENOTDIR`. A `..` after a
//! link went back up the path as written, not up from where the link led.

use super::*;
use linux_object::process::LinuxProcess;

/// The directory `path` names for `proc`, from the root and through no
/// link (a relative `path` from `proc`'s working directory).
pub(super) fn physical_dir(proc: &LinuxProcess, path: &str) -> String {
    let from_root = if path.starts_with('/') {
        String::from(path)
    } else {
        proc.current_working_directory() + "/" + path
    };
    crate::task::real_path(proc, &from_root)
}

#[cfg(test)]
mod tests;
