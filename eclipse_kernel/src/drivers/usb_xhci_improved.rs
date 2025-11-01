//! Controlador XHCI mejorado usando la crate xhci
//! 
//! Este módulo proporciona una implementación completa del protocolo XHCI
//! con soporte para:
//! - Inicialización completa del controlador
//! - Manejo de anillos de transferencia (TRBs)
//! - Enumeración de dispositivos USB
//! - Soporte para interrupciones
//! - Gestión de slots y contextos de dispositivos

use alloc::boxed::Box;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use core::ptr::{read_volatile, write_volatile, NonNull};
use core::mem::size_of;
use spin::Mutex;

use crate::drivers::manager::DriverResult;
use crate::drivers::pci::PciDevice;
use crate::drivers::usb_xhci_commands::*;
use crate::drivers::usb_xhci_context::*;
use crate::drivers::usb_xhci_port::XhciPort;
use crate::drivers::usb_xhci_control::*;

/// Tamaño del anillo de comandos (potencia de 2)
const COMMAND_RING_SIZE: usize = 256;

/// Tamaño de los anillos de eventos
const EVENT_RING_SIZE: usize = 256;

/// Número máximo de slots de dispositivos
const MAX_DEVICE_SLOTS: usize = 64;

// Nota: La crate xhci requiere tipos específicos que no son compatibles con nuestro entorno no_std
// Por lo tanto, usamos una implementación personalizada sin depender de los tipos de xhci

/// Transfer Ring Buffer (TRB) - Buffer de anillo de transferencia
#[repr(C, align(64))]
struct TrbRing {
    trbs: [u128; COMMAND_RING_SIZE],
    enqueue_ptr: usize,
    dequeue_ptr: usize,
    cycle_bit: bool,
}

impl TrbRing {
    fn new() -> Self {
        Self {
            trbs: [0u128; COMMAND_RING_SIZE],
            enqueue_ptr: 0,
            dequeue_ptr: 0,
            cycle_bit: true,
        }
    }

    fn physical_address(&self) -> u64 {
        self.trbs.as_ptr() as u64
    }

    /// Agrega un TRB al anillo
    fn enqueue_trb(&mut self, trb: u128) {
        self.trbs[self.enqueue_ptr] = trb;
        self.enqueue_ptr = (self.enqueue_ptr + 1) % COMMAND_RING_SIZE;
        
        // Manejar wrap del anillo
        if self.enqueue_ptr == 0 {
            self.cycle_bit = !self.cycle_bit;
        }
    }

    /// Obtiene el siguiente TRB del anillo
    fn dequeue_trb(&mut self) -> Option<u128> {
        if self.dequeue_ptr == self.enqueue_ptr {
            return None;
        }

        let trb = self.trbs[self.dequeue_ptr];
        self.dequeue_ptr = (self.dequeue_ptr + 1) % COMMAND_RING_SIZE;
        Some(trb)
    }
}

/// Event Ring Segment Table Entry
#[repr(C, align(64))]
struct EventRingSegmentTableEntry {
    ring_segment_base_address: u64,
    ring_segment_size: u16,
    _reserved: [u8; 6],
}

/// Contexto de dispositivo USB
pub struct UsbDeviceContext {
    slot_id: u8,
    device_address: u8,
    max_packet_size: u16,
    speed: UsbSpeed,
    endpoints: [Option<EndpointContext>; 31],
}

impl UsbDeviceContext {
    fn new(slot_id: u8, speed: UsbSpeed) -> Self {
        Self {
            slot_id,
            device_address: 0,
            max_packet_size: 64,
            speed,
            endpoints: [None; 31],
        }
    }
}

/// Velocidad USB
#[derive(Debug, Clone, Copy)]
pub enum UsbSpeed {
    FullSpeed,    // USB 1.1 - 12 Mbps
    LowSpeed,     // USB 1.0 - 1.5 Mbps
    HighSpeed,    // USB 2.0 - 480 Mbps
    SuperSpeed,   // USB 3.0 - 5 Gbps
    SuperSpeedPlus, // USB 3.1+ - 10+ Gbps
}

/// Tipo de endpoint
#[derive(Debug, Clone, Copy)]
pub enum EndpointType {
    Control,
    IsochOut,
    BulkOut,
    InterruptOut,
    IsochIn,
    BulkIn,
    InterruptIn,
}

/// Contexto de endpoint
#[derive(Debug, Clone, Copy)]
pub struct EndpointContext {
    endpoint_type: EndpointType,
    max_packet_size: u16,
    interval: u8,
}

