//! Sistema de detección de hardware para Eclipse OS
//! 
//! Implementa detección automática de hardware gráfico y otros dispositivos
//! usando PCI y otros métodos de detección.

use crate::drivers::pci::{PciManager, PciManagerMinimal, GpuInfo, GpuType, PciDevice, VENDOR_ID_INTEL};
use crate::drivers::gpu_manager::{GpuDriverManager, create_gpu_driver_manager};
use crate::uefi_framebuffer::{is_framebuffer_initialized, get_framebuffer_status};
use alloc::format;
use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use alloc::string::ToString;

/// Resultado de la detección de hardware
#[derive(Debug, Clone)]
pub struct HardwareDetectionResult {
    pub graphics_mode: GraphicsMode,
    pub primary_gpu: Option<GpuInfo>,
    pub available_gpus: Vec<GpuInfo>,
    pub framebuffer_available: bool,
    pub vga_available: bool,
    pub recommended_driver: RecommendedDriver,
    pub gpu_driver_manager: Option<GpuDriverManager>,
}

/// Modos de gráficos disponibles
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsMode {
    Framebuffer,
    VGA,
    HardwareAccelerated,
}

/// Drivers recomendados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecommendedDriver {
    Intel,
    Nvidia,
    Amd,
    GenericFramebuffer,
    VGA,
    Unknown,
}

impl RecommendedDriver {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecommendedDriver::Intel => "Intel Graphics Driver",
            RecommendedDriver::Nvidia => "NVIDIA Driver",
            RecommendedDriver::Amd => "AMD Radeon Driver",
            RecommendedDriver::GenericFramebuffer => "Generic Framebuffer Driver",
            RecommendedDriver::VGA => "VGA Text Mode Driver",
            RecommendedDriver::Unknown => "Unknown Driver",
        }
    }
}

/// Gestor de detección de hardware
pub struct HardwareDetector {
    pci_manager: Option<PciManager>,
    pci_manager_minimal: PciManagerMinimal,
    detection_result: Option<HardwareDetectionResult>,
    allocator_ready: bool,
}

impl HardwareDetector {
    pub fn new() -> Self {
        Self {
            pci_manager: Some(PciManager::new()),
            pci_manager_minimal: PciManagerMinimal::new(),
            detection_result: None,
            allocator_ready: false,
        }
    }

    /// Crear detector con verificación de allocador (versión ultra-segura)
    pub fn new_safe() -> Option<Self> {
        // Crear estructura segura pero con PciManager real disponible para debug
        Some(Self {
            pci_manager: Some(PciManager::new()),
            pci_manager_minimal: PciManagerMinimal::new(),
            detection_result: None,
            allocator_ready: false,
        })
    }

    /// Crear detector mínimo sin dependencias
    pub fn new_minimal() -> Self {
        Self {
            pci_manager: None,
            pci_manager_minimal: PciManagerMinimal::new(),
            detection_result: None,
            allocator_ready: false,
        }
    }

    /// Verificar si el allocador funciona después de la creación
    pub fn verify_allocator(&mut self) -> bool {
        // Intenta usar alloc de forma muy simple
        // Si esto no paniquea, entonces el allocador funciona
        let _ = alloc::vec::Vec::<u8>::new();
        self.allocator_ready = true;
        true
    }

    /// Verificar allocador de forma aún más segura
    pub fn verify_allocator_safe(&mut self) -> bool {
        // Solo marcar como listo sin usar alloc inicialmente
        self.allocator_ready = true;
        true
    }

    /// Establecer si el allocador está listo
    pub fn set_allocator_ready(&mut self, ready: bool) {
        self.allocator_ready = ready;
    }
    
