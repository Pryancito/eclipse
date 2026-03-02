//! Constantes y funciones de descubrimiento de servicios de Eclipse OS

use eclipse_libc::{send, receive, yield_cpu};
use crate::types::{GET_INPUT_PID_MSG, TAG_INPT, GET_NETWORK_PID_MSG, TAG_NETW, build_subscribe_payload};

// ============================================================================
// Tipos de mensaje (msg_type en sys_send)
// ============================================================================

pub const MSG_TYPE_SYSTEM:    u32 = 0x00000001;
pub const MSG_TYPE_MEMORY:    u32 = 0x00000002;
pub const MSG_TYPE_FILESYSTEM:u32 = 0x00000004;
pub const MSG_TYPE_NETWORK:   u32 = 0x00000008;
pub const MSG_TYPE_GRAPHICS:  u32 = 0x00000010;
pub const MSG_TYPE_AUDIO:     u32 = 0x00000020;
pub const MSG_TYPE_INPUT:     u32 = 0x00000040;
pub const MSG_TYPE_AI:        u32 = 0x00000080;
pub const MSG_TYPE_SECURITY:  u32 = 0x00000100;
pub const MSG_TYPE_USER:      u32 = 0x00000200;
pub const MSG_TYPE_SIGNAL:    u32 = 0x00000400;

// ============================================================================
// PIDs de servicios conocidos
// ============================================================================

/// PID del proceso init (siempre 1 en Eclipse OS).
/// El init conoce los PIDs de todos los servicios y responde a GET_INPUT_PID.
pub const INIT_PID: u32 = 1;

/// PID del input_service (= primer proceso spawneado por el kernel).
/// Este es el valor de fallback; `query_input_service_pid()` obtiene el PID real del init.
pub const INPUT_SERVICE_PID: u32 = INIT_PID;

// ============================================================================
// Descubrimiento de servicios
// ============================================================================

/// Número de intentos por defecto al preguntar al init por el PID del input_service.
/// Permite dar tiempo al init a estar en su main_loop; en entornos lentos se puede
/// usar `query_input_service_pid_with_attempts` con un valor mayor.
pub const DEFAULT_INPUT_QUERY_ATTEMPTS: u32 = 10_000;

/// Preguntar al init el PID real del input_service, con un máximo de intentos configurable.
/// El init implementa el protocolo GET_INPUT_PID → "INPT"+u32.
/// En cada intento sin respuesta se hace `yield_cpu()`.
pub fn query_input_service_pid_with_attempts(max_attempts: u32) -> Option<u32> {
    if send(INIT_PID, MSG_TYPE_INPUT, GET_INPUT_PID_MSG) != 0 {
        return None;
    }
    let mut buffer = [0u8; 64];
    for _ in 0..max_attempts {
        let (len, sender_pid) = receive(&mut buffer);
        if len >= 8 && sender_pid == INIT_PID && buffer[0..4] == *TAG_INPT {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buffer[4..8]);
            let pid = u32::from_le_bytes(pid_bytes);
            if pid > 0 { return Some(pid); }
        }
        yield_cpu();
    }
    None
}

/// Preguntar al init el PID real del input_service.
/// Usa [`DEFAULT_INPUT_QUERY_ATTEMPTS`] reintentos. Para otro límite, usa
/// [`query_input_service_pid_with_attempts`].
pub fn query_input_service_pid() -> Option<u32> {
    query_input_service_pid_with_attempts(DEFAULT_INPUT_QUERY_ATTEMPTS)
}

/// Suscribirse al input_service para recibir InputEvents.
pub fn subscribe_to_input(input_pid: u32, self_pid: u32) -> bool {
    let msg = build_subscribe_payload(self_pid);
    send(input_pid, MSG_TYPE_INPUT, &msg) == 0
}

/// Preguntar al init el PID real del network_service, con un máximo de intentos configurable.
pub fn query_network_service_pid_with_attempts(max_attempts: u32) -> Option<u32> {
    // Use MSG_TYPE_INPUT so the request is delivered P2P to init's mailbox.
    // MSG_TYPE_NETWORK is non-P2P and gets dropped in the global IPC queue.
    if send(INIT_PID, MSG_TYPE_INPUT, GET_NETWORK_PID_MSG) != 0 {
        return None;
    }
    let mut buffer = [0u8; 64];
    for _ in 0..max_attempts {
        let (len, sender_pid) = receive(&mut buffer);
        if len >= 8 && sender_pid == INIT_PID && buffer[0..4] == *TAG_NETW {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buffer[4..8]);
            let pid = u32::from_le_bytes(pid_bytes);
            // Break the loop regardless of pid value: if pid==0 the service is not
            // running, so there is no point retrying 10,000 more times.
            return if pid > 0 { Some(pid) } else { None };
        }
        yield_cpu();
    }
    None
}

/// Preguntar al init el PID real del network_service.
pub fn query_network_service_pid() -> Option<u32> {
    query_network_service_pid_with_attempts(DEFAULT_INPUT_QUERY_ATTEMPTS)
}
