//! Memory-related syscalls implementation
//!
//! Implementation of mmap, munmap, mprotect and brk with robust VMA management.

use alloc::vec::Vec;
use crate::process::{self, current_process_id};
use super::linux_abi_error;

pub mod linux_mmap_abi {
    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;
    pub const PROT_MASK: u64 = 7;
    pub const MAP_FIXED: u64 = 0x10;
    pub const MAP_SHARED: u64 = 0x01;
    pub const MAP_PRIVATE: u64 = 0x02;
    pub const MAP_ANONYMOUS: u64 = 0x20;
    pub const MAP_POPULATE: u64 = 0x08000;
    pub const MAP_HUGETLB: u64 = 0x40000;
    pub const MAP_HUGE_2MB: u64 = 21 << 26;
    pub const USER_ARENA_LO: u64 = 0x6000_0000;
    pub const USER_ARENA_HI: u64 = 0x0000_7000_0000_0000;
    pub const USER_EXEC_STACK_LO: u64 = 0x2000_0000;
    pub const USER_EXEC_STACK_HI: u64 = USER_EXEC_STACK_LO + 0x10_0000;
    pub const ANON_SLACK_BYTES: u64 = 0x8000;
}

pub fn vma_find_gap(vmas: &[crate::process::VMARegion], len: u64) -> Option<u64> {
    let mut v = linux_mmap_abi::USER_ARENA_LO;
    loop {
        if v + len > linux_mmap_abi::USER_ARENA_HI { return None; }
        let mut overlap = false;
        for vma in vmas.iter() {
            if v < vma.end && v + len > vma.start {
                v = vma.end;
                overlap = true;
                break;
            }
        }
        if !overlap { return Some(v); }
    }
}

pub fn vma_remove_range(vmas: &mut Vec<crate::process::VMARegion>, lo: u64, hi: u64) {
    if hi <= lo { return; }
    let old = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                let mut vma_lo = vma.clone();
                vma_lo.end = lo;
                vmas.push(vma_lo);
            }
            if vma.end > hi {
                let mut vma_hi = vma.clone();
                vma_hi.offset += hi - vma.start;
                vma_hi.start = hi;
                vmas.push(vma_hi);
            }
        }
    }
    vma_merge_adjacent(vmas);
}

pub fn vma_mprotect_range(vmas: &mut Vec<crate::process::VMARegion>, lo: u64, hi: u64, prot: u64) {
    if hi <= lo { return; }
    let old = core::mem::take(vmas);
    for vma in old {
        if vma.end <= lo || vma.start >= hi {
            vmas.push(vma);
        } else {
            if vma.start < lo {
                let mut vma_lo = vma.clone();
                vma_lo.end = lo;
                vmas.push(vma_lo);
            }

            let mid_start = vma.start.max(lo);
            let mid_end = vma.end.min(hi);
            let mut new_vma = vma.clone();
            new_vma.offset += mid_start - vma.start;
            new_vma.start = mid_start;
            new_vma.end = mid_end;
            new_vma.flags = prot;
            vmas.push(new_vma);

            if vma.end > hi {
                let mut vma_hi = vma.clone();
                vma_hi.offset += hi - vma.start;
                vma_hi.start = hi;
                vmas.push(vma_hi);
            }
        }
    }
    vma_merge_adjacent(vmas);
}

pub fn vma_merge_adjacent(vmas: &mut Vec<crate::process::VMARegion>) {
    if vmas.len() < 2 { return; }
    vmas.sort_by_key(|v| v.start);
    let old = core::mem::take(vmas);
    let mut iter = old.into_iter();
    if let Some(mut current) = iter.next() {
        for next in iter {
            if current.can_merge(&next) {
                current.end = next.end;
            } else {
                vmas.push(current);
                current = next;
            }
        }
        vmas.push(current);
    }
}

pub fn mprotect_expand_anon_slack(vmas: &[crate::process::VMARegion], mut lo: u64, mut hi: u64, prot: u64) -> (u64, u64) {
    if (prot & linux_mmap_abi::PROT_EXEC) == 0 || hi <= lo {
        return (lo, hi);
    }
    let mut changed = true;
    while changed {
        changed = false;
    for vma in vmas.iter() {
        let is_anon = matches!(vma.object.lock().obj_type, crate::vm_object::VMObjectType::Anonymous);
        if !is_anon { continue; }
        if lo < vma.end && hi > vma.start {
            let na = lo.min(vma.start);
            let ne = hi.max(vma.end);
            if na != lo || ne != hi {
                lo = na;
                hi = ne;
                changed = true;
            }
        }
    }
    }
    (lo, hi)
}

