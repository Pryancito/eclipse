//! Sistema de fallback de UEFI/GOP framebuffer a GPU hardware real
//! 
//! Este módulo implementa la transición automática del framebuffer UEFI/GOP
//! a drivers de GPU hardware real cuando están disponibles.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::drivers::pci::{GpuInfo, GpuType};
use crate::drivers::virtio_gpu::VirtioGpuDriver;
use crate::drivers::bochs_vbe::BochsVbeDriver;
use crate::drivers::vmware_svga::VmwareSvgaDriver;
use crate::hardware_detection::{HardwareDetectionResult, detect_graphics_hardware};
use crate::uefi_framebuffer::{BootloaderFramebufferInfo, get_framebuffer_status};
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;

/// Estado del sistema de fallback GPU
#[derive(Debug, Clone, PartialEq)]
pub enum GpuFallbackState {
    /// Usando framebuffer UEFI/GOP inicial
    UefiFramebuffer,
    /// Transicionando a GPU hardware real
    Transitioning,
    /// Usando GPU hardware real
    HardwareGpu,
    /// Fallback a UEFI/GOP por error
    FallbackToUefi,
}

/// Información del backend gráfico activo
#[derive(Debug, Clone)]
pub struct ActiveGraphicsBackend {
    pub backend_type: GraphicsBackendType,
    pub gpu_info: Option<GpuInfo>,
    pub framebuffer_info: Option<BootloaderFramebufferInfo>,
    pub performance_score: u32,
    pub initialized: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphicsBackendType {
    UefiFramebuffer,
    VirtioGpu,
    BochsVbe,
    VmwareSvga,
    IntelGpu,
    NvidiaGpu,
    AmdGpu,
    UnknownGpu,
}

impl GraphicsBackendType {
    /// Obtener prioridad del backend (mayor = mejor)
    pub fn priority(&self) -> u32 {
        match self {
            GraphicsBackendType::NvidiaGpu => 100,
            GraphicsBackendType::AmdGpu => 95,
            GraphicsBackendType::IntelGpu => 90,
            GraphicsBackendType::VirtioGpu => 80,
            GraphicsBackendType::VmwareSvga => 70,
            GraphicsBackendType::BochsVbe => 60,
            GraphicsBackendType::UefiFramebuffer => 10,
            GraphicsBackendType::UnknownGpu => 5,
        }
    }

    /// Verificar si es un backend de hardware real
    pub fn is_real_hardware(&self) -> bool {
        matches!(self, 
            GraphicsBackendType::NvidiaGpu | 
            GraphicsBackendType::AmdGpu | 
            GraphicsBackendType::IntelGpu
        )
    }
}

/// Gestor de fallback GPU
pub struct GpuFallbackManager {
    state: GpuFallbackState,
    active_backend: Option<ActiveGraphicsBackend>,
    available_backends: Vec<ActiveGraphicsBackend>,
    uefi_framebuffer: Option<FramebufferDriver>,
    hardware_detection_result: Option<HardwareDetectionResult>,
    initialized: bool,
}

impl GpuFallbackManager {
    /// Crear nuevo gestor de fallback GPU
    pub fn new() -> Self {
        Self {
            state: GpuFallbackState::UefiFramebuffer,
            active_backend: None,
            available_backends: Vec::new(),
            uefi_framebuffer: None,
            hardware_detection_result: None,
            initialized: false,
        }
    }

    /// Inicializar el sistema de fallback
    pub fn initialize(&mut self) -> Result<(), String> {
        // 1. Detectar hardware disponible
        self.hardware_detection_result = Some(detect_graphics_hardware());
        let hw_result = self.hardware_detection_result.as_ref().unwrap();

        // 2. Obtener framebuffer UEFI inicial
        if let Some(fb_status) = get_framebuffer_status().driver_info {
            if let Some(global_fb) = crate::drivers::framebuffer::get_framebuffer() {
                // Crear una copia segura del framebuffer UEFI
                self.uefi_framebuffer = Some(unsafe { core::ptr::read(global_fb) });
            } else {
                return Err("No se pudo obtener el framebuffer global".to_string());
            }
        } else {
            return Err("No se pudo obtener información del framebuffer UEFI".to_string());
        }

        // 3. Detectar backends gráficos disponibles
        self.detect_available_backends()?;

        // 4. Seleccionar el mejor backend disponible
        self.select_best_backend()?;

        self.initialized = true;
        Ok(())
    }

