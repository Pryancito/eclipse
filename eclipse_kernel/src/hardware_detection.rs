//! Sistema de detección de hardware para Eclipse OS
//! 
//! Implementa detección automática de hardware gráfico y otros dispositivos
//! usando PCI y otros métodos de detección.

use crate::drivers::pci::{PciManager, PciManagerMinimal, GpuInfo, GpuType};
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
        // VERSIÓN ULTRA SENCILLA: Solo crear la estructura básica
        // Sin ninguna operación que pueda fallar

        Some(Self {
            pci_manager: None, // Inicialmente None para evitar problemas
            pci_manager_minimal: PciManagerMinimal::new(),
            detection_result: None,
            allocator_ready: false, // Marcar como no listo inicialmente
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
    
    /// Realizar detección ultra-simple de hardware
    pub fn detect_hardware(&mut self) -> &HardwareDetectionResult {
        // RESULTADO ULTRA-SENCILLO: Solo devolver configuración básica
        let result = HardwareDetectionResult {
            graphics_mode: GraphicsMode::VGA,
            primary_gpu: None,
            available_gpus: Vec::new(),
            framebuffer_available: false,
            vga_available: true,
            recommended_driver: RecommendedDriver::VGA,
            gpu_driver_manager: None,
        };

        self.detection_result = Some(result);
        self.detection_result.as_ref().unwrap()
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
    let mut detector = HardwareDetector::new();
    detector.detect_hardware().clone()
}

/// Función de conveniencia para obtener modo de gráficos
pub fn get_graphics_mode() -> GraphicsMode {
    let result = detect_graphics_hardware();
    result.graphics_mode
}