pub fn mmap_pte_linux_prot(base_prot: u64, anon_slack: u64, map_end: u64, page_vaddr: u64) -> u64 {
    if anon_slack == 0 { return base_prot; }
    let slack_lo = map_end.saturating_sub(anon_slack);
    if page_vaddr >= slack_lo {
        base_prot | linux_mmap_abi::PROT_EXEC
    } else {
        base_prot
    }
}

pub fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 { return linux_abi_error(22); } // EINVAL
    let pid = current_process_id().unwrap_or(0);
    
    let is_anon = (flags & linux_mmap_abi::MAP_ANONYMOUS) != 0;
    let is_fixed = (flags & linux_mmap_abi::MAP_FIXED) != 0;
    
    if is_anon {
        let aligned_len = (len + 0xFFF) & !0xFFF;
        let slack = if (prot & linux_mmap_abi::PROT_EXEC) != 0 { linux_mmap_abi::ANON_SLACK_BYTES } else { 0 };
        let total_len = aligned_len + slack;

        let target_vaddr = if is_fixed {
            if addr == 0 || (addr & 0xFFF) != 0 { return linux_abi_error(22); }
            addr
        } else {
            if let Some(proc) = crate::process::get_process(pid) {
                let p_proc = proc.proc.lock();
                let r = p_proc.resources.lock();
                if let Some(v) = vma_find_gap(&r.vmas, total_len) {
                    v
                } else { return linux_abi_error(12); } // ENOMEM
            } else { return linux_abi_error(3); }
        };

        if let Some(mut proc) = crate::process::get_process(pid) {
            {
                let mut p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                if is_fixed { vma_remove_range(&mut r.vmas, target_vaddr, target_vaddr + total_len); }
                
                let obj = crate::vm_object::VMObject::new_anonymous(total_len);
                r.vmas.push(crate::process::VMARegion {
                    start: target_vaddr,
                    end: target_vaddr + total_len,
                    flags: prot,
                    object: obj,
                    offset: 0,
                    is_huge: false,
                    is_shared: (flags & linux_mmap_abi::MAP_SHARED) != 0,
                });
                vma_merge_adjacent(&mut r.vmas);
                drop(r);
                p_proc.mem_frames += (total_len + 4095) / 4096;
            }
            crate::process::update_process(pid, proc);
            return target_vaddr;
        }
    } else {
        // File backed mmap (simplified for now)
        if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
            match crate::scheme::fmap(fd_entry.scheme_id, fd_entry.resource_id, offset as usize, len as usize) {
                Ok(phys) => {
                    // Handle WC flag (bit 63)
                    let wc_requested = (phys as u64) & (1 << 63) != 0;
                    let phys_addr_raw = (phys as u64) & !(1 << 63);

                    let phys_addr = if phys_addr_raw >= crate::memory::PHYS_MEM_OFFSET {
                        phys_addr_raw - crate::memory::PHYS_MEM_OFFSET
                    } else { phys_addr_raw };
                    
                    let aligned_len = (len + 0xFFF) & !0xFFF;
                    let target_vaddr = if is_fixed {
                        if addr == 0 || (addr & 0xFFF) != 0 { return linux_abi_error(22); }
                        addr
                    } else {
                        if let Some(proc) = crate::process::get_process(pid) {
                            let p_proc = proc.proc.lock();
                            let r = p_proc.resources.lock();
                            if let Some(v) = vma_find_gap(&r.vmas, aligned_len) {
                                v
                            } else { return linux_abi_error(12); }
                        } else { return linux_abi_error(3); }
                    };

                    if let Some(mut proc) = crate::process::get_process(pid) {
                        {
                            let p_proc = proc.proc.lock();
                            let mut r = p_proc.resources.lock();
                            if is_fixed { vma_remove_range(&mut r.vmas, target_vaddr, target_vaddr + aligned_len); }
                            
                            // Map flags: Present=1, Writable=2, User=4 -> 7
                            let mut pt_flags = (prot | 7) & 7;
                            if wc_requested {
                                pt_flags |= 0x08; // PWT (Write-Through) -> Maps to PAT Index 1 (WC)
                            }
                            
                            crate::memory::map_user_range(r.page_table_phys, target_vaddr, phys_addr, aligned_len, pt_flags);
                            let obj = crate::vm_object::VMObject::new_physical(phys_addr, aligned_len);
                            r.vmas.push(crate::process::VMARegion {
                                start: target_vaddr,
                                end: target_vaddr + aligned_len,
                                flags: prot,
                                object: obj,
                                offset: 0,
                                is_huge: false,
                                is_shared: (flags & linux_mmap_abi::MAP_SHARED) != 0,
                            });
                            vma_merge_adjacent(&mut r.vmas);
                        }
                        crate::process::update_process(pid, proc);
                        return target_vaddr;
                    }
                }
                Err(e) => return (-(e as isize)) as u64,
            }
        }
    }
    linux_abi_error(9) // EBADF
}