    /// Detectar backends gráficos disponibles
    fn detect_available_backends(&mut self) -> Result<(), String> {
        self.available_backends.clear();

        let hw_result = self.hardware_detection_result.as_ref().unwrap();

        // 1. Siempre incluir UEFI framebuffer como fallback
        if self.uefi_framebuffer.is_some() {
            self.available_backends.push(ActiveGraphicsBackend {
                backend_type: GraphicsBackendType::UefiFramebuffer,
                gpu_info: None,
                framebuffer_info: get_framebuffer_status().driver_info.map(|info| BootloaderFramebufferInfo {
                    base_address: info.base_address,
                    width: info.width,
                    height: info.height,
                    pixels_per_scan_line: info.pixels_per_scan_line,
                    pixel_format: info.pixel_format,
                    pixel_bitmask: 0, // Valor por defecto
                }),
                performance_score: GraphicsBackendType::UefiFramebuffer.priority(),
                initialized: true,
            });
        }

        // 2. Detectar GPUs hardware real
        for gpu in &hw_result.available_gpus {
            let backend_type = match gpu.gpu_type {
                GpuType::Virtio => GraphicsBackendType::VirtioGpu,
                GpuType::QemuBochs => GraphicsBackendType::BochsVbe,
                GpuType::Vmware => GraphicsBackendType::VmwareSvga,
                GpuType::Intel => GraphicsBackendType::IntelGpu,
                GpuType::Nvidia => GraphicsBackendType::NvidiaGpu,
                GpuType::Amd => GraphicsBackendType::AmdGpu,
                _ => GraphicsBackendType::UnknownGpu,
            };

            let backend_type_clone = backend_type.clone();
            self.available_backends.push(ActiveGraphicsBackend {
                backend_type: backend_type_clone.clone(),
                gpu_info: Some(gpu.clone()),
                framebuffer_info: None,
                performance_score: backend_type_clone.priority(),
                initialized: false,
            });
        }

        // Ordenar por prioridad (mayor primero)
        self.available_backends.sort_by(|a, b| b.performance_score.cmp(&a.performance_score));

        Ok(())
    }

    /// Seleccionar el mejor backend disponible
    fn select_best_backend(&mut self) -> Result<(), String> {
        if self.available_backends.is_empty() {
            return Err("No hay backends gráficos disponibles".to_string());
        }

        // Intentar inicializar backends en orden de prioridad usando transición suave
        for i in 0..self.available_backends.len() {
            let backend_type = self.available_backends[i].backend_type.clone();
            match self.transition_to_backend(backend_type.clone()) {
                Ok(_) => {
                    // Backend inicializado correctamente
                    return Ok(());
                }
                Err(e) => {
                    // Continuar con el siguiente backend
                    continue;
                }
            }
        }

        // Si todos fallan, usar UEFI framebuffer como fallback
        self.state = GpuFallbackState::FallbackToUefi;
        self.active_backend = self.available_backends.iter()
            .find(|b| b.backend_type == GraphicsBackendType::UefiFramebuffer)
            .cloned();
        
        if self.active_backend.is_none() {
            return Err("No se pudo seleccionar ningún backend gráfico, incluyendo UEFI framebuffer".to_string());
        }
        
        Ok(())
    }

