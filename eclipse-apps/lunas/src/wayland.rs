//! Wayland and XWayland compositor support for Lunas.
//!
//! This module bridges incoming Wayland protocol messages (received via IPC as
//! `CompositorEvent::Wayland`) and X11 messages (via `CompositorEvent::X11`) into
//! Lunas' window management layer.
//!
//! # Architecture
//! - `WaylandCompositor` tracks per-client `WaylandConnection` objects (keyed by PID).
//!   Each connection holds the protocol state machine (registry, compositor, surfaces).
//! - When a Wayland client creates and commits a surface, a `ShellWindow` with
//!   `WindowContent::Wayland { surface_id, conn_idx }` is inserted into the space.
//! - `XwaylandIntegration` wraps `XwmState` from `sidewind_xwayland` and handles
//!   X11 window mapping events forwarded by the XWayland translation layer.

use std::prelude::v1::*;
use sidewind_wayland::WaylandConnection;
use sidewind_xwayland::XwmState;
use crate::compositor::{ShellWindow, WindowContent};

/// Maximum concurrent Wayland client connections.
pub const MAX_WAYLAND_CONNECTIONS: usize = 16;

/// Per-client Wayland connection state.
pub struct ClientConnection {
    /// PID of the Wayland client process.
    pub pid: u32,
    /// Wayland protocol state machine for this client.
    pub conn: WaylandConnection,
    /// Surfaces created by this client: maps surface_id → window_slot_index.
    pub surfaces: alloc::vec::Vec<(u32, Option<usize>)>,
    /// Next surface ID to assign.
    next_surface_id: u32,
}

impl ClientConnection {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            conn: WaylandConnection::new(),
            surfaces: alloc::vec::Vec::new(),
            next_surface_id: 1,
        }
    }

    /// Allocate a new surface ID for this client.
    pub fn alloc_surface_id(&mut self) -> u32 {
        let id = self.next_surface_id;
        self.next_surface_id = self.next_surface_id.wrapping_add(1).max(1);
        self.surfaces.push((id, None));
        id
    }

    /// Associate a surface with a window slot.
    pub fn set_window_for_surface(&mut self, surface_id: u32, window_idx: usize) {
        if let Some(entry) = self.surfaces.iter_mut().find(|(s, _)| *s == surface_id) {
            entry.1 = Some(window_idx);
        }
    }

    /// Find the window index for a surface, if any.
    pub fn window_for_surface(&self, surface_id: u32) -> Option<usize> {
        self.surfaces.iter().find(|(s, _)| *s == surface_id).and_then(|(_, w)| *w)
    }

    /// Remove a surface from tracking.
    pub fn remove_surface(&mut self, surface_id: u32) {
        self.surfaces.retain(|(s, _)| *s != surface_id);
    }
}

/// Tracks all active Wayland client connections.
pub struct WaylandCompositor {
    pub connections: alloc::vec::Vec<ClientConnection>,
    /// Pending response bytes to send back: (pid, data)
    pub pending_responses: alloc::vec::Vec<(u32, alloc::vec::Vec<u8>)>,
}

impl WaylandCompositor {
    pub fn new() -> Self {
        Self {
            connections: alloc::vec::Vec::new(),
            pending_responses: alloc::vec::Vec::new(),
        }
    }

    /// Look up or create a connection slot for the given PID.
    fn get_or_create_connection(&mut self, pid: u32) -> &mut ClientConnection {
        if !self.connections.iter().any(|c| c.pid == pid) {
            if self.connections.len() < MAX_WAYLAND_CONNECTIONS {
                self.connections.push(ClientConnection::new(pid));
            } else {
                // Drop the oldest connection (FIFO eviction by insertion order).
                self.connections.remove(0);
                self.connections.push(ClientConnection::new(pid));
            }
        }
        self.connections.iter_mut().find(|c| c.pid == pid).unwrap()
    }

