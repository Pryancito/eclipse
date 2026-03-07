use std::prelude::v1::*;
use micromath::F32Ext;
use std::libc::{
    mmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS,
    FramebufferInfo,
};
use sidewind::ui::{self, icons, colors, Notification, NotificationPanel, Widget};
use sidewind::{font_terminus_12, font_terminus_14, font_terminus_20};
use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::primitives::{Rectangle, PrimitiveStyleBuilder, Line};
use embedded_graphics::mono_font::{ascii::{FONT_6X12, FONT_10X20}, MonoTextStyle};
use embedded_graphics::text::Text;
use crate::compositor::{ShellWindow, WindowContent, ExternalSurface, WindowButton};
use crate::state::{ServiceInfo};



pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Fallback display dimensions used when the firmware reports zero width/height.
const DEFAULT_WIDTH: u32  = 1920;
const DEFAULT_HEIGHT: u32 = 1080;

pub const STROKE_COLORS: [Rgb888; 5] = [
    colors::ACCENT_BLUE,
    colors::ACCENT_RED,
    colors::ACCENT_GREEN,
    colors::ACCENT_YELLOW,
    colors::WHITE,
];

pub struct FramebufferState {
    pub info: FramebufferInfo,
    pub back_fb_id: u32,
    pub front_fb_id: u32,
    pub back_addr: usize,
    pub front_addr: usize,
    pub background_addr: usize,
    pub gpu: Option<sidewind::gpu::GpuDevice>,
}

impl FramebufferState {
    pub fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing display via DRM...");

        use eclipse_syscall as syscall;

        // 1. Get DRM capabilities
        let caps = syscall::drm_get_caps().ok()?;
        println!("[SMITHAY] DRM Caps: {}x{}, 3D={}", caps.max_width, caps.max_height, caps.has_3d);

        let width = if caps.max_width > 0 { caps.max_width } else { DEFAULT_WIDTH };
        let height = if caps.max_height > 0 { caps.max_height } else { DEFAULT_HEIGHT };
        let pitch = width * 4;
        let fb_size = (pitch as usize) * (height as usize);
        println!("[SMITHAY] Allocating buffers: {}x{} pitch={}, size={} bytes", width, height, pitch, fb_size);

        // 2. Allocate two buffers for double buffering
        let handle1 = syscall::drm_alloc_buffer(fb_size).ok()?;
        let handle2 = syscall::drm_alloc_buffer(fb_size).ok()?;

        // 3. Create Framebuffers
        let fb1_id = syscall::drm_create_fb(handle1, width, height, pitch).ok()?;
        let fb2_id = syscall::drm_create_fb(handle2, width, height, pitch).ok()?;

        // 4. Map both buffers into userspace
        let addr1 = syscall::drm_map_handle(handle1).ok()?;
        let addr2 = syscall::drm_map_handle(handle2).ok()?;

        println!("[SMITHAY] DRM FBs created: {} and {}. Mapped at 0x{:X} and 0x{:X}", 
                 fb1_id, fb2_id, addr1, addr2);

        // 5. Initial page flip to sync kernel state
        let _ = syscall::drm_page_flip(fb1_id);

        let bg_buffer = unsafe { mmap(core::ptr::null_mut(), fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        let background_addr = if bg_buffer.is_null() || bg_buffer as usize == usize::MAX { 0 } else { bg_buffer as usize };

        let info = FramebufferInfo {
            address: addr1 as u64,
            width,
            height,
            pitch,
            bpp: 32,
            red_mask_size: 8, red_mask_shift: 16,
            green_mask_size: 8, green_mask_shift: 8,
            blue_mask_size: 8, blue_mask_shift: 0,
        };

        Some(FramebufferState {
            info,
            back_fb_id: fb2_id,
            front_fb_id: fb1_id,
            back_addr: addr2,
            front_addr: addr1,
            background_addr,
            gpu: Some(sidewind::gpu::GpuDevice::new()),
        })
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        Self {
            info: FramebufferInfo {
                address: 0,
                width: 1024,
                height: 768,
                pitch: 1024 * 4,
                bpp: 32,
                red_mask_size: 8, red_mask_shift: 16,
                green_mask_size: 8, green_mask_shift: 8,
                blue_mask_size: 8, blue_mask_shift: 0,
            },
            back_fb_id: 0,
            front_fb_id: 0,
            back_addr: 0,
            front_addr: 0,
            background_addr: 0,
            gpu: None,
        }
    }

    /// Clear the full screen. Uses GPU fill_rect when in GPU direct mode (NVIDIA BAR1), else CPU.
    pub fn clear_screen(&self, color: Rgb888) {
        let raw = 0xFF_00_00_00
            | ((color.r() as u32) << 16)
            | ((color.g() as u32) << 8)
            | (color.b() as u32);
        let w = self.info.width;
        let h = self.info.height;
        if let Some(ref gpu) = self.gpu {
            if gpu.backend() == sidewind::gpu::GpuBackend::Nvidia && self.back_addr != 0 {
                let mut enc = sidewind::gpu::GpuCommandEncoder::new(gpu);
                if enc.fill_rect(0, 0, w, h, raw).is_ok() {
                    return;
                }
            }
        }
        self.clear_back_buffer_raw(color);
    }

    pub fn clear_back_buffer_raw(&self, color: Rgb888) {
        if self.back_addr == 0 { return; }
        
        let width_px = self.info.width as usize;
        let height = self.info.height as usize;
        let pitch_px = (self.info.pitch / 4).max(self.info.width) as usize;
        let max_pixels = pitch_px.saturating_mul(height);
        
        let raw = 0xFF000000
            | ((color.r() as u32) << 16)
            | ((color.g() as u32) << 8)
            | (color.b() as u32);
        let ptr = self.back_addr as *mut u32;
        // Debug: Log buffer access on first frame or if suspicious
        if height > 0 && width_px > 0 {
             // println!("[SMITHAY] Clearing buffer at 0x{:X}, size={}x{}, pitch={}", self.back_addr, width_px, height, pitch_px);
        }
        for y in 0..height {
            let row_start = y * pitch_px;
            for x in 0..width_px {
                let offset = row_start + x;
                if offset < max_pixels {
                    unsafe {
                        core::ptr::write_volatile(ptr.add(offset), raw);
                    }
                }
            }
        }
    }

    /// Try to map the framebuffer if we're currently in headless mode (front_addr == 0).
    /// Called periodically so that if the framebuffer becomes available after startup
    /// (e.g. after NVIDIA driver initialization completes), the display will start working.
    pub fn try_remap_framebuffer(&mut self) {
        // No-op for DRM, handled in init
    }

    pub fn present_rect(&self, _x: i32, _y: i32, _w: i32, _h: i32) {
        // No-op for DRM zero-copy (full flip)
    }

    pub fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        if self.back_addr == 0 { return; }
        let ptr = self.back_addr as *mut u32;
        for py in (cy - half)..=(cy + half) {
            if py >= 0 && py < height {
                let offset = (py * pitch_px + cx) as usize;
                if offset < max_pixels {
                    unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
                }
            }
        }
        for px in (cx - half)..=(cx + half) {
            if px >= 0 && px < width {
                let offset = (cy * pitch_px + px) as usize;
                if offset < max_pixels {
                    unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
                }
            }
        }
    }

