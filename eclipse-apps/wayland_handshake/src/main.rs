#![no_std]
#![no_main]

use eclipse_libc::{println, send, receive, yield_cpu};
use sidewind_sdk::discover_composer;
use sidewind_core::MSG_TYPE_WAYLAND;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[WAYLAND-TEST] Starting Wayland Handshake Test...");

    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            println!("[WAYLAND-TEST] Discovered compositor at PID {}", pid);
            break pid;
        }
        yield_cpu();
    };

    // 1. Send wl_display::get_registry(new_id = 2)
    // Wayland wire: object_id(1), size(12)<<16 | opcode(1), new_id(2)
    let registry_id = 2u32;
    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(&1u32.to_le_bytes()); // object 1 (wl_display)
    msg[4..8].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes()); // size 12, opcode 1
    msg[8..12].copy_from_slice(&registry_id.to_le_bytes());

    println!("[WAYLAND-TEST] Sending wl_display.get_registry...");
    let _ = send(composer_pid, MSG_TYPE_WAYLAND, &msg);

    // 2. Wait for wl_registry::global event
    // Expected: object_id(2), size(?), opcode(0), name(1), iface("wl_compositor"), version(4)
    println!("[WAYLAND-TEST] Waiting for wl_registry.global event...");
    loop {
        let mut buffer = [0u8; 256];
        let (len, sender) = receive(&mut buffer);
        if len > 0 && sender == composer_pid {
            let obj_id = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
            let size_op = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
            let opcode = (size_op & 0xFFFF) as u16;
            
            println!("[WAYLAND-TEST] Received message: obj={}, op={}", obj_id, opcode);
            
            if obj_id == registry_id && opcode == 0 {
                // Parse global event
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
        yield_cpu();
    }

    println!("[WAYLAND-TEST] Handshake complete.");
    loop { yield_cpu(); }
}
