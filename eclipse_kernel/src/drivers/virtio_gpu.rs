//! Driver esqueleto para Virtio-GPU
//!
//! Objetivo: ofrecer una inicialización mínima y una API de "present" para
//! poder seleccionar este backend cuando la GPU primaria sea Virtio.

use core::ptr;

use super::framebuffer::{Color, FramebufferDriver, FramebufferInfo};
use super::manager::DriverResult;

#[derive(Debug)]
pub struct VirtioGpuDriver {
    initialized: bool,
    pub fb_info: FramebufferInfo,
}

impl VirtioGpuDriver {
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

    /// Inicializa Virtio-GPU. De momento solo deja el esqueleto preparado.
    pub fn initialize(&mut self) -> DriverResult<()> {
        // TODO: detectar dispositivo Virtio (vendor 0x1AF4, device 0x1050/0x105A), mapear BARs,
        // negociar características y crear recurso 2D por defecto.
        self.initialized = true;
        Ok(())
    }

    /// Present de un rectángulo. Por ahora, fallback a un blit directo usando el FramebufferDriver
    /// si ya tenemos un framebuffer GOP activo. En la siguiente iteración, esto enviará comandos
    /// al dispositivo Virtio-GPU.
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

        // Fallback: copia por filas usando la primitiva existente del framebuffer
        // (blit_fast ya optimiza por filas y usa write_bytes/copy cuando es posible)
        target_fb.blit_fast(dst_x, dst_y, src_x, src_y, width, height, src_fb);
        Ok(())
    }
}


