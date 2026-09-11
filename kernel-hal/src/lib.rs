//! Hardware Abstraction Layer

#![cfg_attr(not(feature = "libos"), no_std)]
#![cfg_attr(feature = "libos", feature(thread_id_value))]
#![feature(doc_cfg)]
// #![feature(core_intrinsics)]
#![allow(clippy::uninit_vec)]
#![deny(warnings)]
#![allow(unsafe_code)]
// JUST FOR DEBUG
#![allow(dead_code)]

extern crate alloc;
#[macro_use]
extern crate log;
#[macro_use]
extern crate cfg_if;
#[macro_use]
extern crate lazy_static;

#[macro_use]
mod macros;

mod common;
pub mod config;
mod hal_fn;
mod kernel_handler;
mod utils;

pub mod drivers;

/// Interrupt-safe synchronization primitives shared by kernel subsystems.
pub mod sync {
    pub use zcore_drivers::sync::*;
}

cfg_if! {
    if #[cfg(feature = "libos")] {
        #[path = "libos/mod.rs"]
        mod imp;
    } else {
        #[path = "bare/mod.rs"]
        mod imp;
    }
}

pub(crate) use config::KCONFIG;
pub(crate) use kernel_handler::KHANDLER;

/// Whether this HAL runs guest user code natively inside a host process
/// (`libos`) instead of on the machine itself.
///
/// The two differ in what a user context can actually carry: the libos
/// trap path, for one, saves and restores only the user `fsbase` and
/// discards `gsbase` outright (`push 0  # ignore gs_base` in
/// `syscall_fn_entry`), because the host runtime owns `gs`. Callers that
/// would otherwise promise user code something the HAL cannot deliver ask
/// here first.
pub const LIBOS: bool = cfg!(feature = "libos");

#[cfg(feature = "graphic")]
pub use common::boot_logo;
pub use common::{
    addr, console, context, defs::*, ipi::*, kstats, oops_log, timer_waker, user, watchpoint,
};
pub use config::KernelConfig;
pub use imp::{
    boot::{primary_init, primary_init_early, secondary_init},
    *,
};
pub use kernel_handler::KernelHandler;
pub use utils::{deferred_job, lazy_init::LazyInit, mpsc_queue::MpscQueue};
