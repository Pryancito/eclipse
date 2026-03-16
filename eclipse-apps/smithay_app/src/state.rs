use crate::backend::Backend;
use crate::compositor::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, ShellWindow, MAX_EXTERNAL_SURFACES};
use crate::ipc::handle_sidewind_message;
use crate::render;
use std::prelude::v1::*;
use core::matches;
#[cfg(not(target_os = "linux"))]
use libc::{eclipse_send, ProcessInfo, SystemStats, get_system_stats, get_process_list};
#[cfg(target_os = "linux")]
use eclipse_syscall::{ProcessInfo, SystemStats};
use sidewind::{SideWindEvent, SWND_EVENT_TYPE_RESIZE};
use core::convert::TryInto;
use core::default::Default;
use core::iter::Iterator;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::geometry::{Point, Size};
use eclipse_ipc::types::TAG_WAYL;

#[cfg(target_os = "linux")]
unsafe fn eclipse_send(_dest: u32, _msg_type: u32, _buf: *const core::ffi::c_void, _len: usize, _flags: usize) -> usize { 0 }
#[cfg(target_os = "linux")]
fn get_system_stats(_stats: &mut SystemStats) -> i32 { 0 }
#[cfg(target_os = "linux")]
fn get_process_list(_buf: *mut ProcessInfo, _max: usize) -> usize { 0 }

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
#[derive(Debug, Clone, Copy)]
pub struct WaylandPoolMap {
    pub conn_idx: usize,
    pub pool_id: u32,
    pub vaddr: usize,
    pub size: usize,
}

pub struct SmithayState {
    pub backend: Backend,
    pub space: Space,
    pub input: InputState,
    pub surfaces: [ExternalSurface; MAX_EXTERNAL_SURFACES],
    pub counter: u64,
    /// Eventos Input recibidos (para debug: si se congela el ratón, ver si este valor deja de subir).
    pub input_event_count: u64,
    pub prev_stats: Option<SystemStats>,
    pub last_metrics_update: std::time::Instant,
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
    pub dirty: bool,
    /// Buffer para logs del kernel (evita static mut en draw_static_ui).
    pub log_buf: [u8; 512],
    pub log_len: usize,
    pub wayland_connections: [Option<sidewind::wayland::WaylandConnection>; 32],
    pub wayland_pool_maps: Vec<WaylandPoolMap>,
}

impl SmithayState {
    /// Notifica a clientes externos (SideWind) el nuevo tamaño tras layout/tiling.
    fn notify_external_resize(&self) {
        for i in 0..self.space.window_count {
            if let crate::compositor::WindowContent::External(s_idx) = self.space.windows[i].content {
                if (s_idx as usize) < self.surfaces.len() {
                    let pid = self.surfaces[s_idx as usize].pid;
                    let win = &self.space.windows[i];
                    let se = SideWindEvent {
                        event_type: SWND_EVENT_TYPE_RESIZE,
                        data1: win.w,
                        data2: win.h.saturating_sub(ShellWindow::TITLE_H),
                        data3: 0,
                    };
                    let _ = unsafe {
                        eclipse_send(
                            pid,
                            0x00000040,
                            &se as *const _ as *const core::ffi::c_void,
                            core::mem::size_of::<SideWindEvent>(),
                            0,
                        )
                    };
                }
            }
        }
    }


    pub fn new() -> Option<Self> {
        let mut backend = Backend::new()?;
        // Render the static cosmic background once into the background buffer so that
        // blit_background / blit_background_damaged have valid content to copy from.
        backend.fb.pre_render_background();
        let space = Space::new();
        let input = InputState::new(
            backend.fb.info.width as i32,
            backend.fb.info.height as i32,
        );
        let surfaces = [const { ExternalSurface {
            id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false, ready_to_flip: false
        } }; MAX_EXTERNAL_SURFACES];

        Some(Self {
            backend,
            space,
            input,
            surfaces: [ExternalSurface::default(); MAX_EXTERNAL_SURFACES],
            counter: 0,
            input_event_count: 0,
            prev_stats: None,
            last_metrics_update: std::time::Instant::now(),
            cpu_usage: 0.0,
            mem_usage: 0.0,
            network_pid: None,
            net_rx: 0,
            net_tx: 0,
            prev_net_rx: 0,
            prev_net_tx: 0,
            net_usage: 0.0,
            process_list: [ProcessInfo { pid: 0, name: [0; 16], state: 0, cpu_ticks: 0, mem_frames: 0 }; 32],
            process_count: 0,
            service_list: [ServiceInfo::new(); 32],
            service_count: 0,
            prev_process_ticks: [(0, 0); 32],
            process_cpu_usage: [0.0; 32],
            process_mem_kb: [0; 32],
            dirty: true,
            log_buf: [0; 512],
            log_len: 0,
            wayland_connections: [const { None }; 32],
            wayland_pool_maps: Vec::new(),
        })

    }

