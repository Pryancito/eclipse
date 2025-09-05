//! Implementación del protocolo UDP (User Datagram Protocol)
//! 
//! Protocolo de transporte sin conexión, más simple que TCP

use alloc::vec::Vec;
use core::fmt;

use super::ip::IpAddress;
use super::NetworkError;

/// Puerto UDP
pub type UdpPort = u16;

/// Dirección de socket UDP
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UdpAddress {
    pub ip: IpAddress,
    pub port: UdpPort,
}

impl UdpAddress {
    pub fn new(ip: IpAddress, port: UdpPort) -> Self {
        Self { ip, port }
    }
}

impl fmt::Display for UdpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// Cabecera UDP
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UdpHeader {
    pub source_port: u16,
    pub dest_port: u16,
    pub length: u16,
    pub checksum: u16,
}

impl UdpHeader {
    /// Crear nueva cabecera UDP
    pub fn new(source_port: UdpPort, dest_port: UdpPort, length: u16) -> Self {
        Self {
            source_port,
            dest_port,
            length,
            checksum: 0, // Se calculará
        }
    }
    
    /// Calcular checksum UDP
    pub fn calculate_checksum(&self, source_ip: IpAddress, dest_ip: IpAddress, payload: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        
        // Pseudo-header
        sum += (source_ip.bytes[0] as u32) << 8 | (source_ip.bytes[1] as u32);
        sum += (source_ip.bytes[2] as u32) << 8 | (source_ip.bytes[3] as u32);
        sum += (dest_ip.bytes[0] as u32) << 8 | (dest_ip.bytes[1] as u32);
        sum += (dest_ip.bytes[2] as u32) << 8 | (dest_ip.bytes[3] as u32);
        sum += 17; // Protocolo UDP
        sum += self.length as u32;
        
        // Cabecera UDP
        let header_bytes = unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, 8)
        };
        for i in (0..8).step_by(2) {
            let word = ((header_bytes[i] as u16) << 8) | (header_bytes[i + 1] as u16);
            sum += word as u32;
        }
        
        // Payload
        for i in (0..payload.len()).step_by(2) {
            if i + 1 < payload.len() {
                let word = ((payload[i] as u16) << 8) | (payload[i + 1] as u16);
                sum += word as u32;
            } else {
                let word = (payload[i] as u16) << 8;
                sum += word as u32;
            }
        }
        
        // Sumar carry bits
        while (sum >> 16) != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        
        !(sum as u16)
    }
    
    /// Establecer checksum
    pub fn set_checksum(&mut self, source_ip: IpAddress, dest_ip: IpAddress, payload: &[u8]) {
        self.checksum = 0;
        self.checksum = self.calculate_checksum(source_ip, dest_ip, payload);
    }
    
    /// Verificar checksum
    pub fn is_checksum_valid(&self, source_ip: IpAddress, dest_ip: IpAddress, payload: &[u8]) -> bool {
        self.calculate_checksum(source_ip, dest_ip, payload) == 0
    }
}

/// Datagrama UDP
pub struct UdpDatagram {
    pub header: UdpHeader,
    pub payload: Vec<u8>,
}

impl UdpDatagram {
    /// Crear nuevo datagrama UDP
    pub fn new(source_port: UdpPort, dest_port: UdpPort, payload: Vec<u8>) -> Self {
        let length = 8 + payload.len() as u16;
        let mut header = UdpHeader::new(source_port, dest_port, length);
        header.set_checksum(IpAddress::zero(), IpAddress::zero(), &payload);
        
        Self { header, payload }
    }
    
    /// Serializar datagrama a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.payload.len());
        
        // Serializar header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.header as *const UdpHeader as *const u8, 8)
        };
        bytes.extend_from_slice(header_bytes);
        
        // Agregar payload
        bytes.extend_from_slice(&self.payload);
        
        bytes
    }
    
    /// Crear datagrama desde bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        let header = unsafe {
            *(data.as_ptr() as *const UdpHeader)
        };
        
        let payload = data[8..].to_vec();
        
        Some(Self { header, payload })
    }
    
    /// Obtener puerto origen
    pub fn get_source_port(&self) -> UdpPort {
        self.header.source_port
    }
    
    /// Obtener puerto destino
    pub fn get_dest_port(&self) -> UdpPort {
        self.header.dest_port
    }
    
    /// Obtener longitud
    pub fn get_length(&self) -> u16 {
        self.header.length
    }
    
    /// Obtener payload
    pub fn get_payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Socket UDP
pub struct UdpSocket {
    pub local_addr: UdpAddress,
    pub is_bound: bool,
    pub receive_buffer: Vec<UdpDatagram>,
    pub max_buffer_size: usize,
}

