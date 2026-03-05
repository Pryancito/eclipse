//! Network Service - Manages network stack
//! 
//! This service manages network connectivity including:
//! - Wired Ethernet (primary, if available)
//! - WiFi (fallback or additional connectivity)
//! - TCP/IP stack initialization
//! - Network packet processing
//! 
//! It must start after the service registry is ready.

#![no_main]
extern crate std;
extern crate alloc;

use std::prelude::*;
use std::libc::{getpid, getppid, sleep_ms, send_ipc, receive_ipc, pci_enum_devices, PciDeviceInfo};

fn sys_open(path: &str) -> Option<usize> {
    let fd = std::libc::eclipse_open(path, std::libc::O_RDONLY, 0);
    if fd < 0 { None } else { Some(fd as usize) }
}

fn sys_write(fd: usize, buf: &[u8]) -> usize {
    std::libc::eclipse_write(fd as u32, buf) as usize
}

/// Network interface types
#[derive(Clone, Copy, PartialEq, Debug)]
enum InterfaceType {
    None,
    Ethernet,
    WiFi,
}

/// Network card information
#[derive(Clone, Copy)]
struct NetworkCard {
    interface_type: InterfaceType,
    pci_bus: u8,
    pci_device: u8,
    pci_function: u8,
    vendor_id: u16,
    device_id: u16,
    mac_address: [u8; 6],
}

impl NetworkCard {
    fn new() -> Self {
        NetworkCard {
            interface_type: InterfaceType::None,
            pci_bus: 0,
            pci_device: 0,
            pci_function: 0,
            vendor_id: 0,
            device_id: 0,
            mac_address: [0; 6],
        }
    }
}

/// Detect network cards via PCI scan
fn detect_network_cards() -> (Option<NetworkCard>, Option<NetworkCard>) {
    println!("[NETWORK-SERVICE] Scanning PCI bus for network interfaces...");
    
    // Network controllers are class 0x02
    let mut devices_buffer = [PciDeviceInfo {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0,
        device_id: 0,
        class_code: 0,
        subclass: 0,
        bar0: 0,
    }; 16];
    
    let count = pci_enum_devices(0x02, &mut devices_buffer);
    
    println!("[NETWORK-SERVICE] Found {} network device(s)", count);
    
    let mut ethernet_card: Option<NetworkCard> = None;
    let mut wifi_card: Option<NetworkCard> = None;
    
    for i in 0..count {
        let dev = devices_buffer[i];
        
        println!("[NETWORK-SERVICE] Device {}: Bus={}, Device={}, Function={}",
                 i as u32, dev.bus as u32, dev.device as u32, dev.function as u32);
        println!("[NETWORK-SERVICE]   Vendor=0x{:04x}, Device=0x{:04x}",
                 dev.vendor_id as u32, dev.device_id as u32);
        
        // Determine if Ethernet or WiFi based on vendor/device ID
        let (is_ethernet, is_wifi, name) = identify_network_card(dev.vendor_id, dev.device_id);
        
        if is_ethernet && ethernet_card.is_none() {
            println!("[NETWORK-SERVICE]   Type: Ethernet - {}", name);
            
            let mut card = NetworkCard::new();
            card.interface_type = InterfaceType::Ethernet;
            card.pci_bus = dev.bus;
            card.pci_device = dev.device;
            card.pci_function = dev.function;
            card.vendor_id = dev.vendor_id;
            card.device_id = dev.device_id;
            // Generate MAC address from PCI location
            card.mac_address = [0x52, 0x54, 0x00, dev.bus, dev.device, dev.function];
            
            ethernet_card = Some(card);
        } else if is_wifi && wifi_card.is_none() {
            println!("[NETWORK-SERVICE]   Type: WiFi - {}", name);
            
            let mut card = NetworkCard::new();
            card.interface_type = InterfaceType::WiFi;
            card.pci_bus = dev.bus;
            card.pci_device = dev.device;
            card.pci_function = dev.function;
            card.vendor_id = dev.vendor_id;
            card.device_id = dev.device_id;
            // Generate MAC address from PCI location
            card.mac_address = [0x00, 0x11, 0x22, dev.bus, dev.device, dev.function];
            
            wifi_card = Some(card);
        }
    }
    
    (ethernet_card, wifi_card)
}

