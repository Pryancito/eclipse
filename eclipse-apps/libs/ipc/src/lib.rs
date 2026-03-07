//! `eclipse_ipc` - Biblioteca IPC tipada para Eclipse OS
//!
//! Proporciona una API de alto nivel, tipada y cero-copia para comunicación
//! entre procesos en Eclipse OS. Construida sobre `eclipse_libc`.
//!
//! # Ejemplo
//!
//! ```no_run
//! use eclipse_ipc::prelude::*;
//!
//! let mut ch = IpcChannel::new();
//! if let Some(msg) = ch.recv() {
//!     match msg {
//!         EclipseMessage::Input(ev) => { /* manejar evento */ }
//!         EclipseMessage::SideWind(sw, pid) => { /* manejar ventana */ }
//!         _ => {}
//!     }
//! }
//! ```
#![no_std]

pub mod channel;
pub mod async_channel;
pub mod protocol;
pub mod services;
pub mod types;

#[cfg(not(target_os = "linux"))]
extern crate eclipse_libc;

#[cfg(target_os = "linux")]
pub mod eclipse_libc {
    use eclipse_syscall::InputEvent;

    pub unsafe fn receive(_buf: *mut u8, _len: usize, _from: *mut u32) -> usize { 0 }
    pub fn receive_fast() -> Option<([u8; 24], u32, usize)> { None }
    pub unsafe fn eclipse_send(_dest: u32, _msg_type: u32, _buf: *const core::ffi::c_void, _len: usize, _flags: usize) -> usize { 0 }
    pub unsafe fn yield_cpu() {}
}

#[cfg(feature = "testable")]
pub use types::{parse_fast, parse_slow};

#[cfg(feature = "async")]
pub use crate::async_channel::block_on;

pub mod prelude {
    pub use crate::channel::IpcChannel;
    pub use crate::services::{
        INPUT_SERVICE_PID,
        MSG_TYPE_SYSTEM, MSG_TYPE_MEMORY, MSG_TYPE_FILESYSTEM, MSG_TYPE_NETWORK,
        MSG_TYPE_GRAPHICS, MSG_TYPE_AUDIO, MSG_TYPE_INPUT, MSG_TYPE_AI,
        MSG_TYPE_SECURITY, MSG_TYPE_USER, MSG_TYPE_SIGNAL,
        DEFAULT_INPUT_QUERY_ATTEMPTS,
        query_input_service_pid, query_input_service_pid_with_attempts, subscribe_to_input,
    };
    pub use crate::types::{EclipseMessage, MAX_MSG_LEN};
    pub use crate::protocol::EclipseEncode;

    #[cfg(feature = "async")]
    pub use crate::async_channel::{block_on, RecvFuture};
}

#[cfg(any(test, feature = "testable"))]
pub mod tests;
