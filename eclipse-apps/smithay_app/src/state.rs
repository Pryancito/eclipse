use crate::backend::Backend;
use crate::space::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, MAX_EXTERNAL_SURFACES, ShellWindow, WindowContent};
use crate::{render, compositor};
use std::prelude::v1::*;
use std::libc::{eclipse_send, ProcessInfo, SystemStats};
use sidewind_core::{SideWindEvent, SWND_EVENT_TYPE_RESIZE};
use core::convert::TryInto;
use core::default::Default;
use core::iter::Iterator;

#[derive(Clone, Copy, Default)]
pub struct ServiceInfo {
    pub name: [u8; 16],
    pub state: u32,
    pub pid: u32,
    pub restart_count: u32,
}

impl ServiceInfo {
    pub const fn new() -> Self {
        Self {
            name: [0; 16],
            state: 0,
            pid: 0,
            restart_count: 0,
        }
    }
}

/// SmithayState is the central state of the compositor.

/// It orchestrates the Backend, Space, and Input.
pub struct SmithayState {
    pub backend: Backend,
    pub space: Space,
    pub input: InputState,
    pub surfaces: [ExternalSurface; MAX_EXTERNAL_SURFACES],
    pub counter: u64,
    /// Eventos Input recibidos (para debug: si se congela el ratón, ver si este valor deja de subir).
    pub input_event_count: u64,
    pub prev_stats: Option<std::libc::SystemStats>,
    pub last_metrics_update: u64,
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub network_pid: Option<u32>,
    pub net_rx: u64,
    pub net_tx: u64,
    pub prev_net_rx: u64,
    pub prev_net_tx: u64,
    pub net_usage: f32,
    pub process_list: [ProcessInfo; 32],
    pub process_count: usize,
    pub service_list: [ServiceInfo; 32],
    pub service_count: usize,
    pub prev_process_ticks: [(u32, u64); 32],
    pub process_cpu_usage: [f32; 32],
    pub process_mem_kb: [u64; 32],
}


impl SmithayState {
    pub fn new() -> Option<Self> {
        let backend = Backend::new()?;
        let space = Space::new();
        let input = InputState::new(
            backend.fb.info.width as i32,
            backend.fb.info.height as i32,
        );
        let surfaces = [const { ExternalSurface {
            id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false
        } }; MAX_EXTERNAL_SURFACES];

        Some(Self {
            backend,
            space,
            input,
            surfaces,
            counter: 0,
            input_event_count: 0,
            prev_stats: None,
            last_metrics_update: 0,
            cpu_usage: 0.0,
            mem_usage: 0.0,
            network_pid: None,
            net_rx: 0,
            net_tx: 0,
            prev_net_rx: 0,
            prev_net_tx: 0,
            net_usage: 0.0,
            process_list: [const { ProcessInfo::new() }; 32],
            process_count: 0,
            service_list: [const { ServiceInfo::new() }; 32],
            service_count: 0,
            prev_process_ticks: [(0, 0); 32],
            process_cpu_usage: [0.0; 32],
            process_mem_kb: [0; 32],
        })

    }

