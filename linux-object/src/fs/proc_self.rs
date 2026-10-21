use crate::fs::file::File;
use crate::fs::pseudo::Pseudo;
use crate::fs::{FileDesc, FileLike};
use crate::process::ProcessExt;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use rcore_fs::vfs::*;
use zircon_object::object::KernelObject;
use zircon_object::task::Process;

mod fdinfo;
pub use fdinfo::ProcFdInfoDir;

pub struct ProcSelfFdDir {
    pub process: Arc<Process>,
}

impl INode for ProcSelfFdDir {
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
    /// Its size is how many descriptors are open, as since Linux 6.2
    /// (`proc_fd_getattr`).
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: super::procfs::pid_inode(self.process.id(), super::procfs::PID_FD_DIR_SLOT),
            size: self.process.try_linux().map_or(0, |lp| lp.file_count()),
            blk_size: super::procfs::BLOCK_SIZE,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o555,
            nlinks: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(crate::fs::procfs::ProcFS)
    }
    /// `.` is this directory again and `..` is `/proc/<pid>`.
    ///
    /// Both used to answer with a `Pseudo` directory holding the *text*
    /// `/proc/self/fd`, which is neither. A `Pseudo` implements no `find` and
    /// no `get_entry`, so `opendir("/proc/self/fd/.")` handed back a directory
    /// with nothing in it: libdbus's `_dbus_fd_set_all_close_on_exec()` walks
    /// this directory, and against a path that ends in `/.` it read zero
    /// descriptors and left every one of them inheritable across the exec.
    /// And `..` was the same dead end rather than the parent, so `openat(fd,
    /// "..")` on an open `/proc/<pid>/fd` --- how `fts(3)` climbs back out
    /// without re-resolving the path --- never reached `/proc/<pid>`.
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(ProcSelfFdDir {
                process: self.process.clone(),
            })),
            ".." => Ok(super::procfs::pid_dir_inode(self.process.id())),
            _ => {
                let fd = name.parse::<i32>().map_err(|_| FsError::EntryNotFound)?;
                let file = self
                    .process
                    .try_linux()
                    .ok_or(FsError::EntryNotFound)?
                    .get_file_like(fd.into())
                    .map_err(|_| FsError::EntryNotFound)?;
                Ok(Arc::new(FdLink::of(self.process.id(), fd.into(), &file)))
            }
        }
    }
    /// Each descriptor is listed as a link without being looked at, as
    /// `proc_readfd_common` lists it. Looking would take the open file's
    /// lock, and an open `/proc/self/fd` is one of its own entries: `getdents`
    /// holds that lock for the whole call, so `ls -l /proc/self/fd` hung on
    /// the directory's own descriptor.
    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        let name = self.get_entry(id)?;
        let meta = match (id, name.parse::<i32>()) {
            (2.., Ok(fd)) => fd_link_metadata(self.process.id(), fd.into()),
            _ => self.find(&name)?.metadata()?,
        };
        Ok((meta, name))
    }
    /// The listing opens with `.` and `..`, as every other directory of this
    /// tree does.
    ///
    /// It used to start straight at the first descriptor: `/proc/<pid>/fd` was
    /// the only directory left in `/proc` without the two, which is the same
    /// hole `/proc/net` had. `ls -a` showed neither, and a walk that counts on
    /// finding them --- `fts(3)`, which `find`, `du`, `cp -r`, `rm -r` and
    /// `rsync` are built on --- met a directory with a shape no other has.
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => return Ok(".".to_string()),
            1 => return Ok("..".to_string()),
            _ => {}
        }
        let files = self
            .process
            .try_linux()
            .ok_or(FsError::DeviceError)?
            .get_files()
            .map_err(|_| FsError::DeviceError)?;
        let mut keys: Vec<_> = files.keys().collect();
        keys.sort();
        match keys.get(id - 2) {
            Some(key) => {
                let fd: i32 = (**key).into();
                Ok(fd.to_string())
            }
            None => Err(FsError::EntryNotFound),
        }
    }
}

/// The name of a descriptor that has none of its own: a socket's
/// (`sock_d_dname`: `socket:[ino]`, by the `st_ino` that `fstat` reports),
/// and anything else is named by its type.
pub(crate) fn default_link_name(meta: crate::error::LxResult<Metadata>, kind: &str) -> String {
    match meta {
        Ok(m) if m.type_ == FileType::Socket => alloc::format!("socket:[{}]", m.inode),
        _ => alloc::format!("anon_inode:[{}]", kind),
    }
}

