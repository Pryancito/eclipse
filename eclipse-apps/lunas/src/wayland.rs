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
    /// Map of object_id to its type.
    pub objects: alloc::vec::Vec<(u32, WaylandObjectType)>,
    /// Shared memory pools created by this client.
    pub pools: alloc::vec::Vec<ShmPool>,
    /// Buffers created by this client.
    pub buffers: alloc::vec::Vec<ShmBuffer>,
    /// Mapping of surface_id to its currently attached buffer_id.
    pub attached_buffers: alloc::vec::Vec<(u32, u32)>,
    /// Map of surface_id to its compositor window index.
    pub surfaces: alloc::vec::Vec<(u32, Option<usize>)>,
    /// Does the client have a registry?
    pub registry_id: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WaylandObjectType {
    Display,
    Registry,
    Compositor,
    Shm,
    Shell,
    Surface { id: u32 },
    ShellSurface { surface_id: u32 },
    ShmPool { id: u32 },
    ShmBuffer { id: u32 },
}

#[derive(Debug, Clone)]
pub struct ShmPool {
    pub id: u32,
    pub vaddr: usize,
    pub size: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ShmBuffer {
    pub id: u32,
    pub pool_id: u32,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
}

impl ClientConnection {
    pub fn new(pid: u32) -> Self {
        let mut objects = alloc::vec::Vec::new();
        objects.push((1, WaylandObjectType::Display));
        Self {
            pid,
            objects,
            pools: alloc::vec::Vec::new(),
            buffers: alloc::vec::Vec::new(),
            attached_buffers: alloc::vec::Vec::new(),
            surfaces: alloc::vec::Vec::new(),
            registry_id: None,
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

/// Default SHM pool size when no explicit size is provided (640×480 ARGB).
const DEFAULT_POOL_SIZE: usize = 640 * 480 * 4;

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

    /// Find or create connection
    fn get_or_create_connection(&mut self, pid: u32) -> usize {
        if let Some(idx) = self.connections.iter().position(|c| c.pid == pid) {
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
        }
    }

    pub fn register_surface_window(&mut self, pid: u32, surface_id: u32, w_idx: usize) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.pid == pid) {
            c.set_window_for_surface(surface_id, w_idx);
        }
    }

    /// Process one or more Wayland protocol messages from client `pid`.
    pub fn handle_message(
        &mut self,
        data: &[u8],
        pid: u32,
    ) -> heapless::Vec<WaylandAction, 8> {
        let mut actions = heapless::Vec::new();
        let mut offset = 0;

        let conn_idx = self.get_or_create_connection(pid);

        // Check for "WAYL" tag (4 bytes) if present, common in early implementations
        if data.len() >= 4 && &data[0..4] == b"WAYL" {
            offset = 4;
        }

        // New Wayland messages: [header: 8 bytes, payload...]
        while offset + 8 <= data.len() {
            let header_ptr = data[offset..offset+8].as_ptr() as *const sidewind::wayland::WaylandHeader;
            let header = unsafe { core::ptr::read_unaligned(header_ptr) };
            
            let size = header.length as usize;
            if size < 8 || offset + size > data.len() {
                break;
            }

            let msg_data = &data[offset..offset+size];
            let obj_id = header.object_id;
            let opcode = header.opcode;
            println!("[LUNAS-WAYL] Recv: obj={} op={} len={} from={}", obj_id, opcode, size, pid);

            let action = self.process_single_message(conn_idx, &header, msg_data, pid);
            if action != WaylandAction::None {
                let _ = actions.push(action);
            }

            // Move to next message (must be 4-byte aligned)
            let next_offset = offset + ((size + 3) & !3);
            if next_offset <= offset { break; } 
            offset = next_offset;
        }

        actions
    }