    pub fn handle_ipc(&mut self) {
        // Cap events per frame at 64 to prevent the drain loop from starving the render path
        // when mouse events flood in faster than we can process them (the main cause of hangs).
        let mut events_processed = 0usize;
        while events_processed < 64 {
            match self.backend.poll_event() {
                None => break,
                Some(event) => {
                    events_processed += 1;
                    match event {
                CompositorEvent::Input(ev) => {
                    self.input_event_count += 1;
                    self.input.apply_event(
                        &ev,
                        self.backend.fb.info.width as i32,
                        self.backend.fb.info.height as i32,
                        &mut self.space.windows,
                        &mut self.space.window_count,
                        &self.surfaces,
                    );
                }
                CompositorEvent::SideWind(sw, sender_pid) => {
                    crate::ipc::handle_sidewind_message(
                        sw, 
                        sender_pid, 
                        &mut self.surfaces, 
                        &mut self.space.windows, 
                        &mut self.space.window_count, 
                        &mut self.input
                    );
                }
                CompositorEvent::NetStats(rx, tx) => {
                    self.net_rx = rx;
                    self.net_tx = tx;
                }
                CompositorEvent::ServiceInfo(data) => {
                    // SVCS (4) + Count (4) + [Name(12) + State(4) + PID(4) + Restarts(4)] * Count
                    if data.len() >= 8 && &data[0..4] == b"SVCS" {
                        let count = u32::from_le_bytes(data[4..8].try_into().unwrap_or([0; 4])) as usize;
                        let mut parsed = 0usize;
                        let mut offset = 8;
                        for i in 0..count {
                            if i >= 32 { break; }
                            if data.len() >= offset + 24 {
                                let mut svc = ServiceInfo::new();
                                svc.name[..12].copy_from_slice(&data[offset..offset+12]);
                                offset += 12;
                                svc.state = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                                offset += 4;
                                svc.pid = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                                offset += 4;
                                svc.restart_count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                                offset += 4;
                                self.service_list[i] = svc;
                                parsed += 1;
                            }
                        }
                        // Only update count after successful parsing so a truncated
                        // packet does not expose stale entries from a previous frame.
                        self.service_count = parsed;
                    }
                }
                _ => {} // Handle Wayland/X11 if needed
                    } // end match event
                } // end Some(event)
            } // end match poll_event
        } // end while
    }

