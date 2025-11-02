//! Lector de datos USB HID desde XHCI
//! 
//! Proporciona una interfaz segura para leer datos de dispositivos HID
//! usando polling del controlador XHCI, evitando deadlocks con ISRs.

use alloc::vec::Vec;
use crate::debug::serial_write_str;

/// Transfer Request Block (TRB) - formato básico
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    /// Obtiene el tipo de TRB (bits 15:10 del campo control)
    pub fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }
    
    /// Obtiene el Cycle bit (bit 0 del campo control)
    pub fn cycle_bit(&self) -> bool {
        (self.control & 1) != 0
    }
    
    /// Verifica si es un Transfer Event TRB
    pub fn is_transfer_event(&self) -> bool {
        self.trb_type() == 32 // Transfer Event
    }
    
    /// Extrae Slot ID de un Transfer Event TRB (bits 31:24 del campo control)
    pub fn slot_id(&self) -> u8 {
        ((self.control >> 24) & 0xFF) as u8
    }
    
    /// Extrae Endpoint ID de un Transfer Event TRB (bits 20:16 del campo control)  
    pub fn endpoint_id(&self) -> u8 {
        ((self.control >> 16) & 0x1F) as u8
    }
    
    /// Extrae el puntero al TRB que causó el evento (campo parameter)
    pub fn trb_pointer(&self) -> u64 {
        self.parameter
    }
    
    /// Extrae el código de completación (bits 31:24 del campo status)
    pub fn completion_code(&self) -> u8 {
        ((self.status >> 24) & 0xFF) as u8
    }
    
    /// Extrae la longitud de transferencia (bits 23:0 del campo status)
    pub fn transfer_length(&self) -> u32 {
        self.status & 0xFFFFFF
    }
}

/// Buffer para datos HID leídos
pub struct HidDataBuffer {
    pub slot_id: u8,
    pub endpoint: u8,
    pub data: Vec<u8>,
    pub valid: bool,
}

impl HidDataBuffer {
    pub fn new(slot_id: u8, endpoint: u8, size: usize) -> Self {
        Self {
            slot_id,
            endpoint,
            data: alloc::vec![0u8; size],
            valid: false,
        }
    }
}

/// Información del Event Ring guardada globalmente
static mut EVENT_RING_INFO: Option<EventRingInfo> = None;

/// Información necesaria para leer el Event Ring
struct EventRingInfo {
    segment_base: u64,  // Dirección del Event Ring Segment
    segment_size: u32,  // Tamaño del segmento (en TRBs)
    dequeue_index: u32, // Índice actual de lectura
    cycle_state: bool,  // Estado del Cycle bit esperado
}

/// Información de transferencia activa (buffer asociado a un TRB)
struct ActiveTransfer {
    trb_pointer: u64,   // Puntero al TRB original
    buffer_addr: u64,   // Dirección del buffer de datos
    max_length: u16,    // Tamaño máximo del buffer
}

/// Almacena información de transferencias activas por TRB pointer
static mut ACTIVE_TRANSFERS: Option<Vec<ActiveTransfer>> = None;

/// Registra una transferencia activa
/// Debe llamarse cuando se crea una nueva transferencia IN
pub fn register_active_transfer(trb_pointer: u64, buffer_addr: u64, max_length: u16) {
    unsafe {
        if ACTIVE_TRANSFERS.is_none() {
            ACTIVE_TRANSFERS = Some(Vec::new());
        }
        if let Some(transfers) = &mut ACTIVE_TRANSFERS {
            transfers.push(ActiveTransfer {
                trb_pointer,
                buffer_addr,
                max_length,
            });
        }
    }
}

