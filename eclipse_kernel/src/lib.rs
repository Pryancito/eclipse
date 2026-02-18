//! Eclipse Microkernel - Biblioteca del kernel
//! 
//! Este módulo exporta las funcionalidades del microkernel como biblioteca

#![no_std]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

extern crate alloc;

// pub mod boot;
// pub mod memory;
// pub mod interrupts;
// pub mod ipc;
// pub mod serial;
// pub mod process;
// pub mod scheduler;
// pub mod syscalls;
// pub mod servers;
pub mod binaries;
// pub mod pci;
// pub mod nvidia;
// pub mod virtio;

// pub mod filesystem;
// pub mod ata;
// pub mod fd;  // File descriptor management
// pub mod bcache; // Buffer Cache
// pub mod scheme; // Redox-style schemes
// mod elf_loader;

// Re-exportar tipos importantes
// pub use ipc::{Message, MessageType, ServerId, ClientId};
// pub use memory::{PageTable, PageTableEntry};
// pub use interrupts::InterruptStats;
// pub use process::{Process, ProcessId, ProcessState, Context};
// pub use scheduler::SchedulerStats;
// pub use syscalls::SyscallStats;

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
