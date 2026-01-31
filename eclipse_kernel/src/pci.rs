//! PCI (Peripheral Component Interconnect) Bus Support
//!
//! Implements PCI device enumeration and configuration space access
//! for discovering and configuring VirtIO and other PCI devices.

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;
use alloc::vec::Vec;

/// PCI Configuration Space I/O Ports
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// PCI Vendor IDs
const PCI_VENDOR_INVALID: u16 = 0xFFFF;
const PCI_VENDOR_QEMU: u16 = 0x1AF4; // Red Hat (QEMU VirtIO)

/// PCI Device Classes
const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_CLASS_NETWORK: u8 = 0x02;
const PCI_CLASS_DISPLAY: u8 = 0x03;

/// PCI Configuration Space Registers
const PCI_REG_VENDOR_ID: u8 = 0x00;
const PCI_REG_DEVICE_ID: u8 = 0x02;
const PCI_REG_COMMAND: u8 = 0x04;
const PCI_REG_STATUS: u8 = 0x06;
const PCI_REG_CLASS_CODE: u8 = 0x08;
const PCI_REG_HEADER_TYPE: u8 = 0x0E;
const PCI_REG_BAR0: u8 = 0x10;
const PCI_REG_INTERRUPT_LINE: u8 = 0x3C;

/// PCI Command Register Bits
const PCI_COMMAND_IO: u16 = 0x01;
const PCI_COMMAND_MEMORY: u16 = 0x02;
const PCI_COMMAND_BUS_MASTER: u16 = 0x04;
const PCI_COMMAND_INTERRUPT_DISABLE: u16 = 0x0400;

/// Represents a PCI device
#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub header_type: u8,
    pub bar0: u32,
    pub interrupt_line: u8,
}

impl PciDevice {
    /// Check if this is a VirtIO device
    pub fn is_virtio(&self) -> bool {
        // VirtIO devices have vendor ID 0x1AF4 and device IDs in specific ranges
        self.vendor_id == PCI_VENDOR_QEMU && 
        (self.device_id >= 0x1000 && self.device_id <= 0x107F)
    }

    /// Get the device type string
    pub fn device_type(&self) -> &'static str {
        match (self.class_code, self.subclass) {
            (0x01, 0x00) => "SCSI Controller",
            (0x01, 0x01) => "IDE Controller",
            (0x01, 0x06) => "SATA Controller",
            (0x01, 0x08) => "Storage Controller",
            (0x02, 0x00) => "Ethernet Controller",
            (0x03, 0x00) => "VGA Controller",
            _ => "Unknown Device",
        }
    }
}

/// Global list of discovered PCI devices
static PCI_DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

/// Read from PCI configuration space (8-bit)
unsafe fn pci_config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 3) * 8)) & 0xFF) as u8
}

/// Read from PCI configuration space (16-bit)
unsafe fn pci_config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

/// Read from PCI configuration space (32-bit)
unsafe fn pci_config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    inl(PCI_CONFIG_DATA)
}

/// Write to PCI configuration space (16-bit)
unsafe fn pci_config_write_u16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let shift = (offset & 2) * 8;
    let data = inl(PCI_CONFIG_DATA);
    let new_data = (data & !(0xFFFF << shift)) | ((value as u32) << shift);
    outl(PCI_CONFIG_DATA, new_data);
}

/// Write to PCI configuration space (32-bit)
unsafe fn pci_config_write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    outl(PCI_CONFIG_DATA, value);
}

/// Calculate PCI configuration address
fn pci_config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let bus = bus as u32;
    let device = (device & 0x1F) as u32;
    let function = (function & 0x07) as u32;
    let offset = (offset & 0xFC) as u32;
    
    0x80000000 | (bus << 16) | (device << 11) | (function << 8) | offset
}

/// I/O port output (32-bit)
unsafe fn outl(port: u16, value: u32) {
    core::arch::asm!(
        "out dx, eax",
        in("dx") port,
        in("eax") value,
        options(nostack, preserves_flags)
    );
}

/// I/O port input (32-bit)
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    core::arch::asm!(
        "in eax, dx",
        out("eax") value,
        in("dx") port,
        options(nostack, preserves_flags)
    );
    value
}

