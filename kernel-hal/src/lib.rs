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

/// DEBUG: detector de frame compartido ESCRIBIBLE entre procesos vivos
/// (COW-break fallido). `frame_dealloc` limpia al liberar; el handler de
/// page-fault registra/comprueba el pid dueño de cada frame mapeado escribible.
pub mod dbg_frameowner {
    use core::sync::atomic::{AtomicU32, Ordering};
    const N: usize = 1 << 20; // cubre hasta 4 GiB
    static OWNER: [AtomicU32; N] = {
        const Z: AtomicU32 = AtomicU32::new(0);
        [Z; N]
    };
    /// Limpiar el dueño al liberar el frame (idx = paddr >> 12).
    pub fn clear(frame_idx: usize) {
        if frame_idx < N {
            OWNER[frame_idx].store(0, Ordering::Relaxed);
        }
    }
    /// Registrar `pid` como dueño escribible de `frame_idx`. Devuelve `Some(prev)`
    /// si el frame ya estaba mapeado escribible por OTRO pid vivo (COW-break fail).
    pub fn set_check(frame_idx: usize, pid: u32) -> Option<u32> {
        if frame_idx >= N {
            return None;
        }
        let prev = OWNER[frame_idx].swap(pid, Ordering::Relaxed);
        if prev != 0 && prev != pid {
            Some(prev)
        } else {
            None
        }
    }
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

#[cfg(feature = "graphic")]
pub use common::boot_logo;
pub use common::{addr, console, context, defs::*, ipi::*, user};
pub use config::KernelConfig;
pub use imp::{
    boot::{primary_init, primary_init_early, secondary_init},
    *,
};
pub use kernel_handler::KernelHandler;
pub use utils::{deferred_job, lazy_init::LazyInit, mpsc_queue::MpscQueue};
