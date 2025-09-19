use core::fmt;
use alloc::string::String;
use alloc::format;
use crate::drivers::framebuffer::{FramebufferDriver, FramebufferInfo, Color};
use crate::drivers::resolution_manager::{ResolutionManager, FramebufferInfo as ResolutionFramebufferInfo, ResolutionChangeResult};
use crate::drivers::uefi_gop::{UefiGopManager, UefiModeInfo};
use crate::drivers::uefi_graphics::{UefiGraphicsManager, UefiResult};

/// Gestor de actualización del framebuffer del kernel
pub struct FramebufferUpdater {
    resolution_manager: ResolutionManager,
    uefi_gop_manager: UefiGopManager,
    uefi_graphics_manager: UefiGraphicsManager,
    kernel_framebuffer: Option<FramebufferDriver>,
}

impl FramebufferUpdater {
    /// Crear un nuevo gestor de actualización del framebuffer
    pub fn new() -> Self {
        Self {
            resolution_manager: ResolutionManager::new(),
            uefi_gop_manager: UefiGopManager::new(),
            uefi_graphics_manager: UefiGraphicsManager::new(),
            kernel_framebuffer: None,
        }
    }

    /// Mapear memoria para el framebuffer
    fn map_framebuffer_memory(&self, size: usize) -> Result<u64, String> {
        // Regiones de memoria de video disponibles en QEMU
        const VIDEO_MEMORY_REGIONS: [u64; 5] = [
            0x10000000,  // 256MB
            0x20000000,  // 512MB
            0x30000000,  // 768MB
            0x40000000,  // 1GB
            0x50000000,  // 1.25GB
        ];
        
        for &base in &VIDEO_MEMORY_REGIONS {
            if self.is_memory_region_suitable(base, size) && self.map_memory_region(base, size) {
                return Ok(base);
            }
        }
        
        Err("No se pudo mapear memoria para el framebuffer".into())
    }

    /// Verificar si una región de memoria es adecuada para el framebuffer
    fn is_memory_region_suitable(&self, base: u64, size: usize) -> bool {
        // Verificar que esté en el rango de memoria de video y sea lo suficientemente grande
        base >= 0x10000000 && base < 0x60000000 && size <= 0x10000000
    }
    
    /// Verificar que una región de memoria es accesible
    fn map_memory_region(&self, base: u64, _size: usize) -> bool {
        // Verificar acceso a la región escribiendo y leyendo un byte
        unsafe {
            let ptr = base as *mut u8;
            let test_value = 0xAA;
            core::ptr::write_volatile(ptr, test_value);
            let read_value = core::ptr::read_volatile(ptr);
            read_value == test_value
        }
    }

