//! Handshake Wayland: descubre compositor, envía get_registry, espera global.
//! Ejecutar en Eclipse con compositor: run from init/launcher, no con cargo test.

#![cfg_attr(not(feature = "host-testing"), no_main)]

use eclipse_libc::{c_uint, println, eclipse_send as send, receive, sleep_ms};
use sidewind::{discover_composer, MSG_TYPE_WAYLAND};

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

const DISCOVER_RETRIES: u32 = 30; // ~3 s; luego skip para no colgar en host sin compositor

fn run_test() {
    println!("[WAYLAND-TEST] Starting Wayland Handshake Test...");

    #[cfg(feature = "host-testing")]
    if std::env::var("WAYLAND_HANDSHAKE_RUN").as_deref() != Ok("1") {
        println!("[WAYLAND-TEST] Skipped on host (no compositor). Set WAYLAND_HANDSHAKE_RUN=1 to run.");
        return;
    }

    let composer_pid = 'discover: loop {
        for _ in 0..DISCOVER_RETRIES {
            if let Some(pid) = discover_composer() {
                println!("[WAYLAND-TEST] Discovered compositor at PID {}", pid);
                break 'discover pid;
            }
            unsafe { sleep_ms(100); }
        }
        println!("[WAYLAND-TEST] Skipped (no compositor after {} ms).", DISCOVER_RETRIES * 100);
        unsafe { eclipse_libc::exit(0); }
    };

    // 1. Send wl_display::get_registry(new_id = 2)
    let registry_id = 2u32;
    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(&1u32.to_le_bytes());
    msg[4..8].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes());
    msg[8..12].copy_from_slice(&registry_id.to_le_bytes());

    println!("[WAYLAND-TEST] Sending wl_display.get_registry...");
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, msg.len(), 0); }

    println!("[WAYLAND-TEST] Waiting for wl_registry.global event...");
    loop {
        let mut buffer = [0u8; 256];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 0 && sender == composer_pid {
            let obj_id = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
            let size_op = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
            let opcode = (size_op & 0xFFFF) as u16;
            println!("[WAYLAND-TEST] Received message: obj={}, op={}", obj_id, opcode as c_uint);
            if obj_id == registry_id && opcode == 0 {
                let name = u32::from_le_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);
                let if_len = u32::from_le_bytes([buffer[12], buffer[13], buffer[14], buffer[15]]) as usize;
                let if_len_aligned = (if_len + 3) & !3;
                let interface = unsafe { core::str::from_utf8_unchecked(&buffer[16..16+if_len-1]) };
                let version_offset = 16 + if_len_aligned;
                let version = u32::from_le_bytes([buffer[version_offset], buffer[version_offset+1], buffer[version_offset+2], buffer[version_offset+3]]);
                println!("[WAYLAND-TEST] SUCCESS! Global: {} {} v{}", name, interface, version);
                break;
            }
        }
        unsafe { sleep_ms(10); }
    }

    println!("[WAYLAND-TEST] Handshake complete.");
    #[cfg(feature = "host-testing")]
    return;
    #[cfg(not(feature = "host-testing"))]
    loop { unsafe { sleep_ms(100); } }
}
