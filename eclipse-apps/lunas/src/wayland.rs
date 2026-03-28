//! Native, minimal Wayland protocol implementation for Lunas.
//!
//! Instead of using a full Wayland state machine crate, we manually construct
//! the binary events and handle the handshake for the terminal app.

use std::prelude::v1::*;
use sidewind::xwayland::XwmState;
use crate::compositor::{ShellWindow, WindowContent};

/// Maximum concurrent Wayland client connections.
pub const MAX_WAYLAND_CONNECTIONS: usize = 16;

/// SHM buffer metadata recorded after wl_shm_pool.create_buffer.
#[derive(Clone, Copy, Default)]
pub struct ShmBufferDesc {
    pub pool_fd: i32,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
}

/// Per-client Wayland connection state.
pub struct ClientConnection {
    /// PID of the Wayland client process.
    pub pid: u32,
    /// Surfaces created by this client: maps surface_id → window_slot_index.
    pub surfaces: alloc::vec::Vec<(u32, Option<usize>)>,
    /// Does the client have a registry?
    pub registry_id: Option<u32>,
    /// Bound object IDs for the compositor interfaces (assigned during bind).
    pub bound_compositor_id: Option<u32>,
    pub bound_shm_id: Option<u32>,
    pub bound_shell_id: Option<u32>,
    /// SHM pool state: pool_object_id, fd, and total size in bytes.
    pub shm_pool_obj_id: Option<u32>,
    pub shm_pool_fd: Option<i32>,
    pub shm_pool_size: i32,
    /// Buffer objects: maps buffer_object_id → ShmBufferDesc.
    pub shm_buffers: alloc::vec::Vec<(u32, ShmBufferDesc)>,
    /// Per-surface pending buffer (surface_id → buffer_object_id).
    pub surface_pending_buffer: alloc::vec::Vec<(u32, u32)>,
}

impl ClientConnection {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            surfaces: alloc::vec::Vec::new(),
            registry_id: None,
            bound_compositor_id: None,
            bound_shm_id: None,
            bound_shell_id: None,
            shm_pool_obj_id: None,
            shm_pool_fd: None,
            shm_pool_size: 0,
            shm_buffers: alloc::vec::Vec::new(),
            surface_pending_buffer: alloc::vec::Vec::new(),
        }
    }

    pub fn set_window_for_surface(&mut self, surface_id: u32, window_idx: usize) {
        if let Some(entry) = self.surfaces.iter_mut().find(|(s, _)| *s == surface_id) {
            entry.1 = Some(window_idx);
        }
    }

    pub fn window_for_surface(&self, surface_id: u32) -> Option<usize> {
        self.surfaces.iter().find(|(s, _)| *s == surface_id).and_then(|(_, w)| *w)
    }

    pub fn remove_surface(&mut self, surface_id: u32) {
        self.surfaces.retain(|(s, _)| *s != surface_id);
        self.surface_pending_buffer.retain(|(s, _)| *s != surface_id);
    }

    /// Returns true if `obj_id` is a known surface for this client.
    pub fn is_surface(&self, obj_id: u32) -> bool {
        self.surfaces.iter().any(|(s, _)| *s == obj_id)
    }

    /// Look up the SHM buffer descriptor attached to a surface.
    pub fn attached_buffer_for_surface(&self, surface_id: u32) -> Option<ShmBufferDesc> {
        let buf_id = self.surface_pending_buffer.iter()
            .find(|(s, _)| *s == surface_id)
            .map(|(_, b)| *b)?;
        self.shm_buffers.iter()
            .find(|(id, _)| *id == buf_id)
            .map(|(_, desc)| *desc)
    }
}

pub struct WaylandCompositor {
    pub connections: alloc::vec::Vec<ClientConnection>,
    pub pending_responses: alloc::vec::Vec<(u32, alloc::vec::Vec<u8>)>,
}

impl WaylandCompositor {
    pub fn new() -> Self {
        Self {
            connections: alloc::vec::Vec::new(),
            pending_responses: alloc::vec::Vec::new(),
        }
    }

