//! Implementación del protocolo IP (Internet Protocol)
//! 
//! Soporta IPv4 e IPv6 con funcionalidades básicas de routing y fragmentación

use alloc::vec::Vec;
use core::fmt;

/// Dirección IP (IPv4)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IpAddress {
    pub bytes: [u8; 4],
}

impl IpAddress {
    /// Crear nueva dirección IP
    pub fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self { bytes: [a, b, c, d] }
    }
    
    /// Crear desde array de bytes
    pub fn from_bytes(bytes: [u8; 4]) -> Self {
        Self { bytes }
    }
    
    /// Dirección de loopback (127.0.0.1)
    pub fn loopback() -> Self {
        Self::new(127, 0, 0, 1)
    }
    
    /// Dirección de broadcast (255.255.255.255)
    pub fn broadcast() -> Self {
        Self::new(255, 255, 255, 255)
    }
    
    /// Dirección cero (0.0.0.0)
    pub fn zero() -> Self {
        Self::new(0, 0, 0, 0)
    }
    
    /// Verificar si es dirección de loopback
    pub fn is_loopback(&self) -> bool {
        self.bytes[0] == 127
    }
    
    /// Verificar si es dirección de broadcast
    pub fn is_broadcast(&self) -> bool {
        self.bytes == [255, 255, 255, 255]
    }
    
    /// Verificar si es dirección privada
    pub fn is_private(&self) -> bool {
        // 10.0.0.0/8
        if self.bytes[0] == 10 {
            return true;
        }
        // 172.16.0.0/12
        if self.bytes[0] == 172 && self.bytes[1] >= 16 && self.bytes[1] <= 31 {
            return true;
        }
        // 192.168.0.0/16
        if self.bytes[0] == 192 && self.bytes[1] == 168 {
            return true;
        }
        false
    }
    
    /// Obtener máscara de red para una dirección
    pub fn get_netmask(&self) -> IpAddress {
        if self.is_private() {
            if self.bytes[0] == 10 {
                // 10.0.0.0/8
                Self::new(255, 0, 0, 0)
            } else if self.bytes[0] == 172 {
                // 172.16.0.0/12
                Self::new(255, 240, 0, 0)
            } else {
                // 192.168.0.0/16
                Self::new(255, 255, 0, 0)
            }
        } else {
            // Por defecto, clase C
            Self::new(255, 255, 255, 0)
        }
    }
}

impl fmt::Display for IpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3])
    }
}

/// Versión del protocolo IP
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IpVersion {
    IPv4 = 4,
    IPv6 = 6,
}

/// Protocolos de capa superior soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IpProtocol {
    ICMP = 1,
    TCP = 6,
    UDP = 17,
    Unknown = 0,
}

impl From<u8> for IpProtocol {
    fn from(value: u8) -> Self {
        match value {
            1 => IpProtocol::ICMP,
            6 => IpProtocol::TCP,
            17 => IpProtocol::UDP,
            _ => IpProtocol::Unknown,
        }
    }
}

/// Cabecera IP
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct IpHeader {
    pub version_ihl: u8,        // Versión (4 bits) + IHL (4 bits)
    pub tos: u8,                // Type of Service
    pub total_length: u16,      // Longitud total del paquete
    pub identification: u16,    // Identificación del paquete
    pub flags_fragment: u16,    // Flags (3 bits) + Fragment offset (13 bits)
    pub ttl: u8,                // Time To Live
    pub protocol: u8,           // Protocolo de capa superior
    pub checksum: u16,          // Checksum
    pub source: [u8; 4],        // Dirección IP origen
    pub destination: [u8; 4],   // Dirección IP destino
}

impl IpHeader {
    /// Crear nueva cabecera IP
    pub fn new(source: IpAddress, destination: IpAddress, protocol: IpProtocol, length: u16) -> Self {
        Self {
            version_ihl: 0x45,  // IPv4, IHL=5 (20 bytes)
            tos: 0,
            total_length: length,
            identification: 0,  // Se asignará dinámicamente
            flags_fragment: 0,  // No fragmentar
            ttl: 64,
            protocol: protocol as u8,
            checksum: 0,        // Se calculará
            source: source.bytes,
            destination: destination.bytes,
        }
    }
    
    /// Obtener versión IP
    pub fn get_version(&self) -> IpVersion {
        match (self.version_ihl >> 4) & 0x0F {
            4 => IpVersion::IPv4,
            6 => IpVersion::IPv6,
            _ => IpVersion::IPv4, // Por defecto IPv4
        }
    }
    
    /// Obtener IHL (Internet Header Length)
    pub fn get_ihl(&self) -> u8 {
        (self.version_ihl & 0x0F) * 4
    }
    
