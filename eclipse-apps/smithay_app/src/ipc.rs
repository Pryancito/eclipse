//! Handler IPC del compositor.
//! Usa `eclipse_ipc` como API unificada: fast path y slow path son transparentes.

use std::prelude::*;
use eclipse_ipc::prelude::*;
use eclipse_ipc::types::EclipseMessage;
use sidewind_core::{SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_UPDATE, SWND_OP_COMMIT};
use crate::input::{CompositorEvent, InputState};
use crate::compositor::{ExternalSurface, ShellWindow, WindowContent, MAX_SURFACE_DIM};

pub struct IpcHandler {
    channel: IpcChannel,
    pub message_count: u64,
    /// Intentos de recv (cada poll_event); si sube pero message_count no, el kernel no nos da mensajes.
    pub recv_attempts: u64,
}

impl IpcHandler {
    pub fn new() -> Self {
        Self {
            channel: IpcChannel::new(),
            message_count: 0,
            recv_attempts: 0,
        }
    }

    /// Recibir y clasificar el siguiente mensaje IPC (no bloqueante).
    pub fn process_messages(&mut self) -> Option<CompositorEvent> {
        loop {
            self.recv_attempts += 1;
            match self.channel.recv() {
                None => return None, // Buzón vacío: salir
                Some(EclipseMessage::Input(ev)) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::Input(ev));
                }
                Some(EclipseMessage::SideWind(sw, pid)) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::SideWind(sw, pid));
                }
                Some(EclipseMessage::NetStatsResponse { rx, tx }) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::NetStats(rx, tx));
                }
                Some(EclipseMessage::ServiceInfoResponse { data, len }) => {
                    self.message_count += 1;
                    let mut heap_data = heapless::Vec::<u8, 256>::new();
                    let _ = heap_data.extend_from_slice(&data[..len.min(256)]);
                    return Some(CompositorEvent::ServiceInfo(heap_data));
                }
                Some(_) => continue,
            }
        }
    }
}

pub fn handle_sidewind_message(
    msg: sidewind_core::SideWindMessage,
    sender_pid: u32,
    surfaces: &mut [ExternalSurface],
    windows: &mut [ShellWindow],
    window_count: &mut usize,
    input_state: &mut InputState,
) {
    match msg.op {
        SWND_OP_CREATE => {
            if msg.w == 0 || msg.h == 0 || msg.w > MAX_SURFACE_DIM || msg.h > MAX_SURFACE_DIM {
                return;
            }
            let surface_idx = surfaces.iter().position(|s| !s.active);
            if let Some(s_idx) = surface_idx {
                if *window_count < windows.len() {
                    surfaces[s_idx] = ExternalSurface {
                        id: sender_pid, pid: sender_pid, vaddr: 0x1000,
                        buffer_size: (msg.w * msg.h * 4) as usize, active: true,
                    };
                    windows[*window_count] = ShellWindow {
                        x: msg.x, y: msg.y,
                        w: msg.w as i32, h: msg.h as i32 + 26,
                        curr_x: (msg.x + msg.w as i32 / 2) as f32,
                        curr_y: (msg.y + (msg.h as i32 + 26) / 2) as f32,
                        curr_w: 0.0, curr_h: 0.0,
                        minimized: false, maximized: false, closing: false,
                        stored_rect: (msg.x, msg.y, msg.w as i32, msg.h as i32 + 26),
                        workspace: input_state.current_workspace,
                        content: WindowContent::External(s_idx as u32),
                    };
                    *window_count += 1;
                }
            }
        }
        SWND_OP_DESTROY => {
            if let Some(s_idx) = surfaces.iter().position(|s| s.active && s.pid == sender_pid) {
                surfaces[s_idx].active = false;
                let count = *window_count;
                if count == 0 { return; }
                if let Some(w_idx) = windows[..count].iter().position(|w| {
                    matches!(w.content, WindowContent::External(idx) if idx == s_idx as u32)
                }) {
                    if count > 1 && w_idx < count {
                        for i in w_idx..(count - 1) { windows[i] = windows[i + 1]; }
                    }
                    *window_count = count - 1;
                    if input_state.focused_window == Some(w_idx)  { input_state.focused_window = None; }
                    else if let Some(f) = input_state.focused_window { if f > w_idx { input_state.focused_window = Some(f - 1); } }
                    if input_state.dragging_window == Some(w_idx) { input_state.dragging_window = None; }
                    else if let Some(d) = input_state.dragging_window { if d > w_idx { input_state.dragging_window = Some(d - 1); } }
                    if input_state.resizing_window == Some(w_idx) { input_state.resizing_window = None; }
                    else if let Some(r) = input_state.resizing_window { if r > w_idx { input_state.resizing_window = Some(r - 1); } }
                }
            }
        }
        SWND_OP_UPDATE | SWND_OP_COMMIT => {
            let count = *window_count;
            if let Some(w_idx) = windows[..count].iter().position(|w| {
                matches!(w.content, WindowContent::External(idx)
                    if (idx as usize) < surfaces.len() && surfaces[idx as usize].pid == sender_pid)
            }) {
                if msg.op == SWND_OP_UPDATE {
                    windows[w_idx].x = msg.x;
                    windows[w_idx].y = msg.y;
                    if msg.w > 0 && msg.h > 0 && msg.w <= MAX_SURFACE_DIM && msg.h <= MAX_SURFACE_DIM {
                        windows[w_idx].w = msg.w as i32;
                        windows[w_idx].h = msg.h as i32 + 26;
                        if let WindowContent::External(s_idx) = windows[w_idx].content {
                            surfaces[s_idx as usize].buffer_size = (msg.w * msg.h * 4) as usize;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}
