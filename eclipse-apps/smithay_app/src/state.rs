use crate::backend::Backend;
use crate::compositor::Space;
use crate::input::{InputState, CompositorEvent};
use crate::compositor::{ExternalSurface, ShellWindow, WindowContent, MAX_EXTERNAL_SURFACES};
use crate::ipc::handle_sidewind_message;
use crate::render;
use std::prelude::v1::*;
use core::matches;
#[cfg(target_os = "eclipse")]
use libc::{eclipse_send, write, ProcessInfo, SystemStats, get_system_stats, get_process_list};
#[cfg(not(target_os = "eclipse"))]
use eclipse_syscall::{ProcessInfo, SystemStats};
use sidewind::{SideWindEvent, SWND_EVENT_TYPE_RESIZE};
use core::convert::TryInto;
use core::default::Default;
use core::iter::Iterator;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::geometry::{Point, Size};
use wayland_proto::wl::server::server::WaylandServer;
use wayland_proto::wl::server::objects::{Object, ObjectInner, ObjectLogic, ServerError};
use wayland_proto::wl::protocols::common::{wl_pointer, wl_keyboard, xdg_surface, xdg_toplevel, xdg_wm_base, wl_compositor, wl_shm, wl_seat};
use wayland_proto::wl::protocols::common::*;
use wayland_proto::EclipseWaylandConnection;
use crate::wayland_socket::WaylandSocketServer;
use crate::xwayland::{X11Server, X11Action};
use crate::protocol::{SharedCommits, SharedBuffers, SharedKeyboards, SharedPointers, SharedXwaylandSerials};
use crate::protocol::{AppCompositor, AppShm, AppSeat, AppXdgWmBase};
use eclipse_ipc::types::{NetExtendedStats, TAG_WAYL};
use std::rc::Rc;
use core::cell::RefCell;
use std::collections::BTreeMap;

