//! Handler IPC del compositor.
//! Usa `eclipse_ipc` como API unificada: fast path y slow path son transparentes.

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
    /// Fast path (InputEvent) → slow path (SideWind, control) de forma automática.
    ///
    /// Cuando se recibe un mensaje de control o desconocido (Raw, INPT, NETW, etc.)
    /// se descarta y se reintenta en lugar de devolver None. Esto evita que el
    /// bucle de `process_events` se rompa prematuramente al encontrar mensajes de
    /// arranque residuales (p. ej. respuestas INPT/NETW del init) o respuestas de
    /// estadísticas de red (NSTA) de 20 bytes que llegan por el fast path.
    pub fn process_messages(&mut self) -> Option<CompositorEvent> {
        // Intentar hasta MAX_SKIP mensajes no reconocidos por llamada antes de rendirnos.
        // 32 es suficiente para drenar los mensajes de arranque residuales (INPT, NETW) y
        // respuestas periódicas de stats (NSTA) que normalmente no superan unos pocos por frame.
        // Limitar el número de intentos evita bucles infinitos si llegara una ráfaga de mensajes
        // no reconocidos de un proceso incorrecto.
        const MAX_SKIP: usize = 32;
        for _ in 0..MAX_SKIP {
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
                // Mensajes de control y raw: consumir y reintentar en lugar de devolver None.
                // Devolver None aquí causaría que process_events rompa su bucle aunque haya
                // eventos válidos pendientes en el buzón.
                Some(_) => continue,
            }
        }
        None
    }
}

// ============================================================================
// Funciones de arranque del compositor (delegan a eclipse_ipc::services)
// ============================================================================

pub fn query_input_service_pid() -> Option<u32> {
    eclipse_ipc::services::query_input_service_pid()
}

pub fn query_network_service_pid() -> Option<u32> {
    eclipse_ipc::services::query_network_service_pid()
}

pub fn subscribe_to_input_service(input_pid: u32, self_pid: u32) -> bool {
    eclipse_ipc::services::subscribe_to_input(input_pid, self_pid)
}