    pub fn update(&mut self) {
        self.counter = self.counter.wrapping_add(1);
        self.handle_requests();
        self.space.update_animations(&mut self.surfaces);
        
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;

        let target_notif_x = if self.input.notifications_active { (fb_w - 300) as f32 } else { fb_w as f32 };
        self.input.notif_curr_x += (target_notif_x - self.input.notif_curr_x) * 0.2;

        let target_launcher_y = if self.input.launcher_active { (fb_h - 370) as f32 } else { fb_h as f32 };
        self.input.launcher_curr_y += (target_launcher_y - self.input.launcher_curr_y) * 0.2;

        let target_ws_offset = (self.input.current_workspace as f32) * (fb_w as f32);
        self.input.workspace_offset += (target_ws_offset - self.input.workspace_offset) * 0.15;

        let target_search_y = if self.input.search_active { 0.0 } else { -(fb_h as f32 / 2.0) };
        self.input.search_curr_y += (target_search_y - self.input.search_curr_y) * 0.15;

        // Métricas solo cuando hace falta (dashboard/system central) o cada 60 ticks para CPU/mem básica
        let need_metrics = self.input.dashboard_active || self.input.system_central_active;
        if need_metrics && self.counter % 15 == 0 || !need_metrics && self.counter % 60 == 0 {
            let mut current = SystemStats {
                uptime_ticks: 0, idle_ticks: 0, total_mem_frames: 0, used_mem_frames: 0
            };
            unsafe {
                if std::libc::get_system_stats(&mut current) == 0 {
                    if let Some(prev) = self.prev_stats {
                        let total_delta = current.uptime_ticks.saturating_sub(prev.uptime_ticks);
                        let idle_delta = current.idle_ticks.saturating_sub(prev.idle_ticks);
                        
                        if total_delta > 0 {
                            let busy_delta = total_delta.saturating_sub(idle_delta);
                            self.cpu_usage = (busy_delta as f32) / (total_delta as f32);
                        }
                    }
                    
                    if current.total_mem_frames > 0 {
                        self.mem_usage = (current.used_mem_frames as f32) / (current.total_mem_frames as f32);
                    }
                }
            }
            
            if need_metrics {
            if let Some(pid) = self.network_pid {
                unsafe {
                    let _ = std::libc::eclipse_send(pid as u32, 0x08, b"GET_NET_STATS_MSG".as_ptr() as *const core::ffi::c_void, 17, 0); // MSG_TYPE_NETWORK = 0x08
                }
                
                let rx_delta = self.net_rx.saturating_sub(self.prev_net_rx);
                let tx_delta = self.net_tx.saturating_sub(self.prev_net_tx);
                let total_delta = rx_delta + tx_delta;
                
                let max_bytes_per_sec = 5_000_000.0;
                let bytes_per_sec = (total_delta as f32) * 2.0;
                self.net_usage = (bytes_per_sec / max_bytes_per_sec).clamp(0.0, 1.0);
                
                self.prev_net_rx = self.net_rx;
                self.prev_net_tx = self.net_tx;
            }
            }
            
            if self.input.system_central_active {
                let prev_uptime = self.prev_stats.map(|s| s.uptime_ticks).unwrap_or(0);
                let count = unsafe { std::libc::get_process_list(self.process_list.as_mut_ptr(), 32) };
                if count >= 0 {
                    self.process_count = count as usize;
                    
                    let current_uptime = current.uptime_ticks;
                    let total_delta = current_uptime.saturating_sub(prev_uptime);

                    // Evict tick entries whose PID no longer appears in the active list.
                    // Use the process_list slice directly to avoid a separate copy.
                    let active = &self.process_list[..self.process_count];
                    for j in 0..32 {
                        let stored_pid = self.prev_process_ticks[j].0;
                        if stored_pid != 0 && !active.iter().any(|p| p.pid == stored_pid) {
                            self.prev_process_ticks[j] = (0, 0);
                        }
                    }

                    for i in 0..self.process_count {
                        let p = &self.process_list[i];
                        
                        // Calcular CPU %
                        let mut prev_ticks = 0;
                        for j in 0..32 {
                            if self.prev_process_ticks[j].0 == p.pid {
                                prev_ticks = self.prev_process_ticks[j].1;
                                break;
                            }
                        }
                        
                        if total_delta > 0 {
                            let delta_ticks = p.cpu_ticks.saturating_sub(prev_ticks);
                            self.process_cpu_usage[i] = (delta_ticks as f32 / total_delta as f32) * 100.0;
                        } else {
                            self.process_cpu_usage[i] = 0.0;
                        }
                        
                        // Calcular Memoria (KB) - p.mem_frames son páginas de 4KB
                        self.process_mem_kb[i] = p.mem_frames * 4;

                        // Actualizar histórico de ticks.
                        let mut found = false;
                        for j in 0..32 {
                            if self.prev_process_ticks[j].0 == p.pid {
                                self.prev_process_ticks[j].1 = p.cpu_ticks;
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            // Buscar slot vacío (PID 0)
                            for j in 0..32 {
                                if self.prev_process_ticks[j].0 == 0 {
                                    self.prev_process_ticks[j] = (p.pid, p.cpu_ticks);
                                    break;
                                }
                            }
                        }
                    }
                }
                
                // Pedir info de servicios a systemd (PID 1)
                unsafe {
                    let _ = std::libc::eclipse_send(1, 0, b"GET_SERVICES_INFO\0".as_ptr() as *const core::ffi::c_void, 18, 0);
                }
            }

            // Actualizar prev_stats AL FINAL para no invalidar el delta de procesos
            self.prev_stats = Some(current);
        }
    }


    fn handle_requests(&mut self) {
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;

        // Create new window
        if self.input.request_new_window && self.space.window_count < crate::compositor::MAX_WINDOWS_COUNT {
            let idx = self.space.window_count;
            let win = crate::compositor::ShellWindow {
                x: 60 + (idx as i32) * 20,
                y: 160 + (idx as i32) * 15,
                w: 600,
                h: 380,
                curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, closing: false,
                stored_rect: (0, 0, 0, 0),
                workspace: self.input.current_workspace,
                content: crate::compositor::WindowContent::InternalDemo,
            };
            self.space.map_window(win);
            self.input.focused_window = Some(idx);
            self.input.request_new_window = false;
        } else if self.input.request_new_window {
            self.input.request_new_window = false;
        }

        // Close window
        if self.input.request_close_window {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count {
                    self.space.windows[idx].closing = true;
                }
            }
            self.input.focused_window = None;
            self.input.dragging_window = None;
            self.input.request_close_window = false;
        }

        // Minimize
        if self.input.request_minimize {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count && !self.space.windows[idx].minimized {
                    self.space.windows[idx].minimized = true;
                    self.input.focused_window = None;
                    self.input.dragging_window = None;
                }
            }
            self.input.request_minimize = false;
        }