    /// Return the slot index for a client's connection.
    fn conn_idx_for_pid(&self, pid: u32) -> Option<usize> {
        self.connections.iter().position(|c| c.pid == pid)
    }

    /// Process a raw Wayland protocol message from client `pid`.
    ///
    /// Parses the message through the `WaylandConnection` state machine and,
    /// when a `wl_compositor.create_surface` request is detected, registers a
    /// new surface. Returns `WaylandAction` describing what Lunas should do.
    pub fn handle_message(
        &mut self,
        data: &[u8],
        pid: u32,
    ) -> WaylandAction {
        if data.len() < 8 {
            return WaylandAction::None;
        }

        // Parse the raw Wayland header to detect surface creation.
        let obj_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size_op = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let opcode = (size_op & 0xFFFF) as u16;

        // Ensure connection exists; compute conn_idx before the mutable borrow.
        let conn_idx = {
            if !self.connections.iter().any(|c| c.pid == pid) {
                if self.connections.len() < MAX_WAYLAND_CONNECTIONS {
                    self.connections.push(ClientConnection::new(pid));
                } else {
                    self.connections.remove(0);
                    self.connections.push(ClientConnection::new(pid));
                }
            }
            self.connections.iter().position(|c| c.pid == pid).unwrap()
        };

        // Process message through the protocol state machine.
        let reply = self.connections[conn_idx].conn.process_message(data);
        if let Some(r) = reply {
            if !r.is_empty() {
                self.pending_responses.push((pid, r));
            }
        }

        // Drain any pending events the state machine queued.
        while let Some(ev) = self.connections[conn_idx].conn.pending_events.pop_front() {
            if !ev.is_empty() {
                self.pending_responses.push((pid, ev));
            }
        }

        // Detect wl_compositor.create_surface (opcode 0) — the result new_id is in data[8..12].
        // The sidewind_wayland WlCompositor handler returns the new surface ID as 4 bytes.
        // Check: opcode == 0 and obj_id is likely a compositor object (≥ 3 by protocol convention)
        if opcode == 0 && obj_id >= 3 && data.len() >= 12 {
            let surface_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
            if surface_id > 0 {
                self.connections[conn_idx].surfaces.push((surface_id, None));
                return WaylandAction::CreateSurface { pid, surface_id, conn_idx };
            }
        }

        // Detect wl_surface.commit (opcode 6) — the surface should be made visible.
        // The obj_id is the surface's Wayland object ID.
        if opcode == 6 {
            return WaylandAction::CommitSurface { pid, surface_id: obj_id };
        }

        // Detect wl_surface.destroy (opcode 0 on a wl_surface object).
        // We can only distinguish this from create_surface by obj_id range; surface IDs
        // assigned above start at 1, compositor objects are typically ≥ 3. A safe heuristic:
        // if the object is < 3 treat it as a display/registry message only.
        // For destroy: wl_surface uses opcode 0 for destroy in older protocols. We detect
        // it when obj_id matches a known surface_id for this client.
        if opcode == 0 {
            let surface_id = obj_id;
            let has = self.connections[conn_idx].surfaces.iter().any(|(s, _)| *s == surface_id);
            if has {
                return WaylandAction::DestroySurface { pid, surface_id };
            }
        }

        WaylandAction::None
    }

    /// Register that a surface has been assigned a ShellWindow slot.
    pub fn register_surface_window(&mut self, pid: u32, surface_id: u32, window_idx: usize) {
        if let Some(client) = self.connections.iter_mut().find(|c| c.pid == pid) {
            client.set_window_for_surface(surface_id, window_idx);
        }
    }

    /// Remove all state for a disconnected client.
    pub fn disconnect_client(&mut self, pid: u32) {
        self.connections.retain(|c| c.pid != pid);
    }
}

/// Actions that the compositor module requests from the main state.
#[derive(Debug, PartialEq)]
pub enum WaylandAction {
    None,
    /// A Wayland client created a new surface.
    CreateSurface { pid: u32, surface_id: u32, conn_idx: usize },
    /// A Wayland client committed (made visible) a surface.
    CommitSurface { pid: u32, surface_id: u32 },
    /// A Wayland client destroyed a surface.
    DestroySurface { pid: u32, surface_id: u32 },
}

