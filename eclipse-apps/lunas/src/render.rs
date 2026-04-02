//! Rendering pipeline for Lunas desktop.
//! Handles framebuffer management, background rendering, window compositing,
//! desktop shell drawing, and overlay rendering.

use std::prelude::v1::*;
use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::primitives::{Rectangle, Circle, PrimitiveStyleBuilder};
use embedded_graphics::mono_font::{ascii::FONT_6X12, MonoTextStyle};
use embedded_graphics::text::Text;
use embedded_graphics::geometry::{Point, Size};
use crate::compositor::{ShellWindow, WindowContent, ExternalSurface};
use crate::input::InputState;
use crate::style_engine::StyleEngine;
use crate::assets;
use crate::desktop::{DesktopShell, WallpaperMode};
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
            let dev = match DisplayDevice::open() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[LUNAS] Failed to open display device: {:?}", e);
                    return None;
                }
            };
            eprintln!("[LUNAS] Display device opened: {:?}x{:?}", dev.caps.width, dev.caps.height);

            let fb_front = match dev.create_framebuffer() {
                Ok(fb) => fb,
                Err(e) => {
                    eprintln!("[LUNAS] Failed to create front framebuffer: {:?}", e);
                    return None;
                }
            };
            let fb_back = match dev.create_framebuffer() {
                Ok(fb) => fb,
                Err(e) => {
                    eprintln!("[LUNAS] Failed to create back framebuffer: {:?}", e);
                    return None;
                }
            };

            let background_addr = if let Ok(db) = dev.create_dumb_buffer(dev.caps.width, dev.caps.height, 32) {
                match dev.map_buffer(db.handle, db.size) {
                    Ok(ptr) => ptr as usize,
                    Err(e) => {
                        eprintln!("[LUNAS] Failed to map background buffer: {:?}", e);
                        0
                    }
                }
            } else {
                eprintln!("[LUNAS] Failed to create background dumb buffer");
                0
            };
            
            if background_addr == 0 {
                return None;
            }
            eprintln!("[LUNAS] Framebuffers and background buffer initialized.");

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
    /// Pre-render the desktop background into the background buffer.
    ///
    /// - `WallpaperMode::SolidColor`: fills the buffer with a single colour.
    /// - `WallpaperMode::Gradient`:   top-to-bottom gradient from the base colour
    ///   to a darker complementary tone.
    /// - `WallpaperMode::CosmicTheme`: the classic dark-blue/purple cosmic
    ///   gradient with a 200-star starfield (previous default behaviour).
    pub fn pre_render_background(&mut self, mode: WallpaperMode, color: (u8, u8, u8)) {
        #[cfg(not(test))]
        {
            let pitch_px = self.info.pitch as usize / 4;
            let ptr = self.background_addr as *mut u32;
            let h = self.info.height as usize;
            let w = self.info.width as usize;

            match mode {
                WallpaperMode::SolidColor => {
                    let pixel = 0xFF00_0000u32
                        | ((color.0 as u32) << 16)
                        | ((color.1 as u32) << 8)
                        | (color.2 as u32);
                    for y in 0..h {
                        for x in 0..w {
                            unsafe { core::ptr::write_volatile(ptr.add(y * pitch_px + x), pixel); }
                        }
                    }
                }
                WallpaperMode::Gradient => {
                    // Top: base colour; bottom: darkened by ~50 %.
                    let (r0, g0, b0) = color;
                    let r1 = (r0 as u32).saturating_sub(r0 as u32 / 2) as u8;
                    let g1 = (g0 as u32).saturating_sub(g0 as u32 / 2) as u8;
                    let b1 = (b0 as u32).saturating_sub(b0 as u32 / 2) as u8;
                    let h_norm = h.max(1) as u32; // denominator, pre-computed once
                    let lerp = |a: u8, b: u8, t: u32| -> u8 {
                        (a as u32 + (b as u32).wrapping_sub(a as u32).wrapping_mul(t) / 255) as u8
                    };
                    for y in 0..h {
                        let t = (y as u32 * 255) / h_norm; // 0 (top) → 255 (bottom)
                        let r = lerp(r0, r1, t);
                        let g = lerp(g0, g1, t);
                        let b = lerp(b0, b1, t);
                        let pixel = 0xFF00_0000u32 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                        for x in 0..w {
                            unsafe { core::ptr::write_volatile(ptr.add(y * pitch_px + x), pixel); }
                        }
                    }
                }
                WallpaperMode::CosmicTheme => {
                    // Cosmic gradient: dark blue to deep purple.
                    for y in 0..h {
                        for x in 0..w {
                            let r = (10 + (y * 15 / h)) as u8;
                            let g = (12 + (x * 8  / w)) as u8;
                            let b = (30 + (y * 25 / h)) as u8;
                            let pixel = 0xFF00_0000u32
                                | ((r as u32) << 16)
                                | ((g as u32) << 8)
                                | (b as u32);
                            unsafe { core::ptr::write_volatile(ptr.add(y * pitch_px + x), pixel); }
                        }
                    }
                    // Deterministic starfield.
                    let mut seed = 42u64;
                    for _ in 0..200 {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let sx = ((seed >> 16) as usize) % w;
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let sy = ((seed >> 16) as usize) % h;
                        let brightness = 150 + ((seed >> 32) as u8 % 105);
                        let star_pixel = 0xFF00_0000u32
                            | ((brightness as u32) << 16)
                            | ((brightness as u32) << 8)
                            | (brightness as u32);
                        unsafe { core::ptr::write_volatile(ptr.add(sy * pitch_px + sx), star_pixel); }
                    }
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

    /// Apply a warm tint (night light / blue-light filter) over the entire back buffer.
    /// `strength` is 0-100, where 100 is the maximum blue reduction.
    pub fn apply_night_light(&mut self, strength: u8) {
        #[cfg(not(test))]
        {
            let pitch_px = self.info.pitch as usize / 4;
            let ptr = self.back_addr as *mut u32;
            let reduction = (strength as u32 * NIGHT_LIGHT_MAX_BLUE_REDUCTION) / 100;
            let red_boost = (strength as u32 * NIGHT_LIGHT_RED_WARMTH) / 100;
            for y in 0..self.info.height as usize {
                for x in 0..self.info.width as usize {
                    unsafe {
                        let pixel = core::ptr::read_volatile(ptr.add(y * pitch_px + x));
                        let r = (pixel >> 16) & 0xFF;
                        let g = (pixel >> 8) & 0xFF;
                        let b = pixel & 0xFF;
                        let new_r = (r + red_boost).min(255);
                        let new_b = b.saturating_sub(reduction);
                        let new_pixel = (pixel & 0xFF000000) | (new_r << 16) | (g << 8) | new_b;
                        core::ptr::write_volatile(ptr.add(y * pitch_px + x), new_pixel);
                    }
                }
            }
        }
        #[cfg(test)]
        { let _ = strength; }
    }

    /// Save the current back buffer to /tmp/screenshot.raw on Eclipse targets.
    #[cfg(target_vendor = "eclipse")]
    pub fn save_screenshot(&self) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::File::create("/tmp/screenshot.raw") {
            let pitch = self.info.pitch as usize;
            let h = self.info.height as usize;
            let ptr = self.back_addr as *const u8;
            for row in 0..h {
                let row_slice = unsafe {
                    core::slice::from_raw_parts(ptr.add(row * pitch), pitch)
                };
                let _ = f.write_all(row_slice);
            }
        }
    }

    /// Blit an external surface buffer onto the back buffer.
    /// Blit a pixel buffer onto the back framebuffer.
    ///
    /// - `vaddr`      — virtual address of the source pixel buffer (ARGB8888).
    /// - `src_w`      — number of pixels to copy per row (blit width).
    /// - `src_h`      — number of rows to copy (blit height).
    /// - `src_stride` — actual row width of the source buffer in pixels (≥ src_w).
    ///                  Must equal the stride the client used when writing the buffer so
    ///                  that `row * src_stride + col` gives the correct pixel offset.
    ///                  Pass the same value as `src_w` when the buffer is tightly packed.
    /// - `dst_x/y`    — destination top-left corner in the framebuffer.
    pub fn blit_buffer(&mut self, vaddr: usize, src_w: u32, src_h: u32, src_stride: u32, dst_x: i32, dst_y: i32) {
        // src_stride must be at least as wide as the copy region; a smaller stride would mean
        // reading wrong pixels from adjacent rows.
        debug_assert!(src_stride >= src_w, "blit_buffer: src_stride ({}) must be >= src_w ({})", src_stride, src_w);
        #[cfg(not(test))]
        {
            let fb_w = self.info.width as i32;
            let fb_h = self.info.height as i32;
            let pitch_px = self.info.pitch as usize / 4;
            // Guard against a mis-specified stride at runtime by clamping, so we at worst
            // read fewer pixels than expected rather than overrunning into the wrong row.
            let stride = src_stride.max(src_w) as usize;

            for row in 0..src_h as i32 {
                let dy = dst_y + row;
                if dy < 0 || dy >= fb_h { continue; }
                for col in 0..src_w as i32 {
                    let dx = dst_x + col;
                    if dx < 0 || dx >= fb_w { continue; }
                    let src_offset = (row as usize) * stride + (col as usize);
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

/// Helper to draw a raw RGB888 icon with transparency key (black).
fn draw_raw_icon(fb: &mut FramebufferState, data: &[u8], x: i32, y: i32, w: u32, h: u32) {
    for row in 0..h as i32 {
        for col in 0..w as i32 {
            let offset = (row * w as i32 + col) as usize * 3;
            if offset + 2 >= data.len() { break; }
            let r = data[offset];
            let g = data[offset + 1];
            let b = data[offset + 2];
            
            // Adaptive transparency: Use brightness as alpha.
            let alpha = r.max(g).max(b) as u16;
            if alpha < 10 { continue; }
            
            if alpha < 255 {
                // Blend with taskbar background Rgb(15, 18, 35) to smooth edges
                let br = 15u16; let bg = 18u16; let bb = 35u16;
                let r = ((r as u16 * alpha + br * (255 - alpha)) / 255) as u8;
                let g = ((g as u16 * alpha + bg * (255 - alpha)) / 255) as u8;
                let b = ((b as u16 * alpha + bb * (255 - alpha)) / 255) as u8;
                let color = Rgb888::new(r, g, b);
                let _ = fb.draw_iter(core::iter::once(embedded_graphics::Pixel(Point::new(x + col, y + row), color)));
            } else {
                let color = Rgb888::new(r, g, b);
                let _ = fb.draw_iter(core::iter::once(embedded_graphics::Pixel(Point::new(x + col, y + row), color)));
            }
        }
    }
}

// ── Desktop Shell Rendering ──

/// Pixels from screen edge at which a window drag activates a snap zone guide.
const SNAP_EDGE_THRESHOLD: i32 = 20;

/// Default night light filter strength (0-100). Applied when night light mode is active.
const DEFAULT_NIGHT_LIGHT_STRENGTH: u8 = 60;

/// Maximum reduction of the blue channel in the night light filter (0-255).
const NIGHT_LIGHT_MAX_BLUE_REDUCTION: u32 = 160;

/// Red warmth boost in the night light filter (0-255).
const NIGHT_LIGHT_RED_WARMTH: u32 = 30;

/// Draw semi-transparent snap zone guide rectangles when a window is being dragged.
/// Shows visual guides for half-screen (left/right) and quarter-screen (corners) zones.
fn draw_snap_guides(fb: &mut FramebufferState, input: &InputState) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let cx = input.cursor_x;
    let cy = input.cursor_y;
    let usable_h = fb_h - TASKBAR_HEIGHT;
    let usable_half_h = usable_h / 2;

    // Define snap zones with their trigger region and highlight rect.
    // Each entry: (trigger condition, highlight x, y, w, h)
    struct SnapZone { x: i32, y: i32, w: i32, h: i32 }

    let edge_threshold = SNAP_EDGE_THRESHOLD;
    let center_y = crate::compositor::ShellWindow::TITLE_H;

    let zone: Option<SnapZone> = if cx < edge_threshold && cy < usable_half_h {
        Some(SnapZone { x: 0, y: center_y, w: fb_w / 2, h: usable_half_h - center_y })
    } else if cx >= fb_w - edge_threshold && cy < usable_half_h {
        Some(SnapZone { x: fb_w / 2, y: center_y, w: fb_w / 2, h: usable_half_h - center_y })
    } else if cx < edge_threshold && cy >= usable_half_h {
        Some(SnapZone { x: 0, y: usable_half_h, w: fb_w / 2, h: usable_h - usable_half_h })
    } else if cx >= fb_w - edge_threshold && cy >= usable_half_h {
        Some(SnapZone { x: fb_w / 2, y: usable_half_h, w: fb_w / 2, h: usable_h - usable_half_h })
    } else if cx < edge_threshold {
        Some(SnapZone { x: 0, y: center_y, w: fb_w / 2, h: usable_h - center_y })
    } else if cx >= fb_w - edge_threshold {
        Some(SnapZone { x: fb_w / 2, y: center_y, w: fb_w / 2, h: usable_h - center_y })
    } else if cy < edge_threshold {
        Some(SnapZone { x: 0, y: center_y, w: fb_w, h: usable_h - center_y })
    } else {
        None
    };

    if let Some(z) = zone {
        // Semi-transparent highlight (simulated with a dim filled rect + border)
        let fill_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 60, 140))
            .build();
        let _ = Rectangle::new(Point::new(z.x, z.y), Size::new(z.w as u32, z.h as u32))
            .into_styled(fill_style)
            .draw(fb);
        let border_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(0, 160, 255))
            .stroke_width(2)
            .build();
        let _ = Rectangle::new(Point::new(z.x, z.y), Size::new(z.w as u32, z.h as u32))
            .into_styled(border_style)
            .draw(fb);
    }
}

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
    net_extended_stats: Option<&eclipse_ipc::types::NetExtendedStats>,
    cpu_history: &[f32; 60],
    mem_history: &[f32; 60],
    net_history: &[f32; 60],
    history_pos: usize,
    cpu_temp: u32,
    snp_surfaces: &std::collections::BTreeMap<(u32, u32), ExternalSurface>,
) {
    // 1. Blit background
    fb.blit_background();

    // 1.5. Draw HUD overlay (kernel log) — placed in background so it doesn't block windows
    if log_len > 0 {
        draw_hud_overlay(fb, log_buf, log_len);
    }

    // 2. Draw taskbar
    draw_taskbar(fb, input, desktop, windows, window_count, net_usage);

    // 3. Draw windows (painter's algorithm: back to front)
    for i in 0..window_count {
        let w = &windows[i];
        if w.content == WindowContent::None || w.minimized || w.closing { continue; }
        if w.workspace != input.current_workspace { continue; }
        draw_window(fb, w, surfaces, snp_surfaces, input.focused_window == Some(i), input.window_decoration_style);
    }

    // 3.5. Draw snap zone guides when dragging a window near screen edges/corners
    if input.dragging_window.is_some() {
        draw_snap_guides(fb, input);
    }

    // 4. Draw overlays
    if input.dashboard_active {
        draw_dashboard(fb, cpu_usage, mem_usage, net_usage,
            cpu_history, mem_history, net_history, history_pos, cpu_temp);
    }

    if input.system_central_active {
        draw_system_central(fb, services, service_count);
    }

    if input.network_details_active {
        draw_network_panel(fb, net_usage, net_extended_stats);
    }

    if input.lock_screen_active {
        draw_lock_screen(fb, input);
    }

    if input.launcher_active {
        draw_launcher(fb, desktop, input);
    }

    if input.search_active {
        draw_search_bar(fb, &input.search_query);
    }

    if input.notifications_visible {
        draw_notifications(fb, desktop);
    }

    if input.volume_panel_active {
        draw_volume_popup(fb, desktop);
    }

    if input.clock_panel_active {
        draw_clock_panel(fb, desktop, input.calendar_month_offset);
    }

    if input.context_menu.visible {
        draw_context_menu(fb, &input.context_menu);
    }

    if input.quick_settings_active {
        draw_quick_settings_panel(fb, desktop);
    }

    if input.battery_panel_active {
        draw_battery_panel(fb, desktop);
    }

    if input.net_config_active {
        draw_network_config_panel(fb, input);
    }

    // 5. Draw cursor
    draw_cursor(fb, input.cursor_x, input.cursor_y);

    // 6. Apply night light filter last so it tints everything on screen.
    if desktop.night_light_active {
        fb.apply_night_light(DEFAULT_NIGHT_LIGHT_STRENGTH);
    }
}

/// Taskbar height in pixels.
pub const TASKBAR_HEIGHT: i32 = 44;

/// Pinned app icon size in the taskbar.
pub const TASKBAR_ICON_SIZE: i32 = 32;

/// Spacing between taskbar pinned app icons.
pub const TASKBAR_ICON_SPACING: i32 = 6;

/// Left margin where pinned apps start (after launcher + workspace indicators).
pub const TASKBAR_APPS_START_X: i32 = 160;

/// Width of the system tray area on the right side.
pub const TASKBAR_TRAY_WIDTH: i32 = 220;

/// Maximum characters for a window task title before truncation.
const TASK_TITLE_MAX_CHARS: usize = 16;
/// Characters shown before ellipsis in truncated window titles.
const TASK_TITLE_TRUNCATED_CHARS: usize = 14;
/// Character width for the FONT_6X12 monospaced font.
const FONT_CHAR_WIDTH: i32 = 6;

// ── Running-dot indicator constants ──
/// Width (px) of each running-instance dot.
const RUN_DOT_W: i32 = 4;
/// Gap (px) between adjacent running-instance dots.
const RUN_DOT_GAP: i32 = 2;
/// Stride between dot origins (dot width + gap).
const RUN_DOT_STRIDE: i32 = RUN_DOT_W + RUN_DOT_GAP;

// ── Mini-icon colour derivation constants ──
/// Fallback seed byte when the window title is empty.
const MINI_ICON_SEED_DEFAULT: u8 = 80;
/// Multiplier for the red channel of the mini-icon colour.
const MINI_ICON_R_MUL: u8 = 97;
/// Multiplier for the green channel of the mini-icon colour.
const MINI_ICON_G_MUL: u8 = 71;
/// Multiplier for the blue channel of the mini-icon colour.
const MINI_ICON_B_MUL: u8 = 53;
/// Minimum additive offset for the red channel (keeps colours visible).
const MINI_ICON_R_ADD: u8 = 20;
/// Minimum additive offset for the green channel.
const MINI_ICON_G_ADD: u8 = 60;
/// Minimum additive offset for the blue channel.
const MINI_ICON_B_ADD: u8 = 100;
/// Cap for the red channel (prevents overly bright red-dominant icons).
const MINI_ICON_R_MAX: u8 = 140;
/// Cap for the green channel.
const MINI_ICON_G_MAX: u8 = 160;
/// Cap for the blue channel.
const MINI_ICON_B_MAX: u8 = 210;

// ── Window decoration style title-bar colours ──
/// Default style: focused title bar colour.
const TITLE_DEFAULT_FOCUSED: Rgb888 = Rgb888::new(25, 40, 80);
/// Default style: unfocused title bar colour.
const TITLE_DEFAULT_UNFOCUSED: Rgb888 = Rgb888::new(20, 25, 45);
/// Minimal style: focused title bar colour.
const TITLE_MINIMAL_FOCUSED: Rgb888 = Rgb888::new(40, 42, 54);
/// Minimal style: unfocused title bar colour.
const TITLE_MINIMAL_UNFOCUSED: Rgb888 = Rgb888::new(30, 32, 42);
/// Neon style: focused title bar colour.
const TITLE_NEON_FOCUSED: Rgb888 = Rgb888::new(0, 40, 60);
/// Neon style: unfocused title bar colour.
const TITLE_NEON_UNFOCUSED: Rgb888 = Rgb888::new(0, 25, 40);

/// Draw the bottom taskbar.
fn draw_taskbar(
    fb: &mut FramebufferState,
    input: &InputState,
    desktop: &DesktopShell,
    windows: &[ShellWindow],
    window_count: usize,
    net_usage: f32,
) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let bar_h = TASKBAR_HEIGHT;
    let bar_y = fb_h - bar_h;

    // Taskbar background
    let taskbar_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(15, 18, 35))
        .build();
    let _ = Rectangle::new(Point::new(0, bar_y), Size::new(fb_w as u32, bar_h as u32))
        .into_styled(taskbar_style)
        .draw(fb);

    // Top border accent line
    let border_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(0, 100, 200))
        .build();
    let _ = Rectangle::new(Point::new(0, bar_y), Size::new(fb_w as u32, 1))
        .into_styled(border_style)
        .draw(fb);

    // ── Launcher button ──
    let launcher_is_hovered = input.hovered_taskbar_element == crate::input::TaskbarHit::Launcher;
    let launcher_bg = if input.launcher_active {
        Rgb888::new(0, 128, 255)
    } else {
        Rgb888::new(30, 40, 60)
    };
    let launcher_style = PrimitiveStyleBuilder::new().fill_color(launcher_bg).build();
    let _ = Rectangle::new(Point::new(4, bar_y + 6), Size::new(36, 32))
        .into_styled(launcher_style)
        .draw(fb);
    // Hover border when not active
    if launcher_is_hovered && !input.launcher_active {
        let launcher_hover_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(0, 140, 255))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(4, bar_y + 6), Size::new(36, 32))
            .into_styled(launcher_hover_style)
            .draw(fb);
    }
    // Active indicator dot at bottom of launcher button
    if input.launcher_active {
        let active_dot_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::WHITE)
            .build();
        let _ = Rectangle::new(Point::new(20, bar_y + 36), Size::new(4, 2))
            .into_styled(active_dot_style)
            .draw(fb);
    }

    // Launcher icon (Lunas Logo)
    draw_raw_icon(fb, assets::LUNAS_LOGO, 6, bar_y + 6, 32, 32);

    // ── Workspace indicators ──
    for ws in 0..4u8 {
        let ws_x = 48 + (ws as i32) * 26;
        let active = ws == input.current_workspace;
        let ws_color = if active {
            Rgb888::new(0, 128, 255)
        } else {
            Rgb888::new(40, 50, 70)
        };
        let ws_style = PrimitiveStyleBuilder::new().fill_color(ws_color).build();
        let _ = Rectangle::new(Point::new(ws_x, bar_y + 12), Size::new(20, 20))
            .into_styled(ws_style)
            .draw(fb);

        // Workspace number label
        let ws_label_color = if active {
            Rgb888::WHITE
        } else {
            Rgb888::new(120, 130, 160)
        };
        let ws_label_style = MonoTextStyle::new(&FONT_6X12, ws_label_color);
        let ws_char_buf = [b'0' + ws + 1];
        let ws_label = core::str::from_utf8(&ws_char_buf[..1]).unwrap_or("?");
        let _ = Text::new(ws_label, Point::new(ws_x + 7, bar_y + 26), ws_label_style).draw(fb);

        // Presence dot(s) below the workspace indicator — shown when the workspace
        // has open (non-minimized) windows. Active workspace gets a bright dot;
        // inactive workspaces with windows get a dim dot.
        if !active {
            let ws_has_windows = (0..window_count).any(|wi| {
                let w = &windows[wi];
                w.content != WindowContent::None && !w.closing && w.workspace == ws
            });
            if ws_has_windows {
                let presence_style = PrimitiveStyleBuilder::new()
                    .fill_color(Rgb888::new(80, 100, 160))
                    .build();
                let _ = Rectangle::new(Point::new(ws_x + 8, bar_y + 38), Size::new(4, 2))
                    .into_styled(presence_style)
                    .draw(fb);
            }
        } else {
            // Active workspace: always show a bright dot at the bottom of the indicator
            let presence_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 200, 255))
                .build();
            let _ = Rectangle::new(Point::new(ws_x + 8, bar_y + 38), Size::new(4, 2))
                .into_styled(presence_style)
                .draw(fb);
        }
    }

    // ── Separator after workspaces ──
    let sep_x = TASKBAR_APPS_START_X - 6;
    let sep_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(50, 60, 90))
        .build();
    let _ = Rectangle::new(Point::new(sep_x, bar_y + 8), Size::new(1, 28))
        .into_styled(sep_style)
        .draw(fb);

    // ── Pinned apps ──
    let mut app_x = TASKBAR_APPS_START_X;
    for i in 0..desktop.pinned_count {
        let app = &desktop.pinned_apps[i];
        let (r, g, b) = app.icon_color;

        // Count running windows on this workspace whose title starts with the pinned app name
        let app_name = app.name_str();
        let run_count = (0..window_count).filter(|&w_idx| {
            let w = &windows[w_idx];
            if w.content == WindowContent::None || w.closing { return false; }
            if w.workspace != input.current_workspace { return false; }
            let w_title = w.title_str();
            w_title.len() >= app_name.len() && w_title[..app_name.len()].eq_ignore_ascii_case(app_name)
        }).count();
        let is_running = run_count > 0;

        // Hover highlight
        let is_hovered = input.hovered_taskbar_element == crate::input::TaskbarHit::PinnedApp(i);

        // App icon background
        let icon_bg = if is_hovered {
            Rgb888::new(r.saturating_add(40), g.saturating_add(40), b.saturating_add(40))
        } else if is_running {
            Rgb888::new(r.saturating_add(20), g.saturating_add(20), b.saturating_add(20))
        } else {
            Rgb888::new(r / 3, g / 3, b / 3)
        };
        let icon_style = PrimitiveStyleBuilder::new().fill_color(icon_bg).build();
        let _ = Rectangle::new(
            Point::new(app_x, bar_y + 6),
            Size::new(TASKBAR_ICON_SIZE as u32, TASKBAR_ICON_SIZE as u32),
        )
        .into_styled(icon_style)
        .draw(fb);

        // Hover outline border on the pinned app icon
        if is_hovered {
            let hover_border_style = PrimitiveStyleBuilder::new()
                .stroke_color(Rgb888::new(0, 180, 255))
                .stroke_width(1)
                .build();
            let _ = Rectangle::new(
                Point::new(app_x, bar_y + 6),
                Size::new(TASKBAR_ICON_SIZE as u32, TASKBAR_ICON_SIZE as u32),
            )
            .into_styled(hover_border_style)
            .draw(fb);
        }

        // App icon (minimalist)
        let icon_data = match app_name {
            n if n.eq_ignore_ascii_case("Terminal") => Some(assets::TERMINAL_ICON),
            n if n.eq_ignore_ascii_case("Files") => Some(assets::FILES_ICON),
            n if n.eq_ignore_ascii_case("Editor") => Some(assets::EDITOR_ICON),
            n if n.eq_ignore_ascii_case("Browser") => Some(assets::BROWSER_ICON),
            n if n.eq_ignore_ascii_case("Settings") => Some(assets::SETTINGS_ICON),
            _ => None,
        };

        if let Some(data) = icon_data {
            draw_raw_icon(fb, data, app_x, bar_y + 6, 32, 32);
        } else {
            // Fallback to first-letter icon if no specific icon asset exists
            let letter_color = if is_running {
                Rgb888::WHITE
            } else {
                Rgb888::new(180, 190, 210)
            };
            let letter_style = MonoTextStyle::new(&FONT_6X12, letter_color);
            let first_char = app_name.chars().next().unwrap_or('?');
            let mut char_buf = [0u8; 4];
            let char_str = first_char.encode_utf8(&mut char_buf);
            let _ = Text::new(char_str, Point::new(app_x + 12, bar_y + 26), letter_style).draw(fb);
        }

        // Running indicator: 1-3 dots below the icon depending on instance count
        if run_count > 0 {
            let dot_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 200, 255))
                .build();
            let (dot_start_x, n) = running_dot_layout(run_count, app_x, TASKBAR_ICON_SIZE);
            for d in 0..n {
                let _ = Rectangle::new(
                    Point::new(dot_start_x + d * RUN_DOT_STRIDE, bar_y + 40),
                    Size::new(RUN_DOT_W as u32, 2),
                )
                .into_styled(dot_style)
                .draw(fb);
            }
        }

        app_x += TASKBAR_ICON_SIZE + TASKBAR_ICON_SPACING;
    }

    // ── Separator after pinned apps ──
    let sep2_x = app_x + 2;
    let _ = Rectangle::new(Point::new(sep2_x, bar_y + 8), Size::new(1, 28))
        .into_styled(sep_style)
        .draw(fb);

    // ── Running windows (not matching pinned apps) ──
    let mut win_x = sep2_x + 8;
    let win_item_w = 120i32;
    let tray_start = fb_w - TASKBAR_TRAY_WIDTH;
    let scroll_btn_w: i32 = 16;

    // ── Scroll-left button (◀) — only visible when scrolled past the beginning ──
    let scroll_left_x = win_x;
    if input.task_scroll_offset > 0 {
        let sl_hovered = input.hovered_taskbar_element == crate::input::TaskbarHit::TaskScrollLeft;
        let sl_bg = if sl_hovered { Rgb888::new(60, 80, 130) } else { Rgb888::new(30, 40, 70) };
        let sl_style = PrimitiveStyleBuilder::new().fill_color(sl_bg).build();
        let _ = Rectangle::new(Point::new(scroll_left_x, bar_y + 8), Size::new(scroll_btn_w as u32, 28))
            .into_styled(sl_style)
            .draw(fb);
        let sl_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 200, 240));
        let _ = Text::new("<", Point::new(scroll_left_x + 5, bar_y + 26), sl_text).draw(fb);
        win_x += scroll_btn_w + 2;
    }

    // Reserve space for the scroll-right button area at the end
    let task_area_end = tray_start - 10 - scroll_btn_w - 4;
    let mut task_overflow = false;
    let mut skipped = 0usize;

    for w_idx in 0..window_count {
        let w = &windows[w_idx];
        if w.content == WindowContent::None || w.closing { continue; }
        if w.workspace != input.current_workspace { continue; }

        // Skip windows whose titles match a pinned app (already shown above)
        let w_title = w.title_str();
        let already_pinned = (0..desktop.pinned_count).any(|pi| {
            let pname = desktop.pinned_apps[pi].name_str();
            w_title.len() >= pname.len() && w_title[..pname.len()].eq_ignore_ascii_case(pname)
        });
        if already_pinned { continue; }

        // Apply scroll offset
        if skipped < input.task_scroll_offset {
            skipped += 1;
            continue;
        }

        if win_x + win_item_w > task_area_end {
            task_overflow = true;
            break;
        }

        // Window task button
        let focused = input.focused_window == Some(w_idx);
        let is_minimized = w.minimized;
        let is_hovered = input.hovered_taskbar_element == crate::input::TaskbarHit::WindowTask(w_idx);
        let task_bg = if is_hovered {
            Rgb888::new(45, 60, 95)
        } else if focused {
            Rgb888::new(35, 50, 80)
        } else if is_minimized {
            Rgb888::new(18, 22, 38)
        } else {
            Rgb888::new(25, 30, 50)
        };
        let task_style = PrimitiveStyleBuilder::new().fill_color(task_bg).build();
        let _ = Rectangle::new(Point::new(win_x, bar_y + 8), Size::new(win_item_w as u32, 28))
            .into_styled(task_style)
            .draw(fb);

        // Left accent bar on focused window task (2px vertical stripe)
        if focused {
            let left_accent_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 150, 255))
                .build();
            let _ = Rectangle::new(Point::new(win_x, bar_y + 8), Size::new(2, 28))
                .into_styled(left_accent_style)
                .draw(fb);
        }

        // Small coloured mini-icon (12×12) with the first letter of the window title.
        // Colour is derived from the first byte of the title for a stable per-app hue.
        let title_first_byte = w_title.as_bytes().first().copied().unwrap_or(MINI_ICON_SEED_DEFAULT);
        let icon_r = title_first_byte.wrapping_mul(MINI_ICON_R_MUL).saturating_add(MINI_ICON_R_ADD).min(MINI_ICON_R_MAX);
        let icon_g = title_first_byte.wrapping_mul(MINI_ICON_G_MUL).saturating_add(MINI_ICON_G_ADD).min(MINI_ICON_G_MAX);
        let icon_b = title_first_byte.wrapping_mul(MINI_ICON_B_MUL).saturating_add(MINI_ICON_B_ADD).min(MINI_ICON_B_MAX);
        let mini_icon_bg = if is_minimized {
            Rgb888::new(icon_r / 2, icon_g / 2, icon_b / 2)
        } else {
            Rgb888::new(icon_r, icon_g, icon_b)
        };
        let mini_icon_style = PrimitiveStyleBuilder::new().fill_color(mini_icon_bg).build();
        let _ = Rectangle::new(Point::new(win_x + 4, bar_y + 14), Size::new(12, 12))
            .into_styled(mini_icon_style)
            .draw(fb);
        let first_char = w_title.chars().next().unwrap_or('?');
        let mut icon_char_buf = [0u8; 4];
        let icon_char_str = first_char.encode_utf8(&mut icon_char_buf);
        let icon_letter_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
        let _ = Text::new(icon_char_str, Point::new(win_x + 5, bar_y + 24), icon_letter_style).draw(fb);

        // Window title (truncated with ellipsis), offset right to make room for the mini-icon.
        // On hover, reserve 14px on the right for the close button.
        let title_color = if is_minimized {
            Rgb888::new(100, 110, 130)
        } else {
            Rgb888::new(180, 190, 210)
        };
        let task_text_style = MonoTextStyle::new(&FONT_6X12, title_color);
        let title_x = win_x + 20;
        // When hovered, cap title 14px earlier to leave room for the "×" close button.
        let title_max_chars = if is_hovered { TASK_TITLE_MAX_CHARS - 2 } else { TASK_TITLE_MAX_CHARS };
        let title_truncated_chars = if is_hovered { TASK_TITLE_TRUNCATED_CHARS - 2 } else { TASK_TITLE_TRUNCATED_CHARS };
        if w_title.len() > title_max_chars {
            let truncated_title = &w_title[..title_truncated_chars];
            let _ = Text::new(truncated_title, Point::new(title_x, bar_y + 26), task_text_style).draw(fb);
            let _ = Text::new("..", Point::new(title_x + title_truncated_chars as i32 * FONT_CHAR_WIDTH, bar_y + 26), task_text_style).draw(fb);
        } else {
            let _ = Text::new(w_title, Point::new(title_x, bar_y + 26), task_text_style).draw(fb);
        }

        // Inline close "×" button on the right edge — only on hover.
        if is_hovered {
            let close_x = win_x + win_item_w - 14;
            let close_bg_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(180, 40, 40))
                .build();
            let _ = Rectangle::new(Point::new(close_x, bar_y + 12), Size::new(12, 12))
                .into_styled(close_bg_style)
                .draw(fb);
            let close_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
            let _ = Text::new("x", Point::new(close_x + 3, bar_y + 22), close_text_style).draw(fb);
        }

        // Bottom focus indicator (full-width blue bar under the button)
        if focused {
            let focus_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 128, 255))
                .build();
            let _ = Rectangle::new(Point::new(win_x, bar_y + 38), Size::new(win_item_w as u32, 2))
                .into_styled(focus_style)
                .draw(fb);
        }

        win_x += win_item_w + 4;
    }

    // ── Scroll-right button (▶) — shown when there are more tasks than fit ──
    if task_overflow {
        let sr_x = tray_start - scroll_btn_w - 6;
        let sr_hovered = input.hovered_taskbar_element == crate::input::TaskbarHit::TaskScrollRight;
        let sr_bg = if sr_hovered { Rgb888::new(60, 80, 130) } else { Rgb888::new(30, 40, 70) };
        let sr_style = PrimitiveStyleBuilder::new().fill_color(sr_bg).build();
        let _ = Rectangle::new(Point::new(sr_x, bar_y + 8), Size::new(scroll_btn_w as u32, 28))
            .into_styled(sr_style)
            .draw(fb);
        let sr_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 200, 240));
        let _ = Text::new(">", Point::new(sr_x + 5, bar_y + 26), sr_text).draw(fb);
    }

    // ── System tray area (right side) ──
    let tray_x = tray_start;

    // Subtle tray background tint — visually separates the tray from the window tasks area.
    let tray_bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(12, 15, 30))
        .build();
    let _ = Rectangle::new(Point::new(tray_x + 1, bar_y + 1), Size::new((TASKBAR_TRAY_WIDTH - 1) as u32, (bar_h - 1) as u32))
        .into_styled(tray_bg_style)
        .draw(fb);

    // Tray separator
    let _ = Rectangle::new(Point::new(tray_x, bar_y + 8), Size::new(1, 28))
        .into_styled(sep_style)
        .draw(fb);

    // ── Tiling mode indicator "T" badge ──
    if input.tiling_active {
        let tiling_bg_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 100, 50))
            .build();
        let _ = Rectangle::new(Point::new(tray_x + 8, bar_y + 11), Size::new(14, 14))
            .into_styled(tiling_bg_style)
            .draw(fb);
        let tiling_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 220, 120));
        let _ = Text::new("T", Point::new(tray_x + 10, bar_y + 22), tiling_text).draw(fb);
    }

    // ── Window decoration style badge "M"/"N" ──
    if input.window_decoration_style > 0 {
        let (style_char, style_color) = match input.window_decoration_style {
            1 => ("M", Rgb888::new(140, 150, 170)),
            _ => ("N", Rgb888::new(0, 240, 220)),
        };
        let deco_text = MonoTextStyle::new(&FONT_6X12, style_color);
        let _ = Text::new(style_char, Point::new(tray_x + 26, bar_y + 22), deco_text).draw(fb);
    }

    // ── Brightness mini-bar (24px wide) with sun glyph ──
    {
        let bri = desktop.brightness_level.min(100) as i32;
        let bri_bar_x = tray_x + 38;
        let bri_bar_w_full: i32 = 24;
        let bri_bar_w = (bri * bri_bar_w_full) / 100;
        // Sun glyph above the bar
        let sun_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(220, 200, 80));
        let _ = Text::new("*", Point::new(bri_bar_x, bar_y + 20), sun_style).draw(fb);
        // Background track
        let bri_track_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(30, 38, 60))
            .build();
        let _ = Rectangle::new(Point::new(bri_bar_x, bar_y + 33), Size::new(bri_bar_w_full as u32, 2))
            .into_styled(bri_track_style)
            .draw(fb);
        // Fill
        if bri_bar_w > 0 {
            let bri_fill_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(220, 200, 80))
                .build();
            let _ = Rectangle::new(Point::new(bri_bar_x, bar_y + 33), Size::new(bri_bar_w as u32, 2))
                .into_styled(bri_fill_style)
                .draw(fb);
        }
    }

    // ── Notification bell indicator ──
    let notif_count = desktop.unread_count();
    let notif_x = tray_x + 70;
    draw_raw_icon(fb, assets::NOTIFICATION_ICON, notif_x, bar_y + 8, 24, 24);
    if desktop.do_not_disturb {
        // DND active: red bars + "Z" label
        let dnd_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(220, 60, 60))
            .build();
        let _ = Rectangle::new(Point::new(notif_x + 2, bar_y + 10), Size::new(18, 2))
            .into_styled(dnd_style)
            .draw(fb);
        let _ = Rectangle::new(Point::new(notif_x + 2, bar_y + 15), Size::new(18, 2))
            .into_styled(dnd_style)
            .draw(fb);
        let dnd_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(220, 60, 60));
        let _ = Text::new("Z", Point::new(notif_x + 16, bar_y + 10), dnd_text).draw(fb);
    } else if notif_count > 0 {
        let badge_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(220, 50, 50))
            .build();
        let _ = Rectangle::new(Point::new(notif_x + 6, bar_y + 6), Size::new(12, 12))
            .into_styled(badge_style)
            .draw(fb);
        let badge_text = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
        let mut nbuf = [0u8; 8];
        let nstr = format_u32(&mut nbuf, notif_count as u32);
        let _ = Text::new(nstr, Point::new(notif_x + 8, bar_y + 16), badge_text).draw(fb);
    }

    // ── Volume indicator ──
    let vol_x = tray_x + 100;
    // Hover highlight
    if input.hovered_taskbar_element == crate::input::TaskbarHit::Volume {
        let vol_hover_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(35, 45, 75))
            .build();
        let _ = Rectangle::new(Point::new(vol_x - 6, bar_y + 2), Size::new(28, 40))
            .into_styled(vol_hover_style)
            .draw(fb);
    }
    // Muted: draw icon with red tint via overlay bar; else draw normal icon
    draw_raw_icon(fb, assets::VOLUME_ICON, vol_x - 4, bar_y + 8, 24, 24);
    if desktop.volume_muted {
        // Red X mark over the icon when muted
        let mute_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(220, 50, 50))
            .build();
        let _ = Rectangle::new(Point::new(vol_x + 10, bar_y + 8), Size::new(10, 2))
            .into_styled(mute_style)
            .draw(fb);
        let _ = Rectangle::new(Point::new(vol_x + 10, bar_y + 28), Size::new(10, 2))
            .into_styled(mute_style)
            .draw(fb);
    } else {
        // Volume % text below icon
        let vol_pct_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(120, 140, 200));
        let mut vpct_buf = [0u8; 8];
        let vpct = desktop.volume_level.min(100) as u32;
        let vpct_str = format_u32(&mut vpct_buf, vpct);
        let vol_text_x = vol_x - 4 + (24i32 - vpct_str.len() as i32 * 6) / 2;
        let _ = Text::new(vpct_str, Point::new(vol_text_x, bar_y + 36), vol_pct_style).draw(fb);
    }

    // ── Network icon ──
    let net_x = tray_x + 126;
    // Hover highlight
    if input.hovered_taskbar_element == crate::input::TaskbarHit::Network {
        let net_hover_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(35, 45, 75))
            .build();
        let _ = Rectangle::new(Point::new(net_x - 2, bar_y + 2), Size::new(28, 40))
            .into_styled(net_hover_style)
            .draw(fb);
    }
    draw_raw_icon(fb, assets::NETWORK_ICON, net_x, bar_y + 8, 24, 24);
    // Connectivity status dot: green when actively transferring data, lighter green when idle/connected
    {
        let dot_color = if net_usage > 0.05 { // >5% usage = active transfer
            Rgb888::new(0, 220, 80)   // active data flow
        } else {
            Rgb888::new(60, 180, 60)  // connected but idle
        };
        let dot_style = PrimitiveStyleBuilder::new().fill_color(dot_color).build();
        let _ = Circle::new(Point::new(net_x + 16, bar_y + 6), 6)
            .into_styled(dot_style)
            .draw(fb);
    }

    // ── Night Light indicator: warm amber dot when active ──
    if desktop.night_light_active {
        let nl_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(255, 160, 40))
            .build();
        let _ = Circle::new(Point::new(tray_x + 150, bar_y + 14), 8)
            .into_styled(nl_style)
            .draw(fb);
        let nl_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(15, 10, 0));
        let _ = Text::new("N", Point::new(tray_x + 152, bar_y + 22), nl_text).draw(fb);
    }

    // Clock display (far right) — shows HH:MM and DD/MM when clock is enabled, else branding
    let clock_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 220));
    if desktop.show_clock {
        let mut time_buf = [0u8; 8];
        let h = desktop.clock_hours;
        let m = desktop.clock_minutes;
        time_buf[0] = b'0' + h / 10;
        time_buf[1] = b'0' + h % 10;
        time_buf[2] = b':';
        time_buf[3] = b'0' + m / 10;
        time_buf[4] = b'0' + m % 10;
        let time_str = core::str::from_utf8(&time_buf[..5]).unwrap_or("00:00");
        let _ = Text::new(time_str, Point::new(fb_w - 56, bar_y + 14), clock_style).draw(fb);

        // Date below time (DD/MM with day-of-week prefix)
        let date_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 110, 140));
        let d = desktop.clock_day;
        let mo = desktop.clock_month;
        let year = desktop.clock_year as u32;
        // Three-letter day-of-week abbreviation (Sakamoto: 0=Sun, 1=Mon…5=Fri, 6=Sat)
        const DOW_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let dow_idx = (day_of_week(year, mo as u32, d as u32) % 7) as usize;
        let dow_str = DOW_ABBR.get(dow_idx).copied().unwrap_or("???");
        let _ = Text::new(dow_str, Point::new(fb_w - 56, bar_y + 30), date_style).draw(fb);
    } else {
        let _ = Text::new("LUNAS", Point::new(fb_w - 56, bar_y + 18), clock_style).draw(fb);
    }

    // "Show Desktop" button — thin strip at the very right edge
    let show_desk_color = if input.show_desktop_active {
        Rgb888::new(0, 128, 255)
    } else if input.hovered_taskbar_element == crate::input::TaskbarHit::ShowDesktop {
        Rgb888::new(60, 80, 130)
    } else {
        Rgb888::new(25, 30, 50)
    };
    let show_desk_style = PrimitiveStyleBuilder::new().fill_color(show_desk_color).build();
    let _ = Rectangle::new(Point::new(fb_w - 6, bar_y), Size::new(6, bar_h as u32))
        .into_styled(show_desk_style)
        .draw(fb);
    // Separator line to the left of the button
    let _ = Rectangle::new(Point::new(fb_w - 7, bar_y + 8), Size::new(1, 28))
        .into_styled(sep_style)
        .draw(fb);

    // Drag-and-drop indicator: ghost outline on target PinnedApp icon
    if let Some(drag_src) = input.dragging_pinned_app {
        let hover_hit = input.hovered_taskbar_element;
        if let crate::input::TaskbarHit::PinnedApp(tgt) = hover_hit {
            if tgt != drag_src {
                let target_x = TASKBAR_APPS_START_X + tgt as i32 * (TASKBAR_ICON_SIZE + TASKBAR_ICON_SPACING);
                let ghost_style = PrimitiveStyleBuilder::new()
                    .stroke_color(Rgb888::new(0, 180, 255))
                    .stroke_width(2)
                    .build();
                let _ = Rectangle::new(
                    Point::new(target_x - 1, bar_y + 5),
                    Size::new(TASKBAR_ICON_SIZE as u32 + 2, TASKBAR_ICON_SIZE as u32 + 2),
                )
                .into_styled(ghost_style)
                .draw(fb);
            }
        }
    }

    // Bottom accent for branding (now aligned to clock position)
    let accent_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(0, 100, 200))
        .build();
    let _ = Rectangle::new(Point::new(fb_w - 56, bar_y + bar_h - 2), Size::new(50, 2))
        .into_styled(accent_style)
        .draw(fb);

    // ── Tooltip: floating label above the hovered taskbar element ──
    let tooltip = input.tooltip.as_str();
    if !tooltip.is_empty() && input.hovered_taskbar_element != crate::input::TaskbarHit::None {
        let char_w = FONT_CHAR_WIDTH;
        let tip_len = tooltip.len().min(32) as i32;
        let tip_w = tip_len * char_w + 10;
        let tip_h = 16;
        // Position the tooltip above the taskbar, horizontally centred on the cursor
        let tip_x = (input.cursor_x - tip_w / 2).clamp(2, fb_w - tip_w - 2);
        let tip_y = bar_y - tip_h - 4;

        // Background pill
        let tip_bg_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(30, 35, 60))
            .build();
        let _ = Rectangle::new(Point::new(tip_x, tip_y), Size::new(tip_w as u32, tip_h as u32))
            .into_styled(tip_bg_style)
            .draw(fb);
        // Border
        let tip_border_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(60, 80, 140))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(tip_x, tip_y), Size::new(tip_w as u32, tip_h as u32))
            .into_styled(tip_border_style)
            .draw(fb);
        // Text
        let tip_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 240));
        let _ = Text::new(
            &tooltip[..tooltip.len().min(32)],
            Point::new(tip_x + 5, tip_y + tip_h - 3),
            tip_text_style,
        )
        .draw(fb);
    }
}