    fn process_single_message(
        &mut self,
        conn_idx: usize,
        header: &sidewind::wayland::WaylandHeader,
        data: &[u8],
        pid: u32,
    ) -> WaylandAction {
        use sidewind::wayland::{ID_COMPOSITOR, ID_SHM};
        
        match header.object_id {
            ID_COMPOSITOR => {
                if header.opcode == 1 && data.len() >= 16 { // EDP CreateSurface (opcode 1)
                    let msg_ptr = data.as_ptr() as *const sidewind::wayland::WaylandMsgCreateSurface;
                    let msg = unsafe { core::ptr::read_unaligned(msg_ptr) };
                    let new_id = msg.new_id;
                    println!("[LUNAS-WAYL] Create Surface: id={} from={}", new_id, pid);
                    self.connections[conn_idx].objects.push((new_id, WaylandObjectType::Surface { id: new_id }));
                    self.connections[conn_idx].surfaces.push((new_id, None));
                    return WaylandAction::CreateSurface { 
                        pid, 
                        surface_id: msg.new_id, 
                        conn_idx,
                        width: msg.width,
                        height: msg.height,
                    };
                } else if header.opcode == 1 && data.len() >= 12 {
                    // wl_display.get_registry: store the registry object ID
                    let registry_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    self.connections[conn_idx].registry_id = Some(registry_id);
                    self.connections[conn_idx].objects.push((registry_id, WaylandObjectType::Registry));
                }
            }
            ID_SHM => {
                if header.opcode == 1 && data.len() >= 20 { // EDP CreatePool (opcode 1)
                    return self.handle_create_pool(conn_idx, data, pid);
                }
            }
            _ => {
                let obj_type = self.connections[conn_idx].objects.iter()
                    .find(|(id, _)| *id == header.object_id)
                    .map(|(_, t)| *t);

                match obj_type {
                    Some(WaylandObjectType::Compositor) => {
                        // Standard wl_compositor.create_surface (opcode 0)
                        if header.opcode == 0 && data.len() >= 12 {
                            let new_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                            let width = if data.len() >= 14 { u16::from_le_bytes([data[12], data[13]]) } else { 0 };
                            let height = if data.len() >= 16 { u16::from_le_bytes([data[14], data[15]]) } else { 0 };
                            println!("[LUNAS-WAYL] Create Surface (std): id={} from={}", new_id, pid);
                            self.connections[conn_idx].objects.push((new_id, WaylandObjectType::Surface { id: new_id }));
                            self.connections[conn_idx].surfaces.push((new_id, None));
                            return WaylandAction::CreateSurface { pid, surface_id: new_id, conn_idx, width, height };
                        }
                    }
                    Some(WaylandObjectType::Shm) => {
                        // Standard wl_shm.create_pool (opcode 0)
                        if header.opcode == 0 && data.len() >= 12 {
                            return self.handle_create_pool(conn_idx, data, pid);
                        }
                    }
                    Some(WaylandObjectType::ShmPool { id: pool_id }) => {
                        // Standard wl_shm_pool.create_buffer (opcode 0): store buffer dimensions
                        if header.opcode == 0 && data.len() >= 24 {
                            let buffer_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                            let width  = i32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                            let height = i32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                            self.connections[conn_idx].buffers.push(ShmBuffer {
                                id: buffer_id,
                                pool_id,
                                offset: 0,
                                width,
                                height,
                                stride: width * 4,
                                format: 0,
                            });
                            self.connections[conn_idx].objects.push((buffer_id, WaylandObjectType::ShmBuffer { id: buffer_id }));
                            // Buffer creation itself has no window-level action; commit triggers display.
                        }
                    }
                    Some(WaylandObjectType::Surface { id: surface_id }) => {
                        if header.opcode == 1 && data.len() >= 24 {
                            // EDP CommitFrame: 32-byte message includes direct vaddr;
                            // 24-byte legacy message falls back to pool lookup only.
                            let msg_ptr = data.as_ptr() as *const sidewind::wayland::WaylandMsgCommitFrame;
                            let msg = unsafe { core::ptr::read_unaligned(msg_ptr) };
                            if let Some(vaddr) = self.resolve_commit_vaddr(conn_idx, &msg) {
                                return WaylandAction::CommitFrame {
                                    pid, surface_id,
                                    pool_id: msg.pool_id,
                                    offset: msg.offset,
                                    width: msg.width,
                                    height: msg.height,
                                    stride: msg.stride,
                                    format: msg.format,
                                    vaddr,
                                };
                            }
                        } else if header.opcode == 1 && data.len() >= 12 {
                            // Standard wl_surface.attach (buffer_id at data[8..12])
                            let buffer_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                            if let Some(buf) = self.connections[conn_idx].buffers.iter().find(|b| b.id == buffer_id).copied() {
                                let vaddr = self.connections[conn_idx].pools.iter()
                                    .find(|p| p.id == buf.pool_id)
                                    .map(|p| p.vaddr + buf.offset as usize)
                                    .unwrap_or(0);
                                return WaylandAction::AttachBuffer { pid, surface_id, buffer_id, width: buf.width, height: buf.height, vaddr };
                            }
                        } else if header.opcode == 6 {
                            // Standard wl_surface.commit
                            return WaylandAction::CommitSurface { pid, surface_id };
                        } else if header.opcode == 2 && data.len() >= 40 {
                            // SetTitle: 32-byte null-terminated title after the 8-byte header
                            let mut title = [0u8; 32];
                            title.copy_from_slice(&data[8..40]);
                            return WaylandAction::SetTitle { pid, surface_id, title };
                        }
                    }
                    _ => {
                        // Fallback for unknown object types: if a client sends opcode 0 with
                        // a payload large enough to hold a new_id (≥12 bytes), treat the
                        // request as wl_compositor.create_surface.  This accommodates clients
                        // that bind wl_compositor to an arbitrary object ID without going
                        // through a full registry handshake (e.g. tests, simplified EDP clients).
                        if header.opcode == 0 && data.len() >= 12 {
                            let new_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                            let width  = if data.len() >= 14 { u16::from_le_bytes([data[12], data[13]]) } else { 0 };
                            let height = if data.len() >= 16 { u16::from_le_bytes([data[14], data[15]]) } else { 0 };
                            println!("[LUNAS-WAYL] Create Surface (implicit): id={} from={}", new_id, pid);
                            self.connections[conn_idx].objects.push((new_id, WaylandObjectType::Surface { id: new_id }));
                            self.connections[conn_idx].surfaces.push((new_id, None));
                            return WaylandAction::CreateSurface { pid, surface_id: new_id, conn_idx, width, height };
                        }
                    }
                } // end match obj_type
            }
        }

        WaylandAction::None
    }

