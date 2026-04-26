//! I/O Multiplexing syscalls implementation
//!
//! Implementation of poll, select, and epoll.

use alloc::format;
use alloc::vec::Vec;
use crate::process::{self, current_process_id};
use super::{copy_from_user, copy_to_user, is_user_pointer, linux_abi_error};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FdSet {
    pub fds_bits: [u64; 16], // 1024 bits
}

#[repr(C)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub fn sys_poll(fds_ptr: u64, nfds: u64, timeout: u64) -> u64 {
    if nfds == 0 {
        if timeout == 0 { return 0; }
        // sleep for timeout ms
        crate::scheduler::sleep(timeout);
        return 0;
    }

    let bytes = match (nfds as usize).checked_mul(core::mem::size_of::<PollFd>()) {
        Some(b) => b as u64,
        None => return linux_abi_error(14),
    };
    if !is_user_pointer(fds_ptr, bytes) {
        return linux_abi_error(14);
    }

    let pid = current_process_id().unwrap_or(0);
    let start_tick = crate::interrupts::ticks();

    loop {
        // Copiar (nfds * PollFd) desde userspace, operar en kernel, y copiar de vuelta.
        let mut fds: Vec<PollFd> = Vec::with_capacity(nfds as usize);
        unsafe { fds.set_len(nfds as usize); }
        let fds_bytes = unsafe {
            core::slice::from_raw_parts_mut(fds.as_mut_ptr() as *mut u8, bytes as usize)
        };
        if !copy_from_user(fds_ptr, fds_bytes) {
            return linux_abi_error(14);
        }

        let mut count = 0;
        for pfd in fds.iter_mut() {
            pfd.revents = 0;
            if pfd.fd < 0 { continue; }

            if let Some(fd_entry) = crate::fd::fd_get(pid, pfd.fd as usize) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, pfd.events as usize) {
                    Ok(res) => {
                        pfd.revents = res as i16;
                        if res != 0 { count += 1; }
                    }
                    Err(_) => {
                        pfd.revents = 0x020; // POLLNVAL
                        count += 1;
                    }
                }
            } else {
                pfd.revents = 0x020; // POLLNVAL
                count += 1;
            }
        }

        // Copiar resultados de vuelta a userspace.
        let out_bytes = unsafe {
            core::slice::from_raw_parts(fds.as_ptr() as *const u8, bytes as usize)
        };
        if !copy_to_user(fds_ptr, out_bytes) {
            return linux_abi_error(14);
        }
        
        if count > 0 { return count as u64; }
        if timeout == 0 { return 0; }
        if timeout != u64::MAX && (crate::interrupts::ticks() - start_tick) >= timeout {
            return 0;
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_select(nfds: u64, read_ptr: u64, write_ptr: u64, except_ptr: u64, timeout_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    
    let timeout_ms = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timeval>() as u64) {
            return linux_abi_error(14);
        }
        let mut tv = core::mem::MaybeUninit::<Timeval>::uninit();
        let tv_bytes = unsafe {
            core::slice::from_raw_parts_mut(tv.as_mut_ptr() as *mut u8, core::mem::size_of::<Timeval>())
        };
        if !copy_from_user(timeout_ptr, tv_bytes) {
            return linux_abi_error(14);
        }
        let tv = unsafe { tv.assume_init() };
        Some(tv.tv_sec as u64 * 1000 + (tv.tv_usec as u64 / 1000))
    } else {
        None
    };
    
    let start_tick = crate::interrupts::ticks();
    
    // Copy sets to kernel
    let mut rset = if read_ptr != 0 {
        if !is_user_pointer(read_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(read_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };
    
    let mut wset = if write_ptr != 0 {
        if !is_user_pointer(write_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(write_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };
    
    let mut eset = if except_ptr != 0 {
        if !is_user_pointer(except_ptr, 128) { return linux_abi_error(14); }
        let mut s = core::mem::MaybeUninit::<FdSet>::uninit();
        let s_bytes = unsafe {
            core::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, core::mem::size_of::<FdSet>())
        };
        if !copy_from_user(except_ptr, s_bytes) { return linux_abi_error(14); }
        Some(unsafe { s.assume_init() })
    } else { None };

    loop {
        let mut ready_count = 0;
        let mut out_rset = FdSet::default();
        let mut out_wset = FdSet::default();
        let mut out_eset = FdSet::default();

        for fd in 0..nfds as usize {
            if fd >= 1024 { break; }
            let word = fd / 64;
            let bit = 1u64 << (fd % 64);
            
            let mut events = 0;
            if rset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLIN; }
            if wset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLOUT; }
            if eset.as_ref().map_or(false, |s| (s.fds_bits[word] & bit) != 0) { events |= crate::scheme::event::POLLERR; }
            
            if events == 0 { continue; }
            
            if let Some(fd_entry) = crate::fd::fd_get(pid, fd) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, events) {
                    Ok(revents) if revents != 0 => {
                        if (revents & crate::scheme::event::POLLIN) != 0 { out_rset.fds_bits[word] |= bit; }
                        if (revents & crate::scheme::event::POLLOUT) != 0 { out_wset.fds_bits[word] |= bit; }
                        if (revents & (crate::scheme::event::POLLERR | crate::scheme::event::POLLHUP)) != 0 { out_eset.fds_bits[word] |= bit; }
                        ready_count += 1;
                    }
                    _ => {}
                }
            }
        }
        
        if ready_count > 0 {
            if rset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_rset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(read_ptr, out) { return linux_abi_error(14); }
            }
            if wset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_wset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(write_ptr, out) { return linux_abi_error(14); }
            }
            if eset.is_some() {
                let out = unsafe {
                    core::slice::from_raw_parts(&out_eset as *const FdSet as *const u8, core::mem::size_of::<FdSet>())
                };
                if !copy_to_user(except_ptr, out) { return linux_abi_error(14); }
            }
            return ready_count as u64;
        }
        
        if let Some(ms) = timeout_ms {
            if ms == 0 { return 0; }
            if (crate::interrupts::ticks() - start_tick) >= ms { return 0; }
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_ppoll(fds_ptr: u64, nfds: u64, timeout_ptr: u64, _sigmask_ptr: u64, _sigsetsize: u64) -> u64 {
    // Simplified ppoll (ignoring sigmask for now)
    let timeout = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timespec>() as u64) {
            return linux_abi_error(14);
        }
        let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
        let ts_bytes = unsafe {
            core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
        };
        if !copy_from_user(timeout_ptr, ts_bytes) { return linux_abi_error(14); }
        let ts = unsafe { ts.assume_init() };
        ts.tv_sec as u64 * 1000 + (ts.tv_nsec as u64 / 1_000_000)
    } else {
        u64::MAX
    };
    sys_poll(fds_ptr, nfds, timeout)
}

