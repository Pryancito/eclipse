#![no_main]
extern crate std;
extern crate alloc;
extern crate eclipse_syscall;

pub mod compositor;
pub mod render;
pub mod input;
pub mod ipc;
pub mod space;
pub mod backend;
pub mod state;

use std::prelude::*;
use crate::state::SmithayState;

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    println!("[SMITHAY] Starting via Eclipse Runtime...");
    
    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");
    
    let input_pid = query_input_service_pid().expect("Failed to find input service");
    println!("[SMITHAY] Found input service at PID {}", input_pid);
    
    let my_pid = unsafe { std::libc::getpid() as u32 };
    subscribe_to_inputs(input_pid, my_pid);
    println!("[SMITHAY] Subscribed to input events");

    println!("[SMITHAY] Entering main loop");
    
    loop {
        state.counter += 1;
        
        if state.counter == 1 {
            state.backend.fb.pre_render_background();
        }
        
        if state.counter % 60 == 1 {
             println!("[SMITHAY] Rendering frame {}...", state.counter);
        }
        state.render();
        // Imprescindible: enviar el back buffer a pantalla (GOP o GPU). Sin esto no se dibuja nada.
        state.backend.swap_buffers();
        
        if state.counter % 60 == 1 {
             println!("[SMITHAY] Handling IPC...");
        }
        state.handle_ipc();
        
        // Si arrancamos en headless, intentar mapear framebuffer cuando esté disponible
        if state.counter % 120 == 0 {
            state.backend.fb.try_remap_framebuffer();
        }

        if state.counter % 400 == 0 {
            println!("[SMITHAY] Sending CUAPPA heartbeat to init...");
            unsafe {
                let _ = std::libc::eclipse_send(1, 0, b"HEART\0".as_ptr() as *const core::ffi::c_void, 6, 0);
            }
        }

        if state.counter % 1000 == 0 {
            println!("[SMITHAY] Stable loop iteration {}", state.counter);
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn query_input_service_pid() -> Option<u32> {
    let mut buf = [0u8; 8];
    // Request input PID from init (PID 1)
    // Retry up to 10 times to give init time to process the request
    for i in 0..50 {
        unsafe {
            // Use 0x40 (MessageType::Input) for P2P delivery guarantee
            let _ = std::libc::eclipse_send(1, 0x40, b"GET_INPUT_PID\0".as_ptr() as *const core::ffi::c_void, 14, 0);
            
            // Wait a bit for the response
            std::thread::sleep(std::time::Duration::from_millis(100));
            
            // Drain mailbox to find the INPT message
            let mut found_msg = false;
            loop {
                let mut from: u32 = 0;
                let len = std::libc::receive(buf.as_mut_ptr(), 8, &mut from);
                if len == 0 || from == 0 {
                    break;
                }
                
                if len >= 8 && &buf[..4] == b"INPT" {
                    let pid = u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4]));
                    println!("[SMITHAY] Received input service PID {} from init", pid);
                    return Some(pid);
                } else if len > 0 {
                    println!("[SMITHAY] Received other IPC msg (len {}, from {})", len, from);
                    found_msg = true;
                }
            }
            
            if i % 5 == 0 {
                println!("[SMITHAY] Waiting for input service PID (attempt {}, msg_found={})...", i, found_msg);
            }
        }
        // Wait before retrying
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

fn subscribe_to_inputs(input_pid: u32, my_pid: u32) {
    let mut msg = [0u8; 8];
    msg[..4].copy_from_slice(b"SUBS");
    msg[4..8].copy_from_slice(&my_pid.to_le_bytes());
    unsafe {
        let _ = std::libc::eclipse_send(input_pid as u32, 0, msg.as_ptr() as *const core::ffi::c_void, 8, 0);
    }
}
