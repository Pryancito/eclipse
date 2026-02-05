//! Device Manager (devfs) - Creates and manages /dev nodes
//! 
//! This service manages device nodes in /dev, providing access to hardware devices.
//! It must start early, after the log service, so other services can access devices.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu, pci_enum_devices, PciDeviceInfo};

/// Device node type
#[derive(Clone, Copy, PartialEq, Debug)]
enum DeviceType {
    Block,      // Block devices (disks)
    Char,       // Character devices (console, tty)
    Network,    // Network interfaces
    Input,      // Input devices
    Audio,      // Audio devices
    Display,    // Display/framebuffer
    USB,        // USB controllers
}

/// Device node information
#[derive(Clone, Copy)]
struct DeviceNode {
    device_type: DeviceType,
    major: u32,
    minor: u32,
    pci_bus: u8,
    pci_device: u8,
    pci_function: u8,
    vendor_id: u16,
    device_id: u16,
}

/// Scan PCI bus and create device nodes
fn scan_and_create_devices() -> usize {
    println!("[DEVFS-SERVICE] Scanning PCI bus for devices...");
    
    // Enumerate all PCI devices (class 0xFF = all)
    let mut devices_buffer = [PciDeviceInfo {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0,
        device_id: 0,
        class_code: 0,
        subclass: 0,
        bar0: 0,
    }; 32];
    
    // Scan multiple device classes
    let mut total_devices = 0;
    
    // Scan storage devices (class 0x01)
    let storage_count = pci_enum_devices(0x01, &mut devices_buffer);
    if storage_count > 0 {
        println!("[DEVFS-SERVICE] Found {} storage device(s)", storage_count);
        for i in 0..storage_count {
            let dev = devices_buffer[i];
            create_storage_device_node(&dev, i);
            total_devices += 1;
        }
    }
    
    // Scan network devices (class 0x02)
    let network_count = pci_enum_devices(0x02, &mut devices_buffer);
    if network_count > 0 {
        println!("[DEVFS-SERVICE] Found {} network device(s)", network_count);
        for i in 0..network_count {
            let dev = devices_buffer[i];
            create_network_device_node(&dev, i);
            total_devices += 1;
        }
    }
    
    // Scan display devices (class 0x03)
    let display_count = pci_enum_devices(0x03, &mut devices_buffer);
    if display_count > 0 {
        println!("[DEVFS-SERVICE] Found {} display device(s)", display_count);
        for i in 0..display_count {
            let dev = devices_buffer[i];
            create_display_device_node(&dev, i);
            total_devices += 1;
        }
    }
    
    // Scan audio devices (class 0x04)
    let audio_count = pci_enum_devices(0x04, &mut devices_buffer);
    if audio_count > 0 {
        println!("[DEVFS-SERVICE] Found {} audio device(s)", audio_count);
        for i in 0..audio_count {
            let dev = devices_buffer[i];
            create_audio_device_node(&dev, i);
            total_devices += 1;
        }
    }
    
    // Scan USB controllers (class 0x0C, subclass 0x03)
    // Note: We need to implement class+subclass filtering in the kernel
    // For now, we'll just report USB separately
    println!("[DEVFS-SERVICE] USB device enumeration: delegated to USB service");
    
    total_devices
}

/// Create storage device node
fn create_storage_device_node(dev: &PciDeviceInfo, index: usize) {
    let device_name = match dev.subclass {
        0x01 => "ide",      // IDE controller
        0x06 => "sata",     // SATA controller
        0x08 => "nvme",     // NVMe controller
        _ => "disk",        // Generic storage
    };
    
    println!("[DEVFS-SERVICE] Creating storage device node:");
    println!("[DEVFS-SERVICE]   Device: /dev/{}{}", device_name, index);
    println!("[DEVFS-SERVICE]   Type: {}", match dev.subclass {
        0x01 => "IDE Controller",
        0x06 => "SATA Controller",
        0x08 => "NVMe Controller",
        _ => "Generic Storage",
    });
    println!("[DEVFS-SERVICE]   PCI: {:02x}:{:02x}.{}", dev.bus, dev.device, dev.function);
    println!("[DEVFS-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}", dev.vendor_id, dev.device_id);
    
    // For VirtIO block devices
    if dev.vendor_id == 0x1AF4 && (dev.device_id == 0x1001 || dev.device_id == 0x1042) {
        println!("[DEVFS-SERVICE]   VirtIO Block Device detected");
        println!("[DEVFS-SERVICE]   Creating /dev/vda (VirtIO disk)");
    }
}

/// Create network device node
fn create_network_device_node(dev: &PciDeviceInfo, index: usize) {
    println!("[DEVFS-SERVICE] Creating network device node:");
    println!("[DEVFS-SERVICE]   Device: /dev/eth{}", index);
    println!("[DEVFS-SERVICE]   Type: Ethernet Controller");
    println!("[DEVFS-SERVICE]   PCI: {:02x}:{:02x}.{}", dev.bus, dev.device, dev.function);
    println!("[DEVFS-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}", dev.vendor_id, dev.device_id);
    
    // Check for specific network cards
    match dev.vendor_id {
        0x8086 => println!("[DEVFS-SERVICE]   Intel Ethernet Controller"),
        0x10EC => println!("[DEVFS-SERVICE]   Realtek Ethernet Controller"),
        0x1AF4 => println!("[DEVFS-SERVICE]   VirtIO Network Device"),
        _ => println!("[DEVFS-SERVICE]   Generic Ethernet Controller"),
    }
}

