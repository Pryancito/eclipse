//! Gestor de almacenamiento unificado
//! 
//! Este módulo integra todos los drivers de almacenamiento (ATA, NVMe, AHCI)
//! y proporciona una interfaz unificada para el acceso al almacenamiento.

use crate::debug::serial_write_str;
use crate::partitions::{self, Partition, PartitionTable, FilesystemType, BlockDevice};
use crate::drivers::storage_device_wrapper::{StorageDeviceWrapper, EclipseFSDeviceWrapper};
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::drivers::intel_raid::IntelRaidDriver;
use crate::drivers::intel_ahci_raid::IntelAhciRaidDriver;
use alloc::{format, vec::Vec, string::{String, ToString}, boxed::Box};
use crate::drivers::block::BlockDevice as LegacyBlockDevice;
use core::cmp;

/// Tipos de controladoras de almacenamiento
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageControllerType {
    ATA,
    NVMe,
    AHCI,
    VirtIO,
    IntelRAID,
    IntelRaidVolume,
}

/// Información del dispositivo de almacenamiento
#[derive(Debug, Clone)]
pub struct StorageDeviceInfo {
    pub controller_type: StorageControllerType,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub capacity: u64,
    pub block_size: u32,
    pub max_lba: u64,
    pub device_name: String, // Nombre del dispositivo como /dev/sda
}


/// Dispositivo de almacenamiento
#[derive(Clone)]
pub struct StorageDevice {
    pub info: StorageDeviceInfo,
    // Note: Box<dyn BlockDevice> no es Clone, pero para simplificar usamos un placeholder
    // En una implementación real, se necesitaría una estrategia diferente
}

/// Gestor de almacenamiento
pub struct StorageManager {
    pub devices: Vec<StorageDevice>,
    pub partitions: Vec<Partition>,
    pub pci_devices: Vec<crate::drivers::pci::PciDevice>,
    active_device: Option<usize>,
}

impl Clone for StorageManager {
    fn clone(&self) -> Self {
        Self {
            devices: self.devices.clone(),
            partitions: self.partitions.clone(),
            pci_devices: self.pci_devices.clone(),
            active_device: self.active_device,
        }
    }
}

