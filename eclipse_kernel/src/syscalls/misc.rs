//! Syscalls diversos para Eclipse OS
//! Tiempo, información del sistema, hardware y depuración.

use super::*;
use core::arch::asm;

#[repr(C)]
pub struct SystemStats {
    pub uptime_ticks: u64,
    pub idle_ticks: u64,
    pub total_mem_frames: u64,
    pub used_mem_frames: u64,
    pub cpu_count: u32,
    pub cpu_temp: [i32; 16],
    pub gpu_load: [u8; 4],
    pub gpu_temp: [i8; 4],
    pub gpu_vram_total_bytes: u64,
    pub gpu_vram_used_bytes: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u8,
    pub wall_time_offset: u64,
}

pub fn sys_get_ticks() -> u64 {
    crate::interrupts::ticks()
}

pub fn sys_nanosleep(req_ptr: u64, _rem_ptr: u64) -> u64 {
    if !is_user_pointer(req_ptr, 16) { return super::linux_abi_error(14); }
    let mut ts = [0u64; 2];
    super::copy_from_user(req_ptr, unsafe { core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, 16) });
    let ms = ts[0] * 1000 + ts[1] / 1_000_000;
    super::process_sleep_ms(ms)
}

pub fn sys_gettimeofday(tv_ptr: u64, _tz_ptr: u64) -> u64 {
    if !is_user_pointer(tv_ptr, 16) { return super::linux_abi_error(14); }
    let uptime_ms = crate::interrupts::ticks();
    let offset = WALL_TIME_OFFSET.load(Ordering::Relaxed);
    let sec = offset + (uptime_ms / 1000);
    let usec = (uptime_ms % 1000) * 1000;
    let tv = [sec, usec];
    super::copy_to_user(tv_ptr, unsafe { core::slice::from_raw_parts(tv.as_ptr() as *const u8, 16) });
    0
}

pub fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> u64 {
    if !is_user_pointer(tp_ptr, 16) { return super::linux_abi_error(14); }
    let uptime_ms = crate::interrupts::ticks();
    let offset = WALL_TIME_OFFSET.load(Ordering::Relaxed);
    let (sec, nsec) = match clk_id {
        0 | 1 | 4 => (offset + (uptime_ms / 1000), (uptime_ms % 1000) * 1_000_000),
        _ => return super::linux_abi_error(22),
    };
    let tp = [sec, nsec];
    super::copy_to_user(tp_ptr, unsafe { core::slice::from_raw_parts(tp.as_ptr() as *const u8, 16) });
    0
}

pub fn sys_uname(buf_ptr: u64) -> u64 {
    if !is_user_pointer(buf_ptr, 65 * 6) { return super::linux_abi_error(14); }
    let mut buf = [0u8; 65 * 6];
    let sysname = b"EclipseOS";
    let nodename = b"eclipse";
    let release = b"0.2.0";
    let version = b"v3-modular";
    let machine = b"x86_64";
    
    buf[0..sysname.len()].copy_from_slice(sysname);
    buf[65..65+nodename.len()].copy_from_slice(nodename);
    buf[130..130+release.len()].copy_from_slice(release);
    buf[195..195+version.len()].copy_from_slice(version);
    buf[260..260+machine.len()].copy_from_slice(machine);
    
    super::copy_to_user(buf_ptr, &buf);
    0
}

pub fn sys_getrandom(buf_ptr: u64, len: u64, _flags: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return super::linux_abi_error(14); }
    let mut bounce = alloc::vec![0u8; len as usize];
    if has_rdrand() {
        fill_random_rdrand(&mut bounce);
    } else {
        fill_random_rdtsc(&mut bounce);
    }
    super::copy_to_user(buf_ptr, &bounce);
    len
}

fn has_rdrand() -> bool {
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) _,
            inout("eax") 1 => _,
            out("ecx") ecx,
            out("edx") _,
        );
    }
    (ecx & (1 << 30)) != 0
}

fn fill_random_rdrand(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let mut val: u64 = 0;
        unsafe {
            asm!("rdrand {}", out(reg) val);
        }
        let n = chunk.len();
        chunk.copy_from_slice(&val.to_le_bytes()[..n]);
    }
}

fn fill_random_rdtsc(buf: &mut [u8]) {
    let mut seed = crate::interrupts::ticks();
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (seed >> 32) as u8;
    }
}

pub fn sys_get_last_exec_error(out_ptr: u64, out_len: u64) -> u64 {
    let buf = LAST_EXEC_ERR.lock();
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let copy_len = core::cmp::min(n, out_len as usize);
    super::copy_to_user(out_ptr, &buf[..copy_len]);
    copy_len as u64
}

pub fn sys_read_key() -> u64 {
    crate::interrupts::read_key() as u64
}

pub fn sys_read_mouse_packet() -> u64 {
    crate::interrupts::read_mouse_packet() as u64
}

pub fn sys_get_system_stats(stats_ptr: u64) -> u64 {
    if !is_user_pointer(stats_ptr, core::mem::size_of::<SystemStats>() as u64) { return u64::MAX; }
    let stats = SystemStats {
        uptime_ticks: crate::interrupts::ticks(),
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
        cpu_count: 1,
        cpu_temp: [0; 16],
        gpu_load: [0; 4],
        gpu_temp: [0; 4],
        gpu_vram_total_bytes: 0,
        gpu_vram_used_bytes: 0,
        anomaly_count: 0,
        heap_fragmentation: 0,
        wall_time_offset: WALL_TIME_OFFSET.load(Ordering::Relaxed),
    };
    super::copy_to_user(stats_ptr, unsafe { core::slice::from_raw_parts(&stats as *const _ as *const u8, core::mem::size_of::<SystemStats>()) });
    0
}
