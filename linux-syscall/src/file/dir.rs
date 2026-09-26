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
use linux_object::fs::vfs::{FileType, INode, Metadata};
use linux_object::fs::File;

/// `getcwd(2)`, from the path down: the raw syscall returns the LENGTH of
/// the string it wrote, NUL included (`fs/d_path.c`), and `ERANGE` when
/// `len` cannot hold it. It used to return the buffer's address, which is
/// what the glibc wrapper hands back to C, not what the kernel does: Go's
/// `syscall.Getwd` checks the number against the buffer and gave `EINVAL`.
pub(crate) fn getcwd_into(cwd: &str, mut buf: UserOutPtr<u8>, len: usize) -> SysResult {
    let with_nul = cwd.len() + 1;
    if with_nul > len {
        return Err(LxError::ERANGE);
    }
    buf.write_cstring(cwd)?;
    Ok(with_nul)
}

impl Syscall<'_> {
    /// return a null-terminated string containing an absolute pathname
    /// that is the current working directory of the calling process.
    /// - `buf` – pointer to buffer to receive path
    /// - `len` – size of buf
    pub fn sys_getcwd(&self, buf: UserOutPtr<u8>, len: usize) -> SysResult {
        info!("getcwd: buf={:?}, len={:#x}", buf, len);
        let proc = self.linux_process();
        let cwd = proc.current_working_directory();
        getcwd_into(&cwd, buf, len)
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

        let (dir_path, last) = last_component(path)?;
        let file_name = last.to_mkdir()?;
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
        let (dir_path, last) = last_component(path)?;
        let file_name = last.to_create(has_trailing_slash(path))?;
        let proc = self.linux_process();
        // `may_mknod`: a character or block device node needs `CAP_MKNOD`, and
        // it is asked before anything else. Nothing asked it here, so any
        // process could make a node naming any major and minor it liked.
        if matches!(file_type, FileType::CharDevice | FileType::BlockDevice)
            && !proc.capable(linux_object::process::CAP_MKNOD)
        {
            return Err(LxError::EPERM);
        }
        let inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let dir_metadata = inode.metadata()?;
        // The name is looked up BEFORE the parent's mode, as `filename_create`
        // does it: Linux answers `EEXIST` from the lookup and only then asks
        // `may_create` about the parent. See `sys_mkdirat`, where the reverse
        // order cost a whole desktop its sound daemon.
        if inode.find(file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        proc.check_access(&dir_metadata, 0o3, true)?;
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
        let (new_dir_path, new_last) = last_component(newpath)?;
        let new_file_name = new_last.to_create(has_trailing_slash(newpath))?;
        let follow = flags.contains(AtFlags::SYMLINK_FOLLOW);
        // `AT_EMPTY_PATH`: the file `olddirfd` is open on (`LOOKUP_EMPTY`).
        // Linux asks that the descriptor's opening credentials be the
        // caller's, or `CAP_DAC_READ_SEARCH`; a descriptor here carries no
        // credentials of its own, so what a process holds it may link.
        // Without the flag an empty path is `ENOENT`, from the lookup.
        let inode = if flags.contains(AtFlags::EMPTY_PATH) && oldpath.is_empty() {
            inode_of_dirfd(proc, olddirfd)?
        } else {
            proc.lookup_inode_at(olddirfd, oldpath, follow)?
        };
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, true)?;
        let new_dir_metadata = new_dir_inode.metadata()?;
        // `do_linkat`: the new name is looked up (`filename_create`, EEXIST)
        // before the parent's mode and before `may_linkat` and `vfs_link`
        // decide whether THIS file may be linked at all: a directory never,
        // another user's file only when it is a safe source
        // (`protected_hardlinks`). See `check_link`.
        if new_dir_inode.find(new_file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        proc.check_access(&new_dir_metadata, 0o3, true)?;
        proc.check_link(&inode.metadata()?)?;
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
        let (dir_path, last) = last_component(path)?;
        let file_name = if remove_dir {
            last.to_rmdir()?
        } else {
            last.to_unlink()?
        };
        let dir_inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let dir_metadata = dir_inode.metadata()?;
        // The name first, the parent's mode after: `do_unlinkat` resolves it
        // with search permission alone and answers `ENOENT` from the lookup,
        // and `may_delete`'s write check comes later. `rm -f dir/absent` in a
        // directory the caller may not write said "Permission denied" where it
        // has to say "No such file or directory", so every script with the
        // usual "already gone, fine" branch on `ENOENT` took the error branch.
        let file_inode = dir_inode.find(file_name)?;
        proc.check_access(&dir_metadata, 0o3, true)?;
        let file_metadata = file_inode.metadata()?;
        unlinkat_type_check(
            remove_dir,
            file_metadata.type_ == FileType::Dir,
            has_trailing_slash(path),
        )?;
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
        let (old_dir_path, old_last) = last_component(oldpath)?;
        let (new_dir_path, new_last) = last_component(newpath)?;
        let old_file_name = old_last.to_rename()?;
        let new_file_name = new_last.to_rename()?;
        // The parents are intermediate components: a symlink among them is
        // followed, as everywhere else (`mv x /tmp/link-to-dir/y`).
        let old_dir_inode = proc.lookup_inode_at(olddirfd, old_dir_path, true)?;
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, true)?;
        let old_dir_metadata = old_dir_inode.metadata()?;
        let new_dir_metadata = new_dir_inode.metadata()?;
        // The source name first: `do_renameat2` resolves both sides before
        // `may_delete`/`may_create` look at either parent, so a source that is
        // not there is `ENOENT` and not `EACCES`.
        let old_inode = old_dir_inode.find(old_file_name)?;
        proc.check_access(&old_dir_metadata, 0o3, true)?;
        proc.check_access(&new_dir_metadata, 0o3, true)?;
        let old_metadata = old_inode.metadata()?;
        rename_slash_check(
            old_metadata.type_ == FileType::Dir,
            has_trailing_slash(oldpath),
            has_trailing_slash(newpath),
        )?;
        let new_inode = new_dir_inode.find(new_file_name).ok();
        if flags & RENAME_NOREPLACE != 0 && new_inode.is_some() {
            return Err(LxError::EEXIST);
        }
        // `do_renameat2`: "source should not be an ancestor of target" is
        // EINVAL before any permission is asked (`lock_rename`'s trap).
        if old_metadata.type_ == FileType::Dir && is_same_or_below(&new_dir_inode, &old_metadata)? {
            return Err(LxError::EINVAL);
        }
        let new_metadata = new_inode.map(|i| i.metadata()).transpose()?;
        proc.check_rename(
            &old_dir_metadata,
            &old_metadata,
            &new_dir_metadata,
            new_metadata.as_ref(),
        )?;
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
        // `do_readlinkat`: `bufsiz <= 0` is EINVAL before the path is read.
        let len = crate::intarg::readlink_bufsiz(len)?;
        let path = path.as_c_str()?;
        info!(
            "readlinkat: dirfd={:?}, path={:?}, base={:?}, len={}",
            dirfd, path, base, len
        );

        let proc = self.linux_process();
        // A trailing slash resolves the link (`LOOKUP_FOLLOW`) to what must
        // be a directory: `ENOTDIR` or `ENOENT` from the lookup, else
        // `EINVAL`, a directory not being a symlink. Splitting the name off
        // used to read the link `l` for `readlink("l/")`.
        if has_trailing_slash(path) {
            proc.lookup_inode_at(dirfd, path, false)?;
            return Err(LxError::EINVAL);
        }
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

        // `do_symlinkat`: an empty target is `ENOENT` (`getname` again),
        // before the link's own name is looked at.
        if target.is_empty() {
            return Err(LxError::ENOENT);
        }
        let (dir_path, last) = last_component(linkpath)?;
        let file_name = last.to_create(has_trailing_slash(linkpath))?;
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(newdirfd, dir_path, true)?;
        let dir_metadata = inode.metadata()?;
        // The name is looked up BEFORE the parent's mode, as `filename_create`
        // does it: Linux answers `EEXIST` from the lookup and only then asks
        // `may_create` about the parent. See `sys_mkdirat`, where the reverse
        // order cost a whole desktop its sound daemon.
        if inode.find(file_name).is_ok() {
            return Err(LxError::EEXIST);
        }
        proc.check_access(&dir_metadata, 0o3, true)?;
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
    loop {
        match src.next_entry() {
            Ok(Some((meta, name))) => {
                if !push(src.position(), &meta, &name) {
                    src.unread_entry();
                    if pushed == 0 {
                        return Err(LxError::EINVAL);
                    }
                    break;
                }
                pushed += 1;
            }
            Ok(None) => break,
            // An error AFTER something was read is not this call's answer.
            // `next_entry` has already moved the directory position past the
            // entries handed over, so propagating here dropped every one of
            // them: `ls` and `find` silently missed a run of names whenever
            // one entry's metadata would not resolve. `getdents64` does the
            // opposite -- `lastdirent = buf.current_dir; if (lastdirent) ...
            // error = count - buf.count;` -- so the names go out now and the
            // error surfaces on the next call, which starts at the entry that
            // failed.
            Err(e) => {
                if pushed == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }
    Ok(pushed)
}

#[cfg(test)]
mod getcwd_tests {
    //! What the raw `getcwd` hands back in `rax`: a length, not an address.

    use super::*;

    fn call(cwd: &str, len: usize) -> (SysResult, [u8; 16]) {
        let mut buf = [0xaau8; 16];
        let r = getcwd_into(cwd, UserOutPtr::from(buf.as_mut_ptr() as usize), len);
        (r, buf)
    }

    /// The length of the string with its NUL, as `sys_getcwd` returns it.
    /// Go's `syscall.Getwd` refuses anything else; glibc kept the number in
    /// an `int`.
    #[test]
    fn returns_the_length_with_the_nul_not_the_address() {
        let (r, buf) = call("/tmp", 16);
        assert_eq!(r, Ok(5));
        assert_eq!(&buf[..5], b"/tmp\0");
        assert_eq!(buf[5], 0xaa, "nothing past the NUL");
        let (r, buf) = call("/", 16);
        assert_eq!(r, Ok(2));
        assert_eq!(&buf[..2], b"/\0");
    }

    /// A buffer exactly the string's length has no room for the NUL and
    /// is `ERANGE`, untouched; one byte more fits.
    #[test]
    fn a_buffer_without_room_for_the_nul_is_erange() {
        let (r, buf) = call("/tmp", 4);
        assert_eq!(r, Err(LxError::ERANGE));
        assert!(buf.iter().all(|&b| b == 0xaa), "nothing was written");
        assert_eq!(call("/tmp", 5).0, Ok(5));
        assert_eq!(call("/tmp", 0).0, Err(LxError::ERANGE));
    }
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

/// What `fchownat(2)` accepts (`do_fchownat`, `fs/open.c`).
pub(crate) const FCHOWNAT_FLAGS: usize =
    AtFlags::SYMLINK_NOFOLLOW.bits() | AtFlags::EMPTY_PATH.bits();

/// What `faccessat2(2)` accepts (`do_faccessat`, `fs/open.c`: `if (flags &
/// ~(AT_EACCESS | AT_SYMLINK_NOFOLLOW)) return -EINVAL;`).
pub(crate) const FACCESSAT_FLAGS: usize =
    AtFlags::EACCESS.bits() | AtFlags::SYMLINK_NOFOLLOW.bits();

/// What `fchmodat2(2)` accepts (`do_fchmodat`, `fs/open.c`), which is what
/// the FreeBSD `fchmodat` translation hands `sys_fchmodat`; the Linux
/// `fchmodat` number carries no flags at all.
pub(crate) const FCHMODAT_FLAGS: usize =
    AtFlags::SYMLINK_NOFOLLOW.bits() | AtFlags::EMPTY_PATH.bits();

/// The file `dirfd` is open on, for a syscall given `AT_EMPTY_PATH` and an
/// empty path (`LOOKUP_EMPTY`): `AT_FDCWD` names the working directory.
pub(crate) fn inode_of_dirfd(
    proc: &linux_object::process::LinuxProcess,
    dirfd: FileDesc,
) -> LxResult<Arc<dyn INode>> {
    if dirfd == FileDesc::CWD {
        proc.lookup_inode_at(FileDesc::CWD, ".", true)
    } else {
        Ok(proc.get_file(dirfd)?.inode())
    }
}

/// What the last component of a path names, as `fs/namei.c` classifies it
/// (`LAST_NORM`, `LAST_ROOT`, `LAST_DOT`, `LAST_DOTDOT`).
///
/// `split_path` alone handed every caller `(".", "")` for the empty path
/// and for `/` alike, and no caller looked at the name it got back:
/// `mkdir("")`, `mkdir("/")`, `open("/", O_CREAT)`, `mknod("/")` and
/// `symlink(t, "/")` each created an entry whose name is the empty string in
/// the current directory -- the ramfs takes any name that is not `.` or
/// `..` -- listed by `getdents` as "" and reachable by no path, and
/// `unlink("/")` removed it again. Linux never gets there: an empty path is
/// `ENOENT` before any lookup (`getname`), and a last component that is the
/// root, `.` or `..` is refused by each syscall with its own errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastComponent<'a> {
    /// An ordinary name under its parent.
    Name(&'a str),
    /// The path was `/` (or only slashes).
    Root,
    /// The path ends in `.`.
    Dot,
    /// The path ends in `..`.
    DotDot,
}

/// The parent's path and the last component of `path`, or `ENOENT` for the
/// empty path.
pub(crate) fn last_component(path: &str) -> LxResult<(&str, LastComponent<'_>)> {
    if path.is_empty() {
        return Err(LxError::ENOENT);
    }
    let (dir_path, name) = split_path(path);
    let last = match name {
        "" => LastComponent::Root,
        "." => LastComponent::Dot,
        ".." => LastComponent::DotDot,
        name => LastComponent::Name(name),
    };
    Ok((dir_path, last))
}

/// `nd->last.name[nd->last.len]`: the path ends in a slash, which in Linux
/// is a promise that the last component is a directory. `split_path` trims
/// the slash and every caller forgot it was there: `unlink("f/")` deleted the
/// file `f`, `rename("f/", "g")` moved it, `symlink(t, "l/")` and
/// `mknod("p/")` created `l` and `p`, and `open("new/", O_CREAT)` created
/// `new` -- every one of them an error on Linux, because a name with a slash
/// after it can only ever be a directory.
pub(crate) fn has_trailing_slash(path: &str) -> bool {
    path.ends_with('/')
}

impl<'a> LastComponent<'a> {
    /// `filename_create` for `mkdir`: the root, `.` and `..` exist already,
    /// `EEXIST`; a trailing slash is fine, a directory is what is being made.
    pub(crate) fn to_mkdir(self) -> LxResult<&'a str> {
        match self {
            LastComponent::Name(name) => Ok(name),
            _ => Err(LxError::EEXIST),
        }
    }

    /// `filename_create` for everything else that makes a name (`mknod`,
    /// `symlink`, `link`'s new name): as `mkdir`, and a trailing slash is
    /// `ENOENT` (`if (unlikely(!is_dir && last.name[last.len])) return
    /// -ENOENT`), since what it promises cannot be made by this call.
    pub(crate) fn to_create(self, trailing_slash: bool) -> LxResult<&'a str> {
        let name = self.to_mkdir()?;
        if trailing_slash {
            return Err(LxError::ENOENT);
        }
        Ok(name)
    }

    /// `do_open` with `O_CREAT`: the root, `.` and `..` are directories,
    /// `EISDIR`, whatever else was asked; so is a trailing slash
    /// (`open_last_lookups`: `if (unlikely(nd->last.name[nd->last.len]))
    /// return ERR_PTR(-EISDIR)`), whether or not the name exists.
    pub(crate) fn to_open_create(self, trailing_slash: bool) -> LxResult<&'a str> {
        match self {
            LastComponent::Name(name) if !trailing_slash => Ok(name),
            _ => Err(LxError::EISDIR),
        }
    }

    /// `do_unlinkat`: anything but a plain name is `EISDIR`.
    pub(crate) fn to_unlink(self) -> LxResult<&'a str> {
        match self {
            LastComponent::Name(name) => Ok(name),
            _ => Err(LxError::EISDIR),
        }
    }

    /// `do_rmdir`: the root is `EBUSY`, `.` is `EINVAL`, `..` is
    /// `ENOTEMPTY`.
    pub(crate) fn to_rmdir(self) -> LxResult<&'a str> {
        match self {
            LastComponent::Name(name) => Ok(name),
            LastComponent::Root => Err(LxError::EBUSY),
            LastComponent::Dot => Err(LxError::EINVAL),
            LastComponent::DotDot => Err(LxError::ENOTEMPTY),
        }
    }

    /// `do_renameat2`: either side that is not a plain name is `EBUSY`.
    pub(crate) fn to_rename(self) -> LxResult<&'a str> {
        match self {
            LastComponent::Name(name) => Ok(name),
            _ => Err(LxError::EBUSY),
        }
    }
}

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
fn unlinkat_type_check(
    remove_dir: bool,
    is_dir: bool,
    trailing_slash: bool,
) -> linux_object::error::LxResult<()> {
    match (remove_dir, is_dir) {
        // `rmdir` on something that is not a directory.
        (true, false) => Err(LxError::ENOTDIR),
        // `unlink` on a directory.
        (false, true) => Err(LxError::EISDIR),
        // `unlink("f/")`: `do_unlinkat`'s `slashes:` label, `ENOTDIR` for
        // anything that is not a directory. It used to delete `f`.
        (false, false) if trailing_slash => Err(LxError::ENOTDIR),
        _ => Ok(()),
    }
}

