use core::fmt;
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;

/// Estructura que representa un modo de video
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u32,
    pub refresh_rate: u32,
    pub mode_number: u32,
}

/// Información del framebuffer después del cambio de resolución
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub bits_per_pixel: u32,
    pub pixel_format: u32,
}

/// Resultado de una operación de cambio de resolución
#[derive(Debug)]
pub enum ResolutionChangeResult {
    Success(FramebufferInfo),
    ModeNotFound,
    SetModeFailed,
    InvalidMode,
    UnsupportedResolution,
}

impl fmt::Display for ResolutionChangeResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolutionChangeResult::Success(info) => {
                write!(f, "Resolución cambiada exitosamente: {}x{} @0x{:X}", 
                       info.width, info.height, info.base_address)
            }
            ResolutionChangeResult::ModeNotFound => {
                write!(f, "Modo de video no encontrado")
            }
            ResolutionChangeResult::SetModeFailed => {
                write!(f, "Error al establecer el modo de video")
            }
            ResolutionChangeResult::InvalidMode => {
                write!(f, "Modo de video inválido")
            }
            ResolutionChangeResult::UnsupportedResolution => {
                write!(f, "Resolución no soportada")
            }
        }
    }
}

/// Gestor de resolución de pantalla
pub struct ResolutionManager {
    current_mode: Option<VideoMode>,
    available_modes: Vec<VideoMode>,
    current_framebuffer: Option<FramebufferInfo>,
}

impl ResolutionManager {
    /// Crear un nuevo gestor de resolución
    pub fn new() -> Self {
        Self {
            current_mode: None,
            available_modes: Vec::new(),
            current_framebuffer: None,
        }
    }

    /// Detectar modos de video disponibles usando UEFI GOP
    pub fn detect_available_modes(&mut self) -> Result<usize, &'static str> {
        // En un sistema real, aquí se consultaría UEFI GOP
        // Por ahora, simulamos algunos modos comunes con validación
        self.available_modes.clear();
        
        // Modos de video comunes ordenados por seguridad (más seguros primero)
        let common_modes = [
            (640, 480, 32, 60),   // VGA - Muy seguro
            (800, 600, 32, 60),   // SVGA - Muy seguro
            (1024, 768, 32, 60),  // XGA - Seguro
            (1280, 720, 32, 60),  // HD - Seguro
            (1280, 1024, 32, 60), // SXGA - Seguro
            (1366, 768, 32, 60),  // HD WXGA - Seguro
            (1440, 900, 32, 60),  // WXGA+ - Moderadamente seguro
            (1600, 900, 32, 60),  // HD+ - Moderadamente seguro
            (1680, 1050, 32, 60), // WSXGA+ - Moderadamente seguro
            (1920, 1080, 32, 60), // Full HD - Puede causar problemas
            (2560, 1440, 32, 60), // QHD - Riesgoso
            (3840, 2160, 32, 60), // 4K - Muy riesgoso
        ];

        for (i, (width, height, bpp, refresh)) in common_modes.iter().enumerate() {
            // Solo agregar modos que sean considerados seguros
            if self.is_safe_mode(*width, *height, *bpp, *refresh) {
                self.available_modes.push(VideoMode {
                    width: *width,
                    height: *height,
                    bits_per_pixel: *bpp,
                    refresh_rate: *refresh,
                    mode_number: i as u32,
                });
            }
        }

