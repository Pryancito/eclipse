//! Implementación de anillos de transferencia XHCI
//! 
//! Este módulo maneja los Transfer Ring Buffers (TRBs) y las transferencias USB

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::mem::size_of;

/// Tipos de TRB (Transfer Request Block)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrbType {
    // Transfer TRBs
    Normal = 1,
    Setup = 2,
    Data = 3,
    Status = 4,
    Isoch = 5,
    Link = 6,
    EventData = 7,
    NoOp = 8,
    
    // Command TRBs
    EnableSlot = 9,
    DisableSlot = 10,
    AddressDevice = 11,
    ConfigureEndpoint = 12,
    EvaluateContext = 13,
    ResetEndpoint = 14,
    StopEndpoint = 15,
    SetTrDequeuePointer = 16,
    ResetDevice = 17,
    ForceEvent = 18,
    NegotiateBandwidth = 19,
    SetLatencyToleranceValue = 20,
    GetPortBandwidth = 21,
    ForceHeader = 22,
    NoOpCommand = 23,
    
    // Event TRBs
    TransferEvent = 32,
    CommandCompletion = 33,
    PortStatusChange = 34,
    BandwidthRequest = 35,
    Doorbell = 36,
    HostController = 37,
    DeviceNotification = 38,
    MfindexWrap = 39,
}

/// TRB genérico (128 bits = 16 bytes)
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    /// Crea un TRB vacío
    pub fn new() -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: 0,
        }
    }

    /// Crea un TRB con valores específicos
    pub fn with_values(parameter: u64, status: u32, control: u32) -> Self {
        Self {
            parameter,
            status,
            control,
        }
    }

    /// Obtiene el tipo de TRB
    pub fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }

    /// Establece el tipo de TRB
    pub fn set_trb_type(&mut self, trb_type: TrbType) {
        self.control = (self.control & !(0x3F << 10)) | ((trb_type as u32) << 10);
    }

    /// Obtiene el bit de ciclo
    pub fn cycle_bit(&self) -> bool {
        (self.control & 0x01) != 0
    }

    /// Establece el bit de ciclo
    pub fn set_cycle_bit(&mut self, cycle: bool) {
        if cycle {
            self.control |= 0x01;
        } else {
            self.control &= !0x01;
        }
    }

    /// Convierte el TRB a u128 para almacenamiento
    pub fn to_u128(&self) -> u128 {
        let param = self.parameter as u128;
        let status = (self.status as u128) << 64;
        let control = (self.control as u128) << 96;
        param | status | control
    }

    /// Crea un TRB desde u128
    pub fn from_u128(value: u128) -> Self {
        Self {
            parameter: (value & 0xFFFF_FFFF_FFFF_FFFF) as u64,
            status: ((value >> 64) & 0xFFFF_FFFF) as u32,
            control: ((value >> 96) & 0xFFFF_FFFF) as u32,
        }
    }
}

/// TRB de Setup para transferencias de control
#[repr(C, align(16))]
pub struct SetupStageTrb {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    trb_transfer_length: u32,
    control: u32,
}

impl SetupStageTrb {
    pub fn new(
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        transfer_type: u8,
        cycle_bit: bool,
    ) -> Self {
        let mut control = (TrbType::Setup as u32) << 10;
        control |= (transfer_type as u32) << 16; // TRT (Transfer Type)
        control |= 1 << 6; // IDT (Immediate Data)
        
        if cycle_bit {
            control |= 0x01;
        }
        
        Self {
            request_type,
            request,
            value,
            index,
            length,
            trb_transfer_length: 8, // Setup siempre son 8 bytes
            control,
        }
    }

    pub fn to_trb(&self) -> Trb {
        let parameter = 
            (self.request_type as u64) |
            ((self.request as u64) << 8) |
            ((self.value as u64) << 16) |
            ((self.index as u64) << 32) |
            ((self.length as u64) << 48);
        
        Trb::with_values(parameter, self.trb_transfer_length, self.control)
    }
}

/// TRB de Data para transferencias de control
#[repr(C, align(16))]
pub struct DataStageTrb {
    data_buffer_pointer: u64,
    trb_transfer_length: u32,
    control: u32,
}

impl DataStageTrb {
    pub fn new(
        data_buffer: u64,
        length: u32,
        direction_in: bool,
        cycle_bit: bool,
    ) -> Self {
        let mut control = (TrbType::Data as u32) << 10;
        
        if direction_in {
            control |= 1 << 16; // DIR = 1 (IN)
        }
        
        if cycle_bit {
            control |= 0x01;
        }
        
        Self {
            data_buffer_pointer: data_buffer,
            trb_transfer_length: length,
            control,
        }
    }

    pub fn to_trb(&self) -> Trb {
        Trb::with_values(
            self.data_buffer_pointer,
            self.trb_transfer_length,
            self.control
        )
    }
}

