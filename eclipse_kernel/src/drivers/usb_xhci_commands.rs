//! Comandos XHCI
//! Basado en Redox OS - Implementación completa de comandos del Command Ring

use alloc::vec::Vec;
use alloc::boxed::Box;
use crate::drivers::usb_xhci_transfer::{Trb, TrbType};
use crate::drivers::usb_xhci_context::{InputContext, DeviceContext};

/// Comando Enable Slot
pub struct EnableSlotCommand {
    slot_type: u8,
}

impl EnableSlotCommand {
    pub fn new(slot_type: u8) -> Self {
        Self { slot_type }
    }
    
    /// Convierte el comando a un TRB (basado en Redox)
    pub fn to_trb(&self, cycle: bool) -> Trb {
        // Enable Slot TRB format:
        // bits 0-63: Reserved (0)
        // bits 64-95: Reserved (0)  
        // bits 96-105: Type = 9 (Enable Slot)
        // bits 106-111: Reserved
        // bits 112-116: Slot Type
        // bits 117-127: Reserved
        // bit 0: Cycle
        
        let mut control = ((TrbType::EnableSlot as u32) << 10) | (cycle as u32);
        control |= (self.slot_type as u32 & 0x1F) << 16;  // Bits 16-20: Slot Type
        
        Trb::with_values(0, 0, control)
    }
}

/// Comando Disable Slot
pub struct DisableSlotCommand {
    slot_id: u8,
}

impl DisableSlotCommand {
    pub fn new(slot_id: u8) -> Self {
        Self { slot_id }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::DisableSlot as u32) << 10) | (cycle as u32);
        control |= (self.slot_id as u32) << 24;  // Bits 24-31: Slot ID
        
        Trb::with_values(0, 0, control)
    }
}

/// Comando Address Device
pub struct AddressDeviceCommand {
    input_context_ptr: u64,
    slot_id: u8,
    block_set_address_request: bool,
}

impl AddressDeviceCommand {
    pub fn new(input_context_ptr: u64, slot_id: u8) -> Self {
        Self {
            input_context_ptr,
            slot_id,
            block_set_address_request: false,
        }
    }
    
    /// Crea comando con BSR (Block Set Address Request)
    /// Si BSR=1, el comando no establece la dirección, solo inicializa el slot
    pub fn with_bsr(input_context_ptr: u64, slot_id: u8) -> Self {
        Self {
            input_context_ptr,
            slot_id,
            block_set_address_request: true,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        // Address Device TRB format:
        // bits 0-63: Input Context Pointer (alineado a 16 bytes)
        // bits 64-95: Reserved
        // bits 96-105: Type = 11 (Address Device)
        // bit 106: BSR (Block Set Address Request)
        // bits 107-127: Reserved
        // bits 128-135: Slot ID
        // bit 0: Cycle
        
        let mut control = ((TrbType::AddressDevice as u32) << 10) | (cycle as u32);
        
        if self.block_set_address_request {
            control |= 1 << 9;  // BSR bit
        }
        
        control |= (self.slot_id as u32) << 24;  // Slot ID
        
        Trb::with_values(self.input_context_ptr & !0xF, 0, control)
    }
}

/// Comando Configure Endpoint
pub struct ConfigureEndpointCommand {
    input_context_ptr: u64,
    slot_id: u8,
    deconfigure: bool,
}

impl ConfigureEndpointCommand {
    pub fn new(input_context_ptr: u64, slot_id: u8) -> Self {
        Self {
            input_context_ptr,
            slot_id,
            deconfigure: false,
        }
    }
    
    /// Crea comando para desconfigurar todos los endpoints
    pub fn deconfigure(slot_id: u8) -> Self {
        Self {
            input_context_ptr: 0,
            slot_id,
            deconfigure: true,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::ConfigureEndpoint as u32) << 10) | (cycle as u32);
        
        if self.deconfigure {
            control |= 1 << 9;  // DC (Deconfigure) bit
        }
        
        control |= (self.slot_id as u32) << 24;
        
        Trb::with_values(self.input_context_ptr & !0xF, 0, control)
    }
}

/// Comando Evaluate Context
pub struct EvaluateContextCommand {
    input_context_ptr: u64,
    slot_id: u8,
}

impl EvaluateContextCommand {
    pub fn new(input_context_ptr: u64, slot_id: u8) -> Self {
        Self {
            input_context_ptr,
            slot_id,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::EvaluateContext as u32) << 10) | (cycle as u32);
        control |= (self.slot_id as u32) << 24;
        
