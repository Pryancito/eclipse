//! `eclipse_ipc` - Biblioteca IPC tipada para Eclipse OS
//!
//! Proporciona una API de alto nivel, tipada y cero-copia para comunicación
//! entre procesos en Eclipse OS. Construida sobre `eclipse_libc`.
//!
//! # Ejemplo
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

#![cfg_attr(not(target_env = "gnu"), no_std)]

pub mod channel;
pub mod protocol;
pub mod services;
pub mod types;

#[cfg(feature = "testable")]
pub use types::{parse_fast, parse_slow};

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
}
