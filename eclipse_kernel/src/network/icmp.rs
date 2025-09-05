//! Implementación del protocolo ICMP (Internet Control Message Protocol)
//! 
//! Protocolo para mensajes de control y diagnóstico de red

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::vec::Vec;

use super::ip::IpAddress;
use super::NetworkError;

/// Tipos de mensaje ICMP
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IcmpType {
    EchoReply = 0,
    DestinationUnreachable = 3,
    SourceQuench = 4,
    Redirect = 5,
    EchoRequest = 8,
    TimeExceeded = 11,
    ParameterProblem = 12,
    TimestampRequest = 13,
    TimestampReply = 14,
    Unknown = 255,
}

impl From<u8> for IcmpType {
    fn from(value: u8) -> Self {
        match value {
            0 => IcmpType::EchoReply,
            3 => IcmpType::DestinationUnreachable,
            4 => IcmpType::SourceQuench,
            5 => IcmpType::Redirect,
            8 => IcmpType::EchoRequest,
            11 => IcmpType::TimeExceeded,
            12 => IcmpType::ParameterProblem,
            13 => IcmpType::TimestampRequest,
            14 => IcmpType::TimestampReply,
            _ => IcmpType::Unknown,
        }
    }
}

/// Códigos ICMP
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IcmpCode {
    // Echo Reply/Request
    NoCode = 0,
    
    // Destination Unreachable
    NetUnreachable = 1,
    HostUnreachable = 2,
    ProtocolUnreachable = 3,
    PortUnreachable = 4,
    FragmentationNeeded = 5,
    SourceRouteFailed = 6,
    NetUnknown = 7,
    HostUnknown = 8,
    HostIsolated = 9,
    NetProhibited = 10,
    HostProhibited = 11,
    TOSNetUnreachable = 12,
    TOSHostUnreachable = 13,
    FilterProhibited = 14,
    HostPrecedenceViolation = 15,
    PrecedenceCutoff = 16,
    
    // Time Exceeded
    TTLExceeded = 17,
    FragmentReassemblyTimeExceeded = 18,
    
    Unknown = 255,
}

impl From<u8> for IcmpCode {
    fn from(value: u8) -> Self {
        match value {
            0 => IcmpCode::NoCode,
            1 => IcmpCode::HostUnreachable,
            2 => IcmpCode::ProtocolUnreachable,
            3 => IcmpCode::PortUnreachable,
            4 => IcmpCode::FragmentationNeeded,
            5 => IcmpCode::SourceRouteFailed,
            6 => IcmpCode::NetUnknown,
            7 => IcmpCode::HostUnknown,
            8 => IcmpCode::HostIsolated,
            9 => IcmpCode::NetProhibited,
            10 => IcmpCode::HostProhibited,
            11 => IcmpCode::TOSNetUnreachable,
            12 => IcmpCode::TOSHostUnreachable,
            13 => IcmpCode::FilterProhibited,
            14 => IcmpCode::HostPrecedenceViolation,
            15 => IcmpCode::PrecedenceCutoff,
            _ => IcmpCode::Unknown,
        }
    }
}

/// Cabecera ICMP
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct IcmpHeader {
    pub message_type: u8,
    pub code: u8,
    pub checksum: u16,
    pub identifier: u16,
    pub sequence_number: u16,
}

impl IcmpHeader {
    /// Crear nueva cabecera ICMP
    pub fn new(message_type: IcmpType, code: IcmpCode, identifier: u16, sequence_number: u16) -> Self {
        Self {
            message_type: message_type as u8,
            code: code as u8,
            checksum: 0, // Se calculará
            identifier,
            sequence_number,
        }
    }
    
    /// Obtener tipo de mensaje
    pub fn get_message_type(&self) -> IcmpType {
        IcmpType::from(self.message_type)
    }
    
    /// Obtener código
    pub fn get_code(&self) -> IcmpCode {
        IcmpCode::from(self.code)
    }
    
    /// Calcular checksum ICMP
    pub fn calculate_checksum(&self, payload: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        
        // Cabecera ICMP
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
    pub fn set_checksum(&mut self, payload: &[u8]) {
        self.checksum = 0;
        self.checksum = self.calculate_checksum(payload);
    }
    
    /// Verificar checksum
    pub fn is_checksum_valid(&self, payload: &[u8]) -> bool {
        self.calculate_checksum(payload) == 0
    }
}

/// Mensaje ICMP
pub struct IcmpMessage {
    pub header: IcmpHeader,
    pub payload: Vec<u8>,
}

impl IcmpMessage {
    /// Crear nuevo mensaje ICMP
    pub fn new(message_type: IcmpType, code: IcmpCode, identifier: u16, sequence_number: u16, payload: Vec<u8>) -> Self {
        let mut header = IcmpHeader::new(message_type, code, identifier, sequence_number);
        header.set_checksum(&payload);
        
        Self { header, payload }
    }
    
    /// Crear mensaje Echo Request (ping)
    pub fn echo_request(identifier: u16, sequence_number: u16, data: &[u8]) -> Self {
        Self::new(
            IcmpType::EchoRequest,
            IcmpCode::NoCode,
            identifier,
            sequence_number,
            data.to_vec(),
        )
    }
    
    /// Crear mensaje Echo Reply (pong)
    pub fn echo_reply(identifier: u16, sequence_number: u16, data: &[u8]) -> Self {
        Self::new(
            IcmpType::EchoReply,
            IcmpCode::NoCode,
            identifier,
            sequence_number,
            data.to_vec(),
        )
    }
    