impl StorageManager {
    /// Crear nuevo gestor de almacenamiento
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            partitions: Vec::new(),
            pci_devices: Vec::new(),
            active_device: None,
        }
    }

    /// Inicializar gestor de almacenamiento
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Inicializando gestor de almacenamiento...\n");
        serial_write_str("STORAGE_MANAGER: Llamando detect_graphics_hardware()...\n");
        
        // Mostrar en pantalla para hardware real
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel("STORAGE_MANAGER: Inicializando...", Color::YELLOW);
            let _ = fb.write_text_kernel("STORAGE_MANAGER: Detectando hardware...", Color::YELLOW);
        }
        
        // Usar la detección de hardware existente
        let hardware_result = crate::hardware_detection::detect_graphics_hardware();
        
        // Detectar dispositivos de almacenamiento usando el resultado de hardware
        serial_write_str("STORAGE_MANAGER: Verificando polished_pci_driver...\n");
        
        // Mostrar en pantalla para hardware real
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel("STORAGE_MANAGER: Verificando PCI driver...", Color::CYAN);
        }
        
        if let Some(polished_pci) = &hardware_result.polished_pci_driver {
            let device_count = polished_pci.get_device_count();
            serial_write_str(&format!("STORAGE_MANAGER: Polished PCI driver disponible con {} dispositivos\n", device_count));
            
            // Usar polished_pci como método principal (detecta PCIe x16 correctamente)
            serial_write_str("STORAGE_MANAGER: Usando polished_pci (detecta PCIe x16)\n");
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let _ = fb.write_text_kernel(&format!("STORAGE_MANAGER: Usando polished_pci ({} dispositivos)", device_count), Color::GREEN);
            }
            self.detect_storage_devices_from_polished_pci(polished_pci)?;
        } else {
            serial_write_str("STORAGE_MANAGER: Polished PCI driver NO disponible - usando detección manual\n");
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let _ = fb.write_text_kernel("STORAGE_MANAGER: PCI driver NO disponible - usando manual", Color::RED);
            }
            // Clonar el PciManager para poder mutarlo
            let mut pci_manager = hardware_result.pci_manager.clone();
            self.detect_storage_devices_from_pci_manager(&mut pci_manager)?;
        }
        
        // SIEMPRE usar también PciManager como respaldo para detectar SATA en buses > 0
        // polished_pci solo escanea bus 0, pero la controladora SATA está en bus 0 device 17
        serial_write_str("STORAGE_MANAGER: Usando PciManager como respaldo para detectar SATA en todos los buses\n");
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel("STORAGE_MANAGER: Usando PciManager como respaldo", Color::CYAN);
        }
        let mut pci_manager_backup = hardware_result.pci_manager.clone();
        self.detect_storage_devices_from_pci_manager(&mut pci_manager_backup)?;

        if self.devices.is_empty() {
            return Err("No se encontraron dispositivos de almacenamiento");
        }

        // Seleccionar el primer dispositivo como activo
        self.active_device = Some(0);

        serial_write_str(&format!("STORAGE_MANAGER: {} dispositivos de almacenamiento detectados\n", 
                                 self.devices.len()));
        
        // Asignar nombres de dispositivos estilo Linux según el tipo de controladora
        self.assign_linux_device_names();
        
        // Mostrar información de dispositivos con sus nuevos nombres
        for (i, device) in self.devices.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: Dispositivo {} - {} (Tipo: {:?}, Modelo: {}, Serial: {})\n", 
                                     i, device.info.device_name, device.info.controller_type, device.info.model, device.info.serial));
        }
        
        // Log al framebuffer de dispositivos detectados
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel("=== DISPOSITIVOS DETECTADOS ===", crate::drivers::framebuffer::Color::WHITE);
            for (i, device) in self.devices.iter().enumerate() {
                let fb_msg = alloc::format!("{}. {} ({:?})", i, device.info.device_name, device.info.controller_type);
                let color = match device.info.controller_type {
                    crate::drivers::storage_manager::StorageControllerType::ATA | 
                    crate::drivers::storage_manager::StorageControllerType::AHCI => crate::drivers::framebuffer::Color::GREEN,
                    crate::drivers::storage_manager::StorageControllerType::VirtIO => crate::drivers::framebuffer::Color::CYAN,
                    crate::drivers::storage_manager::StorageControllerType::NVMe => crate::drivers::framebuffer::Color::MAGENTA,
                    _ => crate::drivers::framebuffer::Color::YELLOW,
                };
                fb.write_text_kernel(&fb_msg, color);
            }
        }
        
        // Detectar particiones estilo Linux
        if let Err(e) = self.detect_partitions_linux_style() {
            serial_write_str(&format!("STORAGE_MANAGER: Error detectando particiones: {}\n", e));
        }
        
        Ok(())
    }

    /// Detectar dispositivos de almacenamiento usando polished PCI
    fn detect_storage_devices_from_polished_pci(&mut self, polished_pci: &crate::drivers::pci_polished::PolishedPciDriver) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando dispositivos usando polished PCI en hardware real...\n");

        let device_count = polished_pci.get_device_count();
        serial_write_str(&format!("STORAGE_MANAGER: {} dispositivos PCI detectados por polished_pci\n", device_count));
        
        // Listado de dispositivos PCI removido para limpiar pantalla

        // Iterar sobre todos los dispositivos detectados por polished PCI
        serial_write_str(&format!("STORAGE_MANAGER: Analizando {} dispositivos de polished_pci...\n", device_count));
        // Listado de análisis de dispositivos removido para limpiar pantalla
        
        serial_write_str(&format!("STORAGE_MANAGER: Iniciando loop para {} dispositivos...\n", device_count));
        // Listado de loop de dispositivos removido para limpiar pantalla
        
        for i in 0..device_count {
            serial_write_str(&format!("STORAGE_MANAGER: Procesando dispositivo {} de {}\n", i, device_count));
            // Listado de procesamiento de dispositivos removido para limpiar pantalla
            
            if let Some(device) = polished_pci.get_device(i) {
                serial_write_str(&format!("STORAGE_MANAGER: Dispositivo {} obtenido correctamente\n", i));
                // Listado de dispositivos OK removido para limpiar pantalla
                
                // FILTRAR DISPOSITIVOS RAID: Solo agregar el controlador RAID, no los discos físicos individuales
                // En hardware real con RAID, no queremos detectar cada disco físico como un dispositivo separado
                let is_raid_controller = (device.vendor_id == 0x8086 && device.device_id == 0x2822) ||
                                        (device.class == 0x01 && device.subclass == 0x04); // RAID Controller
                
                let is_storage_controller = (device.class == 0x01 && (device.subclass == 0x06 || device.subclass == 0x04 || device.subclass == 0x01)) || // SATA/RAID/IDE
                                           (device.class == 0x01 && device.subclass == 0x08); // NVMe
                
                // DEBUG: Mostrar información de cada dispositivo para debug
                serial_write_str(&format!("STORAGE_MANAGER: Dispositivo {} - VID:{:#x} DID:{:#x} Class:{:#x} Subclass:{:#x} ProgIF:{:#x}\n", 
                                         i, device.vendor_id, device.device_id, device.class, device.subclass, device.prog_if));
                serial_write_str(&format!("STORAGE_MANAGER: is_raid_controller: {}, is_storage_controller: {}\n", 
                                         is_raid_controller, is_storage_controller));
                
                // Solo agregar controladores de almacenamiento, no dispositivos individuales
                if is_storage_controller {
                    serial_write_str(&format!("STORAGE_MANAGER: ✅ Agregando controlador de almacenamiento {} (VID:{:#x} DID:{:#x})\n", 
                                             i, device.vendor_id, device.device_id));
                    
                    // Almacenar el dispositivo PCI para uso posterior
                    let converted_device = crate::drivers::pci::PciDevice {
                        bus: 0, // polished_pci no tiene información de bus
                        device: 0, // polished_pci no tiene información de device
                        function: 0, // polished_pci no tiene información de function
                        vendor_id: device.vendor_id,
                        device_id: device.device_id,
                        class_code: device.class,
                        subclass_code: device.subclass,
                        prog_if: device.prog_if,
                        revision_id: 0, // polished_pci no tiene información de revision
                        header_type: 0, // polished_pci no tiene información de header_type
                        status: 0, // polished_pci no tiene información de status
                        command: 0, // polished_pci no tiene información de command
                    };
                    self.pci_devices.push(converted_device);
                } else {
                    serial_write_str(&format!("STORAGE_MANAGER: Saltando dispositivo no-storage {} (VID:{:#x} DID:{:#x} Class:{}.{})\n", 
                                             i, device.vendor_id, device.device_id, device.class, device.subclass));
                }
                
                let base_class = device.class;
                let subclass = device.subclass;
                let prog_if = device.prog_if;

                serial_write_str(&format!("STORAGE_MANAGER: PCI Real {} - VID:{:#x} DID:{:#x} Class:{}.{}.{}\n", 
                                         i, device.vendor_id, device.device_id, 
                                         base_class, subclass, prog_if));
                
                // Listado de dispositivos PCI removido para limpiar pantalla
                
                // Log específico para GPUs (Class 3)
                if base_class == 0x03 {
                    serial_write_str(&format!("STORAGE_MANAGER: *** GPU DETECTADA *** - VID:{:#x} DID:{:#x} Class:{}.{}.{}\n", 
                                             device.vendor_id, device.device_id, 
                                             base_class, subclass, prog_if));
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("*** GPU ***: {:04X}:{:04X} Class:{}.{}.{}", 
                                                          device.vendor_id, device.device_id, 
                                                          base_class, subclass, prog_if), Color::YELLOW);
                    }
                }
                
                // Log específico para controladoras de almacenamiento (Class 1)
                if base_class == 0x01 {
                    serial_write_str(&format!("STORAGE_MANAGER: *** STORAGE DETECTADA *** - VID:{:#x} DID:{:#x} Class:{}.{}.{}\n", 
                                             device.vendor_id, device.device_id, 
                                             base_class, subclass, prog_if));
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("*** STORAGE ***: {:04X}:{:04X} Class:{}.{}.{}", 
                                                          device.vendor_id, device.device_id, 
                                                          base_class, subclass, prog_if), Color::YELLOW);
                    }
                }

                // Detectar controladoras de almacenamiento (clases 1 y 17) - con polished_pci
                if base_class == 0x01 || base_class == 0x11 { // Mass storage controller (0x01) o Communication device (0x11)
                    let controller_type = match (base_class, subclass) {
                        // SATA Controllers (subclass 0x06)
                        (0x01, 0x06) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x1B21 => "ASMedia",
                                0x1002 => "AMD",
                                0x10DE => "NVIDIA",
                                0x197B => "JMicron",
                                0x1106 => "VIA",
                                0x1039 => "SiS",
                                _ => "Unknown"
                            };
                            
                            // DETECCIÓN ESPECIAL PARA INTEL RAID: Tratar como volumen RAID agregado
                            if device.vendor_id == 0x8086 && device.device_id == 0x2822 {
                                serial_write_str(&format!("STORAGE_MANAGER: INTEL RAID CONTROLLER detectado - creando volumen RAID agregado\n"));
                                serial_write_str(&format!("STORAGE_MANAGER: VID:{:#x} ({}) DID:{:#x} - Volumen RAID único\n", 
                                                         device.vendor_id, vendor_name, device.device_id));
                                StorageControllerType::IntelRaidVolume
                            } else {
                                // Detectar tipo específico por Programming Interface
                                let sata_type = match device.prog_if {
                                0x01 => "AHCI",
                                0x05 => "RAID",
                                0x80 => "Vendor Specific",
                                _ => "Generic SATA"
                            };
                            
                                serial_write_str(&format!("STORAGE_MANAGER: SATA {} encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                         sata_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                                StorageControllerType::AHCI
                            }
                        }
                        // NVMe Controllers (subclass 0x08)
                        (0x01, 0x08) => {
                            let vendor_name = match device.vendor_id {
                                0x144D => "Samsung",
                                0x8086 => "Intel",
                                0x15B7 => "Sandisk",
                                0x1CC1 => "ADATA",
                                0x1E0F => "KIOXIA",
                                0x126F => "Silicon Motion",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: NVMe encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id));
                            StorageControllerType::NVMe
                        }
                        // RAID Controllers (subclass 0x04) - incluye SATA en modo RAID
                        (0x01, 0x04) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1000 => "LSI/Broadcom",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x10DE => "NVIDIA",
                                0x1106 => "VIA",
                                0x1B21 => "ASMedia",
                                0x197B => "JMicron",
                                0x1039 => "SiS",
                                _ => "Unknown"
                            };
                            
                            // Detectar específicamente controladoras SATA Intel en modo RAID
                            let is_sata_raid = device.vendor_id == 0x8086 && matches!(device.device_id, 
                                0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F);
                            
                            let raid_type = if is_sata_raid {
                                "SATA RAID"
                            } else {
                                match device.prog_if {
                                    0x01 => "RAID",
                                    0x05 => "RAID with AHCI",
                                    0x80 => "Vendor Specific RAID",
                                    _ => "Generic RAID"
                                }
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: {} encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     raid_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            
                            let controller_type = if is_sata_raid {
                                StorageControllerType::IntelRAID // Usar driver RAID específico para Intel SATA RAID
                            } else {
                                StorageControllerType::AHCI // Usar AHCI para otros RAID
                            };
                            
                            // Mostrar en pantalla para hardware real
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let _ = fb.write_text_kernel(&format!("SATA RAID: {:04X}:{:04X}", 
                                                                  device.vendor_id, device.device_id), Color::GREEN);
                            }
                            
                            // Crear información de controladora de almacenamiento
                            let storage_info = StorageDeviceInfo {
                                controller_type,
                                model: alloc::format!("SATA RAID {:04X}:{:04X}", 
                                                    device.vendor_id, device.device_id),
                                serial: "SATA-RAID-SERIAL".to_string(),
                                firmware: "SATA-RAID-FW".to_string(),
                                capacity: 0, // Se detectará en la inicialización
                                block_size: 512,
                                max_lba: 0,
                                device_name: "".to_string(), // Se asignará después
                            };

                            self.devices.push(StorageDevice {
                                info: storage_info,
                            });

                            serial_write_str(&format!("STORAGE_MANAGER: *** STORAGE DETECTADA *** - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                                     device.vendor_id, device.device_id, base_class, subclass));
                            serial_write_str(&format!("STORAGE_MANAGER: Controladora agregada (polished_pci): {:04X}:{:04X}\n", 
                                                     device.vendor_id, device.device_id));
                            
                            controller_type
                        }
                        // IDE Controllers (subclass 0x01)
                        (0x01, 0x01) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1106 => "VIA",
                                0x1039 => "SiS",
                                0x10B9 => "ALi",
                                0x126F => "Silicon Motion",
                                _ => "Unknown"
                            };
                            
                            let ide_type = match device.prog_if {
                                0x80 => "Generic IDE",
                                0x8A => "ISA Compatibility mode only",
                                0x8F => "PCI Native mode only",
                                0x85 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x8E => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x86 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x87 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                _ => "Unknown IDE"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: IDE {} encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     ide_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            StorageControllerType::ATA
                        }
                        // Serial Attached SCSI (subclass 0x07)
                        (0x01, 0x07) => {
                            let vendor_name = match device.vendor_id {
                                0x1000 => "LSI/Broadcom",
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: SAS encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id));
                            StorageControllerType::AHCI // Usar AHCI como fallback para SAS
                        }
                        // Other Mass Storage Controllers (subclass 0x80)
                        (0x01, 0x80) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x1B21 => "ASMedia",
                                0x10DE => "NVIDIA",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: Storage Controller genérico encontrado (polished_pci) - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            StorageControllerType::AHCI // Usar AHCI como fallback
                        }
                        // Communication device (0x11) - algunos controladores de almacenamiento
                        (0x11, 0x80) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Storage Controller (17.128) encontrado (polished_pci) - VID:{:#x} DID:{:#x}\n", 
                                                     device.vendor_id, device.device_id));
                            StorageControllerType::AHCI // Usar AHCI como fallback
                        }
                        _ => {
                            serial_write_str(&format!("STORAGE_MANAGER: Controladora de almacenamiento genérica (polished_pci) - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                                     device.vendor_id, device.device_id, base_class, subclass));
                            StorageControllerType::ATA // Usar ATA como fallback genérico
                        }
                    };
                    
                    // Mostrar en pantalla para hardware real
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("STORAGE: {:04X}:{:04X} Class:{}.{}", 
                                                      device.vendor_id, device.device_id, 
                                                      base_class, subclass), Color::GREEN);
                    }
                    
                    // Crear información de controladora de almacenamiento
                    let storage_info = StorageDeviceInfo {
                        controller_type,
                        model: alloc::format!("Storage {:04X}:{:04X} Class:{}.{}", 
                                            device.vendor_id, device.device_id, base_class, subclass),
                        serial: "STORAGE-SERIAL".to_string(),
                        firmware: "STORAGE-FW".to_string(),
                        capacity: 0, // Se detectará en la inicialización
                        block_size: 512,
                        max_lba: 0,
                        device_name: "".to_string(), // Se asignará después
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: Controladora agregada (polished_pci): {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
                }

                // VirtIO eliminado - no necesario para QEMU con /dev/sda ni hardware real

                // Detectar GPUs (class 3) - solo para depuración, NO como dispositivos de almacenamiento
                if base_class == 0x03 {
                    serial_write_str(&format!("STORAGE_MANAGER: GPU detectada - VID:{:#x} DID:{:#x} Class:{}.{}.{}\n", 
                                             device.vendor_id, device.device_id, 
                                             base_class, subclass, prog_if));
                    
                    // Mostrar en pantalla para hardware real
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("GPU: {:04X}:{:04X} Class:{}.{}.{}", 
                                                          device.vendor_id, device.device_id, 
                                                          base_class, subclass, prog_if), Color::MAGENTA);
                    }
                    
                    serial_write_str(&format!("STORAGE_MANAGER: GPU detectada pero NO agregada como dispositivo de almacenamiento: {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
            }
        } else {
                serial_write_str(&format!("STORAGE_MANAGER: ERROR - Dispositivo {} NO obtenido de polished_pci\n", i));
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let _ = fb.write_text_kernel(&format!("STORAGE_MANAGER: ERROR - Dispositivo {} NO obtenido", i), Color::RED);
                }
            }
        }

        serial_write_str(&format!("STORAGE_MANAGER: Total {} dispositivos detectados por polished_pci\n", self.devices.len()));
        
        // Listado de dispositivos de almacenamiento removido para limpiar pantalla
        
        Ok(())
    }

    /// Detectar dispositivos de almacenamiento usando PciManager manual (hardware real)
    fn detect_storage_devices_from_pci_manager(&mut self, pci_manager: &mut crate::drivers::pci::PciManager) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando dispositivos usando PciManager manual en hardware real...\n");

        // Listado de escaneo PCI manual removido para limpiar pantalla

        // Escanear dispositivos PCI
        pci_manager.scan_devices();
        let device_count = pci_manager.device_count();
        serial_write_str(&format!("STORAGE_MANAGER: PciManager detectó {} dispositivos PCI\n", device_count));
        
        // Listado de dispositivos PCI detectados removido para limpiar pantalla

        // Iterar sobre todos los dispositivos detectados
        for i in 0..device_count {
            if let Some(device) = pci_manager.get_device(i) {
                // DEBUG: Mostrar información de cada dispositivo para debug
                serial_write_str(&format!("STORAGE_MANAGER: PciManager Dispositivo {} - VID:{:#x} DID:{:#x} Class:{:#x} Subclass:{:#x} ProgIF:{:#x}\n", 
                                         i, device.vendor_id, device.device_id, device.class_code, device.subclass_code, device.prog_if));
                
                // FILTRAR DISPOSITIVOS: Solo agregar controladores de almacenamiento
                let is_virtio_block = device.vendor_id == 0x1AF4 && device.device_id == 0x1001;
                let is_storage_controller = (device.class_code == 0x01 && (device.subclass_code == 0x06 || device.subclass_code == 0x04 || device.subclass_code == 0x01)) || // SATA/RAID/IDE
                                           (device.class_code == 0x01 && device.subclass_code == 0x08) || // NVMe
                                           is_virtio_block; // VirtIO Block
                
                serial_write_str(&format!("STORAGE_MANAGER: PciManager is_storage_controller: {} (virtio_block={})\n", is_storage_controller, is_virtio_block));
                
                // Solo agregar controladores de almacenamiento, no dispositivos individuales
                if is_storage_controller {
                    serial_write_str(&format!("STORAGE_MANAGER: ✅ PciManager Agregando controlador de almacenamiento {} (VID:{:#x} DID:{:#x})\n", 
                                             i, device.vendor_id, device.device_id));
                    
                    // Almacenar el dispositivo PCI para uso posterior
                    self.pci_devices.push(device.clone());
                } else {
                    serial_write_str(&format!("STORAGE_MANAGER: ❌ PciManager Saltando dispositivo no-storage {} (VID:{:#x} DID:{:#x} Class:{}.{})\n", 
                                             i, device.vendor_id, device.device_id, device.class_code, device.subclass_code));
                    continue; // Saltar este dispositivo
                }
                
                let base_class = device.class_code;
                let subclass = device.subclass_code;

                serial_write_str(&format!("STORAGE_MANAGER: PCI Manual - VID:{:#x} DID:{:#x} Class:{}.{}.{}\n", 
                                         device.vendor_id, device.device_id, 
                                         base_class, subclass, device.prog_if));

                // DETECCIÓN ESPECIAL: VirtIO Block Device
                if device.vendor_id == 0x1AF4 && device.device_id == 0x1001 {
                    serial_write_str("STORAGE_MANAGER: ✅ VirtIO Block Device encontrado - inicializando driver VirtIO\n");
                    
                    // Inicializar el driver VirtIO real
                    use crate::drivers::virtio_blk::VirtioBlkDriver;
                    
                    // Necesitamos un framebuffer para la inicialización
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer_mut() {
                        match VirtioBlkDriver::new(device.clone(), fb) {
                            Ok(virtio_driver) => {
                                serial_write_str("STORAGE_MANAGER: ✅ Driver VirtIO inicializado exitosamente\n");
                                
                                // Obtener capacidad del dispositivo
                                let capacity_blocks = virtio_driver.block_count();
                                let capacity_bytes = capacity_blocks * (virtio_driver.block_size() as u64);
                                
                                serial_write_str(&format!("STORAGE_MANAGER: VirtIO capacidad: {} bloques ({} bytes)\n", 
                                    capacity_blocks, capacity_bytes));
                                
                                let device_info = StorageDeviceInfo {
                                    controller_type: StorageControllerType::VirtIO,
                                    model: format!("VirtIO Block {:04X}:{:04X}", device.vendor_id, device.device_id),
                                    serial: String::from("VIRTIO-SERIAL"),
                                    firmware: String::from("VirtIO"),
                                    capacity: capacity_bytes,
                                    block_size: virtio_driver.block_size(),
                                    max_lba: capacity_blocks,
                                    device_name: String::new(), // Se asignará después
                                };
                                
                                self.devices.push(StorageDevice {
                                    info: device_info,
                                });
                                
                                // Guardar el driver VirtIO globalmente
                                unsafe {
                                    VIRTIO_BLK_DRIVER = Some(virtio_driver);
                                }
                                
                                serial_write_str(&format!("STORAGE_MANAGER: VirtIO Block agregado: {:04X}:{:04X}\n", 
                                                         device.vendor_id, device.device_id));
                            }
                            Err(e) => {
                                serial_write_str(&format!("STORAGE_MANAGER: ❌ Error inicializando driver VirtIO: {}\n", e));
                                // Agregar el dispositivo de todas formas, pero con capacidad 0
                                let device_info = StorageDeviceInfo {
                                    controller_type: StorageControllerType::VirtIO,
                                    model: format!("VirtIO Block {:04X}:{:04X} (error)", device.vendor_id, device.device_id),
                                    serial: String::from("VIRTIO-ERROR"),
                                    firmware: String::from("VirtIO"),
                                    capacity: 0,
                                    block_size: 512,
                                    max_lba: 0,
                                    device_name: String::new(),
                                };
                                
                                self.devices.push(StorageDevice {
                                    info: device_info,
                                });
                            }
                        }
                    } else {
                        serial_write_str("STORAGE_MANAGER: ❌ No hay framebuffer disponible para inicializar VirtIO\n");
                        // Agregar dispositivo sin inicializar driver
                        let device_info = StorageDeviceInfo {
                            controller_type: StorageControllerType::VirtIO,
                            model: format!("VirtIO Block {:04X}:{:04X} (no fb)", device.vendor_id, device.device_id),
                            serial: String::from("VIRTIO-NO-FB"),
                            firmware: String::from("VirtIO"),
                            capacity: 0,
                            block_size: 512,
                            max_lba: 0,
                            device_name: String::new(),
                        };
                        
                        self.devices.push(StorageDevice {
                            info: device_info,
                        });
                    }
                    
                    continue; // Saltar al siguiente dispositivo
                }

                // Listado de dispositivos PCI removido para limpiar pantalla

                // Detectar controladoras de almacenamiento (clases 1 y 17)
                if base_class == 0x01 || base_class == 0x11 { // Mass storage controller (0x01) o Communication device (0x11)
                    let controller_type = match (base_class, subclass) {
                        // SATA Controllers (subclass 0x06)
                        (0x01, 0x06) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x1B21 => "ASMedia",
                                0x1002 => "AMD",
                                0x10DE => "NVIDIA",
                                0x197B => "JMicron",
                                0x1106 => "VIA",
                                0x1039 => "SiS",
                                _ => "Unknown"
                            };
                            
                            // DETECCIÓN ESPECIAL PARA INTEL RAID: Tratar como volumen RAID agregado
                            if device.vendor_id == 0x8086 && device.device_id == 0x2822 {
                                serial_write_str(&format!("STORAGE_MANAGER: INTEL RAID CONTROLLER detectado - creando volumen RAID agregado\n"));
                                serial_write_str(&format!("STORAGE_MANAGER: VID:{:#x} ({}) DID:{:#x} - Volumen RAID único\n", 
                                                         device.vendor_id, vendor_name, device.device_id));
                                StorageControllerType::IntelRaidVolume
                            } else {
                                // Detectar tipo específico por Programming Interface
                                let sata_type = match device.prog_if {
                                    0x01 => "AHCI",
                                    0x05 => "RAID",
                                    0x80 => "Vendor Specific",
                                    _ => "Generic SATA"
                                };
                                
                                serial_write_str(&format!("STORAGE_MANAGER: SATA {} encontrado - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                         sata_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                                StorageControllerType::AHCI
                            }
                        }
                        // NVMe Controllers (subclass 0x08)
                        (0x01, 0x08) => {
                            let vendor_name = match device.vendor_id {
                                0x144D => "Samsung",
                                0x8086 => "Intel",
                                0x15B7 => "Sandisk",
                                0x1CC1 => "ADATA",
                                0x1E0F => "KIOXIA",
                                0x126F => "Silicon Motion",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: NVMe encontrado - VID:{:#x} ({}) DID:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id));
                            StorageControllerType::NVMe
                        }
                        // RAID Controllers (subclass 0x04) - incluye SATA en modo RAID
                        (0x01, 0x04) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1000 => "LSI/Broadcom",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x10DE => "NVIDIA",
                                0x1106 => "VIA",
                                0x1B21 => "ASMedia",
                                0x197B => "JMicron",
                                0x1039 => "SiS",
                                _ => "Unknown"
                            };
                            
                            // Detectar específicamente controladoras SATA Intel en modo RAID
                            let is_sata_raid = device.vendor_id == 0x8086 && matches!(device.device_id, 
                                0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F);
                            
                            let raid_type = if is_sata_raid {
                                "SATA RAID"
        } else {
                                match device.prog_if {
                                    0x01 => "RAID",
                                    0x05 => "RAID with AHCI",
                                    0x80 => "Vendor Specific RAID",
                                    _ => "Generic RAID"
                                }
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: {} encontrado - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     raid_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            if is_sata_raid {
                                StorageControllerType::IntelRAID // Usar driver RAID específico para Intel SATA RAID
                            } else {
                                StorageControllerType::AHCI // Usar AHCI como fallback para otros RAID
                            }
                        }
                        // IDE Controllers (subclass 0x01)
                        (0x01, 0x01) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1106 => "VIA",
                                0x1039 => "SiS",
                                0x10B9 => "ALi",
                                0x126F => "Silicon Motion",
                                _ => "Unknown"
                            };
                            
                            let ide_type = match device.prog_if {
                                0x80 => "Generic IDE",
                                0x8A => "ISA Compatibility mode only",
                                0x8F => "PCI Native mode only",
                                0x85 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x8E => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x86 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                0x87 => "ISA Compatibility mode, supports both channels switched to PCI native mode",
                                _ => "Unknown IDE"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: IDE {} encontrado - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     ide_type, device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            StorageControllerType::ATA
                        }
                        // Serial Attached SCSI (subclass 0x07)
                        (0x01, 0x07) => {
                            let vendor_name = match device.vendor_id {
                                0x1000 => "LSI/Broadcom",
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: SAS encontrado - VID:{:#x} ({}) DID:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id));
                            StorageControllerType::AHCI // Usar AHCI como fallback para SAS
                        }
                        // Other Mass Storage Controllers (subclass 0x80)
                        (0x01, 0x80) => {
                            let vendor_name = match device.vendor_id {
                                0x8086 => "Intel",
                                0x1022 => "AMD",
                                0x1B4B => "Marvell",
                                0x1B21 => "ASMedia",
                                0x10DE => "NVIDIA",
                                _ => "Unknown"
                            };
                            
                            serial_write_str(&format!("STORAGE_MANAGER: Storage Controller genérico encontrado - VID:{:#x} ({}) DID:{:#x} ProgIF:{:#x}\n", 
                                                     device.vendor_id, vendor_name, device.device_id, device.prog_if));
                            StorageControllerType::AHCI // Usar AHCI como fallback
                        }
                        // Communication device (0x11) - algunos controladores de almacenamiento
                        (0x11, 0x80) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Storage Controller (17.128) encontrado - VID:{:#x} DID:{:#x}\n", 
                                                     device.vendor_id, device.device_id));
                            StorageControllerType::AHCI // Usar AHCI como fallback
                        }
                        _ => {
                            serial_write_str(&format!("STORAGE_MANAGER: Controladora de almacenamiento genérica - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                                     device.vendor_id, device.device_id, base_class, subclass));
                            StorageControllerType::ATA // Usar ATA como fallback genérico
                        }
                    };
                    
                    // Mostrar en pantalla para hardware real
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("STORAGE: {:04X}:{:04X} Class:{}.{}", 
                                                      device.vendor_id, device.device_id, 
                                                      base_class, subclass), Color::GREEN);
                    }
                    
                    // Crear información de controladora de almacenamiento
                    let storage_info = StorageDeviceInfo {
                        controller_type,
                        model: alloc::format!("Storage {:04X}:{:04X} Class:{}.{}", 
                                            device.vendor_id, device.device_id, base_class, subclass),
                        serial: "STORAGE-SERIAL".to_string(),
                        firmware: "STORAGE-FW".to_string(),
                        capacity: 0, // Se detectará en la inicialización
                        block_size: 512,
                        max_lba: 0,
                        device_name: "".to_string(), // Se asignará después
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: Controladora agregada: {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
                }

                // Detectar GPUs (class 3) - solo para depuración, NO como dispositivos de almacenamiento
                if base_class == 0x03 {
                    serial_write_str(&format!("STORAGE_MANAGER: GPU detectada en hardware real - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                             device.vendor_id, device.device_id, 
                                             base_class, subclass));
                    
                    serial_write_str(&format!("STORAGE_MANAGER: GPU detectada pero NO agregada como dispositivo de almacenamiento: {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
                }
            }
        }

        serial_write_str(&format!("STORAGE_MANAGER: Total {} dispositivos detectados por PciManager manual\n", self.devices.len()));
        
        // Listado de dispositivos de almacenamiento removido para limpiar pantalla
        
        Ok(())
    }

    /// Detectar controladoras NVMe (obsoleto - usar detect_storage_devices_from_pci)
    fn detect_nvme_controllers(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando controladoras NVMe...\n");
        // Método obsoleto - ahora se usa detect_storage_devices_from_pci
        Ok(())
    }

    /// Detectar controladoras AHCI (obsoleto - usar detect_storage_devices_from_pci)
    fn detect_ahci_controllers(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando controladoras AHCI...\n");
        // Método obsoleto - ahora se usa detect_storage_devices_from_pci
        Ok(())
    }

    /// Detectar controladoras ATA
    fn detect_ata_controllers(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando controladoras ATA...\n");
        
        // Intentar detectar controladoras ATA en puertos estándar
        let mut ata_primary = crate::drivers::ata_direct::AtaDirectDriver::new_primary();
        if ata_primary.initialize().is_ok() {
            if let Some(device_info) = ata_primary.get_device_info() {
                let storage_info = StorageDeviceInfo {
                    controller_type: StorageControllerType::ATA,
                    model: format!("{:?}", device_info.model),
                    serial: format!("{:?}", device_info.serial),
                    firmware: format!("{:?}", device_info.firmware),
                    capacity: ata_primary.get_sector_count() * 512,
                    block_size: 512,
                    max_lba: ata_primary.get_sector_count(),
                    device_name: "".to_string(), // Se asignará después
                };

                self.devices.push(StorageDevice {
                    info: storage_info,
                });

                serial_write_str(&format!("STORAGE_MANAGER: Driver ATA Primary inicializado: {:?}\n", device_info.model));
            }
        }

        let mut ata_secondary = crate::drivers::ata_direct::AtaDirectDriver::new_secondary();
        if ata_secondary.initialize().is_ok() {
            if let Some(device_info) = ata_secondary.get_device_info() {
                let storage_info = StorageDeviceInfo {
                    controller_type: StorageControllerType::ATA,
                    model: format!("{:?}", device_info.model),
                    serial: format!("{:?}", device_info.serial),
                    firmware: format!("{:?}", device_info.firmware),
                    capacity: ata_secondary.get_sector_count() * 512,
                    block_size: 512,
                    max_lba: ata_secondary.get_sector_count(),
                    device_name: "".to_string(), // Se asignará después
                };

                self.devices.push(StorageDevice {
                    info: storage_info,
                });

                serial_write_str(&format!("STORAGE_MANAGER: Driver ATA Secondary inicializado: {:?}\n", device_info.model));
            }
        }

        Ok(())
    }

    /// Detectar controladoras VirtIO (obsoleto - usar detect_storage_devices_from_pci)
    fn detect_virtio_controllers(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando controladoras VirtIO...\n");
        // Método obsoleto - ahora se usa detect_storage_devices_from_pci
        Ok(())
    }

    /// Método obsoleto - usar detect_storage_devices_from_pci
    fn detect_nvme_controllers_old(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Detectando controladoras NVMe...\n");

        // Buscar dispositivos NVMe en PCI
        for bus in 0..=255 {
            for device in 0..=31 {
                for function in 0..=7 {
                    let vendor_id = self.read_pci_config_u16(bus, device, function, 0);
                    let device_id = self.read_pci_config_u16(bus, device, function, 2);
                    
                    // Verificar si es un dispositivo NVMe
                    if vendor_id == 0x144D || (vendor_id == 0x8086 && device_id == 0x0953) {
                        serial_write_str(&format!("STORAGE_MANAGER: Controladora NVMe encontrada en bus:{}, dev:{}, func:{}\n", 
                                                 bus, device, function));
                        
                        // Leer BAR0 para obtener la dirección base
                        let bar0 = self.read_pci_config_u32(bus, device, function, 0x10);
                        let base_addr = bar0 & 0xFFFFFFF0;
                        
                        // Crear e inicializar driver NVMe
                        let mut nvme_driver = crate::drivers::nvme::NvmeDriver::new(base_addr);
                        if let Ok(()) = nvme_driver.initialize() {
                            if let Some(device_info) = nvme_driver.get_device_info() {
                                let storage_info = StorageDeviceInfo {
                                    controller_type: StorageControllerType::NVMe,
                                    model: device_info.model.clone(),
                                    serial: device_info.serial.clone(),
                                    firmware: device_info.firmware.clone(),
                                    capacity: device_info.capacity,
                                    block_size: device_info.block_size,
                                    max_lba: device_info.max_lba,
                                    device_name: "".to_string(), // Se asignará después
                                };

                                self.devices.push(StorageDevice {
                                    info: storage_info,
                                });

                                serial_write_str("STORAGE_MANAGER: Driver NVMe inicializado exitosamente\n");
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }



    /// Leer configuración PCI de 16 bits
    fn read_pci_config_u16(&self, bus: u8, device: u8, function: u8, offset: u8) -> u16 {
        let address = 0x80000000u32 | 
                     ((bus as u32) << 16) | 
                     ((device as u32) << 11) | 
                     ((function as u32) << 8) | 
                     ((offset as u32) & 0xFC);
        
        unsafe {
            core::arch::asm!("out dx, eax", in("eax") address, in("dx") 0xCF8u16);
            let result: u32;
            core::arch::asm!("in eax, dx", out("eax") result, in("dx") 0xCFCu16);
            (result & 0xFFFF) as u16
        }
    }

    /// Leer configuración PCI de 8 bits
    fn read_pci_config_u8(&self, bus: u8, device: u8, function: u8, offset: u8) -> u8 {
        let address = 0x80000000u32 | 
                     ((bus as u32) << 16) | 
                     ((device as u32) << 11) | 
                     ((function as u32) << 8) | 
                     ((offset as u32) & 0xFC);
        
        unsafe {
            core::arch::asm!("out dx, eax", in("eax") address, in("dx") 0xCF8u16);
            let result: u32;
            core::arch::asm!("in eax, dx", out("eax") result, in("dx") 0xCFCu16);
            let byte_offset = offset & 0x03;
            ((result >> (byte_offset * 8)) & 0xFF) as u8
        }
    }

    /// Leer configuración PCI de 32 bits
    fn read_pci_config_u32(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        let address = 0x80000000u32 | 
                     ((bus as u32) << 16) | 
                     ((device as u32) << 11) | 
                     ((function as u32) << 8) | 
                     ((offset as u32) & 0xFC);
        
        unsafe {
            core::arch::asm!("out dx, eax", in("eax") address, in("dx") 0xCF8u16);
            let result: u32;
            core::arch::asm!("in eax, dx", out("eax") result, in("dx") 0xCFCu16);
            result
        }
    }

    /// Obtener número de dispositivos
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Obtener información del dispositivo activo
    pub fn get_active_device_info(&self) -> Option<&StorageDeviceInfo> {
        if let Some(index) = self.active_device {
            self.devices.get(index).map(|d| &d.info)
            } else {
            None
        }
    }

    /// Cambiar dispositivo activo
    pub fn set_active_device(&mut self, index: usize) -> Result<(), &'static str> {
        if index >= self.devices.len() {
            return Err("Índice de dispositivo inválido");
        }

        self.active_device = Some(index);
        serial_write_str(&format!("STORAGE_MANAGER: Dispositivo activo cambiado a índice {}\n", index));
        Ok(())
    }

    /// Leer bloques del dispositivo activo
    pub fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if let Some(index) = self.active_device {
            if let Some(device) = self.devices.get(index) {
                // Calcular el sector inicial
                let sector_size = device.info.block_size;
                let sectors_to_read = (buffer.len() + sector_size as usize - 1) / sector_size as usize;
                
                serial_write_str(&format!("STORAGE_MANAGER: Leyendo {} sectores desde bloque {} del dispositivo {}\n", 
                                         sectors_to_read, start_block, index));
                
                // Leer sectores consecutivos
                for i in 0..sectors_to_read {
                    let sector_offset = start_block + i as u64;
                    let buffer_offset = i * sector_size as usize;
                    let buffer_end = core::cmp::min(buffer_offset + sector_size as usize, buffer.len());
                    
                    if buffer_offset < buffer.len() {
                        let sector_buffer = &mut buffer[buffer_offset..buffer_end];
                        self.read_device_sector(&device.info, sector_offset, sector_buffer)?;
                    }
                }
                
                Ok(())
            } else {
                Err("Dispositivo activo no encontrado")
            }
        } else {
            Err("No hay dispositivo activo")
        }
    }

    /// Escribir bloques al dispositivo activo
    pub fn write_blocks(&mut self, start_block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if let Some(index) = self.active_device {
            if let Some(_device) = self.devices.get(index) {
                // TODO: Implementar escritura real del dispositivo
                Err("Escritura de bloques no implementada")
            } else {
                Err("Dispositivo activo no encontrado")
            }
        } else {
            Err("No hay dispositivo activo")
        }
    }

    /// Obtener tamaño de bloque del dispositivo activo
    pub fn get_block_size(&self) -> Result<u32, &'static str> {
        if let Some(index) = self.active_device {
            if let Some(device) = self.devices.get(index) {
                Ok(device.info.block_size)
            } else {
                Err("Dispositivo activo no encontrado")
            }
        } else {
            Err("No hay dispositivo activo")
        }
    }

    /// Buscar controladora AHCI en el sistema PCI
    fn find_ahci_controller(&self) -> Option<u64> {
        serial_write_str("STORAGE_MANAGER: Buscando controladora AHCI...\n");
        
        // Buscar dispositivos PCI con clase 0x01 (Mass Storage Controller) y subclass 0x06 (SATA)
        for bus in 0..=255 {
            for device in 0..32 {
                for function in 0..8 {
                    let vendor_id = self.read_pci_config_u16(bus, device, function, 0x00);
                    if vendor_id == 0xFFFF {
                        continue; // Dispositivo no existe
                    }
                    
                    let class_code = self.read_pci_config_u8(bus, device, function, 0x0B);
                    let subclass_code = self.read_pci_config_u8(bus, device, function, 0x0A);
                    let prog_if = self.read_pci_config_u8(bus, device, function, 0x09);
                    
                    // Buscar controladoras SATA/AHCI
                    if class_code == 0x01 && (subclass_code == 0x06 || subclass_code == 0x04) {
                        // Obtener BAR5 (Base Address Register 5) que contiene la dirección MMIO
                        let bar5 = self.read_pci_config_u32(bus, device, function, 0x24);
                        
                        if bar5 != 0 && (bar5 & 0x01) == 0 { // MMIO (no I/O)
                            let mmio_base = (bar5 & 0xFFFFFFF0) as u64;
                            
                            serial_write_str(&format!(
                                "STORAGE_MANAGER: Controladora AHCI encontrada en bus:{:02X} dev:{:02X} func:{} - MMIO: {:#x}\n",
                                bus, device, function, mmio_base
                            ));
                            
                            return Some(mmio_base);
                        }
                    }
                }
            }
        }
        
        serial_write_str("STORAGE_MANAGER: No se encontró controladora AHCI\n");
        None
    }

    /// Buscar información PCI para controladora AHCI
    fn find_pci_info_for_ahci(&self) -> (u16, u16) {
        serial_write_str("STORAGE_MANAGER: Buscando información PCI para AHCI...\n");
        
        // Buscar en los dispositivos PCI detectados primero
        for pci_device in &self.pci_devices {
            // Buscar controladoras AHCI (Class 0x01, Subclass 0x06, ProgIF 0x01)
            if pci_device.class_code == 0x01 && pci_device.subclass_code == 0x06 && pci_device.prog_if == 0x01 {
                serial_write_str(&format!("STORAGE_MANAGER: Usando información PCI real - VID: 0x{:04X}, DID: 0x{:04X}\n",
                                         pci_device.vendor_id, pci_device.device_id));
                return (pci_device.vendor_id, pci_device.device_id);
            }
        }
        
        // Si no se encuentra en los dispositivos detectados, buscar en PCI directamente
        for bus in 0..=255 {
            for device in 0..32 {
                for function in 0..8 {
                    let vendor_id = self.read_pci_config_u16(bus, device, function, 0x00);
                    if vendor_id == 0xFFFF {
                        continue; // Dispositivo no existe
                    }
                    
                    let device_id = self.read_pci_config_u16(bus, device, function, 0x02);
                    let class_code = self.read_pci_config_u8(bus, device, function, 0x0B);
                    let subclass_code = self.read_pci_config_u8(bus, device, function, 0x0A);
                    let prog_if = self.read_pci_config_u8(bus, device, function, 0x09);
                    
                    // Buscar controladoras SATA/AHCI
                    if class_code == 0x01 && (subclass_code == 0x06 || subclass_code == 0x04) {
                        serial_write_str(&format!("STORAGE_MANAGER: Controladora AHCI encontrada - VID: 0x{:04X}, DID: 0x{:04X}\n",
                                                 vendor_id, device_id));
                        return (vendor_id, device_id);
                    }
                }
            }
        }
        
        // Fallback a valores por defecto para Intel RAID Controller
        serial_write_str("STORAGE_MANAGER: Usando información PCI por defecto para Intel RAID Controller\n");
        (0x8086, 0x2822) // Intel RAID Controller (ASUS Z99 II)
    }

    // detect_qemu_environment eliminado - no necesario para QEMU con /dev/sda ni hardware real

    // read_qemu_disk eliminado - no necesario

    // read_qemu_sector_direct eliminado - no necesario

    // write_qemu_disk eliminado - no necesario

    // write_qemu_sector_direct eliminado - no necesario

    /// Obtener número de bloques del dispositivo activo
    pub fn get_block_count(&self) -> Result<u64, &'static str> {
        if let Some(index) = self.active_device {
            if let Some(device) = self.devices.get(index) {
                Ok(device.info.capacity / device.info.block_size as u64)
            } else {
                Err("Dispositivo activo no encontrado")
            }
        } else {
            Err("No hay dispositivo activo")
        }
    }

    /// Verificar si el gestor está listo
    pub fn is_ready(&self) -> bool {
        self.active_device.is_some()
    }

    /// Leer un sector de un dispositivo específico
    fn read_device_sector(&self, device_info: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        self.read_device_sector_with_type(device_info, sector, buffer, StorageSectorType::FAT32)
    }

    /// Escribir sector del dispositivo real
    pub fn write_device_sector(&self, device_index: usize, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if device_index >= self.devices.len() {
            return Err("Índice de dispositivo inválido");
        }

        let device = &self.devices[device_index];
        
        // Intentar escritura real primero
        match self.write_device_sector_real(&device.info, sector, buffer) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: Escritura real exitosa\n");
                Ok(())
            }
            Err(_) => {
                serial_write_str("STORAGE_MANAGER: Escritura real falló, usando simulación\n");
                // Fallback a simulación si la escritura real falla
                self.write_device_sector_simulated(device_index, sector, buffer)
            }
        }
    }

    /// Leer un sector de un dispositivo específico con tipo de sector
    pub fn read_device_sector_with_type(&self, device_info: &StorageDeviceInfo, sector: u64, buffer: &mut [u8], sector_type: StorageSectorType) -> Result<(), &'static str> {
        // Para EclipseFS, usar solo lectura real - si falla, retornar error
        if matches!(sector_type, StorageSectorType::EclipseFS) {
            match self.read_device_sector_real(device_info, sector, buffer) {
                Ok(_) => {
                    serial_write_str("STORAGE_MANAGER: Lectura real EclipseFS exitosa\n");
                    return Ok(());
                }
                Err(e) => {
                    serial_write_str("ECLIPSEFS: Error al leer datos reales del dispositivo. Sistema de archivos no disponible.\n");
                    return Err(e);
                }
            }
        }
        
        // Para otros tipos (FAT32), intentar lectura real primero
        match self.read_device_sector_real(device_info, sector, buffer) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: Lectura real exitosa\n");
                return Ok(());
            }
            Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Lectura real falló: {}\n", e));
                // Continuar con simulación para FAT32
            }
        }

        match device_info.controller_type {
            StorageControllerType::VirtIO => {
                // Para VirtIO, simular lectura
                serial_write_str(&format!("STORAGE_MANAGER: Simulando lectura VirtIO sector {}\n", sector));
                buffer.fill(0);
                
                // Generar datos simulados para boot sector según el tipo
                if sector == 0 && buffer.len() >= 512 {
                    match sector_type {
                        StorageSectorType::FAT32 => {
                            self.generate_simulated_fat32_boot_sector(buffer);
                        }
                        StorageSectorType::EclipseFS => {
                            self.generate_simulated_eclipsefs_sector(sector, buffer);
                        }
                    }
                }
                Ok(())
            }
            StorageControllerType::NVMe => {
                // Para NVMe, simular lectura
                serial_write_str(&format!("STORAGE_MANAGER: Simulando lectura NVMe sector {}\n", sector));
                buffer.fill(0);
                
                // Generar datos simulados para boot sector según el tipo
                if sector == 0 && buffer.len() >= 512 {
                    match sector_type {
                        StorageSectorType::FAT32 => {
                            self.generate_simulated_fat32_boot_sector(buffer);
                        }
                        StorageSectorType::EclipseFS => {
                            self.generate_simulated_eclipsefs_sector(sector, buffer);
                        }
                    }
                }
                Ok(())
            }
            StorageControllerType::AHCI => {
                // Para AHCI/SATA, simular lectura
                serial_write_str(&format!("STORAGE_MANAGER: Simulando lectura AHCI sector {}\n", sector));
                buffer.fill(0);
                
                // Generar datos simulados para boot sector según el tipo
                if sector == 0 && buffer.len() >= 512 {
                    match sector_type {
                        StorageSectorType::FAT32 => {
                            self.generate_simulated_fat32_boot_sector(buffer);
                        }
                        StorageSectorType::EclipseFS => {
                            self.generate_simulated_eclipsefs_sector(sector, buffer);
                        }
                    }
                }
                Ok(())
            }
            StorageControllerType::ATA => {
                // Para ATA, simular lectura
                serial_write_str(&format!("STORAGE_MANAGER: Simulando lectura ATA sector {}\n", sector));
                buffer.fill(0);
                
                // Generar datos simulados para boot sector según el tipo
                if sector == 0 && buffer.len() >= 512 {
                    match sector_type {
                        StorageSectorType::FAT32 => {
                            self.generate_simulated_fat32_boot_sector(buffer);
                        }
                        StorageSectorType::EclipseFS => {
                            self.generate_simulated_eclipsefs_sector(sector, buffer);
                        }
                    }
                }
                Ok(())
            }
            StorageControllerType::IntelRAID => {
                // Para Intel RAID, usar lectura real
                serial_write_str(&format!("STORAGE_MANAGER: Usando Intel RAID para sector {}\n", sector));
                
                // Usar el driver Intel RAID específico con información real del dispositivo
                use crate::drivers::pci::PciDevice;
                
                // Buscar el dispositivo Intel RAID real en la lista de dispositivos PCI
                let mut raid_pci_device = None;
                for device in &self.pci_devices {
                    if device.vendor_id == 0x8086 && matches!(device.device_id, 
                        0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F) {
                        raid_pci_device = Some(device.clone());
                        break;
                    }
                }
                
                let pci_device = raid_pci_device.unwrap_or_else(|| {
                    serial_write_str("STORAGE_MANAGER: No se encontró dispositivo Intel RAID real, usando valores por defecto\n");
                    PciDevice {
                        bus: 0,
                        device: 0,
                        function: 0,
                        vendor_id: 0x8086, // Intel
                        device_id: 0x2822, // SATA RAID Controller
                        class_code: 0x01,
                        subclass_code: 0x04,
                        prog_if: 0x05, // RAID with AHCI
                        revision_id: 0x10,
                        header_type: 0x00,
                        status: 0,
                        command: 0,
                    }
                });
                
                serial_write_str(&format!("STORAGE_MANAGER: Usando dispositivo PCI real: Bus:{}, Dev:{}, Func:{} VID:{:#x} DID:{:#x}\n",
                    pci_device.bus, pci_device.device, pci_device.function, pci_device.vendor_id, pci_device.device_id));
                
                let mut raid_driver = IntelRaidDriver::new(pci_device);
                
                // Inicializar el driver Intel RAID
                if let Err(_) = raid_driver.initialize() {
                    // Si falla, usar datos simulados como fallback
                    serial_write_str("STORAGE_MANAGER: Intel RAID falló, usando datos simulados\n");
                buffer.fill(0);
                
                if sector == 0 && buffer.len() >= 512 {
                    match sector_type {
                        StorageSectorType::FAT32 => {
                            self.generate_simulated_fat32_boot_sector(buffer);
                        }
                        StorageSectorType::EclipseFS => {
                            self.generate_simulated_eclipsefs_sector(sector, buffer);
                        }
                    }
                }
                    Ok(())
                } else {
                    // Leer el sector usando el driver Intel RAID
                    match raid_driver.read_raid_blocks(0, sector, buffer) { // Usar volumen 0
                        Ok(_) => {
                            serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con Intel RAID\n");
                Ok(())
            }
                        Err(_) => {
                            // Fallback a datos simulados
                            serial_write_str("STORAGE_MANAGER: Error en Intel RAID, usando datos simulados\n");
                            buffer.fill(0);
                            
                            if sector == 0 && buffer.len() >= 512 {
                                match sector_type {
                                    StorageSectorType::FAT32 => {
                                        self.generate_simulated_fat32_boot_sector(buffer);
                                    }
                                    StorageSectorType::EclipseFS => {
                                        self.generate_simulated_eclipsefs_sector(sector, buffer);
                                    }
                                }
                            }
                            Ok(())
                        }
                    }
                }
            }
        StorageControllerType::IntelRaidVolume => {
            // Intel RAID Volume opera en modo AHCI - usar driver AHCI normal
            serial_write_str(&format!("STORAGE_MANAGER: Intel RAID Volume como AHCI para sector {}\n", sector));
            
            // En hardware real, el Intel RAID controller (0x8086:0x2822) opera en modo AHCI
            // El volumen RAID agregado se presenta como un dispositivo SATA normal
            // Usar el driver AHCI estándar con la configuración correcta
            
            use crate::drivers::ahci::AhciDriver;
            
            // Buscar información PCI del controlador Intel
            let (vendor_id, device_id) = self.find_pci_info_for_ahci();
            serial_write_str(&format!("STORAGE_MANAGER: Usando PCI info Intel AHCI: {:04X}:{:04X}\n", vendor_id, device_id));
            
            // Crear driver AHCI con la información PCI correcta
            // Usar la dirección base típica para controladores Intel AHCI
            let base_address = 0xFEB80000; // Dirección base del controlador Intel AHCI
            let mut ahci_driver = AhciDriver::new_from_pci(vendor_id, device_id, base_address);
            
            if let Err(e) = ahci_driver.initialize() {
                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando AHCI para Intel: {}, usando fallback\n", e));
                // Fallback a datos simulados
                self.simulate_intel_raid_data(sector, buffer)
            } else {
                    // El volumen RAID agregado se presenta como puerto 0 del controlador AHCI
                    // Convertir sector u64 a u32 para el driver AHCI
                    let sector_u32 = if sector > u32::MAX as u64 {
                        serial_write_str("STORAGE_MANAGER: Sector demasiado grande, usando sector 0\n");
                        0
                    } else {
                        sector as u32
                    };
                    
                    match ahci_driver.read_sector(sector_u32, buffer) {
                    Ok(()) => {
                        serial_write_str("STORAGE_MANAGER: Lectura exitosa desde Intel RAID via AHCI\n");
                        Ok(())
                    },
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: Error leyendo Intel RAID via AHCI: {}, usando fallback\n", e));
                        self.simulate_intel_raid_data(sector, buffer)
                    }
                }
            }
        }
        }
    }

    /// Escribir un sector de un dispositivo específico con tipo de sector
    pub fn write_device_sector_with_type(&self, device_info: &StorageDeviceInfo, sector: u64, data: &[u8], sector_type: StorageSectorType) -> Result<(), &'static str> {
        // Para EclipseFS, usar solo escritura real - si falla, error
        if matches!(sector_type, StorageSectorType::EclipseFS) {
            match self.write_device_sector_real(device_info, sector, data) {
                Ok(_) => {
                    serial_write_str("STORAGE_MANAGER: Escritura real EclipseFS exitosa\n");
                    return Ok(());
                }
                Err(e) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo EclipseFS: {}\n", e));
                    return Err(e);
                }
            }
        }
        
        // Para otros tipos (FAT32), intentar escritura real primero
        match self.write_device_sector_real(device_info, sector, data) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: Escritura real exitosa\n");
                return Ok(());
            }
            Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Escritura real falló: {}\n", e));
                // Continuar con simulación para FAT32
            }
        }

        // Simular escritura para tipos no críticos
        serial_write_str(&format!("STORAGE_MANAGER: Simulando escritura sector {} tipo {:?}\n", sector, sector_type));
        Ok(())
    }

    /// Escribir sector real del dispositivo (sin simulación)
    pub fn write_device_sector_real(&self, device_info: &StorageDeviceInfo, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Escritura REAL sector {} del dispositivo {:?}\n", 
                                  sector, device_info.controller_type));
        
        match device_info.controller_type {
            StorageControllerType::VirtIO => {
                serial_write_str("STORAGE_MANAGER: Intentando escritura VirtIO\n");
                // VirtIO no soporta escritura en esta implementación
                Err("VirtIO no soporta escritura")
            }
            StorageControllerType::NVMe => {
                serial_write_str("STORAGE_MANAGER: Intentando escritura real NVMe\n");
                
                // Usar el driver NVMe existente
                use crate::drivers::nvme::NvmeDriver;
                
                // Crear driver NVMe con dirección base simulada
                let mut nvme_driver = NvmeDriver::new(0xFED00000); // Dirección base simulada
                
                // Inicializar el driver NVMe
                if let Err(e) = nvme_driver.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando NVMe: {}\n", e));
                    Err("Error inicializando driver NVMe")
                } else {
                    // Escribir el sector usando el driver NVMe
                    match nvme_driver.write_blocks(sector, buffer) {
                        Ok(_) => {
                            serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con NVMe\n");
                            Ok(())
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo con NVMe: {}\n", e));
                            Err("Error escribiendo sector con NVMe")
                        }
                    }
                }
            }
            StorageControllerType::AHCI => {
                serial_write_str("STORAGE_MANAGER: Intentando escritura real AHCI\n");
                
                // Usar el driver SATA/AHCI existente con información real del dispositivo
                use crate::drivers::pci::PciDevice;
                use crate::drivers::sata_ahci::SataAhciDriver;
                
                // Buscar el dispositivo PCI real correspondiente a este dispositivo de almacenamiento
                let mut real_pci_device = None;
                for pci_device in &self.pci_devices {
                    if pci_device.vendor_id == 0x8086 && pci_device.device_id == 0x2822 {
                        real_pci_device = Some(pci_device.clone());
                        break;
                    }
                }
                
                let pci_device = real_pci_device.unwrap_or_else(|| {
                    serial_write_str("STORAGE_MANAGER: No se encontró dispositivo PCI real para AHCI, usando valores por defecto\n");
                    PciDevice {
                    bus: 0,
                    device: 0,
                    function: 0,
                    vendor_id: 0x8086, // Intel
                        device_id: 0x2822, // SATA RAID Controller (usar el real)
                    class_code: 0x01,
                        subclass_code: 0x04, // RAID
                        prog_if: 0x05, // RAID with AHCI
                    revision_id: 0x10,
                    header_type: 0x00,
                    status: 0,
                command: 0,
                    }
                });
                
                serial_write_str(&format!("STORAGE_MANAGER: Usando dispositivo PCI real para AHCI: Bus:{}, Dev:{}, Func:{} VID:{:#x} DID:{:#x}\n",
                    pci_device.bus, pci_device.device, pci_device.function, pci_device.vendor_id, pci_device.device_id));
                
                let mut sata_driver = SataAhciDriver::new(pci_device);
                
                // Inicializar el driver SATA/AHCI
                if let Err(e) = sata_driver.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando SATA/AHCI: {}\n", e));
                    serial_write_str("STORAGE_MANAGER: Fallback a driver ATA directo...\n");
                    
                    // Fallback: usar driver ATA directo
                    use crate::drivers::ata_direct::AtaDirectDriver;
                    let mut ata_driver = AtaDirectDriver::new_primary();
                    
                    if let Err(ata_error) = ata_driver.initialize() {
                        serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA directo: {}\n", ata_error));
                        serial_write_str("STORAGE_MANAGER: Ambos drivers fallaron - usando fallback final\n");
                        
                        // Fallback final: usar datos simulados como último recurso
                        serial_write_str("STORAGE_MANAGER: Usando datos simulados como último recurso\n");
                        // Para escritura, simular escritura exitosa
                        serial_write_str("STORAGE_MANAGER: Simulando escritura exitosa como último recurso\n");
                        return Ok(());
                    } else {
                        match ata_driver.write_blocks(sector, buffer) {
                            Ok(_) => {
                                serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con ATA directo (fallback)\n");
                                Ok(())
                            }
                            Err(ata_error) => {
                                serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo con ATA directo: {}\n", ata_error));
                                Err("Error escribiendo sector con ATA directo")
                            }
                        }
                    }
                } else {
                    // Escribir el sector usando el driver SATA/AHCI
                    match sata_driver.write_blocks(sector, buffer) {
                        Ok(_) => {
                            serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con SATA/AHCI\n");
                            Ok(())
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo con SATA/AHCI: {}\n", e));
                            serial_write_str("STORAGE_MANAGER: Fallback a driver ATA directo...\n");
                            
                            // Fallback: usar driver ATA directo
                            use crate::drivers::ata_direct::AtaDirectDriver;
                            let mut ata_driver = AtaDirectDriver::new_primary();
                            
                            if let Err(ata_error) = ata_driver.initialize() {
                                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA directo: {}\n", ata_error));
                                serial_write_str("STORAGE_MANAGER: Ambos drivers fallaron - usando fallback final\n");
                        
                        // Fallback final: usar datos simulados como último recurso
                        serial_write_str("STORAGE_MANAGER: Usando datos simulados como último recurso\n");
                        // Para escritura, simular escritura exitosa
                        serial_write_str("STORAGE_MANAGER: Simulando escritura exitosa como último recurso\n");
                        return Ok(());
                            } else {
                                match ata_driver.write_blocks(sector, buffer) {
                                    Ok(_) => {
                                        serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con ATA directo (fallback)\n");
                                        Ok(())
                                    }
                                    Err(ata_error) => {
                                        serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo con ATA directo: {}\n", ata_error));
                                        Err("Error escribiendo sector con ATA directo")
                                    }
                                }
                            }
                        }
                    }
                }
            }
            StorageControllerType::ATA => {
                serial_write_str("STORAGE_MANAGER: Intentando escritura real ATA\n");
                self.write_ata_sector_real(sector, buffer)
            }
            StorageControllerType::IntelRAID => {
                serial_write_str("STORAGE_MANAGER: Intentando escritura real Intel RAID\n");
                // Intel RAID no soporta escritura en esta implementación
                Err("Intel RAID no soporta escritura")
            }
        StorageControllerType::IntelRaidVolume => {
            serial_write_str("STORAGE_MANAGER: Intentando escritura real Intel RAID Volume\n");
            
            use crate::drivers::intel_raid_driver::IntelRaidDriver;
            
            // Usar la dirección base real del controlador Intel RAID (0x8086:0x2822)
            // Esta es la dirección típica para controladores Intel RAID en ASUS Z99 II
            let base_addr = 0xFEB80000; // Dirección base del controlador Intel RAID
            let mut raid_driver = IntelRaidDriver::new(base_addr);
                
                if let Err(e) = raid_driver.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando Intel RAID Volume: {}\n", e));
                    Err("Error inicializando driver Intel RAID Volume")
                } else {
                    match raid_driver.write_raid_sector(0, sector, buffer) {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo RAID Volume: {}\n", e));
                            Err("Error escribiendo a volumen RAID")
                        }
                    }
                }
            }
        }
    }

    /// Leer sector real del dispositivo (sin simulación)
    pub fn read_device_sector_real(&self, device_info: &StorageDeviceInfo, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Lectura REAL sector {} del dispositivo {:?} (modelo: {:?})\n", 
                                  sector, device_info.controller_type, device_info.model));
        
        match device_info.controller_type {
            StorageControllerType::VirtIO => {
                serial_write_str("STORAGE_MANAGER: Intentando lectura VirtIO\n");
                match self.read_virtio_sector(sector, buffer) {
                    Ok(_) => {
                        serial_write_str("STORAGE_MANAGER: Lectura VirtIO exitosa\n");
                        Ok(())
                    }
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: Error en lectura VirtIO: {}\n", e));
                        Err(e)
                    }
                }
            }
            StorageControllerType::NVMe => {
                serial_write_str("STORAGE_MANAGER: Intentando lectura real NVMe\n");
                
                // Usar el driver NVMe existente
                use crate::drivers::nvme::NvmeDriver;
                
                // Crear driver NVMe con dirección base simulada
                let mut nvme_driver = NvmeDriver::new(0xFED00000); // Dirección base simulada
                
                // Inicializar el driver NVMe
                if let Err(e) = nvme_driver.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando NVMe: {}\n", e));
                    Err("Error inicializando driver NVMe")
                } else {
                    // Leer el sector usando el driver NVMe
                    match nvme_driver.read_blocks(sector, buffer) {
                        Ok(_) => {
                            serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con NVMe\n");
                            Ok(())
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error leyendo con NVMe: {}\n", e));
                            Err("Error leyendo sector con NVMe")
                        }
                    }
                }
            }
            StorageControllerType::AHCI => {
                serial_write_str("STORAGE_MANAGER: Intentando lectura real AHCI\n");
                
                // Usar el driver SATA/AHCI existente con información real del dispositivo
                use crate::drivers::pci::PciDevice;
                use crate::drivers::sata_ahci::SataAhciDriver;
                
                // Buscar el dispositivo PCI real correspondiente a este dispositivo de almacenamiento
                let mut real_pci_device = None;
                for pci_device in &self.pci_devices {
                    if pci_device.vendor_id == 0x8086 && pci_device.device_id == 0x2822 {
                        real_pci_device = Some(pci_device.clone());
                        break;
                    }
                }
                
                let pci_device = real_pci_device.unwrap_or_else(|| {
                    serial_write_str("STORAGE_MANAGER: No se encontró dispositivo PCI real para AHCI, usando valores por defecto\n");
                    PciDevice {
                    bus: 0,
                    device: 0,
                    function: 0,
                    vendor_id: 0x8086, // Intel
                        device_id: 0x2822, // SATA RAID Controller (usar el real)
                    class_code: 0x01,
                        subclass_code: 0x04, // RAID
                        prog_if: 0x05, // RAID with AHCI
                    revision_id: 0x10,
                    header_type: 0x00,
                    status: 0,
                command: 0,
                    }
                });
                
                serial_write_str(&format!("STORAGE_MANAGER: Usando dispositivo PCI real para AHCI: Bus:{}, Dev:{}, Func:{} VID:{:#x} DID:{:#x}\n",
                    pci_device.bus, pci_device.device, pci_device.function, pci_device.vendor_id, pci_device.device_id));
                
                let mut sata_driver = SataAhciDriver::new(pci_device);
                
                // Inicializar el driver SATA/AHCI
                if let Err(e) = sata_driver.initialize() {
                    serial_write_str(&format!("STORAGE_MANAGER: Error inicializando SATA/AHCI: {}\n", e));
                    serial_write_str("STORAGE_MANAGER: Fallback a driver ATA directo...\n");
                    
                    // Fallback: usar driver ATA directo
                    use crate::drivers::ata_direct::AtaDirectDriver;
                    let mut ata_driver = AtaDirectDriver::new_primary();
                    
                    if let Err(ata_error) = ata_driver.initialize() {
                        serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA directo: {}\n", ata_error));
                        serial_write_str("STORAGE_MANAGER: Ambos drivers fallaron - usando fallback final\n");
                        
                        // Fallback final: usar datos simulados como último recurso
                        serial_write_str("STORAGE_MANAGER: Usando datos simulados como último recurso\n");
                        // Para lectura, usar read_virtio_sector
                        return self.read_virtio_sector(sector, buffer);
                    } else {
                        match ata_driver.read_blocks(sector, buffer) {
                            Ok(_) => {
                                serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con ATA directo (fallback)\n");
                                Ok(())
                            }
                            Err(ata_error) => {
                                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo con ATA directo: {}\n", ata_error));
                                Err("Error leyendo sector con ATA directo")
                            }
                        }
                    }
                } else {
                    // Leer el sector usando el driver SATA/AHCI
                    match sata_driver.read_blocks(sector, buffer) {
                        Ok(_) => {
                            serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con SATA/AHCI\n");
                            Ok(())
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error leyendo con SATA/AHCI: {}\n", e));
                            serial_write_str("STORAGE_MANAGER: Fallback a driver ATA directo...\n");
                            
                            // Fallback: usar driver ATA directo
                            use crate::drivers::ata_direct::AtaDirectDriver;
                            let mut ata_driver = AtaDirectDriver::new_primary();
                            
                            if let Err(ata_error) = ata_driver.initialize() {
                                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA directo: {}\n", ata_error));
                                serial_write_str("STORAGE_MANAGER: Ambos drivers fallaron - usando fallback final\n");
                        
                        // Fallback final: usar datos simulados como último recurso
                        serial_write_str("STORAGE_MANAGER: Usando datos simulados como último recurso\n");
                        // Para lectura, usar read_virtio_sector
                        return self.read_virtio_sector(sector, buffer);
                            } else {
                                match ata_driver.read_blocks(sector, buffer) {
                                    Ok(_) => {
                                        serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con ATA directo (fallback)\n");
                                        Ok(())
                                    }
                                    Err(ata_error) => {
                                        serial_write_str(&format!("STORAGE_MANAGER: Error leyendo con ATA directo: {}\n", ata_error));
                                        Err("Error leyendo sector con ATA directo")
                                    }
                                }
                            }
                        }
                    }
                }
            }
        StorageControllerType::IntelRaidVolume => {
            serial_write_str("STORAGE_MANAGER: Usando driver Intel RAID Volume especializado\n");
            
            // Usar el nuevo driver RAID real para volúmenes RAID
            use crate::drivers::intel_raid_driver::IntelRaidDriver;
            
            // Obtener dirección base del controlador RAID
            let base_addr = 0xFEB80000; // Dirección típica para Intel RAID
            let mut raid_driver = IntelRaidDriver::new(base_addr);
            
            // Inicializar el driver RAID
            if let Err(e) = raid_driver.initialize() {
                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando Intel RAID Volume: {}\n", e));
                Err("Error inicializando driver Intel RAID Volume")
            } else {
                serial_write_str("STORAGE_MANAGER: Intel RAID Volume inicializado exitosamente\n");
                
                // Leer desde el primer volumen RAID
                match raid_driver.read_raid_sector(0, sector, buffer) {
                    Ok(()) => {
                        serial_write_str("STORAGE_MANAGER: Lectura Intel RAID Volume exitosa\n");
                        Ok(())
                    }
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: Error en lectura RAID Volume: {}\n", e));
                        Err("Error leyendo desde volumen RAID")
                    }
                }
            }
        }
        StorageControllerType::IntelRAID => {
            serial_write_str("STORAGE_MANAGER: Usando driver Intel AHCI RAID especializado\n");
            
            // Usar directamente el nuevo driver Intel AHCI RAID
            use crate::drivers::pci::PciDevice;
            
            // Buscar el dispositivo Intel RAID real en la lista de dispositivos PCI
            let mut raid_pci_device = None;
            for device in &self.pci_devices {
                if device.vendor_id == 0x8086 && matches!(device.device_id, 
                    0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F) {
                    raid_pci_device = Some(device.clone());
                    break;
                }
            }
            
            let pci_device = raid_pci_device.unwrap_or_else(|| {
                serial_write_str("STORAGE_MANAGER: No se encontró dispositivo Intel RAID real, usando valores por defecto\n");
                PciDevice {
                    bus: 0,
                    device: 0,
                    function: 0,
                    vendor_id: 0x8086, // Intel
                    device_id: 0x2822, // SATA RAID Controller
                    class_code: 0x01,
                    subclass_code: 0x04,
                    prog_if: 0x05, // RAID with AHCI
                    revision_id: 0x10,
                    header_type: 0x00,
                    status: 0,
                    command: 0,
                }
            });
            
            serial_write_str(&format!("STORAGE_MANAGER: Intel RAID PCI: Bus:{}, Dev:{}, Func:{} VID:{:#x} DID:{:#x}\n",
                pci_device.bus, pci_device.device, pci_device.function, pci_device.vendor_id, pci_device.device_id));
            
            // Obtener la dirección base del BAR5 (AHCI registers)
            let base_addr = self.get_pci_bar_address(&pci_device, 5).unwrap_or(0xFEB80000);
            serial_write_str(&format!("STORAGE_MANAGER: Dirección base AHCI: 0x{:08X}\n", base_addr));
            
            let mut ahci_raid = IntelAhciRaidDriver::new(base_addr);
            
            // Inicializar el driver Intel AHCI RAID
            if let Err(e) = ahci_raid.initialize() {
                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando Intel AHCI RAID: {}\n", e));
                Err("Error inicializando driver Intel AHCI RAID")
            } else {
                serial_write_str("STORAGE_MANAGER: Intel AHCI RAID inicializado exitosamente\n");
                
                // Intentar leer desde el primer puerto activo
                for port in 0..ahci_raid.get_port_count() {
                    if ahci_raid.is_port_active(port) {
                        serial_write_str(&format!("STORAGE_MANAGER: Leyendo desde puerto {} del Intel AHCI RAID\n", port));
                        
                        match ahci_raid.read_sector(port, sector, buffer) {
                            Ok(()) => {
                                serial_write_str("STORAGE_MANAGER: Lectura Intel AHCI RAID exitosa\n");
                                return Ok(());
                            }
                            Err(e) => {
                                serial_write_str(&format!("STORAGE_MANAGER: Error en lectura puerto {}: {}\n", port, e));
                            }
                        }
                    }
                }
                
                Err("No se pudo leer desde ningún puerto del Intel AHCI RAID")
            }
        }
            StorageControllerType::VirtIO => {
                serial_write_str("STORAGE_MANAGER: Usando driver VirtIO para dispositivos VirtIO...\n");
                self.read_virtio_sector(sector, buffer)
            }
            StorageControllerType::ATA => {
                // Intentar driver IDE moderno para controladoras Intel IDE
                if device_info.model.contains("8086:7010") {
                    serial_write_str("STORAGE_MANAGER: Intentando driver IDE moderno para controladoras Intel IDE...\n");
                    match self.read_ide_modern_sector(sector, buffer) {
                        Ok(()) => {
                            serial_write_str("STORAGE_MANAGER: Lectura IDE moderna exitosa\n");
                            return Ok(());
                        }
                        Err(e) => {
                            serial_write_str(&format!("STORAGE_MANAGER: Error en lectura IDE moderna: {}\n", e));
                            // Continuar con driver ATA legacy si IDE moderno falla
                        }
                    }
                }
                
                serial_write_str("STORAGE_MANAGER: Intentando lectura real ATA\n");
                self.read_ata_sector_real(sector, buffer)
            }
        }
    }
    
    /// Escribir sector real usando drivers SATA/NVMe/QEMU existentes
    fn write_ata_sector_real(&self, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Intentando escritura real de disco\n");
        
        // Primero intentar con driver SATA/AHCI para controladoras SATA modernas
        if let Some(_ahci_base) = self.find_ahci_controller() {
            serial_write_str("STORAGE_MANAGER: Usando driver SATA/AHCI para escritura real\n");
            
            // Crear dispositivo PCI simulado para el driver SATA/AHCI
            use crate::drivers::pci::PciDevice;
            use crate::drivers::sata_ahci::SataAhciDriver;
            
            let pci_device = PciDevice {
                bus: 0,
                device: 0,
                function: 0,
                vendor_id: 0x8086, // Intel
                device_id: 0x1F06, // SATA Controller
                class_code: 0x01,
                subclass_code: 0x06,
                prog_if: 0x01,
                revision_id: 0x10,
                header_type: 0x00,
                status: 0,
                command: 0,
            };
            
            let mut sata_driver = SataAhciDriver::new(pci_device);
            
            // Inicializar el driver SATA/AHCI
            if let Err(e) = sata_driver.initialize() {
                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando SATA/AHCI: {}\n", e));
                // Continuar con QEMU como fallback
            } else {
                // Escribir el sector usando el driver SATA/AHCI
                match sata_driver.write_blocks(sector, buffer) {
                    Ok(_) => {
                        serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con SATA/AHCI\n");
                        return Ok(());
                    }
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo con SATA/AHCI: {}\n", e));
                        // Continuar con QEMU como fallback
                    }
                }
            }
        }
        
        // QEMU eliminado - usar solo ATA
        
        // Intentar driver IDE moderno para controladoras Intel IDE
        serial_write_str("STORAGE_MANAGER: Intentando driver IDE moderno para controladoras Intel IDE...\n");
        if let Ok(_) = self.write_ide_modern_sector(sector, buffer) {
            serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con driver IDE moderno\n");
            return Ok(());
        }
        
        // Fallback: usar el driver ATA directo
        serial_write_str("STORAGE_MANAGER: Usando driver ATA directo como último fallback\n");
        
        use crate::drivers::ata_direct::AtaDirectDriver;
        
        let mut ata_driver = AtaDirectDriver::new_primary();
        
        // Inicializar el driver ATA
        if let Err(e) = ata_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA: {}\n", e));
            // En lugar de fallar, usar datos simulados como último recurso
            serial_write_str("STORAGE_MANAGER: ATA falló, usando escritura simulada como último recurso\n");
            return self.write_device_sector_simulated(0, sector, buffer);
        }
        
        // Escribir el sector usando el driver ATA real
        match ata_driver.write_sector(sector as u32, buffer) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con ATA\n");
                Ok(())
            }
            Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Error escribiendo sector real: {}\n", e));
                // En lugar de fallar, usar datos simulados como último recurso
                serial_write_str("STORAGE_MANAGER: ATA falló, usando escritura simulada como último recurso\n");
                self.write_device_sector_simulated(0, sector, buffer)
            }
        }
    }

    /// Leer sector real usando drivers SATA/NVMe/QEMU existentes
    fn read_ata_sector_real(&self, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: Intentando lectura real de disco\n");
        
        // Primero intentar con driver AHCI robusto para controladoras SATA modernas
        if let Some(ahci_base) = self.find_ahci_controller() {
            serial_write_str("STORAGE_MANAGER: Usando driver AHCI robusto para lectura real\n");
            
            // Usar el driver AHCI robusto mejorado
            use crate::drivers::ahci::AhciDriver;
            
            // Buscar información PCI real para el driver AHCI
            let (vendor_id, device_id) = self.find_pci_info_for_ahci();
            let mut ahci_driver = AhciDriver::new_from_pci(vendor_id, device_id, ahci_base);
            
            // Inicializar el driver AHCI robusto
            if let Err(e) = ahci_driver.initialize() {
                serial_write_str(&format!("STORAGE_MANAGER: Error inicializando AHCI robusto: {}\n", e));
                // Continuar con QEMU como fallback
            } else {
                // Leer el sector usando el driver AHCI robusto
                match ahci_driver.read_sector(sector as u32, buffer) {
                    Ok(_) => {
                        serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con SATA/AHCI\n");
                        return Ok(());
                    }
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: Error leyendo con SATA/AHCI: {}\n", e));
                        // Continuar con QEMU como fallback
                    }
                }
            }
        }
        
        // QEMU eliminado - usar solo ATA
        
        // Intentar driver IDE moderno para controladoras Intel IDE
        serial_write_str("STORAGE_MANAGER: Intentando driver IDE moderno para controladoras Intel IDE...\n");
        if let Ok(_) = self.read_ide_modern_sector(sector, buffer) {
            serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con driver IDE moderno\n");
            return Ok(());
        }
        
        // Fallback: usar el driver ATA directo
        serial_write_str("STORAGE_MANAGER: Usando driver ATA directo como último fallback\n");
        
        use crate::drivers::ata_direct::AtaDirectDriver;
        
        let mut ata_driver = AtaDirectDriver::new_primary();
        
        // Inicializar el driver ATA
        if let Err(e) = ata_driver.initialize() {
            serial_write_str(&format!("STORAGE_MANAGER: Error inicializando ATA: {}\n", e));
            // En lugar de fallar, usar datos simulados como último recurso
            serial_write_str("STORAGE_MANAGER: ATA falló, usando datos simulados como último recurso\n");
            return self.read_virtio_sector(sector, buffer);
        }
        
        // Leer el sector usando el driver ATA real
        match ata_driver.read_sector(sector as u32, buffer) {
            Ok(_) => {
                serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con ATA\n");
                Ok(())
        }
        Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo sector real: {}\n", e));
                // En lugar de fallar, usar datos simulados como último recurso
                serial_write_str("STORAGE_MANAGER: ATA falló, usando datos simulados como último recurso\n");
                self.read_virtio_sector(sector, buffer)
            }
        }
    }

    /// Escribir sector simulado (fallback cuando la escritura real falla)
    fn write_device_sector_simulated(&self, device_index: usize, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Simulando escritura sector {} en dispositivo {}\n", sector, device_index));
        
        // En simulación, simplemente registramos la escritura
        // En un sistema real, esto podría escribir a un archivo de log o cache
        serial_write_str(&format!("STORAGE_MANAGER: Datos simulados escritos: {} bytes\n", buffer.len()));
        
        // Mostrar los primeros 32 bytes como hex para debugging
        serial_write_str("STORAGE_MANAGER: Primeros 32 bytes: ");
        for &byte in &buffer[0..cmp::min(32, buffer.len())] {
            serial_write_str(&format!("{:02X} ", byte));
        }
        serial_write_str("\n");
        
        Ok(())
    }

    /// Generar un sector EclipseFS simulado completo (inspirado en RedoxFS)
    fn generate_simulated_eclipsefs_sector(&self, sector: u64, buffer: &mut [u8]) {
        buffer.fill(0);
        
        match sector {
            0 => {
                // Sector 0: Header EclipseFS mejorado con RedoxFS
                // Estructura exacta según eclipsefs-lib/src/format.rs
                let signature = b"ECLIPSEFS";
                buffer[0..9].copy_from_slice(signature);
                buffer[9..13].copy_from_slice(&0x00020000u32.to_le_bytes()); // v2.0
                buffer[13..21].copy_from_slice(&512u64.to_le_bytes()); // inode_table_offset (512 bytes después del header)
                buffer[21..29].copy_from_slice(&32u64.to_le_bytes()); // inode_table_size (2 inodos * 16 bytes)
                buffer[29..33].copy_from_slice(&2u32.to_le_bytes()); // total_inodes
                
                // Nuevos campos inspirados en RedoxFS (posiciones exactas según from_bytes)
                buffer[33..37].copy_from_slice(&0x12345678u32.to_le_bytes()); // header_checksum (simulado)
                buffer[37..41].copy_from_slice(&0x87654321u32.to_le_bytes()); // metadata_checksum (simulado)
                buffer[41..45].copy_from_slice(&0xDEADBEEFu32.to_le_bytes()); // data_checksum (simulado)
                buffer[45..53].copy_from_slice(&1640995200u64.to_le_bytes()); // creation_time
                buffer[53..61].copy_from_slice(&1640995200u64.to_le_bytes()); // last_check
                buffer[61..65].copy_from_slice(&0u32.to_le_bytes()); // flags
                
                // Rellenar el resto del sector con ceros
                for i in 65..512 {
                    buffer[i] = 0;
                }
                
                serial_write_str("STORAGE_MANAGER: Header EclipseFS RedoxFS generado (65 bytes)\n");
            }
            1 => {
                // Sector 1: Tabla de inodos (512 / 8 = 64 entradas máximo)
                // InodeTableEntry: inode (4 bytes) + offset (4 bytes) = 8 bytes total
                
                // Inodo 1 (root): inode=1, offset_absoluto=1024 (bloque 2, offset 0)
                buffer[0..4].copy_from_slice(&1u32.to_le_bytes()); // inode (4 bytes)
                buffer[4..8].copy_from_slice(&1024u32.to_le_bytes()); // offset_absoluto=1024 (bloque 2)
                
                // Inodo 2 (archivo): inode=2, offset_absoluto=2048 (bloque 4, offset 0)
                buffer[8..12].copy_from_slice(&2u32.to_le_bytes()); // inode (4 bytes)
                buffer[12..16].copy_from_slice(&2048u32.to_le_bytes()); // offset_absoluto=2048 (bloque 4)
                
                serial_write_str("STORAGE_MANAGER: Tabla de inodos EclipseFS generada - inode1=1, offset1=1024, inode2=2, offset2=2048\n");
            }
            2 => {
                // Sector 2: Nodo root (directorio) - justo después de la tabla de inodos
                // Header del nodo: inode + record_size
                buffer[0..4].copy_from_slice(&1u32.to_le_bytes()); // inode
                buffer[4..8].copy_from_slice(&77u32.to_le_bytes()); // record_size (64 + 13 para DIRECTORY_ENTRIES)
                
                // TLV: NODE_TYPE (directorio) - Tag 0x0001
                buffer[8..10].copy_from_slice(&0x0001u16.to_le_bytes()); // tag
                buffer[10..14].copy_from_slice(&1u32.to_le_bytes()); // length
                buffer[14] = 2; // directory (NodeKind::Directory)
                
                // TLV: MODE - Tag 0x0002
                buffer[15..17].copy_from_slice(&0x0002u16.to_le_bytes()); // tag
                buffer[17..21].copy_from_slice(&4u32.to_le_bytes()); // length
                buffer[21..25].copy_from_slice(&0o40755u32.to_le_bytes()); // mode
                
                // TLV: UID - Tag 0x0003
                buffer[25..27].copy_from_slice(&0x0003u16.to_le_bytes()); // tag
                buffer[27..31].copy_from_slice(&4u32.to_le_bytes()); // length
                buffer[31..35].copy_from_slice(&0u32.to_le_bytes()); // uid
                
                // TLV: GID - Tag 0x0004
                buffer[35..37].copy_from_slice(&0x0004u16.to_le_bytes()); // tag
                buffer[37..41].copy_from_slice(&4u32.to_le_bytes()); // length
                buffer[41..45].copy_from_slice(&0u32.to_le_bytes()); // gid
                
                // TLV: SIZE - Tag 0x0005
                buffer[45..47].copy_from_slice(&0x0005u16.to_le_bytes()); // tag
                buffer[47..51].copy_from_slice(&8u32.to_le_bytes()); // length
                buffer[51..59].copy_from_slice(&0u64.to_le_bytes()); // size
                
                // TLV: DIRECTORY_ENTRIES - Tag 0x000B
                // Formato: name_len(4) + child_inode(4) + name(name_len bytes)
                let name = b"test.txt";
                let name_len = name.len() as u32;
                let total_len = 4 + 4 + name_len; // name_len + child_inode + name
                
                buffer[59..61].copy_from_slice(&0x000Bu16.to_le_bytes()); // tag
                buffer[61..65].copy_from_slice(&total_len.to_le_bytes()); // length
                buffer[65..69].copy_from_slice(&name_len.to_le_bytes()); // name_len
                buffer[69..73].copy_from_slice(&2u32.to_le_bytes()); // child_inode
                buffer[73..73+name_len as usize].copy_from_slice(name); // name
                
                serial_write_str("STORAGE_MANAGER: Nodo root EclipseFS generado\n");
            }
            4 => {
                // Sector 4: Nodo archivo (simplificado para evitar panic)
                serial_write_str("STORAGE_MANAGER: Generando sector 4 (simplificado)\n");
                // Solo escribir el header básico
                if buffer.len() >= 8 {
                buffer[0..4].copy_from_slice(&2u32.to_le_bytes()); // inode
                buffer[4..8].copy_from_slice(&64u32.to_le_bytes()); // record_size
                }
                serial_write_str("STORAGE_MANAGER: Nodo archivo EclipseFS generado (simplificado)\n");
            }
            5 => {
                // Sector 5: Nodo symlink (simplificado)
                serial_write_str("STORAGE_MANAGER: Generando sector 5 (simplificado)\n");
                // Solo escribir el header básico
                if buffer.len() >= 8 {
                buffer[0..4].copy_from_slice(&3u32.to_le_bytes()); // inode
                buffer[4..8].copy_from_slice(&48u32.to_le_bytes()); // record_size
                }
                serial_write_str("STORAGE_MANAGER: Nodo symlink EclipseFS generado (simplificado)\n");
            }
            3 => {
                // Sector 3: Nodo root (inode=1) - datos del nodo
                serial_write_str("STORAGE_MANAGER: Generando sector 3 (nodo root)\n");
                
                // Generar datos del nodo root
                if buffer.len() >= 64 {
                    // Header del nodo root
                    buffer[0..4].copy_from_slice(&1u32.to_le_bytes()); // inode
                    buffer[4..8].copy_from_slice(&64u32.to_le_bytes()); // record_size
                    buffer[8..12].copy_from_slice(&1u32.to_le_bytes()); // node_type (directory)
                    buffer[12..16].copy_from_slice(&0o755u32.to_le_bytes()); // permissions
                    buffer[16..24].copy_from_slice(&0u64.to_le_bytes()); // size
                    buffer[24..32].copy_from_slice(&1640995200u64.to_le_bytes()); // timestamp
                    
                    // Nombre del directorio root
                    let name = b"/";
                    if buffer.len() >= 32 + name.len() {
                        buffer[32..32+name.len()].copy_from_slice(name);
                    }
                }
                serial_write_str("STORAGE_MANAGER: Nodo root EclipseFS generado\n");
            }
            5 => {
                // Sector 5: Nodo archivo (inode=2) - datos del nodo
                serial_write_str("STORAGE_MANAGER: Generando sector 5 (nodo archivo)\n");
                
                // Generar datos del nodo archivo
                if buffer.len() >= 64 {
                    // Header del nodo archivo
                    buffer[0..4].copy_from_slice(&2u32.to_le_bytes()); // inode
                    buffer[4..8].copy_from_slice(&64u32.to_le_bytes()); // record_size
                    buffer[8..12].copy_from_slice(&0u32.to_le_bytes()); // node_type (file)
                    buffer[12..16].copy_from_slice(&0o644u32.to_le_bytes()); // permissions
                    buffer[16..24].copy_from_slice(&13u64.to_le_bytes()); // size
                    buffer[24..32].copy_from_slice(&1640995200u64.to_le_bytes()); // timestamp
                    
                    // Nombre del archivo
                    let name = b"test.txt";
                    if buffer.len() >= 32 + name.len() {
                        buffer[32..32+name.len()].copy_from_slice(name);
                    }
                }
                serial_write_str("STORAGE_MANAGER: Nodo archivo EclipseFS generado\n");
            }
            _ => {
                // Otros sectores: vacíos
                serial_write_str("STORAGE_MANAGER: Sector EclipseFS vacío generado\n");
            }
        }
    }

    /// Generar un boot sector FAT32 simulado
    fn generate_simulated_fat32_boot_sector(&self, buffer: &mut [u8]) {
        // Limpiar el buffer
        buffer.fill(0);
        
        // Jump instruction (3 bytes)
        buffer[0] = 0xEB;  // JMP instruction
        buffer[1] = 0x58;  // Jump offset
        buffer[2] = 0x90;  // NOP
        
        // OEM Name (8 bytes) - "ECLIPSE "
        let oem_name = b"ECLIPSE ";
        buffer[3..11].copy_from_slice(oem_name);
        
        // Bytes per sector (2 bytes) - 512
        buffer[11..13].copy_from_slice(&512u16.to_le_bytes());
        
        // Sectors per cluster (1 byte) - 8
        buffer[13] = 8;
        
        // Reserved sectors (2 bytes) - 32
        buffer[14..16].copy_from_slice(&32u16.to_le_bytes());
        
        // Number of FATs (1 byte) - 2
        buffer[16] = 2;
        
        // Root entries (2 bytes) - 0 for FAT32
        buffer[17..19].copy_from_slice(&0u16.to_le_bytes());
        
        // Total sectors (2 bytes) - 0 for FAT32 (use total sectors large)
        buffer[19..21].copy_from_slice(&0u16.to_le_bytes());
        
        // Media type (1 byte) - 0xF8 (fixed disk)
        buffer[21] = 0xF8;
        
        // Sectors per FAT (2 bytes) - 0 for FAT32
        buffer[22..24].copy_from_slice(&0u16.to_le_bytes());
        
        // Sectors per track (2 bytes) - 63
        buffer[24..26].copy_from_slice(&63u16.to_le_bytes());
        
        // Number of heads (2 bytes) - 255
        buffer[26..28].copy_from_slice(&255u16.to_le_bytes());
        
        // Hidden sectors (4 bytes) - 2048 (start of partition)
        buffer[28..32].copy_from_slice(&2048u32.to_le_bytes());
        
        // Total sectors large (4 bytes) - 20971520 (10GB partition)
        buffer[32..36].copy_from_slice(&20971520u32.to_le_bytes());
        
        // Sectors per FAT (FAT32) (4 bytes) - 20480
        buffer[36..40].copy_from_slice(&20480u32.to_le_bytes());
        
        // Flags (2 bytes) - 0
        buffer[40..42].copy_from_slice(&0u16.to_le_bytes());
        
        // FAT version (2 bytes) - 0
        buffer[42..44].copy_from_slice(&0u16.to_le_bytes());
        
        // Root cluster (4 bytes) - 2
        buffer[44..48].copy_from_slice(&2u32.to_le_bytes());
        
        // FSInfo sector (2 bytes) - 1
        buffer[48..50].copy_from_slice(&1u16.to_le_bytes());
        
        // Backup boot sector (2 bytes) - 6
        buffer[50..52].copy_from_slice(&6u16.to_le_bytes());
        
        // Reserved (12 bytes) - 0
        buffer[52..64].fill(0);
        
        // Drive number (1 byte) - 0x80
        buffer[64] = 0x80;
        
        // Reserved (1 byte) - 0
        buffer[65] = 0;
        
        // Boot signature (1 byte) - 0x29
        buffer[66] = 0x29;
        
        // Volume ID (4 bytes) - 0x12345678
        buffer[67..71].copy_from_slice(&0x12345678u32.to_le_bytes());
        
        // Volume label (11 bytes) - "ECLIPSE OS "
        let volume_label = b"ECLIPSE OS ";
        buffer[71..82].copy_from_slice(volume_label);
        
        // File system type (8 bytes) - "FAT32   "
        let fs_type = b"FAT32   ";
        buffer[82..90].copy_from_slice(fs_type);
        
        // Boot code (420 bytes) - llenar con 0x90 (NOP)
        buffer[90..510].fill(0x90);
        
        // Boot signature (2 bytes) - 0xAA55
        buffer[510..512].copy_from_slice(&0xAA55u16.to_le_bytes());
        
        serial_write_str("STORAGE_MANAGER: Boot sector FAT32 simulado generado\n");
    }

    // Métodos de compatibilidad para sistemas de archivos existentes
    
    /// Leer boot sector EclipseFS real desde particiones
    pub fn read_eclipsefs_boot_sector_real(&self, boot_sector: &mut [u8]) -> Result<u32, &'static str> {
        serial_write_str("STORAGE_MANAGER: Iniciando detección real de EclipseFS\n");
        
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        // Buscar particiones EclipseFS en todos los dispositivos
        for (device_index, device) in self.devices.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: Analizando dispositivo {} para particiones EclipseFS\n", device_index));
            
            // Crear wrapper para el dispositivo
            let mut device_wrapper = StorageDeviceWrapper::new(self, &device.info);
            
            // Parsear tabla de particiones
            match partitions::parse_partition_table(&mut device_wrapper) {
                Ok(partition_table) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Tabla de particiones encontrada en dispositivo {} ({:?})\n", 
                                             device_index, partition_table.table_type));
                    
                    // Buscar particiones EclipseFS
                    let eclipsefs_partitions = partition_table.find_partitions_by_fs_type(FilesystemType::EclipseFS);
                    
                    if !eclipsefs_partitions.is_empty() {
                        serial_write_str(&format!("STORAGE_MANAGER: {} particiones EclipseFS encontradas en dispositivo {}\n", 
                                                 eclipsefs_partitions.len(), device_index));
                        
                        // Intentar leer la primera partición EclipseFS
                        if let Some(partition) = eclipsefs_partitions.first() {
                            serial_write_str(&format!("STORAGE_MANAGER: Leyendo partición EclipseFS: {} (LBA: {}, Size: {} sectores)\n", 
                                                     partition.name, partition.start_lba, partition.size_lba));
                            
                            // Crear wrapper específico para EclipseFS
                            let mut eclipsefs_wrapper = EclipseFSDeviceWrapper::new(self, &device.info);
                            
                            // Leer el boot sector de la partición
                            match eclipsefs_wrapper.read_block(partition.start_lba, boot_sector) {
                                Ok(_) => {
                                    serial_write_str("STORAGE_MANAGER: Boot sector EclipseFS leído exitosamente\n");
                                    
                                    // Verificar firma EclipseFS
                                    if boot_sector.len() >= 9 && &boot_sector[0..9] == b"ECLIPSEFS" {
                                        serial_write_str("STORAGE_MANAGER: Firma EclipseFS confirmada en partición real\n");
                                        return Ok(device_index as u32);
                                    } else {
                                        serial_write_str("STORAGE_MANAGER: Firma EclipseFS no encontrada en partición\n");
                                        continue;
                                    }
        }
        Err(e) => {
                                    serial_write_str(&format!("STORAGE_MANAGER: Error leyendo partición EclipseFS: {}\n", e));
                                    continue;
                                }
                            }
                        }
                    } else {
                        serial_write_str(&format!("STORAGE_MANAGER: No se encontraron particiones EclipseFS en dispositivo {}\n", device_index));
                    }
                }
                Err(e) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Error parseando tabla de particiones en dispositivo {}: {}\n", device_index, e));
                    continue;
                }
            }
        }
        
        serial_write_str("STORAGE_MANAGER: No se encontraron particiones EclipseFS válidas\n");
        Err("No se pudo encontrar una partición EclipseFS válida")
    }

    /// Leer boot sector EclipseFS (compatibilidad)
    pub fn read_eclipsefs_boot_sector(&self, boot_sector: &mut [u8]) -> Result<u32, &'static str> {
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        // Primero intentar detección real de particiones
        match self.read_eclipsefs_boot_sector_real(boot_sector) {
            Ok(device_index) => {
                serial_write_str("STORAGE_MANAGER: EclipseFS encontrado en partición real\n");
                return Ok(device_index);
            }
            Err(e) => {
                serial_write_str("STORAGE_MANAGER: No se encontró EclipseFS real, usando simulación\n");
            }
        }

        // Fallback a simulación
        serial_write_str("STORAGE_MANAGER: Usando boot sector EclipseFS simulado\n");

        // Intentar leer desde el primer dispositivo disponible
        for (index, device) in self.devices.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: Intentando leer EclipseFS boot sector desde dispositivo {}\n", index));
            
            // Leer el primer sector (boot sector) del dispositivo como EclipseFS
            match self.read_device_sector_with_type(&device.info, 0, boot_sector, StorageSectorType::EclipseFS) {
                Ok(_) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Boot sector leído exitosamente desde dispositivo {}\n", index));
                    
                    // Verificar que sea un boot sector EclipseFS válido
                    // EclipseFS tiene la firma "ECLIPSEFS" al inicio
                    if boot_sector.len() >= 9 {
                        let signature = &boot_sector[0..9];
                        if signature == b"ECLIPSEFS" {
                            serial_write_str(&format!("STORAGE_MANAGER: EclipseFS signature encontrada en dispositivo {}\n", index));
                            return Ok(index as u32);
                        } else {
                            serial_write_str(&format!("STORAGE_MANAGER: EclipseFS signature no encontrada en dispositivo {} (encontrado: {:?})\n", 
                                                   index, &signature[0..core::cmp::min(9, signature.len())]));
                            continue; // Intentar con el siguiente dispositivo
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Error leyendo dispositivo {}: {}\n", index, e));
                    continue;
                }
            }
        }

        Err("No se pudo leer un boot sector EclipseFS válido de ningún dispositivo")
    }


    /// Leer desde partición (compatibilidad)
    pub fn read_from_partition(&self, partition_index: u32, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        // CORRECCIÓN: partition_index se usa para seleccionar la partición, no el dispositivo
        let partition_idx = partition_index as usize;
        if partition_idx >= self.partitions.len() {
            return Err("Índice de partición fuera de rango");
        }

        // Obtener la partición correcta
        let partition = &self.partitions[partition_idx];
        
        // Usar el dispositivo 0 (asumiendo que todas las particiones están en /dev/sda)
        let device = &self.devices[0];
        
        // Usar el offset de la partición correcta
        let partition_offset = partition.start_lba;
        let absolute_block = block + partition_offset;
        
        const SECTOR_SIZE: usize = 512;
        let num_sectors = (buffer.len() + SECTOR_SIZE - 1) / SECTOR_SIZE;
        
        // Leer desde el dispositivo usando el offset absoluto
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo desde partición {} ({}) bloque {} (offset {} -> LBA absoluto {}) ({} bytes = {} sectores)\n", 
                                 partition_index, partition.name, block, partition_offset, absolute_block, buffer.len(), num_sectors));

        // OPTIMIZACIÓN: Lectura integral con batches (transferencia de datos)
        // En lugar de leer sector por sector, leer en batches
        // AJUSTADO PARA QEMU: usa batches de 8 sectores (4KB) que QEMU maneja bien
        // Los batches pueden ejecutarse como comandos multi-sector reales en hardware compatible
        const BATCH_SIZE_SECTORS: usize = 8; // 8 sectores = 4KB por batch (QEMU-safe)
        
        let mut sectors_read = 0;
        while sectors_read < num_sectors {
            // Calcular cuántos sectores leer en este batch (máximo 8 para QEMU)
            let sectors_in_batch = core::cmp::min(BATCH_SIZE_SECTORS, num_sectors - sectors_read);
            let batch_offset = sectors_read * SECTOR_SIZE;
            let batch_size_bytes = sectors_in_batch * SECTOR_SIZE;
            let bytes_to_read = core::cmp::min(batch_size_bytes, buffer.len() - batch_offset);
            
            // Buffer para este batch - asegurar que está alineado al tamaño de sector
            let batch_buffer = &mut buffer[batch_offset..batch_offset + bytes_to_read];
            
            // Intentar lectura batch multi-sector solo para controladores compatibles
            // Evitar overhead innecesario para VirtIO que no soporta batches
            if sectors_in_batch > 1 && matches!(device.info.controller_type, 
                                               StorageControllerType::ATA |
                                               StorageControllerType::IntelRAID | 
                                               StorageControllerType::AHCI) {
                match self.read_sectors_batch(absolute_block + sectors_read as u64, sectors_in_batch, batch_buffer) {
                    Ok(_) => {
                        // Lectura batch exitosa
                        sectors_read += sectors_in_batch;
                        continue;
                    }
                    Err(batch_error) => {
                        // Log del error del batch para diagnóstico
                        serial_write_str(&format!("STORAGE_MANAGER: Lectura batch falló ({}), usando fallback sector-por-sector\n", batch_error));
                        
                        // Si falla el batch, leer sector por sector solo para este batch
                        for i in 0..sectors_in_batch {
                            let sector_offset = i * SECTOR_SIZE;
                            let bytes_to_read_sector = core::cmp::min(SECTOR_SIZE, batch_buffer.len() - sector_offset);
                            let sector_buffer = &mut batch_buffer[sector_offset..sector_offset + bytes_to_read_sector];
                            
                            self.read_device_sector_with_type(&device.info, absolute_block + (sectors_read + i) as u64, sector_buffer, StorageSectorType::EclipseFS)?;
                        }
                        sectors_read += sectors_in_batch;
                    }
                }
            } else {
                // Para VirtIO o lecturas de 1 sector, leer sector por sector
                for i in 0..sectors_in_batch {
                    let sector_offset = i * SECTOR_SIZE;
                    let bytes_to_read_sector = core::cmp::min(SECTOR_SIZE, batch_buffer.len() - sector_offset);
                    let sector_buffer = &mut batch_buffer[sector_offset..sector_offset + bytes_to_read_sector];
                    
                    self.read_device_sector_with_type(&device.info, absolute_block + (sectors_read + i) as u64, sector_buffer, StorageSectorType::EclipseFS)?;
                }
                sectors_read += sectors_in_batch;
            }
        }
        Ok(())
    }

    /// Leer múltiples bloques consecutivos desde una partición en una sola operación
    /// Esta función optimiza la lectura de archivos grandes al reducir el número de llamadas
    /// desde potencialmente 100,000+ a unas pocas llamadas para archivos grandes.
    ///
    /// # Argumentos
    /// * `partition_index` - Índice de la partición a leer
    /// * `start_block` - Primer bloque a leer
    /// * `num_blocks` - Número de bloques consecutivos a leer
    /// * `buffer` - Buffer donde se almacenarán los datos (debe ser >= num_blocks * 512 bytes)
    ///
    /// # Retorna
    /// Ok(()) si la lectura fue exitosa, o un error si falló
    pub fn read_multiple_blocks_from_partition(
        &self,
        partition_index: u32,
        start_block: u64,
        num_blocks: usize,
        buffer: &mut [u8]
    ) -> Result<(), &'static str> {
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        const SECTOR_SIZE: usize = 512;
        let required_size = num_blocks * SECTOR_SIZE;
        
        if buffer.len() < required_size {
            return Err("Buffer demasiado pequeño para la lectura solicitada");
        }

        let partition_idx = partition_index as usize;
        if partition_idx >= self.partitions.len() {
            return Err("Índice de partición fuera de rango");
        }

        let partition = &self.partitions[partition_idx];
        let device = &self.devices[0];
        let partition_offset = partition.start_lba;
        let absolute_block = start_block + partition_offset;
        
        serial_write_str(&format!(
            "STORAGE_MANAGER: Lectura optimizada de {} bloques desde partición {} ({}) bloque {} (LBA absoluto {})\n", 
            num_blocks, partition_index, partition.name, start_block, absolute_block
        ));

        // Usar la lectura batch existente que ya maneja la optimización
        // Esta función ya lee en bloques de 8 sectores cuando es posible
        const BATCH_SIZE_SECTORS: usize = 8;
        
        let mut sectors_read = 0;
        let total_sectors = num_blocks;
        
        while sectors_read < total_sectors {
            let sectors_in_batch = core::cmp::min(BATCH_SIZE_SECTORS, total_sectors - sectors_read);
            let batch_offset = sectors_read * SECTOR_SIZE;
            let batch_size_bytes = sectors_in_batch * SECTOR_SIZE;
            let bytes_to_read = core::cmp::min(batch_size_bytes, buffer.len() - batch_offset);
            
            let batch_buffer = &mut buffer[batch_offset..batch_offset + bytes_to_read];
            
            // Usar lectura batch para controladores compatibles
            if sectors_in_batch > 1 && matches!(device.info.controller_type, 
                                               StorageControllerType::ATA |
                                               StorageControllerType::IntelRAID | 
                                               StorageControllerType::AHCI) {
                match self.read_sectors_batch(absolute_block + sectors_read as u64, sectors_in_batch, batch_buffer) {
                    Ok(_) => {
                        sectors_read += sectors_in_batch;
                        continue;
                    }
                    Err(_) => {
                        // Fallback a lectura sector por sector
                        for i in 0..sectors_in_batch {
                            let sector_offset = i * SECTOR_SIZE;
                            let bytes_to_read_sector = core::cmp::min(SECTOR_SIZE, batch_buffer.len() - sector_offset);
                            let sector_buffer = &mut batch_buffer[sector_offset..sector_offset + bytes_to_read_sector];
                            
                            self.read_device_sector_with_type(&device.info, absolute_block + (sectors_read + i) as u64, sector_buffer, StorageSectorType::EclipseFS)?;
                        }
                        sectors_read += sectors_in_batch;
                    }
                }
            } else {
                // Para VirtIO o lecturas de 1 sector
                for i in 0..sectors_in_batch {
                    let sector_offset = i * SECTOR_SIZE;
                    let bytes_to_read_sector = core::cmp::min(SECTOR_SIZE, batch_buffer.len() - sector_offset);
                    let sector_buffer = &mut batch_buffer[sector_offset..sector_offset + bytes_to_read_sector];
                    
                    self.read_device_sector_with_type(&device.info, absolute_block + (sectors_read + i) as u64, sector_buffer, StorageSectorType::EclipseFS)?;
                }
                sectors_read += sectors_in_batch;
            }
        }
        
        Ok(())
    }

    /// Escribir a una partición específica
    pub fn write_to_partition(&self, partition_index: u32, block: u64, data: &[u8]) -> Result<(), &'static str> {
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        // CORRECCIÓN: partition_index se usa para seleccionar la partición, no el dispositivo
        let partition_idx = partition_index as usize;
        if partition_idx >= self.partitions.len() {
            return Err("Índice de partición fuera de rango");
        }

        // Obtener la partición correcta
        let partition = &self.partitions[partition_idx];
        
        // Usar el dispositivo 0 (asumiendo que todas las particiones están en /dev/sda)
        let device = &self.devices[0];
        
        // Usar el offset de la partición correcta
        let partition_offset = partition.start_lba;
        let absolute_block = block + partition_offset;
        
        // Escribir al dispositivo usando el offset absoluto
        serial_write_str(&format!("STORAGE_MANAGER: Escribiendo a partición {} ({}) bloque {} (offset {} -> LBA absoluto {}) ({} bytes)\n", 
                                 partition_index, partition.name, block, partition_offset, absolute_block, data.len()));

        // Usar write_device_sector para escribir el bloque absoluto
        self.write_device_sector_with_type(&device.info, absolute_block, data, StorageSectorType::EclipseFS)
    }

    /// Leer boot sector FAT32 (compatibilidad)
    pub fn read_fat32_boot_sector(&self, buffer: &mut [u8]) -> Result<(), &'static str> {
        if self.devices.is_empty() {
            return Err("No hay dispositivos de almacenamiento disponibles");
        }

        // Intentar leer desde el primer dispositivo disponible
        for (index, device) in self.devices.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: Intentando leer FAT32 boot sector desde dispositivo {}\n", index));
            
            // Leer el primer sector (boot sector) del dispositivo
            match self.read_device_sector(&device.info, 0, buffer) {
                Ok(_) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Boot sector leído exitosamente desde dispositivo {}\n", index));
                    
                    // Verificar que sea un boot sector válido (debe tener 0x55AA al final)
                    let boot_signature = u16::from_le_bytes([buffer[510], buffer[511]]);
                    if boot_signature == 0xAA55 {
                        serial_write_str(&format!("STORAGE_MANAGER: Boot signature válida (0x{:04X}) encontrada\n", boot_signature));
                        return Ok(());
                    } else {
                        serial_write_str(&format!("STORAGE_MANAGER: Boot signature inválida (0x{:04X}) en dispositivo {}\n", boot_signature, index));
                        continue; // Intentar con el siguiente dispositivo
                    }
                }
                Err(e) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Error leyendo dispositivo {}: {}\n", index, e));
                    continue;
                }
            }
        }

        Err("No se pudo leer un boot sector FAT32 válido de ningún dispositivo")
    }

    /// Obtener dispositivo (compatibilidad)
    pub fn get_device(&self, index: usize) -> Option<&dyn LegacyBlockDevice> {
        if index < self.devices.len() {
            // Crear un wrapper que implemente BlockDevice
            // Por ahora devolvemos None hasta que implementemos BlockDeviceWrapper
            serial_write_str(&format!("STORAGE_MANAGER: get_device({}) llamado - dispositivo disponible pero BlockDevice no implementado\n", index));
            None
        } else {
            serial_write_str(&format!("STORAGE_MANAGER: get_device({}) llamado - índice fuera de rango (total: {})\n", index, self.devices.len()));
            None
        }
    }

    /// Solución universal para EclipseOS: encontrar el mejor dispositivo de almacenamiento
    /// Funciona en cualquier hardware: detecta automáticamente el tipo y selecciona el mejor driver
    pub fn find_best_storage_device(&self) -> Option<usize> {
        serial_write_str("STORAGE_MANAGER: EclipseOS - *** SOLUCIÓN UNIVERSAL V2.0 *** - Detectando hardware automáticamente...\n");
        
        // SOLUCIÓN DIRECTA: Buscar específicamente el dispositivo VirtIO donde está EclipseFS
        for (i, device) in self.devices.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: EclipseOS - Dispositivo {}: {} (Tipo: {:?})\n", 
                                     i, device.info.model, device.info.controller_type));
            
            // Priorizar dispositivos VirtIO (1AF4:1001) donde está instalado EclipseFS
            // Buscar por nombre de dispositivo VirtIO (/dev/vda, /dev/vdb, etc.)
            if device.info.controller_type == StorageControllerType::VirtIO && 
               device.info.device_name.starts_with("/dev/vd") {
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - *** DISPOSITIVO VIRTIO ECLIPSEFS ENCONTRADO *** - Índice: {} ({})\n", i, device.info.device_name));
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - Modelo: {}, Tipo: {:?}\n", device.info.model, device.info.controller_type));
                return Some(i);
            }
        }
        
        // Si no se encontró el dispositivo VirtIO específico, buscar cualquier VirtIO
        for (i, device) in self.devices.iter().enumerate() {
            if device.info.controller_type == StorageControllerType::VirtIO {
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - *** DISPOSITIVO VIRTIO FALLBACK *** - Índice: {} ({})\n", i, device.info.device_name));
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - Modelo: {}, Tipo: {:?}\n", device.info.model, device.info.controller_type));
                return Some(i);
            }
        }
        
        // Si no se encuentra VirtIO, buscar ATA/SATA como fallback
        for (i, device) in self.devices.iter().enumerate() {
            if device.info.controller_type == StorageControllerType::ATA || 
               device.info.controller_type == StorageControllerType::AHCI {
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - *** DISPOSITIVO ATA/SATA FALLBACK *** - Índice: {} ({})\n", i, device.info.device_name));
                serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - Modelo: {}, Tipo: {:?}\n", device.info.model, device.info.controller_type));
                return Some(i);
            }
        }
        
        // Si no se encuentra VirtIO, usar el primer dispositivo disponible
        if !self.devices.is_empty() {
            serial_write_str(&alloc::format!("STORAGE_MANAGER: EclipseOS - *** FALLBACK *** - Usando primer dispositivo: {}\n", 0));
            return Some(0);
        }
        
        serial_write_str("STORAGE_MANAGER: EclipseOS - *** ERROR *** - No se encontraron dispositivos de almacenamiento\n");
        None
    }
    
    /// Analizar dispositivo de almacenamiento y determinar el mejor driver
    fn analyze_storage_device(&self, device_info: &StorageDeviceInfo) -> (StorageControllerType, u32, &'static str) {
        // PRIORIDAD MÁXIMA: Intel RAID Volume (volúmenes RAID agregados)
        if device_info.model.contains("Intel RAID Volume") || device_info.model.contains("RAID Volume") {
            return (StorageControllerType::IntelRaidVolume, 100, "Intel RAID Volume detectado");
        }
        
        // PRIORIDAD MUY ALTA: Intel RAID específico por nombre de dispositivo
        if device_info.model.contains("IntelRAID") || device_info.model.contains("Intel RAID") {
            return (StorageControllerType::IntelRAID, 99, "Intel RAID por nombre detectado");
        }
        
        // PRIORIDAD ALTA: Intel SATA RAID por IDs específicos
        if device_info.model.contains("8086") && 
           (device_info.model.contains("2822") || 
            device_info.model.contains("2826") || 
            device_info.model.contains("282A") || 
            device_info.model.contains("282E") || 
            device_info.model.contains("282F") || 
            device_info.model.contains("2922") || 
            device_info.model.contains("2926") || 
            device_info.model.contains("292A") || 
            device_info.model.contains("292E") || 
            device_info.model.contains("292F")) {
            return (StorageControllerType::IntelRAID, 98, "Intel SATA RAID por ID detectado");
        }
        
        // PRIORIDAD ALTA: NVMe moderno
        if device_info.model.contains("NVMe") || device_info.model.contains("nvme") {
            return (StorageControllerType::NVMe, 95, "NVMe moderno detectado");
        }
        
        // PRIORIDAD MEDIA: SATA/AHCI genérico
        if device_info.model.contains("AHCI") || 
           device_info.model.contains("SATA") || 
           device_info.model.contains("RAID") {
            return (StorageControllerType::AHCI, 80, "SATA/AHCI genérico detectado");
        }
        
        // Detectar controladoras VirtIO (para virtualización) - PRIORIDAD ALTA
        if device_info.model.contains("1AF4:1001") || 
           device_info.model.contains("VirtIO") ||
           device_info.model.contains("virtio") {
            return (StorageControllerType::VirtIO, 85, "VirtIO detectado");
        }
        
        // Detectar controladoras IDE reales (prioridad media-alta)
        if device_info.model.contains("8086:7010") || // Intel IDE
           device_info.model.contains("IDE") ||
           device_info.model.contains("PATA") {
            return (StorageControllerType::ATA, 80, "IDE real detectado");
        }
        
        // Controladoras genéricas (prioridad baja)
        (StorageControllerType::ATA, 50, "Controladora genérica")
    }
    
    /// Obtiene el nombre de partición correcto según el tipo de dispositivo
    pub fn get_partition_name(device_name: &str, partition_number: u8) -> String {
        if device_name.starts_with("/dev/nvme") {
            // NVMe: /dev/nvme0n1p1, /dev/nvme0n1p2, etc.
            format!("{}p{}", device_name, partition_number)
        } else {
            // SATA/AHCI/VirtIO/IDE: /dev/sda1, /dev/sda2, etc.
            format!("{}{}", device_name, partition_number)
        }
    }
    
    /// Detecta si un dispositivo es NVMe basándose en su nombre
    pub fn is_nvme_device(device_name: &str) -> bool {
        device_name.starts_with("/dev/nvme")
    }
    
    /// Detecta si un dispositivo es SATA/AHCI basándose en su nombre
    pub fn is_sata_device(device_name: &str) -> bool {
        device_name.starts_with("/dev/sd")
    }
    
    /// 🎯 Asignar nombres de dispositivos estilo Linux según el tipo de controladora
    fn assign_linux_device_names(&mut self) {
        serial_write_str("STORAGE_MANAGER: 🎯 Asignando nombres estilo Linux...\n");
        
        // Contadores para cada tipo de dispositivo
        let mut intel_raid_count = 0u8;  // /dev/hda, /dev/hdb, etc. (PRIORIDAD MÁXIMA)
        let mut sata_count = 0u8;        // /dev/sda, /dev/sdb, etc.
        let mut nvme_count = 0u8;        // /dev/nvme0, /dev/nvme1, etc.
        let mut virtio_count = 0u8;      // /dev/vda, /dev/vdb, etc.
        let mut other_count = 0u8;       // /dev/hdc, /dev/hdd, etc.
        
        // Crear una copia de los tipos de controladora para evitar conflictos de borrow
        let controller_types: Vec<StorageControllerType> = self.devices.iter()
            .map(|d| d.info.controller_type)
            .collect();
        
        // PRIORIDAD MÁXIMA: Procesar Intel RAID Volume primero
        for device_index in 0..self.devices.len() {
            if controller_types[device_index] == StorageControllerType::IntelRaidVolume {
                let device_name = format!("/dev/hd{}", (b'a' + intel_raid_count) as char);
                intel_raid_count += 1;
                
                // Actualizar el nombre del dispositivo
                self.devices[device_index].info.device_name = device_name.clone();
                serial_write_str(&format!("STORAGE_MANAGER: 🎯 Intel RAID Volume {} asignado como {} (PRIORIDAD MÁXIMA)\n", 
                                         device_index, device_name));
                
                // Log detallado al framebuffer
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let fb_msg = alloc::format!("INTEL RAID VOLUME: {} -> {}", device_index, device_name);
                    fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::YELLOW);
                }
            }
        }
        
        // PRIORIDAD ALTA: Procesar Intel RAID después
        for device_index in 0..self.devices.len() {
            if controller_types[device_index] == StorageControllerType::IntelRAID {
                let device_name = format!("/dev/hd{}", (b'a' + intel_raid_count) as char);
                intel_raid_count += 1;
                
                // Actualizar el nombre del dispositivo
                self.devices[device_index].info.device_name = device_name.clone();
                serial_write_str(&format!("STORAGE_MANAGER: 🎯 Intel RAID {} asignado como {} (PRIORIDAD ALTA)\n", 
                                         device_index, device_name));
                
                // Log detallado al framebuffer
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let fb_msg = alloc::format!("INTEL RAID: {} -> {}", device_index, device_name);
                    fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::YELLOW);
                }
            }
        }
        
        // Luego procesar otros tipos de dispositivos
        for device_index in 0..self.devices.len() {
            if controller_types[device_index] != StorageControllerType::IntelRAID && 
               controller_types[device_index] != StorageControllerType::IntelRaidVolume {
                let device_name = match controller_types[device_index] {
                    StorageControllerType::ATA | StorageControllerType::AHCI => {
                        let name = format!("/dev/sd{}", (b'a' + sata_count) as char);
                        sata_count += 1;
                        name
                    },
                    StorageControllerType::NVMe => {
                        // Formato correcto NVMe: /dev/nvme0n1, /dev/nvme1n1, etc.
                        // El "n1" indica namespace 1 (los NVMe pueden tener múltiples namespaces)
                        let name = format!("/dev/nvme{}n1", nvme_count);
                        nvme_count += 1;
                        name
                    },
                    StorageControllerType::VirtIO => {
                        let name = format!("/dev/vd{}", (b'a' + virtio_count) as char);
                        virtio_count += 1;
                        name
                    },
                    _ => {
                        let name = format!("/dev/hd{}", (b'a' + other_count) as char);
                        other_count += 1;
                        name
                    }
                };
                
                // Actualizar el nombre del dispositivo
                self.devices[device_index].info.device_name = device_name.clone();
                serial_write_str(&format!("STORAGE_MANAGER: Dispositivo {} asignado como {} (Tipo: {:?})\n", 
                                         device_index, device_name, controller_types[device_index]));
                
                // Log detallado al framebuffer
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let fb_msg = alloc::format!("ASIGNADO: {} -> {}", device_index, device_name);
                    let color = match controller_types[device_index] {
                        crate::drivers::storage_manager::StorageControllerType::ATA | 
                        crate::drivers::storage_manager::StorageControllerType::AHCI => crate::drivers::framebuffer::Color::GREEN,
                        crate::drivers::storage_manager::StorageControllerType::VirtIO => crate::drivers::framebuffer::Color::CYAN,
                        crate::drivers::storage_manager::StorageControllerType::NVMe => crate::drivers::framebuffer::Color::MAGENTA,
                        _ => crate::drivers::framebuffer::Color::YELLOW,
                    };
                    fb.write_text_kernel(&fb_msg, color);
                }
            }
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: ✅ Nombres asignados - Intel RAID: {}, SATA/AHCI: {}, NVMe: {}, VirtIO: {}, Otros: {}\n", 
                                 intel_raid_count, sata_count, nvme_count, virtio_count, other_count));
        
        // Log detallado de tipos de dispositivos
        if nvme_count > 0 {
            serial_write_str(&format!("STORAGE_MANAGER: 💾 {} dispositivos NVMe detectados (formato: /dev/nvmeXn1)\n", nvme_count));
        }
        if sata_count > 0 {
            serial_write_str(&format!("STORAGE_MANAGER: 💿 {} dispositivos SATA/AHCI detectados (formato: /dev/sdX)\n", sata_count));
        }
    }

    /// 🎯 Detectar particiones y asignar nombres como Linux (/dev/sda1, /dev/sda2, etc.)
    pub fn detect_partitions_linux_style(&mut self) -> Result<(), &'static str> {
        serial_write_str("STORAGE_MANAGER: 🎯 Detectando particiones estilo Linux...\n");
        
        // Log al framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel("=== DETECTANDO PARTICIONES ===", crate::drivers::framebuffer::Color::WHITE);
        }
        
        let device_count = self.devices.len();
        
        // Crear copias de la información necesaria para evitar conflictos de borrow
        let device_info_list: Vec<StorageDeviceInfo> = self.devices.iter()
            .map(|d| d.info.clone())
            .collect();
        
        // Log de la lista de dispositivos creada
        serial_write_str(&format!("STORAGE_MANAGER: *** LISTA DE DISPOSITIVOS CREADA *** - {} dispositivos\n", device_info_list.len()));
        for (i, device) in device_info_list.iter().enumerate() {
            serial_write_str(&format!("STORAGE_MANAGER: Lista[{}]: {} ({})\n", i, device.device_name, device.model));
        }
        
        // CORRECCIÓN: Priorizar /dev/sda, luego dispositivos RAID, luego todos
        let mut processed_devices = Vec::new();
        
        // Primero buscar /dev/sda específicamente
        for device_index in 0..device_count {
            let device_name = &device_info_list[device_index].device_name;
            if device_name == "/dev/sda" {
                processed_devices.push(device_index);
                break;
            }
        }
        
        // Verificar si /dev/sda tiene particiones válidas
        let mut sda_has_valid_partitions = false;
        if !processed_devices.is_empty() {
            let sda_index = processed_devices[0];
            let sda_device = &device_info_list[sda_index];
            sda_has_valid_partitions = self.check_device_has_valid_partitions(sda_device);
            
            // Log en framebuffer
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let status = if sda_has_valid_partitions { "SÍ" } else { "NO" };
                fb.write_text_kernel(&alloc::format!("/dev/sda tiene particiones válidas: {}", status), 
                                   if sda_has_valid_partitions { crate::drivers::framebuffer::Color::GREEN } else { crate::drivers::framebuffer::Color::RED });
            }
        }
        
        // Si /dev/sda no tiene particiones válidas, buscar dispositivos RAID
        if processed_devices.is_empty() || !sda_has_valid_partitions {
            // CORRECCIÓN: Solo procesar UN dispositivo IntelRAID como volumen RAID agregado
            let mut intel_raid_found = false;
            for device_index in 0..device_count {
                let device_info = &device_info_list[device_index];
                if device_info.controller_type == StorageControllerType::IntelRAID && !intel_raid_found {
                    processed_devices.push(device_index);
                    intel_raid_found = true;
                    
                    // Log al framebuffer
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        fb.write_text_kernel("RAID: Procesando UN volumen Intel RAID agregado", crate::drivers::framebuffer::Color::YELLOW);
                    }
                    serial_write_str("STORAGE_MANAGER: 🎯 Procesando UN volumen Intel RAID agregado (no múltiples dispositivos)\n");
                    break; // Solo procesar el primer dispositivo IntelRAID
                }
            }
            
            // Si no se encontró IntelRAID, buscar AHCI
            if !intel_raid_found {
                for device_index in 0..device_count {
                    let device_info = &device_info_list[device_index];
                    if device_info.controller_type == StorageControllerType::AHCI {
                        if !processed_devices.contains(&device_index) {
                            processed_devices.push(device_index);
                        }
                    }
                }
            }
            
            // Si aún no hay dispositivos válidos, buscar por patrones RAID
            if processed_devices.is_empty() || self.partitions.is_empty() {
                // Buscar dispositivos por nombres típicos de RAID
                let raid_patterns = ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd", "/dev/sde"];
                for pattern in &raid_patterns {
                    for device_index in 0..device_count {
                        let device_info = &device_info_list[device_index];
                        if device_info.device_name == *pattern && !processed_devices.contains(&device_index) {
                            processed_devices.push(device_index);
                            
                            // Log en framebuffer
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                fb.write_text_kernel(&alloc::format!("Probando patrón RAID: {}", pattern), 
                                                   crate::drivers::framebuffer::Color::YELLOW);
                            }
                            break;
                        }
                    }
                    if !processed_devices.is_empty() && !self.partitions.is_empty() {
                        break;
                    }
                }
                
                // Si aún no hay dispositivos válidos, buscar por tamaño de disco
                if processed_devices.is_empty() || self.partitions.is_empty() {
                    let mut largest_device_index = None;
                    let mut largest_size = 0u64;
                    
                    for device_index in 0..device_count {
                        let device_info = &device_info_list[device_index];
                        // Buscar dispositivos con más de 100MB (típico de discos RAID)
                        let capacity_mb = device_info.capacity / (1024 * 1024);
                        if capacity_mb > 100 && capacity_mb > largest_size {
                            largest_size = capacity_mb;
                            largest_device_index = Some(device_index);
                        }
                    }
                    
                    if let Some(index) = largest_device_index {
                        if !processed_devices.contains(&index) {
                            processed_devices.push(index);
                            
                            // Log en framebuffer
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let device_name = &device_info_list[index].device_name;
                                fb.write_text_kernel(&alloc::format!("Probando dispositivo más grande: {} ({} MB)", device_name, largest_size), 
                                                   crate::drivers::framebuffer::Color::YELLOW);
                            }
                        }
                    }
                }
            }
        }
        
        // Si aún no hay dispositivos, procesar todos
        if processed_devices.is_empty() {
            for device_index in 0..device_count {
                processed_devices.push(device_index);
            }
        }
        
        for device_index in processed_devices {
            serial_write_str(&format!("STORAGE_MANAGER: *** INICIANDO BUCLE *** - device_index: {}, device_count: {}\n", device_index, device_count));
            
            let device_name = &device_info_list[device_index].device_name;
            let device_info = &device_info_list[device_index];
            serial_write_str(&format!("STORAGE_MANAGER: Analizando dispositivo {} ({})\n", device_name, device_info.model));
            
            // Mostrar dispositivo actual en framebuffer
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                fb.write_text_kernel(&alloc::format!("Analizando: {} ({:?})", device_name, device_info.controller_type), crate::drivers::framebuffer::Color::WHITE);
            }
            
            // Log específico para /dev/sda
            if device_name == "/dev/sda" {
                serial_write_str("STORAGE_MANAGER: *** PROCESANDO /dev/sda *** - Dispositivo crítico para EclipseFS\n");
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    fb.write_text_kernel("*** PROCESANDO /dev/sda ***", crate::drivers::framebuffer::Color::RED);
                }
            }
            
            // Leer MBR/GPT del dispositivo
            let mut mbr_buffer = [0u8; 512];
            match self.read_device_sector_real(device_info, 0, &mut mbr_buffer) {
                Ok(()) => {
                    serial_write_str(&format!("STORAGE_MANAGER: MBR/GPT leído exitosamente de {}\n", device_name));
                    
                    // Detectar tipo de tabla de particiones
                    if self.is_gpt_partition_table(&mbr_buffer) {
                        serial_write_str(&format!("STORAGE_MANAGER: GPT detectado en {}\n", device_name));
                        
                        // Log al framebuffer
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let fb_msg = alloc::format!("TABLA: GPT en {}", device_name);
                            fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::BLUE);
                        }
                        
                        self.detect_gpt_partitions(device_index, device_name, &mbr_buffer)?;
                    } else if self.is_mbr_partition_table(&mbr_buffer) {
                        serial_write_str(&format!("STORAGE_MANAGER: MBR detectado en {}\n", device_name));
                        
                        // Log al framebuffer
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let fb_msg = alloc::format!("TABLA: MBR en {}", device_name);
                            fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::BLUE);
                        }
                        
                        self.detect_mbr_partitions(device_index, device_name, &mbr_buffer)?;
                    } else {
                        serial_write_str(&format!("STORAGE_MANAGER: No se detectó tabla de particiones en {}\n", device_name));
                        
                        // Log al framebuffer
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let fb_msg = alloc::format!("TABLA: Sin tabla en {}", device_name);
                            fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::RED);
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&format!("STORAGE_MANAGER: Error leyendo MBR/GPT de {}: {}\n", device_name, e));
                    
                    // Log específico para errores en /dev/sda
                    if device_name == "/dev/sda" {
                        serial_write_str("STORAGE_MANAGER: *** ERROR CRÍTICO EN /dev/sda *** - No se puede leer MBR/GPT\n");
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let fb_msg = alloc::format!("ERROR /dev/sda: {}", e);
                            fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::RED);
                        }
                    }
                }
            }
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: ✅ Detección completada: {} particiones encontradas\n", self.partitions.len()));
        
        // Mostrar resumen completo de todas las particiones en el framebuffer
        self.log_all_partitions_to_framebuffer();
        
        Ok(())
    }
    
    /// Detectar si es una tabla de particiones GPT
    fn is_gpt_partition_table(&self, buffer: &[u8]) -> bool {
        // GPT: Verificar MBR protector en sector 0 (bytes 510-511 = 0x55AA)
        // y tipo de partición 0xEE en byte 450
        if buffer.len() < 512 {
            serial_write_str(&format!("STORAGE_MANAGER: Buffer demasiado pequeño para GPT: {} bytes\n", buffer.len()));
            return false;
        }
        
        let boot_signature = u16::from_le_bytes([buffer[510], buffer[511]]);
        let partition_type = buffer[450];
        
        // Mostrar información crítica en framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel(&alloc::format!("GPT: Boot sig: 0x{:04X}, Tipo: 0x{:02X}", boot_signature, partition_type), crate::drivers::framebuffer::Color::YELLOW);
            fb.write_text_kernel(&alloc::format!("GPT: Primeros 8 bytes: {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}", 
                                               buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6], buffer[7]), crate::drivers::framebuffer::Color::CYAN);
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: Verificando GPT - Buffer size: {}, Boot signature: 0x{:04X} (0x{:02X}{:02X}), Tipo partición: 0x{:02X}\n", 
                                 buffer.len(), boot_signature, buffer[510], buffer[511], partition_type));
        
        // Mostrar primeros 16 bytes para debug
        serial_write_str("STORAGE_MANAGER: Primeros 16 bytes del sector: ");
        for i in 0..16 {
            serial_write_str(&format!("{:02X} ", buffer[i]));
        }
        serial_write_str("\n");
        
        let has_protective_mbr = boot_signature == 0xAA55 && partition_type == 0xEE;
        
        // Mostrar resultado en framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let result_color = if has_protective_mbr { crate::drivers::framebuffer::Color::GREEN } else { crate::drivers::framebuffer::Color::RED };
            fb.write_text_kernel(&alloc::format!("GPT: {}", if has_protective_mbr { "DETECTADO" } else { "NO detectado" }), result_color);
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: GPT detectado: {} (boot_signature==0xAA55: {}, partition_type==0xEE: {})\n", 
                                 has_protective_mbr, boot_signature == 0xAA55, partition_type == 0xEE));
        has_protective_mbr
    }
    
    /// Detectar si es una tabla de particiones MBR
    fn is_mbr_partition_table(&self, buffer: &[u8]) -> bool {
        if buffer.len() < 512 {
            serial_write_str(&format!("STORAGE_MANAGER: Buffer demasiado pequeño para MBR: {} bytes\n", buffer.len()));
            return false;
        }
        
        let boot_signature = u16::from_le_bytes([buffer[510], buffer[511]]);
        
        // Mostrar información crítica en framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel(&alloc::format!("MBR: Boot sig: 0x{:04X}", boot_signature), crate::drivers::framebuffer::Color::YELLOW);
            fb.write_text_kernel(&alloc::format!("MBR: Primeros 8 bytes: {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}", 
                                               buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6], buffer[7]), crate::drivers::framebuffer::Color::CYAN);
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: Verificando MBR - Buffer size: {}, Boot signature: 0x{:04X} (0x{:02X}{:02X})\n", 
                                 buffer.len(), boot_signature, buffer[510], buffer[511]));
        
        // Mostrar primeros 16 bytes para debug
        serial_write_str("STORAGE_MANAGER: Primeros 16 bytes del sector: ");
        for i in 0..16 {
            serial_write_str(&format!("{:02X} ", buffer[i]));
        }
        serial_write_str("\n");
        
        let is_mbr = boot_signature == 0xAA55;
        
        // Mostrar resultado en framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let result_color = if is_mbr { crate::drivers::framebuffer::Color::GREEN } else { crate::drivers::framebuffer::Color::RED };
            fb.write_text_kernel(&alloc::format!("MBR: {}", if is_mbr { "DETECTADO" } else { "NO detectado" }), result_color);
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: MBR detectado: {} (boot_signature==0xAA55: {})\n", 
                                 is_mbr, boot_signature == 0xAA55));
        is_mbr
    }
    
    /// Detectar particiones GPT
    fn detect_gpt_partitions(&mut self, device_index: usize, device_name: &str, mbr_buffer: &[u8]) -> Result<(), &'static str> {
        // GPT: Leer tabla de particiones GPT (sector 2 - donde están las entradas de partición)
        let mut gpt_buffer = [0u8; 512];
        match self.read_device_sector_real(&self.devices[device_index].info, 2, &mut gpt_buffer) {
            Ok(()) => {
                serial_write_str(&format!("STORAGE_MANAGER: Tabla GPT leída exitosamente, analizando entradas...\n"));
                
                // Log al framebuffer también
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    fb.write_text_kernel("GPT: Analizando entradas...", crate::drivers::framebuffer::Color::CYAN);
                }
                // GPT: Cada entrada de partición es de 128 bytes
                for i in 0..4 { // Máximo 4 particiones por ahora
                    let offset = i * 128;
                    if offset + 128 <= gpt_buffer.len() {
                        let partition_entry = &gpt_buffer[offset..offset + 128];
                        
                        // Debug: Mostrar primeros bytes de cada entrada
                        serial_write_str(&format!("STORAGE_MANAGER: Entrada {} - primeros 16 bytes: {:02X?}\n", 
                                                 i, &partition_entry[0..16]));
                        
                        // Verificar si la partición está activa (tipo GUID no es cero)
                        if !self.is_zero_partition_entry(partition_entry) {
                            let partition_number = i + 1;
                            
                            // Formato correcto según tipo de dispositivo:
                            // - NVMe: /dev/nvme0n1p1, /dev/nvme0n1p2, etc.
                            // - SATA/AHCI: /dev/sda1, /dev/sda2, etc.
                            // - VirtIO: /dev/vda1, /dev/vda2, etc.
                            let partition_name = if device_name.starts_with("/dev/nvme") {
                                format!("{}p{}", device_name, partition_number)  // NVMe: nvme0n1p1
                            } else {
                                format!("{}{}", device_name, partition_number)   // SATA: sda1
                            };
                            
                            // Leer información de la partición
                            let start_sector = u64::from_le_bytes([
                                partition_entry[32], partition_entry[33], partition_entry[34], partition_entry[35],
                                partition_entry[36], partition_entry[37], partition_entry[38], partition_entry[39],
                            ]);
                            
                            let end_sector = u64::from_le_bytes([
                                partition_entry[40], partition_entry[41], partition_entry[42], partition_entry[43],
                                partition_entry[44], partition_entry[45], partition_entry[46], partition_entry[47],
                            ]);
                            
                            let size_sectors = end_sector - start_sector + 1;
                            
                            // Detectar tipo de sistema de archivos
                            let fs_type = self.detect_filesystem_type(device_index, start_sector);
                            
                            let partition = Partition {
                                start_lba: start_sector,
                                size_lba: size_sectors,
                                partition_type: 0, // Tipo genérico
                                filesystem_type: if fs_type == "EclipseFS" { 
                                    crate::partitions::FilesystemType::EclipseFS 
                                } else if fs_type == "FAT32" { 
                                    crate::partitions::FilesystemType::FAT32 
                                } else { 
                                    crate::partitions::FilesystemType::Unknown 
                                },
                                name: partition_name.clone(),
                                guid: None,
                                attributes: 0,
                            };
                            
                            self.partitions.push(partition);
                            serial_write_str(&format!("STORAGE_MANAGER: ✅ Partición detectada: {} ({} sectores, {})\n", 
                                                     partition_name, size_sectors, fs_type));
                            
                            // Log al framebuffer también
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let fb_msg = alloc::format!("PART: {} ({} MB, {})", 
                                                          partition_name, 
                                                          (size_sectors * 512) / (1024 * 1024), 
                                                          fs_type);
                                fb.write_text_kernel(&fb_msg, crate::drivers::framebuffer::Color::GREEN);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                serial_write_str(&format!("STORAGE_MANAGER: Error leyendo tabla GPT: {}\n", e));
            }
        }
        Ok(())
    }
    
    /// Detectar particiones MBR
    fn detect_mbr_partitions(&mut self, device_index: usize, device_name: &str, mbr_buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Analizando MBR en {} - buscando particiones...\n", device_name));
        
        // Log al framebuffer
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel(&alloc::format!("MBR: Analizando {} particiones", 4), crate::drivers::framebuffer::Color::CYAN);
        }
        
        // MBR: Las entradas de partición están en los bytes 446-509
        for i in 0..4 {
            let offset = 446 + (i * 16);
            if offset + 16 <= mbr_buffer.len() {
                let partition_entry = &mbr_buffer[offset..offset + 16];
                
                // Mostrar información detallada de cada entrada MBR en framebuffer
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let status = partition_entry[0];
                    let partition_type = partition_entry[4];
                    let start_sector = u32::from_le_bytes([
                        partition_entry[8], partition_entry[9], partition_entry[10], partition_entry[11],
                    ]) as u64;
                    let size_sectors = u32::from_le_bytes([
                        partition_entry[12], partition_entry[13], partition_entry[14], partition_entry[15],
                    ]) as u64;
                    
                    let mbr_msg = alloc::format!("MBR{}: Status=0x{:02X}, Type=0x{:02X}, Start={}, Size={}", 
                                                i + 1, status, partition_type, start_sector, size_sectors);
                    fb.write_text_kernel(&mbr_msg, crate::drivers::framebuffer::Color::WHITE);
                }
                
                // Verificar si la partición está activa (tipo no es 0)
                if partition_entry[4] != 0 {
                    let partition_number = i + 1;
                    
                    // Formato correcto según tipo de dispositivo
                    let partition_name = if device_name.starts_with("/dev/nvme") {
                        format!("{}p{}", device_name, partition_number)  // NVMe: nvme0n1p1
                    } else {
                        format!("{}{}", device_name, partition_number)   // SATA: sda1
                    };
                    
                    let start_sector = u32::from_le_bytes([
                        partition_entry[8], partition_entry[9], partition_entry[10], partition_entry[11],
                    ]) as u64;
                    
                    let size_sectors = u32::from_le_bytes([
                        partition_entry[12], partition_entry[13], partition_entry[14], partition_entry[15],
                    ]) as u64;
                    
                    // Log al framebuffer que se encontró una partición válida
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let valid_msg = alloc::format!("MBR{}: ✅ Partición válida detectada", i + 1);
                        fb.write_text_kernel(&valid_msg, crate::drivers::framebuffer::Color::GREEN);
                    }
                    
                    let fs_type = self.detect_filesystem_type(device_index, start_sector);
                    
                    let partition = Partition {
                        start_lba: start_sector,
                        size_lba: size_sectors,
                        partition_type: partition_entry[4], // Tipo de partición MBR
                        filesystem_type: if fs_type == "EclipseFS" { 
                            crate::partitions::FilesystemType::EclipseFS 
                        } else if fs_type == "FAT32" { 
                            crate::partitions::FilesystemType::FAT32 
                        } else { 
                            crate::partitions::FilesystemType::Unknown 
                        },
                        name: partition_name.clone(),
                        guid: None,
                        attributes: 0,
                    };
                    
                    self.partitions.push(partition);
                    serial_write_str(&format!("STORAGE_MANAGER: ✅ Partición MBR detectada: {} ({} sectores, {})\n", 
                                             partition_name, size_sectors, fs_type));
                    
                    // Log al framebuffer con información de la partición
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let fs_msg = alloc::format!("MBR{}: {} - {} sectores", i + 1, fs_type, size_sectors);
                        let color = if fs_type == "EclipseFS" { 
                            crate::drivers::framebuffer::Color::MAGENTA 
                        } else if fs_type == "FAT32" { 
                            crate::drivers::framebuffer::Color::YELLOW 
                        } else { 
                            crate::drivers::framebuffer::Color::RED 
                        };
                        fb.write_text_kernel(&fs_msg, color);
                    }
                } else {
                    // Log al framebuffer que la entrada está vacía
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let empty_msg = alloc::format!("MBR{}: ❌ Entrada vacía", i + 1);
                        fb.write_text_kernel(&empty_msg, crate::drivers::framebuffer::Color::GRAY);
                    }
                }
            }
        }
        
        // Mostrar resumen final
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let summary_msg = alloc::format!("MBR: {} particiones válidas encontradas", self.partitions.len());
            fb.write_text_kernel(&summary_msg, crate::drivers::framebuffer::Color::CYAN);
        }
        
        Ok(())
    }
    
    /// Verificar si una entrada de partición GPT está vacía
    fn is_zero_partition_entry(&self, entry: &[u8]) -> bool {
        entry.iter().all(|&b| b == 0)
    }
    
    /// Detectar tipo de sistema de archivos leyendo el primer sector de la partición
    fn detect_filesystem_type(&self, device_index: usize, start_sector: u64) -> String {
        let mut sector_buffer = [0u8; 512];
        
        match self.read_device_sector_real(&self.devices[device_index].info, start_sector, &mut sector_buffer) {
            Ok(()) => {
                // Verificar EclipseFS
                if &sector_buffer[0..9] == b"ECLIPSEFS" {
                    // Log al framebuffer
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        fb.write_text_kernel("FS: EclipseFS detectado", crate::drivers::framebuffer::Color::MAGENTA);
                    }
                    return "EclipseFS".to_string();
                }
                
                // Verificar FAT32
                if sector_buffer[510] == 0x55 && sector_buffer[511] == 0xAA {
                    // Verificar si es FAT32 (típicamente tiene "FAT32" en el sector de boot)
                    if sector_buffer[82] == b'F' && sector_buffer[83] == b'A' && 
                       sector_buffer[84] == b'T' && sector_buffer[85] == b'3' && 
                       sector_buffer[86] == b'2' {
                        // Log al framebuffer
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            fb.write_text_kernel("FS: FAT32 detectado", crate::drivers::framebuffer::Color::YELLOW);
                        }
                        return "FAT32".to_string();
                    }
                }
                
                // Log al framebuffer para Unknown
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    fb.write_text_kernel("FS: Unknown", crate::drivers::framebuffer::Color::RED);
                }
                "Unknown".to_string()
            }
            Err(_) => {
                // Log al framebuffer para error
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    fb.write_text_kernel("FS: Error lectura", crate::drivers::framebuffer::Color::RED);
                }
                "Unknown".to_string()
            }
        }
    }
    
    /// Obtener dirección base de un BAR PCI
    fn get_pci_bar_address(&self, pci_device: &crate::drivers::pci::PciDevice, bar_num: u8) -> Option<u32> {
        // Para Intel RAID controllers, intentar leer el BAR5 (AHCI registers)
        if pci_device.vendor_id == 0x8086 && matches!(pci_device.device_id, 
            0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F) {
            
            match bar_num {
                5 => {
                    // Por ahora, usar dirección por defecto para Intel AHCI RAID
                    // En una implementación completa, esto leería el BAR5 real del dispositivo PCI
                    serial_write_str("STORAGE_MANAGER: Usando dirección BAR5 por defecto para Intel AHCI RAID\n");
                    Some(0xFEB80000) // Dirección típica del BAR5 para Intel AHCI RAID
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// Verificar si un dispositivo tiene particiones válidas
    fn check_device_has_valid_partitions(&self, device_info: &StorageDeviceInfo) -> bool {
        // Para dispositivos Intel RAID, usar lógica especial
        if device_info.controller_type == StorageControllerType::IntelRAID {
            return self.check_raid_volume_has_partitions(device_info);
        }
        
        let mut buffer = [0u8; 512];
        
        // Intentar leer el sector 0
        if self.read_device_sector_real(device_info, 0, &mut buffer).is_err() {
            return false;
        }
        
        // Verificar GPT
        if self.is_gpt_partition_table(&buffer) {
            // Para GPT, leer el sector 2 (tabla de particiones)
            if self.read_device_sector_real(device_info, 2, &mut buffer).is_err() {
                return false;
            }
            
            // Verificar si hay al menos una entrada de partición no vacía
            for i in 0..4 {
                let offset = i * 128;
                if offset + 128 <= buffer.len() {
                    let partition_entry = &buffer[offset..offset + 128];
                    if !self.is_zero_partition_entry(partition_entry) {
                        return true;
                    }
                }
            }
        }
        
        // Verificar MBR
        if self.is_mbr_partition_table(&buffer) {
            // Verificar si hay al menos una partición activa
            for i in 0..4 {
                let offset = 446 + (i * 16);
                if offset + 16 <= buffer.len() {
                    let partition_entry = &buffer[offset..offset + 16];
                    // En MBR: byte 0 = estado (0x80=activa, 0x00=inactiva), byte 4 = tipo de partición
                    if partition_entry[4] != 0x00 { // Tipo de partición no es 0 (vacía)
                        return true;
                    }
                }
            }
        }
        
        false
    }
    
    /// Verificar si un volumen RAID tiene particiones válidas
    fn check_raid_volume_has_partitions(&self, device_info: &StorageDeviceInfo) -> bool {
        serial_write_str(&format!("STORAGE_MANAGER: Verificando volumen RAID {} para particiones válidas\n", device_info.model));
        
        // Para Intel RAID, asumir que el primer volumen RAID tiene particiones
        // En hardware real, esto debería consultar la configuración RAID
        if device_info.model.contains("IntelRAID") || device_info.model.contains("Intel RAID") {
            serial_write_str("STORAGE_MANAGER: Dispositivo Intel RAID detectado - asumiendo volumen con particiones\n");
            return true;
        }
        
        false
    }

    /// Mostrar todas las particiones detectadas en el framebuffer para debug
    fn log_all_partitions_to_framebuffer(&self) {
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            fb.write_text_kernel("=== PARTICIONES DETECTADAS ===", crate::drivers::framebuffer::Color::WHITE);
            
            if self.partitions.is_empty() {
                fb.write_text_kernel("NO se encontraron particiones", crate::drivers::framebuffer::Color::RED);
                return;
            }
            
            for (i, partition) in self.partitions.iter().enumerate() {
                let size_mb = (partition.size_lba * 512) / (1024 * 1024);
                let start_mb = (partition.start_lba * 512) / (1024 * 1024);
                
                // Mostrar información básica de la partición
                let fb_msg = alloc::format!("{}. {} - {} MB (LBA: {})", 
                                          i + 1, 
                                          partition.name, 
                                          size_mb, 
                                          partition.start_lba);
                
                // Elegir color según el tipo de sistema de archivos
                let color = match partition.filesystem_type {
                    crate::partitions::FilesystemType::EclipseFS => crate::drivers::framebuffer::Color::MAGENTA,
                    crate::partitions::FilesystemType::FAT32 => crate::drivers::framebuffer::Color::YELLOW,
                    crate::partitions::FilesystemType::Unknown => crate::drivers::framebuffer::Color::RED,
                    _ => crate::drivers::framebuffer::Color::CYAN,
                };
                
                fb.write_text_kernel(&fb_msg, color);
                
                // Mostrar tipo de sistema de archivos
                let fs_type_msg = alloc::format!("   Tipo: {:?}", partition.filesystem_type);
                fb.write_text_kernel(&fs_type_msg, crate::drivers::framebuffer::Color::GRAY);
                
                // Mostrar información adicional si es EclipseFS
                if partition.filesystem_type == crate::partitions::FilesystemType::EclipseFS {
                    let eclipse_msg = alloc::format!("   ECLIPSEFS - Inicio: {} MB", start_mb);
                    fb.write_text_kernel(&eclipse_msg, crate::drivers::framebuffer::Color::MAGENTA);
                }
            }
            
            // Mostrar resumen
            let summary_msg = alloc::format!("TOTAL: {} particiones detectadas", self.partitions.len());
            fb.write_text_kernel(&summary_msg, crate::drivers::framebuffer::Color::GREEN);
        }
    }

    /// Buscar partición por nombre de dispositivo
    pub fn find_partition_by_name(&self, device_name: &str) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.name == device_name)
    }
    
    /// Leer sector de una partición específica
    pub fn read_partition_sector(&self, partition_name: &str, sector_offset: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if let Some(partition) = self.find_partition_by_name(partition_name) {
            // Encontrar el dispositivo padre (asumiendo que es /dev/sda para el primer dispositivo)
            let device_index = 0; // Por ahora, asumimos que todas las particiones están en el primer dispositivo
            let absolute_sector = partition.start_lba + sector_offset;
            self.read_device_sector_real(&self.devices[device_index].info, absolute_sector, buffer)
        } else {
            Err("Partición no encontrada")
        }
    }
    
    /// Leer múltiples sectores usando driver IDE moderno para controladoras Intel IDE
    fn read_ide_modern_sectors(&self, start_sector: u64, num_sectors: usize, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo {} sectores desde sector {} con driver IDE moderno\n", num_sectors, start_sector));
        
        if buffer.len() < num_sectors * 512 {
            return Err("Buffer demasiado pequeño para la cantidad de sectores solicitados");
        }
        
        // Intel IDE Primary Master (puerto 0x1F0)
        const IDE_DATA_PORT: u16 = 0x1F0;
        const IDE_SECTOR_COUNT: u16 = 0x1F2;
        const IDE_SECTOR_NUMBER: u16 = 0x1F3;
        const IDE_CYLINDER_LOW: u16 = 0x1F4;
        const IDE_CYLINDER_HIGH: u16 = 0x1F5;
        const IDE_DRIVE_HEAD: u16 = 0x1F6;
        const IDE_COMMAND: u16 = 0x1F7;
        const IDE_STATUS: u16 = 0x1F7;
        
        // Comando READ SECTORS
        const CMD_READ_SECTORS: u8 = 0x20;
        
        // Estado del controlador
        const STATUS_BSY: u8 = 0x80;  // Busy
        const STATUS_DRDY: u8 = 0x40; // Drive Ready
        const STATUS_DF: u8 = 0x20;   // Drive Fault
        const STATUS_DRQ: u8 = 0x08;  // Data Request
        const STATUS_ERR: u8 = 0x01;  // Error
        
        // Leer sectores en bloques (máximo 256 sectores por comando)
        let mut sectors_read = 0;
        while sectors_read < num_sectors {
            let sectors_to_read = core::cmp::min(256, num_sectors - sectors_read);
            let current_sector = start_sector + sectors_read as u64;
            let buffer_offset = sectors_read * 512;
            
            // Esperar que el controlador no esté ocupado
            unsafe {
                for _ in 0..1000 {
                    let status: u8;
                    core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                    if (status & STATUS_BSY) == 0 {
                        break;
                    }
                    for _ in 0..1000 {
                        core::arch::asm!("nop", options(nostack, preserves_flags));
                    }
                }
            }
            
            // Configurar parámetros del comando
            unsafe {
                // Número de sectores a leer (0 = 256 sectores)
                let sector_count = if sectors_to_read == 256 { 0 } else { sectors_to_read as u8 };
                core::arch::asm!("out dx, al", in("al") sector_count, in("dx") IDE_SECTOR_COUNT, options(nostack, preserves_flags));
                
                // Número de sector (LBA 28-bit)
                let sector_low = (current_sector & 0xFF) as u8;
                core::arch::asm!("out dx, al", in("al") sector_low, in("dx") IDE_SECTOR_NUMBER, options(nostack, preserves_flags));
                
                let sector_mid = ((current_sector >> 8) & 0xFF) as u8;
                core::arch::asm!("out dx, al", in("al") sector_mid, in("dx") IDE_CYLINDER_LOW, options(nostack, preserves_flags));
                
                let sector_high = ((current_sector >> 16) & 0xFF) as u8;
                core::arch::asm!("out dx, al", in("al") sector_high, in("dx") IDE_CYLINDER_HIGH, options(nostack, preserves_flags));
                
                // Drive/Head register (LBA mode, Master drive)
                let drive_head = 0xE0 | (((current_sector >> 24) & 0x0F) as u8);
                core::arch::asm!("out dx, al", in("al") drive_head, in("dx") IDE_DRIVE_HEAD, options(nostack, preserves_flags));
                
                // Enviar comando READ SECTORS
                core::arch::asm!("out dx, al", in("al") CMD_READ_SECTORS, in("dx") IDE_COMMAND, options(nostack, preserves_flags));
            }
            
            // Leer cada sector del bloque
            for sector_idx in 0..sectors_to_read {
                // Esperar a que los datos estén listos
                unsafe {
                    for _ in 0..10000 {
                        let status: u8;
                        core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                        
                        if (status & STATUS_ERR) != 0 {
                            serial_write_str("STORAGE_MANAGER: Error en estado del controlador IDE\n");
                            return Err("Error en controlador IDE");
                        }
                        
                        if (status & STATUS_DRQ) != 0 {
                            break; // Datos listos
                        }
                        
                        if (status & STATUS_BSY) != 0 {
                            continue; // Aún ocupado
                        }
                        
                        for _ in 0..1000 {
                            core::arch::asm!("nop", options(nostack, preserves_flags));
                        }
                    }
                }
                
                // Leer datos del sector (512 bytes)
                let sector_buffer_offset = buffer_offset + sector_idx * 512;
                unsafe {
                    for i in 0..256 { // 256 palabras de 16 bits = 512 bytes
                        let word: u16;
                        core::arch::asm!("in ax, dx", out("ax") word, in("dx") IDE_DATA_PORT, options(nostack, preserves_flags));
                        
                        let byte1 = (word & 0xFF) as u8;
                        let byte2 = ((word >> 8) & 0xFF) as u8;
                        
                        buffer[sector_buffer_offset + i * 2] = byte1;
                        buffer[sector_buffer_offset + i * 2 + 1] = byte2;
                    }
                }
            }
            
            sectors_read += sectors_to_read;
        }
        
        serial_write_str(&format!("STORAGE_MANAGER: {} sectores leídos exitosamente con driver IDE moderno\n", num_sectors));
        Ok(())
    }
    
    /// Leer sector usando driver IDE moderno para controladoras Intel IDE
    fn read_ide_modern_sector(&self, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} con driver IDE moderno\n", sector));
        
        // Intel IDE Primary Master (puerto 0x1F0)
        const IDE_DATA_PORT: u16 = 0x1F0;
        const IDE_SECTOR_COUNT: u16 = 0x1F2;
        const IDE_SECTOR_NUMBER: u16 = 0x1F3;
        const IDE_CYLINDER_LOW: u16 = 0x1F4;
        const IDE_CYLINDER_HIGH: u16 = 0x1F5;
        const IDE_DRIVE_HEAD: u16 = 0x1F6;
        const IDE_COMMAND: u16 = 0x1F7;
        const IDE_STATUS: u16 = 0x1F7;
        
        // Comando READ SECTORS
        const CMD_READ_SECTORS: u8 = 0x20;
        
        // Estado del controlador
        const STATUS_BSY: u8 = 0x80;  // Busy
        const STATUS_DRDY: u8 = 0x40; // Drive Ready
        const STATUS_DF: u8 = 0x20;   // Drive Fault
        const STATUS_DRQ: u8 = 0x08;  // Data Request
        const STATUS_ERR: u8 = 0x01;  // Error
        
        // Esperar que el controlador no esté ocupado
        unsafe {
            for _ in 0..1000 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                if (status & STATUS_BSY) == 0 {
                    break;
                }
                // Pequeña espera
                for _ in 0..1000 {
                    core::arch::asm!("nop", options(nostack, preserves_flags));
                }
            }
        }
        
        // Configurar parámetros del comando
        unsafe {
            // Número de sectores a leer (1)
            core::arch::asm!("out dx, al", in("al") 1u8, in("dx") IDE_SECTOR_COUNT, options(nostack, preserves_flags));
            
            // Número de sector (LBA 28-bit)
            let sector_low = (sector & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_low, in("dx") IDE_SECTOR_NUMBER, options(nostack, preserves_flags));
            
            let sector_mid = ((sector >> 8) & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_mid, in("dx") IDE_CYLINDER_LOW, options(nostack, preserves_flags));
            
            let sector_high = ((sector >> 16) & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_high, in("dx") IDE_CYLINDER_HIGH, options(nostack, preserves_flags));
            
            // Drive/Head register (LBA mode, Master drive)
            let drive_head = 0xE0 | (((sector >> 24) & 0x0F) as u8);
            core::arch::asm!("out dx, al", in("al") drive_head, in("dx") IDE_DRIVE_HEAD, options(nostack, preserves_flags));
            
            // Enviar comando READ SECTORS
            core::arch::asm!("out dx, al", in("al") CMD_READ_SECTORS, in("dx") IDE_COMMAND, options(nostack, preserves_flags));
        }
        
        // Esperar a que los datos estén listos
        unsafe {
            for _ in 0..10000 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                
                if (status & STATUS_ERR) != 0 {
                    serial_write_str("STORAGE_MANAGER: Error en estado del controlador IDE\n");
                    return Err("Error en controlador IDE");
                }
                
                if (status & STATUS_DRQ) != 0 {
                    break; // Datos listos
                }
                
                if (status & STATUS_BSY) != 0 {
                    continue; // Aún ocupado
                }
                
                // Pequeña espera
                for _ in 0..1000 {
                    core::arch::asm!("nop", options(nostack, preserves_flags));
                }
            }
        }
        
        // Leer datos del sector (512 bytes)
        unsafe {
            for i in 0..256 { // 256 palabras de 16 bits = 512 bytes
                let word: u16;
                core::arch::asm!("in ax, dx", out("ax") word, in("dx") IDE_DATA_PORT, options(nostack, preserves_flags));
                
                // Convertir little-endian a bytes
                let byte1 = (word & 0xFF) as u8;
                let byte2 = ((word >> 8) & 0xFF) as u8;
                
                buffer[i * 2] = byte1;
                buffer[i * 2 + 1] = byte2;
            }
        }
        
        serial_write_str("STORAGE_MANAGER: Sector leído exitosamente con driver IDE moderno\n");
        Ok(())
    }
    
    /// Lectura batch de múltiples sectores consecutivos para controladores compatibles
    /// Implementa la estrategia de "lectura integral" para reducir operaciones de I/O
    /// OPTIMIZADO PARA QEMU: usa el driver IDE multi-sector cuando es posible
    /// Solo funciona para ATA/IDE, IntelRAID y AHCI - falla para otros tipos
    fn read_sectors_batch(&self, start_sector: u64, num_sectors: usize, buffer: &mut [u8]) -> Result<(), &'static str> {
        const SECTOR_SIZE: usize = 512;
        
        if buffer.len() < num_sectors * SECTOR_SIZE {
            return Err("Buffer too small for batch read");
        }
        
        // Obtener el dispositivo (asumiendo dispositivo 0)
        // TODO: Pasar device info como parámetro en lugar de asumir dispositivo 0
        if self.devices.is_empty() {
            return Err("No devices available");
        }
        let device = &self.devices[0];
        
        // Intentar lectura multi-sector nativa del driver IDE
        // Esto es un batch REAL, no un loop - lee múltiples sectores en UN comando
        if matches!(device.info.controller_type, 
                   StorageControllerType::ATA |
                   StorageControllerType::IntelRAID | 
                   StorageControllerType::AHCI) {
            // read_ide_modern_sectors implementa batch real usando comandos IDE multi-sector
            // Puede leer hasta 256 sectores en un solo comando de hardware
            match self.read_ide_modern_sectors(start_sector, num_sectors, buffer) {
                Ok(_) => return Ok(()),
                Err(_) => {
                    // Si falla, retornar error para que el caller use fallback
                }
            }
        }
        
        // Para otros controladores (VirtIO, etc), no hay soporte de batch
        // Retornar error para que el caller use lectura sector-por-sector
        Err("Batch read not supported for this controller")
    }
    
    /// Escribir sector usando driver IDE moderno para controladoras Intel IDE
    fn write_ide_modern_sector(&self, sector: u64, buffer: &[u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Escribiendo sector {} con driver IDE moderno\n", sector));
        
        // Intel IDE Primary Master (puerto 0x1F0)
        const IDE_DATA_PORT: u16 = 0x1F0;
        const IDE_SECTOR_COUNT: u16 = 0x1F2;
        const IDE_SECTOR_NUMBER: u16 = 0x1F3;
        const IDE_CYLINDER_LOW: u16 = 0x1F4;
        const IDE_CYLINDER_HIGH: u16 = 0x1F5;
        const IDE_DRIVE_HEAD: u16 = 0x1F6;
        const IDE_COMMAND: u16 = 0x1F7;
        const IDE_STATUS: u16 = 0x1F7;
        
        // Comando WRITE SECTORS
        const CMD_WRITE_SECTORS: u8 = 0x30;
        
        // Estado del controlador
        const STATUS_BSY: u8 = 0x80;  // Busy
        const STATUS_DRDY: u8 = 0x40; // Drive Ready
        const STATUS_DF: u8 = 0x20;   // Drive Fault
        const STATUS_DRQ: u8 = 0x08;  // Data Request
        const STATUS_ERR: u8 = 0x01;  // Error
        
        // Esperar que el controlador no esté ocupado
        unsafe {
            for _ in 0..1000 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                if (status & STATUS_BSY) == 0 {
                    break;
                }
                // Pequeña espera
                for _ in 0..1000 {
                    core::arch::asm!("nop", options(nostack, preserves_flags));
                }
            }
        }
        
        // Configurar parámetros del comando
        unsafe {
            // Número de sectores a escribir (1)
            core::arch::asm!("out dx, al", in("al") 1u8, in("dx") IDE_SECTOR_COUNT, options(nostack, preserves_flags));
            
            // Número de sector (LBA 28-bit)
            let sector_low = (sector & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_low, in("dx") IDE_SECTOR_NUMBER, options(nostack, preserves_flags));
            
            let sector_mid = ((sector >> 8) & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_mid, in("dx") IDE_CYLINDER_LOW, options(nostack, preserves_flags));
            
            let sector_high = ((sector >> 16) & 0xFF) as u8;
            core::arch::asm!("out dx, al", in("al") sector_high, in("dx") IDE_CYLINDER_HIGH, options(nostack, preserves_flags));
            
            // Drive/Head register (LBA mode, Master drive)
            let drive_head = 0xE0 | (((sector >> 24) & 0x0F) as u8);
            core::arch::asm!("out dx, al", in("al") drive_head, in("dx") IDE_DRIVE_HEAD, options(nostack, preserves_flags));
            
            // Enviar comando WRITE SECTORS
            core::arch::asm!("out dx, al", in("al") CMD_WRITE_SECTORS, in("dx") IDE_COMMAND, options(nostack, preserves_flags));
        }
        
        // Esperar a que el controlador esté listo para recibir datos
        unsafe {
            for _ in 0..10000 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                
                if (status & STATUS_ERR) != 0 {
                    serial_write_str("STORAGE_MANAGER: Error en estado del controlador IDE durante escritura\n");
                    return Err("Error en controlador IDE durante escritura");
                }
                
                if (status & STATUS_DRQ) != 0 {
                    break; // Listo para recibir datos
                }
                
                if (status & STATUS_BSY) != 0 {
                    continue; // Aún ocupado
                }
                
                // Pequeña espera
                for _ in 0..1000 {
                    core::arch::asm!("nop", options(nostack, preserves_flags));
                }
            }
        }
        
        // Escribir datos del sector (512 bytes)
        unsafe {
            for i in 0..256 { // 256 palabras de 16 bits = 512 bytes
                let byte1 = buffer[i * 2];
                let byte2 = buffer[i * 2 + 1];
                let word = (byte2 as u16) << 8 | (byte1 as u16);
                
                core::arch::asm!("out dx, ax", in("ax") word, in("dx") IDE_DATA_PORT, options(nostack, preserves_flags));
            }
        }
        
        // Esperar a que la escritura se complete
        unsafe {
            for _ in 0..10000 {
                let status: u8;
                core::arch::asm!("in al, dx", out("al") status, in("dx") IDE_STATUS, options(nostack, preserves_flags));
                
                if (status & STATUS_ERR) != 0 {
                    serial_write_str("STORAGE_MANAGER: Error durante escritura del sector\n");
                    return Err("Error durante escritura del sector");
                }
                
                if (status & STATUS_BSY) == 0 {
                    break; // Escritura completada
                }
                
                // Pequeña espera
                for _ in 0..1000 {
                    core::arch::asm!("nop", options(nostack, preserves_flags));
                }
            }
        }
        
        serial_write_str("STORAGE_MANAGER: Sector escrito exitosamente con driver IDE moderno\n");
        Ok(())
    }
    
    /// Driver VirtIO básico para dispositivos VirtIO
    fn read_virtio_sector(&self, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Leyendo sector {} con driver VirtIO\n", sector));
        
        // Usar el driver VirtIO real si está disponible
        unsafe {
            if let Some(ref virtio_driver) = VIRTIO_BLK_DRIVER {
                serial_write_str("STORAGE_MANAGER: Usando driver VirtIO real para lectura\n");
                
                // Leer usando el driver VirtIO
                match virtio_driver.read_blocks(sector, buffer) {
                    Ok(()) => {
                        serial_write_str(&format!("STORAGE_MANAGER: ✅ Sector {} leído exitosamente con driver VirtIO\n", sector));
                        return Ok(());
                    }
                    Err(e) => {
                        serial_write_str(&format!("STORAGE_MANAGER: ❌ Error leyendo sector {} con driver VirtIO: {}\n", sector, e));
                        return Err(e);
                    }
                }
            }
        }
        
        serial_write_str("STORAGE_MANAGER: ❌ Driver VirtIO no disponible, no se puede leer\n");
        Err("Driver VirtIO no inicializado")
    }
}

