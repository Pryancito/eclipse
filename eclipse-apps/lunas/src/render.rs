//! Rendering pipeline for Lunas desktop.
//! Handles framebuffer management, background rendering, window compositing,
//! desktop shell drawing, and overlay rendering.

use std::prelude::v1::*;
use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::primitives::{Rectangle, PrimitiveStyleBuilder};
use embedded_graphics::mono_font::{ascii::FONT_6X12, MonoTextStyle};
use embedded_graphics::text::Text;
use embedded_graphics::geometry::{Point, Size};
use crate::compositor::{ShellWindow, WindowContent, ExternalSurface};
use crate::input::InputState;
use crate::style_engine::StyleEngine;
use crate::desktop::DesktopShell;
use crate::state::ServiceInfo;

use crate::display::{self, DisplayDevice, DisplayCaps, ControlDevice};

pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;

/// Framebuffer info from the kernel.
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub address: u64,
}

/// Central framebuffer state for rendering.
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
}

impl FramebufferState {
    #[cfg(not(test))]
    pub fn init() -> Option<Self> {
        #[cfg(target_vendor = "eclipse")]
        {
            let dev = DisplayDevice::open().ok()?;
            let fb_front = dev.create_framebuffer().ok()?;
            let fb_back  = dev.create_framebuffer().ok()?;

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

            Some(Self {
                info,
                back_fb_id: fb_back.fb_id,
                front_fb_id: fb_front.fb_id,
                back_addr: fb_back.addr,
                front_addr: fb_front.addr,
                background_addr,
                drm_fd: dev.fd as usize,
                drm_crtc: dev.crtc,
                gpu: Some(sidewind::gpu::GpuDevice::new()),
            })
        }

        #[cfg(not(target_vendor = "eclipse"))]
        {
            None
        }
    }

    /// Create a mock framebuffer for testing.
    #[cfg(test)]
    pub fn mock() -> Self {
        let info = FramebufferInfo {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            pitch: DEFAULT_WIDTH * 4,
            bpp: 32,
            address: 0,
        };
        Self {
            info,
            back_fb_id: display::control::framebuffer::Handle(0),
            front_fb_id: display::control::framebuffer::Handle(0),
            back_addr: 0x1000,
            front_addr: 0x2000,
            background_addr: 0x3000,
            drm_fd: 0,
            drm_crtc: display::control::CrtcHandle(0),
            gpu: None,
        }
    }

    /// Present the back buffer (page flip).
    pub fn present(&mut self) -> bool {
        #[cfg(all(not(test), target_vendor = "eclipse"))]
        {
            let dev = DisplayDevice {
                fd: self.drm_fd,
                caps: DisplayCaps { width: 0, height: 0, max_width: 0, max_height: 0, pitch: 0 },
                crtc: self.drm_crtc,
                connector: display::control::ConnectorHandle(0),
            };

            let _ = dev.page_flip(self.back_fb_id);
            core::mem::swap(&mut self.back_addr, &mut self.front_addr);
            core::mem::swap(&mut self.back_fb_id, &mut self.front_fb_id);
            true
        }
        #[cfg(any(test, not(target_vendor = "eclipse")))]
        {
            true
        }
    }

    /// Clear the back buffer to a solid color.
    pub fn clear_screen(&mut self, r: u8, g: u8, b: u8) {
        #[cfg(not(test))]
        {
            let pixel = 0xFF00_0000u32 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let pitch_px = self.info.pitch as usize / 4;
            let ptr = self.back_addr as *mut u32;
            for y in 0..self.info.height as usize {
                for x in 0..self.info.width as usize {
                    unsafe {
                        core::ptr::write_volatile(ptr.add(y * pitch_px + x), pixel);
                    }
                }
            }
        }
    }

