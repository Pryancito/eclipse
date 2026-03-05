#![no_std]
#![no_main]

use eclipse_libc::{println, eclipse_send as send, sleep_ms};
use sidewind_sdk::discover_composer;
use sidewind_core::MSG_TYPE_X11;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[X11-BRIDGE] Starting X11 Bridge Test...");

    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            println!("[X11-BRIDGE] Discovered compositor at PID {}", pid);
            break pid;
        }
        sleep_ms(100);
    };

    // Simulate an X11 MapRequest (simplified)
    // We'll use the "X11M" tag followed by "MAP <id>"
    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(b"X11M");
    msg[4..8].copy_from_slice(b"MAP ");
    msg[8..12].copy_from_slice(&1234u32.to_le_bytes()); // window id 1234

    println!("[X11-BRIDGE] Sending MapRequest for window 1234...");
    unsafe { let _ = send(composer_pid, MSG_TYPE_X11, msg.as_ptr() as *const core::ffi::c_void, msg.len(), 0); }

    println!("[X11-BRIDGE] Test message sent. Check smithay_app logs.");
    
    loop { sleep_ms(100); }
}