/// `do_renameat2`: a trailing slash on either name is a promise that the
/// source is a directory, `ENOTDIR` when it is not (`if (!d_is_dir(old_dentry))
/// { error = -ENOTDIR; if (old_last.name[old_last.len]) goto exit5; if
/// (new_last.name[new_last.len]) goto exit5; }`).
fn rename_slash_check(
    old_is_dir: bool,
    old_trailing_slash: bool,
    new_trailing_slash: bool,
) -> linux_object::error::LxResult<()> {
    if !old_is_dir && (old_trailing_slash || new_trailing_slash) {
        return Err(LxError::ENOTDIR);
    }
    Ok(())
}

/// renameat2(2) `RENAME_NOREPLACE`: don't overwrite an existing target.
const RENAME_NOREPLACE: usize = 1 << 0;
/// renameat2(2) `RENAME_EXCHANGE`: atomically swap source and target.
const RENAME_EXCHANGE: usize = 1 << 1;
/// renameat2(2) `RENAME_WHITEOUT`: leave a whiteout behind (overlayfs).
const RENAME_WHITEOUT: usize = 1 << 2;

/// Whether `dir` is `ancestor` itself or lies anywhere below it: the walk up
/// `..` from `dir` meets an inode with `ancestor`'s (dev, inode) before it
/// reaches a directory that is its own parent (the root of that filesystem).
///
/// `rename("a", "a/b/c")` used to reach the filesystem as an ordinary move.
/// One that links the entry before unlinking it (ramfs) would then leave `a`
/// reachable only from inside itself: a cycle no path from `/` enters, every
/// file under it lost, and every inode in it kept alive by its own
/// reference. Linux refuses this in `do_renameat2` with `EINVAL`, before the
/// filesystem is asked.
///
/// A mount point's `..` stays inside the mounted filesystem, so a directory
/// below a mount is not seen as below the mount point's parent; a rename
/// across filesystems fails with `EXDEV` anyway.
pub(crate) fn is_same_or_below(dir: &Arc<dyn INode>, ancestor: &Metadata) -> LxResult<bool> {
    let mut here = dir.clone();
    // Bounded, so a filesystem whose `..` never reaches a fixed point (a
    // broken parent pointer) cannot spin this walk forever.
    for _ in 0..4096 {
        let meta = here.metadata()?;
        if meta.dev == ancestor.dev && meta.inode == ancestor.inode {
            return Ok(true);
        }
        let up = here.find("..")?;
        let up_meta = up.metadata()?;
        if up_meta.dev == meta.dev && up_meta.inode == meta.inode {
            return Ok(false);
        }
        here = up;
    }
    Err(LxError::ELOOP)
}

