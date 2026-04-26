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
const PCI_VENDOR_VBOX: u16 = 0x80EE; // VirtualBox

/// PCI Device Classes
const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_CLASS_NETWORK: u8 = 0x02;
const PCI_CLASS_DISPLAY: u8 = 0x03;
const PCI_CLASS_MULTIMEDIA: u8 = 0x04;  // Audio/Video devices
const PCI_CLASS_BRIDGE: u8 = 0x06;  // Bridge devices
const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;  // Serial bus controllers (USB, etc.)

/// VirtIO PCI Device IDs (Modern/Non-Transitional)
pub const VIRTIO_DEV_NET: u16 = 0x1041;
pub const VIRTIO_DEV_BLOCK: u16 = 0x1042;
pub const VIRTIO_DEV_GPU: u16 = 0x1050;

/// VirtIO PCI Device IDs (Legacy/Transitional)
pub const VIRTIO_DEV_NET_LEGACY: u16 = 0x1000;
pub const VIRTIO_DEV_BLOCK_LEGACY: u16 = 0x1001;
pub const VIRTIO_DEV_GPU_LEGACY: u16 = 0x1010;
pub const VIRTIO_DEV_VGA_MODERN: u16 = 0x1050; // Often shared with GPU

/// PCI Multimedia Subclasses
const PCI_SUBCLASS_AUDIO_AC97: u8 = 0x01;  // AC97 Audio Controller
const PCI_SUBCLASS_AUDIO_HDA: u8 = 0x03;   // Intel High Definition Audio

/// PCI Bridge Subclasses
const PCI_SUBCLASS_BRIDGE_HOST: u8 = 0x00;
const PCI_SUBCLASS_BRIDGE_ISA: u8 = 0x01;
const PCI_SUBCLASS_BRIDGE_PCI: u8 = 0x04;  // PCI-to-PCI bridge

/// PCI Serial Bus Subclass for USB
/// Según la especificación PCI:
///   - class_code = 0x0C (Serial Bus Controller)
///   - subclass   = 0x03 (USB)
///   - prog_if    selecciona UHCI/OHCI/EHCI/XHCI
const PCI_SUBCLASS_USB: u8 = 0x03;

/// PCI Serial Bus Programming Interface (prog_if) values for USB
const PCI_PROGIF_USB_UHCI: u8 = 0x00;  // USB UHCI controller
const PCI_PROGIF_USB_OHCI: u8 = 0x10;  // USB OHCI controller
const PCI_PROGIF_USB_EHCI: u8 = 0x20;  // USB EHCI controller (USB 2.0)
const PCI_PROGIF_USB_XHCI: u8 = 0x30;  // USB XHCI controller (USB 3.0+)

/// PCI Configuration Space Registers
const PCI_REG_VENDOR_ID: u8 = 0x00;
const PCI_REG_DEVICE_ID: u8 = 0x02;
const PCI_REG_COMMAND: u8 = 0x04;
const PCI_REG_STATUS: u8 = 0x06;
const PCI_REG_CLASS_CODE: u8 = 0x08;
const PCI_REG_HEADER_TYPE: u8 = 0x0E;
const PCI_REG_BAR0: u8 = 0x10;
const PCI_REG_CAP_PTR: u8 = 0x34;
const PCI_REG_PRIMARY_BUS: u8 = 0x18;      // For PCI-to-PCI bridges
const PCI_REG_SECONDARY_BUS: u8 = 0x19;    // For PCI-to-PCI bridges
const PCI_REG_SUBORDINATE_BUS: u8 = 0x1A;  // For PCI-to-PCI bridges
const PCI_REG_INTERRUPT_LINE: u8 = 0x3C;

/// PCI Capability IDs
const PCI_CAP_ID_VNDR: u8 = 0x09;
const PCI_CAP_ID_MSI: u8 = 0x05;
const PCI_CAP_ID_MSIX: u8 = 0x11;

