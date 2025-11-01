//! Driver PCI para Eclipse OS
//!
//! Implementa detección y configuración de dispositivos PCI
//! para identificar hardware gráfico y otros dispositivos.

use crate::syslog_info;
use alloc::format;
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr;

/// Configuración de espacio de configuración PCI
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// IDs de fabricantes de GPU conocidos
pub const VENDOR_ID_INTEL: u16 = 0x8086;
pub const VENDOR_ID_NVIDIA: u16 = 0x10DE;
pub const VENDOR_ID_AMD: u16 = 0x1002;
pub const VENDOR_ID_VIA: u16 = 0x1106;
pub const VENDOR_ID_SIS: u16 = 0x1039;
// Virtualización / emulados
pub const VENDOR_ID_QEMU_BOCHS: u16 = 0x1234; // QEMU/Bochs stdvga 0x1111
pub const VENDOR_ID_VIRTIO: u16 = 0x1AF4; // Virtio devices
pub const VENDOR_ID_VMWARE: u16 = 0x15AD; // VMware SVGA II

/// Clases de dispositivos PCI
pub const CLASS_DISPLAY: u8 = 0x03;
pub const SUBCLASS_VGA: u8 = 0x00;
pub const SUBCLASS_3D: u8 = 0x02;
#[inline(always)]
pub unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nostack, preserves_flags));
}

#[inline(always)]
pub unsafe fn inl(port: u16) -> u32 {
    let mut val: u32;
    asm!("in eax, dx", in("dx") port, out("eax") val, options(nostack, preserves_flags));
    val
}

/// Información de un dispositivo PCI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass_code: u8,
    pub prog_if: u8,
    pub revision_id: u8,
    pub header_type: u8,
    pub status: u16,
    pub command: u16,
}

