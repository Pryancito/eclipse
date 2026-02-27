extern crate alloc;
use alloc::vec::Vec;
use core::alloc::Layout;
use embedded_graphics::{
    pixelcolor::{Rgb888, RgbColor},
    prelude::*,
    primitives::{Rectangle, Circle, Line, Polyline, PrimitiveStyleBuilder, Arc},
    text::Text,
    mono_font::MonoTextStyle,
    image::ImageRaw,
};
use eclipse_libc::{
    println, get_framebuffer_info, map_framebuffer, FramebufferInfo, 
    get_gpu_display_info, gpu_alloc_display_buffer, gpu_present, 
    mmap, munmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS, MAP_SHARED, O_RDWR
};
use sidewind_sdk::ui::{self, icons, colors, Notification, NotificationPanel, Taskbar, Widget};
use sidewind_sdk::{font_terminus_12, font_terminus_14, font_terminus_20, font_terminus_24};
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_10X20};
use micromath::F32Ext;

use crate::compositor::{ShellWindow, WindowContent, ExternalSurface, WindowButton, MAX_SURFACE_DIM};


pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_9000_0000_0000;

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
}

impl FramebufferState {
    pub fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing display...");

        let mut dims = [0u32, 0u32];
        let has_gpu = get_gpu_display_info(&mut dims);
        if has_gpu && dims[0] > 0 && dims[1] > 0 {
            let gpu_opt = gpu_alloc_display_buffer(dims[0], dims[1]);
            if let Some(gpu_info) = gpu_opt {
                if gpu_info.vaddr >= 0x1000 {
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
                    let bg_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                    if bg_buffer == 0 || bg_buffer == u64::MAX {
                        return None;
                    }

                    return Some(FramebufferState {
                        info,
                        base_addr: gpu_info.vaddr as usize,
                        front_addr: 0,
                        gpu_resource_id: Some(gpu_info.resource_id),
                        background_addr: bg_buffer as usize,
                    });
                }
            }
        }

        let fb_info = get_framebuffer_info()?;
        let fb_base = map_framebuffer()?;
        let fb_base = if fb_base as u64 >= PHYS_MEM_OFFSET {
            (fb_base as u64 - PHYS_MEM_OFFSET) as usize
        } else {
            fb_base
        };

        let pitch = if fb_info.pitch > 0 { fb_info.pitch } else { fb_info.width * 4 };
        let fb_size = (pitch as u64) * (fb_info.height as u64);
        let back_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        if back_buffer == 0 || back_buffer == u64::MAX {
            return None;
        }

        let mut info = fb_info;
        info.address = fb_base as u64;

        let bg_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if bg_buffer == 0 || bg_buffer == u64::MAX {
            return None;
        }

        Some(FramebufferState {
            info,
            base_addr: back_buffer as usize,
            front_addr: fb_base,
            gpu_resource_id: None,
            background_addr: bg_buffer as usize,
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
            base_addr: 0x1000,
            front_addr: 0,
            gpu_resource_id: None,
            background_addr: 0x2000,
        }
    }

    pub fn clear_back_buffer_raw(&self, color: Rgb888) {
        if self.base_addr < 0x1000 { return; }
        let pitch = self.info.pitch.max(self.info.width * 4);
        let width_px = self.info.width as usize;
        let height = self.info.height as usize;
        let pitch_px = (pitch / 4) as usize;
        let raw = 0xFF000000
            | ((color.r() as u32) << 16)
            | ((color.g() as u32) << 8)
            | (color.b() as u32);
        let ptr = self.base_addr as *mut u32;
        for y in 0..height {
            let row_start = y * pitch_px;
            for x in 0..width_px {
                unsafe {
                    core::ptr::write_volatile(ptr.add(row_start + x), raw);
                }
            }
        }
    }

    pub fn present_rect(&self, x: i32, y: i32, w: i32, h: i32) {
        if self.base_addr < 0x1000 { return; }
        if let Some(rid) = self.gpu_resource_id {
            let fb_w = self.info.width as i32;
            let fb_h = self.info.height as i32;
            let rx = x.clamp(0, fb_w);
            let ry = y.clamp(0, fb_h);
            let rw = w.clamp(0, fb_w - rx);
            let rh = h.clamp(0, fb_h - ry);
            if rw > 0 && rh > 0 {
                let _ = gpu_present(rid, rx as u32, ry as u32, rw as u32, rh as u32);
            }
        }
    }

