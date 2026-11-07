//! Bootstrap and initialization.

use crate::{KernelConfig, KernelHandler, KCONFIG, KHANDLER};

hal_fn_impl! {
    impl mod crate::hal_fn::boot {
        fn primary_init_early(cfg: KernelConfig, handler: &'static impl KernelHandler) {
            KCONFIG.init_once_by(cfg);
            KHANDLER.init_once_by(handler);
            super::drivers::init_early();
        }

        fn primary_init() {
            super::drivers::init();

            #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
            unsafe {
                super::macos::register_sigsegv_handler();
            }

            // A guest that faults in a libos build raises a HOST signal, in
            // this very process. Without this the host kills the lot --
            // kernel, guest and harness -- and a test that was waiting for
            // the exception never hears anything. See `fault_bridge`.
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            unsafe {
                super::fault_bridge::init();
            }

        }
    }
}
