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
    net_extended_stats: Option<&eclipse_ipc::types::NetExtendedStats>,
) {
    // 1. Blit background
    fb.blit_background();

    // 2. Draw taskbar
    draw_taskbar(fb, input, desktop, windows, window_count, cpu_usage, mem_usage, net_usage);

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

    if input.network_details_active {
        draw_network_panel(fb, net_usage, net_extended_stats);
    }

    if input.lock_screen_active {
        draw_lock_screen(fb);
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
        draw_clock_panel(fb, desktop);
    }

    if input.context_menu.visible {
        draw_context_menu(fb, &input.context_menu);
    }

    // 5. Draw cursor
    draw_cursor(fb, input.cursor_x, input.cursor_y);

    // 6. Draw HUD overlay (kernel log)
    if log_len > 0 {
        draw_hud_overlay(fb, log_buf, log_len);
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
pub const TASKBAR_TRAY_WIDTH: i32 = 300;

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

/// Draw the bottom taskbar.
fn draw_taskbar(
    fb: &mut FramebufferState,
    input: &InputState,
    desktop: &DesktopShell,
    windows: &[ShellWindow],
    window_count: usize,
    cpu_usage: f32,
    mem_usage: f32,
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

    // Launcher icon (grid dots)
    let dot_color = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
    let _ = Text::new(":::", Point::new(10, bar_y + 26), dot_color).draw(fb);

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

        // App first-letter icon
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

    // CPU metric text
    let metrics_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 120, 160));
    let mut cpu_buf = [0u8; 16];
    let cpu_str = format_metric(&mut cpu_buf, "CPU:", cpu_usage);
    let _ = Text::new(cpu_str, Point::new(tray_x + 10, bar_y + 18), metrics_style).draw(fb);

    // CPU mini progress bar below the text — turns red above 80 %
    let cpu_bar_w = ((cpu_usage as i32).clamp(0, 100) * 46) / 100;
    if cpu_bar_w > 0 {
        let cpu_bar_color = if cpu_usage > 80.0 {
            Rgb888::new(220, 80, 50)
        } else {
            Rgb888::new(0, 130, 220)
        };
        let cpu_bar_style = PrimitiveStyleBuilder::new().fill_color(cpu_bar_color).build();
        let _ = Rectangle::new(Point::new(tray_x + 10, bar_y + 33), Size::new(cpu_bar_w as u32, 2))
            .into_styled(cpu_bar_style)
            .draw(fb);
    }

    // MEM metric text
    let mut mem_buf = [0u8; 16];
    let mem_str = format_metric(&mut mem_buf, "MEM:", mem_usage);
    let _ = Text::new(mem_str, Point::new(tray_x + 78, bar_y + 18), metrics_style).draw(fb);

    // MEM mini progress bar below the text — turns red above 80 %
    let mem_bar_w = ((mem_usage as i32).clamp(0, 100) * 46) / 100;
    if mem_bar_w > 0 {
        let mem_bar_color = if mem_usage > 80.0 {
            Rgb888::new(220, 80, 50)
        } else {
            Rgb888::new(80, 200, 120)
        };
        let mem_bar_style = PrimitiveStyleBuilder::new().fill_color(mem_bar_color).build();
        let _ = Rectangle::new(Point::new(tray_x + 78, bar_y + 33), Size::new(mem_bar_w as u32, 2))
            .into_styled(mem_bar_style)
            .draw(fb);
    }

    // Tiling mode indicator — small "T" badge between MEM and notifications
    if input.tiling_active {
        let tiling_bg_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 100, 50))
            .build();
        let _ = Rectangle::new(Point::new(tray_x + 138, bar_y + 8), Size::new(14, 14))
            .into_styled(tiling_bg_style)
            .draw(fb);
        let tiling_text = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 220, 120));
        let _ = Text::new("T", Point::new(tray_x + 140, bar_y + 19), tiling_text).draw(fb);
    }

    // Notification bell indicator
    let notif_count = desktop.unread_count();
    let notif_x = tray_x + 155;
    if notif_count > 0 {
        let bell_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(255, 200, 50));
        let _ = Text::new("!", Point::new(notif_x, bar_y + 18), bell_style).draw(fb);

        // Badge with count
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
    } else {
        // Outline box as a quiet bell placeholder when there are no unread notifications.
        let bell_outline = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(70, 85, 115))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(notif_x, bar_y + 11), Size::new(8, 8))
            .into_styled(bell_outline)
            .draw(fb);
    }

    // Volume indicator (mute-aware)
    let vol_x = tray_x + 180;
    let vol_color = if desktop.volume_muted {
        Rgb888::new(220, 50, 50)
    } else {
        Rgb888::new(120, 140, 180)
    };
    let vol_style = MonoTextStyle::new(&FONT_6X12, vol_color);
    let vol_icon = if desktop.volume_muted {
        "X"
    } else if desktop.volume_level > 50 {
        "+"
    } else if desktop.volume_level > 0 {
        "~"
    } else {
        "-"
    };
    let _ = Text::new(vol_icon, Point::new(vol_x, bar_y + 18), vol_style).draw(fb);

    // Volume level bar (below icon) — spans up to 20px (proportional)
    if !desktop.volume_muted && desktop.volume_level > 0 {
        let bar_w = (desktop.volume_level as i32 * 20) / 100;
        let vol_bar_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(0, 150, 255))
            .build();
        let _ = Rectangle::new(
            Point::new(vol_x - 2, bar_y + 22),
            Size::new(bar_w as u32, 2),
        )
        .into_styled(vol_bar_style)
        .draw(fb);
    }

    // Volume percentage text below the bar (e.g. "75" or "M" when muted)
    {
        let vol_pct_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(70, 85, 115));
        if desktop.volume_muted {
            let _ = Text::new("M", Point::new(vol_x - 2, bar_y + 34), vol_pct_style).draw(fb);
        } else {
            let mut vpbuf = [0u8; 4];
            let vp = desktop.volume_level.min(100) as u32;
            let mut vpos = 0usize;
            if vp >= 100 {
                vpbuf[vpos] = b'1'; vpos += 1;
                vpbuf[vpos] = b'0'; vpos += 1;
                vpbuf[vpos] = b'0'; vpos += 1;
            } else {
                vpbuf[vpos] = b'0' + (vp / 10) as u8; vpos += 1;
                vpbuf[vpos] = b'0' + (vp % 10) as u8; vpos += 1;
            }
            let vstr = core::str::from_utf8(&vpbuf[..vpos]).unwrap_or("?");
            let _ = Text::new(vstr, Point::new(vol_x - 2, bar_y + 34), vol_pct_style).draw(fb);
        }
    }

    // Network activity indicator — small coloured dot at tray_x+205.
    // Green when active (net_usage > 0.5%), dim grey when idle.
    {
        let net_x = tray_x + 205;
        let net_connected = net_usage > 0.5;
        let net_dot_color = if net_connected {
            Rgb888::new(0, 210, 130)
        } else {
            Rgb888::new(50, 60, 85)
        };
        let net_dot_style = PrimitiveStyleBuilder::new().fill_color(net_dot_color).build();
        // Outer dot (6×6)
        let _ = Rectangle::new(Point::new(net_x, bar_y + 19), Size::new(6, 6))
            .into_styled(net_dot_style)
            .draw(fb);
        // Label below
        let net_label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(60, 75, 110));
        let net_label = if net_connected { "N" } else { "n" };
        let _ = Text::new(net_label, Point::new(net_x, bar_y + 34), net_label_style).draw(fb);
    }

    // Battery indicator — shown when show_battery is enabled.
    // Position: tray_x + 212 (= fb_w - 88 with tray_width 300).
    if desktop.show_battery {
        let bat_x = tray_x + 212;
        let bat_level = desktop.battery_level.min(100) as i32;
        let bat_charging = desktop.battery_charging;

        // Battery outline (16px × 10px rectangle + 2px bump on right)
        let bat_outline_style = PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::new(100, 120, 160))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(bat_x, bar_y + 17), Size::new(16, 10))
            .into_styled(bat_outline_style)
            .draw(fb);
        // Battery terminal bump
        let bump_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(100, 120, 160))
            .build();
        let _ = Rectangle::new(Point::new(bat_x + 16, bar_y + 19), Size::new(2, 6))
            .into_styled(bump_style)
            .draw(fb);

        // Battery fill (green / yellow / red)
        let fill_w = ((bat_level * 14) / 100).max(0) as u32;
        let fill_color = if bat_charging {
            Rgb888::new(0, 200, 100)
        } else if bat_level > 50 {
            Rgb888::new(80, 200, 80)
        } else if bat_level > 20 {
            Rgb888::new(220, 180, 0)
        } else {
            Rgb888::new(220, 50, 50)
        };
        if fill_w > 0 {
            let fill_style = PrimitiveStyleBuilder::new().fill_color(fill_color).build();
            let _ = Rectangle::new(Point::new(bat_x + 1, bar_y + 18), Size::new(fill_w, 8))
                .into_styled(fill_style)
                .draw(fb);
        }

        // Charging indicator: "+" icon
        if bat_charging {
            let charge_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 240, 120));
            let _ = Text::new("+", Point::new(bat_x + 20, bar_y + 26), charge_style).draw(fb);
        }

        // Battery percentage text below the battery icon
        let bat_pct_color = if bat_level <= 20 {
            Rgb888::new(220, 80, 50)
        } else {
            Rgb888::new(90, 110, 150)
        };
        let bat_pct_style = MonoTextStyle::new(&FONT_6X12, bat_pct_color);
        let mut bat_pct_buf = [0u8; 6];
        let bat_pct_str = format_battery_pct(&mut bat_pct_buf, bat_level as u32);
        let _ = Text::new(bat_pct_str, Point::new(bat_x, bar_y + 33), bat_pct_style).draw(fb);
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
        let dow_idx = day_of_week(year, mo as u32, d as u32) as usize;
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
                // Use max(0) to avoid wrapping negative i32 into a huge u32.
                let src_w = window.w.max(0) as u32;
                let intended_h = ((window.h - ShellWindow::TITLE_H).max(0) as u32).min(content_h as u32);

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