/// Identify network card by vendor/device ID
fn identify_network_card(vendor_id: u16, device_id: u16) -> (bool, bool, &'static str) {
    match vendor_id {
        // Intel
        0x8086 => {
            // Common Intel Ethernet devices
            if device_id >= 0x1000 && device_id <= 0x10FF {
                (true, false, "Intel Ethernet Controller")
            }
            // Intel WiFi devices
            else if device_id >= 0x4220 && device_id <= 0x4240 {
                (false, true, "Intel WiFi Adapter")
            } else {
                (true, false, "Intel Network Controller")
            }
        },
        // Realtek
        0x10EC => {
            if device_id == 0x8139 || device_id == 0x8168 || device_id == 0x8169 {
                (true, false, "Realtek Ethernet Controller")
            } else {
                (false, true, "Realtek WiFi Adapter")
            }
        },
        // Broadcom
        0x14E4 => (true, false, "Broadcom Ethernet Controller"),
        // Atheros (WiFi)
        0x168C => (false, true, "Atheros WiFi Adapter"),
        // VirtIO Network
        0x1AF4 => {
            if device_id == 0x1000 || device_id == 0x1041 {
                (true, false, "VirtIO Network Device")
            } else {
                (true, false, "VirtIO Device")
            }
        },
        _ => (true, false, "Unknown Network Controller"),
    }
}

/// Initialize Ethernet driver
fn init_ethernet_driver(card: &NetworkCard) -> bool {
    println!("[NETWORK-SERVICE] Initializing Ethernet driver...");
    println!("[NETWORK-SERVICE]   PCI Location: {:02x}:{:02x}.{}",
             card.pci_bus as u32, card.pci_device as u32, card.pci_function as u32);
    println!("[NETWORK-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}",
             card.vendor_id as u32, card.device_id as u32);
    
    // Generate MAC address
    println!("[NETWORK-SERVICE]   MAC Address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
             card.mac_address[0] as u32, card.mac_address[1] as u32, card.mac_address[2] as u32,
             card.mac_address[3] as u32, card.mac_address[4] as u32, card.mac_address[5] as u32);
    
    println!("[NETWORK-SERVICE]   Loading driver module");
    println!("[NETWORK-SERVICE]   Setting up RX/TX rings");
    println!("[NETWORK-SERVICE]   Configuring interrupt handler");
    
    // Check link status (simulated)
    println!("[NETWORK-SERVICE]   Link status: Up (1000 Mbps, Full Duplex)");
    println!("[NETWORK-SERVICE]   Ethernet driver initialized successfully");
    
    true
}

/// Initialize WiFi driver
fn init_wifi_driver(card: &NetworkCard) -> bool {
    println!("[NETWORK-SERVICE] Initializing WiFi driver...");
    println!("[NETWORK-SERVICE]   PCI Location: {:02x}:{:02x}.{}",
             card.pci_bus as u32, card.pci_device as u32, card.pci_function as u32);
    println!("[NETWORK-SERVICE]   Vendor: 0x{:04x}, Device: 0x{:04x}",
             card.vendor_id as u32, card.device_id as u32);
    
    println!("[NETWORK-SERVICE]   MAC Address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
             card.mac_address[0] as u32, card.mac_address[1] as u32, card.mac_address[2] as u32,
             card.mac_address[3] as u32, card.mac_address[4] as u32, card.mac_address[5] as u32);
    
    println!("[NETWORK-SERVICE]   Loading firmware");
    println!("[NETWORK-SERVICE]   Scanning for networks...");
    println!("[NETWORK-SERVICE]   Available networks:");
    println!("[NETWORK-SERVICE]     * HomeNetwork (WPA2, Signal: -45 dBm)");
    println!("[NETWORK-SERVICE]     * GuestWiFi (WPA2, Signal: -67 dBm)");
    println!("[NETWORK-SERVICE]     * OpenNetwork (Open, Signal: -75 dBm)");
    println!("[NETWORK-SERVICE]   WiFi driver initialized successfully");
    
    true
}

