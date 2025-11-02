//! Implementación de XHCI usando el crate xhci correctamente
//! 
//! Este módulo reemplaza la implementación manual en usb_xhci_improved.rs
//! y usa las estructuras proporcionadas por el crate xhci.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::convert::TryFrom;
use crate::drivers::manager::DriverResult;
use crate::drivers::pci::PciDevice;
use crate::debug::serial_write_str;

// Importar TRBs del crate xhci
use xhci::ring::trb::event;

/// Representa un TRB raw (128 bits = 4 x u32)
type RawTrb = [u32; 4];

/// Tamaño del Event Ring (en número de TRBs)
const EVENT_RING_SIZE: usize = 256;

/// Tamaño del Command Ring (en número de TRBs)
const COMMAND_RING_SIZE: usize = 256;

/// Event Ring personalizado que usa TRBs del crate xhci
struct EventRing {
    /// Buffer de TRBs raw
    trbs: Vec<RawTrb>,
    /// Índice de dequeue (dónde estamos leyendo)
    dequeue_index: usize,
    /// Cycle bit esperado
    cycle_state: bool,
    /// Dirección física del buffer
    physical_address: u64,
}

impl EventRing {
    /// Crea un nuevo Event Ring
    fn new(size: usize) -> Result<Self, &'static str> {
        // Alocar buffer físicamente contiguo
        let buffer_size = size * core::mem::size_of::<RawTrb>();
        let (virt_addr, phys_addr) = crate::memory::physical::allocate_physically_contiguous(
            buffer_size,
            64 // Alignment de 64 bytes requerido por xHCI
        ).ok_or("No se pudo alocar memoria para Event Ring")?;
        
        // Inicializar TRBs a cero
        let trbs_ptr = virt_addr as *mut RawTrb;
        let mut trbs = Vec::with_capacity(size);
        unsafe {
            for i in 0..size {
                trbs.push([0, 0, 0, 0]);
                core::ptr::write_volatile(trbs_ptr.add(i), [0, 0, 0, 0]);
            }
        }
        
        Ok(Self {
            trbs,
            dequeue_index: 0,
            cycle_state: true, // Inicialmente esperamos cycle = 1
            physical_address: phys_addr,
        })
    }
    
    /// Obtiene la dirección física del ring
    fn physical_address(&self) -> u64 {
        self.physical_address
    }
    
    /// Intenta leer el siguiente evento del ring
    fn pop(&mut self) -> Option<event::Allowed> {
        if self.dequeue_index >= self.trbs.len() {
            return None;
        }
        
        // Leer el TRB actual de forma volátil
        let trb_ptr = self.trbs.as_ptr();
        let raw_trb = unsafe {
            core::ptr::read_volatile(trb_ptr.add(self.dequeue_index))
        };
        
        // Verificar el cycle bit (bit 0 del campo control [3])
        let trb_cycle = (raw_trb[3] & 0x01) != 0;
        
        if trb_cycle != self.cycle_state {
            // No hay más eventos
            return None;
        }
        
        // Intentar parsear el TRB usando el crate xhci
        let event = match event::Allowed::try_from(raw_trb) {
            Ok(evt) => evt,
            Err(_) => {
                // TRB no reconocido, avanzar al siguiente
                self.advance();
                return self.pop(); // Intentar el siguiente recursivamente
            }
        };
        
        // Avanzar al siguiente TRB
        self.advance();
        
        Some(event)
    }
    
    /// Avanza el índice de dequeue
    fn advance(&mut self) {
        self.dequeue_index += 1;
        
        if self.dequeue_index >= self.trbs.len() {
            // Wrap around
            self.dequeue_index = 0;
            self.cycle_state = !self.cycle_state; // Toggle cycle
        }
    }
    
    /// Obtiene el puntero de dequeue actual (dirección física)
    fn dequeue_pointer(&self) -> u64 {
        self.physical_address + (self.dequeue_index * core::mem::size_of::<RawTrb>()) as u64
    }
}

