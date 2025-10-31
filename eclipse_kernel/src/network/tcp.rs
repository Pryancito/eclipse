//! Implementación del protocolo TCP (Transmission Control Protocol)
//!
//! Incluye gestión de conexiones, control de flujo, y estados de conexión

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use super::ip::IpAddress;
use super::NetworkError;

/// Puerto TCP
pub type TcpPort = u16;

/// Dirección de socket TCP
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TcpAddress {
    pub ip: IpAddress,
    pub port: TcpPort,
}

impl TcpAddress {
    pub fn new(ip: IpAddress, port: TcpPort) -> Self {
        Self { ip, port }
    }
}

impl fmt::Display for TcpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// Estados de conexión TCP
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl TcpState {
    /// Verificar si el estado permite envío de datos
    pub fn can_send_data(&self) -> bool {
        matches!(self, TcpState::Established | TcpState::CloseWait)
    }

    /// Verificar si el estado permite recepción de datos
    pub fn can_receive_data(&self) -> bool {
        matches!(
            self,
            TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
        )
    }

    /// Verificar si la conexión está cerrada
    pub fn is_closed(&self) -> bool {
        matches!(self, TcpState::Closed | TcpState::TimeWait)
    }
}

/// Flags TCP
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TcpFlags {
    pub fin: bool,
    pub syn: bool,
    pub rst: bool,
    pub psh: bool,
    pub ack: bool,
    pub urg: bool,
}

impl TcpFlags {
    pub fn new() -> Self {
        Self {
            fin: false,
            syn: false,
            rst: false,
            psh: false,
            ack: false,
            urg: false,
        }
    }

    pub fn to_u8(&self) -> u8 {
        let mut flags = 0u8;
        if self.fin {
            flags |= 0x01;
        }
        if self.syn {
            flags |= 0x02;
        }
        if self.rst {
            flags |= 0x04;
        }
        if self.psh {
            flags |= 0x08;
        }
        if self.ack {
            flags |= 0x10;
        }
        if self.urg {
            flags |= 0x20;
        }
        flags
    }

    pub fn from_u8(flags: u8) -> Self {
        Self {
            fin: (flags & 0x01) != 0,
            syn: (flags & 0x02) != 0,
            rst: (flags & 0x04) != 0,
            psh: (flags & 0x08) != 0,
            ack: (flags & 0x10) != 0,
            urg: (flags & 0x20) != 0,
        }
    }
}

/// Cabecera TCP
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct TcpHeader {
    pub source_port: u16,
    pub dest_port: u16,
    pub sequence: u32,
    pub ack_sequence: u32,
    pub data_offset_flags: u16, // Data offset (4 bits) + Reserved (3 bits) + Flags (9 bits)
    pub window: u16,
    pub checksum: u16,
    pub urgent_ptr: u16,
}

impl TcpHeader {
    /// Crear nueva cabecera TCP
    pub fn new(
        source_port: TcpPort,
        dest_port: TcpPort,
        sequence: u32,
        ack_sequence: u32,
        flags: TcpFlags,
    ) -> Self {
        Self {
            source_port,
            dest_port,
            sequence,
            ack_sequence,
            data_offset_flags: ((5 << 12) | (flags.to_u8() as u16 & 0x1FF)),
            window: 65535,
            checksum: 0,
            urgent_ptr: 0,
        }
    }

    /// Obtener data offset
    pub fn get_data_offset(&self) -> u8 {
        ((self.data_offset_flags >> 12) & 0x0F) as u8
    }

    /// Obtener flags
    pub fn get_flags(&self) -> TcpFlags {
        TcpFlags::from_u8((self.data_offset_flags & 0x1FF) as u8)
    }

    /// Establecer flags
    pub fn set_flags(&mut self, flags: TcpFlags) {
        self.data_offset_flags = (self.data_offset_flags & 0xF000) | (flags.to_u8() as u16 & 0x1FF);
    }