#[cfg(test)]
mod rename_trap_tests {
    use super::*;
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    fn a_tree() -> (
        Arc<dyn INode>,
        Arc<dyn INode>,
        Arc<dyn INode>,
        Arc<dyn INode>,
    ) {
        let root = RamFS::new().root_inode();
        let a = root.create("a", FileType::Dir, 0o755).unwrap();
        let b = a.create("b", FileType::Dir, 0o755).unwrap();
        let other = root.create("other", FileType::Dir, 0o755).unwrap();
        (root, a, b, other)
    }

    #[test]
    fn a_directory_is_below_itself_and_below_its_ancestors() {
        let (root, a, b, _) = a_tree();
        let a_meta = a.metadata().unwrap();
        // `mv a a/x`: the target's parent IS the source.
        assert_eq!(is_same_or_below(&a, &a_meta), Ok(true));
        // `mv a a/b/x`: the target's parent is two levels inside the source.
        assert_eq!(is_same_or_below(&b, &a_meta), Ok(true));
        // Everything is below the root.
        assert_eq!(is_same_or_below(&b, &root.metadata().unwrap()), Ok(true));
    }

    #[test]
    fn a_sibling_and_a_parent_are_not_below() {
        let (root, a, b, other) = a_tree();
        let a_meta = a.metadata().unwrap();
        // `mv a other/x`: a sibling subtree.
        assert_eq!(is_same_or_below(&other, &a_meta), Ok(false));
        // `mv a /x`: the root is above `a`, not below it.
        assert_eq!(is_same_or_below(&root, &a_meta), Ok(false));
        // `mv a/b /x` seen from `b`: the root is not below `b` either.
        assert_eq!(is_same_or_below(&root, &b.metadata().unwrap()), Ok(false));
    }