/// TRB de Status para transferencias de control
#[repr(C, align(16))]
pub struct StatusStageTrb {
    control: u32,
}

impl StatusStageTrb {
    pub fn new(direction_in: bool, cycle_bit: bool, interrupt_on_completion: bool) -> Self {
        let mut control = (TrbType::Status as u32) << 10;
        
        if direction_in {
            control |= 1 << 16; // DIR = 1 (IN)
        }
        
        if interrupt_on_completion {
            control |= 1 << 5; // IOC (Interrupt On Completion)
        }
        
        if cycle_bit {
            control |= 0x01;
        }
        
        Self { control }
    }

    pub fn to_trb(&self) -> Trb {
        Trb::with_values(0, 0, self.control)
    }
}

/// TRB de Link para conectar segmentos de anillos
#[repr(C, align(16))]
pub struct LinkTrb {
    ring_segment_pointer: u64,
    control: u32,
}

impl LinkTrb {
    pub fn new(next_segment: u64, toggle_cycle: bool, cycle_bit: bool) -> Self {
        let mut control = (TrbType::Link as u32) << 10;
        
        if toggle_cycle {
            control |= 1 << 1; // TC (Toggle Cycle)
        }
        
        if cycle_bit {
            control |= 0x01;
        }
        
        Self {
            ring_segment_pointer: next_segment,
            control,
        }
    }

    pub fn to_trb(&self) -> Trb {
        Trb::with_values(self.ring_segment_pointer, 0, self.control)
    }
}

/// Anillo de transferencia genérico (mejorado con link TRB automático, basado en Redox)
#[repr(C, align(64))]
pub struct TransferRing {
    trbs: Vec<Trb>,
    capacity: usize,
    enqueue_index: usize,
    dequeue_index: usize,
    cycle_bit: bool,
    physical_address: u64,
    use_link_trb: bool,  // Si true, el último TRB es un Link TRB
}

impl TransferRing {
    /// Crea un nuevo anillo de transferencia
    /// 
    /// Si `use_link_trb` es true, el anillo usará un Link TRB al final
    /// para wrap-around automático (como en Redox)
    pub fn new(capacity: usize) -> Self {
        Self::new_with_link(capacity, true)
    }
    
    /// Crea un anillo con control explícito de Link TRB
    pub fn new_with_link(capacity: usize, use_link_trb: bool) -> Self {
        let mut trbs = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            trbs.push(Trb::new());
        }
        
        let physical_address = trbs.as_ptr() as u64;
        
        let mut ring = Self {
            trbs,
            capacity,
            enqueue_index: 0,
            dequeue_index: 0,
            cycle_bit: true,
            physical_address,
            use_link_trb,
        };
        
        // Si usamos Link TRB, configurar el último TRB como Link
        if use_link_trb && capacity > 0 {
            let last_index = capacity - 1;
            let link_trb = LinkTrb::new(ring.physical_address, true, ring.cycle_bit);
            ring.trbs[last_index] = link_trb.to_trb();
        }
        
        ring
    }

    /// Obtiene la dirección física del anillo
    pub fn physical_address(&self) -> u64 {
        self.physical_address
    }

    /// Obtiene el siguiente índice para enqueue (estilo Redox)
    fn next_enqueue_index(&mut self) -> usize {
        let i = self.enqueue_index;
        self.enqueue_index += 1;
        
        // Si usamos Link TRB, el último slot está reservado para el Link
        let effective_capacity = if self.use_link_trb {
            self.capacity - 1
        } else {
            self.capacity
        };
        
        // Si llegamos al final, wrap around
        if self.enqueue_index >= effective_capacity {
            self.enqueue_index = 0;
            
            if self.use_link_trb {
                // Actualizar el Link TRB con el nuevo cycle bit
                let link_index = self.capacity - 1;
                let link_trb = LinkTrb::new(self.physical_address, true, self.cycle_bit);
                self.trbs[link_index] = link_trb.to_trb();
                self.cycle_bit = !self.cycle_bit;
            } else {
                self.cycle_bit = !self.cycle_bit;
            }
        }
        
        i
    }
    
    /// Encola un TRB en el anillo (mejorado estilo Redox)
    pub fn enqueue(&mut self, mut trb: Trb) -> Result<(), &'static str> {
        if self.is_full() {
            return Err("Transfer ring full");
        }

        // Obtener siguiente índice y actualizar el ring
        let index = self.next_enqueue_index();
        
        // Establecer el bit de ciclo
        trb.set_cycle_bit(self.cycle_bit);
        
        // Escribir el TRB en el anillo
        self.trbs[index] = trb;
        
        Ok(())
    }
    
    /// Obtiene el próximo TRB y su cycle bit (estilo Redox)
    pub fn next(&mut self) -> (&mut Trb, bool) {
        let index = self.next_enqueue_index();
        (&mut self.trbs[index], self.cycle_bit)
    }

    /// Desencola un TRB del anillo
    pub fn dequeue(&mut self) -> Option<Trb> {
        if self.is_empty() {
            return None;
        }

        let trb = self.trbs[self.dequeue_index];
        
        // Avanzar el índice de dequeue
        self.dequeue_index += 1;
        
        if self.dequeue_index >= self.capacity {
            self.dequeue_index = 0;
        }
        
        Some(trb)
    }

    /// Verifica si el anillo está vacío
    pub fn is_empty(&self) -> bool {
        self.enqueue_index == self.dequeue_index
    }

    /// Verifica si el anillo está lleno
    pub fn is_full(&self) -> bool {
        let effective_capacity = if self.use_link_trb {
            self.capacity - 1
        } else {
            self.capacity
        };
        
        let next_enqueue = (self.enqueue_index + 1) % effective_capacity;
        next_enqueue == self.dequeue_index
    }

    /// Obtiene el número de TRBs en el anillo
    pub fn len(&self) -> usize {
        if self.enqueue_index >= self.dequeue_index {
            self.enqueue_index - self.dequeue_index
        } else {
            self.capacity - self.dequeue_index + self.enqueue_index
        }
    }

    /// Limpia el anillo
    pub fn clear(&mut self) {
        self.enqueue_index = 0;
        self.dequeue_index = 0;
        self.cycle_bit = true;
        
        for trb in &mut self.trbs {
            *trb = Trb::new();
        }
    }
}

