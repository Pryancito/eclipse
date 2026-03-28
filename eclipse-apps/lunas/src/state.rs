//! Central state of the Lunas desktop environment.
//! Orchestrates Backend, Space, Input, IPC, Desktop Shell, and Rendering.

use crate::backend::Backend;
use crate::compositor::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, ShellWindow, MAX_EXTERNAL_SURFACES};
use crate::ipc::handle_sidewind_message;
use crate::render;
use crate::desktop::DesktopShell;
use crate::wayland::{WaylandCompositor, XwaylandIntegration, WaylandAction, XwaylandAction, make_wayland_window};
use std::prelude::v1::*;
use core::matches;
#[cfg(target_vendor = "eclipse")]
use libc::{eclipse_send, ProcessInfo, SystemStats, get_system_stats, get_process_list};
#[cfg(not(target_vendor = "eclipse"))]
use eclipse_syscall::{ProcessInfo, SystemStats};
use sidewind::{SideWindEvent, SWND_EVENT_TYPE_RESIZE};
use eclipse_ipc::types::NetExtendedStats;

#[cfg(not(target_vendor = "eclipse"))]
unsafe fn eclipse_send(_dest: u32, _msg_type: u32, _buf: *const core::ffi::c_void, _len: usize, _flags: usize) -> usize { 0 }
#[cfg(not(target_vendor = "eclipse"))]
fn get_system_stats(_stats: &mut SystemStats) -> i32 { 0 }
#[cfg(not(target_vendor = "eclipse"))]
fn get_process_list(_buf: *mut ProcessInfo, _max: usize) -> isize { 0 }

/// Service information for the system central panel.
#[derive(Clone, Copy, Default)]
pub struct ServiceInfo {
    pub name: [u8; 16],
    pub state: u32,
    pub pid: u32,
    pub restart_count: u32,
}

impl ServiceInfo {
    pub const fn new() -> Self {
        Self { name: [0; 16], state: 0, pid: 0, restart_count: 0 }
    }
}

/// LunasState is the central state of the desktop environment.
pub struct LunasState {
    pub backend: Backend,
    pub space: Space,
    pub input: InputState,
    pub surfaces: [ExternalSurface; MAX_EXTERNAL_SURFACES],
    pub desktop: DesktopShell,
    pub counter: u64,
    pub input_event_count: u64,
    pub prev_stats: Option<SystemStats>,
    pub last_metrics_update: std::time::Instant,
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub cpu_count: u64,
    pub mem_total_kb: u64,
    pub cpu_temp: u32,
    pub gpu_load: u32,
    pub gpu_temp: u32,
    pub gpu_vram_total_kb: u64,
    pub anomaly_count: u32,
    pub heap_fragmentation: u32,
    pub network_pid: Option<u32>,
    pub net_rx: u64,
    pub net_tx: u64,
    pub prev_net_rx: u64,
    pub prev_net_tx: u64,
    pub net_usage: f32,
    pub net_extended_stats: Option<NetExtendedStats>,
    pub process_list: [ProcessInfo; 32],
    pub process_count: usize,
    pub service_list: [ServiceInfo; 32],
    pub service_count: usize,
    pub dirty: bool,
    pub log_buf: [u8; 4096],
    pub log_len: usize,
    pub last_input_tick: u64,
    /// Wayland compositor: manages protocol state for connected Wayland clients.
    pub wayland: WaylandCompositor,
    /// XWayland integration: manages XWayland process and X11 window state.
    pub xwayland: XwaylandIntegration,
}

impl LunasState {
    pub fn new() -> Option<Self> {
        let backend = Backend::new()?;
        let fb_w = backend.fb.info.width as i32;
        let fb_h = backend.fb.info.height as i32;

        let mut state = Self {
            backend,
            space: Space::new(),
            input: InputState::new(fb_w, fb_h),
            surfaces: core::array::from_fn(|_| ExternalSurface::default()),
            desktop: DesktopShell::new(),
            counter: 0,
            input_event_count: 0,
            prev_stats: None,
            last_metrics_update: std::time::Instant::now(),
            cpu_usage: 0.0,
            mem_usage: 0.0,
            cpu_count: 0,
            mem_total_kb: 0,
            cpu_temp: 0,
            gpu_load: 0,
            gpu_temp: 0,
            gpu_vram_total_kb: 0,
            anomaly_count: 0,
            heap_fragmentation: 0,
            network_pid: None,
            net_rx: 0,
            net_tx: 0,
            prev_net_rx: 0,
            prev_net_tx: 0,
            net_usage: 0.0,
            net_extended_stats: None,
            process_list: [ProcessInfo::default(); 32],
            process_count: 0,
            service_list: [ServiceInfo::new(); 32],
            service_count: 0,
            dirty: true,
            log_buf: [0; 4096],
            log_len: 0,
            last_input_tick: 0,
            wayland: WaylandCompositor::new(),
            xwayland: XwaylandIntegration::new(),
        };

        // Pre-render background using the current wallpaper mode and colour.
        state.backend.fb.pre_render_background(
            state.desktop.wallpaper_mode,
            state.desktop.wallpaper_color,
        );

        // Sync pinned app count and names to input state for taskbar hit detection
        state.input.pinned_app_count = state.desktop.pinned_count;
        for i in 0..state.desktop.pinned_count.min(16) {
            let name = state.desktop.pinned_apps[i].name_str();
            let name_bytes = name.as_bytes();
            let len = name_bytes.len().min(32);
            state.input.pinned_app_names[i] = [0u8; 32];
            state.input.pinned_app_names[i][..len].copy_from_slice(&name_bytes[..len]);
        }

        // Welcome notification
        state.desktop.push_notification("Lunas Desktop initialized", 1);

        Some(state)
    }