    /// Pre-render the desktop background (cosmic theme).
    pub fn pre_render_background(&mut self) {
        #[cfg(not(test))]
        {
            let pitch_px = self.info.pitch as usize / 4;
            let ptr = self.background_addr as *mut u32;
            for y in 0..self.info.height as usize {
                for x in 0..self.info.width as usize {
                    // Cosmic gradient: dark blue to deep purple
                    let r = (10 + (y * 15 / self.info.height as usize)) as u8;
                    let g = (12 + (x * 8 / self.info.width as usize)) as u8;
                    let b = (30 + (y * 25 / self.info.height as usize)) as u8;
                    let pixel = 0xFF00_0000u32 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                    unsafe {
                        core::ptr::write_volatile(ptr.add(y * pitch_px + x), pixel);
                    }
                }
            }

            // Draw starfield
            let mut seed = 42u64;
            for _ in 0..200 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let sx = ((seed >> 16) as usize) % self.info.width as usize;
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let sy = ((seed >> 16) as usize) % self.info.height as usize;
                let brightness = 150 + ((seed >> 32) as u8 % 105);
                let star_pixel = 0xFF00_0000u32 | ((brightness as u32) << 16) | ((brightness as u32) << 8) | (brightness as u32);
                unsafe {
                    core::ptr::write_volatile(ptr.add(sy * pitch_px + sx), star_pixel);
                }
            }
        }
    }

    /// Blit the pre-rendered background to the back buffer.
    pub fn blit_background(&mut self) {
        #[cfg(not(test))]
        {
            let size = (self.info.pitch * self.info.height) as usize;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.background_addr as *const u8,
                    self.back_addr as *mut u8,
                    size,
                );
            }
        }
    }

    /// Blit an external surface buffer onto the back buffer.
    pub fn blit_buffer(&mut self, vaddr: usize, src_w: u32, src_h: u32, dst_x: i32, dst_y: i32) {
        #[cfg(not(test))]
        {
            let fb_w = self.info.width as i32;
            let fb_h = self.info.height as i32;
            let pitch_px = self.info.pitch as usize / 4;

            for row in 0..src_h as i32 {
                let dy = dst_y + row;
                if dy < 0 || dy >= fb_h { continue; }
                for col in 0..src_w as i32 {
                    let dx = dst_x + col;
                    if dx < 0 || dx >= fb_w { continue; }
                    let src_offset = (row as usize) * (src_w as usize) + (col as usize);
                    let dst_offset = (dy as usize) * pitch_px + (dx as usize);
                    unsafe {
                        let pixel = core::ptr::read_volatile((vaddr as *const u32).add(src_offset));
                        core::ptr::write_volatile((self.back_addr as *mut u32).add(dst_offset), pixel);
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
        I: IntoIterator<Item = embedded_graphics::Pixel<Self::Color>>,
    {
        #[cfg(not(test))]
        {
            let pitch_px = self.info.pitch as usize / 4;
            let ptr = self.back_addr as *mut u32;
            for embedded_graphics::Pixel(coord, color) in pixels {
                let x = coord.x;
                let y = coord.y;
                if x >= 0 && (x as u32) < self.info.width && y >= 0 && (y as u32) < self.info.height {
                    let pixel = 0xFF00_0000u32 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                    unsafe {
                        core::ptr::write_volatile(ptr.add(y as usize * pitch_px + x as usize), pixel);
                    }
                }
            }
        }
        #[cfg(test)]
        {
            let _ = pixels;
        }
        Ok(())
    }
}

impl OriginDimensions for FramebufferState {
    fn size(&self) -> Size {
        Size::new(self.info.width, self.info.height)
    }
}

// ── Desktop Shell Rendering ──

/// Draw the complete desktop shell (background, taskbar, windows, overlays).
pub fn draw_desktop_shell(
    fb: &mut FramebufferState,
    input: &InputState,
    windows: &[ShellWindow],
    window_count: usize,
    surfaces: &[ExternalSurface],
    desktop: &DesktopShell,
    services: &[ServiceInfo],
    service_count: usize,
    cpu_usage: f32,
    mem_usage: f32,
    net_usage: f32,
    log_buf: &[u8],
    log_len: usize,
) {
    // 1. Blit background
    fb.blit_background();

    // 2. Draw taskbar
    draw_taskbar(fb, input, desktop, cpu_usage, mem_usage);

    // 3. Draw windows (painter's algorithm: back to front)
    for i in 0..window_count {
        let w = &windows[i];
        if w.content == WindowContent::None || w.minimized || w.closing { continue; }
        if w.workspace != input.current_workspace { continue; }
        draw_window(fb, w, surfaces, input.focused_window == Some(i));
    }

    // 4. Draw overlays
    if input.dashboard_active {
        draw_dashboard(fb, cpu_usage, mem_usage, net_usage);
    }

    if input.system_central_active {
        draw_system_central(fb, services, service_count);
    }

    if input.lock_screen_active {
        draw_lock_screen(fb);
    }

    if input.launcher_active {
        draw_launcher(fb, desktop);
    }

    if input.search_active {
        draw_search_bar(fb, &input.search_query);
    }

    if input.notifications_visible {
        draw_notifications(fb, desktop);
    }

    // 5. Draw cursor
    draw_cursor(fb, input.cursor_x, input.cursor_y);

    // 6. Draw HUD overlay (kernel log)
    if log_len > 0 {
        draw_hud_overlay(fb, log_buf, log_len);
    }
}

/// Draw the bottom taskbar.
fn draw_taskbar(
    fb: &mut FramebufferState,
    input: &InputState,
    _desktop: &DesktopShell,
    cpu_usage: f32,
    mem_usage: f32,
) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let bar_h = 44;
    let bar_y = fb_h - bar_h;

    // Taskbar background
    let taskbar_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(15, 18, 35))
        .build();
    let _ = Rectangle::new(Point::new(0, bar_y), Size::new(fb_w as u32, bar_h as u32))
        .into_styled(taskbar_style)
        .draw(fb);

    // Top border
    let border_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(0, 100, 200))
        .build();
    let _ = Rectangle::new(Point::new(0, bar_y), Size::new(fb_w as u32, 1))
        .into_styled(border_style)
        .draw(fb);

    // App launcher button
    let launcher_style = PrimitiveStyleBuilder::new()
        .fill_color(if input.launcher_active { Rgb888::new(0, 128, 255) } else { Rgb888::new(30, 40, 60) })
        .build();
    let _ = Rectangle::new(Point::new(4, bar_y + 4), Size::new(36, 36))
        .into_styled(launcher_style)
        .draw(fb);

    // Launcher icon (grid of dots)
    let dot_color = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new(":::", Point::new(10, bar_y + 26), dot_color).draw(fb);

    // Clock / system tray area
    let clock_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 220));
    let _ = Text::new("LUNAS", Point::new(fb_w - 60, bar_y + 18), clock_style).draw(fb);

    // System metrics in taskbar
    let metrics_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let mut cpu_buf = [0u8; 16];
    let cpu_str = format_metric(&mut cpu_buf, "CPU:", cpu_usage);
    let _ = Text::new(cpu_str, Point::new(fb_w - 200, bar_y + 18), metrics_style).draw(fb);

    let mut mem_buf = [0u8; 16];
    let mem_str = format_metric(&mut mem_buf, "MEM:", mem_usage);
    let _ = Text::new(mem_str, Point::new(fb_w - 130, bar_y + 18), metrics_style).draw(fb);

    // Workspace indicators
    for ws in 0..4u8 {
        let ws_x = 48 + (ws as i32) * 24;
        let active = ws == input.current_workspace;
        let ws_style = PrimitiveStyleBuilder::new()
            .fill_color(if active { Rgb888::new(0, 128, 255) } else { Rgb888::new(40, 50, 70) })
            .build();
        let _ = Rectangle::new(Point::new(ws_x, bar_y + 12), Size::new(18, 18))
            .into_styled(ws_style)
            .draw(fb);
    }
}

