//! Contextos de dispositivos y endpoints XHCI
//! Basado en la implementación de Redox OS

use alloc::vec::Vec;
use alloc::boxed::Box;

/// Slot State (estado del slot)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// El slot está deshabilitado o habilitado pero no configurado
    EnabledOrDisabled = 0,
    
    /// El slot ha recibido un comando Set Address
    Default = 1,
    
    /// El slot ha sido asignado una dirección USB
    Addressed = 2,
    
    /// El slot está completamente configurado
    Configured = 3,
}

impl SlotState {
    pub fn from_u8(value: u8) -> Self {
        match value & 0x1F {
            0 => Self::EnabledOrDisabled,
            1 => Self::Default,
            2 => Self::Addressed,
            3 => Self::Configured,
            _ => Self::EnabledOrDisabled,
        }
    }
}

/// Endpoint State (estado del endpoint)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    Disabled = 0,
    Running = 1,
    Halted = 2,
    Stopped = 3,
    Error = 4,
}

impl EndpointState {
    pub fn from_u8(value: u8) -> Self {
        match value & 0x7 {
            0 => Self::Disabled,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            4 => Self::Error,
            _ => Self::Disabled,
        }
    }
}

/// Tipo de endpoint
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    NotValid = 0,
    IsochOut = 1,
    BulkOut = 2,
    InterruptOut = 3,
    Control = 4,
    IsochIn = 5,
    BulkIn = 6,
    InterruptIn = 7,
}

/// Slot Context (simplificado para no_std)
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy)]
pub struct SlotContext {
    pub route_string: u32,
    pub speed_and_port: u32,
    pub tt_and_entries: u32,
    pub device_address: u32,
    _reserved: [u32; 4],
}

impl SlotContext {
    pub fn new() -> Self {
        Self {
            route_string: 0,
            speed_and_port: 0,
            tt_and_entries: 0,
            device_address: 0,
            _reserved: [0; 4],
        }
    }
    
    /// Obtiene el slot state
    pub fn slot_state(&self) -> SlotState {
        let state = ((self.device_address >> 27) & 0x1F) as u8;
        SlotState::from_u8(state)
    }
    
    /// Establece el slot state
    pub fn set_slot_state(&mut self, state: SlotState) {
        self.device_address = (self.device_address & !(0x1F << 27)) | ((state as u32) << 27);
    }
    
    /// Obtiene la velocidad del dispositivo
    pub fn speed(&self) -> u8 {
        ((self.speed_and_port >> 10) & 0xF) as u8
    }
    
    /// Establece la velocidad del dispositivo
    pub fn set_speed(&mut self, speed: u8) {
        self.speed_and_port = (self.speed_and_port & !(0xF << 10)) | ((speed as u32 & 0xF) << 10);
    }
    
    /// Obtiene el número de puerto root
    pub fn root_hub_port_number(&self) -> u8 {
        ((self.speed_and_port >> 16) & 0xFF) as u8
    }
    
    /// Establece el número de puerto root
    pub fn set_root_hub_port_number(&mut self, port: u8) {
        self.speed_and_port = (self.speed_and_port & !(0xFF << 16)) | ((port as u32) << 16);
    }
}

/// Endpoint Context (simplificado para no_std)
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy)]
pub struct EndpointContext {
    pub state_and_mult: u32,
    pub max_packet_and_burst: u32,
    pub dequeue_ptr_low: u32,
    pub dequeue_ptr_high: u32,
    pub avg_trb_length: u32,
    _reserved: [u32; 3],
}

impl EndpointContext {
    pub fn new() -> Self {
        Self {
            state_and_mult: 0,
            max_packet_and_burst: 0,
            dequeue_ptr_low: 0,
            dequeue_ptr_high: 0,
            avg_trb_length: 0,
            _reserved: [0; 3],
        }
    }
    
    /// Obtiene el endpoint state
    pub fn endpoint_state(&self) -> EndpointState {
        let state = (self.state_and_mult & 0x7) as u8;
        EndpointState::from_u8(state)
    }
    
    /// Establece el endpoint state
    pub fn set_endpoint_state(&mut self, state: EndpointState) {
        self.state_and_mult = (self.state_and_mult & !0x7) | (state as u32);
    }
    
    /// Obtiene el tipo de endpoint
    pub fn endpoint_type(&self) -> EndpointType {
        let ep_type = ((self.state_and_mult >> 3) & 0x7) as u8;
        match ep_type {
            0 => EndpointType::NotValid,
            1 => EndpointType::IsochOut,
            2 => EndpointType::BulkOut,
            3 => EndpointType::InterruptOut,
            4 => EndpointType::Control,
            5 => EndpointType::IsochIn,
            6 => EndpointType::BulkIn,
            7 => EndpointType::InterruptIn,
            _ => EndpointType::NotValid,
        }
    }
    