    pub fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        if self.base_addr < 0x1000 { return; }
        let ptr = self.base_addr as *mut u32;
        for py in (cy - half)..=(cy + half) {
            if py >= 0 && py < height {
                let offset = (py * pitch_px + cx) as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
        for px in (cx - half)..=(cx + half) {
            if px >= 0 && px < width {
                let offset = (cy * pitch_px + px) as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
    }

    pub fn present(&self) -> bool {
        if self.base_addr < 0x1000 { return true; }
        let w = self.info.width;
        let h = self.info.height;
        if let Some(rid) = self.gpu_resource_id {
            gpu_present(rid, 0, 0, w, h)
        } else if self.front_addr >= 0x1000 {
            let pitch = self.info.pitch.max(self.info.width * 4);
            let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.base_addr as *const u8,
                    self.front_addr as *mut u8,
                    size_bytes,
                );
                // sfence flushes the Write-Combining buffer so the GOP framebuffer
                // update is visible to the display controller on real NVIDIA hardware.
                core::arch::asm!("sfence", options(nostack, preserves_flags));
            }
            true
        } else {
            true
        }
    }

    pub fn pre_render_background(&mut self) {
        if self.background_addr < 0x1000 { return; }
        let old_base = self.base_addr;
        self.base_addr = self.background_addr;
        self.clear_back_buffer_raw(colors::COSMIC_DEEP);
        let _ = ui::draw_cosmic_background(self);
        let logo_r = ((self.info.width.min(self.info.height) as i32) / 2 - 120).min(280).max(120);
        let cx = (self.info.width as f32 / 2.0).round() as i32;
        let cy = (self.info.height as f32 / 2.0).round() as i32;
        let _ = ui::draw_eclipse_logo(self, Point::new(cx, cy), 0, logo_r);
        self.base_addr = old_base;
    }

    pub fn blit_background(&self) {
        if self.base_addr < 0x1000 || self.background_addr < 0x1000 { return; }
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
        if self.base_addr < 0x1000 { return; }
        if src.is_null() { return; }
        if w == 0 || h == 0 { return; }
        let fb_w = self.info.width as i32;
        let fb_h = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(self.info.width) as i32;
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
                unsafe {
                    core::ptr::copy_nonoverlapping(src.add(src_row_start), dst_ptr.add(row_offset), w as usize);
                }
            } else {
                for ix in 0..w_i {
                    let dx = x + ix;
                    if dx >= 0 && dx < fb_w {
                        let off = (dy * pitch_px + dx) as usize;
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

impl DrawTarget for FramebufferState {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        if self.base_addr < 0x1000 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.base_addr as *mut u32;
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y as usize).saturating_mul(pitch_px as usize).saturating_add(coord.x as usize);
                if offset >= max_pixels { continue; }
                let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                unsafe { core::ptr::write_volatile(fb_ptr.add(offset), raw_color); }
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        if self.base_addr < 0x1000 { return Ok(()); }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let fb_ptr = self.base_addr as *mut u32;
        let intersection = area.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() { return Ok(()); }
        let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        for y in intersection.top_left.y..intersection.top_left.y + intersection.size.height as i32 {
            let offset_start = (y as usize * pitch_px as usize) + intersection.top_left.x as usize;
            for x in 0..intersection.size.width as usize {
                unsafe { core::ptr::write_volatile(fb_ptr.add(offset_start + x), raw_color); }
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

pub fn draw_dashboard(fb: &mut FramebufferState, counter: u64, cpu: f32, mem: f32, net: f32) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let _ = Rectangle::new(Point::new(0, 0), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(2, 4, 10)).build())
        .draw(fb);
    let _ = ui::draw_grid(fb, Rgb888::new(30, 60, 120), 64, Point::zero());
    use sidewind_sdk::ui::{Panel, Gauge, Terminal, Widget};
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
    
    let term_lines: &[&str] = &[ 
        "eclipse@os:~$ sysinfo --live", 
        &cpu_line,
        &mem_line,
        &net_line,
        "> system status nominal" 
    ];
    let term = Terminal { position: main_panel.position + Point::new(30, 240), size: Size::new(p_w as u32 - 60, 130), lines: term_lines };
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
        // Dado que sidewind_sdk es opaco, intentamos usar un slice directo si es posible.
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

pub fn draw_alt_tab_hud(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused: Option<usize>) {
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

pub fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface], ws_offset: f32, _current_ws: u8, cursor_x: i32, cursor_y: i32) {
    let fb_w = fb.info.width as i32;
    let mut hovered_win_idx: Option<usize> = None;
    let mut hovered_button: Option<WindowButton> = None;
    
    for (i, w) in windows.iter().take(window_count).enumerate().rev() {
        if w.content == WindowContent::None { continue; }
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
        if w.content == WindowContent::None { continue; }
        let effective_x = w.curr_x as i32 + (w.workspace as i32 * fb_w) - ws_offset as i32;
        if effective_x + w.curr_w as i32 <= 0 || effective_x >= fb_w { continue; }
        if w.minimized && w.curr_w < 50.0 { continue; }
        let focused = Some(i) == focused_window;
        let btn_hover = if hovered_win_idx == Some(i) { hovered_button } else { None };
        let _ = draw_window_advanced(fb, w, focused, surfaces, effective_x, btn_hover);
    }
}

pub fn draw_window_advanced(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool, surfaces: &[ExternalSurface], x: i32, button_hover: Option<WindowButton>) -> Result<(), ()> {
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
                let _ = Text::new("TOTAL SERVICES: 42 // UPTIME: 1h 24m", Point::new(cx + 10, cy + 82), text).draw(fb);
                let _ = Text::new("> _", Point::new(cx + 10, cy + 102), prompt).draw(fb);
            }
            WindowContent::External(idx) => {
                if (idx as usize) < surfaces.len() && surfaces[idx as usize].active {
                    let s = &surfaces[idx as usize];
                    if s.vaddr != 0 && s.buffer_size != 0 {
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
        let _ = ui::draw_button_icon_with_hover(fb, Point::new(close_x, btn_y), icons::BTN_CLOSE, button_hover == Some(WindowButton::Close), colors::ACCENT_RED);
        let _ = ui::draw_button_icon_with_hover(fb, Point::new(max_x, btn_y), icons::BTN_MAX, button_hover == Some(WindowButton::Maximize), accent);
        let _ = ui::draw_button_icon_with_hover(fb, Point::new(min_x, btn_y), icons::BTN_MIN, button_hover == Some(WindowButton::Minimize), accent);
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

pub fn draw_static_ui(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, counter: u64, _cursor_x: i32, _cursor_y: i32) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let _ = ui::draw_cosmic_background(fb);
    let mut star_seed = 0xACE1u32;
    let _ = ui::draw_starfield_cosmic(fb, &mut star_seed, Point::zero());
    let center = Point::new(w / 2, h / 2);
    let _ = ui::draw_grid(fb, Rgb888::new(18, 28, 55), 48, Point::zero());
    let logo_r = ((w.min(h) / 2) - 120).min(280).max(120);
    let _ = ui::draw_eclipse_logo(fb, center, counter, logo_r);
    let icon_color = Rgb888::new(100, 200, 255);
    let hex_size = 50;
    let positions = [(center + Point::new(-380, -120), icons::SYSTEM, "SISTEMA", Point::new(-35, 85)), (center + Point::new(-380, 120), icons::APPS, "APLICACIONES", Point::new(-60, 85)), (center + Point::new(380, -120), icons::FILES, "ARCHIVOS", Point::new(-40, 85)), (center + Point::new(380, 120), icons::NETWORK, "RED", Point::new(-15, 85))];
    let label_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    for (p, icon, label, label_off) in positions {
        let _ = ui::draw_glowing_hexagon(fb, p, hex_size, icon_color);
        let _ = ui::draw_standard_icon(fb, p, icon);
        let _ = Text::new(label, p + label_off, label_style).draw(fb);
    }
    // HUD Superior
    let hud_line_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
    let hud_bg = colors::GLASS_PANEL;

    let _ = Rectangle::new(Point::new(15, 15), Size::new(240, 50)).into_styled(PrimitiveStyleBuilder::new().fill_color(hud_bg).build()).draw(fb);
    let _ = Line::new(Point::new(15, 15), Point::new(35, 15)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(15, 15), Point::new(15, 35)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(255, 65), Point::new(235, 65)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(255, 65), Point::new(255, 45)).into_styled(hud_line_style).draw(fb);
    let _ = Text::new("APLICACIONES ACTIVAS", Point::new(30, 45), label_style).draw(fb);

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

    // Logs below
    let mut log_buf = [0u8; 512];
    let n = eclipse_libc::get_logs(&mut log_buf);
    if n > 0 {
        let logs_str = core::str::from_utf8(&log_buf[..n]).unwrap_or("");
        let mut y_off = 60;
        let log_text_style = MonoTextStyle::new(&FONT_6X10, colors::WHITE);
        for line in logs_str.lines() {
            let _ = Text::new(line, Point::new(rx + 20, 15 + y_off), log_text_style).draw(fb);
            y_off += 12;
        }
    }

    let taskbar_y = h - 44;
    let taskbar = Taskbar { width: fb.info.width as u32, y: taskbar_y as i32, active_app: None };
    let _ = taskbar.draw(fb);

    let help_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let _ = Text::new("SUPER: Dash | SUPER+L: Lock | SUPER+V: Notifs", Point::new(w - 450, h - 15), help_style).draw(fb);

    let mut min_count = 0;
    for i in 0..window_count {
        if windows[i].content != WindowContent::None && windows[i].minimized {
            let p = Point::new(100 + (min_count % 3) * 120, 250 + (min_count / 3) * 150);
            let _ = ui::draw_glowing_hexagon(fb, p, 35, colors::ACCENT_BLUE);
            match windows[i].content {
                WindowContent::External(_) => { let _ = ui::draw_hexagonal_icon(fb, p, 32, icons::APPS); },
                _ => { let _ = ui::draw_hexagonal_icon(fb, p, 32, icons::SYSTEM); },
            }
            let label = if let WindowContent::External(_) = windows[i].content { "APP" } else { "DEMO" };
            let _ = Text::new(label, p + Point::new(-15, 60), label_style).draw(fb);
            min_count += 1;
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
