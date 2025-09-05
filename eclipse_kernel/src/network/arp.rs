//! Implementación del protocolo ARP (Address Resolution Protocol)
//! 
//! Protocolo para resolver direcciones IP a direcciones MAC

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::fmt;

use super::ip::IpAddress;
use super::NetworkError;

/// Dirección MAC (6 bytes)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress {
    pub bytes: [u8; 6],
}

impl MacAddress {
    /// Crear nueva dirección MAC
    pub fn new(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8) -> Self {
        Self { bytes: [a, b, c, d, e, f] }
    }
    
    /// Crear desde array de bytes
    pub fn from_bytes(bytes: [u8; 6]) -> Self {
        Self { bytes }
    }
    
    /// Dirección MAC de broadcast (FF:FF:FF:FF:FF:FF)
    pub fn broadcast() -> Self {
        Self::new(0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF)
    }
    
    /// Dirección MAC cero (00:00:00:00:00:00)
    pub fn zero() -> Self {
        Self::new(0x00, 0x00, 0x00, 0x00, 0x00, 0x00)
    }
    
    /// Verificar si es dirección de broadcast
    pub fn is_broadcast(&self) -> bool {
        self.bytes == [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
    }
    
    /// Verificar si es dirección cero
    pub fn is_zero(&self) -> bool {
        self.bytes == [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    }
    
    /// Verificar si es dirección local (primer byte par)
    pub fn is_local(&self) -> bool {
        (self.bytes[0] & 0x02) == 0
    }
    
    /// Verificar si es dirección unicast
    pub fn is_unicast(&self) -> bool {
        !self.is_broadcast() && !self.is_zero()
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
               self.bytes[0], self.bytes[1], self.bytes[2],
               self.bytes[3], self.bytes[4], self.bytes[5])
    }
}

/// Operaciones ARP
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArpOperation {
    Request = 1,
    Reply = 2,
    RarpRequest = 3,
    RarpReply = 4,
    Unknown = 0,
}

impl From<u16> for ArpOperation {
    fn from(value: u16) -> Self {
        match value {
            1 => ArpOperation::Request,
            2 => ArpOperation::Reply,
            3 => ArpOperation::RarpRequest,
            4 => ArpOperation::RarpReply,
            _ => ArpOperation::Unknown,
        }
    }
}

/// Cabecera ARP
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct ArpHeader {
    pub hardware_type: u16,     // Tipo de hardware (1 = Ethernet)
    pub protocol_type: u16,     // Tipo de protocolo (0x0800 = IPv4)
    pub hardware_size: u8,      // Tamaño de dirección hardware (6 para MAC)
    pub protocol_size: u8,      // Tamaño de dirección protocolo (4 para IPv4)
    pub operation: u16,         // Operación ARP
    pub sender_mac: [u8; 6],    // Dirección MAC del emisor
    pub sender_ip: [u8; 4],     // Dirección IP del emisor
    pub target_mac: [u8; 6],    // Dirección MAC del destino
    pub target_ip: [u8; 4],     // Dirección IP del destino
}

impl ArpHeader {
    /// Crear nueva cabecera ARP
    pub fn new(operation: ArpOperation, sender_mac: MacAddress, sender_ip: IpAddress, target_mac: MacAddress, target_ip: IpAddress) -> Self {
        Self {
            hardware_type: 1,           // Ethernet
            protocol_type: 0x0800,      // IPv4
            hardware_size: 6,           // MAC address size
            protocol_size: 4,           // IPv4 address size
            operation: operation as u16,
            sender_mac: sender_mac.bytes,
            sender_ip: sender_ip.bytes,
            target_mac: target_mac.bytes,
            target_ip: target_ip.bytes,
        }
    }
    
    /// Obtener operación ARP
    pub fn get_operation(&self) -> ArpOperation {
        ArpOperation::from(self.operation)
    }
    
    /// Obtener dirección MAC del emisor
    pub fn get_sender_mac(&self) -> MacAddress {
        MacAddress::from_bytes(self.sender_mac)
    }
    
    /// Obtener dirección IP del emisor
    pub fn get_sender_ip(&self) -> IpAddress {
        IpAddress::from_bytes(self.sender_ip)
    }
    
    /// Obtener dirección MAC del destino
    pub fn get_target_mac(&self) -> MacAddress {
        MacAddress::from_bytes(self.target_mac)
    }
    
    /// Obtener dirección IP del destino
    pub fn get_target_ip(&self) -> IpAddress {
        IpAddress::from_bytes(self.target_ip)
    }
    
    /// Establecer dirección MAC del emisor
    pub fn set_sender_mac(&mut self, mac: MacAddress) {
        self.sender_mac = mac.bytes;
    }
    
    /// Establecer dirección IP del emisor
    pub fn set_sender_ip(&mut self, ip: IpAddress) {
        self.sender_ip = ip.bytes;
    }
    
    /// Establecer dirección MAC del destino
    pub fn set_target_mac(&mut self, mac: MacAddress) {
        self.target_mac = mac.bytes;
    }
    