/// Draw a single window with decorations.
fn draw_window(
    fb: &mut FramebufferState,
    window: &ShellWindow,
    surfaces: &[ExternalSurface],
    focused: bool,
) {
    let cx = window.curr_x as i32;
    let cy = window.curr_y as i32;
    let cw = window.curr_w as i32;
    let ch = window.curr_h as i32;

    if cw <= 0 || ch <= 0 { return; }

    // Window shadow
    let shadow_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(0, 0, 0))
        .build();
    let _ = Rectangle::new(Point::new(cx + 4, cy + 4), Size::new(cw as u32, ch as u32))
        .into_styled(shadow_style)
        .draw(fb);

    // Title bar
    let title_color = if focused {
        Rgb888::new(25, 40, 80)
    } else {
        Rgb888::new(20, 25, 45)
    };
    let title_style = PrimitiveStyleBuilder::new().fill_color(title_color).build();
    let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw as u32, ShellWindow::TITLE_H as u32))
        .into_styled(title_style)
        .draw(fb);

    // Title text
    let title_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 230));
    let title = window.title_str();
    let _ = Text::new(title, Point::new(cx + 8, cy + 18), title_text_style).draw(fb);

    // Close button (red circle)
    let close_x = cx + cw - 21;
    let close_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(220, 50, 50)).build();
    let _ = Rectangle::new(Point::new(close_x, cy + 6), Size::new(16, 16))
        .into_styled(close_style)
        .draw(fb);

    // Maximize button (cyan)
    let max_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 180, 220)).build();
    let _ = Rectangle::new(Point::new(close_x - 21, cy + 6), Size::new(16, 16))
        .into_styled(max_style)
        .draw(fb);

    // Minimize button (dim cyan)
    let min_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 120, 160)).build();
    let _ = Rectangle::new(Point::new(close_x - 42, cy + 6), Size::new(16, 16))
        .into_styled(min_style)
        .draw(fb);

    // Window content area
    let content_y = cy + ShellWindow::TITLE_H;
    let content_h = ch - ShellWindow::TITLE_H;
    if content_h <= 0 { return; }

    match window.content {
        WindowContent::External(s_idx) => {
            let s = s_idx as usize;
            if s < surfaces.len() && surfaces[s].active && surfaces[s].ready_to_flip {
                // Derive the blit region from the window content area, but clamp it to the
                // actually-mapped surface buffer so we never read past `vaddr`.
                let src_w = window.w.max(0) as u32;
                let intended_h = (window.h - ShellWindow::TITLE_H).max(0) as u32;
                let intended_h = intended_h.min(content_h as u32);

                // Compute the maximum number of rows available in the buffer.
                // Assume 4 bytes per pixel (ARGB8888) to ensure we do not
                // overrun the mapped region even if the window is larger than the surface.
                let max_pixels = (surfaces[s].mapped_len / 4) as u32;
                let max_h_for_buffer = if src_w > 0 {
                    max_pixels / src_w
                } else {
                    0
                };

                let src_h = intended_h.min(max_h_for_buffer);

                if src_w > 0 && src_h > 0 {
                    fb.blit_buffer(surfaces[s].vaddr, src_w, src_h, cx, content_y);
                }
            } else {
                // Loading indicator
                let loading_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(20, 25, 40)).build();
                let _ = Rectangle::new(Point::new(cx, content_y), Size::new(cw as u32, content_h as u32))
                    .into_styled(loading_style)
                    .draw(fb);
                let loading_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
                let _ = Text::new("Loading...", Point::new(cx + cw / 2 - 30, content_y + content_h / 2), loading_text).draw(fb);
            }
        }
        WindowContent::InternalDemo => {
            let demo_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(15, 20, 35)).build();
            let _ = Rectangle::new(Point::new(cx, content_y), Size::new(cw as u32, content_h as u32))
                .into_styled(demo_style)
                .draw(fb);
            let demo_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 200, 100));
            let _ = Text::new("Lunas Terminal", Point::new(cx + 8, content_y + 20), demo_text).draw(fb);
        }
        WindowContent::Wayland { .. } => {
            // Wayland content already composited
        }
        WindowContent::None => {}
    }

    // Focus highlight border
    if focused {
        let highlight_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(0, 128, 255))
            .stroke_width(2)
            .build();
        let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw as u32, ch as u32))
            .into_styled(highlight_style)
            .draw(fb);
    }
}

