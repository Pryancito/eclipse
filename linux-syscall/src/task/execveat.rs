//! `execveat(2)`: `execve` of a path relative to a directory descriptor, or of
//! the descriptor itself; and what both execs call the program they run.
//!
//! It answered `ENOSYS`. `fexecve(3)` in glibc and in musl tries it first and
//! then falls back to `execve("/proc/self/fd/N")`, which ran the file but
//! named the program `/proc/self/fd/N`: `/proc/self/exe` pointed at a
//! descriptor the new image did not have, and `comm` was its number.
//! Everything else that calls it directly -- a sandbox that runs what it
//! opened, a supervisor that holds a directory -- just failed.
//!
//! `/proc/self/exe` was the path as the caller wrote it: relative after
//! `execve("./prog")`, so wrong after the first `chdir`, and the link's own
//! path after an exec through a symbolic link, so a program that finds its
//! files next to itself -- `java` through `/usr/bin/java`, and `$ORIGIN` in
//! the dynamic linker, which reads `/proc/self/exe` -- looked in the
//! directory of the link.

use super::*;
use crate::file::{at_flags, AtFlags};
use linux_object::fs::File;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};

/// What `execveat` accepts (`do_open_execat`, `fs/exec.c`: `if ((flags &
/// ~(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)) != 0) return ERR_PTR(-EINVAL);`).
const EXECVEAT_FLAGS: usize = AtFlags::SYMLINK_NOFOLLOW.bits() | AtFlags::EMPTY_PATH.bits();

/// `MAXSYMLINKS`: how many links [`real_path`] follows before it gives up on
/// the walk and keeps the path it was given.
const MAX_LINKS: usize = 40;

/// The file an exec runs, and the names it goes by.
pub(super) struct ExecTarget {
    pub(super) inode: Arc<dyn INode>,
    /// The name the loader is handed, which a `#!` interpreter is given to
    /// open: the caller's own path when it names the file without help from
    /// a descriptor, as `bprm->filename` is.
    pub(super) name: String,
    /// Where the file is, from the root and through no link (`d_path`): what
    /// `/proc/self/exe` says, and what the `noexec` and `nosuid` questions
    /// are asked of, since it is that file's mount that answers them.
    pub(super) absolute: String,
    /// `AT_EMPTY_PATH`: the descriptor is the file, and there is no name to
    /// cut `comm` from but the file's own.
    by_descriptor: bool,
}

/// Find what `execveat(dirfd, path, .., flags)` runs; `execve` is `dirfd`
/// `AT_FDCWD` and no flags.
///
/// Linux names a file reached through a descriptor `/dev/fd/N` or
/// `/dev/fd/N/path` to a `#!` interpreter. Here it is the path the file was
/// opened by, which the interpreter can open whether or not the descriptor is
/// close-on-exec.
pub(super) fn exec_target(
    proc: &LinuxProcess,
    dirfd: FileDesc,
    path: &str,
    flags: usize,
) -> LxResult<ExecTarget> {
    let flags = at_flags(flags, EXECVEAT_FLAGS)?;
    if path.is_empty() && flags.contains(AtFlags::EMPTY_PATH) {
        // The descriptor itself; `AT_FDCWD` is the working directory, which
        // is no program (EACCES further on, as any directory).
        let (inode, opened_as) = if dirfd == FileDesc::CWD {
            let cwd = proc.current_working_directory();
            (proc.lookup_inode(&cwd)?, cwd)
        } else {
            // A socket or an eventfd is a descriptor but not a file: Linux
            // gets as far as `S_ISREG` with it, which says `EACCES`.
            let file = proc
                .get_file_like(dirfd)?
                .downcast_arc::<File>()
                .map_err(|_| LxError::EACCES)?;
            (file.inode(), file.path().clone())
        };
        let absolute = real_path(proc, &opened_as);
        return Ok(ExecTarget {
            inode,
            name: absolute.clone(),
            absolute,
            by_descriptor: true,
        });
    }
    let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
    let inode = proc.lookup_inode_at(dirfd, path, follow)?;
    // `O_NOFOLLOW` on the last component: a link there is `ELOOP`.
    if !follow && inode.metadata()?.type_ == FileType::SymLink {
        return Err(LxError::ELOOP);
    }
    // The path as the walk read it, from the root: `..` after a link is the
    // link target's parent, so nothing here may be folded before the walk.
    let walked = if path.starts_with('/') {
        String::from(path)
    } else if dirfd == FileDesc::CWD {
        proc.current_working_directory() + "/" + path
    } else {
        proc.get_file(dirfd)?.path().clone() + "/" + path
    };
    // `/proc/self/fd/N` leads to the open file, wherever it is.
    let magic = proc
        .get_absolute_path(dirfd, path)?
        .strip_prefix("/proc/self/fd/")
        .and_then(|n| FileDesc::try_from(n).ok());
    let walked = match magic {
        Some(fd) => proc.get_file(fd)?.path().clone(),
        None => walked,
    };
    let absolute = real_path(proc, &walked);
    let name = if path.starts_with('/') || dirfd == FileDesc::CWD {
        String::from(path)
    } else {
        walked
    };
    Ok(ExecTarget {
        inode,
        name,
        absolute,
        by_descriptor: false,
    })
}

