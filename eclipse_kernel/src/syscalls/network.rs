//! Syscalls de red y sockets para Eclipse OS
//! Implementa la interfaz de sockets de Linux (AF_INET, AF_UNIX) sobre los esquemas del kernel.

use super::*;
use alloc::format;
use alloc::string::String;

pub fn sys_socket(domain: u64, stype: u64, protocol: u64) -> u64 {
    let path = format!("socket:{}/{}/{}", domain, stype, protocol);
    match crate::scheme::open(&path, 0, 0) {
        Ok((scheme_id, resource_id)) => {
            if let Some(pid) = current_process_id() {
                if let Some(fd) = crate::fd::fd_open(pid, scheme_id, resource_id, 0) {
                    return fd as u64;
                }
            }
        }
        Err(e) => {
            return linux_abi_error(e as i32);
        }
    }
    linux_abi_error(12) // ENOMEM
}

pub fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addr == 0 || addrlen < 2 || !is_user_pointer(addr, addrlen) {
        return linux_abi_error(22); // EINVAL/EFAULT
    }
    
    let family = unsafe { *(addr as *const u16) };
    
    if family == 1 { // AF_UNIX
        let path_start = addr + 2;
        let mut path_len = strlen_user_unique(path_start, (addrlen - 2) as usize);
        
        let is_abstract = if path_len == 0 && addrlen > 2 {
            let first_byte = unsafe { *(path_start as *const u8) };
            first_byte == 0
        } else {
            false
        };

        let mut path_buf = [0u8; 110];
        let mut final_path_str = String::new();

        if is_abstract {
            final_path_str.push('@');
            path_len = (addrlen - 2).min(107);
            copy_from_user(path_start + 1, &mut path_buf[.. (path_len - 1) as usize]);
            if let Ok(s) = core::str::from_utf8(&path_buf[0..(path_len - 1) as usize]) {
                final_path_str.push_str(s);
            }
        } else {
            path_len = path_len.min(107);
            copy_from_user(path_start, &mut path_buf[.. path_len as usize]);
            if let Ok(s) = core::str::from_utf8(&path_buf[0..path_len as usize]) {
                final_path_str.push_str(s);
            }
        }
        
        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                scheme.bind(fd_info.resource_id, final_path_str).ok();
                return 0;
            }
        }
    } else if family == 2 { // AF_INET
        let mut ip = [0u8; 4];
        copy_from_user(addr + 4, &mut ip);
        let port = unsafe { u16::from_be(*( (addr + 2) as *const u16 )) };
        let path = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

        if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
            if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
                if scheme.bind(fd_info.resource_id, path).is_ok() {
                    return 0;
                }
            }
        }
    }
    
    linux_abi_error(22)
}

pub fn sys_listen(fd: u64, _backlog: u64) -> u64 {
    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.listen(fd_info.resource_id) {
                Ok(_) => return 0,
                Err(e) => return linux_abi_error(e as i32),
            }
        }
    }
    linux_abi_error(9) // EBADF
}

pub fn sys_accept(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.accept(fd_info.resource_id) {
                Ok(new_res_id) => {
                    if let Some(new_fd) = crate::fd::fd_open(pid, fd_info.scheme_id, new_res_id, 0) {
                        // TODO: copy address to user
                        let _ = (addr, addrlen);
                        return new_fd as u64;
                    }
                }
                Err(e) => return linux_abi_error(e as i32),
            }
        }
    }
    linux_abi_error(9)
}

pub fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    if addr == 0 || addrlen < 2 || !is_user_pointer(addr, addrlen) {
        return linux_abi_error(22);
    }
    let family = unsafe { *(addr as *const u16) };
    let path = if family == 1 {
        let path_start = addr + 2;
        let path_len = strlen_user_unique(path_start, (addrlen - 2) as usize).min(107);
        let mut path_buf = [0u8; 110];
        copy_from_user(path_start, &mut path_buf[.. path_len as usize]);
        alloc::string::String::from(core::str::from_utf8(&path_buf[.. path_len as usize]).unwrap_or(""))
    } else if family == 2 {
        let mut ip = [0u8; 4];
        copy_from_user(addr + 4, &mut ip);
        let port = unsafe { u16::from_be(*( (addr + 2) as *const u16 )) };
        format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port)
    } else {
        return linux_abi_error(22);
    };

    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            match scheme.connect(fd_info.resource_id, path.as_str()) {
                Ok(_) => return 0,
                Err(e) => return linux_abi_error(e as i32),
            }
        }
    }
    linux_abi_error(9)
}