/// XWayland integration state.
///
/// XWayland is an X11 server that translates X11 clients into Wayland surfaces.
/// Lunas tracks XWayland's PID so it can route X11 window management events to
/// the correct ShellWindows.
pub struct XwaylandIntegration {
    /// PID of the running XWayland process, if any.
    pub xwayland_pid: Option<u32>,
    /// X Window Manager state (atom cache, mapped windows).
    pub xwm: XwmState,
}

impl XwaylandIntegration {
    pub fn new() -> Self {
        Self {
            xwayland_pid: None,
            xwm: XwmState::new(),
        }
    }

    /// Record that XWayland has started with the given PID.
    pub fn set_pid(&mut self, pid: u32) {
        self.xwayland_pid = Some(pid);
    }

    /// Process an X11 protocol event forwarded by XWayland.
    ///
    /// Returns the X11 window ID if a new window was mapped (for Lunas to create
    /// a corresponding `ShellWindow`), or `None` otherwise.
    pub fn handle_x11_event(&mut self, data: &[u8], pid: u32) -> XwaylandAction {
        // Verify this came from the expected XWayland process.
        if let Some(xpid) = self.xwayland_pid {
            if xpid != pid {
                return XwaylandAction::None;
            }
        } else {
            // Accept the first X11 sender as XWayland.
            self.xwayland_pid = Some(pid);
        }

        // Parse minimal X11 event header: 1 byte event type.
        // X11 MapNotify event type = 19, UnmapNotify = 18, DestroyNotify = 17.
        if data.is_empty() {
            return XwaylandAction::None;
        }

        let event_type = data[0] & 0x7F; // strip send_event bit

        match event_type {
            19 => {
                // MapNotify: window_id at bytes 4..8
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                self.xwm.handle_map_request(window_id);
                XwaylandAction::MapWindow { window_id }
            }
            18 => {
                // UnmapNotify: window_id at bytes 4..8
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                XwaylandAction::UnmapWindow { window_id }
            }
            17 => {
                // DestroyNotify: window_id at bytes 4..8
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                self.xwm.windows.retain(|&w| w != window_id);
                XwaylandAction::DestroyWindow { window_id }
            }
            _ => XwaylandAction::None,
        }
    }

    /// Return whether XWayland is currently active.
    pub fn is_active(&self) -> bool {
        self.xwayland_pid.is_some()
    }
}

/// Actions that XWayland integration requests from the main state.
#[derive(Debug, PartialEq)]
pub enum XwaylandAction {
    None,
    /// An X11 window has been mapped (made visible).
    MapWindow { window_id: u32 },
    /// An X11 window has been unmapped (hidden).
    UnmapWindow { window_id: u32 },
    /// An X11 window has been destroyed.
    DestroyWindow { window_id: u32 },
}

