//! CPU information.

use cortex_a::registers::*;
use tock_registers::interfaces::Readable;

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            // Use Aff0 (bits 7:0) of MPIDR_EL1 as CPU ID — supports up to 8 CPUs.
            let id = MPIDR_EL1.get() & 0xFF;
            id as u8
        }

        fn cpu_frequency() -> u16 {
            0
        }

        fn reset() -> ! {
            info!("shutdown...");
            let psci_system_off = 0x8400_0008_usize;
            unsafe {
                core::arch::asm!(
                    "hvc #0",
                    in("x0") psci_system_off
                );
            }
            unreachable!()
        }
    }
}
