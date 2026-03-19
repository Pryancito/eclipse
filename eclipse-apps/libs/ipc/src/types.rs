//! Tipos de mensajes tipados para el IPC de Eclipse OS

use eclipse_syscall::InputEvent;
use sidewind_core::{SideWindMessage, SIDEWIND_TAG};

/// Tamaño máximo de un mensaje IPC (buffer y Raw). Única constante para no desincronizar.
pub const MAX_MSG_LEN: usize = 512;

// ============================================================================
// Tags y payloads de mensajes de control (origen único para parse y envío)
// ============================================================================

pub const TAG_SUBS: &[u8; 4] = b"SUBS";
pub const TAG_INPT: &[u8; 4] = b"INPT";
pub const TAG_SWND: &[u8; 4] = b"SWND";
pub const GET_INPUT_PID_MSG: &[u8; 13] = b"GET_INPUT_PID";

pub const TAG_NETW: &[u8; 4] = b"NETW";
pub const GET_NETWORK_PID_MSG: &[u8; 15] = b"GET_NETWORK_PID";
pub const TAG_NSTA: &[u8; 4] = b"NSTA";
pub const GET_NET_STATS_MSG: &[u8; 13] = b"GET_NET_STATS";
pub const TAG_SVCS: &[u8; 4] = b"SVCS";
pub const TAG_WAYL: &[u8; 4] = b"WAYL";
/// Línea de log del kernel (HUD). Enviada con from=0 cuando el logo ya está dibujado.
pub const TAG_KLOG: &[u8; 4] = b"KLOG";


/// Construye el payload de suscripción (SUBS + self_pid little-endian).
pub fn build_subscribe_payload(self_pid: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(TAG_SUBS);
    buf[4..8].copy_from_slice(&self_pid.to_le_bytes());
    buf
}

/// Construye el payload de respuesta de PID de input (INPT + pid little-endian).
pub fn build_input_pid_response_payload(pid: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(TAG_INPT);
    buf[4..8].copy_from_slice(&pid.to_le_bytes());
    buf
}

/// Mensaje IPC de Eclipse OS tipado y comprobado en compilación.
/// Cubre todos los tipos de comunicación inter-proceso actuales.
#[derive(Debug, Clone)]
pub enum EclipseMessage {
    /// Evento de entrada (teclado / ratón / USB HID).
    /// Llega vía fast path (≤24 bytes, datos en registros CPU).
    Input(InputEvent),

    /// Mensaje del protocolo SideWind (crear/destruir/actualizar ventana).
    /// Llega vía slow path (56 bytes, buffer en memoria).
    /// El segundo campo es el PID del proceso cliente.
    SideWind(SideWindMessage, u32),

    /// Control: solicitud de suscripción a eventos de input.
    Subscribe { subscriber_pid: u32 },

    /// Control: solicitud del PID del input_service.
    GetInputPid,

    /// Respuesta al GetInputPid con el PID real.
    InputPidResponse { pid: u32 },

    /// Control: solicitud del PID del network_service.
    GetNetworkPid,

    /// Respuesta al GetNetworkPid con el PID real.
    NetworkPidResponse { pid: u32 },

    /// Control: solicitud de estadisticas de red.
    GetNetStats,

    /// Respuesta al GetNetStats con rx y tx.
    NetStatsResponse { rx: u64, tx: u64 },

    /// Respuesta con información de servicios desde SystemD.
    ServiceInfoResponse { data: [u8; MAX_MSG_LEN], len: usize },

    /// Línea de log del kernel para el HUD (from=0, prefijo KLOG). Llega cuando el logo ya está dibujado.
    Log { line: [u8; 252], len: usize },

    /// Mensaje del protocolo Wayland (id: u32, size+op: u32, args...).
    /// Llega con prefijo "WAYL".
    Wayland { data: [u8; MAX_MSG_LEN - 4], len: usize },

    /// Mensaje desconocido/raw (fallback para extensibilidad futura).
    Raw { data: [u8; MAX_MSG_LEN], len: usize, from: u32 },
}

// Implementación de los parsers. Visibilidad pub(crate) o pub según feature "testable".
mod impl_parse {
    use super::*;
    use core::option::Option::{self, Some, None};
    use core::cmp::Ord;