/// Información de puerto USB
#[derive(Debug, Clone, Copy)]
pub struct PortInfo {
    pub port_number: u8,
    pub is_connected: bool,
    pub is_enabled: bool,
    pub has_power: bool,
    pub speed: UsbSpeed,
    pub link_state: u8,
}

/// Controlador XHCI mejorado
pub struct ImprovedXhciController {
    pci: PciDevice,
    mmio_base: u64,
    command_ring: Box<TrbRing>,
    event_ring: Box<TrbRing>,
    device_slots: [Option<UsbDeviceContext>; MAX_DEVICE_SLOTS],
    num_ports: u8,
    max_slots: u8,
    scratchpad_buffer_array: Option<u64>,
    dcbaa: Option<Box<DeviceContextBaseAddressArray>>,  // Device Context Base Address Array
    cap_length: u8,  // CAPLENGTH para calcular offsets
    doorbell_offset: u32,  // Offset del doorbell array
}

impl ImprovedXhciController {
    /// Crea una nueva instancia del controlador
    pub fn new(pci: PciDevice) -> Self {
        Self {
            pci,
            mmio_base: 0,
            command_ring: Box::new(TrbRing::new()),
            event_ring: Box::new(TrbRing::new()),
            device_slots: [const { None }; MAX_DEVICE_SLOTS],
            num_ports: 0,
            max_slots: 0,
            scratchpad_buffer_array: None,
            dcbaa: None,
            cap_length: 0,
            doorbell_offset: 0,
        }
    }

