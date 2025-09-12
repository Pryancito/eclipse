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
        // Intentar leer el registro de configuración PCI
        // Si falla, el hardware PCI no está disponible
        unsafe {
            // Intentar leer vendor ID de un dispositivo conocido
            let test_address = 0x80000000u32 | (0 << 16) | (0 << 11) | (0 << 8);
            ptr::write_volatile(PCI_CONFIG_ADDRESS as *mut u32, test_address);
            let result = ptr::read_volatile(PCI_CONFIG_DATA as *mut u32);

            // Si podemos leer algo que no sea todo 1s, el hardware está disponible
            result != 0xFFFFFFFF
        }
    }
    
    /// Escanear todos los dispositivos PCI
    pub fn scan_devices(&mut self) {
        self.device_count = 0;
        self.gpu_count = 0;

        // Solo escanear si el hardware PCI está disponible
        if !self.hardware_available {
            return;
        }

        // Escanear buses principales primero (0-31) para mayor eficiencia
        // Limitar a buses razonables para evitar bloqueos
        let max_bus = if cfg!(target_arch = "x86_64") { 31 } else { 255 };
        let mut scan_count = 0;
        const MAX_SCAN_ATTEMPTS: usize = 10000; // Límite de seguridad

        for bus in 0..(max_bus + 1) {
            // Escanear todos los dispositivos en el bus (0-31)
            for device in 0..32 {
                // Solo escanear función 0 primero para dispositivos multi-función
                for function in 0..8 {
                    // Límite de seguridad para evitar bloqueos infinitos
                    scan_count += 1;
                    if scan_count > MAX_SCAN_ATTEMPTS {
                        return;
                    }

                    if let Some(pci_device) = self.read_pci_device(bus, device, function) {
                        // Verificar si es un dispositivo válido
                        if pci_device.vendor_id != 0xFFFF {
                            self.add_device(pci_device);

                            // Si es una GPU, agregarla a la lista
                            if self.is_gpu_device(&pci_device) {
                                if let Some(gpu_info) = self.create_gpu_info(pci_device) {
                                    self.add_gpu(gpu_info);
                                }
                            }

                            // Si no es función 0, verificar si es multi-función
                            if function == 0 && (pci_device.header_type & 0x80) != 0 {
                                // Es multi-función, continuar escaneando
                                continue;
                            } else if function > 0 {
                                // Es función adicional, continuar
                                continue;
                            }
                        }
                    } else {
                        // Si función 0 no existe, no buscar más funciones
                        if function == 0 {
                            break;
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
            // Escribir dirección con verificación de seguridad
            ptr::write_volatile(PCI_CONFIG_ADDRESS as *mut u32, address);

            // TEMPORALMENTE DESHABILITADO: nop en loop podría causar opcode inválido
            // Pequeña pausa para estabilidad (simulada)
            for _ in 0..10 {
                // Simular nop con spin loop
                core::hint::spin_loop();
            }

            // Leer datos
            ptr::read_volatile(PCI_CONFIG_DATA as *mut u32)
        }
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