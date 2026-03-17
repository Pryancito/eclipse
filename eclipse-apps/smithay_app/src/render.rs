use std::prelude::v1::*;
use core::matches;
#[cfg(target_vendor = "eclipse")]
use libc::{
    mmap, munmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS,
    FramebufferInfo,
};
#[cfg(not(target_vendor = "eclipse"))]
use libc::{mmap, munmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS};

#[cfg(not(target_vendor = "eclipse"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferInfo {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
}

#[cfg(not(target_vendor = "eclipse"))]
fn get_logs(_buf: *mut u8, _max: usize) -> usize { 0 }

use micromath::F32Ext;
#[cfg(target_vendor = "eclipse")]
use libc::ProcessInfo;
#[cfg(not(target_vendor = "eclipse"))]
use eclipse_syscall::ProcessInfo;
use sidewind::ui::{self, icons, colors};
use sidewind::{font_terminus_12, font_terminus_14, font_terminus_20};
use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::primitives::{Rectangle, PrimitiveStyleBuilder, Line};
use embedded_graphics::mono_font::{ascii::{FONT_6X12, FONT_10X20}, MonoTextStyle};
use embedded_graphics::text::Text;
use crate::compositor::{ShellWindow, WindowContent, ExternalSurface, WindowButton};
use crate::state::ServiceInfo;
use crate::display::{self, DisplayDevice, FramebufferDesc, DisplayError, ControlDevice, DisplayCaps};

pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

const DEFAULT_WIDTH: u32  = 1280;
const DEFAULT_HEIGHT: u32 = 800;

const SIDEBAR_ICON_TYPES: [ui::TechCardIconType; 5] = [
    ui::TechCardIconType::ControlPanel,
    ui::TechCardIconType::System,
    ui::TechCardIconType::Apps,
    ui::TechCardIconType::Files,
    ui::TechCardIconType::Network,
];

pub const STROKE_COLORS: [Rgb888; 5] = [
    colors::ACCENT_BLUE,
    colors::ACCENT_RED,
    colors::ACCENT_GREEN,
    colors::ACCENT_YELLOW,
    colors::WHITE,
];

pub struct FramebufferState {
    pub info: FramebufferInfo,
    pub back_fb_id: display::control::framebuffer::Handle,
    pub front_fb_id: display::control::framebuffer::Handle,
    pub back_addr: usize,
    pub front_addr: usize,
    pub background_addr: usize,
    pub drm_fd: usize,
    pub drm_crtc: display::control::CrtcHandle,
    pub gpu: Option<sidewind::gpu::GpuDevice>,
    pub cursor_handle: display::buffer::Handle,
    pub planes: Vec<display::control::PlaneInfo>,
    pub hud_fb_id: display::control::framebuffer::Handle,
    pub hud_handle: display::buffer::Handle,
    pub hud_addr: usize,
}

impl FramebufferState {
    pub fn init() -> Option<Self> {
        #[cfg(target_vendor = "eclipse")]
        {
            let dev = DisplayDevice::open().ok()?;
            let fb_front = dev.create_framebuffer().ok()?;
            let fb_back  = dev.create_framebuffer().ok()?;

            let fb_size = (dev.caps.pitch as usize) * (dev.caps.height as usize);

            let bg_buffer = unsafe {
                mmap(
                    core::ptr::null_mut(),
                    fb_size,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            let background_addr = if bg_buffer.is_null() || bg_buffer as usize == usize::MAX {
                0
            } else {
                bg_buffer as usize
            };

            let info = FramebufferInfo {
                address: fb_front.addr as u64,
                width: dev.caps.width,
                height: dev.caps.height,
                pitch: dev.caps.pitch,
                bpp: 32,
                ..Default::default()
            };

            let mut fb_state = Self {
                info,
                back_fb_id: fb_back.fb_id,
                front_fb_id: fb_front.fb_id,
                back_addr: fb_back.addr,
                front_addr: fb_front.addr,
                background_addr,
                drm_fd: dev.fd as usize,
                drm_crtc: dev.crtc,
                gpu: Some(sidewind::gpu::GpuDevice::new()),
                cursor_handle: display::buffer::Handle(0),
                planes: Vec::new(),
                hud_fb_id: display::control::framebuffer::Handle(0),
                hud_handle: display::buffer::Handle(0),
                hud_addr: 0,
            };

            // Discover planes
            if let Ok(plane_handles) = dev.plane_resources() {
                for ph in plane_handles {
                    if let Ok(info) = dev.get_plane(ph) {
                        fb_state.planes.push(info);
                    }
                }
            }

            fb_state.init_cursor();
            fb_state.init_hud();
            Some(fb_state)
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            None
        }
    }

    /// Initialize hardware cursor image (64x64)
    pub fn init_cursor(&mut self) {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.drm_fd == 0 { return; }
            
            // Allocate 64x64x4 buffer
            let dev = DisplayDevice {
                fd: self.drm_fd,
                caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                crtc: self.drm_crtc,
                connector: display::control::ConnectorHandle(0),
            };

            let size = 64 * 64 * 4;
            if let Ok(db) = dev.create_dumb_buffer(64, 64, 32) {
                self.cursor_handle = db.handle;
                
                // Map and draw cursor image
                if let Ok(addr) = dev.map_buffer(db.handle, size as usize) {
                    let ptr = addr as *mut u32;
                    unsafe {
                        for i in 0..(64 * 64) {
                            // Simple white arrow with outline
                            let x = i % 64;
                            let y = i / 64;
                            
                            let mut color = 0x00000000; // Transparent
                            
                            // Cheap arrow tip
                            if x < 16 && y < 16 && x <= y {
                                if x == y || x == 0 || y == 15 {
                                    color = 0xFF000000; // Black outline
                                } else {
                                    color = 0xFFFFFFFF; // White fill
                                }
                            }
                            
                            core::ptr::write_volatile(ptr.add(i as usize), color);
                        }
                    }
                    unsafe { let _ = munmap(addr as *mut core::ffi::c_void, size); }
                }
                
                // Set cursor (flags=0x01 | 0x02: SET | MOVE)
                // We set it at (0,0) initially
                let _ = dev.set_cursor(self.drm_crtc, 0, 0, db.handle);
            }
        }
    }

    /// Initialize HUD secondary framebuffer
    pub fn init_hud(&mut self) {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.drm_fd == 0 { return; }
            let dev = DisplayDevice {
                fd: self.drm_fd,
                caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                crtc: self.drm_crtc,
                connector: display::control::ConnectorHandle(0),
            };

            // Allocate 400x110 buffer for HUD
            if let Ok(db) = dev.create_dumb_buffer(400, 110, 32) {
                if let Ok(fb_id) = dev.add_framebuffer(db.handle, db.width, db.height, db.pitch) {
                    if let Ok(addr) = dev.map_buffer(db.handle, db.size) {
                        self.hud_fb_id = fb_id;
                        self.hud_handle = db.handle;
                        self.hud_addr = addr as usize;
                        
                        // Clear to transparent
                        unsafe { core::ptr::write_bytes(addr as *mut u8, 0, 400 * 110 * 4); }
                    }
                }
            }
        }
    }
    #[cfg(not(target_vendor = "eclipse"))]
    pub fn init_software(width: u32, height: u32) -> Option<Self> {
        let pitch = width * 4;
        let fb_size = (pitch as usize) * (height as usize);
        let ptr = Box::leak(vec![0u8; fb_size].into_boxed_slice()).as_mut_ptr();
        if ptr.is_null() { return None; }
        
        let bg_ptr = Box::leak(vec![0u8; fb_size].into_boxed_slice()).as_mut_ptr();
        Some(FramebufferState {
            info: FramebufferInfo { address: ptr as u64, width, height, pitch, bpp: 32 },
            back_fb_id: display::control::framebuffer::Handle(0),
            front_fb_id: display::control::framebuffer::Handle(0),
            back_addr: ptr as usize,
            front_addr: ptr as usize,
            background_addr: bg_ptr as usize,
            drm_fd: 0,
            drm_crtc: display::control::CrtcHandle(0),
            gpu: None,
            cursor_handle: display::buffer::Handle(0),
            planes: alloc::vec::Vec::new(),
            hud_fb_id: display::control::framebuffer::Handle(0),
            hud_handle: display::buffer::Handle(0),
            hud_addr: 0,
        })
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        Self {
            info: FramebufferInfo { address: 0, width: 1024, height: 768, pitch: 4096, bpp: 32 },
            back_fb_id: display::control::framebuffer::Handle(0),
            front_fb_id: display::control::framebuffer::Handle(0),
            back_addr: 0, front_addr: 0, background_addr: 0, drm_fd: 0,
            drm_crtc: display::control::CrtcHandle(0),
            gpu: None,
            cursor_handle: display::buffer::Handle(0),
            planes: alloc::vec::Vec::new(),
            hud_fb_id: display::control::framebuffer::Handle(0),
            hud_handle: display::buffer::Handle(0),
            hud_addr: 0,
        }
    }

