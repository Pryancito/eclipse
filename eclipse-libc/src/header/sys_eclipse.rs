//! sys/eclipse.rs - Eclipse OS specific extensions
use crate::types::*;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub uptime_ticks: u64,
    pub idle_ticks: u64,
    pub total_mem_frames: u64,
    pub used_mem_frames: u64,
}

#[no_mangle]
pub unsafe extern "C" fn get_system_stats(stats: *mut SystemStats) -> c_int {
    if stats.is_null() {
        return -1;
    }
    
    let res = eclipse_syscall::syscall1(
        eclipse_syscall::number::SYS_GET_SYSTEM_STATS,
        stats as usize
    );
    
    if res == 0 {
        0
    } else {
        -1
    }
}

pub use eclipse_syscall::ProcessInfo;

#[no_mangle]
pub unsafe extern "C" fn get_process_list(buf: *mut ProcessInfo, max_count: usize) -> isize {

    if buf.is_null() {
        return -1;
    }
    
    let res = eclipse_syscall::syscall2(
        eclipse_syscall::number::SYS_GET_PROCESS_LIST,
        buf as usize,
        max_count
    );
    
    res as isize
}

#[no_mangle]
pub unsafe extern "C" fn eclipse_kill(pid: u32) -> c_int {
    let res = eclipse_syscall::syscall1(
        eclipse_syscall::number::SYS_KILL,
        pid as usize
    );
    
    if res == 0 {
        0
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn set_process_name(name: *const c_char) -> c_int {
    if name.is_null() {
        return -1;
    }
    
    let res = eclipse_syscall::syscall1(
        eclipse_syscall::number::SYS_SET_PROCESS_NAME,
        name as usize
    );
    
    if res == 0 {
        0
    } else {
        -1
    }
}