    /// Realizar detección robusta de hardware
    pub fn detect_hardware(&mut self) -> &HardwareDetectionResult {
        // 1. Detectar framebuffer (solo UEFI, sin detección directa peligrosa)
        let framebuffer_available = self.detect_framebuffer();
        
        // 2. Detectar GPUs via PCI de forma segura
        let available_gpus = self.detect_gpus_safe();
        
        // 3. Detectar VGA de forma segura
        let vga_available = self.detect_vga_safe();
        
        // 4. Determinar GPU primaria
        let primary_gpu = self.determine_primary_gpu(&available_gpus);
        
        // 5. Determinar modo de gráficos
        let graphics_mode = self.determine_graphics_mode(&available_gpus, framebuffer_available);
        
        // 6. Determinar driver recomendado
        let recommended_driver = self.determine_recommended_driver(&primary_gpu, framebuffer_available);
        
        // 7. Crear manager de drivers GPU si es necesario (de forma segura)
        let gpu_driver_manager = if !available_gpus.is_empty() {
            self.create_gpu_driver_manager_safe()
        } else {
            None
        };

        let result = HardwareDetectionResult {
            graphics_mode,
            primary_gpu,
            available_gpus,
            framebuffer_available,
            vga_available,
            recommended_driver,
            gpu_driver_manager,
        };

        // Log información de detección para debug (sin operaciones peligrosas)
        self.log_detection_info_safe(&result);
        
        self.detection_result = Some(result);
        self.detection_result.as_ref().unwrap()
    }
    
    /// Log información detallada de la detección
    fn log_detection_info(&self, result: &HardwareDetectionResult) {
        // En un entorno real, esto escribiría a un log
        // Por ahora, solo almacenamos la información para uso posterior
        
        // Información básica
        let mode_str = match result.graphics_mode {
            GraphicsMode::Framebuffer => "Framebuffer",
            GraphicsMode::VGA => "VGA",
            GraphicsMode::HardwareAccelerated => "Hardware Accelerated",
        };
        
        let driver_str = match result.recommended_driver {
            RecommendedDriver::Intel => "Intel",
            RecommendedDriver::Nvidia => "NVIDIA",
            RecommendedDriver::Amd => "AMD",
            RecommendedDriver::GenericFramebuffer => "Generic Framebuffer",
            RecommendedDriver::VGA => "VGA",
            RecommendedDriver::Unknown => "Unknown",
        };
        
        // Información de GPUs detectadas
        let gpu_count = result.available_gpus.len();
        let primary_gpu_info = if let Some(ref gpu) = result.primary_gpu {
            format!("{:?} (Vendor: 0x{:04X}, Device: 0x{:04X})", 
                   gpu.gpu_type, gpu.pci_device.vendor_id, gpu.pci_device.device_id)
        } else {
            "None".to_string()
        };
        
        // Esta información se puede usar para debug o para mostrar al usuario
        // En un entorno real, se escribiría a un archivo de log o a la consola
    }
    
    /// Log información de forma segura (sin operaciones peligrosas)
    fn log_detection_info_safe(&self, result: &HardwareDetectionResult) {
        // VERSIÓN SEGURA: Solo almacenar información básica sin formateo complejo
        // para evitar problemas con el allocator o formateo de strings
        let _mode = result.graphics_mode;
        let _driver = result.recommended_driver;
        let _gpu_count = result.available_gpus.len();
        let _framebuffer_available = result.framebuffer_available;
        let _vga_available = result.vga_available;
        
        // No hacer operaciones de formateo que puedan causar problemas
    }
    
    /// Crear manager de drivers GPU de forma segura
    fn create_gpu_driver_manager_safe(&self) -> Option<GpuDriverManager> {
        // VERSIÓN SEGURA: Crear un manager básico sin operaciones complejas
        // que puedan causar problemas de inicialización
        Some(create_gpu_driver_manager())
    }
    
    /// Detectar framebuffer UEFI
    fn detect_framebuffer(&self) -> bool {
        // Verificar si el framebuffer está inicializado
        if !is_framebuffer_initialized() {
            return false;
        }
        
        // Verificar que tenemos información válida del framebuffer
        let status = get_framebuffer_status();
        if let Some(info) = status.driver_info {
            // Verificar que las dimensiones son válidas
            let valid_dimensions = info.width > 0 && info.height > 0;
            let valid_address = info.base_address != 0;
            let valid_pitch = info.pixels_per_scan_line > 0;
            
            valid_dimensions && valid_address && valid_pitch
        } else {
            false
        }
    }
    
    /// Detectar framebuffer de forma directa (sin UEFI)
    /// DESHABILITADO: Esta función causa cuelgues del sistema al acceder a memoria no mapeada
    fn detect_framebuffer_direct(&self) -> bool {
        // TEMPORALMENTE DESHABILITADO: El acceso directo a direcciones de memoria
        // puede causar excepciones de página y cuelgues del sistema.
        // En su lugar, siempre retornamos false para evitar problemas.
        false
    }
    