/// Controlador XHCI usando el crate xhci
pub struct XhciControllerWithCrate {
    pci_device: PciDevice,
    mmio_base: u64,
    operational_base: u64,
    runtime_base: u64,
    doorbell_base: u64,
    
    // Event Ring usando TRBs del crate xhci
    event_ring: Option<EventRing>,
    
    max_slots: u8,
    num_ports: u8,
}

impl XhciControllerWithCrate {
    /// Crea una nueva instancia del controlador XHCI
    pub fn new(pci: PciDevice) -> Self {
        Self {
            pci_device: pci,
            mmio_base: 0,
            operational_base: 0,
            runtime_base: 0,
            doorbell_base: 0,
            event_ring: None,
            max_slots: 0,
            num_ports: 0,
        }
    }
    
    /// Inicializa el controlador XHCI
    pub fn initialize(&mut self) -> DriverResult<()> {
        serial_write_str("XHCI_CRATE: Iniciando controlador con crate xhci...\n");
        
        // 1. Habilitar MMIO y Bus Master en PCI
        self.pci_device.enable_mmio_and_bus_master();
        
        // 2. Obtener BAR0 (MMIO base)
        self.read_bars()?;
        
        // 3. Leer registros de capacidad
        self.read_capabilities()?;
        
        // 4. Resetear controlador
        self.reset_controller()?;
        
        // 5. Configurar rings (Command y Event)
        // TODO: Aquí usaremos el crate xhci
        self.setup_rings_with_crate()?;
        
        // 6. Iniciar controlador
        self.start_controller()?;
        
        serial_write_str("XHCI_CRATE: Controlador inicializado exitosamente\n");
        Ok(())
    }
    
    /// Lee los BARs del dispositivo PCI
    fn read_bars(&mut self) -> DriverResult<()> {
        let bars = self.pci_device.read_all_bars();
        
        serial_write_str(&alloc::format!("XHCI_CRATE: BAR0 raw: 0x{:08X}\n", bars[0]));
        
        let is_64bit = (bars[0] & 0x04) != 0;
        let is_mmio = (bars[0] & 0x01) == 0;
        
        if !is_mmio {
            return Err(crate::drivers::manager::DriverError::IoError);
        }
        
        if is_64bit {
            self.mmio_base = ((bars[0] & 0xFFFFFFF0) as u64) | (((bars[1] as u64) << 32));
        } else {
            self.mmio_base = (bars[0] & 0xFFFFFFF0) as u64;
        }
        
        serial_write_str(&alloc::format!(
            "XHCI_CRATE: MMIO Base @ 0x{:016X}\n", self.mmio_base
        ));
        
        Ok(())
    }
    
    /// Lee los registros de capacidad
    fn read_capabilities(&mut self) -> DriverResult<()> {
        unsafe {
            // CAPLENGTH: Longitud de los registros de capacidad
            let caplength = read_volatile(self.mmio_base as *const u8);
            self.operational_base = self.mmio_base + caplength as u64;
            
            // HCSPARAMS1: Parámetros estructurales 1
            let hcsparams1 = read_volatile((self.mmio_base + 0x04) as *const u32);
            self.max_slots = (hcsparams1 & 0xFF) as u8;
            self.num_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
            
            // DBOFF: Doorbell Array Offset
            let dboff = read_volatile((self.mmio_base + 0x14) as *const u32);
            self.doorbell_base = self.mmio_base + dboff as u64;
            
            // RTSOFF: Runtime Register Space Offset
            let rtsoff = read_volatile((self.mmio_base + 0x18) as *const u32);
            self.runtime_base = self.mmio_base + (rtsoff & !0x1F) as u64;
            
            serial_write_str(&alloc::format!(
                "XHCI_CRATE: Operational @ 0x{:016X}, Runtime @ 0x{:016X}\n",
                self.operational_base, self.runtime_base
            ));
            serial_write_str(&alloc::format!(
                "XHCI_CRATE: Max slots: {}, Ports: {}\n",
                self.max_slots, self.num_ports
            ));
        }
        
        Ok(())
    }
    