impl PciDevice {
    /// Leer registro de configuración PCI
    pub fn read_config(&self, offset: u8) -> u32 {
        let address = 0x80000000u32
            | ((self.bus as u32) << 16)
            | ((self.device as u32) << 11)
            | ((self.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            inl(PCI_CONFIG_DATA)
        }
    }

    /// Escribir registro de configuración PCI
    pub fn write_config(&self, offset: u8, value: u32) {
        let address = 0x80000000u32
            | ((self.bus as u32) << 16)
            | ((self.device as u32) << 11)
            | ((self.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            outl(PCI_CONFIG_DATA, value);
        }
    }

    /// Habilitar MMIO y Bus Master
    pub fn enable_mmio_and_bus_master(&self) {
        let command = self.read_config(0x04);
        self.write_config(0x04, command | 0x06); // MMIO + Bus Master
    }

    /// Leer todas las BARs
    pub fn read_all_bars(&self) -> [u32; 6] {
        let mut bars = [0u32; 6];
        for i in 0..6 {
            bars[i] = self.read_config(0x10 + (i as u8 * 4));
        }
        bars
    }

    /// Calcular tamaño de BAR
    pub fn calculate_bar_size(&self, bar_index: usize) -> u32 {
        if bar_index >= 6 {
            return 0;
        }

        let bar_offset = 0x10 + (bar_index as u8 * 4);
        let original_bar = self.read_config(bar_offset);

        // Escribir 0xFFFFFFFF para determinar el tamaño
        self.write_config(bar_offset, 0xFFFFFFFF);
        let size_value = self.read_config(bar_offset);

        // Restaurar valor original
        self.write_config(bar_offset, original_bar);

        // Calcular tamaño real
        if (size_value & 0x1) == 0 {
            // MMIO BAR
            let size = !(size_value & 0xFFFFFFF0) + 1;
            size
        } else {
            // I/O BAR
            let size = !(size_value & 0xFFFFFFFC) + 1;
            size
        }
    }

    /// Leer puntero de capacidades
    pub fn read_capability_pointer(&self) -> u8 {
        let status = self.read_config(0x04);
        if (status & 0x00100000) != 0 {
            (self.read_config(0x34) & 0xFF) as u8
        } else {
            0
        }
    }

    /// Leer capacidad
    pub fn read_capability(&self, offset: u8) -> Option<(u8, u8)> {
        if offset == 0 {
            return None;
        }

        let cap_data = self.read_config(offset);
        let cap_id = (cap_data & 0xFF) as u8;
        let next_offset = ((cap_data >> 8) & 0xFF) as u8;

        Some((cap_id, next_offset))
    }

    pub fn read_config_dword(&self, reg: u8) -> u32 {
        let bus = self.bus as u32;
        let device = self.device as u32;
        let func = self.function as u32;
        let address = 0x80000000 | (bus << 16) | (device << 11) | (func << 8) | (reg as u32 & 0xfc);
        unsafe {
            x86::io::outl(0xCF8, address);
            x86::io::inl(0xCFC)
        }
    }

    /// Lee la dirección base de un registro BAR (Base Address Register).
    /// Maneja BARs de 32 y 64 bits.
    pub fn get_bar(&self, bar_index: u8) -> u64 {
        if bar_index >= 6 {
            return 0;
        }
        let bar_offset = 0x10 + (bar_index as u32 * 4);
        let bar_val = self.read_config_dword(bar_offset as u8);

        // Bit 0: 0 para memoria, 1 para I/O
        let is_mmio = (bar_val & 1) == 0;

        if is_mmio {
            let bar_type = (bar_val >> 1) & 0b11;
            match bar_type {
                0b00 => { // 32-bit
                    (bar_val & 0xFFFFFFF0) as u64
                },
                0b10 => { // 64-bit
                    if bar_index >= 5 { return 0; } // No hay espacio para la parte alta
                    let high_bits = self.read_config_dword(bar_offset as u8 + 4);
                    let low_bits = bar_val & 0xFFFFFFF0;
                    ((high_bits as u64) << 32) | (low_bits as u64)
                },
                _ => 0, // Otros tipos no soportados
            }
        } else { // I/O port BAR (siempre 32-bit)
            (bar_val & 0xFFFFFFFC) as u64
        }
    }

    pub fn find_virtio_capability(&self, cfg_type: u8) -> Option<VirtioPciCap> {
        const PCI_CAP_ID_VNDR: u8 = 0x09;
        
        let mut cap_offset = (self.read_config_dword(0x34) & 0xFF) as u8;

        while cap_offset != 0 {
            let dword0 = self.read_config_dword(cap_offset);
            let cap_id = (dword0 & 0xFF) as u8;
            let next_ptr = ((dword0 >> 8) & 0xFF) as u8;
            
            if cap_id == PCI_CAP_ID_VNDR {
                let cap_len = ((dword0 >> 16) & 0xFF) as u8;
                let cap_type_byte = ((dword0 >> 24) & 0xFF) as u8;

                if cap_type_byte == cfg_type {
                    let dword1 = self.read_config_dword(cap_offset + 4);
                    let bar = (dword1 & 0xFF) as u8;
                    let offset = self.read_config_dword(cap_offset + 8);
                    let length = self.read_config_dword(cap_offset + 12);
                    
                    return Some(VirtioPciCap {
                        cap_vndr: cap_id,
                        cap_next: next_ptr,
                        cap_len,
                        cfg_type: cap_type_byte,
                        bar,
                        padding: [0; 3],
                        offset,
                        length,
                    });
                }
            }

            cap_offset = next_ptr;
        }
        None
    }
}

/// Información específica de GPU
#[derive(Debug, Clone, Copy)]
pub struct GpuInfo {
    pub pci_device: PciDevice,
    pub gpu_type: GpuType,
    pub memory_size: u64,
    pub is_primary: bool,
    pub supports_2d: bool,
    pub supports_3d: bool,
    pub max_resolution: (u32, u32),
}

/// Tipos de GPU
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpuType {
    Intel,
    Nvidia,
    Amd,
    Via,
    Sis,
    QemuBochs,
    Virtio,
    Vmware,
    Unknown,
}

impl GpuType {
    pub fn from_vendor_id(vendor_id: u16) -> Self {
        match vendor_id {
            VENDOR_ID_INTEL => GpuType::Intel,
            VENDOR_ID_NVIDIA => GpuType::Nvidia,
            VENDOR_ID_AMD => GpuType::Amd,
            VENDOR_ID_VIA => GpuType::Via,
            VENDOR_ID_SIS => GpuType::Sis,
            VENDOR_ID_QEMU_BOCHS => GpuType::QemuBochs,
            VENDOR_ID_VIRTIO => GpuType::Virtio,
            VENDOR_ID_VMWARE => GpuType::Vmware,
            _ => GpuType::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GpuType::Intel => "Intel",
            GpuType::Nvidia => "NVIDIA",
            GpuType::Amd => "AMD",
            GpuType::Via => "VIA",
            GpuType::Sis => "SiS",
            GpuType::QemuBochs => "QEMU/Bochs VGA",
            GpuType::Virtio => "Virtio-GPU",
            GpuType::Vmware => "VMware SVGA II",
            GpuType::Unknown => "Unknown",
        }
    }
}

// --- Estructuras y constantes para Capabilities PCI de VirtIO ---
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioPciCap {
    // Cabecera de la capability genérica
    pub cap_vndr: u8,       // Debe ser 0x09 (PCI_CAP_ID_VNDR)
    pub cap_next: u8,       // Siguiente puntero en la lista
    pub cap_len: u8,        // Longitud de esta capability
    // Campos específicos de VirtIO
    pub cfg_type: u8,       // Tipo de capability (VIRTIO_PCI_CAP_*)
    pub bar: u8,            // Índice de la BAR donde se encuentra la estructura
    pub padding: [u8; 3],
    pub offset: u32,        // Offset dentro de la BAR
    pub length: u32,        // Longitud de la estructura en bytes
}

/// Gestor del bus PCI
#[derive(Clone, Debug)]
pub struct PciManager {
    devices: [Option<PciDevice>; 256],
    device_count: usize,
    total_device_count: usize,
    gpus: [Option<GpuInfo>; 16],
    gpu_count: usize,
}

impl PciManager {
    pub fn new() -> Self {
        Self {
            devices: [(); 256].map(|_| None),
            device_count: 0,
            total_device_count: 0,
            gpus: [(); 16].map(|_| None),
            gpu_count: 0,
        }
    }

    /// Escanear todos los dispositivos PCI
    pub fn scan_devices(&mut self) {
        self.device_count = 0;
        self.total_device_count = 0;
        self.gpu_count = 0;

        syslog_info!("PCI", "Iniciando escaneo optimizado de dispositivos PCI");

        // Escanear buses 0-255 (rango completo para encontrar GPUs)
        for bus in 0..=255 {
            syslog_info!("PCI", &format!("Escaneando bus {}", bus));
            let mut devices_found_on_bus = 0;

            // Escanear dispositivos 0..31
            for device in 0..32 {
                // Leer primero función 0 para validar presencia y modo multifunción
                let dev0_opt = self.read_pci_device(bus, device, 0);
                let Some(dev0) = dev0_opt else { continue };
                if dev0.vendor_id == 0xFFFF {
                    continue;
                };
                
                devices_found_on_bus += 1;
                
                // Log detallado con información de fabricante
                let vendor_name = match dev0.vendor_id {
                    0x8086 => "Intel",
                    0x10DE => "NVIDIA", 
                    0x1002 => "AMD",
                    0x1AF4 => "VirtIO",
                    0x15AD => "VMware",
                    0x1234 => "QEMU/Bochs",
                    _ => "Unknown"
                };
                
                syslog_info!("PCI", &format!("Dispositivo encontrado: Bus {} Device {} Function {} - VID:{:04X} ({}) DID:{:04X} Class:{:02X}.{:02X}.{:02X}", 
                                            bus, device, 0, dev0.vendor_id, vendor_name, dev0.device_id, 
                                            dev0.class_code, dev0.subclass_code, dev0.prog_if));

                let is_multifunction = (dev0.header_type & 0x80) != 0;

                // Procesar función 0
                self.total_device_count += 1;
                self.add_device(dev0);
                
                // Verificar si es GPU con logging detallado
                syslog_info!("PCI", &format!("Verificando si es GPU: Class {} == {} (DISPLAY)?", 
                                            dev0.class_code, CLASS_DISPLAY));
                if self.is_gpu_device(&dev0) {
                    syslog_info!(
                        "PCI",
                        &format!(
                            "*** GPU ENCONTRADA ***: {:04X}:{:04X} en bus {}:{}.{} Class:{:02X}.{:02X}.{:02X}",
                            dev0.vendor_id, dev0.device_id, bus, device, 0,
                            dev0.class_code, dev0.subclass_code, dev0.prog_if
                        )
                    );
                    if let Some(gpu_info) = self.create_gpu_info(dev0) {
                        self.add_gpu(gpu_info);
                        syslog_info!("PCI", &format!("GPU agregada exitosamente. Total GPUs: {}", self.gpu_count));
                    } else {
                        syslog_info!("PCI", "ERROR: No se pudo crear información de GPU");
                    }
                } else {
                    syslog_info!("PCI", &format!("No es GPU: Class {} != {} (DISPLAY)", 
                                                dev0.class_code, CLASS_DISPLAY));
                }

                // Si no es multifunción, no hay más funciones
                if !is_multifunction {
                    continue;
                }

                // Procesar funciones 1..7 existentes
                for function in 1..8 {
                    if let Some(pci_device) = self.read_pci_device(bus, device, function) {
                        if pci_device.vendor_id != 0xFFFF {
                            devices_found_on_bus += 1;
                            
                            // Log detallado con información de fabricante para multifunción
                            let vendor_name = match pci_device.vendor_id {
                                0x8086 => "Intel",
                                0x10DE => "NVIDIA", 
                                0x1002 => "AMD",
                                0x1AF4 => "VirtIO",
                                0x15AD => "VMware",
                                0x1234 => "QEMU/Bochs",
                                _ => "Unknown"
                            };
                            
                            syslog_info!("PCI", &format!("Dispositivo multifunción encontrado: Bus {} Device {} Function {} - VID:{:04X} ({}) DID:{:04X} Class:{:02X}.{:02X}.{:02X}", 
                                                        bus, device, function, pci_device.vendor_id, vendor_name, pci_device.device_id, 
                                                        pci_device.class_code, pci_device.subclass_code, pci_device.prog_if));
                            
                            self.total_device_count += 1;
                            self.add_device(pci_device);
                            
                            // Verificar si es GPU con logging detallado
                            syslog_info!("PCI", &format!("Verificando si es GPU multifunción: Class {} == {} (DISPLAY)?", 
                                                        pci_device.class_code, CLASS_DISPLAY));
                            if self.is_gpu_device(&pci_device) {
                                syslog_info!(
                                    "PCI",
                                    &format!(
                                        "*** GPU MULTIFUNCIÓN ENCONTRADA ***: {:04X}:{:04X} en bus {}:{}.{} Class:{:02X}.{:02X}.{:02X}",
                                        pci_device.vendor_id,
                                        pci_device.device_id,
                                        bus,
                                        device,
                                        function,
                                        pci_device.class_code, pci_device.subclass_code, pci_device.prog_if
                                    )
                                );
                                if let Some(gpu_info) = self.create_gpu_info(pci_device) {
                                    self.add_gpu(gpu_info);
                                    syslog_info!("PCI", &format!("GPU multifunción agregada exitosamente. Total GPUs: {}", self.gpu_count));
                                } else {
                                    syslog_info!("PCI", "ERROR: No se pudo crear información de GPU multifunción");
                                }
                            } else {
                                syslog_info!("PCI", &format!("No es GPU multifunción: Class {} != {} (DISPLAY)", 
                                                            pci_device.class_code, CLASS_DISPLAY));
                            }
                        }
                    }
                }
                
                // Límite de dispositivos para evitar cuelgues (aumentado para rango completo)
                if self.device_count >= 256 {
                    syslog_info!("PCI", "Límite de dispositivos alcanzado, deteniendo escaneo");
                    break;
                }
            }
            
            syslog_info!("PCI", &format!("Bus {} completado: {} dispositivos encontrados", bus, devices_found_on_bus));
        }

        syslog_info!(
            "PCI",
            &format!(
                "Escaneo completado: {} dispositivos, {} GPUs (almacenados: {})",
                self.total_device_count, self.gpu_count, self.device_count
            )
        );
    }

    /// Escaneo silencioso (sin logs) y acotado; útil para QEMU
    pub fn scan_devices_quiet(&mut self) {
        self.device_count = 0;
        self.total_device_count = 0;
        self.gpu_count = 0;
        // Escaneo silencioso de todos los buses
        for bus in 0..=255 {
            for device in 0..32 {
                // Validar función 0
                let dev0_opt = self.read_pci_device(bus, device, 0);
                let Some(dev0) = dev0_opt else { continue };
                if dev0.vendor_id == 0xFFFF {
                    continue;
                };

                let is_multifunction = (dev0.header_type & 0x80) != 0;
                // Función 0
                self.total_device_count += 1;
                self.add_device(dev0);
                if self.is_gpu_device(&dev0) {
                    if let Some(gpu_info) = self.create_gpu_info(dev0) {
                        self.add_gpu(gpu_info);
                    }
                }

                if !is_multifunction {
                    continue;
                }
                for function in 1..8 {
                    if let Some(pci_device) = self.read_pci_device(bus, device, function) {
                        if pci_device.vendor_id != 0xFFFF {
                            self.total_device_count += 1;
                            self.add_device(pci_device);
                            if self.is_gpu_device(&pci_device) {
                                if let Some(gpu_info) = self.create_gpu_info(pci_device) {
                                    self.add_gpu(gpu_info);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Leer información de un dispositivo PCI específico
    fn read_pci_device(&self, bus: u8, device: u8, function: u8) -> Option<PciDevice> {
        // Leer vendor ID y device ID
        let vendor_device = self.read_config_dword(bus, device, function, 0x00);
        let vendor_id = (vendor_device & 0xFFFF) as u16;
        let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;

        // Si vendor ID es 0xFFFF, el dispositivo no existe
        if vendor_id == 0xFFFF {
            return None;
        }

        // Leer command y status
        let command_status = self.read_config_dword(bus, device, function, 0x04);
        let command = (command_status & 0xFFFF) as u16;
        let status = ((command_status >> 16) & 0xFFFF) as u16;

        // Leer class code, subclass, prog if, revision
        let class_revision = self.read_config_dword(bus, device, function, 0x08);
        let revision_id = (class_revision & 0xFF) as u8;
        let prog_if = ((class_revision >> 8) & 0xFF) as u8;
        let subclass_code = ((class_revision >> 16) & 0xFF) as u8;
        let class_code = ((class_revision >> 24) & 0xFF) as u8;

        // Leer header type
        let header_type = (self.read_config_dword(bus, device, function, 0x0C) >> 16) as u8;

        Some(PciDevice {
            bus,
            device,
            function,
            vendor_id,
            device_id,
            class_code,
            subclass_code,
            prog_if,
            revision_id,
            header_type,
            status,
            command,
        })
    }

    /// Leer un dword de configuración PCI
    fn read_config_dword(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        let address = 0x80000000u32
            | ((bus as u32) << 16)
            | ((device as u32) << 11)
            | ((function as u32) << 8)
            | ((offset as u32) & 0xFC);
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            inl(PCI_CONFIG_DATA)
        }
    }

    /// Agregar dispositivo a la lista
    fn add_device(&mut self, device: PciDevice) {
        if self.device_count < self.devices.len() {
            self.devices[self.device_count] = Some(device);
            self.device_count += 1;
        }
    }

    /// Verificar si un dispositivo es una GPU
    fn is_gpu_device(&self, device: &PciDevice) -> bool {
        // Detectar cualquier dispositivo de clase display (0x03)
        if device.class_code == CLASS_DISPLAY {
            let vendor_name = match device.vendor_id {
                0x8086 => "Intel",
                0x10DE => "NVIDIA", 
                0x1002 => "AMD",
                0x1AF4 => "VirtIO",
                0x15AD => "VMware",
                0x1234 => "QEMU/Bochs",
                _ => "Unknown"
            };
            
            syslog_info!("PCI", &format!("*** GPU CONFIRMADA ***: VID:{:04X} ({}) DID:{:04X} Class:{:02X} Subclass:{:02X} ProgIF:{:02X}", 
                                        device.vendor_id, vendor_name, device.device_id, device.class_code, device.subclass_code, device.prog_if));
            return true;
        }
        
        // También detectar dispositivos con VID conocidos de GPU aunque no tengan clase 0x03
        if matches!(device.vendor_id, 0x8086 | 0x10DE | 0x1002) {
            syslog_info!("PCI", &format!("*** POSIBLE GPU POR VID ***: VID:{:04X} DID:{:04X} Class:{:02X} (no es clase display pero VID sugiere GPU)", 
                                        device.vendor_id, device.device_id, device.class_code));
        }
        
        false
    }

    /// Crear información de GPU a partir de dispositivo PCI
    fn create_gpu_info(&self, device: PciDevice) -> Option<GpuInfo> {
        let gpu_type = GpuType::from_vendor_id(device.vendor_id);

        // Obtener información específica de la GPU
        let (memory_size, supports_2d, supports_3d, max_resolution) =
            self.get_gpu_capabilities(&device, gpu_type);

        Some(GpuInfo {
            pci_device: device,
            gpu_type,
            memory_size,
            is_primary: self.gpu_count == 0, // Primera GPU es la primaria
            supports_2d,
            supports_3d,
            max_resolution,
        })
    }

    /// Obtener capacidades de la GPU
    fn get_gpu_capabilities(
        &self,
        device: &PciDevice,
        gpu_type: GpuType,
    ) -> (u64, bool, bool, (u32, u32)) {
        match gpu_type {
            GpuType::Intel => {
                // Intel Graphics - capacidades básicas
                (256 * 1024 * 1024, true, false, (1920, 1080)) // 256MB, 2D, 1920x1080
            }
            GpuType::Nvidia => {
                // NVIDIA - capacidades avanzadas
                (1024 * 1024 * 1024, true, true, (3840, 2160)) // 1GB, 2D+3D, 4K
            }
            GpuType::Amd => {
                // AMD - capacidades avanzadas
                (512 * 1024 * 1024, true, true, (2560, 1440)) // 512MB, 2D+3D, 1440p
            }
            GpuType::QemuBochs => {
                // QEMU/Bochs std VGA suele reportar ~16MB
                (16 * 1024 * 1024, true, false, (1600, 1200))
            }
            GpuType::Virtio => {
                // Virtio-GPU moderno, valores conservadores
                (64 * 1024 * 1024, true, true, (1920, 1080))
            }
            GpuType::Vmware => {
                // VMware SVGA II, valores típicos
                (128 * 1024 * 1024, true, true, (2560, 1440))
            }
            _ => {
                // GPU desconocida - capacidades mínimas
                (64 * 1024 * 1024, true, false, (1024, 768)) // 64MB, 2D, 1024x768
            }
        }
    }

    /// Agregar GPU a la lista
    fn add_gpu(&mut self, gpu: GpuInfo) {
        if self.gpu_count < self.gpus.len() {
            self.gpus[self.gpu_count] = Some(gpu);
            self.gpu_count += 1;
        }
    }

    /// Obtener lista de GPUs detectadas
    pub fn get_gpus(&self) -> &[Option<GpuInfo>] {
        &self.gpus[..self.gpu_count]
    }

    /// Obtener GPU primaria
    pub fn get_primary_gpu(&self) -> Option<&GpuInfo> {
        self.gpus
            .iter()
            .find_map(|gpu| gpu.as_ref())
            .filter(|gpu| gpu.is_primary)
    }

    /// Obtener número total de dispositivos
    pub fn device_count(&self) -> usize {
        self.device_count
    }

    /// Obtiene una referencia al array de dispositivos
    pub fn get_devices(&self) -> &[Option<PciDevice>; 256] {
        &self.devices
    }

    /// Obtener número total de dispositivos detectados (aunque no se almacenen todos)
    pub fn total_device_count(&self) -> usize {
        self.total_device_count
    }

    /// Obtener número total de GPUs
    pub fn gpu_count(&self) -> usize {
        self.gpu_count
    }

    /// Obtener información de un dispositivo específico
    pub fn get_device(&self, index: usize) -> Option<&PciDevice> {
        self.devices.get(index)?.as_ref()
    }

    /// Escanear todos los buses PCI (alias para scan_devices)
    pub fn scan_all_buses(&mut self) {
        self.scan_devices();
    }

    /// Buscar dispositivos por clase y subclase
    pub fn find_devices_by_class_subclass(&self, class: u8, subclass: u8) -> Vec<PciDevice> {
        let mut result = Vec::new();
        for i in 0..self.device_count {
            if let Some(device) = &self.devices[i] {
                if device.class_code == class && device.subclass_code == subclass {
                    result.push(*device);
                }
            }
        }
        result
    }
}

/// Función de conveniencia para escanear PCI
pub fn scan_pci_devices() -> PciManager {
    let mut manager = PciManager::new();
    manager.scan_devices();
    manager
}
