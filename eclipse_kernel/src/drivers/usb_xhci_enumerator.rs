//! Device Enumerator para XHCI
//! Basado en la implementación de Redox OS con async/await simulado para no_std

use alloc::vec::Vec;
use alloc::collections::VecDeque;
use crate::drivers::usb_xhci_port::{XhciPort, PortFlags, PortInfo};
use crate::drivers::usb_xhci_context::{SlotState, EndpointState};

/// Solicitud de enumeración de dispositivo
#[derive(Debug, Clone, Copy)]
pub struct DeviceEnumerationRequest {
    pub port_id: u8,
    pub request_type: EnumerationRequestType,
}

/// Tipo de solicitud de enumeración
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumerationRequestType {
    /// Dispositivo conectado - enumerar
    Attach,
    
    /// Dispositivo desconectado - limpiar
    Detach,
    
    /// Re-enumerar dispositivo existente
    ReEnumerate,
}

/// Estado de enumeración de un dispositivo
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumerationState {
    /// No hay dispositivo
    None,
    
    /// Dispositivo detectado, esperando reset
    Detected,
    
    /// Puerto en reset
    Resetting,
    
    /// Puerto reseteado, esperando habilitación
    Reset,
    
    /// Habilitado, listo para Get Descriptor
    Enabled,
    
    /// Obteniendo descriptores
    GettingDescriptors,
    
    /// Asignando dirección
    Addressing,
    
    /// Dispositivo completamente enumerado
    Enumerated,
    
    /// Error en enumeración
    Error,
}

/// Enumerator de dispositivos USB
pub struct DeviceEnumerator {
    /// Cola de solicitudes de enumeración
    request_queue: VecDeque<DeviceEnumerationRequest>,
    
    /// Estados de enumeración por puerto
    port_states: [EnumerationState; 256],
    
    /// Contador de dispositivos enumerados
    enumerated_count: u32,
}

impl DeviceEnumerator {
    pub fn new() -> Self {
        Self {
            request_queue: VecDeque::new(),
            port_states: [EnumerationState::None; 256],
            enumerated_count: 0,
        }
    }
    
    /// Agrega una solicitud de enumeración a la cola
    pub fn enqueue_request(&mut self, port_id: u8, request_type: EnumerationRequestType) {
        let request = DeviceEnumerationRequest {
            port_id,
            request_type,
        };
        
        self.request_queue.push_back(request);
        
        crate::debug::serial_write_str(&alloc::format!(
            "ENUM: Encolada solicitud {:?} para puerto {}\n",
            request_type, port_id
        ));
    }
    
    /// Obtiene la siguiente solicitud pendiente
    pub fn dequeue_request(&mut self) -> Option<DeviceEnumerationRequest> {
        self.request_queue.pop_front()
    }
    
    /// Verifica si hay solicitudes pendientes
    pub fn has_pending_requests(&self) -> bool {
        !self.request_queue.is_empty()
    }
    
    /// Obtiene el estado de enumeración de un puerto
    pub fn get_port_state(&self, port_id: u8) -> EnumerationState {
        if (port_id as usize) < self.port_states.len() {
            self.port_states[port_id as usize]
        } else {
            EnumerationState::None
        }
    }
    
    /// Establece el estado de enumeración de un puerto
    pub fn set_port_state(&mut self, port_id: u8, state: EnumerationState) {
        if (port_id as usize) < self.port_states.len() {
            let old_state = self.port_states[port_id as usize];
            self.port_states[port_id as usize] = state;
            
            crate::debug::serial_write_str(&alloc::format!(
                "ENUM: Puerto {} cambió de estado {:?} -> {:?}\n",
                port_id, old_state, state
            ));
        }
    }
    
    /// Procesa una solicitud de attach
    pub fn process_attach_request(&mut self, port_id: u8, port: &XhciPort) -> Result<(), &'static str> {
        let port_info = port.get_info();
        
        crate::debug::serial_write_str(&alloc::format!(
            "ENUM: Procesando attach para puerto {} (conectado={}, habilitado={})\n",
            port_id, port_info.connected, port_info.enabled
        ));
        
        // Verificar que el dispositivo está conectado
        if !port_info.connected {
            return Err("Device not connected");
        }
        
        // Si no está habilitado (USB 2.0), necesitamos reset
        if !port_info.enabled {
            self.set_port_state(port_id, EnumerationState::Detected);
            
            crate::debug::serial_write_str(&alloc::format!(
                "ENUM: Puerto {} no habilitado, iniciando reset\n",
                port_id
            ));
            
            // Limpiar todos los bits de cambio antes del reset
            port.clear_all_change_bits();
            
            // Iniciar reset
            port.set_reset();
            self.set_port_state(port_id, EnumerationState::Resetting);
            
            // En un sistema real, esperaríamos a que PRC se setee
            // Por ahora solo retornamos OK
            return Ok(());
        }
        
        // Puerto ya habilitado (USB 3.0), proceder con enumeración
        self.set_port_state(port_id, EnumerationState::Enabled);
        
        crate::debug::serial_write_str(&alloc::format!(
            "ENUM: Puerto {} habilitado, velocidad: {}\n",
            port_id, port_info.speed_str()
        ));
        
        Ok(())
    }
    
    /// Procesa una solicitud de detach
    pub fn process_detach_request(&mut self, port_id: u8) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "ENUM: Procesando detach para puerto {}\n",
            port_id
        ));
        
        self.set_port_state(port_id, EnumerationState::None);
        
        // Aquí se liberaría el slot, se limpiarían los endpoints, etc.
        
        Ok(())
    }
    
    /// Obtiene estadísticas del enumerator
    pub fn get_stats(&self) -> EnumeratorStats {
        let pending = self.request_queue.len();
        let mut attached = 0;
        let mut enumerated = 0;
        let mut error = 0;
        
        for state in &self.port_states {
            match state {
                EnumerationState::None => {},
                EnumerationState::Enumerated => enumerated += 1,
                EnumerationState::Error => error += 1,
                _ => attached += 1,
            }
        }
        
        EnumeratorStats {
            pending_requests: pending,
            devices_attached: attached,
            devices_enumerated: enumerated,
            devices_error: error,
        }
    }
}

/// Estadísticas del enumerator
#[derive(Debug, Clone, Copy)]
pub struct EnumeratorStats {
    pub pending_requests: usize,
    pub devices_attached: u32,
    pub devices_enumerated: u32,
    pub devices_error: u32,
}