    /// Resetea el controlador
    fn reset_controller(&mut self) -> DriverResult<()> {
        serial_write_str("XHCI_CRATE: Reseteando controlador...\n");
        
        unsafe {
            // USBCMD: Detener el controlador (Run/Stop = 0)
            let usbcmd_ptr = self.operational_base as *mut u32;
            let mut usbcmd = read_volatile(usbcmd_ptr);
            usbcmd &= !0x01; // Clear RS bit
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que se detenga (HCHalted = 1)
            let usbsts_ptr = (self.operational_base + 0x04) as *const u32;
            let mut timeout = 0;
            while timeout < 100000 {
                let usbsts = read_volatile(usbsts_ptr);
                if (usbsts & 0x01) != 0 {
                    break;
                }
                timeout += 1;
                core::hint::spin_loop();
            }
            
            // USBCMD: Reset (HCRST = 1)
            usbcmd = read_volatile(usbcmd_ptr);
            usbcmd |= 0x02; // Set HCRST bit
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que complete el reset
            timeout = 0;
            while timeout < 100000 {
                let cmd = read_volatile(usbcmd_ptr);
                if (cmd & 0x02) == 0 {
                    break;
                }
                timeout += 1;
                core::hint::spin_loop();
            }
        }
        
        serial_write_str("XHCI_CRATE: Reset completado\n");
        Ok(())
    }
    
    /// Configura los rings usando el crate xhci
    fn setup_rings_with_crate(&mut self) -> DriverResult<()> {
        serial_write_str("XHCI_CRATE: Configurando rings con crate xhci...\n");
        
        // 1. Crear Event Ring usando memoria físicamente contigua
        serial_write_str("XHCI_CRATE: Creando Event Ring...\n");
        
        let event_ring = EventRing::new(EVENT_RING_SIZE)
            .map_err(|_| crate::drivers::manager::DriverError::IoError)?;
        
        let event_ring_phys = event_ring.physical_address();
        
        serial_write_str(&alloc::format!(
            "XHCI_CRATE: Event Ring creado @ 0x{:016X} ({} TRBs)\n",
            event_ring_phys, EVENT_RING_SIZE
        ));
        
        // 2. Configurar Event Ring Segment Table (ERST)
        serial_write_str("XHCI_CRATE: Configurando ERST...\n");
        
        unsafe {
            // Crear ERST Entry (alineado a 64 bytes)
            #[repr(C, align(64))]
            struct ERSTEntry {
                ring_segment_base: u64,
                ring_segment_size: u16,
                _reserved: [u8; 6],
            }
            
            static mut ERST_TABLE: ERSTEntry = ERSTEntry {
                ring_segment_base: 0,
                ring_segment_size: 0,
                _reserved: [0; 6],
            };
            
            ERST_TABLE.ring_segment_base = event_ring_phys;
            ERST_TABLE.ring_segment_size = EVENT_RING_SIZE as u16;
            
            let erst_phys = &ERST_TABLE as *const _ as u64;
            
            // Configurar registros del Interrupter 0
            let ir0_base = self.runtime_base + 0x20;
            
            // ERSTSZ
            write_volatile((ir0_base + 0x08) as *mut u32, 1);
            
            // ERSTBA
            write_volatile((ir0_base + 0x10) as *mut u64, erst_phys & !0x3F);
            
            // ERDP
            write_volatile((ir0_base + 0x18) as *mut u64, event_ring_phys);
            
            serial_write_str(&alloc::format!(
                "XHCI_CRATE: ERST @ 0x{:016X}, ERDP @ 0x{:016X}\n",
                erst_phys, event_ring_phys
            ));
        }
        
        // Guardar Event Ring
        self.event_ring = Some(event_ring);
        
        // 3. Configurar Command Ring (simplificado por ahora)
        serial_write_str("XHCI_CRATE: Configurando Command Ring...\n");
        
        unsafe {
            let cmd_ring_buffer = alloc::vec![0u128; COMMAND_RING_SIZE];
            let cmd_ring_addr = cmd_ring_buffer.as_ptr() as u64;
            core::mem::forget(cmd_ring_buffer);
            
            // Configurar CRCR
            write_volatile((self.operational_base + 0x18) as *mut u64, cmd_ring_addr | 0x01);
            
            serial_write_str(&alloc::format!(
                "XHCI_CRATE: Command Ring @ 0x{:016X}\n", cmd_ring_addr
            ));
        }
        
        serial_write_str("XHCI_CRATE: Rings configurados exitosamente\n");
        Ok(())
    }
    