        Ok(self.available_modes.len())
    }

    /// Verificar si un modo es seguro para el monitor
    fn is_safe_mode(&self, width: u32, height: u32, bpp: u32, refresh: u32) -> bool {
        // Validar parámetros básicos
        if width == 0 || height == 0 || bpp == 0 || refresh == 0 {
            return false;
        }

        // Validar que sea una resolución estándar
        if !self.is_standard_resolution(width, height) {
            return false;
        }

        // Validar bits por píxel
        if bpp != 32 && bpp != 24 && bpp != 16 {
            return false;
        }

        // Validar refresh rate (no más de 75Hz para evitar problemas)
        if refresh > 75 {
            return false;
        }

        // Para resoluciones altas, ser más conservador
        if width > 1920 || height > 1080 {
            return false; // Solo permitir resoluciones hasta Full HD
        }

        true
    }

    /// Obtener lista de modos disponibles
    pub fn get_available_modes(&self) -> &[VideoMode] {
        &self.available_modes
    }

    /// Buscar un modo de video específico
    pub fn find_mode(&self, width: u32, height: u32, bits_per_pixel: u32) -> Option<&VideoMode> {
        self.available_modes.iter().find(|mode| {
            mode.width == width && mode.height == height && mode.bits_per_pixel == bits_per_pixel
        })
    }

    /// Cambiar a un modo de video específico con validación mejorada
    pub fn set_mode(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> ResolutionChangeResult {
        // Validar parámetros básicos
        if width == 0 || height == 0 || bits_per_pixel == 0 {
            return ResolutionChangeResult::InvalidMode;
        }

        // Validar que la resolución no sea demasiado alta (evitar pérdida de señal)
        if width > 3840 || height > 2160 {
            return ResolutionChangeResult::UnsupportedResolution;
        }

        // Validar que la resolución sea una resolución estándar conocida
        if !self.is_standard_resolution(width, height) {
            return ResolutionChangeResult::UnsupportedResolution;
        }

        // Buscar el modo solicitado
        let mode = match self.find_mode(width, height, bits_per_pixel) {
            Some(mode) => *mode,
            None => return ResolutionChangeResult::ModeNotFound,
        };

        // Validar que el modo sea compatible con el hardware actual
        if !self.validate_mode_compatibility(&mode) {
            return ResolutionChangeResult::UnsupportedResolution;
        }

        // Aplicar el modo de video con validación adicional
        match self.apply_video_mode_safe(&mode) {
            Ok(framebuffer_info) => {
                self.current_mode = Some(mode);
                self.current_framebuffer = Some(framebuffer_info);
                ResolutionChangeResult::Success(framebuffer_info)
            }
            Err(_) => ResolutionChangeResult::SetModeFailed,
        }
    }

    /// Verificar si una resolución es estándar y conocida
    fn is_standard_resolution(&self, width: u32, height: u32) -> bool {
        let standard_resolutions = [
            (640, 480),   // VGA
            (800, 600),   // SVGA
            (1024, 768),  // XGA
            (1280, 720),  // HD
            (1280, 1024), // SXGA
            (1366, 768),  // HD WXGA
            (1440, 900),  // WXGA+
            (1600, 900),  // HD+
            (1680, 1050), // WSXGA+
            (1920, 1080), // Full HD
            (2560, 1440), // QHD
            (3840, 2160), // 4K
        ];

        standard_resolutions.iter().any(|(w, h)| *w == width && *h == height)
    }

    /// Validar compatibilidad del modo con el hardware actual
    fn validate_mode_compatibility(&self, mode: &VideoMode) -> bool {
        // Validar que el modo no sea demasiado agresivo
        if mode.width > 1920 || mode.height > 1080 {
            // Para resoluciones altas, ser más conservador
            return false;
        }

        // Validar que el refresh rate sea razonable
        if mode.refresh_rate > 75 {
            return false;
        }

        // Validar que los bits por píxel sean soportados
        if mode.bits_per_pixel != 32 && mode.bits_per_pixel != 24 && mode.bits_per_pixel != 16 {
            return false;
        }

        true
    }

    /// Aplicar modo de video de forma segura
    fn apply_video_mode_safe(&self, mode: &VideoMode) -> Result<FramebufferInfo, &'static str> {
        // En un sistema real, aquí se:
        // 1. Guardaría el modo actual
        // 2. Llamaría a UEFI GOP SetMode con validación
        // 3. Verificaría que el cambio fue exitoso
        // 4. Si falla, restauraría el modo anterior
        
        // Simulamos la información del framebuffer con validación adicional
        let framebuffer_info = FramebufferInfo {
            base_address: 0xE0000000, // Dirección simulada
            width: mode.width,
            height: mode.height,
            pixels_per_scan_line: mode.width, // Asumimos stride = width
            bits_per_pixel: mode.bits_per_pixel,
            pixel_format: 0x00E07F00, // Formato RGB
        };

        // Simular una pequeña pausa para evitar cambios demasiado rápidos
        // En un sistema real, esto sería una espera de sincronización
        
        Ok(framebuffer_info)
    }

    /// Aplicar un modo de video (simulado) - versión original
    fn apply_video_mode(&self, mode: &VideoMode) -> Result<FramebufferInfo, &'static str> {
        // En un sistema real, aquí se:
        // 1. Llamaría a UEFI GOP SetMode
        // 2. Obtendría la nueva información del framebuffer
        // 3. Actualizaría las tablas de páginas
        
        // Simulamos la información del framebuffer
        let framebuffer_info = FramebufferInfo {
            base_address: 0xE0000000, // Dirección simulada
            width: mode.width,
            height: mode.height,
            pixels_per_scan_line: mode.width, // Asumimos stride = width
            bits_per_pixel: mode.bits_per_pixel,
            pixel_format: 0x00E07F00, // Formato RGB
        };

        Ok(framebuffer_info)
    }

    /// Obtener el modo actual
    pub fn get_current_mode(&self) -> Option<&VideoMode> {
        self.current_mode.as_ref()
    }

    /// Obtener información del framebuffer actual
    pub fn get_current_framebuffer(&self) -> Option<&FramebufferInfo> {
        self.current_framebuffer.as_ref()
    }

    /// Cambiar a la resolución más alta disponible
    pub fn set_highest_resolution(&mut self) -> ResolutionChangeResult {
        if self.available_modes.is_empty() {
            return ResolutionChangeResult::ModeNotFound;
        }

        // Encontrar el modo con la resolución más alta
        let highest_mode = self.available_modes.iter()
            .max_by_key(|mode| mode.width * mode.height)
            .unwrap();

        self.set_mode(highest_mode.width, highest_mode.height, highest_mode.bits_per_pixel)
    }

    /// Cambiar a la mejor resolución segura disponible
    pub fn set_safest_resolution(&mut self) -> ResolutionChangeResult {
        if self.available_modes.is_empty() {
            return ResolutionChangeResult::ModeNotFound;
        }

        // Priorizar resoluciones seguras y conocidas
        let safe_resolutions = [
            (1024, 768),  // XGA - Muy seguro
            (800, 600),   // SVGA - Muy seguro
            (1280, 720),  // HD - Seguro
            (1280, 1024), // SXGA - Seguro
            (1366, 768),  // HD WXGA - Seguro
            (640, 480),   // VGA - Muy seguro pero baja resolución
        ];

        // Buscar la primera resolución segura disponible
        for (width, height) in safe_resolutions.iter() {
            if let Some(mode) = self.find_mode(*width, *height, 32) {
                return self.set_mode(mode.width, mode.height, mode.bits_per_pixel);
            }
        }

        // Si no se encuentra ninguna resolución segura, usar la primera disponible
        let first_mode = &self.available_modes[0];
        self.set_mode(first_mode.width, first_mode.height, first_mode.bits_per_pixel)
    }

    /// Obtener la mejor resolución recomendada para el monitor
    pub fn get_recommended_resolution(&self) -> Option<(u32, u32, u32)> {
        if self.available_modes.is_empty() {
            return None;
        }

        // Buscar la mejor resolución balanceada (no muy alta, no muy baja)
        let recommended_modes = [
            (1024, 768, 32),  // XGA - Ideal para la mayoría de monitores
            (1280, 720, 32),  // HD - Buena para monitores modernos
            (1280, 1024, 32), // SXGA - Buena para monitores 4:3
            (1366, 768, 32),  // HD WXGA - Buena para laptops
            (800, 600, 32),   // SVGA - Fallback seguro
        ];

        for (width, height, bpp) in recommended_modes.iter() {
            if self.find_mode(*width, *height, *bpp).is_some() {
                return Some((*width, *height, *bpp));
            }
        }

        // Si no se encuentra ninguna recomendada, usar la primera disponible
        let first_mode = &self.available_modes[0];
        Some((first_mode.width, first_mode.height, first_mode.bits_per_pixel))
    }

    /// Cambiar a una resolución específica por nombre
    pub fn set_resolution_by_name(&mut self, name: &str) -> ResolutionChangeResult {
        let (width, height, bpp) = match name.to_lowercase().as_str() {
            "vga" => (640, 480, 32),
            "svga" => (800, 600, 32),
            "xga" => (1024, 768, 32),
            "hd" => (1280, 720, 32),
            "sxga" => (1280, 1024, 32),
            "wxga" => (1366, 768, 32),
            "wxga+" => (1440, 900, 32),
            "hd+" => (1600, 900, 32),
            "wsxga+" => (1680, 1050, 32),
            "fhd" | "1080p" => (1920, 1080, 32),
            "qhd" | "1440p" => (2560, 1440, 32),
            "4k" | "2160p" => (3840, 2160, 32),
            _ => return ResolutionChangeResult::UnsupportedResolution,
        };

        self.set_mode(width, height, bpp)
    }

    /// Obtener información de resolución en formato legible
    pub fn get_resolution_info(&self) -> String {
        if let Some(mode) = &self.current_mode {
            format!("{}x{} @{}bpp ({}Hz)", 
                   mode.width, mode.height, mode.bits_per_pixel, mode.refresh_rate)
        } else {
            "No hay modo establecido".to_string()
        }
    }

    /// Listar todos los modos disponibles en formato legible
    pub fn list_available_modes(&self) -> String {
        if self.available_modes.is_empty() {
            return "No hay modos disponibles".to_string();
        }

        let mut result = String::new();
        for (i, mode) in self.available_modes.iter().enumerate() {
            result.push_str(&format!("{}: {}x{} @{}bpp ({}Hz)\n", 
                                   i, mode.width, mode.height, mode.bits_per_pixel, mode.refresh_rate));
        }
        result
    }
}

