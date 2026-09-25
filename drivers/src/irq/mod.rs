//! External interrupt request and handle.

cfg_if::cfg_if! {
    if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv_intc;
        mod riscv_plic;

        /// Implementation of risc-v interrupt controller.
        #[doc(cfg(any(target_arch = "riscv32", target_arch = "riscv64")))]
        pub mod riscv {
            pub use super::riscv_intc::{Intc, ScauseIntCode};
            pub use super::riscv_plic::Plic;
        }
    } else if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
        mod x86_apic;
        /// Implementation of x86 Advanced Programmable Interrupt Controller.
        #[doc(cfg(any(target_arch = "x86", target_arch = "x86_64")))]
        pub mod x86 {
            pub use super::x86_apic::Apic;
        }
    } else if #[cfg(target_arch = "aarch64")] {
        pub mod gic_400;
    }
}

/// The GICv2 driver, pulled into the host build for its tests alone.
///
/// It is `cfg`-ed to aarch64 above, so no job that runs tests ever compiled a
/// line of it -- and what it gets wrong is per-core register banking, which is
/// exactly the sort of thing that needs a test rather than a boot. Nothing in
/// it is architecture-specific: the distributor and CPU interface are reached
/// through volatile `u32` accesses, which a host buffer answers as well as a
/// board does.
///
/// `dead_code` is allowed on this copy alone: nothing outside the module's own
/// tests calls into it here, while the real aarch64 build keeps every warning
/// it had.
#[cfg(all(test, not(target_arch = "aarch64")))]
#[path = "gic_400.rs"]
#[allow(dead_code)]
mod gic_400_host_tests;