/// Context menu item height.
pub const CONTEXT_MENU_ITEM_H: i32 = 28;
/// Context menu width.
pub const CONTEXT_MENU_W: i32 = 180;

/// Width of the clock/calendar panel.
pub const CLOCK_PANEL_W: i32 = 168;
/// Height of the clock/calendar panel.
pub const CLOCK_PANEL_H: i32 = 128;

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
        let search_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 130, 180));
        let _ = Text::new("Filter: ", Point::new(px + 10, py + 42), search_style).draw(fb);
        let query_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 220, 255));
        let _ = Text::new(input.search_query.as_str(), Point::new(px + 58, py + 42), query_style).draw(fb);
    }

    // Render pinned apps from desktop (filtered by search query if active)
    let app_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 230));
    let hovered_launcher = input.launcher_hovered_index;

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

        // Hover highlight
        let is_hovered = hovered_launcher == Some(i);
        if is_hovered {
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

        // App icon (colored square with first letter)
        let icon_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(r, g, b)).build();
        let _ = Rectangle::new(Point::new(px + 12, app_y - 8), Size::new(24, 24))
            .into_styled(icon_style)
            .draw(fb);
        let first_char = app_name.chars().next().unwrap_or('?');
        let mut char_buf = [0u8; 4];
        let char_str = first_char.encode_utf8(&mut char_buf);
        let icon_letter_style = MonoTextStyle::new(&FONT_6X12, Rgb888::WHITE);
        let _ = Text::new(char_str, Point::new(px + 18, app_y + 4), icon_letter_style).draw(fb);

        // App name
        let _ = Text::new(app_name, Point::new(px + 44, app_y + 4), app_style).draw(fb);

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