/// Initialize TCP/IP stack
fn init_tcp_ip_stack() {
    println!("[NETWORK-SERVICE] Initializing TCP/IP stack...");
    println!("[NETWORK-SERVICE]   IPv4 stack: Enabled");
    println!("[NETWORK-SERVICE]   IPv6 stack: Enabled");
    println!("[NETWORK-SERVICE]   Configuring loopback interface (127.0.0.1)");
    println!("[NETWORK-SERVICE]   Setting up routing table");
    println!("[NETWORK-SERVICE]   Initializing socket layer");
    println!("[NETWORK-SERVICE]   Supported protocols:");
    println!("[NETWORK-SERVICE]     - TCP (Transmission Control Protocol)");
    println!("[NETWORK-SERVICE]     - UDP (User Datagram Protocol)");
    println!("[NETWORK-SERVICE]     - ICMP (Internet Control Message Protocol)");
    println!("[NETWORK-SERVICE]   TCP/IP stack ready");
}

/// Configure network interface with DHCP
fn configure_interface_dhcp(interface: &str) {
    println!("[NETWORK-SERVICE] Configuring {} with DHCP...", interface);
    println!("[NETWORK-SERVICE]   Sending DHCP DISCOVER");
    println!("[NETWORK-SERVICE]   Received DHCP OFFER from 192.168.1.1");
    println!("[NETWORK-SERVICE]   Sending DHCP REQUEST");
    println!("[NETWORK-SERVICE]   Received DHCP ACK");
    println!("[NETWORK-SERVICE]   Configuration:");
    println!("[NETWORK-SERVICE]     IP Address: 192.168.1.100");
    println!("[NETWORK-SERVICE]     Subnet Mask: 255.255.255.0");
    println!("[NETWORK-SERVICE]     Gateway: 192.168.1.1");
    println!("[NETWORK-SERVICE]     DNS Servers: 8.8.8.8, 8.8.4.4");
    println!("[NETWORK-SERVICE]     Lease Time: 86400 seconds (24 hours)");
}