    pub fn clear_screen(&self, color: Rgb888) {
        let raw = 0xFF_00_00_00 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        if let Some(ref gpu) = self.gpu {
            if gpu.backend() == sidewind::gpu::GpuBackend::Nvidia && self.back_addr != 0 {
                let mut enc = sidewind::gpu::GpuCommandEncoder::new(gpu);
                if enc.fill_rect(0, 0, self.info.width, self.info.height, raw).is_ok() { return; }
            }
        }
        self.clear_back_buffer_raw(color);
    }

    pub fn clear_back_buffer_raw(&self, color: Rgb888) {
        if self.back_addr == 0 { return; }
        let w = self.info.width as usize;
        let h = self.info.height as usize;
        let pitch_px = (self.info.pitch / 4) as usize; 
        let raw = 0xFF_00_00_00 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        let ptr = self.back_addr as *mut u32;

        for y in 0..h {
            let row = y * pitch_px;
            unsafe {
                core::slice::from_raw_parts_mut(ptr.add(row), w).fill(raw);
            }
        }
    }

    pub fn try_remap_framebuffer(&mut self) {}
    pub fn present_rect(&self, _x: i32, _y: i32, _w: i32, _h: i32) {}

    #[inline]
    pub fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        if self.back_addr == 0 { return; }
        let w = self.info.width as i32;
        let h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4) as usize;
        let ptr = self.back_addr as *mut u32;

        for py in (cy - half)..=(cy + half) {
            if py >= 0 && py < h && cx >= 0 && cx < w {
                let offset = (py as usize * pitch_px) + cx as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
        for px in (cx - half)..=(cx + half) {
            if cy >= 0 && cy < h && px >= 0 && px < w {
                let offset = (cy as usize * pitch_px) + px as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
    }

    pub fn present(&mut self) -> bool {
        if self.back_addr == 0 { return true; }
        #[cfg(target_vendor = "eclipse")]
        {
            // Perform the page flip.
            match eclipse_syscall::drm_page_flip(self.back_fb_id.0) {
                Ok(_) => {
                    core::mem::swap(&mut self.back_fb_id, &mut self.front_fb_id);
                    core::mem::swap(&mut self.back_addr, &mut self.front_addr);
                    let dev = DisplayDevice { 
                        fd: self.drm_fd, 
                        caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                        crtc: self.drm_crtc,
                        connector: display::control::ConnectorHandle(0),
                    };
                    
                    // Wait for VBlank to synchronize and throttle.
                    let _ = dev.wait_vblank(self.drm_crtc);
                    
                    true
                }
                Err(_) => false,
            }
        }
        #[cfg(not(target_vendor = "eclipse"))] { true }
    }

    /// Move hardware cursor
    pub fn set_cursor_position(&self, x: i32, y: i32) {
        #[cfg(target_vendor = "eclipse")]
        {
            if self.drm_fd > 0 {
                let dev = DisplayDevice {
                    fd: self.drm_fd,
                    caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                    crtc: self.drm_crtc,
                    connector: display::control::ConnectorHandle(0),
                };
                // DRM_CURSOR_MOVE is 0x02
                // Since we already have cursor_handle set, we might not need it for MOVE, 
                // but passing 0 (or previous handle) should work.
                let _ = dev.set_cursor(self.drm_crtc, x, y, display::buffer::Handle(0));
            }
        }
    }

    /// Set an overlay plane configuration
    pub fn set_overlay_plane(&self, fb_id: display::control::framebuffer::Handle, x: i32, y: i32, w: u32, h: u32) {
        #[cfg(target_vendor = "eclipse")]
        {
            // Find an overlay plane (plane_type 0)
            if let Some(plane) = self.planes.iter().find(|p| p.plane_type == 0) {
                let dev = DisplayDevice {
                    fd: self.drm_fd,
                    caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                    crtc: self.drm_crtc,
                    connector: display::control::ConnectorHandle(0),
                };
                let _ = dev.set_plane(
                    plane.handle,
                    self.drm_crtc,
                    fb_id,
                    x, y, w, h,
                    0, 0, w << 16, h << 16, // Source coordinates in 16.16 fixed point
                );
            }
        }
    }


    pub fn pre_render_background(&mut self) {
        if self.background_addr == 0 { return; }
        let old_base = self.back_addr;
        self.back_addr = self.background_addr;
        self.clear_back_buffer_raw(colors::COSMIC_DEEP);
        let _ = ui::draw_cosmic_background(self);
        let mut star_seed = 0xACE1u32;
        let _ = ui::draw_starfield_cosmic(self, &mut star_seed, Point::zero());
        let _ = ui::draw_grid(self, Rgb888::new(18, 28, 55), 48, Point::zero());
        self.back_addr = old_base;
    }

    pub fn blit_background(&self) {
        if self.back_addr == 0 || self.background_addr == 0 { return; }
        let size_bytes = (self.info.pitch as usize) * (self.info.height as usize);
        unsafe { core::ptr::copy_nonoverlapping(self.background_addr as *const u8, self.back_addr as *mut u8, size_bytes); }
    }


    pub fn blit_buffer(&mut self, x: i32, y: i32, w: u32, h: u32, src: *const u32, src_size: usize) {
        if self.back_addr == 0 || src.is_null() || w == 0 || h == 0 { return; }
        let fb_w = self.info.width as i32;
        let fb_h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4) as usize;
        let dst_ptr = self.back_addr as *mut u32;

        let required_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if required_bytes == 0 || required_bytes > src_size { return; }

        let ix_start = (0).max(-x) as usize;
        let ix_end = (w as i32).min(fb_w - x) as usize;
        if ix_start >= ix_end { return; }
        let row_copy_len = ix_end - ix_start;

        for iy in 0..h as i32 {
            let dy = y + iy;
            if dy < 0 || dy >= fb_h { continue; }
            
            let src_row_off = (iy as usize) * (w as usize) + ix_start;
            let dst_row_off = (dy as usize * pitch_px) + (x + ix_start as i32) as usize;
            
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add(src_row_off),
                    dst_ptr.add(dst_row_off),
                    row_copy_len
                );
            }
        }
    }
}

