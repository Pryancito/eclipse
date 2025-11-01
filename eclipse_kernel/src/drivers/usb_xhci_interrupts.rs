//! Soporte para interrupciones XHCI
//!
//! Este módulo implementa el manejo de interrupciones y eventos del controlador XHCI

use alloc::vec;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;

use crate::drivers::usb_xhci_transfer::{Trb, TrbType, CompletionEvent, CompletionCode};

/// Event Ring Segment Table Entry (mejorado basado en Redox)
/// 
/// Estructura alineada a 16 bytes que define un segmento del Event Ring
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct EventRingSegmentTableEntry {
    /// Dirección física del segmento (bits bajos)
    pub address_low: u32,
    
    /// Dirección física del segmento (bits altos)
    pub address_high: u32,
    
    /// Tamaño del segmento en número de TRBs
    pub size: u16,
    
    /// Reservado
    _reserved: u16,
    _reserved2: u32,
}

impl EventRingSegmentTableEntry {
    pub fn new(base_addr: u64, size: u16) -> Self {
        Self {
            address_low: base_addr as u32,
            address_high: (base_addr >> 32) as u32,
            size,
            _reserved: 0,
            _reserved2: 0,
        }
    }
    
    /// Obtiene la dirección física completa
    pub fn address(&self) -> u64 {
        (self.address_low as u64) | ((self.address_high as u64) << 32)
    }
    
    /// Establece la dirección física
    pub fn set_address(&mut self, addr: u64) {
        self.address_low = addr as u32;
        self.address_high = (addr >> 32) as u32;
    }
}

/// Event Ring (mejorado basado en Redox)
#[repr(C, align(64))]
pub struct EventRing {
    /// Segment Table Entries
    segment_table: Vec<EventRingSegmentTableEntry>,
    
    /// Segmentos del Event Ring (cada uno contiene TRBs)
    segments: Vec<Box<[Trb]>>,
    
    /// Índice del segmento actual
    current_segment: usize,
    
    /// Índice de dequeue dentro del segmento actual
    dequeue_index: usize,
    
    /// Cycle bit actual
    cycle_bit: bool,
    
    /// Event Ring Dequeue Pointer
    erdp: u64,
}

impl EventRing {
    /// Crea un nuevo Event Ring con un solo segmento (estilo Redox)
    pub fn new(segment_size: usize) -> Self {
        let mut trbs = Vec::with_capacity(segment_size);
        for _ in 0..segment_size {
            trbs.push(Trb::new());
        }
        
        let trb_slice = trbs.into_boxed_slice();
        let base_addr = trb_slice.as_ptr() as u64;
        
        // Crear Segment Table Entry (STE)
        let ste = EventRingSegmentTableEntry::new(base_addr, segment_size as u16);
        
        Self {
            segment_table: vec![ste],
            segments: vec![trb_slice],
            current_segment: 0,
            dequeue_index: 0,
            cycle_bit: true,
            erdp: base_addr,
        }
    }
    
    /// Crea un Event Ring con múltiples segmentos
    pub fn new_multi_segment(segment_count: usize, segment_size: usize) -> Self {
        let mut segments = Vec::with_capacity(segment_count);
        let mut segment_table = Vec::with_capacity(segment_count);
        
        for _ in 0..segment_count {
            let mut trbs = Vec::with_capacity(segment_size);
            for _ in 0..segment_size {
                trbs.push(Trb::new());
            }
            
            let trb_slice = trbs.into_boxed_slice();
            let base_addr = trb_slice.as_ptr() as u64;
            
            let ste = EventRingSegmentTableEntry::new(base_addr, segment_size as u16);
            segment_table.push(ste);
            segments.push(trb_slice);
        }
        
        let erdp = if !segments.is_empty() {
            segments[0].as_ptr() as u64
        } else {
            0
        };
        
        Self {
            segment_table,
            segments,
            current_segment: 0,
            dequeue_index: 0,
            cycle_bit: true,
            erdp,
        }
    }

    /// Obtiene la dirección de la tabla de segmentos (ERSTBA)
    pub fn segment_table_address(&self) -> u64 {
        self.segment_table.as_ptr() as u64
    }
    
    /// Obtiene el tamaño de la tabla de segmentos (ERSTSZ)
    pub fn segment_table_size(&self) -> u16 {
        self.segment_table.len() as u16
    }

    /// Obtiene el número de segmentos
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Obtiene el puntero de dequeue actual
    pub fn dequeue_pointer(&self) -> u64 {
        self.erdp
    }