/// Render simulated terminal content into the window content area.
/// Used by `WindowContent::InternalDemo` and as fallback for `WindowContent::Snp`
/// when the client has not yet committed a pixel buffer.
fn draw_terminal_demo(
    fb: &mut FramebufferState,
    cx: i32,
    content_y: i32,
    cw: i32,
    content_h: i32,
) {
    // Dark navy terminal background
    let demo_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(15, 20, 35)).build();
    let _ = Rectangle::new(Point::new(cx, content_y), Size::new(cw as u32, content_h as u32))
        .into_styled(demo_style)
        .draw(fb);

    // Header separator line
    let sep_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(40, 50, 80)).build();
    let _ = Rectangle::new(Point::new(cx, content_y), Size::new(cw as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    let green = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 200, 100));
    let white = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let cyan  = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 200, 220));
    let dim   = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));

    let line_h = 14;
    let x0 = cx + 8;
    let max_y = content_y + content_h;
    let mut ly = content_y + 4 + 12;

    let lines: &[(&str, &MonoTextStyle<Rgb888>)] = &[
        ("lunas@eclipse:~$ ls /usr/share/lunas", &white),
        ("bin/  config/  fonts/  icons/  themes/", &cyan),
        ("lunas@eclipse:~$ uname -a", &white),
        ("Eclipse OS 1.0 lunas-compositor x86_64", &cyan),
        ("lunas@eclipse:~$ cat /etc/motd", &white),
        ("Welcome to Eclipse OS - Lunas Desktop", &green),
        ("lunas@eclipse:~$ uptime", &white),
        ("up 0 days, 00:42, load: 0.12 0.08 0.05", &cyan),
        ("lunas@eclipse:~$ free -h", &white),
        ("Mem:  512M total, 387M used, 125M free", &cyan),
        ("lunas@eclipse:~$ ps aux | head -3", &white),
        ("PID  USER   CMD", &dim),
        ("  1  root   /sbin/init", &cyan),
        ("  2  lunas  compositor", &cyan),
        ("lunas@eclipse:~$ ", &green),
    ];

    for &(text, style) in lines {
        if ly > max_y - 4 { break; }
        let _ = Text::new(text, Point::new(x0, ly), *style).draw(fb);
        ly += line_h;
    }

    // Cursor block at end of last prompt line
    let prompt_text = "lunas@eclipse:~$ ";
    let cursor_x = x0 + (prompt_text.len() as i32) * 6;
    let cursor_y_top = ly - line_h - 2;
    if cursor_y_top < max_y - 4 {
        let cursor_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::WHITE).build();
        let _ = Rectangle::new(Point::new(cursor_x, cursor_y_top), Size::new(6, 12))
            .into_styled(cursor_style)
            .draw(fb);
    }
}