    /// Handle a single compositor event.
    pub fn handle_event(&mut self, event: &CompositorEvent) {
        match event {
            CompositorEvent::Input(ev) => {
                self.input_event_count += 1;
                self.last_input_tick = self.counter;
                let dirty = self.input.apply_event(
                    ev,
                    &mut self.space.windows,
                    &mut self.space.window_count,
                    &mut self.surfaces,
                );
                if dirty { self.dirty = true; }

                // Apply tiling layout if active
                if self.input.tiling_active {
                    let fb_w = self.backend.fb.info.width as i32;
                    let fb_h = self.backend.fb.info.height as i32;
                    self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
                }
            }
            CompositorEvent::SideWind(msg, pid) => {
                let fb_w = self.backend.fb.info.width as i32;
                let fb_h = self.backend.fb.info.height as i32;
                handle_sidewind_message(
                    msg, *pid,
                    &mut self.surfaces,
                    &mut self.space.windows,
                    &mut self.space.window_count,
                    &mut self.input,
                    fb_w, fb_h,
                );
                self.dirty = true;
            }
            CompositorEvent::NetStats(rx, tx) => {
                self.prev_net_rx = self.net_rx;
                self.prev_net_tx = self.net_tx;
                self.net_rx = *rx;
                self.net_tx = *tx;
                let delta_rx = self.net_rx.saturating_sub(self.prev_net_rx);
                let delta_tx = self.net_tx.saturating_sub(self.prev_net_tx);
                let total_delta = delta_rx + delta_tx;
                // Normalize to percentage (assuming 1 Gbps max)
                self.net_usage = ((total_delta as f64 / 125_000_000.0) * 100.0) as f32;
                self.dirty = true;
            }
            CompositorEvent::NetExtendedStats(stats) => {
                self.net_extended_stats = Some(*stats);
                self.dirty = true;
            }
            CompositorEvent::ServiceInfo(data) => {
                if data.len() >= 28 && self.service_count < 32 {
                    let svc = &mut self.service_list[self.service_count];
                    svc.name[..16].copy_from_slice(&data[0..16]);
                    svc.state = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                    svc.pid = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                    svc.restart_count = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
                    self.service_count += 1;
                    self.dirty = true;
                }
            }
            CompositorEvent::KernelLog(line) => {
                let total_needed = line.len() + 1; // line + newline
                if total_needed > self.log_buf.len() {
                    // Single line is larger than entire buffer (unlikely)
                    return;
                }

                while self.log_len + total_needed > self.log_buf.len() {
                    // Shift buffer left by removing the oldest message (up to the first newline)
                    if let Some(first_nl) = self.log_buf[..self.log_len].iter().position(|&b| b == b'\n') {
                        let shift = first_nl + 1;
                        self.log_buf.copy_within(shift..self.log_len, 0);
                        self.log_len -= shift;
                    } else {
                        // Current buffer has no newline? Just clear it to make room
                        self.log_len = 0;
                        break;
                    }
                }

                // Append newest log line
                let start = self.log_len;
                self.log_buf[start..start + line.len()].copy_from_slice(line);
                self.log_len += line.len();
                
                // Add the newline
                self.log_buf[self.log_len] = b'\n';
                self.log_len += 1;
                
                self.dirty = true;
            }
            CompositorEvent::Wayland(data, pid) => {
                let pid = *pid;
                let fb_w = self.backend.fb.info.width as i32;
                let fb_h = self.backend.fb.info.height as i32;
                let action = self.wayland.handle_message(data, pid);
                match action {
                    WaylandAction::CreateSurface { pid, surface_id, conn_idx } => {
                        if self.space.window_count < self.space.windows.len() {
                            let win = make_wayland_window(
                                surface_id, conn_idx,
                                fb_w, fb_h,
                                self.input.current_workspace,
                                b"Wayland",
                            );
                            let w_idx = self.space.window_count;
                            self.space.map_window(win);
                            self.wayland.register_surface_window(pid, surface_id, w_idx);
                        }
                        self.dirty = true;
                    }
                    WaylandAction::CommitSurface { pid, surface_id } => {
                        // Mark the window as ready to show (no external buffer in Wayland
                        // windows — they render themselves; just ensure it's not minimized).
                        if let Some(conn) = self.wayland.connections.iter().find(|c| c.pid == pid) {
                            if let Some(w_idx) = conn.window_for_surface(surface_id) {
                                if w_idx < self.space.window_count {
                                    self.space.windows[w_idx].minimized = false;
                                }
                            }
                        }
                        self.dirty = true;
                    }
                    WaylandAction::DestroySurface { pid, surface_id } => {
                        let w_idx_opt = self.wayland.connections.iter()
                            .find(|c| c.pid == pid)
                            .and_then(|c| c.window_for_surface(surface_id));
                        if let Some(w_idx) = w_idx_opt {
                            if w_idx < self.space.window_count {
                                self.space.windows[w_idx].closing = true;
                                if self.input.focused_window == Some(w_idx) {
                                    self.input.focused_window = None;
                                }
                            }
                        }
                        if let Some(c) = self.wayland.connections.iter_mut().find(|c| c.pid == pid) {
                            c.remove_surface(surface_id);
                        }
                        self.dirty = true;
                    }
                    WaylandAction::None => {
                        self.dirty = true;
                    }
                }
            }
            CompositorEvent::X11(data, pid) => {
                let pid = *pid;
                let fb_w = self.backend.fb.info.width as i32;
                let fb_h = self.backend.fb.info.height as i32;
                let action = self.xwayland.handle_x11_event(data, pid);
                match action {
                    XwaylandAction::MapWindow { window_id } => {
                        // Create a ShellWindow for this X11 window if there's room.
                        if self.space.window_count < self.space.windows.len() {
                            // Use window_id as a unique surface_id; conn_idx = 0 (XWayland slot)
                            use crate::compositor::WindowContent;
                            let mut title_buf = [0u8; 32];
                            title_buf[..6].copy_from_slice(b"X11App");
                            let win = ShellWindow {
                                x: 80, y: ShellWindow::TITLE_H + 30,
                                w: (fb_w / 2).max(320),
                                h: (fb_h / 2).max(240),
                                curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
                                content: WindowContent::Wayland {
                                    surface_id: window_id,
                                    conn_idx: 0,
                                },
                                workspace: self.input.current_workspace,
                                title: title_buf,
                                ..Default::default()
                            };
                            self.space.map_window(win);
                        }
                        self.dirty = true;
                    }
                    XwaylandAction::UnmapWindow { window_id } => {
                        use crate::compositor::WindowContent;
                        let count = self.space.window_count;
                        if let Some(w_idx) = self.space.windows[..count].iter().position(|w| {
                            matches!(w.content, WindowContent::Wayland { surface_id, .. } if surface_id == window_id)
                        }) {
                            self.space.windows[w_idx].minimized = true;
                            if self.input.focused_window == Some(w_idx) {
                                self.input.focused_window = None;
                            }
                        }
                        self.dirty = true;
                    }
                    XwaylandAction::DestroyWindow { window_id } => {
                        use crate::compositor::WindowContent;
                        let count = self.space.window_count;
                        if let Some(w_idx) = self.space.windows[..count].iter().position(|w| {
                            matches!(w.content, WindowContent::Wayland { surface_id, .. } if surface_id == window_id)
                        }) {
                            self.space.windows[w_idx].closing = true;
                            if self.input.focused_window == Some(w_idx) {
                                self.input.focused_window = None;
                            }
                        }
                        self.dirty = true;
                    }
                    XwaylandAction::None => {
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Drain IPC messages and process all pending events.
    pub fn handle_ipc(&mut self) {
        #[cfg(not(test))]
        self.backend.drain_ipc_into_pending(128);

        // Process a fixed amount of events per frame to maintain 60 FPS
        let mut events_processed = 0usize;
        const EVENTS_PER_FRAME: usize = 64;
        while events_processed < EVENTS_PER_FRAME {
            if let Some(event) = self.backend.poll_event() {
                self.handle_event(&event);
                events_processed += 1;
            } else {
                break;
            }
        }
    }

    /// Update animations, metrics, and layout. Returns true if rendering is needed.
    pub fn update(&mut self) -> bool {
        self.counter += 1;

        // Update window animations
        let animating = self.space.update_animations(&mut self.surfaces);
        if animating != 0 {
            self.dirty = true;
        }

        // Update launcher hover (needs access to desktop.pinned_apps)
        if self.input.launcher_active {
            use crate::input::launcher_hit_test;
            let new_hover = launcher_hit_test(
                self.input.cursor_x,
                self.input.cursor_y,
                self.input.fb_height,
                self.desktop.pinned_count,
                &self.desktop.pinned_apps,
                self.input.search_active,
                self.input.search_query.as_str(),
            );
            if new_hover != self.input.launcher_hovered_index {
                self.input.launcher_hovered_index = new_hover;
                self.dirty = true;
            }
        } else if self.input.launcher_hovered_index.is_some() {
            self.input.launcher_hovered_index = None;
        }

        // Process taskbar actions from input
        self.process_taskbar_actions();

        // Update system metrics periodically
        self.update_metrics_if_needed();

        let needs_render = self.dirty;
        self.dirty = false;
        needs_render
    }

    /// Process pending taskbar actions (pinned app launch, volume toggle, clock, launcher).
    fn process_taskbar_actions(&mut self) {
        // Handle pinned-app drag reorder (higher priority than click)
        if let Some((src, dst)) = self.input.pending_pinned_swap.take() {
            self.desktop.swap_pinned_apps(src, dst);
            self.sync_pinned_apps_to_input();
            self.dirty = true;
        }

        // Handle pinned app click — focus running window or launch the app
        if let Some(app_idx) = self.input.last_pinned_app_click.take() {
            // Copy app name to a local buffer to avoid borrow conflicts
            let mut name_buf = [0u8; 32];
            let name_len = if app_idx < self.desktop.pinned_count {
                let name = self.desktop.pinned_apps[app_idx].name_str();
                let len = name.len().min(32);
                name_buf[..len].copy_from_slice(&name.as_bytes()[..len]);
                len
            } else {
                0
            };
            let app_name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("");

            // Search for a running window on the current workspace matching this pinned app
            let mut running_window: Option<usize> = None;
            if !app_name.is_empty() {
                for w_idx in 0..self.space.window_count {
                    let w = &self.space.windows[w_idx];
                    if w.content == crate::compositor::WindowContent::None || w.closing { continue; }
                    if w.workspace != self.input.current_workspace { continue; }
                    let w_title = w.title_str();
                    if w_title.len() >= app_name.len()
                        && w_title[..app_name.len()].eq_ignore_ascii_case(app_name)
                    {
                        running_window = Some(w_idx);
                        break;
                    }
                }
            }

            if let Some(w_idx) = running_window {
                // Focus and restore the running window instead of launching again
                self.space.windows[w_idx].minimized = false;
                self.input.focused_window = Some(w_idx);
                self.dirty = true;
            } else {
                self.launch_pinned_app(app_idx);
            }
        }

        // Handle launcher click — do hit test with desktop.pinned_apps
        if let Some((cx, cy)) = self.input.launcher_click_pos.take() {
            use crate::input::launcher_hit_test;
            let hit = launcher_hit_test(
                cx, cy,
                self.input.fb_height,
                self.desktop.pinned_count,
                &self.desktop.pinned_apps,
                self.input.search_active,
                self.input.search_query.as_str(),
            );
            if let Some(app_idx) = hit {
                self.launch_pinned_app(app_idx);
                self.input.launcher_active = false;
                self.input.search_active = false;
                self.input.search_query.clear();
                self.dirty = true;
            } else {
                // Clicked outside launcher items — check if outside panel to close
                use crate::render::{launcher_panel_bounds, TASKBAR_HEIGHT};
                let (panel_x, panel_y, panel_w, panel_h) =
                    launcher_panel_bounds(self.input.fb_height);
                if cx < panel_x || cx >= panel_x + panel_w
                    || cy < panel_y || cy >= panel_y + panel_h
                {
                    // Clicked outside launcher → close it
                    self.input.launcher_active = false;
                    self.input.search_active = false;
                    self.input.search_query.clear();
                    self.dirty = true;
                }
            }
        }

        // Handle launcher app click from direct index (alternative path)
        if let Some(app_idx) = self.input.launcher_app_click.take() {
            self.launch_pinned_app(app_idx);
            self.input.launcher_active = false;
            self.dirty = true;
        }

        // Handle volume click — toggle mute
        if self.input.volume_clicked {
            self.input.volume_clicked = false;
            self.desktop.volume_muted = !self.desktop.volume_muted;
            self.dirty = true;
        }

        // Handle clock click — toggle calendar panel (not dashboard)
        if self.input.clock_clicked {
            self.input.clock_clicked = false;
            self.input.clock_panel_active = !self.input.clock_panel_active;
            self.dirty = true;
        }

        // Handle "Show Desktop" toggle — minimize or restore all workspace windows
        let prev_show_desktop = self.input.show_desktop_active;
        // (show_desktop_active was already toggled in apply_event on button press)
        // We need to detect a change and act on it.
        // Use a flag: if show_desktop_active just became true, minimize; if false, restore.
        // We detect the transition by checking if the mask is zero (first time) vs non-zero.
        if self.input.show_desktop_active && self.input.show_desktop_minimized_mask == 0 {
            // Minimize all non-minimized windows on the current workspace
            let ws = self.input.current_workspace;
            let mut mask = 0u32;
            for i in 0..self.space.window_count.min(32) {
                let w = &mut self.space.windows[i];
                if w.content == crate::compositor::WindowContent::None || w.closing { continue; }
                if w.workspace != ws { continue; }
                if !w.minimized {
                    w.minimized = true;
                    mask |= 1u32 << i;
                }
            }
            self.input.show_desktop_minimized_mask = mask;
            self.input.focused_window = None;
            self.dirty = true;
        } else if !self.input.show_desktop_active && self.input.show_desktop_minimized_mask != 0 {
            // Restore windows that were minimized by Show Desktop
            let mask = self.input.show_desktop_minimized_mask;
            for i in 0..self.space.window_count.min(32) {
                if (mask >> i) & 1 != 0 {
                    self.space.windows[i].minimized = false;
                }
            }
            self.input.show_desktop_minimized_mask = 0;
            self.dirty = true;
        }
        let _ = prev_show_desktop;

        // Handle notification panel close → mark all read
        if self.input.notifications_mark_read {
            self.input.notifications_mark_read = false;
            self.desktop.mark_all_read();
            self.dirty = true;
        }

        // Handle pending context menu action
        use crate::input::ContextAction;
        let action = self.input.pending_context_action;
        if action != ContextAction::None {
            self.input.pending_context_action = ContextAction::None;
            match action {
                ContextAction::NewWindow => {
                    use crate::compositor::{ShellWindow, WindowContent};
                    let fb_w = self.backend.fb.info.width as i32;
                    let fb_h = self.backend.fb.info.height as i32;
                    let win = ShellWindow {
                        x: 100 + (self.space.window_count as i32 * 30) % (fb_w / 2),
                        y: 100 + (self.space.window_count as i32 * 30) % (fb_h / 3),
                        w: 400, h: 300,
                        curr_x: 100.0, curr_y: 100.0, curr_w: 400.0, curr_h: 300.0,
                        content: WindowContent::InternalDemo,
                        workspace: self.input.current_workspace,
                        ..Default::default()
                    };
                    let title = b"Lunas Terminal\0";
                    let idx = self.space.window_count;
                    self.space.map_window(win);
                    if idx < self.space.window_count {
                        let len = title.len().min(32);
                        self.space.windows[idx].title[..len].copy_from_slice(&title[..len]);
                    }
                    self.input.focused_window = Some(idx);
                    self.dirty = true;
                }
                ContextAction::ToggleTiling => {
                    self.input.tiling_active = !self.input.tiling_active;
                    self.dirty = true;
                }
                ContextAction::OpenDashboard => {
                    self.input.dashboard_active = !self.input.dashboard_active;
                    self.dirty = true;
                }
                ContextAction::CycleWallpaper => {
                    self.desktop.wallpaper_mode = match self.desktop.wallpaper_mode {
                        crate::desktop::WallpaperMode::SolidColor => crate::desktop::WallpaperMode::Gradient,
                        crate::desktop::WallpaperMode::Gradient => crate::desktop::WallpaperMode::CosmicTheme,
                        crate::desktop::WallpaperMode::CosmicTheme => crate::desktop::WallpaperMode::SolidColor,
                    };
                    // Re-render the background buffer for the new wallpaper mode.
                    self.backend.fb.pre_render_background(
                        self.desktop.wallpaper_mode,
                        self.desktop.wallpaper_color,
                    );
                    self.dirty = true;
                }
                ContextAction::CloseWindow(idx) => {
                    if idx < self.space.window_count {
                        self.space.windows[idx].closing = true;
                        if self.input.focused_window == Some(idx) {
                            self.input.focused_window = None;
                        }
                        self.dirty = true;
                    }
                }
                ContextAction::MinimizeWindow(idx) => {
                    if idx < self.space.window_count {
                        let w = &mut self.space.windows[idx];
                        w.minimized = !w.minimized;
                        if w.minimized && self.input.focused_window == Some(idx) {
                            self.input.focused_window = None;
                        }
                        self.dirty = true;
                    }
                }
                ContextAction::MaximizeWindow(idx) => {
                    if idx < self.space.window_count {
                        let fb_w = self.backend.fb.info.width as i32;
                        let fb_h = self.backend.fb.info.height as i32;
                        let w = &mut self.space.windows[idx];
                        if w.maximized {
                            let (sx, sy, sw, sh) = w.stored_rect;
                            w.x = sx; w.y = sy; w.w = sw; w.h = sh;
                            w.maximized = false;
                        } else {
                            w.stored_rect = (w.x, w.y, w.w, w.h);
                            w.x = 0;
                            w.y = ShellWindow::TITLE_H;
                            w.w = fb_w;
                            w.h = fb_h - ShellWindow::TITLE_H - 44;
                            w.maximized = true;
                        }
                        self.dirty = true;
                    }
                }
                ContextAction::VolumeUp => {
                    self.desktop.volume_level = (self.desktop.volume_level + 10).min(100);
                    self.input.volume_level = self.desktop.volume_level;
                    self.dirty = true;
                }
                ContextAction::VolumeDown => {
                    self.desktop.volume_level = self.desktop.volume_level.saturating_sub(10);
                    self.input.volume_level = self.desktop.volume_level;
                    self.dirty = true;
                }
                ContextAction::ToggleMute => {
                    self.desktop.volume_muted = !self.desktop.volume_muted;
                    self.input.volume_muted = self.desktop.volume_muted;
                    self.dirty = true;
                }
                ContextAction::SetVolume(level) => {
                    self.desktop.volume_level = level;
                    self.desktop.volume_muted = false;
                    self.input.volume_level = level;
                    self.input.volume_muted = false;
                    self.dirty = true;
                }
                ContextAction::LaunchPinnedApp(app_idx) => {
                    // Focus running window matching this pinned app, or launch if none
                    let mut name_buf = [0u8; 32];
                    let name_len = if app_idx < self.desktop.pinned_count {
                        let name = self.desktop.pinned_apps[app_idx].name_str();
                        let len = name.len().min(32);
                        name_buf[..len].copy_from_slice(&name.as_bytes()[..len]);
                        len
                    } else {
                        0
                    };
                    let app_name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("");
                    let mut running_window: Option<usize> = None;
                    if !app_name.is_empty() {
                        for w_idx in 0..self.space.window_count {
                            let w = &self.space.windows[w_idx];
                            if w.content == crate::compositor::WindowContent::None || w.closing { continue; }
                            if w.workspace != self.input.current_workspace { continue; }
                            let w_title = w.title_str();
                            // Use byte comparison to avoid UTF-8 boundary issues
                            let title_bytes = w_title.as_bytes();
                            let name_bytes = app_name.as_bytes();
                            if title_bytes.len() >= name_bytes.len()
                                && title_bytes[..name_bytes.len()].eq_ignore_ascii_case(name_bytes)
                            {
                                running_window = Some(w_idx);
                                break;
                            }
                        }
                    }
                    if let Some(w_idx) = running_window {
                        self.space.windows[w_idx].minimized = false;
                        self.input.focused_window = Some(w_idx);
                    } else {
                        self.launch_pinned_app(app_idx);
                    }
                    self.dirty = true;
                }
                ContextAction::UnpinApp(app_idx) => {
                    self.desktop.unpin_app(app_idx);
                    self.sync_pinned_apps_to_input();
                    self.dirty = true;
                }
                ContextAction::PinApp(w_idx) => {
                    if w_idx < self.space.window_count
                        && self.desktop.pinned_count < crate::desktop::MAX_PINNED_APPS
                    {
                        // Copy window title to avoid borrow conflict
                        let mut title_buf = [0u8; 32];
                        let title_str = self.space.windows[w_idx].title_str();
                        let title_len = title_str.len().min(32);
                        title_buf[..title_len].copy_from_slice(&title_str.as_bytes()[..title_len]);

                        if let Ok(title) = core::str::from_utf8(&title_buf[..title_len]) {
                            if !title.is_empty() {
                                // Skip if already pinned
                                let already = (0..self.desktop.pinned_count).any(|i| {
                                    self.desktop.pinned_apps[i].name_str().eq_ignore_ascii_case(title)
                                });
                                if !already {
                                    self.desktop.pin_app(title, 0, 180, 255);
                                    self.sync_pinned_apps_to_input();
                                }
                            }
                        }
                    }
                    self.dirty = true;
                }
                ContextAction::BrightnessUp => {
                    self.desktop.brightness_level = (self.desktop.brightness_level + 10).min(100);
                    self.dirty = true;
                }
                ContextAction::BrightnessDown => {
                    self.desktop.brightness_level = self.desktop.brightness_level.saturating_sub(10);
                    self.dirty = true;
                }
                ContextAction::SetBrightness(level) => {
                    self.desktop.brightness_level = level;
                    self.dirty = true;
                }
                ContextAction::ToggleDoNotDisturb => {
                    self.desktop.do_not_disturb = !self.desktop.do_not_disturb;
                    self.input.do_not_disturb = self.desktop.do_not_disturb;
                    self.dirty = true;
                }
                ContextAction::ToggleNightLight => {
                    self.desktop.night_light_active = !self.desktop.night_light_active;
                    self.input.night_light_active = self.desktop.night_light_active;
                    self.dirty = true;
                }
                ContextAction::TakeScreenshot => {
                    // Capture back buffer to disk on Eclipse targets.
                    #[cfg(target_vendor = "eclipse")]
                    {
                        self.backend.fb.save_screenshot();
                    }
                    self.dirty = true;
                }
                ContextAction::MarkNotificationsRead => {
                    self.desktop.mark_all_read();
                    self.dirty = true;
                }
                ContextAction::ToggleLauncher => {
                    self.input.launcher_active = !self.input.launcher_active;
                    self.dirty = true;
                }
                ContextAction::ToggleLock => {
                    self.input.lock_screen_active = !self.input.lock_screen_active;
                    self.dirty = true;
                }
                ContextAction::ShowDesktop => {
                    self.input.show_desktop_active = !self.input.show_desktop_active;
                    self.dirty = true;
                }
                ContextAction::SwitchWorkspace(ws) => {
                    self.input.current_workspace = ws;
                    self.dirty = true;
                }
                ContextAction::None => {}
            }
        }
    }

    /// Synchronise pinned app count and names from DesktopShell into InputState
    /// so that taskbar hit-testing stays in sync after any pin/unpin operation.
    /// Also syncs other mirrored fields (volume, battery, notification count).
    fn sync_pinned_apps_to_input(&mut self) {
        self.input.pinned_app_count = self.desktop.pinned_count;
        for i in 0..self.desktop.pinned_count.min(16) {
            let name = self.desktop.pinned_apps[i].name_str();
            let name_bytes = name.as_bytes();
            let len = name_bytes.len().min(32);
            self.input.pinned_app_names[i] = [0u8; 32];
            self.input.pinned_app_names[i][..len].copy_from_slice(&name_bytes[..len]);
        }
        // Sync state-aware tooltip fields
        self.input.volume_level = self.desktop.volume_level;
        self.input.battery_level = self.desktop.battery_level;
        self.input.notification_count = self.desktop.notification_count;
        self.input.do_not_disturb = self.desktop.do_not_disturb;
        self.input.night_light_active = self.desktop.night_light_active;
        self.input.volume_muted = self.desktop.volume_muted;
    }

    /// Launch a pinned app by its index, looking up its exec_path.
    fn launch_pinned_app(&mut self, app_idx: usize) {
        if app_idx < self.desktop.pinned_count {
            // Copy exec_path to a local buffer to avoid borrow conflict
            let mut path_buf = [0u8; 64];
            path_buf.copy_from_slice(&self.desktop.pinned_apps[app_idx].exec_path);
            let len = path_buf.iter().position(|&b| b == 0).unwrap_or(64);
            if len > 0 {
                if let Ok(exec) = core::str::from_utf8(&path_buf[..len]) {
                    self.launch_app(exec);
                }
            }
        }
    }

    /// Launch an application by its executable path.
    fn launch_app(&mut self, _exec_path: &str) {
        #[cfg(target_vendor = "eclipse")]
        {
            let _ = std::process::Command::new(_exec_path).spawn();
        }
        // On non-Eclipse targets (tests), we just record the intent
        #[cfg(not(target_vendor = "eclipse"))]
        {
            // No-op: app launching is only available on Eclipse OS
        }
        self.dirty = true;
    }

    /// Update system metrics at a throttled rate.
    fn update_metrics_if_needed(&mut self) {
        let interval_ms = if self.input.dashboard_active || self.input.system_central_active {
            800
        } else {
            4000
        };

        let now = std::time::Instant::now();
        if now.duration_since(self.last_metrics_update).as_millis() < interval_ms {
            return;
        }
        self.last_metrics_update = now;

        // System stats
        let mut stats = SystemStats::default();
        let _ = unsafe { get_system_stats(&mut stats) };

        if let Some(ref prev) = self.prev_stats {
            let total_delta = stats.uptime_ticks.saturating_sub(prev.uptime_ticks);
            let idle_delta = stats.idle_ticks.saturating_sub(prev.idle_ticks);
            if total_delta > 0 {
                self.cpu_usage = ((total_delta - idle_delta) as f64 / total_delta as f64 * 100.0) as f32;
            }
        }

        if stats.total_mem_frames > 0 {
            self.mem_usage = (stats.used_mem_frames as f64 / stats.total_mem_frames as f64 * 100.0) as f32;
            self.mem_total_kb = stats.total_mem_frames * 4;
        }

        self.cpu_count = stats.cpu_count;
        self.cpu_temp = stats.cpu_temp[0];
        self.gpu_load = stats.gpu_load[0];
        self.gpu_temp = stats.gpu_temp[0];
        self.gpu_vram_total_kb = stats.gpu_vram_total_bytes / 1024;
        self.anomaly_count = stats.anomaly_count;
        self.heap_fragmentation = stats.heap_fragmentation;
        self.prev_stats = Some(stats);

        // Update clock and date from wall time offset (Unix timestamp in seconds)
        const SECONDS_PER_DAY: u64 = 86400;
        let secs_today = if stats.wall_time_offset > 0 {
            (stats.wall_time_offset % SECONDS_PER_DAY) as u32
        } else {
            // Fallback: derive from uptime ticks (milliseconds) for basic progression
            let secs = (stats.uptime_ticks / 1000) as u32;
            secs % SECONDS_PER_DAY as u32
        };
        self.desktop.clock_hours = (secs_today / 3600) as u8;
        self.desktop.clock_minutes = ((secs_today % 3600) / 60) as u8;

        // Compute day/month from Unix timestamp
        if stats.wall_time_offset > 0 {
            let (month, day) = unix_timestamp_to_date(stats.wall_time_offset);
            self.desktop.clock_month = month;
            self.desktop.clock_day = day;
        }

        // Process list
        self.process_count = unsafe { get_process_list(self.process_list.as_mut_ptr(), 32) } as usize;

        self.dirty = true;
    }

    /// Render the desktop to the framebuffer.
    pub fn render(&mut self) {
        render::draw_desktop_shell(
            &mut self.backend.fb,
            &self.input,
            &self.space.windows,
            self.space.window_count,
            &self.surfaces,
            &self.desktop,
            &self.service_list,
            self.service_count,
            self.cpu_usage,
            self.mem_usage,
            self.net_usage,
            &self.log_buf,
            self.log_len,
            self.net_extended_stats.as_ref(),
        );
        self.backend.swap_buffers();
    }

    /// Notify external clients about a resize event.
    pub fn notify_external_resize(&self, window_idx: usize) {
        if window_idx >= self.space.window_count { return; }
        let w = &self.space.windows[window_idx];
        if let crate::compositor::WindowContent::External(s_idx) = w.content {
            let s = s_idx as usize;
            if s < self.surfaces.len() && self.surfaces[s].active {
                let ev = SideWindEvent {
                    event_type: SWND_EVENT_TYPE_RESIZE,
                    data1: w.w,
                    data2: w.h - ShellWindow::TITLE_H,
                    data3: 0,
                };
                let _ = unsafe {
                    eclipse_send(
                        self.surfaces[s].pid,
                        sidewind::MSG_TYPE_INPUT,
                        &ev as *const _ as *const core::ffi::c_void,
                        core::mem::size_of::<SideWindEvent>(),
                        0,
                    )
                };
            }
        }
    }
}

/// Convert a Unix timestamp (seconds since epoch) to (month, day).
fn unix_timestamp_to_date(timestamp: u64) -> (u8, u8) {
    let days_since_epoch = (timestamp / 86400) as u32;
    // Simple date calculation from days since 1970-01-01
    let mut year = 1970u32;
    let mut remaining_days = days_since_epoch;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let leap = is_leap_year(year);
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];

    let mut month = 1u8;
    for (i, &days) in month_days.iter().enumerate() {
        if remaining_days < days {
            month = (i + 1) as u8;
            break;
        }
        remaining_days -= days;
        // If we consumed all months, remaining_days stays for the last month
        if i == 11 {
            month = 12;
        }
    }

    let day = (remaining_days + 1) as u8;
    (month, day)
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::{ShellWindow, WindowContent};

    #[test]
    fn test_state_init() {
        let state = LunasState::new().expect("init");
        assert_eq!(state.counter, 0);
        assert_eq!(state.space.window_count, 0);
        assert!(state.dirty);
        assert_eq!(state.desktop.pinned_count, 5);
        assert_eq!(state.desktop.notification_count, 1); // Welcome notification
        assert_eq!(state.input.pinned_app_count, 5); // Synced from desktop
    }

    #[test]
    fn test_maximize_request() {
        let mut state = LunasState::new().expect("init");
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        state.space.map_window(win);
        state.input.focused_window = Some(0);

        // Simulate maximize
        let fb_w = state.backend.fb.info.width as i32;
        let fb_h = state.backend.fb.info.height as i32;
        let w = &mut state.space.windows[0];
        w.stored_rect = (w.x, w.y, w.w, w.h);
        w.x = 0;
        w.y = ShellWindow::TITLE_H;
        w.w = fb_w;
        w.h = fb_h - ShellWindow::TITLE_H - 44;
        w.maximized = true;

        assert!(state.space.windows[0].maximized);
        assert_eq!(state.space.windows[0].x, 0);
    }

    #[test]
    fn test_minimize_request() {
        let mut state = LunasState::new().expect("init");
        let win = ShellWindow {
            x: 50, y: 50, w: 200, h: 200,
            curr_x: 50.0, curr_y: 50.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        state.space.map_window(win);
        state.space.windows[0].minimized = true;
        assert!(state.space.windows[0].minimized);
    }

    #[test]
    fn test_close_request() {
        let mut state = LunasState::new().expect("init");
        let win = ShellWindow {
            x: 50, y: 50, w: 200, h: 200,
            curr_x: 50.0, curr_y: 50.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        state.space.map_window(win);
        state.space.windows[0].closing = true;
        assert!(state.space.windows[0].closing);
    }

    #[test]
    fn test_service_info_new() {
        let svc = ServiceInfo::new();
        assert_eq!(svc.state, 0);
        assert_eq!(svc.pid, 0);
    }

    #[test]
    fn test_desktop_notification() {
        let mut state = LunasState::new().expect("init");
        state.desktop.push_notification("Test alert", 2);
        assert_eq!(state.desktop.notification_count, 2); // 1 welcome + 1 test
    }

    #[test]
    fn test_handle_event_net_stats() {
        let mut state = LunasState::new().expect("init");
        state.handle_event(&CompositorEvent::NetStats(1000, 500));
        assert_eq!(state.net_rx, 1000);
        assert_eq!(state.net_tx, 500);
        assert!(state.dirty);
    }

    #[test]
    fn test_handle_event_kernel_log() {
        let mut state = LunasState::new().expect("init");
        let mut log = heapless::Vec::<u8, 252>::new();
        let _ = log.extend_from_slice(b"test log");
        state.handle_event(&CompositorEvent::KernelLog(log));
        assert!(state.log_len > 0);
    }

    #[test]
    fn test_pinned_app_click_consumed() {
        let mut state = LunasState::new().expect("init");
        // Simulate a pinned app click
        state.input.last_pinned_app_click = Some(0);
        state.dirty = false;
        let _ = state.update();
        // The click should have been consumed
        assert_eq!(state.input.last_pinned_app_click, None);
    }

    #[test]
    fn test_pinned_app_click_focuses_running_window() {
        let mut state = LunasState::new().expect("init");
        // Add a window whose title starts with the first pinned app's name
        let app_name = state.desktop.pinned_apps[0].name_str().to_string();
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            ..Default::default()
        };
        state.space.map_window(win);
        // Set window title to match the pinned app name
        let title_bytes = app_name.as_bytes();
        let len = title_bytes.len().min(32);
        state.space.windows[0].title[..len].copy_from_slice(&title_bytes[..len]);

        state.input.current_workspace = 0;

        // Simulate clicking the first pinned app
        state.input.last_pinned_app_click = Some(0);
        let _ = state.update();

        // Click should have been consumed
        assert_eq!(state.input.last_pinned_app_click, None);
        // The running window should be focused instead of launching a new app
        assert_eq!(state.input.focused_window, Some(0));
        assert!(!state.space.windows[0].minimized);
    }

    #[test]
    fn test_pinned_app_click_restores_minimized_running_window() {
        let mut state = LunasState::new().expect("init");
        let app_name = state.desktop.pinned_apps[0].name_str().to_string();
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            minimized: true,
            ..Default::default()
        };
        state.space.map_window(win);
        let title_bytes = app_name.as_bytes();
        let len = title_bytes.len().min(32);
        state.space.windows[0].title[..len].copy_from_slice(&title_bytes[..len]);

        state.input.current_workspace = 0;

        // Clicking pinned app with minimized matching window should restore and focus it
        state.input.last_pinned_app_click = Some(0);
        let _ = state.update();

        assert_eq!(state.input.focused_window, Some(0));
        assert!(!state.space.windows[0].minimized, "window should be restored");
    }

    #[test]
    fn test_volume_toggle_mute() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        assert!(!state.desktop.volume_muted);
        // Simulate mute toggle via context action
        state.input.pending_context_action = ContextAction::ToggleMute;
        let _ = state.update();
        assert!(state.desktop.volume_muted);
        // Toggle again to unmute
        state.input.pending_context_action = ContextAction::ToggleMute;
        let _ = state.update();
        assert!(!state.desktop.volume_muted);
    }

    #[test]
    fn test_clock_click_toggles_calendar_panel() {
        let mut state = LunasState::new().expect("init");
        assert!(!state.input.clock_panel_active);
        state.input.clock_clicked = true;
        let _ = state.update();
        assert!(state.input.clock_panel_active, "clock click should open calendar panel");
        assert!(!state.input.clock_clicked, "clock_clicked should be cleared");
        // Click again to close
        state.input.clock_clicked = true;
        let _ = state.update();
        assert!(!state.input.clock_panel_active, "second click should close calendar panel");
    }

    #[test]
    fn test_launcher_click_launches_app() {
        let mut state = LunasState::new().expect("init");
        state.input.launcher_active = true;

        // Click position of first launcher item
        // Panel at y = fb_h - 44 - 400 - 10
        let fb_h = state.backend.fb.info.height as i32;
        let panel_y = fb_h - 44 - 400 - 10;
        let item_y = panel_y + 50; // first item

        state.input.launcher_click_pos = Some((100, item_y + 5));
        let _ = state.update();

        // Launcher should close after clicking an item
        assert!(!state.input.launcher_active);
        assert_eq!(state.input.launcher_click_pos, None);
    }

    #[test]
    fn test_launcher_click_outside_closes() {
        let mut state = LunasState::new().expect("init");
        state.input.launcher_active = true;

        // Click far outside the launcher panel
        state.input.launcher_click_pos = Some((800, 400));
        let _ = state.update();

        // Launcher should close when clicking outside
        assert!(!state.input.launcher_active);
    }

    #[test]
    fn test_launcher_hover_updates() {
        let mut state = LunasState::new().expect("init");
        state.input.launcher_active = true;

        // Position cursor over first launcher item
        let fb_h = state.backend.fb.info.height as i32;
        let panel_y = fb_h - 44 - 400 - 10;
        state.input.cursor_x = 100;
        state.input.cursor_y = panel_y + 50 + 5;

        let _ = state.update();
        assert!(state.input.launcher_hovered_index.is_some());
    }

    #[test]
    fn test_notification_mark_read_on_close() {
        let mut state = LunasState::new().expect("init");
        // Add notifications
        state.desktop.push_notification("Alert 1", 1);
        state.desktop.push_notification("Alert 2", 1);
        assert!(state.desktop.unread_count() > 0);

        // Simulate notification panel being closed by clicking on it
        state.input.notifications_mark_read = true;
        let _ = state.update();

        // All notifications should be marked as read
        assert_eq!(state.desktop.unread_count(), 0);
        assert!(!state.input.notifications_mark_read);
    }

    #[test]
    fn test_context_action_new_window() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        assert_eq!(state.space.window_count, 0);

        state.input.pending_context_action = ContextAction::NewWindow;
        let _ = state.update();

        assert_eq!(state.space.window_count, 1);
        assert_eq!(state.input.pending_context_action, ContextAction::None);
    }

    #[test]
    fn test_context_action_toggle_tiling() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        assert!(!state.input.tiling_active);

        state.input.pending_context_action = ContextAction::ToggleTiling;
        let _ = state.update();
        assert!(state.input.tiling_active);
    }

    #[test]
    fn test_context_action_cycle_wallpaper() {
        use crate::input::ContextAction;
        use crate::desktop::WallpaperMode;
        let mut state = LunasState::new().expect("init");
        assert_eq!(state.desktop.wallpaper_mode, WallpaperMode::CosmicTheme);

        state.input.pending_context_action = ContextAction::CycleWallpaper;
        let _ = state.update();
        assert_eq!(state.desktop.wallpaper_mode, WallpaperMode::SolidColor);

        state.input.pending_context_action = ContextAction::CycleWallpaper;
        let _ = state.update();
        assert_eq!(state.desktop.wallpaper_mode, WallpaperMode::Gradient);
    }

    #[test]
    fn test_context_action_close_window() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        state.space.map_window(win);
        state.input.focused_window = Some(0);

        state.input.pending_context_action = ContextAction::CloseWindow(0);
        let _ = state.update();
        assert!(state.space.windows[0].closing);
        assert_eq!(state.input.focused_window, None);
    }

