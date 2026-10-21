//! Who a path walk searches the directories on its way as.
//!
//! `link_path_walk` asks `may_lookup` -- search (`x`) permission on the
//! directory -- before it looks up each name, the last one included, from
//! the working directory or the descriptor as they are and without going
//! back over their ancestors. The walk here asked nobody, so any user opened
//! a file inside another user's 0700 directory, and root did with its DAC
//! capabilities dropped.

use super::proc_self::is_fd_dir_of;
use super::FileDesc;
use crate::error::{LxError, LxResult};
use crate::process::{
    Credentials, LinuxProcess, ACCESS_EXEC, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH,
};
use alloc::sync::Arc;
use rcore_fs::vfs::{FileType, FsError, INode, Metadata, Search, Walked};
use zircon_object::object::KernelObject;

/// Whose permission a path walk asks of each directory on its way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walker {
    /// The caller, by its fs ids and its effective capabilities: every walk
    /// a syscall makes.
    Caller,
    /// The caller by its real ids and what they hold: `access(2)` without
    /// `AT_EACCESS` (`access_override_creds`), whose walk is the real user's.
    RealIds,
    /// Nobody: the kernel finding the working directory again by its name,
    /// which Linux keeps as the directory itself and never walks to.
    Kernel,
}

impl LinuxProcess {
    /// A walk that asks, before each name, whether `walker` may search the
    /// directory the name is in.
    pub(crate) fn walked_by(&self, walker: Walker) -> Walked {
        let use_effective = match walker {
            Walker::Caller => true,
            Walker::RealIds => false,
            Walker::Kernel => return Walked::default(),
        };
        Walked {
            search: self.search_as(use_effective),
            ..Walked::default()
        }
    }

    /// The directory a syscall looks its last name up in, and where the
    /// walk ended: `dir_path` walked as the caller, and then searched
    /// itself, which `link_path_walk` asks of it before it hands the last
    /// name back. A name is never looked up in a directory the caller may
    /// not search, so what is in it -- EEXIST, ENOENT, a link -- is no
    /// answer to give.
    pub fn lookup_parent_walked(
        &self,
        dirfd: FileDesc,
        dir_path: &str,
    ) -> LxResult<(Arc<dyn INode>, Walked)> {
        let (dir, mut walked) = self.lookup_inode_walked(dirfd, dir_path, true)?;
        let meta = dir.metadata()?;
        if meta.type_ != FileType::Dir {
            return Err(LxError::ENOTDIR);
        }
        walked.may_look_in(&dir, &meta)?;
        Ok((dir, walked))
    }

    /// [`Self::lookup_parent_walked`], without where the walk ended.
    pub fn lookup_parent_at(&self, dirfd: FileDesc, dir_path: &str) -> LxResult<Arc<dyn INode>> {
        Ok(self.lookup_parent_walked(dirfd, dir_path)?.0)
    }
}

/// What a process's walks ask each directory, by its effective ids and by
/// its real ones, each made once for a set of credentials: every walk would
/// otherwise copy the credentials into a question of its own.
#[derive(Default)]
pub(crate) struct Searches {
    caller: Option<(Credentials, Search)>,
    real_ids: Option<(Credentials, Search)>,
}

impl Searches {
    /// The question a walk as `creds` asks, by their effective ids or their
    /// real ones; none for whoever may search every directory, who is asked
    /// nothing, as the kernel is not.
    pub(crate) fn ask_as(&mut self, creds: &Credentials, use_effective: bool) -> Option<Search> {
        let caps = if use_effective {
            creds.caps
        } else {
            creds.caps.for_access(creds.ruid)
        };
        if caps.has(CAP_DAC_READ_SEARCH) || caps.has(CAP_DAC_OVERRIDE) {
            return None;
        }
        let kept = if use_effective {
            &mut self.caller
        } else {
            &mut self.real_ids
        };
        match kept {
            Some((as_of, search)) if as_of == creds => Some(search.clone()),
            _ => {
                let asked = creds.clone();
                let search: Search = Arc::new(move |dir: &dyn INode, meta: &Metadata| {
                    may_search(&asked, use_effective, current_process_id(), dir, meta)
                });
                *kept = Some((creds.clone(), search.clone()));
                Some(search)
            }
        }
    }
}

/// The process of the thread making the call, whose own `fd` directory it
/// may always search; 0, which is no process, on a host test thread.
fn current_process_id() -> u64 {
    kernel_hal::thread::get_current_thread()
        .and_then(|t| t.downcast::<zircon_object::task::Thread>().ok())
        .map_or(0, |t| t.proc().id())
}

/// `inode_permission(dir, MAY_EXEC)` as `creds` (by their effective or
/// their real ids), and `proc_fd_permission`: a process searches its own
/// `fd` directory whatever its owner and mode say.
fn may_search(
    creds: &Credentials,
    use_effective: bool,
    caller: u64,
    dir: &dyn INode,
    meta: &Metadata,
) -> rcore_fs::vfs::Result<()> {
    let verdict = LinuxProcess::access_verdict(
        creds,
        meta.uid as u32,
        meta.gid as u32,
        meta.mode,
        true,
        ACCESS_EXEC,
        use_effective,
    );
    if verdict.is_ok() || is_fd_dir_of(dir, caller) {
        Ok(())
    } else {
        Err(FsError::NoPermission)
    }
}

/// Sets `meta`, with a new mode, on `inode`. A directory everybody could
/// search is no gate, and the path cache serves a walk through it without
/// asking, so a new mode of a directory makes it forget what it remembers.
/// A new owner needs nothing: a gate is asked with its metadata as it is,
/// and a directory everybody may search stays one whoever owns it.
pub fn set_mode(inode: &Arc<dyn INode>, meta: &Metadata) -> LxResult {
    inode.set_metadata(meta)?;
    if meta.type_ == FileType::Dir {
        super::dcache_invalidate();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
