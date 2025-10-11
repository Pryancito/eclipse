//! Gestor de almacenamiento simplificado para hardware real
//! 
//! Este módulo proporciona una interfaz simplificada para el acceso al almacenamiento
//! sin soporte RAID, enfocado en funcionar en hardware real.

use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;
use alloc::boxed::Box;

use crate::debug::serial_write_str;
use crate::drivers::ata_direct::AtaDirectDriver;
use crate::drivers::ahci::AhciDriver;
use crate::drivers::framebuffer::{get_framebuffer, Color};
use crate::filesystem::fat32::Fat32DeviceInfo;
use crate::filesystem::eclipsefs::{EclipseFSDeviceInfo, EclipseFSWrapper};

/// Tipos de controladores de almacenamiento soportados
#[derive(Debug, Clone, PartialEq)]
pub enum StorageControllerType {
    ATA,
    NVMe,
    AHCI,
    VirtIO,
    IDE,
}

/// Tipos de sectores de almacenamiento
#[derive(Debug, Clone, PartialEq)]
pub enum StorageSectorType {
    FAT32,
    EclipseFS,
    MBR,
    GPT,
}

// Configuración: habilitar AHCI en hardware real (con fallback ATA seguro)
const AHCI_ENABLED: bool = true;
// Configuración: permitir detección PCI (necesaria para AHCI real)
const FORCE_ATA_ONLY: bool = false;
// Verbosidad de logs AHCI (true = detallado, false = conciso)
const AHCI_VERBOSE: bool = false;

// Driver ATA cacheado (evitar re-inicializaciones repetidas)
static mut CACHED_ATA: Option<AtaDirectDriver> = None;

// === Buffers DMA estáticos y alineados por puerto (0..7) ===
// Requisitos AHCI:
// - FIS: alineación 256 bytes (usamos 256)
// - Command List (CLB): alineación 1KB (usamos 1024)
// - Command Table (CT): alineación 128 bytes (usamos 128); tamaño >= 256B + PRD

#[repr(align(256))]
#[derive(Copy, Clone)]
struct Align256([u8; 256]);
#[repr(align(1024))]
#[derive(Copy, Clone)]
struct Align1024([u8; 1024]);
#[repr(align(128))]
#[derive(Copy, Clone)]
struct Align128([u8; 4096]); // tabla suficientemente grande por puerto

static mut AHCI_FIS_AREA: [Align256; 8] = [Align256([0; 256]); 8];
static mut AHCI_CMD_LIST: [Align1024; 8] = [Align1024([0; 1024]); 8];
static mut AHCI_CMD_TABLE: [Align128; 8] = [Align128([0; 4096]); 8];

#[inline(always)]
fn get_dma_fis_base(port: u8) -> u32 {
    unsafe { (&AHCI_FIS_AREA[port as usize].0 as *const u8 as usize & 0xFFFF_FFF0) as u32 }
}
#[inline(always)]
fn get_dma_clb_base(port: u8) -> u32 {
    unsafe { (&AHCI_CMD_LIST[port as usize].0 as *const u8 as usize & 0xFFFF_FC00) as u32 }
}
#[inline(always)]
fn get_dma_ct_base(port: u8) -> u32 {
    unsafe { (&AHCI_CMD_TABLE[port as usize].0 as *const u8 as usize & 0xFFFF_FF80) as u32 }
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
    pub block_size: u32,
    pub controller_type: StorageControllerType,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl StorageDeviceInfo {
    /// Crear nuevo dispositivo de almacenamiento
    pub fn new(name: String, model: String, serial: String, firmware: String, 
               capacity: u64, block_size: u32, controller_type: StorageControllerType,
               vendor_id: u16, device_id: u16, bus: u8, device: u8, function: u8) -> Self {
        Self {
            name,
            model,
            serial,
            firmware,
            capacity,
            block_size,
            controller_type,
            vendor_id,
            device_id,
            bus,
            device,
            function,
        }
    }
}

/// Información de una partición
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub name: String, // Nombre de la partición (ej: "/dev/sda1")
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
    pub devices: Vec<StorageDeviceInfo>,
    pub partitions: Vec<PartitionInfo>,
    is_ready: AtomicBool,
}

impl StorageManager {
    #[inline(always)]
    fn is_zeroed(buf: &[u8]) -> bool { buf.iter().all(|&b| b == 0) }
    #[inline(always)]
    fn fb_log(&self, msg: &str, color: Color) {
        if let Some(mut fb) = get_framebuffer() {
            fb.write_text_kernel(msg, color);
        }
    }
    /// Crear nueva instancia del StorageManager
    pub fn new() -> Self {
        serial_write_str("STORAGE_MANAGER: Inicializando StorageManager simplificado\n");
        
        Self {
            devices: Vec::new(),
            partitions: Vec::new(),
            is_ready: AtomicBool::new(false),
        }
    }

    /// Inicializar el StorageManager
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: >>> Inicio initialize()\n");
        serial_write_str("STORAGE_MANAGER: Iniciando detección de dispositivos de almacenamiento\n");
        
        // Detectar dispositivos PCI de almacenamiento
        self.detect_storage_devices()?;
        
        // Detectar particiones en cada dispositivo
        serial_write_str("STORAGE_MANAGER: Detección de dispositivos completada, iniciando detección de particiones\n");
        self.detect_partitions()?;
        
        // Marcar como listo
        self.is_ready.store(true, Ordering::SeqCst);
        
        serial_write_str(&format!("STORAGE_MANAGER: Inicialización completa - {} dispositivos, {} particiones\n", 
                                 self.devices.len(), self.partitions.len()));
        serial_write_str("STORAGE_MANAGER: <<< Fin initialize()\n");
        