#[repr(C)]
pub struct OverlayDrawTarget {
    pub addr: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

impl DrawTarget for OverlayDrawTarget {
    type Color = Rgb888;
    type Error = core::convert::Infallible;
    
    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where I: IntoIterator<Item = Pixel<Self::Color>> {
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < self.width as i32 && coord.y >= 0 && coord.y < self.height as i32 {
                let offset = (coord.y as usize * self.pitch as usize) + (coord.x as usize * 4);
                let ptr = (self.addr + offset) as *mut u32;
                let c = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                unsafe { core::ptr::write_volatile(ptr, c); }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for OverlayDrawTarget {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for FramebufferState {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    #[inline]
    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error> where I: IntoIterator<Item = Pixel<Self::Color>> {
        if self.back_addr == 0 { return Ok(()); }
        let w = self.info.width as i32;
        let h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4) as usize;
        let fb_ptr = self.back_addr as *mut u32;

        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < w && coord.y >= 0 && coord.y < h {
                let offset = (coord.y as usize * pitch_px) + coord.x as usize;
                let raw_color = 0xFF_00_00_00 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                unsafe { core::ptr::write_volatile(fb_ptr.add(offset), raw_color); }
            }
        }
        Ok(())
    }

    #[inline]
    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        if self.back_addr == 0 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let fb_ptr = self.back_addr as *mut u32;
        
        let intersection = area.intersection(&Rectangle::new(Point::zero(), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() { return Ok(()); }
        
        let raw_color = 0xFF_00_00_00 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        
        let x_start = intersection.top_left.x;
        let x_end = x_start + intersection.size.width as i32;
        let y_start = intersection.top_left.y;
        let y_end = y_start + intersection.size.height as i32;
        let row_width = (x_end - x_start) as usize;

        for y in y_start..y_end {
            let row_base = (y * pitch_px + x_start) as usize;
            unsafe {
                core::slice::from_raw_parts_mut(fb_ptr.add(row_base), row_width).fill(raw_color);
            }
        }
        Ok(())
    }
}

impl OriginDimensions for FramebufferState {
    fn size(&self) -> Size { Size::new(self.info.width, self.info.height) }
}

impl FramebufferState {
    #[inline]
    pub fn blur_rect(&mut self, rect: &Rectangle, radius: i32) {
        if true { return; } // Disabled for performance stability
        if self.back_addr == 0 || radius <= 0 { return; }
        let w = self.info.width as i32;
        let h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4) as usize;
        let fb_ptr = self.back_addr as *mut u32;

        let inter = rect.intersection(&Rectangle::new(Point::zero(), Size::new(w as u32, h as u32)));
        if inter.is_zero_sized() { return; }

        let rx = inter.top_left.x; let ry = inter.top_left.y;
        let rw = inter.size.width as i32; let rh = inter.size.height as i32;

        for y in ry..ry + rh {
            for x in rx..rx + rw {
                let mut r = 0; let mut g = 0; let mut b = 0; let mut count = 0;
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let nx = x + dx; let ny = y + dy;
                        if nx >= rx && nx < rx + rw && ny >= ry && ny < ry + rh {
                            let off = (ny as usize * pitch_px) + nx as usize;
                            let c = unsafe { core::ptr::read_volatile(fb_ptr.add(off)) };
                            r += (c >> 16) & 0xFF; g += (c >> 8) & 0xFF; b += c & 0xFF;
                            count += 1;
                        }
                    }
                }
                if count > 0 {
                    let new_c = 0xFF_00_00_00 | ((r/count) << 16) | ((g/count) << 8) | (b/count);
                    unsafe { core::ptr::write_volatile(fb_ptr.add((y as usize * pitch_px) + x as usize), new_c); }
                }
            }
        }
    }

    #[inline] pub fn draw_sdf_shadow(&mut self, rect: &Rectangle, radius: i32) { self.draw_sdf_effect(rect, radius, Rgb888::new(0, 0, 0), 120, 8.0) }
    #[inline] pub fn draw_sdf_glow(&mut self, rect: &Rectangle, radius: i32, color: Rgb888) { self.draw_sdf_effect(rect, radius, color, 180, 8.0) }

    #[inline]
    fn draw_sdf_effect(&mut self, rect: &Rectangle, radius: i32, color: Rgb888, intensity: u32, corner_radius: f32) {
        if true { return; } // Disabled for performance stability
        if self.back_addr == 0 || radius <= 0 { return; }
        let w = self.info.width as i32;
        let h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4) as usize;
        let fb_ptr = self.back_addr as *mut u32;