/// Draw a single window with decorations.
/// `decoration_style`: 0 = default (dark blue), 1 = minimal (charcoal), 2 = neon (cyan accent).
fn draw_window(
    fb: &mut FramebufferState,
    window: &ShellWindow,
    surfaces: &[ExternalSurface],
    snp_surfaces: &std::collections::BTreeMap<(u32, u32), ExternalSurface>,
    focused: bool,
    decoration_style: u8,
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

    // Title bar — colour varies with decoration style
    let title_color = match decoration_style {
        1 => if focused { TITLE_MINIMAL_FOCUSED } else { TITLE_MINIMAL_UNFOCUSED }, // minimal
        2 => if focused { TITLE_NEON_FOCUSED } else { TITLE_NEON_UNFOCUSED },       // neon
        _ => if focused { TITLE_DEFAULT_FOCUSED } else { TITLE_DEFAULT_UNFOCUSED }, // default
    };
    let title_style = PrimitiveStyleBuilder::new().fill_color(title_color).build();
    let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw as u32, ShellWindow::TITLE_H as u32))
        .into_styled(title_style)
        .draw(fb);

    // Neon style: a bright accent line at the top of the title bar
    if decoration_style == 2 {
        let neon_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 240, 220)).build();
        let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw as u32, 2))
            .into_styled(neon_style)
            .draw(fb);
    }

    // Title text
    let title_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 230));
    let title = window.title_str();
    let _ = Text::new(title, Point::new(cx + 8, cy + 18), title_text_style).draw(fb);

    // Window control buttons
    let close_x = cx + cw - 21;
    let btn_y = cy + 6;
    let btn_size = 12;
    
    // Close (Red)
    let close_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(220, 50, 50)).build();
    let _ = Circle::new(Point::new(close_x + 2, btn_y + 2), btn_size)
        .into_styled(close_style)
        .draw(fb);

    // Maximize (Yellow)
    let max_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(230, 180, 50)).build();
    let _ = Circle::new(Point::new(close_x - 19, btn_y + 2), btn_size)
        .into_styled(max_style)
        .draw(fb);

    // Minimize (Green)
    let min_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(50, 200, 80)).build();
    let _ = Circle::new(Point::new(close_x - 40, btn_y + 2), btn_size)
        .into_styled(min_style)
        .draw(fb);

    let content_y = cy + ShellWindow::TITLE_H;
    let content_h = ch - ShellWindow::TITLE_H;
    
    if content_h > 0 {
        match window.content {
            WindowContent::Snp { surface_id, pid } => {
                if let Some(surface) = snp_surfaces.get(&(pid, surface_id)) {
                    if surface.active && surface.vaddr != 0 {
                        let mut src_ptr = surface.vaddr as *const u32;
                        let fb_ptr = fb.back_addr as *mut u32;
                        let fb_stride = fb.info.width as usize;
                        
                        for row in 0..content_h {
                            let dy = content_y + row;
                            if dy < 0 || dy >= fb.info.height as i32 { continue; }
                            
                            let dest_idx = (dy as usize) * fb_stride + (cx as usize);
                            let _ = unsafe {
                                core::ptr::copy_nonoverlapping(
                                    src_ptr,
                                    fb_ptr.add(dest_idx),
                                    cw as usize
                                )
                            };
                            src_ptr = unsafe { src_ptr.add(cw as usize) };
                        }
                    } else {
                        draw_terminal_demo(fb, cx, content_y, cw, content_h);
                    }
                } else {
                    draw_terminal_demo(fb, cx, content_y, cw, content_h);
                }
            }
            WindowContent::External(s_idx) => {
                let s = s_idx as usize;
                if s < surfaces.len() && surfaces[s].active && surfaces[s].ready_to_flip {
                    // El SHM tiene stride = buffer_w (CREATE). No usar window.w como stride:
                    // si el marco es más ancho que el buffer, leer con window.w corrompe filas.
                    let buf_w = surfaces[s].buffer_w.max(1);
                    let buf_h = surfaces[s].buffer_h.max(1);
                    let want_w = window.w.max(0) as u32;
                    let want_h = ((window.h - ShellWindow::TITLE_H).max(0) as u32)
                        .min(content_h as u32);
                    let src_w = want_w.min(buf_w);
                    let src_h = want_h.min(buf_h);

                    if src_w > 0 && src_h > 0 {
                        fb.blit_buffer(surfaces[s].vaddr, src_w, src_h, buf_w, cx, content_y);
                    }
                } else {
                    draw_terminal_demo(fb, cx, content_y, cw, content_h);
                }
            }
            WindowContent::InternalDemo => {
                draw_terminal_demo(fb, cx, content_y, cw, content_h);
            }
            WindowContent::X11 { .. } => {
                draw_terminal_demo(fb, cx, content_y, cw, content_h);
            }
            WindowContent::None => {}
        }
    }

    // Focus highlight border
    if focused {
        let highlight_color = match decoration_style {
            1 => Rgb888::new(100, 110, 140),
            2 => Rgb888::new(0, 240, 220),
            _ => Rgb888::new(0, 128, 255),
        };
        let highlight_style = PrimitiveStyleBuilder::new()
            .stroke_color(highlight_color)
            .stroke_width(2)
            .build();
        let _ = Rectangle::new(Point::new(cx, cy), Size::new(cw as u32, ch as u32))
            .into_styled(highlight_style)
            .draw(fb);
    }
}

