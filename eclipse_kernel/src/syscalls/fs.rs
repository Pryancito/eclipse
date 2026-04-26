//! Filesystem-related syscalls implementation
//!
//! Implementation of VFS operations, scheme interaction, and file descriptor management.

use crate::process::current_process_id;
use alloc::format;
use alloc::vec::Vec;
use super::{copy_from_user, copy_to_user, is_user_pointer, linux_abi_error};

pub fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                if n > 0 && !copy_to_user(buf_ptr, &kbuf[..n]) { return linux_abi_error(14); }
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        if len > 0 && !copy_from_user(buf_ptr, &mut kbuf) { return linux_abi_error(14); }
        match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_open(path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    let pid = current_process_id().unwrap_or(0);
    match crate::scheme::open(path, flags as usize, mode as u32) {
        Ok((scheme_id, resource_id)) => {
            crate::fd::fd_create(pid, scheme_id, resource_id).unwrap_or(0) as u64
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_openat(_dfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    // Basic openat stub, should handle relative paths using dfd
    sys_open(path_ptr, flags, mode)
}

pub fn sys_close(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        crate::fd::fd_close(pid, fd as usize);
        let _ = crate::scheme::close(fd_entry.scheme_id, fd_entry.resource_id);
        0
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::lseek(fd_entry.scheme_id, fd_entry.resource_id, offset as isize, whence as usize, fd_entry.offset) {
            Ok(n) => {
                crate::fd::fd_set_offset(pid, fd as usize, n as u64);
                n as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, request as usize, arg as usize) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_ftruncate(fd: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ftruncate(fd_entry.scheme_id, fd_entry.resource_id, len as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_pread64(fd: u64, buf_ptr: u64, len: u64, offset: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        match crate::scheme::pread(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, offset) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_pwrite64(fd: u64, buf_ptr: u64, len: u64, offset: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
        let mut kbuf = Vec::with_capacity(len as usize);
        unsafe { kbuf.set_len(len as usize); }
        if len > 0 && !copy_from_user(buf_ptr, &mut kbuf) { return linux_abi_error(14); }
        match crate::scheme::pwrite(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, offset) {
            Ok(n) => n as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}


pub fn sys_writev(fd: u64, iov_ptr: u64, count: u64) -> u64 {
    let mut total = 0;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            if len > 0 && !copy_from_user(base, &mut kbuf) { return linux_abi_error(14); }
            match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, fd_entry.offset) {
                Ok(n) => {
                    crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                    total += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_readv(fd: u64, iov_ptr: u64, count: u64) -> u64 {
    let mut total = 0;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, fd_entry.offset) {
                Ok(n) => {
                    crate::fd::fd_set_offset(pid, fd as usize, fd_entry.offset + n as u64);
                    if n > 0 && !copy_to_user(base, &kbuf[..n]) { return linux_abi_error(14); }
                    total += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_preadv(fd: u64, iov_ptr: u64, count: u64, offset: u64) -> u64 {
    let mut total = 0;
    let mut current_offset = offset;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            match crate::scheme::pread(fd_entry.scheme_id, fd_entry.resource_id, &mut kbuf, current_offset) {
                Ok(n) => {
                    total += n as u64;
                    current_offset += n as u64;
                    if n > 0 && !copy_to_user(base, &kbuf[..n]) { return linux_abi_error(14); }
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}

pub fn sys_pwritev(fd: u64, iov_ptr: u64, count: u64, offset: u64) -> u64 {
    let mut total = 0;
    let mut current_offset = offset;
    let pid = current_process_id().unwrap_or(0);
    for i in 0..count {
        let iov_addr = iov_ptr + i * 16;
        if !is_user_pointer(iov_addr, 16) { return linux_abi_error(14); }
        let mut raw = [0u8; 16];
        if !copy_from_user(iov_addr, &mut raw) { return linux_abi_error(14); }
        let base = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            if len > 0 && !is_user_pointer(base, len) { return linux_abi_error(14); }
            let mut kbuf = Vec::with_capacity(len as usize);
            unsafe { kbuf.set_len(len as usize); }
            if len > 0 && !copy_from_user(base, &mut kbuf) { return linux_abi_error(14); }
            match crate::scheme::pwrite(fd_entry.scheme_id, fd_entry.resource_id, &kbuf, current_offset) {
                Ok(n) => {
                    total += n as u64;
                    current_offset += n as u64;
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        } else {
            return linux_abi_error(9);
        }
    }
    total
}



pub fn sys_faccessat(_dfd: u64, path_ptr: u64, _mode: u64, _flags: u64) -> u64 {
    // Basic stub: check if path is valid
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    0
}

pub fn sys_pipe(pipefd_ptr: u64) -> u64 {
    sys_pipe2(pipefd_ptr, 0)
}

pub fn sys_pipe2(pipefd_ptr: u64, flags: u64) -> u64 {
    if !is_user_pointer(pipefd_ptr, 8) { return linux_abi_error(14); }
    let pid = current_process_id().unwrap_or(0);
    let scheme_id = match crate::scheme::get_scheme_id("pipe") {
        Some(id) => id,
        None => return linux_abi_error(38),
    };
    
    let (rh, wh) = crate::pipe::PIPE_SCHEME.new_pipe();
    
    // Set O_NONBLOCK if requested (0x800) or O_CLOEXEC (0x80000)
    if (flags & 0x800) != 0 {
        crate::pipe::PIPE_SCHEME.set_nonblock(rh, true);
        crate::pipe::PIPE_SCHEME.set_nonblock(wh, true);
    }

    let fd1 = match crate::fd::fd_create(pid, scheme_id, rh) {
        Some(fd) => fd,
        None => {
            use crate::scheme::Scheme;
            let _ = crate::pipe::PIPE_SCHEME.close(rh);
            let _ = crate::pipe::PIPE_SCHEME.close(wh);
            return linux_abi_error(24);
        }
    };
    let fd2 = match crate::fd::fd_create(pid, scheme_id, wh) {
        Some(fd) => fd,
        None => {
            use crate::scheme::Scheme;
            let _ = crate::fd::fd_close(pid, fd1);
            let _ = crate::pipe::PIPE_SCHEME.close(wh);
            return linux_abi_error(24);
        }
    };

    let b1 = (fd1 as i32).to_le_bytes();
    let b2 = (fd2 as i32).to_le_bytes();
    if !copy_to_user(pipefd_ptr, &b1) { return linux_abi_error(14); }
    if !copy_to_user(pipefd_ptr + 4, &b2) { return linux_abi_error(14); }
    0
}

pub fn sys_mkdir(path_ptr: u64, mode: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::mkdir(path, mode as u32) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_rmdir(path_ptr: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::rmdir(path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_unlink(path_ptr: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");

    match crate::scheme::unlink(path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_rename(old_ptr: u64, new_ptr: u64) -> u64 {
    let old_len = super::strlen_user_unique(old_ptr, 1023);
    let new_len = super::strlen_user_unique(new_ptr, 1023);
    if old_len == 0 || new_len == 0 { return linux_abi_error(2); }
    
    let mut old_buf = [0u8; 1024];
    let mut new_buf = [0u8; 1024];
    if !super::copy_from_user(old_ptr, &mut old_buf[..old_len]) { return linux_abi_error(14); }
    if !super::copy_from_user(new_ptr, &mut new_buf[..new_len]) { return linux_abi_error(14); }
    
    let old_path = core::str::from_utf8(&old_buf[..old_len]).unwrap_or("");
    let new_path = core::str::from_utf8(&new_buf[..new_len]).unwrap_or("");

    match crate::scheme::rename(old_path, new_path) {
        Ok(_) => 0,
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_readdir(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    sys_getdents64(fd, buf_ptr, len)
}

pub fn sys_mount(src_ptr: u64, _target_ptr: u64) -> u64 {
    let src_len = super::strlen_user_unique(src_ptr, 1023);
    if src_len == 0 { return linux_abi_error(2); }
    let mut src_buf = [0u8; 1024];
    if !super::copy_from_user(src_ptr, &mut src_buf[..src_len]) { return linux_abi_error(14); }
    let src_path = core::str::from_utf8(&src_buf[..src_len]).unwrap_or("");
    
    // We only support mounting to the global root for now.
    match crate::filesystem::Filesystem::mount(src_path) {
        Ok(_) => 0,
        Err(_) => u64::MAX,
    }
}

pub fn sys_fmap(fd: u64, offset: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
            Ok(phys) => phys as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        u64::MAX
    }
}

pub fn sys_stat(path_ptr: u64, stat_ptr: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 1023);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..len]).unwrap_or("");
    
    if !is_user_pointer(stat_ptr, core::mem::size_of::<crate::scheme::Stat>() as u64) {
        return linux_abi_error(14);
    }

    let mut s = crate::scheme::Stat::default();
    match crate::scheme::stat(path, &mut s) {
        Ok(_) => {
            let out = unsafe {
                core::slice::from_raw_parts(&s as *const crate::scheme::Stat as *const u8, core::mem::size_of::<crate::scheme::Stat>())
            };
            if !copy_to_user(stat_ptr, out) { return linux_abi_error(14); }
            0
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_fstat(fd: u64, stat_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(stat_ptr, core::mem::size_of::<crate::scheme::Stat>() as u64) {
            return linux_abi_error(14);
        }
        let mut s = crate::scheme::Stat::default();
        match crate::scheme::fstat(fd_entry.scheme_id, fd_entry.resource_id, &mut s) {
            Ok(_) => {
                let out = unsafe {
                    core::slice::from_raw_parts(&s as *const crate::scheme::Stat as *const u8, core::mem::size_of::<crate::scheme::Stat>())
                };
                if !copy_to_user(stat_ptr, out) { return linux_abi_error(14); }
                0
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fstatat(_dfd: u64, path_ptr: u64, stat_ptr: u64, _flags: u64) -> u64 {
    sys_stat(path_ptr, stat_ptr)
}

pub fn sys_sync() -> u64 {
    0
}

pub fn sys_truncate(path_ptr: u64, len: u64) -> u64 {
    let path_len = super::strlen_user_unique(path_ptr, 1023);
    if path_len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..path_len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..path_len]).unwrap_or("");

    match crate::scheme::open(path, 0x02, 0) { // O_RDWR
        Ok((sid, rid)) => {
            let res = match crate::scheme::ftruncate(sid, rid, len as usize) {
                Ok(_) => 0,
                Err(e) => (-(e as isize)) as u64,
            };
            let _ = crate::scheme::close(sid, rid);
            res
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_creat(path_ptr: u64, mode: u64) -> u64 {
    sys_open(path_ptr, 0x40 | 0x01 | 0x08, mode) // O_CREAT|O_WRONLY|O_TRUNC
}

pub fn sys_link(old_ptr: u64, new_ptr: u64) -> u64 {
    // For now, no hardlinks across schemes
    linux_abi_error(38)
}

pub fn sys_mkdirat(dfd: u64, path_ptr: u64, mode: u64) -> u64 {
    // Simple implementation: ignore dfd if path is absolute or AT_FDCWD
    sys_mkdir(path_ptr, mode)
}

pub fn sys_symlink(_old: u64, _new: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_renameat2(old_dfd: u64, old_ptr: u64, new_dfd: u64, new_ptr: u64, _flags: u64) -> u64 {
    sys_rename(old_ptr, new_ptr)
}

pub fn sys_readlink(path_ptr: u64, buf_ptr: u64, len: u64) -> u64 {
    let path_len = super::strlen_user_unique(path_ptr, 1023);
    if path_len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 1024];
    if !super::copy_from_user(path_ptr, &mut path_buf[..path_len]) { return linux_abi_error(14); }
    let path = core::str::from_utf8(&path_buf[..path_len]).unwrap_or("");

    match crate::scheme::readlink(path, len as usize) {
        Ok(target) => {
            let target_bytes = target.as_bytes();
            let n = target_bytes.len().min(len as usize);
            if !super::copy_to_user(buf_ptr, &target_bytes[..n]) {
                return linux_abi_error(14);
            }
            n as u64
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_chmod(_path: u64, _mode: u64) -> u64 { 0 }
pub fn sys_fchmod(_fd: u64, _mode: u64) -> u64 { 0 }
pub fn sys_chown(_path: u64, _owner: u64, _group: u64) -> u64 { 0 }
pub fn sys_fchown(_fd: u64, _owner: u64, _group: u64) -> u64 { 0 }
pub fn sys_lchown(_path: u64, _owner: u64, _group: u64) -> u64 { 0 }

pub fn sys_umask(mask: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let mut old_mask = 0;
    let _ = crate::process::modify_process(pid, |p| {
        let mut proc = p.proc.lock();
        old_mask = proc.umask;
        proc.umask = (mask & 0o777) as u32;
    });
    old_mask as u64
}

pub fn sys_getdents64(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::getdents(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(entries) => {
                let mut pos = 0;
                for name in entries {
                    let name_bytes = name.as_bytes();
                    let reclen = (8 + 8 + 2 + 1 + name_bytes.len() + 1 + 7) & !7;
                    if pos + reclen > len as usize { break; }
                    
                    let mut entry_buf = [0u8; 256];
                    unsafe {
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr() as *mut u64, 1);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(8) as *mut u64, 0);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(16) as *mut u16, reclen as u16);
                        core::ptr::write_unaligned(entry_buf.as_mut_ptr().add(18) as *mut u8, 8);
                    }
                    entry_buf[19..19+name_bytes.len()].copy_from_slice(name_bytes);
                    entry_buf[19+name_bytes.len()] = 0;
                    
                    if !super::copy_to_user(buf_ptr + pos as u64, &entry_buf[..reclen]) {
                        return linux_abi_error(14);
                    }
                    pos += reclen;
                }
                pos as u64
            }
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_getcwd(buf_ptr: u64, len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(p) = crate::process::get_process(pid) {
        let proc = p.proc.lock();
        let cw_len = proc.cwd_len;
        if len < (cw_len + 1) as u64 { return linux_abi_error(34); } // ERANGE
        if !super::copy_to_user(buf_ptr, &proc.cwd[..cw_len]) { return linux_abi_error(14); }
        if !super::copy_to_user(buf_ptr + cw_len as u64, &[0]) { return linux_abi_error(14); }
        buf_ptr
    } else {
        u64::MAX
    }
}

pub fn sys_chdir(path_ptr: u64) -> u64 {
    let len = super::strlen_user_unique(path_ptr, 127);
    if len == 0 { return linux_abi_error(2); }
    let mut path_buf = [0u8; 128];
    if !super::copy_from_user(path_ptr, &mut path_buf[..len]) { return linux_abi_error(14); }
    
    let pid = current_process_id().unwrap_or(0);
    let _ = crate::process::modify_process(pid, |p| {
        let mut proc = p.proc.lock();
        let actual_len = len.min(127);
        proc.cwd[..actual_len].copy_from_slice(&path_buf[..actual_len]);
        proc.cwd[actual_len] = 0;
        proc.cwd_len = actual_len;
    });
    0
}

pub fn sys_fchdir(_fd: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_fcntl(fd: u64, cmd: u64, _arg: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match cmd {
            0 => { // F_DUPFD
                crate::fd::fd_create(pid, fd_entry.scheme_id, fd_entry.resource_id).unwrap_or(0) as u64
            }
            1 => 0, // F_GETFD
            2 => 0, // F_SETFD
            3 => 0, // F_GETFL
            4 => 0, // F_SETFL
            _ => 0,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_flock(fd: u64, operation: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::flock(fd_entry.scheme_id, fd_entry.resource_id, operation as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fsync(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::fsync(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_fdatasync(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::fdatasync(fd_entry.scheme_id, fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_memfd_create(name_ptr: u64, _flags: u64) -> u64 {
    let mut name_buf = [0u8; 32];
    let mut name = "memfd";
    if name_ptr != 0 {
        if super::copy_from_user(name_ptr, &mut name_buf) {
             if let Ok(n) = core::str::from_utf8(&name_buf) {
                 name = n.trim_matches(char::from(0));
             }
        }
    }

    let pid = current_process_id().unwrap_or(0);
    let unique_name = format!("memfd:{}:{}", pid, name);
    let path = format!("shm:{}", unique_name);
    
    // O_CREAT (0x40) | O_RDWR (0x02)
    match crate::scheme::open(&path, 0x40 | 0x02, 0o666) {
        Ok((sid, rid)) => {
            // Unlink immediately so it's anonymous
            let _ = crate::scheme::unlink(&path);
            crate::fd::fd_create(pid, sid, rid).unwrap_or(usize::MAX) as u64
        }
        Err(e) => (-(e as isize)) as u64,
    }
}

pub fn sys_dup(fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        crate::fd::fd_create(pid, fd_entry.scheme_id, fd_entry.resource_id).unwrap_or(0) as u64
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_dup2(old_fd: u64, new_fd: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, old_fd as usize) {
        crate::fd::fd_replace(pid, new_fd as usize, fd_entry.scheme_id, fd_entry.resource_id, fd_entry.flags);
        new_fd
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_dup3(old_fd: u64, new_fd: u64, _flags: u64) -> u64 {
    sys_dup2(old_fd, new_fd)
}