#[cfg(not(target_os = "eclipse"))]
unsafe fn eclipse_send(_dest: u32, _msg_type: u32, _buf: *const core::ffi::c_void, _len: usize, _flags: usize) -> usize { 0 }
#[cfg(not(target_os = "eclipse"))]
fn get_system_stats(_stats: &mut SystemStats) -> i32 { 0 }
#[cfg(not(target_os = "eclipse"))]
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
    pub prev_process_ticks: [(u32, u64); 32],
    pub process_cpu_usage: [f32; 32],
    pub process_mem_kb: [u64; 32],
    pub dirty: bool,
    /// Buffer para logs del kernel (evita static mut en draw_static_ui).
    pub log_buf: [u8; 512],
    pub log_len: usize,
    /// Valor de `counter` la última vez que se procesó un evento Input.
    /// Sirve de watchdog: si el main loop sigue avanzando pero este valor no cambia,
    /// los eventos de ratón han dejado de llegar (IPC muerto o input_service bloqueado).
    pub last_input_tick: u64,
    #[cfg(any(not(target_os = "linux"), test))]
    pub wayland_connections: [Option<EclipseWaylandConnection>; 32],
    #[cfg(any(not(target_os = "linux"), test))]
    pub wayland_pool_maps: Vec<WaylandPoolMap>,
    /// Última vez que se recibió un mensaje IPC (para el heartbeat).
    pub last_ipc_activity: std::time::Instant,
    pub style_engine: crate::style_engine::StyleEngine,
    pub dashboard_view: Option<std::boxed::Box<dyn crate::stylus::Widget>>,

    // --- New Wayland/XWayland fields ---
    pub wayland_server: WaylandServer,
    pub wayland_socket: Option<WaylandSocketServer>,
    pub x11_server: Option<X11Server>,
    pub shared_commits: SharedCommits,
    pub shared_buffers: SharedBuffers,
    pub kb_registry: SharedKeyboards,
    pub ptr_registry: SharedPointers,
    pub x11_serials: SharedXwaylandSerials,
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


    pub fn new() -> Option<Box<Self>> {
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
        
        let style_engine = crate::style_engine::StyleEngine::new();

        let mut state = Box::new(Self {
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
            last_input_tick: 0,
            #[cfg(any(not(target_os = "linux"), test))]
            wayland_connections: [const { None }; 32],
            #[cfg(any(not(target_os = "linux"), test))]
            wayland_pool_maps: Vec::new(),
            last_ipc_activity: std::time::Instant::now(),
            style_engine,
            dashboard_view: None,

            wayland_server: WaylandServer::new(),
            wayland_socket: None, // initialized below
            x11_server: None,     // initialized below
            shared_commits: Rc::new(RefCell::new(Vec::new())),
            shared_buffers: Rc::new(RefCell::new(BTreeMap::new())),
            kb_registry: Rc::new(RefCell::new(BTreeMap::new())),
            ptr_registry: Rc::new(RefCell::new(BTreeMap::new())),
            x11_serials: Rc::new(RefCell::new(BTreeMap::new())),
        });

        // Initialize Wayland Standard Socket (/tmp/wayland-0)
        state.wayland_socket = WaylandSocketServer::new("/tmp/wayland-0");

        // Initialize X11 Server (:0)
        state.x11_server = X11Server::new(state.backend.fb.info.width as u16, state.backend.fb.info.height as u16);

        // Register Global Interfaces
        let sc = state.shared_commits.clone();
        let sb = state.shared_buffers.clone();
        let sc2 = state.shared_commits.clone();
        let sb2 = state.shared_buffers.clone();
        let sc3 = state.shared_commits.clone();
        let sb3 = state.shared_buffers.clone();
        let kr = state.kb_registry.clone();
        let pr = state.ptr_registry.clone();

        state.wayland_server.register_global(
            "wl_compositor", 4,
            move || {
                let sc = sc.clone();
                let sb = sb.clone();
                ObjectInner::Rc(Rc::new(RefCell::new(AppCompositor {
                    pending_commits: sc,
                    buffer_registry: sb,
                })))
            },
            Object::new::<wl_compositor::WlCompositor>
        );
        state.wayland_server.register_global(
            "wl_shm", 1,
            move || {
                let sb2 = sb2.clone();
                ObjectInner::Rc(Rc::new(RefCell::new(AppShm {
                    buffer_registry: sb2,
                })))
            },
            Object::new::<wl_shm::WlShm>
        );
        state.wayland_server.register_global(
            "wl_seat", 7,
            move || {
                let kr = kr.clone();
                let pr = pr.clone();
                ObjectInner::Rc(Rc::new(RefCell::new(AppSeat {
                    keyboard_registry: kr,
                    pointer_registry: pr,
                })))
            },
            Object::new::<wl_seat::WlSeat>
        );
        state.wayland_server.register_global(
            "xdg_wm_base", 2,
            move || {
                let sc3 = sc3.clone();
                let sb3 = sb3.clone();
                ObjectInner::Rc(Rc::new(RefCell::new(AppXdgWmBase {
                    pending_commits: sc3,
                    buffer_registry: sb3,
                })))
            },
            Object::new::<xdg_wm_base::XdgWmBase>
        );

        state.rebuild_dashboard();
        Some(state)

    }

    /// Reconstruye el árbol de widgets de Stylus para el dashboard
    pub fn rebuild_dashboard(&mut self) {
        use crate::stylus::{Column, Row, widgets::{Gauge, Button}};

        let mut root = Column::new().spacing(40.0);

        // Fila 1: Vitals (CPU, MEM, NET)
        let row1 = Row::new().spacing(20.0)
            .push(Gauge::new(self.cpu_usage * 100.0))
            .push(Gauge::new(self.mem_usage * 100.0))
            .push(Gauge::new(self.net_usage * 100.0));
        
        // Fila 2: Thermals
        let cpu_t = (self.cpu_temp as f32 / 10.0).clamp(0.0, 100.0);
        let gpu_l = (self.gpu_load as f32).clamp(0.0, 100.0);
        let gpu_t = (self.gpu_temp as f32 / 10.0).clamp(0.0, 100.0);

        let row2 = Row::new().spacing(20.0)
            .push(Gauge::new(cpu_t))
            .push(Gauge::new(gpu_l))
            .push(Gauge::new(gpu_t));

        // Fila 3: Actions
        let row3 = Row::new().spacing(20.0)
            .push(Button::new(1, "CONTROL"))
            .push(Button::new(2, "NETWORK"))
            .push(Button::new(3, "TERMINAL"));

        root = root.push(row1).push(row2).push(row3);

        self.dashboard_view = Some(std::boxed::Box::new(root));
    }

    pub fn handle_event(&mut self, event: &CompositorEvent) {
        match event {
            CompositorEvent::Input(ev) => {
                self.input_event_count += 1;
                // Registrar el tick del contador para el watchdog de inanición de input.
                self.last_input_tick = self.counter;
                self.input.apply_event(
                    ev,
                    self.backend.fb.info.width as i32,
                    self.backend.fb.info.height as i32,
                    &mut self.space.windows,
                    &mut self.space.window_count,
                    &self.surfaces,
                );
                
                // Route to Wayland
                let (cx, cy) = (self.input.cursor_x, self.input.cursor_y);
                match ev.event_type {
                    1 => { // Move
                        for (&client_id, &ptr_id) in (*self.ptr_registry).borrow().iter() {
                            if let Some(client) = self.wayland_server.clients.get(&client_id) {
                                let event = wl_pointer::Event::Motion {
                                    time: self.counter as u32,
                                    surface_x: cx as f32,
                                    surface_y: cy as f32,
                                };
                                let _ = client.send_event(ptr_id, event);
                            }
                        }
                    }
                    2 | 3 => { // Button
                        let btn = if ev.event_type == 2 { 0x110 } else { 0 }; // BTN_LEFT approx
                        let btn_state = if ev.code != 0 { 1 } else { 0 };
                        for (&client_id, &ptr_id) in (*self.ptr_registry).borrow().iter() {
                            if let Some(client) = self.wayland_server.clients.get(&client_id) {
                                let event = wl_pointer::Event::Button {
                                    serial: self.counter as u32,
                                    time: self.counter as u32,
                                    button: btn,
                                    state: btn_state,
                                };
                                let _ = client.send_event(ptr_id, event);
                            }
                        }
                    }
                    4 | 5 => { // Key
                        let key = ev.code as u32;
                        let key_state = if ev.value != 0 { 1 } else { 0 };
                        for (&client_id, &kb_id) in (*self.kb_registry).borrow().iter() {
                            if let Some(client) = self.wayland_server.clients.get(&client_id) {
                                let event = wl_keyboard::Event::Key {
                                    serial: self.counter as u32,
                                    time: self.counter as u32,
                                    key,
                                    state: key_state,
                                };
                                let _ = client.send_event(kb_id, event);
                            }
                        }
                    }
                    _ => {}
                }

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
            CompositorEvent::NetExtendedStats(stats) => {
                self.net_extended_stats = Some(*stats);
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
                    // Debug: helps to see why a service is missing from the UI.
                    // Print only when System Central is active to limit spam.
                    let mut parsed = 0usize;
                    let mut offset = 8;
                    for i in 0..count {
                        if i >= 32 { break; }
                        if data.len() >= offset + 28 {
                            let mut svc = ServiceInfo::new();
                            svc.name[..16].copy_from_slice(&data[offset..offset+16]);
                            offset += 16;
                            svc.state = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                            offset += 4;
                            svc.pid = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                            offset += 4;
                            svc.restart_count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap_or([0; 4]));
                            offset += 4;
                            self.service_list[i] = svc;
                            parsed += 1;

                            if self.input.system_central_active {
                                let name_raw = core::str::from_utf8(&self.service_list[i].name).unwrap_or("?");
                                let name_str = match name_raw.find('\0') {
                                    Some(pos) => &name_raw[..pos],
                                    None => name_raw,
                                }.trim();
                            }
                        }
                    }
                    self.service_count = parsed;
                }
                self.dirty = true;
            }
            #[cfg(any(not(target_os = "linux"), test))]
            #[cfg(any(not(target_os = "linux"), test))]
            CompositorEvent::Wayland(data, sender_pid) => {
                let client_id = wayland_proto::wl::server::client::ClientId(*sender_pid);
                if !self.wayland_server.clients.contains_key(&client_id) {
                    let conn = Rc::new(RefCell::new(EclipseWaylandConnection::new(*sender_pid, 0)));
                    self.wayland_server.add_client(client_id, conn);
                }
                
                let _ = self.wayland_server.process_message(client_id, &data, &[]);
                
                // Process output events/buffers (this is now mostly handled by WaylandServer internally
                // and drip_commits for rendering). We don't need a legacy process_wayland_events loop.
                self.dirty = true;
            }
            _ => {} // Handle X11 if needed
        }
    }

    /// Procesa IPC sin colgar el frame: drena el buzón del kernel (evita "buzon lleno")
    /// pero limita eventos procesados por frame para seguir haciendo render/update a ~60 FPS.
    pub fn handle_ipc(&mut self) {
        #[cfg(not(test))]
        self.backend.drain_ipc_into_pending(128);
        const EVENTS_PER_FRAME: usize = 64;
        let mut events_processed = 0usize;
        while events_processed < EVENTS_PER_FRAME {
            match self.backend.poll_event() {
                None => break,
                Some(event) => {
                    self.last_ipc_activity = std::time::Instant::now();
                    events_processed += 1;
                    self.handle_event(&event);
                }
            }
        }
    }

    fn handle_wayland_socket(&mut self) {
        if let Some(socket) = self.wayland_socket.as_mut() {
            if socket.poll(&mut self.wayland_server) {
                self.dirty = true;
            }
        }
    }

    fn handle_x11(&mut self) {
        if let Some(server) = self.x11_server.as_mut() {
            let actions = server.poll(self.backend.fb.info.width as u16, self.backend.fb.info.height as u16);
            for action in actions {
                match action {
                    X11Action::MapWindow { window_id, client_id, x, y, width, height, title } => {
                         if self.space.window_count < crate::compositor::MAX_WINDOWS_COUNT {
                             let win_idx = self.space.window_count;
                             let mut win = ShellWindow::new_empty();
                             win.x = x as i32;
                             win.y = y as i32;
                             win.w = width as i32;
                             win.h = (height as i32) + ShellWindow::TITLE_H;
                             win.curr_x = win.x as f32;
                             win.curr_y = win.y as f32;
                             win.workspace = self.input.current_workspace;
                             win.content = WindowContent::Wayland { surface_id: window_id, conn_idx: 0 }; 
                             self.space.map_window(win);
                             self.input.focused_window = Some(win_idx);
                         }
                    }
                    X11Action::FrameReady { window_id, pixels, width, height } => {
                        for i in 0..self.space.window_count {
                            if let WindowContent::Wayland { surface_id, .. } = self.space.windows[i].content {
                                if surface_id == window_id {
                                     let win = &mut self.space.windows[i];
                                     win.wayland_vaddr = pixels.as_ptr() as usize; 
                                     win.wayland_w = width;
                                     win.wayland_h = height;
                                     win.wayland_stride = width * 4;
                                     // In this toy implementation, we probably need to keep the Vec alive.
                                     // For now we just blit it eventually.
                                     break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            server.flush_events();
        }
    }

    fn drip_commits(&mut self) {
        if (*self.shared_commits).borrow().is_empty() { return; }

        let mut commits_vec: std::vec::Vec<_> = (*self.shared_commits).borrow_mut().drain(..).collect();
        for commit in commits_vec {
             let mut found = false;
             for i in 0..self.space.window_count {
                 if let crate::compositor::WindowContent::Wayland { surface_id, .. } = self.space.windows[i].content {
                     if surface_id == commit.surface_id {
                         let win = &mut self.space.windows[i];
                         win.wayland_vaddr = commit.vaddr;
                         win.wayland_w = commit.width;
                         win.wayland_h = commit.height;
                         win.wayland_stride = commit.stride;
                         found = true;
                         break;
                     }
                 }
             }

             if !found && self.space.window_count < crate::compositor::MAX_WINDOWS_COUNT {
                 let win_idx = self.space.window_count;
                 let mut win = ShellWindow::new_empty();
                 win.x = 100 + (win_idx as i32) * 30;
                 win.y = 100 + (win_idx as i32) * 30;
                 win.w = commit.width as i32;
                 win.h = (commit.height as i32) + ShellWindow::TITLE_H;
                 win.curr_x = win.x as f32;
                 win.curr_y = win.y as f32;
                 win.workspace = self.input.current_workspace;
                 win.content = crate::compositor::WindowContent::Wayland { surface_id: commit.surface_id, conn_idx: 0 };
                 win.wayland_vaddr = commit.vaddr;
                 win.wayland_w = commit.width;
                 win.wayland_h = commit.height;
                 win.wayland_stride = commit.stride;
                 self.space.map_window(win);
                 self.input.focused_window = Some(win_idx);
             }
             self.dirty = true;
        }
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn ensure_wayland_pool_mapped(&mut self, _conn_idx: usize, _buffer_id: u32) {
        // This is now handled by AppShm in protocol.rs
    }

    pub fn update(&mut self) -> bool {
        self.counter = self.counter.wrapping_add(1);
        self.handle_requests();
        
        // --- Process Wayland/X11 ---
        self.handle_wayland_socket();
        self.handle_x11();
        self.drip_commits();

        let window_count_before = self.space.window_count;
        let fb_w = self.backend.fb.info.width as i32;
        let fb_h = self.backend.fb.info.height as i32;
        let busy_animations = self.update_animations_and_layout(fb_w, fb_h, window_count_before);
        let busy_metrics = self.update_metrics_if_needed();
        let busy = busy_animations || busy_metrics || self.dirty;
        busy
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
        let need_metrics = self.input.dashboard_active || self.input.system_central_active || self.input.network_active;
        let metrics_interval = if need_metrics { 800u64 } else { 4000u64 };

        if metrics_elapsed.as_millis() as u64 >= metrics_interval {
            self.last_metrics_update = now;
            let mut current = SystemStats {
                uptime_ticks: 0,
                idle_ticks: 0,
                total_mem_frames: 0,
                used_mem_frames: 0,
                cpu_count: 0,
                cpu_temp: [0; 16],
                gpu_load: [0; 4],
                gpu_temp: [0; 4],
                gpu_vram_total_bytes: 0,
                gpu_vram_used_bytes: 0,
                anomaly_count: 0,
                heap_fragmentation: 0,
                wall_time_offset: 0,
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

                    // Para etiquetas del dashboard (CPU total y RAM total).
                    self.cpu_count = current.cpu_count;
                    self.mem_total_kb = current.total_mem_frames.saturating_mul(4);
                }
            }

            if need_metrics {
                if let Some(pid) = self.network_pid {
                    let _ = unsafe { eclipse_send(pid as u32, 0x08, b"GET_NET_STATS".as_ptr() as *const core::ffi::c_void, 13, 0) }; // MSG_TYPE_NETWORK = 0x08

                    let rx_delta = self.net_rx.saturating_sub(self.prev_net_rx);
                    let tx_delta = self.net_tx.saturating_sub(self.prev_net_tx);
                    let total_delta = rx_delta + tx_delta;

                    let max_bytes_per_sec = 5_000_000.0;
                    let bytes_per_sec = (total_delta as f32) * 2.0;
                    self.net_usage = (bytes_per_sec / max_bytes_per_sec).clamp(0.0, 1.0);

                    if self.input.network_active {
                        let _ = unsafe { eclipse_send(pid as u32, 0x08, b"GET_NET_EXT_STATS".as_ptr() as *const core::ffi::c_void, 17, 0) };
                    }

                    if self.input.apply_static_config {
                        self.input.apply_static_config = false;
                        let mut msg = [0u8; 512];
                        let header = eclipse_ipc::types::NetRequestHeader {
                            magic: *eclipse_ipc::types::TAG_NETW,
                            op: eclipse_ipc::types::NetOp::SetStaticConfig,
                            request_id: 0,
                            client_pid: pid as u32, // self pid or similar
                            resource_id: 0,
                        };
                        unsafe {
                            core::ptr::copy_nonoverlapping(&header as *const _ as *const u8, msg.as_mut_ptr(), core::mem::size_of::<eclipse_ipc::types::NetRequestHeader>());
                            let config_bytes = core::slice::from_raw_parts(&self.input.static_config as *const _ as *const u8, core::mem::size_of::<eclipse_ipc::types::NetStaticConfig>());
                            core::ptr::copy_nonoverlapping(config_bytes.as_ptr(), msg.as_mut_ptr().add(core::mem::size_of::<eclipse_ipc::types::NetRequestHeader>()), config_bytes.len());
                            let total_len = core::mem::size_of::<eclipse_ipc::types::NetRequestHeader>() + config_bytes.len();
                            let _ = eclipse_send(pid as u32, 0x08, msg.as_ptr() as *const core::ffi::c_void, total_len, 0);
                        }
                    }

                    if self.input.renew_dhcp {
                        self.input.renew_dhcp = false;
                        let header = eclipse_ipc::types::NetRequestHeader {
                            magic: *eclipse_ipc::types::TAG_NETW,
                            op: eclipse_ipc::types::NetOp::SetDhcpConfig,
                            request_id: 0,
                            client_pid: pid as u32,
                            resource_id: 0,
                        };
                        unsafe {
                            let _ = eclipse_send(pid as u32, 0x08, &header as *const _ as *const core::ffi::c_void, core::mem::size_of::<eclipse_ipc::types::NetRequestHeader>(), 0);
                        }
                    }

                    self.prev_net_rx = self.net_rx;
                    self.prev_net_tx = self.net_tx;
                }

                // AI-CORE Vitals
                self.cpu_temp = current.cpu_temp[0]; // BSP Temp

                // GPU VRAM: gauge = VRAM usada / VRAM total (en todas las GPUs).
                let total_vram_kb = current.gpu_vram_total_bytes / 1024;
                let used_vram_kb = current.gpu_vram_used_bytes / 1024;
                self.gpu_vram_total_kb = total_vram_kb;
                if total_vram_kb > 0 {
                    let pct = (used_vram_kb.saturating_mul(100) / total_vram_kb).min(100);
                    self.gpu_load = pct as u32;
                } else {
                    self.gpu_load = 0;
                }

                // Temperatura promedio de todas las GPUs (ignoramos ceros en GPUs no detectadas).
                let mut t_sum: u64 = 0;
                let mut t_cnt: u64 = 0;
                for &t in current.gpu_temp.iter() {
                    if t > 0 {
                        t_sum = t_sum.saturating_add(t as u64);
                        t_cnt += 1;
                    }
                }
                self.gpu_temp = if t_cnt > 0 { (t_sum / t_cnt) as u32 } else { 0 };

                self.anomaly_count = current.anomaly_count;
                self.heap_fragmentation = current.heap_fragmentation;
            }

            // Siempre refrescamos la lista de procesos para poder logear memoria de smithay_app,
            // aunque el overlay de System Central no esté activo.
            let prev_uptime = self.prev_stats.map(|s| s.uptime_ticks).unwrap_or(0);
            let count = unsafe { get_process_list(self.process_list.as_mut_ptr(), 32) };
            if count >= 0 {
                self.process_count = count as usize;

                // Descubrir PID del servicio de red
                for p in &self.process_list[..self.process_count] {
                    let name_len = p.name.iter().position(|&b| b == 0).unwrap_or(16);
                    let name = &p.name[..name_len];
                    if name == b"network" || name.ends_with(b"network_service") || name == b"network_service" || name.windows(11).any(|w| w == b"network_ser") {
                        self.network_pid = Some(p.pid);
                        break;
                    }
                }

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

                    if total_delta > 0 && prev_ticks > 0 {
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

                // Periodically cleanup surfaces of dead processes to avoid the 1GB memory leak
                if self.counter % 60 == 0 { // Every ~1 second (assuming 60fps)
                    let mut i = 0;
                    while i < self.space.window_count {
                        if let crate::compositor::WindowContent::External(s_idx) = self.space.windows[i].content {
                            if (s_idx as usize) < self.surfaces.len() {
                                let pid = self.surfaces[s_idx as usize].pid;
                                if pid != 0 && pid != 1 && pid != unsafe { libc::getpid() as u32 } {
                                    let mut alive = false;
                                    for p in self.process_list.iter().take(self.process_count) {
                                        if p.pid == pid {
                                            alive = true;
                                            break;
                                        }
                                    }
                                    if !alive {
                                        println!("[SMITHAY] Auto-cleaning dead surface for PID {}", pid);
                                        self.space.unmap_window(i, &mut self.surfaces);
                                        // unmap_window shifts the rest of the windows, so we don't increment i
                                        continue;
                                    }
                                }
                            }
                        }
                        i += 1;
                    }
                }

                // Log de diagnóstico: memoria y CPU de smithay_app y contadores internos, SIEMPRE.
                #[cfg(target_os = "eclipse")]
                {
                    let self_pid = unsafe { libc::getpid() as u32 };
                    let mut _self_mem_kb = 0u64;
                    let mut _self_cpu = 0.0f32;
                    for i in 0..self.process_count {
                        let p = &self.process_list[i];
                        if p.pid == self_pid {
                            _self_mem_kb = self.process_mem_kb[i];
                            _self_cpu = self.process_cpu_usage[i];
                            break;
                        }
                    }
                }
            }

            // Sólo pedimos info de servicios cuando System Central está activo.
            if self.input.system_central_active {
                let _ = unsafe { eclipse_send(1, 0, b"GET_SERVICES_INFO\0".as_ptr() as *const core::ffi::c_void, 18, 0) };
            }

            // Actualizar prev_stats AL FINAL para no invalidar el delta de procesos
            self.prev_stats = Some(current);
            self.rebuild_dashboard();
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
                curr_x: 60.0 + (idx as f32) * 20.0 + 300.0,
                curr_y: 160.0 + (idx as f32) * 15.0 + 190.0,
                curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, closing: false,
                stored_rect: (60 + (idx as i32) * 20, 160 + (idx as i32) * 15, 600, 380),
                workspace: self.input.current_workspace,
                content: crate::compositor::WindowContent::InternalDemo,
                damage: std::vec::Vec::new(),
                buffer_handle: None,
                is_dmabuf: false,
                is_panel: false,
                wayland_vaddr: 0,
                wayland_w: 0,
                wayland_h: 0,
                wayland_stride: 0,
                ..Default::default()
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
                self.input.system_central_active = false;
                self.input.network_active = false;
                // Force immediate metrics update
                self.last_metrics_update = std::time::Instant::now() - std::time::Duration::from_millis(5000);
            }
            self.dirty = true;
        }

        if self.input.request_system_central {
            self.input.system_central_active = !self.input.system_central_active;
            self.input.request_system_central = false;
            if self.input.system_central_active {
                self.input.dashboard_active = false;
                self.input.network_active = false;
                // Force immediate metrics update
                self.last_metrics_update = std::time::Instant::now() - std::time::Duration::from_millis(5000);
            }
            self.dirty = true;
        }

        if self.input.request_network {
            self.input.network_active = !self.input.network_active;
            self.input.request_network = false;
            if self.input.network_active {
                self.input.dashboard_active = false;
                self.input.system_central_active = false;
                // Force immediate metrics update
                self.last_metrics_update = std::time::Instant::now() - std::time::Duration::from_millis(5000);
            }
            self.dirty = true;
        }

        if self.input.renew_dhcp {
            self.input.renew_dhcp = false;
            if let Some(pid) = self.network_pid {
                let _ = unsafe { eclipse_send(pid as u32, 0x08, b"RENEW_DHCP".as_ptr() as *const core::ffi::c_void, 10, 0) };
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
            // Fondo de escritorio + logo + sidebar + HUD de logs
            // Igual que en v0.1.6: el HUD forma parte del fondo, no de un overlay separado.
            let uptime_ticks = if let Some(stats) = &self.prev_stats {
                stats.uptime_ticks
            } else {
                0
            };

            render::draw_desktop_shell(
                &mut self.backend.fb,
                &self.style_engine,
                self.dashboard_view.as_ref(),
                &self.space.windows,
                self.space.window_count,
                self.counter,
                self.input.cursor_x,
                self.input.cursor_y,
                &mut self.log_buf,
                &mut self.log_len,
                self.input.dashboard_active,
                self.input.system_central_active,
                self.input.network_active,
                self.cpu_usage,
                self.mem_usage,
                self.net_usage,
                self.cpu_temp,
                self.gpu_load,
                self.gpu_temp,
                self.anomaly_count,
                self.heap_fragmentation,
                uptime_ticks,
                self.cpu_count,
                self.mem_total_kb,
                self.gpu_vram_total_kb,
                &self.service_list,
                &self.process_list,
                &self.process_cpu_usage,
                &self.process_mem_kb,
            );

            if self.input.network_active {
                render::draw_network_dashboard(
                    &mut self.backend.fb,
                    self.counter,
                    self.net_extended_stats.as_ref(),
                    &self.input,
                );
            }

            if !self.input.dashboard_active && !self.input.system_central_active && !self.input.network_active {
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
                    self.counter,
                );
                // Clear damage after drawing
                for i in 0..self.space.window_count {
                    self.space.windows[i].damage.clear();
                }
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

            if self.input.alt_tab_active {
                render::draw_alt_tab_hud(&mut self.backend.fb, &self.space.windows, self.space.window_count, self.input.focused_window);
            }
        } else {
            render::draw_lock_screen(&mut self.backend.fb, self.counter);
        }
        // Draw software cursor (forced for visibility until DRM hardware cursor is confirmed stable)
        render::draw_cursor(
            &mut self.backend.fb,
            embedded_graphics::prelude::Point::new(self.input.cursor_x, self.input.cursor_y),
        );

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
            damage: std::vec::Vec::new(),
            buffer_handle: None,
            is_dmabuf: false,
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
            damage: std::vec::Vec::new(),
            buffer_handle: None,
            is_dmabuf: false,
            is_panel: false,
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
            damage: std::vec::Vec::new(),
            buffer_handle: None,
            is_dmabuf: false,
            is_panel: false,
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
