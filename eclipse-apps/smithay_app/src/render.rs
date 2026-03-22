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
use embedded_graphics::primitives::{Rectangle, PrimitiveStyleBuilder, Line, Circle, Polyline, RoundedRectangle, CornerRadii};
use embedded_graphics::mono_font::{ascii::{FONT_6X12, FONT_10X20, FONT_6X10}, MonoTextStyle};
use embedded_graphics::text::Text;
use crate::compositor::{ShellWindow, WindowContent, ExternalSurface, WindowButton};
use crate::state::ServiceInfo;
use eclipse_ipc::types::NetExtendedStats;
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

            let background_addr = if let Ok(db) = dev.create_dumb_buffer(dev.caps.width, dev.caps.height, 32) {
                dev.map_buffer(db.handle, db.size).unwrap_or(core::ptr::null_mut()) as usize
            } else {
                0
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
                // Passing the actual handle ensures the cursor stays visible on some drivers.
                let _ = dev.set_cursor(self.drm_crtc, x, y, self.cursor_handle);
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
        
        // Try GPU blit if available (internal VRAM to VRAM copy)
        if let Some(ref gpu) = self.gpu {
            let mut encoder = sidewind::gpu::GpuCommandEncoder::new(gpu);
            if encoder.blit(0, 0, 0, 0, self.info.width, self.info.height).is_ok() {
                return;
            }
        }

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

        // Attempt GPU acceleration if available
        if let Some(gpu) = self.gpu.as_ref() {
            let mut encoder = sidewind::gpu::GpuCommandEncoder::new(gpu);
            if encoder.fill_rect(
                intersection.top_left.x as u32,
                intersection.top_left.y as u32,
                intersection.size.width,
                intersection.size.height,
                raw_color
            ).is_ok() {
                return Ok(());
            }
        }
        
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
        let r_inv = 1.0 / (radius as f32);

        for y in inter.top_left.y..inter.top_left.y + inter.size.height as i32 {
            let row_off = y as usize * pitch_px;
            for x in inter.top_left.x..inter.top_left.x + inter.size.width as i32 {
                let dx = (x as f32 - cx).abs() - half_w;
                let dy = (y as f32 - cy).abs() - half_h;
                
                // Rounded Rect SDF approximation (O(1) per pixel)
                // dist is distance to the rounded box
                let d_max_x = dx.max(0.0);
                let d_max_y = dy.max(0.0);
                let dist = (d_max_x * d_max_x + d_max_y * d_max_y).sqrt() + dx.min(0.0).max(dy.min(0.0)) - corner_radius;
                
                if dist > 0.0 && dist < radius as f32 {
                    let alpha = 1.0 - (dist * r_inv);
                    let a_scaled = (alpha * alpha * intensity as f32) as u32; 
                    if a_scaled > 0 {
                        let off = row_off + x as usize;
                        let bg = unsafe { core::ptr::read_volatile(fb_ptr.add(off)) };
                        let inv_a = 255 - a_scaled;
                        let r = (((bg >> 16) & 0xFF) * inv_a + (color.r() as u32) * a_scaled) / 255;
                        let g = (((bg >> 8) & 0xFF) * inv_a + (color.g() as u32) * a_scaled) / 255;
                        let b = ((bg & 0xFF) * inv_a + (color.b() as u32) * a_scaled) / 255;
                        unsafe { core::ptr::write_volatile(fb_ptr.add(off), 0xFF_00_00_00 | (r << 16) | (g << 8) | b); }
                    }
                }
            }
    }
}
}

