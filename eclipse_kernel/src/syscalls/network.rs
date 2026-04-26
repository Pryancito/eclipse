//! Network-related syscalls implementation

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use crate::process::current_process_id;
use super::{linux_abi_error, copy_from_user, copy_to_user, is_user_pointer};

#[repr(C)]
pub struct Msghdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: i32,
}

#[repr(C)]
pub struct Iovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

#[repr(C)]
pub struct Cmsghdr {
    pub cmsg_len: u64,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

pub fn sys_socket(domain: u64, type_: u64, protocol: u64) -> u64 {
    let path = format!("socket:{}/{}/{}", domain, type_, protocol);
    match crate::scheme::open(&path, 0, 0) {
        Ok((sid, rid)) => {
            let pid = current_process_id().unwrap_or(0);
            crate::fd::fd_create(pid, sid, rid).unwrap_or(0) as u64
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_connect(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let mut buf = Vec::with_capacity(addr_len as usize);
            unsafe { buf.set_len(addr_len as usize); }
            if !copy_from_user(addr_ptr, &mut buf) {
                return linux_abi_error(14);
            }
            
            // Extract path for UNIX sockets (skip family)
            let path = if addr_len > 2 {
                let s = String::from_utf8_lossy(&buf[2..]);
                s.trim_matches('\0').to_string()
            } else {
                String::new()
            };

            let socket_scheme = match crate::servers::get_socket_scheme() {
                Some(s) => s,
                None => return linux_abi_error(38),
            };

            match socket_scheme.connect(fd_entry.resource_id, &path) {
                Ok(_) => 0,
                Err(e) => linux_abi_error(e as i32),
            }
        } else { linux_abi_error(9) }
    } else { linux_abi_error(9) }
}

pub fn sys_accept(fd: u64, _addr_ptr: u64, _addr_len_ptr: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            let socket_scheme = match crate::servers::get_socket_scheme() {
                Some(s) => s,
                None => return linux_abi_error(38),
            };

            match socket_scheme.accept(fd_entry.resource_id) {
                Ok(new_rid) => {
                    crate::fd::fd_create(pid, fd_entry.scheme_id, new_rid).unwrap_or(0) as u64
                }
                Err(e) => linux_abi_error(e as i32),
            }
        } else { linux_abi_error(9) }
    } else { linux_abi_error(9) }
}

pub fn sys_sendto(fd: u64, buf_ptr: u64, len: u64, _flags: u64, _dest_ptr: u64, _dest_len: u64) -> u64 {
    super::fs::sys_write(fd, buf_ptr, len)
}

pub fn sys_recvfrom(fd: u64, buf_ptr: u64, len: u64, _flags: u64, _src_ptr: u64, _src_len_ptr: u64) -> u64 {
    super::fs::sys_read(fd, buf_ptr, len)
}