/// Draw the system dashboard overlay.
fn draw_dashboard(fb: &mut FramebufferState, cpu_usage: f32, mem_usage: f32, net_usage: f32) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let panel_w = 400;
    let panel_h = 300;
    let px = (fb_w - panel_w) / 2;
    let py = (fb_h - panel_h) / 2;

    // Semi-transparent background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(15, 18, 30)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 100, 200))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("SYSTEM DASHBOARD", Point::new(px + 120, py + 24), title_style).draw(fb);

    // CPU gauge
    draw_gauge(fb, px + 20, py + 50, 360, 30, cpu_usage, "CPU", Rgb888::new(0, 200, 100));

    // Memory gauge
    draw_gauge(fb, px + 20, py + 100, 360, 30, mem_usage, "MEM", Rgb888::new(0, 150, 255));

    // Network gauge
    draw_gauge(fb, px + 20, py + 150, 360, 30, net_usage, "NET", Rgb888::new(200, 100, 255));

    // Status text
    let status_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let _ = Text::new("Press Super to close", Point::new(px + 110, py + 280), status_style).draw(fb);
}

/// Draw a progress gauge.
fn draw_gauge(fb: &mut FramebufferState, x: i32, y: i32, w: i32, h: i32, value: f32, label: &str, color: Rgb888) {
    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 35, 55)).build();
    let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Fill
    let fill_w = ((w as f32) * (value / 100.0).min(1.0)) as u32;
    if fill_w > 0 {
        let fill_style = PrimitiveStyleBuilder::new().fill_color(color).build();
        let _ = Rectangle::new(Point::new(x, y), Size::new(fill_w, h as u32))
            .into_styled(fill_style)
            .draw(fb);
    }

    // Label
    let label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new(label, Point::new(x + 4, y + h - 8), label_style).draw(fb);
}