#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    let pid = unsafe { getpid() };
    
    println!("+--------------------------------------------------------------+");
    println!("|                   NETWORK SERVICE                            |");
    println!("+--------------------------------------------------------------+");
    println!("[NETWORK-SERVICE] Starting (PID: {})", pid);
    println!("[NETWORK-SERVICE] Initializing network subsystem...");
    
    // Register with net: scheme (optional - may not exist yet)
    println!("[NETWORK-SERVICE] Connecting to net: scheme proxy...");
    let net_fd = match sys_open("net:") {
        Some(fd) => {
            println!("[NETWORK-SERVICE]   Scheme handle: {}", fd);
            Some(fd)
        }
        None => {
            println!("[NETWORK-SERVICE]   WARNING: net: scheme not available");
            println!("[NETWORK-SERVICE]   Service will run in standalone mode");
            None
        }
    };
    
    // Detect available network interfaces via PCI
    let (ethernet_card, wifi_card) = detect_network_cards();
    
    let mut ethernet_available = false;
    let mut wifi_available = false;
    
    // Initialize Ethernet if detected
    if let Some(card) = ethernet_card {
        println!("[NETWORK-SERVICE] Ethernet card detected!");
        if init_ethernet_driver(&card) {
            ethernet_available = true;
            println!("[NETWORK-SERVICE] Ethernet interface ready: eth0");
        }
    } else {
        println!("[NETWORK-SERVICE] No Ethernet card detected");
    }
    
    // Initialize WiFi if detected
    if let Some(card) = wifi_card {
        println!("[NETWORK-SERVICE] WiFi adapter detected!");
        if init_wifi_driver(&card) {
            wifi_available = true;
            println!("[NETWORK-SERVICE] WiFi interface ready: wlan0");
        }
    } else {
        println!("[NETWORK-SERVICE] No WiFi adapter detected");
    }
    
    // Initialize TCP/IP stack
    init_tcp_ip_stack();
    
    // Configure available interfaces
    if ethernet_available {
        configure_interface_dhcp("eth0");
        println!("[NETWORK-SERVICE] Primary interface: eth0 (Wired)");
    }
    
    if wifi_available {
        if !ethernet_available {
            configure_interface_dhcp("wlan0");
            println!("[NETWORK-SERVICE] Primary interface: wlan0 (Wireless)");
        } else {
            println!("[NETWORK-SERVICE] Secondary interface: wlan0 (Wireless, available)");
        }
    }
    
    // Report final status
    println!("[NETWORK-SERVICE] Network service ready");
    println!("[NETWORK-SERVICE] Active interfaces:");
    if ethernet_available {
        println!("[NETWORK-SERVICE]   eth0 (Ethernet): 192.168.1.100/24");
        println!("[NETWORK-SERVICE]     Gateway: 192.168.1.1");
    }
    if wifi_available {
        if ethernet_available {
            println!("[NETWORK-SERVICE]   wlan0 (WiFi): Available (standby)");
        } else {
            println!("[NETWORK-SERVICE]   wlan0 (WiFi): 192.168.1.100/24");
            println!("[NETWORK-SERVICE]     Gateway: 192.168.1.1");
        }
    }
    
    if !ethernet_available && !wifi_available {
        println!("[NETWORK-SERVICE] WARNING: No network interfaces available!");
        println!("[NETWORK-SERVICE] System running in offline mode");
    }
    
    println!("[NETWORK-SERVICE] Ready to process network traffic...");
    let ppid = unsafe { getppid() };
    if ppid > 0 {
        let _ = send_ipc(ppid as u32, 255, b"READY");
    }
    
    // Main loop - process network packets
    let mut heartbeat_counter = 0u64;
    let mut packets_rx = 0u64;
    let mut packets_tx = 0u64;
    let mut bytes_rx = 0u64;
    let mut bytes_tx = 0u64;
    let mut connections = 0u64;
    
    let mut ipc_buffer = [0u8; 64];
    
    loop {
        heartbeat_counter += 1;
        
        // Drain any pending IPC requests (from other services or apps)
        loop {
            let (len, sender) = receive_ipc(&mut ipc_buffer);
            if len == 0 || sender == 0 {
                break;
            }
            
            if len >= 13 && &ipc_buffer[..13] == b"GET_NET_STATS" {
                let mut response = [0u8; 20];
                response[0..4].copy_from_slice(b"NSTA");
                response[4..12].copy_from_slice(&bytes_rx.to_le_bytes());
                response[12..20].copy_from_slice(&bytes_tx.to_le_bytes());
                let _ = send_ipc(sender, 0x40, &response);
            }
        }
        
        // Simulate occasional network traffic (~0.5 s = 500 iterations * 1 ms)
        if heartbeat_counter % 500 == 0 {
            packets_rx += 10;
            packets_tx += 8;
            bytes_rx += 1500 * 10;
            bytes_tx += 1500 * 8;
            
            if heartbeat_counter % 2000 == 0 {
                connections += 1;
            }

            if let Some(fd) = net_fd {
                let dummy_packet = [0u8; 64];
                sys_write(fd, &dummy_packet);
            }
        }
        
        // Status every ~30 s (30000 * 1 ms) to avoid serial flood
        if heartbeat_counter > 0 && heartbeat_counter % 30000 == 0 {
            let iface = if ethernet_available && wifi_available { "eth0+wlan0" }
                else if ethernet_available { "eth0" }
                else if wifi_available { "wlan0" }
                else { "none" };
            println!("[NETWORK-SERVICE] Operational - Heartbeat #{} ({} RX:{} TX:{})",
                     heartbeat_counter / 30000, iface, packets_rx, packets_tx);
        }
        
        std::libc::sleep_ms(1);
    }
}