pub fn draw_network_dashboard(
    fb: &mut FramebufferState,
    counter: u64,
    stats: Option<&NetExtendedStats>,
) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let sidebar_width = (w / 10).clamp(140, 220);
    let right_area_w = w - sidebar_width;
    
    // Background dim
    let _ = Rectangle::new(Point::new(sidebar_width, 0), Size::new(right_area_w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(2, 4, 10)).build())
        .draw(fb);

    use sidewind::ui::{Panel, Widget};
    let p_w = 700;
    let p_h = 480;
    let px = sidebar_width + (right_area_w - p_w) / 2;
    let py = (h - p_h) / 2;
    let main_panel = Panel { 
        position: Point::new(px, py), 
        size: Size::new(p_w as u32, p_h as u32), 
        title: "ESTADO DE RED // CONNECTIVITY CORE" 
    };
    let _ = main_panel.draw(fb);

    if let Some(s) = stats {
        // Draw Interfaces (lo and eth0)
        let card_w = 330;
        let card_h = 360;
        let card_y = py + 60;
        
        // Loopback Card
        let lo_pos = Point::new(px + 10, card_y);
        draw_network_interface_card(
            fb, lo_pos, "lo (Loopback)", s.lo_up != 0, 
            &s.lo_ipv4, s.lo_ipv4_prefix, &s.lo_ipv6, s.lo_ipv6_prefix, 
            None, None, None, None, counter
        );
        
        // Ethernet Card
        let eth_pos = Point::new(px + p_w - card_w - 10, card_y);
        draw_network_interface_card(
            fb, eth_pos, "eth0 (Physical)", s.eth0_up != 0, 
            &s.eth0_ipv4, s.eth0_ipv4_prefix, &s.eth0_ipv6, s.eth0_ipv6_prefix, 
            Some(&s.eth0_gateway), Some(&s.eth0_gateway_ipv6), 
            Some(&s.eth0_dns), Some(&s.eth0_dns_ipv6), counter
        );

        // Add RENOVAR IP (DHCP) Button at the bottom center of the panel
        let btn_w = 200;
        let btn_h = 30;
        let btn_x = px + p_w / 2 - btn_w / 2;
        let btn_y = py + p_h - btn_h - 20;

        let btn_rect = Rectangle::new(Point::new(btn_x, btn_y), Size::new(btn_w as u32, btn_h as u32));
        let fill = if (counter / 30) % 2 == 0 { colors::ACCENT_CYAN } else { colors::ACCENT_BLUE };
        
        let _ = RoundedRectangle::with_equal_corners(btn_rect, Size::new(4, 4))
            .into_styled(PrimitiveStyleBuilder::new().fill_color(fill).stroke_color(colors::WHITE).stroke_width(1).build()).draw(fb);
        let btn_text_style = MonoTextStyle::new(&FONT_10X20, colors::BACKGROUND_DEEP);
        let text_w = 17 * 10; 
        let _ = Text::new("RENOVAR IP (DHCP)", Point::new(btn_x + (btn_w - text_w) / 2, btn_y + 20), btn_text_style).draw(fb);

    } else {
        let text_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_RED);
        let _ = Text::new("ESPERANDO DATOS DEL SERVICIO DE RED...", Point::new(px + 60, py + 200), text_style).draw(fb);
    }
}

fn format_ipv6(ipv6: &[u8; 16], prefix: Option<u8>) -> heapless::String<128> {
    use core::fmt::Write;
    let mut ipv6_str = heapless::String::<128>::new();
    let mut words = [0u16; 8];
    for i in 0..8 {
        words[i] = u16::from_be_bytes([ipv6[i*2], ipv6[i*2+1]]);
    }
    let mut zero_start = -1;
    let mut zero_len = 0;
    let mut best_start = -1;
    let mut best_len = 0;
    for i in 0..8 {
        if words[i] == 0 {
            if zero_start == -1 { zero_start = i as i32; zero_len = 1; }
            else { zero_len += 1; }
        } else {
            if zero_len > best_len { best_len = zero_len; best_start = zero_start; }
            zero_start = -1; zero_len = 0;
        }
    }
    if zero_len > best_len { best_len = zero_len; best_start = zero_start; }

    if best_len >= 2 {
        for i in 0..8 {
            if i as i32 == best_start {
                let _ = write!(&mut ipv6_str, "::");
            } else if i as i32 > best_start && (i as i32) < best_start + best_len {
                continue;
            } else {
                let show_sep = i < 7 && (i as i32 + 1 != best_start);
                let _ = write!(&mut ipv6_str, "{:x}{}", words[i], if show_sep { ":" } else { "" });
            }
        }
    } else {
        for i in 0..8 {
            let _ = write!(&mut ipv6_str, "{:x}{}", words[i], if i < 7 { ":" } else { "" });
        }
    }
    if let Some(p) = prefix {
        let _ = write!(&mut ipv6_str, "/{}", p);
    }
    ipv6_str
}