    /// Establece el tipo de endpoint
    pub fn set_endpoint_type(&mut self, ep_type: EndpointType) {
        self.state_and_mult = (self.state_and_mult & !(0x7 << 3)) | ((ep_type as u32) << 3);
    }
    
    /// Obtiene la dirección del TR Dequeue Pointer
    pub fn tr_dequeue_pointer(&self) -> u64 {
        (self.dequeue_ptr_low as u64) | ((self.dequeue_ptr_high as u64) << 32)
    }
    
    /// Establece la dirección del TR Dequeue Pointer
    pub fn set_tr_dequeue_pointer(&mut self, addr: u64, dcs: bool) {
        // Bit 0 es el DCS (Dequeue Cycle State)
        self.dequeue_ptr_low = (addr as u32 & !0xF) | (dcs as u32);
        self.dequeue_ptr_high = (addr >> 32) as u32;
    }
    
    /// Obtiene el max packet size
    pub fn max_packet_size(&self) -> u16 {
        (self.max_packet_and_burst >> 16) as u16
    }
    
    /// Establece el max packet size
    pub fn set_max_packet_size(&mut self, size: u16) {
        self.max_packet_and_burst = (self.max_packet_and_burst & 0xFFFF) | ((size as u32) << 16);
    }
}

/// Device Context (32 bytes slot + 31 endpoints de 32 bytes cada uno)
#[repr(C, align(64))]
pub struct DeviceContext {
    pub slot: SlotContext,
    pub endpoints: [EndpointContext; 31],
}

impl DeviceContext {
    pub fn new() -> Self {
        Self {
            slot: SlotContext::new(),
            endpoints: [EndpointContext::new(); 31],
        }
    }
    
    /// Obtiene el endpoint context para un endpoint específico
    pub fn endpoint(&self, endpoint_id: u8) -> Option<&EndpointContext> {
        if endpoint_id == 0 || endpoint_id > 31 {
            return None;
        }
        Some(&self.endpoints[(endpoint_id - 1) as usize])
    }
    
    /// Obtiene el endpoint context mutable
    pub fn endpoint_mut(&mut self, endpoint_id: u8) -> Option<&mut EndpointContext> {
        if endpoint_id == 0 || endpoint_id > 31 {
            return None;
        }
        Some(&mut self.endpoints[(endpoint_id - 1) as usize])
    }
}

/// Input Context (para comandos que modifican device context)
#[repr(C, align(64))]
pub struct InputContext {
    pub drop_context_flags: u32,
    pub add_context_flags: u32,
    _reserved: [u32; 5],
    pub control: u32,
    pub device: DeviceContext,
}

impl InputContext {
    pub fn new() -> Self {
        Self {
            drop_context_flags: 0,
            add_context_flags: 0,
            _reserved: [0; 5],
            control: 0,
            device: DeviceContext::new(),
        }
    }
    
    /// Marca un endpoint para ser agregado
    pub fn add_endpoint(&mut self, endpoint_id: u8) {
        if endpoint_id <= 31 {
            self.add_context_flags |= 1 << endpoint_id;
        }
    }
    
    /// Marca un endpoint para ser removido
    pub fn drop_endpoint(&mut self, endpoint_id: u8) {
        if endpoint_id <= 31 {
            self.drop_context_flags |= 1 << endpoint_id;
        }
    }
}

/// Device Context Base Address Array (DCBAA)
/// Contiene punteros a los device contexts de cada slot
#[repr(C, align(64))]
pub struct DeviceContextBaseAddressArray {
    pub addresses: Box<[u64; 256]>,
    pub contexts: Vec<Box<DeviceContext>>,
}

impl DeviceContextBaseAddressArray {
    pub fn new(max_slots: u8) -> Self {
        let mut addresses = Box::new([0u64; 256]);
        let mut contexts = Vec::with_capacity(max_slots as usize);
        
        // Crear device contexts para cada slot
        for i in 0..max_slots as usize {
            let context = Box::new(DeviceContext::new());
            addresses[i] = &*context as *const DeviceContext as u64;
            contexts.push(context);
        }
        
        Self {
            addresses,
            contexts,
        }
    }
    
    /// Obtiene la dirección física del DCBAA
    pub fn physical_address(&self) -> u64 {
        self.addresses.as_ptr() as u64
    }
    
    /// Obtiene el device context para un slot
    pub fn get_context(&self, slot_id: u8) -> Option<&DeviceContext> {
        if slot_id == 0 || slot_id as usize > self.contexts.len() {
            return None;
        }
        Some(&self.contexts[(slot_id - 1) as usize])
    }
    
    /// Obtiene el device context mutable para un slot
    pub fn get_context_mut(&mut self, slot_id: u8) -> Option<&mut DeviceContext> {
        if slot_id == 0 || slot_id as usize > self.contexts.len() {
            return None;
        }
        Some(&mut self.contexts[(slot_id - 1) as usize])
    }
}