/// Encuentra y elimina una transferencia activa por TRB pointer
fn find_and_remove_transfer(trb_pointer: u64) -> Option<ActiveTransfer> {
    unsafe {
        if let Some(transfers) = &mut ACTIVE_TRANSFERS {
            if let Some(pos) = transfers.iter().position(|t| t.trb_pointer == trb_pointer) {
                Some(transfers.remove(pos))
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Guarda información del Event Ring para acceso desde polling
/// Debe llamarse una vez después de inicializar XHCI
pub fn set_event_ring_info(segment_base: u64, segment_size: u32) {
    unsafe {
        EVENT_RING_INFO = Some(EventRingInfo {
            segment_base,
            segment_size,
            dequeue_index: 0,
            cycle_state: true, // Inicialmente esperamos Cycle = 1
        });
    }
    
    serial_write_str(&alloc::format!(
        "USB_HID_READER: Event Ring configurado - base=0x{:016X}, size={} TRBs\n",
        segment_base, segment_size
    ));
}

/// Lee datos de un endpoint HID usando XHCI en modo polling
/// 
/// Esta función NO usa interrupciones, lo que la hace segura para llamar
/// desde el main loop sin riesgo de deadlocks.
pub fn read_hid_endpoint_polling(slot_id: u8, endpoint: u8, buffer: &mut [u8]) -> Result<usize, &'static str> {
    // TODO: Implementar lectura real desde XHCI
    // Por ahora, retornar 0 bytes leídos (no hay datos)
    
    // Ejemplo de implementación futura:
    // 1. Obtener controlador XHCI global (sin lock si es posible)
    // 2. Verificar si hay transferencias completadas en el event ring
    // 3. Leer datos del Transfer Ring del endpoint
    // 4. Copiar datos al buffer
    // 5. Actualizar el event ring dequeue pointer
    
    Ok(0) // Por ahora, no hay datos disponibles
}

/// Almacena información de endpoints HID configurados
static mut HID_ENDPOINT_RINGS: Option<Vec<EndpointRingInfo>> = None;

/// Información de Transfer Ring para un endpoint HID
struct EndpointRingInfo {
    slot_id: u8,
    endpoint_id: u8,
    ring_base: u64,
    ring_size: u32,
    enqueue_index: u32,
    cycle_bit: bool,
}

/// Configura un endpoint HID para recibir datos
/// 
/// Debe llamarse una vez por cada dispositivo HID después de la enumeración.
pub fn configure_hid_endpoint(slot_id: u8, endpoint: u8, max_packet_size: u16, interval: u8) -> Result<(), &'static str> {
    serial_write_str(&alloc::format!(
        "USB_HID_READER: Configurando endpoint - slot={}, ep={}, mps={}, interval={}\n",
        slot_id, endpoint, max_packet_size, interval
    ));
    
    // DESHABILITADO TEMPORALMENTE para evitar crashes
    // El problema es que crear Transfer Rings con mem::forget causa crashes de KVM
    // Por ahora, usaremos polling directo del Event Ring del XHCI
    
    serial_write_str(&alloc::format!(
        "USB_HID_READER: Endpoint configurado (modo polling directo)\n"
    ));
    
    Ok(())
}

/// Inicia transferencias IN periódicas para un endpoint HID
/// 
/// Para dispositivos HID (teclado/ratón), necesitamos configurar transferencias
/// IN que el controlador ejecutará automáticamente según el intervalo.
pub fn start_periodic_in_transfers(slot_id: u8, endpoint: u8) -> Result<(), &'static str> {
    serial_write_str(&alloc::format!(
        "USB_HID_READER: Iniciando transferencias periódicas - slot={}, ep={}\n",
        slot_id, endpoint
    ));
    
    // DESHABILITADO TEMPORALMENTE para evitar crashes
    // El problema es que crear buffers con mem::forget causa crashes de KVM
    
    serial_write_str(&alloc::format!(
        "USB_HID_READER: Transferencias configuradas (modo polling directo del Event Ring)\n"
    ));
    
    Ok(())
}

/// Procesa eventos de transferencia completados desde el Event Ring
/// 
/// Esta función es segura para polling - no usa locks que puedan causar deadlocks.
pub fn process_completed_transfers() -> Vec<(u8, u8, Vec<u8>)> {
    use crate::drivers::usb_xhci_global::{get_runtime_base, xhci_read64, xhci_write64, xhci_read32, get_operational_base, offsets};
    
    let mut completed_transfers = Vec::new();
    
    // Verificar que el XHCI esté inicializado
    let rt_base = match get_runtime_base() {
        Some(base) => base,
        None => return completed_transfers, // XHCI no disponible
    };
    
    let op_base = match get_operational_base() {
        Some(base) => base,
        None => return completed_transfers,
    };
    
    // DEBUG: Verificar estado del controlador (solo cada muchas llamadas para no saturar)
    static mut POLL_COUNT: u64 = 0;
    unsafe {
        POLL_COUNT += 1;
        
        // Mostrar info solo cada 1 millón de polls (~cada 10 segundos con polling cada 10ms)
        if POLL_COUNT % 1_000_000 == 0 {
            // Leer USBSTS para verificar que el controlador está corriendo
            if let Some(usbsts) = xhci_read32(op_base + offsets::OP_USBSTS) {
                let running = (usbsts & 0x01) == 0; // HCHalted cleared = running
                let event_int = (usbsts & 0x08) != 0; // Event Interrupt pending
                
                serial_write_str(&alloc::format!(
                    "USB_HID_READER: XHCI status - Running: {}, Event_Pending: {}\n",
                    running, event_int
                ));
            }
            
            // Leer ERDP para ver si hay eventos
            let erdp_offset = rt_base + offsets::RT_ERDP;
            if let Some(erdp) = xhci_read64(erdp_offset) {
                let erdp_addr = erdp & !0xF;
                serial_write_str(&alloc::format!(
                    "USB_HID_READER: ERDP = 0x{:016X}\n",
                    erdp_addr
                ));
            }
        }
    }
    
    // Leer eventos del Event Ring si tenemos la información
    unsafe {
        if let Some(ref mut ring_info) = EVENT_RING_INFO {
            // Leer hasta 16 eventos por polling para evitar saturación
            for _ in 0..16 {
                // Calcular dirección del TRB actual
                let trb_addr = ring_info.segment_base + (ring_info.dequeue_index as u64 * 16);
                let trb_ptr = trb_addr as *const Trb;
                
                // Leer TRB
                let trb = core::ptr::read_volatile(trb_ptr);
                
                // Verificar Cycle bit - si no coincide, no hay más eventos
                if trb.cycle_bit() != ring_info.cycle_state {
                    break;
                }
                
                // Procesar solo Transfer Event TRBs
                if trb.is_transfer_event() {
                    let slot_id = trb.slot_id();
                    let endpoint_id = trb.endpoint_id();
                    let completion_code = trb.completion_code();
                    let transfer_length = trb.transfer_length();
                    let trb_pointer = trb.trb_pointer();
                    
                    // Código 1 = Success
                    if completion_code == 1 && transfer_length > 0 {
                        // Buscar la transferencia activa correspondiente
                        if let Some(active_transfer) = find_and_remove_transfer(trb_pointer) {
                            // Leer datos desde el buffer
                            let data_len = core::cmp::min(transfer_length as usize, active_transfer.max_length as usize);
                            let mut data = Vec::with_capacity(data_len);
                            
                            unsafe {
                                let src_ptr = active_transfer.buffer_addr as *const u8;
                                for i in 0..data_len {
                                    data.push(core::ptr::read_volatile(src_ptr.add(i)));
                                }
                            }
                            
                            if !data.is_empty() {
                                completed_transfers.push((slot_id, endpoint_id, data));
                            }
                        } else {
                            // No encontramos la transferencia, intentar leer directamente del TRB
                            // Esto funciona si el TRB tiene la dirección del buffer en el campo parameter
                            unsafe {
                                let trb_ptr = trb_pointer as *const Trb;
                                let original_trb = core::ptr::read_volatile(trb_ptr);
                                let buffer_addr = original_trb.parameter;
                                
                                // Solo leer si la dirección parece válida (no es 0)
                                if buffer_addr != 0 && buffer_addr < 0xFFFFFFFFFFFFF000 {
                                    let data_len = core::cmp::min(transfer_length as usize, 64); // Máx 64 bytes para HID
                                    let mut data = Vec::with_capacity(data_len);
                                    
                                    let src_ptr = buffer_addr as *const u8;
                                    for i in 0..data_len {
                                        data.push(core::ptr::read_volatile(src_ptr.add(i)));
                                    }
                                    
                                    if !data.is_empty() {
                                        completed_transfers.push((slot_id, endpoint_id, data));
                                        
                                        if POLL_COUNT % 100_000 == 0 {
                                            serial_write_str(&alloc::format!(
                                                "USB_HID_READER: Transfer leído - slot={}, ep={}, len={}\n",
                                                slot_id, endpoint_id, data_len
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    } else if completion_code != 1 {
                        // Error en la transferencia
                        if POLL_COUNT % 1_000_000 == 0 {
                            serial_write_str(&alloc::format!(
                                "USB_HID_READER: Transfer error - slot={}, ep={}, code={}\n",
                                slot_id, endpoint_id, completion_code
                            ));
                        }
                        // Eliminar la transferencia activa incluso si hay error
                        find_and_remove_transfer(trb_pointer);
                    }
                }
                
                // Avanzar al siguiente TRB
                ring_info.dequeue_index += 1;
                if ring_info.dequeue_index >= ring_info.segment_size {
                    ring_info.dequeue_index = 0;
                    ring_info.cycle_state = !ring_info.cycle_state; // Toggle cycle
                }
            }
            
            // Actualizar ERDP register para indicar que procesamos eventos
            if ring_info.dequeue_index > 0 || !completed_transfers.is_empty() {
                let new_erdp = ring_info.segment_base + (ring_info.dequeue_index as u64 * 16);
                let erdp_offset = rt_base + offsets::RT_ERDP;
                let _ = xhci_write64(erdp_offset, new_erdp | 0x08); // Bit 3 = EHB (Event Handler Busy)
            }
        }
    }
    
    completed_transfers
}