/// Draw the system central panel (services monitor).
fn draw_system_central(fb: &mut FramebufferState, services: &[ServiceInfo], service_count: usize) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let panel_w = 500;
    let panel_h = 350;
    let px = (fb_w - panel_w) / 2;
    let py = (fb_h - panel_h) / 2;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(12, 15, 28)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("SYSTEM CENTRAL", Point::new(px + 170, py + 24), title_style).draw(fb);

    // Column headers
    let header_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 130, 180));
    let _ = Text::new("PID", Point::new(px + 10, py + 50), header_style).draw(fb);
    let _ = Text::new("NAME", Point::new(px + 60, py + 50), header_style).draw(fb);
    let _ = Text::new("STATE", Point::new(px + 260, py + 50), header_style).draw(fb);
    let _ = Text::new("RESTARTS", Point::new(px + 380, py + 50), header_style).draw(fb);

    // Service rows
    let row_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    for i in 0..service_count.min(16) {
        let row_y = py + 70 + (i as i32) * 16;
        let svc = &services[i];

        let mut pid_buf = [0u8; 8];
        let pid_str = format_u32(&mut pid_buf, svc.pid);
        let _ = Text::new(pid_str, Point::new(px + 10, row_y), row_style).draw(fb);

        let name_len = svc.name.iter().position(|&b| b == 0).unwrap_or(16);
        let name = core::str::from_utf8(&svc.name[..name_len]).unwrap_or("?");
        let _ = Text::new(name, Point::new(px + 60, row_y), row_style).draw(fb);

        let state_str = match svc.state {
            1 => "ACTIVE",
            2 => "FAILED",
            _ => "IDLE",
        };
        let state_color = match svc.state {
            1 => Rgb888::new(0, 200, 100),
            2 => Rgb888::new(220, 50, 50),
            _ => Rgb888::new(100, 100, 100),
        };
        let state_style = MonoTextStyle::new(&FONT_6X12, state_color);
        let _ = Text::new(state_str, Point::new(px + 260, row_y), state_style).draw(fb);

        let mut rc_buf = [0u8; 8];
        let rc_str = format_u32(&mut rc_buf, svc.restart_count);
        let _ = Text::new(rc_str, Point::new(px + 380, row_y), row_style).draw(fb);
    }
}