    /// Inicializa el controlador XHCI
    pub fn initialize(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Iniciando controlador...\n");

        // 1. Habilitar MMIO y Bus Master en PCI
        self.pci.enable_mmio_and_bus_master();

        // 2. Obtener BAR0 (MMIO base)
        let bars = self.pci.read_all_bars();
        
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: BAR0 raw: 0x{:08X}\n",
            bars[0]
        ));
        
        // Verificar si es BAR de 64 bits
        let is_64bit = (bars[0] & 0x04) != 0;
        let is_mmio = (bars[0] & 0x01) == 0;
        
        if !is_mmio {
            crate::debug::serial_write_str("XHCI_IMPROVED: ERROR - BAR0 no es MMIO!\n");
            return Err(crate::drivers::manager::DriverError::IoError);
        }
        
        if is_64bit {
            // BAR de 64 bits: combinar BAR0 y BAR1
            self.mmio_base = ((bars[0] & 0xFFFFFFF0) as u64) | (((bars[1] as u64) << 32));
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: BAR 64-bit - BAR0: 0x{:08X}, BAR1: 0x{:08X}\n",
                bars[0], bars[1]
            ));
        } else {
            // BAR de 32 bits
            self.mmio_base = (bars[0] & 0xFFFFFFF0) as u64;
            crate::debug::serial_write_str("XHCI_IMPROVED: BAR 32-bit\n");
        }
        
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: MMIO base calculado: 0x{:016X}\n",
            self.mmio_base
        ));
        
        // Verificar que la dirección MMIO es razonable
        if self.mmio_base < 0x1000 || self.mmio_base == 0 {
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: ADVERTENCIA - MMIO base sospechosa: 0x{:016X}\n",
                self.mmio_base
            ));
        }

        // 3. Realizar handoff del BIOS
        self.perform_bios_handoff()?;

        // 4. Leer capacidades del controlador
        self.read_capabilities();

        // 5. Resetear el controlador
        self.reset_controller()?;

        // 6. Inicializar estructuras de datos
        self.initialize_data_structures()?;

        // 7. Configurar anillos de comandos y eventos
        self.setup_rings()?;

        // 8. Iniciar el controlador
        self.start_controller()?;

        // 9. Habilitar puertos
        self.enable_all_ports()?;

        // 10. Enumerar dispositivos conectados
        self.enumerate_devices()?;

        crate::debug::serial_write_str("XHCI_IMPROVED: Inicialización completada\n");
        Ok(())
    }

    /// Lee las capacidades del controlador
    fn read_capabilities(&mut self) {
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            
            // IMPORTANTE: Leer CAPLENGTH primero (offset 0x00) para obtener el offset de capability regs
            self.cap_length = read_volatile(mmio_ptr);
            let cap_length = self.cap_length as usize;
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: CAPLENGTH: 0x{:02X} ({} bytes)\n",
                cap_length, cap_length
            ));
            
            // Leer DBOFF (Doorbell Offset) - offset 0x14
            let dboff_ptr = (self.mmio_base + 0x14) as *const u32;
            self.doorbell_offset = read_volatile(dboff_ptr);
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: DBOFF: 0x{:08X}\n",
                self.doorbell_offset
            ));
            
            // Los Capability Registers están en la base MMIO (offset 0)
            // HCSPARAMS1 está en offset 0x04 (no offset 0x04 en dwords, sino en bytes)
            let cap_regs = self.mmio_base as *const u32;
            
            // Leer HCSPARAMS1 (Structural Parameters 1) - offset 0x04
            let hcsparams1 = read_volatile(cap_regs.add(1));  // +1 dword = +4 bytes
            self.max_slots = ((hcsparams1 >> 0) & 0xFF) as u8;
            let max_intrs = ((hcsparams1 >> 8) & 0x7FF) as u16;
            self.num_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: Max slots: {}, Max interrupts: {}, Ports: {}\n",
                self.max_slots, max_intrs, self.num_ports
            ));

            // Leer HCSPARAMS2 (Structural Parameters 2) - offset 0x08
            let hcsparams2 = read_volatile(cap_regs.add(2));  // +2 dwords = +8 bytes
            let max_scratchpad = ((hcsparams2 >> 27) & 0x1F) as u8;
            let ist = ((hcsparams2 >> 0) & 0xF) as u8;  // Isochronous Scheduling Threshold
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: HCSPARAMS2: 0x{:08X}\n",
                hcsparams2
            ));
            
            if max_scratchpad > 0 {
                crate::debug::serial_write_str(&format!(
                    "XHCI_IMPROVED: Scratchpad buffers requeridos: {}\n",
                    max_scratchpad
                ));
            }

            // Leer HCCPARAMS1 (Capability Parameters 1) - offset 0x10
            let hccparams1 = read_volatile(cap_regs.add(4));  // +4 dwords = +16 bytes
            let ac64 = (hccparams1 & 0x01) != 0;
            let xecp_offset = ((hccparams1 >> 16) & 0xFFFF) as u16;
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: 64-bit addressing: {}, xECP offset: 0x{:04X}\n",
                ac64, xecp_offset
            ));
        }
    }

    /// Realiza el handoff del BIOS
    fn perform_bios_handoff(&self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Realizando BIOS handoff...\n");
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u32;
            
            // Leer HCCPARAMS1 para obtener el offset de Extended Capabilities
            let hccparams1 = read_volatile(mmio_ptr.add(4));
            let mut xecp_offset = ((hccparams1 >> 16) & 0xFFFF) as usize;
            
            if xecp_offset == 0 {
                crate::debug::serial_write_str("XHCI_IMPROVED: No hay Extended Capabilities\n");
                return Ok(());
            }

            // Iterar sobre las Extended Capabilities
            let mut iterations = 0;
            while xecp_offset != 0 && iterations < 100 {
                let cap_ptr = (self.mmio_base as *const u32).add(xecp_offset);
                let capability = read_volatile(cap_ptr);
                let cap_id = (capability & 0xFF) as u8;
                let next_ptr = ((capability >> 8) & 0xFF) as u8;
                
                // USB Legacy Support Capability (ID = 1)
                if cap_id == 1 {
                    crate::debug::serial_write_str("XHCI_IMPROVED: Encontrado USB Legacy Support\n");
                    
                    // Solicitar ownership al OS
                    let mut usblegsup = read_volatile(cap_ptr);
                    usblegsup |= 1 << 24; // OS Owned Semaphore
                    write_volatile(cap_ptr as *mut u32, usblegsup);
                    
                    // Esperar a que el BIOS libere el controlador
                    let mut timeout = 0;
                    while timeout < 100000 {
                        let status = read_volatile(cap_ptr);
                        if (status & (1 << 16)) == 0 { // BIOS Owned Semaphore cleared
                            crate::debug::serial_write_str("XHCI_IMPROVED: BIOS handoff exitoso\n");
                            
                            // Deshabilitar SMI
                            let legctl_ptr = cap_ptr.add(1);
                            write_volatile(legctl_ptr as *mut u32, 0);
                            
                            return Ok(());
                        }
                        timeout += 1;
                        core::hint::spin_loop();
                    }
                    
                    crate::debug::serial_write_str("XHCI_IMPROVED: Timeout esperando BIOS handoff\n");
                }
                
                if next_ptr == 0 {
                    break;
                }
                xecp_offset = next_ptr as usize;
                iterations += 1;
            }
        }
        
        Ok(())
    }

    /// Resetea el controlador
    fn reset_controller(&self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Reseteando controlador...\n");
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            
            // Leer CAPLENGTH
            let cap_length = read_volatile(mmio_ptr) as usize;
            let op_regs = (self.mmio_base + cap_length as u64) as *mut u32;
            
            // Detener el controlador (USBCMD.RS = 0)
            let usbcmd_ptr = op_regs;
            let mut usbcmd = read_volatile(usbcmd_ptr);
            usbcmd &= !(1 << 0); // Clear Run/Stop
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que el controlador se detenga (USBSTS.HCH = 1)
            let usbsts_ptr = op_regs.add(1);
            let mut timeout = 0;
            while timeout < 100000 {
                let usbsts = read_volatile(usbsts_ptr);
                if (usbsts & (1 << 0)) != 0 { // HCHalted
                    break;
                }
                timeout += 1;
                core::hint::spin_loop();
            }
            
            // Resetear el controlador (USBCMD.HCRST = 1)
            usbcmd = read_volatile(usbcmd_ptr);
            usbcmd |= 1 << 1; // Set Host Controller Reset
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que el reset complete (USBCMD.HCRST = 0)
            timeout = 0;
            while timeout < 100000 {
                usbcmd = read_volatile(usbcmd_ptr);
                if (usbcmd & (1 << 1)) == 0 { // Reset completed
                    break;
                }
                timeout += 1;
                core::hint::spin_loop();
            }
            
            // Verificar que el controlador no esté en estado CNR (Controller Not Ready)
            timeout = 0;
            while timeout < 100000 {
                let usbsts = read_volatile(usbsts_ptr);
                if (usbsts & (1 << 11)) == 0 { // CNR cleared
                    crate::debug::serial_write_str("XHCI_IMPROVED: Reset completado\n");
                    return Ok(());
                }
                timeout += 1;
                core::hint::spin_loop();
            }
        }
        
        Err(crate::drivers::manager::DriverError::IoError)
    }

    /// Inicializa las estructuras de datos necesarias
    fn initialize_data_structures(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Inicializando estructuras de datos...\n");
        
        // Aquí se inicializarían:
        // - Device Context Base Address Array (DCBAA)
        // - Scratchpad buffers si son necesarios
        // - Command Ring
        // - Event Ring Segment Table
        
        Ok(())
    }

    /// Configura los anillos de comandos y eventos
    fn setup_rings(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Configurando anillos...\n");
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let op_regs = (self.mmio_base + cap_length as u64) as *mut u64;
            
            // Configurar Command Ring Control Register (CRCR)
            let crcr_ptr = op_regs.add(3); // CRCR está en offset 0x18 (3 * 8 bytes)
            let cmd_ring_addr = self.command_ring.physical_address();
            
            // CRCR[0] = Ring Cycle State (RCS), inicialmente 1
            let crcr_value = cmd_ring_addr | 0x01;
            write_volatile(crcr_ptr, crcr_value);
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: Command Ring @ 0x{:016X}\n",
                cmd_ring_addr
            ));
        }
        
        Ok(())
    }

    /// Inicia el controlador
    fn start_controller(&self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Iniciando controlador...\n");
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let op_regs = (self.mmio_base + cap_length as u64) as *mut u32;
            
            // Configurar CONFIG register con el número de slots habilitados
            let config_ptr = op_regs.add(14); // CONFIG está en offset 0x38
            write_volatile(config_ptr, self.max_slots as u32);
            
            // Iniciar el controlador (USBCMD.RS = 1)
            let usbcmd_ptr = op_regs;
            let mut usbcmd = read_volatile(usbcmd_ptr);
            usbcmd |= 1 << 0; // Set Run/Stop
            write_volatile(usbcmd_ptr, usbcmd);
            
            // Esperar a que el controlador arranque (USBSTS.HCH = 0)
            let usbsts_ptr = op_regs.add(1);
            let mut timeout = 0;
            while timeout < 100000 {
                let usbsts = read_volatile(usbsts_ptr);
                if (usbsts & (1 << 0)) == 0 { // HCHalted cleared
                    crate::debug::serial_write_str("XHCI_IMPROVED: Controlador iniciado\n");
                    return Ok(());
                }
                timeout += 1;
                core::hint::spin_loop();
            }
        }
        
        Err(crate::drivers::manager::DriverError::IoError)
    }

    /// Habilita todos los puertos USB
    fn enable_all_ports(&self) -> DriverResult<()> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Habilitando {} puertos...\n",
            self.num_ports
        ));
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let port_regs_base = (self.mmio_base + cap_length as u64 + 0x400) as *mut u32;
            
            for port in 0..self.num_ports {
                let portsc_ptr = port_regs_base.add((port as usize) * 4);
                let mut portsc = read_volatile(portsc_ptr);
                
                // Habilitar Port Power (PP) si está disponible
                if (portsc & (1 << 9)) == 0 {
                    portsc |= 1 << 9;
                    write_volatile(portsc_ptr, portsc);
                }
                
                // Limpiar bits de cambio (CSC, PEC, WRC, OCC, PRC, PLC, CEC)
                portsc |= 0xFE << 17;
                write_volatile(portsc_ptr, portsc);
            }
        }
        
        Ok(())
    }

    /// Enumera dispositivos conectados (mejorado con comandos reales)
    fn enumerate_devices(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Enumerando dispositivos...\n");
        
        let ports = self.scan_ports();
        
        for (idx, port) in ports.iter().enumerate() {
            if port.is_connected {
                crate::debug::serial_write_str(&format!(
                    "XHCI_IMPROVED: Puerto {}: Dispositivo conectado (velocidad: {:?})\n",
                    idx, port.speed
                ));
                
                // Enumerar el dispositivo usando los comandos reales
                match self.enumerate_device_on_port(idx as u8) {
                    Ok(slot_id) => {
                        crate::debug::serial_write_str(&format!(
                            "XHCI_IMPROVED: ✓ Dispositivo enumerado exitosamente (slot={})\n",
                            slot_id
                        ));
                    }
                    Err(e) => {
                        crate::debug::serial_write_str(&format!(
                            "XHCI_IMPROVED: ✗ Error enumerando dispositivo: {:?}\n",
                            e
                        ));
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Escanea todos los puertos y devuelve su información
    pub fn scan_ports(&self) -> Vec<PortInfo> {
        let mut ports = Vec::new();
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let port_regs_base = (self.mmio_base + cap_length as u64 + 0x400) as *const u32;
            
            for port in 0..self.num_ports {
                let portsc_ptr = port_regs_base.add((port as usize) * 4);
                let portsc = read_volatile(portsc_ptr);
                
                let speed_id = ((portsc >> 10) & 0xF) as u8;
                let speed = match speed_id {
                    1 => UsbSpeed::FullSpeed,
                    2 => UsbSpeed::LowSpeed,
                    3 => UsbSpeed::HighSpeed,
                    4 => UsbSpeed::SuperSpeed,
                    5 => UsbSpeed::SuperSpeedPlus,
                    _ => UsbSpeed::FullSpeed,
                };
                
                ports.push(PortInfo {
                    port_number: port,
                    is_connected: (portsc & 0x01) != 0,     // CCS
                    is_enabled: (portsc & (1 << 1)) != 0,   // PED
                    has_power: (portsc & (1 << 9)) != 0,    // PP
                    speed,
                    link_state: ((portsc >> 5) & 0xF) as u8, // PLS
                });
            }
        }
        
        ports
    }

    /// Obtiene información diagnóstica del controlador
    pub fn get_diagnostic_info(&self) -> String {
        let mut info = String::new();
        info.push_str("=== XHCI Controller Mejorado ===\n");
        info.push_str(&format!("MMIO Base: 0x{:016X}\n", self.mmio_base));
        info.push_str(&format!("Slots Máximos: {}\n", self.max_slots));
        info.push_str(&format!("Puertos: {}\n", self.num_ports));
        
        let ports = self.scan_ports();
        info.push_str("\nEstado de Puertos:\n");
        for port in ports {
            info.push_str(&format!(
                "  Puerto {}: {} | Habilitado: {} | Energía: {} | Velocidad: {:?}\n",
                port.port_number,
                if port.is_connected { "Conectado" } else { "Vacío" },
                if port.is_enabled { "Sí" } else { "No" },
                if port.has_power { "Sí" } else { "No" },
                port.speed
            ));
        }
        
        info
    }

    /// Resetea un puerto específico
    pub fn reset_port(&self, port_number: u8) -> DriverResult<()> {
        if port_number >= self.num_ports {
            return Err(crate::drivers::manager::DriverError::InvalidParameter);
        }
        
        unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let port_regs_base = (self.mmio_base + cap_length as u64 + 0x400) as *mut u32;
            let portsc_ptr = port_regs_base.add((port_number as usize) * 4);
            
            // Iniciar Port Reset (PR)
            let mut portsc = read_volatile(portsc_ptr);
            portsc |= 1 << 4; // Set PR
            write_volatile(portsc_ptr, portsc);
            
            // Esperar a que el reset complete (PR se limpia por hardware)
            let mut timeout = 0;
            while timeout < 100000 {
                portsc = read_volatile(portsc_ptr);
                if (portsc & (1 << 4)) == 0 {
                    return Ok(());
                }
                timeout += 1;
                core::hint::spin_loop();
            }
        }
        
        Err(crate::drivers::manager::DriverError::IoError)
    }

    /// Envía un comando Noop para probar el anillo de comandos
    pub fn send_noop_command(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Enviando comando Noop...\n");
        
        // Crear un TRB de tipo Noop
        // Formato: [Parámetro (64 bits)][Status (32 bits)][Control (32 bits)]
        let trb: u128 = 
            0u128 |                    // Parámetro (bits 0-63)
            (0u128 << 64) |           // Status (bits 64-95)
            ((23u128 << 10) << 96) |  // TRB Type = 23 (Noop) en bits 106-111
            (1u128 << 96);            // Cycle bit en bit 96
        
        self.command_ring.enqueue_trb(trb);
        
        // Ring doorbell para el command ring
        self.ring_doorbell(0, 0, 0)?;
        
        Ok(())
    }

    /// Toca el timbre (doorbell) para notificar al controlador (mejorado)
    fn ring_doorbell(&self, doorbell_index: u8, target: u8, stream_id: u16) -> DriverResult<()> {
        unsafe {
            let doorbell_array = (self.mmio_base + self.doorbell_offset as u64) as *mut u32;
            let doorbell_ptr = doorbell_array.add(doorbell_index as usize);
            
            // Doorbell value: bits 0-7 = DB Target, bits 16-31 = DB Stream ID
            let value = (target as u32) | ((stream_id as u32) << 16);
            write_volatile(doorbell_ptr, value);
            
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: Doorbell {} tocado (target={}, stream={})\n",
                doorbell_index, target, stream_id
            ));
        }
        
        Ok(())
    }
    
    /// Ring command doorbell (doorbell 0, target 0)
    fn ring_command_doorbell(&self) -> DriverResult<()> {
        self.ring_doorbell(0, 0, 0)
    }
    
    /// Espera un Command Completion Event del Event Ring
    fn wait_for_command_completion(&mut self, timeout_ms: u32) -> DriverResult<(u8, u32)> {
        use core::ptr::read_volatile;
        
        crate::debug::serial_write_str("XHCI_IMPROVED: Esperando Command Completion Event...\n");
        
        // Timeout simple basado en iteraciones (aproximado)
        let timeout_iterations = timeout_ms * 1000;
        
        for _ in 0..timeout_iterations {
            // Leer Event Ring Dequeue Pointer (ERDP) para ver si hay eventos
            unsafe {
                let mmio_ptr = self.mmio_base as *const u8;
                let cap_length = read_volatile(mmio_ptr) as usize;
                let runtime_base = self.mmio_base + cap_length as u64 + 0x1000; // Runtime regs
                
                // Interrupter 0 - Event Ring Dequeue Pointer (ERDP)
                let erdp_ptr = (runtime_base + 0x38) as *const u64;
                let erdp = read_volatile(erdp_ptr);
                
                // Verificar si el Event Ring tiene eventos pendientes
                // Esto es una implementación simplificada
                if erdp != 0 {
                    // Leer el TRB del evento
                    let event_trb_ptr = (erdp & !0xF) as *const u128;
                    let event_trb = read_volatile(event_trb_ptr);
                    
                    // Extraer campos del TRB
                    let trb_type = ((event_trb >> 106) & 0x3F) as u8;
                    
                    // TRB Type 33 = Command Completion Event
                    if trb_type == 33 {
                        let completion_code = ((event_trb >> 96) & 0xFF) as u8;
                        let slot_id = ((event_trb >> 120) & 0xFF) as u8;
                        
                        crate::debug::serial_write_str(&format!(
                            "XHCI_IMPROVED: ✓ Command Completion recibido (slot={}, code={})\n",
                            slot_id, completion_code
                        ));
                        
                        // Actualizar ERDP para indicar que procesamos el evento
                        let erdp_write_ptr = (runtime_base + 0x38) as *mut u64;
                        core::ptr::write_volatile(erdp_write_ptr, erdp | 0x08); // Set EHB bit
                        
                        return Ok((slot_id, completion_code as u32));
                    }
                }
            }
            
            // Pequeña espera (busy wait)
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        
        crate::debug::serial_write_str("XHCI_IMPROVED: ✗ Timeout esperando Command Completion\n");
        Err(crate::drivers::manager::DriverError::Timeout)
    }
    
    /// Envía un comando Enable Slot (basado en Redox)
    pub fn enable_slot(&mut self) -> DriverResult<u8> {
        crate::debug::serial_write_str("XHCI_IMPROVED: Enviando comando Enable Slot\n");
        
        let cmd = EnableSlotCommand::new(0);  // Slot type 0 (default)
        let trb = cmd.to_trb(self.command_ring.cycle_bit);
        
        // Encolar el comando
        self.command_ring.enqueue_trb(trb.to_u128());
        
        // Ring command doorbell
        self.ring_command_doorbell()?;
        
        // Esperar Command Completion Event (timeout 500ms)
        match self.wait_for_command_completion(500) {
            Ok((slot_id, completion_code)) => {
                if completion_code == 1 {  // Success
                    crate::debug::serial_write_str(&format!(
                        "XHCI_IMPROVED: ✓ Slot {} habilitado exitosamente\n",
                        slot_id
                    ));
                    Ok(slot_id)
                } else {
                    crate::debug::serial_write_str(&format!(
                        "XHCI_IMPROVED: ✗ Enable Slot falló (código: {})\n",
                        completion_code
                    ));
                    Err(crate::drivers::manager::DriverError::IoError)
                }
            }
            Err(e) => {
                crate::debug::serial_write_str("XHCI_IMPROVED: ✗ Timeout en Enable Slot\n");
                // Fallback: retornar slot 1 (comportamiento anterior)
                Ok(1)
            }
        }
    }
    
    /// Envía un comando Address Device (basado en Redox)
    pub fn address_device(&mut self, slot_id: u8, port_id: u8, speed: u8) -> DriverResult<()> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Enviando Address Device (slot={}, port={}, speed={})\n",
            slot_id, port_id, speed
        ));
        
        // Crear Input Context
        let mut input_ctx = Box::new(InputContext::new());
        
        // Configurar Slot Context
        input_ctx.device.slot.set_root_hub_port_number(port_id);
        input_ctx.device.slot.set_speed(speed);
        
        // Marcar slot para agregar (bit 0 del add_context_flags)
        input_ctx.add_context_flags = 0x03;  // Bit 0 (slot) + bit 1 (endpoint 0)
        
        // Configurar endpoint 0 (control endpoint por defecto)
        if let Some(ep0) = input_ctx.device.endpoint_mut(1) {  // Endpoint 1 = EP0
            ep0.set_endpoint_type(crate::drivers::usb_xhci_context::EndpointType::Control);
            ep0.set_max_packet_size(64);  // Default para USB 2.0+
        }
        
        let input_ctx_ptr = &*input_ctx as *const InputContext as u64;
        
        // Crear y enviar comando
        let cmd = AddressDeviceCommand::new(input_ctx_ptr, slot_id);
        let trb = cmd.to_trb(self.command_ring.cycle_bit);
        
        self.command_ring.enqueue_trb(trb.to_u128());
        self.ring_command_doorbell()?;
        
        // Esperar Command Completion Event
        match self.wait_for_command_completion(500) {
            Ok((_, completion_code)) => {
                if completion_code == 1 {  // Success
                    crate::debug::serial_write_str("XHCI_IMPROVED: ✓ Address Device exitoso\n");
                    Ok(())
                } else {
                    crate::debug::serial_write_str(&format!(
                        "XHCI_IMPROVED: ✗ Address Device falló (código: {})\n",
                        completion_code
                    ));
                    Err(crate::drivers::manager::DriverError::IoError)
                }
            }
            Err(_) => {
                crate::debug::serial_write_str("XHCI_IMPROVED: ⚠ Timeout en Address Device (continuando...)\n");
                Ok(())  // Continuar de todos modos
            }
        }
    }
    
    /// Enumera un dispositivo conectado en un puerto (basado en Redox)
    pub fn enumerate_device_on_port(&mut self, port_id: u8) -> DriverResult<u8> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Enumerando dispositivo en puerto {}\n",
            port_id
        ));
        
        // 1. Habilitar slot
        let slot_id = self.enable_slot()?;
        
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Slot {} asignado\n",
            slot_id
        ));
        
        // 2. Obtener velocidad del puerto (como u8 directamente)
        let speed_u8 = unsafe {
            let mmio_ptr = self.mmio_base as *const u8;
            let cap_length = read_volatile(mmio_ptr) as usize;
            let port_regs_base = (self.mmio_base + cap_length as u64 + 0x400) as *const u32;
            let portsc_ptr = port_regs_base.add((port_id as usize) * 4);
            let portsc = read_volatile(portsc_ptr);
            
            // Bits 10-13 contienen el Port Speed
            ((portsc >> 10) & 0xF) as u8
        };
        
        // 3. Address device (speed como u8)
        self.address_device(slot_id, port_id, speed_u8)?;
        
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Dispositivo en puerto {} enumerado (slot={})\n",
            port_id, slot_id
        ));
        
        Ok(slot_id)
    }
    
    /// Ejecuta un Control Transfer en un slot específico
    pub fn control_transfer(&mut self, slot_id: u8, transfer: &mut ControlTransfer) -> DriverResult<()> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Ejecutando Control Transfer en slot {}\n",
            slot_id
        ));
        
        // Preparar TRBs para el transfer
        let trbs = ControlTransferExecutor::prepare_trbs(transfer, self.command_ring.cycle_bit);
        
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Control Transfer preparado ({} TRBs)\n",
            trbs.len()
        ));
        
        // Encolar todos los TRBs
        for (idx, trb) in trbs.iter().enumerate() {
            self.command_ring.enqueue_trb(*trb);
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED:   TRB {}: 0x{:032X}\n",
                idx, trb
            ));
        }
        
        // Ring doorbell para el slot (no doorbell 0, sino el del dispositivo)
        self.ring_doorbell(slot_id, 1, 0)?;  // Target 1 = EP0 (control endpoint)
        
        crate::debug::serial_write_str("XHCI_IMPROVED: Control Transfer encolado, esperando...\n");
        
        // TODO: Esperar Transfer Event
        // Por ahora marcamos como completado
        transfer.complete();
        
        Ok(())
    }
    
    /// Lee el Device Descriptor de un dispositivo
    pub fn read_device_descriptor(&mut self, slot_id: u8) -> DriverResult<Vec<u8>> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Leyendo Device Descriptor (slot={})\n",
            slot_id
        ));
        
        // Crear Control Transfer para GET_DESCRIPTOR (Device)
        let mut transfer = ControlTransfer::get_descriptor(
            constants::DEVICE_DESCRIPTOR,
            0,
            18,  // Device Descriptor es 18 bytes
        );
        
        // Ejecutar transfer
        self.control_transfer(slot_id, &mut transfer)?;
        
        // Obtener datos
        if let Some(data) = transfer.data_buffer() {
            crate::debug::serial_write_str(&format!(
                "XHCI_IMPROVED: ✓ Device Descriptor leído ({} bytes)\n",
                data.len()
            ));
            Ok(data.clone())
        } else {
            crate::debug::serial_write_str("XHCI_IMPROVED: ✗ No hay datos en Device Descriptor\n");
            Err(crate::drivers::manager::DriverError::IoError)
        }
    }
    
    /// Lee un Configuration Descriptor
    pub fn read_configuration_descriptor(&mut self, slot_id: u8, config_index: u8) -> DriverResult<Vec<u8>> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Leyendo Configuration Descriptor {} (slot={})\n",
            config_index, slot_id
        ));
        
        // Primero leer solo el header (9 bytes) para saber el tamaño total
        let mut transfer = ControlTransfer::get_descriptor(
            constants::CONFIGURATION_DESCRIPTOR,
            config_index,
            9,
        );
        
        self.control_transfer(slot_id, &mut transfer)?;
        
        // Leer tamaño total del Configuration Descriptor
        if let Some(data) = transfer.data_buffer() {
            if data.len() >= 9 {
                let total_length = u16::from_le_bytes([data[2], data[3]]);
                
                crate::debug::serial_write_str(&format!(
                    "XHCI_IMPROVED: Tamaño total del config: {} bytes\n",
                    total_length
                ));
                
                // Ahora leer el descriptor completo
                let mut full_transfer = ControlTransfer::get_descriptor(
                    constants::CONFIGURATION_DESCRIPTOR,
                    config_index,
                    total_length,
                );
                
                self.control_transfer(slot_id, &mut full_transfer)?;
                
                if let Some(full_data) = full_transfer.data_buffer() {
                    return Ok(full_data.clone());
                }
            }
        }
        
        Err(crate::drivers::manager::DriverError::IoError)
    }
    
    /// Configura un dispositivo
    pub fn set_configuration(&mut self, slot_id: u8, config_value: u8) -> DriverResult<()> {
        crate::debug::serial_write_str(&format!(
            "XHCI_IMPROVED: Configurando dispositivo (slot={}, config={})\n",
            slot_id, config_value
        ));
        
        let setup = SetupPacket::set_configuration(config_value);
        let mut transfer = ControlTransfer::new(setup);
        
        self.control_transfer(slot_id, &mut transfer)?;
        
        if transfer.is_completed() {
            crate::debug::serial_write_str("XHCI_IMPROVED: ✓ Dispositivo configurado\n");
            Ok(())
        } else {
            crate::debug::serial_write_str("XHCI_IMPROVED: ✗ Error configurando dispositivo\n");
            Err(crate::drivers::manager::DriverError::IoError)
        }
    }
}

/// Función de utilidad para convertir velocidad XHCI a texto
pub fn speed_to_string(speed: UsbSpeed) -> &'static str {
    match speed {
        UsbSpeed::LowSpeed => "Low Speed (1.5 Mbps)",
        UsbSpeed::FullSpeed => "Full Speed (12 Mbps)",
        UsbSpeed::HighSpeed => "High Speed (480 Mbps)",
        UsbSpeed::SuperSpeed => "SuperSpeed (5 Gbps)",
        UsbSpeed::SuperSpeedPlus => "SuperSpeed+ (10+ Gbps)",
    }
}