        // Maximize
        if self.input.request_maximize {
            if let Some(idx) = self.input.focused_window {
                if idx < self.space.window_count {
                    let win = &mut self.space.windows[idx];
                    if win.maximized {
                        let (x, y, w, h) = win.stored_rect;
                        win.x = x; win.y = y; win.w = w; win.h = h;
                        win.maximized = false;
                    } else {
                        win.stored_rect = (win.x, win.y, win.w, win.h);
                        win.x = 0; win.y = 0;
                        win.w = fb_w;
                        win.h = fb_h - 45;
                        win.maximized = true;
                    }
                    // Notify client if external
                    if let crate::compositor::WindowContent::External(s_idx) = win.content {
                        if (s_idx as usize) < self.surfaces.len() {
                            let pid = self.surfaces[s_idx as usize].pid;
                            let se = SideWindEvent { 
                                event_type: SWND_EVENT_TYPE_RESIZE, 
                                data1: win.w, data2: win.h - ShellWindow::TITLE_H, data3: 0 
                            };
                            let _ = unsafe { eclipse_send(pid, 0x00000040, &se as *const _ as *const core::ffi::c_void, core::mem::size_of::<SideWindEvent>(), 0) };
                        }
                    }
                }
            }
            self.input.request_maximize = false;
        }

        // Restore
        if self.input.request_restore {
            if let Some(idx) = (0..self.space.window_count)
                .rev()
                .find(|&i| {
                    !matches!(self.space.windows[i].content, crate::compositor::WindowContent::None)
                        && self.space.windows[i].minimized
                })
            {
                self.space.windows[idx].minimized = false;
                self.space.raise_window(idx);
                self.input.focused_window = Some(self.space.window_count - 1);
            }
            self.input.request_restore = false;
        }