/// VirtIO PCI capability types (VIRTIO_PCI_CAP_*)
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// virtio_pci_cap layout (read from PCI config)
/// cap_vndr=0, cap_next=1, cap_len=2, cfg_type=3, bar=4, id=5, offset=8, length=12
/// virtio_pci_notify_cap extends with notify_off_multiplier at offset 16

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
    pub bar0: u64,
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

    /// Check if this is a VirtIO GPU
    pub fn is_virtio_gpu(&self) -> bool {
        self.is_virtio() && (self.device_id == VIRTIO_DEV_GPU || self.device_id == VIRTIO_DEV_GPU_LEGACY)
    }

    /// Check if this is a VirtualBox VGA device
    pub fn is_vbox_gpu(&self) -> bool {
        self.vendor_id == PCI_VENDOR_VBOX && self.device_id == 0xBEEF
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
            // USB controllers: class 0x0C, subclass 0x03, prog_if distingue tipo
            (0x0C, PCI_SUBCLASS_USB) => match self.prog_if {
                PCI_PROGIF_USB_UHCI => "USB UHCI Controller",
                PCI_PROGIF_USB_OHCI => "USB OHCI Controller",
                PCI_PROGIF_USB_EHCI => "USB EHCI Controller",
                PCI_PROGIF_USB_XHCI => "USB XHCI Controller",
                _ => "USB Controller",
            },
            _ => {
                if self.is_virtio() {
                    match self.device_id {
                        VIRTIO_DEV_NET | VIRTIO_DEV_NET_LEGACY => "VirtIO Network Card",
                        VIRTIO_DEV_BLOCK | VIRTIO_DEV_BLOCK_LEGACY => "VirtIO Block Device",
                        VIRTIO_DEV_GPU => "VirtIO GPU",
                        _ => "VirtIO Device",
                    }
                } else {
                    "Unknown Device"
                }
            },
        }
    }
    
    /// Check if this is a PCI-to-PCI bridge.
    /// Uses header type as the authoritative indicator (Type 1 = bridge per PCI spec),
    /// in addition to the class/subclass check, to handle non-standard bridge
    /// subclasses found on some real hardware (e.g. semi-transparent bridges).
    pub fn is_pci_bridge(&self) -> bool {
        (self.class_code == PCI_CLASS_BRIDGE && self.subclass == PCI_SUBCLASS_BRIDGE_PCI)
            || (self.header_type & 0x7F) == 0x01
    }

    /// Check if this is a USB controller
    pub fn is_usb_controller(&self) -> bool {
        self.class_code == PCI_CLASS_SERIAL_BUS &&
        self.subclass == PCI_SUBCLASS_USB &&
        (self.prog_if == PCI_PROGIF_USB_UHCI ||
         self.prog_if == PCI_PROGIF_USB_OHCI ||
         self.prog_if == PCI_PROGIF_USB_EHCI ||
         self.prog_if == PCI_PROGIF_USB_XHCI)
    }

    /// Get USB controller type
    pub fn usb_controller_type(&self) -> Option<&'static str> {
        if !self.is_usb_controller() {
            return None;
        }
        match self.prog_if {
            PCI_PROGIF_USB_UHCI => Some("UHCI (USB 1.1)"),
            PCI_PROGIF_USB_OHCI => Some("OHCI (USB 1.1)"),
            PCI_PROGIF_USB_EHCI => Some("EHCI (USB 2.0)"),
            PCI_PROGIF_USB_XHCI => Some("XHCI (USB 3.0+)"),
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

/// Global lock for PCI configuration space access (0xCF8/0xCFC)
static PCI_CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Global list of discovered PCI devices
static PCI_DEVICES: Mutex<Vec<PciDevice>> = Mutex::new(Vec::new());

/// Read from PCI configuration space (8-bit)
pub unsafe fn pci_config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let _lock = PCI_CONFIG_LOCK.lock();
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 3) * 8)) & 0xFF) as u8
}

