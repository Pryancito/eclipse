//! Handler IPC del compositor.
//! Usa `eclipse_ipc` como API unificada: fast path y slow path son transparentes.

use std::prelude::v1::*;
pub use eclipse_ipc::prelude::*;
// use eclipse_ipc::types::EclipseMessage; // Removed as per instruction, but will need to be qualified below
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
    /// Intentos de recv (cada poll_event); si sube pero message_count no, el kernel no nos da mensajes.
    pub recv_attempts: u64,
    /// Número de llamadas consecutivas a process_messages() que no entregaron ningún evento
    /// reconocido. Si sube indefinidamente indica que el buzón está vacío o lleno de mensajes
    /// desconocidos y el IPC es funcional pero sin eventos útiles.
    pub consecutive_empty: u64,
    #[cfg(test)]
    pub mock_events: alloc::vec::Vec<CompositorEvent>,
}

impl IpcHandler {
    pub fn new() -> Self {
        Self {
            channel: IpcChannel::new(),
            message_count: 0,
            recv_attempts: 0,
            consecutive_empty: 0,
            #[cfg(test)]
            mock_events: alloc::vec::Vec::new(),
        }
    }

    /// Recibir y clasificar el siguiente mensaje IPC (no bloqueante).
    /// Procesa hasta un máximo de mensajes por llamada para evitar bloqueos por flood.
    pub fn process_messages(&mut self) -> Option<CompositorEvent> {
        #[cfg(test)]
        if !self.mock_events.is_empty() {
            self.consecutive_empty = 0;
            return Some(self.mock_events.remove(0));
        }

        // Procesar hasta MAX_SKIP=32 mensajes seguidos si son ignorados por el compositor,
        // para dar oportunidad al renderizado y no morir en un bucle infinito
        // si recibimos basura o un flood de eventos desconocidos.
        // El límite de 32 evita que process_messages() sea un punto de bloqueo:
        // siempre retorna en tiempo acotado independientemente del contenido del buzón.
        const MAX_SKIP: usize = 32;
        for _ in 0..MAX_SKIP {
            self.recv_attempts += 1;
            #[cfg(not(test))]
            let recv_res = self.channel.recv();
            #[cfg(test)]
            let recv_res: Option<eclipse_ipc::types::EclipseMessage> = None;

            match recv_res {
                None => {
                    // Buzón vacío: no hay más mensajes que procesar.
                    self.consecutive_empty = self.consecutive_empty.saturating_add(1);
                    return None;
                }
                Some(EclipseMessage::Input(ev)) => {
                    self.message_count += 1;
                    self.consecutive_empty = 0;
                    // En Eclipse OS (`target_os = "none"`) el tipo del fast-path (`eclipse_syscall::InputEvent`)
                    // difiere del usado en `CompositorEvent::Input` (definido en `eclipse_libc`). Las dos
                    // structs tienen la misma representación, así que copiamos campo a campo.
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
                    self.consecutive_empty = 0;
                    return Some(CompositorEvent::SideWind(sw, pid));
                }
                Some(EclipseMessage::NetStatsResponse { rx, tx }) => {
                    self.message_count += 1;
                    self.consecutive_empty = 0;
                    return Some(CompositorEvent::NetStats(rx, tx));
                }
                Some(EclipseMessage::ServiceInfoResponse { data, len }) => {
                    self.message_count += 1;
                    self.consecutive_empty = 0;
                    let mut heap_data = heapless::Vec::<u8, 256>::new();
                    let _ = heap_data.extend_from_slice(&data[..len.min(256)]);
                    return Some(CompositorEvent::ServiceInfo(heap_data));
                }
                Some(EclipseMessage::Log { line, len }) => {
                    self.message_count += 1;
                    self.consecutive_empty = 0;
                    let mut v = heapless::Vec::<u8, 252>::new();
                    let _ = v.extend_from_slice(&line[..len.min(252)]);
                    return Some(CompositorEvent::KernelLog(v));
                }
                Some(EclipseMessage::Wayland { data, len }) => {
                    self.message_count += 1;
                    self.consecutive_empty = 0;
                    let mut vec = heapless::Vec::new();
                    let _ = vec.extend_from_slice(&data[..len]);
                    // Kernel fast-path Wayland messages currently do not carry the sender PID,
                    // so we pass 0 as a placeholder. SmithayState uses the PID only as a
                    // coarse index into the connection table.
                    return Some(CompositorEvent::Wayland(vec, 0));
                }
                Some(_) => {
                    // Mensaje no reconocido o no procesado por el compositor:
                    // Continuamos el bucle interno para intentar sacar el siguiente.
                    // No incrementamos consecutive_empty: hay actividad en el buzón, aunque no
                    // sea útil para el compositor. Esto evita falsos positivos de "IPC muerto".
                    continue;
                }
            }
        }

        // Límite de intentos alcanzado: el buzón tiene muchos mensajes no reconocidos.
        // Retornamos None para que el compositor pueda hacer render y no quedar bloqueado.
        // El siguiente frame continuará vaciando el buzón.
        None
    }
}

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

            let surface_idx = surfaces.iter().position(|s| !s.active);
            if let Some(s_idx) = surface_idx {
                if *window_count < windows.len() {
                    // 1. Construct path to shared memory file
                    let mut path = [0u8; 64];
                    path[0..5].copy_from_slice(b"/tmp/");
                    let mut name_len = 0;
                    for i in 0..32 {
                        if msg.name[i] == 0 { break; }
                        path[5+i] = msg.name[i];
                        name_len += 1;
                    }
                    path[5+name_len] = 0; // Null terminator for C string

                    // 2. Open and mmap the surface buffer
                    #[cfg(target_vendor = "eclipse")]
                    let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_NONBLOCK, 0) };
                    #[cfg(not(target_vendor = "eclipse"))]
                    let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_NONBLOCK, 0) };

                    if fd < 0 {
                        println!("[SMITHAY] Error: Failed to open SHM file {:?}", unsafe { core::str::from_utf8_unchecked(&path[..5+name_len]) });
                        return;
                    }

                    let vaddr = unsafe { mmap(core::ptr::null_mut(), buffer_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
                    unsafe { close(fd) };

                    if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) {
                        println!("[SMITHAY] Error: mmap failed for surface");
                        return;
                    }

                    surfaces[s_idx] = ExternalSurface {
                        id: sender_pid, pid: sender_pid, vaddr: vaddr as usize,
                        buffer_size, active: true, ready_to_flip: false,
                    };
                    
                    // Clamp initial window position to screen bounds
                    let margin = 50;
                    let clamped_x = msg.x.clamp(margin, (fb_width - 100).max(margin));
                    let clamped_y = msg.y.clamp(ShellWindow::TITLE_H, (fb_height - 100).max(ShellWindow::TITLE_H));

                    windows[*window_count] = ShellWindow {
                        x: clamped_x, y: clamped_y,
                        w: msg.w as i32, h: msg.h as i32 + 26,
                        curr_x: (clamped_x + msg.w as i32 / 2) as f32,
                        curr_y: (clamped_y + (msg.h as i32 + 26) / 2) as f32,
                        curr_w: 0.0, curr_h: 0.0,
                        minimized: false, maximized: false, closing: false,
                        stored_rect: (clamped_x, clamped_y, msg.w as i32, msg.h as i32 + 26),
                        workspace: input_state.current_workspace,
                        content: WindowContent::External(s_idx as u32),
                        damage: alloc::vec::Vec::new(),
                    };
                    let _new_idx = *window_count;
                    *window_count += 1;
                    // Damage tracking removed
                }
            }
        }
        SWND_OP_DESTROY => {
            if let Some(s_idx) = surfaces.iter().position(|s| s.active && s.pid == sender_pid) {
                let count = *window_count;
                if let Some(w_idx) = windows[..count].iter().position(|w| {
                    matches!(w.content, WindowContent::External(idx) if idx == s_idx as u32)
                }) {
                    // Damage tracking removed
                    surfaces[s_idx].unmap();
                    if count > 1 && w_idx < count - 1 {
                        for i in w_idx..(count - 1) {
                            windows[i] = windows[i + 1].clone();
                        }
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
                // Damage tracking removed
                if msg.op == SWND_OP_UPDATE {
                    windows[w_idx].x = msg.x;
                    windows[w_idx].y = msg.y;
                    if msg.w > 0 && msg.h > 0 && msg.w <= MAX_SURFACE_DIM && msg.h <= MAX_SURFACE_DIM {
                        windows[w_idx].w = msg.w as i32;
                        windows[w_idx].h = msg.h as i32 + 26;
                        if let WindowContent::External(s_idx) = windows[w_idx].content {
                            surfaces[s_idx as usize].buffer_size = (msg.w as usize).saturating_mul(msg.h as usize).saturating_mul(4);
                            surfaces[s_idx as usize].ready_to_flip = false; // Need a new commit
                        }
                    }
                    // Damage tracking removed
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
            device_id: 1,
            event_type: 0,
            code: 0x1E,
            value: 1,
            timestamp: 0,
        });
        handler.mock_events.push(ev.clone());
        
        let result = handler.process_messages();
        assert!(result.is_some());
        assert_eq!(handler.message_count, 0); // message_count only increments for REAL messages
    }

    #[test]
    fn test_handle_sidewind_create_invalid() {
        let mut surfaces = [ExternalSurface::default(); 32];
        let mut windows: [ShellWindow; 32] = core::array::from_fn(|_| ShellWindow::default());
        let mut window_count = 0;
        let mut input_state = InputState::new(1920, 1080);
        
        let msg = SideWindMessage {
            tag: SIDEWIND_TAG,
            op: sidewind::SWND_OP_CREATE,
            x: 100, y: 100, w: 0, h: 100, // Invalid width
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

        let msg = sidewind::SideWindMessage {
            tag: SIDEWIND_TAG,
            op: SWND_OP_CREATE,
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

        let msg = sidewind::SideWindMessage {
            tag: SIDEWIND_TAG,
            op: SWND_OP_DESTROY,
            x: 0, y: 0, w: 0, h: 0,
            name: [0; 32],
        };
        handle_sidewind_message(&msg, 123, &mut surfaces, &mut windows, &mut window_count, &mut input_state, 1920, 1080);
        assert_eq!(window_count, 0);
    }

    #[test]
    fn test_consecutive_empty_increments_on_empty_mailbox() {
        let mut handler = IpcHandler::new();
        // No mock events → recv returns None → consecutive_empty should increment
        let result = handler.process_messages();
        assert!(result.is_none());
        assert_eq!(handler.consecutive_empty, 1);
        // Second call also empty: consecutive_empty should keep growing
        let _ = handler.process_messages();
        assert_eq!(handler.consecutive_empty, 2);
    }

    #[test]
    fn test_consecutive_empty_resets_on_message() {
        let mut handler = IpcHandler::new();
        use eclipse_syscall::InputEvent;
        // Simulate a few empty polls
        let _ = handler.process_messages();
        let _ = handler.process_messages();
        assert_eq!(handler.consecutive_empty, 2);
        // Now push a real event: consecutive_empty should reset to 0
        handler.mock_events.push(CompositorEvent::Input(InputEvent {
            device_id: 1,
            event_type: 0,
            code: 0x1E,
            value: 1,
            timestamp: 0,
        }));
        let result = handler.process_messages();
        assert!(result.is_some());
        assert_eq!(handler.consecutive_empty, 0);
    }

    #[test]
    fn test_process_messages_is_bounded_no_infinite_loop() {
        let mut handler = IpcHandler::new();
        // Verify that process_messages() always returns in bounded time:
        // with no mock events, it returns None immediately.
        for _ in 0..1000 {
            let res = handler.process_messages();
            assert!(res.is_none());
        }
        // recv_attempts grows linearly (1 per call when mailbox is empty)
        assert_eq!(handler.recv_attempts, 1000);
    }
}
