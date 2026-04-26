//! Graphics and DRM-related syscalls implementation
//!
//! Support for VirtIO-GPU, Virgl 3D, and standard display buffer management.

use crate::process::{self, current_process_id};
use super::linux_abi_error;
use super::{copy_to_user, is_user_pointer};

pub fn sys_get_framebuffer_info(user_buffer: u64) -> u64 {
    use crate::servers::FramebufferInfo;

    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, core::mem::size_of::<FramebufferInfo>() as u64) {
        return u64::MAX;
    }

    let (fb_phys, width, height, pitch, fb_size) = {
        let k = &crate::boot::get_boot_info().framebuffer;
        if crate::boot::gop_framebuffer_valid() {
            let addr = if k.base_address >= crate::memory::PHYS_MEM_OFFSET {
                k.base_address - crate::memory::PHYS_MEM_OFFSET
            } else {
                k.base_address
            };
            let pitch = k.pixels_per_scan_line * 4;
            let size = (pitch as u64).saturating_mul(k.height as u64);
            (addr, k.width, k.height, pitch, size)
        } else if let Some((phys, w, h, p, size)) = crate::virtio::get_primary_virtio_display() {
            (phys, w, h, p, size as u64)
        } else if let Some((phys, _bar1, w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
            let size = (pitch as u64).saturating_mul(h as u64);
            (phys, w, h, pitch, size)
        } else {
            return u64::MAX;
        }
    };

    // Estilo “syscalls antiguas”: devolver una VA mapeada en el proceso (no un físico).
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return u64::MAX;
    }
    let fb_vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, fb_phys, fb_size);
    if fb_vaddr == 0 {
        return u64::MAX;
    }

    let syscall_fb = FramebufferInfo {
        address: fb_vaddr,
        width,
        height,
        pitch,
        bpp: 32,
        red_mask_size: 8,
        red_mask_shift: 16,
        green_mask_size: 8,
        green_mask_shift: 8,
        blue_mask_size: 8,
        blue_mask_shift: 0,
    };
    let out = unsafe {
        core::slice::from_raw_parts(&syscall_fb as *const FramebufferInfo as *const u8, core::mem::size_of::<FramebufferInfo>())
    };
    if !copy_to_user(user_buffer, out) { return u64::MAX; }
    0
}

