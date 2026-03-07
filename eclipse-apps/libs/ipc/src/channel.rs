//! Canal IPC de alto nivel para Eclipse OS.
//! Gestiona automáticamente fast path (≤24 bytes, registros) y slow path (buffer).

use core::cmp::Ord;
use crate::eclipse_libc::{receive, receive_fast, eclipse_send as send, yield_cpu};
use sidewind_core::SideWindMessage;
use crate::types::{EclipseMessage, MAX_MSG_LEN, parse_fast, parse_slow, build_subscribe_payload, build_input_pid_response_payload};

/// Canal IPC para un proceso Eclipse OS.
///
/// Encapsula la lógica de fast path / slow path de forma transparente.
/// El llamador solo recibe `Option<EclipseMessage>` y no necesita
/// saber nada de buffers, syscalls ni registros CPU.
/// El buffer del slow path se reutiliza en cada `recv()` para evitar alloc en stack.
pub struct IpcChannel {
    /// Total de mensajes recibidos (para estadísticas)
    pub message_count: u64,
    /// Buffer reutilizable para el slow path (mensajes >24 bytes).
    slow_buf: [u8; MAX_MSG_LEN],
}

impl IpcChannel {
    /// Crear un canal nuevo
    pub const fn new() -> Self {
        Self {
            message_count: 0,
            slow_buf: [0u8; MAX_MSG_LEN],
        }
    }

    /// Intentar recibir un mensaje (no bloqueante).
    ///
    /// **Orden:** siempre se intenta primero el **fast path** (mensajes ≤24 bytes en registros,
    /// p. ej. `InputEvent`). Si no hay mensaje pequeño, se intenta el **slow path** (buffer en
    /// memoria; SideWind, control, etc.). Si en fast path hay datos pero no se reconocen (tamaño
    /// distinto o tipo desconocido), se descartan y se devuelve `None` *sin* leer el slow path, para
    /// no consumir otro mensaje por error.
    ///
    /// Devuelve `None` si no hay mensaje disponible o si el mensaje pequeño no pudo parsearse.
    pub fn recv(&mut self) -> Option<EclipseMessage> {
        // --- Fast path ---
        if let Some((data, from, len)) = receive_fast() {
            if let Some(msg) = parse_fast(&data, from, len) {
                self.message_count += 1;
                return Some(msg);
            }
            
            // Mensaje pequeño no reconocido (p. ej. señal desconocida):
            // Fallback a Raw para no perder el mensaje de la cola.
            let mut raw_data = [0u8; 256];
            raw_data[..24].copy_from_slice(&data);
            self.message_count += 1;
            return Some(EclipseMessage::Raw { data: raw_data, len, from });
        }

        // --- Slow path ---
        let mut from: u32 = 0;
        let len = unsafe { receive(self.slow_buf.as_mut_ptr(), MAX_MSG_LEN, &mut from) };
        if len > 0 {
            if let Some(msg) = parse_slow(&self.slow_buf, len.min(MAX_MSG_LEN), from) {
                self.message_count += 1;
                return Some(msg);
            }
        }

        None
    }

    /// Recibir un mensaje de forma asíncrona (devuelve un `Future`).
    ///
    /// Requiere la feature `async`. Puedes usar `eclipse_ipc::block_on(&mut fut)` para
    /// esperar al mensaje sin un executor, o `.await` si tu runtime lo soporta.
    #[cfg(feature = "async")]
    pub fn recv_async(&mut self) -> crate::async_channel::RecvFuture<'_> {
        crate::async_channel::RecvFuture { channel: self }
    }

    /// Recibir un mensaje esperando hasta `max_attempts` intentos (cada uno: recv + yield).
    /// Útil en arranque o cuando se necesita bloquear hasta recibir algo (p. ej. respuesta de init).
    ///
    /// Devuelve el primer mensaje recibido, o `None` si tras `max_attempts` no llegó ninguno.
    pub fn recv_blocking_for(&mut self, max_attempts: u32) -> Option<EclipseMessage> {
        for _ in 0..max_attempts {
            if let Some(msg) = self.recv() {
                return Some(msg);
            }
            unsafe { yield_cpu() };
        }
        None
    }

    /// Enviar bytes crudos a un PID/servidor.
    /// Retorna `true` si el envío fue aceptado por el kernel.
    pub fn send_raw(dest_pid: u32, msg_type: u32, data: &[u8]) -> bool {
        unsafe { send(dest_pid, msg_type, data.as_ptr() as *const core::ffi::c_void, data.len(), 0) == 0 }
    }

    /// Enviar un mensaje de suscripción a un PID.
    pub fn send_subscribe(dest_pid: u32, self_pid: u32) -> bool {
        let buf = build_subscribe_payload(self_pid);
        Self::send_raw(dest_pid, crate::services::MSG_TYPE_INPUT, &buf)
    }

    /// Enviar petición de PID del input_service.
    pub fn send_get_input_pid(dest_pid: u32) -> bool {
        Self::send_raw(dest_pid, crate::services::MSG_TYPE_INPUT, crate::types::GET_INPUT_PID_MSG)
    }

    /// Enviar respuesta de PID del input_service.
    pub fn send_input_pid_response(dest_pid: u32, input_pid: u32) -> bool {
        let buf = build_input_pid_response_payload(input_pid);
        Self::send_raw(dest_pid, crate::services::MSG_TYPE_INPUT, &buf)
    }

    /// Enviar un mensaje SideWind al compositor (create/destroy/update/commit).
    /// Serializa el struct como bytes con tag SWND; el compositor debe usar `MSG_TYPE_GRAPHICS`.
    pub fn send_sidewind(dest_pid: u32, msg: &SideWindMessage) -> bool {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                msg as *const SideWindMessage as *const u8,
                core::mem::size_of::<SideWindMessage>(),
            )
        };
        Self::send_raw(dest_pid, crate::services::MSG_TYPE_GRAPHICS, bytes)
    }
}
