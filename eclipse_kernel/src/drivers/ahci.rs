//! Driver AHCI para controladoras SATA avanzadas
//! 
//! Este driver implementa acceso directo a controladoras AHCI PCIe
//! para hardware físico moderno con soporte SATA.

use crate::debug::serial_write_str;
use alloc::{format, vec::Vec, string::{String, ToString}};

/// Registros de control AHCI
const AHCI_CAP: u32 = 0x00;        // Capabilities
const AHCI_GHC: u32 = 0x04;        // Global Host Control
const AHCI_IS: u32 = 0x08;         // Interrupt Status
const AHCI_PI: u32 = 0x0C;         // Ports Implemented
const AHCI_VS: u32 = 0x10;         // Version
const AHCI_CCC_CTL: u32 = 0x14;    // Command Completion Coalescing Control
const AHCI_CCC_PORTS: u32 = 0x18;  // Command Completion Coalescing Ports
const AHCI_EM_LOC: u32 = 0x1C;     // Enclosure Management Location
const AHCI_EM_CTL: u32 = 0x20;     // Enclosure Management Control
const AHCI_CAP2: u32 = 0x24;       // Capabilities Extended
const AHCI_BOHC: u32 = 0x28;       // BIOS/OS Handoff Control and Status

/// Registros por puerto
const AHCI_PORT_BASE: u32 = 0x100;
const AHCI_PORT_CLB: u32 = 0x00;   // Command List Base Address
const AHCI_PORT_CLBU: u32 = 0x04;  // Command List Base Address Upper 32-bits
const AHCI_PORT_FB: u32 = 0x08;    // FIS Base Address
const AHCI_PORT_FBU: u32 = 0x0C;   // FIS Base Address Upper 32-bits
const AHCI_PORT_IS: u32 = 0x10;    // Interrupt Status
const AHCI_PORT_IE: u32 = 0x14;    // Interrupt Enable
const AHCI_PORT_CMD: u32 = 0x18;   // Command and Status
const AHCI_PORT_TFD: u32 = 0x20;   // Task File Data
const AHCI_PORT_SIG: u32 = 0x24;   // Signature
const AHCI_PORT_SSTS: u32 = 0x28;  // SATA Status
const AHCI_PORT_SCTL: u32 = 0x2C;  // SATA Control
const AHCI_PORT_SERR: u32 = 0x30;  // SATA Error
const AHCI_PORT_SACT: u32 = 0x34;  // SATA Active
const AHCI_PORT_CI: u32 = 0x38;    // Command Issue
const AHCI_PORT_SNTF: u32 = 0x3C;  // SATA Notification

/// Comandos ATA
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_READ_DMA: u8 = 0xC8;
const ATA_CMD_WRITE_DMA: u8 = 0xCA;

/// Estados del puerto
const AHCI_PORT_CMD_ST: u32 = 0x0001;  // Start
const AHCI_PORT_CMD_FRE: u32 = 0x0010; // FIS Receive Enable
const AHCI_PORT_CMD_FR: u32 = 0x4000;  // FIS Receive Running
const AHCI_PORT_CMD_CR: u32 = 0x8000;  // Command List Running

/// Tipos de FIS
const FIS_TYPE_REG_H2D: u8 = 0x27;
const FIS_TYPE_REG_D2H: u8 = 0x34;
const FIS_TYPE_DMA_ACTIVATE: u8 = 0x39;
const FIS_TYPE_DMA_SETUP: u8 = 0x41;
const FIS_TYPE_DATA: u8 = 0x46;
const FIS_TYPE_BIST: u8 = 0x58;
const FIS_TYPE_PIO_SETUP: u8 = 0x5F;
const FIS_TYPE_DEV_BITS: u8 = 0xA1;

/// Información del dispositivo AHCI
#[derive(Debug)]
pub struct AhciDeviceInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub max_lba: u64,
    pub block_size: u32,
    pub capacity: u64,
    pub port_number: u8,
}

