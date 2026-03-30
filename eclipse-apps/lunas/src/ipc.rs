//! IPC message handler for Lunas desktop.
//! Uses eclipse_ipc for unified fast-path and slow-path communication.

use std::prelude::v1::*;
pub use eclipse_ipc::prelude::*;
use sidewind::{SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_UPDATE, SWND_OP_COMMIT};
use crate::input::{CompositorEvent, InputState};
use crate::compositor::{ExternalSurface, ShellWindow, WindowContent, MAX_SURFACE_DIM, MAX_SURFACE_BYTES};
use core::matches;
#[cfg(target_vendor = "eclipse")]
use libc::{open, mmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_NONBLOCK};
#[cfg(not(target_vendor = "eclipse"))]
use libc::{open, mmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_NONBLOCK};

pub struct IpcHandler {
    channel: IpcChannel,
    pub message_count: u64,
    pub recv_attempts: u64,
    #[cfg(test)]
    pub mock_events: alloc::vec::Vec<CompositorEvent>,
}

impl IpcHandler {
    pub fn new() -> Self {
        Self {
            channel: IpcChannel::new(),
            message_count: 0,
            recv_attempts: 0,
            #[cfg(test)]
            mock_events: alloc::vec::Vec::new(),
        }
    }

    /// Receive and classify the next IPC message (non-blocking).
    pub fn process_messages(&mut self) -> Option<CompositorEvent> {
        #[cfg(test)]
        if !self.mock_events.is_empty() {
            return Some(self.mock_events.remove(0));
        }

        for _ in 0..16 {
            self.recv_attempts += 1;
            #[cfg(not(test))]
            let recv_res = self.channel.recv();
            #[cfg(test)]
            let recv_res: Option<eclipse_ipc::types::EclipseMessage> = None;

            match recv_res {
                None => return None,
                Some(EclipseMessage::Input(ev)) => {
                    self.message_count += 1;
                    #[cfg(target_os = "none")]
                    let ev_converted = {
                        use crate::libc::InputEvent as LibcInputEvent;
                        LibcInputEvent {
                            device_id: ev.device_id,
                            event_type: ev.event_type,
                            code: ev.code,
                            value: ev.value,
                            timestamp: ev.timestamp,
                        }
                    };
                    #[cfg(not(target_os = "none"))]
                    let ev_converted = ev;
                    return Some(CompositorEvent::Input(ev_converted));
                }
                Some(EclipseMessage::SideWind(sw, pid)) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::SideWind(sw, pid));
                }
                Some(EclipseMessage::NetStatsResponse { rx, tx }) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::NetStats(rx, tx));
                }
                Some(EclipseMessage::NetExtendedStatsResponse(stats)) => {
                    self.message_count += 1;
                    return Some(CompositorEvent::NetExtendedStats(stats));
                }
                Some(EclipseMessage::ServiceInfoResponse { data, len }) => {
                    self.message_count += 1;
                    let mut heap_data = heapless::Vec::<u8, 512>::new();
                    let _ = heap_data.extend_from_slice(&data[..len.min(512)]);
                    return Some(CompositorEvent::ServiceInfo(heap_data));
                }
                Some(EclipseMessage::Log { line, len }) => {
                    self.message_count += 1;
                    let mut v = heapless::Vec::<u8, 252>::new();
                    let _ = v.extend_from_slice(&line[..len.min(252)]);
                    return Some(CompositorEvent::KernelLog(v));
                }
                Some(EclipseMessage::Wayland { data, len, from }) => {
                    self.message_count += 1;
                    let mut vec = heapless::Vec::new();
                    let _ = vec.extend_from_slice(&data[..len]);
                    return Some(CompositorEvent::Wayland(vec, from));
                }
                Some(EclipseMessage::Raw { data, len, from }) => {
                    continue;
                }
                Some(_) => {
                    continue;
                }
            }
        }
        None
    }

    /// Send a Wayland protocol message to a client PID.
    pub fn send_wayland(&mut self, target_pid: u32, data: &[u8]) {
        #[cfg(not(test))]
        let _ = self.channel.send_wayland(target_pid, data);
        #[cfg(test)]
        let _ = (target_pid, data);
    }
}