    /// Process a raw Wayland protocol message from client `pid`.
    pub fn handle_message(
        &mut self,
        data: &[u8],
        pid: u32,
    ) -> WaylandAction {
        if data.len() < 8 {
            return WaylandAction::None;
        }

        // Wayland Header: [obj_id: u32, size_op: u32]
        let obj_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size_op = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let opcode = (size_op & 0xFFFF) as u16;

        // Find or create connection
        let conn_idx = if let Some(idx) = self.connections.iter().position(|c| c.pid == pid) {
            idx
        } else {
            if self.connections.len() < MAX_WAYLAND_CONNECTIONS {
                self.connections.push(ClientConnection::new(pid));
                self.connections.len() - 1
            } else {
                self.connections.remove(0);
                self.connections.push(ClientConnection::new(pid));
                self.connections.len() - 1
            }
        };

        match obj_id {
            1 => { // wl_display
                if opcode == 1 { // get_registry(new_id)
                    if data.len() >= 12 {
                        let registry_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                        self.connections[conn_idx].registry_id = Some(registry_id);
                        self.respond_with_globals(pid, registry_id);
                    }
                }
            }
            _ => {
                // ── Registry bind ──────────────────────────────────────────────────────
                // bind(name:u, interface:s, version:u, new_id:n)
                // data[8..] = args (after 8-byte header)
                if let Some(reg_id) = self.connections[conn_idx].registry_id {
                    if obj_id == reg_id && opcode == 0 && data.len() >= 16 {
                        // Read: name (u32), string length (u32), interface bytes
                        let name = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                        let str_len = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
                        if str_len > 0 && data.len() >= 16 + str_len {
                            let iface_bytes = &data[16..16 + str_len.saturating_sub(1)]; // strip null
                            // Padded string length (round up to 4)
                            let padded = (str_len + 3) & !3;
                            // new_id is after: 8 header + 4 name + 4 str_len + padded_str + 4 version
                            let new_id_offset = 8 + 4 + 4 + padded + 4;
                            if data.len() >= new_id_offset + 4 {
                                let new_id = u32::from_le_bytes([
                                    data[new_id_offset], data[new_id_offset+1],
                                    data[new_id_offset+2], data[new_id_offset+3],
                                ]);
                                match iface_bytes {
                                    b"wl_compositor" => {
                                        self.connections[conn_idx].bound_compositor_id = Some(new_id);
                                    }
                                    b"wl_shm" => {
                                        self.connections[conn_idx].bound_shm_id = Some(new_id);
                                    }
                                    b"wl_shell" => {
                                        self.connections[conn_idx].bound_shell_id = Some(new_id);
                                    }
                                    _ => {}
                                }
                                let _ = name; // name is the global sequence number, not needed further
                            }
                        }
                        return WaylandAction::None;
                    }
                }

                let conn = &self.connections[conn_idx];

                // ── wl_compositor.create_surface ───────────────────────────────────────
                // Opcode 0; arg: new_id (u32)
                let is_compositor = conn.bound_compositor_id == Some(obj_id)
                    || (conn.bound_compositor_id.is_none() && obj_id == 4);
                if opcode == 0 && is_compositor && data.len() >= 12 {
                    let surface_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    if surface_id > 0 {
                        self.connections[conn_idx].surfaces.push((surface_id, None));
                        return WaylandAction::CreateSurface { pid, surface_id, conn_idx };
                    }
                }

                // ── wl_shm.create_pool ────────────────────────────────────────────────
                // Opcode 0; args: new_id (u32), fd (u32/i32), size (i32)
                let is_shm = conn.bound_shm_id == Some(obj_id)
                    || (conn.bound_shm_id.is_none() && obj_id == 5);
                if opcode == 0 && is_shm && data.len() >= 20 {
                    let pool_id  = u32::from_le_bytes([data[8],  data[9],  data[10], data[11]]);
                    let pool_fd  = i32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                    let pool_sz  = i32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                    self.connections[conn_idx].shm_pool_obj_id = Some(pool_id);
                    self.connections[conn_idx].shm_pool_fd     = Some(pool_fd);
                    self.connections[conn_idx].shm_pool_size   = pool_sz;
                    return WaylandAction::None;
                }

                // ── wl_shm_pool.create_buffer ─────────────────────────────────────────
                // Opcode 0; args: new_id, offset, width, height, stride, format
                let is_pool = conn.shm_pool_obj_id == Some(obj_id);
                if opcode == 0 && is_pool && data.len() >= 32 {
                    let buf_id  = u32::from_le_bytes([data[8],  data[9],  data[10], data[11]]);
                    let offset  = i32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                    let width   = i32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                    let height  = i32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                    let stride  = i32::from_le_bytes([data[24], data[25], data[26], data[27]]);
                    let format  = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);
                    let pool_fd = conn.shm_pool_fd.unwrap_or(-1);
                    let desc = ShmBufferDesc { pool_fd, offset, width, height, stride, format };
                    self.connections[conn_idx].shm_buffers.push((buf_id, desc));
                    return WaylandAction::None;
                }

                // ── wl_surface.attach ─────────────────────────────────────────────────
                // Opcode 1; args: buffer (object), x (i32), y (i32)
                let conn = &self.connections[conn_idx];
                if opcode == 1 && conn.is_surface(obj_id) && data.len() >= 12 {
                    let buf_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    // Record or update the pending buffer for this surface
                    if let Some(entry) = self.connections[conn_idx].surface_pending_buffer
                        .iter_mut().find(|(s, _)| *s == obj_id)
                    {
                        entry.1 = buf_id;
                    } else {
                        self.connections[conn_idx].surface_pending_buffer.push((obj_id, buf_id));
                    }
                    return WaylandAction::None;
                }

                // ── wl_surface.commit ─────────────────────────────────────────────────
                // Opcode 6; no args
                let conn = &self.connections[conn_idx];
                if opcode == 6 && conn.is_surface(obj_id) {
                    let shm_info = conn.attached_buffer_for_surface(obj_id);
                    return WaylandAction::CommitSurface {
                        pid,
                        surface_id: obj_id,
                        shm_buffer: shm_info,
                    };
                }
            }
        }

