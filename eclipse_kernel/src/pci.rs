//! PCI (Peripheral Component Interconnect) Bus Support
//!
//! Implements PCI device enumeration and configuration space access
//! for discovering and configuring VirtIO and other PCI devices.
//!
//! ## Current Features
//! - Multi-bus enumeration (scans all 256 possible buses)
//! - PCI-to-PCI bridge detection and secondary bus scanning
//! - Multi-function device support
//! - BAR (Base Address Register) access
//! - Device enabling (I/O, Memory, Bus Master)
//!
//! ## Limitations
//! - No MSI/MSI-X interrupt configuration
//! - No PCI Express (PCIe) advanced features
//! - No hot-plug support
//! - No power management
//!
//! ## Future Enhancements
//! - MSI/MSI-X interrupt support
//! - PCIe capability parsing
//! - Device hot-plug detection
//! - Power management (D0-D3 states)

use core::ptr::{read_volatile, write_volatile};
use spin::Mutex;
use alloc::vec::Vec;

/// PCI Configuration Space I/O Ports
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// PCI Vendor IDs
const PCI_VENDOR_INVALID: u16 = 0xFFFF;
const PCI_VENDOR_QEMU: u16 = 0x1AF4; // Red Hat (QEMU VirtIO)
const PCI_VENDOR_NVIDIA: u16 = 0x10DE; // NVIDIA Corporation
const PCI_VENDOR_INTEL: u16 = 0x8086; // Intel Corporation
const PCI_VENDOR_AMD: u16 = 0x1022; // AMD

/// PCI Device Classes
const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_CLASS_NETWORK: u8 = 0x02;
const PCI_CLASS_DISPLAY: u8 = 0x03;
const PCI_CLASS_MULTIMEDIA: u8 = 0x04;  // Audio/Video devices
const PCI_CLASS_BRIDGE: u8 = 0x06;  // Bridge devices
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;  // Serial bus controllers (USB, etc.)

/// PCI Multimedia Subclasses
const PCI_SUBCLASS_AUDIO_AC97: u8 = 0x01;  // AC97 Audio Controller
const PCI_SUBCLASS_AUDIO_HDA: u8 = 0x03;   // Intel High Definition Audio

/// PCI Bridge Subclasses
const PCI_SUBCLASS_BRIDGE_HOST: u8 = 0x00;
const PCI_SUBCLASS_BRIDGE_ISA: u8 = 0x01;
const PCI_SUBCLASS_BRIDGE_PCI: u8 = 0x04;  // PCI-to-PCI bridge

/// PCI Serial Bus Subclasses
const PCI_SUBCLASS_USB_UHCI: u8 = 0x00;  // USB UHCI controller
const PCI_SUBCLASS_USB_OHCI: u8 = 0x10;  // USB OHCI controller
const PCI_SUBCLASS_USB_EHCI: u8 = 0x20;  // USB EHCI controller (USB 2.0)
const PCI_SUBCLASS_USB_XHCI: u8 = 0x30;  // USB XHCI controller (USB 3.0+)

