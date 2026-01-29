//! FFI bindings for libwayland and wlroots
//! 
//! This module provides Rust bindings to the C libraries

pub mod libwayland;
pub mod wlroots;

/// Common types used across bindings
pub type WaylandResult<T> = Result<T, WaylandError>;

#[derive(Debug, Clone, Copy)]
pub enum WaylandError {
    NotAvailable,
    InitFailed,
    InvalidArgument,
    OutOfMemory,
}

impl core::fmt::Display for WaylandError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WaylandError::NotAvailable => write!(f, "Library not available"),
            WaylandError::InitFailed => write!(f, "Initialization failed"),
            WaylandError::InvalidArgument => write!(f, "Invalid argument"),
            WaylandError::OutOfMemory => write!(f, "Out of memory"),
        }
    }
}
