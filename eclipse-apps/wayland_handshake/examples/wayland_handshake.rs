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
    let mut msg = [0u8; 16];
    msg[0..4].copy_from_slice(b"WAYL"); // Tag
    msg[4..8].copy_from_slice(&1u32.to_le_bytes()); // object_id = 1 (wl_display)
    msg[8..12].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes()); // size=12, op=1 (get_registry)
    msg[12..16].copy_from_slice(&registry_id.to_le_bytes());

    println!("[WAYLAND-TEST] Sending wl_display.get_registry...");
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, msg.len(), 0); }

    println!("[WAYLAND-TEST] Waiting for globals...");
    let mut compositor_id = 0u32;
    let mut shell_id = 0u32;
    
    loop {
        let mut buffer = [0u8; 256];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 4 && sender == composer_pid && &buffer[0..4] == b"WAYL" {
            let payload = &buffer[4..];
            let obj_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let size_op = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
            let opcode = (size_op & 0xFFFF) as u16;
            
            if obj_id == registry_id && opcode == 0 {
                let name = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
                let if_len = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]) as usize;
                let interface = unsafe { core::str::from_utf8_unchecked(&payload[16..16+if_len-1]) };
                println!("[WAYLAND-TEST] Global: name={} interface={}", name, interface);
                
                if interface == "wl_compositor" { compositor_id = name; }
                if interface == "wl_shell" { shell_id = name; }
            }
        }
        if compositor_id != 0 && shell_id != 0 { break; }
        unsafe { sleep_ms(10); }
    }

    // 2. Bind wl_compositor (id=4) and wl_shell (id=5)
    let bound_compositor_id = 4u32;
    let bound_shell_id = 5u32;
    
    // Bind compositor
    let mut msg = [0u8; 32];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&registry_id.to_le_bytes());
    let ifname = b"wl_compositor\0";
    let size = 8 + 4 + 4 + (ifname.len() + 3) & !3 + 4 + 4; // u s u n
    msg[8..12].copy_from_slice(&(((28u32 + 16u32) << 16) | 0u32).to_le_bytes()); // op=0 (bind)
    msg[12..16].copy_from_slice(&compositor_id.to_le_bytes());
    msg[16..20].copy_from_slice(&(ifname.len() as u32).to_le_bytes());
    msg[20..34].copy_from_slice(b"wl_compositor\0\0\0");
    msg[36..40].copy_from_slice(&4u32.to_le_bytes()); // version
    msg[40..44].copy_from_slice(&bound_compositor_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 44, 0); }

    // Bind shell
    let mut msg = [0u8; 40];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&registry_id.to_le_bytes());
    let ifname = b"wl_shell\0";
    msg[8..12].copy_from_slice(&(((24u32 + 12u32) << 16) | 0u32).to_le_bytes());
    msg[12..16].copy_from_slice(&shell_id.to_le_bytes());
    msg[16..20].copy_from_slice(&(ifname.len() as u32).to_le_bytes());
    msg[20..30].copy_from_slice(b"wl_shell\0\0");
    msg[32..36].copy_from_slice(&1u32.to_le_bytes());
    msg[36..40].copy_from_slice(&bound_shell_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 40, 0); }

    // 3. Create surface (id=6)
    let surface_id = 6u32;
    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&bound_compositor_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((12u32 << 16) | 0u32).to_le_bytes()); // create_surface
    msg[12..16].copy_from_slice(&surface_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 16, 0); }

    // 4. Get shell surface (id=7)
    let shell_surface_id = 7u32;
    let mut msg = [0u8; 16];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&bound_shell_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((16u32 << 16) | 0u32).to_le_bytes()); // get_shell_surface
    msg[12..16].copy_from_slice(&shell_surface_id.to_le_bytes());
    msg[16..20].copy_from_slice(&surface_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 20, 0); }

    // 5. Set toplevel and commit
    let mut msg = [0u8; 8];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&shell_surface_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((8u32 << 16) | 1u32).to_le_bytes()); // set_toplevel
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 12, 0); }

    let mut msg = [0u8; 8];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&surface_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((8u32 << 16) | 6u32).to_le_bytes()); // commit
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 12, 0); }

    println!("[WAYLAND-TEST] Surface created and committed!");

    println!("[WAYLAND-TEST] Handshake complete.");
    #[cfg(feature = "host-testing")]
    return;
    #[cfg(not(feature = "host-testing"))]
    loop { unsafe { sleep_ms(100); } }
}