        let s_rect = Rectangle::new(rect.top_left - Point::new(radius, radius), Size::new(rect.size.width + radius as u32 * 2, rect.size.height + radius as u32 * 2));
        let inter = s_rect.intersection(&Rectangle::new(Point::zero(), Size::new(w as u32, h as u32)));
        if inter.is_zero_sized() { return; }

        let half_w = rect.size.width as f32 / 2.0 - corner_radius;
        let half_h = rect.size.height as f32 / 2.0 - corner_radius;
        let cx = rect.top_left.x as f32 + rect.size.width as f32 / 2.0;
        let cy = rect.top_left.y as f32 + rect.size.height as f32 / 2.0;

        for y in inter.top_left.y..inter.top_left.y + inter.size.height as i32 {
            for x in inter.top_left.x..inter.top_left.x + inter.size.width as i32 {
                let dx = (x as f32 - cx).abs() - half_w;
                let dy = (y as f32 - cy).abs() - half_h;
                let dist = dx.max(0.0).hypot(dy.max(0.0)) + dx.min(0.0).max(dy.min(0.0)) - corner_radius;
                
                if dist > 0.0 && dist < radius as f32 {
                    let alpha = 1.0 - (dist / radius as f32);
                    let a_scaled = (alpha * alpha * intensity as f32) as u32; 
                    if a_scaled > 0 {
                        let off = (y as usize * pitch_px) + x as usize;
                        let bg = unsafe { core::ptr::read_volatile(fb_ptr.add(off)) };
                        let r = (((bg >> 16) & 0xFF) * (255 - a_scaled) + (color.r() as u32) * a_scaled) / 255;
                        let g = (((bg >> 8) & 0xFF) * (255 - a_scaled) + (color.g() as u32) * a_scaled) / 255;
                        let b = ((bg & 0xFF) * (255 - a_scaled) + (color.b() as u32) * a_scaled) / 255;
                        unsafe { core::ptr::write_volatile(fb_ptr.add(off), 0xFF_00_00_00 | (r << 16) | (g << 8) | b); }
                    }
                }
            }
        }
    }
}

pub fn draw_dashboard(fb: &mut FramebufferState, _counter: u64, cpu: f32, mem: f32, net: f32, uptime_ticks: u64) {
    let cpu = if cpu.is_nan() { 0.0 } else { cpu };
    let mem = if mem.is_nan() { 0.0 } else { mem };
    let net = if net.is_nan() { 0.0 } else { net };
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let _ = Rectangle::new(Point::new(0, 0), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(2, 4, 10)).build())
        .draw(fb);
    let _ = ui::draw_grid(fb, Rgb888::new(30, 60, 120), 64, Point::zero());
    use sidewind::ui::{Panel, Gauge, Terminal, Widget};
    let p_w = 600;
    let p_h = 400;
    let px = (w - p_w) / 2;
    let py = (h - p_h) / 2;
    let main_panel = Panel { position: Point::new(px, py), size: Size::new(p_w as u32, p_h as u32), title: "ANALISIS DE SISTEMA // DASHBOARD" };
    
    // fb.blur_rect(&Rectangle::new(main_panel.position, main_panel.size), 4);
    let _ = main_panel.draw(fb);
    
    let g1 = Gauge { center: main_panel.position + Point::new(120, 180), radius: 70, value: cpu, label: "CARGA CPU" };
    let _ = g1.draw(fb);
    let g2 = Gauge { center: main_panel.position + Point::new(300, 180), radius: 70, value: mem, label: "MEMORIA RAM" };
    let _ = g2.draw(fb);
    let g3 = Gauge { center: main_panel.position + Point::new(480, 180), radius: 70, value: net, label: "RED INT" };
    let _ = g3.draw(fb);

    let mut cpu_line = heapless::String::<32>::new();
    let _ = core::fmt::write(&mut cpu_line, format_args!("CPU: {}%", (cpu * 100.0) as u32));
    let mut mem_line = heapless::String::<32>::new();
    let _ = core::fmt::write(&mut mem_line, format_args!("MEM: {}%", (mem * 100.0) as u32));
    let mut net_line = heapless::String::<32>::new();
    let _ = core::fmt::write(&mut net_line, format_args!("NET: {}%", (net * 100.0) as u32));
    let mut uptime_line = heapless::String::<32>::new();
    let uptime_secs = uptime_ticks / 1000;
    let _ = core::fmt::write(&mut uptime_line, format_args!("UPTIME: {}h {}m", uptime_secs / 3600, (uptime_secs / 60) % 60));

    let term_lines: &[&str] = &[ 
        "eclipse@os:~$ sysinfo --live", 
        &cpu_line,
        &mem_line,
        &net_line,
        &uptime_line,
        "> system status nominal" 
    ];
    let term = Terminal { position: main_panel.position + Point::new(30, 220), size: Size::new(p_w as u32 - 60, 150), lines: term_lines };
    let _ = term.draw(fb);
    let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
    let _ = Text::new("PRESIONE 'SUPER' PARA VOLVER AL ESCRITORIO", Point::new(w / 2 - 200, h - 100), label_style).draw(fb);
}

pub fn draw_lock_screen(fb: &mut FramebufferState, counter: u64) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let center = Point::new((w as f32 / 2.0).round() as i32, (h as f32 / 2.0).round() as i32);
    let _ = fb.clear(colors::BACKGROUND_DEEP);
    let logo_r = ((w.min(h) / 2) - 100).min(300).max(150);
    let _ = ui::draw_eclipse_logo(fb, center, counter, logo_r);
    let label_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
    let label_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
    let lbl_pos = center + Point::new(-90, 220);
    let _ = Text::new("SISTEMA BLOQUEADO", lbl_pos + Point::new(1, 1), label_glow).draw(fb);
    let _ = Text::new("SISTEMA BLOQUEADO", lbl_pos, label_style).draw(fb);
    let total_secs = 74520 + (counter / 60) % 86400; 
    let hrs = (total_secs / 3600) % 24;
    let mins = (total_secs / 60) % 60;
    let secs = total_secs % 60;
    let mut time_str = heapless::String::<12>::new();
    let _ = core::fmt::write(&mut time_str, format_args!("{:02}:{:02}:{:02}", hrs, mins, secs));
    let time_pos = center + Point::new(-45, -280);
    let _ = Text::new(&time_str, time_pos, MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE)).draw(fb);
}

