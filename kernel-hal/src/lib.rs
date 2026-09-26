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

/// The per-CPU block, pulled into the host build for its tests alone.
///
/// `bare/` is `not(feature = "libos")`, and the host suite is a `libos` build,
/// so nothing outside a kernel build had ever compiled this module — while the
/// emulator, which does, boots one or two cores and never exercises what it
/// decides. The module carries its own seams for the two things it cannot have
/// on a host (the per-CPU register and the hardware CPU id); everything else it
/// runs here is the code the machine runs.
#[cfg(all(test, feature = "libos"))]
#[path = "bare/percpu.rs"]
mod bare_percpu;

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
    addr, affinity_walk, cache_maint, cmdline, console, context, deadline, defs::*, dma_pin,
    dma_quarantine, fault_diag, fault_slots, hart_walk, ipi::*, kaddr, kstats, ksyms, oops_log,
    panic_lock, phys_watch, timer_waker, user, watchpoint,
};
pub use config::KernelConfig;
pub use imp::{
    boot::{primary_init, primary_init_early, secondary_init},
    *,
};
pub use kernel_handler::KernelHandler;
pub use utils::{deferred_job, lazy_init::LazyInit, mpsc_queue::MpscQueue};
