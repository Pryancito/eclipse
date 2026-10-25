//! Where a process is: the working directory itself, and the path it is
//! known by.
//!
//! It was only the path, and every relative name was looked up from the root
//! along it again. So a directory renamed while a process was in it took
//! every relative name with it: after an `mv` of the shell's own directory,
//! `ls`, `cd ..` and `getcwd` all failed with `ENOENT`, where Linux follows
//! the directory to its new name. In one that was removed, `.` could not be
//! opened or stat'ed and `cd ..` could not get out, so the process was stuck
//! there. And a directory removed and made again under the same name was
//! taken for the old one.
//!
//! Now the directory is held, as Linux holds `fs->pwd`. Relative names start
//! from it, whatever it is called now. The path is a cache of its name. It
//! holds while no name anywhere has changed (the `dcache` epoch). After a
//! change it is looked up again, and when it leads somewhere else the name is
//! found by climbing `..` to the root.

use super::*;
use crate::fs::{FileDesc, Walker};
use core::any::Any;
use rcore_fs_mountfs::MNode;

/// The directory a process is in, held.
#[derive(Clone)]
pub(super) struct HeldDirectory {
    dir: Arc<dyn INode>,
    /// The `dcache` epoch at which the path was last found to lead to `dir`,
    /// or [`NEVER`].
    seen: u64,
}

/// An epoch the cache never reaches: the path has not been checked yet.
const NEVER: u64 = u64::MAX;

/// How far `..` is climbed looking for the root: as many directories as a
/// `PATH_MAX` path can name.
const MAX_DEPTH: usize = 2048;

impl LinuxProcess {
    /// The path of the working directory now, or the last one it had once it
    /// has been removed.
    pub fn current_working_directory(&self) -> String {
        self.working_directory_path()
            .unwrap_or_else(|| self.working_directory_as_entered())
    }

    /// The path the working directory was entered by, or last found at,
    /// without asking whether it still leads there.
    pub(crate) fn working_directory_as_entered(&self) -> String {
        String::from("/") + &self.inner.lock().current_working_directory
    }

    /// The working directory, where a relative name is looked up from, and
    /// its path ([`Self::current_working_directory`]).
    pub fn working_directory(&self) -> LxResult<(Arc<dyn INode>, String)> {
        let held = self.inner.lock().working_directory.clone();
        let path = self.current_working_directory();
        match held {
            Some(held) => Ok((held.dir, path)),
            None => Ok((self.root_inode().lookup(&path)?, path)),
        }
    }

    /// Change working directory to `path`, from the working directory when
    /// it is relative, holding the directory it names.
    pub fn change_directory(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        let from = match path.as_bytes()[0] {
            b'/' => String::new(),
            _ => self.inner.lock().current_working_directory.clone(),
        };
        let folded = String::from("/") + &fold(&from, path);
        let dir = self
            .lookup_inode_at_by(FileDesc::CWD, &folded, true, Walker::Kernel)
            .ok();
        self.set_working_directory(&folded, dir);
    }

    /// Go into `dir`, which `path` names from the root (`chdir`, `fchdir`).
    pub fn enter_directory(&self, dir: Arc<dyn INode>, path: &str) {
        self.set_working_directory(&fold("", path), Some(dir));
    }

    fn set_working_directory(&self, folded: &str, dir: Option<Arc<dyn INode>>) {
        let mut inner = self.inner.lock();
        inner.current_working_directory = String::from(folded.trim_start_matches('/'));
        inner.working_directory = dir.map(|dir| HeldDirectory { dir, seen: NEVER });
    }

    /// The path of the working directory now, or `None` once it has been
    /// removed (`getcwd`'s `ENOENT`).
    pub fn working_directory_path(&self) -> Option<String> {
        let (held, entered) = {
            let inner = self.inner.lock();
            let entered = String::from("/") + &inner.current_working_directory;
            (inner.working_directory.clone(), entered)
        };
        let Some(held) = held else {
            // Never held: whatever the path names, if it is a directory.
            let dir = self
                .named_by(&entered)
                .and_then(|inode| Ok(inode.metadata()?));
            return matches!(dir, Ok(m) if m.type_ == FileType::Dir).then_some(entered);
        };
        // Taken before the lookups, so a name that changes during them makes
        // the next call look again.
        let now = crate::fs::dcache::epoch();
        if held.seen == now {
            return Some(entered);
        }
        let (path, renamed) = match self.named_by(&entered) {
            Ok(named) if same_node(&named, &held.dir) => (entered, false),
            _ => (self.climb_to_root(&held.dir)?, true),
        };
        let mut inner = self.inner.lock();
        if let Some(kept) = inner.working_directory.as_mut() {
            if same_object(&kept.dir, &held.dir) {
                kept.seen = now;
                if renamed {
                    inner.current_working_directory = String::from(&path[1..]);
                }
            }
        }
        Some(path)
    }

