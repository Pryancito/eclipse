//! Trait de codificación para mensajes de fast path (≤24 bytes en registros)

use eclipse_libc::InputEvent;

/// Trait que deben implementar los mensajes que quieran usar el fast path IPC.
/// El fast path pasa los datos directamente en registros CPU (rdi/rsi/rdx),
/// sin ningún buffer en memoria de usuario. Solo válido si size ≤ 24 bytes.
pub trait EclipseEncode {
    /// Codifica el mensaje en 24 bytes (little-endian) para el fast path.
    fn encode_fast(&self) -> [u8; 24];

    /// Tipo de mensaje IPC (MSG_TYPE_*) para el campo msg_type del kernel.
    fn msg_type() -> u32;

    /// Tamaño real de los datos (puede ser < 24).
    fn data_size() -> u32;
}

impl EclipseEncode for InputEvent {
    fn encode_fast(&self) -> [u8; 24] {
        // InputEvent es #[repr(C)], 24 bytes con padding — podemos copiarlo directamente.
        let mut buf = [0u8; 24];
        unsafe {
            core::ptr::copy_nonoverlapping(
                self as *const InputEvent as *const u8,
                buf.as_mut_ptr(),
                core::mem::size_of::<InputEvent>(),
            );
        }
        buf
    }

    fn msg_type() -> u32 {
        crate::services::MSG_TYPE_INPUT
    }

    fn data_size() -> u32 {
        core::mem::size_of::<InputEvent>() as u32
    }
}
