//! Backend esqueleto para VMware SVGA II

use super::framebuffer::{FramebufferDriver, FramebufferInfo};
use super::manager::DriverResult;

pub struct VmwareSvgaDriver {
    initialized: bool,
    pub fb_info: FramebufferInfo,
}

impl VmwareSvgaDriver {
    pub const fn new() -> Self {
        Self {
            initialized: false,
            fb_info: FramebufferInfo {
                base_address: 0,
                width: 0,
                height: 0,
                pixels_per_scan_line: 0,
                pixel_format: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                reserved_mask: 0,
            },
        }
    }

    pub fn initialize(&mut self) -> DriverResult<()> {
        // TODO: mapear BAR0 y negociar versión FIFO
        self.initialized = true;
        Ok(())
    }

    pub fn present_rect(
        &mut self,
        target_fb: &mut FramebufferDriver,
        src_x: u32,
        src_y: u32,
        dst_x: u32,
        dst_y: u32,
        width: u32,
        height: u32,
        src_fb: &FramebufferDriver,
    ) -> DriverResult<()> {
        if !self.initialized {
            return Ok(());
        }
        target_fb.blit_fast(dst_x, dst_y, src_x, src_y, width, height, src_fb);
        Ok(())
    }
}