/// `/proc/<pid>/fd/N` as `lstat` and `readlink` see it: a link whose text
/// is the descriptor's name, 64 bytes long whatever that name is, with
/// `r-x` for reading and `-wx` for writing (`tid_fd_update_inode`), and no
/// bit at all for an `O_PATH` descriptor.
///
/// Every descriptor that was not a `File` used to be missing here: a
/// socket or an eventfd was in the listing, but its entry was `ENOENT`, so
/// `ls -l /proc/<pid>/fd` printed an error for it and `lsof` could not tell
/// what it was.
///
/// Followed, it leads to the open file ([`fd_target`]) and not to its text,
/// which names no file for a pipe or a socket and the wrong one once the
/// file is unlinked or renamed. Only `/proc/self/fd/N` did, spelled just
/// so, so `/dev/stdin` of a pipe, `/proc/<pid>/fd/N` and
/// `/proc/self/fd/N/name` of a directory led nowhere.
pub struct FdLink {
    name: String,
    mode: u16,
    inode: usize,
    /// Not held: a descriptor of the link (`O_PATH|O_NOFOLLOW`) keeps no
    /// pipe open.
    file: Weak<dyn FileLike>,
}

impl FdLink {
    /// The entry for `fd`, which is `file`, of process `pid`.
    pub fn of(pid: u64, fd: FileDesc, file: &Arc<dyn FileLike>) -> Self {
        let flags = file.flags();
        let read = if flags.readable() { 0o500 } else { 0 };
        let write = if flags.writable() { 0o300 } else { 0 };
        FdLink {
            name: file.link_name(),
            mode: read | write,
            inode: fd_link_metadata(pid, fd).inode,
            file: Arc::downgrade(file),
        }
    }
}

/// What `lstat` and a listing of `/proc/<pid>/fd` say of the entry for
/// `fd`, without looking at the descriptor. Every entry was inode 0, and
/// so `d_ino` 0, which a glibc older than 2.37 skips as a deleted entry.
/// The numbers start above those of `fdinfo` (`fdinfo_inode`), which can
/// reach 2^32 + 2^42.
fn fd_link_metadata(pid: u64, fd: FileDesc) -> Metadata {
    let fd: i32 = fd.into();
    let inode = (1 << 43) + (pid as usize) * (crate::process::NR_OPEN as usize) + fd as usize;
    super::procfs::link_metadata(inode, 64)
}

/// Where a descriptor's link leads (`nd_jump_link`): to the open file itself,
/// and its name, so a reopen is that file even once it is unlinked; or to the
/// object, when the descriptor is no file, which `sys_openat` names by the
/// object and not by a walk.
pub fn fd_target(file: Arc<dyn FileLike>) -> (Arc<dyn INode>, String) {
    match file.downcast_arc::<File>() {
        Ok(file) => (file.inode(), file.path().clone()),
        Err(object) => (Arc::new(FdObject(object)), String::new()),
    }
}

