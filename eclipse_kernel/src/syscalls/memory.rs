//! Syscalls de gestión de memoria para Eclipse OS

use super::*;
use crate::process::{self, VMARegion};
use crate::memory;

// Linux mmap constants
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;

pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

pub const USER_ARENA_LO: u64 = 0x0000_1000_0000_0000;
pub const USER_ARENA_HI: u64 = 0x0000_7000_0000_0000;
pub const ANON_SLACK_BYTES: u64 = 0x1000 * 1024; // 4MB extra for safety in some allocs

pub fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: u64, _offset: u64) -> u64 {
    if length == 0 || length > 0x0000_7FFF_FFFF_FFFF { return super::linux_abi_error(22); }
    let aligned_length = (length + 0xFFF) & !0xFFF;
    let current_pid = match current_process_id() {
        Some(pid) => pid,
        None => return super::linux_abi_error(3),
    };

    let fd_entry = if (flags & MAP_ANONYMOUS) == 0 && fd < 64 {
        crate::fd::fd_get(current_pid, fd as usize)
    } else {
        None
    };

    if let Some(mut proc) = process::get_process(current_pid) {
        let mut r = proc.resources.lock();
        let page_table_phys = r.page_table_phys;

        let anon_slack: u64 = if (prot & PROT_EXEC) != 0 { ANON_SLACK_BYTES } else { 0 };

        let map_start = if addr != 0 && (flags & MAP_FIXED) != 0 {
            if (addr & 0xFFF) != 0 { return super::linux_abi_error(22); }
            memory::unmap_user_range(page_table_phys, addr, aligned_length + anon_slack);
            addr
        } else {
            USER_ARENA_LO + (current_pid as u64 * 0x1000_0000) % (USER_ARENA_HI - USER_ARENA_LO)
        };

        let map_end = map_start + aligned_length + anon_slack;
        let mut curr = map_start;
        while curr < map_end {
            if let Some(phys) = memory::alloc_phys_frame_for_anon_mmap() {
                memory::map_user_page_4kb(page_table_phys, curr, phys, 0x7);
                proc.mem_frames += 1;
            }
            curr += 4096;
        }

        r.vmas.push(VMARegion {
            start: map_start,
            end: map_end,
            flags: prot,
            file_backed: fd_entry.is_some(),
            anon_kernel_slack: anon_slack,
            shared_anon_id: None,
            is_huge: false,
        });

        drop(r);
        crate::process::update_process(current_pid, proc);
        return map_start;
    }
    super::linux_abi_error(12)
}

pub fn sys_munmap(addr: u64, length: u64) -> u64 {
    if (addr & 0xFFF) != 0 || length == 0 { return super::linux_abi_error(22); }
    if let Some(pid) = current_process_id() {
        if let Some(proc) = process::get_process(pid) {
            let page_table = proc.resources.lock().page_table_phys;
            memory::unmap_user_range(page_table, addr, length);
            return 0;
        }
    }
    super::linux_abi_error(3)
}

pub fn sys_mprotect(_addr: u64, _len: u64, _prot: u64) -> u64 { 0 }

pub fn sys_brk(addr: u64) -> u64 {
    if let Some(pid) = current_process_id() {
        if let Some(mut proc) = process::get_process(pid) {
            let mut r = proc.resources.lock();
            let current_brk = r.brk_current;
            if addr == 0 { return current_brk; }
            if addr < current_brk { return current_brk; }
            
            let old_end = (current_brk + 4095) & !4095;
            let new_end = (addr + 4095) & !4095;
            let mut curr = old_end;
            while curr < new_end {
                if let Some(phys) = memory::alloc_phys_frame_for_anon_mmap() {
                    memory::map_user_page_4kb(r.page_table_phys, curr, phys, 0x7);
                    proc.mem_frames += 1;
                }
                curr += 4096;
            }
            r.brk_current = addr;
            return addr;
        }
    }
    0
}

pub fn sys_madvise(_addr: u64, _len: u64, _advice: u64) -> u64 { 0 }
pub fn sys_mremap(old_addr: u64, old_size: u64, new_size: u64, _flags: u64, _new_addr: u64) -> u64 {
    if new_size > old_size {
        return sys_mmap(old_addr + old_size, new_size - old_size, 3, 0x20 | 0x10, 0, 0);
    }
    old_addr
}
