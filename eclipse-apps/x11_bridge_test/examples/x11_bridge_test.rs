//! X11 Bridge Test: descubre compositor y envía MapRequest de prueba.
//! Ejecutar en Eclipse con compositor, no con cargo test.

#![cfg_attr(not(feature = "host-testing"), no_main)]

use eclipse_libc::{println, eclipse_send as send, sleep_ms};
use sidewind::{discover_composer, MSG_TYPE_X11};

#[cfg(not(feature = "host-testing"))]
#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    run_test();
    0
}

#[cfg(feature = "host-testing")]
fn main() {
    run_test();
}

const DISCOVER_RETRIES: u32 = 30;

fn run_test() {
    println!("[X11-BRIDGE] Starting X11 Bridge Test...");

    let composer_pid = 'discover: loop {
        for _ in 0..DISCOVER_RETRIES {
            if let Some(pid) = discover_composer() {
                println!("[X11-BRIDGE] Discovered compositor at PID {}", pid);
                break 'discover pid;
            }
            unsafe { sleep_ms(100); }
        }
        println!("[X11-BRIDGE] Skipped (no compositor after {} ms).", DISCOVER_RETRIES * 100);
        unsafe { eclipse_libc::exit(0); }
    };

    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(b"X11M");
    msg[4..8].copy_from_slice(b"MAP ");
    msg[8..12].copy_from_slice(&1234u32.to_le_bytes());

    println!("[X11-BRIDGE] Sending MapRequest for window 1234...");
    unsafe { let _ = send(composer_pid, MSG_TYPE_X11, msg.as_ptr() as *const core::ffi::c_void, msg.len(), 0); }

    println!("[X11-BRIDGE] Test message sent. Check smithay_app logs.");
    #[cfg(feature = "host-testing")]
    return;
    #[cfg(not(feature = "host-testing"))]
    loop { unsafe { sleep_ms(100); } }
}
