//! Driver PCI para Eclipse OS
//! 
//! Implementa detección y configuración de dispositivos PCI
//! para identificar hardware gráfico y otros dispositivos.

use core::ptr;
use core::arch::asm;
use crate::syslog_info;
use alloc::format;

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
pub const VENDOR_ID_VIRTIO: u16 = 0x1AF4;     // Virtio devices
pub const VENDOR_ID_VMWARE: u16 = 0x15AD;     // VMware SVGA II

/// Clases de dispositivos PCI
pub const CLASS_DISPLAY: u8 = 0x03;
pub const SUBCLASS_VGA: u8 = 0x00;
pub const SUBCLASS_3D: u8 = 0x02;
#[inline(always)]
unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inl(port: u16) -> u32 {
    let mut val: u32;
    asm!("in eax, dx", in("dx") port, out("eax") val, options(nostack, preserves_flags));
    val
}

/// Información de un dispositivo PCI
#[derive(Debug, Clone, Copy)]
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

/// Gestor del bus PCI
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
        
        syslog_info!("PCI", "Iniciando escaneo completo de dispositivos PCI");
        
        // Escanear todos los buses 0-255
        for bus in 0..=255 {
            syslog_info!("PCI", &format!("Escaneando bus {}", bus));

            // Escanear dispositivos 0..31
            for device in 0..32 {
                // Leer primero función 0 para validar presencia y modo multifunción
                let dev0_opt = self.read_pci_device(bus, device, 0);
                let Some(dev0) = dev0_opt else { continue };
                if dev0.vendor_id == 0xFFFF { continue };

                let is_multifunction = (dev0.header_type & 0x80) != 0;

                // Procesar función 0
                self.total_device_count += 1;
                self.add_device(dev0);
                if self.is_gpu_device(&dev0) {
                    syslog_info!("PCI", &format!(
                        "GPU encontrada: {:04X}:{:04X} en bus {}:{}.{}",
                        dev0.vendor_id, dev0.device_id, bus, device, 0
                    ));
                    if let Some(gpu_info) = self.create_gpu_info(dev0) { self.add_gpu(gpu_info); }
                }

                // Si no es multifunción, no hay más funciones
                if !is_multifunction { continue }

                // Procesar funciones 1..7 existentes
                for function in 1..8 {
                    if let Some(pci_device) = self.read_pci_device(bus, device, function) {
                        if pci_device.vendor_id != 0xFFFF {
                            self.total_device_count += 1;
                            self.add_device(pci_device);
                            if self.is_gpu_device(&pci_device) {
                                syslog_info!("PCI", &format!(
                                    "GPU encontrada: {:04X}:{:04X} en bus {}:{}.{}",
                                    pci_device.vendor_id, pci_device.device_id, bus, device, function
                                ));
                                if let Some(gpu_info) = self.create_gpu_info(pci_device) { self.add_gpu(gpu_info); }
                            }
                        }
                    }
                }
            }
        }
        
        syslog_info!("PCI", &format!("Escaneo completado: {} dispositivos, {} GPUs (almacenados: {})", self.total_device_count, self.gpu_count, self.device_count));
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
                if dev0.vendor_id == 0xFFFF { continue };

                let is_multifunction = (dev0.header_type & 0x80) != 0;
                // Función 0
                self.total_device_count += 1;
                self.add_device(dev0);
                if self.is_gpu_device(&dev0) {
                    if let Some(gpu_info) = self.create_gpu_info(dev0) { self.add_gpu(gpu_info); }
                }

                if !is_multifunction { continue }
                for function in 1..8 {
                    if let Some(pci_device) = self.read_pci_device(bus, device, function) {
                        if pci_device.vendor_id != 0xFFFF {
                            self.total_device_count += 1;
                            self.add_device(pci_device);
                            if self.is_gpu_device(&pci_device) {
                                if let Some(gpu_info) = self.create_gpu_info(pci_device) { self.add_gpu(gpu_info); }
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
        device.class_code == CLASS_DISPLAY && 
        (device.subclass_code == SUBCLASS_VGA || device.subclass_code == SUBCLASS_3D)
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
    fn get_gpu_capabilities(&self, device: &PciDevice, gpu_type: GpuType) -> (u64, bool, bool, (u32, u32)) {
        match gpu_type {
            GpuType::Intel => {
                // Intel Graphics - capacidades básicas
                (256 * 1024 * 1024, true, false, (1920, 1080)) // 256MB, 2D, 1920x1080
            },
            GpuType::Nvidia => {
                // NVIDIA - capacidades avanzadas
                (1024 * 1024 * 1024, true, true, (3840, 2160)) // 1GB, 2D+3D, 4K
            },
            GpuType::Amd => {
                // AMD - capacidades avanzadas
                (512 * 1024 * 1024, true, true, (2560, 1440)) // 512MB, 2D+3D, 1440p
            },
            GpuType::QemuBochs => {
                // QEMU/Bochs std VGA suele reportar ~16MB
                (16 * 1024 * 1024, true, false, (1600, 1200))
            },
            GpuType::Virtio => {
                // Virtio-GPU moderno, valores conservadores
                (64 * 1024 * 1024, true, true, (1920, 1080))
            },
            GpuType::Vmware => {
                // VMware SVGA II, valores típicos
                (128 * 1024 * 1024, true, true, (2560, 1440))
            },
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
        self.gpus.iter()
            .find_map(|gpu| gpu.as_ref())
            .filter(|gpu| gpu.is_primary)
    }
    
    /// Obtener número total de dispositivos
    pub fn device_count(&self) -> usize {
        self.device_count
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
}

/// Función de conveniencia para escanear PCI
pub fn scan_pci_devices() -> PciManager {
    let mut manager = PciManager::new();
    manager.scan_devices();
    manager
}