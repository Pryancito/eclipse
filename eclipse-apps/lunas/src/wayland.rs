//! Native, minimal Wayland protocol implementation for Lunas.
//!
//! Instead of using a full Wayland state machine crate, we manually construct
//! the binary events and handle the handshake for the terminal app.

use std::prelude::v1::*;
use sidewind::xwayland::XwmState;
use crate::compositor::{ShellWindow, WindowContent};

/// Maximum concurrent Wayland client connections.
pub const MAX_WAYLAND_CONNECTIONS: usize = 16;

/// Per-client Wayland connection state.
pub struct ClientConnection {
    /// PID of the Wayland client process.
    pub pid: u32,
    /// Surfaces created by this client: maps surface_id → window_slot_index.
    pub surfaces: alloc::vec::Vec<(u32, Option<usize>)>,
    /// Does the client have a registry?
    pub registry_id: Option<u32>,
    /// Global object names
    pub compositor_name: u32,
    pub shm_name: u32,
    pub shell_name: u32,
}

impl ClientConnection {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            surfaces: alloc::vec::Vec::new(),
            registry_id: None,
            compositor_name: 1,
            shm_name: 2,
            shell_name: 3,
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
        let _size = (size_op >> 16) as usize;

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
                // If this is a bind request on the registry
                if let Some(reg_id) = self.connections[conn_idx].registry_id {
                    if obj_id == reg_id && opcode == 0 { // bind(name, interface, version, new_id)
                        // For now we just ignore the bind request but we could track the bound IDs.
                    }
                }

                // Create Surface (opcode 0 on wl_compositor)
                // In terminal, compositor bound ID is likely 4.
                if opcode == 0 && obj_id == 4 && data.len() >= 12 {
                    let surface_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    if surface_id > 0 {
                        self.connections[conn_idx].surfaces.push((surface_id, None));
                        return WaylandAction::CreateSurface { pid, surface_id, conn_idx };
                    }
                }

                // Surface Commit (opcode 6 on wl_surface)
                // In terminal, surface ID starts at 7.
                if opcode == 6 && obj_id >= 7 {
                    return WaylandAction::CommitSurface { pid, surface_id: obj_id };
                }
            }
        }

        WaylandAction::None
    }

    /// Construct and queue global registry events.
    fn respond_with_globals(&mut self, pid: u32, registry_id: u32) {
        let mut globals = alloc::vec::Vec::new();
        
        // Helper to append a global event
        let mut add_global = |name: u32, interface: &str, version: u32| {
            let mut ev = alloc::vec::Vec::new();
            ev.extend_from_slice(&registry_id.to_le_bytes()); // sender = registry
            let opcode = 0u16; // global event
            let if_len = interface.len() + 1; // including null
            let payload_size = 4 + 4 + if_len + ((4 - (if_len % 4)) % 4) + 4;
            let total_size = 4 + 4 + payload_size;
            
            ev.extend_from_slice(&((total_size as u32) << 16 | (opcode as u32)).to_le_bytes());
            ev.extend_from_slice(&name.to_le_bytes());
            ev.extend_from_slice(&(if_len as u32).to_le_bytes());
            ev.extend_from_slice(interface.as_bytes());
            ev.push(0); // null terminator
            while ev.len() % 4 != 0 { ev.push(0); } // padding
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
    CommitSurface { pid: u32, surface_id: u32 },
    DestroySurface { pid: u32, surface_id: u32 },
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