pub fn sys_munmap(addr: u64, len: u64) -> u64 {
    if addr == 0 || (addr & 0xFFF) != 0 || len == 0 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(proc) = crate::process::get_process(pid) {
        let p_proc = proc.proc.lock();
        let mut _r = p_proc.resources.lock();
        vma_remove_range(&mut _r.vmas, addr, addr + len);
        // In a real kernel we would unmap pages from PT here
        return 0;
    }
    linux_abi_error(3)
}

pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    if addr == 0 || (addr & 0xFFF) != 0 || len == 0 { return linux_abi_error(22); }
    let pid = current_process_id().unwrap_or(0);
    if let Some(proc) = crate::process::get_process(pid) {
        let p_proc = proc.proc.lock();
        let mut r = p_proc.resources.lock();
        let (lo, hi) = mprotect_expand_anon_slack(&r.vmas, addr, addr + len, prot);
        vma_mprotect_range(&mut r.vmas, lo, hi, prot);
        return 0;
    }
    linux_abi_error(3)
}

pub fn sys_brk(new_brk: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(mut proc) = crate::process::get_process(pid) {
        let (old_brk, current_brk) = {
                let mut p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                let old_brk = r.brk_current;
                if new_brk == 0 || new_brk < old_brk { return old_brk; }
                
                let aligned_new = (new_brk + 0xFFF) & !0xFFF;
                if aligned_new > old_brk {
                    // Check if it fits
                    let mut overlap = false;
                    for vma in r.vmas.iter() {
                        if old_brk < vma.end && aligned_new > vma.start {
                            overlap = true;
                            break;
                        }
                    }
                    if overlap { return old_brk; }
                    
                    // Expand
                    let obj = crate::vm_object::VMObject::new_anonymous(aligned_new - old_brk);
                    r.vmas.push(crate::process::VMARegion {
                        start: old_brk,
                        end: aligned_new,
                        flags: 3, // RW
                        object: obj,
                        offset: 0,
                        is_huge: false,
                        is_shared: true, // Internal kernel maps are usually shared
                    });
                    vma_merge_adjacent(&mut r.vmas);
                    r.brk_current = aligned_new;
                    let current = r.brk_current;
                    drop(r);
                    p_proc.mem_frames += ((aligned_new - old_brk) / 4096) as u64;
                    (old_brk, current)
                } else {
                    let current = r.brk_current;
                    (old_brk, current)
                }
        };

        crate::process::update_process(pid, proc);
        return current_brk;
    }
    0
}

pub fn sys_mremap(addr: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> u64 {
    // Basic stub
    linux_abi_error(38)
}

pub fn sys_madvise(addr: u64, len: u64, advice: u64) -> u64 {
    // Linux advice is often a hint; we can return success for most common cases
    0
}

pub fn sys_map_framebuffer(addr_ptr: u64, size_ptr: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some((phys, w, h, pitch, size, _)) = crate::boot::get_fb_info() {
        // Map the physical framebuffer into user space
        let target_vaddr = 0x4000_0000; // Fixed virtual address for FB or find gap
        let aligned_size = (size + 4095) & !4095;
        
        if let Some(mut proc) = crate::process::get_process(pid) {
            {
                let p_proc = proc.proc.lock();
                let mut r = p_proc.resources.lock();
                crate::memory::map_user_range(r.page_table_phys, target_vaddr, phys, aligned_size as u64, 3 << 0); // RW
                let obj = crate::vm_object::VMObject::new_physical(phys, aligned_size as u64);
                r.vmas.push(crate::process::VMARegion {
                    start: target_vaddr,
                    end: target_vaddr + aligned_size as u64,
                    flags: 3, // RW
                    object: obj,
                    offset: 0,
                    is_huge: false,
                    is_shared: true, // Framebuffer is shared
                });
                vma_merge_adjacent(&mut r.vmas);
            }
            crate::process::update_process(pid, proc);
            
            if addr_ptr != 0 && super::is_user_pointer(addr_ptr, 8) {
                unsafe { *(addr_ptr as *mut u64) = target_vaddr; }
            }
            if size_ptr != 0 && super::is_user_pointer(size_ptr, 8) {
                unsafe { *(size_ptr as *mut u64) = size as u64; }
            }
            return 0;
        }
    }
    linux_abi_error(5) // EIO
}