    #[cfg_attr(not(feature = "testable"), allow(dead_code))]
    pub fn parse_fast(data: &[u8; 24], _from: u32, len: usize) -> Option<EclipseMessage> {
        if len == core::mem::size_of::<InputEvent>() {
            let ev = unsafe { core::ptr::read_unaligned(data.as_ptr() as *const InputEvent) };
            return Some(EclipseMessage::Input(ev));
        }
        
        // Handle small control/response messages that arrive via fast path (<= 24 bytes)
        if len >= 8 {
            if data[0..4] == *TAG_INPT {
                let mut pid_bytes = [0u8; 4];
                pid_bytes.copy_from_slice(&data[4..8]);
                return Some(EclipseMessage::InputPidResponse {
                    pid: u32::from_le_bytes(pid_bytes),
                });
            }
            if data[0..4] == *TAG_NETW {
                let mut pid_bytes = [0u8; 4];
                pid_bytes.copy_from_slice(&data[4..8]);
                return Some(EclipseMessage::NetworkPidResponse {
                    pid: u32::from_le_bytes(pid_bytes),
                });
            }
        }
        
        if len >= 20 && data[0..4] == *TAG_NSTA {
            let mut rx_bytes = [0u8; 8];
            let mut tx_bytes = [0u8; 8];
            rx_bytes.copy_from_slice(&data[4..12]);
            tx_bytes.copy_from_slice(&data[12..20]);
            return Some(EclipseMessage::NetStatsResponse {
                rx: u64::from_le_bytes(rx_bytes),
                tx: u64::from_le_bytes(tx_bytes),
            });
        }

        None
    }

    #[cfg_attr(not(feature = "testable"), allow(dead_code))]
    pub fn parse_slow(buf: &[u8], len: usize, from: u32) -> Option<EclipseMessage> {
        if len == 0 {
            return None;
        }
        if len >= core::mem::size_of::<SideWindMessage>() && len >= 4 && buf[0..4] == *TAG_SWND {
            let sw = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const SideWindMessage) };
            if sw.tag == SIDEWIND_TAG {
                return Some(EclipseMessage::SideWind(sw, from));
            }
        }
        if len >= 8 && buf[0..4] == *TAG_SUBS {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buf[4..8]);
            return Some(EclipseMessage::Subscribe {
                subscriber_pid: u32::from_le_bytes(pid_bytes),
            });
        }
        if len >= 13 && buf[0..13] == *GET_INPUT_PID_MSG {
            return Some(EclipseMessage::GetInputPid);
        }
        if len >= 8 && buf[0..4] == *TAG_INPT {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buf[4..8]);
            return Some(EclipseMessage::InputPidResponse {
                pid: u32::from_le_bytes(pid_bytes),
            });
        }
        if len >= 15 && buf[0..15] == *GET_NETWORK_PID_MSG {
            return Some(EclipseMessage::GetNetworkPid);
        }
        if len >= 8 && buf[0..4] == *TAG_NETW {
            let mut pid_bytes = [0u8; 4];
            pid_bytes.copy_from_slice(&buf[4..8]);
            return Some(EclipseMessage::NetworkPidResponse {
                pid: u32::from_le_bytes(pid_bytes),
            });
        }
        if len >= 13 && buf[0..13] == *GET_NET_STATS_MSG {
            return Some(EclipseMessage::GetNetStats);
        }
        if len >= 20 && buf[0..4] == *TAG_NSTA {
            let mut rx_bytes = [0u8; 8];
            let mut tx_bytes = [0u8; 8];
            rx_bytes.copy_from_slice(&buf[4..12]);
            tx_bytes.copy_from_slice(&buf[12..20]);
            return Some(EclipseMessage::NetStatsResponse {
                rx: u64::from_le_bytes(rx_bytes),
                tx: u64::from_le_bytes(tx_bytes),
            });
        }
        if len >= 8 && buf[0..4] == *TAG_SVCS {
            let mut data = [0u8; MAX_MSG_LEN];
            let copy_len = len.min(MAX_MSG_LEN);
            data[..copy_len].copy_from_slice(&buf[..copy_len]);
            return Some(EclipseMessage::ServiceInfoResponse { data, len: copy_len });
        }
        if len >= 4 && buf[0..4] == *TAG_WAYL {
            let mut data = [0u8; MAX_MSG_LEN - 4];
            let payload_len = len.saturating_sub(4).min(MAX_MSG_LEN - 4);
            data[..payload_len].copy_from_slice(&buf[4..4 + payload_len]);
            return Some(EclipseMessage::Wayland { data, len: payload_len });
        }
        if from == 0 && len >= 4 && buf[0..4] == *TAG_KLOG {
            let mut line = [0u8; 252];
            let line_len = (len - 4).min(252);
            line[..line_len].copy_from_slice(&buf[4..4 + line_len]);
            return Some(EclipseMessage::Log { line, len: line_len });
        }

        if len == core::mem::size_of::<InputEvent>() {
            let ev = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const InputEvent) };
            return Some(EclipseMessage::Input(ev));
        }
        let mut data = [0u8; MAX_MSG_LEN];
        let copy_len = len.min(MAX_MSG_LEN);
        data[..copy_len].copy_from_slice(&buf[..copy_len]);
        Some(EclipseMessage::Raw { data, len: copy_len, from })
    }
}

#[cfg(any(test, feature = "testable"))]
pub use impl_parse::{parse_fast, parse_slow};
#[cfg(not(any(test, feature = "testable")))]
pub(crate) use impl_parse::{parse_fast, parse_slow};