    pub fn handle_event(&mut self, event: &CompositorEvent) {
        match event {
            CompositorEvent::Input(ev) => {
                self.input_event_count += 1;
                self.input.apply_event(
                    ev,
                    self.backend.fb.info.width as i32,
                    self.backend.fb.info.height as i32,
                    &mut self.space.windows,
                    &mut self.space.window_count,
                    &self.surfaces,
                );
                
                // If we are dragging a window or cursor moved, full redraw
                // For now, if anything but move happened, we might need more.
                // Move is the most common.
                self.dirty = true;
            }
            CompositorEvent::SideWind(sw, sender_pid) => {
                let fb_w = self.backend.fb.info.width as i32;
                let fb_h = self.backend.fb.info.height as i32;
                handle_sidewind_message(
                    sw, 
                    *sender_pid, 
                    &mut self.surfaces, 
                    &mut self.space.windows, 
                    &mut self.space.window_count, 
                    &mut self.input,
                    fb_w,
                    fb_h,
                );
                self.dirty = true;
            }
            CompositorEvent::NetStats(rx, tx) => {
                self.net_rx = *rx;
                self.net_tx = *tx;
                self.dirty = true;
            }
            CompositorEvent::KernelLog(line) => {
                // Store log line directly in the buffer for the HUD
                let line_bytes = &line[..line.len()];
                let new_len = line_bytes.len();
                
                if self.log_len + new_len + 1 > 512 {
                    // Primitive: clear and restart if full to avoid complex shifting for now
                    self.log_len = 0;
                }
                
                let start = self.log_len;
                self.log_buf[start..start + new_len].copy_from_slice(line_bytes);
                self.log_len += new_len;
                self.log_buf[self.log_len] = b'\n';
                self.log_len += 1;
                
                self.dirty = true;
            }
            CompositorEvent::ServiceInfo(data) => {
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
                    self.service_count = parsed;
                }
                self.dirty = true;
            }
            CompositorEvent::Wayland(data, sender_pid) => {
                // Find or create connection for this PID
                let conn_idx = self.wayland_connections.iter().position(|c| {
                    if let Some(_conn) = c {
                        // We need a way to associate connection with PID.
                        // For now, let's assume one client per PID and use a simple search or index.
                        // Actually, wayland_connections doesn't store the PID yet.
                        // Let's use a simpler mapping for now.
                        false // TODO: Fix mapping
                    } else { false }
                });

                // Since we don't have a good mapping yet, let's just use the first available or a hash.
                let idx = (*sender_pid as usize) % 32;
                if self.wayland_connections[idx].is_none() {
                    self.wayland_connections[idx] = Some(sidewind::wayland::WaylandConnection::new());
                }

                if let Some(conn) = self.wayland_connections[idx].as_mut() {
                    let _ = conn.process_message(&data);
                }
                self.process_wayland_events(idx);
                
                // Process any pending events and send them back
                if let Some(conn) = self.wayland_connections[idx].as_mut() {
                    while let Some(msg_payload) = conn.pending_events.pop_front() {
                        let mut tagged_msg = [0u8; 256];
                        tagged_msg[0..4].copy_from_slice(TAG_WAYL);
                        let p_len = msg_payload.len().min(252);
                        tagged_msg[4..4+p_len].copy_from_slice(&msg_payload[..p_len]);
                        let _ = unsafe { eclipse_send(*sender_pid, sidewind::MSG_TYPE_WAYLAND, tagged_msg.as_ptr() as *const core::ffi::c_void, 4 + p_len, 0) };
                    }
                }
                self.dirty = true;
            }
            _ => {} // Handle X11 if needed
        }
    }

    pub fn handle_ipc(&mut self) {
        let mut events_processed = 0usize;
        while events_processed < 64 {
            match self.backend.poll_event() {
                None => break,
                Some(event) => {
                    events_processed += 1;
                    self.handle_event(&event);
                }
            }
        }
    }

    fn process_wayland_events(&mut self, conn_idx: usize) {
        loop {
            let ev = if let Some(conn) = self.wayland_connections[conn_idx].as_mut() {
                conn.internal_events.pop_front()
            } else {
                None
            };
            
            let ev = if let Some(ev) = ev { ev } else { break };

            match ev {
                sidewind::wayland::WaylandInternalEvent::ShellSurfaceCreated { surface_id, .. } => {
                    if self.space.window_count < crate::compositor::MAX_WINDOWS_COUNT {
                        let win_idx = self.space.window_count;
                        let win = crate::compositor::ShellWindow {
                            x: 100 + (win_idx as i32) * 20,
                            y: 100 + (win_idx as i32) * 20,
                            w: 640,
                            h: 480 + crate::compositor::ShellWindow::TITLE_H,
                            workspace: self.input.current_workspace,
                            content: crate::compositor::WindowContent::Wayland { surface_id, conn_idx },
                            ..Default::default()
                        };
                        self.space.map_window(win);
                        self.input.focused_window = Some(win_idx);
                        self.dirty = true;
                    }
                }
                sidewind::wayland::WaylandInternalEvent::SurfaceCommitted { surface_id, buffer_id, damage } => {
                    // Ensure the buffer's pool is mapped
                    if let Some(buf_id) = buffer_id {
                        self.ensure_wayland_pool_mapped(conn_idx, buf_id);
                    }
                    // Find the window for this surface and append damage
                    for i in 0..self.space.window_count {
                        let w = &mut self.space.windows[i];
                        if let crate::compositor::WindowContent::Wayland { surface_id: s_id, conn_idx: c_idx } = w.content {
                            if s_id == surface_id && c_idx == conn_idx {
                                w.damage.extend(damage);
                                break;
                            }
                        }
                    }
                    // Trigger repaint
                    self.dirty = true;
                }
            }
        }
    }

    fn ensure_wayland_pool_mapped(&mut self, conn_idx: usize, buffer_id: u32) {
        let info = {
            let conn = if let Some(c) = &mut self.wayland_connections[conn_idx] { c } else { return };
            if let Some(obj) = conn.registry.get_mut(buffer_id) {
                obj.as_buffer()
            } else { None }
        };
        
        let info = if let Some(i) = info { i } else { return };
        
        if self.wayland_pool_maps.iter().any(|m| m.conn_idx == conn_idx && m.pool_id == info.pool_id) {
            return;
        }

        let mut vaddr = 0usize;
        let mut size = 0usize;

        #[cfg(not(target_os = "linux"))]
        {
            // On Eclipse OS, we use the shm_pool_fd as a handle to open the shm region.
            // In our prototype, the client passes an ID that we use as a filename in shm: scheme.
            let path = format!("shm:/{}", info.shm_pool_fd);
            if let Ok(fd) = eclipse_syscall::open(&path, eclipse_syscall::flag::O_RDWR) {
                let mut st = eclipse_syscall::Stat::default();
                if let Ok(()) = eclipse_syscall::fstat(fd, &mut st) {
                    size = st.size as usize;
                    if let Ok(mapped) = eclipse_syscall::mmap(
                        0, 
                        size, 
                        eclipse_syscall::flag::PROT_READ | eclipse_syscall::flag::PROT_WRITE, 
                        eclipse_syscall::flag::MAP_SHARED, 
                        fd as isize, 
                        0
                    ) {
                        vaddr = mapped;
                    }
                }
                let _ = eclipse_syscall::close(fd);
            }
        }

        if vaddr != 0 {
            self.wayland_pool_maps.push(WaylandPoolMap {
                conn_idx,
                pool_id: info.pool_id,
                vaddr,
                size,
            });
        }
    }

    pub fn update(&mut self) -> bool {
        self.counter = self.counter.wrapping_add(1);
        self.handle_requests();
        let window_count_before = self.space.window_count;
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;
        let busy_animations = self.update_animations_and_layout(fb_w, fb_h, window_count_before);
        let busy_metrics = self.update_metrics_if_needed();
        busy_animations || busy_metrics || self.dirty
    }

    /// Actualiza animaciones (ventanas, overlays, logo/sidebar/HUD) y devuelve
    /// true si el frame debe considerarse "busy" (necesita seguir avanzando).
    fn update_animations_and_layout(&mut self, fb_w: i32, fb_h: i32, window_count_before: usize) -> bool {
        // Optimize animation tracking by using a bitmask of changed windows
        let animating_mask = self.space.update_animations(&mut self.surfaces);
        let mut busy = animating_mask != 0;

        // Re-apply tiled layout if a window was closed
        if self.input.tiling_active && self.space.window_count < window_count_before {
            self.input.focused_window = if self.space.window_count > 0 {
                Some(self.space.window_count - 1)
            } else {
                None
            };
            self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
            self.notify_external_resize();
        }

        // Handle global busy states (workspace shifts, etc.) - dirty ya marcado si busy
        const EPSILON: f32 = 0.5;


        let target_launcher_y = if self.input.launcher_active { (fb_h - 370) as f32 } else { fb_h as f32 };
        let diff_launcher = target_launcher_y - self.input.launcher_curr_y;
        if diff_launcher.abs() > EPSILON {
            self.input.launcher_curr_y += diff_launcher * 0.2;
            busy = true;
        } else {
            self.input.launcher_curr_y = target_launcher_y;
        }

        let target_ws_offset = (self.input.current_workspace as f32) * (fb_w as f32);
        let diff_ws = target_ws_offset - self.input.workspace_offset;
        if diff_ws.abs() > EPSILON {
            self.input.workspace_offset += diff_ws * 0.15;
            busy = true;
        } else {
            self.input.workspace_offset = target_ws_offset;
        }

        let target_search_y = if self.input.search_active { 0.0 } else { -(fb_h as f32 / 2.0) };
        let diff_search = target_search_y - self.input.search_curr_y;
        if diff_search.abs() > EPSILON {
            self.input.search_curr_y += diff_search * 0.15;
            busy = true;
        } else {
            self.input.search_curr_y = target_search_y;
        }

        // Animaciones de logo/sidebar/HUD: marcar daño solo cada N frames para reducir
        // trabajo cuando el sistema está idle (especialmente bajo TCG en QEMU).
        if !self.input.dashboard_active && !self.input.system_central_active && !self.input.lock_active {
            if self.counter % 3 == 0 {
                // Logo: draw_eclipse_logo renders rings out to ~280 px radius; use 300 px margin.
                // Damage tracking removed: self.damage_rect(...) calls here removed.
                busy = true;
            }
        } else if self.input.lock_active {
            // Lock screen is fully animated (logo + clock); damage tracking removed.
            busy = true;
        }

        busy
    }

    /// Actualiza métricas de sistema/procesos cuando corresponde; devuelve true si se ha
    /// actualizado algo que debería disparar un render (dirty).
    fn update_metrics_if_needed(&mut self) -> bool {
        let now = std::time::Instant::now();
        let metrics_elapsed = self.last_metrics_update.elapsed();
        let need_metrics = self.input.dashboard_active || self.input.system_central_active;
        let metrics_interval = if need_metrics { 500u64 } else { 3000u64 };

        if metrics_elapsed.as_millis() as u64 >= metrics_interval {
            self.last_metrics_update = now;
            let mut current = SystemStats {
                uptime_ticks: 0, idle_ticks: 0, total_mem_frames: 0, used_mem_frames: 0
            };
            unsafe {
                if get_system_stats(&mut current) == 0 {
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
                    let _ = unsafe { eclipse_send(pid as u32, 0x08, b"GET_NET_STATS_MSG".as_ptr() as *const core::ffi::c_void, 17, 0) }; // MSG_TYPE_NETWORK = 0x08

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
                let count = unsafe { get_process_list(self.process_list.as_mut_ptr(), 32) };
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
                let _ = unsafe { eclipse_send(1, 0, b"GET_SERVICES_INFO\0".as_ptr() as *const core::ffi::c_void, 18, 0) };
            }

            // Actualizar prev_stats AL FINAL para no invalidar el delta de procesos
            self.prev_stats = Some(current);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    #[inline(never)]
    fn handle_requests(&mut self) {
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;

        // Toggle tiling (cosmic-comp style master+stack)
        if self.input.request_toggle_tiling {
            self.input.tiling_active = !self.input.tiling_active;
            self.input.request_toggle_tiling = false;
            if self.input.tiling_active {
                self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
                self.notify_external_resize();
            }
            self.dirty = true;
        }

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
                damage: alloc::vec::Vec::new(),
            };
            self.space.map_window(win);
            self.input.focused_window = Some(idx);
            self.input.request_new_window = false;
            if self.input.tiling_active {
                self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
                self.notify_external_resize();
            }
            self.dirty = true;
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
            self.dirty = true;
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
            self.dirty = true;
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
            self.dirty = true;
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
            self.dirty = true;
        }

        // Cycle Focus
        if self.input.request_cycle_forward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(0);
                if let Some(next) = crate::compositor::next_visible(current, true, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(next);
                    self.input.focused_window = Some(self.space.window_count - 1);
                    if self.input.tiling_active {
                        self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
                        self.notify_external_resize();
                    }
                }
            }
            self.input.request_cycle_forward = false;
            self.dirty = true;
        }
        if self.input.request_cycle_backward {
            if self.space.window_count > 1 {
                let current = self.input.focused_window.unwrap_or(self.space.window_count - 1);
                if let Some(prev) = crate::compositor::next_visible(current, false, &self.space.windows, self.space.window_count) {
                    self.space.raise_window(prev);
                    self.input.focused_window = Some(self.space.window_count - 1);
                    if self.input.tiling_active {
                        self.space.apply_tiled_layout(fb_w, fb_h, self.input.focused_window);
                        self.notify_external_resize();
                    }
                }
            }
            self.input.request_cycle_backward = false;
            self.dirty = true;
        }

        // Dashboard / System Central: damage full screen al abrir overlay
        if self.input.request_dashboard {
            self.input.dashboard_active = !self.input.dashboard_active;
            self.input.request_dashboard = false;
            if self.input.dashboard_active {
            }
            self.dirty = true;
        }

        // Center Cursor
        if self.input.request_center_cursor {
            self.input.cursor_x = fb_w / 2;
            self.input.cursor_y = fb_h / 2;
            self.input.request_center_cursor = false;
        }
    }

    #[inline(never)]
    pub fn render(&mut self) {
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;
        let full_rect = Rectangle::new(Point::new(0, 0), Size::new(fb_w as u32, fb_h as u32));

        if !self.input.lock_active {
            render::draw_desktop_shell(
                &mut self.backend.fb, 
                &self.space.windows, 
                self.space.window_count, 
                self.counter, 
                self.input.cursor_x, 
                self.input.cursor_y,
                &mut self.log_buf,
                &mut self.log_len,
            );

            if !self.input.dashboard_active && !self.input.system_central_active {
                render::draw_shell_windows(
                    &mut self.backend.fb, 
                    &self.space.windows, 
                    self.space.window_count, 
                    self.input.focused_window, 
                    &self.surfaces,
                    &self.wayland_connections,
                    &self.wayland_pool_maps,
                    self.input.workspace_offset, 
                    self.input.current_workspace,
                    self.input.cursor_x, 
                    self.input.cursor_y, 
                    self.counter,
                );
                // Clear damage after drawing
                for i in 0..self.space.window_count {
                    self.space.windows[i].damage.clear();
                }
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
        } else {
            render::draw_lock_screen(&mut self.backend.fb, self.counter);
        }

        render::draw_cursor(&mut self.backend.fb, embedded_graphics::prelude::Point::new(self.input.cursor_x, self.input.cursor_y));

        self.backend.fb.present();

        self.dirty = false;
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
            damage: alloc::vec::Vec::new(),
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
            damage: alloc::vec::Vec::new(),
        });
        
        state.input.focused_window = Some(0);
        state.input.request_minimize = true;
        state.update();
        
        assert!(state.space.windows[0].minimized);
        assert_eq!(state.input.focused_window, None);
    }

    #[test]
    fn test_close_request() {
        let mut state = SmithayState::new().unwrap();
        state.space.map_window(ShellWindow {
            x: 50, y: 50, w: 200, h: 150,
            curr_x: 50.0, curr_y: 50.0, curr_w: 200.0, curr_h: 150.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (50, 50, 200, 150), workspace: 0,
            content: WindowContent::InternalDemo,
            damage: alloc::vec::Vec::new(),
        });
        state.input.focused_window = Some(0);
        state.input.request_close_window = true;
        state.update();
        assert!(state.space.windows[0].closing);
    }

    #[test]
    fn test_service_info_new() {
        let info = ServiceInfo::new();
        assert_eq!(info.state, 0);
        assert_eq!(info.pid, 0);
    }

    #[test]
    fn test_wayland_surface_commit_and_damage() {
        let mut state = SmithayState::new().unwrap();
        let surface_id = 42;
        let conn_idx = 0;
        
        // 1. Map a Wayland window
        state.space.map_window(ShellWindow {
            content: WindowContent::Wayland { surface_id, conn_idx },
            ..Default::default()
        });
        
        // 2. Mock a connection
        state.wayland_connections[conn_idx] = Some(sidewind::wayland::WaylandConnection::new());
        state.dirty = false;

        // 3. Simulate SurfaceCommitted with damage
        let damage = vec![(10, 10, 100, 100)];
        let ev = sidewind::wayland::WaylandInternalEvent::SurfaceCommitted {
            surface_id,
            buffer_id: None,
            damage: damage.clone(),
        };
        state.wayland_connections[conn_idx].as_mut().unwrap().internal_events.push_back(ev);
        
        // 4. Process events
        state.process_wayland_events(conn_idx);
        
        // 5. Verify damage propagation
        assert!(state.dirty, "state should be dirty after commit");
        assert_eq!(state.space.windows[0].damage.len(), 1);
        assert_eq!(state.space.windows[0].damage[0], (10, 10, 100, 100));
    }
}