pub fn sys_sendmsg(fd: u64, msg_ptr: u64, _flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };

    if !is_user_pointer(msg_ptr, core::mem::size_of::<Msghdr>() as u64) {
        return linux_abi_error(14);
    }
    let mut msg = core::mem::MaybeUninit::<Msghdr>::uninit();
    let msg_bytes = unsafe {
        core::slice::from_raw_parts_mut(msg.as_mut_ptr() as *mut u8, core::mem::size_of::<Msghdr>())
    };
    if !copy_from_user(msg_ptr, msg_bytes) { return linux_abi_error(14); }
    let msg = unsafe { msg.assume_init() };

    // Accumulate data from iovecs
    let mut data = Vec::new();
    for i in 0..msg.msg_iovlen {
        let iov_ptr = msg.msg_iov + i * core::mem::size_of::<Iovec>() as u64;
        if !is_user_pointer(iov_ptr, core::mem::size_of::<Iovec>() as u64) { break; }
        let mut iov = core::mem::MaybeUninit::<Iovec>::uninit();
        let iov_bytes = unsafe {
            core::slice::from_raw_parts_mut(iov.as_mut_ptr() as *mut u8, core::mem::size_of::<Iovec>())
        };
        if !copy_from_user(iov_ptr, iov_bytes) { return linux_abi_error(14); }
        let iov = unsafe { iov.assume_init() };
        if iov.iov_len > 0 {
            let mut buf = Vec::with_capacity(iov.iov_len as usize);
            unsafe { buf.set_len(iov.iov_len as usize); }
            if copy_from_user(iov.iov_base, &mut buf) {
                data.extend_from_slice(&buf);
            }
        }
    }

    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };

    // Handle control messages (SCM_RIGHTS)
    if msg.msg_control != 0 && msg.msg_controllen >= 16 {
        let mut fds_to_send = Vec::new();
        // Simplified: just read the first cmsghdr
        if !is_user_pointer(msg.msg_control, core::mem::size_of::<Cmsghdr>() as u64) {
            return linux_abi_error(14);
        }
        let mut cmsg = core::mem::MaybeUninit::<Cmsghdr>::uninit();
        let cmsg_bytes = unsafe {
            core::slice::from_raw_parts_mut(cmsg.as_mut_ptr() as *mut u8, core::mem::size_of::<Cmsghdr>())
        };
        if !copy_from_user(msg.msg_control, cmsg_bytes) { return linux_abi_error(14); }
        let cmsg = unsafe { cmsg.assume_init() };
        if cmsg.cmsg_level == 1 && cmsg.cmsg_type == 1 { // SOL_SOCKET, SCM_RIGHTS
            let fd_count = (cmsg.cmsg_len - 16) / 4;
            for i in 0..fd_count {
                let ufd_addr = msg.msg_control + 16 + i * 4;
                if !is_user_pointer(ufd_addr, 4) { return linux_abi_error(14); }
                let mut b = [0u8; 4];
                if !copy_from_user(ufd_addr, &mut b) { return linux_abi_error(14); }
                let ufd = i32::from_le_bytes(b);
                if let Some(entry) = crate::fd::fd_get(pid, ufd as usize) {
                    fds_to_send.push((entry.scheme_id, entry.resource_id));
                }
            }
        }
        if !fds_to_send.is_empty() {
            socket_scheme.socket_enqueue_fds(fd_entry.resource_id, fds_to_send);
        }
    }

    match socket_scheme.socket_write_raw(fd_entry.resource_id, &data) {
        Ok(n) => n as u64,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_recvmsg(fd: u64, msg_ptr: u64, _flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };

    if !is_user_pointer(msg_ptr, core::mem::size_of::<Msghdr>() as u64) {
        return linux_abi_error(14);
    }
    let mut msg = core::mem::MaybeUninit::<Msghdr>::uninit();
    let msg_bytes = unsafe {
        core::slice::from_raw_parts_mut(msg.as_mut_ptr() as *mut u8, core::mem::size_of::<Msghdr>())
    };
    if !copy_from_user(msg_ptr, msg_bytes) { return linux_abi_error(14); }
    let mut msg = unsafe { msg.assume_init() };

    // Calculate total buffer space in iovecs
    let mut total_space = 0;
    let mut iovecs = Vec::new();
    for i in 0..msg.msg_iovlen {
        let iov_ptr = msg.msg_iov + i * core::mem::size_of::<Iovec>() as u64;
        if !is_user_pointer(iov_ptr, core::mem::size_of::<Iovec>() as u64) { break; }
        let mut iov = core::mem::MaybeUninit::<Iovec>::uninit();
        let iov_bytes = unsafe {
            core::slice::from_raw_parts_mut(iov.as_mut_ptr() as *mut u8, core::mem::size_of::<Iovec>())
        };
        if !copy_from_user(iov_ptr, iov_bytes) { return linux_abi_error(14); }
        let iov = unsafe { iov.assume_init() };
        total_space += iov.iov_len;
        iovecs.push(iov);
    }

    let mut data = Vec::with_capacity(total_space as usize);
    unsafe { data.set_len(total_space as usize); }

    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };

    let n = match socket_scheme.socket_read_raw(fd_entry.resource_id, &mut data) {
        Ok(n) => n,
        Err(e) => return linux_abi_error(e as i32),
    };

    // Distribute data back to iovecs
    let mut remaining = n;
    let mut offset = 0;
    for iov in iovecs {
        if remaining == 0 { break; }
        let to_copy = core::cmp::min(remaining as u64, iov.iov_len) as usize;
        if copy_to_user(iov.iov_base, &data[offset..offset+to_copy]) {
            remaining -= to_copy;
            offset += to_copy;
        }
    }

    // Handle delivered FDs
    if msg.msg_control != 0 && msg.msg_controllen >= 16 {
        if let Some(fds) = socket_scheme.socket_dequeue_fds(fd_entry.resource_id) {
            let cmsg = Cmsghdr {
                cmsg_len: (16 + fds.len() * 4) as u64,
                cmsg_level: 1, // SOL_SOCKET
                cmsg_type: 1,  // SCM_RIGHTS
            };
            let out = unsafe {
                core::slice::from_raw_parts(&cmsg as *const Cmsghdr as *const u8, core::mem::size_of::<Cmsghdr>())
            };
            if !copy_to_user(msg.msg_control, out) { return linux_abi_error(14); }
            for (i, (sid, rid)) in fds.iter().enumerate() {
                if let Some(ufd) = crate::fd::fd_create(pid, *sid, *rid) {
                    let addr = msg.msg_control + 16 + i as u64 * 4;
                    let b = (ufd as i32).to_le_bytes();
                    if !copy_to_user(addr, &b) { return linux_abi_error(14); }
                }
            }
        }
    }

    n as u64
}

