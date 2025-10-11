use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;

use crate::debug::serial_write_str;
use crate::drivers::ata_direct::AtaDirectDriver;
use crate::drivers::ahci::SataAhciDriver;
use crate::drivers::ide_modern::IdeModernDriver;
use crate::filesystem::fat32::Fat32DeviceInfo;
use crate::filesystem::eclipsefs::{EclipseFSDeviceInfo, EclipseFSWrapper};
use crate::vfs::Vfs;

/// Tipos de controladores de almacenamiento soportados
#[derive(Debug, Clone, PartialEq)]
pub enum StorageControllerType {
    ATA,
    NVMe,
    AHCI,
    VirtIO,
    IDE,
}

/// Información de un dispositivo PCI
#[derive(Debug, Clone)]
struct PciDeviceInfo {
    vendor_id: u16,
    device_id: u16,
    class_code: u8,
    subclass_code: u8,
    prog_if: u8,
}

/// Información de un dispositivo de almacenamiento
#[derive(Debug, Clone)]
pub struct StorageDeviceInfo {
    pub name: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub capacity: u64,
    pub controller_type: StorageControllerType,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Información de una partición
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub device_name: String,
    pub partition_index: u32,
    pub start_lba: u64,
    pub size_lba: u64,
    pub partition_type: u8,
    pub filesystem_type: String,
    pub bootable: bool,
}

/// Gestor de dispositivos de almacenamiento
pub struct StorageManager {
    devices: Vec<StorageDeviceInfo>,
    partitions: Vec<PartitionInfo>,
    is_ready: AtomicBool,
}

impl StorageManager {
    /// Crear nueva instancia del StorageManager
    pub fn new() -> Self {
        serial_write_str("STORAGE_MANAGER: Inicializando nuevo StorageManager sin RAID\n");
        
        Self {
            devices: Vec::new(),
            partitions: Vec::new(),
            is_ready: AtomicBool::new(false),
        }
    }

    /// Inicializar el StorageManager
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Iniciando detección de dispositivos de almacenamiento\n");
        
        // Detectar dispositivos PCI de almacenamiento
        self.detect_storage_devices()?;
        
        // Detectar particiones en cada dispositivo
        self.detect_partitions()?;
        
        // Marcar como listo
        self.is_ready.store(true, Ordering::SeqCst);
        
        serial_write_str(&format!("STORAGE_MANAGER: Inicialización completa - {} dispositivos, {} particiones\n", 
                                 self.devices.len(), self.partitions.len()));
        
