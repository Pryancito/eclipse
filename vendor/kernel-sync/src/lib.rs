#![no_std]

cfg_if::cfg_if! {
    if #[cfg(all(target_os = "none", feature = "ticket"))] {
        extern crate alloc;
        mod interrupt;
        pub use interrupt::current_cpu_id;
        #[cfg(any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "riscv32",
            target_arch = "riscv64"
        ))]
        pub use interrupt::set_logical_cpu_id;
        pub mod mcslock;
        pub mod rwlock;
        pub use {rwlock::*, mcslock::*};
        pub mod ticket;
        pub use ticket::{TicketMutex as Mutex, TicketMutexGuard as MutexGuard};
    } else if #[cfg(target_os = "none")] {
        extern crate alloc;
        mod interrupt;
        pub use interrupt::current_cpu_id;
        #[cfg(any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "riscv32",
            target_arch = "riscv64"
        ))]
        pub use interrupt::set_logical_cpu_id;
        pub mod mcslock;
        pub mod rwlock;
        pub use {rwlock::*, mcslock::*};
        pub mod spin;
        pub use spin::{SpinMutex as Mutex, SpinMutexGuard as MutexGuard};
    } else {
        pub use spin::*;
    }
}
