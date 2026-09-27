//! Useful synchronization primitives.
#![deny(missing_docs)]

/// The test-only switch behind [`wait_interrupted`], so every interruptible
/// wait in the crate can be driven from a host test.
#[cfg(test)]
pub(crate) use self::event_bus::test_interrupt;
pub use self::event_bus::*;
pub use self::semaphore::*;

mod event_bus;
mod semaphore;
pub mod shared_futex;