/// Draw the system dashboard overlay.
fn draw_dashboard(fb: &mut FramebufferState, cpu_usage: f32, mem_usage: f32, net_usage: f32,
    cpu_history: &[f32; 60], mem_history: &[f32; 60], net_history: &[f32; 60], history_pos: usize,
    cpu_temp: u32) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let panel_w = 400;
    let panel_h = 360;
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

    // CPU gauge + sparkline
    draw_gauge(fb, px + 50, py + 50, 330, 30, cpu_usage, "CPU", Rgb888::new(0, 200, 100));
    draw_sparkline(fb, px + 50, py + 80, 330, 18, cpu_history, history_pos, Rgb888::new(0, 200, 100));

    // Memory gauge + sparkline
    draw_gauge(fb, px + 50, py + 110, 330, 30, mem_usage, "MEM", Rgb888::new(0, 150, 255));
    draw_sparkline(fb, px + 50, py + 140, 330, 18, mem_history, history_pos, Rgb888::new(0, 150, 255));

    // Network gauge + sparkline
    draw_gauge(fb, px + 50, py + 170, 330, 30, net_usage, "NET", Rgb888::new(200, 100, 255));
    draw_sparkline(fb, px + 50, py + 200, 330, 18, net_history, history_pos, Rgb888::new(200, 100, 255));

    // CPU temperature display
    if cpu_temp > 0 {
        let temp_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 180, 100));
        let mut buf = [0u8; 16];
        let s = format_temp(&mut buf, cpu_temp);
        let _ = Text::new(s, Point::new(px + 50, py + 240), temp_style).draw(fb);
    }

    // Status text
    let status_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let _ = Text::new("Press Super to close", Point::new(px + 110, py + 340), status_style).draw(fb);
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

    // Icon next to gauge
    let icon_data = match label {
        "CPU" => Some(assets::CPU_ICON),
        "MEM" => Some(assets::RAM_ICON),
        "NET" => Some(assets::NETWORK_ICON),
        _ => None,
    };
    if let Some(data) = icon_data {
        draw_raw_icon(fb, data, x - 35, y + 3, 24, 24);
    }

    // Label
    let label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new(label, Point::new(x + 4, y + h - 8), label_style).draw(fb);
}

/// Draw a mini sparkline (line graph) from a ring buffer of values.
fn draw_sparkline(fb: &mut FramebufferState, x: i32, y: i32, w: i32, h: i32,
    history: &[f32; 60], history_pos: usize, color: Rgb888) {
    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(20, 24, 40)).build();
    let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let n = 60usize;
    let n_i32 = n as i32;
    let dot_style = PrimitiveStyleBuilder::new().fill_color(color).build();
    for i in 0..n {
        let data_idx = (history_pos + i) % n;
        let val = history[data_idx].clamp(0.0, 100.0);
        let px = x + (i as i32 * w) / n_i32;
        let py = y + h - 1 - ((val / 100.0) * (h - 1) as f32) as i32;
        let _ = Rectangle::new(Point::new(px, py.max(y)), Size::new(2, 2))
            .into_styled(dot_style)
            .draw(fb);
    }
}