    /// Inicia el controlador
    fn start_controller(&mut self) -> DriverResult<()> {
        serial_write_str("XHCI_CRATE: Iniciando controlador...\n");
        
        unsafe {
            // CONFIG: Configurar número de slots
            let config_ptr = (self.operational_base + 0x38) as *mut u32;
            write_volatile(config_ptr, self.max_slots as u32);
            
            // USBCMD: Iniciar (Run/Stop = 1)
            let usbcmd_ptr = self.operational_base as *mut u32;
            let mut usbcmd = read_volatile(usbcmd_ptr);
            usbcmd |= 0x01; // Set RS bit
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que arranque (HCHalted = 0)
            let usbsts_ptr = (self.operational_base + 0x04) as *const u32;
            let mut timeout = 0;
            while timeout < 100000 {
                let usbsts = read_volatile(usbsts_ptr);
                if (usbsts & 0x01) == 0 {
                    serial_write_str("XHCI_CRATE: Controlador iniciado\n");
                    return Ok(());
                }
                timeout += 1;
                core::hint::spin_loop();
            }
        }
        
        Err(crate::drivers::manager::DriverError::IoError)
    }
    
    /// Obtiene la dirección MMIO base
    pub fn get_mmio_base(&self) -> u64 {
        self.mmio_base
    }
    
    /// Obtiene la dirección base de runtime registers
    pub fn get_runtime_base(&self) -> u64 {
        self.runtime_base
    }
    
    /// Procesa eventos del Event Ring
    /// 
    /// Esta función debe llamarse periódicamente desde el main loop
    /// para procesar eventos del hardware USB.
    pub fn process_events(&mut self) -> usize {
        let mut events_processed = 0;
        
        if let Some(event_ring) = &mut self.event_ring {
            // Procesar hasta 16 eventos por llamada para evitar saturación
            for _ in 0..16 {
                match event_ring.pop() {
                    Some(event::Allowed::TransferEvent(trb)) => {
                        events_processed += 1;
                        Self::handle_transfer_event(trb);
                    }
                    Some(event::Allowed::CommandCompletion(trb)) => {
                        events_processed += 1;
                        Self::handle_command_completion(trb);
                    }
                    Some(event::Allowed::PortStatusChange(trb)) => {
                        events_processed += 1;
                        Self::handle_port_status_change(trb);
                    }
                    Some(_) => {
                        // Otros eventos ignorados por ahora
                        events_processed += 1;
                    }
                    None => {
                        // No hay más eventos
                        break;
                    }
                }
            }
            
            // Actualizar ERDP si procesamos eventos
            if events_processed > 0 {
                let new_erdp = event_ring.dequeue_pointer();
                unsafe {
                    let erdp_ptr = (self.runtime_base + 0x20 + 0x18) as *mut u64;
                    write_volatile(erdp_ptr, new_erdp | 0x08); // Bit 3 = EHB
                }
            }
        }
        
        events_processed
    }
    
    /// Maneja un Transfer Event (datos USB recibidos/enviados)
    fn handle_transfer_event(trb: event::TransferEvent) {
        let slot_id = trb.slot_id();
        let endpoint_id = trb.endpoint_id();
        let transfer_length = trb.trb_transfer_length();
        
        // Solo log por ahora
        serial_write_str(&alloc::format!(
            "XHCI_CRATE: Transfer Event - slot={}, ep={}, len={}\n",
            slot_id, endpoint_id, transfer_length
        ));
        
        // TODO: Leer datos del buffer y pasarlos a usb_hid
    }
    
    /// Maneja un Command Completion Event
    fn handle_command_completion(trb: event::CommandCompletion) {
        let completion_code = trb.completion_code();
        
        serial_write_str(&alloc::format!(
            "XHCI_CRATE: Command Completion - code={:?}\n",
            completion_code
        ));
        
        // TODO: Notificar a quien envió el comando
    }
    