/// Draw the context menu overlay.
fn draw_context_menu(fb: &mut FramebufferState, menu: &crate::input::ContextMenu) {
    let menu_w = CONTEXT_MENU_W;
    let item_h = CONTEXT_MENU_ITEM_H;
    let menu_h = (menu.item_count as i32) * item_h;
    let mx = menu.x;
    let my = menu.y;

    // Background
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(22, 26, 48)).build();
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

    // Items
    for i in 0..menu.item_count {
        let iy = my + (i as i32) * item_h;

        // Hover highlight
        if menu.hovered_index == Some(i) {
            let hover_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(35, 50, 80))
                .build();
            let _ = Rectangle::new(Point::new(mx + 1, iy), Size::new((menu_w - 2) as u32, item_h as u32))
                .into_styled(hover_style)
                .draw(fb);
            // Left accent bar on hovered item
            let accent_style = PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::new(0, 140, 255))
                .build();
            let _ = Rectangle::new(Point::new(mx + 1, iy), Size::new(2, item_h as u32))
                .into_styled(accent_style)
                .draw(fb);
        }

        let text_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(200, 210, 230));
        let label = menu.items[i].label_str();
        let _ = Text::new(label, Point::new(mx + 12, iy + 19), text_style).draw(fb);
    }
}

/// Draw the volume popup panel.
fn draw_volume_popup(fb: &mut FramebufferState, desktop: &DesktopShell) {
    let panel_w = VOLUME_PANEL_W;
    let panel_h = VOLUME_PANEL_H;
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let tray_start = fb_w - TASKBAR_TRAY_WIDTH;
    let px = tray_start + 160;
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
    let panel_w: i32 = 420;
    let panel_h: i32 = 280;
    let px = (fb_w - panel_w) / 2;
    let py = (fb_h - panel_h) / 2;

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
    let _ = Text::new("NETWORK DETAILS", Point::new(px + 140, py + 24), title_style).draw(fb);

    // Network gauge
    draw_gauge(fb, px + 20, py + 45, 380, 24, net_usage, "NET", Rgb888::new(200, 100, 255));

    let info_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(180, 190, 210));
    let label_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(100, 130, 180));

    if let Some(stats) = net_extended_stats {
        // Loopback interface status
        let lo_status = if stats.lo_up != 0 { "UP" } else { "DOWN" };
        let lo_color = if stats.lo_up != 0 { Rgb888::new(0, 200, 100) } else { Rgb888::new(220, 50, 50) };
        let lo_style = MonoTextStyle::new(&FONT_6X12, lo_color);
        let _ = Text::new("lo:", Point::new(px + 20, py + 90), label_style).draw(fb);
        let _ = Text::new(lo_status, Point::new(px + 60, py + 90), lo_style).draw(fb);

        // lo IPv4
        let mut ip_buf = [0u8; 20];
        let ip_len = format_ipv4(&mut ip_buf, &stats.lo_ipv4);
        let _ = Text::new(core::str::from_utf8(&ip_buf[..ip_len]).unwrap_or("?"), Point::new(px + 100, py + 90), info_style).draw(fb);

        // eth0 interface status
        let eth_status = if stats.eth0_up != 0 { "UP" } else { "DOWN" };
        let eth_color = if stats.eth0_up != 0 { Rgb888::new(0, 200, 100) } else { Rgb888::new(220, 50, 50) };
        let eth_style = MonoTextStyle::new(&FONT_6X12, eth_color);
        let _ = Text::new("eth0:", Point::new(px + 20, py + 115), label_style).draw(fb);
        let _ = Text::new(eth_status, Point::new(px + 80, py + 115), eth_style).draw(fb);

        // eth0 IPv4
        let mut ip_buf2 = [0u8; 20];
        let ip_len2 = format_ipv4(&mut ip_buf2, &stats.eth0_ipv4);
        let _ = Text::new(core::str::from_utf8(&ip_buf2[..ip_len2]).unwrap_or("?"), Point::new(px + 120, py + 115), info_style).draw(fb);

        // Gateway
        let _ = Text::new("Gateway:", Point::new(px + 20, py + 140), label_style).draw(fb);
        let mut gw_buf = [0u8; 20];
        let gw_len = format_ipv4(&mut gw_buf, &stats.eth0_gateway);
        let _ = Text::new(core::str::from_utf8(&gw_buf[..gw_len]).unwrap_or("?"), Point::new(px + 120, py + 140), info_style).draw(fb);

        // DNS
        let _ = Text::new("DNS:", Point::new(px + 20, py + 160), label_style).draw(fb);
        let mut dns_buf = [0u8; 20];
        let dns_len = format_ipv4(&mut dns_buf, &stats.eth0_dns);
        let _ = Text::new(core::str::from_utf8(&dns_buf[..dns_len]).unwrap_or("?"), Point::new(px + 120, py + 160), info_style).draw(fb);

        // RX bytes
        let _ = Text::new("RX bytes:", Point::new(px + 20, py + 190), label_style).draw(fb);
        let mut buf3 = [0u8; 16];
        let rx_bytes_str = format_u64(&mut buf3, stats.rx_bytes);
        let _ = Text::new(rx_bytes_str, Point::new(px + 160, py + 190), info_style).draw(fb);

        // TX bytes
        let _ = Text::new("TX bytes:", Point::new(px + 20, py + 210), label_style).draw(fb);
        let mut buf4 = [0u8; 16];
        let tx_bytes_str = format_u64(&mut buf4, stats.tx_bytes);
        let _ = Text::new(tx_bytes_str, Point::new(px + 160, py + 210), info_style).draw(fb);
    } else {
        let _ = Text::new("No extended stats", Point::new(px + 120, py + 130), label_style).draw(fb);
        let _ = Text::new("available", Point::new(px + 150, py + 150), label_style).draw(fb);
    }

    // Close hint
    let hint_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(80, 90, 110));
    let _ = Text::new("Super+E to close", Point::new(px + 140, py + 260), hint_style).draw(fb);
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