fn draw_network_interface_card(
    fb: &mut FramebufferState,
    pos: Point,
    name: &str,
    is_up: bool,
    ipv4: &[u8; 4],
    ipv4_prefix: u8,
    ipv6: &[u8; 16],
    ipv6_prefix: u8,
    gw_v4: Option<&[u8; 4]>,
    gw_v6: Option<&[u8; 16]>,
    dns_v4: Option<&[u8; 4]>,
    dns_v6: Option<&[u8; 16]>,
    counter: u64,
) {
    let w = 330;
    let h = 360;
    let color = if is_up { colors::ACCENT_CYAN } else { colors::ACCENT_RED };
    
    // Glass card effect
    let _ = RoundedRectangle::with_equal_corners(
        Rectangle::new(pos, Size::new(w as u32, h as u32)),
        Size::new(8, 8)
    ).into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_FROSTED).stroke_color(color).stroke_width(2).build()).draw(fb);
    
    // Title
    let title_style = MonoTextStyle::new(&FONT_10X20, color);
    let _ = Text::new(name, pos + Point::new(20, 30), title_style).draw(fb);
    
    // Status indicator
    let status_text = if is_up { "● ONLINE" } else { "○ OFFLINE" };
    let _ = Text::new(status_text, pos + Point::new(w as i32 - 120, 30), title_style).draw(fb);
    
    // IPs
    let info_style = MonoTextStyle::new(&FONT_6X12, colors::WHITE);
    let mut ip_str = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut ip_str, format_args!("IPv4: {}.{}.{}.{}/{}", ipv4[0], ipv4[1], ipv4[2], ipv4[3], ipv4_prefix));
    let _ = Text::new(&ip_str, pos + Point::new(20, 70), info_style).draw(fb);
    
    use core::fmt::Write;
    let mut ipv6_str = heapless::String::<128>::new();
    let _ = write!(&mut ipv6_str, "IPv6: {}", format_ipv6(ipv6, Some(ipv6_prefix)));
    let _ = Text::new(&ipv6_str, pos + Point::new(20, 95), info_style).draw(fb);

    if let (Some(gw4), Some(gw6), Some(dns4), Some(dns6)) = (gw_v4, gw_v6, dns_v4, dns_v6) {
        let label_style = MonoTextStyle::new(&FONT_6X12, colors::ACCENT_CYAN);
        
        // Gateway Section
        let _ = Text::new("Gateway:", pos + Point::new(20, 150), label_style).draw(fb);
        let mut gw4_str = heapless::String::<64>::new();
        let _ = core::fmt::write(&mut gw4_str, format_args!("IPv4: {}.{}.{}.{}", gw4[0], gw4[1], gw4[2], gw4[3]));
        let _ = Text::new(&gw4_str, pos + Point::new(20, 170), info_style).draw(fb);
        
        let mut gw6_str = heapless::String::<128>::new();
        let _ = write!(&mut gw6_str, "IPv6: {}", format_ipv6(gw6, None));
        let _ = Text::new(&gw6_str, pos + Point::new(20, 195), info_style).draw(fb);

        // DNS Section
        let _ = Text::new("DNS Server:", pos + Point::new(20, 250), label_style).draw(fb);
        let mut dns4_str = heapless::String::<64>::new();
        let _ = core::fmt::write(&mut dns4_str, format_args!("IPv4: {}.{}.{}.{}", dns4[0], dns4[1], dns4[2], dns4[3]));
        let _ = Text::new(&dns4_str, pos + Point::new(20, 270), info_style).draw(fb);
        
        let mut dns6_str = heapless::String::<128>::new();
        let _ = write!(&mut dns6_str, "IPv6: {}", format_ipv6(dns6, None));
        let _ = Text::new(&dns6_str, pos + Point::new(20, 295), info_style).draw(fb);
    }

    // Decorative bits
    if is_up && (counter / 30) % 2 == 0 {
        let _ = Circle::with_center(pos + Point::new(w as i32 - 30, h as i32 - 30), 6)
            .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::ACCENT_CYAN).build()).draw(fb);
    }
}

fn draw_traffic_monitor(
    fb: &mut FramebufferState,
    pos: Point,
    w: i32,
    h: i32,
    rx: u64,
    tx: u64,
    counter: u64,
) {
    let _ = Rectangle::new(pos, Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::BACKGROUND_DEEP).stroke_color(colors::GLASS_BORDER).stroke_width(1).build()).draw(fb);
    
    let text_style = MonoTextStyle::new(&FONT_6X12, colors::WHITE);
    let mut rx_str = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut rx_str, format_args!("RX TOTAL: {} KB", rx / 1024));
    let _ = Text::new(&rx_str, pos + Point::new(20, 30), text_style).draw(fb);
    
    let mut tx_str = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut tx_str, format_args!("TX TOTAL: {} KB", tx / 1024));
    let _ = Text::new(&tx_str, pos + Point::new(20, 50), text_style).draw(fb);

    // Mini graph (simulated)
    let graph_x = pos.x + 20;
    let graph_y = pos.y + 70;
    let graph_w = w - 40;
    let graph_h = h - 90;
    
    let line_style = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_BLUE).stroke_width(1).build();
    let _ = Rectangle::new(Point::new(graph_x, graph_y), Size::new(graph_w as u32, graph_h as u32))
        .into_styled(PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(20, 40, 80)).stroke_width(1).build()).draw(fb);

    for i in 0..graph_w/4 {
        let x = graph_x + i * 4;
        let h_val = (((counter as i32 + i * 10) as f32 * 0.1).sin().abs() * graph_h as f32 * 0.7) as i32;
        let _ = Line::new(Point::new(x, graph_y + graph_h), Point::new(x, graph_y + graph_h - h_val))
            .into_styled(line_style).draw(fb);
    }
}

