// devfs_service - Device Manager

#![no_main]
extern crate std;
extern crate eclipse_syscall;

use std::prelude::*;

const SYS_REGISTER_DEVICE: usize = 27;

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
        eclipse_syscall::syscall3(
            SYS_REGISTER_DEVICE,
            name.as_ptr() as usize,
            name.len(),
            type_id as usize,
        ) == 0
    }
}

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    println!("[DevFS] Starting Device Manager Service...");

    if register_device("null", DeviceType::Char) {
        println!("[DevFS] Registered /dev/null");
    }
    if register_device("zero", DeviceType::Char) {
        println!("[DevFS] Registered /dev/zero");
    }
    if register_device("console", DeviceType::Char) {
        println!("[DevFS] Registered /dev/console");
    }
    if register_device("vda", DeviceType::Block) {
        println!("[DevFS] Registered /dev/vda");
    }

    println!("[DevFS] Initialization complete. Entering main loop.");
    let ppid = unsafe { std::libc::getppid() };
    if ppid > 0 {
        let _ = std::libc::send_ipc(ppid as u32, 255, b"READY");
    }

    let mut heartbeat_counter = 0u64;
    loop {
        heartbeat_counter += 1;
        if heartbeat_counter % 300 == 0 {
            println!("[DevFS] Operational - Heartbeat #{}", heartbeat_counter / 300);
        }
        unsafe { std::libc::sleep_ms(100); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_type_display_enum() {
        assert_eq!(DeviceType::Block as u64, 0);
        assert_eq!(DeviceType::Char as u64, 1);
        assert_eq!(DeviceType::Display as u64, 5);
    }
}