        Trb::with_values(self.input_context_ptr & !0xF, 0, control)
    }
}

/// Comando Reset Endpoint
pub struct ResetEndpointCommand {
    slot_id: u8,
    endpoint_id: u8,
    transfer_state_preserve: bool,
}

impl ResetEndpointCommand {
    pub fn new(slot_id: u8, endpoint_id: u8) -> Self {
        Self {
            slot_id,
            endpoint_id,
            transfer_state_preserve: false,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::ResetEndpoint as u32) << 10) | (cycle as u32);
        
        if self.transfer_state_preserve {
            control |= 1 << 9;  // TSP bit
        }
        
        control |= (self.endpoint_id as u32) << 16;  // Endpoint ID
        control |= (self.slot_id as u32) << 24;      // Slot ID
        
        Trb::with_values(0, 0, control)
    }
}

/// Comando Stop Endpoint
pub struct StopEndpointCommand {
    slot_id: u8,
    endpoint_id: u8,
    suspend: bool,
}

impl StopEndpointCommand {
    pub fn new(slot_id: u8, endpoint_id: u8) -> Self {
        Self {
            slot_id,
            endpoint_id,
            suspend: false,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::StopEndpoint as u32) << 10) | (cycle as u32);
        
        if self.suspend {
            control |= 1 << 23;  // SP (Suspend) bit
        }
        
        control |= (self.endpoint_id as u32) << 16;
        control |= (self.slot_id as u32) << 24;
        
        Trb::with_values(0, 0, control)
    }
}

/// Comando Set TR Dequeue Pointer
pub struct SetTrDequeuePointerCommand {
    dequeue_ptr: u64,
    stream_id: u16,
    slot_id: u8,
    endpoint_id: u8,
}

impl SetTrDequeuePointerCommand {
    pub fn new(dequeue_ptr: u64, slot_id: u8, endpoint_id: u8) -> Self {
        Self {
            dequeue_ptr,
            stream_id: 0,
            slot_id,
            endpoint_id,
        }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        // Parameter contiene: dequeue pointer (bits 4-63) + DCS (bit 0) + SCT (bits 1-3)
        let parameter = (self.dequeue_ptr & !0xF) | 1;  // DCS = 1 por defecto
        
        // Status contiene: Stream ID (bits 16-31)
        let status = (self.stream_id as u32) << 16;
        
        let mut control = ((TrbType::SetTrDequeuePointer as u32) << 10) | (cycle as u32);
        control |= (self.endpoint_id as u32) << 16;
        control |= (self.slot_id as u32) << 24;
        
        Trb::with_values(parameter, status, control)
    }
}

/// Comando Reset Device
pub struct ResetDeviceCommand {
    slot_id: u8,
}

impl ResetDeviceCommand {
    pub fn new(slot_id: u8) -> Self {
        Self { slot_id }
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut control = ((TrbType::ResetDevice as u32) << 10) | (cycle as u32);
        control |= (self.slot_id as u32) << 24;
        
        Trb::with_values(0, 0, control)
    }
}

/// Comando No-Op (para testing)
pub struct NoOpCommand;

impl NoOpCommand {
    pub fn new() -> Self {
        Self
    }
    
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let control = ((TrbType::NoOpCommand as u32) << 10) | (cycle as u32);
        Trb::with_values(0, 0, control)
    }
}

/// Event de completación de comando
#[derive(Debug, Clone, Copy)]
pub struct CommandCompletionEvent {
    pub command_trb_ptr: u64,
    pub completion_code: u8,
    pub completion_parameter: u32,
    pub slot_id: u8,
}

impl CommandCompletionEvent {
    /// Parsea un TRB de Command Completion Event
    pub fn from_trb(trb: &Trb) -> Self {
        Self {
            command_trb_ptr: trb.parameter,
            completion_code: ((trb.status >> 24) & 0xFF) as u8,
            completion_parameter: trb.status & 0xFFFFFF,
            slot_id: ((trb.control >> 24) & 0xFF) as u8,
        }
    }
    
    /// Verifica si el comando fue exitoso
    pub fn is_success(&self) -> bool {
        self.completion_code == 1  // Success
    }
    
    /// Obtiene el slot ID asignado (para Enable Slot)
    pub fn get_slot_id(&self) -> u8 {
        self.slot_id
    }
}