/// Draw the lock screen overlay.
fn draw_lock_screen(fb: &mut FramebufferState) {
    let fb_w = fb.info.width;
    let fb_h = fb.info.height;

    // Full-screen dark overlay
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 8, 18)).build();
    let _ = Rectangle::new(Point::zero(), Size::new(fb_w, fb_h))
        .into_styled(bg_style)
        .draw(fb);

    // Lock icon area
    let cx = (fb_w as i32) / 2;
    let cy = (fb_h as i32) / 2 - 40;
    let lock_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 100, 200)).build();
    let _ = Rectangle::new(Point::new(cx - 30, cy), Size::new(60, 50))
        .into_styled(lock_style)
        .draw(fb);

    let text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new("SISTEMA BLOQUEADO", Point::new(cx - 54, cy + 80), text_style).draw(fb);

    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let _ = Text::new("Press L to unlock", Point::new(cx - 48, cy + 100), hint_style).draw(fb);
}

/// Draw the app launcher panel.
fn draw_launcher(fb: &mut FramebufferState, _desktop: &DesktopShell) {
    let _fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let panel_w = 300;
    let panel_h = 400;
    let px = 10;
    let py = fb_h - 44 - panel_h - 10;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(18, 22, 40)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("APPLICATIONS", Point::new(px + 90, py + 24), title_style).draw(fb);

    // Application entries
    let app_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 230));
    let apps = ["Terminal", "File Manager", "Text Editor", "Calculator", "Settings", "System Monitor", "Browser", "Network"];
    for (i, app_name) in apps.iter().enumerate() {
        let app_y = py + 50 + (i as i32) * 28;

        // App icon placeholder
        let icon_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 100 + (i as u8 * 15), 200)).build();
        let _ = Rectangle::new(Point::new(px + 12, app_y - 8), Size::new(20, 20))
            .into_styled(icon_style)
            .draw(fb);

        let _ = Text::new(app_name, Point::new(px + 40, app_y + 4), app_style).draw(fb);
    }
}

/// Draw the search bar overlay.
fn draw_search_bar(fb: &mut FramebufferState, query: &str) {
    let fb_w = fb.info.width as i32;
    let bar_w = 400;
    let bar_h = 40;
    let px = (fb_w - bar_w) / 2;
    let py = 60;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(25, 30, 50)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(bar_w as u32, bar_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 128, 255))
        .stroke_width(2)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(bar_w as u32, bar_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    if query.is_empty() {
        let placeholder_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 110, 130));
        let _ = Text::new("Search...", Point::new(px + 12, py + 26), placeholder_style).draw(fb);
    } else {
        let _ = Text::new(query, Point::new(px + 12, py + 26), text_style).draw(fb);
    }
}

/// Draw the notifications panel.
fn draw_notifications(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let fb_w = fb.info.width as i32;
    let panel_w = 300;
    let panel_h = 250;
    let px = fb_w - panel_w - 10;
    let py = 10;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(18, 22, 40)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("NOTIFICATIONS", Point::new(px + 90, py + 24), title_style).draw(fb);

    // Display notifications
    let notif_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    for i in 0..desktop.notification_count.min(8) {
        let notif = &desktop.notifications[i];
        let notif_y = py + 50 + (i as i32) * 24;
        let msg_len = notif.message.iter().position(|&b| b == 0).unwrap_or(64);
        let msg = core::str::from_utf8(&notif.message[..msg_len]).unwrap_or("");
        let _ = Text::new(msg, Point::new(px + 10, notif_y), notif_style).draw(fb);
    }

    if desktop.notification_count == 0 {
        let empty_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 110));
        let _ = Text::new("No notifications", Point::new(px + 80, py + 130), empty_style).draw(fb);
    }
}