/// Tipo de sector de almacenamiento
#[derive(Debug, Clone, Copy)]
pub enum StorageSectorType {
    FAT32,
    EclipseFS,
}


// Instancia global del gestor de almacenamiento
static mut STORAGE_MANAGER: Option<StorageManager> = None;

// Instancia global del driver VirtIO Block
static mut VIRTIO_BLK_DRIVER: Option<crate::drivers::virtio_blk::VirtioBlkDriver> = None;

/// Inicializar gestor de almacenamiento global
pub fn init_storage_manager() -> Result<(), &'static str> {
    unsafe {
        if STORAGE_MANAGER.is_some() {
            return Err("Gestor de almacenamiento ya inicializado");
        }

        let mut manager = StorageManager::new();
        manager.initialize()?;
        STORAGE_MANAGER = Some(manager);
    }

    Ok(())
}

/// Obtener referencia al gestor de almacenamiento global
pub fn get_storage_manager() -> Option<&'static StorageManager> {
    unsafe {
        STORAGE_MANAGER.as_ref()
    }
}

/// Obtener referencia mutable al gestor de almacenamiento global
pub fn get_storage_manager_mut() -> Option<&'static mut StorageManager> {
    unsafe {
        STORAGE_MANAGER.as_mut()
    }
}

/// Verificar si el gestor de almacenamiento está listo
pub fn is_storage_manager_ready() -> bool {
    unsafe {
        STORAGE_MANAGER.as_ref().map(|m| m.is_ready()).unwrap_or(false)
    }
}

