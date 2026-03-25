//! sys/eclipse.rs - Eclipse OS specific extensions
use crate::types::*;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub uptime_ticks: u64,
    pub idle_ticks: u64,
    pub total_mem_frames: u64,
    pub used_mem_frames: u64,
    pub cpu_count: u64,
    // AI-CORE Vitals
    pub cpu_temp: [u32; 16],
    pub gpu_load: [u32; 4],
    pub gpu_temp: [u32; 4],
    pub gpu_vram_total_bytes: u64,
    pub gpu_vram_used_bytes: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
    pub wall_time_offset: u64,
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

#[no_mangle]
pub unsafe extern "C" fn set_time(secs: u64) -> c_int {
    let res = eclipse_syscall::syscall1(eclipse_syscall::number::SYS_SET_TIME, secs as usize);
    if res == 0 { 0 } else { -1 }
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

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferInfo {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u16,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[no_mangle]
pub unsafe extern "C" fn get_framebuffer_info() -> core::option::Option<FramebufferInfo> {
    let mut fb_info = FramebufferInfo::default();
    let res = eclipse_syscall::syscall1(
        eclipse_syscall::number::SYS_GET_FRAMEBUFFER_INFO,
        &mut fb_info as *mut _ as usize
    );
    if res == 0 { core::option::Option::Some(fb_info) } else { core::option::Option::None }
}

#[no_mangle]
pub unsafe extern "C" fn map_framebuffer() -> core::option::Option<usize> {
    let res = eclipse_syscall::syscall0(eclipse_syscall::number::SYS_MAP_FRAMEBUFFER);
    if res != 0 { core::option::Option::Some(res as usize) } else { core::option::Option::None }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GpuDisplayBufferInfo {
    pub vaddr: u64,
    pub resource_id: u32,
    pub pitch: u32,
    pub size: u64,
}

#[no_mangle]
pub unsafe extern "C" fn get_gpu_display_info(out: *mut [u32; 2]) -> bool {
    let res = eclipse_syscall::syscall1(
        eclipse_syscall::number::SYS_GET_GPU_DISPLAY_INFO,
        out as usize
    );
    res != usize::MAX
}

#[no_mangle]
pub unsafe extern "C" fn gpu_alloc_display_buffer(width: u32, height: u32) -> core::option::Option<GpuDisplayBufferInfo> {
    let mut out = GpuDisplayBufferInfo::default();
    let res = eclipse_syscall::syscall3(
        eclipse_syscall::number::SYS_GPU_ALLOC_DISPLAY_BUFFER,
        width as usize,
        height as usize,
        &mut out as *mut _ as usize
    );
    if res == 0 { core::option::Option::Some(out) } else { core::option::Option::None }
}

#[no_mangle]
pub unsafe extern "C" fn gpu_present(resource_id: u32, x: u32, y: u32, w: u32, h: u32) -> bool {
    let res = eclipse_syscall::syscall5(
        eclipse_syscall::number::SYS_GPU_PRESENT,
        resource_id as usize,
        x as usize,
        y as usize,
        w as usize,
        h as usize
    );
    res != usize::MAX
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    pub device_id: u32,
    pub event_type: u8,  // 0=key, 1=mouse_move, 2=mouse_button, 3=mouse_scroll
    pub code: u16,
    pub value: i32,
    pub timestamp: u64,
}

#[no_mangle]
pub unsafe extern "C" fn eclipse_send(target: u32, msg_type: u32, data: *const c_void, len: size_t, _flags: i32) -> isize {
    // In Eclipse OS, SYS_SEND (3) is for IPC
    let res = eclipse_syscall::syscall4(
        eclipse_syscall::number::SYS_SEND,
        target as usize,
        msg_type as usize,
        data as usize,
        len
    );
    res as isize
}

#[no_mangle]
pub unsafe extern "C" fn receive(buffer: *mut u8, len: size_t, sender_pid: *mut u32) -> usize {
    let mut pid_temp: u64 = 0;
    let res = eclipse_syscall::syscall3(
        eclipse_syscall::number::SYS_RECEIVE,
        buffer as usize,
        len,
        &mut pid_temp as *mut _ as usize
    );
    if !sender_pid.is_null() {
        *sender_pid = pid_temp as u32;
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn get_logs(buf: *mut u8, len: size_t) -> usize {
    let res = eclipse_syscall::syscall2(
        eclipse_syscall::number::SYS_GET_LOGS,
        buf as usize,
        len
    );
    res
}

#[no_mangle]
pub fn receive_fast() -> core::option::Option<([u8; 24], u32, usize)> {
    let size: usize;
    let w0: usize;
    let w1: usize;
    let w2: usize;
    let from: usize;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") eclipse_syscall::number::SYS_RECEIVE_FAST => size,
            lateout("rdi") w0,
            lateout("rsi") w1,
            lateout("rdx") w2,
            lateout("rcx") from,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
            options(nostack),
        );
    }
    if size > 0 {
        let mut data = [0u8; 24];
        data[0..8].copy_from_slice(&w0.to_le_bytes());
        data[8..16].copy_from_slice(&w1.to_le_bytes());
        data[16..24].copy_from_slice(&w2.to_le_bytes());
        core::option::Option::Some((data, from as u32, size))
    } else {
        core::option::Option::None
    }
}

pub fn sleep_ms(ms: u64) {
    if ms == 0 {
        unsafe { eclipse_syscall::syscall0(eclipse_syscall::number::SYS_YIELD) };
        return;
    }
    let ts: [i64; 2] = [(ms / 1000) as i64, ((ms % 1000) * 1_000_000) as i64];
    unsafe {
        eclipse_syscall::syscall1(
            eclipse_syscall::number::SYS_NANOSLEEP,
            ts.as_ptr() as usize
        );
    }
}

#[no_mangle]
pub unsafe extern "C" fn yield_cpu() {
    let _ = eclipse_syscall::syscall0(eclipse_syscall::number::SYS_YIELD);
}

pub fn get_service_binary(service_id: u32) -> (*const u8, usize) {
    let mut ptr: u64 = 0;
    let mut size: u64 = 0;
    let res = unsafe {
        eclipse_syscall::syscall3(
            eclipse_syscall::number::SYS_GET_SERVICE_BINARY,
            service_id as usize,
            &mut ptr as *mut u64 as usize,
            &mut size as *mut u64 as usize
        )
    };
    if res == 0 {
        (ptr as *const u8, size as usize)
    } else {
        (core::ptr::null(), 0)
    }
}

pub fn exec(binary: &[u8]) -> i32 {
    unsafe {
        eclipse_syscall::syscall2(
            eclipse_syscall::number::SYS_EXEC,
            binary.as_ptr() as usize,
            binary.len()
        ) as i32
    }
}

pub fn gpu_command(kind: usize, command: usize, payload: &[u8]) -> isize {
    unsafe {
        eclipse_syscall::syscall4(
            eclipse_syscall::number::SYS_GPU_COMMAND,
            kind,
            command,
            payload.as_ptr() as usize,
            payload.len()
        ) as isize
    }
}
