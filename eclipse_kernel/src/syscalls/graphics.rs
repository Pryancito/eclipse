//! Syscalls de gráficos y aceleración para Eclipse OS
//! DRM, KMS, Framebuffer y VirtIO-GPU.

use super::*;

pub fn sys_map_fb(target_addr: u64) -> u64 {
    if target_addr == 0 || target_addr < 0x20000000 { return super::linux_abi_error(22); }
    if let Some((phys, _w, _h, _p, size, _source)) = crate::boot::get_fb_info() {
        let cr3 = crate::memory::get_cr3();
        let len = (size + 0xFFF) & !0xFFF;
        
        let curr_pid = current_process_id().unwrap_or(0);
        if curr_pid != 0 {
            let flags_val = crate::memory::linux_prot_to_leaf_pte_bits(3); // RW, NX
            crate::memory::map_phys_bulk(cr3, phys, len as u64, target_addr, flags_val);
            crate::process::register_mmap_vma(curr_pid, target_addr, len as u64, 3, 0x01 | 0x10); // PRIVATE | ANONYMOUS (FB)
            return 0;
        }
    }
    super::linux_abi_error(1)
}

pub fn sys_get_fb_info(info_ptr: u64) -> u64 {
    if info_ptr == 0 || !is_user_pointer(info_ptr, 24) { return super::linux_abi_error(14); }
    if let Some((phys, w, h, p, size, _source)) = crate::boot::get_fb_info() {
        let info = [phys, w as u64, h as u64, p as u64, size as u64];
        super::copy_to_user(info_ptr, unsafe { core::slice::from_raw_parts(info.as_ptr() as *const u8, 40) });
        return 0;
    }
    super::linux_abi_error(1)
}

pub fn sys_drm_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    let curr_pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(curr_pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, request as usize, arg as usize) {
            Ok(ret) => return ret as u64,
            Err(e) => return super::linux_abi_error(e as i32),
        }
    }
    super::linux_abi_error(9)
}

pub fn sys_drm_get_caps(_fd: u64, _arg: u64) -> u64 { 0 }
pub fn sys_drm_alloc_buffer(_fd: u64, _arg: u64) -> u64 { 0 }
pub fn sys_drm_create_fb(_fd: u64, _arg: u64) -> u64 { 0 }
pub fn sys_drm_map_handle(_fd: u64, _arg: u64) -> u64 { 0 }

pub fn sys_drm_drop_master(_fd: u64) -> u64 { 0 }
pub fn sys_drm_set_master(_fd: u64) -> u64 { 0 }
pub fn sys_drm_auth_magic(_fd: u64, _magic: u64) -> u64 { 0 }
pub fn sys_drm_get_magic(_fd: u64, magic_ptr: u64) -> u64 {
    if magic_ptr != 0 { super::copy_to_user(magic_ptr, &[1, 0, 0, 0]); }
    0
}