pub fn sys_shutdown(fd: u64, how: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.shutdown(fd_entry.resource_id, how as i32) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_bind(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        if !copy_from_user(addr_ptr, &mut buf) {
            return linux_abi_error(14);
        }
        
        let path = if addr_len > 2 {
            String::from_utf8_lossy(&buf[2..]).trim_matches('\0').to_string()
        } else {
            String::new()
        };

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.bind(fd_entry.resource_id, path) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_listen(fd: u64, _backlog: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.listen(fd_entry.resource_id) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getsockname(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(addr_len_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(addr_len_ptr, &mut b) { return linux_abi_error(14); }
        let addr_len = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.getsockname(fd_entry.resource_id, &mut buf) {
            Ok(n) => {
                // Prepend family (1 for AF_UNIX)
                if addr_len >= 2 {
                    buf[0] = 1; buf[1] = 0;
                }
                if !copy_to_user(addr_ptr, &buf) { return linux_abi_error(14); }
                let out = ((n + 2) as u32).to_le_bytes();
                if !copy_to_user(addr_len_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getpeername(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(addr_len_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(addr_len_ptr, &mut b) { return linux_abi_error(14); }
        let addr_len = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(addr_len as usize);
        unsafe { buf.set_len(addr_len as usize); }
        
        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };

        match socket_scheme.getpeername(fd_entry.resource_id, &mut buf) {
            Ok(n) => {
                if addr_len >= 2 {
                    buf[0] = 1; buf[1] = 0;
                }
                if !copy_to_user(addr_ptr, &buf) { return linux_abi_error(14); }
                let out = ((n + 2) as u32).to_le_bytes();
                if !copy_to_user(addr_len_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_socketpair(domain: u64, type_: u64, protocol: u64, sv_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let socket_scheme = match crate::servers::get_socket_scheme() {
        Some(s) => s,
        None => return linux_abi_error(38),
    };
    
    match socket_scheme.socketpair(domain as u32, type_ as u32, protocol as u32) {
        Ok((s1, s2)) => {
            let scheme_id = match crate::scheme::get_scheme_id("socket") {
                Some(id) => id,
                None => return linux_abi_error(38),
            };
            
            let fd1 = match crate::fd::fd_create(pid, scheme_id, s1) {
                Some(fd) => fd,
                None => {
                    use crate::scheme::Scheme;
                    let _ = socket_scheme.close(s1);
                    let _ = socket_scheme.close(s2);
                    return linux_abi_error(24);
                }
            };
            let fd2 = match crate::fd::fd_create(pid, scheme_id, s2) {
                Some(fd) => fd,
                None => {
                    use crate::scheme::Scheme;
                    let _ = crate::fd::fd_close(pid, fd1);
                    let _ = socket_scheme.close(s2);
                    return linux_abi_error(24);
                }
            };
            
            if !is_user_pointer(sv_ptr, 8) { return linux_abi_error(14); }
            let b1 = (fd1 as i32).to_le_bytes();
            let b2 = (fd2 as i32).to_le_bytes();
            if !copy_to_user(sv_ptr, &b1) { return linux_abi_error(14); }
            if !copy_to_user(sv_ptr + 4, &b2) { return linux_abi_error(14); }
            0
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_setsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        let mut buf = Vec::with_capacity(optlen as usize);
        unsafe { buf.set_len(optlen as usize); }
        let _ = copy_from_user(optval, &mut buf);

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.setsockopt(fd_entry.resource_id, level as i32, optname as i32, &buf) {
            Ok(_) => 0,
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}

pub fn sys_getsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        if !is_user_pointer(optlen_ptr, 4) { return linux_abi_error(14); }
        let mut b = [0u8; 4];
        if !copy_from_user(optlen_ptr, &mut b) { return linux_abi_error(14); }
        let optlen = u32::from_le_bytes(b);
        
        let mut buf = Vec::with_capacity(optlen as usize);
        unsafe { buf.set_len(optlen as usize); }

        let socket_scheme = match crate::servers::get_socket_scheme() {
            Some(s) => s,
            None => return linux_abi_error(38),
        };
        match socket_scheme.getsockopt(fd_entry.resource_id, level as i32, optname as i32, &mut buf) {
            Ok(n) => {
                if !copy_to_user(optval, &buf) { return linux_abi_error(14); }
                let out = (n as u32).to_le_bytes();
                if !copy_to_user(optlen_ptr, &out) { return linux_abi_error(14); }
                0
            }
            Err(e) => linux_abi_error(e as i32),
        }
    } else { linux_abi_error(9) }
}
