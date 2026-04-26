//! Miscellaneous syscalls implementation
//!
//! Time, system info, and Eclipse-specific management syscalls.

use crate::process::{self, current_process_id};
use super::{copy_from_user, copy_to_user, is_user_pointer, linux_abi_error};
use core::sync::atomic::Ordering;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

static HOSTNAME: Mutex<Option<String>> = Mutex::new(None);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
pub struct Utsname {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

#[repr(C)]
pub struct SysInfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

#[repr(C)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

pub fn sys_yield() -> u64 {
    crate::scheduler::yield_cpu();
    0
}

pub fn sys_nanosleep(req_ptr: u64, rem_ptr: u64) -> u64 {
    if !is_user_pointer(req_ptr, 16) { return linux_abi_error(14); }
    
    let mut ts = core::mem::MaybeUninit::<Timespec>::uninit();
    let ts_bytes = unsafe {
        core::slice::from_raw_parts_mut(ts.as_mut_ptr() as *mut u8, core::mem::size_of::<Timespec>())
    };
    if !copy_from_user(req_ptr, ts_bytes) {
        return linux_abi_error(14);
    }
    let ts = unsafe { ts.assume_init() };
    let ms = ts.tv_sec.saturating_mul(1000).saturating_add(ts.tv_nsec / 1_000_000);
    
    crate::scheduler::sleep(ms as u64);
    
    if rem_ptr != 0 && is_user_pointer(rem_ptr, 16) {
        let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
        let out = unsafe {
            core::slice::from_raw_parts(&zero as *const Timespec as *const u8, core::mem::size_of::<Timespec>())
        };
        if !copy_to_user(rem_ptr, out) {
            return linux_abi_error(14);
        }
    }
    0
}

pub fn sys_gettimeofday(tv_ptr: u64, _tz_ptr: u64) -> u64 {
    if tv_ptr == 0 { return 0; }
    if !is_user_pointer(tv_ptr, core::mem::size_of::<Timeval>() as u64) {
        return linux_abi_error(14);
    }
    // `WALL_TIME_OFFSET` = (Unix time in s) − (uptime in s); ticks ≈ ms de uptime.
    let ticks = crate::interrupts::ticks();
    let wall_off_sec = super::WALL_TIME_OFFSET.load(Ordering::Relaxed);
    let sec = (wall_off_sec + ticks / 1000) as i64;
    let usec = ((ticks % 1000) * 1000) as i64;
    let tv = Timeval { tv_sec: sec, tv_usec: usec };
    let out = unsafe {
        core::slice::from_raw_parts(&tv as *const Timeval as *const u8, core::mem::size_of::<Timeval>())
    };
    if !copy_to_user(tv_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_clock_gettime(clk_id: u64, tp_ptr: u64) -> u64 {
    if !is_user_pointer(tp_ptr, core::mem::size_of::<Timespec>() as u64) {
        return linux_abi_error(14);
    }
    
    let uptime_ms = crate::interrupts::ticks();
    let (sec, nsec) = match clk_id {
        0 => { // CLOCK_REALTIME
            let off = super::WALL_TIME_OFFSET.load(Ordering::Relaxed);
            let s = off + uptime_ms / 1000;
            (s as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
        1 | 4 => { // CLOCK_MONOTONIC / CLOCK_BOOTTIME
            ((uptime_ms / 1000) as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
        _ => { // Otros relojes: monotónico
            ((uptime_ms / 1000) as i64, ((uptime_ms % 1000) * 1_000_000) as i64)
        }
    };
    
    let ts = Timespec { tv_sec: sec, tv_nsec: nsec };
    let out = unsafe {
        core::slice::from_raw_parts(&ts as *const Timespec as *const u8, core::mem::size_of::<Timespec>())
    };
    if !copy_to_user(tp_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_getrlimit(resource: u64, rlim_ptr: u64) -> u64 {
    if !is_user_pointer(rlim_ptr, 16) { return linux_abi_error(14); }
    let limit = match resource {
        7 => Rlimit { rlim_cur: 1024, rlim_max: 4096 }, // RLIMIT_NOFILE
        _ => Rlimit { rlim_cur: u64::MAX, rlim_max: u64::MAX },
    };
    let out = unsafe {
        core::slice::from_raw_parts(&limit as *const Rlimit as *const u8, core::mem::size_of::<Rlimit>())
    };
    if !copy_to_user(rlim_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_getrusage(_who: u64, usage_ptr: u64) -> u64 {
    if !is_user_pointer(usage_ptr, core::mem::size_of::<Rusage>() as u64) {
        return linux_abi_error(14);
    }
    let usage = unsafe { core::mem::zeroed::<Rusage>() };
    let out = unsafe {
        core::slice::from_raw_parts(&usage as *const Rusage as *const u8, core::mem::size_of::<Rusage>())
    };
    if !copy_to_user(usage_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_sysinfo(info_ptr: u64) -> u64 {
    if !is_user_pointer(info_ptr, core::mem::size_of::<SysInfo>() as u64) {
        return linux_abi_error(14);
    }
    
    let (total_frames, used_frames) = crate::memory::get_memory_stats();
    let sched_stats = crate::scheduler::get_stats();
    
    let info = SysInfo {
        uptime: (sched_stats.total_ticks / 1000) as i64,
        loads: [0, 0, 0],
        totalram: total_frames * 4096,
        freeram: total_frames.saturating_sub(used_frames) * 4096,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: crate::process::process_count() as u16,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
    };
    
    let out = unsafe {
        core::slice::from_raw_parts(&info as *const SysInfo as *const u8, core::mem::size_of::<SysInfo>())
    };
    if !copy_to_user(info_ptr, out) { return linux_abi_error(14); }
    0
}

pub fn sys_uname(buf_ptr: u64) -> u64 {
    if !is_user_pointer(buf_ptr, 390) { return linux_abi_error(14); }
    
    let mut uts = Utsname {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
        domainname: [0; 65],
    };
    
    fill_uts_buf(&mut uts.sysname, "Eclipse");
    {
        let h = HOSTNAME.lock();
        fill_uts_buf(&mut uts.nodename, h.as_deref().unwrap_or("eclipse"));
    }
    fill_uts_buf(&mut uts.release, "3.0.0-eclipse");
    fill_uts_buf(&mut uts.version, "#1 SMP Eclipse Microkernel");
    fill_uts_buf(&mut uts.machine, "x86_64");
    
    let out = unsafe {
        core::slice::from_raw_parts(&uts as *const Utsname as *const u8, core::mem::size_of::<Utsname>())
    };
    if !copy_to_user(buf_ptr, out) { return linux_abi_error(14); }
    0
}

fn fill_uts_buf(buf: &mut [u8; 65], s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(64);
    buf[..n].copy_from_slice(&bytes[..n]);
}

pub fn sys_sethostname(name_ptr: u64, len: u64) -> u64 {
    if len > 64 { return linux_abi_error(22); }
    let mut buf = alloc::vec![0u8; len as usize];
    if !super::copy_from_user(name_ptr, &mut buf) {
        return linux_abi_error(14);
    }
    if let Ok(name) = core::str::from_utf8(&buf) {
        *HOSTNAME.lock() = Some(String::from(name));
        0
    } else {
        linux_abi_error(22)
    }
}

pub fn sys_getrandom(buf_ptr: u64, len: u64, _flags: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return linux_abi_error(14); }
    let mut i = 0;
    while i < len {
        let rnd = crate::cpu::get_random_u64();
        let bytes = rnd.to_ne_bytes();
        let to_copy = core::cmp::min(bytes.len() as u64, len - i);
        if !super::copy_to_user(buf_ptr + i, &bytes[..to_copy as usize]) {
            return linux_abi_error(14);
        }
        i += to_copy;
    }
    len
}

pub fn sys_membarrier(_cmd: u64, _flags: u64, _cpu_id: u64) -> u64 {
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }
    0
}

pub fn sys_get_service_binary(_service_id: u64, _buf_ptr: u64, _buf_size: u64) -> u64 {
    linux_abi_error(38) // ENOSYS
}

pub fn sys_get_logs(buf_ptr: u64, len: u64) -> u64 {
    if !is_user_pointer(buf_ptr, len) { return u64::MAX; }
    let count = crate::serial::copy_logs_to_user(buf_ptr, len);
    count as u64
}

pub fn sys_stop_progress() -> u64 {
    crate::progress::stop_logging();
    0
}

pub fn sys_register_log_hud(pid: u64) -> u64 {
    crate::progress::set_log_hud_pid(pid as u32);
    0
}

pub fn sys_get_storage_device_count() -> u64 {
    crate::storage::device_count() as u64
}

pub fn sys_get_system_stats(stats_ptr: u64) -> u64 {
    use super::SystemStats;

    if stats_ptr == 0 || !is_user_pointer(stats_ptr, core::mem::size_of::<SystemStats>() as u64) {
        return u64::MAX;
    }

    let sched_stats = crate::scheduler::get_stats();
    crate::nvidia::update_all_gpu_vitals();
    let vitals = crate::ai_core::get_vitals();
    let (total_frames, used_frames) = crate::memory::get_memory_stats();

    let stats = SystemStats {
        uptime_ms: sched_stats.total_ticks,
        idle_ms: sched_stats.idle_ticks,
        total_memory_kb: total_frames * 4,
        free_memory_kb: total_frames.saturating_sub(used_frames) * 4,
        cpu_load: vitals.cpu_load,
        cpu_temp: vitals.cpu_temp,
        gpu_load: vitals.gpu_load,
        gpu_temp: vitals.gpu_temp,
        gpu_vram_total_kb: vitals.gpu_vram_total_bytes / 1024,
        gpu_vram_used_kb: vitals.gpu_vram_used_bytes / 1024,
        anomaly_count: vitals.anomaly_count,
        heap_fragmentation: vitals.heap_fragmentation,
        wall_time_offset: super::WALL_TIME_OFFSET.load(Ordering::Relaxed),
    };

    let out = unsafe {
        core::slice::from_raw_parts(&stats as *const SystemStats as *const u8, core::mem::size_of::<SystemStats>())
    };
    if !copy_to_user(stats_ptr, out) { return linux_abi_error(14); }
    0
}

/// Fija el reloj de pared: `time` = Unix time en segundos; se guarda el offset frente al uptime.
pub fn sys_set_time(secs: u64) -> u64 {
    let uptime_ms = crate::scheduler::get_stats().total_ticks;
    let offset = secs.saturating_sub(uptime_ms / 1000);
    super::WALL_TIME_OFFSET.store(offset, Ordering::Relaxed);
    0
}

pub fn sys_read_key() -> u64 {
    crate::interrupts::read_key() as u64
}

pub fn sys_read_mouse_packet() -> u64 {
    crate::interrupts::read_mouse_packet() as u64
}

pub fn sys_register_device(_name_ptr: u64, _name_len: u64, _type_id: u64) -> u64 {
    0
}

pub fn sys_prlimit64(_pid: u64, resource: u64, new_limit_ptr: u64, old_limit_ptr: u64) -> u64 {
    let _ = new_limit_ptr;
    if old_limit_ptr != 0 {
        if !is_user_pointer(old_limit_ptr, 16) { return linux_abi_error(14); }
        let limit = match resource {
            7 => Rlimit { rlim_cur: 1024, rlim_max: 4096 },
            _ => Rlimit { rlim_cur: u64::MAX, rlim_max: u64::MAX },
        };
        let out = unsafe {
            core::slice::from_raw_parts(&limit as *const Rlimit as *const u8, core::mem::size_of::<Rlimit>())
        };
        if !copy_to_user(old_limit_ptr, out) { return linux_abi_error(14); }
    }
    0
}

pub fn sys_pci_enum_devices(buf_ptr: u64, max_count: u64, _a: u64) -> u64 {
    if buf_ptr == 0 {
        return crate::pci::get_device_count() as u64;
    }
    if !is_user_pointer(buf_ptr, max_count * core::mem::size_of::<crate::pci::PciDevice>() as u64) {
        return u64::MAX;
    }
    crate::pci::enum_devices_to_user(buf_ptr, max_count as usize) as u64
}

pub fn sys_pci_read_config(address: u64, offset: u64, size: u64) -> u64 {
    let bus = ((address >> 16) & 0xFF) as u8;
    let slot = ((address >> 8) & 0xFF) as u8;
    let func = (address & 0xFF) as u8;
    unsafe {
        match size {
            1 => crate::pci::pci_config_read_u8(bus, slot, func, offset as u8) as u64,
            2 => crate::pci::pci_config_read_u16(bus, slot, func, offset as u8) as u64,
            4 => crate::pci::pci_config_read_u32(bus, slot, func, offset as u8) as u64,
            _ => 0,
        }
    }
}

pub fn sys_pci_write_config(address: u64, offset: u64, size: u64, value: u64) -> u64 {
    let bus = ((address >> 16) & 0xFF) as u8;
    let slot = ((address >> 8) & 0xFF) as u8;
    let func = (address & 0xFF) as u8;
    unsafe {
        match size {
            1 => crate::pci::pci_config_write_u8(bus, slot, func, offset as u8, value as u8),
            2 => crate::pci::pci_config_write_u16(bus, slot, func, offset as u8, value as u16),
            4 => crate::pci::pci_config_write_u32(bus, slot, func, offset as u8, value as u32),
            _ => (),
        }
    }
    0
}