    /// Desencola el siguiente evento
    pub fn dequeue_event(&mut self) -> Option<Trb> {
        if self.segments.is_empty() {
            return None;
        }

        let segment = &self.segments[self.current_segment];
        let trb = segment[self.dequeue_index];
        
        // Verificar el bit de ciclo
        if trb.cycle_bit() != self.cycle_bit {
            // No hay eventos nuevos
            return None;
        }

        // Avanzar el dequeue pointer
        self.dequeue_index += 1;
        
        // Si llegamos al final del segmento, ir al siguiente
        if self.dequeue_index >= segment.len() {
            self.dequeue_index = 0;
            self.current_segment = (self.current_segment + 1) % self.segments.len();
            
            // Si volvimos al primer segmento, toggle el cycle bit
            if self.current_segment == 0 {
                self.cycle_bit = !self.cycle_bit;
            }
        }

        // Actualizar ERDP
        self.erdp = self.segments[self.current_segment].as_ptr() as u64 
                    + (self.dequeue_index * core::mem::size_of::<Trb>()) as u64;

        Some(trb)
    }

    /// Procesa todos los eventos pendientes
    pub fn process_events<F>(&mut self, mut handler: F) -> usize 
    where
        F: FnMut(&Trb),
    {
        let mut count = 0;
        
        while let Some(event) = self.dequeue_event() {
            handler(&event);
            count += 1;
            
            // Limitar el número de eventos procesados en una llamada
            if count >= 256 {
                break;
            }
        }
        
        count
    }
}

/// Interrupter XHCI (hay múltiples interrupters, típicamente se usa el 0)
pub struct XhciInterrupter {
    mmio_base: u64,
    interrupter_index: u8,
    event_ring: EventRing,
    pending_events: VecDeque<XhciEvent>,
    enabled: AtomicBool,
    events_processed: AtomicU32,
}

impl XhciInterrupter {
    /// Crea un nuevo interrupter
    pub fn new(mmio_base: u64, interrupter_index: u8, event_ring_size: usize) -> Self {
        Self {
            mmio_base,
            interrupter_index,
            event_ring: EventRing::new(event_ring_size),
            pending_events: VecDeque::new(),
            enabled: AtomicBool::new(false),
            events_processed: AtomicU32::new(0),
        }
    }

