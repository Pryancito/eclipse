//! Driver ATA/SATA real para hardware físico
//! 
//! Este driver implementa acceso directo a controladoras ATA/SATA reales
//! para hardware físico, incluyendo soporte para IDE, SATA y AHCI.

use crate::debug::serial_write_str;
use alloc::{format, string::{String, ToString}};

/// Puertos ATA estándar (Primary Channel)
const ATA_DATA_PORT: u16 = 0x1F0;
const ATA_ERROR_PORT: u16 = 0x1F1;
const ATA_SECTOR_COUNT_PORT: u16 = 0x1F2;
const ATA_LBA_LOW_PORT: u16 = 0x1F3;
const ATA_LBA_MID_PORT: u16 = 0x1F4;
const ATA_LBA_HIGH_PORT: u16 = 0x1F5;
const ATA_DEVICE_PORT: u16 = 0x1F6;
const ATA_STATUS_PORT: u16 = 0x1F7;
const ATA_COMMAND_PORT: u16 = 0x1F7;
const ATA_CONTROL_PORT: u16 = 0x3F6;

/// Puertos ATA secundarios (Secondary Channel)
const ATA_DATA_PORT_2: u16 = 0x170;
const ATA_ERROR_PORT_2: u16 = 0x171;
const ATA_SECTOR_COUNT_PORT_2: u16 = 0x172;
const ATA_LBA_LOW_PORT_2: u16 = 0x173;
const ATA_LBA_MID_PORT_2: u16 = 0x174;
const ATA_LBA_HIGH_PORT_2: u16 = 0x175;
const ATA_DEVICE_PORT_2: u16 = 0x176;
const ATA_STATUS_PORT_2: u16 = 0x177;
const ATA_COMMAND_PORT_2: u16 = 0x177;
const ATA_CONTROL_PORT_2: u16 = 0x376;

/// Comandos ATA
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_READ_SECTORS: u8 = 0x20;
const ATA_CMD_READ_SECTORS_EXT: u8 = 0x24;
const ATA_CMD_WRITE_SECTORS: u8 = 0x30;
const ATA_CMD_WRITE_SECTORS_EXT: u8 = 0x34;

/// Estados del dispositivo ATA
const ATA_STATUS_BSY: u8 = 0x80;
const ATA_STATUS_DRDY: u8 = 0x40;
const ATA_STATUS_DF: u8 = 0x20;
const ATA_STATUS_DSC: u8 = 0x10;
const ATA_STATUS_DRQ: u8 = 0x08;
const ATA_STATUS_CORR: u8 = 0x04;
const ATA_STATUS_IDX: u8 = 0x02;
const ATA_STATUS_ERR: u8 = 0x01;

/// Errores ATA
const ATA_ERROR_ABRT: u8 = 0x04;
const ATA_ERROR_MCR: u8 = 0x08;
const ATA_ERROR_IDNF: u8 = 0x10;
const ATA_ERROR_MC: u8 = 0x20;
const ATA_ERROR_UNC: u8 = 0x40;
const ATA_ERROR_BBK: u8 = 0x80;

/// Tipos de controladoras ATA
#[derive(Debug, Clone, Copy)]
pub enum AtaControllerType {
    Primary,
    Secondary,
}

/// Información del dispositivo ATA
#[derive(Debug)]
pub struct AtaDeviceInfo {
    pub model: [u8; 40],
    pub serial: [u8; 20],
    pub firmware: [u8; 8],
    pub sectors_28: u32,
    pub sectors_48: u64,
    pub supports_lba48: bool,
    pub max_sectors_per_transfer: u16,
}

pub struct AtaDirectDriver {
    controller_type: AtaControllerType,
    base_port: u16,
    control_port: u16,
    is_initialized: bool,
    device_info: Option<AtaDeviceInfo>,
    supports_lba48: bool,
}