pub fn sys_pselect6(nfds: u64, read_ptr: u64, write_ptr: u64, except_ptr: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
    // Simplified pselect6 (ignoring sigmask for now)
    let timeout_ms = if timeout_ptr != 0 {
        if !is_user_pointer(timeout_ptr, core::mem::size_of::<Timespec>() as u64) {
            return linux_abi_error(14);
        }
        let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
        let ts_bytes = unsafe {
            core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
        };
        if !copy_from_user(timeout_ptr, ts_bytes) { return linux_abi_error(14); }
        let ts = unsafe { ts.assume_init() };
        Some(ts.tv_sec as u64 * 1000 + (ts.tv_nsec as u64 / 1_000_000))
    } else {
        None
    };
    
    // We need to convert Timespec to Timeval for sys_select if we want to reuse it,
    // or just implement it here.
    
    // For simplicity, I'll just reuse sys_select logic but with Timespec.
    // (Actual implementation would be a common helper).
    
    // Stub for now:
    sys_select(nfds, read_ptr, write_ptr, except_ptr, 0) // TODO: implement properly
}

pub fn sys_epoll_create1(_flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    match crate::scheme::open("epoll:", 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    let event = if event_ptr != 0 {
        if !is_user_pointer(event_ptr, core::mem::size_of::<crate::epoll::EpollEvent>() as u64) {
            return linux_abi_error(14);
        }
        let mut ev = core::mem::MaybeUninit::<crate::epoll::EpollEvent>::uninit();
        let ev_bytes = unsafe {
            core::slice::from_raw_parts_mut(ev.as_mut_ptr() as *mut u8, core::mem::size_of::<crate::epoll::EpollEvent>())
        };
        if !copy_from_user(event_ptr, ev_bytes) { return linux_abi_error(14); }
        Some(unsafe { ev.assume_init() })
    } else {
        None
    };
    
    let epoll_scheme = crate::epoll::get_epoll_scheme();
    match epoll_scheme.ctl(epfd_entry.resource_id, op as usize, fd as usize, event) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_epoll_wait(epfd: u64, events_ptr: u64, maxevents: u64, timeout: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let epfd_entry = match crate::fd::fd_get(pid, epfd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9), // EBADF
    };
    
    let epoll_scheme = crate::epoll::get_epoll_scheme();
    let watched_fds = match epoll_scheme.get_instance_watched_fds(epfd_entry.resource_id) {
        Some(w) => w,
        None => return linux_abi_error(22), // EINVAL
    };
    
    let start_tick = crate::interrupts::ticks();
    // In Linux, timeout is in ms. -1 means infinite.
    let timeout_ms = if timeout == u64::MAX { None } else { Some(timeout) };

    loop {
        let mut count = 0;
        for (fd, ev_cfg) in &watched_fds {
            if count >= maxevents { break; }
            
            if let Some(fd_entry) = crate::fd::fd_get(pid, *fd) {
                match crate::scheme::poll(fd_entry.scheme_id, fd_entry.resource_id, ev_cfg.events as usize) {
                    Ok(revents) if revents != 0 => {
                        let out_ev = crate::epoll::EpollEvent {
                            events: revents as u32,
                            data: ev_cfg.data,
                        };
                        unsafe {
                            let ptr = (events_ptr as *mut crate::epoll::EpollEvent).add(count as usize);
                            core::ptr::write_unaligned(ptr, out_ev);
                        }
                        count += 1;
                    }
                    _ => {}
                }
            }
        }
        
        if count > 0 { return count as u64; }
        if let Some(ms) = timeout_ms {
            if ms == 0 { return 0; }
            if (crate::interrupts::ticks() - start_tick) >= ms { return 0; }
        }
        
        crate::scheduler::yield_cpu();
    }
}

pub fn sys_timerfd_create(clockid: u64, flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let path = format!("{}/{}", clockid, flags);
    match crate::scheme::open(&format!("timerfd:{}", path), 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_timerfd_settime(fd: u64, flags: u64, new_ptr: u64, old_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };
    
    if !is_user_pointer(new_ptr, core::mem::size_of::<crate::timerfd::Itimerspec>() as u64) {
        return linux_abi_error(14);
    }
    let new_val = unsafe { core::ptr::read_unaligned(new_ptr as *const crate::timerfd::Itimerspec) };
    
    let timerfd_scheme = crate::timerfd::get_timerfd_scheme();
    match timerfd_scheme.settime(fd_entry.resource_id, flags as i32, &new_val, old_ptr) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_timerfd_gettime(fd: u64, cur_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let fd_entry = match crate::fd::fd_get(pid, fd as usize) {
        Some(e) => e,
        None => return linux_abi_error(9),
    };
    
    let timerfd_scheme = crate::timerfd::get_timerfd_scheme();
    match timerfd_scheme.gettime(fd_entry.resource_id, cur_ptr) {
        Ok(_) => 0,
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_eventfd2(initval: u64, flags: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    let path = format!("{}/{}", initval, flags);
    match crate::scheme::open(&format!("eventfd:{}", path), 0, 0) {
        Ok((scheme_id, resource_id)) => {
            match crate::fd::fd_create(pid, scheme_id, resource_id) {
                Some(fd) => fd as u64,
                None => {
                    let _ = crate::scheme::close(scheme_id, resource_id);
                    linux_abi_error(24) // EMFILE
                }
            }
        }
        Err(e) => linux_abi_error(e as i32),
    }
}

pub fn sys_inotify_init1(_flags: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_inotify_add_watch(_fd: u64, _path_ptr: u64, _mask: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_pause() -> u64 {
    let pid = current_process_id().unwrap_or(0);
    loop {
        if crate::process::get_pending_signals(pid) != 0 {
            return linux_abi_error(4); // EINTR
        }
        crate::scheduler::yield_cpu();
    }
}