pub fn draw_dashboard(
    fb: &mut FramebufferState, 
    counter: u64, 
    cpu: f32, 
    mem: f32, 
    net: f32, 
    cpu_temp: u32,
    gpu_load: u32,
    gpu_temp: u32,
    anomalies: u32,
    frag: u32,
    uptime_ticks: u64,
    cpu_count: u64,
    mem_total_kb: u64,
    gpu_vram_total_kb: u64,
) {
    let cpu = if cpu.is_finite() { cpu } else { 0.0 };
    let mem = if mem.is_finite() { mem } else { 0.0 };
    let net = if net.is_finite() { net } else { 0.0 };
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let _ = Rectangle::new(Point::new(0, 0), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(2, 4, 10)).build())
        .draw(fb);
    let _ = ui::draw_grid(fb, Rgb888::new(30, 60, 120), 64, Point::zero());
    use sidewind::ui::{Panel, Gauge, Terminal, Widget};
    let p_w = 640;
    let p_h = 420;
    let px = (w - p_w) / 2;
    let py = (h - p_h) / 2;
    let main_panel = Panel { position: Point::new(px, py), size: Size::new(p_w as u32, p_h as u32), title: "ANALISIS DE SISTEMA // DASHBOARD" };
    
    let _ = main_panel.draw(fb);
    
    // Gauges uniformes en 2 filas x 3 columnas (mismo tamaño/espaciado).
    // Panel: p_w=640. Usamos centros con margen simétrico.
    let gauge_r: u32 = 50;
    let top_y: i32 = 160;
    let bot_y: i32 = 290;
    let x0: i32 = 100;
    let x1: i32 = 320;
    let x2: i32 = 540;

    // Labels dinámicas para los gauges (Gauge.label requiere &'static str).
    // Usamos buffers estáticos y `from_utf8_unchecked` porque solo escribimos ASCII.
    static mut CPU_LABEL_BUF: [u8; 64] = [0; 64];
    static mut RAM_LABEL_BUF: [u8; 64] = [0; 64];
    static mut GPU_VRAM_LABEL_BUF: [u8; 64] = [0; 64];

    let cpu_label: &'static str = {
        let mut tmp = heapless::String::<64>::new();
        let _ = core::fmt::write(&mut tmp, format_args!("{} CPU", cpu_count));
        unsafe {
            CPU_LABEL_BUF[..tmp.len()].copy_from_slice(tmp.as_bytes());
            core::str::from_utf8_unchecked(&CPU_LABEL_BUF[..tmp.len()])
        }
    };

    let (ram_val, ram_unit) = if mem_total_kb >= 1024 * 1024 {
        (mem_total_kb / (1024 * 1024), "GB")
    } else {
        (mem_total_kb / 1024, "MB")
    };

    let ram_label: &'static str = {
        let mut tmp = heapless::String::<64>::new();
        let _ = core::fmt::write(&mut tmp, format_args!("{} {} RAM", ram_val, ram_unit));
        unsafe {
            RAM_LABEL_BUF[..tmp.len()].copy_from_slice(tmp.as_bytes());
            core::str::from_utf8_unchecked(&RAM_LABEL_BUF[..tmp.len()])
        }
    };

    let (vram_val, vram_unit) = if gpu_vram_total_kb >= 1024 * 1024 {
        (gpu_vram_total_kb / (1024 * 1024), "GB")
    } else {
        (gpu_vram_total_kb / 1024, "MB")
    };

    let gpu_vram_label: &'static str = {
        let mut tmp = heapless::String::<64>::new();
        let _ = core::fmt::write(&mut tmp, format_args!("TOTAL: {} {}", vram_val, vram_unit));
        unsafe {
            GPU_VRAM_LABEL_BUF[..tmp.len()].copy_from_slice(tmp.as_bytes());
            core::str::from_utf8_unchecked(&GPU_VRAM_LABEL_BUF[..tmp.len()])
        }
    };

    // Fila 1: CPU, RAM, RED
    let _ = Gauge { center: main_panel.position + Point::new(x0, top_y), radius: gauge_r, value: cpu, label: cpu_label, unit: "%" }.draw(fb);
    let _ = Gauge { center: main_panel.position + Point::new(x1, top_y), radius: gauge_r, value: mem, label: ram_label, unit: "%" }.draw(fb);
    let _ = Gauge { center: main_panel.position + Point::new(x2, top_y), radius: gauge_r, value: net, label: "RED INT", unit: "%" }.draw(fb);

    // Fila 2: Temperatura CPU, Carga GPU, Temperatura GPU
    let cpu_t_f = (cpu_temp as f32 / 1000.0).clamp(0.0, 1.0); // ~0-100C
    let gpu_l_f = (gpu_load as f32 / 100.0).clamp(0.0, 1.0);
    let gpu_t_f = (gpu_temp as f32 / 1000.0).clamp(0.0, 1.0);

    let _ = Gauge { center: main_panel.position + Point::new(x0, bot_y), radius: gauge_r, value: cpu_t_f, label: "TEMP CPU", unit: "C" }.draw(fb);
    let _ = Gauge { center: main_panel.position + Point::new(x1, bot_y), radius: gauge_r, value: gpu_l_f, label: gpu_vram_label, unit: "%" }.draw(fb);
    let _ = Gauge { center: main_panel.position + Point::new(x2, bot_y), radius: gauge_r, value: gpu_t_f, label: "TEMP GPU AVG", unit: "C" }.draw(fb);
