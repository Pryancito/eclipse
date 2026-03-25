//! Eclipse OS Syscall Interface
//! Type-safe syscall wrappers inspired by redox-syscall

#![no_std]

pub mod number;
pub mod error;
pub mod flag;
pub mod call;
pub use call::*;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    pub device_id: u32,
    pub event_type: u8, // 0=key, 1=mouse_move, 2=mouse_button, 3=mouse_scroll
    pub code: u16,
    pub value: i32,
    pub timestamp: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
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
    pub wall_time_offset: u64, // Unix timestamp offset (seconds)
}

#[repr(C)]

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessInfo {
    pub pid: u32,
    pub state: u32,
    pub name: [u8; 16],
    pub cpu_ticks: u64,
    pub mem_frames: u64,
}

impl ProcessInfo {
    pub const fn new() -> Self {
        Self {
            pid: 0,
            state: 0,
            name: [0; 16],
            cpu_ticks: 0,
            mem_frames: 0,
        }
    }
}

pub use crate::error::{Error, Result};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmCaps {
    pub has_3d: bool,
    pub has_cursor: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// Information about a running process

pub use number::*;

#[cfg(target_arch = "x86_64")]
mod arch {
    #[inline(always)]
    pub unsafe fn syscall0(n: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall1(n: usize, arg1: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall2(n: usize, arg1: usize, arg2: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall3(n: usize, arg1: usize, arg2: usize, arg3: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall4(n: usize, arg1: usize, arg2: usize, arg3: usize, arg4: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall5(n: usize, arg1: usize, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }

    #[inline(always)]
    pub unsafe fn syscall6(n: usize, arg1: usize, arg2: usize, arg3: usize, arg4: usize, arg5: usize, arg6: usize) -> usize {
        let ret: usize;
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            in("r9") arg6,
            out("rcx") _,
            out("r11") _,
            lateout("rax") ret
        );
        ret
    }
}

pub use arch::*;