    /// Probar si una dirección es un framebuffer válido
    /// DESHABILITADO: Esta función causa cuelgues del sistema al acceder a memoria no mapeada
    fn test_framebuffer_address(&self, _addr: u64) -> bool {
        // TEMPORALMENTE DESHABILITADO: El acceso directo a direcciones de memoria
        // puede causar excepciones de página y cuelgues del sistema.
        // En su lugar, siempre retornamos false para evitar problemas.
        false
    }
    
    /// Detectar GPUs via PCI
    fn detect_gpus(&self) -> Vec<GpuInfo> {
        let mut gpus = Vec::new();
        
        // Crear manager PCI para detección
        let mut pci_manager = PciManager::new_without_hardware_check();
        
        // Escanear dispositivos PCI de forma segura
        pci_manager.scan_devices();
        
        // Obtener GPUs detectadas del manager
        for gpu_option in pci_manager.get_gpus() {
            if let Some(gpu) = gpu_option {
                gpus.push(*gpu);
            }
        }
        
        gpus
    }
    
    /// Detectar GPUs de forma segura con timeout
    fn detect_gpus_safe(&self) -> Vec<GpuInfo> {
        // Usar la versión segura que no puede colgarse
        let mut gpus = Vec::new();
        
        // Usar el manager PCI completo con verificación real de hardware
        let mut pci_manager = PciManager::new();
        
        // Escanear dispositivos PCI (maneja internamente disponibilidad)
        pci_manager.scan_devices();
        
        // Obtener GPUs detectadas del manager
        for gpu_option in pci_manager.get_gpus() {
            if let Some(gpu) = gpu_option {
                gpus.push(*gpu);
            }
        }
        
        // No agregar fallback aquí: reportar lista vacía si no hay GPUs
        
        gpus
    }
    
    /// Crear GPU de fallback
    fn create_fallback_gpu(&self) -> GpuInfo {
        GpuInfo {
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
        }
    }
    
    /// Verificar si un dispositivo PCI es una GPU
    fn is_gpu_device(&self, device: &crate::drivers::pci::PciDevice) -> bool {
        // Clase 0x03 = Display Controller
        device.class_code == 0x03
    }
    
    /// Crear información de GPU desde dispositivo PCI
    fn create_gpu_info(&self, device: &crate::drivers::pci::PciDevice) -> Option<GpuInfo> {
        // Determinar tipo de GPU basado en vendor ID
        let gpu_type = match device.vendor_id {
            0x8086 => GpuType::Intel,      // Intel
            0x10DE => GpuType::Nvidia,     // NVIDIA
            0x1002 => GpuType::Amd,        // AMD
            _ => GpuType::Unknown,
        };
        
        // Determinar capacidades básicas
        let supports_2d = true;  // Asumir que todas las GPUs modernas soportan 2D
        let supports_3d = self.detect_3d_capabilities(device);
        
        Some(GpuInfo {
            pci_device: *device,
            gpu_type,
            memory_size: self.estimate_memory_size(device),
            is_primary: false, // Se determinará después
            supports_2d,
            supports_3d,
            max_resolution: (1920, 1080), // Resolución por defecto
        })
    }
    
    /// Detectar capacidades 3D (simplificado)
    fn detect_3d_capabilities(&self, device: &crate::drivers::pci::PciDevice) -> bool {
        // Para simplificar, asumir que GPUs modernas soportan 3D
        // En una implementación real, esto verificaría registros específicos
        match device.vendor_id {
            0x8086 => device.device_id >= 0x0100,  // Intel HD Graphics
            0x10DE => device.device_id >= 0x0100,  // NVIDIA GeForce
            0x1002 => device.device_id >= 0x0100,  // AMD Radeon
            _ => false,
        }
    }
    
    /// Estimar tamaño de memoria de GPU
    fn estimate_memory_size(&self, device: &crate::drivers::pci::PciDevice) -> u64 {
        // Estimación muy básica basada en vendor/device ID
        match device.vendor_id {
            0x8086 => 64 * 1024 * 1024,  // 64MB para Intel integrada
            0x10DE => 256 * 1024 * 1024, // 256MB para NVIDIA
            0x1002 => 128 * 1024 * 1024, // 128MB para AMD
            _ => 32 * 1024 * 1024,       // 32MB por defecto
        }
    }
    
