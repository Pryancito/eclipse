//! Driver PCI para Eclipse OS
//! 
//! Implementa detección y configuración de dispositivos PCI
//! para identificar hardware gráfico y otros dispositivos.

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

/// Clases de dispositivos PCI
pub const CLASS_DISPLAY: u8 = 0x03;
pub const SUBCLASS_VGA: u8 = 0x00;
pub const SUBCLASS_3D: u8 = 0x02;

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
            // Escribir dirección
            ptr::write_volatile(PCI_CONFIG_ADDRESS as *mut u32, address);
            // Leer datos
            ptr::read_volatile(PCI_CONFIG_DATA as *mut u32)
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
            // Escribir dirección
            ptr::write_volatile(PCI_CONFIG_ADDRESS as *mut u32, address);
            // Escribir datos
            ptr::write_volatile(PCI_CONFIG_DATA as *mut u32, value);
        }
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
            let address = 0x80000000u32
                | ((bus as u32) << 16)
                | ((device as u32) << 11)
                | ((function as u32) << 8)
                | ((offset as u32) & 0xFC);
            
            // Escribir dirección
            ptr::write_volatile(PCI_CONFIG_ADDRESS as *mut u32, address);
            
            // Pequeña pausa para estabilidad
            for _ in 0..10 {
                core::hint::spin_loop();
            }
            
            // Leer datos
            let result = ptr::read_volatile(PCI_CONFIG_DATA as *mut u32);
            
            // Verificar que la lectura fue válida
            if result == 0xFFFFFFFF && Self::vendor_id_from_result(result) == 0xFFFF {
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

        // Verificar si PCI está disponible
        if !self.hardware_available {
            self.create_fallback_gpu();
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
        
        // Si no se encontraron GPUs reales, crear una de fallback
        if self.gpu_count == 0 {
            self.create_fallback_gpu();
        }
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
                if memory_size == 0 { memory_size = 1024 * 1024 * 1024; }
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
    
    /// Estimar el tamaño de un BAR
    fn estimate_bar_size(&self, bar: u32) -> u64 {
        // Implementación simplificada para estimar el tamaño del BAR
        // En una implementación real, esto sería más complejo
        if bar == 0 || bar == 0xFFFFFFFF {
            return 0;
        }
        
        // Estimación básica basada en el valor del BAR
        let size = (bar & 0xFFFFFFF0) as u64;
        if size > 0 {
            size.max(1024 * 1024) // Mínimo 1MB
        } else {
            0
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

/// Función de conveniencia para escanear PCI
pub fn scan_pci_devices() -> PciManager {
    let mut manager = PciManager::new();
    manager.scan_devices();
    manager
}