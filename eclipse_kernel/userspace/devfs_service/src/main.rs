// devfs_service - Device Manager

use std::prelude::v1::*;
use eclipse_relibc as libc;

use eclipse_syscall::number::SYS_REGISTER_DEVICE;

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

fn main() {
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
    let ppid = unsafe { libc::getppid() };
    if ppid > 0 {
        let _ = libc::send_ipc(ppid as u32, 255, b"READY");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_millis(5));
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
