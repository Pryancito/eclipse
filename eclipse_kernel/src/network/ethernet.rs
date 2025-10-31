//! Protocolo Ethernet
//! 
//! Implementa el protocolo Ethernet para la capa de enlace de datos

/// Dirección MAC (6 bytes)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacAddress {
    pub bytes: [u8; 6],
}

impl MacAddress {
    /// Crear una nueva dirección MAC
    pub const fn new(bytes: [u8; 6]) -> Self {
        Self { bytes }
    }
    
    /// Dirección MAC de broadcast
    pub const fn broadcast() -> Self {
        Self { bytes: [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF] }
    }
    
    /// Dirección MAC nula
    pub const fn null() -> Self {
        Self { bytes: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00] }
    }
    
    /// Verificar si es broadcast
    pub fn is_broadcast(&self) -> bool {
        self.bytes == [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
    }
    
    /// Verificar si es nula
    pub fn is_null(&self) -> bool {
        self.bytes == [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    }
}

/// Tipos de protocolo Ethernet
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EtherType {
    IPv4 = 0x0800,
    IPv6 = 0x86DD,
    ARP = 0x0806,
    VLAN = 0x8100,
    MPLS = 0x8847,
    Unknown = 0x0000,
}

impl From<u16> for EtherType {
    fn from(value: u16) -> Self {
        match value {
            0x0800 => EtherType::IPv4,
            0x86DD => EtherType::IPv6,
            0x0806 => EtherType::ARP,
            0x8100 => EtherType::VLAN, // VLAN-tagged frame
            0x88A8 => EtherType::VLAN, // 802.1ad VLAN
            0x8847 => EtherType::MPLS, // MPLS unicast
            0x8848 => EtherType::MPLS, // MPLS multicast
            _ => EtherType::Unknown,
        }
    }
}

impl From<EtherType> for u16 {
    fn from(ether_type: EtherType) -> Self {
        ether_type as u16
    }
}

/// Cabecera Ethernet
#[derive(Debug, Clone, Copy)]
pub struct EthernetHeader {
    pub destination: MacAddress,
    pub source: MacAddress,
    pub ether_type: EtherType,
}

impl EthernetHeader {
    /// Crear nueva cabecera Ethernet
    pub fn new(destination: MacAddress, source: MacAddress, ether_type: EtherType) -> Self {
        Self {
            destination,
            source,
            ether_type,
        }
    }
    
    /// Tamaño de la cabecera Ethernet
    pub const fn size() -> usize {
        14 // 6 + 6 + 2 bytes
    }
    
    /// Serializar la cabecera a bytes
    pub fn to_bytes(&self) -> [u8; 14] {
        let mut bytes = [0u8; 14];
        
        // Destino (6 bytes)
        bytes[0..6].copy_from_slice(&self.destination.bytes);
        
        // Origen (6 bytes)
        bytes[6..12].copy_from_slice(&self.source.bytes);
        
        // Tipo de protocolo (2 bytes, big-endian)
        let ether_type_bytes = (self.ether_type as u16).to_be_bytes();
        bytes[12..14].copy_from_slice(&ether_type_bytes);
        
        bytes
    }
    
    /// Deserializar cabecera desde bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 14 {
            return None;
        }
        
        let mut dest_bytes = [0u8; 6];
        dest_bytes.copy_from_slice(&bytes[0..6]);
        
        let mut src_bytes = [0u8; 6];
        src_bytes.copy_from_slice(&bytes[6..12]);
        
        let ether_type_bytes = [bytes[12], bytes[13]];
        let ether_type = u16::from_be_bytes(ether_type_bytes);
        
        Some(Self {
            destination: MacAddress::new(dest_bytes),
            source: MacAddress::new(src_bytes),
            ether_type: EtherType::from(ether_type),
        })
    }
}

/// Frame Ethernet completo
#[derive(Debug, Clone)]
pub struct EthernetFrame {
    pub header: EthernetHeader,
    pub payload: [u8; 1500], // MTU estándar
    pub payload_len: usize,
}

impl EthernetFrame {
    /// Crear nuevo frame Ethernet
    pub fn new(header: EthernetHeader, payload: &[u8]) -> Self {
        let mut frame_payload = [0u8; 1500];
        let payload_len = core::cmp::min(payload.len(), 1500);
        frame_payload[..payload_len].copy_from_slice(&payload[..payload_len]);
        
        Self {
            header,
            payload: frame_payload,
            payload_len,
        }
    }
    
    /// Tamaño total del frame
    pub fn total_size(&self) -> usize {
        EthernetHeader::size() + self.payload_len
    }
    
    /// Serializar frame completo a bytes
    pub fn to_bytes(&self) -> [u8; 1514] { // 14 + 1500
        let mut bytes = [0u8; 1514];
        
        // Cabecera
        let header_bytes = self.header.to_bytes();
        bytes[0..14].copy_from_slice(&header_bytes);
        
        // Payload
        bytes[14..14 + self.payload_len].copy_from_slice(&self.payload[..self.payload_len]);
        
        bytes
    }
    
    /// Deserializar frame desde bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 14 {
            return None;
        }
        
        let header = EthernetHeader::from_bytes(bytes)?;
        let payload_len = bytes.len() - 14;
        
        if payload_len > 1500 {
            return None;
        }
        
        let mut frame_payload = [0u8; 1500];
        frame_payload[..payload_len].copy_from_slice(&bytes[14..14 + payload_len]);
        
        Some(Self {
            header,
            payload: frame_payload,
            payload_len,
        })
    }
}

/// Gestor de protocolos Ethernet
pub struct EthernetManager {
    pub local_mac: MacAddress,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl EthernetManager {
    /// Crear nuevo gestor Ethernet
    pub fn new(local_mac: MacAddress) -> Self {
        Self {
            local_mac,
            frames_sent: 0,
            frames_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }
    
    /// Enviar frame Ethernet
    pub fn send_frame(&mut self, frame: &EthernetFrame) -> bool {
        // En una implementación real, esto enviaría el frame por el hardware
        self.frames_sent += 1;
        self.bytes_sent += frame.total_size() as u64;
        true
    }
    
    /// Recibir frame Ethernet
    pub fn receive_frame(&mut self, frame: &EthernetFrame) -> bool {
        // Verificar si el frame es para nosotros
        if !frame.header.destination.is_broadcast() && 
           frame.header.destination != self.local_mac {
            return false;
        }
        
        self.frames_received += 1;
        self.bytes_received += frame.total_size() as u64;
        true
    }
    
    /// Obtener estadísticas
    pub fn get_statistics(&self) -> (u64, u64, u64, u64) {
        (self.frames_sent, self.frames_received, self.bytes_sent, self.bytes_received)
    }
}