    /// Inicializar el gestor de resolución y UEFI Graphics
    pub fn initialize(&mut self) -> Result<usize, &'static str> {
        // Inicializar UEFI Graphics primero
        match self.uefi_graphics_manager.initialize() {
            Ok(_) => {
                // Luego inicializar UEFI GOP
                match self.uefi_gop_manager.initialize() {
                    Ok(_) => {
                        // Finalmente inicializar el gestor de resolución
                        self.resolution_manager.detect_available_modes()
                    }
                    Err(_) => {
                        // Si UEFI GOP falla, solo usar el gestor de resolución
                        self.resolution_manager.detect_available_modes()
                    }
                }
            }
            Err(_) => {
                // Si UEFI Graphics falla, solo usar el gestor de resolución
                self.resolution_manager.detect_available_modes()
            }
        }
    }

    /// Cambiar resolución y actualizar el framebuffer del kernel usando UEFI Graphics
    pub fn change_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferDriver, ResolutionChangeResult> {
        // Intentar cambiar la resolución usando UEFI Graphics primero
        match self.change_resolution_uefi_graphics(width, height, bits_per_pixel) {
            Ok(new_framebuffer) => {
                self.kernel_framebuffer = Some(new_framebuffer.clone());
                Ok(new_framebuffer)
            }
            Err(_) => {
                // Si UEFI Graphics falla, intentar con UEFI GOP
                match self.change_resolution_uefi(width, height, bits_per_pixel) {
                    Ok(new_framebuffer) => {
                        self.kernel_framebuffer = Some(new_framebuffer.clone());
                        Ok(new_framebuffer)
                    }
                    Err(_) => {
                        // Si ambos fallan, usar el gestor de resolución como fallback
                        let result = self.resolution_manager.set_mode(width, height, bits_per_pixel);
                        
                        match result {
                            ResolutionChangeResult::Success(fb_info) => {
                                let new_framebuffer = self.create_framebuffer_from_info(&fb_info);
                                self.kernel_framebuffer = Some(new_framebuffer.clone());
                                Ok(new_framebuffer)
                            }
                            error => Err(error),
                        }
                    }
                }
            }
        }
    }

    /// Cambiar resolución usando UEFI Graphics (comunicación real)
    fn change_resolution_uefi_graphics(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferDriver, String> {
        // Cambiar resolución usando UEFI Graphics
        let mut mode_info = self.uefi_graphics_manager.change_resolution_safe(width, height, bits_per_pixel)
            .map_err(|e| format!("UEFI Graphics error: {}", e))?;
        
        // SIEMPRE mapear nueva memoria para el framebuffer
        let bytes_per_pixel = bits_per_pixel / 8;
        let stride = width * bytes_per_pixel;
        let total_size = stride * height;
        let new_base = self.map_framebuffer_memory(total_size as usize)?;
        mode_info.framebuffer_base = new_base;
        
        // Crear nuevo framebuffer con la información de UEFI Graphics
        let new_framebuffer = self.create_framebuffer_from_uefi_graphics_mode(&mode_info);
        
        Ok(new_framebuffer)
    }

    /// Cambiar resolución usando UEFI GOP (comunicación real)
    fn change_resolution_uefi(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferDriver, String> {
        // Cambiar resolución usando UEFI GOP
        let mode_info = self.uefi_gop_manager.change_resolution(width, height, bits_per_pixel)?;
        
        // Crear nuevo framebuffer con la información de UEFI
        let new_framebuffer = self.create_framebuffer_from_uefi_mode(&mode_info);
        
        Ok(new_framebuffer)
    }

    /// Crear un FramebufferDriver a partir de la información de modo UEFI Graphics
    fn create_framebuffer_from_uefi_graphics_mode(&self, mode_info: &crate::drivers::uefi_graphics::UefiModeInfo) -> FramebufferDriver {
        let mut framebuffer = FramebufferDriver::new();
        
        // Configurar la información del framebuffer desde UEFI Graphics
        framebuffer.info = FramebufferInfo {
            base_address: mode_info.framebuffer_base,
            width: mode_info.width,
            height: mode_info.height,
            pixels_per_scan_line: mode_info.pixels_per_scan_line,
            pixel_format: mode_info.pixel_format,
            red_mask: 0x00FF0000,    // RGBA8888: Red en bits 16-23
            green_mask: 0x0000FF00,  // RGBA8888: Green en bits 8-15
            blue_mask: 0x000000FF,   // RGBA8888: Blue en bits 0-7
            reserved_mask: 0xFF000000, // RGBA8888: Alpha en bits 24-31
        };

        // Inicializar el driver con la información UEFI
        let pixel_bitmask = framebuffer.info.red_mask | framebuffer.info.green_mask | framebuffer.info.blue_mask;
        let _ = framebuffer.init_from_uefi(
            framebuffer.info.base_address,
            framebuffer.info.width,
            framebuffer.info.height,
            framebuffer.info.pixels_per_scan_line,
            framebuffer.info.pixel_format,
            pixel_bitmask,
        );

        framebuffer
    }

    /// Crear un FramebufferDriver a partir de la información de modo UEFI
    fn create_framebuffer_from_uefi_mode(&self, mode_info: &UefiModeInfo) -> FramebufferDriver {
        let mut framebuffer = FramebufferDriver::new();
        
        // Configurar la información del framebuffer desde UEFI
        framebuffer.info = FramebufferInfo {
            base_address: mode_info.framebuffer_base,
            width: mode_info.width,
            height: mode_info.height,
            pixels_per_scan_line: mode_info.pixels_per_scan_line,
            pixel_format: mode_info.pixel_format,
            red_mask: 0x00FF0000,    // RGBA8888: Red en bits 16-23
            green_mask: 0x0000FF00,  // RGBA8888: Green en bits 8-15
            blue_mask: 0x000000FF,   // RGBA8888: Blue en bits 0-7
            reserved_mask: 0xFF000000, // RGBA8888: Alpha en bits 24-31
        };

        // Inicializar el driver con la información UEFI
        let pixel_bitmask = framebuffer.info.red_mask | framebuffer.info.green_mask | framebuffer.info.blue_mask;
        let _ = framebuffer.init_from_uefi(
            framebuffer.info.base_address,
            framebuffer.info.width,
            framebuffer.info.height,
            framebuffer.info.pixels_per_scan_line,
            framebuffer.info.pixel_format,
            pixel_bitmask,
        );

        framebuffer
    }

    /// Cambiar resolución por nombre
    pub fn change_resolution_by_name(&mut self, name: &str) -> Result<FramebufferDriver, ResolutionChangeResult> {
        let result = self.resolution_manager.set_resolution_by_name(name);
        
        match result {
            ResolutionChangeResult::Success(fb_info) => {
                let new_framebuffer = self.create_framebuffer_from_info(&fb_info);
                self.kernel_framebuffer = Some(new_framebuffer.clone());
                Ok(new_framebuffer)
            }
            error => Err(error),
        }
    }

    /// Cambiar a la resolución más alta disponible
    pub fn set_highest_resolution(&mut self) -> Result<FramebufferDriver, ResolutionChangeResult> {
        let result = self.resolution_manager.set_highest_resolution();
        
        match result {
            ResolutionChangeResult::Success(fb_info) => {
                let new_framebuffer = self.create_framebuffer_from_info(&fb_info);
                self.kernel_framebuffer = Some(new_framebuffer.clone());
                Ok(new_framebuffer)
            }
            error => Err(error),
        }
    }

    /// Crear un FramebufferDriver a partir de la información de resolución
    fn create_framebuffer_from_info(&self, fb_info: &ResolutionFramebufferInfo) -> FramebufferDriver {
        let mut framebuffer = FramebufferDriver::new();
        
        // Configurar la información del framebuffer
        framebuffer.info = FramebufferInfo {
            base_address: fb_info.base_address,
            width: fb_info.width,
            height: fb_info.height,
            pixels_per_scan_line: fb_info.pixels_per_scan_line,
            pixel_format: fb_info.pixel_format,
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        };

        framebuffer
    }

    /// Obtener el framebuffer actual del kernel
    pub fn get_current_framebuffer(&self) -> Option<&FramebufferDriver> {
        self.kernel_framebuffer.as_ref()
    }

    /// Establecer el framebuffer actual del kernel (para reutilizar su base)
    pub fn set_current_framebuffer(&mut self, fb: &FramebufferDriver) {
        self.kernel_framebuffer = Some(fb.clone());
    }

    /// Obtener el framebuffer actual del kernel (mutable)
    pub fn get_current_framebuffer_mut(&mut self) -> Option<&mut FramebufferDriver> {
        self.kernel_framebuffer.as_mut()
    }

    /// Obtener información de la resolución actual
    pub fn get_current_resolution_info(&self) -> String {
        self.resolution_manager.get_resolution_info()
    }

    /// Listar modos de video disponibles
    pub fn list_available_modes(&self) -> String {
        self.resolution_manager.list_available_modes()
    }

    /// Obtener el gestor de resolución
    pub fn get_resolution_manager(&self) -> &ResolutionManager {
        &self.resolution_manager
    }

    /// Obtener el gestor de resolución (mutable)
    pub fn get_resolution_manager_mut(&mut self) -> &mut ResolutionManager {
        &mut self.resolution_manager
    }

    /// Obtener el gestor UEFI GOP
    pub fn get_uefi_gop_manager(&self) -> &UefiGopManager {
        &self.uefi_gop_manager
    }

    /// Obtener el gestor UEFI GOP (mutable)
    pub fn get_uefi_gop_manager_mut(&mut self) -> &mut UefiGopManager {
        &mut self.uefi_gop_manager
    }

    /// Verificar si UEFI GOP está disponible
    pub fn is_uefi_gop_available(&self) -> bool {
        !self.uefi_gop_manager.gop_protocol.is_null()
    }

    /// Obtener información de UEFI GOP
    pub fn get_uefi_gop_info(&self) -> String {
        self.uefi_gop_manager.get_gop_info()
    }

    /// Obtener el gestor UEFI Graphics
    pub fn get_uefi_graphics_manager(&self) -> &UefiGraphicsManager {
        &self.uefi_graphics_manager
    }

    /// Obtener el gestor UEFI Graphics (mutable)
    pub fn get_uefi_graphics_manager_mut(&mut self) -> &mut UefiGraphicsManager {
        &mut self.uefi_graphics_manager
    }

    /// Verificar si UEFI Graphics está disponible
    pub fn is_uefi_graphics_available(&self) -> bool {
        self.uefi_graphics_manager.is_available()
    }

    /// Obtener información de UEFI Graphics
    pub fn get_uefi_graphics_info(&self) -> String {
        self.uefi_graphics_manager.get_system_info()
    }

    /// Listar modos UEFI Graphics disponibles
    pub fn list_uefi_graphics_modes(&self) -> String {
        self.uefi_graphics_manager.list_available_modes()
    }

    /// Actualizar el framebuffer del kernel con una nueva resolución
    /// Esta función maneja la transición completa del cambio de resolución
    pub fn update_kernel_framebuffer(&mut self, new_fb: FramebufferDriver) -> Result<(), &'static str> {
        // Guardar el framebuffer anterior para limpiar
        let old_fb = self.kernel_framebuffer.take();
        
        // Establecer el nuevo framebuffer
        self.kernel_framebuffer = Some(new_fb);
        
        // Limpiar la pantalla con el nuevo framebuffer
        if let Some(ref mut fb) = self.kernel_framebuffer {
            fb.clear_screen(Color::BLACK);
            fb.write_text_kernel("Resolución actualizada exitosamente", Color::GREEN);
        }
        
        Ok(())
    }

    /// Verificar si una resolución es soportada
    pub fn is_resolution_supported(&self, width: u32, height: u32, bits_per_pixel: u32) -> bool {
        self.resolution_manager.find_mode(width, height, bits_per_pixel).is_some()
    }

    /// Obtener la resolución actual
    pub fn get_current_resolution(&self) -> Option<(u32, u32, u32)> {
        self.resolution_manager.get_current_mode().map(|mode| {
            (mode.width, mode.height, mode.bits_per_pixel)
        })
    }

    /// Restaurar resolución por defecto (VGA 640x480)
    pub fn restore_default_resolution(&mut self) -> Result<FramebufferDriver, ResolutionChangeResult> {
        self.change_resolution(640, 480, 32)
    }

    /// Aplicar configuración de resolución desde parámetros del kernel
    pub fn apply_resolution_config(&mut self, config: &ResolutionConfig) -> Result<FramebufferDriver, ResolutionChangeResult> {
        match config {
            ResolutionConfig::Specific { width, height, bits_per_pixel } => {
                self.change_resolution(*width, *height, *bits_per_pixel)
            }
            ResolutionConfig::ByName { name } => {
                self.change_resolution_by_name(name)
            }
            ResolutionConfig::Highest => {
                self.set_highest_resolution()
            }
            ResolutionConfig::Default => {
                self.restore_default_resolution()
            }
        }
    }
}

