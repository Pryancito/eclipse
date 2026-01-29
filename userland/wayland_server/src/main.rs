//! Wayland Server Main Entry Point

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;
use linked_list_allocator::LockedHeap;
use wayland_server::*;

/// Heap size (2MB)
const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Global allocator
#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// Initialize allocator
fn init_allocator() {
    unsafe {
        static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
        HEAP.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

/// Syscall definitions
const SYS_WRITE: i64 = 1;
const SYS_EXIT: i64 = 60;
const STDOUT: i32 = 1;

/// Write to stdout
fn print(msg: &str) {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_WRITE,
            in("rdi") STDOUT,
            in("rsi") msg.as_ptr(),
            in("rdx") msg.len(),
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

/// Exit process
fn exit(code: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_EXIT,
            in("rdi") code,
            options(noreturn)
        );
    }
}

/// Main entry point
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Initialize heap
    init_allocator();

    print("=== Wayland Server for Eclipse OS ===\n");
    print("Initializing Wayland compositor...\n");

    // Create server
    let mut server = WaylandServer::new();
    print("Server created\n");

    // Create compositor
    let mut compositor = Compositor::new();
    print("Compositor created\n");

    // Bind socket
    print("Binding to /tmp/wayland-0...\n");
    let mut socket = match UnixSocket::bind("/tmp/wayland-0") {
        Ok(s) => {
            print("Socket bound successfully\n");
            s
        }
        Err(e) => {
            print("Failed to bind socket\n");
            exit(1);
        }
    };

    // Listen for connections
    if let Err(_) = socket.listen(5) {
        print("Failed to listen on socket\n");
        exit(1);
    }
    print("Listening for client connections...\n");

    server.running = true;
    print("Wayland server running\n");

    // Main event loop
    let mut iteration = 0;
    while server.running && iteration < 100 {
        // In a real implementation:
        // 1. Poll for new connections
        // 2. Read messages from clients
        // 3. Process messages
        // 4. Render frames
        // 5. Send events to clients

        // Render frame
        if let Err(_) = compositor.render() {
            print("Render error\n");
        }

        iteration += 1;
        
        // Simulate some work
        if iteration % 20 == 0 {
            print("Server iteration...\n");
        }
    }

    print("Wayland server shutting down...\n");
    
    // Cleanup
    let _ = socket.close();
    
    print("Wayland server terminated\n");
    exit(0);
}