/// Read from PCI configuration space (16-bit)
pub unsafe fn pci_config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let _lock = PCI_CONFIG_LOCK.lock();
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let data = inl(PCI_CONFIG_DATA);
    ((data >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

/// Read from PCI configuration space (32-bit)
pub unsafe fn pci_config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let _lock = PCI_CONFIG_LOCK.lock();
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    inl(PCI_CONFIG_DATA)
}

/// Write to PCI configuration space (8-bit)
pub unsafe fn pci_config_write_u8(bus: u8, device: u8, function: u8, offset: u8, value: u8) {
    let _lock = PCI_CONFIG_LOCK.lock();
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let shift = (offset & 3) * 8;
    let data = inl(PCI_CONFIG_DATA);
    let new_data = (data & !(0xFF << shift)) | ((value as u32) << shift);
    outl(PCI_CONFIG_DATA, new_data);
}

/// Write to PCI configuration space (16-bit)
pub unsafe fn pci_config_write_u16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let _lock = PCI_CONFIG_LOCK.lock();
    let address = pci_config_address(bus, device, function, offset);
    outl(PCI_CONFIG_ADDRESS, address);
    let shift = (offset & 2) * 8;
    let data = inl(PCI_CONFIG_DATA);
    let new_data = (data & !(0xFFFF << shift)) | ((value as u32) << shift);
    outl(PCI_CONFIG_DATA, new_data);
}