/// Constructor de transferencias de control USB
pub struct ControlTransferBuilder {
    setup_trb: Option<SetupStageTrb>,
    data_trb: Option<DataStageTrb>,
    status_trb: Option<StatusStageTrb>,
    cycle_bit: bool,
}

impl ControlTransferBuilder {
    pub fn new(cycle_bit: bool) -> Self {
        Self {
            setup_trb: None,
            data_trb: None,
            status_trb: None,
            cycle_bit,
        }
    }

    /// Agrega la etapa de setup
    pub fn setup(
        mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Self {
        // Determinar el tipo de transferencia
        let transfer_type = if length == 0 {
            0 // No Data Stage
        } else if (request_type & 0x80) != 0 {
            3 // IN Data Stage
        } else {
            2 // OUT Data Stage
        };
        
        self.setup_trb = Some(SetupStageTrb::new(
            request_type,
            request,
            value,
            index,
            length,
            transfer_type,
            self.cycle_bit,
        ));
        self
    }

    /// Agrega la etapa de datos
    pub fn data(mut self, buffer: u64, length: u32, direction_in: bool) -> Self {
        self.data_trb = Some(DataStageTrb::new(
            buffer,
            length,
            direction_in,
            self.cycle_bit,
        ));
        self
    }

    /// Agrega la etapa de status
    pub fn status(mut self, direction_in: bool, interrupt: bool) -> Self {
        self.status_trb = Some(StatusStageTrb::new(
            direction_in,
            self.cycle_bit,
            interrupt,
        ));
        self
    }

    /// Construye la transferencia y la encola en el anillo
    pub fn build_into(self, ring: &mut TransferRing) -> Result<(), &'static str> {
        if let Some(setup) = self.setup_trb {
            ring.enqueue(setup.to_trb())?;
        } else {
            return Err("Setup stage is required");
        }

        if let Some(data) = self.data_trb {
            ring.enqueue(data.to_trb())?;
        }

        if let Some(status) = self.status_trb {
            ring.enqueue(status.to_trb())?;
        } else {
            return Err("Status stage is required");
        }

        Ok(())
    }
}

/// Resultado de completación de un evento
#[derive(Debug, Clone, Copy)]
pub struct CompletionEvent {
    pub trb_pointer: u64,
    pub completion_code: u8,
    pub transfer_length: u32,
    pub slot_id: u8,
    pub endpoint_id: u8,
}

impl CompletionEvent {
    /// Parsea un TRB de evento de transferencia
    pub fn from_transfer_event_trb(trb: &Trb) -> Self {
        Self {
            trb_pointer: trb.parameter,
            completion_code: ((trb.status >> 24) & 0xFF) as u8,
            transfer_length: trb.status & 0xFFFFFF,
            slot_id: ((trb.control >> 24) & 0xFF) as u8,
            endpoint_id: ((trb.control >> 16) & 0x1F) as u8,
        }
    }

    /// Verifica si la transferencia fue exitosa
    pub fn is_success(&self) -> bool {
        self.completion_code == 1 // Success
    }

    /// Verifica si fue un short packet
    pub fn is_short_packet(&self) -> bool {
        self.completion_code == 13 // Short Packet
    }
}

