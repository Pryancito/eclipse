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

        // Native Wayland messages: [obj_id: u32, size_op: u32, ...]
        while offset + 8 <= data.len() {
            let obj_id = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
            let size_op = u32::from_le_bytes([data[offset+4], data[offset+5], data[offset+6], data[offset+7]]);
            let opcode = (size_op & 0xFFFF) as u16;
            let size = (size_op >> 16) as usize;

            if size < 8 || offset + size > data.len() {
                if offset + 8 == data.len() && size == 0 { break; }
                println!("[LUNAS-WAYL] Error: Invalid message size {} at offset {} (total={}) from PID {}", size, offset, data.len(), pid);
                break;
            }

            let msg_data = &data[offset..offset+size];
            println!("[LUNAS-WAYL] Recv: obj={} op={} len={} from={}", obj_id, opcode, size, pid);

            let action = self.process_single_message(conn_idx, obj_id, opcode, msg_data, pid);
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
        obj_id: u32,
        opcode: u16,
        data: &[u8],
        pid: u32,
    ) -> WaylandAction {
        // Find object type
        let obj_type = self.connections[conn_idx].objects.iter()
            .find(|(id, _)| *id == obj_id)
            .map(|(_, t)| *t);

        match obj_type {
            Some(WaylandObjectType::Display) => {
                if opcode == 1 { // get_registry(new_id)
                    if data.len() >= 12 {
                        let registry_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                        self.connections[conn_idx].registry_id = Some(registry_id);
                        self.connections[conn_idx].objects.push((registry_id, WaylandObjectType::Registry));
                        self.respond_with_globals(pid, registry_id);
                    }
                }
            }
            Some(WaylandObjectType::Registry) => {
                if opcode == 0 && data.len() >= 16 { // bind(name, interface, version, new_id)
                    let name = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    let if_len = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
                    let padded_len = (if_len + 3) & !3;
                    let version_idx = 16 + padded_len;
                    let new_id_idx = 20 + padded_len;
                    
                    if data.len() >= new_id_idx + 4 {
                        let new_id = u32::from_le_bytes([data[new_id_idx], data[new_id_idx+1], data[new_id_idx+2], data[new_id_idx+3]]);
                        
                        let interface_name = if if_len >= 13 && &data[16..16+13] == b"wl_compositor" {
                            Some(WaylandObjectType::Compositor)
                        } else if if_len >= 6 && &data[16..16+6] == b"wl_shm" {
                            Some(WaylandObjectType::Shm)
                        } else if if_len >= 8 && &data[16..16+8] == b"wl_shell" {
                            Some(WaylandObjectType::Shell)
                        } else {
                            None
                        };

                        if let Some(t) = interface_name {
                            println!("[LUNAS-WAYL] Bind: name={} new_id={} (type={:?}) from={}", name, new_id, t, pid);
                            self.connections[conn_idx].objects.push((new_id, t));
                        }
                    }
                }
            }
            Some(WaylandObjectType::Compositor) => {
                if opcode == 0 && data.len() >= 12 { // create_surface(new_id)
                    let surface_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    println!("[LUNAS-WAYL] Create Surface: id={} from={}", surface_id, pid);
                    self.connections[conn_idx].objects.push((surface_id, WaylandObjectType::Surface { id: surface_id }));
                    self.connections[conn_idx].surfaces.push((surface_id, None));
                    return WaylandAction::CreateSurface { pid, surface_id, conn_idx };
                }
            }
            Some(WaylandObjectType::Shm) => {
                if opcode == 0 && data.len() >= 20 { // create_pool(new_id, fd, size)
                    let pool_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    let size = i32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
                    
                    let path = b"/tmp/Terminal\0";
                    let fd = unsafe { libc::open(path.as_ptr() as *const core::ffi::c_char, libc::O_RDONLY, 0) };
                    if fd >= 0 {
                        let vaddr = unsafe { libc::mmap(core::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
                        unsafe { libc::close(fd); }
                        if !vaddr.is_null() && vaddr != (-1isize as *mut core::ffi::c_void) {
                            println!("[LUNAS-WAYL] Created SHM pool {} size={} vaddr={:p} from={}", pool_id, size, vaddr, pid);
                            self.connections[conn_idx].pools.push(ShmPool { id: pool_id, vaddr: vaddr as usize, size });
                            self.connections[conn_idx].objects.push((pool_id, WaylandObjectType::ShmPool { id: pool_id }));
                        } else {
                            println!("[LUNAS-WAYL] Error: mmap failed for pool {} size={} from={}", pool_id, size, pid);
                        }
                    } else {
                        println!("[LUNAS-WAYL] Error: open /tmp/Terminal failed for pool {} from={}", pool_id, pid);
                    }
                }
            }
            Some(WaylandObjectType::Shell) => {
                if opcode == 0 && data.len() >= 16 { // get_shell_surface(new_id, surface)
                    let new_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    let surface_id = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                    println!("[LUNAS-WAYL] Shell: get_shell_surface new_id={} surface={} from={}", new_id, surface_id, pid);
                    self.connections[conn_idx].objects.push((new_id, WaylandObjectType::ShellSurface { surface_id }));
                }
            }
            Some(WaylandObjectType::Surface { id: surface_id }) => {
                if opcode == 1 { // attach(buffer, x, y)
                    if data.len() >= 20 {
                        let buffer_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                        println!("[LUNAS-WAYL] Surface {}: attach buffer {} from={}", surface_id, buffer_id, pid);
                        self.connections[conn_idx].attached_buffers.retain(|(s, _)| *s != surface_id);
                        self.connections[conn_idx].attached_buffers.push((surface_id, buffer_id));
                        
                        if let Some(buffer) = self.connections[conn_idx].buffers.iter().find(|b| b.id == buffer_id) {
                            if let Some(pool) = self.connections[conn_idx].pools.iter().find(|p| p.id == buffer.pool_id) {
                                let vaddr = pool.vaddr + buffer.offset as usize;
                                return WaylandAction::AttachBuffer { 
                                    pid, surface_id, vaddr,
                                    width: buffer.width, height: buffer.height,
                                };
                            }
                        }
                    }
                } else if opcode == 6 { // commit
                    println!("[LUNAS-WAYL] Surface {}: commit from={}", surface_id, pid);
                    return WaylandAction::CommitSurface { pid, surface_id };
                }
            }
            Some(WaylandObjectType::ShellSurface { surface_id }) => {
                if opcode == 1 { // set_toplevel
                    println!("[LUNAS-WAYL] ShellSurface for surface {}: set_toplevel from={}", surface_id, pid);
                }
            }
            Some(WaylandObjectType::ShmPool { id: pool_id }) => {
                if opcode == 0 && data.len() >= 32 { // create_buffer(new_id, offset, width, height, stride, format)
                    let buffer_id = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    let offset = i32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                    let width = i32::from_le_bytes([data[16], data[17], data[18], data[19]]);
                    let height = i32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                    let stride = i32::from_le_bytes([data[24], data[25], data[26], data[27]]);
                    let format = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);
                    println!("[LUNAS-WAYL] Created Buffer {} from pool {} ({}x{}, offset={})", buffer_id, pool_id, width, height, offset);
                    self.connections[conn_idx].buffers.push(ShmBuffer {
                        id: buffer_id, pool_id, offset, width, height, stride, format
                    });
                    self.connections[conn_idx].objects.push((buffer_id, WaylandObjectType::ShmBuffer { id: buffer_id }));
                }
            }
            _ => {
                println!("[LUNAS-WAYL] Warn: Unknown object {} or opcode {} from={}. Hex: {:?}", obj_id, opcode, pid, &data[..core::cmp::min(data.len(), 16)]);
            }
        }

        WaylandAction::None
    }

    /// Construct and queue global registry events.
    fn respond_with_globals(&mut self, pid: u32, registry_id: u32) {
        println!("[LUNAS-WAYL] Sending globals to PID {} (reg_id={})", pid, registry_id);
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

        println!("[LUNAS-WAYL] Queued glob message size: {}", globals.len());
        self.pending_responses.push((pid, globals));
    }

    pub fn disconnect_client(&mut self, pid: u32) {
        self.connections.retain(|c| c.pid != pid);
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WaylandAction {
    None,
    CreateSurface { pid: u32, surface_id: u32, conn_idx: usize },
    AttachBuffer { pid: u32, surface_id: u32, vaddr: usize, width: i32, height: i32 },
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

        let actions = compositor.handle_message(&msg, pid);
        assert_eq!(actions.len(), 1);
        
        assert_eq!(compositor.pending_responses.len(), 1);
        let (resp_pid, data) = &compositor.pending_responses[0];
        assert_eq!(*resp_pid, pid);
        assert_eq!(data.len(), 96);
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
}