    #[test]
    fn test_context_action_minimize_window() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        state.space.map_window(win);
        state.input.focused_window = Some(0);

        state.input.pending_context_action = ContextAction::MinimizeWindow(0);
        let _ = state.update();
        assert!(state.space.windows[0].minimized);
        assert_eq!(state.input.focused_window, None);

        // Toggle back to restore
        state.input.pending_context_action = ContextAction::MinimizeWindow(0);
        let _ = state.update();
        assert!(!state.space.windows[0].minimized);
    }

    #[test]
    fn test_context_action_volume_up_down() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        let initial_vol = state.desktop.volume_level;

        state.input.pending_context_action = ContextAction::VolumeUp;
        let _ = state.update();
        assert_eq!(state.desktop.volume_level, initial_vol + 10);

        state.input.pending_context_action = ContextAction::VolumeDown;
        let _ = state.update();
        assert_eq!(state.desktop.volume_level, initial_vol);
    }

    #[test]
    fn test_unix_timestamp_to_date() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let (month, day) = unix_timestamp_to_date(1704067200);
        assert_eq!(month, 1);
        assert_eq!(day, 1);
    }

    #[test]
    fn test_unix_timestamp_to_date_leap_year() {
        // 2024-02-29 00:00:00 UTC = 1709164800
        let (month, day) = unix_timestamp_to_date(1709164800);
        assert_eq!(month, 2);
        assert_eq!(day, 29);
    }

    #[test]
    fn test_unix_timestamp_to_date_epoch() {
        // 1970-01-01 = 0
        let (month, day) = unix_timestamp_to_date(0);
        assert_eq!(month, 1);
        assert_eq!(day, 1);
    }

    #[test]
    fn test_date_fields_initialized() {
        let state = LunasState::new().expect("init");
        assert_eq!(state.desktop.clock_day, 1);
        assert_eq!(state.desktop.clock_month, 1);
    }

    #[test]
    fn test_wayland_compositor_initialized() {
        let state = LunasState::new().expect("init");
        assert!(state.wayland.connections.is_empty(), "no wayland connections at startup");
        assert!(!state.xwayland.is_active(), "xwayland not active at startup");
    }

    #[test]
    fn test_handle_event_wayland_create_surface() {
        let mut state = LunasState::new().expect("init");
        assert_eq!(state.space.window_count, 0);

        // Simulate wl_compositor.create_surface: obj=4, opcode=0, new_id=5
        let mut msg = heapless::Vec::<u8, 512>::new();
        let _ = msg.extend_from_slice(&4u32.to_le_bytes()); // obj_id (compositor)
        let _ = msg.extend_from_slice(&((12u32 << 16) | 0u32).to_le_bytes()); // size=12, op=0
        let _ = msg.extend_from_slice(&5u32.to_le_bytes()); // new surface id

        state.handle_event(&CompositorEvent::Wayland(msg, 42));
        // A new ShellWindow should have been created for the Wayland surface.
        assert_eq!(state.space.window_count, 1);
        assert!(matches!(
            state.space.windows[0].content,
            WindowContent::Wayland { surface_id: 5, .. }
        ));
    }

    #[test]
    fn test_handle_event_wayland_commit_surface() {
        let mut state = LunasState::new().expect("init");

        // First create a surface
        let mut msg = heapless::Vec::<u8, 512>::new();
        let _ = msg.extend_from_slice(&4u32.to_le_bytes());
        let _ = msg.extend_from_slice(&((12u32 << 16) | 0u32).to_le_bytes());
        let _ = msg.extend_from_slice(&5u32.to_le_bytes());
        state.handle_event(&CompositorEvent::Wayland(msg, 42));
        assert_eq!(state.space.window_count, 1);

        // Minimize the window manually
        state.space.windows[0].minimized = true;

        // Commit the surface — should restore it
        let mut commit_msg = heapless::Vec::<u8, 512>::new();
        let _ = commit_msg.extend_from_slice(&5u32.to_le_bytes()); // obj_id = surface
        let _ = commit_msg.extend_from_slice(&((8u32 << 16) | 6u32).to_le_bytes()); // op=6 commit
        state.handle_event(&CompositorEvent::Wayland(commit_msg, 42));
        assert!(!state.space.windows[0].minimized, "commit should restore window");
    }

    #[test]
    fn test_handle_event_x11_map_window() {
        let mut state = LunasState::new().expect("init");
        assert_eq!(state.space.window_count, 0);

        // Simulate X11 MapNotify event (type=19, window_id=77)
        let mut ev = heapless::Vec::<u8, 512>::new();
        let mut buf = [0u8; 32];
        buf[0] = 19; // MapNotify
        buf[4..8].copy_from_slice(&77u32.to_le_bytes());
        let _ = ev.extend_from_slice(&buf);
        state.xwayland.set_pid(55);
        state.handle_event(&CompositorEvent::X11(ev, 55));

        assert_eq!(state.space.window_count, 1, "X11 MapNotify should create a window");
        assert!(matches!(
            state.space.windows[0].content,
            WindowContent::Wayland { surface_id: 77, .. }
        ));
    }

    #[test]
    fn test_handle_event_x11_destroy_window() {
        use crate::compositor::WindowContent;
        let mut state = LunasState::new().expect("init");
        state.xwayland.set_pid(55);

        // Map a window first
        let mut map_ev = heapless::Vec::<u8, 512>::new();
        let mut map_buf = [0u8; 32];
        map_buf[0] = 19;
        map_buf[4..8].copy_from_slice(&10u32.to_le_bytes());
        let _ = map_ev.extend_from_slice(&map_buf);
        state.handle_event(&CompositorEvent::X11(map_ev, 55));
        assert_eq!(state.space.window_count, 1);

        // Now destroy it
        let mut destroy_ev = heapless::Vec::<u8, 512>::new();
        let mut destroy_buf = [0u8; 32];
        destroy_buf[0] = 17; // DestroyNotify
        destroy_buf[4..8].copy_from_slice(&10u32.to_le_bytes());
        let _ = destroy_ev.extend_from_slice(&destroy_buf);
        state.handle_event(&CompositorEvent::X11(destroy_ev, 55));
        assert!(state.space.windows[0].closing, "DestroyNotify should mark window as closing");
    }

    #[test]
    fn test_pinned_app_names_synced_at_init() {
        use crate::desktop::DesktopShell;
        let state = LunasState::new().expect("init");
        // Verify that input.pinned_app_names mirrors the desktop's pinned apps
        for i in 0..state.desktop.pinned_count.min(16) {
            let desktop_name = state.desktop.pinned_apps[i].name_str();
            let input_name_bytes = &state.input.pinned_app_names[i];
            let input_name_len = input_name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
            let input_name = core::str::from_utf8(&input_name_bytes[..input_name_len]).unwrap_or("");
            assert_eq!(
                desktop_name, input_name,
                "pinned_app_names[{i}] should match desktop pinned app name"
            );
        }
        // The count should also be in sync
        assert_eq!(state.input.pinned_app_count, state.desktop.pinned_count);
    }

    #[test]
    fn test_context_action_launch_pinned_app_focuses_running_window() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        // Add a window matching the first pinned app ("Terminal")
        let app_name = state.desktop.pinned_apps[0].name_str().to_string();
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            minimized: true,
            ..Default::default()
        };
        let idx = state.space.window_count;
        state.space.map_window(win);
        let title_bytes = app_name.as_bytes();
        let title_len = title_bytes.len().min(32);
        state.space.windows[idx].title[..title_len].copy_from_slice(&title_bytes[..title_len]);

        // LaunchPinnedApp(0) should focus and restore the running window
        state.input.pending_context_action = ContextAction::LaunchPinnedApp(0);
        let _ = state.update();
        assert!(!state.space.windows[idx].minimized, "window should be restored");
        assert_eq!(state.input.focused_window, Some(idx), "window should be focused");
    }

    #[test]
    fn test_context_action_unpin_app_syncs_input() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        let initial_count = state.desktop.pinned_count; // 5
        assert_eq!(state.input.pinned_app_count, initial_count);

        // Unpin the first app (Terminal)
        state.input.pending_context_action = ContextAction::UnpinApp(0);
        let _ = state.update();

        assert_eq!(state.desktop.pinned_count, initial_count - 1, "desktop pinned count should decrease");
        assert_eq!(state.input.pinned_app_count, initial_count - 1, "input pinned count should be synced");
        // The first app should now be "Files" (was at index 1)
        assert_eq!(state.desktop.pinned_apps[0].name_str(), "Files");
    }

    #[test]
    fn test_context_action_pin_app_adds_to_taskbar() {
        use crate::input::ContextAction;
        use crate::compositor::{ShellWindow, WindowContent};
        let mut state = LunasState::new().expect("init");
        let initial_count = state.desktop.pinned_count; // 5

        // Add a window with a unique title not yet pinned
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            ..Default::default()
        };
        let idx = state.space.window_count;
        state.space.map_window(win);
        let title = b"MyUniqueApp";
        state.space.windows[idx].title[..title.len()].copy_from_slice(title);

        // Pin the window
        state.input.pending_context_action = ContextAction::PinApp(idx);
        let _ = state.update();

        assert_eq!(state.desktop.pinned_count, initial_count + 1, "pinned count should increase");
        assert_eq!(state.input.pinned_app_count, initial_count + 1, "input count should be synced");
        // Last pinned app should be "MyUniqueApp"
        assert_eq!(state.desktop.pinned_apps[initial_count].name_str(), "MyUniqueApp");
    }

    #[test]
    fn test_context_action_pin_app_skips_duplicate() {
        use crate::input::ContextAction;
        use crate::compositor::{ShellWindow, WindowContent};
        let mut state = LunasState::new().expect("init");
        let initial_count = state.desktop.pinned_count; // 5

        // Add a window titled "Terminal" (already pinned)
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            ..Default::default()
        };
        let idx = state.space.window_count;
        state.space.map_window(win);
        let title = b"Terminal";
        state.space.windows[idx].title[..title.len()].copy_from_slice(title);

        // Try to pin it — should be a no-op since "Terminal" is already pinned
        state.input.pending_context_action = ContextAction::PinApp(idx);
        let _ = state.update();

        assert_eq!(state.desktop.pinned_count, initial_count, "duplicate pin should be skipped");
    }

    #[test]
    fn test_show_desktop_minimizes_and_restores_windows() {
        use crate::compositor::{ShellWindow, WindowContent};
        let mut state = LunasState::new().expect("init");

        // Add two visible windows on workspace 0
        let win_a = ShellWindow {
            x: 100, y: 100, w: 200, h: 200,
            curr_x: 100.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            ..Default::default()
        };
        let win_b = ShellWindow {
            x: 400, y: 100, w: 200, h: 200,
            curr_x: 400.0, curr_y: 100.0, curr_w: 200.0, curr_h: 200.0,
            content: WindowContent::InternalDemo,
            workspace: 0,
            ..Default::default()
        };
        let ia = state.space.window_count;
        state.space.map_window(win_a);
        let ib = state.space.window_count;
        state.space.map_window(win_b);
        assert!(!state.space.windows[ia].minimized);
        assert!(!state.space.windows[ib].minimized);

        // Activate show-desktop
        state.input.show_desktop_active = true;
        let _ = state.update();
        assert!(state.space.windows[ia].minimized, "window A should be minimized");
        assert!(state.space.windows[ib].minimized, "window B should be minimized");
        assert_ne!(state.input.show_desktop_minimized_mask, 0, "mask should be non-zero");

        // Deactivate show-desktop — windows should be restored
        state.input.show_desktop_active = false;
        let _ = state.update();
        assert!(!state.space.windows[ia].minimized, "window A should be restored");
        assert!(!state.space.windows[ib].minimized, "window B should be restored");
        assert_eq!(state.input.show_desktop_minimized_mask, 0, "mask should be cleared");
    }

    #[test]
    fn test_pinned_app_drag_swap() {
        use crate::input::ContextAction;
        let mut state = LunasState::new().expect("init");
        let initial_first = state.desktop.pinned_apps[0].name_str().to_string();
        let initial_second = state.desktop.pinned_apps[1].name_str().to_string();

        // Signal a drag swap: move app 0 to position 1
        state.input.pending_pinned_swap = Some((0, 1));
        let _ = state.update();

        assert_eq!(state.desktop.pinned_apps[0].name_str(), initial_second, "apps should be swapped");
        assert_eq!(state.desktop.pinned_apps[1].name_str(), initial_first, "apps should be swapped");
        // Input state should be synced
        let new_name = {
            let b = &state.input.pinned_app_names[0];
            let len = b.iter().position(|&x| x == 0).unwrap_or(32);
            core::str::from_utf8(&b[..len]).unwrap_or("").to_string()
        };
        assert_eq!(new_name, initial_second, "input pinned names should be synced after swap");
        // No pending action
        assert_eq!(state.input.pending_context_action, ContextAction::None);
    }

    #[test]
    fn test_desktop_battery_fields_default() {
        let state = LunasState::new().expect("init");
        // Battery fields should be initialized
        assert_eq!(state.desktop.battery_level, 80, "default battery level should be 80");
        assert!(!state.desktop.battery_charging, "default should not be charging");
        assert!(state.desktop.show_battery, "show_battery should be true by default");
    }

    #[test]
    fn test_desktop_swap_pinned_apps_noop_same_index() {
        let mut state = LunasState::new().expect("init");
        let name0_before = state.desktop.pinned_apps[0].name_str().to_string();
        state.desktop.swap_pinned_apps(0, 0);
        assert_eq!(state.desktop.pinned_apps[0].name_str(), name0_before.as_str(), "swap(0,0) should be no-op");
    }

    #[test]
    fn test_calendar_fields_initialized() {
        let state = LunasState::new().expect("init");
        assert_eq!(state.desktop.clock_year, 2026, "clock_year should be initialized");
        assert!(state.desktop.clock_day >= 1 && state.desktop.clock_day <= 31);
        assert!(state.desktop.clock_month >= 1 && state.desktop.clock_month <= 12);
    }
}
