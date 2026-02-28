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
