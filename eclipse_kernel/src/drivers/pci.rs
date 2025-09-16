//! Driver PCI para Eclipse OS
//! 
//! Implementa detección y configuración de dispositivos PCI
//! para identificar hardware gráfico y otros dispositivos.

use core::ptr;
use core::arch::asm;

/// Configuración de espacio de configuración PCI
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

/// IDs de fabricantes de GPU conocidos
pub const VENDOR_ID_INTEL: u16 = 0x8086;
pub const VENDOR_ID_NVIDIA: u16 = 0x10DE;
pub const VENDOR_ID_AMD: u16 = 0x1002;
pub const VENDOR_ID_VIA: u16 = 0x1106;
pub const VENDOR_ID_SIS: u16 = 0x1039;

/// Clases de dispositivos PCI
pub const CLASS_DISPLAY: u8 = 0x03;
pub const SUBCLASS_VGA: u8 = 0x00;
pub const SUBCLASS_3D: u8 = 0x02;
pub const SUBCLASS_DISPLAY_OTHER: u8 = 0x80;

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
        let address = 0x8000_0000u32
            | ((self.bus as u32) << 16)
            | ((self.device as u32) << 11)
            | ((self.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        
        unsafe {
            io_outl(PCI_CONFIG_ADDRESS, address);
            // Pequeña pausa
            core::hint::spin_loop();
            io_inl(PCI_CONFIG_DATA)
        }
    }
    
    /// Escribir registro de configuración PCI
    pub fn write_config(&self, offset: u8, value: u32) {
        let address = 0x8000_0000u32
            | ((self.bus as u32) << 16)
            | ((self.device as u32) << 11)
            | ((self.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        
        unsafe {
            io_outl(PCI_CONFIG_ADDRESS, address);
            io_outl(PCI_CONFIG_DATA, value);
        }
    }

    /// Leer command y status del dispositivo
    pub fn read_command_status(&self) -> (u16, u16) {
        let v = self.read_config(0x04);
        let command = (v & 0xFFFF) as u16;
        let status = ((v >> 16) & 0xFFFF) as u16;
        (command, status)
    }

    /// Habilitar MMIO y Bus Master en el dispositivo
    pub fn enable_mmio_and_bus_master(&self) {
        let (command, status) = self.read_command_status();
        let mut new_command = command;
        // Bit 1: Memory Space Enable, Bit 2: Bus Master Enable
        new_command |= 1 << 1;
        new_command |= 1 << 2;
        let value = (status as u32) << 16 | (new_command as u32);
        self.write_config(0x04, value);
    }

    /// Leer un BAR de 32 bits (BAR0..BAR5)
    pub fn read_bar(&self, bar_index: u8) -> u32 {
        let index = core::cmp::min(bar_index, 5);
        let offset = 0x10 + index * 4;
        self.read_config(offset)
    }

    /// Leer todos los BARs (BAR0..BAR5)
    pub fn read_all_bars(&self) -> [u32; 6] {
        let mut bars = [0u32; 6];
        for i in 0..6 {
            bars[i] = self.read_bar(i as u8);
        }
        bars
    }

    /// Calcular tamaño real de un BAR usando máscara
    pub fn calculate_bar_size(&self, bar_index: u8) -> u64 {
        let bar = self.read_bar(bar_index);
        if bar == 0 || bar == 0xFFFFFFFF { return 0; }
        
        // Guardar valor original
        let original = bar;
        // Escribir todo 1s para leer el tamaño
        self.write_config(0x10 + (bar_index as u8) * 4, 0xFFFFFFFF);
        let masked = self.read_bar(bar_index);
        // Restaurar valor original
        self.write_config(0x10 + (bar_index as u8) * 4, original);
        
        // Calcular tamaño basado en bits bajos
        if (masked & 0x1) == 0 { // Memoria
            if (masked & 0x6) == 0x4 { // 64-bit memory BAR
                // Para BARs 64-bit, necesitamos leer el siguiente BAR también
                if bar_index < 5 {
                    let next_bar = self.read_bar(bar_index + 1);
                    let next_original = next_bar;
                    self.write_config(0x10 + (bar_index + 1) * 4, 0xFFFFFFFF);
                    let masked_next = self.read_bar(bar_index + 1);
                    self.write_config(0x10 + (bar_index + 1) * 4, next_original);
                    
                    let full_masked = ((masked_next as u64) << 32) | (masked as u64 & 0xFFFFFFF0);
                    let size = !full_masked + 1;
                    size
                } else {
                    // BAR 64-bit en la última posición, usar solo 32 bits
                    let size = !(masked & 0xFFFFFFF0) + 1;
                    size as u64
                }
            } else { // 32-bit memory BAR
                let size = !(masked & 0xFFFFFFF0) + 1;
                size as u64
            }
        } else { // I/O
            let size = !(masked & 0xFFFFFFFC) + 1;
            size as u64
        }
    }

    /// Leer capability list pointer
    pub fn read_capability_pointer(&self) -> u8 {
        let status = (self.read_config(0x04) >> 16) as u16;
        if (status & 0x10) != 0 { // Capabilities bit set
            (self.read_config(0x34) & 0xFF) as u8
        } else {
            0
        }
    }

    /// Leer capability específica
    pub fn read_capability(&self, offset: u8) -> Option<(u8, u8)> {
        if offset == 0 { return None; }
        let cap = self.read_config(offset);
        let id = (cap & 0xFF) as u8;
        let next = ((cap >> 8) & 0xFF) as u8;
        Some((id, next))
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
    Qemu,
    Virtio,
    Qxl,
    Vmware,
    Via,
    Sis,
    Unknown,
}

impl GpuType {
    pub fn from_vendor_id(vendor_id: u16) -> Self {
        match vendor_id {
            VENDOR_ID_INTEL => GpuType::Intel,
            VENDOR_ID_NVIDIA => GpuType::Nvidia,
            VENDOR_ID_AMD => GpuType::Amd,
            0x1234 => GpuType::Qemu,   // QEMU/Bochs std VGA
            0x1AF4 => GpuType::Virtio, // Virtio GPU
            0x1B36 => GpuType::Qxl,    // QXL (Red Hat)
            0x15AD => GpuType::Vmware, // VMware SVGA
            VENDOR_ID_VIA => GpuType::Via,
            VENDOR_ID_SIS => GpuType::Sis,
            _ => GpuType::Unknown,
        }
    }
    
    pub fn as_str(&self) -> &'static str {
        match self {
            GpuType::Intel => "Intel",
            GpuType::Nvidia => "NVIDIA",
            GpuType::Amd => "AMD",
            GpuType::Qemu => "QEMU StdVGA",
            GpuType::Virtio => "Virtio GPU",
            GpuType::Qxl => "QXL",
            GpuType::Vmware => "VMware SVGA",
            GpuType::Via => "VIA",
            GpuType::Sis => "SiS",
            GpuType::Unknown => "Unknown",
        }
    }
}

/// Gestor del bus PCI
pub struct PciManager {
    devices: [Option<PciDevice>; 256],
    device_count: usize,
    gpus: [Option<GpuInfo>; 16],
    gpu_count: usize,
    hardware_available: bool,
}

/// Versión minimal del PciManager para inicialización segura
#[derive(Clone, Copy)]
pub struct PciManagerMinimal {
    device_count: usize,
    gpu_count: usize,
    hardware_available: bool,
}

impl PciManagerMinimal {
    /// Crear versión minimal sin arrays grandes
    pub fn new() -> Self {
        Self {
            device_count: 0,
            gpu_count: 0,
            hardware_available: false,
        }
    }
}

impl PciManager {
    pub fn new() -> Self {
        let mut manager = Self {
            devices: [(); 256].map(|_| None),
            device_count: 0,
            gpus: [(); 16].map(|_| None),
            gpu_count: 0,
            hardware_available: false,
        };

        // Verificar si el hardware PCI está disponible
        manager.hardware_available = manager.check_pci_hardware();

        manager
    }

    /// Crear PciManager sin verificar hardware (versión ultra-minimal)
    pub fn new_without_hardware_check() -> Self {
        // VERSIÓN ULTRA-MINIMAL: Crear arrays de forma manual para evitar problemas
        let devices = [None; 256];
        let gpus = [None; 16];

        Self {
            devices,
            device_count: 0,
            gpus,
            gpu_count: 0,
            hardware_available: false, // No verificado aún
        }
    }

    /// Convertir de PciManagerMinimal a PciManager completo
    pub fn from_minimal(minimal: PciManagerMinimal) -> Self {
        Self {
            devices: [None; 256],
            device_count: minimal.device_count,
            gpus: [None; 16],
            gpu_count: minimal.gpu_count,
            hardware_available: minimal.hardware_available,
        }
    }

    /// Escanear dispositivos PCI de forma segura (sin operaciones de hardware)
    pub fn scan_devices_safe(&mut self) {
        // DEBUG: Función segura que no hace operaciones de hardware
        // Logging removido temporalmente para evitar breakpoint

        // Solo inicializar contadores, no hacer escaneo real
        self.device_count = 0;
        self.gpu_count = 0;

        // Logging removido temporalmente para evitar breakpoint
    }

    /// Verificar si el hardware PCI está disponible
    fn check_pci_hardware(&self) -> bool {
        // Verificar si el sistema soporta PCI de forma segura
        // Intentar leer un dispositivo conocido (host bridge) en bus 0, device 0, function 0
        match self.try_read_pci_safe(0, 0, 0, 0x00) {
            Ok(vendor_device) => {
                let vendor_id = (vendor_device & 0xFFFF) as u16;
                // Si el vendor ID no es 0xFFFF, el PCI está disponible
                vendor_id != 0xFFFF
            },
            Err(_) => {
                // Si falla la lectura, asumir que PCI no está disponible
                false
            }
        }
    }
    
    /// Intentar leer PCI de forma segura con manejo de errores
    fn try_read_pci_safe(&self, bus: u8, device: u8, function: u8, offset: u8) -> Result<u32, &'static str> {
        // Verificar parámetros válidos
        if bus > 255 || device > 31 || function > 7 || offset > 252 {
            return Err("Parámetros PCI inválidos");
        }
        
        // Verificar que el offset esté alineado a 4 bytes
        if offset & 0x3 != 0 {
            return Err("Offset PCI no alineado");
        }
        
        // Intentar leer de forma segura
        unsafe {
            let address = 0x8000_0000u32
                | ((bus as u32) << 16)
                | ((device as u32) << 11)
                | ((function as u32) << 8)
                | ((offset as u32) & 0xFC);

            io_outl(PCI_CONFIG_ADDRESS, address);
            // Pequeña pausa
            for _ in 0..10 { core::hint::spin_loop(); }
            let result = io_inl(PCI_CONFIG_DATA);

            if result == 0xFFFF_FFFF {
                Err("Dispositivo PCI no encontrado")
            } else {
                Ok(result)
            }
        }
    }
    
    /// Extraer vendor ID de un resultado de lectura PCI
    fn vendor_id_from_result(result: u32) -> u16 {
        (result & 0xFFFF) as u16
    }
    
    /// Escanear todos los dispositivos PCI
    pub fn scan_devices(&mut self) {
        self.device_count = 0;
        self.gpu_count = 0;

        // Si la disponibilidad aún no fue comprobada, comprobar ahora
        if !self.hardware_available {
            self.hardware_available = self.check_pci_hardware();
        }
        if !self.hardware_available {
            // No forzar GPU de fallback aquí, reportar 0 dispositivos/GPUs
            return;
        }

        // Escanear buses PCI de forma segura
        for bus in 0..=255 {
            for device in 0..=31 {
                // Verificar si hay un dispositivo en esta dirección
                if let Ok(device_info) = self.read_pci_device_safe(bus, device, 0) {
                    self.add_device(device_info);
                    
                    // Verificar si es una GPU
                    if self.is_gpu_device(&device_info) {
                        if let Some(gpu_info) = self.create_gpu_info(device_info) {
                            self.add_gpu(gpu_info);
                        }
                    }
                    
                    // Verificar funciones adicionales (para dispositivos multifunción)
                    let header = (self.read_config_dword(bus, device, 0, 0x0C) >> 16) as u8;
                    let is_multifunction = (header & 0x80) != 0;
                    if is_multifunction {
                        for function in 1..=7 {
                            if let Ok(func_info) = self.read_pci_device_safe(bus, device, function) {
                                self.add_device(func_info);
                                
                                if self.is_gpu_device(&func_info) {
                                    if let Some(gpu_info) = self.create_gpu_info(func_info) {
                                        self.add_gpu(gpu_info);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        // No crear GPUs ficticias: si no hay, reportar 0
    }
    
    /// Crear GPU de fallback cuando no se detecta hardware real
    fn create_fallback_gpu(&mut self) {
        let fallback_gpu = GpuInfo {
            pci_device: PciDevice {
                bus: 0,
                device: 2,
                function: 0,
                vendor_id: VENDOR_ID_INTEL,
                device_id: 0x1234,
                class_code: 0x03, // VGA class
                subclass_code: 0x00,
                prog_if: 0x00,
                revision_id: 0x01,
                header_type: 0x00,
                status: 0x0000,
                command: 0x0000,
            },
            gpu_type: GpuType::Intel,
            memory_size: 64 * 1024 * 1024, // 64 MB
            is_primary: true,
            supports_2d: true,
            supports_3d: false,
            max_resolution: (1920, 1080),
        };
        
        self.add_gpu(fallback_gpu);
    }
    
    /// Leer información de un dispositivo PCI específico de forma segura
    fn read_pci_device_safe(&self, bus: u8, device: u8, function: u8) -> Result<PciDevice, &'static str> {
        // Leer vendor ID y device ID
        let vendor_device = self.try_read_pci_safe(bus, device, function, 0x00)?;
        let vendor_id = (vendor_device & 0xFFFF) as u16;
        let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;
        
        // Si vendor ID es 0xFFFF, el dispositivo no existe
        if vendor_id == 0xFFFF {
            return Err("Dispositivo PCI no encontrado");
        }
        
        // Leer command y status
        let command_status = self.try_read_pci_safe(bus, device, function, 0x04)?;
        let command = (command_status & 0xFFFF) as u16;
        let status = ((command_status >> 16) & 0xFFFF) as u16;
        
        // Leer class code, subclass, prog if, revision
        let class_revision = self.try_read_pci_safe(bus, device, function, 0x08)?;
        let revision_id = (class_revision & 0xFF) as u8;
        let prog_if = ((class_revision >> 8) & 0xFF) as u8;
        let subclass_code = ((class_revision >> 16) & 0xFF) as u8;
        let class_code = ((class_revision >> 24) & 0xFF) as u8;
        
        // Leer header type
        let header_type = (self.try_read_pci_safe(bus, device, function, 0x0C)? >> 16) as u8;
        
        Ok(PciDevice {
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
    
    /// Leer información de un dispositivo PCI específico (versión legacy)
    fn read_pci_device(&self, bus: u8, device: u8, function: u8) -> Option<PciDevice> {
        self.read_pci_device_safe(bus, device, function).ok()
    }
    
    /// Leer un dword de configuración PCI
    fn read_config_dword(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        // Usar la implementación segura
        self.try_read_pci_safe(bus, device, function, offset).unwrap_or(0xFFFFFFFF)
    }

    /// Verificar si el hardware PCI está disponible
    pub fn is_hardware_available(&self) -> bool {
        self.hardware_available
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
        // Solo contar GPUs reales: clase Display (0x03) y subclases comunes
        if device.class_code != CLASS_DISPLAY { return false; }
        matches!(device.subclass_code, SUBCLASS_VGA | SUBCLASS_3D | SUBCLASS_DISPLAY_OTHER)
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
        // Intentar leer capacidades reales del dispositivo
        let (memory_size, supports_2d, supports_3d, max_resolution) = 
            self.detect_real_gpu_capabilities(device, gpu_type);
        
        // Si no se pudieron detectar capacidades reales, usar valores por defecto
        if memory_size == 0 {
            self.get_default_gpu_capabilities(gpu_type)
        } else {
            (memory_size, supports_2d, supports_3d, max_resolution)
        }
    }
    
    /// Detectar capacidades reales de la GPU
    fn detect_real_gpu_capabilities(&self, device: &PciDevice, gpu_type: GpuType) -> (u64, bool, bool, (u32, u32)) {
        // Intentar leer registros específicos de la GPU
        let mut memory_size = 0u64;
        let mut supports_2d = true; // Asumir que todas las GPUs soportan 2D
        let mut supports_3d = false;
        let mut max_resolution = (1024, 768);
        
        // Leer BAR0 (Base Address Register) para estimar memoria
        if let Ok(bar0) = self.try_read_pci_safe(device.bus, device.device, device.function, 0x10) {
            if bar0 & 0x1 == 0 { // Si es memoria (no I/O)
                let bar_size = self.estimate_bar_size(bar0);
                memory_size = bar_size;
            }
        }
        
        // Leer BAR2 si existe (común en GPUs)
        if let Ok(bar2) = self.try_read_pci_safe(device.bus, device.device, device.function, 0x18) {
            if bar2 & 0x1 == 0 && memory_size == 0 {
                let bar_size = self.estimate_bar_size(bar2);
                memory_size = bar_size;
            }
        }
        
        // Determinar capacidades basadas en el tipo de GPU
        match gpu_type {
            GpuType::Intel => {
                supports_3d = device.device_id >= 0x0100; // Intel HD Graphics
                max_resolution = if supports_3d { (2560, 1440) } else { (1920, 1080) };
                if memory_size == 0 { memory_size = 256 * 1024 * 1024; }
            },
            GpuType::Nvidia => {
                supports_3d = true; // NVIDIA siempre soporta 3D
                max_resolution = (3840, 2160); // 4K
                // Para NVIDIA, priorizar detección real desde BARs
                let mut total_detected = 0u64;
                for bar_idx in 0..6 {
                    let bar = self.try_read_pci_safe(device.bus, device.device, device.function, 0x10 + bar_idx * 4).unwrap_or(0);
                    if bar != 0 && bar != 0xFFFFFFFF && (bar & 0x1) == 0 { // Memoria válida
                        let bar_size = self.estimate_bar_size(bar);
                        total_detected += bar_size;
                    }
                }
                
                if total_detected > 0 {
                    memory_size = total_detected;
                } else {
                    // Fallback: estimación basada en device ID solo si no se detectó nada
                    memory_size = self.estimate_nvidia_memory(device.device_id);
                }
            },
            GpuType::Amd => {
                supports_3d = true; // AMD siempre soporta 3D
                max_resolution = (2560, 1440); // 1440p
                if memory_size == 0 { memory_size = 512 * 1024 * 1024; }
            },
            _ => {
                supports_3d = false;
                max_resolution = (1024, 768);
                if memory_size == 0 { memory_size = 64 * 1024 * 1024; }
            }
        }
        
        (memory_size, supports_2d, supports_3d, max_resolution)
    }
    
    /// Estimar el tamaño de un BAR usando el método de máscara
    fn estimate_bar_size(&self, bar: u32) -> u64 {
        if bar == 0 || bar == 0xFFFFFFFF {
            return 0;
        }
        
        // Verificar si es un BAR de memoria (bit 0 = 0)
        if (bar & 0x1) == 0 {
            // BAR de memoria
            let size_mask = bar & 0xFFFFFFF0; // Máscara de 4KB alineada
            if size_mask > 0 {
                // Calcular tamaño real basado en los bits de dirección
                let size = (1u64 << (32 - size_mask.leading_zeros())) as u64;
                size.max(1024 * 1024) // Mínimo 1MB
            } else {
                0
            }
        } else {
            // BAR de I/O (bit 0 = 1)
            let size_mask = bar & 0xFFFFFFFC; // Máscara de 4 bytes alineada
            if size_mask > 0 {
                let size = (1u64 << (32 - size_mask.leading_zeros())) as u64;
                size.max(4) // Mínimo 4 bytes para I/O
            } else {
                0
            }
        }
    }
    
    /// Estimar memoria de GPU NVIDIA basada en device ID
    fn estimate_nvidia_memory(&self, device_id: u16) -> u64 {
        // Estimaciones basadas en device IDs comunes de NVIDIA
        match device_id {
            // RTX 40 series (8GB+)
            0x2782..=0x27FF => 8 * 1024 * 1024 * 1024, // RTX 4080/4090
            // RTX 30 series (6-24GB)
            0x2204..=0x22FF => 6 * 1024 * 1024 * 1024, // RTX 3060
            0x2484..=0x24FF => 8 * 1024 * 1024 * 1024, // RTX 3070
            0x2504..=0x25FF => 10 * 1024 * 1024 * 1024, // RTX 3080
            0x2206..=0x2207 => 12 * 1024 * 1024 * 1024, // RTX 3080 Ti
            0x2208..=0x2209 => 24 * 1024 * 1024 * 1024, // RTX 3090
            // RTX 20 series (6-11GB)
            0x1F02..=0x1F03 => 6 * 1024 * 1024 * 1024, // RTX 2060
            0x1F04..=0x1F05 => 8 * 1024 * 1024 * 1024, // RTX 2070
            0x1F06..=0x1F07 => 8 * 1024 * 1024 * 1024, // RTX 2060 SUPER
            0x1E04..=0x1E05 => 8 * 1024 * 1024 * 1024, // RTX 2080
            0x1E07..=0x1E08 => 11 * 1024 * 1024 * 1024, // RTX 2080 Ti
            // GTX 16 series (4-8GB)
            0x2182..=0x21FF => 4 * 1024 * 1024 * 1024, // GTX 1650
            0x1F80..=0x1F81 => 6 * 1024 * 1024 * 1024, // GTX 1660
            0x1F82..=0x1F83 => 6 * 1024 * 1024 * 1024, // GTX 1660 Ti
            // GTX 10 series (2-11GB)
            0x1B80..=0x1B81 => 2 * 1024 * 1024 * 1024, // GTX 1050
            0x1B82..=0x1B83 => 4 * 1024 * 1024 * 1024, // GTX 1050 Ti
            0x1C02..=0x1C03 => 3 * 1024 * 1024 * 1024, // GTX 1060
            0x1B00..=0x1B01 => 4 * 1024 * 1024 * 1024, // GTX 1070
            0x1B02..=0x1B03 => 8 * 1024 * 1024 * 1024, // GTX 1080
            0x1B06..=0x1B07 => 11 * 1024 * 1024 * 1024, // GTX 1080 Ti
            // Fallback para GPUs NVIDIA desconocidas
            _ => 8 * 1024 * 1024 * 1024, // 8GB por defecto para NVIDIA moderna
        }
    }
    
    /// Obtener capacidades por defecto de la GPU
    fn get_default_gpu_capabilities(&self, gpu_type: GpuType) -> (u64, bool, bool, (u32, u32)) {
        match gpu_type {
            GpuType::Intel => {
                (256 * 1024 * 1024, true, false, (1920, 1080)) // 256MB, 2D, 1920x1080
            },
            GpuType::Nvidia => {
                (1024 * 1024 * 1024, true, true, (3840, 2160)) // 1GB, 2D+3D, 4K
            },
            GpuType::Amd => {
                (512 * 1024 * 1024, true, true, (2560, 1440)) // 512MB, 2D+3D, 1440p
            },
            _ => {
                (64 * 1024 * 1024, true, false, (1024, 768)) // 64MB, 2D, 1024x768
            }
        }
    }
    
    /// Agregar GPU a la lista
    fn add_gpu(&mut self, gpu: GpuInfo) {
        // Evitar duplicados por funciones múltiples del mismo dispositivo (mismo bus:device)
        let bus = gpu.pci_device.bus;
        let dev = gpu.pci_device.device;
        let already_present = self.gpus[..self.gpu_count]
            .iter()
            .filter_map(|g| g.as_ref())
            .any(|g| g.pci_device.bus == bus && g.pci_device.device == dev);
        if already_present { return; }

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
    
    /// Obtener número total de GPUs
    pub fn gpu_count(&self) -> usize {
        self.gpu_count
    }
    
    /// Obtener información de un dispositivo específico
    pub fn get_device(&self, index: usize) -> Option<&PciDevice> {
        self.devices.get(index)?.as_ref()
    }
}

// IO de puertos 32-bit
#[inline(always)]
unsafe fn io_outl(port: u16, value: u32) {
    asm!(
        "out dx, eax",
        in("dx") port,
        in("eax") value,
        options(nomem, nostack, preserves_flags)
    );
}

#[inline(always)]
unsafe fn io_inl(port: u16) -> u32 {
    let value: u32;
    asm!(
        "in eax, dx",
        in("dx") port,
        out("eax") value,
        options(nomem, nostack, preserves_flags)
    );
    value
}

/// Función de conveniencia para escanear PCI
pub fn scan_pci_devices() -> PciManager {
    let mut manager = PciManager::new();
    manager.scan_devices();
    manager
}