    /// What `path`, from the root and through no link, names now. From the
    /// root inode, as relative names used to be found, except in `/proc`,
    /// `/sys` and `/dev`, which the kernel's own walk goes straight into.
    fn named_by(&self, path: &str) -> LxResult<Arc<dyn INode>> {
        if crate::fs::virtual_fs_top(path).is_some() {
            return self.lookup_inode_at_by(FileDesc::CWD, path, true, Walker::Kernel);
        }
        Ok(self.root_inode().lookup(path)?)
    }

    /// Whether the working directory has been removed since the process went
    /// into it.
    pub fn working_directory_removed(&self) -> bool {
        self.working_directory_path().is_none()
    }

    /// What `/proc/<pid>/cwd` points at: the working directory, and
    /// ` (deleted)` after it once it has gone (`d_path`).
    pub fn working_directory_link(&self) -> String {
        self.working_directory_path()
            .unwrap_or_else(|| self.working_directory_as_entered() + " (deleted)")
    }

    /// The path of `dir` from the root, found by climbing `..` and asking
    /// each parent what it calls the child. `None` when some directory on the
    /// way is no longer in its parent, or the climb ends at a top that is not
    /// the root.
    fn climb_to_root(&self, dir: &Arc<dyn INode>) -> Option<String> {
        let root = self.root_inode();
        let mut names = Vec::new();
        let mut here = dir.clone();
        for _ in 0..MAX_DEPTH {
            if same_node(&here, &root) {
                names.reverse();
                return Some(String::from("/") + &names.join("/"));
            }
            let up = here.find("..").ok()?;
            names.push(name_in(&up, &here)?);
            here = up;
        }
        None
    }
}

/// `path` folded onto `from`: each `.` dropped and each `..` taken back,
/// with no leading `/`.
fn fold(from: &str, path: &str) -> String {
    let mut parts: Vec<&str> = from.split('/').filter(|x| !x.is_empty()).collect();
    for seg in path.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            _ => parts.push(seg),
        }
    }
    parts.join("/")
}

/// The same `Arc`, whatever it is.
fn same_object(a: &Arc<dyn INode>, b: &Arc<dyn INode>) -> bool {
    Arc::as_ptr(a) as *const u8 == Arc::as_ptr(b) as *const u8
}

/// Whether `a` and `b` are one directory. Through the mounts that is the
/// same inode of the same mounted filesystem, since every filesystem here
/// reports `dev` 0 and two of them number their inodes alike. Outside them
/// (`/dev`, `/proc`, a test's own `RamFS`) it is the number.
fn same_node(a: &Arc<dyn INode>, b: &Arc<dyn INode>) -> bool {
    if same_object(a, b) {
        return true;
    }
    match (mnode(a), mnode(b)) {
        (Some(a), Some(b)) => a.is_same_node(b),
        (None, None) => matches!(
            (a.metadata(), b.metadata()),
            (Ok(x), Ok(y)) if x.dev == y.dev && x.inode == y.inode
        ),
        _ => false,
    }
}

/// The `MNode` `inode` is, if it is one. Asked of `Any` itself: an `MNode`
/// answers `as_any_ref` with the inode it wraps, so that downcasts see
/// through the mounts, and that answer is never an `MNode`.
fn mnode(inode: &Arc<dyn INode>) -> Option<&MNode> {
    (&**inode as &dyn Any).downcast_ref::<MNode>()
}

/// The name `dir` has for `child`. Through the mounts, `find` gives a mount
/// point's name the root of what is mounted on it, which is the child a
/// climb out of that filesystem comes from.
fn name_in(dir: &Arc<dyn INode>, child: &Arc<dyn INode>) -> Option<String> {
    for id in 0.. {
        let name = dir.get_entry(id).ok()?;
        if name == "." || name == ".." {
            continue;
        }
        if dir.find(&name).is_ok_and(|found| same_node(&found, child)) {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests;