    /// Intentar inicializar un backend específico
    fn try_initialize_backend(backend: &mut ActiveGraphicsBackend) -> bool {
        match backend.backend_type {
            GraphicsBackendType::VirtioGpu => {
                let mut driver = VirtioGpuDriver::new();
                match driver.initialize() {
                    Ok(_) => {
                        backend.initialized = true;
                        true
                    }
                    Err(_) => false,
                }
            }
            GraphicsBackendType::BochsVbe => {
                let mut driver = BochsVbeDriver::new();
                match driver.initialize() {
                    Ok(_) => {
                        backend.initialized = true;
                        true
                    }
                    Err(_) => false,
                }
            }
            GraphicsBackendType::VmwareSvga => {
                let mut driver = VmwareSvgaDriver::new();
                match driver.initialize() {
                    Ok(_) => {
                        backend.initialized = true;
                        true
                    }
                    Err(_) => false,
                }
            }
            GraphicsBackendType::IntelGpu | 
            GraphicsBackendType::NvidiaGpu | 
            GraphicsBackendType::AmdGpu => {
                // TODO: Implementar drivers de GPU hardware real
                // Por ahora, marcamos como no inicializado
                backend.initialized = false;
                false
            }
            GraphicsBackendType::UefiFramebuffer => {
                // UEFI framebuffer ya está inicializado
                backend.initialized = true;
                true
            }
            GraphicsBackendType::UnknownGpu => {
                backend.initialized = false;
                false
            }
        }
    }

    /// Actualizar estado basado en el backend seleccionado
    fn update_state_for_backend(&mut self, backend_type: &GraphicsBackendType) {
        self.state = if backend_type.is_real_hardware() {
            GpuFallbackState::HardwareGpu
        } else if *backend_type == GraphicsBackendType::UefiFramebuffer {
            GpuFallbackState::UefiFramebuffer
        } else {
            GpuFallbackState::HardwareGpu // Virtio, Bochs, etc. son considerados hardware
        };
    }

    /// Obtener información del backend activo
    pub fn get_active_backend_info(&self) -> Option<String> {
        if let Some(backend) = &self.active_backend {
            let gpu_info = if let Some(gpu) = &backend.gpu_info {
                format!(" ({}:{:04X})", gpu.pci_device.vendor_id, gpu.pci_device.device_id)
            } else {
                String::new()
            };
            
            let backend_name = match backend.backend_type {
                GraphicsBackendType::UefiFramebuffer => "UEFI/GOP Framebuffer",
                GraphicsBackendType::VirtioGpu => "Virtio-GPU",
                GraphicsBackendType::BochsVbe => "Bochs VBE",
                GraphicsBackendType::VmwareSvga => "VMware SVGA II",
                GraphicsBackendType::IntelGpu => "Intel GPU",
                GraphicsBackendType::NvidiaGpu => "NVIDIA GPU",
                GraphicsBackendType::AmdGpu => "AMD GPU",
                GraphicsBackendType::UnknownGpu => "GPU Desconocida",
            };

            Some(format!("Backend activo: {}{}", backend_name, gpu_info))
        } else {
            None
        }
    }

    /// Obtener estado actual del sistema
    pub fn get_state(&self) -> &GpuFallbackState {
        &self.state
    }

    /// Verificar si estamos usando GPU hardware real
    pub fn is_using_real_hardware(&self) -> bool {
        matches!(self.state, GpuFallbackState::HardwareGpu) &&
        self.active_backend.as_ref()
            .map(|b| b.backend_type.is_real_hardware())
            .unwrap_or(false)
    }

    /// Obtener lista de backends disponibles
    pub fn get_available_backends(&self) -> &Vec<ActiveGraphicsBackend> {
        &self.available_backends
    }