        Ok(())
    }

    /// Detectar dispositivos de almacenamiento desde PCI
    fn detect_storage_devices(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando dispositivos PCI de almacenamiento\n");
        
        if FORCE_ATA_ONLY {
            serial_write_str("STORAGE_MANAGER: FORCE_ATA_ONLY activo - creando /dev/sda (ATA) sin escaneo PCI\n");
            let sata_device = StorageDeviceInfo::new(
                String::from("/dev/sda"),
                String::from("Generic ATA Drive"),
                String::from("ATA-0000"),
                String::from("FW-ATA"),
                1073741824,
                512,
                StorageControllerType::ATA,
                0,
                0,
                0,
                0,
                0,
            );
            self.devices.push(sata_device);
            serial_write_str("STORAGE_MANAGER: Dispositivo /dev/sda (ATA) creado\n");
            return Ok(());
        }
        
        // Detectar dispositivos PCI reales
        self.detect_pci_storage_devices()?;
        
        // No crear dispositivos de prueba en hardware real
        if self.devices.is_empty() {
            serial_write_str("STORAGE_MANAGER: No se encontraron dispositivos PCI\n");
        }

        Ok(())
    }

    /// Detectar dispositivos PCI reales (usando PciManager)
    fn detect_pci_storage_devices(&mut self) -> Result<(), &'static str> {
        use crate::drivers::framebuffer::Color;
        use crate::drivers::pci::{PciManager};
        serial_write_str("STORAGE_MANAGER: Escaneando bus PCI para dispositivos de almacenamiento\n");
        self.fb_log("PCI: Escaneando dispositivos...", Color::WHITE);

        let mut pci = PciManager::new();
        pci.scan_devices_quiet();

        // Filtrar clase 0x01 (Mass Storage)
        let mut found = 0usize;
        for i in 0..pci.device_count() {
            if let Some(dev) = pci.get_device(i) {
                if dev.class_code == 0x01 { // Mass Storage
                    let ctrl = match dev.subclass_code {
                        0x06 => StorageControllerType::AHCI,
                        0x01 => StorageControllerType::IDE,
                        0x08 => StorageControllerType::NVMe,
                        _ => StorageControllerType::ATA,
                    };

                    let name = match ctrl {
                        StorageControllerType::AHCI | StorageControllerType::ATA | StorageControllerType::IDE =>
                            format!("/dev/sd{}", (b'a' + self.devices.len() as u8) as char),
                        StorageControllerType::NVMe => format!("/dev/nvme{}", self.devices.len()),
                        StorageControllerType::VirtIO => format!("/dev/vda"),
                    };

                    let storage_device = StorageDeviceInfo::new(
                        name,
                        format!("PCI {:04X}:{:04X}", dev.vendor_id, dev.device_id),
                        format!("PCI-{:02X}:{:02X}.{}", dev.bus, dev.device, dev.function),
                        String::from("PCI-FW"),
                        0, // capacidad desconocida
                        512,
                        ctrl,
                        dev.vendor_id,
                        dev.device_id,
                        dev.bus,
                        dev.device,
                        dev.function,
                    );
                    self.devices.push(storage_device);
                    found += 1;
                }
            }
        }

        self.fb_log(&alloc::format!("PCI: {} controladoras de almacenamiento", found), Color::CYAN);
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

        StorageDeviceInfo::new(
            device_name,
            format!("PCI {:04X}:{:04X}", pci_info.vendor_id, pci_info.device_id),
            format!("PCI-{:02X}:{:02X}.{}", bus, device, function),
            String::from("PCI-FW"),
            1073741824, // 1GB por defecto
            512, // 512 bytes por sector
                        controller_type,
            pci_info.vendor_id,
            pci_info.device_id,
            bus,
            device,
            function,
        )
    }

    /// Crear dispositivos de prueba
    fn create_test_devices(&mut self) {
        serial_write_str("STORAGE_MANAGER: Creando dispositivos de prueba\n");
        
        // Dispositivo SATA de ejemplo
        let sata_device = StorageDeviceInfo::new(
            String::from("/dev/sda"),
            String::from("Test SATA Drive"),
            String::from("SATA-12345"),
            String::from("FW-1.0"),
            1073741824, // 1GB
            512, // 512 bytes por sector
            StorageControllerType::ATA,
            0x8086,
            0x2822,
            0,
            1,
            1,
        );
        
        self.devices.push(sata_device);
        
        serial_write_str(&format!("STORAGE_MANAGER: Creado dispositivo: {}\n", self.devices[0].name));
    }

    /// Detectar particiones en todos los dispositivos
    fn detect_partitions(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando particiones\n");
        
        let device_names: Vec<String> = self.devices.iter().map(|d| d.name.clone()).collect();
        
        for device_name in device_names {
            serial_write_str(&format!("STORAGE_MANAGER: Analizando particiones en {}\n", device_name));
            
            // Clonar el dispositivo para evitar problemas de borrow
            let device = self.devices.iter().find(|d| d.name == device_name).cloned();
            
            if let Some(device) = device {
                // Intentar detectar GPT primero
                if let Ok(partitions) = self.detect_gpt_partitions(&device) {
                    serial_write_str(&format!("STORAGE_MANAGER: Encontradas {} particiones GPT en {}\n", partitions.len(), device_name));
                    self.partitions.extend(partitions);
                } else if let Ok(partitions) = self.detect_mbr_partitions(&device) {
                    serial_write_str(&format!("STORAGE_MANAGER: Encontradas {} particiones MBR en {}\n", partitions.len(), device_name));
                    self.partitions.extend(partitions);
        } else {
                    serial_write_str(&format!("STORAGE_MANAGER: No se encontraron particiones en {} (no se crearán particiones de prueba)\n", device_name));
                }
            }
        }
        
        Ok(())
    }

    /// Detectar particiones GPT
    fn detect_gpt_partitions(&self, device: &StorageDeviceInfo) -> Result<Vec<PartitionInfo>, &'static str> {
        let mut buffer = [0u8; 512];
        let mut partitions = Vec::new();
        
        // Leer GPT Header (sector 1)
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector 1 (GPT Header) de {}\n", device.name));
        self.fb_log(&alloc::format!("GPT: Leyendo header en LBA 1 de {}", device.name), Color::CYAN);
        self.read_device_sector(&device.name, 1, &mut buffer)?;
        // Dump primeros 16 bytes para ver firma
        let sig_hex: alloc::string::String = (0..16)
            .map(|i| alloc::format!("{:02X}", buffer[i])).collect::<alloc::vec::Vec<_>>()
            .join(" ");
        serial_write_str(&format!("STORAGE_MANAGER: GPT header bytes[0..16]: {}\n", sig_hex));
        self.fb_log(&alloc::format!("GPT[0..16]: {}", sig_hex), Color::LIGHT_GRAY);
        
        // Verificar firma GPT
        if &buffer[0..8] != b"EFI PART" {
            serial_write_str("STORAGE_MANAGER: Firma GPT no encontrada en sector 1\n");
            self.fb_log("GPT: firma no encontrada en LBA 1", Color::YELLOW);
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
            let fs_type_display = filesystem_type.clone();
            
            let partition = PartitionInfo {
                name: format!("{}{}", device.name, partitions.len() + 1),
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
                                     partitions.len(), fs_type_display, size_lba));
        }
        
        Ok(partitions)
    }

    /// Detectar particiones MBR
    fn detect_mbr_partitions(&self, device: &StorageDeviceInfo) -> Result<Vec<PartitionInfo>, &'static str> {
        let mut buffer = [0u8; 512];
        let mut partitions = Vec::new();
        
        // Leer MBR (sector 0)
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector 0 (MBR) de {}\n", device.name));
        self.fb_log(&alloc::format!("MBR: Leyendo LBA 0 de {}", device.name), Color::CYAN);
        self.read_device_sector(&device.name, 0, &mut buffer)?;
        serial_write_str(&format!(
            "STORAGE_MANAGER: MBR signature bytes: {:02X} {:02X}\n",
            buffer[510], buffer[511]
        ));
        self.fb_log(&alloc::format!("MBR sig: {:02X}{:02X}", buffer[511], buffer[510]), Color::LIGHT_GRAY);
        
        // Si todo son ceros, probar ATA directamente
        if Self::is_zeroed(&buffer) {
            self.fb_log("MBR: Todo ceros, probando ATA directo...", Color::YELLOW);
            let mut ata_buffer = [0u8; 512];
            if let Ok(()) = self.read_ata_sector_fallback(device, 0, &mut ata_buffer) {
                let ata_sig = (ata_buffer[511] as u16) << 8 | ata_buffer[510] as u16;
                self.fb_log(&alloc::format!("ATA sig: {:04X}", ata_sig), Color::YELLOW);
                if !Self::is_zeroed(&ata_buffer) {
                    self.fb_log("ATA: Lectura exitosa, copiando datos", Color::GREEN);
                    buffer.copy_from_slice(&ata_buffer);
                }
            }
        }
        
        // Detección específica para chipset X99
        if device.name.contains("sda") {
            self.fb_log("X99: Detectado disco sda, verificando puerto SATA...", Color::CYAN);
            
            // Debug: mostrar estado del buffer antes de ATA
            let is_buffer_zeroed = Self::is_zeroed(&buffer);
            self.fb_log(&alloc::format!("X99: Buffer es todo ceros: {}", is_buffer_zeroed), Color::CYAN);
            
            let mut ata_success = false;
            // Probar puertos SATA específicos del X99
            for port in 0..8u8 {
                let mut port_buffer = [0u8; 512];
                if let Ok(()) = self.read_ata_port_direct(port, 0, &mut port_buffer) {
                    let port_sig = (port_buffer[511] as u16) << 8 | port_buffer[510] as u16;
                    self.fb_log(&alloc::format!("X99: Puerto ATA {} sig: {:04X}", port, port_sig), Color::CYAN);
                    if port_sig == 0x55AA {
                        self.fb_log(&alloc::format!("X99: Puerto SATA {} tiene MBR válido", port), Color::GREEN);
                        buffer.copy_from_slice(&port_buffer);
                        ata_success = true;
                        break;
                    }
                } else {
                    self.fb_log(&alloc::format!("X99: Puerto ATA {} falló", port), Color::RED);
                }
            }
            
            // Debug: mostrar estado del buffer después de ATA
            let is_buffer_zeroed_after = Self::is_zeroed(&buffer);
            self.fb_log(&alloc::format!("X99: Buffer después de ATA es todo ceros: {}", is_buffer_zeroed_after), Color::CYAN);
            
            // Si ATA falla (no encontró MBR válido), probar múltiples controladores SATA
            if !ata_success {
                self.fb_log("X99: ATA falló, probando múltiples controladores SATA...", Color::YELLOW);
                let mut ahci_buffer = [0u8; 512];
                
                // Lista de controladores SATA conocidos en tu placa X99
                // ORDEN CORREGIDO: Principal primero (donde está tu disco según Linux)
                let sata_controllers = [
                    ("AHCI Principal (tu disco aquí)", 0x92f24000u32),
                    ("AHCI Adicional", 0x92f26000u32),
                    ("AHCI Secundario", 0x92f27000u32),
                ];
                
                let mut found_device = false;
                for (name, mmio_base) in sata_controllers.iter() {
                    if AHCI_VERBOSE {
                        self.fb_log(&alloc::format!("X99: Probando {} en MMIO 0x{:08X}", name, mmio_base), Color::CYAN);
                    } else {
                        self.fb_log(&alloc::format!("X99: {} -> MMIO 0x{:08X}", name, mmio_base), Color::CYAN);
                    }
                    
                    // Verificar si el controlador existe leyendo CAP
                    let cap = unsafe { core::ptr::read_volatile((*mmio_base) as *const u32) };
                    
                    // SIEMPRE probar el controlador principal (donde está tu disco según Linux)
                    if *mmio_base == 0x92f24000u32 {
                        if AHCI_VERBOSE {
                            self.fb_log(&alloc::format!("X99: {} - FORZANDO prueba (tu disco aquí, MMIO: 0x{:08X}, CAP: 0x{:08X})", name, *mmio_base, cap), Color::GREEN);
                        } else {
                            self.fb_log(&alloc::format!("X99: {} seleccionado (CAP 0x{:08X})", name, cap), Color::GREEN);
                        }
                    } else if cap == 0xFFFFFFFF || cap == 0x00000000 {
                        if AHCI_VERBOSE {
                            self.fb_log(&alloc::format!("X99: {} no disponible (MMIO: 0x{:08X}, CAP: 0x{:08X})", name, *mmio_base, cap), Color::RED);
                        }
                        continue;
                    }
                    
                    if AHCI_VERBOSE {
                        self.fb_log(&alloc::format!("X99: {} disponible (CAP: 0x{:08X})", name, cap), Color::GREEN);
                    }
                    
                    // Leer PI después de estabilizar
                    for _ in 0..100000 { core::hint::spin_loop(); }
                    let pi = unsafe { core::ptr::read_volatile((*mmio_base + 0x0C) as *const u32) };
                    self.fb_log(&alloc::format!("X99: {} PI: 0x{:08X}", name, pi), Color::CYAN);
                    
                    // Si el controlador principal solo reporta puerto 0, forzar puertos adicionales
                    if *mmio_base == 0x92f24000u32 && pi == 0x00000001 {
                        if AHCI_VERBOSE {
                            self.fb_log("X99: Controlador principal solo reporta puerto 0, forzando puertos 0-7", Color::YELLOW);
                            let pi_forced = 0x000000FF; // Puertos 0-7
                            self.fb_log(&alloc::format!("X99: PI forzado: 0x{:08X}", pi_forced), Color::GREEN);
                        }
                        
                        // Probar puertos con mapeo dual: leer DET desde 0x92F26000 y ejecutar en 0x92F24000
                        let port_priority = [0u8, 1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8];

                        for port in port_priority {
                            // Comprobar enlace desde el HBA de mapeo
                            let port_base_map = 0x92f26000u32 + 0x100 + (port as u32 * 0x80);
                            let px_ssts = unsafe { core::ptr::read_volatile((port_base_map + 0x28) as *const u32) };
                            let det_status = px_ssts & 0x0F;
                            if det_status == 0x03 {
                                if !AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Link activo en puerto {} (DET=3)", port), Color::GREEN); }
                                // Ejecutar comando en el HBA principal
                                match self.read_ahci_port_with_map_and_exec(0x92f26000u32, 0x92f24000u32, port, 0, &mut ahci_buffer) {
                                    Ok(()) => {
                                        let ahci_sig = (ahci_buffer[511] as u16) << 8 | ahci_buffer[510] as u16;
                                        if ahci_sig == 0x55AA {
                                            self.fb_log(&alloc::format!("X99: ¡MBR válido encontrado en {} puerto {}!", name, port), Color::GREEN);
                                            buffer.copy_from_slice(&ahci_buffer);
                                            found_device = true;
                                            break;
                                        } else if AHCI_VERBOSE {
                                            self.fb_log(&alloc::format!("X99: {} puerto {} - MBR inválido (sig: {:04X})", name, port, ahci_sig), Color::RED);
                                        }
                                    }
                                    Err(e) => { if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: {} puerto {} error: {}", name, port, e), Color::RED); } }
                                }
                            } else if AHCI_VERBOSE {
                                self.fb_log(&alloc::format!("X99: puerto {} sin enlace (DET=0x{:01X})", port, det_status), Color::LIGHT_GRAY);
                            }
                        }
                        
                        if found_device {
                            break;
                        }
                    } else {
                        // Lógica normal para otros controladores
                        // Probar puertos en orden de prioridad: puerto 1 primero (donde está tu disco según Linux)
                        let port_priority = if *mmio_base == 0x92f24000u32 {
                            [0u8, 1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8] // En este hardware, ata1 => puerto 0
                        } else {
                            [0u8, 1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8] // Puerto 0 primero para otros
                        };
                        
                        for port in port_priority {
                            if (pi & (1 << port)) != 0 {
                                if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: {} puerto {} (implementado)", name, port), Color::CYAN); }

                                // En el principal: usar mapa 0x92f26000 para estado y ejecutar en 0x92f24000
                                let read_result = if *mmio_base == 0x92f24000u32 {
                                    self.read_ahci_port_with_map_and_exec(0x92f26000u32, 0x92f24000u32, port, 0, &mut ahci_buffer)
                                } else {
                                    self.read_ahci_port_direct_with_mmio(*mmio_base, port, 0, &mut ahci_buffer)
                                };

                                match read_result {
                                    Ok(()) => {
                                        let ahci_sig = (ahci_buffer[511] as u16) << 8 | ahci_buffer[510] as u16;
                                        if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: {} puerto {} sig: {:04X}", name, port, ahci_sig), Color::YELLOW); }
                                        if ahci_sig == 0x55AA {
                                            self.fb_log(&alloc::format!("X99: ¡MBR válido encontrado en {} puerto {}!", name, port), Color::GREEN);
                                            buffer.copy_from_slice(&ahci_buffer);
                                            found_device = true;
                                            break;
                                        } else if AHCI_VERBOSE {
                                            self.fb_log(&alloc::format!("X99: {} puerto {} - MBR inválido (sig: {:04X})", name, port, ahci_sig), Color::RED);
                                        }
                                    }
                                    Err(e) => {
                                        if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: {} puerto {} error: {}", name, port, e), Color::RED); }
                                    }
                                }
                            }
                        }
                    }
                    
                    if found_device {
                        break;
                    }
                }
                
                if !found_device {
                    self.fb_log("X99: No se encontró dispositivo válido en ningún controlador SATA", Color::RED);
                }
            } else {
                self.fb_log("X99: ATA tuvo éxito, saltando búsqueda de controladores SATA", Color::GREEN);
            }
        }
        // Dump 16 bytes a partir de 446 para ver primera entrada
        let mut entry_hex = alloc::string::String::new();
        for i in 0..16 { let _ = entry_hex.push_str(&alloc::format!("{:02X} ", buffer[446 + i])); }
        serial_write_str(&format!("STORAGE_MANAGER: MBR entry0 bytes: {}\n", entry_hex));
        self.fb_log(&alloc::format!("MBR entry0: {}", entry_hex), Color::LIGHT_GRAY);
        
        // Verificar firma de arranque MBR
        if buffer[510] != 0x55 || buffer[511] != 0xAA {
            self.fb_log("MBR: firma inválida (no 0x55AA)", Color::YELLOW);
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
            let fs_type_display = filesystem_type.clone();
            
            let partition = PartitionInfo {
                name: format!("{}{}", device.name, i + 1),
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
                                     i + 1, fs_type_display, size_lba, if bootable { "bootable" } else { "no bootable" }));
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
            return Ok(String::from("FAT32"));
        }
        
        // Verificar EclipseFS
        if &buffer[0..9] == b"ECLIPSEFS" {
            return Ok(String::from("EclipseFS"));
        }
        
        // Verificar ext4
        if &buffer[1080..1084] == b"\x53\xEF" {
            return Ok(String::from("ext4"));
        }
        
        // Por defecto, desconocido
        Ok(String::from("Unknown"))
    }

    /// Crear particiones de prueba
    fn create_test_partitions(&mut self, device: &StorageDeviceInfo) {
        // Partición FAT32 de ejemplo
        let fat32_partition = PartitionInfo {
            name: format!("{}1", device.name),
            device_name: device.name.clone(),
            partition_index: 1,
            start_lba: 2048,
            size_lba: 204800, // 100MB
            partition_type: 0x0C, // FAT32
            filesystem_type: String::from("FAT32"),
            bootable: true,
        };
        
        // Partición EclipseFS de ejemplo
        let eclipsefs_partition = PartitionInfo {
            name: format!("{}2", device.name),
            device_name: device.name.clone(),
            partition_index: 2,
            start_lba: 206848,
            size_lba: device.capacity / 512 - 206848,
            partition_type: 0xAF, // EclipseFS
            filesystem_type: String::from("EclipseFS"),
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
        let mut maybe_device = self.devices.iter().find(|d| d.name == device_name);
        // Compat: si es una partición (/dev/sda1), mapear al dispositivo base (/dev/sda)
        let base_name;
        if maybe_device.is_none() {
            if device_name.starts_with("/dev/sd") {
                let trimmed = device_name.trim_end_matches(|c: char| c.is_ascii_digit());
                base_name = alloc::string::String::from(trimmed);
                maybe_device = self.devices.iter().find(|d| d.name == base_name);
            }
        }
        let device = maybe_device.ok_or("Dispositivo no encontrado")?;
        
        // Usar el driver apropiado según el tipo de controlador
        match device.controller_type {
            StorageControllerType::AHCI => {
                if AHCI_ENABLED {
                    // Intento AHCI; si devuelve ceros, caer a ATA
                    match self.read_ahci_sector(device, sector, buffer) {
                        Ok(()) => {
                            if Self::is_zeroed(buffer) {
                                serial_write_str("STORAGE_MANAGER: Lectura AHCI devolvió todo ceros, probando ATA fallback\n");
                                let _ = self.read_ata_sector_fallback(device, sector, buffer);
                            }
                            Ok(())
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: AHCI fallo: {} -> ATA fallback\n", e));
                            self.read_ata_sector_fallback(device, sector, buffer)
                        }
                    }
                } else {
                    serial_write_str("STORAGE_MANAGER: AHCI deshabilitado, usando ATA fallback\n");
                    self.read_ata_sector_fallback(device, sector, buffer)
                }
            },
            StorageControllerType::IDE => {
                self.read_ide_sector(device, sector, buffer)
            },
            StorageControllerType::ATA => {
                self.read_ata_sector_fallback(device, sector, buffer)
            },
            StorageControllerType::NVMe => {
                self.read_nvme_sector(device, sector, buffer)
            },
            StorageControllerType::VirtIO => {
                self.read_virtio_sector(device, sector, buffer)
            },
        }
    }

    /// Leer sector usando driver AHCI con fallback robusto
    fn read_ahci_sector(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver AHCI para {}\n", device.name));
        
        // Intentar obtener MMIO real desde PCI BAR5 (o BAR0 de ser necesario)
        let mmio_base = {
            use crate::drivers::pci::PciDevice as RealPciDevice;
            let pci_dev = RealPciDevice {
                bus: device.bus,
                device: device.device,
                function: device.function,
                vendor_id: device.vendor_id,
                device_id: device.device_id,
                class_code: 0x01,
                subclass_code: 0x06,
                prog_if: 0x01,
                revision_id: 0,
                header_type: 0,
                status: 0,
                command: 0,
            };
            pci_dev.enable_mmio_and_bus_master();
            let bar5 = pci_dev.get_bar(5) as u64;
            let bar0 = pci_dev.get_bar(0) as u64;
            let chosen = if bar5 != 0 { bar5 } else { bar0 };
            if chosen == 0 { 0 } else { chosen }
        };
        
        if mmio_base == 0 {
            serial_write_str("STORAGE_MANAGER: MMIO AHCI no disponible (BARs = 0)\n");
            return self.read_ata_sector_fallback(device, sector, buffer);
        }
        
        // Inicializar AHCI con MMIO real
        let mut ahci_driver = AhciDriver::new_from_pci(device.vendor_id, device.device_id, mmio_base);
        
        if let Err(e) = ahci_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando AHCI: {}\n", e));
            serial_write_str("STORAGE_MANAGER: Intentando fallback a driver ATA...\n");
            return self.read_ata_sector_fallback(device, sector, buffer);
        }
        
        let sector_u32 = if sector > u32::MAX as u64 { 0 } else { sector as u32 };
        match ahci_driver.read_sector(sector_u32, buffer) {
            Ok(_) => Ok(()),
            Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector AHCI: {}\n", e));
                serial_write_str("STORAGE_MANAGER: Intentando fallback a driver ATA...\n");
                self.read_ata_sector_fallback(device, sector, buffer)
            }
        }
    }
    
    /// Leer puerto AHCI con MMIO base específico - Implementación basada en Linux ahci.c
    fn read_ahci_port_direct_with_mmio(&self, ahci_base: u32, port: u8, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if AHCI_VERBOSE {
            self.fb_log(&alloc::format!("X99: Usando MMIO AHCI: 0x{:08X}", ahci_base), Color::GREEN);
        }
        
        unsafe {
            // Leer CAP (Capabilities) - offset 0x00
            let cap = core::ptr::read_volatile((ahci_base + 0x00) as *const u32);
            // Leer GHC (Global Host Control) - offset 0x04
            let mut ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
            
            if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: CAP: 0x{:08X}, GHC: 0x{:08X}", cap, ghc), Color::CYAN); }
            
            // Verificar que AHCI esté habilitado (GHC bit 31)
            if (ghc & 0x80000000) == 0 {
                self.fb_log("X99: Habilitando AHCI...", Color::YELLOW);
                ghc |= 0x80000000; // Habilitar AHCI
                core::ptr::write_volatile((ahci_base + 0x04) as *mut u32, ghc);
                
                // Esperar a que se habilite
                for _ in 0..100000 {
                    let new_ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
                    if (new_ghc & 0x80000000) != 0 {
                        break;
                    }
                    core::hint::spin_loop();
                }
                
                ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
                if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: GHC después de habilitar: 0x{:08X}", ghc), Color::CYAN); }
            }
            
            // Leer PI (Ports Implemented) - offset 0x0C
            let pi = core::ptr::read_volatile((ahci_base + 0x0C) as *const u32);
            if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: PI: 0x{:08X} (puertos implementados)", pi), Color::CYAN); }
            
            // Verificar que el puerto solicitado esté implementado
            if (pi & (1 << port)) == 0 {
                if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Puerto {} no implementado (PI: 0x{:08X})", port, pi), Color::RED); }
                return Err("Puerto no implementado");
            }
            
            if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Puerto {} implementado correctamente", port), Color::GREEN); }
            
            // Calcular base del puerto (puerto 0 = 0x100, puerto 1 = 0x180, etc.)
            let port_base = ahci_base + 0x100 + (port as u32 * 0x80);
            if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Puerto {} base: 0x{:08X}", port, port_base), Color::CYAN); }
            
            // === INICIALIZACIÓN DE PUERTO BASADA EN LINUX ahci.c ===
            
            // 1) Detener puerto completamente (como ahci_stop_port en Linux)
            if AHCI_VERBOSE { self.fb_log("X99: Deteniendo puerto...", Color::YELLOW); }
            let mut px_cmd = core::ptr::read_volatile((port_base + 0x18) as *const u32);
            px_cmd &= !(0x01 | 0x10); // Limpiar ST (bit 0) y FRE (bit 4)
            core::ptr::write_volatile((port_base + 0x18) as *mut u32, px_cmd);
            
            // Esperar a que CR (Command Running) y FR (FIS Receive) se limpien
            for _ in 0..1_000_000 {
                let status = core::ptr::read_volatile((port_base + 0x18) as *const u32);
                let cr = (status & 0x0000_8000) != 0; // CR bit 15
                let fr = (status & 0x0000_0040) != 0;  // FR bit 6
                if !cr && !fr { break; }
                core::hint::spin_loop();
            }
            
            // 2) Limpiar interrupciones y errores (como ahci_port_intr en Linux)
            if AHCI_VERBOSE { self.fb_log("X99: Limpiando interrupciones y errores...", Color::YELLOW); }
            core::ptr::write_volatile((port_base + 0x10) as *mut u32, 0xFFFF_FFFF); // PxIS - limpiar todas las interrupciones
            core::ptr::write_volatile((port_base + 0x30) as *mut u32, 0xFFFF_FFFF); // PxSERR - limpiar todos los errores
            
            // 3) COMRESET sequence (como ahci_port_resume en Linux)
            if AHCI_VERBOSE { self.fb_log("X99: Ejecutando COMRESET...", Color::YELLOW); }
            let mut px_sctl = core::ptr::read_volatile((port_base + 0x2C) as *const u32);
            
            // DET = 1 (COMRESET)
            px_sctl = (px_sctl & !0x0F) | 0x01;
            core::ptr::write_volatile((port_base + 0x2C) as *mut u32, px_sctl);
            
            // Esperar 10ms como en Linux
            for _ in 0..100_000 { core::hint::spin_loop(); }
            
            // DET = 0 (normal operation)
            px_sctl = (px_sctl & !0x0F) | 0x00;
            core::ptr::write_volatile((port_base + 0x2C) as *mut u32, px_sctl);
            
            // Esperar a que el dispositivo se detecte (DET = 3)
            if AHCI_VERBOSE { self.fb_log("X99: Esperando detección de dispositivo...", Color::YELLOW); }
            for _ in 0..2_000_000 {
                let px_ssts = core::ptr::read_volatile((port_base + 0x28) as *const u32);
                let det_status = px_ssts & 0x0F;
                let ipm_status = (px_ssts >> 8) & 0x0F;
                
                if det_status == 0x03 {
                    if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Dispositivo detectado - DET:0x{:01X}, IPM:0x{:01X}", det_status, ipm_status), Color::GREEN); }
                    // En modo conciso, anunciar solo puertos con enlace
                    if !AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: Link activo en puerto {} (DET=3)", port), Color::GREEN); }
                    break;
                } else if det_status == 0x00 {
                    if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: No hay dispositivo - DET:0x{:01X}", det_status), Color::RED); }
                    return Err("Dispositivo no presente");
                }
                core::hint::spin_loop();
            }
            
            // 4) Configurar estructuras de datos (como ahci_port_start en Linux)
            if AHCI_VERBOSE { self.fb_log("X99: Configurando estructuras de datos...", Color::YELLOW); }
            
            // Configurar FIS Receive Area (DMA alineado)
            let fis_base = get_dma_fis_base(port);
            core::ptr::write_volatile((port_base + 0x08) as *mut u32, fis_base);
            core::ptr::write_volatile((port_base + 0x0C) as *mut u32, 0);
            
            // Configurar Command List (DMA alineado)
            let cmd_list_base = get_dma_clb_base(port);
            core::ptr::write_volatile((port_base + 0x00) as *mut u32, cmd_list_base);
            core::ptr::write_volatile((port_base + 0x04) as *mut u32, 0);
            
            // Limpiar FIS y Command List
            unsafe { AHCI_FIS_AREA[port as usize].0.fill(0); }
            unsafe { AHCI_CMD_LIST[port as usize].0.fill(0); }
            
            // 5) Habilitar recepción de FIS (FRE) primero
            if AHCI_VERBOSE { self.fb_log("X99: Habilitando recepción de FIS...", Color::YELLOW); }
            px_cmd = core::ptr::read_volatile((port_base + 0x18) as *const u32);
            px_cmd |= 0x10; // FRE (FIS Receive Enable)
            core::ptr::write_volatile((port_base + 0x18) as *mut u32, px_cmd);
            
            // Esperar a que FRE se estabilice
            for _ in 0..200_000 { core::hint::spin_loop(); }
            
            // 6) Arrancar el puerto (ST)
            if AHCI_VERBOSE { self.fb_log("X99: Arrancando puerto...", Color::YELLOW); }
            px_cmd |= 0x01; // ST (Start)
            core::ptr::write_volatile((port_base + 0x18) as *mut u32, px_cmd);
            
            // Esperar a que el puerto esté listo (CR = 0)
            for _ in 0..1_000_000 {
                let status = core::ptr::read_volatile((port_base + 0x18) as *const u32);
                if (status & 0x8000) == 0 { // CR bit 15 = 0
                    break;
                }
                core::hint::spin_loop();
            }
            
            // 7) Configurar comando READ SECTOR
            if AHCI_VERBOSE { self.fb_log("X99: Configurando comando READ SECTOR...", Color::YELLOW); }
            
            // Configurar Command Table (DMA alineado)
            let cmd_table_base = get_dma_ct_base(port);
            let cmd_list_entry = (cmd_table_base & 0xFFFFFFF0) | 0x05; // 5 DWORDS FIS + PRD
            core::ptr::write_volatile((cmd_list_base) as *mut u32, cmd_list_entry);
            
            // Configurar FIS de READ SECTOR en Command Table
            let cmd_table = cmd_table_base as *mut u32;
            *cmd_table.add(0) = 0x8027EC00; // FIS Register H2D + Command Register (0x20 = READ SECTOR)
            *cmd_table.add(1) = 0x00000000; // Features Low/High
            *cmd_table.add(2) = (sector & 0xFFFFFFFF) as u32; // LBA Low/Mid/High
            *cmd_table.add(3) = ((sector >> 32) & 0xFFFFFFFF) as u32; // LBA Low/Mid/High
            *cmd_table.add(4) = 0x00000000; // Count Low/High, ICC, Control
            
            // Configurar PRD (Physical Region Descriptor)
            *cmd_table.add(5) = buffer.as_ptr() as u32; // Data Base Address
            *cmd_table.add(6) = 0x00000000; // Data Base Address High
            *cmd_table.add(7) = (buffer.len() as u32 - 1) | 0x80000000; // Byte Count + Interrupt
            
            // Limpiar buffer
            for b in buffer.iter_mut() { *b = 0; }
            
            // 8) Ejecutar comando
            if AHCI_VERBOSE { self.fb_log("X99: Ejecutando comando READ SECTOR...", Color::YELLOW); }
            core::ptr::write_volatile((port_base + 0x38) as *mut u32, 1); // PxCI bit 0
            
            // Esperar a que termine
            for _ in 0..2_000_000 {
                let px_ci = core::ptr::read_volatile((port_base + 0x38) as *const u32);
                if px_ci == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            
            // 9) Verificar resultado
            if AHCI_VERBOSE { self.fb_log(&alloc::format!("X99: AHCI puerto {} completado, leyendo datos...", port), Color::GREEN); }
            
            // Verificar que tenemos datos válidos
            let signature = ((buffer[511] as u16) << 8) | (buffer[510] as u16);
            if signature == 0x55AA {
                self.fb_log(&alloc::format!("X99: ¡MBR válido en puerto {}!", port), Color::GREEN);
            } else if AHCI_VERBOSE {
                self.fb_log(&alloc::format!("X99: Puerto {} - MBR inválido (sig: 0x{:04X})", port, signature), Color::RED);
            }
        }
        
        Ok(())
    }

    /// Leer puerto AHCI usando HBA dual: map_base para PI/SSTS y exec_base para comandos
    fn read_ahci_port_with_map_and_exec(&self, map_base: u32, exec_base: u32, port: u8, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        // Leer PI desde map_base
        let pi_map = unsafe { core::ptr::read_volatile((map_base + 0x0C) as *const u32) };
        if (pi_map & (1 << port)) == 0 {
            return Err("Puerto no implementado (map)");
        }

        // Comprobar DET desde map_base (PxSSTS)
        let port_base_map = map_base + 0x100 + (port as u32 * 0x80);
        let px_ssts = unsafe { core::ptr::read_volatile((port_base_map + 0x28) as *const u32) };
        let det_status = px_ssts & 0x0F;
        if det_status != 0x03 {
            return Err("Sin enlace (DET != 3)");
        }

        // Ejecutar comando en exec_base sin validar PI allí (algunas PCH reportan PI distinto)
        self.read_ahci_port_direct_with_mmio(exec_base, port, sector, buffer)
    }

    /// Fallback a driver ATA cuando AHCI falla
    fn read_ahci_port_direct(&self, port: u8, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        // Usar MMIO base exacto de Linux: 0x92f24000 (Region 0)
        let ahci_base = 0x92f24000u32;
        
        self.fb_log(&alloc::format!("X99: Usando MMIO AHCI Linux: 0x{:08X}", ahci_base), Color::GREEN);
        
        unsafe {
            // Leer CAP (Capabilities) - offset 0x00
            let cap = core::ptr::read_volatile((ahci_base + 0x00) as *const u32);
            // Leer GHC (Global Host Control) - offset 0x04
            let mut ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
            
            self.fb_log(&alloc::format!("X99: CAP: 0x{:08X}, GHC: 0x{:08X}", cap, ghc), Color::CYAN);
            
            // Verificar que AHCI esté habilitado (GHC bit 31)
            if (ghc & 0x80000000) == 0 {
                self.fb_log("X99: Habilitando AHCI...", Color::YELLOW);
                ghc |= 0x80000000; // Habilitar AHCI
                core::ptr::write_volatile((ahci_base + 0x04) as *mut u32, ghc);
                
                // Esperar a que se habilite
                for _ in 0..100000 {
                    let new_ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
                    if (new_ghc & 0x80000000) != 0 {
                        break;
                    }
                    core::hint::spin_loop();
                }
                
                ghc = core::ptr::read_volatile((ahci_base + 0x04) as *const u32);
                self.fb_log(&alloc::format!("X99: GHC después de habilitar: 0x{:08X}", ghc), Color::CYAN);
            }
            
            // Leer PI (Ports Implemented) - offset 0x0C
            let pi = core::ptr::read_volatile((ahci_base + 0x0C) as *const u32);
            self.fb_log(&alloc::format!("X99: PI: 0x{:08X} (puertos implementados)", pi), Color::CYAN);
            
            // Verificar que el puerto solicitado esté implementado
            if (pi & (1 << port)) == 0 {
                self.fb_log(&alloc::format!("X99: Puerto {} no implementado (PI: 0x{:08X})", port, pi), Color::RED);
                return Err("Puerto no implementado");
            }
            
            self.fb_log(&alloc::format!("X99: Puerto {} implementado correctamente", port), Color::GREEN);
            
            // Calcular base del puerto (puerto 0 = 0x100, puerto 1 = 0x180, etc.)
            let port_base = ahci_base + 0x100 + (port as u32 * 0x80);
            self.fb_log(&alloc::format!("X99: Puerto {} base: 0x{:08X}", port, port_base), Color::CYAN);
            
            // Leer PxSSTS (Port Status) - offset 0x28 desde port_base
            let px_ssts = core::ptr::read_volatile((port_base + 0x28) as *const u32);
            let det_status = px_ssts & 0x0F;
            let ipm_status = (px_ssts >> 8) & 0x0F;
            
            self.fb_log(&alloc::format!("X99: Puerto {} PxSSTS: 0x{:08X} (DET:0x{:01X}, IPM:0x{:01X})", 
                port, px_ssts, det_status, ipm_status), Color::CYAN);
            
            // Verificar estado del dispositivo
            if det_status == 0 {
                self.fb_log(&alloc::format!("X99: Puerto {} - No hay dispositivo (DET=0x0)", port), Color::RED);
                return Err("Dispositivo no presente");
            } else if det_status != 0x3 {
                self.fb_log(&alloc::format!("X99: Puerto {} - Dispositivo en estado DET=0x{:01X} (no óptimo, pero continuando)", 
                    port, det_status), Color::YELLOW);
                // Continuar de todas formas - algunos dispositivos pueden funcionar
            }
            
            // Leer PxSIG (Port Signature) - offset 0x24
            let px_sig = core::ptr::read_volatile((port_base + 0x24) as *const u32);
            self.fb_log(&alloc::format!("X99: Puerto {} PxSIG: 0x{:08X}", port, px_sig), Color::CYAN);
            
            // Configurar FIS Receive Area - usar buffers DMA alineados
            let fis_base = get_dma_fis_base(port);
            core::ptr::write_volatile((port_base + 0x08) as *mut u32, fis_base);
            core::ptr::write_volatile((port_base + 0x0C) as *mut u32, 0);
            
            // Configurar Command List - usar buffers DMA alineados
            let cmd_list_base = get_dma_clb_base(port);
            core::ptr::write_volatile((port_base + 0x00) as *mut u32, cmd_list_base);
            core::ptr::write_volatile((port_base + 0x04) as *mut u32, 0);
            
            // Limpiar FIS y Command List
            unsafe { AHCI_FIS_AREA[port as usize].0.fill(0); }
            unsafe { AHCI_CMD_LIST[port as usize].0.fill(0); }
            
            // Configurar Command Table - usar buffers DMA alineados
            let cmd_table_base = get_dma_ct_base(port);
            let cmd_list_entry = (cmd_table_base & 0xFFFFFFF0) | 0x05; // 5 DWORDS FIS + PRD
            core::ptr::write_volatile((cmd_list_base) as *mut u32, cmd_list_entry);
            
            // Configurar FIS de READ SECTOR en Command Table
            let cmd_table = cmd_table_base as *mut u32;
            *cmd_table.add(0) = 0x8027EC00; // FIS Register H2D + Command Register (0x20 = READ SECTOR)
            *cmd_table.add(1) = 0x00000000; // Features Low/High
            *cmd_table.add(2) = (sector & 0xFFFFFFFF) as u32; // LBA Low/Mid/High
            *cmd_table.add(3) = ((sector >> 32) & 0xFFFFFFFF) as u32; // LBA Low/Mid/High
            *cmd_table.add(4) = 0x00000000; // Count Low/High, ICC, Control
            
            // Configurar PRD (Physical Region Descriptor)
            *cmd_table.add(5) = buffer.as_ptr() as u32; // Data Base Address
            *cmd_table.add(6) = 0x00000000; // Data Base Address High
            *cmd_table.add(7) = (buffer.len() as u32 - 1) | 0x80000000; // Byte Count + Interrupt
            
            // Limpiar buffer
            for b in buffer.iter_mut() { *b = 0; }
            
            // Habilitar el puerto (PxCMD bit 0)
            let px_cmd = core::ptr::read_volatile((port_base + 0x18) as *const u32);
            core::ptr::write_volatile((port_base + 0x18) as *mut u32, px_cmd | 0x01);
            
            // Esperar a que el puerto esté listo (PxCMD bit 15 = 0)
            for _ in 0..1000000 {
                let px_cmd = core::ptr::read_volatile((port_base + 0x18) as *const u32);
                if (px_cmd & 0x8000) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            
            // Ejecutar comando (PxCI bit 0)
            core::ptr::write_volatile((port_base + 0x38) as *mut u32, 1);
            
            // Esperar a que termine
            for _ in 0..1000000 {
                let px_ci = core::ptr::read_volatile((port_base + 0x38) as *const u32);
                if px_ci == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            
            // Leer datos del buffer
            self.fb_log(&alloc::format!("X99: AHCI puerto {} completado, leyendo datos...", port), Color::GREEN);
            
            // Verificar que tenemos datos válidos
            let signature = ((buffer[511] as u16) << 8) | (buffer[510] as u16);
            self.fb_log(&alloc::format!("X99: AHCI puerto {} sig: 0x{:04X}", port, signature), Color::GREEN);
            
            if signature == 0x55AA {
                self.fb_log(&alloc::format!("X99: ¡MBR válido encontrado en puerto {}!", port), Color::GREEN);
            }
        }
        
        Ok(())
    }
    
    /// Obtener la dirección MMIO base del controlador AHCI desde PCI
    fn get_ahci_mmio_base(&self) -> Result<u32, &'static str> {
        // Usar directamente el MMIO base conocido de Linux
        self.fb_log("X99: Usando MMIO AHCI conocido de Linux...", Color::GREEN);
        Ok(0x92f24000u32)
    }
    
    /// Leer palabra de configuración PCI con inicialización robusta
    fn read_pci_config_word(&self, bus: u8, device: u8, function: u8, offset: u8) -> u16 {
        let address = 0x80000000u32 | 
            ((bus as u32) << 16) | 
            ((device as u32) << 11) | 
            ((function as u32) << 8) | 
            ((offset as u32) & 0xFC);
        
        unsafe {
            // Inicializar puertos PCI si es necesario
            self.initialize_pci_configuration_space();
            
            // Escribir dirección PCI
            core::ptr::write_volatile(0xCF8 as *mut u32, address);
            
            // Pausa más larga para estabilización
            for _ in 0..1000 { core::hint::spin_loop(); }
            
            // Múltiples intentos de lectura para estabilidad
            let mut data = 0u32;
            for attempt in 0..10 {
                data = core::ptr::read_volatile(0xCFC as *const u32);
                
                // Si leemos datos válidos (no todo ceros ni FFFF), usar ese valor
                if data != 0x00000000 && data != 0xFFFFFFFF {
                    break;
                }
                
                // Pausa más larga entre intentos
                for _ in 0..10000 { core::hint::spin_loop(); }
                
                // Re-escribir dirección PCI para reinicializar
                core::ptr::write_volatile(0xCF8 as *mut u32, address);
            }
            
            // Extraer la palabra correcta según el offset
            if (offset & 0x02) == 0 {
                (data & 0xFFFF) as u16
                } else {
                ((data >> 16) & 0xFFFF) as u16
            }
        }
    }
    
    /// Inicializar el espacio de configuración PCI
    fn initialize_pci_configuration_space(&self) {
        unsafe {
            // Verificar si los puertos PCI están disponibles
            let test_address = 0x80000000u32; // Bus 0, Device 0, Function 0, Offset 0
            
            // Escribir dirección de prueba
            core::ptr::write_volatile(0xCF8 as *mut u32, test_address);
            
            // Pausa para estabilización
            for _ in 0..10000 { core::hint::spin_loop(); }
            
            // Leer datos de prueba
            let test_data = core::ptr::read_volatile(0xCFC as *const u32);
            
            self.fb_log(&alloc::format!("X99: PCI Test - Dirección: 0x{:08X}, Datos: 0x{:08X}", 
                test_address, test_data), Color::CYAN);
            
            // Si los datos son válidos, los puertos PCI funcionan
            if test_data != 0x00000000 && test_data != 0xFFFFFFFF {
                self.fb_log("X99: Puertos PCI funcionando correctamente", Color::GREEN);
                } else {
                self.fb_log("X99: Puertos PCI no responden", Color::RED);
            }
        }
    }
    
    /// Leer dword de configuración PCI
    fn read_pci_config_dword(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        let address = 0x80000000u32 | 
            ((bus as u32) << 16) | 
            ((device as u32) << 11) | 
            ((function as u32) << 8) | 
            ((offset as u32) & 0xFC);
        
        unsafe {
            core::ptr::write_volatile(0xCF8 as *mut u32, address);
            core::ptr::read_volatile(0xCFC as *const u32)
        }
    }
    
    fn read_ata_port_direct(&self, port: u8, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        // Puertos SATA específicos del chipset X99
        let base_port = match port {
            0 => 0x1F0,  // Puerto SATA 0 (primario)
            1 => 0x170,  // Puerto SATA 1 (secundario)  
            2 => 0x1E8,  // Puerto SATA 2
            3 => 0x168,  // Puerto SATA 3
            4 => 0x1E0,  // Puerto SATA 4
            5 => 0x160,  // Puerto SATA 5
            6 => 0x1D8,  // Puerto SATA 6
            7 => 0x158,  // Puerto SATA 7
            _ => return Err("Puerto SATA no válido")
        };
        
        // Leer sector usando puerto directo
        unsafe {
            // Seleccionar dispositivo (master)
            x86::io::outb(base_port + 6, 0xA0);
            // Comando READ SECTORS
            x86::io::outb(base_port + 7, 0x20);
            // Sector count
            x86::io::outb(base_port + 2, 1);
            // LBA low
            x86::io::outb(base_port + 3, (sector & 0xFF) as u8);
            // LBA mid
            x86::io::outb(base_port + 4, ((sector >> 8) & 0xFF) as u8);
            // LBA high
            x86::io::outb(base_port + 5, ((sector >> 16) & 0xFF) as u8);
            // Device/Head
            x86::io::outb(base_port + 6, 0x40 | (((sector >> 24) & 0x0F) as u8));
            
            // Esperar a que esté listo
            for _ in 0..1000 {
                let status = x86::io::inb(base_port + 7);
                if (status & 0x80) == 0 && (status & 0x08) != 0 {
                    break;
                }
            }
            
            // Leer datos
            for i in 0..256 {
                let word = x86::io::inw(base_port);
                buffer[i * 2] = (word & 0xFF) as u8;
                buffer[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
            }
        }
        
                Ok(())
    }
    
    fn read_ata_sector_fallback(&self, device: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Fallback ATA para {}\n", device.name));
        
        // Crear/usar driver ATA cacheado
        let ata_init_result = unsafe {
            if CACHED_ATA.is_none() {
                serial_write_str("STORAGE_MANAGER: Inicializando driver ATA (cache)\n");
                let mut drv = AtaDirectDriver::new_primary();
                if let Err(e) = drv.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA fallback: {}\n", e));
                    None
            } else {
                    CACHED_ATA = Some(drv);
                    serial_write_str("STORAGE_MANAGER: Driver ATA cacheado listo\n");
                    CACHED_ATA.as_mut()
                }
                } else {
                CACHED_ATA.as_mut()
            }
        };

        if ata_init_result.is_none() {
            serial_write_str("STORAGE_MANAGER: Error inicializando ATA fallback (cache)\n");
            return Err("Error inicializando ATA");
        }
        let ata_driver = unsafe { CACHED_ATA.as_mut().unwrap() };
        
        // Leer sector con ATA
        let sector_u32 = if sector > u32::MAX as u64 {
            serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
            0
        } else {
            sector as u32
        };
        
        match ata_driver.read_sector(sector_u32, buffer) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: ✅ Fallback ATA exitoso\n");
                Ok(())
            },
        Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector ATA fallback: {}\n", e));
                Err("Error leyendo sector ATA")
            }
        }
    }
    
    /// Datos simulados como último recurso
    fn read_simulated_sector(&self, _device: &StorageDeviceInfo, _sector: u64, _buffer: &mut [u8]) -> Result<(), &'static str> {
        Err("Datos simulados deshabilitados en hardware real")
    }
    
    /// Leer sector usando driver IDE
    fn read_ide_sector(&self, device: &StorageDeviceInfo, _sector: u64, _buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver IDE para {} (delegando a ATA)\n", device.name));
        // En QEMU, el dispositivo IDE funciona con el mismo acceso PIO que ATA.
        // Delegamos al fallback ATA para poder leer sectores.
        self.read_ata_sector_fallback(device, _sector, _buffer)
    }
    
    /// Leer sector usando driver NVMe
    fn read_nvme_sector(&self, device: &StorageDeviceInfo, _sector: u64, _buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver NVMe para {}\n", device.name));
        // TODO: Implementar driver NVMe real
        Err("Driver NVMe no implementado")
    }
    
    /// Leer sector usando driver VirtIO
    fn read_virtio_sector(&self, device: &StorageDeviceInfo, _sector: u64, _buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Usando driver VirtIO para {}\n", device.name));
        // TODO: Implementar driver VirtIO real
        Err("Driver VirtIO no implementado")
    }

    /// Leer sector de una partición
    pub fn read_from_partition(&self, device_name: &str, partition_index: u32, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        // CORRECCIÓN: EclipseFS pasa partition_index = 2 para /dev/sda2, pero necesitamos convertir a índice de array
        // Las particiones se almacenan con partition_index = 1 y 2, pero el array es 0-indexado
        let partition_array_index = (partition_index - 1) as usize;
        
        // Extraer el nombre del dispositivo base (sin número de partición)
        // Ej: "/dev/sda2" -> "/dev/sda", "/dev/sda1" -> "/dev/sda"
        let base_device_name = if device_name.ends_with(char::is_numeric) {
            // Encontrar donde termina el nombre base y empieza el número
            let mut base = device_name;
            while base.len() > 0 && base.chars().last().map_or(false, |c| c.is_numeric()) {
                base = &base[..base.len() - 1];
            }
            base
        } else {
            device_name
        };
        
        serial_write_str(&format!("STORAGE_MANAGER: Buscando partición index {} (array index {}) en device {} (base: {})\n", 
                                 partition_index, partition_array_index, device_name, base_device_name));
        
        if partition_array_index >= self.partitions.len() {
            return Err("Índice de partición fuera de rango");
        }
        
        let partition = &self.partitions[partition_array_index];
        
        // Verificar que el nombre del dispositivo base coincida
        if !device_name.is_empty() && !base_device_name.is_empty() && partition.device_name != base_device_name {
            serial_write_str(&format!("STORAGE_MANAGER: ADVERTENCIA - device_name base '{}' no coincide con partition.device_name '{}'\n",
                                     base_device_name, partition.device_name));
            // Continuar de todos modos si el partition_index es correcto
        }
        
        // Calcular sector absoluto
        let absolute_sector = partition.start_lba + sector;
        
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} (absoluto: {}) de partición {}:{}\n", 
                                 sector, absolute_sector, partition.device_name, partition_index));
        
        // Usar el device_name de la partición almacenada
        self.read_device_sector(&partition.device_name, absolute_sector, buffer)
    }

    /// Obtener dispositivos candidatos para FAT32
    pub fn get_fat32_candidates(&self) -> Vec<Fat32DeviceInfo> {
        let mut candidates = Vec::new();
        
        for partition in &self.partitions {
            if partition.filesystem_type == "FAT32" {
                let device_info = Fat32DeviceInfo {
                    device_name: partition.device_name.clone(),
                    start_lba: partition.start_lba,
                    size_lba: partition.size_lba,
                    additional_info: Some(format!("partition_index: {}", partition.partition_index)),
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
                    start_lba: partition.start_lba,
                    size_lba: partition.size_lba,
                    additional_info: Some(format!("partition_index: {}", partition.partition_index)),
                };
                candidates.push(device_info);
            }
        }
        
        candidates
    }

    /// Obtener número de dispositivos (para compatibilidad)
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Encontrar el mejor dispositivo de almacenamiento (para compatibilidad)
    pub fn find_best_storage_device(&self) -> Option<usize> {
        // Retornar el primer dispositivo disponible
        if !self.devices.is_empty() {
            Some(0)
                        } else {
            None
        }
    }

    /// Escribir a una partición (para compatibilidad)
    pub fn write_to_partition(&mut self, device_name: &str, partition_index: u32, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        // Encontrar la partición
        let partition = self.partitions.iter()
            .find(|p| p.device_name == device_name && p.partition_index == partition_index)
            .ok_or("Partición no encontrada")?;
        
        // Calcular sector absoluto
        let absolute_sector = partition.start_lba + sector;
        
        serial_write_str(&format!("STORAGE_MANAGER: Escribiendo sector {} (absoluto: {}) a partición {}:{}\n", 
                                 sector, absolute_sector, device_name, partition_index));
        
        // TODO: Implementar escritura real a la partición
        Ok(())
    }

    /// Encontrar partición por nombre (para compatibilidad)
    pub fn find_partition_by_name(&self, name: &str) -> Option<&PartitionInfo> {
        self.partitions.iter().find(|p| p.name == name)
    }

    /// Obtener dispositivo por índice (para compatibilidad)
    pub fn get_device(&self, index: usize) -> Option<&StorageDeviceInfo> {
        self.devices.get(index)
    }

    /// Leer sector de un dispositivo con tipo (para compatibilidad)
    pub fn read_device_sector_with_type(&self, device_info: &StorageDeviceInfo, sector: u64, buffer: &mut [u8], sector_type: StorageSectorType) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} de {} (tipo: {:?})\n", sector, device_info.name, sector_type));
        
        // Usar el driver apropiado según el tipo de controlador
        match device_info.controller_type {
            StorageControllerType::AHCI => {
                self.read_ahci_sector(device_info, sector, buffer)
            },
            StorageControllerType::IDE => {
                self.read_ide_sector(device_info, sector, buffer)
            },
            StorageControllerType::ATA => {
                self.read_ata_sector_fallback(device_info, sector, buffer)
            },
            StorageControllerType::NVMe => {
                self.read_nvme_sector(device_info, sector, buffer)
            },
            StorageControllerType::VirtIO => {
                self.read_virtio_sector(device_info, sector, buffer)
            },
        }
    }

    /// Leer sector de un dispositivo (alias para compatibilidad)
    pub fn read_device_sector_real(&self, device_name: &str, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        self.read_device_sector(device_name, sector, buffer)
    }

    /// Escribir sector a un dispositivo (para compatibilidad)
    pub fn write_device_sector(&self, device_name: &str, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Escribiendo sector {} a {}\n", sector, device_name));
        
        // Encontrar el dispositivo
        let device = self.devices.iter()
            .find(|d| d.name == device_name)
            .ok_or("Dispositivo no encontrado")?;
        
        // TODO: Implementar escritura real
        serial_write_str("STORAGE_MANAGER: Escritura simulada (no implementada)\n");
        Ok(())
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

/// Alias para compatibilidad
pub fn init_storage_manager() -> Result<(), &'static str> {
    initialize_storage_manager()
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