/// PCI Configuration Space Registers
const PCI_REG_VENDOR_ID: u8 = 0x00;
const PCI_REG_DEVICE_ID: u8 = 0x02;
const PCI_REG_COMMAND: u8 = 0x04;
const PCI_REG_STATUS: u8 = 0x06;
const PCI_REG_CLASS_CODE: u8 = 0x08;
const PCI_REG_HEADER_TYPE: u8 = 0x0E;
const PCI_REG_BAR0: u8 = 0x10;
const PCI_REG_PRIMARY_BUS: u8 = 0x18;      // For PCI-to-PCI bridges
const PCI_REG_SECONDARY_BUS: u8 = 0x19;    // For PCI-to-PCI bridges
const PCI_REG_SUBORDINATE_BUS: u8 = 0x1A;  // For PCI-to-PCI bridges
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

    /// Check if this is an NVIDIA GPU
    pub fn is_nvidia_gpu(&self) -> bool {
        self.vendor_id == PCI_VENDOR_NVIDIA && self.class_code == PCI_CLASS_DISPLAY
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
            (0x04, 0x01) => "AC97 Audio Controller",
            (0x04, 0x03) => "Intel HDA Audio Controller",
            (0x06, 0x00) => "Host Bridge",
            (0x06, 0x01) => "ISA Bridge",
            (0x06, 0x04) => "PCI-to-PCI Bridge",
            (0x0C, 0x00) => "USB UHCI Controller",
            (0x0C, 0x10) => "USB OHCI Controller",
            (0x0C, 0x20) => "USB EHCI Controller",
            (0x0C, 0x30) => "USB XHCI Controller",
            _ => "Unknown Device",
        }
    }
    
    /// Check if this is a PCI-to-PCI bridge
    pub fn is_pci_bridge(&self) -> bool {
        self.class_code == PCI_CLASS_BRIDGE && self.subclass == PCI_SUBCLASS_BRIDGE_PCI
    }

    /// Check if this is a USB controller
    pub fn is_usb_controller(&self) -> bool {
        self.class_code == PCI_CLASS_SERIAL_BUS && 
        (self.subclass == PCI_SUBCLASS_USB_UHCI || 
         self.subclass == PCI_SUBCLASS_USB_OHCI ||
         self.subclass == PCI_SUBCLASS_USB_EHCI ||
         self.subclass == PCI_SUBCLASS_USB_XHCI)
    }

    /// Get USB controller type
    pub fn usb_controller_type(&self) -> Option<&'static str> {
        if !self.is_usb_controller() {
            return None;
        }
        match self.subclass {
            PCI_SUBCLASS_USB_UHCI => Some("UHCI (USB 1.1)"),
            PCI_SUBCLASS_USB_OHCI => Some("OHCI (USB 1.1)"),
            PCI_SUBCLASS_USB_EHCI => Some("EHCI (USB 2.0)"),
            PCI_SUBCLASS_USB_XHCI => Some("XHCI (USB 3.0+)"),
            _ => None,
        }
    }

    /// Check if this is an audio device
    pub fn is_audio_device(&self) -> bool {
        self.class_code == PCI_CLASS_MULTIMEDIA && 
        (self.subclass == PCI_SUBCLASS_AUDIO_AC97 || 
         self.subclass == PCI_SUBCLASS_AUDIO_HDA)
    }

    /// Check if this is an Intel HDA audio controller
    pub fn is_intel_hda(&self) -> bool {
        self.class_code == PCI_CLASS_MULTIMEDIA && 
        self.subclass == PCI_SUBCLASS_AUDIO_HDA
    }

    /// Check if this is an AC97 audio controller
    pub fn is_ac97(&self) -> bool {
        self.class_code == PCI_CLASS_MULTIMEDIA && 
        self.subclass == PCI_SUBCLASS_AUDIO_AC97
    }

    /// Get audio device type string
    pub fn audio_device_type(&self) -> Option<&'static str> {
        if !self.is_audio_device() {
            return None;
        }
        match self.subclass {
            PCI_SUBCLASS_AUDIO_AC97 => Some("AC97 Audio"),
            PCI_SUBCLASS_AUDIO_HDA => Some("Intel HDA"),
            _ => Some("Unknown Audio"),
        }
    }
}

/// Global list of discovered PCI devices
static PCI_DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

/// Read from PCI configuration space (8-bit)
pub unsafe fn pci_config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 3) * 8)) & 0xFF) as u8
}

/// Read from PCI configuration space (16-bit)
pub unsafe fn pci_config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

/// Read from PCI configuration space (32-bit)
pub unsafe fn pci_config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    inl(PCI_CONFIG_DATA)
}