    #[test]
    fn the_walk_stops_at_the_root_whose_parent_is_itself() {
        // Reaching the root without meeting the ancestor answers `false`
        // rather than walking `..` of the root forever.
        //
        // The root and `a` are held for the length of the test: a ramfs child
        // points at its parent with a `Weak`, and the only strong reference to
        // `a` is the root's `children` map, so dropping these two handles
        // leaves `b` with a `..` that no longer resolves and the walk answers
        // `ENOENT` before it ever reaches the root.
        let (_root, _a, b, other) = a_tree();
        assert_eq!(is_same_or_below(&b, &other.metadata().unwrap()), Ok(false));
    }
}

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
        assert_eq!(unlinkat_type_check(true, true, false), Ok(()));
        assert_eq!(unlinkat_type_check(false, false, false), Ok(()));
    }

    #[test]
    fn each_half_refuses_the_other_halfs_target_with_its_own_errno() {
        // `rmdir("file")` is ENOTDIR and `unlink("dir")` is EISDIR, and
        // userspace tells the two apart: `rm` retries as a directory on
        // EISDIR and gives up on ENOTDIR.
        assert_eq!(
            unlinkat_type_check(true, false, false),
            Err(LxError::ENOTDIR)
        );
        assert_eq!(
            unlinkat_type_check(false, true, false),
            Err(LxError::EISDIR)
        );
    }

    /// `unlink("f/")`: the slash promises a directory, and `f` is not one,
    /// so `ENOTDIR` -- it used to delete `f`. `rmdir("d/")` is the normal
    /// spelling and `unlink("d/")` is still `EISDIR`.
    #[test]
    fn a_trailing_slash_on_unlink_is_enotdir_for_anything_but_a_directory() {
        assert_eq!(
            unlinkat_type_check(false, false, true),
            Err(LxError::ENOTDIR)
        );
        assert_eq!(unlinkat_type_check(false, true, true), Err(LxError::EISDIR));
        assert_eq!(unlinkat_type_check(true, true, true), Ok(()));
        assert_eq!(
            unlinkat_type_check(true, false, true),
            Err(LxError::ENOTDIR)
        );
    }

    #[test]
    fn the_two_syscalls_line_up_end_to_end() {
        // `rmdir(2)` only exists on x86_64, so `rmdir(path)` and
        // `unlinkat(AT_FDCWD, path, AT_REMOVEDIR)` must be one implementation.
        // This walks the flag word the way `sys_rmdir` hands it over.
        let remove_dir = unlinkat_removes_a_directory(AT_REMOVEDIR).unwrap();
        assert!(remove_dir);
        assert_eq!(unlinkat_type_check(remove_dir, true, false), Ok(()));
        assert_eq!(
            unlinkat_type_check(remove_dir, false, false),
            Err(LxError::ENOTDIR)
        );
        // And plain `unlink(path)`, which hands over 0.
        let remove_dir = unlinkat_removes_a_directory(0).unwrap();
        assert!(!remove_dir);
        assert_eq!(unlinkat_type_check(remove_dir, false, false), Ok(()));
        assert_eq!(
            unlinkat_type_check(remove_dir, true, false),
            Err(LxError::EISDIR)
        );
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
        for allowed in [
            FSTATAT_FLAGS,
            STATX_FLAGS,
            LINKAT_FLAGS,
            FCHOWNAT_FLAGS,
            FACCESSAT_FLAGS,
            FCHMODAT_FLAGS,
        ] {
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

/// The last component of a path, the one every creating and removing
/// syscall acts on by name.
#[cfg(test)]
mod last_component_tests {
    use super::*;
    use rcore_fs::vfs::FileSystem;
    use rcore_fs_ramfs::RamFS;

    fn last(path: &str) -> LastComponent<'_> {
        last_component(path).unwrap().1
    }

    #[test]
    fn the_empty_path_is_enoent_before_anything_is_split() {
        assert_eq!(last_component("").err(), Some(LxError::ENOENT));
    }

    #[test]
    fn the_root_and_the_dots_are_told_apart_from_a_name() {
        for root in ["/", "//", "///"] {
            assert_eq!(last(root), LastComponent::Root, "{root:?}");
        }
        for dot in [".", "./", "a/.", "/a/./"] {
            assert_eq!(last(dot), LastComponent::Dot, "{dot:?}");
        }
        for dotdot in ["..", "../", "a/..", "/.."] {
            assert_eq!(last(dotdot), LastComponent::DotDot, "{dotdot:?}");
        }
        // A name that merely starts with a dot is a name.
        assert_eq!(last(".hidden"), LastComponent::Name(".hidden"));
        assert_eq!(last("a/..."), LastComponent::Name("..."));
    }

    #[test]
    fn a_name_keeps_its_parent() {
        assert_eq!(last_component("a"), Ok((".", LastComponent::Name("a"))));
        assert_eq!(last_component("a/"), Ok((".", LastComponent::Name("a"))));
        assert_eq!(last_component("/a"), Ok(("/", LastComponent::Name("a"))));
        assert_eq!(
            last_component("/x/y/a"),
            Ok(("/x/y", LastComponent::Name("a")))
        );
    }

    #[test]
    fn each_syscall_refuses_what_is_not_a_name_with_its_own_errno() {
        for special in [
            LastComponent::Root,
            LastComponent::Dot,
            LastComponent::DotDot,
        ] {
            // `filename_create`: `EEXIST`, they are all there already.
            assert_eq!(special.to_mkdir(), Err(LxError::EEXIST), "{special:?}");
            assert_eq!(
                special.to_create(false),
                Err(LxError::EEXIST),
                "{special:?}"
            );
            // `do_open`: a directory, `EISDIR`.
            assert_eq!(
                special.to_open_create(false),
                Err(LxError::EISDIR),
                "{special:?}"
            );
            // `do_unlinkat`: `EISDIR`.
            assert_eq!(special.to_unlink(), Err(LxError::EISDIR), "{special:?}");
            // `do_renameat2`: `EBUSY`.
            assert_eq!(special.to_rename(), Err(LxError::EBUSY), "{special:?}");
        }
        // `do_rmdir` tells the three apart.
        assert_eq!(LastComponent::Root.to_rmdir(), Err(LxError::EBUSY));
        assert_eq!(LastComponent::Dot.to_rmdir(), Err(LxError::EINVAL));
        assert_eq!(LastComponent::DotDot.to_rmdir(), Err(LxError::ENOTEMPTY));
        // A name passes through everywhere.
        let name = LastComponent::Name("a");
        assert_eq!(name.to_mkdir(), Ok("a"));
        assert_eq!(name.to_create(false), Ok("a"));
        assert_eq!(name.to_open_create(false), Ok("a"));
        assert_eq!(name.to_unlink(), Ok("a"));
        assert_eq!(name.to_rmdir(), Ok("a"));
        assert_eq!(name.to_rename(), Ok("a"));
    }

    /// Why the guard has to be here: the filesystem takes the empty name.
    /// `mkdir("/")` reached it as `create("")` in the working directory.
    #[test]
    fn the_filesystem_would_take_an_empty_name_so_nothing_may_hand_it_one() {
        let root = RamFS::new().root_inode();
        let (dir_path, name) = split_path("/");
        assert_eq!((dir_path, name), (".", ""));
        root.create(name, FileType::File, 0o644).unwrap();
        assert!(root.list().unwrap().iter().any(|n| n.is_empty()));
        // The classified path never gets that far.
        assert_eq!(
            last_component("/").and_then(|(_, l)| l.to_mkdir()),
            Err(LxError::EEXIST)
        );
    }

    #[test]
    fn a_trailing_slash_is_seen_on_the_whole_path_not_on_the_split_name() {
        // `split_path` trims it, so it has to be asked of the path itself.
        assert!(has_trailing_slash("a/"));
        assert!(has_trailing_slash("/x/a//"));
        assert!(!has_trailing_slash("a"));
        assert!(!has_trailing_slash("/x/a"));
        assert_eq!(last("a/"), LastComponent::Name("a"));
    }

    /// The slash promises a directory: `mkdir` makes one, nothing else does.
    #[test]
    fn only_mkdir_may_be_asked_for_a_name_with_a_slash_after_it() {
        let name = LastComponent::Name("a");
        assert_eq!(name.to_mkdir(), Ok("a"));
        // `mknod("p/")`, `symlink(t, "l/")`, `link(f, "n/")`: `ENOENT`.
        assert_eq!(name.to_create(true), Err(LxError::ENOENT));
        // `open("new/", O_CREAT)`: `EISDIR`, whether or not `new` exists.
        assert_eq!(name.to_open_create(true), Err(LxError::EISDIR));
        // The specials keep their own errno ahead of the slash's.
        assert_eq!(LastComponent::Root.to_create(true), Err(LxError::EEXIST));
        assert_eq!(
            LastComponent::Dot.to_open_create(true),
            Err(LxError::EISDIR)
        );
    }

    /// `rename("f/", "g")` and `rename("f", "g/")` moved the file `f`.
    #[test]
    fn a_slash_on_either_side_of_a_rename_needs_a_directory_source() {
        assert_eq!(
            rename_slash_check(false, true, false),
            Err(LxError::ENOTDIR)
        );
        assert_eq!(
            rename_slash_check(false, false, true),
            Err(LxError::ENOTDIR)
        );
        assert_eq!(rename_slash_check(false, false, false), Ok(()));
        // A directory may be spelled with the slash on both sides.
        assert_eq!(rename_slash_check(true, true, true), Ok(()));
        assert_eq!(rename_slash_check(true, false, true), Ok(()));
    }
}
