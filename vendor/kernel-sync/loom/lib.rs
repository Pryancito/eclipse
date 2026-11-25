//! The `lock` crate's lock algorithms, compiled against loom.
//!
//! Each module below is the kernel's own source file (`#[path]` into
//! `../src`), not a copy: `prims.rs` there hands them loom's atomics under
//! `cfg(loom)`, which `build.rs` sets for this crate only. What is not the
//! kernel's is the interrupt layer (`interrupt.rs` here: interrupt masking is
//! per-CPU and outside what loom models) and the models themselves.
#![allow(dead_code, unused_imports)]

pub const MAX_CORE_NUM: usize = 64;

#[path = "../src/prims.rs"]
mod prims;

mod interrupt;

#[path = "../src/fn_slot.rs"]
pub mod fn_slot;

#[path = "../src/deadlock.rs"]
mod deadlock;

#[path = "../src/ticket.rs"]
pub mod ticket;

#[path = "../src/spin.rs"]
pub mod spin;

#[path = "../src/mcslock.rs"]
pub mod mcslock;

#[path = "../src/rwlock.rs"]
pub mod rwlock;

#[cfg(test)]
mod models;