/// Write to PCI configuration space (16-bit)
pub unsafe fn pci_config_write_u16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
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
/// Also scans secondary buses if the device is a PCI-to-PCI bridge
unsafe fn scan_device(bus: u8, device: u8) {
    // Check function 0 first
    if let Some(pci_dev) = scan_function(bus, device, 0) {
        PCI_DEVICES.lock().push(pci_dev);
        
        // If this is a PCI-to-PCI bridge, scan the secondary bus
        if pci_dev.is_pci_bridge() {
            let secondary_bus = pci_config_read_u8(bus, device, 0, PCI_REG_SECONDARY_BUS);
            if secondary_bus != 0 {
                scan_bus(secondary_bus);
            }
        }
        
        // If multi-function device, scan other functions
        if (pci_dev.header_type & 0x80) != 0 {
            for function in 1..8 {
                if let Some(func_dev) = scan_function(bus, device, function) {
                    PCI_DEVICES.lock().push(func_dev);
                    
                    // Check if any other function is also a bridge
                    if func_dev.is_pci_bridge() {
                        let secondary_bus = pci_config_read_u8(bus, device, function, PCI_REG_SECONDARY_BUS);
                        if secondary_bus != 0 {
                            scan_bus(secondary_bus);
                        }
                    }
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
    serial::serial_print("[PCI] Scanning all PCI buses (with bridge detection)...\n");
    
    unsafe {
        // Scan bus 0 (main bus), which will recursively scan bridges
        scan_bus(0);
        
        let devices = PCI_DEVICES.lock();
        serial::serial_print("[PCI] Found ");
        serial::serial_print_dec(devices.len() as u64);
        serial::serial_print(" PCI device(s) across all buses\n");
        
        // Count bridges
        let bridge_count = devices.iter().filter(|d| d.is_pci_bridge()).count();
        if bridge_count > 0 {
            serial::serial_print("[PCI]   Detected ");
            serial::serial_print_dec(bridge_count as u64);
            serial::serial_print(" PCI-to-PCI bridge(s)\n");
        }
        
        // Count NVIDIA GPUs
        let nvidia_count = devices.iter().filter(|d| d.is_nvidia_gpu()).count();
        if nvidia_count > 0 {
            serial::serial_print("[PCI]   Detected ");
            serial::serial_print_dec(nvidia_count as u64);
            serial::serial_print(" NVIDIA GPU(s)\n");
        }
        
        for dev in devices.iter() {
            serial::serial_print("[PCI]   Bus ");
            serial::serial_print_dec(dev.bus as u64);
            serial::serial_print(" Device ");
            serial::serial_print_dec(dev.device as u64);
            serial::serial_print(" Func ");
            serial::serial_print_dec(dev.function as u64);
            serial::serial_print(": Vendor=");
            serial::serial_print_hex(dev.vendor_id as u64);
            serial::serial_print(" Device=");
            serial::serial_print_hex(dev.device_id as u64);
            serial::serial_print(" Class=");
            serial::serial_print_hex(dev.class_code as u64);
            serial::serial_print(" Type=");
            serial::serial_print(dev.device_type());
            if dev.is_nvidia_gpu() {
                serial::serial_print(" [NVIDIA GPU]");
            } else if dev.is_virtio() {
                serial::serial_print(" [VirtIO");
                // Identify specific VirtIO device types
                if dev.device_id == 0x1001 || dev.device_id == 0x1042 {
                    serial::serial_print(" Block]");
                } else {
                    serial::serial_print("]");
                }
            }
            serial::serial_print("\n");
        }
    }
}

/// Find first VirtIO block device
pub fn find_virtio_block_device() -> Option<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter().find(|dev| {
        // VirtIO block device IDs:
        // 0x1001 = Legacy VirtIO block device
        // 0x1042 = Modern/transitional VirtIO block device  
        dev.is_virtio() && (dev.device_id == 0x1001 || dev.device_id == 0x1042)
    }).copied()
}

/// Find all NVIDIA GPUs
pub fn find_nvidia_gpus() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.is_nvidia_gpu())
        .copied()
        .collect()
}

/// Find first NVIDIA GPU
pub fn find_nvidia_gpu() -> Option<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .find(|dev| dev.is_nvidia_gpu())
        .copied()
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

/// Find all audio devices (Intel HDA, AC97, etc.)
pub fn find_audio_devices() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.is_audio_device())
        .copied()
        .collect()
}

/// Find first Intel HDA audio device
pub fn find_intel_hda() -> Option<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .find(|dev| dev.is_intel_hda())
        .copied()
}

/// Find first AC97 audio device
pub fn find_ac97() -> Option<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .find(|dev| dev.is_ac97())
        .copied()
}