        Ok(())
    }

    /// Detectar dispositivos de almacenamiento desde PCI
    fn detect_storage_devices(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando dispositivos PCI de almacenamiento\n");
        
        // Detectar dispositivos PCI reales
        self.detect_pci_storage_devices()?;
        
        // Si no se encontraron dispositivos, crear dispositivos de prueba
        if self.devices.is_empty() {
            serial_write_str("STORAGE_MANAGER: No se encontraron dispositivos PCI, usando dispositivos de prueba\n");
            self.create_test_devices();
        }
        
        Ok(())
    }

    /// Detectar dispositivos PCI reales
    fn detect_pci_storage_devices(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Escaneando bus PCI para dispositivos de almacenamiento\n");
        
        // Escanear bus PCI (implementación simplificada)
        for bus in 0..8 {
            for device in 0..32 {
                for function in 0..8 {
                    if let Ok(pci_info) = self.read_pci_config(bus, device, function) {
                        if self.is_storage_controller(&pci_info) {
                            serial_write_str(&format!("STORAGE_MANAGER: Encontrado controlador de almacenamiento en {:02X}:{:02X}.{}\n", 
                                                     bus, device, function));
                            
                            let storage_device = self.create_storage_device_from_pci(pci_info, bus, device, function);
                            self.devices.push(storage_device);
                        }
                    }
                }
            }
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: Detectados {} dispositivos de almacenamiento\n", self.devices.len()));
        Ok(())
    }

    /// Leer configuración PCI
    fn read_pci_config(&self, bus: u8, device: u8, function: u8) -> Result<PciDeviceInfo, &'static str> {
        // TODO: Implementar lectura PCI real
        // Por ahora, simular algunos dispositivos conocidos
        if bus == 0 && device == 1 && function == 1 {
            // Simular Intel SATA Controller
            return Ok(PciDeviceInfo {
                vendor_id: 0x8086,
                device_id: 0x2822,
                class_code: 0x01,
                subclass_code: 0x06,
                prog_if: 0x01,
            });
        }
        
        Err("Dispositivo no encontrado")
    }

    /// Verificar si es un controlador de almacenamiento
    fn is_storage_controller(&self, pci_info: &PciDeviceInfo) -> bool {
        // Clase 0x01 = Mass Storage Controller
        pci_info.class_code == 0x01 && (
            pci_info.subclass_code == 0x01 || // IDE Controller
            pci_info.subclass_code == 0x06 || // SATA Controller
            pci_info.subclass_code == 0x08    // NVMe Controller
        )
    }

    /// Crear dispositivo de almacenamiento desde información PCI
    fn create_storage_device_from_pci(&self, pci_info: PciDeviceInfo, bus: u8, device: u8, function: u8) -> StorageDeviceInfo {
        let controller_type = match pci_info.subclass_code {
            0x01 => StorageControllerType::IDE,
            0x06 => StorageControllerType::AHCI,
            0x08 => StorageControllerType::NVMe,
            _ => StorageControllerType::ATA,
        };

        let device_name = match controller_type {
            StorageControllerType::AHCI => format!("/dev/sd{}", (b'a' + self.devices.len() as u8) as char),
            StorageControllerType::IDE => format!("/dev/hd{}", (b'a' + self.devices.len() as u8) as char),
            _ => format!("/dev/nvme{}", self.devices.len()),
        };

        StorageDeviceInfo {
            name: device_name,
            model: format!("PCI {:04X}:{:04X}", pci_info.vendor_id, pci_info.device_id),
            serial: format!("PCI-{:02X}:{:02X}.{}", bus, device, function),
            firmware: "PCI-FW".to_string(),
            capacity: 1073741824, // 1GB por defecto
            controller_type,
            vendor_id: pci_info.vendor_id,
            device_id: pci_info.device_id,
            bus,
            device,
            function,
        }
    }

    /// Crear dispositivos de prueba
    fn create_test_devices(&mut self) {
        serial_write_str("STORAGE_MANAGER: Creando dispositivos de prueba\n");
        
        // Dispositivo SATA de ejemplo
        let sata_device = StorageDeviceInfo {
            name: "/dev/sda".to_string(),
            model: "Test SATA Drive".to_string(),
            serial: "SATA-12345".to_string(),
            firmware: "FW-1.0".to_string(),
            capacity: 1073741824, // 1GB
            controller_type: StorageControllerType::AHCI,
            vendor_id: 0x8086,
            device_id: 0x2822,
            bus: 0,
            device: 1,
            function: 1,
        };
        
        self.devices.push(sata_device);
        
        serial_write_str(&format!("STORAGE_MANAGER: Creado dispositivo: {}\n", self.devices[0].name));
    }

    /// Detectar particiones en todos los dispositivos
    fn detect_partitions(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando particiones\n");
        
        for device in &self.devices {
            serial_write_str(&format!("STORAGE_MANAGER: Analizando particiones en {}\n", device.name));
            
            // Intentar detectar GPT primero
            if let Ok(partitions) = self.detect_gpt_partitions(device) {
                serial_write_str(&format!("STORAGE_MANAGER: Encontradas {} particiones GPT en {}\n", partitions.len(), device.name));
                self.partitions.extend(partitions);
            } else if let Ok(partitions) = self.detect_mbr_partitions(device) {
                serial_write_str(&format!("STORAGE_MANAGER: Encontradas {} particiones MBR en {}\n", partitions.len(), device.name));
                self.partitions.extend(partitions);
            } else {
                serial_write_str(&format!("STORAGE_MANAGER: No se encontraron particiones en {}, usando particiones de prueba\n", device.name));
                self.create_test_partitions(device);
            }
        }
        
        Ok(())
    }

    /// Detectar particiones GPT
    fn detect_gpt_partitions(&self, device: &StorageDeviceInfo) -> Result<Vec<PartitionInfo>, &'static str> {
        let mut buffer = [0u8; 512];
        let mut partitions = Vec::new();
        
        // Leer GPT Header (sector 1)
        self.read_device_sector(&device.name, 1, &mut buffer)?;
        
        // Verificar firma GPT
        if &buffer[0..8] != b"EFI PART" {
            return Err("No es una tabla GPT válida");
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: GPT válido encontrado en {}\n", device.name));
        
        // Leer tabla de particiones (sector 2)
        self.read_device_sector(&device.name, 2, &mut buffer)?;
        
        // Procesar entradas de particiones GPT (hasta 4 entradas por sector)
        for i in 0..4 {
            let offset = i * 128; // Cada entrada GPT es de 128 bytes
            if offset + 128 > buffer.len() {
                break;
            }
            
            let partition_entry = &buffer[offset..offset + 128];
            
            // Verificar si la entrada está vacía (GUID de tipo todo ceros)
            if partition_entry[0..16].iter().all(|&b| b == 0) {
                continue;
            }
            
            // Leer información de la partición
            let start_lba = u64::from_le_bytes([
                partition_entry[32], partition_entry[33], partition_entry[34], partition_entry[35],
                partition_entry[36], partition_entry[37], partition_entry[38], partition_entry[39],
            ]);
            
            let end_lba = u64::from_le_bytes([
                partition_entry[40], partition_entry[41], partition_entry[42], partition_entry[43],
                partition_entry[44], partition_entry[45], partition_entry[46], partition_entry[47],
            ]);
            
            if start_lba == 0 || end_lba == 0 {
                continue;
            }
            
            let size_lba = end_lba - start_lba + 1;
            let partition_type = self.get_gpt_partition_type(&partition_entry[0..16]);
            let filesystem_type = self.detect_filesystem_type(device, start_lba)?;
            
            let partition = PartitionInfo {
                device_name: device.name.clone(),
                partition_index: partitions.len() as u32 + 1,
                start_lba,
                size_lba,
                partition_type: 0x00, // GPT no usa partition_type como MBR
                filesystem_type,
                bootable: false, // GPT no usa flag bootable como MBR
            };
            
            partitions.push(partition);
            serial_write_str(&format!("STORAGE_MANAGER: Partición GPT {}: {} ({} sectores)\n", 
                                     partitions.len(), filesystem_type, size_lba));
        }
        
        Ok(partitions)
    }

    /// Detectar particiones MBR
    fn detect_mbr_partitions(&self, device: &StorageDeviceInfo) -> Result<Vec<PartitionInfo>, &'static str> {
        let mut buffer = [0u8; 512];
        let mut partitions = Vec::new();
        
        // Leer MBR (sector 0)
        self.read_device_sector(&device.name, 0, &mut buffer)?;
        
        // Verificar firma de arranque MBR
        if buffer[510] != 0x55 || buffer[511] != 0xAA {
            return Err("No es una tabla MBR válida");
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: MBR válido encontrado en {}\n", device.name));
        
        // Procesar 4 entradas de partición MBR
        for i in 0..4 {
            let offset = 446 + (i * 16); // Cada entrada MBR es de 16 bytes
            let partition_entry = &buffer[offset..offset + 16];
            
            // Verificar si la partición está activa (tipo != 0)
            let partition_type = partition_entry[4];
            if partition_type == 0x00 {
                continue;
            }
            
            // Leer información de la partición
            let start_lba = u32::from_le_bytes([
                partition_entry[8], partition_entry[9], partition_entry[10], partition_entry[11],
            ]) as u64;
            
            let size_lba = u32::from_le_bytes([
                partition_entry[12], partition_entry[13], partition_entry[14], partition_entry[15],
            ]) as u64;
            
            if start_lba == 0 || size_lba == 0 {
                continue;
            }
            
            let bootable = partition_entry[0] == 0x80;
            let filesystem_type = self.detect_filesystem_type(device, start_lba)?;
            
            let partition = PartitionInfo {
                device_name: device.name.clone(),
                partition_index: i as u32 + 1,
                start_lba,
                size_lba,
                partition_type,
                filesystem_type,
                bootable,
            };
            
            partitions.push(partition);
            serial_write_str(&format!("STORAGE_MANAGER: Partición MBR {}: {} ({} sectores, {})\n", 
                                     i + 1, filesystem_type, size_lba, if bootable { "bootable" } else { "no bootable" }));
        }
        
        Ok(partitions)
    }

    /// Obtener tipo de partición GPT
    fn get_gpt_partition_type(&self, guid: &[u8]) -> u8 {
        // GUIDs comunes para tipos de partición
        match guid {
            // FAT32
            [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B] => 0x0C,
            // Linux Filesystem
            [0x0F, 0xE8, 0x8C, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4] => 0x83,
            // EclipseFS (personalizado)
            [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4] => 0xAF,
            _ => 0x00,
        }
    }

    /// Detectar tipo de sistema de archivos
    fn detect_filesystem_type(&self, device: &StorageDeviceInfo, start_lba: u64) -> Result<String, &'static str> {
        let mut buffer = [0u8; 512];
        self.read_device_sector(&device.name, start_lba, &mut buffer)?;
        
        // Verificar FAT32
        if &buffer[82..90] == b"FAT32   " && buffer[510] == 0x55 && buffer[511] == 0xAA {
            return Ok("FAT32".to_string());
        }
        
        // Verificar EclipseFS
        if &buffer[0..9] == b"ECLIPSEFS" {
            return Ok("EclipseFS".to_string());
        }
        
        // Verificar ext4
        if &buffer[1080..1084] == b"\x53\xEF" {
            return Ok("ext4".to_string());
        }
        
        // Por defecto, desconocido
        Ok("Unknown".to_string())
    }

    /// Crear particiones de prueba
    fn create_test_partitions(&mut self, device: &StorageDeviceInfo) {
        // Partición FAT32 de ejemplo
        let fat32_partition = PartitionInfo {
            device_name: device.name.clone(),
            partition_index: 1,
            start_lba: 2048,
            size_lba: 204800, // 100MB
            partition_type: 0x0C, // FAT32
            filesystem_type: "FAT32".to_string(),
            bootable: true,
        };
        
        // Partición EclipseFS de ejemplo
        let eclipsefs_partition = PartitionInfo {
            device_name: device.name.clone(),
            partition_index: 2,
            start_lba: 206848,
            size_lba: device.capacity / 512 - 206848,
            partition_type: 0xAF, // EclipseFS
            filesystem_type: "EclipseFS".to_string(),
            bootable: false,
        };
        
        self.partitions.push(fat32_partition);
        self.partitions.push(eclipsefs_partition);
        
        serial_write_str(&format!("STORAGE_MANAGER: Creadas 2 particiones en {}\n", device.name));
    }

    /// Verificar si el StorageManager está listo
    pub fn is_ready(&self) -> bool {
        self.is_ready.load(Ordering::SeqCst)
    }

    /// Obtener información de dispositivos
    pub fn get_devices(&self) -> &[StorageDeviceInfo] {
        &self.devices
    }

    /// Obtener información de particiones
    pub fn get_partitions(&self) -> &[PartitionInfo] {
        &self.partitions
    }

    /// Leer sector de un dispositivo
    pub fn read_device_sector(&self, device_name: &str, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} de {}\n", sector, device_name));
        
        // Encontrar el dispositivo
        let device = self.devices.iter()
            .find(|d| d.name == device_name)
            .ok_or("Dispositivo no encontrado")?;
        
        // Usar el driver apropiado según el tipo de controlador
        match device.controller_type {
            StorageControllerType::AHCI => {
                self.read_ahci_sector(device, sector, buffer)
            },
            StorageControllerType::IDE => {
                self.read_ide_sector(device, sector, buffer)
            },
            StorageControllerType::ATA => {
                self.read_ata_sector(device, sector, buffer)
            },
            StorageControllerType::NVMe => {
                self.read_nvme_sector(device, sector, buffer)
            },
            StorageControllerType::VirtIO => {
                self.read_virtio_sector(device, sector, buffer)
            },
        }
    }

    /// Leer sector usando driver AHCI
    fn read_ahci_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver AHCI para {}\n", device.name));
        
        // Crear driver AHCI
        let mut ahci_driver = SataAhciDriver::new();
        
        // Inicializar driver
        if let Err(e) = ahci_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando AHCI: {}\n", e));
            return Err("Error inicializando AHCI");
        }
        
        // Leer sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ahci_driver.read_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector AHCI: {}\n", e));
                "Error leyendo sector AHCI"
            })
    }

    /// Leer sector usando driver IDE
    fn read_ide_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver IDE para {}\n", device.name));
        
        // Crear driver IDE
        let mut ide_driver = IdeModernDriver::new();
        
        // Inicializar driver
        if let Err(e) = ide_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando IDE: {}\n", e));
            return Err("Error inicializando IDE");
        }
        
        // Leer sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ide_driver.read_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector IDE: {}\n", e));
                "Error leyendo sector IDE"
            })
    }

    /// Leer sector usando driver ATA
    fn read_ata_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver ATA para {}\n", device.name));
        
        // Crear driver ATA
        let mut ata_driver = AtaDirectDriver::new();
        
        // Inicializar driver
        if let Err(e) = ata_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA: {}\n", e));
            return Err("Error inicializando ATA");
        }
        
        // Leer sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ata_driver.read_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector ATA: {}\n", e));
                "Error leyendo sector ATA"
            })
    }

    /// Leer sector usando driver NVMe
    fn read_nvme_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver NVMe para {}\n", device.name));
        
        // TODO: Implementar driver NVMe
        serial_write_str("STORAGE_MANAGER: Driver NVMe no implementado, usando datos simulados\n");
        
        // Simular datos
        buffer.fill(0);
        if sector == 0 {
            buffer[510] = 0x55;
            buffer[511] = 0xAA;
        }
        
        Ok(())
    }

    /// Leer sector usando driver VirtIO
    fn read_virtio_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver VirtIO para {}\n", device.name));
        
        // TODO: Implementar driver VirtIO
        serial_write_str("STORAGE_MANAGER: Driver VirtIO no implementado, usando datos simulados\n");
        
        // Simular datos
        buffer.fill(0);
        if sector == 0 {
            buffer[510] = 0x55;
            buffer[511] = 0xAA;
        }
        
        Ok(())
    }

    /// Escribir sector a un dispositivo
    pub fn write_device_sector(&self, device_name: &str, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Escribiendo sector {} a {}\n", sector, device_name));
        
        // Encontrar el dispositivo
        let device = self.devices.iter()
            .find(|d| d.name == device_name)
            .ok_or("Dispositivo no encontrado")?;
        
        // Usar el driver apropiado según el tipo de controlador
        match device.controller_type {
            StorageControllerType::AHCI => {
                self.write_ahci_sector(device, sector, buffer)
            },
            StorageControllerType::IDE => {
                self.write_ide_sector(device, sector, buffer)
            },
            StorageControllerType::ATA => {
                self.write_ata_sector(device, sector, buffer)
            },
            StorageControllerType::NVMe => {
                self.write_nvme_sector(device, sector, buffer)
            },
            StorageControllerType::VirtIO => {
                self.write_virtio_sector(device, sector, buffer)
            },
        }
    }

    /// Escribir sector usando driver AHCI
    fn write_ahci_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver AHCI para escribir en {}\n", device.name));
        
        // Crear driver AHCI
        let mut ahci_driver = SataAhciDriver::new();
        
        // Inicializar driver
        if let Err(e) = ahci_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando AHCI: {}\n", e));
            return Err("Error inicializando AHCI");
        }
        
        // Escribir sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ahci_driver.write_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo sector AHCI: {}\n", e));
                "Error escribiendo sector AHCI"
            })
    }

    /// Escribir sector usando driver IDE
    fn write_ide_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver IDE para escribir en {}\n", device.name));
        
        // Crear driver IDE
        let mut ide_driver = IdeModernDriver::new();
        
        // Inicializar driver
        if let Err(e) = ide_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando IDE: {}\n", e));
            return Err("Error inicializando IDE");
        }
        
        // Escribir sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ide_driver.write_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo sector IDE: {}\n", e));
                "Error escribiendo sector IDE"
            })
    }

    /// Escribir sector usando driver ATA
    fn write_ata_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver ATA para escribir en {}\n", device.name));
        
        // Crear driver ATA
        let mut ata_driver = AtaDirectDriver::new();
        
        // Inicializar driver
        if let Err(e) = ata_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA: {}\n", e));
            return Err("Error inicializando ATA");
        }
        
        // Escribir sector
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        ata_driver.write_sector(sector_u32, buffer)
            .map_err(|e| {
                serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo sector ATA: {}\n", e));
                "Error escribiendo sector ATA"
            })
    }

    /// Escribir sector usando driver NVMe
    fn write_nvme_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver NVMe para escribir en {}\n", device.name));
        
        // TODO: Implementar driver NVMe
        serial_write_str("STORAGE_MANAGER: Driver NVMe no implementado, simulando escritura\n");
        
        Ok(())
    }

    /// Escribir sector usando driver VirtIO
    fn write_virtio_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver VirtIO para escribir en {}\n", device.name));
        
        // TODO: Implementar driver VirtIO
        serial_write_str("STORAGE_MANAGER: Driver VirtIO no implementado, simulando escritura\n");
        
        Ok(())
    }

    /// Leer sector de una partición
    pub fn read_from_partition(&self, device_name: &str, partition_index: u32, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        // Encontrar la partición
        let partition = self.partitions.iter()
            .find(|p| p.device_name == device_name && p.partition_index == partition_index)
            .ok_or("Partición no encontrada")?;
        
        // Calcular sector absoluto
        let absolute_sector = partition.start_lba + sector;
        
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} (absoluto: {}) de partición {}:{}\n", 
                                 sector, absolute_sector, device_name, partition_index));
        
        // TODO: Implementar lectura real desde el dispositivo
        buffer.fill(0);
        
        Ok(())
    }

    /// Obtener dispositivos candidatos para FAT32
    pub fn get_fat32_candidates(&self) -> Vec<Fat32DeviceInfo> {
        let mut candidates = Vec::new();
        
        for partition in &self.partitions {
            if partition.filesystem_type == "FAT32" {
                let device_info = Fat32DeviceInfo {
                    device_name: partition.device_name.clone(),
                    partition_index: partition.partition_index,
                    start_lba: partition.start_lba,
                    size_lba: partition.size_lba,
                };
                candidates.push(device_info);
            }
        }
        
        candidates
    }

    /// Obtener dispositivos candidatos para EclipseFS
    pub fn get_eclipsefs_candidates(&self) -> Vec<EclipseFSDeviceInfo> {
        let mut candidates = Vec::new();
        
        for partition in &self.partitions {
            if partition.filesystem_type == "EclipseFS" {
                let device_info = EclipseFSDeviceInfo {
                    device_name: partition.device_name.clone(),
                    partition_index: partition.partition_index,
                    start_lba: partition.start_lba,
                    size_lba: partition.size_lba,
                };
                candidates.push(device_info);
            }
        }
        
        candidates
    }
}

/// Instancia global del StorageManager
static mut STORAGE_MANAGER: Option<StorageManager> = None;

/// Inicializar el StorageManager global
pub fn initialize_storage_manager() -> Result<(), &'static str> {
    unsafe {
        if STORAGE_MANAGER.is_some() {
            return Err("StorageManager ya está inicializado");
        }
        
        let mut manager = StorageManager::new();
        manager.initialize()?;
        STORAGE_MANAGER = Some(manager);
        
        Ok(())
    }
}

/// Obtener referencia al StorageManager global
pub fn get_storage_manager() -> Option<&'static StorageManager> {
    unsafe {
        STORAGE_MANAGER.as_ref()
    }
}

/// Verificar si el StorageManager está listo
pub fn is_storage_manager_ready() -> bool {
    unsafe {
        STORAGE_MANAGER.as_ref().map(|m| m.is_ready()).unwrap_or(false)
    }
}