/// Draw the mouse cursor.
fn draw_cursor(fb: &mut FramebufferState, cx: i32, cy: i32) {
    let cursor_color = Rgb888::WHITE;
    let cursor_style = PrimitiveStyleBuilder::new().fill_color(cursor_color).build();

    // Simple arrow cursor (8x12)
    for dy in 0..12 {
        let width = (dy + 1).min(8);
        let _ = Rectangle::new(Point::new(cx, cy + dy), Size::new(width as u32, 1))
            .into_styled(cursor_style)
            .draw(fb);
    }
}

/// Draw the HUD overlay with kernel log messages.
fn draw_hud_overlay(fb: &mut FramebufferState, log_buf: &[u8], log_len: usize) {
    let hud_w = 350;
    let hud_h = 100;
    let hud_x = fb.info.width as i32 - hud_w - 10;
    let hud_y = fb.info.height as i32 - 44 - hud_h - 10;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(10, 12, 25)).build();
    let _ = Rectangle::new(Point::new(hud_x, hud_y), Size::new(hud_w as u32, hud_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 60, 140))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(hud_x, hud_y), Size::new(hud_w as u32, hud_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let status_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 200, 100));
    let _ = Text::new("SISTEMA ONLINE", Point::new(hud_x + 8, hud_y + 16), status_style).draw(fb);

    // Log text
    let log_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(120, 140, 180));
    let safe_len = log_len.min(log_buf.len());
    if let Ok(text) = core::str::from_utf8(&log_buf[..safe_len]) {
        let mut line_y = hud_y + 32;
        for line in text.split('\n').take(5) {
            let truncated = if line.len() > 50 { &line[..50] } else { line };
            let _ = Text::new(truncated, Point::new(hud_x + 8, line_y), log_style).draw(fb);
            line_y += 14;
        }
    }
}

// ── Helper functions ──

fn format_metric<'a>(buf: &'a mut [u8; 16], label: &str, value: f32) -> &'a str {
    let label_bytes = label.as_bytes();
    let label_len = label_bytes.len().min(6);
    buf[..label_len].copy_from_slice(&label_bytes[..label_len]);

    let int_part = (value as u32).min(999);
    let mut pos = label_len;
    if int_part >= 100 {
        buf[pos] = (int_part / 100 % 10) as u8 + b'0';
        pos += 1;
    }
    buf[pos] = (int_part / 10 % 10) as u8 + b'0';
    pos += 1;
    buf[pos] = (int_part % 10) as u8 + b'0';
    pos += 1;
    buf[pos] = b'%';
    pos += 1;

    core::str::from_utf8(&buf[..pos]).unwrap_or("??")
}

fn format_u32<'a>(buf: &'a mut [u8; 8], val: u32) -> &'a str {
    if val == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut n = val;
    let mut pos = 7;
    while n > 0 && pos > 0 {
        buf[pos] = (n % 10) as u8 + b'0';
        n /= 10;
        pos -= 1;
    }
    pos += 1;
    core::str::from_utf8(&buf[pos..8]).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_framebuffer_state_mock() {
        let fb = FramebufferState::mock();
        assert_eq!(fb.info.width, DEFAULT_WIDTH);
        assert_eq!(fb.info.height, DEFAULT_HEIGHT);
    }

    #[test]
    fn test_format_metric() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "CPU:", 45.0);
        assert!(s.contains("CPU:"));
        assert!(s.contains("45%"));
    }

    #[test]
    fn test_format_metric_100() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "CPU:", 100.0);
        assert!(s.contains("100%"), "expected '100%' in '{}'", s);
    }

    #[test]
    fn test_format_metric_zero() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "MEM:", 0.0);
        assert!(s.contains("00%"), "expected '00%' in '{}'", s);
    }

    #[test]
    fn test_format_metric_single_digit() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "NET:", 5.0);
        assert!(s.contains("05%"), "expected '05%' in '{}'", s);
    }

    #[test]
    fn test_format_u32() {
        let mut buf = [0u8; 8];
        let s = format_u32(&mut buf, 123);
        assert_eq!(s, "123");
    }

    #[test]
    fn test_format_u32_zero() {
        let mut buf = [0u8; 8];
        let s = format_u32(&mut buf, 0);
        assert_eq!(s, "0");
    }
}