pub fn draw_alt_tab_hud(fb: &mut FramebufferState, _windows: &[ShellWindow], window_count: usize, focused: Option<usize>) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let panel_w = 600;
    let panel_h = 240;
    let px = w / 2 - panel_w / 2;
    let py = h / 2 - panel_h / 2;
    let rect = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32));
    let _ = ui::draw_glass_card(fb, rect, "CONMUTADOR // SISTEMA", colors::ACCENT_CYAN);
    let _ = ui::draw_glowing_hexagon(fb, Point::new(px + 40, py + 25), 18, colors::ACCENT_CYAN);
    let title_glow = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, Rgb888::new(40, 120, 180));
    let title_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
    let item_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
    let focus_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::ACCENT_CYAN);
    let title_pos = Point::new(w / 2 - 130, py + 35);
    let _ = Text::new("CONMUTADOR // VENTANAS", title_pos + Point::new(1, 1), title_glow).draw(fb);
    let _ = Text::new("CONMUTADOR // VENTANAS", title_pos, title_style).draw(fb);
    for i in 0..window_count {
        let iy = h / 2 - panel_h / 2 + 70 + (i as i32 * 30);
        let style = if Some(i) == focused { focus_style } else { item_style };
        let prefix = if Some(i) == focused { "> " } else { "  " };
        let _ = Text::new(prefix, Point::new(w / 2 - 180, iy), style).draw(fb);
        let _ = Text::new("Shell Window", Point::new(w / 2 - 150, iy), style).draw(fb);
    }
}

pub fn draw_quick_settings(fb: &mut FramebufferState) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let rect = Rectangle::new(Point::new(w - 260, h - 210), Size::new(250, 160));
    let _ = ui::draw_glass_card(fb, rect, "QUICK SETTINGS", colors::GLOW_HI);
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
    let _ = Text::new("RED:  [ESTABLE]", Point::new(w - 240, h - 170), text_style).draw(fb);
    let bar_size = Size::new(200, 15);
    let _ = Text::new("VOL", Point::new(w - 240, h - 135), text_style).draw(fb);
    let _ = ui::draw_technical_bar(fb, Point::new(w - 240, h - 130), bar_size, 0.6, colors::ACCENT_CYAN);
    let _ = Text::new("ENRG", Point::new(w - 240, h - 95), text_style).draw(fb);
    let _ = ui::draw_technical_bar(fb, Point::new(w - 240, h - 90), bar_size, 0.92, colors::GLOW_HI);
}

pub fn draw_context_menu(fb: &mut FramebufferState, pos: Point) {
    let rect = Rectangle::new(pos, Size::new(200, 150));
    let bg_style = PrimitiveStyleBuilder::new().fill_color(colors::GLASS_PANEL).stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
    let _ = rect.into_styled(bg_style).draw(fb);
    let _ = Rectangle::new(pos + Point::new(2, 2), Size::new(196, 2)).into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build()).draw(fb);
    let menu_glow = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, Rgb888::new(40, 120, 180));
    let menu_title = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
    let _ = Text::new("MENU", pos + Point::new(16, 19), menu_glow).draw(fb);
    let _ = Text::new("MENU", pos + Point::new(15, 18), menu_title).draw(fb);
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
    let items = ["Nueva Ventana", "Configurar Fondo", "Cambiar Tema", "Propiedades"];
    for (i, item) in items.iter().enumerate() {
        let _ = Text::new(item, pos + Point::new(15, 38 + (i as i32 * 35)), text_style).draw(fb);
    }
}

pub fn window_button_hover_at(cursor_x: i32, cursor_y: i32, wx: i32, wy: i32, ww: i32) -> Option<WindowButton> {
    let btn_y = wy + (ShellWindow::TITLE_H - ui::BUTTON_ICON_SIZE as i32) / 2;
    let btn_margin = 5;
    let btn_size = ui::BUTTON_ICON_SIZE as i32;
    if cursor_y < btn_y || cursor_y >= btn_y + btn_size { return None; }
    if cursor_x < wx || cursor_x >= wx + ww { return None; }
    let close_x = wx + ww - btn_size - btn_margin;
    if cursor_x >= close_x && cursor_x < close_x + btn_size { return Some(WindowButton::Close); }
    let max_x = close_x - btn_size - btn_margin;
    if cursor_x >= max_x && cursor_x < max_x + btn_size { return Some(WindowButton::Maximize); }
    let min_x = max_x - btn_size - btn_margin;
    if cursor_x >= min_x && cursor_x < min_x + btn_size { return Some(WindowButton::Minimize); }
    None
}