/// Build a default `ShellWindow` for a newly created Wayland surface.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wayland_compositor_new() {
        let wc = WaylandCompositor::new();
        assert!(wc.connections.is_empty());
        assert!(wc.pending_responses.is_empty());
    }

    #[test]
    fn test_get_or_create_connection() {
        let mut wc = WaylandCompositor::new();
        wc.get_or_create_connection(42);
        assert_eq!(wc.connections.len(), 1);
        assert_eq!(wc.connections[0].pid, 42);
        // Calling again with same PID should not add a new connection.
        wc.get_or_create_connection(42);
        assert_eq!(wc.connections.len(), 1);
    }

    #[test]
    fn test_get_or_create_multiple_connections() {
        let mut wc = WaylandCompositor::new();
        wc.get_or_create_connection(1);
        wc.get_or_create_connection(2);
        wc.get_or_create_connection(3);
        assert_eq!(wc.connections.len(), 3);
    }

    #[test]
    fn test_disconnect_client() {
        let mut wc = WaylandCompositor::new();
        wc.get_or_create_connection(10);
        wc.get_or_create_connection(20);
        assert_eq!(wc.connections.len(), 2);
        wc.disconnect_client(10);
        assert_eq!(wc.connections.len(), 1);
        assert_eq!(wc.connections[0].pid, 20);
    }

    #[test]
    fn test_client_connection_alloc_surface() {
        let mut cc = ClientConnection::new(99);
        let id1 = cc.alloc_surface_id();
        let id2 = cc.alloc_surface_id();
        assert_ne!(id1, id2);
        assert_eq!(cc.surfaces.len(), 2);
    }

    #[test]
    fn test_register_and_query_surface_window() {
        let mut cc = ClientConnection::new(99);
        let sid = cc.alloc_surface_id();
        assert_eq!(cc.window_for_surface(sid), None);
        cc.set_window_for_surface(sid, 5);
        assert_eq!(cc.window_for_surface(sid), Some(5));
    }

    #[test]
    fn test_register_surface_window_via_compositor() {
        let mut wc = WaylandCompositor::new();
        wc.get_or_create_connection(7);
        wc.connections[0].surfaces.push((1, None));
        wc.register_surface_window(7, 1, 3);
        assert_eq!(wc.connections[0].window_for_surface(1), Some(3));
    }

    #[test]
    fn test_handle_message_too_short() {
        let mut wc = WaylandCompositor::new();
        let action = wc.handle_message(&[0u8; 4], 5);
        assert_eq!(action, WaylandAction::None);
    }

    #[test]
    fn test_handle_message_get_registry() {
        let mut wc = WaylandCompositor::new();
        // wl_display.get_registry: obj=1, size=12, op=1, new_id=2
        let mut msg = [0u8; 12];
        msg[0..4].copy_from_slice(&1u32.to_le_bytes()); // obj_id = 1 (wl_display)
        msg[4..8].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes()); // size=12, op=1
        msg[8..12].copy_from_slice(&2u32.to_le_bytes()); // new_id = 2 (registry)
        let action = wc.handle_message(&msg, 100);
        // get_registry doesn't create a surface → should be None
        assert_eq!(action, WaylandAction::None);
        // Connection should have been created
        assert_eq!(wc.connections.len(), 1);
    }

    #[test]
    fn test_handle_message_create_surface() {
        let mut wc = WaylandCompositor::new();
        // Simulate wl_compositor.create_surface: obj=4 (compositor), opcode=0, new_id=5
        let mut msg = [0u8; 12];
        msg[0..4].copy_from_slice(&4u32.to_le_bytes()); // obj_id = 4 (compositor)
        msg[4..8].copy_from_slice(&((12u32 << 16) | 0u32).to_le_bytes()); // size=12, op=0
        msg[8..12].copy_from_slice(&5u32.to_le_bytes()); // new surface id=5
        let action = wc.handle_message(&msg, 200);
        assert!(matches!(action, WaylandAction::CreateSurface { pid: 200, surface_id: 5, .. }));
    }

    #[test]
    fn test_handle_message_commit_surface() {
        let mut wc = WaylandCompositor::new();
        // Simulate wl_surface.commit: obj=5, opcode=6
        let mut msg = [0u8; 8];
        msg[0..4].copy_from_slice(&5u32.to_le_bytes()); // obj_id = 5 (surface)
        msg[4..8].copy_from_slice(&((8u32 << 16) | 6u32).to_le_bytes()); // size=8, op=6
        let action = wc.handle_message(&msg, 300);
        assert_eq!(action, WaylandAction::CommitSurface { pid: 300, surface_id: 5 });
    }

    #[test]
    fn test_max_connections_evicts_oldest() {
        let mut wc = WaylandCompositor::new();
        for i in 0..MAX_WAYLAND_CONNECTIONS {
            wc.get_or_create_connection(i as u32);
        }
        assert_eq!(wc.connections.len(), MAX_WAYLAND_CONNECTIONS);
        // Adding one more should evict the oldest (pid=0)
        wc.get_or_create_connection(99);
        assert_eq!(wc.connections.len(), MAX_WAYLAND_CONNECTIONS);
        assert!(!wc.connections.iter().any(|c| c.pid == 0));
        assert!(wc.connections.iter().any(|c| c.pid == 99));
    }

    // ── XwaylandIntegration tests ──

    #[test]
    fn test_xwayland_integration_new() {
        let xi = XwaylandIntegration::new();
        assert!(xi.xwayland_pid.is_none());
        assert!(!xi.is_active());
    }

    #[test]
    fn test_xwayland_set_pid() {
        let mut xi = XwaylandIntegration::new();
        xi.set_pid(999);
        assert_eq!(xi.xwayland_pid, Some(999));
        assert!(xi.is_active());
    }

    #[test]
    fn test_xwayland_handle_map_notify() {
        let mut xi = XwaylandIntegration::new();
        xi.set_pid(55);
        // X11 MapNotify: event_type=19, window_id at [4..8]
        let mut ev = [0u8; 32];
        ev[0] = 19; // MapNotify
        ev[4..8].copy_from_slice(&42u32.to_le_bytes()); // window_id
        let action = xi.handle_x11_event(&ev, 55);
        assert_eq!(action, XwaylandAction::MapWindow { window_id: 42 });
        assert!(xi.xwm.windows.contains(&42));
    }

    #[test]
    fn test_xwayland_handle_unmap_notify() {
        let mut xi = XwaylandIntegration::new();
        xi.set_pid(55);
        xi.xwm.handle_map_request(10);
        let mut ev = [0u8; 32];
        ev[0] = 18; // UnmapNotify
        ev[4..8].copy_from_slice(&10u32.to_le_bytes());
        let action = xi.handle_x11_event(&ev, 55);
        assert_eq!(action, XwaylandAction::UnmapWindow { window_id: 10 });
    }

    #[test]
    fn test_xwayland_handle_destroy_notify() {
        let mut xi = XwaylandIntegration::new();
        xi.set_pid(55);
        xi.xwm.handle_map_request(77);
        assert!(xi.xwm.windows.contains(&77));
        let mut ev = [0u8; 32];
        ev[0] = 17; // DestroyNotify
        ev[4..8].copy_from_slice(&77u32.to_le_bytes());
        let action = xi.handle_x11_event(&ev, 55);
        assert_eq!(action, XwaylandAction::DestroyWindow { window_id: 77 });
        assert!(!xi.xwm.windows.contains(&77));
    }

    #[test]
    fn test_xwayland_ignores_wrong_pid() {
        let mut xi = XwaylandIntegration::new();
        xi.set_pid(55);
        let mut ev = [0u8; 32];
        ev[0] = 19; // MapNotify
        ev[4..8].copy_from_slice(&1u32.to_le_bytes());
        let action = xi.handle_x11_event(&ev, 99); // wrong PID
        assert_eq!(action, XwaylandAction::None);
    }

    #[test]
    fn test_xwayland_auto_detect_pid() {
        let mut xi = XwaylandIntegration::new();
        // No PID set — first sender is registered as XWayland
        let mut ev = [0u8; 32];
        ev[0] = 19;
        ev[4..8].copy_from_slice(&3u32.to_le_bytes());
        let action = xi.handle_x11_event(&ev, 123);
        assert_eq!(action, XwaylandAction::MapWindow { window_id: 3 });
        assert_eq!(xi.xwayland_pid, Some(123));
    }

    #[test]
    fn test_make_wayland_window() {
        let win = make_wayland_window(5, 2, 1920, 1080, 0, b"MyApp");
        assert_eq!(win.content, WindowContent::Wayland { surface_id: 5, conn_idx: 2 });
        assert!(win.w > 0 && win.h > 0);
        assert_eq!(win.workspace, 0);
        assert_eq!(&win.title[..5], b"MyApp");
    }
}