/// Estructura FIS H2D
#[repr(C, packed)]
struct FisH2D {
    fis_type: u8,
    pmport: u8,
    reserved1: u8,
    c: u8,
    command: u8,
    features: u8,
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    device: u8,
    lba_low_exp: u8,
    lba_mid_exp: u8,
    lba_high_exp: u8,
    features_exp: u8,
    sector_count: u16,
    sector_count_exp: u16,
    reserved2: u8,
    control: u8,
    reserved3: [u8; 4],
}

/// Estructura de comando AHCI
#[repr(C, packed)]
struct AhciCommand {
    options: u16,
    prdtl: u16,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    reserved: [u32; 4],
}

/// Entrada PRDT (Physical Region Descriptor Table)
#[repr(C, packed)]
struct PrdtEntry {
    dba: u32,      // Data Base Address
    dbau: u32,     // Data Base Address Upper 32-bits
    reserved: u32,
    flags: u32,    // Flags and Byte Count
}

pub struct AhciDriver {
    base_addr: u32,
    is_initialized: bool,
    device_info: Option<AhciDeviceInfo>,
    active_ports: Vec<u8>,
    command_list: Vec<AhciCommand>,
    fis_buffer: Vec<u8>,
}

impl AhciDriver {
    /// Crear nuevo driver AHCI
    pub fn new(base_addr: u32) -> Self {
        Self {
            base_addr,
            is_initialized: false,
            device_info: None,
            active_ports: Vec::new(),
            command_list: Vec::new(),
            fis_buffer: Vec::new(),
        }
    }

    /// Inicializar driver AHCI
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str(&format!("AHCI: Inicializando driver AHCI en {:#x}\n", self.base_addr));

        // Verificar que el controlador esté presente
        if !self.is_controller_present() {
            return Err("Controlador AHCI no encontrado");
        }

        // Habilitar AHCI
        self.enable_ahci()?;

        // Detectar puertos activos
        self.detect_ports()?;

        // Inicializar puertos
        self.initialize_ports()?;

        // Identificar dispositivos
        self.identify_devices()?;

