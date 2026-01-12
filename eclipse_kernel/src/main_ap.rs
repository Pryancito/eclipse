//! Entry point for Application Processors (secondary cores)

use core::sync::atomic::{AtomicU32, Ordering};
use crate::drivers::advanced::acpi::get_acpi_manager;

/// Counter of active APs
pub static AP_ONLINE_COUNT: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    // 1. We are in 64-bit mode, paging enabled, interrupts disabled.
    // However, we are running on a temporary stack (the 16-bit one at 0x8000).
    // We MUST switch to a proper stack immediately.
    // For this simple implementation, we might just use a small static stack per core 
    // or assume the 0x8000 stack is enough for basic initialization (risky).
    
    // Increment online count to signal the BSP we are alive.
    AP_ONLINE_COUNT.fetch_add(1, Ordering::SeqCst);
    
    // Get Local APIC ID to identify ourselves
    // We can't print easily because the serial port lock might be held by BSP.
    // We'll trust the count for now.
    
    // TODO: Init IDT, Lapic, Scheduler...
    
    // Spin forever
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}