/// Write to PCI configuration space (32-bit)
pub unsafe fn pci_config_write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let _lock = PCI_CONFIG_LOCK.lock();
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
    let mut bar0 = pci_config_read_u32(bus, device, function, PCI_REG_BAR0) as u64;
    
    // Check if BAR0 is a 64-bit memory BAR (bits 2:1 = 0b10) and combine with BAR1
    if (bar0 & 0x1) == 0 && (bar0 & 0x6) == 0x4 {
        let bar1 = pci_config_read_u32(bus, device, function, PCI_REG_BAR0 + 4) as u64;
        bar0 |= bar1 << 32;
    }
    // Strip lower flag bits so bar0 holds the actual physical base address
    bar0 = if (bar0 & 0x1) != 0 { bar0 & !0x3 } else { bar0 & !0xF };
    
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
        // Enable device (Bus Master + memory/IO access)
        enable_device(&pci_dev, true);
        
        PCI_DEVICES.lock().push(pci_dev);
        
        // If this is a PCI-to-PCI bridge, scan the secondary bus
        if pci_dev.is_pci_bridge() {
            let secondary_bus = pci_config_read_u8(bus, device, 0, PCI_REG_SECONDARY_BUS);
            if secondary_bus != 0 && secondary_bus > bus {
                scan_bus(secondary_bus);
            }
        }
        
        // If multi-function device, scan other functions
        if (pci_dev.header_type & 0x80) != 0 {
            for function in 1..8 {
                if let Some(func_dev) = scan_function(bus, device, function) {
                    // Enable device
                    enable_device(&func_dev, true);
                    
                    PCI_DEVICES.lock().push(func_dev);
                    
                    // Check if any other function is also a bridge
                    if func_dev.is_pci_bridge() {
                        let secondary_bus = pci_config_read_u8(bus, device, function, PCI_REG_SECONDARY_BUS);
                        if secondary_bus != 0 && secondary_bus > bus {
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

/// Supplementary scan of all PCI buses (0-255) to catch devices missed by
/// bridge traversal on real hardware with unusual PCIe topologies.
/// Only adds devices not already present in PCI_DEVICES.
unsafe fn supplementary_scan() {
    for bus in 0u8..=255 {
        for device in 0..32u8 {
            // Quick check: skip empty slots (no device at function 0)
            let vendor_id = pci_config_read_u16(bus, device, 0, PCI_REG_VENDOR_ID);
            if vendor_id == PCI_VENDOR_INVALID {
                continue;
            }
            // Skip if any function of this device was already found via bridge traversal
            let already_known = {
                let devices = PCI_DEVICES.lock();
                devices.iter().any(|d| d.bus == bus && d.device == device)
            };
            if !already_known {
                scan_device(bus, device);
            }
        }
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

        // Supplementary direct scan of all buses to catch NVIDIA GPUs (or other
        // devices) that may be missed when PCIe bridges have non-standard
        // configurations on real hardware (e.g. secondary_bus not strictly > primary).
        supplementary_scan();
        
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
                match dev.device_id {
                    VIRTIO_DEV_BLOCK | VIRTIO_DEV_BLOCK_LEGACY => serial::serial_print(" Block]"),
                    VIRTIO_DEV_NET | VIRTIO_DEV_NET_LEGACY => serial::serial_print(" Net]"),
                    VIRTIO_DEV_GPU => serial::serial_print(" GPU]"),
                    _ => serial::serial_print("]"),
                }
            }
            serial::serial_print("\n");
        }
    }
}
/// Get a list of all discovered PCI devices
pub fn get_all_devices() -> alloc::vec::Vec<PciDevice> {
    PCI_DEVICES.lock().clone()
}

/// SATA AHCI: class 0x01, subclass 0x06, prog_if 0x01
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_PROGIF_AHCI: u8 = 0x01;

/// NVMe: class 0x01, subclass 0x08
const PCI_SUBCLASS_NVME: u8 = 0x08;

/// Find first NVMe controller
pub fn find_nvme_controller() -> Option<PciDevice> {
    find_all_nvme_controllers().into_iter().next()
}

/// Find all NVMe controllers
pub fn find_all_nvme_controllers() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter().filter(|dev| {
        dev.class_code == PCI_CLASS_STORAGE
            && dev.subclass == PCI_SUBCLASS_NVME
    }).copied().collect()
}

/// Find first SATA AHCI controller
pub fn find_sata_ahci() -> Option<PciDevice> {
    find_all_sata_ahci().first().copied()
}

/// Find all SATA AHCI controllers
/// Matches:
///   - class=0x01, subclass=0x06, prog_if=0x01  (standard AHCI)
///   - class=0x01, subclass=0x01, prog_if=0x01  (some Intel chipsets report IDE-like subclass but AHCI mode)
pub fn find_all_sata_ahci() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter().filter(|dev| {
        dev.class_code == PCI_CLASS_STORAGE
            && dev.prog_if == PCI_PROGIF_AHCI
            && (dev.subclass == PCI_SUBCLASS_SATA || dev.subclass == 0x01)
    }).copied().collect()
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

pub fn get_device_count() -> usize {
    PCI_DEVICES.lock().len()
}

/// Find all display-class devices (GPUs, including VirtIO, Intel, etc.)
pub fn find_all_gpus() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.class_code == PCI_CLASS_DISPLAY)
        .copied()
        .collect()
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

/// Get BAR (Base Address Register) value as 64-bit physical address.
/// The lower bits of a BAR encode flags (memory/IO type, prefetchable) and are
/// stripped so the returned value is the actual physical base address.
pub unsafe fn get_bar(dev: &PciDevice, bar_index: u8) -> u64 {
    let mut bar = pci_config_read_u32(dev.bus, dev.device, dev.function, PCI_REG_BAR0 + (bar_index * 4)) as u64;
    
    if (bar & 0x1) != 0 {
        // I/O space BAR: bits 31:2 = address, bits 1:0 = flags
        bar & !0x3
    } else {
        // Memory space BAR: check if 64-bit (bits 2:1 = 0b10)
        if (bar & 0x6) == 0x4 {
            let next_bar = pci_config_read_u32(dev.bus, dev.device, dev.function, PCI_REG_BAR0 + (bar_index * 4) + 4) as u64;
            bar |= next_bar << 32;
        }
        // Strip the lower 4 flag bits (memory space indicator, type, prefetchable)
        bar & !0xF
    }
}

/// Get the size of a BAR (Base Address Register) by writing all 1s and reading back
/// how many bits the device actually implements.
pub unsafe fn get_bar_size(dev: &PciDevice, bar_index: u8) -> u64 {
    let offset = PCI_REG_BAR0 + (bar_index * 4);
    
    // Safety for real hardware: disable Memory and IO decoding before probing BAR size.
    // Probing involves writing 0xFFFFFFFF, which causes the device to temporarily 
    // respond to a massive address range, potentially conflicting with others.
    let original_cmd = pci_config_read_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND);
    pci_config_write_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND, 
                        original_cmd & !(PCI_COMMAND_IO | PCI_COMMAND_MEMORY));

    // Save current BAR value
    let original = pci_config_read_u32(dev.bus, dev.device, dev.function, offset);
    
    // Write all 1s to the BAR
    pci_config_write_u32(dev.bus, dev.device, dev.function, offset, 0xFFFF_FFFF);
    
    // Read back bits implemented by the device
    let mask = pci_config_read_u32(dev.bus, dev.device, dev.function, offset);
    
    // Restore original value
    pci_config_write_u32(dev.bus, dev.device, dev.function, offset, original);
    
    let size = if (original & 0x1) != 0 {
        // I/O space BAR: size is ~mask + 1
        let size_mask = mask & !0x3;
        if size_mask == 0 { 0 }
        else { (!(size_mask) as u64).wrapping_add(1) & 0xFFFF_FFFF }
    } else {
        // Memory space BAR: check for 64-bit bits 2:1 = 0b10
        if (original & 0x6) == 0x4 {
            let original_hi = pci_config_read_u32(dev.bus, dev.device, dev.function, offset + 4);
            pci_config_write_u32(dev.bus, dev.device, dev.function, offset + 4, 0xFFFF_FFFF);
            let mask_hi = pci_config_read_u32(dev.bus, dev.device, dev.function, offset + 4);
            pci_config_write_u32(dev.bus, dev.device, dev.function, offset + 4, original_hi);
            
            let full_mask = ((mask_hi as u64) << 32) | (mask as u64);
            let size_mask = full_mask & !0xF;
            if size_mask == 0 { 0 }
            else { (!size_mask).wrapping_add(1) }
        } else {
            let size_mask = mask & !0xF;
            if size_mask == 0 { 0 }
            else { ((!size_mask) as u64).wrapping_add(1) & 0xFFFF_FFFF }
        }
    };

    pci_config_write_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND, original_cmd);
    size
}