/// Format battery level as "XX%" or "100%" into a caller-provided buffer.
/// Returns a `&str` slice into the buffer.
fn format_battery_pct<'a>(buf: &'a mut [u8; 6], level: u32) -> &'a str {
    let pct = level.min(100);
    let mut pos = 0usize;
    if pct >= 100 {
        buf[pos] = b'1'; pos += 1;
        buf[pos] = b'0'; pos += 1;
        buf[pos] = b'0'; pos += 1;
    } else {
        buf[pos] = b'0' + (pct / 10) as u8; pos += 1;
        buf[pos] = b'0' + (pct % 10) as u8; pos += 1;
    }
    buf[pos] = b'%'; pos += 1;
    core::str::from_utf8(&buf[..pos]).unwrap_or("?%")
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

    // Header: "Mon Mar 2026"
    let header_style = MonoTextStyle::new(&FONT_6X12, Rgb888::new(0, 180, 255));
    let mon_name = month_name(desktop.clock_month);
    // Build "MMM YYYY" header in a stack buffer
    let mut hbuf = [0u8; 12];
    let mname_bytes = mon_name.as_bytes();
    hbuf[..3].copy_from_slice(&mname_bytes[..3]);
    hbuf[3] = b' ';
    let y = desktop.clock_year as u32;
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

    let m = desktop.clock_month as u32;
    let y = desktop.clock_year as u32;
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

        if day == desktop.clock_day as u32 {
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
    fn test_format_battery_pct_full() {
        let mut buf = [0u8; 6];
        assert_eq!(format_battery_pct(&mut buf, 100), "100%");
    }

    #[test]
    fn test_format_battery_pct_zero() {
        let mut buf = [0u8; 6];
        assert_eq!(format_battery_pct(&mut buf, 0), "00%");
    }

    #[test]
    fn test_format_battery_pct_single_digit() {
        let mut buf = [0u8; 6];
        assert_eq!(format_battery_pct(&mut buf, 5), "05%");
    }

    #[test]
    fn test_format_battery_pct_two_digits() {
        let mut buf = [0u8; 6];
        assert_eq!(format_battery_pct(&mut buf, 83), "83%");
    }

    #[test]
    fn test_format_battery_pct_clamps_over_100() {
        let mut buf = [0u8; 6];
        // Values > 100 should be clamped to 100.
        assert_eq!(format_battery_pct(&mut buf, 120), "100%");
    }

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
}