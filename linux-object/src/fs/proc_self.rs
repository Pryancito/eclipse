use crate::fs::pseudo::Pseudo;
use crate::process::ProcessExt;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use rcore_fs::vfs::*;
use zircon_object::object::KernelObject;
use zircon_object::task::Process;

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
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: super::procfs::pid_inode(self.process.id(), super::procfs::PID_FD_DIR_SLOT),
            size: 0,
            blk_size: 0,
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
                    .get_file(fd.into())
                    .map_err(|_| FsError::EntryNotFound)?;
                Ok(Arc::new(Pseudo::new(file.path(), FileType::SymLink)))
            }
        }
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