#[inline(never)]
pub fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface], ws_offset: f32, _current_ws: u8, cursor_x: i32, cursor_y: i32, uptime_ticks: u64) {
    let fb_w = fb.info.width as i32;
    let mut hovered_win_idx: Option<usize> = None;
    let mut hovered_button: Option<WindowButton> = None;

    // First pass: find hovered window/button
    for (i, w) in windows.iter().take(window_count).enumerate().rev() {
        if matches!(w.content, WindowContent::None) { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        let wy = w.curr_y as i32;
        let ww = w.curr_w as i32;
        let wh = w.curr_h as i32;
        if effective_x + ww <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && ww < 50 { continue; }

        if cursor_x >= effective_x && cursor_x < effective_x + ww && cursor_y >= wy && cursor_y < wy + wh {
            if hovered_win_idx.is_none() {
                hovered_button = window_button_hover_at(cursor_x, cursor_y, effective_x, wy, ww);
                hovered_win_idx = Some(i);
            }
        }
    }

    // Second pass: draw windows (painter's algorithm)
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if matches!(w.content, WindowContent::None) { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        let ww = w.curr_w as i32;
        if effective_x + ww <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && ww < 50 { continue; }

        let focused = Some(i) == focused_window;
        let btn_hover = if hovered_win_idx == Some(i) { hovered_button.clone() } else { None };
        let _ = draw_window_advanced(fb, w, focused, surfaces, effective_x, btn_hover, uptime_ticks);
    }
}

pub fn draw_window_advanced(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool, surfaces: &[ExternalSurface], x: i32, button_hover: Option<WindowButton>, uptime_ticks: u64) -> Result<(), ()> {
    // Damage tracking removed; draw decoration fully if visible.
    draw_window_decoration_at(fb, w, is_focused, x, button_hover);

    if w.curr_w > 100.0 {
        match w.content {
            WindowContent::InternalDemo => {
                let wx = x;
                let wy = w.curr_y as i32;
                let ww = w.curr_w as i32;
                let wh = w.curr_h as i32;
                let content_top = wy + ShellWindow::TITLE_H;
                let content_h = (wh - ShellWindow::TITLE_H).max(0);
                let pad = 8;
                let cx = wx + pad;
                let cy = content_top + pad;
                let cw = (ww - pad * 2).max(0) as u32;
                let ch = (content_h - pad * 2).max(0) as u32;
                let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw, ch))
                    .into_styled(PrimitiveStyleBuilder::new().stroke_color(colors::GLOW_MID).stroke_width(1).build()).draw(fb);
                
                let prompt = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
                let text = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::WARM_WHITE);
                let _ = Text::new("> eclipse --version", Point::new(cx + 10, cy + 22), prompt).draw(fb);
                let _ = Text::new("Eclipse OS 0.1.0 // kernel 6.x", Point::new(cx + 10, cy + 42), text).draw(fb);
                let _ = Text::new("> status --active", Point::new(cx + 10, cy + 62), prompt).draw(fb);
                let mut uptime_line = heapless::String::<64>::new();
                let uptime_secs = uptime_ticks / 1000;
                let _ = core::fmt::write(&mut uptime_line, format_args!("TOTAL SERVICES: 42 // UPTIME: {}h {}m", uptime_secs / 3600, (uptime_secs / 60) % 60));
                let _ = Text::new(&uptime_line, Point::new(cx + 10, cy + 82), text).draw(fb);
                let _ = Text::new("> _", Point::new(cx + 10, cy + 102), prompt).draw(fb);
            }
            WindowContent::External(idx) => {
                if (idx as usize) < surfaces.len() && surfaces[idx as usize].active {
                    let s = &surfaces[idx as usize];
                    // Evitar Page Fault (#14): rechazar vaddr fuera de rango plausible de mmap.
                    // CR2=0x2bf086ff9e5 (~280 GiB) indica puntero corrupto o mapping ya liberado.
                    const MAX_PLAUSIBLE_VADDR: usize = 0x20_0000_0000; // 128 GiB
                    let vaddr_ok = s.vaddr != 0
                        && s.vaddr != 0x1000
                        && s.vaddr <= MAX_PLAUSIBLE_VADDR
                        && s.buffer_size != 0
                        && s.vaddr.saturating_add(s.buffer_size) <= MAX_PLAUSIBLE_VADDR;
                    if vaddr_ok {
                        let wx = x;
                        let wy = w.curr_y as i32;
                        let ww = (w.curr_w as i32).max(0);
                        let wh = (w.curr_h as i32).max(0);
                        let content_w = (ww - 10).max(0) as u32;
                        let content_h = (wh - ShellWindow::TITLE_H - 10).max(0) as u32;
                        if content_w > 0 && content_h > 0 {
                            let needed = (content_w as usize).saturating_mul(content_h as usize).saturating_mul(4);
                            if needed <= s.buffer_size {
                                let content_rect = Rectangle::new(Point::new(wx + 5, wy + ShellWindow::TITLE_H + 5), Size::new(content_w, content_h));
                                fb.blit_buffer(wx + 5, wy + ShellWindow::TITLE_H + 5, content_w, content_h, s.vaddr as *const u32, s.buffer_size);
                            }
                        }
                    }
                }
            }
            // Ventanas Wayland se dibujan a través del pipeline de sidewind/wayland y damage;
            // aquí solo mantenemos la decoración y dejamos el contenido como está.
            WindowContent::Wayland { .. } => {
                // No hacemos nada especial en el cuerpo: el contenido ya debería estar en el framebuffer.
            }
            WindowContent::None => {}
        }
    }
    Ok(())
}

pub fn draw_window_decoration_at(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool, x: i32, button_hover: Option<WindowButton>) {
    let wx = x;
    let wy = w.curr_y as i32;
    let ww = w.curr_w as i32;
    let wh = w.curr_h as i32;
    let rect = Rectangle::new(Point::new(wx, wy), Size::new(ww as u32, wh as u32));
    let accent = if is_focused { colors::ACCENT_CYAN } else { colors::GLOW_DIM };
    // Sin blur/SDF para estabilidad y menor uso de memoria (mismo estilo que smithay_wayland)

    let _ = ui::draw_glass_card(fb, rect, "ECLIPSE // TERMINAL", accent);

    if ww > 100 {
        let title_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::WHITE);
        let _ = Text::new("ECLIPSE // TERMINAL", Point::new(wx + 10, wy + 18), title_style).draw(fb);
    }
    if ww > 80 {
        let btn_y = wy + (ShellWindow::TITLE_H - ui::BUTTON_ICON_SIZE as i32) / 2;
        let btn_margin = 5;
        let close_x = wx + ww - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let max_x = close_x - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let min_x = max_x - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let _ = ui::draw_button_icon_with_hover(
            fb,
            Point::new(close_x, btn_y),
            icons::BTN_CLOSE,
            matches!(button_hover, Some(WindowButton::Close)),
            colors::ACCENT_RED,
        );
        let _ = ui::draw_button_icon_with_hover(
            fb,
            Point::new(max_x, btn_y),
            icons::BTN_MAX,
            matches!(button_hover, Some(WindowButton::Maximize)),
            accent,
        );
        let _ = ui::draw_button_icon_with_hover(
            fb,
            Point::new(min_x, btn_y),
            icons::BTN_MIN,
            matches!(button_hover, Some(WindowButton::Minimize)),
            accent,
        );
    }
    let handle_style = PrimitiveStyleBuilder::new().stroke_color(accent).stroke_width(1).build();
    let _ = Rectangle::new(
        Point::new(wx + ww - ShellWindow::RESIZE_HANDLE_SIZE, wy + wh - ShellWindow::RESIZE_HANDLE_SIZE),
        Size::new(ShellWindow::RESIZE_HANDLE_SIZE as u32, ShellWindow::RESIZE_HANDLE_SIZE as u32)
    ).into_styled(handle_style).draw(fb);

    if is_focused {
        let corner_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_HIGHLIGHT).stroke_width(2).build();
        let c_len = 15;
        let _ = Line::new(Point::new(wx, wy), Point::new(wx + c_len, wy)).into_styled(corner_style).draw(fb);
        let _ = Line::new(Point::new(wx, wy), Point::new(wx, wy + c_len)).into_styled(corner_style).draw(fb);
        let _ = Line::new(Point::new(wx + ww, wy), Point::new(wx + ww - c_len, wy)).into_styled(corner_style).draw(fb);
        let _ = Line::new(Point::new(wx + ww, wy), Point::new(wx + ww, wy + c_len)).into_styled(corner_style).draw(fb);
    }
}

/// Shell de escritorio compartido: background, logo, sidebar, HUD.
/// Usado por smithay_wayland y Eclipse para unificar el pipeline.
pub fn draw_desktop_shell(
    fb: &mut FramebufferState,
    windows: &[ShellWindow],
    window_count: usize,
    counter: u64,
    cursor_x: i32,
    cursor_y: i32,
    log_buf: &mut [u8; 512],
    log_len: &mut usize,
) {
    draw_static_ui(fb, windows, window_count, counter, cursor_x, cursor_y, log_buf, log_len);
}