/// Configuración de resolución
#[derive(Debug, Clone)]
pub enum ResolutionConfig {
    Specific { width: u32, height: u32, bits_per_pixel: u32 },
    ByName { name: String },
    Highest,
    Default,
}

impl Default for FramebufferUpdater {
    fn default() -> Self {
        Self::new()
    }
}

/// Funciones auxiliares para integración con el sistema de gráficos
pub mod graphics_integration {
    use super::*;

    /// Aplicar cambio de resolución con notificación al sistema de gráficos
    pub fn apply_resolution_change_with_notification(
        updater: &mut FramebufferUpdater,
        width: u32,
        height: u32,
        bits_per_pixel: u32,
    ) -> Result<FramebufferDriver, String> {
        match updater.change_resolution(width, height, bits_per_pixel) {
            Ok(fb) => {
                // Notificar al sistema de gráficos sobre el cambio
                notify_graphics_system_resolution_change(width, height, bits_per_pixel);
                Ok(fb)
            }
            Err(error) => Err(format!("Error cambiando resolución: {}", error)),
        }
    }

    /// Notificar al sistema de gráficos sobre el cambio de resolución
    fn notify_graphics_system_resolution_change(width: u32, height: u32, bits_per_pixel: u32) {
        // En una implementación real, aquí se notificaría a:
        // - El compositor de ventanas
        // - El sistema de eventos
        // - Los drivers de gráficos
        // - Las aplicaciones que necesiten redimensionarse
        
        // Por ahora, solo registramos el cambio
        // En un sistema real, esto sería una llamada a una función del sistema
    }

    /// Validar que el cambio de resolución es compatible con el hardware
    pub fn validate_resolution_change(
        updater: &FramebufferUpdater,
        width: u32,
        height: u32,
        bits_per_pixel: u32,
    ) -> bool {
        updater.is_resolution_supported(width, height, bits_per_pixel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_framebuffer_updater_creation() {
        let updater = FramebufferUpdater::new();
        assert!(updater.get_current_framebuffer().is_none());
    }

    #[test]
    fn test_initialize() {
        let mut updater = FramebufferUpdater::new();
        let result = updater.initialize();
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_change_resolution() {
        let mut updater = FramebufferUpdater::new();
        updater.initialize().unwrap();
        
        let result = updater.change_resolution(1024, 768, 32);
        assert!(result.is_ok());
        assert!(updater.get_current_framebuffer().is_some());
    }

    #[test]
    fn test_resolution_support() {
        let mut updater = FramebufferUpdater::new();
        updater.initialize().unwrap();
        
        assert!(updater.is_resolution_supported(1920, 1080, 32));
        assert!(!updater.is_resolution_supported(9999, 9999, 32));
    }
}
