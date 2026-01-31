//! EclipseFS Userspace Server
//! 
//! Servidor de sistema de archivos EclipseFS para el microkernel Eclipse OS.
//! Este servidor se ejecuta en espacio de usuario y proporciona todas las
//! operaciones del sistema de archivos EclipseFS vía IPC.

pub mod server;
pub mod messages;
pub mod operations;

pub use server::EclipseFSServer;
pub use messages::{Message, MessageType, EclipseFSCommand};
pub use operations::FileDescriptor;

/// Versión del servidor EclipseFS
pub const ECLIPSEFS_SERVER_VERSION: &str = "0.1.0";