pub fn sys_sendto(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> u64 {
    if buf == 0 || len == 0 || !is_user_pointer(buf, len) { return linux_abi_error(22); }
    let mut bounce = [0u8; 2048];
    let copy_len = len.min(2048) as usize;
    copy_from_user(buf, &mut bounce[..copy_len]);

    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            let _ = (flags, addr, addrlen);
            match crate::scheme::write(fd_info.scheme_id, fd_info.resource_id, &bounce[..copy_len], 0) {
                Ok(n) => return n as u64,
                Err(e) => return linux_abi_error(e as i32),
            }
        }
    }
    linux_abi_error(9)
}

pub fn sys_recvfrom(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> u64 {
    if buf == 0 || len == 0 || !is_user_pointer(buf, len) { return linux_abi_error(22); }
    let mut bounce = [0u8; 2048];
    let copy_len = len.min(2048) as usize;

    if let (Some(pid), Some(scheme)) = (current_process_id(), crate::servers::get_socket_scheme()) {
        if let Some(fd_info) = crate::fd::fd_get(pid, fd as usize) {
            let _ = (flags, addr, addrlen);
            match crate::scheme::read(fd_info.scheme_id, fd_info.resource_id, &mut bounce[..copy_len], 0) {
                Ok(n) => {
                    copy_to_user(buf, &bounce[..n]);
                    return n as u64;
                }
                Err(e) => return linux_abi_error(e as i32),
            }
        }
    }
    linux_abi_error(9)
}

pub fn sys_sendmsg(fd: u64, msg: u64, flags: u64) -> u64 {
    // Basic msghdr stub
    if msg == 0 || !is_user_pointer(msg, 16) { return linux_abi_error(22); }
    let iov_ptr = unsafe { *(msg as *const u64) };
    let iov_cnt = unsafe { *((msg + 8) as *const u64) };
    if iov_ptr == 0 || iov_cnt == 0 { return 0; }
    
    let mut total = 0u64;
    for i in 0..iov_cnt {
        let base = unsafe { *((iov_ptr + i*16) as *const u64) };
        let len = unsafe { *((iov_ptr + i*16 + 8) as *const u64) };
        let n = sys_sendto(fd, base, len, flags, 0, 0);
        if n == u64::MAX { break; }
        total += n;
    }
    total
}

pub fn sys_recvmsg(fd: u64, msg: u64, flags: u64) -> u64 {
    if msg == 0 || !is_user_pointer(msg, 16) { return linux_abi_error(22); }
    let iov_ptr = unsafe { *(msg as *const u64) };
    let iov_cnt = unsafe { *((msg + 8) as *const u64) };
    if iov_ptr == 0 || iov_cnt == 0 { return 0; }

    let base = unsafe { *(iov_ptr as *const u64) };
    let len = unsafe { *((iov_ptr + 8) as *const u64) };
    sys_recvfrom(fd, base, len, flags, 0, 0)
}

pub fn sys_shutdown(fd: u64, _how: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        crate::fd::fd_close(pid, fd as usize);
        return 0;
    }
    linux_abi_error(9)
}

pub fn sys_getsockname(_fd: u64, _addr: u64, _addrlen: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_getpeername(_fd: u64, _addr: u64, _addrlen: u64) -> u64 {
    linux_abi_error(38)
}

pub fn sys_setsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64) -> u64 {
    0 // Stub success
}

pub fn sys_getsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64) -> u64 {
    0 // Stub success
}

pub fn sys_socketpair(domain: u64, stype: u64, protocol: u64, sv_ptr: u64) -> u64 {
    if sv_ptr == 0 || !is_user_pointer(sv_ptr, 8) { return linux_abi_error(22); }
    let fd1 = sys_socket(domain, stype, protocol);
    let fd2 = sys_socket(domain, stype, protocol);
    if fd1 != u64::MAX && fd2 != u64::MAX {
        let fds = [fd1 as i32, fd2 as i32];
        copy_to_user(sv_ptr, unsafe { core::slice::from_raw_parts(&fds as *const _ as *const u8, 8) });
        return 0;
    }
    linux_abi_error(12)
}