/// Format a CPU temperature value as "CPU: XXdegC" into a provided buffer.
/// The monospace bitmap font used by lunas does not include the Unicode degree
/// glyph (U+00B0), so we write the ASCII suffix "degC" instead.
fn format_temp<'a>(buf: &'a mut [u8; 16], temp: u32) -> &'a str {
    // "CPU: " prefix
    buf[0] = b'C'; buf[1] = b'P'; buf[2] = b'U'; buf[3] = b':'; buf[4] = b' ';
    let mut pos = 5;
    // Write temperature digits
    if temp >= 100 {
        buf[pos] = b'0' + ((temp / 100) % 10) as u8; pos += 1;
    }
    if temp >= 10 {
        buf[pos] = b'0' + ((temp / 10) % 10) as u8; pos += 1;
    }
    buf[pos] = b'0' + (temp % 10) as u8; pos += 1;
    // Suffix "degC" (ASCII workaround; font lacks U+00B0 degree symbol)
    buf[pos..pos + 4].copy_from_slice(b"degC");
    pos += 4;
    core::str::from_utf8(&buf[..pos]).unwrap_or("CPU: ?")
}
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

/// Draw the lock screen overlay with PIN entry pad.
fn draw_lock_screen(fb: &mut FramebufferState, input: &InputState) {
    let fb_w = fb.info.width;
    let fb_h = fb.info.height;

    // Full-screen dark overlay
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 8, 18)).build();
    let _ = Rectangle::new(Point::zero(), Size::new(fb_w, fb_h))
        .into_styled(bg_style)
        .draw(fb);

    let cx = (fb_w as i32) / 2;
    let cy = (fb_h as i32) / 2 - 80;

    // "LOCKED" label at top
    let locked_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let _ = Text::new("LOCKED", Point::new(cx - 18, cy - 30), locked_style).draw(fb);

    // Title
    let text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new("SISTEMA BLOQUEADO", Point::new(cx - 54, cy), text_style).draw(fb);

    // PIN dots (4 circles)
    let dot_y = cy + 25;
    let dot_spacing = 30;
    let dot_start_x = cx - (dot_spacing * 2) + dot_spacing / 2;
    for i in 0..4 {
        let dx = dot_start_x + i * dot_spacing;
        if (i as usize) < input.lock_pin_len {
            // Filled dot
            let filled = PrimitiveStyleBuilder::new().fill_color(Rgb888::WHITE).build();
            let _ = Rectangle::new(Point::new(dx - 5, dot_y - 5), Size::new(10, 10))
                .into_styled(filled)
                .draw(fb);
        } else {
            // Empty outline
            let outline = PrimitiveStyleBuilder::new()
                .stroke_color(Rgb888::new(100, 120, 160))
                .stroke_width(1)
                .build();
            let _ = Rectangle::new(Point::new(dx - 5, dot_y - 5), Size::new(10, 10))
                .into_styled(outline)
                .draw(fb);
        }
    }

    // PIN pad grid: 3 columns x 4 rows
    let btn_w: i32 = 50;
    let btn_h: i32 = 40;
    let gap: i32 = 5;
    let grid_w = btn_w * 3 + gap * 2;
    let grid_x = cx - grid_w / 2;
    let grid_y = cy + 60;

    let digits: &[&str; 12] = &["1","2","3","4","5","6","7","8","9","*","0","#"];
    let btn_bg = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 40, 70)).build();
    let btn_text = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);

    for row in 0..4 {
        for col in 0..3 {
            let idx = row * 3 + col;
            let bx = grid_x + col as i32 * (btn_w + gap);
            let by = grid_y + row as i32 * (btn_h + gap);
            let _ = Rectangle::new(Point::new(bx, by), Size::new(btn_w as u32, btn_h as u32))
                .into_styled(btn_bg)
                .draw(fb);
            let _ = Text::new(digits[idx], Point::new(bx + btn_w / 2 - 3, by + btn_h / 2 + 4), btn_text).draw(fb);
        }
    }

    // Hint text
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let hint_y = grid_y + 4 * (btn_h + gap) + 10;
    let _ = Text::new("Enter to confirm", Point::new(cx - 48, hint_y), hint_style).draw(fb);
    let _ = Text::new("Backspace to delete", Point::new(cx - 57, hint_y + 16), hint_style).draw(fb);

    // Failed attempts warning
    if input.lock_pin_attempts > 0 {
        let red_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(255, 60, 60));
        let mut msg_buf = [0u8; 32];
        let msg = format_pin_fail(&mut msg_buf, input.lock_pin_attempts);
        let _ = Text::new(msg, Point::new(cx - 72, hint_y + 36), red_style).draw(fb);
    }
}

/// Format "Incorrect PIN (N attempts)" into a provided buffer.
fn format_pin_fail<'a>(buf: &'a mut [u8; 32], attempts: u8) -> &'a str {
    let prefix = b"Incorrect PIN (";
    let suffix = b" attempts)";
    let mut pos = 0;
    for &b in prefix { buf[pos] = b; pos += 1; }
    if attempts >= 10 {
        buf[pos] = b'0' + (attempts / 10); pos += 1;
    }
    buf[pos] = b'0' + (attempts % 10); pos += 1;
    for &b in suffix { if pos < 32 { buf[pos] = b; pos += 1; } }
    core::str::from_utf8(&buf[..pos]).unwrap_or("Incorrect PIN")
}

/// Launcher panel constants.
pub const LAUNCHER_PANEL_W: i32 = 300;
pub const LAUNCHER_PANEL_H: i32 = 400;
pub const LAUNCHER_PANEL_X: i32 = 10;
pub const LAUNCHER_ITEM_H: i32 = 36;
pub const LAUNCHER_ITEMS_Y_OFFSET: i32 = 50;
/// Maximum items visible in the launcher.
pub const LAUNCHER_MAX_VISIBLE: usize = 9;

/// Width of the notification panel.
pub const NOTIF_PANEL_W: i32 = 300;
/// Height of the notification panel.
pub const NOTIF_PANEL_H: i32 = 250;

/// Width of the volume popup panel.
pub const VOLUME_PANEL_W: i32 = 180;
/// Height of the volume popup panel.
pub const VOLUME_PANEL_H: i32 = 100;

/// Context menu regular item height.
pub const CONTEXT_MENU_ITEM_H: i32 = 28;
/// Context menu separator height (thin visual divider).
pub const CONTEXT_MENU_SEP_H: i32 = 8;
/// Context menu width.
pub const CONTEXT_MENU_W: i32 = 200;

/// Width of the clock/calendar panel.
pub const CLOCK_PANEL_W: i32 = 168;
/// Height of the clock/calendar panel.
pub const CLOCK_PANEL_H: i32 = 128;

/// Width of the network configuration panel.
pub const NET_CONFIG_PANEL_W: i32 = 480;
/// Height of the network configuration panel.
pub const NET_CONFIG_PANEL_H: i32 = 290;

/// Compute the launcher panel bounds (x, y, w, h) given the framebuffer height.
pub fn launcher_panel_bounds(fb_height: i32) -> (i32, i32, i32, i32) {
    let py = fb_height - TASKBAR_HEIGHT - LAUNCHER_PANEL_H - 10;
    (LAUNCHER_PANEL_X, py, LAUNCHER_PANEL_W, LAUNCHER_PANEL_H)
}

/// Draw the app launcher panel.
fn draw_launcher(fb: &mut FramebufferState, desktop: &DesktopShell, input: &InputState) {
    let fb_h = fb.info.height as i32;
    let panel_w = LAUNCHER_PANEL_W;
    let panel_h = LAUNCHER_PANEL_H;
    let px = LAUNCHER_PANEL_X;
    let py = fb_h - TASKBAR_HEIGHT - panel_h - 10;

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

    // Search hint if search is active
    if input.search_active && !input.search_query.is_empty() {
        draw_raw_icon(fb, assets::SEARCH_ICON, px + 10, py + 32, 24, 24);
        let query_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 220, 255));
        let _ = Text::new(input.search_query.as_str(), Point::new(px + 40, py + 48), query_style).draw(fb);
    }

    // Render pinned apps from desktop (filtered by search query if active)
    let hovered_launcher = input.launcher_hovered_index;
    let keyboard_launcher = input.launcher_keyboard_index;

    let mut visible_idx: i32 = 0;
    for i in 0..desktop.pinned_count {
        if visible_idx >= LAUNCHER_MAX_VISIBLE as i32 { break; }
        let app = &desktop.pinned_apps[i];
        let app_name = app.name_str();

        // Filter by search query
        if input.search_active && !input.search_query.is_empty() {
            let query = input.search_query.as_str();
            let name_lower_matches = app_name.len() >= query.len()
                && app_name[..query.len()].eq_ignore_ascii_case(query);
            if !name_lower_matches { continue; }
        }

        let app_y = py + LAUNCHER_ITEMS_Y_OFFSET + visible_idx * LAUNCHER_ITEM_H;
        let (r, g, b) = app.icon_color;

        // Keyboard selection highlight (brighter accent border) or mouse hover highlight
        let is_keyboard_selected = keyboard_launcher == Some(i);
        let is_hovered = hovered_launcher == Some(i);
        if is_keyboard_selected {
            let sel_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(20, 50, 100))
                .stroke_color(Rgb888::new(0, 140, 255))
                .stroke_width(1)
                .build();
            let _ = Rectangle::new(
                Point::new(px + 4, app_y - 10),
                Size::new((panel_w - 8) as u32, LAUNCHER_ITEM_H as u32),
            )
            .into_styled(sel_style)
            .draw(fb);
        } else if is_hovered {
            let hover_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(30, 40, 70))
                .build();
            let _ = Rectangle::new(
                Point::new(px + 4, app_y - 10),
                Size::new((panel_w - 8) as u32, LAUNCHER_ITEM_H as u32),
            )
            .into_styled(hover_style)
            .draw(fb);
        }

        // App icon (minimalist)
        let icon_data = match app_name {
            n if n.eq_ignore_ascii_case("Terminal") => Some(assets::TERMINAL_ICON),
            n if n.eq_ignore_ascii_case("Files") => Some(assets::FILES_ICON),
            n if n.eq_ignore_ascii_case("Editor") => Some(assets::EDITOR_ICON),
            n if n.eq_ignore_ascii_case("Browser") => Some(assets::BROWSER_ICON),
            n if n.eq_ignore_ascii_case("Settings") => Some(assets::SETTINGS_ICON),
            _ => None,
        };

        if let Some(data) = icon_data {
            draw_raw_icon(fb, data, px + 12, app_y - 8, 32, 32);
        } else {
            // Fallback: App icon (colored square with first letter)
            let icon_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(r, g, b)).build();
            let _ = Rectangle::new(Point::new(px + 12, app_y - 8), Size::new(24, 24))
                .into_styled(icon_style)
                .draw(fb);
            let first_char = app_name.chars().next().unwrap_or('?');
            let mut char_buf = [0u8; 4];
            let char_str = first_char.encode_utf8(&mut char_buf);
            let icon_letter_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
            let _ = Text::new(char_str, Point::new(px + 18, app_y + 4), icon_letter_style).draw(fb);
        }

        // App name (bright when keyboard-selected)
        let name_color = if is_keyboard_selected {
            Rgb888::new(255, 255, 255)
        } else {
            Rgb888::new(200, 210, 230)
        };
        let name_style = MonoTextStyle::new(&FONT_6X12, name_color);
        let _ = Text::new(app_name, Point::new(px + 44, app_y + 4), name_style).draw(fb);

        // Exec path hint (dim)
        let exec_str = app.exec_path_str();
        if !exec_str.is_empty() {
            let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(70, 80, 110));
            let _ = Text::new(exec_str, Point::new(px + 44, app_y + 16), hint_style).draw(fb);
        }

        visible_idx += 1;
    }

    if visible_idx == 0 {
        let empty_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 110));
        let _ = Text::new("No matching apps", Point::new(px + 80, py + 200), empty_style).draw(fb);
    }

    // Keyboard navigation hint at the bottom of the launcher panel.
    // \x18 = ASCII CAN (↑ arrow glyph in CP437/bitmap fonts), \x19 = EM (↓ arrow glyph).
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(60, 70, 100));
    let _ = Text::new(
        "^v sel  Enter launch  Esc close",
        Point::new(px + 8, py + panel_h - 10),
        hint_style,
    )
    .draw(fb);
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

    // Icon
    draw_raw_icon(fb, assets::SEARCH_ICON, px + 12, py + 8, 24, 24);

    let text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    if query.is_empty() {
        let placeholder_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 110, 130));
        let _ = Text::new("Search apps...", Point::new(px + 44, py + 26), placeholder_style).draw(fb);
    } else {
        let _ = Text::new(query, Point::new(px + 44, py + 26), text_style).draw(fb);
    }
}

/// Draw the notifications panel.
fn draw_notifications(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let fb_w = fb.info.width as i32;
    let panel_w = NOTIF_PANEL_W;
    let panel_h = NOTIF_PANEL_H;
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

    // DND status banner
    if desktop.do_not_disturb {
        let dnd_bg_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(60, 20, 20))
            .build();
        let _ = Rectangle::new(Point::new(px + 4, py + 30), Size::new((panel_w - 8) as u32, 14))
            .into_styled(dnd_bg_style)
            .draw(fb);
        let dnd_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(220, 100, 100));
        let _ = Text::new("Do Not Disturb is ON", Point::new(px + 42, py + 40), dnd_text).draw(fb);
    }

    // Display notifications
    let notif_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    let items_start_y = if desktop.do_not_disturb { py + 54 } else { py + 50 };
    for i in 0..desktop.notification_count.min(8) {
        let notif = &desktop.notifications[i];
        let notif_y = items_start_y + (i as i32) * 25;
        if notif_y + 12 > py + panel_h { break; }
        let msg_len = notif.message.iter().position(|&b| b == 0).unwrap_or(64);
        let msg = core::str::from_utf8(&notif.message[..msg_len]).unwrap_or("");

        // Unread indicator dot
        if !notif.read {
            let dot_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 140, 255))
                .build();
            let _ = Rectangle::new(Point::new(px + 6, notif_y - 8), Size::new(4, 4))
                .into_styled(dot_style)
                .draw(fb);
        }
        // Notification Icon (Bell)
        draw_raw_icon(fb, assets::NOTIFICATION_ICON, px + 8, notif_y - 12, 20, 20);
        let _ = Text::new(msg, Point::new(px + 32, notif_y), notif_style).draw(fb);

        // Dismiss [×] button
        let dismiss_x = px + panel_w - 18;
        let dismiss_y = notif_y - 10;
        let dismiss_bg = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(180, 40, 40))
            .build();
        let _ = Rectangle::new(Point::new(dismiss_x, dismiss_y), Size::new(12, 12))
            .into_styled(dismiss_bg)
            .draw(fb);
        let dismiss_text = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
        let _ = Text::new("x", Point::new(dismiss_x + 3, dismiss_y + 10), dismiss_text).draw(fb);
    }

    if desktop.notification_count == 0 {
        let empty_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 110));
        let _ = Text::new("No notifications", Point::new(px + 80, py + 130), empty_style).draw(fb);
    }
}