        // Cycle Focus
        if self.input.request_cycle_forward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(0);
                if let Some(next) = crate::compositor::next_visible(current, true, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(next);
                    self.input.focused_window = Some(self.space.window_count - 1);
                }
            }
            self.input.request_cycle_forward = false;
        }
        if self.input.request_cycle_backward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(self.space.window_count - 1);
                if let Some(prev) = crate::compositor::next_visible(current, false, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(prev);
                    self.input.focused_window = Some(self.space.window_count - 1);
                }
            }
            self.input.request_cycle_backward = false;
        }

        // Dashboard
        if self.input.request_dashboard {
            self.input.dashboard_active = !self.input.dashboard_active;
            self.input.request_dashboard = false;
        }

        // Center Cursor
        if self.input.request_center_cursor {
            self.input.cursor_x = fb_w / 2;
            self.input.cursor_y = fb_h / 2;
            self.input.request_center_cursor = false;
        }
    }

    pub fn render(&mut self) {
        if !self.input.lock_active {
            render::draw_static_ui(
                &mut self.backend.fb, 
                &self.space.windows, 
                self.space.window_count, 
                self.counter, 
                self.input.cursor_x, 
                self.input.cursor_y
            );
            
            // Prototype hardware acceleration
            render::gpu_test_render(&self.backend.fb, self.counter);

            if !self.input.dashboard_active && !self.input.system_central_active {
                render::draw_shell_windows(
                    &mut self.backend.fb, 
                    &self.space.windows, 
                    self.space.window_count, 
                    self.input.focused_window, 
                    &self.surfaces, 
                    self.input.workspace_offset, 
                    self.input.current_workspace,
                    self.input.cursor_x, 
                    self.input.cursor_y,
                    self.prev_stats.map(|s| s.uptime_ticks).unwrap_or(0)
                );
            } else if self.input.dashboard_active {
                render::draw_dashboard(&mut self.backend.fb, self.counter, self.cpu_usage, self.mem_usage, self.net_usage, self.prev_stats.map(|s| s.uptime_ticks).unwrap_or(0));
            } else if self.input.system_central_active {
                render::draw_system_central(
                    &mut self.backend.fb, 
                    self.counter, 
                    &self.service_list[..self.service_count], 
                    &self.process_list[..self.process_count],
                    &self.process_cpu_usage,
                    &self.process_mem_kb,
                    self.prev_stats.map(|s| s.uptime_ticks).unwrap_or(0)
                );
            }


            if self.input.quick_settings_active { render::draw_quick_settings(&mut self.backend.fb); }
            if self.input.context_menu_active { render::draw_context_menu(&mut self.backend.fb, self.input.context_menu_pos); }
            
            render::draw_launcher(&mut self.backend.fb, self.input.launcher_curr_y);
            render::draw_notifications(&mut self.backend.fb, &self.input.notifications, self.input.notif_curr_x);
            
            if self.input.alt_tab_active { 
                render::draw_alt_tab_hud(&mut self.backend.fb, &self.space.windows, self.space.window_count, self.input.focused_window); 
            }
            
            if self.input.search_active || self.input.search_curr_y > -(self.backend.fb.info.height as f32 / 2.0) + 5.0 {
                render::draw_search_hud(
                    &mut self.backend.fb, 
                    &self.input.search_query, 
                    self.input.search_selected_idx, 
                    self.counter, 
                    self.input.search_curr_y
                );
            }

            // Desktop "Stroke" drawing
            if self.input.mouse_buttons & 1 != 0 && self.input.dragging_window.is_none() {
                render::draw_stroke(&mut self.backend.fb, self.input.cursor_x, self.input.cursor_y, self.input.stroke_color);
            }
        } else {
            render::draw_lock_screen(&mut self.backend.fb, self.counter);
        }

        render::draw_cursor(&mut self.backend.fb, embedded_graphics::prelude::Point::new(self.input.cursor_x, self.input.cursor_y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::{ShellWindow, WindowContent};

    #[test]
    fn test_state_init() {
        let state = SmithayState::new();
        assert!(state.is_some());
        let s = state.unwrap();
        assert_eq!(s.backend.fb.info.width, 1024);
    }

    #[test]
    fn test_maximize_request() {
        let mut state = SmithayState::new().unwrap();
        
        // Setup a window
        state.space.map_window(ShellWindow {
            x: 100, y: 100, w: 400, h: 300,
            curr_x: 100.0, curr_y: 100.0, curr_w: 400.0, curr_h: 300.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (100, 100, 400, 300),
            workspace: 0, content: WindowContent::InternalDemo,
        });
        
        state.input.focused_window = Some(0);
        state.input.request_maximize = true;
        
        state.update(); // calls handle_requests
        
        let win = &state.space.windows[0];
        assert!(win.maximized);
        assert_eq!(win.x, 0);
        assert_eq!(win.y, 0);
        assert_eq!(win.w, 1024);
        assert_eq!(win.h, 768 - 45); // h - 45 as per code
        
        // Restore
        state.input.request_maximize = true;
        state.update();
        let win = &state.space.windows[0];
        assert!(!win.maximized);
        assert_eq!(win.x, 100);
        assert_eq!(win.w, 400);
    }

    #[test]
    fn test_minimize_request() {
        let mut state = SmithayState::new().unwrap();
        state.space.map_window(ShellWindow {
            x: 100, y: 100, w: 400, h: 300,
            curr_x: 100.0, curr_y: 100.0, curr_w: 400.0, curr_h: 300.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (100, 100, 400, 300),
            workspace: 0, content: WindowContent::InternalDemo,
        });
        
        state.input.focused_window = Some(0);
        state.input.request_minimize = true;
        state.update();
        
        assert!(state.space.windows[0].minimized);
        assert_eq!(state.input.focused_window, None);
    }
}