    /// Establecer dirección IP del destino
    pub fn set_target_ip(&mut self, ip: IpAddress) {
        self.target_ip = ip.bytes;
    }
}

/// Paquete ARP
pub struct ArpPacket {
    pub header: ArpHeader,
}

impl ArpPacket {
    /// Crear nuevo paquete ARP
    pub fn new(operation: ArpOperation, sender_mac: MacAddress, sender_ip: IpAddress, target_mac: MacAddress, target_ip: IpAddress) -> Self {
        Self {
            header: ArpHeader::new(operation, sender_mac, sender_ip, target_mac, target_ip),
        }
    }
    
    /// Crear solicitud ARP
    pub fn request(sender_mac: MacAddress, sender_ip: IpAddress, target_ip: IpAddress) -> Self {
        Self::new(
            ArpOperation::Request,
            sender_mac,
            sender_ip,
            MacAddress::zero(),
            target_ip,
        )
    }
    
    /// Crear respuesta ARP
    pub fn reply(sender_mac: MacAddress, sender_ip: IpAddress, target_mac: MacAddress, target_ip: IpAddress) -> Self {
        Self::new(
            ArpOperation::Reply,
            sender_mac,
            sender_ip,
            target_mac,
            target_ip,
        )
    }
    
    /// Serializar paquete a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(28);
        
        // Serializar header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.header as *const ArpHeader as *const u8, 28)
        };
        bytes.extend_from_slice(header_bytes);
        
        bytes
    }
    
    /// Crear paquete desde bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 28 {
            return None;
        }
        
        let header = unsafe {
            *(data.as_ptr() as *const ArpHeader)
        };
        
        Some(Self { header })
    }
    
    /// Obtener operación
    pub fn get_operation(&self) -> ArpOperation {
        self.header.get_operation()
    }
    
    /// Obtener dirección MAC del emisor
    pub fn get_sender_mac(&self) -> MacAddress {
        self.header.get_sender_mac()
    }
    
    /// Obtener dirección IP del emisor
    pub fn get_sender_ip(&self) -> IpAddress {
        self.header.get_sender_ip()
    }
    
    /// Obtener dirección MAC del destino
    pub fn get_target_mac(&self) -> MacAddress {
        self.header.get_target_mac()
    }
    
    /// Obtener dirección IP del destino
    pub fn get_target_ip(&self) -> IpAddress {
        self.header.get_target_ip()
    }
    
    /// Verificar si es solicitud ARP
    pub fn is_request(&self) -> bool {
        self.get_operation() == ArpOperation::Request
    }
    
    /// Verificar si es respuesta ARP
    pub fn is_reply(&self) -> bool {
        self.get_operation() == ArpOperation::Reply
    }
}

/// Entrada de la tabla ARP
#[derive(Debug, Clone)]
pub struct ArpEntry {
    pub ip: IpAddress,
    pub mac: MacAddress,
    pub timestamp: u64,
    pub is_static: bool,
}

impl ArpEntry {
    /// Crear nueva entrada ARP
    pub fn new(ip: IpAddress, mac: MacAddress, is_static: bool) -> Self {
        Self {
            ip,
            mac,
            timestamp: 0, // Se establecerá al agregar a la tabla
            is_static,
        }
    }
    
    /// Verificar si la entrada ha expirado
    pub fn is_expired(&self, current_time: u64, ttl: u64) -> bool {
        !self.is_static && (current_time - self.timestamp) > ttl
    }
    
    /// Actualizar timestamp
    pub fn update_timestamp(&mut self, timestamp: u64) {
        self.timestamp = timestamp;
    }
}

/// Tabla ARP
pub struct ArpTable {
    pub entries: BTreeMap<IpAddress, ArpEntry>,
    pub default_ttl: u64,
    pub max_entries: usize,
}

impl ArpTable {
    /// Crear nueva tabla ARP
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            default_ttl: 300, // 5 minutos
            max_entries: 1024,
        }
    }
    
    /// Agregar entrada a la tabla
    pub fn add_entry(&mut self, ip: IpAddress, mac: MacAddress, is_static: bool, timestamp: u64) {
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(&ip) {
            // Eliminar entrada más antigua si la tabla está llena
            self.remove_oldest_entry();
        }
        
        let mut entry = ArpEntry::new(ip, mac, is_static);
        entry.update_timestamp(timestamp);
        self.entries.insert(ip, entry);
    }
    
    /// Buscar dirección MAC por IP
    pub fn lookup(&self, ip: IpAddress) -> Option<MacAddress> {
        self.entries.get(&ip).map(|entry| entry.mac)
    }
    
    /// Eliminar entrada de la tabla
    pub fn remove_entry(&mut self, ip: IpAddress) -> bool {
        self.entries.remove(&ip).is_some()
    }
    
    /// Limpiar entradas expiradas
    pub fn cleanup_expired(&mut self, current_time: u64) {
        let expired_ips: Vec<IpAddress> = self.entries
            .iter()
            .filter(|(_, entry)| entry.is_expired(current_time, self.default_ttl))
            .map(|(ip, _)| *ip)
            .collect();
        
        for ip in expired_ips {
            self.entries.remove(&ip);
        }
    }
    
    /// Eliminar entrada más antigua
    fn remove_oldest_entry(&mut self) {
        if let Some((oldest_ip, _)) = self.entries
            .iter()
            .min_by_key(|(_, entry)| entry.timestamp)
            .map(|(ip, _)| (*ip, ()))
        {
            self.entries.remove(&oldest_ip);
        }
    }
    
    /// Obtener número de entradas
    pub fn size(&self) -> usize {
        self.entries.len()
    }
    
    /// Verificar si la tabla está llena
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.max_entries
    }
    
    /// Obtener todas las entradas
    pub fn get_entries(&self) -> Vec<ArpEntry> {
        self.entries.values().cloned().collect()
    }
}

