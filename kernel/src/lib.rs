//! Eclipse Microkernel - Biblioteca del kernel
//! 
//! Este módulo exporta las funcionalidades del microkernel como biblioteca

#![no_std]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

pub mod boot;
pub mod memory;
pub mod interrupts;
pub mod ipc;
pub mod serial;

// Re-exportar tipos importantes
pub use ipc::{Message, MessageType, ServerId, ClientId};
pub use memory::{PageTable, PageTableEntry};
pub use interrupts::InterruptStats;

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