    /// Crear mensaje Destination Unreachable
    pub fn destination_unreachable(code: IcmpCode, original_packet: &[u8]) -> Self {
        // ICMP incluye los primeros 8 bytes del paquete original
        let mut payload = Vec::new();
        payload.extend_from_slice(&original_packet[..core::cmp::min(8, original_packet.len())]);
        
        Self::new(
            IcmpType::DestinationUnreachable,
            code,
            0,
            0,
            payload,
        )
    }
    
    /// Crear mensaje Time Exceeded
    pub fn time_exceeded(code: IcmpCode, original_packet: &[u8]) -> Self {
        let mut payload = Vec::new();
        payload.extend_from_slice(&original_packet[..core::cmp::min(8, original_packet.len())]);
        
        Self::new(
            IcmpType::TimeExceeded,
            code,
            0,
            0,
            payload,
        )
    }
    
    /// Serializar mensaje a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.payload.len());
        
        // Serializar header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.header as *const IcmpHeader as *const u8, 8)
        };
        bytes.extend_from_slice(header_bytes);
        
        // Agregar payload
        bytes.extend_from_slice(&self.payload);
        
        bytes
    }
    
    /// Crear mensaje desde bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        let header = unsafe {
            *(data.as_ptr() as *const IcmpHeader)
        };
        
        let payload = data[8..].to_vec();
        
        // Verificar checksum
        if !header.is_checksum_valid(&payload) {
            return None;
        }
        
        Some(Self { header, payload })
    }
    
    /// Obtener tipo de mensaje
    pub fn get_message_type(&self) -> IcmpType {
        self.header.get_message_type()
    }
    
    /// Obtener código
    pub fn get_code(&self) -> IcmpCode {
        self.header.get_code()
    }
    
    /// Obtener identificador
    pub fn get_identifier(&self) -> u16 {
        self.header.identifier
    }
    
    /// Obtener número de secuencia
    pub fn get_sequence_number(&self) -> u16 {
        self.header.sequence_number
    }
    
    /// Obtener payload
    pub fn get_payload(&self) -> &[u8] {
        &self.payload
    }
    
    /// Verificar si es Echo Request
    pub fn is_echo_request(&self) -> bool {
        self.get_message_type() == IcmpType::EchoRequest
    }
    
    /// Verificar si es Echo Reply
    pub fn is_echo_reply(&self) -> bool {
        self.get_message_type() == IcmpType::EchoReply
    }
    
    /// Verificar si es mensaje de error
    pub fn is_error_message(&self) -> bool {
        matches!(
            self.get_message_type(),
            IcmpType::DestinationUnreachable | IcmpType::TimeExceeded | IcmpType::ParameterProblem
        )
    }
}

/// Procesador ICMP
pub struct IcmpProcessor {
    pub echo_identifier: u16,
    pub echo_sequence: u16,
    pub stats: IcmpStats,
}

impl IcmpProcessor {
    /// Crear nuevo procesador ICMP
    pub fn new() -> Self {
        Self {
            echo_identifier: 1,
            echo_sequence: 1,
            stats: IcmpStats::new(),
        }
    }
    
    /// Procesar mensaje ICMP recibido
    pub fn process_message(&mut self, message: IcmpMessage, source_ip: IpAddress) -> Result<Option<IcmpMessage>, NetworkError> {
        self.stats.messages_received += 1;
        
        match message.get_message_type() {
            IcmpType::EchoRequest => {
                // Responder con Echo Reply
                let reply = IcmpMessage::echo_reply(
                    message.get_identifier(),
                    message.get_sequence_number(),
                    message.get_payload(),
                );
                self.stats.echo_requests += 1;
                Ok(Some(reply))
            },
            IcmpType::EchoReply => {
                // Procesar respuesta de ping
                self.stats.echo_replies += 1;
                Ok(None)
            },
            IcmpType::DestinationUnreachable => {
                self.stats.destination_unreachable += 1;
                Ok(None)
            },
            IcmpType::TimeExceeded => {
                self.stats.time_exceeded += 1;
                Ok(None)
            },
            _ => {
                self.stats.unknown_messages += 1;
                Ok(None)
            }
        }
    }
    
    /// Enviar ping
    pub fn send_ping(&mut self, data: &[u8]) -> IcmpMessage {
        let message = IcmpMessage::echo_request(
            self.echo_identifier,
            self.echo_sequence,
            data,
        );
        
        self.echo_sequence += 1;
        self.stats.pings_sent += 1;
        
        message
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> &IcmpStats {
        &self.stats
    }
}

/// Estadísticas ICMP
#[derive(Debug, Clone)]
pub struct IcmpStats {
    pub messages_received: u64,
    pub messages_sent: u64,
    pub echo_requests: u64,
    pub echo_replies: u64,
    pub destination_unreachable: u64,
    pub time_exceeded: u64,
    pub parameter_problems: u64,
    pub redirects: u64,
    pub pings_sent: u64,
    pub unknown_messages: u64,
    pub checksum_errors: u64,
}

impl IcmpStats {
    pub fn new() -> Self {
        Self {
            messages_received: 0,
            messages_sent: 0,
            echo_requests: 0,
            echo_replies: 0,
            destination_unreachable: 0,
            time_exceeded: 0,
            parameter_problems: 0,
            redirects: 0,
            pings_sent: 0,
            unknown_messages: 0,
            checksum_errors: 0,
        }
    }
    
    pub fn increment_sent(&mut self) {
        self.messages_sent += 1;
    }
    
    pub fn increment_received(&mut self) {
        self.messages_received += 1;
    }
    
    pub fn increment_checksum_errors(&mut self) {
        self.checksum_errors += 1;
    }
}