    /// Calcular checksum TCP
    pub fn calculate_checksum(
        &self,
        source_ip: IpAddress,
        dest_ip: IpAddress,
        payload: &[u8],
    ) -> u16 {
        let mut sum: u32 = 0;

        // Pseudo-header
        sum += (source_ip.bytes[0] as u32) << 8 | (source_ip.bytes[1] as u32);
        sum += (source_ip.bytes[2] as u32) << 8 | (source_ip.bytes[3] as u32);
        sum += (dest_ip.bytes[0] as u32) << 8 | (dest_ip.bytes[1] as u32);
        sum += (dest_ip.bytes[2] as u32) << 8 | (dest_ip.bytes[3] as u32);
        sum += 6; // Protocolo TCP
        sum += (20 + payload.len()) as u32; // Longitud TCP

        // Cabecera TCP
        let header_bytes =
            unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, 20) };
        for i in (0..20).step_by(2) {
            if i + 1 < 20 {
                let word = ((header_bytes[i] as u16) << 8) | (header_bytes[i + 1] as u16);
                sum += word as u32;
            }
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
}

/// Segmento TCP
pub struct TcpSegment {
    pub header: TcpHeader,
    pub payload: Vec<u8>,
}

impl TcpSegment {
    /// Crear nuevo segmento TCP
    pub fn new(
        source_port: TcpPort,
        dest_port: TcpPort,
        sequence: u32,
        ack_sequence: u32,
        flags: TcpFlags,
        payload: Vec<u8>,
    ) -> Self {
        let mut header = TcpHeader::new(source_port, dest_port, sequence, ack_sequence, flags);
        header.set_checksum(IpAddress::zero(), IpAddress::zero(), &payload);

        Self { header, payload }
    }

    /// Serializar segmento a bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(20 + self.payload.len());

        // Serializar header
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&self.header as *const TcpHeader as *const u8, 20)
        };
        bytes.extend_from_slice(header_bytes);

        // Agregar payload
        bytes.extend_from_slice(&self.payload);

        bytes
    }

    /// Crear segmento desde bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 20 {
            return None;
        }

        let header = unsafe { *(data.as_ptr() as *const TcpHeader) };

        let payload = data[20..].to_vec();

        Some(Self { header, payload })
    }
}

/// Conexión TCP
pub struct TcpConnection {
    pub local_addr: TcpAddress,
    pub remote_addr: TcpAddress,
    pub state: TcpState,
    pub local_sequence: u32,
    pub remote_sequence: u32,
    pub local_ack: u32,
    pub remote_ack: u32,
    pub send_window: u16,
    pub receive_window: u16,
    pub send_buffer: VecDeque<u8>,
    pub receive_buffer: VecDeque<u8>,
    pub retransmit_queue: VecDeque<TcpSegment>,
    pub retransmit_timeout: u64,
    pub last_activity: u64,
}

impl TcpConnection {
    /// Crear nueva conexión TCP
    pub fn new(local_addr: TcpAddress, remote_addr: TcpAddress) -> Self {
        Self {
            local_addr,
            remote_addr,
            state: TcpState::Closed,
            local_sequence: 0,
            remote_sequence: 0,
            local_ack: 0,
            remote_ack: 0,
            send_window: 65535,
            receive_window: 65535,
            send_buffer: VecDeque::new(),
            receive_buffer: VecDeque::new(),
            retransmit_queue: VecDeque::new(),
            retransmit_timeout: 1000, // 1 segundo
            last_activity: 0,
        }
    }

    /// Iniciar conexión (enviar SYN)
    pub fn connect(&mut self) -> Result<TcpSegment, NetworkError> {
        if self.state != TcpState::Closed {
            return Err(NetworkError::ProtocolError);
        }

        self.state = TcpState::SynSent;
        self.local_sequence = self.generate_initial_sequence();

        let flags = TcpFlags {
            syn: true,
            ..TcpFlags::new()
        };

        let segment = TcpSegment::new(
            self.local_addr.port,
            self.remote_addr.port,
            self.local_sequence,
            0,
            flags,
            Vec::new(),
        );

        self.local_sequence += 1;
        Ok(segment)
    }