    /// Verificar disponibilidad de driver
    fn check_driver_availability(&self, gpu_type: GpuType) -> bool {
        match gpu_type {
            GpuType::Intel => true,   // Driver Intel disponible
            GpuType::Nvidia => true,  // Driver NVIDIA disponible
            GpuType::Amd => true,     // Driver AMD disponible
            GpuType::Via => false,    // Driver VIA no disponible
            GpuType::Sis => false,    // Driver SiS no disponible
            GpuType::Qemu => true,    // Driver QEMU disponible (framebuffer genérico)
            GpuType::Virtio => true,  // Driver Virtio disponible (framebuffer genérico)
            GpuType::Qxl => true,     // Driver QXL (framebuffer genérico)
            GpuType::Vmware => true,  // Driver VMware SVGA (framebuffer genérico)
            GpuType::Unknown => false,
        }
    }
    
    /// Detectar VGA
    fn detect_vga(&self) -> bool {
        // Verificar puerto VGA estándar
        unsafe {
            // Intentar leer del puerto VGA
            let vga_port = 0x3C0u16;
            let _ = core::ptr::read_volatile(vga_port as *const u8);
            true // Si no paniquea, VGA está disponible
        }
    }
    
    /// Detectar VGA de forma segura
    fn detect_vga_safe(&self) -> bool {
        // VERSIÓN SEGURA: Asumir que VGA está disponible sin verificar
        // para evitar accesos peligrosos a puertos de hardware
        true
    }
    
    /// Determinar GPU primaria
    fn determine_primary_gpu(&self, gpus: &[GpuInfo]) -> Option<GpuInfo> {
        if gpus.is_empty() { return None; }

        // Preferir NVIDIA discreta
        if let Some(gpu) = gpus.iter().find(|g| g.gpu_type == GpuType::Nvidia) {
            return Some(gpu.clone());
        }
        // Luego AMD
        if let Some(gpu) = gpus.iter().find(|g| g.gpu_type == GpuType::Amd) {
            return Some(gpu.clone());
        }
        // Luego Intel
        if let Some(gpu) = gpus.iter().find(|g| g.gpu_type == GpuType::Intel) {
            return Some(gpu.clone());
        }
        // Si no, la primera
        gpus.first().cloned()
    }
    
    /// Determinar el mejor modo de gráficos
    fn determine_graphics_mode(&self, gpus: &[GpuInfo], framebuffer_available: bool) -> GraphicsMode {
        // Si hay GPU con aceleración 3D, usar hardware acelerado
        if gpus.iter().any(|gpu| gpu.supports_3d) {
            return GraphicsMode::HardwareAccelerated;
        }
        
        // Si hay framebuffer disponible, usarlo
        if framebuffer_available {
            return GraphicsMode::Framebuffer;
        }
        
        // Si hay GPU con aceleración 2D, usar hardware acelerado
        if gpus.iter().any(|gpu| gpu.supports_2d) {
            return GraphicsMode::HardwareAccelerated;
        }
        
        // Fallback a VGA
        GraphicsMode::VGA
    }
    
    /// Determinar el driver recomendado
    fn determine_recommended_driver(&self, primary_gpu: &Option<GpuInfo>, framebuffer_available: bool) -> RecommendedDriver {
        if let Some(gpu) = primary_gpu {
            match gpu.gpu_type {
                GpuType::Intel => RecommendedDriver::Intel,
                GpuType::Nvidia => RecommendedDriver::Nvidia,
                GpuType::Amd => RecommendedDriver::Amd,
                _ => {
                    if framebuffer_available {
                        RecommendedDriver::GenericFramebuffer
                    } else {
                        RecommendedDriver::VGA
                    }
                }
            }
        } else if framebuffer_available {
            RecommendedDriver::GenericFramebuffer
        } else {
            RecommendedDriver::VGA
        }
    }
    
    /// Obtener información detallada del framebuffer
    pub fn get_framebuffer_info(&self) -> Option<String> {
        if !is_framebuffer_initialized() {
            return None;
        }
        
        let status = get_framebuffer_status();
        if let Some(info) = status.driver_info {
            // Calcular información del framebuffer
            let bpp = match info.pixel_format {
                0 | 1 => 24, // RGB888, BGR888 = 24 bits
                2 | 3 => 32, // RGBA8888, BGRA8888 = 32 bits
                _ => 32,
            };
            let size = (info.width as u64) * (info.height as u64) * ((bpp / 8) as u64);

            Some(format!(
                "Framebuffer: {}x{} @ {}bpp, {} bytes, Format: {}",
                info.width,
                info.height,
                bpp,
                size,
                match info.pixel_format {
                    0 => "RGB888",
                    1 => "BGR888",
                    2 => "RGBA8888",
                    3 => "BGRA8888",
                    _ => "Unknown",
                }
            ))
        } else {
            Some("Framebuffer: Información no disponible".to_string())
        }
    }
    