    /// Maneja un Port Status Change Event
    fn handle_port_status_change(trb: event::PortStatusChange) {
        let port_id = trb.port_id();
        
        serial_write_str(&alloc::format!(
            "XHCI_CRATE: Port {} status changed\n",
            port_id
        ));
        
        // TODO: Iniciar enumeración del dispositivo
    }
    
    /// Toca el doorbell para un slot/endpoint específico
    /// 
    /// # Parámetros
    /// - `slot_id`: ID del slot (0 = command ring, 1-255 = device slots)
    /// - `target`: Target (típicamente endpoint ID o stream ID)
    /// 
    /// El doorbell notifica al controlador que hay trabajo pendiente en un ring.
    pub fn ring_doorbell(&self, slot_id: u8, target: u8) -> Result<(), &'static str> {
        if self.doorbell_base == 0 {
            return Err("Doorbell array no configurado");
        }
        
        unsafe {
            // Cada doorbell es un registro de 32 bits
            // Offset = doorbell_base + (slot_id * 4)
            let doorbell_ptr = (self.doorbell_base + (slot_id as u64 * 4)) as *mut u32;
            
            // Formato del doorbell value:
            // Bits 0-7: DB Target (endpoint ID o stream ID)
            // Bits 8-15: DB Stream ID (0 para no-streams)
            // Bits 16-31: Reserved
            let doorbell_value = target as u32;
            
            write_volatile(doorbell_ptr, doorbell_value);
            
            serial_write_str(&alloc::format!(
                "XHCI_CRATE: Doorbell {} tocado (target={})\n",
                slot_id, target
            ));
        }
        
        Ok(())
    }
    
    /// Toca el doorbell del Command Ring
    pub fn ring_command_doorbell(&self) -> Result<(), &'static str> {
        self.ring_doorbell(0, 0) // Slot 0, Target 0 = Command Ring
    }
    
    /// Toca el doorbell de un endpoint específico
    /// 
    /// # Parámetros
    /// - `slot_id`: ID del slot del dispositivo (1-255)
    /// - `endpoint_id`: ID del endpoint (1-31)
    pub fn ring_endpoint_doorbell(&self, slot_id: u8, endpoint_id: u8) -> Result<(), &'static str> {
        if slot_id == 0 {
            return Err("Slot 0 es para Command Ring, use ring_command_doorbell()");
        }
        
        if endpoint_id == 0 || endpoint_id > 31 {
            return Err("Endpoint ID debe estar entre 1 y 31");
        }
        
        self.ring_doorbell(slot_id, endpoint_id)
    }
}

/// Controlador XHCI global (opcional - para testing del nuevo controlador)
static mut XHCI_WITH_CRATE: Option<XhciControllerWithCrate> = None;

/// Inicializa el controlador XHCI global usando el nuevo implementation
pub fn init_xhci_with_crate(pci_device: PciDevice) -> Result<(), &'static str> {
    unsafe {
        if XHCI_WITH_CRATE.is_some() {
            return Err("XHCI ya inicializado");
        }
        
        let mut controller = XhciControllerWithCrate::new(pci_device);
        controller.initialize().map_err(|_| "Error inicializando XHCI")?;
        
        // Guardar MMIO base globalmente
        let mmio_base = controller.get_mmio_base();
        crate::drivers::usb_xhci_global::set_xhci_mmio_base(mmio_base);
        
        XHCI_WITH_CRATE = Some(controller);
    }
    
    Ok(())
}

/// Procesa eventos del controlador XHCI global
pub fn process_xhci_events() -> usize {
    unsafe {
        if let Some(controller) = &mut XHCI_WITH_CRATE {
            controller.process_events()
        } else {
            0
        }
    }
}

/// Obtiene información del controlador para debugging
pub fn get_xhci_info() -> Option<(u64, u64)> {
    unsafe {
        XHCI_WITH_CRATE.as_ref().map(|c| (c.get_mmio_base(), c.get_runtime_base()))
    }
}




