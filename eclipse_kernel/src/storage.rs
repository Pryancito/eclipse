use alloc::vec::Vec;
use alloc::sync::Arc;
use spin::Mutex;

/// Trait representing a block device with 4096-byte blocks.
pub trait BlockDevice: Send + Sync {
    fn read(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str>;
    fn capacity(&self) -> u64; // Capacity in 4096-byte blocks
    fn name(&self) -> &'static str; // Driver name (e.g., "VirtIO", "AHCI", "NVMe")
}

static DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

/// Register a new block device in the global registry.
pub fn register_device(device: Arc<dyn BlockDevice>) {
    let mut devices = DEVICES.lock();
    crate::serial::serial_print("[STORAGE] Registered ");
    crate::serial::serial_print(device.name());
    crate::serial::serial_print(" device as disk:");
    crate::serial::serial_print_dec(devices.len() as u64);
    crate::serial::serial_print("\n");
    devices.push(device);
}

/// Get a device by index.
pub fn get_device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    DEVICES.lock().get(index).cloned()
}

/// Get the number of registered block devices.
pub fn device_count() -> usize {
    DEVICES.lock().len()
}
