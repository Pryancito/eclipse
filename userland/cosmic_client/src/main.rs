//! COSMIC Desktop Main Entry Point

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;
use linked_list_allocator::LockedHeap;
use cosmic_client::*;

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

    print("=== COSMIC Desktop Environment ===\n");
    print("Eclipse OS Modern Desktop\n\n");

    // Connect to Wayland
    print("Connecting to Wayland compositor...\n");
    let mut client = match WaylandClient::connect("/tmp/wayland-0") {
        Ok(c) => {
            print("Connected to Wayland\n");
            c
        }
        Err(_) => {
            print("Failed to connect to Wayland\n");
            exit(1);
        }
    };

    // Get registry
    print("Getting registry...\n");
    if let Err(_) = client.get_registry() {
        print("Failed to get registry\n");
        exit(1);
    }

    // Bind compositor
    print("Binding compositor...\n");
    if let Err(_) = client.bind(1, "wl_compositor", 4) {
        print("Failed to bind compositor\n");
        exit(1);
    }

    // Create panel surface
    print("Creating panel surface...\n");
    let panel_surface_id = match client.create_surface() {
        Ok(id) => {
            print("Panel surface created\n");
            id
        }
        Err(_) => {
            print("Failed to create panel surface\n");
            exit(1);
        }
    };

    // Create COSMIC panel
    print("Initializing COSMIC panel...\n");
    let mut panel = CosmicPanel::new(panel_surface_id, PanelPosition::Top);
    panel.layout_items();
    print("Panel initialized with items:\n");
    print("  - App Launcher\n");
    print("  - Workspaces\n");
    print("  - Window List\n");
    print("  - System Tray\n");
    print("  - Clock\n");
    print("  - Settings\n");

    // Create app launcher
    print("\nInitializing application launcher...\n");
    let mut launcher = AppLauncher::new();
    print("Registered applications:\n");
    for app in launcher.apps.iter() {
        print("  - ");
        print(app.name.as_str());
        print("\n");
    }

    // Create window manager
    print("\nInitializing window manager...\n");
    let mut wm = WindowManager::new();
    print("Window manager ready\n");

    // Main event loop
    print("\nCOSMIC Desktop running...\n");
    let mut iteration = 0;
    while iteration < 100 {
        // In a real implementation:
        // 1. Process Wayland events
        // 2. Handle user input
        // 3. Update window states
        // 4. Render panel and windows
        // 5. Dispatch to applications

        // Simulate work
        if iteration % 20 == 0 {
            print("Desktop iteration ");
            print(if iteration < 10 { "0" } else { "" });
            // Can't easily print numbers in no_std, so just indicate progress
            print("...\n");
        }

        // Render panel
        panel.render();

        iteration += 1;
    }

    print("\nShutting down COSMIC Desktop...\n");
    
    // Cleanup
    let _ = client.disconnect();
    
    print("COSMIC Desktop terminated\n");
    exit(0);
}