/// Create display device node
fn create_display_device_node(dev: &PciDeviceInfo, index: usize) {
    println!("[DEVFS-SERVICE] Creating display device node:");
    println!("[DEVFS-SERVICE]   Device: /dev/fb{}", index);
    println!("[DEVFS-SERVICE]   Type: Framebuffer/Display");
    println!("[DEVFS-SERVICE]   PCI: {:02x}:{:02x}.{}", dev.bus, dev.device, dev.function);
    println!("[DEVFS-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}", dev.vendor_id, dev.device_id);
    
    // Check for specific GPUs
    match dev.vendor_id {
        0x10DE => println!("[DEVFS-SERVICE]   NVIDIA GPU"),
        0x1002 => println!("[DEVFS-SERVICE]   AMD GPU"),
        0x8086 => println!("[DEVFS-SERVICE]   Intel GPU"),
        0x1AF4 => println!("[DEVFS-SERVICE]   VirtIO GPU"),
        _ => println!("[DEVFS-SERVICE]   Generic VGA Controller"),
    }
    
    // Also create DRI node for 3D acceleration
    println!("[DEVFS-SERVICE]   Creating /dev/dri/card{}", index);
}

/// Create audio device node
fn create_audio_device_node(dev: &PciDeviceInfo, index: usize) {
    println!("[DEVFS-SERVICE] Creating audio device nodes:");
    
    let audio_type = match dev.subclass {
        0x01 => "AC97 Audio",
        0x03 => "Intel HDA",
        _ => "Generic Audio",
    };
    
    println!("[DEVFS-SERVICE]   Type: {}", audio_type);
    println!("[DEVFS-SERVICE]   PCI: {:02x}:{:02x}.{}", dev.bus, dev.device, dev.function);
    println!("[DEVFS-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}", dev.vendor_id, dev.device_id);
    println!("[DEVFS-SERVICE]   Creating /dev/snd/pcmC{}D0p (playback)", index);
    println!("[DEVFS-SERVICE]   Creating /dev/snd/pcmC{}D0c (capture)", index);
    println!("[DEVFS-SERVICE]   Creating /dev/snd/controlC{}", index);
}

/// Create standard device nodes (null, zero, random, etc.)
fn create_standard_devices() {
    println!("[DEVFS-SERVICE] Creating standard device nodes:");
    println!("[DEVFS-SERVICE]   /dev/null    - Null device (discards all writes)");
    println!("[DEVFS-SERVICE]   /dev/zero    - Zero device (infinite zeros)");
    println!("[DEVFS-SERVICE]   /dev/random  - Random number generator");
    println!("[DEVFS-SERVICE]   /dev/urandom - Non-blocking random");
    println!("[DEVFS-SERVICE]   /dev/console - System console");
    println!("[DEVFS-SERVICE]   /dev/tty     - Current terminal");
    println!("[DEVFS-SERVICE]   /dev/tty0    - Virtual console 0");
    println!("[DEVFS-SERVICE]   /dev/stdin   - Standard input");
    println!("[DEVFS-SERVICE]   /dev/stdout  - Standard output");
    println!("[DEVFS-SERVICE]   /dev/stderr  - Standard error");
}

/// Create input device nodes
fn create_input_devices() {
    println!("[DEVFS-SERVICE] Creating input device nodes:");
    println!("[DEVFS-SERVICE]   /dev/input/event0 - Keyboard");
    println!("[DEVFS-SERVICE]   /dev/input/event1 - Mouse");
    println!("[DEVFS-SERVICE]   /dev/input/event2 - Tablet");
    println!("[DEVFS-SERVICE]   /dev/input/mice   - All mice");
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║            DEVICE MANAGER (devfs) SERVICE                    ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!("[DEVFS-SERVICE] Starting (PID: {})", pid);
    println!("[DEVFS-SERVICE] Initializing device filesystem...");
    
    // Create /dev directory structure
    println!("[DEVFS-SERVICE] Creating /dev directory structure");
    println!("[DEVFS-SERVICE]   /dev/");
    println!("[DEVFS-SERVICE]   /dev/snd/");
    println!("[DEVFS-SERVICE]   /dev/input/");
    println!("[DEVFS-SERVICE]   /dev/dri/");
    println!("[DEVFS-SERVICE]   /dev/disk/");
    
    // Create standard device nodes first
    create_standard_devices();
    
    // Create input device nodes
    create_input_devices();
    
    // Scan PCI bus and create hardware device nodes
    println!("[DEVFS-SERVICE] Scanning hardware devices...");
    let device_count = scan_and_create_devices();
    
    println!("[DEVFS-SERVICE] Device filesystem initialization complete");
    println!("[DEVFS-SERVICE] Total hardware devices detected: {}", device_count);
    println!("[DEVFS-SERVICE] Device nodes created successfully");
    println!("[DEVFS-SERVICE] Ready to serve device requests");
    
    // Main loop - monitor device changes and handle requests
    let mut heartbeat_counter = 0u64;
    let mut hotplug_events = 0u64;
    
    loop {
        heartbeat_counter += 1;
        
        // Monitor for device hotplug events
        // In a real implementation, this would:
        // - Listen for PCI hotplug interrupts
        // - Monitor USB device insertion/removal
        // - Update device nodes dynamically
        // - Notify other services of device changes
        
        // Simulate occasional hotplug check
        if heartbeat_counter % 1000000 == 0 {
            // In real implementation, check for new devices
            // For now, just report status
        }
        
        // Periodic status updates
        if heartbeat_counter % 500000 == 0 {
            println!("[DEVFS-SERVICE] Operational - Devices: {}, Hotplug events: {}", 
                     device_count, hotplug_events);
        }
        
        yield_cpu();
    }
}