    pub fn present(&mut self) -> bool {
        if self.back_addr == 0 { return true; }
        
        let res = eclipse_syscall::drm_page_flip(self.back_fb_id);
        if res.is_ok() {
            // Swap handles for the next frame
            core::mem::swap(&mut self.back_fb_id, &mut self.front_fb_id);
            core::mem::swap(&mut self.back_addr, &mut self.front_addr);
            true
        } else {
            false
        }
    }

    pub fn present_damaged(&mut self, _rects: &[Rectangle]) -> bool {
        self.present() // Full flip for zero-copy
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
        let pitch = self.info.pitch.max(self.info.width * 4);
        let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.background_addr as *const u8,
                self.back_addr as *mut u8,
                size_bytes,
            );
        }
    }

    pub fn blit_background_damaged(&self, rects: &[Rectangle]) {
        if self.back_addr == 0 || self.background_addr == 0 || rects.is_empty() { return; }
        
        let pitch = self.info.pitch.max(self.info.width * 4) as usize;
        let fb_w = self.info.width as i32;
        let fb_h = self.info.height as i32;

        for r in rects {
            let rx = r.top_left.x.max(0);
            let ry = r.top_left.y.max(0);
            let rw = (r.size.width as i32).min(fb_w - rx);
            let rh = (r.size.height as i32).min(fb_h - ry);

            if rw <= 0 || rh <= 0 { continue; }

            for y in ry..ry + rh {
                let offset = (y as usize * pitch) + (rx as usize * 4);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (self.background_addr + offset) as *const u8,
                        (self.back_addr + offset) as *mut u8,
                        rw as usize * 4,
                    );
                }
            }
        }
    }

    pub fn blit_buffer(&mut self, x: i32, y: i32, w: u32, h: u32, src: *const u32, src_size: usize) {
        if self.back_addr == 0 {
            // Defensive: logging very rarely to avoid spamming
            return;
        }
        if src.is_null() || (src as usize) == 0 {
            return;
        }
        if w == 0 || h == 0 { return; }
        let fb_w = self.info.width as i32;
        let fb_h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(self.info.width) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(fb_h as usize);
        let dst_ptr = self.back_addr as *mut u32;
        let w_i = w as i32;
        for iy in 0..h as i32 {
            let dy = y + iy;
            if dy < 0 || dy >= fb_h { continue; }
            let src_row_start = (iy * w_i) as usize;
            let bytes_needed = (src_row_start + w as usize).saturating_mul(4);
            if bytes_needed > src_size { break; }
            if x >= 0 && x + w_i <= fb_w {
                let row_offset = (dy * pitch_px + x) as usize;
                if row_offset + (w as usize) <= max_pixels {
                    unsafe {
                        core::ptr::copy_nonoverlapping(src.add(src_row_start), dst_ptr.add(row_offset), w as usize);
                    }
                }
            } else {
                for ix in 0..w_i {
                    let dx = x + ix;
                    if dx >= 0 && dx < fb_w {
                        let off = (dy * pitch_px + dx) as usize;
                        if off < max_pixels {
                            unsafe {
                                let color = core::ptr::read_volatile(src.add(src_row_start + ix as usize));
                                core::ptr::write_volatile(dst_ptr.add(off), color);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl DrawTarget for FramebufferState {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        if self.back_addr == 0 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.back_addr as *mut u32;
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y as usize).saturating_mul(pitch_px as usize).saturating_add(coord.x as usize);
                if offset < max_pixels {
                    let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                    unsafe { core::ptr::write_volatile(fb_ptr.add(offset), raw_color); }
                }
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        if self.back_addr == 0 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.back_addr as *mut u32;
        
        let intersection = area.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() { return Ok(()); }
        
        let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        
        for y in intersection.top_left.y..intersection.top_left.y + intersection.size.height as i32 {
            if y < 0 || y >= height { continue; }
            let offset_start = (y * pitch_px) as isize;
            for x in intersection.top_left.x..intersection.top_left.x + intersection.size.width as i32 {
                if x < 0 || x >= width { continue; }
                let offset = (offset_start + x as isize) as usize;
                if offset < max_pixels {
                    unsafe { core::ptr::write_volatile(fb_ptr.add(offset), raw_color); }
                }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for FramebufferState {
    fn size(&self) -> Size {
        Size::new(self.info.width as u32, self.info.height as u32)
    }
}

impl FramebufferState {
    pub fn blur_rect(&mut self, rect: Rectangle, radius: i32) {
        if self.back_addr == 0 || radius <= 0 { return; }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let fb_ptr = self.back_addr as *mut u32;

        let inter = rect.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if inter.is_zero_sized() { return; }

        let rx = inter.top_left.x;
        let ry = inter.top_left.y;
        let rw = inter.size.width as i32;
        let rh = inter.size.height as i32;

        for y in ry..ry + rh {
            for x in rx..rx + rw {
                let mut r = 0u32;
                let mut g = 0u32;
                let mut b = 0u32;
                let mut count = 0u32;

                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let nx = x + dx;
                        let ny = y + dy;
                        if nx >= rx && nx < rx + rw && ny >= ry && ny < ry + rh {
                            let off = (ny * pitch_px + nx) as usize;
                            let color = unsafe { core::ptr::read_volatile(fb_ptr.add(off)) };
                            r += (color >> 16) & 0xFF;
                            g += (color >> 8) & 0xFF;
                            b += color & 0xFF;
                            count += 1;
                        }
                    }
                }
                if count > 0 {
                    let avg_r = r / count;
                    let avg_g = g / count;
                    let avg_b = b / count;
                    let new_color = 0xFF000000 | (avg_r << 16) | (avg_g << 8) | avg_b;
                    let off = (y * pitch_px + x) as usize;
                    unsafe { core::ptr::write_volatile(fb_ptr.add(off), new_color); }
                }
            }
        }
    }

    pub fn draw_sdf_shadow(&mut self, rect: Rectangle, radius: i32) {
        self.draw_sdf_effect(rect, radius, Rgb888::new(0, 0, 0), 120, 8.0)
    }

    pub fn draw_sdf_glow(&mut self, rect: Rectangle, radius: i32, color: Rgb888) {
        self.draw_sdf_effect(rect, radius, color, 180, 8.0)
    }

    fn draw_sdf_effect(&mut self, rect: Rectangle, radius: i32, color: Rgb888, intensity: u32, corner_radius: f32) {
        if self.back_addr == 0 || radius <= 0 { return; }
        // ... (existing bounds check) ...
        let fb_w = self.info.width as i32;
        let fb_h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(self.info.width) as i32;
        let fb_ptr = self.back_addr as *mut u32;

        let shadow_rect = Rectangle::new(
            rect.top_left - Point::new(radius, radius),
            Size::new(rect.size.width + radius as u32 * 2, rect.size.height + radius as u32 * 2)
        );
        let intersection = shadow_rect.intersection(&Rectangle::new(Point::zero(), Size::new(fb_w as u32, fb_h as u32)));
        if intersection.is_zero_sized() { return; }

        let half_w = rect.size.width as f32 / 2.0 - corner_radius;
        let half_h = rect.size.height as f32 / 2.0 - corner_radius;
        let cx = rect.top_left.x as f32 + rect.size.width as f32 / 2.0;
        let cy = rect.top_left.y as f32 + rect.size.height as f32 / 2.0;

        for y in intersection.top_left.y..intersection.top_left.y + intersection.size.height as i32 {
            for x in intersection.top_left.x..intersection.top_left.x + intersection.size.width as i32 {
                let dx = (x as f32 - cx).abs() - half_w;
                let dy = (y as f32 - cy).abs() - half_h;
                let dist = dx.max(0.0).hypot(dy.max(0.0)) + dx.min(0.0).max(dy.min(0.0)) - corner_radius;
                
                if dist > 0.0 && dist < radius as f32 {
                    let alpha = 1.0 - (dist / radius as f32);
                    let alpha_scaled = (alpha * alpha * intensity as f32) as u32; 
                    if alpha_scaled > 0 {
                        let off = (y * pitch_px + x) as usize;
                        let bg = unsafe { core::ptr::read_volatile(fb_ptr.add(off)) };
                        let r_bg = (bg >> 16) & 0xFF;
                        let g_bg = (bg >> 8) & 0xFF;
                        let b_bg = bg & 0xFF;
                        
                        let r = (r_bg * (255 - alpha_scaled) + (color.r() as u32) * alpha_scaled) / 255;
                        let g = (g_bg * (255 - alpha_scaled) + (color.g() as u32) * alpha_scaled) / 255;
                        let b = (b_bg * (255 - alpha_scaled) + (color.b() as u32) * alpha_scaled) / 255;
                        
                        let final_color = 0xFF000000 | (r << 16) | (g << 8) | b;
                        unsafe { core::ptr::write_volatile(fb_ptr.add(off), final_color); }
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
    
    // Frosted Glass effect for Dashboard (Using explicit rect)
    fb.blur_rect(Rectangle::new(main_panel.position, main_panel.size), 4);
    
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

pub fn draw_notifications(fb: &mut FramebufferState, notifications: &[Option<Notification>], curr_x: f32) {
    let h = fb.info.height as i32;
    // Filtrar notificaciones activas sin usar Vec (evita leak en bucle de renderizado)
    let mut active = [Option::<Notification>::None; 5];
    let mut count = 0;
    for n in notifications {
        if let Some(val) = n {
            if count < 5 {
                active[count] = Some(*val);
                count += 1;
            }
        }
    }
    
    // Solo renderizar si hay alguna activa (SDK NotificationPanel requiere un slice)
    if count > 0 {
        // En lugar de Vec, usamos un slice de las primeras 'count' notificaciones
        // Pero NotificationPanel.notifications requiere un &[Notification].
        // Refactorizamos: iterar y dibujar manualmente o usar un buffer intermedio.
        // Dado que sidewind es opaco, intentamos usar un slice directo si es posible.
        // Pero active es Option<Notification>. 
        // Simplificación: iterar directamente los items válidos.
        
        let mut valid_objs = [Notification { 
            title: "", body: "", 
            icon_type: 0
        }; 5];
        
        let mut valid_count = 0;
        for i in 0..count {
            if let Some(n) = active[i] {
                valid_objs[valid_count] = n;
                valid_count += 1;
            }
        }

        let panel = NotificationPanel { 
            position: Point::new(curr_x as i32, 80), 
            size: Size::new(300, h as u32 - 160), 
            notifications: &valid_objs[..valid_count] 
        };
        let _ = panel.draw(fb);
    }
}

pub fn draw_search_hud(fb: &mut FramebufferState, query: &str, selected_idx: usize, counter: u64, curr_y: f32) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let panel_w = 600;
    let panel_h = 70;
    let px = (w - panel_w) / 2;
    let py = (h / 4) + curr_y as i32;
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_PANEL).stroke_color(colors::GLOW_HI).stroke_width(2).build())
        .draw(fb);
    let _ = Rectangle::new(Point::new(px + 2, py + 2), Size::new((panel_w - 4) as u32, 2)).into_styled(PrimitiveStyleBuilder::new().fill_color(colors::GLASS_HIGHLIGHT).build()).draw(fb);
    let _ = Rectangle::new(Point::new(px - 3, py - 3), Size::new(panel_w as u32 + 6, panel_h as u32 + 6)).into_styled(PrimitiveStyleBuilder::new().stroke_color(Rgb888::new(30, 100, 200)).stroke_width(2).build()).draw(fb);
    let _ = ui::draw_glowing_hexagon(fb, Point::new(px + 40, py + 35), 18, colors::ACCENT_CYAN);
    let text_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
    let mut display_query = heapless::String::<64>::new();
    let _ = display_query.push_str("> ");
    let _ = display_query.push_str(query);
    if (counter / 30) % 2 == 0 { let _ = display_query.push('_'); }
    let _ = Text::new(&display_query, Point::new(px + 80, py + 45), text_style).draw(fb);
    if query.is_empty() {
        let hint_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::GLOW_DIM);
        let _ = Text::new("ESCRIBA EL NOMBRE DE UNA APLICACION O COMANDO...", Point::new(px + 80, py + 42), hint_style).draw(fb);
    } else {
        let results = ["EJECUTAR TERMINAL", "SISTEMA: WORKSPACE 1", "SISTEMA: WORKSPACE 2", "ANALISIS DIAGNOSTICO", "BLOQUEAR ESTACION"];
        for i in 0..results.len() {
            let ry = py + panel_h + 10 + (i as i32 * 45);
            let is_selected = i == selected_idx % results.len();
            let bg_color = if is_selected { colors::GLOW_MID } else { colors::GLASS_PANEL };
            let text_color = if is_selected { colors::WHITE } else { colors::GLOW_MID };
            let _ = Rectangle::new(Point::new(px, ry), Size::new(panel_w as u32, 40))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(bg_color).stroke_color(colors::GLOW_DIM).stroke_width(1).build()).draw(fb);
            let _ = Text::new(results[i], Point::new(px + 20, ry + 26), MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, text_color)).draw(fb);
            if is_selected { let _ = Rectangle::new(Point::new(px - 10, ry + 5), Size::new(4, 30)).into_styled(PrimitiveStyleBuilder::new().fill_color(colors::ACCENT_CYAN).build()).draw(fb); }
        }
    }
}

pub fn draw_launcher(fb: &mut FramebufferState, curr_y: f32) {
    let ly = curr_y as i32;
    let rect = Rectangle::new(Point::new(10, ly), Size::new(340, 340));
    let _ = ui::draw_glass_card(fb, rect, "EJECUTAR // SERVICIOS", colors::ACCENT_CYAN);

    let bracket_style = PrimitiveStyleBuilder::new().stroke_color(colors::ACCENT_CYAN).stroke_width(1).build();
    let tl = rect.top_left;
    let br = rect.top_left + Point::new(rect.size.width as i32, rect.size.height as i32);
    let _ = Line::new(tl, tl + Point::new(35, 0)).into_styled(bracket_style).draw(fb);
    let _ = Line::new(tl, tl + Point::new(0, 35)).into_styled(bracket_style).draw(fb);
    let _ = Line::new(br - Point::new(36, 1), br - Point::new(1, 1)).into_styled(bracket_style).draw(fb);
    let _ = Line::new(br - Point::new(1, 36), br - Point::new(1, 1)).into_styled(bracket_style).draw(fb);

    let title_glow = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, Rgb888::new(40, 120, 180));
    let title_style = MonoTextStyle::new(&font_terminus_14::FONT_TERMINUS_14, colors::ACCENT_CYAN);
    let _ = Text::new("EJECUTAR // SERVICIOS", Point::new(31, ly + 39), title_glow).draw(fb);
    let _ = Text::new("EJECUTAR // SERVICIOS", Point::new(30, ly + 38), title_style).draw(fb);

    let item_style = MonoTextStyle::new(&font_terminus_20::FONT_TERMINUS_20, colors::WHITE);
    let items = [("Terminal", icons::SYSTEM), ("Archivos", icons::FILES), ("Red", icons::NETWORK), ("Ajustes", icons::APPS)];
    for (i, (name, icon)) in items.iter().enumerate() {
        let py = ly + 75 + (i as i32 * 62);
        let _ = ui::draw_glowing_hexagon(fb, Point::new(50, py + 20), 22, colors::ACCENT_CYAN);
        let _ = ui::draw_standard_icon(fb, Point::new(50, py + 20), *icon);
        let _ = Text::new(name, Point::new(85, py + 28), item_style).draw(fb);
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

pub fn draw_alt_tab_hud(fb: &mut FramebufferState, _windows: &[ShellWindow], window_count: usize, focused: Option<usize>) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let panel_w = 600;
    let panel_h = 50;
    let px = w / 2 - panel_w / 2;
    let py = h / 2 - 250;
    let rect = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32));
    let _ = ui::draw_glass_card(fb, rect, "SEARCH // EXECUTE", colors::ACCENT_CYAN);
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
fn intersects_any(rect: &Rectangle, damage: &[Rectangle]) -> bool {
    for d in damage {
        // Simple intersection check
        let x1 = d.top_left.x; let y1 = d.top_left.y;
        let x2 = x1 + d.size.width as i32; let y2 = y1 + d.size.height as i32;
        let x3 = rect.top_left.x; let y3 = rect.top_left.y;
        let x4 = x3 + rect.size.width as i32; let y4 = y3 + rect.size.height as i32;
        
        if x1 < x4 && x2 > x3 && y1 < y4 && y2 > y3 {
            return true;
        }
    }
    false
}

pub fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface], ws_offset: f32, _current_ws: u8, cursor_x: i32, cursor_y: i32, uptime_ticks: u64, damage_rects: &[Rectangle]) {
    let fb_w = fb.info.width as i32;
    let mut hovered_win_idx: Option<usize> = None;
    let mut hovered_button: Option<WindowButton> = None;
    
    // broad-phase occlusion culling stack
    let _occluding_rects = heapless::Vec::<Rectangle, 16>::new();

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

    // Render loop from bottom to top (back to front)
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if matches!(w.content, WindowContent::None) { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        let wy = w.curr_y as i32;
        let ww = w.curr_w as i32;
        let wh = w.curr_h as i32;
        
        if effective_x + ww <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && ww < 50 { continue; }

        let window_rect = Rectangle::new(Point::new(effective_x, wy), Size::new(ww as u32, wh as u32));

        // Robust Occlusion Culling: Calculate visible damaged areas
        let mut visible_damage = heapless::Vec::<Rectangle, 16>::new();
        for &dr in damage_rects {
            if let Some(inter) = crate::damage::rect_intersection(window_rect, dr) {
                let _ = visible_damage.push(inter);
            }
        }
        if visible_damage.is_empty() { continue; }

        // Subtract every opaque window above
        for j in i + 1..window_count {
            let upper = &windows[j];
            if upper.is_opaque(surfaces) {
                let ux = upper.curr_x as i32 + (upper.workspace as i32 * fb_w) - ws_offset as i32;
                let upper_rect = Rectangle::new(Point::new(ux, upper.curr_y as i32), Size::new(upper.curr_w as u32, upper.curr_h as u32));
                
                let mut next_pass = heapless::Vec::<Rectangle, 16>::new();
                for &vr in &visible_damage {
                    let sub = crate::damage::subtract_rect(vr, upper_rect);
                    for s in sub {
                        if next_pass.len() < 16 { let _ = next_pass.push(s); }
                    }
                }
                visible_damage = next_pass;
                if visible_damage.is_empty() { break; }
            }
        }
        if visible_damage.is_empty() { continue; }

        let focused = Some(i) == focused_window;
        let btn_hover = if hovered_win_idx == Some(i) { hovered_button.clone() } else { None };
        let _ = draw_window_advanced(fb, w, focused, surfaces, effective_x, btn_hover, uptime_ticks, &visible_damage);
    }
}

pub fn draw_window_advanced(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool, surfaces: &[ExternalSurface], x: i32, button_hover: Option<WindowButton>, uptime_ticks: u64, damage: &[Rectangle]) -> Result<(), ()> {
    let _window_rect = Rectangle::new(Point::new(x, w.curr_y as i32), Size::new(w.curr_w as u32, w.curr_h as u32));
    
    // We only draw decorations if they are in damage
    // For simplicity, we draw the decoration if the window header intersects damage
    let header_rect = Rectangle::new(Point::new(x, w.curr_y as i32), Size::new(w.curr_w as u32, ShellWindow::TITLE_H as u32));
    if intersects_any(&header_rect, damage) {
        draw_window_decoration_at(fb, w, is_focused, x, button_hover);
    }

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
                    // Security: Ensure vaddr is valid and not the old placeholder 0x1000
                    if s.vaddr != 0 && s.vaddr != 0x1000 && s.buffer_size != 0 {
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
                                
                                // Precise damage blitting: only blit intersections with damage rects
                                for d in damage {
                                    let overlap = d.intersection(&content_rect);
                                    if overlap.size.width > 0 && overlap.size.height > 0 {
                                        // Calculate sub-rect coordinates relative to window content
                                        let _sub_x = overlap.top_left.x - content_rect.top_left.x;
                                        let _sub_y = overlap.top_left.y - content_rect.top_left.y;
                                        
                                        let _src_ptr = (s.vaddr as *const u32).wrapping_add((_sub_y as usize * content_w as usize) + _sub_x as usize);
                                        
                                        // FOR NOW: just blit once if there is ANY overlap, it's already an improvement over full screen.
                                        fb.blit_buffer(wx + 5, wy + ShellWindow::TITLE_H + 5, content_w, content_h, s.vaddr as *const u32, s.buffer_size);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
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
    
    // Blur background under window (Frosted Glass)
    fb.blur_rect(rect, 3);
    
    // SDF Shadow or Glow
    if is_focused {
        fb.draw_sdf_glow(rect, 20, colors::ACCENT_CYAN);
    } else {
        fb.draw_sdf_shadow(rect, 15);
    }

    let _ = ui::draw_glass_card(fb, rect, "ECLIPSE // TERMINAL", accent);

    // Glossy title glow
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
        // Top-left
        let _ = Line::new(Point::new(wx, wy), Point::new(wx + c_len, wy)).into_styled(corner_style).draw(fb);
        let _ = Line::new(Point::new(wx, wy), Point::new(wx, wy + c_len)).into_styled(corner_style).draw(fb);
        // Top-right
        let _ = Line::new(Point::new(wx + ww, wy), Point::new(wx + ww - c_len, wy)).into_styled(corner_style).draw(fb);
        let _ = Line::new(Point::new(wx + ww, wy), Point::new(wx + ww, wy + c_len)).into_styled(corner_style).draw(fb);
    }
}

pub fn draw_static_ui(fb: &mut FramebufferState, _windows: &[ShellWindow], _window_count: usize, counter: u64, _cursor_x: i32, _cursor_y: i32, damage: &[Rectangle]) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;

    if damage.is_empty() {
        fb.blit_background();
    } else {
        fb.blit_background_damaged(damage);
    }


    let center = Point::new(w / 2, h / 2);
    let logo_r = ((w.min(h) / 2) - 120).min(280).max(120);
    let logo_rect = Rectangle::new(Point::new(center.x - logo_r, center.y - logo_r), Size::new(logo_r as u32 * 2, logo_r as u32 * 2));
    if intersects_any(&logo_rect, damage) {
        let _ = ui::draw_eclipse_logo(fb, center, counter, logo_r);
    }

    let label_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    // Menú Lateral Izquierdo (Icons)
    let icon_types = [
        ui::TechCardIconType::ControlPanel,
        ui::TechCardIconType::System,
        ui::TechCardIconType::Apps,
        ui::TechCardIconType::Files,
        ui::TechCardIconType::Network,
    ];

    let sidebar_width = (fb.info.width as i32 / 10).clamp(140, 220);
    let sidebar_x = 0; 
    let icon_slot_h = h / icon_types.len() as i32;
    let sidebar_y_start = 0;
    
    for (i, icon_type) in icon_types.iter().enumerate() {
        let py = sidebar_y_start + (i as i32 * icon_slot_h);
        let icon_rect = Rectangle::new(Point::new(sidebar_x, py), Size::new(sidebar_width as u32, icon_slot_h as u32));
        if intersects_any(&icon_rect, damage) {
            let hover = _cursor_x >= sidebar_x && _cursor_x <= sidebar_x + sidebar_width 
                     && _cursor_y >= py && _cursor_y <= py + icon_slot_h;
            let _ = ui::draw_tech_card_icon(fb, Point::new(sidebar_x, py), *icon_type, hover, sidebar_width, icon_slot_h, counter);
        }
    }
    // HUD Superior
    let hud_line_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
    let hud_bg = colors::GLASS_PANEL;


    let box_w = 400;
    let rx = w - box_w - 15;
    let hud_h = 110;
    let hud_rect = Rectangle::new(Point::new(rx, 15), Size::new(box_w as u32, hud_h as u32));
    if !intersects_any(&hud_rect, damage) { return; }

    let _ = Rectangle::new(Point::new(rx, 15), Size::new(box_w as u32, hud_h as u32)).into_styled(PrimitiveStyleBuilder::new().fill_color(hud_bg).build()).draw(fb);
    let _ = Line::new(Point::new(w - 15, 15), Point::new(w - 35, 15)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(w - 15, 15), Point::new(w - 15, 35)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(rx, 15 + hud_h), Point::new(rx + 20, 15 + hud_h)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(rx, 15 + hud_h), Point::new(rx, 15 + hud_h - 20)).into_styled(hud_line_style).draw(fb);
    
    // Status header
    let dot = if (counter / 15) % 2 == 0 { "*" } else { " " };
    let _ = Text::new("SISTEMA ONLINE ", Point::new(rx + 20, 42), label_style).draw(fb);
    let _ = Text::new(dot, Point::new(rx + 210, 42), label_style).draw(fb);

    // Logs below - throttle to once every 10 frames to save syscall overhead
    static mut LOG_BUF: [u8; 512] = [0u8; 512];
    static mut LOG_LEN: usize = 0;
    if counter % 10 == 0 {
        unsafe {
            *(&raw mut LOG_LEN) = std::libc::get_logs((&raw mut LOG_BUF) as *mut u8, 512);
        }
    }

    unsafe {
        let len = *(&raw mut LOG_LEN);
        if len > 0 {
            let buf_ptr = &raw mut LOG_BUF;
            let slice = core::slice::from_raw_parts(buf_ptr as *const u8, len);
            let logs_str = core::str::from_utf8(slice).unwrap_or("");
            let mut y_off = 60;
            let log_text_style = MonoTextStyle::new(&FONT_6X12, colors::WHITE);
            for line in logs_str.lines() {
                let _ = Text::new(line, Point::new(rx + 20, 15 + y_off), log_text_style).draw(fb);
                y_off += 12;
            }
        }
    }

    // Taskbar and help text removed as requested
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
pub fn draw_system_central(
    fb: &mut FramebufferState, 
    _counter: u64, 
    services: &[ServiceInfo], 
    processes: &[std::libc::ProcessInfo],
    process_cpu: &[f32; 32],
    process_mem: &[u64; 32],
    uptime_ticks: u64,
) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    
    let sidebar_width = (w / 10).clamp(140, 220);
    let panel_x = sidebar_width;
    let panel_w = w - sidebar_width;

    // Background (shifted to not cover sidebar)
    let _ = Rectangle::new(Point::new(panel_x, 0), Size::new(panel_w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 10, 25)).build())
        .draw(fb);
    let _ = ui::draw_grid(fb, Rgb888::new(20, 40, 80), 64, Point::new(panel_x, 0));

    let _margin = 20;
    let half_h = (h - 60) / 2;
    
    // Top Half: SERVICES
    let uptime_secs = uptime_ticks / 1000;
    let mut title_buf = heapless::String::<64>::new();
    let _ = core::fmt::write(&mut title_buf, format_args!("SISTEMA CENTRAL // SERVICIOS [UPTIME: {}h {}m]", uptime_secs / 3600, (uptime_secs / 60) % 60));
    let svc_rect = Rectangle::new(Point::new(panel_x + 20, 20), Size::new(panel_w as u32 - 40, half_h as u32));
    let _ = ui::draw_glass_card(fb, svc_rect, &title_buf, colors::ACCENT_CYAN);
    
    let header_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_CYAN);
    let text_style = MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::WHITE);
    let row_h = 24;
    let start_y = 65;
    
    // Relative columns within the panel
    let col_id = panel_x + 40;
    let col_name = panel_x + 80;
    let col_state = panel_x + 220;
    let col_cpu = panel_x + 320;
    let col_mem = panel_x + 400;
    let col_restarts = panel_x + 490;
    let col_options = panel_x + 590;

    // Headers
    let cols = [("ID", col_id), ("NOMBRE", col_name), ("ESTADO", col_state), ("CPU", col_cpu), ("MEM", col_mem), ("REINICIOS", col_restarts), ("OPCIONES", col_options)];
    for (name, x) in cols.iter() {
        let _ = Text::new(name, Point::new(*x, start_y), header_style).draw(fb);
    }
    
    for (i, svc) in services.iter().enumerate() {
        let y = start_y + 25 + (i as i32 * row_h);
        if y > half_h + 20 - 20 { break; }
        
        // Name
        let name_raw = core::str::from_utf8(&svc.name).unwrap_or("?");
        let name_str = match name_raw.find('\0') {
            Some(pos) => &name_raw[..pos],
            None => name_raw,
        }.trim();
        let _ = Text::new(name_str, Point::new(col_name, y), text_style).draw(fb);

        // PID
        let mut pid_str = heapless::String::<10>::new();
        if svc.state == 0 || (svc.pid == 0 && name_str != "kernel") {
            let _ = pid_str.push_str("---");
        } else {
            let _ = core::fmt::write(&mut pid_str, format_args!("{}", svc.pid));
        }
        let _ = Text::new(&pid_str, Point::new(col_id, y), text_style).draw(fb);
        
        // State
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
        
        // Find metrics for this service
        let mut svc_cpu: f32 = 0.0;
        let mut svc_mem_kb = 0;
        for (j, p) in processes.iter().enumerate() {
            // Strict safety: check both index and pid
            if j < process_cpu.len() && j < process_mem.len() {
                if p.pid == svc.pid && svc.pid != 0 {
                    svc_cpu = process_cpu[j];
                    svc_mem_kb = process_mem[j];
                    break;
                }
            }
        }

        // CPU
        let mut cpu_str = heapless::String::<12>::new();
        let svc_cpu_f = if svc_cpu.is_nan() { 0.0 } else { svc_cpu };
        let _ = core::fmt::write(&mut cpu_str, format_args!("{:.1}%", svc_cpu_f));
        let _ = Text::new(&cpu_str, Point::new(col_cpu, y), text_style).draw(fb);

        // MEM
        let mut mem_str = heapless::String::<16>::new();
        if svc_mem_kb > 1024 {
            let _ = core::fmt::write(&mut mem_str, format_args!("{:.1} MB", svc_mem_kb as f32 / 1024.0));
        } else {
            let _ = core::fmt::write(&mut mem_str, format_args!("{} KB", svc_mem_kb));
        }
        let _ = Text::new(&mem_str, Point::new(col_mem, y), text_style).draw(fb);

        // Restarts
        let mut rest_str = heapless::String::<10>::new();
        let _ = core::fmt::write(&mut rest_str, format_args!("{}", svc.restart_count));
        let _ = Text::new(&rest_str, Point::new(col_restarts, y), text_style).draw(fb);
        
        // Options (Mock buttons)
        let _ = Text::new("[REINICIAR]", Point::new(col_options, y), header_style).draw(fb);
        let _ = Text::new("[PARAR]", Point::new(col_options + 100, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_RED)).draw(fb);
    }
    
    // Bottom Half: PROGRAMS
    let prog_rect = Rectangle::new(Point::new(panel_x + 20, 40 + half_h), Size::new(panel_w as u32 - 40, half_h as u32));
    let _ = ui::draw_glass_card(fb, prog_rect, "SISTEMA CENTRAL // PROGRAMAS DE USUARIO", colors::ACCENT_GREEN);
    
    let start_y_prog = 40 + half_h + 45;
    // Headers
    let col_prog_pid = panel_x + 40;
    let col_prog_name = panel_x + 80;
    let col_prog_cpu = panel_x + 240;
    let col_prog_mem = panel_x + 340;
    let col_prog_red = panel_x + 440;
    let col_prog_options = panel_x + 590;

    let cols_prog = [("PID", col_prog_pid), ("NOMBRE", col_prog_name), ("CPU", col_prog_cpu), ("MEM", col_prog_mem), ("RED", col_prog_red), ("OPCIONES", col_prog_options)];
    for (name, x) in cols_prog.iter() {
        let _ = Text::new(name, Point::new(*x, start_y_prog), header_style).draw(fb);
    }

    // Filter out services from process list
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
        
        // PID
        let mut pid_str = heapless::String::<10>::new();
        let _ = core::fmt::write(&mut pid_str, format_args!("{}", p.pid));
        let _ = Text::new(&pid_str, Point::new(col_prog_pid, y), text_style).draw(fb);
        
        // Name
        let _ = Text::new(p_name, Point::new(col_prog_name, y), text_style).draw(fb);
        
        // CPU
        let mut cpu_val: f32 = 0.0;
        if p_idx < process_cpu.len() {
            cpu_val = process_cpu[p_idx];
        }
        let mut cpu_str = heapless::String::<12>::new();
        let cpu_val_f = if cpu_val.is_nan() { 0.0 } else { cpu_val };
        let _ = core::fmt::write(&mut cpu_str, format_args!("{:.1}%", cpu_val_f));
        let _ = Text::new(&cpu_str, Point::new(col_prog_cpu, y), text_style).draw(fb);
        
        // MEM
        let mut mem_str = heapless::String::<16>::new();
        let mut mem_kb = 0;
        if p_idx < process_mem.len() {
            mem_kb = process_mem[p_idx];
        }
        if mem_kb > 1024 {
            let _ = core::fmt::write(&mut mem_str, format_args!("{:.1} MB", mem_kb as f32 / 1024.0));
        } else {
            let _ = core::fmt::write(&mut mem_str, format_args!("{} KB", mem_kb));
        }
        let _ = Text::new(&mem_str, Point::new(col_prog_mem, y), text_style).draw(fb);
        
        // RED
        let _ = Text::new("0 bps", Point::new(col_prog_red, y), text_style).draw(fb);
        
        // Options
        let _ = Text::new("[MATAR]", Point::new(col_prog_options, y), MonoTextStyle::new(&font_terminus_12::FONT_TERMINUS_12, colors::ACCENT_RED)).draw(fb);
        
        display_idx += 1;
    }
}

/// Test hardware-accelerated rendering using the new Gpu API
pub fn gpu_test_render(fb: &FramebufferState, _counter: u64) {
    if let Some(_gpu) = &fb.gpu {
        // Disabled to avoid stalls and serial latency in main loop
        /*
        if _counter % 60 == 0 {
            let mut encoder = sidewind::gpu::GpuCommandEncoder::new(_gpu);
            let color = match (_counter / 60) % 3 {
                0 => 0x00FF0000, 
                1 => 0x0000FF00, 
                _ => 0x000000FF, 
            };
            let _ = encoder.fill_rect(50, 50, 200, 200, color);
        }
        */
    }
}
