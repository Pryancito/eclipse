//! hunter's ransomware hooks on the file syscalls (see `hunter::ransomware`).
//!
//! hunter judges the *shape* of what a process does to files -- many of them
//! rewritten with random-looking bytes, renamed to one new extension, the
//! originals deleted -- so it has to be told about every write, rename and
//! unlink of a regular file, with the path it is known by. The syscalls call
//! the three helpers here at the point where the operation has passed its own
//! checks and is about to happen, and refuse it with `EPERM` when hunter says
//! no (the `behavior` domain in `Enforce`, or a process already contained).
//!
//! With the domain off each helper is one atomic load: nothing is downcast,
//! no path is built.

use super::*;
use alloc::string::String;
use linux_object::error::LxResult;
use linux_object::fs::vfs::Walked;

impl Syscall<'_> {
    /// Whether `chunk` may be written to `file_like`. Only a regular file
    /// counts: sockets are not `File`s, and pipes, FIFOs and device nodes are
    /// not anybody's documents.
    pub(super) fn hunter_allows_write(&self, file_like: &Arc<dyn FileLike>, chunk: &[u8]) -> bool {
        if !hunter::behavior::watching() {
            return true;
        }
        let file = match file_like.downcast_ref::<File>() {
            Some(f) => f,
            None => return true,
        };
        if file.inode().downcast_ref::<Pipe>().is_some() {
            return true;
        }
        let mut size = || file.metadata().map_or(0, |m| m.size as u64);
        hunter::check_file_write(self.zircon_process().id(), file.path(), chunk, &mut size)
    }

    /// Whether the rename of `old_name` in the directory `old_dir` walked to
    /// `new_name` in `new_dir` may go ahead.
    pub(super) fn hunter_allows_rename(
        &self,
        old_dir: &Walked,
        old_name: &str,
        new_dir: &Walked,
        new_name: &str,
    ) -> LxResult<()> {
        if !hunter::behavior::watching() {
            return Ok(());
        }
        let (old, new) = (joined(old_dir, old_name), joined(new_dir, new_name));
        if hunter::check_file_rename(self.zircon_process().id(), &old, &new) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }

    /// Whether `name` in the directory `dir` walked to may be unlinked.
    pub(super) fn hunter_allows_unlink(&self, dir: &Walked, name: &str) -> LxResult<()> {
        if !hunter::behavior::watching() {
            return Ok(());
        }
        if hunter::check_file_unlink(self.zircon_process().id(), &joined(dir, name)) {
            Ok(())
        } else {
            Err(LxError::EPERM)
        }
    }
}

/// The absolute path of `name` in the directory a walk ended on. `Walked`
/// names the directory from `/`, whatever `dirfd` and relative path the
/// caller used, so `rename("a", "b")` in `/home/u` is judged as
/// `/home/u/a` -> `/home/u/b`, the same paths a canary is declared by.
fn joined(dir: &Walked, name: &str) -> String {
    let mut p = dir.path();
    if !p.ends_with('/') {
        p.push('/');
    }
    p.push_str(name);
    p
}