/// Scan a single PCI function
unsafe fn scan_function(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    let vendor_id = pci_config_read_u16(bus, device, function, PCI_REG_VENDOR_ID);
    
    if vendor_id == PCI_VENDOR_INVALID {
        return None;
    }
    
    let device_id = pci_config_read_u16(bus, device, function, PCI_REG_DEVICE_ID);
    let class_info = pci_config_read_u32(bus, device, function, PCI_REG_CLASS_CODE);
    let class_code = ((class_info >> 24) & 0xFF) as u8;
    let subclass = ((class_info >> 16) & 0xFF) as u8;
    let prog_if = ((class_info >> 8) & 0xFF) as u8;
    let header_type = pci_config_read_u8(bus, device, function, PCI_REG_HEADER_TYPE);
    let bar0 = pci_config_read_u32(bus, device, function, PCI_REG_BAR0);
    let interrupt_line = pci_config_read_u8(bus, device, function, PCI_REG_INTERRUPT_LINE);
    
    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        class_code,
        subclass,
        prog_if,
        header_type,
        bar0,
        interrupt_line,
    })
}

/// Scan a single PCI device (all functions)
unsafe fn scan_device(bus: u8, device: u8) {
    // Check function 0 first
    if let Some(pci_dev) = scan_function(bus, device, 0) {
        PCI_DEVICES.lock().push(pci_dev);
        
        // If multi-function device, scan other functions
        if (pci_dev.header_type & 0x80) != 0 {
            for function in 1..8 {
                if let Some(func_dev) = scan_function(bus, device, function) {
                    PCI_DEVICES.lock().push(func_dev);
                }
            }
        }
    }
}

/// Scan a single PCI bus
unsafe fn scan_bus(bus: u8) {
    for device in 0..32 {
        scan_device(bus, device);
    }
}

/// Initialize PCI subsystem and scan all devices
pub fn init() {
    use crate::serial;
    
    serial::serial_print("[PCI] Initializing PCI subsystem...\n");
    
    unsafe {
        // Scan bus 0 (main bus)
        scan_bus(0);
        
        let devices = PCI_DEVICES.lock();
        serial::serial_print("[PCI] Found ");
        serial::serial_print_dec(devices.len() as u64);
        serial::serial_print(" PCI device(s)\n");
        
        for dev in devices.iter() {
            serial::serial_print("[PCI]   Bus ");
            serial::serial_print_dec(dev.bus as u64);
            serial::serial_print(" Device ");
            serial::serial_print_dec(dev.device as u64);
            serial::serial_print(" Func ");
            serial::serial_print_dec(dev.function as u64);
            serial::serial_print(": Vendor=0x");
            serial::serial_print_hex(dev.vendor_id as u64);
            serial::serial_print(" Device=0x");
            serial::serial_print_hex(dev.device_id as u64);
            serial::serial_print(" Class=0x");
            serial::serial_print_hex(dev.class_code as u64);
            serial::serial_print(" Type=");
            serial::serial_print(dev.device_type());
            if dev.is_virtio() {
                serial::serial_print(" [VirtIO]");
            }
            serial::serial_print("\n");
        }
    }
}

/// Find first VirtIO block device
pub fn find_virtio_block_device() -> Option<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter().find(|dev| {
        dev.is_virtio() && dev.device_id == 0x1001 // VirtIO block device
    }).copied()
}

/// Enable PCI device (set command register)
pub unsafe fn enable_device(dev: &PciDevice, enable_bus_master: bool) {
    let mut command = pci_config_read_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND);
    command |= PCI_COMMAND_IO | PCI_COMMAND_MEMORY;
    if enable_bus_master {
        command |= PCI_COMMAND_BUS_MASTER;
    }
    pci_config_write_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND, command);
}

/// Get BAR (Base Address Register) value
pub unsafe fn get_bar(dev: &PciDevice, bar_index: u8) -> u32 {
    pci_config_read_u32(dev.bus, dev.device, dev.function, PCI_REG_BAR0 + (bar_index * 4))
}