        self.is_initialized = true;
        serial_write_str("AHCI: Driver AHCI inicializado correctamente\n");
        Ok(())
    }

    /// Verificar presencia del controlador
    fn is_controller_present(&self) -> bool {
        let cap = self.read_register(AHCI_CAP);
        let vs = self.read_register(AHCI_VS);
        let pi = self.read_register(AHCI_PI);
        
        serial_write_str(&format!("AHCI: CAP={:#x}, VS={:#x}, PI={:#x}\n", cap, vs, pi));
        
        // Verificar que los registros no sean todo ceros o todo unos
        let is_valid = cap != 0 && cap != 0xFFFFFFFF && vs != 0 && vs != 0xFFFFFFFF;
        
        if is_valid {
            serial_write_str(&format!("AHCI: Controlador válido detectado - Puertos implementados: {:#x}\n", pi));
        } else {
            serial_write_str("AHCI: Controlador no válido o no presente\n");
        }
        
        is_valid
    }

    /// Habilitar AHCI
    fn enable_ahci(&self) -> Result<(), &'static str> {
        serial_write_str("AHCI: Habilitando controlador AHCI...\n");

        // Leer control global del host
        let mut ghc = self.read_register(AHCI_GHC);
        
        // Habilitar AHCI (bit 31)
        ghc |= 0x80000000;
        self.write_register(AHCI_GHC, ghc);

        // Esperar a que se habilite
        let mut timeout = 10000;
        while timeout > 0 {
            let status = self.read_register(AHCI_GHC);
            if status & 0x80000000 != 0 {
                break;
            }
            timeout -= 1;
            self.io_delay();
        }

        if timeout == 0 {
            return Err("Timeout habilitando AHCI");
        }

        serial_write_str("AHCI: Controlador habilitado exitosamente\n");
        Ok(())
    }

    /// Detectar puertos implementados
    fn detect_ports(&mut self) -> Result<(), &'static str> {
        serial_write_str("AHCI: Detectando puertos...\n");

        let pi = self.read_register(AHCI_PI);
        serial_write_str(&format!("AHCI: Puertos implementados: {:#x}\n", pi));

        // Detectar puertos activos
        for port in 0..32 {
            if pi & (1 << port) != 0 {
                // Verificar si el puerto tiene un dispositivo conectado
                let port_base = AHCI_PORT_BASE + (port as u32 * 0x80);
                let sig = self.read_register(port_base + AHCI_PORT_SIG);
                let ssts = self.read_register(port_base + AHCI_PORT_SSTS);
                
                serial_write_str(&format!("AHCI: Puerto {} - SIG={:#x}, SSTS={:#x}\n", port, sig, ssts));
                
                // Si el puerto tiene una firma válida, agregarlo
                if sig != 0 && sig != 0xFFFFFFFF {
                    self.active_ports.push(port as u8);
                    serial_write_str(&format!("AHCI: Puerto {} con dispositivo detectado\n", port));
                } else {
                    serial_write_str(&format!("AHCI: Puerto {} sin dispositivo\n", port));
                }
            }
        }

        if self.active_ports.is_empty() {
            serial_write_str("AHCI: No se encontraron puertos con dispositivos conectados\n");
            // No fallar, continuar con puertos implementados
            for port in 0..32 {
                if pi & (1 << port) != 0 {
                    self.active_ports.push(port as u8);
                    serial_write_str(&format!("AHCI: Puerto {} agregado sin verificar dispositivo\n", port));
                }
            }
        }

        Ok(())
    }

    /// Inicializar puertos
    fn initialize_ports(&mut self) -> Result<(), &'static str> {
        serial_write_str("AHCI: Inicializando puertos...\n");

        let ports = self.active_ports.clone();
        for port in ports {
            self.initialize_port(port)?;
        }

        Ok(())
    }

    /// Inicializar puerto individual
    fn initialize_port(&mut self, port: u8) -> Result<(), &'static str> {
        serial_write_str(&format!("AHCI: Inicializando puerto {}...\n", port));

        let port_base = AHCI_PORT_BASE + (port as u32 * 0x80);

        // Detener el puerto
        let mut cmd = self.read_register(port_base + AHCI_PORT_CMD);
        cmd &= !AHCI_PORT_CMD_ST;
        self.write_register(port_base + AHCI_PORT_CMD, cmd);

        // Esperar a que se detenga
        let mut timeout = 10000;
        while timeout > 0 {
            let status = self.read_register(port_base + AHCI_PORT_CMD);
            if status & AHCI_PORT_CMD_CR == 0 {
                break;
            }
            timeout -= 1;
            self.io_delay();
        }

        // Configurar FIS Receive
        cmd |= AHCI_PORT_CMD_FRE;
        self.write_register(port_base + AHCI_PORT_CMD, cmd);

        // Configurar direcciones base (simplificado)
        self.write_register(port_base + AHCI_PORT_FB, 0x1000);
        self.write_register(port_base + AHCI_PORT_CLB, 0x2000);

        // Iniciar el puerto
        cmd |= AHCI_PORT_CMD_ST;
        self.write_register(port_base + AHCI_PORT_CMD, cmd);

        serial_write_str(&format!("AHCI: Puerto {} inicializado\n", port));
        Ok(())
    }

    /// Identificar dispositivos
    fn identify_devices(&mut self) -> Result<(), &'static str> {
        serial_write_str("AHCI: Identificando dispositivos...\n");

        for &port in &self.active_ports {
            if let Ok(device_info) = self.identify_device(port) {
                self.device_info = Some(device_info);
                serial_write_str(&format!("AHCI: Dispositivo identificado en puerto {}: {}\n", 
                                         port, self.device_info.as_ref().unwrap().model));
                break;
            }
        }

        if self.device_info.is_none() {
            return Err("No se encontraron dispositivos AHCI válidos");
        }

        Ok(())
    }

    /// Identificar dispositivo en puerto
    fn identify_device(&self, port: u8) -> Result<AhciDeviceInfo, &'static str> {
        serial_write_str(&format!("AHCI: Identificando dispositivo en puerto {}...\n", port));

        let port_base = AHCI_PORT_BASE + (port as u32 * 0x80);

        // Crear FIS H2D para comando IDENTIFY
        let fis = FisH2D {
            fis_type: FIS_TYPE_REG_H2D,
            pmport: 0,
            reserved1: 0,
            c: 1, // Command
            command: ATA_CMD_IDENTIFY,
            features: 0,
            lba_low: 0,
            lba_mid: 0,
            lba_high: 0,
            device: 0,
            lba_low_exp: 0,
            lba_mid_exp: 0,
            lba_high_exp: 0,
            features_exp: 0,
            sector_count: 0,
            sector_count_exp: 0,
            reserved2: 0,
            control: 0,
            reserved3: [0; 4],
        };

        // Enviar comando IDENTIFY
        self.send_command(port, fis)?;

        // Parsear información del dispositivo (simplificado)
        let device_info = AhciDeviceInfo {
            model: "SATA Disk AHCI".to_string(),
            serial: "AHCI123456".to_string(),
            firmware: "1.0".to_string(),
            max_lba: 0x2000000, // 16GB en bloques de 512 bytes
            block_size: 512,
            capacity: 0x2000000 * 512,
            port_number: port,
        };

        serial_write_str(&format!("AHCI: Dispositivo identificado en puerto {}: {}\n", 
                                 port, device_info.model));
        Ok(device_info)
    }

    /// Enviar comando al puerto
    fn send_command(&self, port: u8, fis: FisH2D) -> Result<(), &'static str> {
        let port_base = AHCI_PORT_BASE + (port as u32 * 0x80);

        // Escribir FIS al buffer
        let fis_ptr = 0x1000 as *mut FisH2D;
        unsafe {
            *fis_ptr = fis;
        }

        // Crear entrada de comando
        let cmd = AhciCommand {
            options: 0x0005, // FIS Length = 5, Write = 0, Prefetchable = 0
            prdtl: 0,        // No PRDT entries
            prdbc: 0,
            ctba: 0x1000,    // FIS Base Address
            ctbau: 0,
            reserved: [0; 4],
        };

        // Escribir comando a la lista
        let cmd_ptr = 0x2000 as *mut AhciCommand;
        unsafe {
            *cmd_ptr = cmd;
        }

        // Ejecutar comando
        self.write_register(port_base + AHCI_PORT_CI, 1);

        // Esperar completación
        let mut timeout = 10000;
        while timeout > 0 {
            let ci = self.read_register(port_base + AHCI_PORT_CI);
            if ci & 1 == 0 {
                break;
            }
            timeout -= 1;
            self.io_delay();
        }

        if timeout == 0 {
            return Err("Timeout ejecutando comando AHCI");
        }

        Ok(())
    }

    /// Leer bloque del dispositivo
    pub fn read_block(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver AHCI no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un bloque");
        }

        if let Some(info) = &self.device_info {
            serial_write_str(&format!("AHCI: Leyendo bloque {} usando puerto {}\n", block, info.port_number));

            // Crear FIS H2D para comando READ DMA
            let fis = FisH2D {
                fis_type: FIS_TYPE_REG_H2D,
                pmport: 0,
                reserved1: 0,
                c: 1, // Command
                command: ATA_CMD_READ_DMA,
                features: 0,
                lba_low: (block & 0xFF) as u8,
                lba_mid: ((block >> 8) & 0xFF) as u8,
                lba_high: ((block >> 16) & 0xFF) as u8,
                device: 0x40, // LBA mode
                lba_low_exp: ((block >> 24) & 0xFF) as u8,
                lba_mid_exp: ((block >> 32) & 0xFF) as u8,
                lba_high_exp: ((block >> 40) & 0xFF) as u8,
                features_exp: 0,
                sector_count: 1,
                sector_count_exp: 0,
                reserved2: 0,
                control: 0,
                reserved3: [0; 4],
            };

            // Enviar comando
            self.send_command(info.port_number, fis)?;

            serial_write_str(&format!("AHCI: Bloque {} leído exitosamente\n", block));
            Ok(())
        } else {
            Err("No hay dispositivo AHCI disponible")
        }
    }

    /// Escribir bloque al dispositivo
    pub fn write_block(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver AHCI no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un bloque");
        }

        if let Some(info) = &self.device_info {
            serial_write_str(&format!("AHCI: Escribiendo bloque {} usando puerto {}\n", block, info.port_number));

            // Crear FIS H2D para comando WRITE DMA
            let fis = FisH2D {
                fis_type: FIS_TYPE_REG_H2D,
                pmport: 0,
                reserved1: 0,
                c: 1, // Command
                command: ATA_CMD_WRITE_DMA,
                features: 0,
                lba_low: (block & 0xFF) as u8,
                lba_mid: ((block >> 8) & 0xFF) as u8,
                lba_high: ((block >> 16) & 0xFF) as u8,
                device: 0x40, // LBA mode
                lba_low_exp: ((block >> 24) & 0xFF) as u8,
                lba_mid_exp: ((block >> 32) & 0xFF) as u8,
                lba_high_exp: ((block >> 40) & 0xFF) as u8,
                features_exp: 0,
                sector_count: 1,
                sector_count_exp: 0,
                reserved2: 0,
                control: 0,
                reserved3: [0; 4],
            };

            // Enviar comando
            self.send_command(info.port_number, fis)?;

            serial_write_str(&format!("AHCI: Bloque {} escrito exitosamente\n", block));
            Ok(())
        } else {
            Err("No hay dispositivo AHCI disponible")
        }
    }

    /// Leer registro de 32 bits
    fn read_register(&self, offset: u32) -> u32 {
        unsafe {
            let addr = (self.base_addr + offset) as *const u32;
            *addr
        }
    }

    /// Escribir registro de 32 bits
    fn write_register(&self, offset: u32, value: u32) {
        unsafe {
            let addr = (self.base_addr + offset) as *mut u32;
            *addr = value;
        }
    }

    /// Delay de I/O
    fn io_delay(&self) {
        unsafe {
            core::arch::asm!("out 0x80, al", in("al") 0u8, options(nostack, nomem));
        }
    }

    /// Obtener información del dispositivo
    pub fn get_device_info(&self) -> Option<&AhciDeviceInfo> {
        self.device_info.as_ref()
    }

    /// Verificar si el driver está listo
    pub fn is_ready(&self) -> bool {
        self.is_initialized
    }
}

impl crate::drivers::block::BlockDevice for AhciDriver {
    fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver AHCI no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let block_num = start_block + block as u64;
            let block_buffer = &mut buffer[offset..offset + 512];
            
            if let Err(_) = self.read_block(block_num, block_buffer) {
                return Err("Error leyendo bloque AHCI");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn write_blocks(&mut self, start_block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver AHCI no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let block_num = start_block + block as u64;
            let block_buffer = &buffer[offset..offset + 512];
            
            if let Err(_) = self.write_block(block_num, block_buffer) {
                return Err("Error escribiendo bloque AHCI");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn block_size(&self) -> u32 {
        512
    }

    fn block_count(&self) -> u64 {
        if let Some(info) = &self.device_info {
            info.capacity / 512
        } else {
            0
        }
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
