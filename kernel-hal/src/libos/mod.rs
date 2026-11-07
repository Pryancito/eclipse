mod drivers;
mod dummy;
mod mock_mem;

pub mod boot;
pub mod config;
pub mod cpu;
pub mod input_poll;
pub mod interrupt;
pub mod mem;
pub mod net;
pub mod stack_guard;
pub mod thread;
pub mod timer;
pub mod vdso;
pub mod vm;

#[path = "special.rs"]
#[doc(cfg(feature = "libos"))]
pub mod libos;

pub use super::hal_fn::rand;

hal_fn_impl_default!(rand, super::hal_fn::console);

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
mod macos;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod fault_bridge;
/// The same bridge for the other Linux host, under the same name so the
/// callers (`context.rs`, `boot.rs`, the loader) read one module path.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[path = "fault_bridge_aarch64.rs"]
pub mod fault_bridge;

/// Non-SMP initialization.
pub fn init() {
    drivers::init_early();
    boot::primary_init();
}