/// Find first capability with given ID. Returns offset in config space (0 = not found).
pub unsafe fn pci_find_capability(dev: &PciDevice, cap_id: u8) -> u8 {
    let mut pos = pci_config_read_u8(dev.bus, dev.device, dev.function, PCI_REG_CAP_PTR);
    if pos == 0 {
        return 0;
    }
    while pos != 0 {
        let id = pci_config_read_u8(dev.bus, dev.device, dev.function, pos);
        if id == cap_id {
            return pos;
        }
        let next = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 1);
        if next == pos { break; } // Prevent infinite loops
        pos = next;
        if pos < 0x40 && pos != 0 { break; } // Sanity check for malformed lists
    }
    0
}

/// Enable MSI (Message Signaled Interrupts) for a device.
/// On x86_64, this writes the vector to the MSI data register and the
/// target CPU to the MSI address register.
pub unsafe fn pci_enable_msi(dev: &PciDevice, vector: u8, cpu_id: u32) -> bool {
    let pos = pci_find_capability(dev, PCI_CAP_ID_MSI);
    if pos == 0 {
        return false;
    }

    let msg_ctrl = pci_config_read_u16(dev.bus, dev.device, dev.function, pos + 2);
    let is_64bit = (msg_ctrl & 0x80) != 0;

    // x86 MSI Address: 0xFEE00000 | (dest_id << 12)
    let addr: u32 = 0xFEE00000 | (cpu_id << 12);
    let data: u32 = vector as u32;

    pci_config_write_u32(dev.bus, dev.device, dev.function, pos + 4, addr);
    if is_64bit {
        pci_config_write_u32(dev.bus, dev.device, dev.function, pos + 8, 0); // Addr HI
        pci_config_write_u16(dev.bus, dev.device, dev.function, pos + 12, data as u16);
    } else {
        pci_config_write_u16(dev.bus, dev.device, dev.function, pos + 8, data as u16);
    }

    // Enable MSI in Message Control register (bit 0)
    pci_config_write_u16(dev.bus, dev.device, dev.function, pos + 2, msg_ctrl | 0x01);
    
    // Also ensure Bus Master is enabled in Command register
    let cmd = pci_config_read_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND);
    pci_config_write_u16(dev.bus, dev.device, dev.function, PCI_REG_COMMAND, cmd | PCI_COMMAND_BUS_MASTER);

    true
}

