use std::prelude::v1::*;
use micromath::F32Ext;
use std::libc::{
    get_framebuffer_info, map_framebuffer, FramebufferInfo, 
    get_gpu_display_info, gpu_alloc_display_buffer, gpu_present, 
    mmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS
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



pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_9000_0000_0000;

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
    pub base_addr: usize,   
    pub front_addr: usize,  
    pub gpu_resource_id: Option<u32>,  
    pub background_addr: usize, 
    pub gpu: Option<sidewind::gpu::GpuDevice>,
}

impl FramebufferState {
    pub fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing display...");

        let mut dims = [0u32, 0u32];
        let has_gpu = unsafe { get_gpu_display_info(&mut dims) };
        if has_gpu && dims[0] > 0 && dims[1] > 0 {
            let gpu_opt = unsafe { gpu_alloc_display_buffer(dims[0], dims[1]) };
            if let Some(gpu_info) = gpu_opt {
                if gpu_info.vaddr != 0 {
                    let info = FramebufferInfo {
                        address: 0,
                        width: dims[0],
                        height: dims[1],
                        pitch: if gpu_info.pitch > 0 { gpu_info.pitch } else { dims[0] * 4 },
                        bpp: 32,
                        red_mask_size: 8,
                        red_mask_shift: 16,
                        green_mask_size: 8,
                        green_mask_shift: 8,
                        blue_mask_size: 8,
                        blue_mask_shift: 0,
                    };
                    let fb_size = (info.pitch as u64) * (info.height as u64);
                    let bg_buffer = unsafe { mmap(core::ptr::null_mut(), fb_size as usize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
                    if bg_buffer.is_null() || bg_buffer as usize == usize::MAX {
                        return None;
                    }

                    return Some(FramebufferState {
                        info,
                        base_addr: gpu_info.vaddr as usize,
                        front_addr: 0,
                        gpu_resource_id: Some(gpu_info.resource_id),
                        background_addr: bg_buffer as usize,
                        gpu: Some(sidewind::gpu::GpuDevice::new()),
                    });
                }
            }
        }

        // get_framebuffer_info() can return None on real hardware when EFI GOP is
        // invalid, no VirtIO GPU is present, and the NVIDIA BAR1 is not set up yet.
        // In that case, use default 1920×1080 dimensions and run in headless mode so
        // that the compositor process always starts rather than silently doing nothing.
        let fb_info = unsafe { get_framebuffer_info() }.unwrap_or_else(|| {
            println!("[SMITHAY] WARNING: get_framebuffer_info failed, using default 1920x1080 headless");
            FramebufferInfo {
                address: 0,
                width: 0,
                height: 0,
                pitch: 0,
                bpp: 32,
                red_mask_size: 8,
                red_mask_shift: 16,
                green_mask_size: 8,
                green_mask_shift: 8,
                blue_mask_size: 8,
                blue_mask_shift: 0,
            }
        });

        // map_framebuffer() maps the physical framebuffer into process address space.
        // On real NVIDIA hardware (no VirtIO, no EFI GOP) this maps BAR1 linear aperture.
        // If it fails (e.g. early during NVIDIA init), run headless until try_remap_framebuffer succeeds.
        let front_addr = match unsafe { map_framebuffer() } {
            Some(addr) => {
                println!("[SMITHAY] Using linear framebuffer (NVIDIA BAR1 or GOP)");
                addr
            }
            None => {
                println!("[SMITHAY] WARNING: map_framebuffer failed, running headless");
                0
            }
        };

        // Use a conservative 1920x1080 default when EFI GOP reports zero dimensions
        // so that the back-buffer allocation below never uses size 0.
        let width  = if fb_info.width  > 0 { fb_info.width  } else { DEFAULT_WIDTH };
        let height = if fb_info.height > 0 { fb_info.height } else { DEFAULT_HEIGHT };
        let pitch = if fb_info.pitch > 0 { fb_info.pitch } else { width * 4 };
        let fb_size = (pitch as u64) * (height as u64);
        let back_buffer = unsafe { mmap(core::ptr::null_mut(), fb_size as usize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };

        if back_buffer.is_null() || back_buffer as usize == usize::MAX {
            return None;
        }

        let bg_buffer = unsafe { mmap(core::ptr::null_mut(), fb_size as usize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        if bg_buffer.is_null() || bg_buffer as usize == usize::MAX {
            return None;
        }

        // Use NVIDIA backend when we have a linear framebuffer (BAR1 or GOP); enables gpu_command(1, ...) for 2D ops.
        let gpu = Some(sidewind::gpu::GpuDevice::for_backend(sidewind::gpu::GpuBackend::Nvidia));

        let mut info = fb_info;
        info.width  = width;
        info.height = height;
        info.pitch  = pitch;
        info.address = front_addr as u64;

        Some(FramebufferState {
            info,
            base_addr: back_buffer as usize,
            front_addr,
            gpu_resource_id: None,
            background_addr: bg_buffer as usize,
            gpu,
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
            base_addr: 0,
            front_addr: 0,
            gpu_resource_id: None,
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
            if gpu.backend() == sidewind::gpu::GpuBackend::Nvidia && self.base_addr == self.front_addr && self.front_addr != 0 {
                let mut enc = sidewind::gpu::GpuCommandEncoder::new(gpu);
                if enc.fill_rect(0, 0, w, h, raw).is_ok() {
                    return;
                }
            }
        }
        self.clear_back_buffer_raw(color);
    }

    pub fn clear_back_buffer_raw(&self, color: Rgb888) {
        if self.base_addr == 0 { return; }
        
        let width_px = self.info.width as usize;
        let height = self.info.height as usize;
        let pitch_px = (self.info.pitch / 4).max(self.info.width) as usize;
        let max_pixels = pitch_px.saturating_mul(height);
        
        let raw = 0xFF000000
            | ((color.r() as u32) << 16)
            | ((color.g() as u32) << 8)
            | (color.b() as u32);
        let ptr = self.base_addr as *mut u32;
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
        if self.gpu_resource_id.is_some() || self.front_addr != 0 {
            return; // already mapped
        }
        if let Some(addr) = unsafe { map_framebuffer() } {
            let vaddr = if addr as u64 >= PHYS_MEM_OFFSET {
                (addr as u64 - PHYS_MEM_OFFSET) as usize
            } else {
                addr
            };
            if vaddr != 0 {
                println!("[SMITHAY] Framebuffer mapped at 0x{:X}, switching to display mode", vaddr);
                self.front_addr = vaddr;
                self.info.address = vaddr as u64;
            }
        }
    }

    pub fn present_rect(&self, x: i32, y: i32, w: i32, h: i32) {
        if self.base_addr == 0 { return; }
        if let Some(rid) = self.gpu_resource_id {
            let fb_w = self.info.width as i32;
            let fb_h = self.info.height as i32;
            let rx = x.clamp(0, fb_w);
            let ry = y.clamp(0, fb_h);
            let rw = w.clamp(0, fb_w - rx);
            let rh = h.clamp(0, fb_h - ry);
            if rw > 0 && rh > 0 {
                let _ = unsafe { gpu_present(rid, rx as u32, ry as u32, rw as u32, rh as u32) };
            }
        }
    }

    pub fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        if self.base_addr == 0 { return; }
        let ptr = self.base_addr as *mut u32;
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

    pub fn present(&self) -> bool {
        if self.base_addr == 0 { return true; }
        let w = self.info.width;
        let h = self.info.height;
        if let Some(rid) = self.gpu_resource_id {
            unsafe { gpu_present(rid, 0, 0, w, h) }
        } else if self.front_addr != 0 {
            let pitch = self.info.pitch.max(self.info.width * 4);
            let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.base_addr as *const u8,
                    self.front_addr as *mut u8,
                    size_bytes,
                );
                core::arch::asm!("sfence", options(nostack, preserves_flags));
            }
            true
        } else {
            true
        }
    }

    pub fn pre_render_background(&mut self) {
        if self.background_addr == 0 { return; }
        let old_base = self.base_addr;
        self.base_addr = self.background_addr;
        self.clear_back_buffer_raw(colors::COSMIC_DEEP);
        let _ = ui::draw_cosmic_background(self);
        let mut star_seed = 0xACE1u32;
        let _ = ui::draw_starfield_cosmic(self, &mut star_seed, Point::zero());
        let _ = ui::draw_grid(self, Rgb888::new(18, 28, 55), 48, Point::zero());
        self.base_addr = old_base;
    }

    pub fn blit_background(&self) {
        if self.base_addr == 0 || self.background_addr == 0 { return; }
        let pitch = self.info.pitch.max(self.info.width * 4);
        let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.background_addr as *const u8,
                self.base_addr as *mut u8,
                size_bytes,
            );
        }
    }

    pub fn blit_buffer(&mut self, x: i32, y: i32, w: u32, h: u32, src: *const u32, src_size: usize) {
        if self.base_addr == 0 {
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
        let dst_ptr = self.base_addr as *mut u32;
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
        if self.base_addr == 0 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.base_addr as *mut u32;
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
        if self.base_addr == 0 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.base_addr as *mut u32;
        
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

pub fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface], ws_offset: f32, _current_ws: u8, cursor_x: i32, cursor_y: i32, uptime_ticks: u64) {
    let fb_w = fb.info.width as i32;
    let mut hovered_win_idx: Option<usize> = None;
    let mut hovered_button: Option<WindowButton> = None;
    
    for (i, w) in windows.iter().take(window_count).enumerate().rev() {
        if matches!(w.content, WindowContent::None) { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        let wy = w.curr_y as i32;
        let ww = w.curr_w as i32;
        let wh = w.curr_h as i32;
        if effective_x + ww <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && ww < 50 { continue; }
        if cursor_x >= effective_x && cursor_x < effective_x + ww && cursor_y >= wy && cursor_y < wy + wh {
            hovered_button = window_button_hover_at(cursor_x, cursor_y, effective_x, wy, ww);
            hovered_win_idx = Some(i);
            break;
        }
    }
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if matches!(w.content, WindowContent::None) { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        if effective_x + w.curr_w as i32 <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && w.curr_w < 50.0 { continue; }
        let focused = Some(i) == focused_window;
        let btn_hover = if hovered_win_idx == Some(i) { hovered_button.clone() } else { None };
        let _ = draw_window_advanced(fb, w, focused, surfaces, effective_x, btn_hover, uptime_ticks);
    }
}

pub fn draw_window_advanced(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool, surfaces: &[ExternalSurface], x: i32, button_hover: Option<WindowButton>, uptime_ticks: u64) -> Result<(), ()> {
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
                                fb.blit_buffer(wx + 5, wy + ShellWindow::TITLE_H + 5, content_w, content_h, s.vaddr as *const u32, s.buffer_size);
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
    let _ = ui::draw_window_shadow(fb, rect);
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

pub fn draw_static_ui(fb: &mut FramebufferState, _windows: &[ShellWindow], _window_count: usize, counter: u64, _cursor_x: i32, _cursor_y: i32) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;

    fb.blit_background();

    let center = Point::new(w / 2, h / 2);
    let logo_r = ((w.min(h) / 2) - 120).min(280).max(120);
    let _ = ui::draw_eclipse_logo(fb, center, counter, logo_r);
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
        let hover = _cursor_x >= sidebar_x && _cursor_x <= sidebar_x + sidebar_width 
                 && _cursor_y >= py && _cursor_y <= py + icon_slot_h;
        let _ = ui::draw_tech_card_icon(fb, Point::new(sidebar_x, py), *icon_type, hover, sidebar_width, icon_slot_h, counter);
    }
    // HUD Superior
    let hud_line_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
    let hud_bg = colors::GLASS_PANEL;


    let box_w = 400;
    let rx = w - box_w - 15;
    let hud_h = 110;
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