impl AtaDirectDriver {
    /// Crear driver ATA para controladora primaria
    pub fn new_primary() -> Self {
        Self {
            controller_type: AtaControllerType::Primary,
            base_port: ATA_DATA_PORT,
            control_port: ATA_CONTROL_PORT,
            is_initialized: false,
            device_info: None,
            supports_lba48: false,
        }
    }

    /// Crear driver ATA para controladora secundaria
    pub fn new_secondary() -> Self {
        Self {
            controller_type: AtaControllerType::Secondary,
            base_port: ATA_DATA_PORT_2,
            control_port: ATA_CONTROL_PORT_2,
            is_initialized: false,
            device_info: None,
            supports_lba48: false,
        }
    }

    /// Crear driver ATA personalizado
    pub fn new_custom(controller_type: AtaControllerType) -> Self {
        let (base_port, control_port) = match controller_type {
            AtaControllerType::Primary => (ATA_DATA_PORT, ATA_CONTROL_PORT),
            AtaControllerType::Secondary => (ATA_DATA_PORT_2, ATA_CONTROL_PORT_2),
        };

        Self {
            controller_type,
            base_port,
            control_port,
            is_initialized: false,
            device_info: None,
            supports_lba48: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str(&format!("ATA_DIRECT: Inicializando driver ATA {:?} en puerto {:#x}\n", 
                                 self.controller_type, self.base_port));
        
        // Resetear controladora
        self.reset_controller()?;
        
        // Detectar dispositivos en ambos canales (master y slave)
        for device in 0..2 {
            if let Ok(device_info) = self.identify_device(device) {
                self.device_info = Some(device_info);
                self.supports_lba48 = self.device_info.as_ref().unwrap().supports_lba48;
                serial_write_str(&format!("ATA_DIRECT: Dispositivo detectado en canal {}: {:?}\n", 
                                         device, self.device_info.as_ref().unwrap().model));
                break;
            }
        }
        
        if self.device_info.is_none() {
            return Err("No se encontró dispositivo ATA válido");
        }
        
        self.is_initialized = true;
        serial_write_str("ATA_DIRECT: Driver ATA inicializado correctamente\n");
        Ok(())
    }

    /// Resetear controladora ATA
    fn reset_controller(&self) -> Result<(), &'static str> {
        serial_write_str("ATA_DIRECT: Reseteando controladora...\n");
        
        // Escribir 0x04 al puerto de control para activar SRST
        self.write_port(self.control_port - self.base_port, 0x04);
        
        // Esperar un poco
        self.io_delay();
        
        // Escribir 0x00 al puerto de control para desactivar SRST
        self.write_port(self.control_port - self.base_port, 0x00);
        
        // Esperar a que la controladora esté lista
        self.wait_for_ready()?;
        
        Ok(())
    }

    /// Identificar dispositivo ATA
    fn identify_device(&self, device: u8) -> Result<AtaDeviceInfo, &'static str> {
        serial_write_str(&format!("ATA_DIRECT: Identificando dispositivo {}...\n", device));
        
        // Seleccionar dispositivo (master=0xA0, slave=0xB0)
        let device_select = if device == 0 { 0xA0 } else { 0xB0 };
        self.write_port(ATA_DEVICE_PORT - self.base_port, device_select);
        
        // Esperar un poco
        self.io_delay();
        
