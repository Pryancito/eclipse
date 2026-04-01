//! Synchronization Module - Re-exports and internal primitive implementations
pub mod mod_impl;
pub use self::mod_impl::{Mutex, MutexGuard, Condvar};
pub use alloc::sync::{Arc, Weak};