/// Draw the Quick Settings panel — a compact panel with common toggles.
/// Accessible via Super+Q; positioned in the top-right above the tray.
fn draw_quick_settings_panel(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let pw = 220i32;
    let ph = 220i32;
    let px = fb_w - pw - 10;
    let py = fb_h - TASKBAR_HEIGHT - ph - 5;

    // Background
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(18, 22, 40))
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(bg_style)
        .draw(fb);
    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("QUICK SETTINGS", Point::new(px + 42, py + 16), title_style).draw(fb);

    // Separator
    let sep_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(40, 50, 80))
        .build();
    let _ = Rectangle::new(Point::new(px + 4, py + 22), Size::new((pw - 8) as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    // Helper closure to draw a toggle row
    let draw_toggle = |fb: &mut FramebufferState, row_y: i32, label: &str, active: bool| {
        // Toggle background pill
        let pill_color = if active { Rgb888::new(0, 100, 200) } else { Rgb888::new(30, 36, 60) };
        let pill_style = PrimitiveStyleBuilder::new().fill_color(pill_color).build();
        let _ = Rectangle::new(Point::new(px + pw - 46, row_y), Size::new(36, 16))
            .into_styled(pill_style)
            .draw(fb);
        // Toggle knob
        let knob_x = if active { px + pw - 46 + 20 } else { px + pw - 46 + 2 };
        let knob_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::WHITE).build();
        let _ = Rectangle::new(Point::new(knob_x, row_y + 2), Size::new(14, 12))
            .into_styled(knob_style)
            .draw(fb);
        // Label
        let label_color = if active { Rgb888::new(200, 210, 255) } else { Rgb888::new(130, 140, 170) };
        let label_style = MonoTextStyle::new(&FONT_6X12, label_color);
        let _ = Text::new(label, Point::new(px + 10, row_y + 12), label_style).draw(fb);
    };

    draw_toggle(fb, py + 34, "Do Not Disturb", desktop.do_not_disturb);
    draw_toggle(fb, py + 62, "Night Light", desktop.night_light_active);
    draw_toggle(fb, py + 90, "Volume Mute", desktop.volume_muted);

    // Brightness row
    let br_label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(130, 140, 170));
    let _ = Text::new("Brightness", Point::new(px + 10, py + 130), br_label_style).draw(fb);

    // Brightness percentage label (right-aligned)
    let mut bri_pct_buf = [0u8; 8];
    let bri_pct_str = format_u32(&mut bri_pct_buf, desktop.brightness_level as u32);
    let bri_pct_x = px + pw - 4 - (bri_pct_str.len() as i32 + 1) * 6;
    let pct_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 200, 200));
    let _ = Text::new(bri_pct_str, Point::new(bri_pct_x, py + 130), pct_style).draw(fb);
    let _ = Text::new("%", Point::new(bri_pct_x + bri_pct_str.len() as i32 * 6, py + 130), pct_style).draw(fb);

    let bri_track_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 38, 60)).build();
    let _ = Rectangle::new(Point::new(px + 10, py + 134), Size::new((pw - 20) as u32, 6))
        .into_styled(bri_track_style)
        .draw(fb);
    let bri_fill_w = ((pw - 20) as u32 * desktop.brightness_level as u32) / 100;
    if bri_fill_w > 0 {
        let bri_fill_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(220, 200, 80)).build();
        let _ = Rectangle::new(Point::new(px + 10, py + 134), Size::new(bri_fill_w, 6))
            .into_styled(bri_fill_style)
            .draw(fb);
    }
    // Brightness thumb knob
    let bri_thumb_x = px + 10 + (bri_fill_w as i32) - 2;
    let thumb_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::WHITE).build();
    let _ = Rectangle::new(Point::new(bri_thumb_x.max(px + 10), py + 131), Size::new(4, 10))
        .into_styled(thumb_style)
        .draw(fb);

    // Volume row
    let vol_label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(130, 140, 170));
    let _ = Text::new("Volume", Point::new(px + 10, py + 160), vol_label_style).draw(fb);

    // Volume percentage label (right-aligned)
    let mut vol_pct_buf = [0u8; 8];
    let vol_pct_str = format_u32(&mut vol_pct_buf, desktop.volume_level as u32);
    let vol_pct_x = px + pw - 4 - (vol_pct_str.len() as i32 + 1) * 6;
    let _ = Text::new(vol_pct_str, Point::new(vol_pct_x, py + 160), pct_style).draw(fb);
    let _ = Text::new("%", Point::new(vol_pct_x + vol_pct_str.len() as i32 * 6, py + 160), pct_style).draw(fb);

    let vol_track_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 38, 60)).build();
    let _ = Rectangle::new(Point::new(px + 10, py + 164), Size::new((pw - 20) as u32, 6))
        .into_styled(vol_track_style)
        .draw(fb);
    let vol_fill_w = ((pw - 20) as u32 * desktop.volume_level as u32) / 100;
    if vol_fill_w > 0 {
        let vol_fill_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 150, 255)).build();
        let _ = Rectangle::new(Point::new(px + 10, py + 164), Size::new(vol_fill_w, 6))
            .into_styled(vol_fill_style)
            .draw(fb);
    }
    // Volume thumb knob
    let vol_thumb_x = px + 10 + (vol_fill_w as i32) - 2;
    let _ = Rectangle::new(Point::new(vol_thumb_x.max(px + 10), py + 161), Size::new(4, 10))
        .into_styled(thumb_style)
        .draw(fb);

    // Close hint
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(70, 80, 110));
    let _ = Text::new("Super+Q to close", Point::new(px + 30, py + ph - 10), hint_style).draw(fb);
}

/// Draw the battery/power info panel (toggled by clicking the battery tray icon).
pub fn draw_battery_panel(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let pw = 200i32;
    let ph = 160i32;
    // Positioned above the battery indicator in the tray (right side, above taskbar)
    let px = fb_w - pw - 10;
    let py = fb_h - TASKBAR_HEIGHT - ph - 5;

    // Background
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(18, 22, 40))
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(bg_style)
        .draw(fb);
    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("BATTERY", Point::new(px + 68, py + 16), title_style).draw(fb);

    // Separator
    let sep_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(40, 50, 80))
        .build();
    let _ = Rectangle::new(Point::new(px + 4, py + 22), Size::new((pw - 8) as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    // Battery level bar
    let level = desktop.battery_level.min(100) as u32;
    let bar_w = (pw - 40) as u32;
    let filled_w = bar_w * level / 100;

    // Track
    let track_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(30, 38, 60))
        .stroke_color(Rgb888::new(60, 70, 110))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px + 20, py + 38), Size::new(bar_w, 22))
        .into_styled(track_style)
        .draw(fb);

    // Fill colour: red < 20%, yellow < 50%, green otherwise
    let fill_color = if level < 20 {
        Rgb888::new(220, 40, 40)
    } else if level < 50 {
        Rgb888::new(220, 180, 40)
    } else {
        Rgb888::new(40, 200, 80)
    };
    if filled_w > 0 {
        let fill_style = PrimitiveStyleBuilder::new().fill_color(fill_color).build();
        let _ = Rectangle::new(Point::new(px + 20, py + 38), Size::new(filled_w, 22))
            .into_styled(fill_style)
            .draw(fb);
    }

    // Battery level percentage text
    let mut pct_buf = [0u8; 8];
    let pct_val = format_u32(&mut pct_buf, level as u32);
    // Append "%" to the number string in a small stack buffer
    let mut pct_full_buf = [0u8; 8];
    let pct_len = pct_val.len();
    pct_full_buf[..pct_len].copy_from_slice(pct_val.as_bytes());
    pct_full_buf[pct_len] = b'%';
    let pct_str = core::str::from_utf8(&pct_full_buf[..pct_len + 1]).unwrap_or("?%");
    let pct_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new(pct_str, Point::new(px + pw / 2 - 10, py + 54), pct_style).draw(fb);

    // Charging status
    let charge_label = if desktop.battery_charging { "Charging" } else { "On Battery" };
    let charge_color = if desktop.battery_charging {
        Rgb888::new(80, 220, 120)
    } else {
        Rgb888::new(180, 180, 200)
    };
    let charge_style = MonoTextStyle::new(&FONT_6X12, charge_color);
    let _ = Text::new(charge_label, Point::new(px + 10, py + 82), charge_style).draw(fb);

    // Separator
    let _ = Rectangle::new(Point::new(px + 4, py + 92), Size::new((pw - 8) as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    // Brightness row (re-uses brightness from desktop)
    let br_label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(130, 140, 170));
    let _ = Text::new("Brightness", Point::new(px + 10, py + 108), br_label_style).draw(fb);
    let bri_track_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 38, 60)).build();
    let _ = Rectangle::new(Point::new(px + 10, py + 112), Size::new((pw - 20) as u32, 6))
        .into_styled(bri_track_style)
        .draw(fb);
    let bri_fill_w = ((pw - 20) as u32 * desktop.brightness_level as u32) / 100;
    if bri_fill_w > 0 {
        let bri_fill_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(220, 200, 80)).build();
        let _ = Rectangle::new(Point::new(px + 10, py + 112), Size::new(bri_fill_w, 6))
            .into_styled(bri_fill_style)
            .draw(fb);
    }

    // Close hint
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(70, 80, 110));
    let _ = Text::new("Click battery to close", Point::new(px + 12, py + ph - 10), hint_style).draw(fb);
}

/// Draw the mouse cursor.
fn draw_cursor(fb: &mut FramebufferState, cx: i32, cy: i32) {
    // Premium minimalist cursor icon
    draw_raw_icon(fb, assets::CURSOR_ICON, cx, cy, 16, 16);
}

/// Draw the HUD overlay with kernel log messages.
fn draw_hud_overlay(fb: &mut FramebufferState, log_buf: &[u8], log_len: usize) {
    let hud_w = 400;
    let hud_h = 180; // Taller for more lines
    let hud_x = fb.info.width as i32 - hud_w - 20;
    // Positioned slightly above the taskbar
    let hud_y = fb.info.height as i32 - 44 - hud_h - 20;

    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 7, 15)).build();
    let _ = Rectangle::new(Point::new(hud_x, hud_y), Size::new(hud_w as u32, hud_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 100, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(hud_x, hud_y), Size::new(hud_w as u32, hud_h as u32))
        .into_styled(border_style)
        .draw(fb);

    let status_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 255, 120));
    let _ = Text::new("SISTEMA ONLINE", Point::new(hud_x + 10, hud_y + 18), status_style).draw(fb);

    // Log text — show the LATEST messages (scrolling logic)
    let log_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(140, 160, 200));
    let safe_len = log_len.min(log_buf.len());
    if let Ok(text) = core::str::from_utf8(&log_buf[..safe_len]) {
        let lines: std::vec::Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        let max_lines = 11;
        let start_idx = if lines.len() > max_lines { lines.len() - max_lines } else { 0 };
        
        let mut line_y = hud_y + 36;
        for &line in &lines[start_idx..] {
            let truncated = if line.len() > 60 { &line[..60] } else { line };
            let _ = Text::new(truncated, Point::new(hud_x + 10, line_y), log_style).draw(fb);
            line_y += 12;
        }
    }
}

/// Draw the context menu overlay.
fn draw_context_menu(fb: &mut FramebufferState, menu: &crate::input::ContextMenu) {
    let menu_w = CONTEXT_MENU_W;
    let mx = menu.x;
    let my = menu.y;
    let menu_h = menu.total_height();

    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(20, 24, 45)).build();
    let _ = Rectangle::new(Point::new(mx, my), Size::new(menu_w as u32, menu_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 90, 200))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(mx, my), Size::new(menu_w as u32, menu_h as u32))
        .into_styled(border_style)
        .draw(fb);

    // Items — rendered with variable heights
    let mut iy = my;
    for i in 0..menu.item_count {
        let item = &menu.items[i];
        if item.separator {
            // Separator: thin horizontal line centered in the SEP_H slot
            let line_y = iy + CONTEXT_MENU_SEP_H / 2;
            let sep_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(45, 55, 85))
                .build();
            let _ = Rectangle::new(Point::new(mx + 6, line_y), Size::new((menu_w - 12) as u32, 1))
                .into_styled(sep_style)
                .draw(fb);
            iy += CONTEXT_MENU_SEP_H;
            continue;
        }

        let item_h = CONTEXT_MENU_ITEM_H;

        // Hover highlight
        if menu.hovered_index == Some(i) {
            let hover_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(35, 50, 80))
                .build();
            let _ = Rectangle::new(Point::new(mx + 1, iy), Size::new((menu_w - 2) as u32, item_h as u32))
                .into_styled(hover_style)
                .draw(fb);
            // Left accent bar
            let accent_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 140, 255))
                .build();
            let _ = Rectangle::new(Point::new(mx + 1, iy), Size::new(2, item_h as u32))
                .into_styled(accent_style)
                .draw(fb);
        }

        // Checkmark indicator (filled square dot) before the label
        if item.checked {
            let dot_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 180, 100))
                .build();
            let _ = Rectangle::new(Point::new(mx + 7, iy + item_h / 2 - 3), Size::new(6, 6))
                .into_styled(dot_style)
                .draw(fb);
        }

        // Label — offset right to make room for checkmark area
        let text_color = if menu.hovered_index == Some(i) {
            Rgb888::new(220, 230, 255)
        } else {
            Rgb888::new(190, 200, 220)
        };
        let text_style = MonoTextStyle::new(&FONT_6X12, text_color);
        let label = item.label_str();
        let _ = Text::new(label, Point::new(mx + 18, iy + item_h / 2 + 5), text_style).draw(fb);

        iy += item_h;
    }
}