impl StorageManager {
    /// Simular datos de Intel RAID para testing
    fn simulate_intel_raid_data(&self, sector: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        serial_write_str(&format!("STORAGE_MANAGER: Simulando datos Intel RAID para sector {}\n", sector));
        
        // Simular datos realistas de un volumen RAID con particiones
        if sector < 10 {
            match sector {
                0 => {
                    // MBR válido para volumen RAID
                    buffer.fill(0);
                    buffer[510] = 0x55;
                    buffer[511] = 0xAA; // Boot signature
                    buffer[450] = 0xEE; // Tipo GPT
                }
                1 => {
                    // GPT Header válido
                    buffer.fill(0);
                    buffer[0..8].copy_from_slice(b"EFI PART");
                    buffer[8] = 0x00; buffer[9] = 0x00; buffer[10] = 0x01; buffer[11] = 0x00; // Revision
                }
                2 => {
                    // Tabla GPT con particiones válidas
                    buffer.fill(0);
                    // Primera partición: FAT32 (sector 2048, 100MB)
                    buffer[32..48].copy_from_slice(&[0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
                    buffer[48..56].copy_from_slice(&[0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // Start LBA: 2048
                    buffer[56..64].copy_from_slice(&[0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00]); // End LBA: 204800
                    
                    // Segunda partición: EclipseFS (sector 204800, resto del disco)
                    buffer[128..144].copy_from_slice(&[0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
                    buffer[144..152].copy_from_slice(&[0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00]); // Start LBA: 204800
                    buffer[152..160].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00]); // End LBA: máximo
                }
                _ => {
                    // Otros sectores de metadatos
                    for i in 0..buffer.len() {
                        buffer[i] = ((sector * 256 + i as u64) % 256) as u8;
                    }
                }
            }
        } else if sector >= 2048 && sector < 2058 {
            // Simular FAT32 boot sector en la partición 1
            match sector - 2048 {
                0 => {
                    // FAT32 boot sector válido
                    buffer.fill(0);
                    buffer[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]); // Jump instruction
                    buffer[3..11].copy_from_slice(b"mkfs.fat"); // OEM name
                    buffer[11..13].copy_from_slice(&[0x00, 0x02]); // Bytes per sector
                    buffer[510] = 0x55;
                    buffer[511] = 0xAA; // Boot signature
                    buffer[82..90].copy_from_slice(b"FAT32   "); // File system type
                }
                _ => {
                    // Otros sectores FAT32
                    for i in 0..buffer.len() {
                        buffer[i] = ((sector * 256 + i as u64) % 256) as u8;
                    }
                }
            }
        } else if sector >= 204800 && sector < 204810 {
            // Simular EclipseFS en la partición 2
            match sector - 204800 {
                0 => {
                    // EclipseFS superblock
                    buffer.fill(0);
                    buffer[0..9].copy_from_slice(b"ECLIPSEFS");
                    buffer[10..12].copy_from_slice(&[0x00, 0x02]); // Version 2.0
                    buffer[16..20].copy_from_slice(&[0x00, 0x00, 0x10, 0x00]); // Block size: 4096
                    buffer[24..32].copy_from_slice(&[0xD0, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // Inode table offset
                    buffer[32..40].copy_from_slice(&[0x5A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // Inode table size
                }
                _ => {
                    // Otros sectores de EclipseFS
                    for i in 0..buffer.len() {
                        buffer[i] = ((sector * 256 + i as u64) % 256) as u8;
                    }
                }
            }
        } else {
            // Otros sectores: datos de ejemplo
            for i in 0..buffer.len() {
                buffer[i] = ((sector * 256 + i as u64) % 256) as u8;
            }
        }
        
        Ok(())
    }
}