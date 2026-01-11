//! Driver de red principal integrando VirtIO y smoltcp
//!
//! Este módulo gestiona la inicialización de la red y el stack TCP/IP.

use crate::drivers::{
    DriverResult,
    pci::{self, PciManager, PciDevice},
    virtio_net::VirtioNetDriver,
};
use crate::debug::serial_write_str;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spin::Mutex;

use smoltcp::iface::{Interface, SocketSet, Config, Routes, SocketStorage};
// NeighborCache es manejado internamente en 0.10
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer as TcpSocketBuffer};
use smoltcp::socket::udp::{Socket as UdpSocket, PacketBuffer as UdpSocketBuffer, PacketMetadata as UdpPacketMetadata};
use smoltcp::wire::{EthernetAddress, IpCidr, IpAddress, Ipv4Address, IpEndpoint};
use smoltcp::time::Instant;
use smoltcp::phy::Device;

// Instancia global del driver VirtIO (mutex interno)
pub static VIRTIO_NET: VirtioNetDriver = VirtioNetDriver::new_empty();

// Stack de red global
pub static NETWORK_INTERFACE: Mutex<Option<Interface>> = Mutex::new(None);
pub static SOCKET_SET: Mutex<Option<SocketSet<'static>>> = Mutex::new(None);

// Contador de tiempo simple (milisegundos)
static mut TIME_MILLIS: i64 = 0;

pub fn init_network_drivers() -> DriverResult<()> {
    serial_write_str("NET: Iniciando sistema de red...\n");

    // 1. Escanear bus PCI buscando VirtIO Net
    let mut pci_manager = PciManager::new();
    pci_manager.scan_devices();
    
    let mut found = false;
    for i in 0..pci_manager.device_count() {
        if let Some(device) = pci_manager.get_device(i) {
            // Vendor 0x1AF4 (VirtIO), Device 0x1000 (Legacy Net)
            if device.vendor_id == 0x1AF4 && device.device_id == 0x1000 {
                serial_write_str("NET: Dispositivo VirtIO Net encontrado!\n");
                
                // init() maneja el locking internamente
                if let Err(e) = VIRTIO_NET.init(*device) {
                    serial_write_str(&format!("NET: Error inicializando driver: {}\n", e));
                } else {
                    serial_write_str("NET: Driver inicializado correctamente.\n");
                    // Inicializar stack smoltcp
                    init_smoltcp_stack();
                    found = true;
                }
                break; // Usar el primero encontrado
            }
        }
    }
    
    if !found {
        serial_write_str("NET: No se encontró dispositivo de red compatible.\n");
    }

    Ok(())
}

fn init_smoltcp_stack() {
    let mac_bytes = VIRTIO_NET.mac_address();
    let ethernet_addr = EthernetAddress::from_bytes(&mac_bytes);
    
    // Configurar IP inicial (DHCP lo actualizará luego si implementamos cliente DHCP)
    // Por ahora estática para tests QEMU user net: 10.0.2.15/24, Gateway 10.0.2.2
    let ip_addr = IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24);
    
    // Configurar interfaz
    let mut config = Config::new(ethernet_addr.into());
    config.random_seed = 1234; // Semilla aleatoria
    
    // Crear device wrapper para el builder
    let device_ref = &VIRTIO_NET;
    let mut device_mut = device_ref;

    let timestamp = Instant::from_millis(0);
    let mut iface = Interface::new(config, &mut device_mut, timestamp);
    
    iface.update_ip_addrs(|ip_addrs| {
        let _ = ip_addrs.push(ip_addr);
    });
    
    // routes_mut() para smoltcp 0.10
    iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).unwrap();
    
    *NETWORK_INTERFACE.lock() = Some(iface);
    
    // Crear sockets ejemplo
    let mut sockets = SocketSet::new(alloc::vec![]);
    // Ejemplo: Socket UDP para DNS o tests
    let udp_rx_buffer = UdpSocketBuffer::new(alloc::vec![UdpPacketMetadataWithPayload::EMPTY], alloc::vec![0; 64]);
    let udp_tx_buffer = UdpSocketBuffer::new(alloc::vec![UdpPacketMetadataWithPayload::EMPTY], alloc::vec![0; 64]);
    let udp_socket = UdpSocket::new(udp_rx_buffer, udp_tx_buffer);
    sockets.add(udp_socket);
    
    *SOCKET_SET.lock() = Some(sockets);
    
    serial_write_str("NET: Stack TCP/IP inicializado (IP 10.0.2.15/24).\n");
}

// UdpPacketMetadata disponible como alias
type UdpPacketMetadataWithPayload = UdpPacketMetadata;

/// Función polling que debe ser llamada regularmente (timer interrupt o idle loop)
pub fn network_poll() {
    unsafe { TIME_MILLIS += 10; } // Simular avance de tiempo
    let timestamp = Instant::from_millis(unsafe { TIME_MILLIS });
    
    let mut iface_guard = NETWORK_INTERFACE.lock();
    let mut sockets_guard = SOCKET_SET.lock();
    
    // Poll con smoltcp 0.10: iface.poll(timestamp, device, socket_set) -> bool
    if let (Some(iface), Some(sockets)) = (iface_guard.as_mut(), sockets_guard.as_mut()) {
        let device_ref = &VIRTIO_NET; // Referencia al driver
        let mut device_mut_ref = device_ref; // &mut &VirtioNetDriver
        
        // El compilador puede inferir el tipo D from argumento device
        let processed = iface.poll(timestamp, &mut device_mut_ref, sockets);
        if processed {
             // Actividad de red procesada
        }
    }
}

/// Obtener estadísticas de red (compatible con API anterior)
pub fn get_network_statistics() -> (usize, usize, usize) {
    let connected = if NETWORK_INTERFACE.lock().is_some() { 1 } else { 0 };
    (1, connected, 0)
}

/// Inicializar gestor de red (dummy para compatibilidad con main)
pub fn init_network_manager() {
    // Ya se inicializa en init_network_drivers
}