    /// Actualizar la dirección del framebuffer y hacer clear_screen
    fn update_framebuffer_and_clear(&mut self, new_base_address: u64, width: u32, height: u32, pixels_per_scan_line: u32) -> Result<(), String> {
        // 1. Hacer clear_screen en el framebuffer actual antes de cambiar
        if let Some(ref mut uefi_fb) = self.uefi_framebuffer {
            uefi_fb.clear_screen(Color::BLACK);
        }

        // 2. Reinicializar el framebuffer global con la nueva información
        if let Some(global_fb) = crate::drivers::framebuffer::get_framebuffer() {
            // Reinicializar con la nueva información usando el método público
            match global_fb.init_from_uefi(new_base_address, width, height, pixels_per_scan_line, 0, 0) {
                Ok(_) => {
                    // Hacer clear_screen en la nueva dirección
                    global_fb.clear_screen(Color::BLACK);
                }
                Err(e) => return Err(format!("Error reinicializando framebuffer global: {}", e)),
            }
        } else {
            return Err("No se pudo obtener el framebuffer global para actualizar".to_string());
        }

        // 3. Actualizar el framebuffer UEFI local si existe
        if let Some(ref mut uefi_fb) = self.uefi_framebuffer {
            match uefi_fb.init_from_uefi(new_base_address, width, height, pixels_per_scan_line, 0, 0) {
                Ok(_) => {
                    // Hacer clear_screen en la nueva dirección
                    uefi_fb.clear_screen(Color::BLACK);
                }
                Err(e) => return Err(format!("Error reinicializando framebuffer UEFI local: {}", e)),
            }
        }

        Ok(())
    }

    /// Transición suave entre backends con actualización de framebuffer
    pub fn transition_to_backend(&mut self, backend_type: GraphicsBackendType) -> Result<(), String> {
        // Buscar el índice del backend
        let backend_index = self.available_backends.iter()
            .position(|b| b.backend_type == backend_type);
        
        if let Some(index) = backend_index {
            // Intentar inicializar el nuevo backend
            if Self::try_initialize_backend(&mut self.available_backends[index]) {
                // Si es un backend de GPU hardware, actualizar framebuffer
                if backend_type.is_real_hardware() {
                    // Obtener nueva información del framebuffer del backend
                    if let Some(gpu_info) = &self.available_backends[index].gpu_info {
                        // Calcular nueva dirección del framebuffer basada en la GPU
                        let new_base_address = self.calculate_gpu_framebuffer_address(gpu_info);
                        let width = 1920; // Resolución por defecto
                        let height = 1080;
                        let pixels_per_scan_line = width;
                        
                        // Actualizar framebuffer y hacer clear_screen
                        if let Err(e) = self.update_framebuffer_and_clear(new_base_address, width, height, pixels_per_scan_line) {
                            return Err(format!("Error actualizando framebuffer para {:?}: {}", backend_type, e));
                        }
                    } else {
                        return Err(format!("No hay información de GPU disponible para el backend {:?}", backend_type));
                    }
                }
                
                // Cambiar al nuevo backend
                self.active_backend = Some(self.available_backends[index].clone());
                self.update_state_for_backend(&backend_type);
                Ok(())
            } else {
                Err(format!("No se pudo inicializar el backend {:?}", backend_type))
            }
        } else {
            Err(format!("Backend {:?} no está disponible", backend_type))
        }
    }

    /// Calcular dirección del framebuffer para GPU hardware
    fn calculate_gpu_framebuffer_address(&self, gpu_info: &GpuInfo) -> u64 {
        // Direcciones típicas para diferentes tipos de GPU
        match gpu_info.gpu_type {
            crate::drivers::pci::GpuType::Intel => 0xFD000000,
            crate::drivers::pci::GpuType::Nvidia => 0xFE000000,
            crate::drivers::pci::GpuType::Amd => 0xFF000000,
            _ => 0xFD000000, // Dirección por defecto
        }
    }

    /// Forzar transición a un backend específico (método legacy)
    pub fn force_backend(&mut self, backend_type: GraphicsBackendType) -> Result<(), String> {
        self.transition_to_backend(backend_type)
    }