/// Find VirtIO capability by cfg_type. Returns (bar, offset, length).
/// For NOTIFY_CFG, use pci_find_virtio_notify_cap for notify_off_multiplier.
pub unsafe fn pci_find_virtio_cap(dev: &PciDevice, cfg_type: u8) -> Option<(u8, u32, u32)> {
    let mut pos = pci_config_read_u8(dev.bus, dev.device, dev.function, PCI_REG_CAP_PTR);
    while pos != 0 {
        let cap_id = pci_config_read_u8(dev.bus, dev.device, dev.function, pos);
        let next = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 1);
        if cap_id == PCI_CAP_ID_VNDR {
            let cap_cfg_type = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 3);
            let bar = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 4);
            if bar < 6 && cap_cfg_type == cfg_type {
                let offset = pci_config_read_u32(dev.bus, dev.device, dev.function, pos + 8);
                let length = pci_config_read_u32(dev.bus, dev.device, dev.function, pos + 12);
                return Some((bar, offset, length));
            }
        }
        pos = next;
    }
    None
}

/// Find VirtIO NOTIFY capability and return (bar, offset, length, notify_off_multiplier).
pub unsafe fn pci_find_virtio_notify_cap(dev: &PciDevice) -> Option<(u8, u32, u32, u32)> {
    let mut pos = pci_config_read_u8(dev.bus, dev.device, dev.function, PCI_REG_CAP_PTR);
    while pos != 0 {
        let cap_id = pci_config_read_u8(dev.bus, dev.device, dev.function, pos);
        let next = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 1);
        if cap_id == PCI_CAP_ID_VNDR {
            let cap_cfg_type = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 3);
            let bar = pci_config_read_u8(dev.bus, dev.device, dev.function, pos + 4);
            if bar < 6 && cap_cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
                let offset = pci_config_read_u32(dev.bus, dev.device, dev.function, pos + 8);
                let length = pci_config_read_u32(dev.bus, dev.device, dev.function, pos + 12);
                let mult = pci_config_read_u32(dev.bus, dev.device, dev.function, pos + 16);
                return Some((bar, offset, length, mult));
            }
        }
        pos = next;
    }
    None
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

/// Enumerate all discovered PCI devices to a userspace buffer
pub fn enum_devices_to_user(buf_ptr: u64, max_count: usize) -> usize {
    let devices = PCI_DEVICES.lock();
    let count = core::cmp::min(devices.len(), max_count);
    
    for i in 0..count {
        let dev = &devices[i];
        unsafe {
            core::ptr::write_unaligned((buf_ptr as *mut PciDevice).add(i), *dev);
        }
    }
    count
}

/// Find all USB controllers
pub fn find_usb_controllers() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.is_usb_controller())
        .copied()
        .collect()
}

/// Find USB controllers by type
pub fn find_usb_by_type(prog_if: u8) -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.is_usb_controller() && dev.prog_if == prog_if)
        .copied()
        .collect()
}

/// Find XHCI (USB 3.0+) controllers
pub fn find_xhci_controllers() -> alloc::vec::Vec<PciDevice> {
    find_usb_by_type(PCI_PROGIF_USB_XHCI)
}

/// Find EHCI (USB 2.0) controllers
pub fn find_ehci_controllers() -> alloc::vec::Vec<PciDevice> {
    find_usb_by_type(PCI_PROGIF_USB_EHCI)
}

/// Find OHCI (USB 1.1) controllers
pub fn find_ohci_controllers() -> alloc::vec::Vec<PciDevice> {
    find_usb_by_type(PCI_PROGIF_USB_OHCI)
}

/// Find UHCI (USB 1.1) controllers
pub fn find_uhci_controllers() -> alloc::vec::Vec<PciDevice> {
    find_usb_by_type(PCI_PROGIF_USB_UHCI)
}

/// Find all network controllers (Ethernet, WiFi, etc.)
pub fn find_network_controllers() -> alloc::vec::Vec<PciDevice> {
    let devices = PCI_DEVICES.lock();
    devices.iter()
        .filter(|dev| dev.class_code == 0x02)
        .copied()
        .collect()
}
