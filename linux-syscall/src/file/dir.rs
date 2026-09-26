//! Directory operations
//!
//! - getcwd
//! - chdir
//! - mkdir(at)
//! - rmdir(at)
//! - getdents64
//! - link(at)
//! - unlink(at)
//! - rename(at)
//! - readlink(at)

use super::*;
use alloc::string::String;
use bitflags::bitflags;
use kernel_hal::user::UserOutPtr;
use linux_object::error::LxResult;
use linux_object::fs::vfs::{FileType, Metadata};
use linux_object::fs::File;

impl Syscall<'_> {
    /// return a null-terminated string containing an absolute pathname
    /// that is the current working directory of the calling process.
    /// - `buf` – pointer to buffer to receive path
    /// - `len` – size of buf
    pub fn sys_getcwd(&self, mut buf: UserOutPtr<u8>, len: usize) -> SysResult {
        info!("getcwd: buf={:?}, len={:#x}", buf, len);
        let proc = self.linux_process();
        let cwd = proc.current_working_directory();
        if cwd.len() + 1 > len {
            return Err(LxError::ERANGE);
        }
        buf.write_cstring(&cwd)?;
        Ok(buf.as_addr())
    }

    /// Change the current directory.
    /// - `path` – pointer to string with name of path
    pub fn sys_chdir(&self, path: UserInPtr<u8>) -> SysResult {
        let path = path.as_c_str()?;
        info!("chdir: path={:?}", path);

        let proc = self.linux_process();
        let inode = proc.lookup_inode(path)?;
        let info = inode.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(LxError::ENOTDIR);
        }
        proc.check_access(&info, 0o1, true)?;
        proc.change_directory(path);
        Ok(0)
    }

    /// Make a directory.
    /// - path – pointer to string with directory name
    /// - mode – file system permissions mode
    pub fn sys_mkdir(&self, path: UserInPtr<u8>, mode: usize) -> SysResult {
        self.sys_mkdirat(FileDesc::CWD, path, mode)
    }

    /// create directory relative to directory file descriptor
    pub fn sys_mkdirat(&self, dirfd: FileDesc, path: UserInPtr<u8>, mode: usize) -> SysResult {
        let path = path.as_c_str()?;
        // TODO: check pathname
        info!(
            "mkdirat: dirfd={:?}, path={:?}, mode={:#o}",
            dirfd, path, mode
        );

        let (dir_path, file_name) = split_path(path);
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let dir_metadata = inode.metadata()?;
        // Existence BEFORE write permission, as Linux does (`filename_create`
        // answers EEXIST from the lookup, before `may_create` ever looks at the
        // parent's mode). The reverse order made an unprivileged `mkdir` of a
        // directory that already exists under a root-owned parent fail with
        // EACCES instead of EEXIST — and PulseAudio's `pa_make_secure_dir`
        // (`mkdir(); if (errno != EEXIST) fail`) therefore aborted the
        // system-mode daemon after it dropped to user `pulse`: `Failed to
        // create secure directory (/var/lib/pulse): Permission denied`, in a
        // respawn loop, on real hardware.
        if inode.find(file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        proc.check_access(&dir_metadata, 0o3, true)?;
        let create_mode = proc.apply_umask(mode as u16);
        let created = inode.create(file_name, FileType::Dir, create_mode as u32)?;
        proc.initialize_created_metadata(&created, Some(&dir_metadata), create_mode, true)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// create a special or ordinary file (see mknod(2)).
    pub fn sys_mknod(&self, path: UserInPtr<u8>, mode: usize, dev: usize) -> SysResult {
        self.sys_mknodat(FileDesc::CWD, path, mode, dev)
    }

    /// create a special or ordinary file relative to a directory fd.
    pub fn sys_mknodat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        mode: usize,
        dev: usize,
    ) -> SysResult {
        let path = path.as_c_str()?;
        info!(
            "mknodat: dirfd={:?}, path={:?}, mode={:#o}, dev={:#x}",
            dirfd, path, mode, dev
        );
        const S_IFMT: usize = 0o170000;
        let file_type = match mode & S_IFMT {
            0o010000 => FileType::NamedPipe,   // S_IFIFO
            0o020000 => FileType::CharDevice,  // S_IFCHR
            0o060000 => FileType::BlockDevice, // S_IFBLK
            0o140000 => FileType::Socket,      // S_IFSOCK
            0o100000 | 0 => FileType::File,    // S_IFREG / unspecified => regular file
            _ => return Err(LxError::EINVAL),  // directories must use mkdir
        };
        let (dir_path, file_name) = split_path(path);
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let dir_metadata = inode.metadata()?;
        proc.check_access(&dir_metadata, 0o3, true)?;
        if inode.find(file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        let create_mode = proc.apply_umask((mode & 0o7777) as u16);
        let rdev = if matches!(file_type, FileType::CharDevice | FileType::BlockDevice) {
            dev
        } else {
            0
        };
        let created = inode.create2(file_name, file_type, create_mode as u32, rdev)?;
        proc.initialize_created_metadata(&created, Some(&dir_metadata), create_mode, false)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// Remove a directory.
    /// - path – pointer to string with directory name
    ///
    /// `rmdir(2)` only exists on x86_64; everywhere else libc reaches this
    /// through `unlinkat(AT_FDCWD, path, AT_REMOVEDIR)`, so the two must not be
    /// separate implementations that can drift apart.
    pub fn sys_rmdir(&self, path: UserInPtr<u8>) -> SysResult {
        self.sys_unlinkat(FileDesc::CWD, path, AT_REMOVEDIR)
    }

    /// get directory entries
    /// TODO: get ino from dirent
    /// - fd – file describe
    pub fn sys_getdents64(
        &self,
        fd: FileDesc,
        mut buf: UserOutPtr<u8>,
        buf_size: usize,
    ) -> SysResult {
        info!(
            "getdents64: fd={:?}, ptr={:?}, buf_size={}",
            fd, buf, buf_size
        );
        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        let info = file.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(LxError::ENOTDIR);
        }
        let cap_size = buf_size.min(256 * 1024);
        let mut kbuf = vec![0; cap_size];
        let mut writer = DirentBufWriter::new(&mut kbuf);
        let mut file = file;
        collect_dirents(&mut file, |next, meta, name| {
            writer.try_write(
                meta.inode as u64,
                next,
                DirentType::from(meta.type_).bits(),
                name,
            )
        })?;
        buf.write_array(writer.as_slice())?;
        Ok(writer.written_size)
    }

    /// creates a new link (also known as a hard link) to an existing file.
    pub fn sys_link(&self, oldpath: UserInPtr<u8>, newpath: UserInPtr<u8>) -> SysResult {
        self.sys_linkat(FileDesc::CWD, oldpath, FileDesc::CWD, newpath, 0)
    }

    /// create file link relative to directory file descriptors
    /// If the pathname given in oldpath is relative,
    /// then it is interpreted relative to the directory referred to by the file descriptor olddirfd
    pub fn sys_linkat(
        &self,
        olddirfd: FileDesc,
        oldpath: UserInPtr<u8>,
        newdirfd: FileDesc,
        newpath: UserInPtr<u8>,
        flags: usize,
    ) -> SysResult {
        // Flags first, as Linux does: a bad flag word is EINVAL whatever the
        // paths are.
        let flags = at_flags(flags, LINKAT_FLAGS)?;
        let oldpath = oldpath.as_c_str()?;
        let newpath = newpath.as_c_str()?;
        info!(
            "linkat: olddirfd={:?}, oldpath={:?}, newdirfd={:?}, newpath={:?}, flags={:?}",
            olddirfd, oldpath, newdirfd, newpath, flags
        );

        let proc = self.linux_process();
        let (new_dir_path, new_file_name) = split_path(newpath);
        let follow = flags.contains(AtFlags::SYMLINK_FOLLOW);
        let inode = proc.lookup_inode_at(olddirfd, oldpath, follow)?;
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, true)?;
        let new_dir_metadata = new_dir_inode.metadata()?;
        proc.check_access(&new_dir_metadata, 0o3, true)?;
        new_dir_inode.link(new_file_name, &inode)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// delete name/possibly file it refers to
    /// If that name was the last link to a file and no processes have the file open, the file is deleted.
    /// If the name was the last link to a file but any processes still have the file open,
    /// the file will remain in existence until the last file descriptor referring to it is closed.
    pub fn sys_unlink(&self, path: UserInPtr<u8>) -> SysResult {
        self.sys_unlinkat(FileDesc::CWD, path, 0)
    }

    /// remove directory entry relative to directory file descriptor
    /// The unlinkat() system call operates in exactly the same way as either unlink or rmdir.
    pub fn sys_unlinkat(&self, dirfd: FileDesc, path: UserInPtr<u8>, flags: usize) -> SysResult {
        let path = path.as_c_str()?;
        // hard code special path
        let path = if path == "/dev/shm/testshm" {
            "/testshm"
        } else {
            path
        };
        let remove_dir = unlinkat_removes_a_directory(flags)?;
        info!(
            "unlinkat: dirfd={:?}, path={:?}, remove_dir={}",
            dirfd, path, remove_dir
        );

        let proc = self.linux_process();
        let (dir_path, file_name) = split_path(path);
        let dir_inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let dir_metadata = dir_inode.metadata()?;
        proc.check_access(&dir_metadata, 0o3, true)?;
        let file_inode = dir_inode.find(file_name)?;
        let file_metadata = file_inode.metadata()?;
        unlinkat_type_check(remove_dir, file_metadata.type_ == FileType::Dir)?;
        proc.check_sticky(&dir_metadata, &file_metadata)?;
        dir_inode.unlink(file_name)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// change name/location of file
    pub fn sys_rename(&self, oldpath: UserInPtr<u8>, newpath: UserInPtr<u8>) -> SysResult {
        self.sys_renameat(FileDesc::CWD, oldpath, FileDesc::CWD, newpath)
    }

    /// rename file relative to directory file descriptors
    pub fn sys_renameat(
        &self,
        olddirfd: FileDesc,
        oldpath: UserInPtr<u8>,
        newdirfd: FileDesc,
        newpath: UserInPtr<u8>,
    ) -> SysResult {
        self.sys_renameat2(olddirfd, oldpath, newdirfd, newpath, 0)
    }

    /// rename with a `flags` argument (see renameat2(2)).
    ///
    /// `RENAME_NOREPLACE` is honoured: the rename fails with `EEXIST` instead
    /// of clobbering an existing target — what coreutils `mv -n` and atomic
    /// create-then-publish patterns rely on. `RENAME_EXCHANGE` / `WHITEOUT`
    /// answer `EINVAL`, the documented reply of a filesystem without support,
    /// so callers take their fallback path.
    pub fn sys_renameat2(
        &self,
        olddirfd: FileDesc,
        oldpath: UserInPtr<u8>,
        newdirfd: FileDesc,
        newpath: UserInPtr<u8>,
        flags: usize,
    ) -> SysResult {
        let oldpath = oldpath.as_c_str()?;
        let newpath = newpath.as_c_str()?;
        info!(
            "renameat2: olddirfd={:?}, oldpath={:?}, newdirfd={:?}, newpath={:?}, flags={:#x}",
            olddirfd, oldpath, newdirfd, newpath, flags
        );
        check_rename_flags(flags)?;

        let proc = self.linux_process();
        let (old_dir_path, old_file_name) = split_path(oldpath);
        let (new_dir_path, new_file_name) = split_path(newpath);
        let old_dir_inode = proc.lookup_inode_at(olddirfd, old_dir_path, false)?;
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, false)?;
        let old_dir_metadata = old_dir_inode.metadata()?;
        let new_dir_metadata = new_dir_inode.metadata()?;
        proc.check_access(&old_dir_metadata, 0o3, true)?;
        proc.check_access(&new_dir_metadata, 0o3, true)?;
        let old_inode = old_dir_inode.find(old_file_name)?;
        let old_metadata = old_inode.metadata()?;
        proc.check_sticky(&old_dir_metadata, &old_metadata)?;
        if flags & RENAME_NOREPLACE != 0 && new_dir_inode.find(new_file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        old_dir_inode.move_(old_file_name, &new_dir_inode, new_file_name)?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// read value of symbolic link
    pub fn sys_readlink(&self, path: UserInPtr<u8>, base: UserOutPtr<u8>, len: usize) -> SysResult {
        self.sys_readlinkat(FileDesc::CWD, path, base, len)
    }

    /// read value of symbolic link relative to directory file descriptor
    /// readlink() places the contents of the symbolic link path in the buffer base, which has size len
    /// TODO: recursive link resolution and loop detection
    pub fn sys_readlinkat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        mut base: UserOutPtr<u8>,
        len: usize,
    ) -> SysResult {
        let path = path.as_c_str()?;
        info!(
            "readlinkat: dirfd={:?}, path={:?}, base={:?}, len={}",
            dirfd, path, base, len
        );

        let proc = self.linux_process();
        // readlink(2) must follow symlinks in *intermediate* path components but
        // NOT the final one.
        //
        // Two resolution strategies are needed:
        //  1. Whole-path, no-follow: this is what reaches the magic links that
        //     `lookup_inode_at` special-cases on the FULL path string
        //     (`/proc/self/exe`, `/proc/self/fd/N`, ...). Firefox reads
        //     `/proc/self/exe` to find its install dir, so this MUST work.
        //  2. Parent-follow + final-no-follow: needed for a link whose parent
        //     dir is itself reached through a symlink — e.g. libdrm's
        //     readlink("/sys/dev/char/226:0/device/subsystem") where `device`
        //     is a symlink; strategy 1 stops at it (no-follow) and fails.
        // Try (1) first and accept it only when it yields a symlink; otherwise
        // fall back to (2). (1) never matched the magic links before because
        // this function split the path and bypassed the full-path special case.
        let inode = match proc.lookup_inode_at(dirfd, path, false) {
            Ok(i)
                if i.metadata()
                    .map(|m| m.type_ == FileType::SymLink)
                    .unwrap_or(false) =>
            {
                i
            }
            _ => {
                let (dir_path, file_name) = split_path(path);
                if file_name.is_empty() || file_name == "." || file_name == ".." {
                    proc.lookup_inode_at(dirfd, path, false)?
                } else {
                    let dir = proc.lookup_inode_at(dirfd, dir_path, true)?;
                    dir.find(file_name).map_err(LxError::from)?
                }
            }
        };
        if inode.metadata()?.type_ != FileType::SymLink {
            return Err(LxError::EINVAL);
        }
        // TODO: recursive link resolution and loop detection
        let cap_len = len.min(4096);
        let mut buf = vec![0; cap_len];
        let len = inode.read_at(0, &mut buf)?;
        base.write_array(&buf[..len])?;
        Ok(len)
    }

    /// Change the current directory to the one specified by the file descriptor.
    pub fn sys_fchdir(&self, fd: FileDesc) -> SysResult {
        info!("fchdir: fd={:?}", fd);

        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        let info = file.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(LxError::ENOTDIR);
        }
        proc.check_access(&info, 0o1, true)?;
        proc.change_directory(file.path());
        Ok(0)
    }

    /// create a symbolic link relative to directory file descriptor
    pub fn sys_symlinkat(
        &self,
        target: UserInPtr<u8>,
        newdirfd: FileDesc,
        linkpath: UserInPtr<u8>,
    ) -> SysResult {
        let target = target.as_c_str()?;
        let linkpath = linkpath.as_c_str()?;
        info!(
            "symlinkat: target={:?}, newdirfd={:?}, linkpath={:?}",
            target, newdirfd, linkpath
        );

        let (dir_path, file_name) = split_path(linkpath);
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(newdirfd, dir_path, true)?;
        let dir_metadata = inode.metadata()?;
        proc.check_access(&dir_metadata, 0o3, true)?;
        if inode.find(file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        let mode = proc.apply_umask(0o777);
        let symlink_inode = inode.create(file_name, FileType::SymLink, mode as u32)?;
        proc.initialize_created_metadata(&symlink_inode, Some(&dir_metadata), mode, false)?;
        symlink_inode.write_at(0, target.as_bytes())?;
        linux_object::fs::dcache_invalidate();
        Ok(0)
    }

    /// create a symbolic link
    pub fn sys_symlink(&self, target: UserInPtr<u8>, linkpath: UserInPtr<u8>) -> SysResult {
        self.sys_symlinkat(target, FileDesc::CWD, linkpath)
    }
}

/// A directory position that can be read one entry at a time and, when the
/// entry just read turns out not to fit, told to hand it back.
///
/// `getdents64` and FreeBSD's `getdirentries` both read an entry *before*
/// they know whether its record fits in what is left of the caller's buffer.
/// Both used to drop the one that did not: consumed from the directory
/// position, written nowhere, absent from every later call. A listing that
/// took more than one buffer lost one name per buffer, and glibc's `readdir`
/// asks in 32 KiB pieces, so any directory past a few hundred entries came
/// out short in `ls`, `find`, `rm -r` and everything built on them.
pub(crate) trait DirEntries {
    /// The next entry, or `None` at the end of the directory.
    fn next_entry(&mut self) -> LxResult<Option<(Metadata, String)>>;
    /// Hand back the entry `next_entry` just returned.
    fn unread_entry(&mut self);
    /// The position an `lseek` takes to resume right after the entry
    /// `next_entry` just returned: the entry's `d_off`.
    fn position(&self) -> u64;
}

impl DirEntries for Arc<File> {
    fn next_entry(&mut self) -> LxResult<Option<(Metadata, String)>> {
        match self.read_entry_with_metadata() {
            Ok(entry) => Ok(Some(entry)),
            Err(LxError::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn unread_entry(&mut self) {
        File::unread_entry(self);
    }

    fn position(&self) -> u64 {
        self.dir_position()
    }
}

/// Feed directory entries to `push` until it refuses one or the directory
/// ends. `push` gets the position that follows the entry (its `d_off`: what
/// `lseek` takes to resume after it, and what glibc's `telldir` reports),
/// and returns whether the entry fitted; the one it refuses is handed back
/// to `src`, so the next call starts with it.
///
/// Returns how many entries were pushed. Refusing the very first one is
/// `EINVAL`, as Linux answers a buffer too small to hold a single record
/// (`filldir64`: `if (reclen > buf->count) return -EINVAL`): the caller
/// would otherwise take an empty answer for the end of the directory.
pub(crate) fn collect_dirents(
    src: &mut impl DirEntries,
    mut push: impl FnMut(u64, &Metadata, &str) -> bool,
) -> LxResult<usize> {
    let mut pushed = 0;
    while let Some((meta, name)) = src.next_entry()? {
        if !push(src.position(), &meta, &name) {
            src.unread_entry();
            if pushed == 0 {
                return Err(LxError::EINVAL);
            }
            break;
        }
        pushed += 1;
    }
    Ok(pushed)
}

#[cfg(test)]
mod dirent_collection_tests {
    use super::*;
    use alloc::vec::Vec;
    use core::convert::TryInto;
    use linux_object::fs::vfs::Timespec;

    /// A directory that hands out the names it was given, once each, and
    /// remembers its position like a `File` does.
    struct Names {
        names: Vec<&'static str>,
        pos: usize,
    }

    fn entry(ino: usize) -> Metadata {
        Metadata {
            dev: 0,
            inode: ino,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        }
    }

    impl DirEntries for Names {
        fn next_entry(&mut self) -> LxResult<Option<(Metadata, String)>> {
            let name = match self.names.get(self.pos) {
                Some(n) => *n,
                None => return Ok(None),
            };
            self.pos += 1;
            Ok(Some((entry(self.pos), String::from(name))))
        }

        fn unread_entry(&mut self) {
            self.pos -= 1;
        }

        fn position(&self) -> u64 {
            self.pos as u64
        }
    }

    /// Collect with room for `room` entries per call.
    fn call(src: &mut Names, room: usize) -> LxResult<Vec<String>> {
        let mut got = Vec::new();
        collect_dirents(src, |_, _, name| {
            if got.len() == room {
                return false;
            }
            got.push(String::from(name));
            true
        })?;
        Ok(got)
    }

    #[test]
    fn an_entry_that_does_not_fit_comes_out_on_the_next_call() {
        let mut dir = Names {
            names: vec!["a", "b", "c", "d", "e"],
            pos: 0,
        };
        // Two per call: the third entry is read, refused, and must not be lost.
        assert_eq!(call(&mut dir, 2).unwrap(), ["a", "b"]);
        assert_eq!(call(&mut dir, 2).unwrap(), ["c", "d"]);
        assert_eq!(call(&mut dir, 2).unwrap(), ["e"]);
        // The end of the directory is an empty answer, not an error.
        assert_eq!(call(&mut dir, 2).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn the_count_is_what_was_pushed() {
        let mut dir = Names {
            names: vec!["a", "b", "c"],
            pos: 0,
        };
        let mut seen = 0;
        let n = collect_dirents(&mut dir, |_, _, _| {
            seen += 1;
            seen <= 2
        })
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(dir.pos, 2, "the refused entry is back in the directory");
    }

    /// `d_off` is where `lseek` resumes after the entry, so it is the
    /// position *after* it, and it climbs with the listing. It was 0 for
    /// every entry, which made `seekdir(telldir())` a `rewinddir`.
    #[test]
    fn each_entry_carries_the_position_that_follows_it() {
        let mut dir = Names {
            names: vec!["a", "b", "c"],
            pos: 0,
        };
        let mut offs = Vec::new();
        collect_dirents(&mut dir, |next, _, _| {
            offs.push(next);
            true
        })
        .unwrap();
        assert_eq!(offs, [1, 2, 3]);
        // Read back from where the second entry said to resume: the third.
        dir.pos = offs[1] as usize;
        assert_eq!(call(&mut dir, 8).unwrap(), ["c"]);
    }

    #[test]
    fn the_writer_puts_the_offset_it_was_given_in_d_off() {
        let mut buf = [0u8; 64];
        let mut w = DirentBufWriter::new(&mut buf);
        assert!(w.try_write(7, 0x1234, DirentType::REG.bits(), "hi"));
        let b = w.as_slice();
        assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), 7);
        // `d_off` is the second field of `linux_dirent64`.
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 0x1234);
    }

    #[test]
    fn a_buffer_too_small_for_one_entry_is_einval_and_keeps_the_entry() {
        let mut dir = Names {
            names: vec!["a", "b"],
            pos: 0,
        };
        assert_eq!(call(&mut dir, 0), Err(LxError::EINVAL));
        // Nothing was consumed: a bigger buffer starts where it should.
        assert_eq!(call(&mut dir, 8).unwrap(), ["a", "b"]);
    }

    #[test]
    fn an_error_from_the_directory_comes_through() {
        struct Broken;
        impl DirEntries for Broken {
            fn next_entry(&mut self) -> LxResult<Option<(Metadata, String)>> {
                Err(LxError::EIO)
            }
            fn unread_entry(&mut self) {}
            fn position(&self) -> u64 {
                0
            }
        }
        assert_eq!(
            collect_dirents(&mut Broken, |_, _, _| true),
            Err(LxError::EIO)
        );
    }
}

#[allow(dead_code)]
#[repr(Rust, packed)] // Don't use 'C'. Or its size will align up to 8 bytes.
pub struct LinuxDirent64 {
    /// Inode number
    ino: u64,
    /// Offset to next structure
    offset: u64,
    /// Size of this dirent
    reclen: u16,
    /// File type
    type_: u8,
    /// Filename (null-terminated)
    name: [u8; 0],
}

/// directory entry buffer writer
struct DirentBufWriter<'a> {
    buf: &'a mut [u8],
    rest_size: usize,
    written_size: usize,
}

impl<'a> DirentBufWriter<'a> {
    /// create a buffer writer
    fn new(buf: &'a mut [u8]) -> Self {
        DirentBufWriter {
            rest_size: buf.len(),
            written_size: 0,
            buf,
        }
    }

    /// write data; `offset` is the entry's `d_off`, the directory position
    /// that follows it.
    fn try_write(&mut self, inode: u64, offset: u64, type_: u8, name: &str) -> bool {
        let len = core::mem::size_of::<LinuxDirent64>() + name.len() + 1;
        let len = len.div_ceil(8) * 8; // align up
        if self.rest_size < len {
            return false;
        }
        let dent = LinuxDirent64 {
            ino: inode,
            offset,
            reclen: len as u16,
            type_,
            name: [],
        };
        #[allow(unsafe_code)]
        unsafe {
            let ptr = self.buf.as_ptr().add(self.written_size) as *mut LinuxDirent64;
            ptr.write(dent);
            let name_ptr = ptr.add(1) as *mut u8;
            name_ptr.copy_from_nonoverlapping(name.as_ptr(), name.len());
            name_ptr.add(name.len()).write(0);
        }
        self.rest_size -= len;
        self.written_size += len;
        true
    }

    /// to slice
    fn as_slice(&self) -> &[u8] {
        &self.buf[..self.written_size]
    }
}

bitflags! {
    pub struct DirentType: u8 {
        const UNKNOWN  = 0;
        /// FIFO (named pipe)
        const FIFO = 1;
        /// Character device
        const CHR  = 2;
        /// Directory
        const DIR  = 4;
        /// Block device
        const BLK = 6;
        /// Regular file
        const REG = 8;
        /// Symbolic link
        const LNK = 10;
        /// UNIX domain socket
        const SOCK  = 12;
        /// ???
        const WHT = 14;
    }
}

impl From<FileType> for DirentType {
    fn from(type_: FileType) -> Self {
        match type_ {
            FileType::File => Self::REG,
            FileType::Dir => Self::DIR,
            FileType::SymLink => Self::LNK,
            FileType::CharDevice => Self::CHR,
            FileType::BlockDevice => Self::BLK,
            FileType::Socket => Self::SOCK,
            FileType::NamedPipe => Self::FIFO,
        }
    }
}

bitflags! {
    pub struct AtFlags: usize {
        const EMPTY_PATH = 0x1000;
        const SYMLINK_NOFOLLOW = 0x100;
        const EACCESS = 0x200;
        const SYMLINK_FOLLOW = 0x400;
    }
}

/// `AT_NO_AUTOMOUNT`: do not trigger an automount traversing the path.
///
/// There is nothing to automount here, but coreutils' `stat` and `ls` put it
/// on every `statx` they make, so a kernel that refuses it refuses them.
pub(crate) const AT_NO_AUTOMOUNT: usize = 0x800;

/// `AT_STATX_SYNC_TYPE`: the two bits `statx(2)` uses to ask for a forced
/// (`AT_STATX_FORCE_SYNC`, 0x2000) or a skipped (`AT_STATX_DONT_SYNC`,
/// 0x4000) sync. Nothing here syncs to answer a stat, which is what a local
/// filesystem does, so they are accepted and ignored -- but they are accepted.
pub(crate) const AT_STATX_SYNC_TYPE: usize = 0x6000;

/// The `AT_*` bits a syscall accepts, checked against the **raw** flag word.
///
/// Every `AT_*` caller in this tree reached for `AtFlags::from_bits_truncate`,
/// which drops in silence every bit it does not know: the kernel nodding at
/// something userspace said and it did not understand. Two ways that bites.
///
/// The bit that means something to another syscall gets through. `AtFlags`
/// holds `AT_EACCESS` (0x200), which is `faccessat`'s, so
/// `fstatat(fd, path, buf, AT_EACCESS)` was a perfectly good stat here and is
/// `EINVAL` on Linux -- the same collision that already cost `unlinkat` its
/// `AT_REMOVEDIR` (see [`AT_REMOVEDIR`]).
///
/// And the bit `AtFlags` cannot even name is dropped *correctly* by accident,
/// so the allowed set has to be raw bits: `AT_NO_AUTOMOUNT` and the `statx`
/// sync bits are ones Linux accepts and this kernel has nothing to do about,
/// and refusing them would break the callers that send them.
pub(crate) fn at_flags(flags: usize, allowed: usize) -> Result<AtFlags, LxError> {
    if flags & !allowed != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(AtFlags::from_bits_truncate(flags))
}

/// What `fstatat(2)` accepts (`vfs_fstatat`, `fs/stat.c`).
pub(crate) const FSTATAT_FLAGS: usize =
    AtFlags::SYMLINK_NOFOLLOW.bits() | AT_NO_AUTOMOUNT | AtFlags::EMPTY_PATH.bits();

/// What `statx(2)` accepts: `fstatat`'s set plus the sync-type bits.
pub(crate) const STATX_FLAGS: usize = FSTATAT_FLAGS | AT_STATX_SYNC_TYPE;

/// What `linkat(2)` accepts (`do_linkat`, `fs/namei.c`).
pub(crate) const LINKAT_FLAGS: usize = AtFlags::SYMLINK_FOLLOW.bits() | AtFlags::EMPTY_PATH.bits();

/// `unlinkat(2)`'s `AT_REMOVEDIR`, which turns it into `rmdir`.
///
/// It is not in `AtFlags` and must not be: Linux gives it the same value as
/// `AT_EACCESS` (0x200), because the two belong to different syscalls. Folded
/// into one bitflags type they become the same flag, and `unlinkat` reads a
/// request to remove a directory as `faccessat`'s "use the effective uid".
pub(crate) const AT_REMOVEDIR: usize = 0x200;

/// Which of the two syscalls hiding behind `unlinkat(2)` the caller asked for.
///
/// ```c
/// if ((flag & ~AT_REMOVEDIR) != 0) return -EINVAL;
/// if (flag & AT_REMOVEDIR) return do_rmdir(dfd, ...);
/// return do_unlinkat(dfd, ...);
/// ```
///
/// This flag used to be parsed into `AtFlags` and then ignored, so every
/// `unlinkat(fd, path, AT_REMOVEDIR)` hit the `unlink` half and answered
/// EISDIR. `rmdir(2)` is only wired up on x86_64, so on aarch64 and riscv64 —
/// where libc has nothing else to call — no directory could be removed at all.
fn unlinkat_removes_a_directory(flags: usize) -> linux_object::error::LxResult<bool> {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(flags & AT_REMOVEDIR != 0)
}

/// The two halves of `unlinkat(2)` disagree about directories on purpose:
/// `unlink` refuses one, `rmdir` demands one.
///
/// Split from the syscall because the lookup around it needs a live process
/// and this does not, and because getting it backwards is silent: it deletes
/// the wrong kind of thing, or refuses the right one.
fn unlinkat_type_check(remove_dir: bool, is_dir: bool) -> linux_object::error::LxResult<()> {
    match (remove_dir, is_dir) {
        // `rmdir` on something that is not a directory.
        (true, false) => Err(LxError::ENOTDIR),
        // `unlink` on a directory.
        (false, true) => Err(LxError::EISDIR),
        _ => Ok(()),
    }
}

/// renameat2(2) `RENAME_NOREPLACE`: don't overwrite an existing target.
const RENAME_NOREPLACE: usize = 1 << 0;
/// renameat2(2) `RENAME_EXCHANGE`: atomically swap source and target.
const RENAME_EXCHANGE: usize = 1 << 1;
/// renameat2(2) `RENAME_WHITEOUT`: leave a whiteout behind (overlayfs).
const RENAME_WHITEOUT: usize = 1 << 2;

/// Validate a renameat2 `flags` argument (renameat2(2)). Pure, so the flag
/// matrix is unit-testable: unknown bits and the documented mutually-exclusive
/// combinations are `EINVAL`; `EXCHANGE`/`WHITEOUT` also answer `EINVAL`
/// because no filesystem here implements them — the reply Linux gives on such
/// filesystems, which callers already handle with a fallback.
fn check_rename_flags(flags: usize) -> SysResult {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & RENAME_EXCHANGE != 0 && flags & (RENAME_NOREPLACE | RENAME_WHITEOUT) != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & (RENAME_EXCHANGE | RENAME_WHITEOUT) != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(0)
}

#[cfg(test)]
mod rename_flag_tests {
    use super::*;

    #[test]
    fn plain_and_noreplace_are_accepted() {
        assert_eq!(check_rename_flags(0), Ok(0));
        assert_eq!(check_rename_flags(RENAME_NOREPLACE), Ok(0));
    }

    #[test]
    fn unsupported_and_invalid_combinations_are_einval() {
        for flags in [
            RENAME_EXCHANGE,
            RENAME_WHITEOUT,
            RENAME_EXCHANGE | RENAME_NOREPLACE,
            RENAME_EXCHANGE | RENAME_WHITEOUT,
            1 << 3,
            usize::MAX,
        ] {
            assert_eq!(
                check_rename_flags(flags),
                Err(LxError::EINVAL),
                "{flags:#x}"
            );
        }
    }
}

/// `unlinkat(2)` is two syscalls behind one number, and the flag that chooses
/// between them was parsed into `AtFlags` and then never read.
///
/// Every `unlinkat(fd, path, AT_REMOVEDIR)` therefore took the `unlink` half
/// and answered EISDIR. On x86_64 that stayed hidden because libc has
/// `rmdir(2)` to call instead; on aarch64 and riscv64 there is no such syscall,
/// so no directory could be removed at all — not by `rm -r`, not by a package
/// manager cleaning up, not by anything.
///
/// What these tests do **not** cover: the flag word `sys_rmdir` hands to
/// `sys_unlinkat`. Changing it to 0 leaves every test here green, because
/// calling either syscall needs a live process and a filesystem and neither
/// can be built from the host. The contract below is pinned; the one call site
/// that depends on it is not, so read it before changing it.
#[cfg(test)]
mod unlinkat_flag_tests {
    use super::*;

    #[test]
    fn no_flags_is_the_unlink_half() {
        assert_eq!(unlinkat_removes_a_directory(0), Ok(false));
    }

    #[test]
    fn at_removedir_is_the_rmdir_half() {
        assert_eq!(unlinkat_removes_a_directory(AT_REMOVEDIR), Ok(true));
    }

    #[test]
    fn at_removedir_is_the_number_linux_uses() {
        // `include/uapi/linux/fcntl.h`: `#define AT_REMOVEDIR 0x200`. Getting
        // this wrong does not fail loudly — it silently picks the other half.
        assert_eq!(AT_REMOVEDIR, 0x200);
    }

    #[test]
    fn it_is_deliberately_not_in_at_flags() {
        // Linux gives AT_REMOVEDIR and AT_EACCESS the same value, because they
        // belong to different syscalls. `AtFlags` holds AT_EACCESS, so folding
        // AT_REMOVEDIR in would make the two indistinguishable and put this
        // bug straight back.
        assert_eq!(AT_REMOVEDIR, AtFlags::EACCESS.bits());
        assert!(AtFlags::from_bits(AT_REMOVEDIR).is_some());
    }

    #[test]
    fn rmdir_takes_directories_and_unlink_takes_everything_else() {
        assert_eq!(unlinkat_type_check(true, true), Ok(()));
        assert_eq!(unlinkat_type_check(false, false), Ok(()));
    }

    #[test]
    fn each_half_refuses_the_other_halfs_target_with_its_own_errno() {
        // `rmdir("file")` is ENOTDIR and `unlink("dir")` is EISDIR, and
        // userspace tells the two apart: `rm` retries as a directory on
        // EISDIR and gives up on ENOTDIR.
        assert_eq!(unlinkat_type_check(true, false), Err(LxError::ENOTDIR));
        assert_eq!(unlinkat_type_check(false, true), Err(LxError::EISDIR));
    }

    #[test]
    fn the_two_syscalls_line_up_end_to_end() {
        // `rmdir(2)` only exists on x86_64, so `rmdir(path)` and
        // `unlinkat(AT_FDCWD, path, AT_REMOVEDIR)` must be one implementation.
        // This walks the flag word the way `sys_rmdir` hands it over.
        let remove_dir = unlinkat_removes_a_directory(AT_REMOVEDIR).unwrap();
        assert!(remove_dir);
        assert_eq!(unlinkat_type_check(remove_dir, true), Ok(()));
        assert_eq!(
            unlinkat_type_check(remove_dir, false),
            Err(LxError::ENOTDIR)
        );
        // And plain `unlink(path)`, which hands over 0.
        let remove_dir = unlinkat_removes_a_directory(0).unwrap();
        assert!(!remove_dir);
        assert_eq!(unlinkat_type_check(remove_dir, false), Ok(()));
        assert_eq!(unlinkat_type_check(remove_dir, true), Err(LxError::EISDIR));
    }

    #[test]
    fn any_other_flag_is_einval() {
        // `if ((flag & ~AT_REMOVEDIR) != 0) return -EINVAL;`. Parsed as
        // `AtFlags` these were dropped in silence, so `unlinkat` accepted
        // flags it does not implement and reported success.
        for bad in [
            AtFlags::SYMLINK_NOFOLLOW.bits(),
            AtFlags::EMPTY_PATH.bits(),
            AtFlags::SYMLINK_FOLLOW.bits(),
            AT_REMOVEDIR | AtFlags::EMPTY_PATH.bits(),
            usize::MAX,
        ] {
            assert_eq!(
                unlinkat_removes_a_directory(bad),
                Err(LxError::EINVAL),
                "flag {:#x}",
                bad
            );
        }
    }
}

/// Which `AT_*` bits each syscall will take.
///
/// `AtFlags::from_bits_truncate` drops what it does not know, so every one of
/// these syscalls used to accept any flag word at all and act on whichever
/// bits it happened to recognise. Two shapes of that, and the tests below
/// pin both: a bit that means something to a *different* syscall got through,
/// and a bit Linux accepts is one `AtFlags` cannot even name, so the check
/// cannot be `from_bits` either.
#[cfg(test)]
mod at_flags_tests {
    use super::*;

    /// `fs/stat.c`, `fs/namei.c`, `include/uapi/linux/fcntl.h`.
    #[test]
    fn the_allowed_sets_are_the_numbers_linux_uses() {
        assert_eq!(AtFlags::SYMLINK_NOFOLLOW.bits(), 0x100);
        assert_eq!(AtFlags::EACCESS.bits(), 0x200);
        assert_eq!(AtFlags::SYMLINK_FOLLOW.bits(), 0x400);
        assert_eq!(AT_NO_AUTOMOUNT, 0x800);
        assert_eq!(AtFlags::EMPTY_PATH.bits(), 0x1000);
        assert_eq!(AT_STATX_SYNC_TYPE, 0x6000);
        assert_eq!(FSTATAT_FLAGS, 0x100 | 0x800 | 0x1000);
        assert_eq!(STATX_FLAGS, 0x100 | 0x800 | 0x1000 | 0x6000);
        assert_eq!(LINKAT_FLAGS, 0x400 | 0x1000);
    }

    /// `AT_EACCESS` is `faccessat`'s and `AT_REMOVEDIR` is `unlinkat`'s, and
    /// Linux gives them the same bit because they belong to different
    /// syscalls. `fstatat` has no business with either, and used to take both.
    #[test]
    fn fstatat_refuses_the_bit_that_belongs_to_another_syscall() {
        assert_eq!(AtFlags::EACCESS.bits(), AT_REMOVEDIR);
        assert_eq!(
            at_flags(AtFlags::EACCESS.bits(), FSTATAT_FLAGS).err(),
            Some(LxError::EINVAL)
        );
        assert_eq!(
            at_flags(AtFlags::SYMLINK_FOLLOW.bits(), FSTATAT_FLAGS).err(),
            Some(LxError::EINVAL),
            "AT_SYMLINK_FOLLOW is linkat's"
        );
    }

    /// The other shape, and the reason the check is on raw bits: coreutils'
    /// `stat` and `ls` send `AT_NO_AUTOMOUNT` on every call, and `AtFlags`
    /// has no name for it. A check written as `from_bits(...).ok_or(EINVAL)`
    /// would have refused `ls`.
    #[test]
    fn fstatat_accepts_at_no_automount_which_at_flags_cannot_name() {
        assert!(AtFlags::from_bits(AT_NO_AUTOMOUNT).is_none());
        assert_eq!(
            at_flags(AT_NO_AUTOMOUNT, FSTATAT_FLAGS),
            Ok(AtFlags::empty())
        );
    }

    #[test]
    fn statx_takes_its_sync_bits_and_fstatat_does_not() {
        for bit in [0x2000, 0x4000, AT_STATX_SYNC_TYPE] {
            assert_eq!(at_flags(bit, STATX_FLAGS), Ok(AtFlags::empty()), "{bit:#x}");
            assert_eq!(
                at_flags(bit, FSTATAT_FLAGS).err(),
                Some(LxError::EINVAL),
                "{bit:#x}"
            );
        }
    }

    #[test]
    fn linkat_takes_symlink_follow_and_refuses_symlink_nofollow() {
        assert_eq!(
            at_flags(AtFlags::SYMLINK_FOLLOW.bits(), LINKAT_FLAGS),
            Ok(AtFlags::SYMLINK_FOLLOW)
        );
        assert_eq!(
            at_flags(AtFlags::SYMLINK_NOFOLLOW.bits(), LINKAT_FLAGS).err(),
            Some(LxError::EINVAL)
        );
    }

    #[test]
    fn a_bit_no_syscall_knows_is_einval_everywhere() {
        for allowed in [FSTATAT_FLAGS, STATX_FLAGS, LINKAT_FLAGS] {
            assert_eq!(at_flags(0, allowed), Ok(AtFlags::empty()));
            for bit in 0..usize::BITS as usize {
                let flag = 1usize << bit;
                if allowed & flag != 0 {
                    continue;
                }
                assert_eq!(
                    at_flags(flag, allowed).err(),
                    Some(LxError::EINVAL),
                    "{flag:#x} slipped past {allowed:#x}"
                );
            }
        }
    }

    /// What survives the check still has to arrive parsed, or the syscall
    /// stops following symlinks and stops honouring AT_EMPTY_PATH.
    #[test]
    fn the_bits_that_are_allowed_still_reach_the_caller() {
        let flags = AtFlags::SYMLINK_NOFOLLOW.bits() | AtFlags::EMPTY_PATH.bits();
        let parsed = at_flags(flags | AT_NO_AUTOMOUNT, FSTATAT_FLAGS).unwrap();
        assert!(parsed.contains(AtFlags::SYMLINK_NOFOLLOW));
        assert!(parsed.contains(AtFlags::EMPTY_PATH));
        assert!(!parsed.contains(AtFlags::SYMLINK_FOLLOW));
    }
}