    /// Common pool creation logic used by both EDP (ID_SHM) and standard (wl_shm object type).
    fn handle_create_pool(&mut self, conn_idx: usize, data: &[u8], pid: u32) -> WaylandAction {
        // new_id at data[8..12], size at data[12..16] (EDP) or data[16..20] (std - fallback)
        if data.len() < 12 { return WaylandAction::None; }
        let new_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let pool_size = if data.len() >= 16 {
            u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize
        } else { 0 };
        // Try to mmap /tmp/Terminal for the shared pixel buffer
        let path = b"/tmp/Terminal\0";
        let fd = unsafe { libc::open(path.as_ptr() as *const core::ffi::c_char, libc::O_RDONLY, 0) };
        let vaddr = if fd >= 0 {
            let sz = if pool_size > 0 { pool_size } else { DEFAULT_POOL_SIZE };
            let v = unsafe { libc::mmap(core::ptr::null_mut(), sz, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
            unsafe { libc::close(fd); }
            if !v.is_null() && v != (-1isize as *mut core::ffi::c_void) {
                println!("[LUNAS-WAYL] SHM pool {} mmap OK vaddr={:p} from={}", new_id, v, pid);
                v as usize
            } else { 0 }
        } else { 0 };
        println!("[LUNAS-WAYL] Created SHM pool {} size={} vaddr={:#x} from={}", new_id, pool_size, vaddr, pid);
        self.connections[conn_idx].pools.push(ShmPool { id: new_id, vaddr, size: pool_size });
        self.connections[conn_idx].objects.push((new_id, WaylandObjectType::ShmPool { id: new_id }));
        WaylandAction::None
    }

    /// Resolve the pixel-buffer vaddr for a CommitFrame message.
    ///
    /// Priority:
    ///   1. pool-based address (pool.vaddr + offset) when the pool mmap succeeded (vaddr != 0)
    ///   2. direct vaddr from the message (client's virtual address, valid when the OS uses a
    ///      shared address space or the client passes a kernel-visible address)
    ///
    /// Returns `None` if neither source provides a valid (non-zero) address.
    fn resolve_commit_vaddr(
        &self,
        conn_idx: usize,
        msg: &sidewind::wayland::WaylandMsgCommitFrame,
    ) -> Option<usize> {
        // Try pool-based address first, but only when the pool mmap succeeded.
        // If pool.vaddr is 0 (mmap failed or returned a null mapping), fall through so
        // we do not blit from address 0 and corrupt the framebuffer with stale data.
        if let Some(pool) = self.connections[conn_idx].pools.iter().find(|p| p.id == msg.pool_id) {
            if pool.vaddr != 0 {
                return Some(pool.vaddr + msg.offset as usize);
            }
        }
        // Fall back to the direct virtual address embedded in the commit message.
        if msg.vaddr != 0 {
            Some(msg.vaddr as usize)
        } else {
            None
        }
    }

    /// Construct and queue global registry events. (No longer used in EDP)
    fn respond_with_globals(&mut self, _pid: u32, _registry_id: u32) {
    }

    pub fn disconnect_client(&mut self, pid: u32) {
        self.connections.retain(|c| c.pid != pid);
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WaylandAction {
    None,
    CreateSurface { pid: u32, surface_id: u32, conn_idx: usize, width: u16, height: u16 },
    CommitFrame { 
        pid: u32, 
        surface_id: u32, 
        pool_id: u32, 
        offset: u32,
        width: u16,
        height: u16,
        stride: u16,
        format: u16,
        vaddr: usize,
    },
    DestroySurface { pid: u32, surface_id: u32 },
    /// Buffer attached to surface (standard Wayland wl_surface.attach).
    AttachBuffer { pid: u32, surface_id: u32, buffer_id: u32, width: i32, height: i32, vaddr: usize },
    /// Surface committed (standard Wayland wl_surface.commit, opcode 6).
    CommitSurface { pid: u32, surface_id: u32 },
    /// Client requested a window-title change.
    SetTitle { pid: u32, surface_id: u32, title: [u8; 32] },
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
                println!("[LUNAS-XWAYL] MapNotify window={:#x} from={}", window_id, pid);
                (&mut self.xwm).handle_map_request(window_id);
                XwaylandAction::MapWindow { window_id }
            }
            18 => { // UnmapNotify
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                println!("[LUNAS-XWAYL] UnmapNotify window={:#x} from={}", window_id, pid);
                XwaylandAction::UnmapWindow { window_id }
            }
            17 => { // DestroyNotify
                if data.len() < 8 { return XwaylandAction::None; }
                let window_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                println!("[LUNAS-XWAYL] DestroyNotify window={:#x} from={}", window_id, pid);
                self.xwm.windows.retain(|&w| w != window_id);
                XwaylandAction::DestroyWindow { window_id }
            }
            _ => {
                println!("[LUNAS-XWAYL] Unknown X11 event type={} len={} from={}", event_type, data.len(), pid);
                XwaylandAction::None
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wayland_handshake() {
        let mut compositor = WaylandCompositor::new();
        let pid = 100u32;
        let registry_id = 2u32;
        
        let mut msg = [0u8; 12];
        msg[0..4].copy_from_slice(&1u32.to_le_bytes());
        msg[4..8].copy_from_slice(&((12u32 << 16) | 1u16 as u32).to_le_bytes());
        msg[8..12].copy_from_slice(&registry_id.to_le_bytes());

        let _actions = compositor.handle_message(&msg, pid);

        // In the simplified EDP protocol, get_registry stores the registry_id in the
        // connection but does not send a globals response. Verify the registry is stored.
        let conn = compositor.connections.iter().find(|c| c.pid == pid).expect("connection created");
        assert_eq!(conn.registry_id, Some(registry_id), "registry_id stored in connection");
    }

    #[test]
    fn test_wayland_create_surface() {
        let mut compositor = WaylandCompositor::new();
        let pid = 100u32;
        let surface_id = 7u32;
        
        let mut msg = [0u8; 12];
        msg[0..4].copy_from_slice(&4u32.to_le_bytes());
        msg[4..8].copy_from_slice(&((12u32 << 16) | 0u16 as u32).to_le_bytes());
        msg[8..12].copy_from_slice(&surface_id.to_le_bytes());

        let actions = compositor.handle_message(&msg, pid);
        assert_eq!(actions.len(), 1);
        match actions[0] {
            WaylandAction::CreateSurface { pid: p, surface_id: s, .. } => {
                assert_eq!(p, pid);
                assert_eq!(s, surface_id);
            }
            _ => panic!("Expected CreateSurface action"),
        }
    }

    #[test]
    fn test_wayland_shm_pool() {
        let mut compositor = WaylandCompositor::new();
        let pid = 101u32;
        
        // Manual setup
        let idx = compositor.get_or_create_connection(pid);
        compositor.connections[idx].objects.push((5, WaylandObjectType::Shm));

        let mut msg = [0u8; 20];
        msg[0..4].copy_from_slice(&5u32.to_le_bytes()); 
        msg[4..8].copy_from_slice(&((20u32 << 16) | 0u32).to_le_bytes());
        msg[8..12].copy_from_slice(&10u32.to_le_bytes()); // pool_id
        msg[16..20].copy_from_slice(&4096u32.to_le_bytes());

        let _ = compositor.handle_message(&msg, pid);
        assert_eq!(compositor.connections[0].pools.len(), 1);
        assert_eq!(compositor.connections[0].pools[0].id, 10);
    }

    #[test]
    fn test_wayland_surface_lifecycle() {
        let mut compositor = WaylandCompositor::new();
        let pid = 102u32;
        let surface_id = 7u32;
        let buffer_id = 8u32;
        let pool_id = 10u32;
        
        let idx = compositor.get_or_create_connection(pid);
        compositor.connections[idx].objects.push((4, WaylandObjectType::Compositor));
        compositor.connections[idx].objects.push((5, WaylandObjectType::Shm));

        // 1. Create Pool
        let mut pool_msg = [0u8; 20];
        pool_msg[0..4].copy_from_slice(&5u32.to_le_bytes());
        pool_msg[4..8].copy_from_slice(&((20u32 << 16) | 0u32).to_le_bytes());
        pool_msg[8..12].copy_from_slice(&pool_id.to_le_bytes());
        pool_msg[16..20].copy_from_slice(&4096u32.to_le_bytes());
        compositor.handle_message(&pool_msg, pid);

        // 2. Create Buffer from Pool
        let mut buf_msg = [0u8; 32];
        buf_msg[0..4].copy_from_slice(&pool_id.to_le_bytes());
        buf_msg[4..8].copy_from_slice(&((32u32 << 16) | 0u32).to_le_bytes());
        buf_msg[8..12].copy_from_slice(&buffer_id.to_le_bytes());
        buf_msg[16..20].copy_from_slice(&64i32.to_le_bytes()); // width
        buf_msg[20..24].copy_from_slice(&64i32.to_le_bytes()); // height
        compositor.handle_message(&buf_msg, pid);

        // 3. Create Surface
        let mut surf_msg = [0u8; 12];
        surf_msg[0..4].copy_from_slice(&4u32.to_le_bytes()); 
        surf_msg[4..8].copy_from_slice(&((12u32 << 16) | 0u32).to_le_bytes()); 
        surf_msg[8..12].copy_from_slice(&surface_id.to_le_bytes());
        compositor.handle_message(&surf_msg, pid);

        // 4. Attach Buffer to Surface
        let mut attach_msg = [0u8; 20];
        attach_msg[0..4].copy_from_slice(&surface_id.to_le_bytes());
        attach_msg[4..8].copy_from_slice(&((20u32 << 16) | 1u32).to_le_bytes());
        attach_msg[8..12].copy_from_slice(&buffer_id.to_le_bytes());
        let actions = compositor.handle_message(&attach_msg, pid);
        assert_eq!(actions.len(), 1);
        match actions[0] {
            WaylandAction::AttachBuffer { width, height, .. } => {
                assert_eq!(width, 64);
                assert_eq!(height, 64);
            }
            _ => panic!("Expected AttachBuffer action"),
        }

        // 5. Commit
        let mut commit_msg = [0u8; 8];
        commit_msg[0..4].copy_from_slice(&surface_id.to_le_bytes());
        commit_msg[4..8].copy_from_slice(&((8u32 << 16) | 6u32).to_le_bytes());
        let actions = compositor.handle_message(&commit_msg, pid);
        assert_eq!(actions[0], WaylandAction::CommitSurface { pid, surface_id });
    }

    #[test]
    fn test_wayland_multi_client_isolation() {
        let mut compositor = WaylandCompositor::new();
        let pid1 = 101u32;
        let pid2 = 102u32;

        let idx1 = compositor.get_or_create_connection(pid1);
        compositor.connections[idx1].objects.push((4, WaylandObjectType::Compositor));

        let mut surf_msg = [0u8; 12];
        surf_msg[0..4].copy_from_slice(&4u32.to_le_bytes()); 
        surf_msg[4..8].copy_from_slice(&((12u32 << 16) | 0u32).to_le_bytes());
        surf_msg[8..12].copy_from_slice(&7u32.to_le_bytes());
        compositor.handle_message(&surf_msg, pid1);

        assert_eq!(compositor.connections.len(), 1);
        assert_eq!(compositor.connections[0].surfaces.len(), 1);

        let idx2 = compositor.get_or_create_connection(pid2);
        compositor.connections[idx2].objects.push((4, WaylandObjectType::Compositor));
        surf_msg[8..12].copy_from_slice(&8u32.to_le_bytes());
        compositor.handle_message(&surf_msg, pid2);

        assert_eq!(compositor.connections.len(), 2);
    }

    /// When the pool mmap fails (pool.vaddr == 0), resolve_commit_vaddr must fall back to the
    /// direct vaddr embedded in the CommitFrame message instead of returning Some(0).
    /// Returning Some(0) would cause the compositor to blit from address 0, reading the physical
    /// framebuffer and rendering stale compositor output inside the client window's content area.
    #[test]
    fn test_resolve_commit_vaddr_falls_back_to_msg_vaddr_when_pool_vaddr_zero() {
        use sidewind::wayland::{WaylandHeader, WaylandMsgCommitFrame};

        let mut compositor = WaylandCompositor::new();
        let pid = 200u32;
        let pool_id = 0x1001u32;
        let surface_id = 0x2001u32;
        let direct_vaddr: u64 = 0x5000_0000;

        let idx = compositor.get_or_create_connection(pid);
        // Add the pool with vaddr=0 (simulates a failed mmap).
        compositor.connections[idx].pools.push(ShmPool { id: pool_id, vaddr: 0, size: 4096 });
        compositor.connections[idx].objects.push((surface_id, WaylandObjectType::Surface { id: surface_id }));
        compositor.connections[idx].surfaces.push((surface_id, None));

        // Build an EDP CommitFrame message with direct vaddr set.
        let mut commit = WaylandMsgCommitFrame::default();
        commit.header = WaylandHeader::new(surface_id, 1, 32);
        commit.pool_id = pool_id;
        commit.offset = 0;
        commit.width = 640;
        commit.height = 480;
        commit.stride = 2560;
        commit.format = 0;
        commit.vaddr = direct_vaddr;

        let mut buf = [0u8; 32];
        unsafe { core::ptr::write_unaligned(buf.as_mut_ptr() as *mut WaylandMsgCommitFrame, commit); }

        let actions = compositor.handle_message(&buf, pid);

        // The CommitFrame action must carry the direct_vaddr, NOT 0.
        assert_eq!(actions.len(), 1, "expected one CommitFrame action");
        match actions[0] {
            WaylandAction::CommitFrame { vaddr, .. } => {
                assert_ne!(vaddr, 0, "vaddr must not be 0 (pool mmap failed; must fall back to msg.vaddr)");
                assert_eq!(vaddr, direct_vaddr as usize, "vaddr must equal the direct address from the message");
            }
            _ => panic!("expected CommitFrame action, got {:?}", actions[0]),
        }
    }

    /// When pool.vaddr is valid (non-zero), resolve_commit_vaddr must prefer it over msg.vaddr.
    #[test]
    fn test_resolve_commit_vaddr_prefers_pool_vaddr_when_valid() {
        use sidewind::wayland::{WaylandHeader, WaylandMsgCommitFrame};

        let mut compositor = WaylandCompositor::new();
        let pid = 201u32;
        let pool_id = 0x1002u32;
        let surface_id = 0x2002u32;
        let pool_vaddr: usize = 0x4000_0000;
        let msg_vaddr: u64 = 0x5000_0000;

        let idx = compositor.get_or_create_connection(pid);
        compositor.connections[idx].pools.push(ShmPool { id: pool_id, vaddr: pool_vaddr, size: 4096 });
        compositor.connections[idx].objects.push((surface_id, WaylandObjectType::Surface { id: surface_id }));
        compositor.connections[idx].surfaces.push((surface_id, None));

        let mut commit = WaylandMsgCommitFrame::default();
        commit.header = WaylandHeader::new(surface_id, 1, 32);
        commit.pool_id = pool_id;
        commit.offset = 0;
        commit.width = 64;
        commit.height = 64;
        commit.stride = 256;
        commit.format = 0;
        commit.vaddr = msg_vaddr;

        let mut buf = [0u8; 32];
        unsafe { core::ptr::write_unaligned(buf.as_mut_ptr() as *mut WaylandMsgCommitFrame, commit); }

        let actions = compositor.handle_message(&buf, pid);
        assert_eq!(actions.len(), 1, "expected one CommitFrame action");
        match actions[0] {
            WaylandAction::CommitFrame { vaddr, .. } => {
                assert_eq!(vaddr, pool_vaddr, "pool vaddr must be preferred over msg.vaddr when pool mmap succeeded");
            }
            _ => panic!("expected CommitFrame action, got {:?}", actions[0]),
        }
    }
}
