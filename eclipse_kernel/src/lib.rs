//! Eclipse Microkernel - Biblioteca del kernel
//! 
//! Este módulo exporta las funcionalidades del microkernel como biblioteca

#![cfg_attr(not(test), no_std)]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

extern crate alloc;

pub mod boot;
pub mod memory;
pub mod interrupts;
pub mod drm;
pub mod serial;
pub mod ipc;
pub mod process;
pub mod cpu;
pub mod scheduler;
pub mod syscalls;
pub mod servers;
pub mod binaries;
pub mod pci;
pub mod nvidia;
pub mod virtio;

pub mod filesystem;
pub mod ata;
pub mod ahci;
pub mod nvme;
pub mod storage;
pub mod fd;
pub mod progress;
pub mod scheme;
pub mod bcache;
pub mod usb_hid;
mod elf_loader;
pub mod acpi;
pub mod apic;
pub mod sw_cursor;   // Software cursor for real-hardware (non-VirtIO) EFI GOP framebuffer
pub mod sync;        // Synchronization primitives
mod memory_builtins;
mod drm_scheme;

// Re-exportar tipos importantes
pub use ipc::{Message, MessageType, ServerId, ClientId};
pub use memory::{PageTable, PageTableEntry};
pub use process::{Process, ProcessId, ProcessState, Context};
// pub use scheduler::SchedulerStats;
// pub use syscalls::SyscallStats;

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
