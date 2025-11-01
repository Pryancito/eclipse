//! Interfaces de Red Virtuales para Eclipse OS
//!
//! Este módulo contiene implementaciones de interfaces de red virtuales
//! que permiten simular redes para desarrollo y pruebas.

use super::network::*;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

// Re-export types from network module
use super::network::{NetworkError, NetworkResult, Protocol, NetworkPacket, Ipv4Addr, MacAddr};

/// Interfaz de red virtual que simula una conexión de red
pub struct VirtualNetworkInterface {
    /// Nombre de la interfaz
    name: String,
    /// Dirección IP asignada
    ip_addr: Option<Ipv4Addr>,
    /// Dirección MAC
    mac_addr: MacAddr,
    /// Estado de la interfaz
    is_up: bool,
    /// Buffer de paquetes transmitidos
    tx_buffer: Vec<NetworkPacket>,
    /// Buffer de paquetes para recepción
    rx_queue: Vec<NetworkPacket>,
}

impl VirtualNetworkInterface {
    /// Crea una nueva interfaz de red virtual
    pub fn new(name: &str) -> Self {
        // Generar una dirección MAC "virtual" basada en el nombre
        let name_bytes = name.as_bytes();
        let mac_bytes = [
            0xAA, // Prefijo virtual
            0xBB,
            name_bytes.get(0).copied().unwrap_or(0),
            name_bytes.get(1).copied().unwrap_or(0),
            name_bytes.get(2).copied().unwrap_or(0),
            name_bytes.len() as u8,
        ];

        Self {
            name: name.to_string(),
            ip_addr: None,
            mac_addr: MacAddr::from(mac_bytes),
            is_up: false,
            tx_buffer: Vec::new(),
            rx_queue: Vec::new(),
        }
    }

    /// Simula la recepción de un paquete (para pruebas)
    pub fn simulate_packet_receive(&mut self, packet: NetworkPacket) {
        self.rx_queue.push(packet);
    }

    /// Obtiene los paquetes transmitidos (para inspección)
    pub fn get_transmitted_packets(&self) -> &[NetworkPacket] {
        &self.tx_buffer
    }

    /// Limpia el buffer de transmisión
    pub fn clear_tx_buffer(&mut self) {
        self.tx_buffer.clear();
    }
}

impl NetworkInterface for VirtualNetworkInterface {
    fn send_packet(&mut self, packet: &NetworkPacket) -> NetworkResult<()> {
        if !self.is_up {
            return Err(crate::network::NetworkError::Other("Interfaz no está activa".to_string()));
        }

        // En una interfaz virtual, simplemente almacenamos el paquete
        self.tx_buffer.push(packet.clone());

        // Simular latencia de red básica
        // En un sistema real, aquí se enviaría por hardware

        Ok(())
    }

    fn receive_packet(&mut self) -> NetworkResult<Option<NetworkPacket>> {
        if !self.is_up {
            return Err(crate::network::NetworkError::Other("Interfaz no está activa".to_string()));
        }

        // Devolver el siguiente paquete de la cola RX si hay
        Ok(self.rx_queue.pop())
    }

    fn get_ip_address(&self) -> Option<Ipv4Addr> {
        self.ip_addr
    }

    fn set_ip_address(&mut self, addr: Ipv4Addr) {
        self.ip_addr = Some(addr);
        self.is_up = true; // Activar la interfaz cuando se asigna IP
    }

    fn get_mac_address(&self) -> Option<MacAddr> {
        Some(self.mac_addr)
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn is_up(&self) -> bool {
        self.is_up
    }
}

/// Router virtual que puede enrutar paquetes entre interfaces
pub struct VirtualRouter {
    /// Interfaces conectadas al router
    interfaces: Vec<Box<dyn NetworkInterface>>,
    /// Tabla de enrutamiento simple
    routing_table: Vec<(Ipv4Addr, String)>, // (red, interfaz)
}

impl VirtualRouter {
    /// Crea un nuevo router virtual
    pub fn new() -> Self {
        Self {
            interfaces: Vec::new(),
            routing_table: Vec::new(),
        }
    }

    /// Agrega una interfaz al router
    pub fn add_interface(&mut self, interface: Box<dyn NetworkInterface>) {
        self.interfaces.push(interface);
    }

    /// Agrega una ruta a la tabla de enrutamiento
    pub fn add_route(&mut self, network: Ipv4Addr, interface_name: &str) {
        self.routing_table.push((network, interface_name.to_string()));
    }

    /// Procesa paquetes entre interfaces conectadas
    pub fn process_packets(&mut self) -> NetworkResult<()> {
        // Para cada interfaz, verificar si hay paquetes para enrutar
        for i in 0..self.interfaces.len() {
            if let Ok(Some(packet)) = self.interfaces[i].receive_packet() {
                // Encontrar la interfaz de destino
                if let Some(dest_interface) = self.find_destination_interface(&packet.dst_addr) {
                    if dest_interface != i {
                        // Reenviar el paquete a la interfaz correcta
                        let _ = self.interfaces[dest_interface].send_packet(&packet);
                    }
                }
            }
        }
        Ok(())
    }