    /// Aceptar conexión (enviar SYN+ACK)
    pub fn accept(&mut self, syn_segment: &TcpSegment) -> Result<TcpSegment, NetworkError> {
        if self.state != TcpState::Listen {
            return Err(NetworkError::ProtocolError);
        }

        self.state = TcpState::SynReceived;
        self.remote_addr = TcpAddress::new(IpAddress::zero(), syn_segment.header.source_port);
        self.remote_sequence = syn_segment.header.sequence;
        self.local_sequence = self.generate_initial_sequence();
        self.local_ack = syn_segment.header.sequence + 1;

        let flags = TcpFlags {
            syn: true,
            ack: true,
            ..TcpFlags::new()
        };

        let segment = TcpSegment::new(
            self.local_addr.port,
            self.remote_addr.port,
            self.local_sequence,
            self.local_ack,
            flags,
            Vec::new(),
        );

        self.local_sequence += 1;
        Ok(segment)
    }

    /// Enviar datos
    pub fn send_data(&mut self, data: &[u8]) -> Result<TcpSegment, NetworkError> {
        if !self.state.can_send_data() {
            return Err(NetworkError::ProtocolError);
        }

        // Agregar datos al buffer de envío
        self.send_buffer.extend(data.iter());

        // Crear segmento con datos
        let mut payload = Vec::new();
        let send_size = core::cmp::min(data.len(), self.send_window as usize);
        payload.extend_from_slice(&data[..send_size]);

        let flags = TcpFlags {
            psh: true,
            ack: true,
            ..TcpFlags::new()
        };

        let segment = TcpSegment::new(
            self.local_addr.port,
            self.remote_addr.port,
            self.local_sequence,
            self.local_ack,
            flags,
            payload,
        );

        self.local_sequence += send_size as u32;
        Ok(segment)
    }

    /// Recibir datos
    pub fn receive_data(&mut self, segment: &TcpSegment) -> Result<Vec<u8>, NetworkError> {
        if !self.state.can_receive_data() {
            return Err(NetworkError::ProtocolError);
        }

        // Verificar secuencia
        if segment.header.sequence != self.remote_ack {
            return Err(NetworkError::ProtocolError);
        }

        // Actualizar ACK
        self.remote_ack = segment.header.sequence + segment.payload.len() as u32;

        // Agregar datos al buffer de recepción
        self.receive_buffer.extend(segment.payload.iter());

        // Extraer datos del buffer
        let mut data = Vec::new();
        while let Some(byte) = self.receive_buffer.pop_front() {
            data.push(byte);
        }

        Ok(data)
    }

    /// Cerrar conexión (enviar FIN)
    pub fn close(&mut self) -> Result<TcpSegment, NetworkError> {
        if !self.state.can_send_data() {
            return Err(NetworkError::ProtocolError);
        }

        self.state = TcpState::FinWait1;

        let flags = TcpFlags {
            fin: true,
            ack: true,
            ..TcpFlags::new()
        };

        let segment = TcpSegment::new(
            self.local_addr.port,
            self.remote_addr.port,
            self.local_sequence,
            self.local_ack,
            flags,
            Vec::new(),
        );

        self.local_sequence += 1;
        Ok(segment)
    }

    /// Procesar segmento recibido
    pub fn process_segment(
        &mut self,
        segment: &TcpSegment,
    ) -> Result<Option<TcpSegment>, NetworkError> {
        let flags = segment.header.get_flags();

        match self.state {
            TcpState::SynSent => {
                if flags.syn && flags.ack {
                    self.state = TcpState::Established;
                    self.remote_sequence = segment.header.sequence;
                    self.local_ack = segment.header.sequence + 1;

                    // Enviar ACK
                    let ack_flags = TcpFlags {
                        ack: true,
                        ..TcpFlags::new()
                    };

                    let ack_segment = TcpSegment::new(
                        self.local_addr.port,
                        self.remote_addr.port,
                        self.local_sequence,
                        self.local_ack,
                        ack_flags,
                        Vec::new(),
                    );

                    return Ok(Some(ack_segment));
                }
            }
            TcpState::Established => {
                if flags.fin {
                    self.state = TcpState::CloseWait;
                    self.remote_sequence = segment.header.sequence;
                    self.local_ack = segment.header.sequence + 1;

                    // Enviar ACK
                    let ack_flags = TcpFlags {
                        ack: true,
                        ..TcpFlags::new()
                    };

                    let ack_segment = TcpSegment::new(
                        self.local_addr.port,
                        self.remote_addr.port,
                        self.local_sequence,
                        self.local_ack,
                        ack_flags,
                        Vec::new(),
                    );

                    return Ok(Some(ack_segment));
                } else if flags.ack && !segment.payload.is_empty() {
                    // Procesar datos
                    let _data = self.receive_data(segment)?;
                    return Ok(None);
                }
            }
            _ => {
                // Otros estados
            }
        }

        Ok(None)
    }