/*
    let mut cpu_line = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut cpu_line, format_args!("CPU: {}% @ {:.1}C", (cpu * 100.0) as u32, cpu_temp as f32 / 10.0));
    let mut gpu_line = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut gpu_line, format_args!("GPU: {}% @ {:.1}C", gpu_load, gpu_temp as f32 / 10.0));
    
    let mut anomaly_line = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut anomaly_line, format_args!("AI-CORE SECURITY: {} ANOMALIES", anomalies));
    let mut heap_line = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut heap_line, format_args!("HEAP FRAG: {}%", frag));

    let mut uptime_line = heapless::String::<64>::new();
    let uptime_secs = uptime_ticks / 1000;
    let _ = core::fmt::write(&mut uptime_line, format_args!("UPTIME: {}h {}m", uptime_secs / 3600, (uptime_secs / 60) % 60));

    let term_lines: &[&str] = &[ 
        "eclipse@os:~$ ai-core --vitals", 
        &cpu_line,
        &gpu_line,
        &anomaly_line,
        &heap_line,
        &uptime_line,
        "> system status nominal" 
    ];
    let term = Terminal { position: main_panel.position + Point::new(380, 220), size: Size::new(240, 160), lines: term_lines };
    let _ = term.draw(fb);
    */
    let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
    let _ = Text::new("PRESIONE 'SUPER' PARA VOLVER AL ESCRITORIO", Point::new(w / 2 - 200, h - 80), label_style).draw(fb);
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
            WindowContent::Wayland { .. } if w.is_dmabuf => {
                if let Some(handle) = w.buffer_handle {
                    if let Some(ref gpu) = fb.gpu {
                        let mut encoder = sidewind::gpu::GpuCommandEncoder::new(gpu);
                        let _ = encoder.blit_from_handle(
                            handle,
                            0, 0, // src_x, src_y
                            x as u32, (w.curr_y as i32 + ShellWindow::TITLE_H) as u32, // dst_x, dst_y
                            w.curr_w as u32, (w.curr_h as i32 - ShellWindow::TITLE_H).max(0) as u32
                        );
                    }
                }
            }
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
                let _ = Text::new("Eclipse OS 0.2.0 // kernel 6.x", Point::new(cx + 10, cy + 42), text).draw(fb);
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
                    const MAX_PLAUSIBLE_VADDR: usize = 0x1_0000_0000; // 4 GiB
                    let vaddr_ok = s.vaddr >= 0x1000_0000 // Above binary base
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
                    } else if s.active {
                        // Log only if it was supposed to be active but has a weird address
                        println!("[SMITHAY] Skip render: PID {} has invalid vaddr 0x{:x}", s.pid, s.vaddr);
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
    let accent = if is_focused { 
        fb.draw_sdf_glow(&rect, 10, colors::ACCENT_CYAN);
        colors::ACCENT_CYAN 
    } else { 
        fb.draw_sdf_shadow(&rect, 8);
        colors::GLOW_DIM 
    };

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
    dashboard_active: bool,
    sys_central_active: bool,
    network_active: bool,
    cpu: f32, mem: f32, net: f32,
    cpu_temp: u32, gpu_load: u32, gpu_temp: u32,
    anomalies: u32, frag: u32, uptime_ticks: u64,
    cpu_count: u64, mem_total_kb: u64, gpu_vram_total_kb: u64,
    services: &[ServiceInfo],
    processes: &[ProcessInfo],
    process_cpu: &[f32; 32],
    process_mem: &[u64; 32],
) {
    draw_static_ui(fb, windows, window_count, counter, cursor_x, cursor_y, log_buf, log_len, 
        dashboard_active, sys_central_active, network_active,
        cpu, mem, net, cpu_temp, gpu_load, gpu_temp, anomalies, frag, uptime_ticks,
        cpu_count, mem_total_kb, gpu_vram_total_kb, services, processes, process_cpu, process_mem);
}