/// Handle a SideWind protocol message (window create/destroy/update/commit).
pub fn handle_sidewind_message(
    msg: &sidewind::SideWindMessage,
    sender_pid: u32,
    surfaces: &mut [ExternalSurface],
    windows: &mut [ShellWindow],
    window_count: &mut usize,
    input_state: &mut InputState,
    fb_width: i32,
    fb_height: i32,
) {
    match msg.op {
        SWND_OP_CREATE => {
            if msg.w == 0 || msg.h == 0 || msg.w > MAX_SURFACE_DIM || msg.h > MAX_SURFACE_DIM {
                return;
            }

            let buffer_size = (msg.w as usize).saturating_mul(msg.h as usize).saturating_mul(4);
            if buffer_size > MAX_SURFACE_BYTES as usize {
                return;
            }

            // Check if this PID already has a window; if so, reuse its surface slot
            // and update the existing window instead of creating a duplicate.
            let count = *window_count;
            let existing_s_idx = surfaces.iter().position(|s| s.active && s.pid == sender_pid);
            let existing_window_idx = if let Some(es_idx) = existing_s_idx {
                windows[..count].iter().position(|w| {
                    matches!(w.content, WindowContent::External(idx) if idx == es_idx as u32)
                })
            } else {
                None
            };

            let surface_idx = if let Some(existing_idx) = existing_s_idx {
                surfaces[existing_idx].unmap();
                Some(existing_idx)
            } else {
                surfaces.iter().position(|s| !s.active)
            };

            if let Some(s_idx) = surface_idx {
                // Only require a free window slot if we are NOT reusing an existing window.
                if existing_window_idx.is_none() && *window_count >= windows.len() {
                    return;
                }

                let mut path = [0u8; 64];
                path[0..5].copy_from_slice(b"/tmp/");
                let mut name_len = 0;
                for i in 0..32 {
                    let b = msg.name[i];
                    if b == 0 { break; }
                    // Allow only a conservative subset of ASCII characters to avoid
                    // path traversal or injection (no '/', no '.', no control bytes).
                    if !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                        return;
                    }
                    path[5 + name_len] = b;
                    name_len += 1;
                }
                if name_len == 0 {
                    return;
                }
                path[5 + name_len] = 0;

                #[cfg(target_vendor = "eclipse")]
                let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_NONBLOCK, 0) };
                #[cfg(not(target_vendor = "eclipse"))]
                let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_NONBLOCK, 0) };

                if fd < 0 { return; }

                let vaddr = unsafe { mmap(core::ptr::null_mut(), buffer_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
                unsafe { close(fd) };

                if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) {
                    return;
                }

                surfaces[s_idx] = ExternalSurface {
                    id: sender_pid, pid: sender_pid, vaddr: vaddr as usize,
                    buffer_size, mapped_len: buffer_size, active: true, ready_to_flip: false,
                };

                let margin = 50;
                let clamped_x = msg.x.clamp(margin, (fb_width - 100).max(margin));
                let clamped_y = msg.y.clamp(ShellWindow::TITLE_H, (fb_height - 100).max(ShellWindow::TITLE_H));

                let mut title = [0u8; 32];
                title[..name_len.min(32)].copy_from_slice(&msg.name[..name_len.min(32)]);

                let new_window = ShellWindow {
                    x: clamped_x, y: clamped_y,
                    w: msg.w as i32, h: msg.h as i32 + ShellWindow::TITLE_H,
                    curr_x: (clamped_x + msg.w as i32 / 2) as f32,
                    curr_y: (clamped_y + (msg.h as i32 + ShellWindow::TITLE_H) / 2) as f32,
                    curr_w: 0.0, curr_h: 0.0,
                    minimized: false, maximized: false, closing: false,
                    stored_rect: (clamped_x, clamped_y, msg.w as i32, msg.h as i32 + ShellWindow::TITLE_H),
                    workspace: input_state.current_workspace,
                    content: WindowContent::External(s_idx as u32),
                    damage: alloc::vec::Vec::new(),
                    buffer_handle: None,
                    is_dmabuf: false,
                    title,
                };

                if let Some(w_idx) = existing_window_idx {
                    // Reuse the existing window slot for this PID.
                    windows[w_idx] = new_window;
                } else {
                    windows[*window_count] = new_window;
                    *window_count += 1;
                }
            }
        }
        SWND_OP_DESTROY => {
            if let Some(s_idx) = surfaces.iter().position(|s| s.active && s.pid == sender_pid) {
                let count = *window_count;
                if let Some(w_idx) = windows[..count].iter().position(|w| {
                    matches!(w.content, WindowContent::External(idx) if idx == s_idx as u32)
                }) {
                    surfaces[s_idx].unmap();
                    if count > 1 && w_idx < count - 1 {
                        for i in w_idx..(count - 1) {
                            windows[i] = windows[i + 1].clone();
                        }
                    }
                    *window_count = count - 1;
                    if input_state.focused_window == Some(w_idx) { input_state.focused_window = None; }
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
                        windows[w_idx].h = msg.h as i32 + ShellWindow::TITLE_H;
                        if let WindowContent::External(s_idx) = windows[w_idx].content {
                            // Do not change buffer_size or mapped_len here; the mapping
                            // length is determined at CREATE time.  The render blit is
                            // clamped to the mapped extent so oversized windows are safe.
                            surfaces[s_idx as usize].ready_to_flip = false;
                        }
                    }
                } else if msg.op == SWND_OP_COMMIT {
                    if let WindowContent::External(s_idx) = windows[w_idx].content {
                        surfaces[s_idx as usize].ready_to_flip = true;
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidewind::{SideWindMessage, SIDEWIND_TAG};
    use eclipse_syscall::InputEvent;

    #[test]
    fn test_ipc_handler_mock_events() {
        let mut handler = IpcHandler::new();
        let ev = CompositorEvent::Input(InputEvent {
            device_id: 1, event_type: 0, code: 0x1E, value: 1, timestamp: 0,
        });
        handler.mock_events.push(ev.clone());
        let result = handler.process_messages();
        assert!(result.is_some());
        assert_eq!(handler.message_count, 0);
    }

    #[test]
    fn test_handle_sidewind_create_invalid() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: sidewind::SWND_OP_CREATE,
            x: 100, y: 100, w: 0, h: 100,
            name: [0; 32],
        };
        handle_sidewind_message(&msg, 123, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0);
    }

    #[test]
    fn test_handle_sidewind_create_invalid_height() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: SWND_OP_CREATE,
            x: 100, y: 100, w: 100, h: 0,
            name: [0; 32],
        };
        handle_sidewind_message(&msg, 123, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0);
    }

    #[test]
    fn test_handle_sidewind_destroy_no_panic() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: SWND_OP_DESTROY,
            x: 0, y: 0, w: 0, h: 0,
            name: [0; 32],
        };
        handle_sidewind_message(&msg, 123, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0);
    }

    #[test]
    fn test_create_rejects_empty_name() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        // All-zero name should be rejected (empty name).
        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: SWND_OP_CREATE,
            x: 100, y: 100, w: 200, h: 200,
            name: [0; 32],
        };
        handle_sidewind_message(&msg, 42, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0, "empty name should be rejected");
    }

    #[test]
    fn test_create_rejects_path_traversal() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        // Name containing '/' should be rejected.
        let mut bad_name = [0u8; 32];
        bad_name[0] = b'.';
        bad_name[1] = b'.';
        bad_name[2] = b'/';
        bad_name[3] = b'x';
        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: SWND_OP_CREATE,
            x: 100, y: 100, w: 200, h: 200,
            name: bad_name,
        };
        handle_sidewind_message(&msg, 42, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0, "path traversal name should be rejected");
    }

    #[test]
    fn test_create_rejects_dot_in_name() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);

        // Name containing '.' should be rejected (no dots allowed).
        let mut bad_name = [0u8; 32];
        bad_name[0] = b'.';
        bad_name[1] = b'h';
        bad_name[2] = b'i';
        let msg = SideWindMessage {
            tag: SIDEWIND_TAG, op: SWND_OP_CREATE,
            x: 100, y: 100, w: 200, h: 200,
            name: bad_name,
        };
        handle_sidewind_message(&msg, 42, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0, "name with dots should be rejected");
    }
}