impl Default for ResolutionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Funciones auxiliares para integración con UEFI
pub mod uefi_integration {
    use super::*;

    /// Detectar modos usando UEFI GOP (implementación real)
    pub fn detect_modes_uefi_gop() -> Result<Vec<VideoMode>, &'static str> {
        // En una implementación real, aquí se:
        // 1. Obtendría el protocolo GOP de UEFI System Table
        // 2. Llamaría a QueryMode para cada modo disponible
        // 3. Construiría la lista de VideoMode
        
        // Por ahora, devolvemos una lista vacía
        Ok(Vec::new())
    }

    /// Cambiar modo usando UEFI GOP (implementación real)
    pub fn set_mode_uefi_gop(mode_number: u32) -> Result<FramebufferInfo, &'static str> {
        // En una implementación real, aquí se:
        // 1. Llamaría a UEFI GOP SetMode
        // 2. Obtendría la nueva información del framebuffer
        // 3. Actualizaría las estructuras del kernel
        
        Err("UEFI GOP no implementado")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolution_manager_creation() {
        let manager = ResolutionManager::new();
        assert!(manager.get_current_mode().is_none());
        assert!(manager.get_current_framebuffer().is_none());
    }

    #[test]
    fn test_detect_modes() {
        let mut manager = ResolutionManager::new();
        let count = manager.detect_available_modes().unwrap();
        assert!(count > 0);
        assert!(!manager.available_modes.is_empty());
    }

    #[test]
    fn test_find_mode() {
        let mut manager = ResolutionManager::new();
        manager.detect_available_modes().unwrap();
        
        let mode = manager.find_mode(1920, 1080, 32);
        assert!(mode.is_some());
        assert_eq!(mode.unwrap().width, 1920);
        assert_eq!(mode.unwrap().height, 1080);
    }

    #[test]
    fn test_set_mode() {
        let mut manager = ResolutionManager::new();
        manager.detect_available_modes().unwrap();
        
        let result = manager.set_mode(1920, 1080, 32);
        match result {
            ResolutionChangeResult::Success(_) => {
                assert!(manager.get_current_mode().is_some());
                assert!(manager.get_current_framebuffer().is_some());
            }
            _ => panic!("Expected success"),
        }
    }
}