    /// Inicializa el interrupter en el hardware
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "XHCI_INT: Inicializando interrupter {}\n",
            self.interrupter_index
        ));

        unsafe {
            // Calcular dirección de los registros del interrupter
            // Runtime registers base está en RTSOFF (offset 0x18 en capability regs)
            let cap_ptr = self.mmio_base as *const u32;
            let rtsoff = read_volatile(cap_ptr.add(6)); // RTSOFF en dwords
            
            let runtime_base = self.mmio_base + rtsoff as u64;
            let interrupter_base = runtime_base + 0x20 + (self.interrupter_index as u64 * 0x20);
            
            // IMAN (Interrupter Management)
            let iman_ptr = interrupter_base as *mut u32;
            write_volatile(iman_ptr, 0x02); // IE (Interrupt Enable) = 1, IP (Interrupt Pending) = 0
            
            // IMOD (Interrupter Moderation)
            let imod_ptr = (interrupter_base + 0x04) as *mut u32;
            write_volatile(imod_ptr, 4000); // 1ms @ 250ns intervals
            
            // ERSTSZ (Event Ring Segment Table Size)
            let erstsz_ptr = (interrupter_base + 0x08) as *mut u32;
            write_volatile(erstsz_ptr, self.event_ring.segment_count() as u32);
            
            // ERSTBA (Event Ring Segment Table Base Address)
            let erstba_ptr = (interrupter_base + 0x10) as *mut u64;
            write_volatile(erstba_ptr, self.event_ring.segment_table_address());
            
            // ERDP (Event Ring Dequeue Pointer)
            let erdp_ptr = (interrupter_base + 0x18) as *mut u64;
            let erdp_value = self.event_ring.dequeue_pointer() | 0x08; // Set EHB (Event Handler Busy)
            write_volatile(erdp_ptr, erdp_value);
        }

        self.enabled.store(true, Ordering::SeqCst);
        
        crate::debug::serial_write_str(&alloc::format!(
            "XHCI_INT: Interrupter {} inicializado\n",
            self.interrupter_index
        ));
        
        Ok(())
    }

    /// Handler de interrupción - debe ser llamado desde el ISR
    pub fn handle_interrupt(&mut self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }

        // Procesar eventos del Event Ring
        let count = self.event_ring.process_events(|trb| {
            let event_type = trb.trb_type();
            
            match event_type {
                32 => { // Transfer Event
                    let event = CompletionEvent::from_transfer_event_trb(trb);
                    self.pending_events.push_back(XhciEvent::TransferComplete(event));
                }
                33 => { // Command Completion Event
                    self.pending_events.push_back(XhciEvent::CommandComplete(*trb));
                }
                34 => { // Port Status Change Event
                    let port_id = ((trb.control >> 24) & 0xFF) as u8;
                    self.pending_events.push_back(XhciEvent::PortStatusChange(port_id));
                }
                37 => { // Host Controller Event
                    let completion_code = ((trb.status >> 24) & 0xFF) as u8;
                    self.pending_events.push_back(XhciEvent::HostControllerEvent(completion_code));
                }
                _ => {
                    // Evento desconocido o no manejado
                    crate::debug::serial_write_str(&alloc::format!(
                        "XHCI_INT: Evento desconocido tipo {}\n",
                        event_type
                    ));
                }
            }
        });

        if count > 0 {
            self.events_processed.fetch_add(count as u32, Ordering::Relaxed);
            
            // Actualizar ERDP para indicar que procesamos los eventos
            unsafe {
                let cap_ptr = self.mmio_base as *const u32;
                let rtsoff = read_volatile(cap_ptr.add(6));
                let runtime_base = self.mmio_base + rtsoff as u64;
                let interrupter_base = runtime_base + 0x20 + (self.interrupter_index as u64 * 0x20);
                let erdp_ptr = (interrupter_base + 0x18) as *mut u64;
                
                write_volatile(erdp_ptr, self.event_ring.dequeue_pointer());
                
                // Limpiar el bit de Interrupt Pending
                let iman_ptr = interrupter_base as *mut u32;
                let mut iman = read_volatile(iman_ptr);
                iman |= 0x01; // IP = 1 (write 1 to clear)
                write_volatile(iman_ptr, iman);
            }
        }
    }

    /// Obtiene el siguiente evento pendiente
    pub fn pop_event(&mut self) -> Option<XhciEvent> {
        self.pending_events.pop_front()
    }

    /// Verifica si hay eventos pendientes
    pub fn has_pending_events(&self) -> bool {
        !self.pending_events.is_empty()
    }

    /// Obtiene el número total de eventos procesados
    pub fn total_events_processed(&self) -> u32 {
        self.events_processed.load(Ordering::Relaxed)
    }

    /// Habilita las interrupciones
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
        
        unsafe {
            let cap_ptr = self.mmio_base as *const u32;
            let rtsoff = read_volatile(cap_ptr.add(6));
            let runtime_base = self.mmio_base + rtsoff as u64;
            let interrupter_base = runtime_base + 0x20 + (self.interrupter_index as u64 * 0x20);
            let iman_ptr = interrupter_base as *mut u32;
            
            let mut iman = read_volatile(iman_ptr);
            iman |= 0x02; // IE = 1
            write_volatile(iman_ptr, iman);
        }
    }

    /// Deshabilita las interrupciones
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        
        unsafe {
            let cap_ptr = self.mmio_base as *const u32;
            let rtsoff = read_volatile(cap_ptr.add(6));
            let runtime_base = self.mmio_base + rtsoff as u64;
            let interrupter_base = runtime_base + 0x20 + (self.interrupter_index as u64 * 0x20);
            let iman_ptr = interrupter_base as *mut u32;
            
            let mut iman = read_volatile(iman_ptr);
            iman &= !0x02; // IE = 0
            write_volatile(iman_ptr, iman);
        }
    }
}

/// Tipos de eventos XHCI
#[derive(Debug, Clone)]
pub enum XhciEvent {
    /// Evento de transferencia completada
    TransferComplete(CompletionEvent),
    
    /// Evento de comando completado
    CommandComplete(Trb),
    
    /// Cambio de estado de puerto
    PortStatusChange(u8),
    
    /// Evento del controlador
    HostControllerEvent(u8),
    
    /// Solicitud de ancho de banda
    BandwidthRequest,
    
    /// Timbre tocado
    Doorbell(u8),
    
    /// Notificación de dispositivo
    DeviceNotification(u8, u16),
}