/// Procesador ARP
pub struct ArpProcessor {
    pub table: ArpTable,
    pub local_mac: MacAddress,
    pub local_ip: IpAddress,
    pub stats: ArpStats,
}

impl ArpProcessor {
    /// Crear nuevo procesador ARP
    pub fn new(local_mac: MacAddress, local_ip: IpAddress) -> Self {
        Self {
            table: ArpTable::new(),
            local_mac,
            local_ip,
            stats: ArpStats::new(),
        }
    }
    
    /// Procesar paquete ARP recibido
    pub fn process_packet(&mut self, packet: ArpPacket, timestamp: u64) -> Result<Option<ArpPacket>, NetworkError> {
        self.stats.packets_received += 1;
        
        match packet.get_operation() {
            ArpOperation::Request => {
                self.stats.requests_received += 1;
                
                // Verificar si la solicitud es para nosotros
                if packet.get_target_ip() == self.local_ip {
                    // Responder con nuestra dirección MAC
                    let reply = ArpPacket::reply(
                        self.local_mac,
                        self.local_ip,
                        packet.get_sender_mac(),
                        packet.get_sender_ip(),
                    );
                    
                    self.stats.replies_sent += 1;
                    return Ok(Some(reply));
                }
                
                Ok(None)
            },
            ArpOperation::Reply => {
                self.stats.replies_received += 1;
                
                // Agregar entrada a la tabla ARP
                self.table.add_entry(
                    packet.get_sender_ip(),
                    packet.get_sender_mac(),
                    false,
                    timestamp,
                );
                
                Ok(None)
            },
            _ => {
                self.stats.unknown_operations += 1;
                Ok(None)
            }
        }
    }
    
    /// Enviar solicitud ARP
    pub fn send_request(&mut self, target_ip: IpAddress) -> ArpPacket {
        let request = ArpPacket::request(
            self.local_mac,
            self.local_ip,
            target_ip,
        );
        
        self.stats.requests_sent += 1;
        request
    }
    
    /// Resolver dirección IP a MAC
    pub fn resolve(&mut self, ip: IpAddress, timestamp: u64) -> Option<MacAddress> {
        // Buscar en la tabla ARP
        if let Some(mac) = self.table.lookup(ip) {
            return Some(mac);
        }
        
        // Si no está en la tabla, enviar solicitud ARP
        let _request = self.send_request(ip);
        
        // En un sistema real, esperaríamos la respuesta
        // Por ahora, retornamos None
        None
    }
    
    /// Agregar entrada estática
    pub fn add_static_entry(&mut self, ip: IpAddress, mac: MacAddress, timestamp: u64) {
        self.table.add_entry(ip, mac, true, timestamp);
    }
    
    /// Limpiar tabla ARP
    pub fn cleanup(&mut self, current_time: u64) {
        self.table.cleanup_expired(current_time);
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> &ArpStats {
        &self.stats
    }
    
    /// Obtener tabla ARP
    pub fn get_table(&self) -> &ArpTable {
        &self.table
    }
}

/// Estadísticas ARP
#[derive(Debug, Clone)]
pub struct ArpStats {
    pub packets_received: u64,
    pub packets_sent: u64,
    pub requests_received: u64,
    pub requests_sent: u64,
    pub replies_received: u64,
    pub replies_sent: u64,
    pub unknown_operations: u64,
    pub table_hits: u64,
    pub table_misses: u64,
}

impl ArpStats {
    pub fn new() -> Self {
        Self {
            packets_received: 0,
            packets_sent: 0,
            requests_received: 0,
            requests_sent: 0,
            replies_received: 0,
            replies_sent: 0,
            unknown_operations: 0,
            table_hits: 0,
            table_misses: 0,
        }
    }
    
    pub fn increment_received(&mut self) {
        self.packets_received += 1;
    }
    
    pub fn increment_sent(&mut self) {
        self.packets_sent += 1;
    }
    
    pub fn increment_table_hit(&mut self) {
        self.table_hits += 1;
    }
    
    pub fn increment_table_miss(&mut self) {
        self.table_misses += 1;
    }
}