/// Draw the volume popup panel.
fn draw_volume_popup(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let panel_w = VOLUME_PANEL_W;
    let panel_h = VOLUME_PANEL_H;
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let tray_start = fb_w - TASKBAR_TRAY_WIDTH;
    // Centre the panel above the volume icon (now at tray_start + 96..120)
    let px = (tray_start + 18).min(fb_w - panel_w - 4);
    let py = fb_h - TASKBAR_HEIGHT - panel_h - 5;

    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(18, 22, 40)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("VOLUME", Point::new(px + 60, py + 20), title_style).draw(fb);

    // Mute indicator
    let mute_text = if desktop.volume_muted { "MUTED" } else { "ACTIVE" };
    let mute_color = if desktop.volume_muted {
        Rgb888::new(220, 50, 50)
    } else {
        Rgb888::new(0, 200, 100)
    };
    let mute_style = MonoTextStyle::new(&FONT_6X12, mute_color);
    let _ = Text::new(mute_text, Point::new(px + 60, py + 40), mute_style).draw(fb);

    // Volume bar background
    let bar_x = px + 15;
    let bar_y = py + 55;
    let bar_w = panel_w - 30;
    let bar_h: i32 = 16;
    let bar_bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(30, 35, 55)).build();
    let _ = Rectangle::new(Point::new(bar_x, bar_y), Size::new(bar_w as u32, bar_h as u32))
        .into_styled(bar_bg_style)
        .draw(fb);

    // Volume bar fill
    if !desktop.volume_muted {
        let fill_w = ((bar_w as u32) * (desktop.volume_level as u32)) / 100;
        if fill_w > 0 {
            let fill_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 150, 255))
                .build();
            let _ = Rectangle::new(Point::new(bar_x, bar_y), Size::new(fill_w, bar_h as u32))
                .into_styled(fill_style)
                .draw(fb);
        }
    }

    // Volume level text
    let level_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    let mut vol_buf = [0u8; 8];
    let vol_str = format_u32(&mut vol_buf, desktop.volume_level as u32);
    let _ = Text::new(vol_str, Point::new(px + 70, py + 88), level_style).draw(fb);
    let _ = Text::new("%", Point::new(px + 70 + (vol_str.len() as i32) * 6, py + 88), level_style).draw(fb);
}

/// Draw the network details panel.
fn draw_network_panel(
    fb: &mut FramebufferState,
    net_usage: f32,
    net_extended_stats: Option<&eclipse_ipc::types::NetExtendedStats>,
) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let panel_w: i32 = 300;
    let panel_h: i32 = 260;
    // Position above the network icon in the tray (right side, above taskbar)
    let tray_start = fb_w - TASKBAR_TRAY_WIDTH;
    let net_icon_x = tray_start + 126;
    let px = (net_icon_x - panel_w / 2).clamp(4, fb_w - panel_w - 4);
    let py = fb_h - TASKBAR_HEIGHT - panel_h - 5;

    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(12, 15, 28)).build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(bg_style)
        .draw(fb);

    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(panel_w as u32, panel_h as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("NETWORK DETAILS", Point::new(px + 60, py + 20), title_style).draw(fb);

    // Separator
    let sep_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(40, 50, 80)).build();
    let _ = Rectangle::new(Point::new(px + 4, py + 26), Size::new((panel_w - 8) as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    // Network usage gauge
    draw_gauge(fb, px + 12, py + 34, panel_w - 24, 20, net_usage, "NET", Rgb888::new(200, 100, 255));

    let info_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    let label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 130, 180));

    if let Some(stats) = net_extended_stats {
        // Loopback interface status
        let lo_status = if stats.lo_up != 0 { "UP" } else { "DOWN" };
        let lo_color = if stats.lo_up != 0 { Rgb888::new(0, 200, 100) } else { Rgb888::new(220, 50, 50) };
        let lo_style = MonoTextStyle::new(&FONT_6X12, lo_color);
        let _ = Text::new("lo:", Point::new(px + 12, py + 74), label_style).draw(fb);
        let _ = Text::new(lo_status, Point::new(px + 50, py + 74), lo_style).draw(fb);

        // lo IPv4
        let mut ip_buf = [0u8; 20];
        let ip_len = format_ipv4(&mut ip_buf, &stats.lo_ipv4);
        let _ = Text::new(core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("?"), Point::new(px + 90, py + 74), info_style).draw(fb);

        // eth0 interface status
        let eth_status = if stats.eth0_up != 0 { "UP" } else { "DOWN" };
        let eth_color = if stats.eth0_up != 0 { Rgb888::new(0, 200, 100) } else { Rgb888::new(220, 50, 50) };
        let eth_style = MonoTextStyle::new(&FONT_6X12, eth_color);
        let _ = Text::new("eth0:", Point::new(px + 12, py + 96), label_style).draw(fb);
        let _ = Text::new(eth_status, Point::new(px + 60, py + 96), eth_style).draw(fb);

        // eth0 IPv4
        let mut ip_buf2 = [0u8; 20];
        let ip_len2 = format_ipv4(&mut ip_buf2, &stats.eth0_ipv4);
        let _ = Text::new(core::str::from_utf8(&ip_buf2[..ip_len2]).unwrap_or("?"), Point::new(px + 100, py + 96), info_style).draw(fb);

        // Gateway
        let _ = Text::new("Gateway:", Point::new(px + 12, py + 118), label_style).draw(fb);
        let mut gw_buf = [0u8; 20];
        let gw_len = format_ipv4(&mut gw_buf, &stats.eth0_gateway);
        let _ = Text::new(core::str::from_utf8(&gw_buf[..gw_len]).unwrap_or("?"), Point::new(px + 100, py + 118), info_style).draw(fb);

        // DNS
        let _ = Text::new("DNS:", Point::new(px + 12, py + 140), label_style).draw(fb);
        let mut dns_buf = [0u8; 20];
        let dns_len = format_ipv4(&mut dns_buf, &stats.eth0_dns);
        let _ = Text::new(core::str::from_utf8(&dns_buf[..dns_len]).unwrap_or("?"), Point::new(px + 100, py + 140), info_style).draw(fb);

        // RX bytes
        let _ = Text::new("RX:", Point::new(px + 12, py + 168), label_style).draw(fb);
        let mut buf3 = [0u8; 16];
        let rx_bytes_str = format_u64(&mut buf3, stats.rx_bytes);
        let _ = Text::new(rx_bytes_str, Point::new(px + 50, py + 168), info_style).draw(fb);

        // TX bytes
        let _ = Text::new("TX:", Point::new(px + 12, py + 188), label_style).draw(fb);
        let mut buf4 = [0u8; 16];
        let tx_bytes_str = format_u64(&mut buf4, stats.tx_bytes);
        let _ = Text::new(tx_bytes_str, Point::new(px + 50, py + 188), info_style).draw(fb);
    } else {
        let _ = Text::new("No extended stats", Point::new(px + 60, py + 120), label_style).draw(fb);
        let _ = Text::new("available", Point::new(px + 90, py + 140), label_style).draw(fb);
    }

    // Close hint
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 110));
    let _ = Text::new("Super+E to close", Point::new(px + 60, py + 246), hint_style).draw(fb);
}

/// Draw the network configuration panel (DHCP / Static IP settings).
fn draw_network_config_panel(fb: &mut FramebufferState, input: &InputState) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let pw = NET_CONFIG_PANEL_W;
    let ph = NET_CONFIG_PANEL_H;
    // Centre the panel vertically above the taskbar
    let px = (fb_w - pw) / 2;
    let py = (fb_h - TASKBAR_HEIGHT - ph) / 2;

    // Semi-transparent dark backdrop
    let backdrop_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(5, 8, 20))
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(backdrop_style)
        .draw(fb);

    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 100, 220))
        .stroke_width(2)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title bar
    let title_bar_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(10, 20, 50))
        .build();
    let _ = Rectangle::new(Point::new(px + 2, py + 2), Size::new((pw - 4) as u32, 22))
        .into_styled(title_bar_style)
        .draw(fb);
    let title_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let _ = Text::new("CONFIGURACION DE RED", Point::new(px + 130, py + 16), title_style).draw(fb);

    // Close [×] button at top-right
    let close_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 80, 80));
    let _ = Text::new("[x]", Point::new(px + pw - 24, py + 16), close_style).draw(fb);

    // Separator below title
    let sep_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(30, 45, 80))
        .build();
    let _ = Rectangle::new(Point::new(px + 4, py + 26), Size::new((pw - 8) as u32, 1))
        .into_styled(sep_style)
        .draw(fb);

    // ── DHCP / Static toggle buttons ──
    let btn_y = py + 34;
    let btn_h: u32 = 22;

    // DHCP button
    let dhcp_active = !input.net_manual_mode;
    let dhcp_fill = if dhcp_active { Rgb888::new(0, 90, 200) } else { Rgb888::new(20, 25, 50) };
    let dhcp_stroke = if dhcp_active { Rgb888::new(0, 180, 255) } else { Rgb888::new(60, 70, 110) };
    let dhcp_btn_style = PrimitiveStyleBuilder::new()
        .fill_color(dhcp_fill)
        .stroke_color(dhcp_stroke)
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px + 20, btn_y), Size::new(110, btn_h))
        .into_styled(dhcp_btn_style)
        .draw(fb);
    let dhcp_text_color = if dhcp_active { Rgb888::new(0, 220, 255) } else { Rgb888::new(140, 150, 180) };
    let dhcp_text_style = MonoTextStyle::new(&FONT_6X12, dhcp_text_color);
    let _ = Text::new("DHCP (Auto)", Point::new(px + 30, btn_y + 15), dhcp_text_style).draw(fb);

    // Static/Manual button
    let static_active = input.net_manual_mode;
    let static_fill = if static_active { Rgb888::new(0, 90, 200) } else { Rgb888::new(20, 25, 50) };
    let static_stroke = if static_active { Rgb888::new(0, 180, 255) } else { Rgb888::new(60, 70, 110) };
    let static_btn_style = PrimitiveStyleBuilder::new()
        .fill_color(static_fill)
        .stroke_color(static_stroke)
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px + 140, btn_y), Size::new(130, btn_h))
        .into_styled(static_btn_style)
        .draw(fb);
    let static_text_color = if static_active { Rgb888::new(0, 220, 255) } else { Rgb888::new(140, 150, 180) };
    let static_text_style = MonoTextStyle::new(&FONT_6X12, static_text_color);
    let _ = Text::new("IP Estatica", Point::new(px + 155, btn_y + 15), static_text_style).draw(fb);

    let label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 130, 180));
    let value_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 200, 230));
    let edit_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);

    if !input.net_manual_mode {
        // ── DHCP mode: show current config read-only ──
        let info_y = py + 80;
        let _ = Text::new("Modo:", Point::new(px + 16, info_y), label_style).draw(fb);
        let _ = Text::new("DHCP — Direccion asignada automaticamente", Point::new(px + 80, info_y), value_style).draw(fb);

        let cfg = &input.net_static_config;
        let mut ip_buf = [0u8; 20];
        let ip_len = format_ipv4(&mut ip_buf, &cfg.ipv4);
        let _ = Text::new("IP actual:", Point::new(px + 16, info_y + 24), label_style).draw(fb);
        let _ = Text::new(core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("—"), Point::new(px + 110, info_y + 24), value_style).draw(fb);

        let mut gw_buf = [0u8; 20];
        let gw_len = format_ipv4(&mut gw_buf, &cfg.gateway_v4);
        let _ = Text::new("Gateway:", Point::new(px + 16, info_y + 48), label_style).draw(fb);
        let _ = Text::new(core::str::from_utf8(&gw_buf[..gw_len]).unwrap_or("—"), Point::new(px + 110, info_y + 48), value_style).draw(fb);

        let mut dns_buf = [0u8; 20];
        let dns_len = format_ipv4(&mut dns_buf, &cfg.dns_v4);
        let _ = Text::new("DNS:", Point::new(px + 16, info_y + 72), label_style).draw(fb);
        let _ = Text::new(core::str::from_utf8(&dns_buf[..dns_len]).unwrap_or("—"), Point::new(px + 110, info_y + 72), value_style).draw(fb);

        // Renew IP button
        let btn_x = px + pw / 2 - 80;
        let btn_y2 = py + 200;
        let renew_btn_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 80, 180))
            .stroke_color(Rgb888::new(0, 160, 255))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(btn_x, btn_y2), Size::new(160, 28))
            .into_styled(renew_btn_style)
            .draw(fb);
        let btn_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 220, 255));
        let _ = Text::new("Renovar IP (DHCP)", Point::new(btn_x + 14, btn_y2 + 18), btn_text_style).draw(fb);
    } else {
        // ── Static IP mode: editable form ──
        let cfg = &input.net_static_config;

        // Field labels and current values
        let fields: [(&str, [u8; 4], bool); 4] = [
            ("Direccion IP:", cfg.ipv4, true),
            ("Mascara (/prefix):", [0, 0, 0, cfg.ipv4_prefix], false), // prefix special-cased below
            ("Puerta de enlace:", cfg.gateway_v4, true),
            ("DNS primario:", cfg.dns_v4, true),
        ];

        for (i, (label, raw_val, is_ipv4)) in fields.iter().enumerate() {
            let fy = py + 80 + (i as i32 * 38);
            let fid = (i as u8) + 1;
            let is_editing = input.net_edit_field == fid;

            let _ = Text::new(label, Point::new(px + 16, fy), label_style).draw(fb);

            // Value box
            let box_x = px + pw / 2 - 20;
            let box_w: u32 = (pw / 2 + 10) as u32;
            let box_style = PrimitiveStyleBuilder::new()
                .stroke_color(if is_editing { Rgb888::new(0, 180, 255) } else { Rgb888::new(40, 55, 85) })
                .stroke_width(if is_editing { 2 } else { 1 })
                .build();
            let _ = Rectangle::new(Point::new(box_x, fy - 14), Size::new(box_w, 20))
                .into_styled(box_style)
                .draw(fb);

            // Value text (editing buffer or formatted value)
            let style = if is_editing { edit_style } else { value_style };
            if is_editing {
                let buf_str = input.net_edit_buffer.as_str();
                let _ = Text::new(buf_str, Point::new(box_x + 4, fy), style).draw(fb);
                // Blinking cursor placeholder
                let cursor_x = box_x + 4 + (buf_str.len() as i32) * 6;
                let cursor_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 200, 255));
                let _ = Text::new("|", Point::new(cursor_x, fy), cursor_style).draw(fb);
            } else if *is_ipv4 {
                let mut vbuf = [0u8; 20];
                let vlen = format_ipv4(&mut vbuf, raw_val);
                let _ = Text::new(core::str::from_utf8(&vbuf[..vlen]).unwrap_or("?"), Point::new(box_x + 4, fy), style).draw(fb);
            } else {
                // Prefix field (stored in raw_val[3])
                let mut pbuf = [0u8; 8];
                let pstr = format_u32(&mut pbuf, raw_val[3] as u32);
                let _ = Text::new(pstr, Point::new(box_x + 4, fy), style).draw(fb);
            }

            // Click hint
            if !is_editing {
                let hint = MonoTextStyle::new(&FONT_6X12, Rgb888::new(50, 65, 95));
                let _ = Text::new("[click]", Point::new(box_x + box_w as i32 - 48, fy), hint).draw(fb);
            }
        }

        // Apply button
        let apply_x = px + pw / 2 - 80;
        let apply_y = py + 242;
        let apply_btn_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 120, 60))
            .stroke_color(Rgb888::new(0, 220, 100))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(apply_x, apply_y), Size::new(160, 26))
            .into_styled(apply_btn_style)
            .draw(fb);
        let apply_text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 230, 110));
        let _ = Text::new("Aplicar Cambios", Point::new(apply_x + 18, apply_y + 17), apply_text_style).draw(fb);
    }

    // Close hint at bottom
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(70, 80, 110));
    let _ = Text::new("Esc para cerrar", Point::new(px + pw - 110, py + ph - 10), hint_style).draw(fb);
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