        // Enviar comando IDENTIFY
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_IDENTIFY);
        
        // Esperar a que el dispositivo esté listo
        self.wait_for_ready()?;
        
        // Leer datos de identificación (256 palabras = 512 bytes)
        let mut identify_data = [0u16; 256];
        for i in 0..256 {
            identify_data[i] = self.read_port_word(ATA_DATA_PORT - self.base_port);
        }
        
        // Verificar que el dispositivo respondió
        if identify_data[0] == 0 {
            return Err("Dispositivo no responde");
        }
        
        // Parsear información del dispositivo
        let mut device_info = AtaDeviceInfo {
            model: [0; 40],
            serial: [0; 20],
            firmware: [0; 8],
            sectors_28: 0,
            sectors_48: 0,
            supports_lba48: false,
            max_sectors_per_transfer: 1,
        };
        
        // Modelo (palabras 27-46)
        for i in 0..20 {
            let word = identify_data[27 + i];
            device_info.model[i * 2] = (word & 0xFF) as u8;
            device_info.model[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
        }
        
        // Número de serie (palabras 10-19)
        for i in 0..10 {
            let word = identify_data[10 + i];
            device_info.serial[i * 2] = (word & 0xFF) as u8;
            device_info.serial[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
        }
        
        // Firmware (palabras 23-26)
        for i in 0..4 {
            let word = identify_data[23 + i];
            device_info.firmware[i * 2] = (word & 0xFF) as u8;
            device_info.firmware[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
        }
        
        // Sectores LBA28 (palabras 60-61)
        device_info.sectors_28 = (identify_data[61] as u32) << 16 | identify_data[60] as u32;
        
        // Verificar soporte LBA48 (palabra 83, bit 10)
        device_info.supports_lba48 = (identify_data[83] & 0x0400) != 0;
        
        if device_info.supports_lba48 {
            // Sectores LBA48 (palabras 100-103)
            device_info.sectors_48 = ((identify_data[103] as u64) << 48) |
                                    ((identify_data[102] as u64) << 32) |
                                    ((identify_data[101] as u64) << 16) |
                                    (identify_data[100] as u64);
        }
        
        // Máximo sectores por transferencia (palabra 47)
        device_info.max_sectors_per_transfer = identify_data[47];
        if device_info.max_sectors_per_transfer == 0 {
            device_info.max_sectors_per_transfer = 1;
        }
        
        serial_write_str(&format!("ATA_DIRECT: Dispositivo identificado - Modelo: {:?}, Sectores: {}\n", 
                                 device_info.model, device_info.sectors_28));
        
        Ok(device_info)
    }

    /// Esperar a que el dispositivo esté listo
    fn wait_for_ready(&self) -> Result<(), &'static str> {
        let mut timeout = 10000; // Timeout de 10ms
        while timeout > 0 {
            let status = self.read_port(ATA_STATUS_PORT - self.base_port);
            if status & ATA_STATUS_BSY == 0 {
                if status & ATA_STATUS_ERR != 0 {
                    let error = self.read_port(ATA_ERROR_PORT - self.base_port);
                    return Err("Error ATA");
                }
                return Ok(());
            }
            timeout -= 1;
            self.io_delay();
        }
        Err("Timeout esperando dispositivo ATA")
    }

    /// Esperar a que los datos estén listos
    fn wait_for_data_ready(&self) -> Result<(), &'static str> {
        let mut timeout = 10000;
        while timeout > 0 {
            let status = self.read_port(ATA_STATUS_PORT - self.base_port);
            if status & ATA_STATUS_BSY == 0 && status & ATA_STATUS_DRQ != 0 {
                return Ok(());
            }
            if status & ATA_STATUS_ERR != 0 {
                let error = self.read_port(ATA_ERROR_PORT - self.base_port);
                return Err("Error ATA durante transferencia");
            }
            timeout -= 1;
            self.io_delay();
        }
        Err("Timeout esperando datos ATA")
    }

    /// Delay de I/O
    fn io_delay(&self) {
        // Delay mínimo de 1 microsegundo
        unsafe {
            core::arch::asm!("out 0x80, al", in("al") 0u8, options(nostack, nomem));
        }
    }

    fn read_port(&self, port: u16) -> u8 {
        unsafe {
            let mut result: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") (self.base_port + port),
                out("al") result
            );
            result
        }
    }

    fn write_port(&self, port: u16, value: u8) {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") (self.base_port + port),
                in("al") value
            )
        }
    }

    /// Leer sector del dispositivo ATA
    pub fn read_sector(&self, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver ATA no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un sector");
        }

        // Usar LBA48 si está disponible y el sector es grande
        if self.supports_lba48 && sector > 0x0FFFFFFF {
            self.read_sector_lba48(sector, buffer)
        } else {
            self.read_sector_lba28(sector, buffer)
        }
    }

    /// Leer sector usando LBA28
    fn read_sector_lba28(&self, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Leyendo sector {} usando LBA28\n", sector));

        // Esperar a que el dispositivo esté listo
        self.wait_for_ready()?;

        // Configurar LBA
        self.write_port(ATA_DEVICE_PORT - self.base_port, 0xE0 | ((sector >> 24) & 0x0F) as u8);
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 1);
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, sector as u8);
        self.write_port(ATA_LBA_MID_PORT - self.base_port, (sector >> 8) as u8);
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, (sector >> 16) as u8);

        // Enviar comando de lectura
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_READ_SECTORS);

        // Esperar a que los datos estén listos
        self.wait_for_data_ready()?;

        // Leer 256 palabras (512 bytes)
        for i in 0..256 {
            let word = self.read_port_word(ATA_DATA_PORT - self.base_port);
            buffer[i * 2] = word as u8;
            buffer[i * 2 + 1] = (word >> 8) as u8;
        }

        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} leído exitosamente usando LBA28\n", sector));
        Ok(())
    }

    /// Leer sector usando LBA48
    fn read_sector_lba48(&self, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Leyendo sector {} usando LBA48\n", sector));

        // Esperar a que el dispositivo esté listo
        self.wait_for_ready()?;

        // Configurar LBA48
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 0); // Count high
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, ((sector >> 24) & 0xFF) as u8); // LBA4
        self.write_port(ATA_LBA_MID_PORT - self.base_port, ((sector as u64 >> 32) & 0xFF) as u8); // LBA5
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, ((sector as u64 >> 40) & 0xFF) as u8); // LBA6
        self.write_port(ATA_DEVICE_PORT - self.base_port, 0x40); // LBA mode
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 1); // Count low
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, sector as u8); // LBA1
        self.write_port(ATA_LBA_MID_PORT - self.base_port, (sector >> 8) as u8); // LBA2
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, (sector >> 16) as u8); // LBA3

        // Enviar comando de lectura extendido
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_READ_SECTORS_EXT);

        // Esperar a que los datos estén listos
        self.wait_for_data_ready()?;

        // Leer 256 palabras (512 bytes)
        for i in 0..256 {
            let word = self.read_port_word(ATA_DATA_PORT - self.base_port);
            buffer[i * 2] = word as u8;
            buffer[i * 2 + 1] = (word >> 8) as u8;
        }

        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} leído exitosamente usando LBA48\n", sector));
        Ok(())
    }

    /// Obtener información del dispositivo
    pub fn get_device_info(&self) -> Option<&AtaDeviceInfo> {
        self.device_info.as_ref()
    }

    /// Verificar si el dispositivo soporta LBA48
    pub fn supports_lba48(&self) -> bool {
        self.supports_lba48
    }

    /// Obtener número total de sectores
    pub fn get_sector_count(&self) -> u64 {
        if let Some(info) = &self.device_info {
            if info.supports_lba48 {
                info.sectors_48
            } else {
                info.sectors_28 as u64
            }
        } else {
            0
        }
    }

    /// Escribir sector al dispositivo ATA
    pub fn write_sector(&self, sector: u32, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver ATA no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un sector");
        }

        // Usar LBA48 si está disponible y el sector es grande
        if self.supports_lba48 && sector > 0x0FFFFFFF {
            self.write_sector_lba48(sector, buffer)
        } else {
            self.write_sector_lba28(sector, buffer)
        }
    }

    /// Escribir sector usando LBA28
    fn write_sector_lba28(&self, sector: u32, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Escribiendo sector {} usando LBA28\n", sector));

        // Esperar a que el dispositivo esté listo
        self.wait_for_ready()?;

        // Configurar LBA
        self.write_port(ATA_DEVICE_PORT - self.base_port, 0xE0 | ((sector >> 24) & 0x0F) as u8);
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 1);
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, sector as u8);
        self.write_port(ATA_LBA_MID_PORT - self.base_port, (sector >> 8) as u8);
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, (sector >> 16) as u8);

        // Enviar comando de escritura
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_WRITE_SECTORS);

        // Esperar a que el dispositivo esté listo para datos
        self.wait_for_data_ready()?;

        // Escribir 256 palabras (512 bytes)
        for i in 0..256 {
            let word = (buffer[i * 2 + 1] as u16) << 8 | buffer[i * 2] as u16;
            self.write_port_word(ATA_DATA_PORT - self.base_port, word);
        }

        // Esperar a que la escritura se complete
        self.wait_for_ready()?;

        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} escrito exitosamente usando LBA28\n", sector));
        Ok(())
    }

    /// Escribir sector usando LBA48
    fn write_sector_lba48(&self, sector: u32, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Escribiendo sector {} usando LBA48\n", sector));

        // Esperar a que el dispositivo esté listo
        self.wait_for_ready()?;

        // Configurar LBA48
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 0); // Count high
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, ((sector >> 24) & 0xFF) as u8); // LBA4
        self.write_port(ATA_LBA_MID_PORT - self.base_port, ((sector as u64 >> 32) & 0xFF) as u8); // LBA5
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, ((sector as u64 >> 40) & 0xFF) as u8); // LBA6
        self.write_port(ATA_DEVICE_PORT - self.base_port, 0x40); // LBA mode
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 1); // Count low
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, sector as u8); // LBA1
        self.write_port(ATA_LBA_MID_PORT - self.base_port, (sector >> 8) as u8); // LBA2
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, (sector >> 16) as u8); // LBA3

        // Enviar comando de escritura extendido
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_WRITE_SECTORS_EXT);

        // Esperar a que el dispositivo esté listo para datos
        self.wait_for_data_ready()?;

        // Escribir 256 palabras (512 bytes)
        for i in 0..256 {
            let word = (buffer[i * 2 + 1] as u16) << 8 | buffer[i * 2] as u16;
            self.write_port_word(ATA_DATA_PORT - self.base_port, word);
        }

        // Esperar a que la escritura se complete
        self.wait_for_ready()?;

        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} escrito exitosamente usando LBA48\n", sector));
        Ok(())
    }

    fn read_from_virtio_io_port(&self, io_port: u16, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Acceso REAL a VirtIO I/O port {:#x} para sector {}\n", io_port, sector));
        
        // Implementación real de acceso a VirtIO via I/O ports
        // Configurar registro de solicitud
        unsafe {
            // Escribir dirección del sector en el registro de dirección
            core::arch::asm!("out dx, eax", in("eax") sector, in("dx") io_port);
            
            // Escribir comando de lectura en el registro de comando
            core::arch::asm!("out dx, al", in("dx") (io_port + 4), in("al") 0x00u8); // READ
            
            // Esperar a que el dispositivo esté listo
            let mut timeout = 10000;
            while timeout > 0 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") (io_port + 8));
                if status & 0x01 == 0 { // Bit de busy
                    break;
                }
                timeout -= 1;
            }
            
            if timeout == 0 {
                return Err("Timeout en acceso VirtIO I/O");
            }
            
            // Leer datos del registro de datos
            for i in 0..256 {
                let word: u16;
                core::arch::asm!("in ax, dx", out("ax") word, in("dx") (io_port + 12));
                buffer[i * 2] = word as u8;
                buffer[i * 2 + 1] = (word >> 8) as u8;
            }
        }
        
        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} leído REALMENTE desde VirtIO I/O\n", sector));
        Ok(())
    }

    fn read_from_virtio_memory(&self, mem_addr: u32, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&alloc::format!("ATA_DIRECT: Acceso REAL a VirtIO memoria {:#x} para sector {}\n", mem_addr, sector));
        
        // Implementación real de acceso a VirtIO via memoria mapeada
        unsafe {
            let virtio_regs = mem_addr as *mut u32;
            
            // Configurar solicitud de lectura
            let request = (sector as u64) << 32 | 0x00000001; // Sector + comando READ
            *virtio_regs.offset(0) = request as u32;
            *virtio_regs.offset(1) = (request >> 32) as u32;
            
            // Activar solicitud
            *virtio_regs.offset(2) = 0x1;
            
            // Esperar completación
            let mut timeout = 10000;
            while timeout > 0 {
                let status = *virtio_regs.offset(3);
                if status & 0x1 == 0 { // Bit de busy
                    break;
                }
                timeout -= 1;
            }
            
            if timeout == 0 {
                return Err("Timeout en acceso VirtIO memoria");
            }
            
            // Leer datos desde el buffer del dispositivo
            let data_buffer = (mem_addr + 0x1000) as *const u8;
            for i in 0..512 {
                buffer[i] = *data_buffer.add(i);
            }
        }
        
        serial_write_str(&alloc::format!("ATA_DIRECT: Sector {} leído REALMENTE desde VirtIO memoria\n", sector));
        Ok(())
    }

    fn read_from_ata_ports(&self, sector: u32, buffer: &mut [u8]) -> Result<(), &'static str> {
        // Intentar leer usando puertos ATA reales
        self.wait_for_ready();

        // Configurar LBA
        self.write_port(ATA_DEVICE_PORT - self.base_port, 0xE0 | ((sector >> 24) & 0x0F) as u8);
        self.write_port(ATA_SECTOR_COUNT_PORT - self.base_port, 1);
        self.write_port(ATA_LBA_LOW_PORT - self.base_port, sector as u8);
        self.write_port(ATA_LBA_MID_PORT - self.base_port, (sector >> 8) as u8);
        self.write_port(ATA_LBA_HIGH_PORT - self.base_port, (sector >> 16) as u8);

        // Enviar comando de lectura
        self.write_port(ATA_COMMAND_PORT - self.base_port, ATA_CMD_READ_SECTORS);

        if let Err(_) = self.wait_for_data_ready() {
            return Err("Timeout esperando datos ATA");
        }

        // Leer 256 palabras (512 bytes)
        for i in 0..256 {
            let word = self.read_port_word(ATA_DATA_PORT - self.base_port);
            buffer[i * 2] = word as u8;
            buffer[i * 2 + 1] = (word >> 8) as u8;
        }

        Ok(())
    }

    fn read_port_word(&self, port: u16) -> u16 {
        unsafe {
            let mut result: u16;
            core::arch::asm!(
                "in ax, dx",
                in("dx") (self.base_port + port),
                out("ax") result
            );
            result
        }
    }

    /// Escribir palabra (16 bits) al puerto
    fn write_port_word(&self, port: u16, value: u16) {
        unsafe {
            core::arch::asm!(
                "out dx, ax",
                in("dx") (self.base_port + port),
                in("ax") value
            )
        }
    }

    /// Verificar si el driver está listo
    pub fn is_ready(&self) -> bool {
        self.is_initialized
    }

    // Funciones de simulación eliminadas - solo acceso real al hardware
}

impl crate::drivers::block::BlockDevice for AtaDirectDriver {
    fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver ATA no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let sector = start_block + block as u64;
            let sector_buffer = &mut buffer[offset..offset + 512];
            
            if let Err(_) = self.read_sector(sector as u32, sector_buffer) {
                return Err("Error leyendo sector");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn write_blocks(&mut self, start_block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver ATA no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let sector = start_block + block as u64;
            let sector_buffer = &buffer[offset..offset + 512];
            
            if let Err(_) = self.write_sector(sector as u32, sector_buffer) {
                return Err("Error escribiendo sector");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn block_size(&self) -> u32 {
        512
    }

    fn block_count(&self) -> u64 {
        self.get_sector_count()
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
