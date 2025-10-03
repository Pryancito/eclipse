//! Gestor de almacenamiento unificado
//! 
//! Este módulo integra todos los drivers de almacenamiento (ATA, NVMe, AHCI)
//! y proporciona una interfaz unificada para el acceso al almacenamiento.

use crate::debug::serial_write_str;
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use alloc::{format, vec::Vec, string::{String, ToString}, boxed::Box};
use crate::drivers::block::BlockDevice;

/// Tipos de controladoras de almacenamiento
#[derive(Debug, Clone, Copy)]
pub enum StorageControllerType {
    ATA,
    NVMe,
    AHCI,
    VirtIO,
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
    devices: Vec<StorageDevice>,
    active_device: Option<usize>,
}

impl Clone for StorageManager {
    fn clone(&self) -> Self {
        Self {
            devices: self.devices.clone(),
            active_device: self.active_device,
        }
    }
}

impl StorageManager {
    /// Crear nuevo gestor de almacenamiento
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
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
                            StorageControllerType::AHCI // Usar AHCI como fallback para RAID/SATA
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
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: Controladora agregada (polished_pci): {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
                }

                // Detectar VirtIO Block (vendor 0x1AF4, device 0x1001) - Virtualización
                if device.vendor_id == 0x1AF4 && device.device_id == 0x1001 {
                    serial_write_str(&format!("STORAGE_MANAGER: Dispositivo VirtIO Block encontrado - VID:{:#x} DID:{:#x}\n", 
                                             device.vendor_id, device.device_id));
                    
                    // Crear información de VirtIO
                    let storage_info = StorageDeviceInfo {
                        controller_type: StorageControllerType::VirtIO,
                        model: "VirtIO Block Device".to_string(),
                        serial: "VIRTIO-SERIAL".to_string(),
                        firmware: "VIRTIO-FW".to_string(),
                        capacity: 0, // Se detectará en la inicialización
                        block_size: 512,
                        max_lba: 0,
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str("STORAGE_MANAGER: VirtIO Block agregado\n");
                }

                // Detectar GPUs (class 3) - para depuración
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
                    
                    // Crear información de GPU como dispositivo de almacenamiento simulado
                    let storage_info = StorageDeviceInfo {
                        controller_type: StorageControllerType::ATA, // Usar ATA como placeholder
                        model: alloc::format!("GPU {:04X}:{:04X}", device.vendor_id, device.device_id),
                        serial: "GPU-SERIAL".to_string(),
                        firmware: "GPU-FW".to_string(),
                        capacity: 0,
                        block_size: 512,
                        max_lba: 0,
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: GPU agregada: {:04X}:{:04X}\n", 
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
                let base_class = device.class_code;
                let subclass = device.subclass_code;

                serial_write_str(&format!("STORAGE_MANAGER: PCI Manual - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                         device.vendor_id, device.device_id, 
                                         base_class, subclass));

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
                            StorageControllerType::AHCI // Usar AHCI como fallback para RAID/SATA
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
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: Controladora agregada: {:04X}:{:04X}\n", 
                                             device.vendor_id, device.device_id));
                }

                // Detectar GPUs (class 3) - para depuración
                if base_class == 0x03 {
                    serial_write_str(&format!("STORAGE_MANAGER: GPU detectada en hardware real - VID:{:#x} DID:{:#x} Class:{}.{}\n", 
                                             device.vendor_id, device.device_id, 
                                             base_class, subclass));
                    
                    // Crear información de GPU como dispositivo de almacenamiento simulado
                    let storage_info = StorageDeviceInfo {
                        controller_type: StorageControllerType::ATA, // Usar ATA como placeholder
                        model: alloc::format!("GPU {:04X}:{:04X}", device.vendor_id, device.device_id),
                        serial: "GPU-SERIAL".to_string(),
                        firmware: "GPU-FW".to_string(),
                        capacity: 0,
                        block_size: 512,
                        max_lba: 0,
                    };

                    self.devices.push(StorageDevice {
                        info: storage_info,
                    });

                    serial_write_str(&format!("STORAGE_MANAGER: GPU agregada: {:04X}:{:04X}\n", 
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
            if let Some(_device) = self.devices.get(index) {
                // TODO: Implementar lectura real del dispositivo
                Err("Lectura de bloques no implementada")
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

    // Métodos de compatibilidad para sistemas de archivos existentes
    
    /// Leer boot sector EclipseFS (compatibilidad)
    pub fn read_eclipsefs_boot_sector(&self, _boot_sector: &mut [u8]) -> Result<u32, &'static str> {
        Err("Método no implementado en nuevo StorageManager")
    }

    /// Obtener información de partición (compatibilidad)
    pub fn get_partition_info(&self, _partition_index: u32) -> Result<PartitionInfo, &'static str> {
        Err("Método no implementado en nuevo StorageManager")
    }

    /// Leer desde partición (compatibilidad)
    pub fn read_from_partition(&self, _partition_index: u32, _block: u64, _buffer: &mut [u8]) -> Result<(), &'static str> {
        Err("Método no implementado en nuevo StorageManager")
    }

    /// Leer boot sector FAT32 (compatibilidad)
    pub fn read_fat32_boot_sector(&self, _buffer: &mut [u8]) -> Result<(), &'static str> {
        Err("Método no implementado en nuevo StorageManager")
    }

    /// Obtener dispositivo (compatibilidad)
    pub fn get_device(&self, _index: usize) -> Option<&dyn BlockDevice> {
        None
    }
}

/// Información de partición (compatibilidad)
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub start_lba: u64,
    pub size_lba: u64,
    pub partition_type: u8,
    // Campos de compatibilidad
    pub start_sector: u64,
    pub size_sectors: u64,
}

// Instancia global del gestor de almacenamiento
static mut STORAGE_MANAGER: Option<StorageManager> = None;

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