impl XhciEvent {
    /// Describe el evento en texto
    pub fn description(&self) -> alloc::string::String {
        match self {
            Self::TransferComplete(event) => {
                let code = CompletionCode::from_u8(event.completion_code);
                alloc::format!(
                    "Transfer Complete - Slot {}, EP {}, Code: {}",
                    event.slot_id,
                    event.endpoint_id,
                    code.description()
                )
            }
            Self::CommandComplete(_) => {
                alloc::string::String::from("Command Complete")
            }
            Self::PortStatusChange(port) => {
                alloc::format!("Port {} Status Change", port)
            }
            Self::HostControllerEvent(code) => {
                let comp_code = CompletionCode::from_u8(*code);
                alloc::format!("Host Controller Event: {}", comp_code.description())
            }
            Self::BandwidthRequest => {
                alloc::string::String::from("Bandwidth Request")
            }
            Self::Doorbell(db) => {
                alloc::format!("Doorbell {}", db)
            }
            Self::DeviceNotification(slot, data) => {
                alloc::format!("Device Notification - Slot {}, Data: 0x{:04X}", slot, data)
            }
        }
    }
}

/// Gestor global de interrupciones XHCI
pub static XHCI_INTERRUPT_MANAGER: Mutex<Option<XhciInterruptManager>> = Mutex::new(None);

/// Manager de interrupciones XHCI
pub struct XhciInterruptManager {
    interrupters: Vec<Box<XhciInterrupter>>,
    mmio_base: u64,
}

impl XhciInterruptManager {
    /// Crea un nuevo manager de interrupciones
    pub fn new(mmio_base: u64) -> Self {
        Self {
            interrupters: Vec::new(),
            mmio_base,
        }
    }

    /// Agrega un interrupter
    pub fn add_interrupter(&mut self, interrupter: XhciInterrupter) {
        self.interrupters.push(Box::new(interrupter));
    }

    /// Inicializa todos los interrupters
    pub fn initialize_all(&mut self) -> Result<(), &'static str> {
        for interrupter in &mut self.interrupters {
            interrupter.initialize()?;
        }
        Ok(())
    }

    /// Procesa interrupciones de todos los interrupters
    pub fn handle_interrupt(&mut self) {
        for interrupter in &mut self.interrupters {
            interrupter.handle_interrupt();
        }
    }

    /// Obtiene el primer evento pendiente de cualquier interrupter
    pub fn pop_any_event(&mut self) -> Option<(u8, XhciEvent)> {
        for (idx, interrupter) in self.interrupters.iter_mut().enumerate() {
            if let Some(event) = interrupter.pop_event() {
                return Some((idx as u8, event));
            }
        }
        None
    }

    /// Habilita todos los interrupters
    pub fn enable_all(&self) {
        for interrupter in &self.interrupters {
            interrupter.enable();
        }
    }

    /// Deshabilita todos los interrupters
    pub fn disable_all(&self) {
        for interrupter in &self.interrupters {
            interrupter.disable();
        }
    }

    /// Obtiene estadísticas de interrupciones
    pub fn get_statistics(&self) -> alloc::string::String {
        let mut stats = alloc::string::String::from("=== XHCI Interrupt Statistics ===\n");
        
        for (idx, interrupter) in self.interrupters.iter().enumerate() {
            stats.push_str(&alloc::format!(
                "Interrupter {}: {} eventos procesados\n",
                idx,
                interrupter.total_events_processed()
            ));
        }
        
        stats
    }
}

/// Inicializa el sistema de interrupciones XHCI
pub fn init_xhci_interrupts(mmio_base: u64, num_interrupters: u8) -> Result<(), &'static str> {
    crate::debug::serial_write_str("XHCI_INT: Inicializando sistema de interrupciones\n");
    
    let mut manager = XhciInterruptManager::new(mmio_base);
    
    // Crear interrupters (típicamente solo usamos el 0)
    for i in 0..num_interrupters {
        let interrupter = XhciInterrupter::new(mmio_base, i, 256);
        manager.add_interrupter(interrupter);
    }
    
    // Inicializar todos
    manager.initialize_all()?;
    
    // Guardar en el global
    *XHCI_INTERRUPT_MANAGER.lock() = Some(manager);
    
    crate::debug::serial_write_str("XHCI_INT: Sistema de interrupciones inicializado\n");
    Ok(())
}

/// Handler de interrupción global (debe ser llamado desde el ISR de XHCI)
pub fn xhci_interrupt_handler() {
    if let Some(ref mut manager) = *XHCI_INTERRUPT_MANAGER.lock() {
        manager.handle_interrupt();
    }
}

/// Procesa eventos pendientes
pub fn process_xhci_events<F>(mut handler: F) -> usize
where
    F: FnMut(&XhciEvent),
{
    let mut count = 0;
    
    if let Some(ref mut manager) = *XHCI_INTERRUPT_MANAGER.lock() {
        while let Some((_interrupter_idx, event)) = manager.pop_any_event() {
            handler(&event);
            count += 1;
            
            // Limitar procesamiento
            if count >= 100 {
                break;
            }
        }
    }
    
    count
}