    /// Obtener flags
    pub fn get_flags(&self) -> u8 {
        (self.flags_fragment >> 13) as u8
    }
    
    /// Obtener fragment offset
    pub fn get_fragment_offset(&self) -> u16 {
        self.flags_fragment & 0x1FFF
    }
    
    /// Verificar si es fragmento
    pub fn is_fragment(&self) -> bool {
        self.get_fragment_offset() != 0 || (self.get_flags() & 0x01) != 0
    }
    
    /// Verificar si no fragmentar
    pub fn dont_fragment(&self) -> bool {
        (self.get_flags() & 0x02) != 0
    }
    
    /// Verificar si más fragmentos
    pub fn more_fragments(&self) -> bool {
        (self.get_flags() & 0x01) != 0
    }
    
    /// Calcular checksum
    pub fn calculate_checksum(&self) -> u16 {
        let mut sum: u32 = 0;
        let header_bytes = unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, 20)
        };
        
        // Sumar palabras de 16 bits
        for i in (0..20).step_by(2) {
            if i + 1 < 20 {
                let word = ((header_bytes[i] as u16) << 8) | (header_bytes[i + 1] as u16);
                sum += word as u32;
            }
        }
        
        // Sumar carry bits
        while (sum >> 16) != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        
        !(sum as u16)
    }
    
    /// Validar checksum
    pub fn is_checksum_valid(&self) -> bool {
        self.calculate_checksum() == 0
    }
    
    /// Establecer checksum
    pub fn set_checksum(&mut self) {
        self.checksum = 0;
        self.checksum = self.calculate_checksum();
    }
}

/// Paquete IP completo
pub struct IpPacket {
    pub header: IpHeader,
    pub payload: Vec<u8>,
}

impl IpPacket {
    /// Crear nuevo paquete IP
    pub fn new(source: IpAddress, destination: IpAddress, protocol: IpProtocol, payload: Vec<u8>) -> Self {
        let total_length = 20 + payload.len() as u16;
        let mut header = IpHeader::new(source, destination, protocol, total_length);
        header.set_checksum();
        
        Self { header, payload }
    }
    
    /// Serializar paquete a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20 + self.payload.len());
        
        // Serializar header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.header as *const IpHeader as *const u8, 20)
        };
        bytes.extend_from_slice(header_bytes);
        
        // Agregar payload
        bytes.extend_from_slice(&self.payload);
        
        bytes
    }
    
    /// Crear paquete desde bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 20 {
            return None;
        }
        
        let header = unsafe {
            *(data.as_ptr() as *const IpHeader)
        };
        
        if !header.is_checksum_valid() {
            return None;
        }
        
        let payload = data[20..].to_vec();
        
        Some(Self { header, payload })
    }
    
    /// Obtener dirección origen
    pub fn get_source(&self) -> IpAddress {
        IpAddress::from_bytes(self.header.source)
    }
    
    /// Obtener dirección destino
    pub fn get_destination(&self) -> IpAddress {
        IpAddress::from_bytes(self.header.destination)
    }
    
    /// Obtener protocolo
    pub fn get_protocol(&self) -> IpProtocol {
        IpProtocol::from(self.header.protocol)
    }
    
    /// Obtener longitud total
    pub fn get_total_length(&self) -> u16 {
        self.header.total_length
    }
    
    /// Obtener TTL
    pub fn get_ttl(&self) -> u8 {
        self.header.ttl
    }
    
    /// Decrementar TTL
    pub fn decrement_ttl(&mut self) -> bool {
        if self.header.ttl > 0 {
            self.header.ttl -= 1;
            self.header.set_checksum();
            true
        } else {
            false
        }
    }
}

/// Estadísticas IP
#[derive(Debug, Clone)]
pub struct IpStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_dropped: u64,
    pub checksum_errors: u64,
    pub fragment_errors: u64,
    pub ttl_expired: u64,
}

impl IpStats {
    pub fn new() -> Self {
        Self {
            packets_sent: 0,
            packets_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            packets_dropped: 0,
            checksum_errors: 0,
            fragment_errors: 0,
            ttl_expired: 0,
        }
    }
    
    pub fn increment_sent(&mut self, bytes: u64) {
        self.packets_sent += 1;
        self.bytes_sent += bytes;
    }
    
    pub fn increment_received(&mut self, bytes: u64) {
        self.packets_received += 1;
        self.bytes_received += bytes;
    }
    
    pub fn increment_dropped(&mut self) {
        self.packets_dropped += 1;
    }
    
    pub fn increment_checksum_errors(&mut self) {
        self.checksum_errors += 1;
    }
    
    pub fn increment_fragment_errors(&mut self) {
        self.fragment_errors += 1;
    }
    
    pub fn increment_ttl_expired(&mut self) {
        self.ttl_expired += 1;
    }
}