pub fn draw_static_ui(fb: &mut FramebufferState, _windows: &[ShellWindow], _window_count: usize, counter: u64, _cursor_x: i32, _cursor_y: i32, log_buf: &mut [u8; 512], log_len: &mut usize) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;

    fb.blit_background();


    let center = Point::new(w / 2, h / 2);
    let logo_r = ((w.min(h) / 2) - 120).min(280).max(120);
    let logo_rect = Rectangle::new(Point::new(center.x - logo_r, center.y - logo_r), Size::new(logo_r as u32 * 2, logo_r as u32 * 2));
    let _ = ui::draw_eclipse_logo(fb, center, counter, logo_r);

    let label_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let sidebar_width = (fb.info.width as i32 / 10).clamp(140, 220);
    let sidebar_x = 0; 
    let icon_slot_h = h / SIDEBAR_ICON_TYPES.len() as i32;
    let sidebar_y_start = 0;
    
    for (i, icon_type) in SIDEBAR_ICON_TYPES.iter().enumerate() {
        let py = sidebar_y_start + (i as i32 * icon_slot_h);
        let hover = _cursor_x >= sidebar_x && _cursor_x <= sidebar_x + sidebar_width 
                 && _cursor_y >= py && _cursor_y <= py + icon_slot_h;
        let _ = ui::draw_tech_card_icon(fb, Point::new(sidebar_x, py), *icon_type, hover, sidebar_width, icon_slot_h, counter);
    }

}

pub fn draw_hud_overlay(
    target: &mut OverlayDrawTarget,
    counter: u32,
    log_buf: &[u8],
    log_len: &usize,
) {
    let w = target.width as i32;
    let h = target.height as i32;

    let label_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let hud_line_style = PrimitiveStyleBuilder::new()
        .stroke_color(colors::GLASS_BORDER)
        .stroke_width(1)
        .build();
    let hud_bg = colors::GLASS_PANEL;

    // Este HUD replica el diseño de v0.1.6 pero dibujado dentro del overlay:
    // caja de 400x110 px, con esquinas recortadas y título "SISTEMA ONLINE".

    // Fondo de la caja
    let hud_rect = Rectangle::new(Point::new(0, 0), Size::new(w as u32, h as u32));
    let _ = hud_rect
        .into_styled(PrimitiveStyleBuilder::new().fill_color(hud_bg).build())
        .draw(target);

    // Esquinas "recortadas" como en la versión antigua
    let _ = Line::new(Point::new(w - 1, 0), Point::new(w - 21, 0))
        .into_styled(hud_line_style)
        .draw(target);
    let _ = Line::new(Point::new(w - 1, 0), Point::new(w - 1, 20))
        .into_styled(hud_line_style)
        .draw(target);
    let _ = Line::new(Point::new(0, h - 1), Point::new(20, h - 1))
        .into_styled(hud_line_style)
        .draw(target);
    let _ = Line::new(Point::new(0, h - 1), Point::new(0, h - 21))
        .into_styled(hud_line_style)
        .draw(target);

    // Título y estado "SISTEMA ONLINE" como en 0.1.6
    let dot = if (counter / 15) % 2 == 0 { "*" } else { " " };
    let _ = Text::new("SISTEMA ONLINE ", Point::new(20, 27), label_style).draw(target);
    let _ = Text::new(dot, Point::new(210, 27), label_style).draw(target);

    // Logs: mismas fuentes/espaciado que en v0.1.6
    const MAX_LOG_LINES: usize = 8;
    if *log_len > 0 && *log_len <= log_buf.len() {
        let slice = &log_buf[..*log_len];
        let logs_str = core::str::from_utf8(slice).unwrap_or("");
        let mut y_off = 45;
        let log_text_style = MonoTextStyle::new(&FONT_6X12, colors::WHITE);
        for line in logs_str.lines().take(MAX_LOG_LINES) {
            let _ = Text::new(line, Point::new(20, y_off), log_text_style).draw(target);
            y_off += 12;
        }
    }
}

pub fn draw_cursor(fb: &mut FramebufferState, pos: Point) {
    let _ = ui::draw_hud_cursor(fb, pos, colors::ACCENT_CYAN);
}