impl INode for FdLink {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let rest = self.name.as_bytes().get(offset..).unwrap_or(&[]);
        let len = rest.len().min(buf.len());
        buf[..len].copy_from_slice(&rest[..len]);
        Ok(len)
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
        Ok(Metadata {
            mode: self.mode,
            ..super::procfs::link_metadata(self.inode, 64)
        })
    }
    /// While the descriptor is open (`proc_fd_link`).
    fn jump(&self) -> Option<Result<(Arc<dyn INode>, String)>> {
        self.file.upgrade().map(|file| Ok(fd_target(file)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// What [`ProcessLink`] leads to.
#[derive(Clone, Copy)]
pub enum Held {
    WorkingDirectory,
    Root,
    Program,
}

/// `/proc/<pid>/cwd`, `root` and `exe`. The text is a path, but the link
/// leads (`proc_cwd_link`, `proc_root_link`, `proc_exe_link`) where the
/// process is, from its own root, and not where that text leads from
/// wherever it is followed: from `/proc` an absolute text started over at
/// the root of `/proc`, so `/proc/self/cwd/f` was `ENOENT`.
///
/// A walk through one depends on whose link it is, so the path cache does
/// not keep it.
pub struct ProcessLink {
    pid: u64,
    held: Held,
    text: Pseudo,
}

impl ProcessLink {
    pub fn new(pid: u64, held: Held, text: &str) -> Self {
        ProcessLink {
            pid,
            held,
            text: Pseudo::new(text, FileType::SymLink),
        }
    }

    fn lead(&self) -> Result<(Arc<dyn INode>, String)> {
        let proc = crate::process::find_task(self.pid as _).ok_or(FsError::EntryNotFound)?;
        let lp = proc.try_linux().ok_or(FsError::EntryNotFound)?;
        let root = lp.root_inode();
        let path = match self.held {
            Held::Root => return Ok((root.clone(), String::from("/"))),
            Held::WorkingDirectory => lp.current_working_directory(),
            // No program is in `/proc`: one recorded there would lead back
            // here, and a kernel task has none.
            Held::Program => match lp.execute_path() {
                exe if exe.is_empty() || exe.starts_with("/proc/") => {
                    return Err(FsError::EntryNotFound)
                }
                exe => exe,
            },
        };
        let mut at = Walked {
            root: Some(root.clone()),
            ..Walked::default()
        };
        let found = root.lookup_walked(&path, super::FOLLOW_MAX_DEPTH, Some(&mut at))?;
        Ok((found, at.path()))
    }
}

impl INode for ProcessLink {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.text.read_at(offset, buf)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        self.text.poll()
    }
    /// Size 0, as in Linux, and a number of its own: it was the `Pseudo`'s 0.
    fn metadata(&self) -> Result<Metadata> {
        use super::procfs::{pid_inode, PID_CWD_SLOT, PID_EXE_SLOT, PID_ROOT_SLOT};
        let slot = match self.held {
            Held::WorkingDirectory => PID_CWD_SLOT,
            Held::Root => PID_ROOT_SLOT,
            Held::Program => PID_EXE_SLOT,
        };
        Ok(super::procfs::link_metadata(pid_inode(self.pid, slot), 0))
    }
    fn jump(&self) -> Option<Result<(Arc<dyn INode>, String)>> {
        Some(self.lead())
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// Where `/proc/<pid>/fd/N` leads when the descriptor is no file: to the
/// object itself. `stat` through the link describes it as `fstat` does, and
/// an `O_PATH` open takes hold of it; any other open is `ENXIO`, since
/// neither a socket (`sock_no_open`) nor an anonymous inode can be opened
/// again.
pub struct FdObject(pub Arc<dyn FileLike>);

impl INode for FdObject {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Err(FsError::NotSupported)
    }
    fn metadata(&self) -> Result<Metadata> {
        self.0.metadata().map_err(|_| FsError::NotSupported)
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod fd_link_tests;

#[cfg(test)]
mod proc_self_fd_tests {
    //! `/proc/<pid>/fd` was the last directory of the tree without `.` and
    //! `..`: the listing started at the first descriptor, and `find` answered
    //! both names with a `Pseudo` holding the path as text, which is a
    //! directory that contains nothing and is not the parent.

    use super::*;
    use crate::fs::file::File;
    use crate::fs::pseudo::Pseudo;
    use crate::process::LinuxProcess;
    use rcore_fs::vfs::{FileType, INode};
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::Job;

    fn a_process_with(files: usize) -> Arc<Process> {
        let proc = Process::create_with_fixed_id_ext(
            &Job::root(),
            7701,
            "fdtest",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        for i in 0..files {
            let inode: Arc<dyn INode> = Arc::new(Pseudo::new("x", FileType::File));
            let file = File::new(
                inode,
                crate::fs::OpenFlags::RDONLY,
                alloc::format!("/tmp/{}", i),
            );
            proc.linux().add_file(file).unwrap();
        }
        proc
    }

    #[test]
    fn the_listing_opens_with_dot_and_dotdot() {
        let dir = ProcSelfFdDir {
            process: a_process_with(0),
        };
        assert_eq!(dir.get_entry(0).unwrap(), ".");
        assert_eq!(dir.get_entry(1).unwrap(), "..");
    }

    #[test]
    fn the_descriptors_come_after_the_two_dots_and_none_is_lost() {
        // The shift is the part that is easy to get wrong: with the dots
        // written in but the index left alone, the first descriptor would be
        // listed twice and the last one not at all.
        let proc = a_process_with(3);
        let dir = ProcSelfFdDir {
            process: proc.clone(),
        };
        let mut listed = alloc::vec::Vec::new();
        let mut id = 0;
        while let Ok(name) = dir.get_entry(id) {
            listed.push(name);
            id += 1;
        }
        let expected: alloc::vec::Vec<alloc::string::String> = {
            let mut fds: alloc::vec::Vec<i32> = proc
                .linux()
                .get_files()
                .unwrap()
                .keys()
                .map(|fd| (*fd).into())
                .collect();
            fds.sort();
            core::iter::once(".".to_string())
                .chain(core::iter::once("..".to_string()))
                .chain(fds.iter().map(|fd| fd.to_string()))
                .collect()
        };
        assert_eq!(listed, expected);
        // A fresh process already carries stdin, stdout and stderr, so the
        // listing is the whole table plus the two dots and nothing else.
        assert_eq!(
            listed.len(),
            proc.linux().get_files().unwrap().len() + 2,
            "every descriptor, once, after the two dots"
        );
    }

    #[test]
    fn dot_is_this_directory_again_and_not_an_empty_one() {
        let dir = ProcSelfFdDir {
            process: a_process_with(2),
        };
        let dot = dir.find(".").unwrap();
        // A `Pseudo` answers `NotSupported` to both of these, so the listing
        // of `/proc/self/fd/.` was empty and its inode was 0.
        assert_eq!(dot.get_entry(0).unwrap(), ".");
        assert_eq!(dot.get_entry(2).unwrap(), dir.get_entry(2).unwrap());
        assert_eq!(dot.metadata().unwrap().inode, dir.metadata().unwrap().inode);
    }

    #[test]
    fn dotdot_is_the_process_directory_and_not_this_one() {
        let proc = a_process_with(0);
        let dir = ProcSelfFdDir {
            process: proc.clone(),
        };
        let up = dir.find("..").unwrap();
        assert_eq!(
            up.metadata().unwrap().inode,
            super::super::procfs::pid_inode(proc.id(), 0),
            "`..` of /proc/<pid>/fd is /proc/<pid>"
        );
        assert_ne!(up.metadata().unwrap().inode, dir.metadata().unwrap().inode);
        assert_eq!(up.metadata().unwrap().type_, FileType::Dir);
    }
}