    /// Encuentra la interfaz apropiada para una dirección de destino
    fn find_destination_interface(&self, dst_addr: &Ipv4Addr) -> Option<usize> {
        // Búsqueda simple: verificar si la dirección coincide con alguna ruta
        for (network, interface_name) in &self.routing_table {
            // Verificación básica de red (solo primer octeto por simplicidad)
            if network.octets[0] == dst_addr.octets[0] {
                // Encontrar el índice de la interfaz
                for (i, interface) in self.interfaces.iter().enumerate() {
                    if interface.get_name() == interface_name {
                        return Some(i);
                    }
                }
            }
        }
        None
    }

    /// Obtiene estadísticas del router
    pub fn get_stats(&self) -> RouterStats {
        RouterStats {
            connected_interfaces: self.interfaces.len(),
            routing_entries: self.routing_table.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouterStats {
    /// Número de interfaces conectadas
    pub connected_interfaces: usize,
    /// Número de entradas de enrutamiento
    pub routing_entries: usize,
}

/// Simulador de red para pruebas
pub struct NetworkSimulator {
    /// Router virtual
    router: VirtualRouter,
    /// Interfaces de red simuladas
    interfaces: Vec<Box<VirtualNetworkInterface>>,
}

impl NetworkSimulator {
    /// Crea un nuevo simulador de red
    pub fn new() -> Self {
        Self {
            router: VirtualRouter::new(),
            interfaces: Vec::new(),
        }
    }

    /// Agrega una interfaz simulada a la red
    pub fn add_interface(&mut self, name: &str, ip_addr: Ipv4Addr) -> NetworkResult<usize> {
        let mut interface = Box::new(VirtualNetworkInterface::new(name));
        interface.set_ip_address(ip_addr);

        let interface_id = self.interfaces.len();
        self.interfaces.push(interface.clone());
        self.router.add_interface(interface);

        // Agregar ruta automática basada en el primer octeto de IP
        let network = Ipv4Addr::new(ip_addr.octets[0], 0, 0, 0);
        self.router.add_route(network, name);

        Ok(interface_id)
    }

    /// Envía un paquete de una interfaz a otra
    pub fn send_packet_between(&mut self, from_interface: usize, to_interface: usize, packet: NetworkPacket) -> NetworkResult<()> {
        if from_interface >= self.interfaces.len() || to_interface >= self.interfaces.len() {
            return Err(crate::network::NetworkError::InterfaceNotFound);
        }

        // Simular envío del paquete
        self.interfaces[from_interface].simulate_packet_receive(packet);

        // Procesar el enrutamiento
        self.router.process_packets()?;

        Ok(())
    }

    /// Obtiene una interfaz por ID
    pub fn get_interface(&self, id: usize) -> Option<&VirtualNetworkInterface> {
        self.interfaces.get(id).map(|i| i.as_ref())
    }

    /// Obtiene una interfaz por ID (mutable)
    pub fn get_interface_mut(&mut self, id: usize) -> Option<&mut VirtualNetworkInterface> {
        self.interfaces.get_mut(id).map(|i| i.as_mut())
    }

    /// Ejecuta una ronda de procesamiento de red
    pub fn process_network(&mut self) -> NetworkResult<()> {
        self.router.process_packets()
    }

    /// Obtiene estadísticas de la simulación
    pub fn get_stats(&self) -> NetworkSimulatorStats {
        let mut total_tx_packets = 0;
        let mut total_rx_packets = 0;

        for interface in &self.interfaces {
            total_tx_packets += interface.tx_buffer.len();
            // Para RX, contaríamos paquetes procesados, pero por simplicidad usamos el tamaño de la cola
        }

        NetworkSimulatorStats {
            interfaces_count: self.interfaces.len(),
            total_tx_packets,
            total_rx_packets,
            router_stats: self.router.get_stats(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkSimulatorStats {
    /// Número de interfaces simuladas
    pub interfaces_count: usize,
    /// Total de paquetes transmitidos
    pub total_tx_packets: usize,
    /// Total de paquetes recibidos
    pub total_rx_packets: usize,
    /// Estadísticas del router
    pub router_stats: RouterStats,
}

/// Función de demostración del sistema de red virtual
pub fn demo_virtual_networking() -> NetworkResult<()> {


    // Crear simulador de red
    let mut simulator = NetworkSimulator::new();

    // Agregar interfaces
    let iface1 = simulator.add_interface("eth0", Ipv4Addr::new(192, 168, 1, 10))?;
    let iface2 = simulator.add_interface("eth1", Ipv4Addr::new(192, 168, 1, 20))?;

    let _ = (&iface1, &iface2);

    // Crear un paquete de prueba
    let test_data = b"Hola desde la red virtual!";
    let packet = NetworkPacket::new(
        test_data.to_vec(),
        Ipv4Addr::new(192, 168, 1, 10),
        Ipv4Addr::new(192, 168, 1, 20),
        Protocol::Udp,
    ).with_ports(12345, 54321);

    // Enviar paquete
    simulator.send_packet_between(iface1, iface2, packet)?;

    // Procesar la red
    simulator.process_network()?;

    // Verificar que el paquete fue recibido
    if let Some(iface) = simulator.get_interface(iface2) {
        let tx_packets = iface.get_transmitted_packets();
        let _ = tx_packets.len();
    }

    // Mostrar estadísticas
    let _ = simulator.get_stats();

    Ok(())
}
