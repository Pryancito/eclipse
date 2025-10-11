//! Wrappers nativos de syscalls para operaciones del instalador
//! 
//! Este módulo proporciona wrappers Rust sobre syscalls de Linux
//! para evitar dependencias en binarios externos.

pub mod mount;
pub mod disk;
pub mod partition;

pub use mount::*;
pub use disk::*;
pub use partition::*;