impl UdpSocket {
    /// Crear nuevo socket UDP
    pub fn new(local_addr: UdpAddress) -> Self {
        Self {
            local_addr,
            is_bound: false,
            receive_buffer: Vec::new(),
            max_buffer_size: 1024,
        }
    }
    
    /// Vincular socket a una dirección
    pub fn bind(&mut self) -> Result<(), NetworkError> {
        if self.is_bound {
            return Err(NetworkError::AddressInUse);
        }
        
        self.is_bound = true;
        Ok(())
    }
    
    /// Enviar datagrama
    pub fn send_to(&mut self, dest_addr: UdpAddress, data: &[u8]) -> Result<UdpDatagram, NetworkError> {
        if !self.is_bound {
            return Err(NetworkError::ProtocolError);
        }
        
        let datagram = UdpDatagram::new(
            self.local_addr.port,
            dest_addr.port,
            data.to_vec(),
        );
        
        Ok(datagram)
    }
    
    /// Recibir datagrama
    pub fn receive_from(&mut self) -> Result<(UdpDatagram, UdpAddress), NetworkError> {
        if !self.is_bound {
            return Err(NetworkError::ProtocolError);
        }
        
        if let Some(datagram) = self.receive_buffer.pop() {
            let source_addr = UdpAddress::new(IpAddress::zero(), datagram.get_source_port());
            Ok((datagram, source_addr))
        } else {
            Err(NetworkError::Timeout)
        }
    }
    
    /// Procesar datagrama recibido
    pub fn process_datagram(&mut self, datagram: UdpDatagram, source_addr: UdpAddress) -> Result<(), NetworkError> {
        if !self.is_bound {
            return Err(NetworkError::ProtocolError);
        }
        
        // Verificar si el datagrama es para este socket
        if datagram.get_dest_port() != self.local_addr.port {
            return Err(NetworkError::ProtocolError);
        }
        
        // Verificar checksum
        if !datagram.header.is_checksum_valid(source_addr.ip, self.local_addr.ip, &datagram.payload) {
            return Err(NetworkError::ProtocolError);
        }
        
        // Agregar al buffer de recepción
        if self.receive_buffer.len() < self.max_buffer_size {
            self.receive_buffer.push(datagram);
        }
        
        Ok(())
    }
    
    /// Verificar si hay datos disponibles
    pub fn has_data(&self) -> bool {
        !self.receive_buffer.is_empty()
    }
    
    /// Obtener número de datagramas en buffer
    pub fn get_buffer_size(&self) -> usize {
        self.receive_buffer.len()
    }
    
    /// Limpiar buffer de recepción
    pub fn clear_buffer(&mut self) {
        self.receive_buffer.clear();
    }
    
    /// Obtener dirección local
    pub fn get_local_addr(&self) -> UdpAddress {
        self.local_addr
    }
    
    /// Verificar si el socket está vinculado
    pub fn is_bound(&self) -> bool {
        self.is_bound
    }
    
    /// Obtener estadísticas del socket
    pub fn get_stats(&self) -> UdpSocketStats {
        UdpSocketStats {
            local_addr: self.local_addr,
            is_bound: self.is_bound,
            buffer_size: self.receive_buffer.len(),
            max_buffer_size: self.max_buffer_size,
        }
    }
}

/// Estadísticas de socket UDP
#[derive(Debug)]
pub struct UdpSocketStats {
    pub local_addr: UdpAddress,
    pub is_bound: bool,
    pub buffer_size: usize,
    pub max_buffer_size: usize,
}

/// Estadísticas UDP globales
#[derive(Debug, Clone)]
pub struct UdpStats {
    pub sockets_active: u32,
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub checksum_errors: u64,
    pub buffer_overflows: u64,
    pub port_errors: u64,
}

impl UdpStats {
    pub fn new() -> Self {
        Self {
            sockets_active: 0,
            datagrams_sent: 0,
            datagrams_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            checksum_errors: 0,
            buffer_overflows: 0,
            port_errors: 0,
        }
    }
    
    pub fn increment_sent(&mut self, bytes: u64) {
        self.datagrams_sent += 1;
        self.bytes_sent += bytes;
    }
    
    pub fn increment_received(&mut self, bytes: u64) {
        self.datagrams_received += 1;
        self.bytes_received += bytes;
    }
    
    pub fn increment_checksum_errors(&mut self) {
        self.checksum_errors += 1;
    }
    
    pub fn increment_buffer_overflows(&mut self) {
        self.buffer_overflows += 1;
    }
    
    pub fn increment_port_errors(&mut self) {
        self.port_errors += 1;
    }
}