/// Códigos de completación XHCI
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompletionCode {
    Invalid = 0,
    Success = 1,
    DataBufferError = 2,
    BabbleDetected = 3,
    UsbTransactionError = 4,
    TrbError = 5,
    StallError = 6,
    ResourceError = 7,
    BandwidthError = 8,
    NoSlotsAvailable = 9,
    InvalidStreamType = 10,
    SlotNotEnabled = 11,
    EndpointNotEnabled = 12,
    ShortPacket = 13,
    RingUnderrun = 14,
    RingOverrun = 15,
    VfEventRingFull = 16,
    ParameterError = 17,
    BandwidthOverrun = 18,
    ContextStateError = 19,
    NoPingResponse = 20,
    EventRingFull = 21,
    IncompatibleDevice = 22,
    MissedService = 23,
    CommandRingStopped = 24,
    CommandAborted = 25,
    Stopped = 26,
    StoppedLengthInvalid = 27,
    StoppedShortPacket = 28,
    MaxExitLatency = 29,
    IsochBufferOverrun = 31,
    EventLostError = 32,
    UndefinedError = 33,
    InvalidStreamId = 34,
    SecondaryBandwidthError = 35,
    SplitTransactionError = 36,
}

impl CompletionCode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Invalid,
            1 => Self::Success,
            2 => Self::DataBufferError,
            3 => Self::BabbleDetected,
            4 => Self::UsbTransactionError,
            5 => Self::TrbError,
            6 => Self::StallError,
            7 => Self::ResourceError,
            8 => Self::BandwidthError,
            9 => Self::NoSlotsAvailable,
            10 => Self::InvalidStreamType,
            11 => Self::SlotNotEnabled,
            12 => Self::EndpointNotEnabled,
            13 => Self::ShortPacket,
            14 => Self::RingUnderrun,
            15 => Self::RingOverrun,
            16 => Self::VfEventRingFull,
            17 => Self::ParameterError,
            18 => Self::BandwidthOverrun,
            19 => Self::ContextStateError,
            20 => Self::NoPingResponse,
            21 => Self::EventRingFull,
            22 => Self::IncompatibleDevice,
            23 => Self::MissedService,
            24 => Self::CommandRingStopped,
            25 => Self::CommandAborted,
            26 => Self::Stopped,
            27 => Self::StoppedLengthInvalid,
            28 => Self::StoppedShortPacket,
            29 => Self::MaxExitLatency,
            31 => Self::IsochBufferOverrun,
            32 => Self::EventLostError,
            33 => Self::UndefinedError,
            34 => Self::InvalidStreamId,
            35 => Self::SecondaryBandwidthError,
            36 => Self::SplitTransactionError,
            _ => Self::UndefinedError,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Invalid => "Invalid",
            Self::Success => "Success",
            Self::DataBufferError => "Data Buffer Error",
            Self::BabbleDetected => "Babble Detected",
            Self::UsbTransactionError => "USB Transaction Error",
            Self::TrbError => "TRB Error",
            Self::StallError => "Stall Error",
            Self::ResourceError => "Resource Error",
            Self::BandwidthError => "Bandwidth Error",
            Self::NoSlotsAvailable => "No Slots Available",
            Self::InvalidStreamType => "Invalid Stream Type",
            Self::SlotNotEnabled => "Slot Not Enabled",
            Self::EndpointNotEnabled => "Endpoint Not Enabled",
            Self::ShortPacket => "Short Packet",
            Self::RingUnderrun => "Ring Underrun",
            Self::RingOverrun => "Ring Overrun",
            Self::VfEventRingFull => "VF Event Ring Full",
            Self::ParameterError => "Parameter Error",
            Self::BandwidthOverrun => "Bandwidth Overrun",
            Self::ContextStateError => "Context State Error",
            Self::NoPingResponse => "No Ping Response",
            Self::EventRingFull => "Event Ring Full",
            Self::IncompatibleDevice => "Incompatible Device",
            Self::MissedService => "Missed Service",
            Self::CommandRingStopped => "Command Ring Stopped",
            Self::CommandAborted => "Command Aborted",
            Self::Stopped => "Stopped",
            Self::StoppedLengthInvalid => "Stopped - Length Invalid",
            Self::StoppedShortPacket => "Stopped - Short Packet",
            Self::MaxExitLatency => "Max Exit Latency",
            Self::IsochBufferOverrun => "Isochronous Buffer Overrun",
            Self::EventLostError => "Event Lost Error",
            Self::UndefinedError => "Undefined Error",
            Self::InvalidStreamId => "Invalid Stream ID",
            Self::SecondaryBandwidthError => "Secondary Bandwidth Error",
            Self::SplitTransactionError => "Split Transaction Error",
        }
    }
}