/// `path`, from the root whether or not it starts with `/`, with each
/// symbolic link on it replaced by where it leads and each `.` and `..` taken
/// away: `realpath(3)`, done the way the lookup walked it.
///
/// A step the walk cannot take -- a component that is not there any more, a
/// link that cannot be read, a chain longer than `MAXSYMLINKS` -- leaves the
/// path as given, folded, which is what this said before it followed links.
pub(super) fn real_path(proc: &LinuxProcess, path: &str) -> String {
    let fold = |parts: &[String]| String::from("/") + &parts.join("/");
    let given = || {
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ => parts.push(String::from(part)),
            }
        }
        fold(&parts)
    };
    // What is left to walk, last component first, and what has been walked.
    let mut todo: Vec<String> = path.rsplit('/').map(String::from).collect();
    let mut done: Vec<String> = Vec::new();
    let mut links = 0;
    while let Some(part) = todo.pop() {
        match part.as_str() {
            "" | "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        done.push(part);
        let here = fold(&done);
        let Ok(inode) = proc.lookup_inode_at(FileDesc::CWD, &here, false) else {
            return given();
        };
        if !matches!(inode.metadata(), Ok(m) if m.type_ == FileType::SymLink) {
            continue;
        }
        links += 1;
        let mut target = vec![0u8; 4096];
        let Ok(len) = inode.read_at(0, &mut target) else {
            return given();
        };
        let Ok(target) = core::str::from_utf8(&target[..len]) else {
            return given();
        };
        if links > MAX_LINKS || target.is_empty() {
            return given();
        }
        done.pop();
        if target.starts_with('/') {
            done.clear();
        }
        todo.extend(target.rsplit('/').map(String::from));
    }
    fold(&done)
}

impl ExecTarget {
    /// What `/proc/self/exe` and `comm` say once the exec is done, given
    /// `loaded`, the path the loader ran last: [`Self::name`] for a program,
    /// the interpreter's for a script, which the loader looks up from the
    /// root.
    ///
    /// `/proc/self/exe` is the file, from the root and through no link,
    /// whatever name ran it; a script's is its interpreter's, as on Linux.
    /// `comm` is cut from the name the caller used (`kbasename(bprm->
    /// filename)`), so a script is called by its own name and not `sh`; with
    /// no name, from the file that runs (the dentry Linux takes it from).
    pub(super) fn identity(&self, proc: &LinuxProcess, loaded: String) -> (String, String) {
        let exe = if loaded == self.name {
            self.absolute.clone()
        } else {
            real_path(proc, &loaded)
        };
        let comm = if self.by_descriptor {
            comm_from_path(&exe)
        } else {
            comm_from_path(&self.name)
        };
        let comm = String::from(comm);
        (exe, comm)
    }
}

impl Syscall<'_> {
    /// `execveat(2)`: see [`exec_target`] for what it runs; the rest is
    /// `execve`'s.
    pub async fn sys_execveat(
        &mut self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        argv: UserInPtr<UserInPtr<u8>>,
        envp: UserInPtr<UserInPtr<u8>>,
        flags: usize,
    ) -> SysResult {
        self.exec_at(dirfd, path, argv, envp, flags).await
    }
}

#[cfg(test)]
mod tests;