        WaylandAction::None
    }

    /// Construct and queue global registry events.
    fn respond_with_globals(&mut self, pid: u32, registry_id: u32) {
        let mut globals = alloc::vec::Vec::new();

        let mut add_global = |name: u32, interface: &str, version: u32| {
            let mut ev = alloc::vec::Vec::new();
            ev.extend_from_slice(&registry_id.to_le_bytes()); // sender = registry
            let opcode = 0u16; // global event
            let if_len = interface.len() + 1; // including null terminator
            let padded_str = (if_len + 3) & !3;
            // payload = name:4 + if_len:4 + padded_string + version:4
            let payload_size = 4 + 4 + padded_str + 4;
            let total_size = 8 + payload_size; // 8-byte header + payload

            ev.extend_from_slice(&(((total_size as u32) << 16) | (opcode as u32)).to_le_bytes());
            ev.extend_from_slice(&name.to_le_bytes());
            ev.extend_from_slice(&(if_len as u32).to_le_bytes());
            ev.extend_from_slice(interface.as_bytes());
            ev.push(0u8); // null terminator
            while ev.len() % 4 != 0 { ev.push(0u8); } // padding
            ev.extend_from_slice(&version.to_le_bytes());

            globals.extend_from_slice(&ev);
        };

        add_global(1, "wl_compositor", 4);
        add_global(2, "wl_shm", 1);
        add_global(3, "wl_shell", 1);

        self.pending_responses.push((pid, globals));
    }

    pub fn register_surface_window(&mut self, pid: u32, surface_id: u32, window_idx: usize) {
        if let Some(client) = self.connections.iter_mut().find(|c| c.pid == pid) {
            client.set_window_for_surface(surface_id, window_idx);
        }
    }

    pub fn disconnect_client(&mut self, pid: u32) {
        self.connections.retain(|c| c.pid != pid);
    }
}

#[derive(Debug, PartialEq)]
pub enum WaylandAction {
    None,
    CreateSurface { pid: u32, surface_id: u32, conn_idx: usize },
    CommitSurface { pid: u32, surface_id: u32, shm_buffer: Option<ShmBufferDesc> },
    DestroySurface { pid: u32, surface_id: u32 },
}

impl core::fmt::Debug for ShmBufferDesc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ShmBufferDesc {{ fd: {}, {}x{} }}", self.pool_fd, self.width, self.height)
    }
}

impl PartialEq for ShmBufferDesc {
    fn eq(&self, other: &Self) -> bool {
        self.pool_fd == other.pool_fd
            && self.offset == other.offset
            && self.width == other.width
            && self.height == other.height
            && self.stride == other.stride
            && self.format == other.format
    }
}

/// XWayland integration state.
pub struct XwaylandIntegration {
    pub xwayland_pid: Option<u32>,
    pub xwm: XwmState,
}

impl XwaylandIntegration {
    pub fn new() -> Self {
        Self {
            xwayland_pid: None,
            xwm: XwmState::new(),
        }
    }

    pub fn set_pid(&mut self, pid: u32) {
        self.xwayland_pid = Some(pid);
    }

    pub fn handle_x11_event(&mut self, data: &[u8], pid: u32) -> XwaylandAction {
        if let Some(xpid) = self.xwayland_pid {
            if xpid != pid { return XwaylandAction::None; }
        } else {
            self.xwayland_pid = Some(pid);
        }

        if data.is_empty() { return XwaylandAction::None; }
        let event_type = data[0] & 0x7F;

        match event_type {
            19 => { // MapNotify
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                self.xwm.handle_map_request(window_id);
                XwaylandAction::MapWindow { window_id }
            }
            18 => { // UnmapNotify
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                XwaylandAction::UnmapWindow { window_id }
            }
            17 => { // DestroyNotify
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                self.xwm.windows.retain(|&w| w != window_id);
                XwaylandAction::DestroyWindow { window_id }
            }
            _ => XwaylandAction::None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.xwayland_pid.is_some()
    }
}

pub enum XwaylandAction {
    None,
    MapWindow { window_id: u32 },
    UnmapWindow { window_id: u32 },
    DestroyWindow { window_id: u32 },
}

pub fn make_wayland_window(
    surface_id: u32,
    conn_idx: usize,
    fb_width: i32,
    fb_height: i32,
    workspace: u8,
    title: &[u8],
) -> ShellWindow {
    let x = 60;
    let y = ShellWindow::TITLE_H + 20;
    let w = (fb_width / 2).max(320);
    let h = (fb_height / 2).max(240);
    let mut title_buf = [0u8; 32];
    let copy = title.len().min(31);
    title_buf[..copy].copy_from_slice(&title[..copy]);
    ShellWindow {
        x, y, w, h,
        curr_x: (x + w / 2) as f32,
        curr_y: (y + h / 2) as f32,
        curr_w: 0.0, curr_h: 0.0,
        content: WindowContent::Wayland { surface_id, conn_idx },
        workspace,
        title: title_buf,
        ..Default::default()
    }
}