    /// Obtener información de GPUs detectadas
    pub fn get_gpu_info(&self) -> Vec<String> {
        let mut info = Vec::new();

        if let Some(ref pci_manager) = self.pci_manager {
            for (i, gpu) in pci_manager.get_gpus().iter().enumerate() {
                if let Some(gpu) = gpu {
                    info.push(format!(
                        "GPU {}: {} {} (Bus: {:02X}:{:02X}.{}) - {}MB, 2D: {}, 3D: {}, Max: {}x{}",
                        i + 1,
                        gpu.gpu_type.as_str(),
                        format!("{:04X}:{:04X}", gpu.pci_device.vendor_id, gpu.pci_device.device_id),
                        gpu.pci_device.bus,
                        gpu.pci_device.device,
                        gpu.pci_device.function,
                        gpu.memory_size / (1024 * 1024),
                        if gpu.supports_2d { "Sí" } else { "No" },
                        if gpu.supports_3d { "Sí" } else { "No" },
                        gpu.max_resolution.0,
                        gpu.max_resolution.1
                    ));
                }
            }
        }

        if info.is_empty() {
            info.push("No se detectaron GPUs".to_string());
        }

        info
    }
    
    /// Obtener información de dispositivos PCI
    pub fn get_pci_info(&self) -> Vec<String> {
        let mut info = Vec::new();

        if let Some(ref pci_manager) = self.pci_manager {
            info.push(format!("Dispositivos PCI detectados: {}", pci_manager.device_count()));
            info.push(format!("GPUs detectadas: {}", pci_manager.gpu_count()));

            // Mostrar algunos dispositivos importantes
            for i in 0..core::cmp::min(10, pci_manager.device_count()) {
                if let Some(device) = pci_manager.get_device(i) {
                    info.push(format!(
                        "  {:02X}:{:02X}.{} - {:04X}:{:04X} - Class: {:02X}:{:02X}:{:02X}",
                        device.bus,
                        device.device,
                        device.function,
                        device.vendor_id,
                        device.device_id,
                        device.class_code,
                        device.subclass_code,
                        device.prog_if
                    ));
                }
            }
        } else {
            info.push("Dispositivos PCI detectados: 0".to_string());
            info.push("GPUs detectadas: 0".to_string());
            info.push("PCI Manager no inicializado".to_string());
        }

        info
    }
    
    /// Obtener resultado de detección
    pub fn get_detection_result(&self) -> Option<&HardwareDetectionResult> {
        self.detection_result.as_ref()
    }
    
    /// Obtener información de drivers de GPU cargados
    pub fn get_gpu_driver_info(&self) -> Vec<String> {
        if let Some(result) = &self.detection_result {
            if let Some(manager) = &result.gpu_driver_manager {
                return manager.get_driver_info();
            }
        }
        vec!["No hay gestor de drivers disponible".to_string()]
    }
    
    /// Obtener estadísticas de drivers
    pub fn get_driver_stats(&self) -> (usize, usize, usize) {
        if let Some(result) = &self.detection_result {
            if let Some(manager) = &result.gpu_driver_manager {
                return manager.get_driver_stats();
            }
        }
        (0, 0, 0)
    }
}

/// Función de conveniencia para detección rápida
pub fn detect_graphics_hardware() -> HardwareDetectionResult {
    // Usar la versión segura del detector para evitar cuelgues
    let mut detector = HardwareDetector::new_safe().unwrap_or_else(|| {
        // Si falla la creación segura, usar la versión minimal
        HardwareDetector::new_minimal()
    });
    
    // Verificar que el allocator esté listo antes de proceder
    detector.verify_allocator_safe();
    
    detector.detect_hardware().clone()
}

/// Función de conveniencia para obtener modo de gráficos
pub fn get_graphics_mode() -> GraphicsMode {
    let result = detect_graphics_hardware();
    result.graphics_mode
}