pub fn draw_static_ui(
    fb: &mut FramebufferState, 
    _windows: &[ShellWindow], 
    _window_count: usize, 
    counter: u64, 
    _cursor_x: i32, 
    _cursor_y: i32, 
    log_buf: &mut [u8; 512], 
    _log_len: &mut usize,
    dashboard_active: bool,
    sys_central_active: bool,
    network_active: bool,
    cpu: f32, mem: f32, net: f32,
    cpu_temp: u32, gpu_load: u32, gpu_temp: u32,
    anomalies: u32, frag: u32, uptime_ticks: u64,
    cpu_count: u64, mem_total_kb: u64, gpu_vram_total_kb: u64,
    services: &[ServiceInfo],
    processes: &[ProcessInfo],
    process_cpu: &[f32; 32],
    process_mem: &[u64; 32],
) {
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
        
        // Active view highlight bar
        let active = match icon_type {
            ui::TechCardIconType::ControlPanel => dashboard_active,
            ui::TechCardIconType::System => sys_central_active,
            ui::TechCardIconType::Network => network_active,
            _ => false,
        };
        
        let draw_hover = hover || active;
        if draw_hover {
             let _ = Rectangle::new(Point::new(sidebar_x, py + 10), Size::new(4, (icon_slot_h - 20) as u32))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::ACCENT_CYAN).build()).draw(fb);
        }

        let _ = ui::draw_tech_card_icon(fb, Point::new(sidebar_x, py), *icon_type, draw_hover, sidebar_width, icon_slot_h, counter);
    }

    if dashboard_active {
        draw_dashboard(fb, counter, cpu, mem, net, cpu_temp, gpu_load, gpu_temp, 
            anomalies, frag, uptime_ticks, cpu_count, mem_total_kb, gpu_vram_total_kb);
    }

    if sys_central_active {
        draw_system_central(fb, counter, services, processes, process_cpu, process_mem, uptime_ticks);
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

pub fn draw_pill_button<D>(target: &mut D, pos: Point, width: u32, text: &str, color: Rgb888) -> Result<(), D::Error>
where D: DrawTarget<Color = Rgb888> {
    let rect = Rectangle::new(pos, Size::new(width, 18));
    let radius = CornerRadii::new(Size::new(9, 9));
    let pill = RoundedRectangle::new(rect, radius);
    let style = PrimitiveStyleBuilder::new()
        .stroke_color(color)
        .stroke_width(1)
        .build();
    let _ = pill.into_styled(style).draw(target)?;
    
    let text_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, color);
    let tw = text.len() as i32 * 6;
    let tx = pos.x + (width as i32 - tw) / 2;
    let ty = pos.y + 13;
    let _ = Text::new(text, Point::new(tx, ty), text_style).draw(target)?;
    Ok(())
}
#[inline(never)]
pub fn draw_system_central(
    fb: &mut FramebufferState, 
    counter: u64, 
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

    let panel_w = (fb.info.width as i32) - sidebar_width - 80;
    let panel_h = (fb.info.height as i32) - 80;
    let half_h = panel_h / 2;
    let panel_x = sidebar_width + 40;
    
    let total_secs = uptime_ticks / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let mut title_buf = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut title_buf, format_args!("SISTEMA CENTRAL // SERVICIOS [UPTIME: {}h {}m]", hours, mins));

    let svc_rect = Rectangle::new(Point::new(panel_x + 20, 20), Size::new(panel_w as u32 - 40, half_h as u32));
    let _ = ui::draw_glass_card(fb, svc_rect, &title_buf, colors::ACCENT_CYAN);
    
    let header_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_CYAN);
    let text_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::WHITE);
    let text_style_cyan = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_CYAN);
    let row_h = 24;
    let start_y = 65;
    const MAX_MEM_KB: u64 = 2 * 1024 * 1024; // 2 GB in KB
    
    let col_id = panel_x + 40;
    let col_name = panel_x + 80;
    let col_state = panel_x + 220;
    let col_cpu = panel_x + 320;
    let col_mem = panel_x + 400;
    let col_restarts = panel_x + 490;
    let col_options = panel_x + 590;

    Text::new("ID", Point::new(col_id, start_y), header_style).draw(fb).ok();
    Text::new("NOMBRE", Point::new(col_name, start_y), header_style).draw(fb).ok();
    Text::new("ESTADO", Point::new(col_state, start_y), header_style).draw(fb).ok();
    Text::new("CPU", Point::new(col_cpu, start_y), header_style).draw(fb).ok();
    Text::new("MEM", Point::new(col_mem, start_y), header_style).draw(fb).ok();
    Text::new("REINICIOS", Point::new(col_restarts, start_y), header_style).draw(fb).ok();
    Text::new("OPCIONES", Point::new(col_options, start_y), header_style).draw(fb).ok();
    
    let mut buf = heapless::String::<16>::new();

    // The kernel treats the `gui` service as a one-shot launcher and clears its pid/state
    // after `exec()` into `smithay_app`. For the overlay we still want to show `gui` as
    // active when this compositor process is alive.
    #[cfg(target_vendor = "eclipse")]
    let self_pid: u32 = unsafe { libc::getpid() as u32 };
    #[cfg(not(target_vendor = "eclipse"))]
    let self_pid: u32 = 0;
    let gui_running = self_pid != 0 && processes.iter().any(|p| p.pid == self_pid);

    for (i, svc) in services.iter().enumerate() {
        let y = start_y + 25 + (i as i32 * row_h);
        if y > half_h + 20 - 20 { break; }
        
        let name_raw = core::str::from_utf8(&svc.name).unwrap_or("?");
        let name_str = match name_raw.find('\0') {
            Some(pos) => &name_raw[..pos],
            None => name_raw,
        }.trim();
        let _ = Text::new(name_str, Point::new(col_name, y), text_style).draw(fb);

        // Override displayed state/pid for the `gui` service row.
        let mut svc_state = svc.state;
        let mut svc_pid = svc.pid;
        if name_str == "gui" {
            svc_state = if gui_running { 2 } else { 0 };
            svc_pid = if gui_running { self_pid } else { 0 };
        }

        buf.clear();
        if svc_state == 0 || (svc_pid == 0 && name_str != "kernel") {
            let _ = buf.push_str("---");
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{}", svc_pid));
        }
        let _ = Text::new(&buf, Point::new(col_id, y), text_style).draw(fb);
        
        let state_str = match svc_state {
            0 => "Inactive",
            1 => "Activating",
            2 => "Active",
            3 => "Failed",
            4 => "Stopping",
            _ => "Unknown",
        };
        let (state_color, dot_color) = match svc_state {
            2 => (colors::WHITE, colors::ACCENT_GREEN),
            3 => (colors::ACCENT_RED, colors::ACCENT_RED),
            0 => (colors::GLOW_DIM, colors::GLOW_DIM),
            _ => (colors::ACCENT_YELLOW, colors::ACCENT_YELLOW),
        };
        
        // Target state dot like screenshot
        let _ = ui::draw_glowing_circle(fb, Point::new(col_state - 10, y - 5), 3, dot_color);
        let _ = Text::new(state_str, Point::new(col_state, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, state_color)).draw(fb);
        
        let mut svc_cpu: f32 = 0.0;
        let mut svc_mem_kb = 0;
        for (j, p) in processes.iter().enumerate() {
            if j < process_cpu.len() && j < process_mem.len() {
                if p.pid == svc_pid && svc_pid != 0 {
                    svc_cpu = process_cpu[j];
                    svc_mem_kb = process_mem[j];
                    break;
                }
            }
        }

        buf.clear();
        let svc_cpu_f = if svc_cpu.is_finite() { svc_cpu } else { 0.0 };
        let _ = core::fmt::write(&mut buf, format_args!("{:.1}%", svc_cpu_f));
        let _ = Text::new(&buf, Point::new(col_cpu, y), text_style).draw(fb);

        let _ = ui::draw_technical_heartbeat(
            fb,
            Point::new(col_cpu + 36, y - 9),
            Size::new(44, 12),
            counter.wrapping_add((i as u64).wrapping_mul(13)),
            colors::ACCENT_CYAN,
        );

        buf.clear();
        if svc_mem_kb > 1024 {
            let svc_mem_mb = svc_mem_kb as f32 / 1024.0;
            let svc_mem_mb_f = if svc_mem_mb.is_finite() { svc_mem_mb } else { 0.0 };
            let _ = core::fmt::write(&mut buf, format_args!("{:.1} MB", svc_mem_mb_f));
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{} KB", svc_mem_kb));
        }
        let _ = Text::new(&buf, Point::new(col_mem, y), text_style).draw(fb);

        let mem_frac = (svc_mem_kb as f32 / MAX_MEM_KB as f32).clamp(0.0, 1.0);
        let _ = ui::draw_technical_bar(
            fb,
            Point::new(col_mem + 50, y - 8),
            Size::new(32, 8),
            mem_frac,
            colors::ACCENT_CYAN,
        );

        buf.clear();
        let _ = core::fmt::write(&mut buf, format_args!("{}", svc.restart_count));
        let _ = Text::new(&buf, Point::new(col_restarts, y), text_style).draw(fb);
        
        let _ = draw_pill_button(fb, Point::new(col_options, y - 12), 80, "REINICIAR", colors::ACCENT_CYAN);
        let _ = draw_pill_button(fb, Point::new(col_options + 90, y - 12), 60, "PARAR", colors::ACCENT_RED);
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

    Text::new("PID", Point::new(col_prog_pid, start_y_prog), header_style).draw(fb).ok();
    Text::new("NOMBRE", Point::new(col_prog_name, start_y_prog), header_style).draw(fb).ok();
    Text::new("CPU", Point::new(col_prog_cpu, start_y_prog), header_style).draw(fb).ok();
    Text::new("MEM", Point::new(col_prog_mem, start_y_prog), header_style).draw(fb).ok();
    Text::new("RED", Point::new(col_prog_red, start_y_prog), header_style).draw(fb).ok();
    Text::new("OPCIONES", Point::new(col_prog_options, start_y_prog), header_style).draw(fb).ok();

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
        let cpu_val_f = if cpu_val.is_finite() { cpu_val } else { 0.0 };
        let _ = core::fmt::write(&mut buf, format_args!("{:.1}%", cpu_val_f));
        let _ = Text::new(&buf, Point::new(col_prog_cpu, y), text_style).draw(fb);

        let _ = ui::draw_technical_heartbeat(
            fb,
            Point::new(col_prog_cpu + 36, y - 9),
            Size::new(58, 12),
            counter.wrapping_add((display_idx as u64).wrapping_mul(17)),
            colors::ACCENT_GREEN,
        );
        
        buf.clear();
        let mut mem_kb = 0u64;
        if p_idx < process_mem.len() {
            mem_kb = process_mem[p_idx];
        }
        if mem_kb > 1024 {
            let mem_mb = mem_kb as f32 / 1024.0;
            let mem_mb_f = if mem_mb.is_finite() { mem_mb } else { 0.0 };
            let _ = core::fmt::write(&mut buf, format_args!("{:.1} MB", mem_mb_f));
        } else {
            let _ = core::fmt::write(&mut buf, format_args!("{} KB", mem_kb));
        }
        let _ = Text::new(&buf, Point::new(col_prog_mem, y), text_style).draw(fb);

        let prog_mem_frac = (mem_kb as f32 / MAX_MEM_KB as f32).clamp(0.0, 1.0);
        let _ = ui::draw_technical_bar(
            fb,
            Point::new(col_prog_mem + 62, y - 8),
            Size::new(30, 8),
            prog_mem_frac,
            colors::ACCENT_GREEN,
        );
        
        let _ = Text::new("0.0 bps", Point::new(col_prog_red, y - 6), text_style).draw(fb);
        let _ = Text::new("0.0 bps", Point::new(col_prog_red, y + 6), text_style_cyan).draw(fb);
        let _ = ui::draw_technical_heartbeat(
            fb,
            Point::new(col_prog_red + 56, y - 9),
            Size::new(84, 14),
            counter.wrapping_add((display_idx as u64).wrapping_mul(11)),
            colors::ACCENT_CYAN,
        );
        let _ = ui::draw_technical_heartbeat(
            fb,
            Point::new(col_prog_red + 56, y - 4),
            Size::new(84, 14),
            counter.wrapping_add((display_idx as u64).wrapping_mul(11)).wrapping_add(30),
            colors::ACCENT_VIOLET,
        );
        
        let _ = draw_pill_button(fb, Point::new(col_prog_options, y - 12), 65, "MATAR", colors::ACCENT_RED);
        
        display_idx += 1;
    }
}

pub fn gpu_test_render(fb: &FramebufferState, _counter: u64) {
    if let Some(_gpu) = &fb.gpu {
    }
}
