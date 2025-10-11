//! Librería para el instalador de Eclipse OS
//! 
//! Esta librería contiene las estructuras y funciones necesarias
//! para que el instalador pueda crear imágenes de EclipseFS y FAT32,
//! así como wrappers nativos para operaciones de sistema sin dependencias externas.

// Nota: installer-lib se compila con std porque el instalador se ejecuta en userspace
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

extern crate alloc;

pub mod filesystem;

#[cfg(feature = "std")]
pub mod sys;

// Re-exportar las estructuras principales
pub use filesystem::eclipsefs::*;
pub use filesystem::fat32::*;
pub use filesystem::vfs::*;

#[cfg(feature = "std")]
pub use sys::*;
