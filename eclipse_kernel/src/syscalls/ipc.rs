//! Syscalls de comunicación entre procesos (IPC) para Eclipse OS
//! Incluye mensajería nativa, tuberías, eventos y sockets de Linux.

use super::*;
use alloc::format;
use crate::ipc::{send_message, receive_message, MessageType};

pub fn sys_send(server_id: u64, msg_type: u64, data_ptr: u64, data_len: u64) -> u64 {
    SYSCALL_STATS.send_calls.fetch_add(1, Ordering::Relaxed);
    if data_len > 0 && data_ptr != 0 && data_ptr < 0x1000 { return u64::MAX; }
    if let Some(client_id) = current_process_id() {
        let message_type = match msg_type {
            1 => MessageType::System,
            255 => MessageType::Signal,
            2 => MessageType::Memory,
            4 => MessageType::FileSystem,
            8 => MessageType::Network,
            0x10 => MessageType::Graphics,
            0x20 => MessageType::Audio,
            0x40 => MessageType::Input,
            _ => MessageType::User,
        };
        let len = core::cmp::min(data_len as usize, 512);
        let mut data = [0u8; 512];
        if len > 0 && data_ptr != 0 {
            if !copy_from_user(data_ptr, &mut data[..len]) { return u64::MAX; }
        }
        if send_message(client_id, server_id as u32, message_type, &data[..len]) { return 0; }
    }
    u64::MAX
}

pub fn sys_receive(buffer_ptr: u64, size: u64, sender_pid_ptr: u64) -> u64 {
    SYSCALL_STATS.receive_calls.fetch_add(1, Ordering::Relaxed);
    if buffer_ptr < 0x1000 || size == 0 || size > 4096 { return u64::MAX; }
    if let Some(client_id) = current_process_id() {
        if let Some(msg) = receive_message(client_id) {
            let data_len = (msg.data_size as usize).min(msg.data.len());
            let copy_len = core::cmp::min(size as usize, data_len);
            if copy_len > 0 { copy_to_user(buffer_ptr, &msg.data[..copy_len]); }
            if sender_pid_ptr != 0 && is_user_pointer(sender_pid_ptr, 4) {
                copy_to_user(sender_pid_ptr, &msg.from.to_ne_bytes());
            }
            return copy_len as u64;
        }
    }
    0
}

pub fn sys_receive_fast(buffer_ptr: u64, size: u64, sender_pid_ptr: u64) -> u64 {
    sys_receive(buffer_ptr, size, sender_pid_ptr)
}

pub fn sys_yield() -> u64 {
    SYSCALL_STATS.yield_calls.fetch_add(1, Ordering::Relaxed);
    yield_cpu();
    0
}

pub fn sys_pause() -> u64 {
    crate::scheduler::yield_cpu();
    0
}

pub fn sys_pipe(pipefd_ptr: u64) -> u64 {
    sys_pipe2(pipefd_ptr, 0)
}

pub fn sys_pipe2(pipefd_ptr: u64, _flags: u64) -> u64 {
    if pipefd_ptr == 0 || !is_user_pointer(pipefd_ptr, 8) { return u64::MAX; }
    let pid = current_process_id().unwrap_or(0);
    let (rh, wh) = crate::pipe::PIPE_SCHEME.new_pipe();
    let sid = crate::scheme::get_scheme_id("pipe").unwrap_or(0);
    if let Some(rfd) = crate::fd::fd_open(pid, sid, rh, 0) {
        if let Some(wfd) = crate::fd::fd_open(pid, sid, wh, 0) {
            let fds = [rfd as i32, wfd as i32];
            copy_to_user(pipefd_ptr, unsafe { core::slice::from_raw_parts(fds.as_ptr() as *const u8, 8) });
            return 0;
        }
    }
    u64::MAX
}

pub fn sys_eventfd2(initval: u64, flags: u64) -> u64 {
    // Stub
    linux_abi_error(38)
}

pub fn sys_socket(domain: u64, stype: u64, protocol: u64) -> u64 {
    crate::syscalls::network::sys_socket(domain, stype, protocol)
}

pub fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_connect(fd, addr, addrlen)
}

pub fn sys_accept(fd: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_accept(fd, addr, addrlen)
}

pub fn sys_sendto(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_sendto(fd, buf, len, flags, addr, addrlen)
}

pub fn sys_recvfrom(fd: u64, buf: u64, len: u64, flags: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_recvfrom(fd, buf, len, flags, addr, addrlen)
}

pub fn sys_sendmsg(fd: u64, msg: u64, flags: u64) -> u64 {
    crate::syscalls::network::sys_sendmsg(fd, msg, flags)
}

pub fn sys_recvmsg(fd: u64, msg: u64, flags: u64) -> u64 {
    crate::syscalls::network::sys_recvmsg(fd, msg, flags)
}

pub fn sys_shutdown(fd: u64, how: u64) -> u64 {
    crate::syscalls::network::sys_shutdown(fd, how)
}

pub fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_bind(fd, addr, addrlen)
}

pub fn sys_listen(fd: u64, backlog: u64) -> u64 {
    crate::syscalls::network::sys_listen(fd, backlog)
}

pub fn sys_getsockname(fd: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_getsockname(fd, addr, addrlen)
}

pub fn sys_getpeername(fd: u64, addr: u64, addrlen: u64) -> u64 {
    crate::syscalls::network::sys_getpeername(fd, addr, addrlen)
}

pub fn sys_setsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    crate::syscalls::network::sys_setsockopt(fd, level, optname, optval, optlen)
}

pub fn sys_getsockopt(fd: u64, level: u64, optname: u64, optval: u64, optlen: u64) -> u64 {
    crate::syscalls::network::sys_getsockopt(fd, level, optname, optval, optlen)
}

pub fn sys_socketpair(domain: u64, stype: u64, protocol: u64, sv_ptr: u64) -> u64 {
    crate::syscalls::network::sys_socketpair(domain, stype, protocol, sv_ptr)
}
