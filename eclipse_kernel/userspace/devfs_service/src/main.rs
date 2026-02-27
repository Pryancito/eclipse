// devfs_service main.rs - Device Manager

#![no_std]
#![no_main]

use eclipse_libc::{
    println, syscall3, yield_cpu, getppid, send
};

const SYS_REGISTER_DEVICE: u64 = 27;

#[allow(dead_code)]
#[repr(u64)]
enum DeviceType {
    Block = 0,
    Char = 1,
    Network = 2,
    Input = 3,
    Audio = 4,
    Display = 5,
    USB = 6,
    Unknown = 7,
}

fn register_device(name: &str, type_id: DeviceType) -> bool {
    unsafe {
        syscall3(
            SYS_REGISTER_DEVICE,
            name.as_ptr() as u64,
            name.len() as u64,
            type_id as u64,
        ) == 0
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[DevFS] Starting Device Manager Service...");

    // Register standard devices
    if register_device("null", DeviceType::Char) {
        println!("[DevFS] Registered /dev/null");
    }
    
    if register_device("zero", DeviceType::Char) {
        println!("[DevFS] Registered /dev/zero");
    }

    if register_device("console", DeviceType::Char) {
        println!("[DevFS] Registered /dev/console");
    }
    
    // Register Block Devices (e.g. vda)
    // In a real system, we'd scan PCI/VirtIO, but for now we hardcode vda
    // since the kernel knows how to handle it via read_device(315)
    if register_device("vda", DeviceType::Block) {
        println!("[DevFS] Registered /dev/vda");
    }

    println!("[DevFS] Initialization complete. Entering main loop.");
    let ppid = getppid();
    if ppid > 0 {
        let _ = send(ppid, 255, b"READY");
    }

    loop {
        // In the future: listen for udev events or scan bus
        yield_cpu();
    }
}
