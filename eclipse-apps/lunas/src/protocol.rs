//! Sidewind Native Protocol (SNP) Compositor implementation for Lunas.
//!
//! Handles surface life-cycle, buffer management and event dispatching
//! for SNP clients (e.g. Terminal) via Unified Ring Buffer (URB).

use std::prelude::v1::*;
use sidewind::protocol::*;
use crate::compositor::{ShellWindow, WindowContent};

/// Maximum concurrent SNP client connections.
pub const MAX_SNP_CONNECTIONS: usize = 32;

pub struct ClientConnection {
    pub pid: u32,
    pub surfaces: Vec<u32>, // IDs of surfaces owned by this client
    pub pending_events: Vec<SnpCommand>,
    pub ring_ptr: *mut SnpRingControl,
    pub commands_ptr: *mut SnpCommand,
}

impl ClientConnection {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            surfaces: Vec::new(),
            pending_events: Vec::new(),
            ring_ptr: core::ptr::null_mut(),
            commands_ptr: core::ptr::null_mut(),
        }
    }
}

pub struct SnpCompositor {
    pub connections: Vec<ClientConnection>,
}

impl SnpCompositor {
    pub fn new() -> Self {
        Self {
            connections: Vec::with_capacity(MAX_SNP_CONNECTIONS),
        }
    }

    pub fn get_or_create_connection(&mut self, pid: u32) -> &mut ClientConnection {
        if let Some(idx) = self.connections.iter().position(|c| c.pid == pid) {
            &mut self.connections[idx]
        } else {
            if self.connections.len() >= MAX_SNP_CONNECTIONS {
                self.connections.remove(0);
            }
            self.connections.push(ClientConnection::new(pid));
            self.connections.last_mut().unwrap()
        }
    }

    /// Process all active rings and return high-level actions.
    pub fn poll_active_rings(&mut self) -> Vec<SnpAction> {
        let mut actions = Vec::new();
        // Use a loop with indexing to avoid long-lived mutable borrow of self.connections
        for i in 0..self.connections.len() {
            let conn = &mut self.connections[i];
            if conn.ring_ptr.is_null() { continue; }
            
            unsafe {
                let ring = &mut *conn.ring_ptr;
                while ring.head != ring.tail {
                    let cmd_idx = ring.head % ring.size;
                    let cmd = &*conn.commands_ptr.add(cmd_idx as usize);
                    
                    let action = Self::process_command_static(cmd, conn.pid);
                    if action != SnpAction::None {
                        if let SnpAction::CreateSurface { surface_id, .. } = action {
                            conn.surfaces.push(surface_id);
                        }
                        actions.push(action);
                    }
                    
                    ring.head = ring.head.wrapping_add(1);
                }
            }
        }
        actions
    }

    fn process_command_static(cmd: &SnpCommand, pid: u32) -> SnpAction {
        let opcode = unsafe { core::mem::transmute::<u32, SnpOpcode>(cmd.opcode) };
        
        match opcode {
            SnpOpcode::LayerCreate => {
                let msg = unsafe { cmd.get_payload::<SnpPayloadLayerCreate>() };
                return SnpAction::CreateSurface { 
                    pid, 
                    surface_id: cmd.layer_id, 
                    width: msg.width, 
                    height: msg.height,
                    name: msg.name,
                };
            }
            SnpOpcode::Commit => {
                return SnpAction::CommitSurface { 
                    pid, 
                    surface_id: cmd.layer_id,
                    fence: cmd.fence,
                };
            }
            SnpOpcode::Destroy => {
                return SnpAction::SurfaceDestroy { pid, surface_id: cmd.layer_id };
            }
            _ => SnpAction::None,
        }
    }

    /// Process an SNP message from client (Fallback/Bootstrap).
    pub fn handle_message(&mut self, data: &[u8], pid: u32) -> Vec<SnpAction> {
        let mut actions = Vec::new();
        if data.len() >= 64 {
            let cmd_ptr = data.as_ptr() as *const SnpCommand;
            let cmd = unsafe { core::ptr::read_unaligned(cmd_ptr) };
            let action = Self::process_command_static(&cmd, pid);
            if action != SnpAction::None {
                actions.push(action);
                
                // Track surface
                if let SnpAction::CreateSurface { surface_id, .. } = action {
                    let conn = self.get_or_create_connection(pid);
                    conn.surfaces.push(surface_id);
                }
            }
        }
        actions
    }

    pub fn send_event(&mut self, pid: u32, obj_id: u32, opcode: SnpOpcode, payload: &[u8]) {
        if let Some(conn) = self.connections.iter_mut().find(|c| c.pid == pid) {
            let mut cmd = SnpCommand::new(opcode, obj_id);
            let len = payload.len().min(32);
            cmd.payload[..len].copy_from_slice(&payload[..len]);
            conn.pending_events.push(cmd);
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SnpAction {
    None,
    CreateSurface { pid: u32, surface_id: u32, width: u16, height: u16, name: [u8; 24] },
    AttachBuffer { pid: u32, surface_id: u32, buffer_id: u32 },
    CommitSurface { pid: u32, surface_id: u32, fence: u64 },
    SetTitle { pid: u32, surface_id: u32, title: [u8; 32] },
    SurfaceDestroy { pid: u32, surface_id: u32 },
}

pub fn make_snp_window(
    surface_id: u32,
    _fb_width: i32,
    _fb_height: i32,
    workspace: u8,
    title: &[u8],
) -> ShellWindow {
    let x = 100;
    let y = 100;
    let w = 640;
    let h = 480;
    let mut title_buf = [0u8; 32];
    let copy = title.len().min(31);
    title_buf[..copy].copy_from_slice(&title[..copy]);
    ShellWindow {
        x, y, w, h: h + ShellWindow::TITLE_H,
        curr_x: (x + w / 2) as f32,
        curr_y: (y + (h + ShellWindow::TITLE_H) / 2) as f32,
        curr_w: 0.0, curr_h: 0.0,
        content: WindowContent::Snp { surface_id, pid: 0 }, 
        workspace,
        title: title_buf,
        ..Default::default()
    }
}