pub fn draw_stroke(fb: &mut FramebufferState, x: i32, y: i32, color_idx: u8) {
    let d = 4u32;
    let color = STROKE_COLORS[color_idx.min(4) as usize];
    let _ = Rectangle::new(Point::new(x, y), Size::new(d, d))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(fb);
}
#[inline(never)]
pub fn draw_system_central(
    fb: &mut FramebufferState, 
    _counter: u64, 
    services: &[ServiceInfo], 
    processes: &[ProcessInfo],
    process_cpu: &[f32; 32],
    process_mem: &[u64; 32],
    uptime_ticks: u64,
) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    
    let sidebar_width = (w / 10).clamp(140, 220);
    let panel_x = sidebar_width;
    let panel_w = w - sidebar_width;

    let _ = Rectangle::new(Point::new(panel_x, 0), Size::new(panel_w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 10, 25)).build())
        .draw(fb);
    let _ = ui::draw_grid(fb, Rgb888::new(20, 40, 80), 64, Point::new(panel_x, 0));

    let half_h = (h - 60) / 2;
    
    let uptime_secs = uptime_ticks / 1000;
    let mut title_buf = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut title_buf, format_args!("SISTEMA CENTRAL // SERVICIOS [UPTIME: {}h {}m]", uptime_secs / 3600, (uptime_secs / 60) % 60));
    let svc_rect = Rectangle::new(Point::new(panel_x + 20, 20), Size::new(panel_w as u32 - 40, half_h as u32));
    let _ = ui::draw_glass_card(fb, svc_rect, &title_buf, colors::ACCENT_CYAN);
    
    let header_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_CYAN);
    let text_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::WHITE);
    let row_h = 24;
    let start_y = 65;
    
    let col_id = panel_x + 40;
    let col_name = panel_x + 80;
    let col_state = panel_x + 220;
    let col_cpu = panel_x + 320;
    let col_mem = panel_x + 400;
    let col_restarts = panel_x + 490;
    let col_options = panel_x + 590;

    let cols = [("ID", col_id), ("NOMBRE", col_name), ("ESTADO", col_state), ("CPU", col_cpu), ("MEM", col_mem), ("REINICIOS", col_restarts), ("OPCIONES", col_options)];
    // FIX: Uso de iteración sin referencias para evitar tipos complejos en arrays
    for (name, x) in cols {
        let _ = Text::new(name, Point::new(x, start_y), header_style).draw(fb);
    }
    
    let mut buf = heapless::String::<16>::new();

    for (i, svc) in services.iter().enumerate() {
        let y = start_y + 25 + (i as i32 * row_h);
        if y > half_h + 20 - 20 { break; }
        
        let name_raw = core::str::from_utf8(&svc.name).unwrap_or("?");
        let name_str = match name_raw.find('\0') {
            Some(pos) => &name_raw[..pos],
            None => name_raw,
        }.trim();
        let _ = Text::new(name_str, Point::new(col_name, y), text_style).draw(fb);

        buf.clear();
        if svc.state == 0 || (svc.pid == 0 && name_str != "kernel") {
            let _ = buf.push_str("---");
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{}", svc.pid));
        }
        let _ = Text::new(&buf, Point::new(col_id, y), text_style).draw(fb);
        
        let state_str = match svc.state {
            0 => "Inactive",
            1 => "Activating",
            2 => "Active",
            3 => "Failed",
            4 => "Stopping",
            _ => "Unknown",
        };
        let state_color = match svc.state {
            2 => colors::ACCENT_GREEN,
            3 => colors::ACCENT_RED,
            _ => colors::ACCENT_YELLOW,
        };
        let _ = Text::new(state_str, Point::new(col_state, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, state_color)).draw(fb);
        
        let mut svc_cpu: f32 = 0.0;
        let mut svc_mem_kb = 0;
        for (j, p) in processes.iter().enumerate() {
            if j < process_cpu.len() && j < process_mem.len() {
                if p.pid == svc.pid && svc.pid != 0 {
                    svc_cpu = process_cpu[j];
                    svc_mem_kb = process_mem[j];
                    break;
                }
            }
        }

        buf.clear();
        let svc_cpu_f = if svc_cpu.is_nan() { 0.0 } else { svc_cpu };
        let _ = core::fmt::write(&mut buf, format_args!("{:.1}%", svc_cpu_f));
        let _ = Text::new(&buf, Point::new(col_cpu, y), text_style).draw(fb);

        buf.clear();
        if svc_mem_kb > 1024 {
            let _ = core::fmt::write(&mut buf, format_args!("{:.1} MB", svc_mem_kb as f32 / 1024.0));
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{} KB", svc_mem_kb));
        }
        let _ = Text::new(&buf, Point::new(col_mem, y), text_style).draw(fb);

        buf.clear();
        let _ = core::fmt::write(&mut buf, format_args!("{}", svc.restart_count));
        let _ = Text::new(&buf, Point::new(col_restarts, y), text_style).draw(fb);
        
        let _ = Text::new("[REINICIAR]", Point::new(col_options, y), header_style).draw(fb);
        let _ = Text::new("[PARAR]", Point::new(col_options + 100, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_RED)).draw(fb);
    }
    
    let prog_rect = Rectangle::new(Point::new(panel_x + 20, 40 + half_h), Size::new(panel_w as u32 - 40, half_h as u32));
    let _ = ui::draw_glass_card(fb, prog_rect, "SISTEMA CENTRAL // PROGRAMAS DE USUARIO", colors::ACCENT_GREEN);
    
    let start_y_prog = 40 + half_h + 45;
    let col_prog_pid = panel_x + 40;
    let col_prog_name = panel_x + 80;
    let col_prog_cpu = panel_x + 240;
    let col_prog_mem = panel_x + 340;
    let col_prog_red = panel_x + 440;
    let col_prog_options = panel_x + 590;

    let cols_prog = [("PID", col_prog_pid), ("NOMBRE", col_prog_name), ("CPU", col_prog_cpu), ("MEM", col_prog_mem), ("RED", col_prog_red), ("OPCIONES", col_prog_options)];
    // FIX: Uso de iteración sin referencias
    for (name, x) in cols_prog {
        let _ = Text::new(name, Point::new(x, start_y_prog), header_style).draw(fb);
    }

    let mut display_idx = 0;
    for (p_idx, p) in processes.iter().enumerate() {
        if p.pid <= 1 { continue; }
        
        let p_name_raw = core::str::from_utf8(&p.name).unwrap_or("?");
        let p_name = match p_name_raw.find('\0') {
            Some(pos) => &p_name_raw[..pos],
            None => p_name_raw,
        }.trim();
        
        let mut is_service = false;
        for s in services {
            let s_name_raw = core::str::from_utf8(&s.name).unwrap_or("?");
            let s_name = match s_name_raw.find('\0') {
                Some(pos) => &s_name_raw[..pos],
                None => s_name_raw,
            }.trim();
            if (p.pid != 0 && p.pid == s.pid) || p_name == s_name {
                is_service = true;
                break;
            }
        }
        if is_service { continue; }

        let y = start_y_prog + 25 + (display_idx * row_h);
        if y > h - 20 { break; }
        
        buf.clear();
        let _ = core::fmt::write(&mut buf, format_args!("{}", p.pid));
        let _ = Text::new(&buf, Point::new(col_prog_pid, y), text_style).draw(fb);
        
        let _ = Text::new(p_name, Point::new(col_prog_name, y), text_style).draw(fb);
        
        let mut cpu_val: f32 = 0.0;
        if p_idx < process_cpu.len() {
            cpu_val = process_cpu[p_idx];
        }
        buf.clear();
        let cpu_val_f = if cpu_val.is_nan() { 0.0 } else { cpu_val };
        let _ = core::fmt::write(&mut buf, format_args!("{:.1}%", cpu_val_f));
        let _ = Text::new(&buf, Point::new(col_prog_cpu, y), text_style).draw(fb);
        
        buf.clear();
        let mut mem_kb = 0u64;
        if p_idx < process_mem.len() {
            mem_kb = process_mem[p_idx];
        }
        if mem_kb > 1024 {
            let _ = core::fmt::write(&mut buf, format_args!("{:.1} MB", mem_kb as f32 / 1024.0));
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{} KB", mem_kb));
        }
        let _ = Text::new(&buf, Point::new(col_prog_mem, y), text_style).draw(fb);
        
        let _ = Text::new("0 bps", Point::new(col_prog_red, y), text_style).draw(fb);
        
        let _ = Text::new("[MATAR]", Point::new(col_prog_options, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_RED)).draw(fb);
        
        display_idx += 1;
    }
}

pub fn gpu_test_render(fb: &FramebufferState, _counter: u64) {
    if let Some(_gpu) = &fb.gpu {
    }
}