/// Compute the x offset of the first running-indicator dot for a pinned app icon.
/// Returns `(dot_start_x, n_dots)` where `n_dots` is clamped to 3.
/// `icon_left_x` is the left edge of the icon (e.g. `app_x`).
/// `icon_size` is the icon width (e.g. `TASKBAR_ICON_SIZE`).
fn running_dot_layout(run_count: usize, icon_left_x: i32, icon_size: i32) -> (i32, i32) {
    let n = run_count.min(3) as i32;
    if n == 0 { return (0, 0); }
    // n dots of RUN_DOT_W each, with RUN_DOT_GAP between them (no trailing gap)
    let total_w = n * RUN_DOT_STRIDE - RUN_DOT_GAP;
    let dot_start_x = icon_left_x + icon_size / 2 - total_w / 2;
    (dot_start_x, n)
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

fn format_u64<'a>(buf: &'a mut [u8; 16], val: u64) -> &'a str {
    if val == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut n = val;
    let mut pos: usize = 15;
    while n > 0 && pos > 0 {
        buf[pos] = (n % 10) as u8 + b'0';
        n /= 10;
        pos -= 1;
    }
    pos += 1;
    core::str::from_utf8(&buf[pos..16]).unwrap_or("?")
}

fn format_ipv4(buf: &mut [u8; 20], ip: &[u8; 4]) -> usize {
    let mut pos = 0;
    for (i, &octet) in ip.iter().enumerate() {
        if i > 0 {
            buf[pos] = b'.';
            pos += 1;
        }
        if octet >= 100 {
            buf[pos] = b'0' + octet / 100;
            pos += 1;
        }
        if octet >= 10 {
            buf[pos] = b'0' + (octet / 10) % 10;
            pos += 1;
        }
        buf[pos] = b'0' + octet % 10;
        pos += 1;
    }
    pos
}

/// Returns the day of week for a given date using Tomohiko Sakamoto's algorithm.
/// Returns 0 = Sunday, 1 = Monday, …, 5 = Friday, 6 = Saturday.
fn day_of_week(mut y: u32, m: u32, d: u32) -> u32 {
    const T: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 { y -= 1; }
    (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7
}

/// Returns the number of days in a given month.
fn days_in_month(m: u32, y: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 400 == 0 => 29,
        2 if y % 100 == 0 => 28,
        2 if y % 4 == 0 => 29,
        _ => 28,
    }
}

/// Month name abbreviations (Jan=1 … Dec=12).
fn month_name(m: u8) -> &'static str {
    match m {
        1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr",
        5 => "May", 6 => "Jun", 7 => "Jul", 8 => "Aug",
        9 => "Sep", 10 => "Oct", 11 => "Nov", 12 => "Dec",
        _ => "???",
    }
}

/// Draw the clock/calendar popup panel above the clock area.
pub fn draw_clock_panel(
    fb: &mut FramebufferState,
    desktop: &crate::desktop::DesktopShell,
    calendar_offset: i8,
) {
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;

    let pw = CLOCK_PANEL_W;
    let ph = CLOCK_PANEL_H;
    // Align right edge with the right edge of the clock; sit just above the taskbar.
    let px = (fb_w - 6 - pw).max(0);
    let py = fb_h - TASKBAR_HEIGHT - ph - 5;

    // Background
    let bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(18, 22, 40))
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(bg_style)
        .draw(fb);
    // Border
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb888::new(0, 80, 180))
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(Point::new(px, py), Size::new(pw as u32, ph as u32))
        .into_styled(border_style)
        .draw(fb);

    // Apply month offset to get display month/year
    let (disp_month, disp_year) = {
        let mut m = desktop.clock_month as i32;
        let mut y = desktop.clock_year as i32;
        m += calendar_offset as i32;
        while m < 1 { m += 12; y -= 1; }
        while m > 12 { m -= 12; y += 1; }
        (m as u32, y as u32)
    };

    // Navigation arrows
    let arrow_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 140, 200));
    let _ = Text::new("<", Point::new(px + 4, py + 14), arrow_style).draw(fb);
    let _ = Text::new(">", Point::new(px + pw - 14, py + 14), arrow_style).draw(fb);

    // Header: "MMM YYYY"
    let header_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let mon_name = month_name(disp_month as u8);
    // Build "MMM YYYY" header in a stack buffer
    let mut hbuf = [0u8; 12];
    let mname_bytes = mon_name.as_bytes();
    hbuf[..3].copy_from_slice(&mname_bytes[..3]);
    hbuf[3] = b' ';
    let y = disp_year;
    hbuf[4] = b'0' + ((y / 1000) % 10) as u8;
    hbuf[5] = b'0' + ((y / 100) % 10) as u8;
    hbuf[6] = b'0' + ((y / 10) % 10) as u8;
    hbuf[7] = b'0' + (y % 10) as u8;
    let header_str = core::str::from_utf8(&hbuf[..8]).unwrap_or("??? 0000");
    let _ = Text::new(header_str, Point::new(px + 34, py + 14), header_style).draw(fb);

    // Day-of-week labels row: "Mo Tu We Th Fr Sa Su"
    let dow_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    const DOW_LABELS: &[&str] = &["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
    for (i, label) in DOW_LABELS.iter().enumerate() {
        let _ = Text::new(label, Point::new(px + 4 + i as i32 * 23, py + 28), dow_style).draw(fb);
    }

    // Calendar grid
    let day_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 220));
    let today_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 220, 255));
    let today_bg_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(0, 60, 120))
        .build();

    let m = disp_month;
    let y = disp_year;
    let total_days = days_in_month(m, y);
    // day_of_week returns 0=Sun,1=Mon…6=Sat; we want 0=Mon … 6=Sun
    let first_dow_sun = day_of_week(y, m, 1); // 0=Sun
    let col_start = if first_dow_sun == 0 { 6 } else { first_dow_sun - 1 }; // convert to Mon-first

    let mut col = col_start;
    let mut row = 0i32;
    let mut dbuf = [0u8; 8];
    for day in 1..=total_days {
        let cell_x = px + 4 + col as i32 * 23;
        let cell_y = py + 42 + row * 14;

        // Only highlight today if viewing the current month
        if calendar_offset == 0 && day == desktop.clock_day as u32 {
            // Highlight today
            let _ = Rectangle::new(Point::new(cell_x - 1, cell_y - 10), Size::new(18, 13))
                .into_styled(today_bg_style)
                .draw(fb);
            let d_str = format_u32(&mut dbuf, day);
            let _ = Text::new(d_str, Point::new(cell_x, cell_y), today_style).draw(fb);
        } else {
            let d_str = format_u32(&mut dbuf, day);
            let _ = Text::new(d_str, Point::new(cell_x, cell_y), day_style).draw(fb);
        }

        col += 1;
        if col >= 7 {
            col = 0;
            row += 1;
        }
    }
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
        assert_eq!(s, "CPU:45%");
    }

    #[test]
    fn test_format_metric_100() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "CPU:", 100.0);
        assert_eq!(s, "CPU:100%");
    }

    #[test]
    fn test_format_metric_zero() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "MEM:", 0.0);
        assert_eq!(s, "MEM:00%");
    }

    #[test]
    fn test_format_metric_single_digit() {
        let mut buf = [0u8; 16];
        let s = format_metric(&mut buf, "NET:", 5.0);
        assert_eq!(s, "NET:05%");
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

    #[test]
    fn test_format_u64_basic() {
        let mut buf = [0u8; 16];
        let s = format_u64(&mut buf, 123456789);
        assert_eq!(s, "123456789");
    }

    #[test]
    fn test_format_u64_zero() {
        let mut buf = [0u8; 16];
        let s = format_u64(&mut buf, 0);
        assert_eq!(s, "0");
    }

    #[test]
    fn test_format_ipv4() {
        let mut buf = [0u8; 20];
        let len = format_ipv4(&mut buf, &[192, 168, 1, 100]);
        let ip = core::str::from_utf8(&buf[..len]).unwrap();
        assert_eq!(ip, "192.168.1.100");
    }

    #[test]
    fn test_format_ipv4_zeros() {
        let mut buf = [0u8; 20];
        let len = format_ipv4(&mut buf, &[0, 0, 0, 0]);
        let ip = core::str::from_utf8(&buf[..len]).unwrap();
        assert_eq!(ip, "0.0.0.0");
    }

    #[test]
    fn test_format_ipv4_loopback() {
        let mut buf = [0u8; 20];
        let len = format_ipv4(&mut buf, &[127, 0, 0, 1]);
        let ip = core::str::from_utf8(&buf[..len]).unwrap();
        assert_eq!(ip, "127.0.0.1");
    }

    // ── Tests for new visual-improvement helpers ──

    #[test]
    fn test_running_dot_layout_zero() {
        // No running instances → returns (0, 0).
        let (_, n) = running_dot_layout(0, 160, 32);
        assert_eq!(n, 0);
    }

    #[test]
    fn test_running_dot_layout_one() {
        // 1 instance → 1 dot, centred under the icon.
        let (start_x, n) = running_dot_layout(1, 160, 32);
        assert_eq!(n, 1);
        // icon centre = 160 + 16 = 176; total_w = 4; offset = 176 - 2 = 174
        assert_eq!(start_x, 174);
    }

    #[test]
    fn test_running_dot_layout_two() {
        // 2 instances → 2 dots.
        let (start_x, n) = running_dot_layout(2, 160, 32);
        assert_eq!(n, 2);
        // total_w = 2*RUN_DOT_STRIDE - RUN_DOT_GAP = 10; offset = 5; start = 176 - 5 = 171
        assert_eq!(start_x, 171);
    }

    #[test]
    fn test_running_dot_layout_three() {
        // 3 instances → 3 dots.
        let (start_x, n) = running_dot_layout(3, 160, 32);
        assert_eq!(n, 3);
        // total_w = 3*RUN_DOT_STRIDE - RUN_DOT_GAP = 16; offset = 8; start = 176 - 8 = 168
        assert_eq!(start_x, 168);
    }

    #[test]
    fn test_running_dot_layout_clamped_to_three() {
        // Even with 5 running instances, only 3 dots are shown.
        let (_, n) = running_dot_layout(5, 160, 32);
        assert_eq!(n, 3);
    }

    #[test]
    fn test_running_dot_spacing_consistent() {
        // Adjacent dots are RUN_DOT_STRIDE pixels apart.
        let (start_x_3, n) = running_dot_layout(3, 0, 32);
        assert_eq!(n, 3);
        let dot0 = start_x_3;
        let dot1 = start_x_3 + RUN_DOT_STRIDE;
        let dot2 = start_x_3 + RUN_DOT_STRIDE * 2;
        assert_eq!(dot1 - dot0, RUN_DOT_STRIDE);
        assert_eq!(dot2 - dot1, RUN_DOT_STRIDE);
    }

    // ── Tests for day-of-week abbreviation ──

    #[test]
    fn test_day_of_week_known_dates() {
        // 2026-03-27 is a Friday — Sakamoto returns 5 (0=Sun,1=Mon…5=Fri,6=Sat).
        assert_eq!(day_of_week(2026, 3, 27), 5);
    }

    #[test]
    fn test_day_of_week_monday() {
        // 2026-03-23 is a Monday → Sakamoto returns 1.
        assert_eq!(day_of_week(2026, 3, 23), 1);
    }

    #[test]
    fn test_day_of_week_sunday() {
        // 2026-03-29 is a Sunday → Sakamoto returns 0.
        assert_eq!(day_of_week(2026, 3, 29), 0);
    }

    #[test]
    fn test_day_of_week_abbr_array() {
        const DOW_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        // 2026-03-27 is Friday → index 5 → "Fri"
        let idx = day_of_week(2026, 3, 27) as usize;
        assert_eq!(DOW_ABBR[idx], "Fri");
        // 2026-03-23 is Monday → index 1 → "Mon"
        let idx2 = day_of_week(2026, 3, 23) as usize;
        assert_eq!(DOW_ABBR[idx2], "Mon");
    }

    // ── Tests for wallpaper mode pre-rendering ──

    #[test]
    fn test_pre_render_background_solid_color_no_panic() {
        // In test mode the function body is skipped, so this just verifies
        // the signature accepts all three WallpaperMode variants.
        let mut fb = FramebufferState::mock();
        fb.pre_render_background(WallpaperMode::SolidColor, (40, 80, 120));
        // No assertion needed — the call must not panic.
    }

    #[test]
    fn test_pre_render_background_gradient_no_panic() {
        let mut fb = FramebufferState::mock();
        fb.pre_render_background(WallpaperMode::Gradient, (100, 50, 200));
    }

    #[test]
    fn test_pre_render_background_cosmic_no_panic() {
        let mut fb = FramebufferState::mock();
        fb.pre_render_background(WallpaperMode::CosmicTheme, (10, 15, 30));
    }

    #[test]
    fn test_gradient_lerp_at_zero() {
        // At t=0 (top of screen) the output colour equals the base colour.
        let (r0, g0, b0): (u8, u8, u8) = (100, 80, 200);
        let lerp = |a: u8, b: u8, t: u32| -> u8 {
            (a as u32 + (b as u32).wrapping_sub(a as u32).wrapping_mul(t) / 255) as u8
        };
        let r1 = (r0 as u32).saturating_sub(r0 as u32 / 2) as u8;
        let g1 = (g0 as u32).saturating_sub(g0 as u32 / 2) as u8;
        let b1 = (b0 as u32).saturating_sub(b0 as u32 / 2) as u8;
        assert_eq!(lerp(r0, r1, 0), r0);
        assert_eq!(lerp(g0, g1, 0), g0);
        assert_eq!(lerp(b0, b1, 0), b0);
    }

    #[test]
    fn test_gradient_lerp_darkens_toward_bottom() {
        // The bottom colour should be no brighter than the top colour.
        let (r0, g0, b0): (u8, u8, u8) = (200, 180, 160);
        let r1 = (r0 as u32).saturating_sub(r0 as u32 / 2) as u8;
        let g1 = (g0 as u32).saturating_sub(g0 as u32 / 2) as u8;
        let b1 = (b0 as u32).saturating_sub(b0 as u32 / 2) as u8;
        assert!(r1 <= r0, "bottom red should be <= top red");
        assert!(g1 <= g0, "bottom green should be <= top green");
        assert!(b1 <= b0, "bottom blue should be <= top blue");
    }

    #[test]
    fn test_solid_color_pixel_value() {
        // Verify the pixel encoding formula for a known colour.
        let (r, g, b): (u8, u8, u8) = (255, 128, 0);
        let pixel = 0xFF00_0000u32 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        assert_eq!((pixel >> 16) & 0xFF, 255);
        assert_eq!((pixel >> 8) & 0xFF, 128);
        assert_eq!(pixel & 0xFF, 0);
    }
}