//! Central state of the Lunas desktop environment.
//! Orchestrates Backend, Space, Input, IPC, Desktop Shell, and Rendering.

use crate::backend::Backend;
use crate::compositor::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, ShellWindow, MAX_EXTERNAL_SURFACES};
use crate::ipc::handle_sidewind_message;
use crate::render;
use crate::desktop::DesktopShell;
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
    pub log_buf: [u8; 512],
    pub log_len: usize,
    pub last_input_tick: u64,
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
            log_buf: [0; 512],
            log_len: 0,
            last_input_tick: 0,
        };

        // Pre-render background
        state.backend.fb.pre_render_background();

        // Sync pinned app count to input state for taskbar click detection
        state.input.pinned_app_count = state.desktop.pinned_count;

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
                let available = self.log_buf.len() - self.log_len;
                let copy_len = line.len().min(available);
                if copy_len > 0 {
                    self.log_buf[self.log_len..self.log_len + copy_len]
                        .copy_from_slice(&line[..copy_len]);
                    self.log_len += copy_len;
                }
                // Add newline if space
                if self.log_len < self.log_buf.len() {
                    self.log_buf[self.log_len] = b'\n';
                    self.log_len += 1;
                }
                self.dirty = true;
            }
            CompositorEvent::Wayland(_data, _pid) => {
                // Wayland message processing (placeholder for protocol handling)
                self.dirty = true;
            }
            CompositorEvent::X11(_data, _pid) => {
                // X11 message processing (placeholder)
                self.dirty = true;
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

        // Update system metrics periodically
        self.update_metrics_if_needed();

        let needs_render = self.dirty;
        self.dirty = false;
        needs_render
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
}
