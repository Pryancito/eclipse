//! `/proc/<pid>/fdinfo`: one file per descriptor, with its offset, its flags
//! and whatever its kind says of itself (`proc_fdinfo_operations`).
//!
//! It did not exist. `lsof` and `ss` read it for the offset and the flags of
//! each descriptor, and the eventfd, pidfd and signalfd lines are the only
//! place a program can learn an eventfd's count or the pid a pidfd names
//! without consuming or waiting for anything.

use super::ProcSelfFdDir;
use crate::fs::file::File;
use crate::fs::pseudo::Pseudo;
use crate::fs::{FileDesc, OpenFlags};
use crate::process::ProcessExt;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::convert::TryFrom;
use rcore_fs::vfs::*;
use zircon_object::object::KernelObject;
use zircon_object::task::Process;

/// Inode numbers of the files in `/proc/<pid>/fdinfo`, one per descriptor
/// number (`NR_OPEN` of them) per pid. They start above every other range of
/// procfs, the per-thread one (`TID_INODE_BASE`) included, so that two of
/// them are never the same file to `diff` or `cmp`, which compare by inode
/// before they read.
const FDINFO_INODE_BASE: usize = 0x1_0000_0000;

fn fdinfo_inode(pid: u64, fd: FileDesc) -> usize {
    let fd: i32 = fd.into();
    FDINFO_INODE_BASE + (pid as usize) * (crate::process::NR_OPEN as usize) + fd as usize
}

/// `/proc/<pid>/fdinfo`, listed as `/proc/<pid>/fd` is: `.`, `..` and then
/// each descriptor, by number.
pub struct ProcFdInfoDir {
    pub process: Arc<Process>,
}

impl ProcFdInfoDir {
    fn fd_dir(&self) -> ProcSelfFdDir {
        ProcSelfFdDir {
            process: self.process.clone(),
        }
    }
}

impl INode for ProcFdInfoDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        let metadata = Metadata {
            inode: crate::fs::procfs::pid_inode(
                self.process.id(),
                crate::fs::procfs::PID_FDINFO_DIR_SLOT,
            ),
            nlinks: 2,
            blk_size: crate::fs::procfs::BLOCK_SIZE,
            ..Pseudo::new("", FileType::Dir).metadata()?
        };
        Ok(crate::fs::procfs::owner::of(&self.process, metadata))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(crate::fs::procfs::ProcFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(ProcFdInfoDir {
                process: self.process.clone(),
            })),
            ".." => Ok(crate::fs::procfs::pid_dir_inode(self.process.id())),
            _ => {
                let fd = FileDesc::try_from(name).map_err(|_| FsError::EntryNotFound)?;
                self.process
                    .try_linux()
                    .ok_or(FsError::EntryNotFound)?
                    .get_file_like(fd)
                    .map_err(|_| FsError::EntryNotFound)?;
                Ok(Arc::new(FdInfo {
                    process: self.process.clone(),
                    fd,
                }))
            }
        }
    }
    /// The entries are described without looking at the descriptors, for
    /// the reason `ProcSelfFdDir`'s are: an open `/proc/self/fdinfo` is one
    /// of its own entries, and `getdents` holds its lock while it lists.
    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        let name = self.get_entry(id)?;
        let meta = match id {
            0 | 1 => self.find(&name)?.metadata()?,
            _ => FdInfo {
                process: self.process.clone(),
                fd: FileDesc::try_from(name.as_str()).map_err(|_| FsError::EntryNotFound)?,
            }
            .metadata()?,
        };
        Ok((meta, name))
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        self.fd_dir().get_entry(id)
    }
}

/// `/proc/<pid>/fdinfo/N`, written when it is read: `pos`, `flags` (the
/// `F_GETFL` flags, with `O_CLOEXEC` if the descriptor has it), `mnt_id`,
/// `ino`, and then the lines of the descriptor's kind.
///
/// `mnt_id` is always 0: a descriptor does not keep the mount it was opened
/// on, so it cannot name one of the numbers in `/proc/<pid>/mountinfo`.
pub struct FdInfo {
    process: Arc<Process>,
    fd: FileDesc,
}

impl FdInfo {
    fn render(&self) -> Result<String> {
        let linux = self.process.try_linux().ok_or(FsError::EntryNotFound)?;
        let file = linux
            .get_file_like(self.fd)
            .map_err(|_| FsError::EntryNotFound)?;
        let mut flags = file.flags().status();
        flags.set(
            OpenFlags::CLOEXEC,
            linux.fd_cloexec(self.fd).unwrap_or(false),
        );
        // `f_pos`, which only a `File` has; `dir_position` is its offset.
        let pos = file.downcast_ref::<File>().map_or(0, |f| f.dir_position());
        let ino = file.metadata().map_or(0, |m| m.inode);
        Ok(alloc::format!(
            "pos:\t{}\nflags:\t0{:o}\nmnt_id:\t0\nino:\t{}\n{}",
            pos,
            flags.bits(),
            ino,
            file.fdinfo()
        ))
    }
}

impl INode for FdInfo {
    /// A descriptor closed since this file was opened is `ENOENT`, as
    /// `seq_fdinfo_open`'s `seq_show` answers it.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let text = self.render()?;
        let rest = text.as_bytes().get(offset..).unwrap_or(&[]);
        let len = rest.len().min(buf.len());
        buf[..len].copy_from_slice(&rest[..len]);
        Ok(len)
    }
    fn seq_text(&self) -> Option<Result<alloc::vec::Vec<u8>>> {
        Some(self.render().map(String::into_bytes))
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    /// Never looks at the descriptor: the listing calls this.
    fn metadata(&self) -> Result<Metadata> {
        let metadata = Metadata {
            inode: fdinfo_inode(self.process.id(), self.fd),
            size: 0,
            blk_size: crate::fs::procfs::BLOCK_SIZE,
            ..Pseudo::new("", FileType::File).metadata()?
        };
        Ok(crate::fs::procfs::owner::of(&self.process, metadata))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod inode_tests;
#[cfg(test)]
mod tests;