// ============================================================================
// Lógica de ventanas SideWind (sin cambios, solo movida aquí para claridad)
// ============================================================================

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
                if let Some(w_idx) = windows.iter().position(|w| w.content == WindowContent::External(s_idx as u32)) {
                    for i in w_idx..(*window_count - 1) { windows[i] = windows[i + 1]; }
                    *window_count -= 1;
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
            if let Some(w_idx) = windows.iter().position(|w| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use eclipse_libc::{mock_push_receive, mock_push_receive_fast, mock_clear};
    use eclipse_libc::InputEvent;
    use eclipse_ipc::prelude::EclipseEncode;
    use sidewind_core::{SideWindMessage, SIDEWIND_TAG};

    /// Global mutex to serialize all tests that touch the shared mock queues.
    /// The mock implementation (`MOCK_RECEIVE_QUEUE`, `MOCK_RECEIVE_FAST_QUEUE`) is
    /// process-wide state. Without serialization, parallel test threads corrupt each
    /// other's pushes/pops, causing spurious assertion failures.
    static MOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_ipc_input_routing() {
        let _guard = MOCK_LOCK.lock().unwrap();
        mock_clear();
        let mut handler = IpcHandler::new();
        
        let ev = InputEvent {
            device_id: 1,
            event_type: 0,
            code: 30,
            value: 1,
            timestamp: 12345,
        };
        let data = unsafe { core::slice::from_raw_parts(&ev as *const _ as *const u8, core::mem::size_of::<InputEvent>()) };
        mock_push_receive(data.to_vec(), 500);
        
        let result = handler.process_messages();
        assert!(result.is_some());
        match result.unwrap() {
            CompositorEvent::Input(recv_ev) => {
                assert_eq!(recv_ev.code, 30);
                assert_eq!(recv_ev.value, 1);
            }
            _ => panic!("Expected Input event"),
        }
    }

    #[test]
    fn test_ipc_sidewind_routing() {
        let _guard = MOCK_LOCK.lock().unwrap();
        mock_clear();
        let mut handler = IpcHandler::new();
        
        let sw = SideWindMessage::new_create(10, 20, 100, 200, "TestWin");
        let data = unsafe { core::slice::from_raw_parts(&sw as *const _ as *const u8, core::mem::size_of::<SideWindMessage>()) };
        mock_push_receive(data.to_vec(), 600);
        
        let result = handler.process_messages();
        assert!(result.is_some());
        match result.unwrap() {
            CompositorEvent::SideWind(recv_sw, sender_pid) => {
                assert_eq!(sender_pid, 600);
                assert_eq!(recv_sw.tag, SIDEWIND_TAG);
                assert_eq!(recv_sw.op, sidewind_core::SWND_OP_CREATE);
                assert_eq!(recv_sw.w, 100);
            }
            _ => panic!("Expected SideWind event"),
        }
    }

    #[test]
    fn test_handle_create_window() {
        let mut surfaces = [const { ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false } }; 16];
        let mut windows = [const { ShellWindow {
            x: 0, y: 0, w: 0, h: 0,
            curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
            minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0),
            workspace: 0, content: WindowContent::None,
        } }; 32];
        let mut window_count = 0;
        let mut input_state = InputState::new(1024, 768);
        
        let msg = SideWindMessage::new_create(100, 100, 400, 300, "App");
        handle_sidewind_message(msg, 700, &mut surfaces, &mut windows, &mut window_count, &mut input_state);
        
        assert_eq!(window_count, 1);
        assert_eq!(windows[0].x, 100);
        assert_eq!(windows[0].w, 400);
        assert_eq!(windows[0].h, 300 + 26); // Title bar height
        assert!(surfaces.iter().any(|s| s.active && s.pid == 700));
    }

    #[test]
    fn test_handle_destroy_window() {
        let mut surfaces = [const { ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false } }; 16];
        let mut windows = [const { ShellWindow {
            x: 0, y: 0, w: 0, h: 0,
            curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
            minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0),
            workspace: 0, content: WindowContent::None,
        } }; 32];
        let mut window_count = 0;
        let mut input_state = InputState::new(1024, 768);
        
        // Create first
        let msg_create = SideWindMessage::new_create(100, 100, 400, 300, "App");
        handle_sidewind_message(msg_create, 700, &mut surfaces, &mut windows, &mut window_count, &mut input_state);
        assert_eq!(window_count, 1);
        
        // Destroy
        let msg_destroy = SideWindMessage { op: SWND_OP_DESTROY, x: 0, y: 0, w: 0, h: 0, tag: SIDEWIND_TAG, name: [0; 32] };
        handle_sidewind_message(msg_destroy, 700, &mut surfaces, &mut windows, &mut window_count, &mut input_state);
        
        assert_eq!(window_count, 0);
        assert!(!surfaces[0].active);
    }

    /// Verifica que los mensajes no reconocidos (p.ej. respuestas INPT/NETW del arranque)
    /// son descartados automáticamente y el bucle continúa hasta encontrar un evento válido.
    /// Regresión para el bug de congelación: `_ => None` rompía process_events prematuramente.
    #[test]
    fn test_unrecognized_messages_skipped() {
        let _guard = MOCK_LOCK.lock().unwrap();
        mock_clear();
        let mut handler = IpcHandler::new();

        // Simular una respuesta INPT residual del arranque (8 bytes, no reconocida por compositor)
        let mut inpt_msg = vec![0u8; 8];
        inpt_msg[0..4].copy_from_slice(b"INPT");
        inpt_msg[4..8].copy_from_slice(&5u32.to_le_bytes());
        mock_push_receive(inpt_msg, 1); // desde init (PID 1)

        // Evento de entrada válido a continuación
        let ev = InputEvent {
            device_id: 1,
            event_type: 0,
            code: 30,
            value: 1,
            timestamp: 12345,
        };
        let data = unsafe { core::slice::from_raw_parts(&ev as *const _ as *const u8, core::mem::size_of::<InputEvent>()) };
        mock_push_receive(data.to_vec(), 500);

        // process_messages debe saltarse el INPT y devolver el InputEvent válido
        let result = handler.process_messages();
        assert!(result.is_some(), "process_messages debe devolver el InputEvent aunque haya un INPT previo");
        match result.unwrap() {
            CompositorEvent::Input(recv_ev) => {
                assert_eq!(recv_ev.code, 30);
                assert_eq!(recv_ev.value, 1);
            }
            _ => panic!("Se esperaba un CompositorEvent::Input"),
        }
        assert_eq!(handler.message_count, 1, "Solo 1 mensaje reconocido esperado");
    }

    #[test]
    fn test_bench_ipc_stress_1m() {
        use std::time::Instant;
        let _guard = MOCK_LOCK.lock().unwrap();
        mock_clear();
        let mut handler = IpcHandler::new();
        
        let ev = InputEvent {
            device_id: 1, event_type: 0, code: 1, value: 1, timestamp: 0,
        };
        let encoded = ev.encode_fast();
        let fast_size = core::mem::size_of::<InputEvent>();
        
        let start = Instant::now();
        let total_messages = 1_000_000;
        let batch_size = 100_000;
        let mut processed = 0;
        
        while processed < total_messages {
            // Feed batch (fast path = eventos ratón/teclado reales)
            for _ in 0..batch_size {
                mock_push_receive_fast(encoded, 0, fast_size);
            }
            
            // Process batch
            for _ in 0..batch_size {
                let res = handler.process_messages();
                if res.is_none() {
                    panic!("Loop failed");
                }
            }
            processed += batch_size;
            
            println!("Processed {}k messages...", processed / 1_000);
        }
        
        let duration = start.elapsed();
        let mps = total_messages as f64 / duration.as_secs_f64();
        
        println!("\n--- 1M IPC STRESS TEST RESULTS ---");
        println!("Total Messages: {}", total_messages);
        println!("Total Time: {:?}", duration);
        println!("Throughput: {:.2} MPS (Messages Per Second)", mps);
        println!("------------------------------------\n");
        
        assert_eq!(handler.message_count, total_messages as u64);
    }
}
