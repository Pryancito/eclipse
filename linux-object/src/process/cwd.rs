//! Whether a process's working directory is still there.
//!
//! A directory removed while a process was in it went on being reported by
//! `getcwd` as if nothing had happened, where Linux says `ENOENT` (a shell
//! then warns "getcwd: cannot access parent directories"), and
//! `/proc/<pid>/cwd` did not exist at all, so `pwdx`, `lsof` and a
//! `readlink("/proc/self/cwd")` found nothing.

use super::*;
use crate::fs::FileDesc;

impl LinuxProcess {
    /// Whether the working directory has gone since the process went into
    /// it: its path no longer names a directory.
    ///
    /// The working directory is kept as a path, so one removed and made
    /// again under the same name counts as still there, where Linux would
    /// still see the removed one.
    pub fn working_directory_removed(&self) -> bool {
        let cwd = self.current_working_directory();
        let dir = self
            .lookup_inode_at(FileDesc::CWD, &cwd, true)
            .and_then(|inode| Ok(inode.metadata()?));
        !matches!(dir, Ok(m) if m.type_ == FileType::Dir)
    }

    /// What `/proc/<pid>/cwd` points at: the working directory, and
    /// ` (deleted)` after it once it has gone (`d_path`).
    pub fn working_directory_link(&self) -> String {
        let cwd = self.current_working_directory();
        if self.working_directory_removed() {
            cwd + " (deleted)"
        } else {
            cwd
        }
    }
}

#[cfg(test)]
mod tests;