    /// Generar número de secuencia inicial
    fn generate_initial_sequence(&self) -> u32 {
        // Implementación simplificada - en un sistema real usaría un generador criptográfico
        (self.local_addr.port as u32) << 16 | (self.remote_addr.port as u32)
    }

    /// Verificar si la conexión está activa
    pub fn is_active(&self) -> bool {
        !self.state.is_closed()
    }

    /// Obtener estadísticas de la conexión
    pub fn get_stats(&self) -> TcpConnectionStats {
        TcpConnectionStats {
            local_addr: self.local_addr,
            remote_addr: self.remote_addr,
            state: self.state,
            send_buffer_size: self.send_buffer.len(),
            receive_buffer_size: self.receive_buffer.len(),
            send_window: self.send_window,
            receive_window: self.receive_window,
        }
    }
}

/// Estadísticas de conexión TCP
#[derive(Debug)]
pub struct TcpConnectionStats {
    pub local_addr: TcpAddress,
    pub remote_addr: TcpAddress,
    pub state: TcpState,
    pub send_buffer_size: usize,
    pub receive_buffer_size: usize,
    pub send_window: u16,
    pub receive_window: u16,
}

/// Socket TCP
pub struct TcpSocket {
    pub connection: TcpConnection,
    pub is_listening: bool,
    pub backlog: usize,
}

impl TcpSocket {
    /// Crear nuevo socket TCP
    pub fn new(local_addr: TcpAddress) -> Self {
        Self {
            connection: TcpConnection::new(local_addr, TcpAddress::new(IpAddress::zero(), 0)),
            is_listening: false,
            backlog: 5,
        }
    }

    /// Escuchar en el socket
    pub fn listen(&mut self) -> Result<(), NetworkError> {
        if self.connection.state != TcpState::Closed {
            return Err(NetworkError::ProtocolError);
        }

        self.connection.state = TcpState::Listen;
        self.is_listening = true;
        Ok(())
    }

    /// Conectar a una dirección remota
    pub fn connect(&mut self, remote_addr: TcpAddress) -> Result<TcpSegment, NetworkError> {
        self.connection.remote_addr = remote_addr;
        self.connection.connect()
    }

    /// Aceptar conexión entrante
    pub fn accept(&mut self, syn_segment: &TcpSegment) -> Result<TcpSegment, NetworkError> {
        if !self.is_listening {
            return Err(NetworkError::ProtocolError);
        }

        self.connection.accept(syn_segment)
    }

    /// Enviar datos
    pub fn send(&mut self, data: &[u8]) -> Result<TcpSegment, NetworkError> {
        self.connection.send_data(data)
    }

    /// Recibir datos
    pub fn receive(&mut self, segment: &TcpSegment) -> Result<Vec<u8>, NetworkError> {
        self.connection.receive_data(segment)
    }

    /// Cerrar socket
    pub fn close(&mut self) -> Result<TcpSegment, NetworkError> {
        self.connection.close()
    }

    /// Obtener estado del socket
    pub fn get_state(&self) -> TcpState {
        self.connection.state
    }

    /// Verificar si el socket está conectado
    pub fn is_connected(&self) -> bool {
        self.connection.state == TcpState::Established
    }

    /// Verificar si el socket está escuchando
    pub fn is_listening(&self) -> bool {
        self.is_listening && self.connection.state == TcpState::Listen
    }
}

/// Estadísticas TCP globales
#[derive(Debug, Clone)]
pub struct TcpStats {
    pub connections_active: u32,
    pub connections_total: u64,
    pub segments_sent: u64,
    pub segments_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub connection_errors: u64,
    pub checksum_errors: u64,
    pub retransmissions: u64,
}

impl TcpStats {
    pub fn new() -> Self {
        Self {
            connections_active: 0,
            connections_total: 0,
            segments_sent: 0,
            segments_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            connection_errors: 0,
            checksum_errors: 0,
            retransmissions: 0,
        }
    }
}