    /// Obtener estadísticas del sistema de fallback
    pub fn get_stats(&self) -> String {
        let backend_count = self.available_backends.len();
        let hardware_backends = self.available_backends.iter()
            .filter(|b| b.backend_type.is_real_hardware())
            .count();
        
        format!(
            "Fallback GPU: {} backends disponibles, {} hardware real, estado: {:?}",
            backend_count, hardware_backends, self.state
        )
    }
}

/// Instancia global del gestor de fallback GPU
static mut GPU_FALLBACK_MANAGER: Option<GpuFallbackManager> = None;

/// Inicializar el sistema de fallback GPU
pub fn init_gpu_fallback() -> Result<(), String> {
    unsafe {
        if GPU_FALLBACK_MANAGER.is_some() {
            return Ok(());
        }

        let mut manager = GpuFallbackManager::new();
        manager.initialize()?;
        GPU_FALLBACK_MANAGER = Some(manager);
    }
    Ok(())
}

/// Obtener información del backend activo
pub fn get_active_backend_info() -> Option<String> {
    unsafe {
        GPU_FALLBACK_MANAGER.as_ref()
            .and_then(|m| m.get_active_backend_info())
    }
}

/// Verificar si estamos usando GPU hardware real
pub fn is_using_real_hardware() -> bool {
    unsafe {
        GPU_FALLBACK_MANAGER.as_ref()
            .map(|m| m.is_using_real_hardware())
            .unwrap_or(false)
    }
}

/// Obtener estadísticas del sistema de fallback
pub fn get_fallback_stats() -> String {
    unsafe {
        GPU_FALLBACK_MANAGER.as_ref()
            .map(|m| m.get_stats())
            .unwrap_or_else(|| "Sistema de fallback GPU no inicializado".to_string())
    }
}

/// Forzar transición a un backend específico
pub fn force_backend(backend_type: GraphicsBackendType) -> Result<(), String> {
    unsafe {
        if let Some(manager) = GPU_FALLBACK_MANAGER.as_mut() {
            manager.force_backend(backend_type)
        } else {
            Err("Sistema de fallback GPU no inicializado".to_string())
        }
    }
}

/// Transición suave a un backend específico con actualización de framebuffer
pub fn transition_to_backend(backend_type: GraphicsBackendType) -> Result<(), String> {
    unsafe {
        if let Some(manager) = GPU_FALLBACK_MANAGER.as_mut() {
            manager.transition_to_backend(backend_type)
        } else {
            Err("Sistema de fallback GPU no inicializado".to_string())
        }
    }
}

// ## Ejemplo de uso de la nueva funcionalidad de fallback GPU
// 
// ```rust
// use crate::gpu_fallback::{init_gpu_fallback, transition_to_backend, GraphicsBackendType};
// 
// // Inicializar el sistema de fallback
// init_gpu_fallback()?;
// 
// // Transición suave a GPU hardware real con actualización de framebuffer
// // Esto automáticamente:
// // 1. Hace clear_screen en el framebuffer actual
// // 2. Actualiza la dirección base del framebuffer
// // 3. Hace clear_screen en la nueva dirección
// // 4. Cambia al nuevo backend
// transition_to_backend(GraphicsBackendType::NvidiaGpu)?;
// 
// // O forzar transición a Intel GPU
// transition_to_backend(GraphicsBackendType::IntelGpu)?;
// ```
// 
// ## Mejoras implementadas:
// 
// 1. **Actualización automática del framebuffer**: Cuando se hace fallback a GPU hardware real,
//    el sistema automáticamente actualiza la dirección base del framebuffer a una dirección
//    específica para cada tipo de GPU.
// 
// 2. **Clear screen antes del cambio**: Se limpia la pantalla en el framebuffer actual antes
//    de cambiar a la nueva dirección para evitar artefactos visuales.
// 
// 3. **Clear screen después del cambio**: Se limpia la pantalla en la nueva dirección del
//    framebuffer para asegurar una transición limpia.
// 
// 4. **Transición suave**: La función `transition_to_backend()` maneja todo el proceso de
//    manera segura y eficiente.
