//! Syscalls de sistema de archivos para Eclipse OS
//! Operaciones básicas: read, write, open, close, stat, etc.

use super::*;
use alloc::format;
use alloc::string::String;
use crate::scheme::Scheme;
use crate::filesystem::FileSystemScheme;

pub fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return super::linux_abi_error(14); }
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
            let mut bounce = alloc::vec![0u8; len as usize];
            let offset = fd_entry.offset;
            match crate::scheme::read(fd_entry.scheme_id, fd_entry.resource_id, &mut bounce, offset) {
                Ok(n) => {
                    super::copy_to_user(buf_ptr, &bounce[..n]);
                    crate::fd::fd_update_offset(curr_pid, fd as usize, offset + n as u64);
                    return n as u64;
                }
                Err(e) => return super::linux_abi_error(e as i32),
            }
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return super::linux_abi_error(14); }
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
            let mut bounce = alloc::vec![0u8; len as usize];
            super::copy_from_user(buf_ptr, &mut bounce);
            let offset = fd_entry.offset;
            match crate::scheme::write(fd_entry.scheme_id, fd_entry.resource_id, &bounce, offset) {
                Ok(n) => {
                    crate::fd::fd_update_offset(curr_pid, fd as usize, offset + n as u64);
                    return n as u64;
                },
                Err(e) => return super::linux_abi_error(e as i32),
            }
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_open(path_ptr: u64, flags: u64, mode: u64) -> u64 {
    if path_ptr == 0 { return super::linux_abi_error(22); }
    let path_len = strlen_user_unique(path_ptr, 1024);
    let mut path_buf = alloc::vec![0u8; path_len as usize];
    super::copy_from_user(path_ptr, &mut path_buf);
    let path = core::str::from_utf8(&path_buf).unwrap_or("");
    
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        let resolved = if path.starts_with('/') { String::from(path) }
                       else { crate::process::resolve_path_cwd(curr_pid, path) };
        
        let scheme_path = super::user_path_to_scheme_path(&resolved);
        
        match crate::scheme::open(&scheme_path, flags as usize, mode as u32) {
            Ok((scheme_id, resource_id)) => {
                if let Some(fd) = crate::fd::fd_open(curr_pid, scheme_id, resource_id, flags as u32) {
                    return fd as u64;
                }
                return super::linux_abi_error(24); // EMFILE
            }
            Err(e) => return super::linux_abi_error(e as i32),
        }
    }
    super::linux_abi_error(1)
}

pub fn sys_openat(_dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    sys_open(path_ptr, flags, mode)
}

pub fn sys_close(fd: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if crate::fd::fd_close(curr_pid, fd as usize) { return 0; }
    }
    super::linux_abi_error(9)
}

pub fn sys_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
            match crate::scheme::lseek(fd_entry.scheme_id, fd_entry.resource_id, offset as isize, whence as usize, fd_entry.offset) {
                Ok(n) => {
                    crate::fd::fd_update_offset(curr_pid, fd as usize, n as u64);
                    return n as u64;
                },
                Err(e) => return super::linux_abi_error(e as i32),
            }
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_stat(path_ptr: u64, stat_ptr: u64) -> u64 {
    if !is_user_pointer(stat_ptr, 144) { return super::linux_abi_error(14); }
    let s = LinuxStat::default();
    super::copy_to_user(stat_ptr, unsafe { core::slice::from_raw_parts(&s as *const _ as *const u8, 144) });
    0
}

pub fn sys_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if !is_user_pointer(stat_ptr, 144) { return super::linux_abi_error(14); }
    let s = LinuxStat::default();
    super::copy_to_user(stat_ptr, unsafe { core::slice::from_raw_parts(&s as *const _ as *const u8, 144) });
    0
}

pub fn sys_dup(fd: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(new_fd) = crate::fd::fd_dup(curr_pid, fd as usize) {
            return new_fd as u64;
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_dup2(old_fd: u64, new_fd: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if crate::fd::fd_dup2(curr_pid, old_fd as usize, new_fd as usize) {
            return new_fd;
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_fcntl(_fd: u64, _cmd: u64, _arg: u64) -> u64 {
    0
}

pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
            match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, request as usize, arg as usize) {
                Ok(ret) => return ret as u64,
                Err(e) => return super::linux_abi_error(e as i32),
            }
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_getdents64(_fd: u64, dirp_ptr: u64, count: u64) -> u64 {
    if !is_user_pointer(dirp_ptr, count) { return super::linux_abi_error(14); }
    0
}

pub fn sys_mkdir(path_ptr: u64, mode: u64) -> u64 {
    if path_ptr == 0 { return super::linux_abi_error(22); }
    let path_len = strlen_user_unique(path_ptr, 256);
    let mut path_buf = alloc::vec![0u8; path_len as usize];
    super::copy_from_user(path_ptr, &mut path_buf);
    let path = core::str::from_utf8(&path_buf).unwrap_or("");
    
    match FileSystemScheme.mkdir(path, mode as u32) {
        Ok(_) => 0,
        Err(e) => super::linux_abi_error(e as i32),
    }
}

pub fn sys_rmdir(_path_ptr: u64) -> u64 {
    super::linux_abi_error(38)
}

pub fn sys_unlink(path_ptr: u64) -> u64 {
    if path_ptr == 0 { return super::linux_abi_error(22); }
    let path_len = strlen_user_unique(path_ptr, 256);
    let mut path_buf = alloc::vec![0u8; path_len as usize];
    super::copy_from_user(path_ptr, &mut path_buf);
    let path = core::str::from_utf8(&path_buf).unwrap_or("");
    
    match FileSystemScheme.unlink(path) {
        Ok(_) => 0,
        Err(e) => super::linux_abi_error(e as i32),
    }
}

pub fn sys_ftruncate(fd: u64, length: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
            match crate::scheme::ftruncate(fd_entry.scheme_id, fd_entry.resource_id, length as usize) {
                Ok(ret) => return ret as u64,
                Err(e) => return super::linux_abi_error(e as i32),
            }
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_getcwd(buf_ptr: u64, size: u64) -> u64 {
    if !is_user_pointer(buf_ptr, size) { return super::linux_abi_error(14); }
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        let path = crate::process::get_process_cwd(curr_pid);
        let bytes = path.as_bytes();
        let n = core::cmp::min(bytes.len(), size as usize - 1);
        super::copy_to_user(buf_ptr, &bytes[..n]);
        super::copy_to_user(buf_ptr + n as u64, &[0]);
        return n as u64;
    }
    super::linux_abi_error(1)
}

pub fn sys_chdir(path_ptr: u64) -> u64 {
    if path_ptr == 0 { return super::linux_abi_error(22); }
    let path_len = strlen_user_unique(path_ptr, 256);
    let mut path_buf = alloc::vec![0u8; path_len as usize];
    super::copy_from_user(path_ptr, &mut path_buf);
    let path = core::str::from_utf8(&path_buf).unwrap_or("");
    
    let curr_pid = current_process_id().unwrap_or(0);
    if curr_pid != 0 {
        let resolved = if path.starts_with('/') { String::from(path) }
                       else { crate::process::resolve_path_cwd(curr_pid, path) };
        if crate::process::set_process_cwd(curr_pid, &resolved) { return 0; }
    }
    super::linux_abi_error(2)
}
