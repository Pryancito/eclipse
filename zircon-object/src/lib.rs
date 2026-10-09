//! Zircon kernel objects
//!
//! # Feature flags
//!
//! - `elf`: Enables `zircon_object::util::elf_loader`.
//! - `hypervisor`: Enables `zircon_object::hypervisor` (`Guest` and `Vcpu`).

#![no_std]
// The `#[bench]` rows that live next to the code they measure need
// `test::Bencher`, and the harness that runs them is nightly-only.
// `cfg_attr(test, ...)` keeps the feature gate out of every build that is not
// the host test build, so the kernel still compiles on a toolchain without it.
#![cfg_attr(test, feature(test))]
#![deny(warnings)]
#![allow(unexpected_cfgs)]
// #![deny(missing_docs)] 形同虚设了

extern crate alloc;

#[macro_use]
extern crate log;

#[cfg(test)]
#[macro_use]
extern crate std;

// The bench harness. `extern crate test` is what makes `test::Bencher` and
// `test::black_box` nameable from the inline `mod benches` blocks; the rows
// live beside the mechanisms they time, inside the test module that can see
// those modules' private helpers. The `benches/*.rs` files are separate
// crates and carry their own `#![feature(test)]`.
#[cfg(test)]
extern crate test;

pub mod debuglog;
pub mod dev;
mod error;
#[cfg(feature = "hypervisor")]
pub mod hypervisor;
pub mod ipc;
pub mod object;
pub mod signal;
pub mod task;
pub mod util;
pub mod vm;

pub use self::error::*;