pub fn sys_get_gpu_display_info(user_buffer: u64) -> u64 {
    if user_buffer == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(user_buffer, 8) {
        return u64::MAX;
    }
    let Some((width, height)) = crate::virtio::get_gpu_display_info() else {
        return u64::MAX;
    };
    if !copy_to_user(user_buffer, &width.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(user_buffer + 4, &height.to_le_bytes()) { return u64::MAX; }
    0
}

pub fn sys_set_cursor_position(arg1: u64, arg2: u64) -> u64 {
    let x = arg1 as i32;
    let y = arg2 as i32;
    
    // Try via unified DRM subsystem first (handles VirtIO hardware cursor and future drivers)
    // Flags 0x02 = DRM_CURSOR_MOVE
    if !crate::drm::set_cursor(0, x, y, 0, 0x02) {
        // Fall back to legacy / software cursor if DRM failed or no driver supports it
        crate::sw_cursor::update(x as u32, y as u32);
    }
    0
}

pub fn sys_gpu_alloc_display_buffer(width: u64, height: u64, out_ptr: u64) -> u64 {
    let (width, height) = (width as u32, height as u32);
    if width == 0 || height == 0 || out_ptr == 0 {
        return u64::MAX;
    }
    if !is_user_pointer(out_ptr, 24) {
        return u64::MAX;
    }
    let Some((phys_addr, resource_id, pitch, size)) = crate::virtio::gpu_alloc_display_buffer(width, height) else {
        return u64::MAX;
    };
    let current_pid = crate::process::current_process_id();
    let page_table_phys = crate::process::get_process_page_table(current_pid);
    if page_table_phys == 0 {
        return u64::MAX;
    }
    let vaddr = crate::memory::map_framebuffer_for_process(page_table_phys, phys_addr, size as u64);
    if vaddr == 0 {
        return u64::MAX;
    }
    if !copy_to_user(out_ptr, &vaddr.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 8, &resource_id.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 12, &pitch.to_le_bytes()) { return u64::MAX; }
    if !copy_to_user(out_ptr + 16, &(size as u64).to_le_bytes()) { return u64::MAX; }
    0
}

pub fn sys_gpu_present(resource_id: u64, x: u64, y: u64, w: u64, h: u64) -> u64 {
    if crate::virtio::gpu_present(
        resource_id as u32,
        x as u32,
        y as u32,
        w as u32,
        h as u32,
    ) {
        0
    } else {
        u64::MAX
    }
}

pub fn sys_gpu_command(cmd: u64, arg1: u64, arg2: u64) -> u64 {
    0
}

pub fn sys_gpu_get_backend() -> u64 {
    // 1: VirtIO GPU
    // 2: NVIDIA
    // 3: EFI GOP (Fallback)
    if crate::virtio::get_primary_virtio_display().is_some() {
        1
    } else if crate::nvidia::get_nvidia_fb_info().is_some() {
        2
    } else {
        3
    }
}

pub fn sys_drm_page_flip(fd: u64, crtc_id: u64, fb_id: u64, flags: u64, user_data: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0x100 /* DRM_IOCTL_PAGE_FLIP */, fb_id as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_get_caps(fd: u64, cap_ptr: u64) -> u64 {
    // Route to scheme ioctl
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC010640C /* DRM_IOCTL_GET_CAP */, cap_ptr as usize) {
            Ok(_) => 0,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_alloc_buffer(fd: u64, width: u64, height: u64, bpp: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeCreateDumb {
            height: u32, width: u32, bpp: u32, flags: u32,
            handle: u32, pitch: u32, size: u64,
        }
        let mut arg = DrmModeCreateDumb {
            height: height as u32,
            width: width as u32,
            bpp: bpp as u32,
            flags: 0, handle: 0, pitch: 0, size: 0,
        };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC02064B2 /* DRM_IOCTL_MODE_CREATE_DUMB */, &mut arg as *mut _ as usize) {
            Ok(_) => arg.handle as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_create_fb(fd: u64, handle: u64, width: u64, height: u64, pitch: u64, bpp: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeFbCmd {
            fb_id: u32, width: u32, height: u32, pitch: u32,
            bpp: u32, depth: u32, handle: u32,
        }
        let mut cmd = DrmModeFbCmd {
            fb_id: 0,
            width: width as u32,
            height: height as u32,
            pitch: pitch as u32,
            bpp: bpp as u32,
            depth: 24, // Assuming 24-bit depth
            handle: handle as u32,
        };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC01C64AE /* DRM_IOCTL_MODE_ADDFB */, &mut cmd as *mut _ as usize) {
            Ok(_) => cmd.fb_id as u64,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

pub fn sys_drm_map_handle(fd: u64, handle: u64) -> u64 {
    let pid = current_process_id().unwrap_or(0);
    if let Some(fd_entry) = crate::fd::fd_get(pid, fd as usize) {
        #[repr(C)]
        struct DrmModeMapDumb {
            handle: u32, pad: u32, offset: u64,
        }
        let mut map = DrmModeMapDumb { handle: handle as u32, pad: 0, offset: 0 };
        match crate::scheme::ioctl(fd_entry.scheme_id, fd_entry.resource_id, 0xC01064B3 /* DRM_IOCTL_MODE_MAP_DUMB */, &mut map as *mut _ as usize) {
            Ok(_) => map.offset,
            Err(e) => (-(e as isize)) as u64,
        }
    } else {
        linux_abi_error(9)
    }
}

// VirtIO-GPU / Virgl 3D Syscalls

pub fn sys_virgl_ctx_create(_ctx_id: u64, name_ptr: u64, name_len: u64) -> u64 {
    let mut name_buf = [0u8; 64];
    let l = (name_len as usize).min(63);
    if name_ptr != 0 { let _ = super::copy_from_user(name_ptr, &mut name_buf[..l]); }
    
    match crate::virtio::virgl_ctx_create(&name_buf[..l]) {
        Some(id) => id as u64,
        None => u64::MAX,
    }
}

pub fn sys_virgl_ctx_destroy(ctx_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_destroy(ctx_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_ctx_attach_resource(ctx_id: u64, res_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_attach_resource(ctx_id as u32, res_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_ctx_detach_resource(ctx_id: u64, res_id: u64) -> u64 {
    if crate::virtio::virgl_ctx_detach_resource(ctx_id as u32, res_id as u32) { 0 } else { u64::MAX }
}

pub fn sys_virgl_alloc_backing(res_id: u64, size: u64) -> u64 {
    if let Some((phys, _)) = crate::virtio::virgl_alloc_backing(size as usize) {
        phys
    } else {
        0
    }
}

pub fn sys_virgl_resource_attach_backing(res_id: u64, phys_addr: u64, size: u64) -> u64 {
    if crate::virtio::virgl_resource_attach_backing(res_id as u32, phys_addr, size as usize) { 0 } else { u64::MAX }
}

pub fn sys_virgl_submit_3d(ctx_id: u64, cmd_ptr: u64, cmd_len: u64) -> u64 {
    if !super::is_user_pointer(cmd_ptr, cmd_len) { return linux_abi_error(14); }
    
    // We need to copy the command buffer to kernel space because the driver
    // expects a slice and might use it for DMA.
    let mut cmd_buf = alloc::vec![0u8; cmd_len as usize];
    let _ = super::copy_from_user(cmd_ptr, &mut cmd_buf);
    
    if crate::virtio::virgl_submit_3d(ctx_id as u32, &cmd_buf) {
        0
    } else {
        u64::MAX
    }
}
