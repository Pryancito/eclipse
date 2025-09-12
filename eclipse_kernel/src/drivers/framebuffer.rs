//! Driver de Framebuffer para Eclipse OS
//! 
//! Implementa un sistema de framebuffer robusto basado en las mejores prácticas
//! de Rust y optimizado para sistemas bare metal con UEFI.

use core::ptr;
use core::cmp::min;
use core::mem;
use core::ops::{Index, IndexMut};
use alloc::format;
use alloc::string::String;
use crate::drivers::pci::{GpuType, GpuInfo};
use crate::drivers::intel_graphics::{IntelGraphicsDriver, IntelDriverState};
use crate::drivers::nvidia_graphics::{NvidiaGraphicsDriver, NvidiaDriverState};
use crate::drivers::amd_graphics::{AmdGraphicsDriver, AmdDriverState};

/// Trait para operaciones básicas de dibujo
pub trait Drawable {
    /// Dibujar un pixel en las coordenadas especificadas
    fn put_pixel(&mut self, x: u32, y: u32, color: Color);
    
    /// Leer un pixel de las coordenadas especificadas
    fn get_pixel(&self, x: u32, y: u32) -> Color;
    
    /// Llenar un rectángulo con color
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color);
    
    /// Limpiar toda la superficie con un color
    fn clear(&mut self, color: Color);
}

/// Trait para operaciones de texto
pub trait TextRenderer {
    /// Escribir texto en las coordenadas especificadas
    fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color);
    
    /// Obtener dimensiones de un carácter
    fn char_dimensions(&self) -> (u32, u32);
}

/// Trait para operaciones de geometría
pub trait GeometryRenderer {
    /// Dibujar una línea
    fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color);
    
    /// Dibujar un rectángulo (solo bordes)
    fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color);
    
    /// Dibujar un círculo
    fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color);
}

/// Trait para operaciones de blit (copia de superficies)
pub trait Blittable {
    /// Copiar región de otra superficie
    fn blit_from<T: Drawable>(&mut self, src: &T, src_x: u32, src_y: u32, 
                              dst_x: u32, dst_y: u32, width: u32, height: u32);
}

/// Información del framebuffer obtenida del hardware
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

/// Formatos de pixel soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    RGB888,     // 24-bit RGB
    RGBA8888,   // 32-bit RGBA
    BGR888,     // 24-bit BGR
    BGRA8888,   // 32-bit BGRA
    RGB565,     // 16-bit RGB
    BGR565,     // 16-bit BGR
    Unknown,
}

impl PixelFormat {
    pub fn from_uefi_format(format: u32) -> Self {
        match format {
            0 => PixelFormat::RGB888,      // PixelRedGreenBlueReserved8BitPerColor
            1 => PixelFormat::BGR888,      // PixelBlueGreenRedReserved8BitPerColor
            2 => PixelFormat::RGB565,      // PixelBitMask
            3 => PixelFormat::BGR565,      // PixelBltOnly
            _ => PixelFormat::Unknown,
        }
    }
    
    pub fn bytes_per_pixel(&self) -> u8 {
        match self {
            PixelFormat::RGB888 => 3,
            PixelFormat::RGBA8888 => 4,
            PixelFormat::BGR888 => 3,
            PixelFormat::BGRA8888 => 4,
            PixelFormat::RGB565 => 2,
            PixelFormat::BGR565 => 2,
            PixelFormat::Unknown => 4, // Default to 4 bytes
        }
    }
}

/// Color RGBA con operaciones avanzadas
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }
    
    /// Crear color desde valor hexadecimal (0x00RRGGBB, donde 0x00FF0000 es rojo)
    pub fn from_hex(hex: u32) -> Self {
        // 0x00RRGGBB: Rojo en bits 16-23, Verde en 8-15, Azul en 0-7
        Self::rgb(
            ((hex >> 16) & 0xFF) as u8, // Rojo
            ((hex >> 8) & 0xFF) as u8,  // Verde
            (hex & 0xFF) as u8          // Azul
        )
    }
    
    /// Crear color desde valor hexadecimal con alpha (0xAARRGGBB, donde 0xFFFF0000 es rojo opaco)
    pub fn from_hex_alpha(hex: u32) -> Self {
        // 0xAARRGGBB: Alpha en bits 24-31, Rojo en 16-23, Verde en 8-15, Azul en 0-7
        Self::new(
            ((hex >> 16) & 0xFF) as u8, // Rojo
            ((hex >> 8) & 0xFF) as u8,  // Verde
            (hex & 0xFF) as u8,         // Azul
            ((hex >> 24) & 0xFF) as u8  // Alpha
        )
    }

    /// Convertir a valor u32 RGBA
    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | 
        ((self.r as u32) << 16) | 
        ((self.g as u32) << 8) | 
        (self.b as u32)
    }

    /// Mezclar dos colores con alpha blending
    pub fn blend(&self, other: Color) -> Color {
        let alpha = other.a as f32 / 255.0;
        let inv_alpha = 1.0 - alpha;
        
        Color::new(
            (self.r as f32 * inv_alpha + other.r as f32 * alpha) as u8,
            (self.g as f32 * inv_alpha + other.g as f32 * alpha) as u8,
            (self.b as f32 * inv_alpha + other.b as f32 * alpha) as u8,
            self.a.max(other.a)
        )
    }
    
    /// Aplicar factor de brillo (0.0 = negro, 1.0 = original, >1.0 = más brillante)
    pub fn brighten(&self, factor: f32) -> Color {
        Color::new(
            (self.r as f32 * factor).min(255.0) as u8,
            (self.g as f32 * factor).min(255.0) as u8,
            (self.b as f32 * factor).min(255.0) as u8,
            self.a
        )
    }
    
    /// Obtener luminancia del color
    pub fn luminance(&self) -> f32 {
        0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32
    }
    
    /// Verificar si el color es oscuro (luminancia < 128)
    pub fn is_dark(&self) -> bool {
        self.luminance() < 128.0
    }
    
    /// Convertir color a pixel según el formato
    pub fn to_pixel(&self, format: PixelFormat) -> u32 {
        match format {
            PixelFormat::RGBA8888 => {
                ((self.a as u32) << 24) | 
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGRA8888 => {
                ((self.a as u32) << 24) | 
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB888 => {
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGR888 => {
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB565 => {
                (((self.r as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.b as u32) >> 3)
            },
            PixelFormat::BGR565 => {
                (((self.b as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.r as u32) >> 3)
            },
            PixelFormat::Unknown => 0,
        }
    }
    
    /// Crear color desde un pixel según el formato
    pub fn from_pixel(pixel: u32, format: PixelFormat) -> Self {
        match format {
            PixelFormat::RGBA8888 => {
                Self {
                    r: ((pixel >> 16) & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: (pixel & 0xFF) as u8,
                    a: ((pixel >> 24) & 0xFF) as u8,
                }
            },
            PixelFormat::BGRA8888 => {
                Self {
                    r: (pixel & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: ((pixel >> 16) & 0xFF) as u8,
                    a: ((pixel >> 24) & 0xFF) as u8,
                }
            },
            PixelFormat::RGB888 => {
                Self {
                    r: ((pixel >> 16) & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: (pixel & 0xFF) as u8,
                    a: 255,
                }
            },
            PixelFormat::BGR888 => {
                Self {
                    r: (pixel & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: ((pixel >> 16) & 0xFF) as u8,
                    a: 255,
                }
            },
            PixelFormat::RGB565 => {
                Self {
                    r: (((pixel >> 11) & 0x1F) << 3) as u8,
                    g: (((pixel >> 5) & 0x3F) << 2) as u8,
                    b: ((pixel & 0x1F) << 3) as u8,
                    a: 255,
                }
            },
            PixelFormat::BGR565 => {
                Self {
                    r: ((pixel & 0x1F) << 3) as u8,
                    g: (((pixel >> 5) & 0x3F) << 2) as u8,
                    b: (((pixel >> 11) & 0x1F) << 3) as u8,
                    a: 255,
                }
            },
            PixelFormat::Unknown => Self::new(0, 0, 0, 0),
        }
    }
}

/// Tipo de aceleración de hardware disponible
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HardwareAcceleration {
    None,
    Intel2D,
    Nvidia2D,
    Amd2D,
    Generic2D,
}

/// Información de capacidades de aceleración
#[derive(Debug, Clone)]
pub struct AccelerationCapabilities {
    pub supports_hardware_blit: bool,
    pub supports_hardware_fill: bool,
    pub supports_hardware_alpha: bool,
    pub supports_hardware_gradients: bool,
    pub supports_hardware_scaling: bool,
    pub supports_hardware_rotation: bool,
    pub max_blit_size: (u32, u32),
    pub memory_bandwidth: u64, // MB/s
}

/// Trait para aceleración de hardware 2D
pub trait HardwareAccelerated {
    /// Obtener tipo de aceleración disponible
    fn acceleration_type(&self) -> HardwareAcceleration;
    
    /// Obtener capacidades de aceleración
    fn acceleration_capabilities(&self) -> AccelerationCapabilities;
    
    /// Blit acelerado por hardware
    fn hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str>;
    
    /// Fill acelerado por hardware
    fn hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str>;
    
    /// Alpha blending acelerado por hardware
    fn hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str>;
    
    /// Escalado acelerado por hardware
    fn hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str>;
}

/// Gestor de aceleración de hardware
#[derive(Debug, Clone)]
pub struct HardwareAccelerationManager {
    gpu_type: Option<GpuType>,
    capabilities: AccelerationCapabilities,
    is_initialized: bool,
}

impl HardwareAccelerationManager {
    /// Crear nuevo gestor de aceleración
    pub fn new() -> Self {
        Self {
            gpu_type: None,
            capabilities: AccelerationCapabilities {
                supports_hardware_blit: false,
                supports_hardware_fill: false,
                supports_hardware_alpha: false,
                supports_hardware_gradients: false,
                supports_hardware_scaling: false,
                supports_hardware_rotation: false,
                max_blit_size: (0, 0),
                memory_bandwidth: 0,
            },
            is_initialized: false,
        }
    }
    
    /// Inicializar con información de GPU
    pub fn initialize_with_gpu(&mut self, gpu_info: &GpuInfo) -> Result<(), &'static str> {
        self.gpu_type = Some(gpu_info.gpu_type);
        
        match gpu_info.gpu_type {
            GpuType::Intel => {
                // Crear driver Intel (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: false,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: false,
                    max_blit_size: (4096, 4096),
                    memory_bandwidth: 10000, // 10 GB/s estimado
                };
            },
            GpuType::Nvidia => {
                // Crear driver NVIDIA (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: true,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: true,
                    max_blit_size: (8192, 8192),
                    memory_bandwidth: 20000, // 20 GB/s estimado
                };
            },
            GpuType::Amd => {
                // Crear driver AMD (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: true,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: false,
                    max_blit_size: (6144, 6144),
                    memory_bandwidth: 15000, // 15 GB/s estimado
                };
            },
            _ => {
                // Sin aceleración de hardware
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: false,
                    supports_hardware_fill: false,
                    supports_hardware_alpha: false,
                    supports_hardware_gradients: false,
                    supports_hardware_scaling: false,
                    supports_hardware_rotation: false,
                    max_blit_size: (0, 0),
                    memory_bandwidth: 0,
                };
            }
        }
        
        self.is_initialized = true;
        Ok(())
    }
    
    /// Obtener capacidades de aceleración
    pub fn get_capabilities(&self) -> &AccelerationCapabilities {
        &self.capabilities
    }
    
    /// Verificar si hay aceleración disponible
    pub fn has_acceleration(&self) -> bool {
        self.is_initialized && self.capabilities.supports_hardware_blit
    }
    
    /// Obtener tipo de aceleración
    pub fn get_acceleration_type(&self) -> HardwareAcceleration {
        match self.gpu_type {
            Some(GpuType::Intel) => HardwareAcceleration::Intel2D,
            Some(GpuType::Nvidia) => HardwareAcceleration::Nvidia2D,
            Some(GpuType::Amd) => HardwareAcceleration::Amd2D,
            _ => HardwareAcceleration::None,
        }
    }
}

impl Color {
    // Colores básicos
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    pub const GREEN: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };
    pub const YELLOW: Color = Color { r: 255, g: 255, b: 0, a: 255 };
    pub const CYAN: Color = Color { r: 0, g: 255, b: 255, a: 255 };
    pub const MAGENTA: Color = Color { r: 255, g: 0, b: 255, a: 255 };
    
    // Colores del sistema
    pub const DARK_BLUE: Color = Color { r: 0, g: 0, b: 128, a: 255 };
    pub const DARKER_BLUE: Color = Color { r: 0, g: 0, b: 64, a: 255 };
    pub const GRAY: Color = Color { r: 128, g: 128, b: 128, a: 255 };
    pub const DARK_GRAY: Color = Color { r: 64, g: 64, b: 64, a: 255 };
    pub const LIGHT_GRAY: Color = Color { r: 192, g: 192, b: 192, a: 255 };
    
    // Colores adicionales para UI
    pub const ORANGE: Color = Color { r: 255, g: 165, b: 0, a: 255 };
    pub const PURPLE: Color = Color { r: 128, g: 0, b: 128, a: 255 };
    pub const PINK: Color = Color { r: 255, g: 192, b: 203, a: 255 };
    pub const BROWN: Color = Color { r: 165, g: 42, b: 42, a: 255 };
    pub const LIME: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const TEAL: Color = Color { r: 0, g: 128, b: 128, a: 255 };
    pub const NAVY: Color = Color { r: 0, g: 0, b: 128, a: 255 };
    pub const MAROON: Color = Color { r: 128, g: 0, b: 0, a: 255 };
    pub const OLIVE: Color = Color { r: 128, g: 128, b: 0, a: 255 };
    
    // Colores semitransparentes
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const SEMI_TRANSPARENT_BLACK: Color = Color { r: 0, g: 0, b: 0, a: 128 };
    pub const SEMI_TRANSPARENT_WHITE: Color = Color { r: 255, g: 255, b: 255, a: 128 };
}

/// Driver de Framebuffer
#[derive(Debug, Clone)]
pub struct FramebufferDriver {
    pub info: FramebufferInfo,
    buffer: *mut u8,
    is_initialized: bool,
    hardware_acceleration: HardwareAccelerationManager,
}

impl FramebufferDriver {
    /// Crear nuevo driver de framebuffer
    pub fn new() -> Self {
        Self {
            info: FramebufferInfo {
                base_address: 0,
                width: 0,
                height: 0,
                pixels_per_scan_line: 0,
                pixel_format: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                reserved_mask: 0,
            },
            buffer: ptr::null_mut(),
            is_initialized: false,
            hardware_acceleration: HardwareAccelerationManager::new(),
        }
    }
    
    /// Inicializar framebuffer con información de UEFI
    pub fn init_from_uefi(&mut self, 
                          base_address: u64,
                          width: u32,
                          height: u32,
                          pixels_per_scan_line: u32,
                          pixel_format: u32,
                          pixel_bitmask: u32) -> Result<(), &'static str> {
        
        // Validar parámetros básicos con más detalle
        if base_address == 0 {
            return Err("Invalid base address");
        }
        if width == 0 {
            return Err("Invalid width (cannot be zero)");
        }
        if height == 0 {
            return Err("Invalid height (cannot be zero)");
        }
        if pixels_per_scan_line == 0 && width == 0 {
            return Err("Both pixels_per_scan_line and width cannot be zero");
        }
        
        // Determinar formato de pixel
        let format = PixelFormat::from_uefi_format(pixel_format);
        if format == PixelFormat::Unknown {
            return Err("Unsupported pixel format");
        }
        
        // Calcular bytes por pixel usando el método del enum
        let bytes_per_pixel = format.bytes_per_pixel();

        // Log para debugging (comentado porque serial_write_hex32 no está disponible aquí)
        // serial_write_str("FB: format=");
        // serial_write_hex32(format);
        // serial_write_str(" bytes_per_pixel=");
        // serial_write_hex32(bytes_per_pixel as u32);
        // serial_write_str("\r\n");

        // Calcular pitch (bytes per scanline)
        let pitch = if pixels_per_scan_line > 0 {
            pixels_per_scan_line * bytes_per_pixel as u32
        } else {
            width * bytes_per_pixel as u32
        };

        // ✅ CORRECCIÓN: El pitch ya está en bytes, no multiplicar por bytes_per_pixel nuevamente
        // pixels_per_scan_line ya es el número de píxeles, multiplicarlo por bytes_per_pixel
        // nos da los bytes por línea correctamente
        
        // Calcular tamaño total del buffer
        let size = (height * pitch) as u64;
        
        // Configurar información del framebuffer
        // Evitar división por cero con validación adicional
        let pixels_per_line = if bytes_per_pixel > 0 && pitch > 0 {
            pitch / (bytes_per_pixel as u32)
        } else if pixels_per_scan_line > 0 {
            pixels_per_scan_line // Usar el valor original si hay problemas
        } else if width > 0 {
            width // Usar width como fallback
        } else {
            1920 // Valor por defecto seguro
        };

        self.info = FramebufferInfo {
            base_address,
            width,
            height,
            pixels_per_scan_line: pixels_per_line,
            pixel_format,
            red_mask: 0,      // Se configurarán según el formato
            green_mask: 0,
            blue_mask: 0,
            reserved_mask: 0,
        };
        
        // Configurar offsets según el formato
        self.configure_pixel_offsets();
        
        // ✅ MAPEO SEGURO: Verificar que la dirección sea válida
        if base_address < 0x1000 {
            return Err("Invalid framebuffer base address");
        }
        
        // Mapear memoria del framebuffer de forma segura
        self.buffer = base_address as *mut u8;
        
        // Validar que el mapeo sea válido
        if self.buffer.is_null() {
            return Err("Failed to map framebuffer memory");
        }
        
        // ✅ VALIDACIÓN ADICIONAL: Verificar que podemos leer el primer byte (de forma segura)
        unsafe {
            // Solo intentar leer si la dirección parece razonable
            if base_address >= 0x1000 && base_address < 0x100000000 { // Hasta 4GB
                // Intentar leer el primer byte para verificar que la memoria es accesible
                let test_byte = core::ptr::read_volatile(self.buffer);
                // Si llegamos aquí, la memoria es accesible
                core::ptr::write_volatile(self.buffer, test_byte); // Restaurar el valor original
            }
        }

        self.is_initialized = true;

        // ❌ REMOVER: No llamar clear_screen aquí para evitar page faults
        // La limpieza se hará después de la inicialización exitosa
        
        Ok(())
    }
    
    /// Inicializar aceleración de hardware con información de GPU
    pub fn init_hardware_acceleration(&mut self, gpu_info: &GpuInfo) -> Result<(), &'static str> {
        self.hardware_acceleration.initialize_with_gpu(gpu_info)
    }
    
    /// Obtener capacidades de aceleración de hardware
    pub fn get_acceleration_capabilities(&self) -> &AccelerationCapabilities {
        self.hardware_acceleration.get_capabilities()
    }
    
    /// Verificar si hay aceleración de hardware disponible
    pub fn has_hardware_acceleration(&self) -> bool {
        self.hardware_acceleration.has_acceleration()
    }
    
    /// Obtener tipo de aceleración de hardware
    pub fn get_acceleration_type(&self) -> HardwareAcceleration {
        self.hardware_acceleration.get_acceleration_type()
    }
    
    /// Configurar offsets de pixel según el formato
    fn configure_pixel_offsets(&mut self) {
        // Configurar máscaras según el formato de pixel
        match self.info.pixel_format {
            0 => { // RGB888
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            },
            1 => { // BGR888
                self.info.red_mask = 0x000000FF;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x00FF0000;
                self.info.reserved_mask = 0xFF000000;
            },
            2 => { // RGBA8888
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            },
            3 => { // BGRA8888
                self.info.red_mask = 0x0000FF00;
                self.info.green_mask = 0x00FF0000;
                self.info.blue_mask = 0xFF000000;
                self.info.reserved_mask = 0x000000FF;
            },
            _ => {
                // Formato desconocido, usar valores por defecto (RGBA8888)
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            }
        }
    }
    
    /// Verificar si el framebuffer está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }
    
    /// Obtener información del framebuffer
    pub fn get_info(&self) -> &FramebufferInfo {
        &self.info
    }
    
    /// Obtener puntero a pixel en coordenadas (x, y)
    fn get_pixel_ptr(&self, x: u32, y: u32) -> *mut u8 {
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return ptr::null_mut();
        }

        // Determinar bytes por pixel basado en el formato
        let bytes_per_pixel = match self.info.pixel_format {
            0 | 1 => 3, // RGB888, BGR888
            2 | 3 => 4, // RGBA8888, BGRA8888
            _ => 4,     // Por defecto 4 bytes
        };

        let offset = (y * self.info.pixels_per_scan_line * bytes_per_pixel + x * bytes_per_pixel) as isize;

        unsafe { self.buffer.offset(offset) }
    }
    
    /// Escribir un pixel de forma segura
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        // Las validaciones de inicialización y coordenadas ya son correctas.
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return;
        }
        
        // Obtener el puntero al píxel y el formato de forma centralizada.
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if pixel_ptr.is_null() {
            return;
        }
    
        // Convertir el color a un valor de 32 bits y determinar los bytes a escribir
        let bytes_per_pixel = self.bytes_per_pixel();
        let pixel_value = self.color_to_pixel(color);
        
        unsafe {
            match bytes_per_pixel {
                1 => {
                    let p = pixel_ptr as *mut u8;
                    core::ptr::write_volatile(p, pixel_value as u8);
                },
                2 => {
                    let p = pixel_ptr as *mut u16;
                    core::ptr::write_volatile(p, pixel_value as u16);
                },
                3 => {
                    // Escribir 3 bytes eficientemente sin 3 llamadas separadas.
                    let p = pixel_ptr as *mut u8;
                    let color_bytes = pixel_value.to_le_bytes(); // Convertir a un array de bytes
                    core::ptr::copy_nonoverlapping(color_bytes.as_ptr(), p, 3);
                },
                4 => {
                    let p = pixel_ptr as *mut u32;
                    core::ptr::write_volatile(p, pixel_value);
                },
                _ => { /* No hacer nada para formatos no soportados */ },
            }
        }
    }
    
    fn color_to_pixel(&self, color: Color) -> u32 {
        match self.info.pixel_format {
            0 => { // RGB888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32)
            },
            1 => { // BGR888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32)
            },
            2 => { // RGBA8888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            },
            3 => { // BGRA8888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32) | ((color.a as u32) << 24)
            },
            _ => { // Por defecto o formato desconocido
                // Usar una representación segura por defecto
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            }
        }
    }

    /// Leer un pixel
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            // Determinar bytes por pixel basado en el formato
            let bytes_per_pixel = match self.info.pixel_format {
                0 | 1 => 3, // RGB888, BGR888
                2 | 3 => 4, // RGBA8888, BGRA8888
                _ => 4,     // Por defecto 4 bytes
            };
            
            unsafe {
                let pixel_value = match bytes_per_pixel {
                    1 => *pixel_ptr as u32,
                    2 => {
                        let pixel_ptr_16 = pixel_ptr as *mut u16;
                        *pixel_ptr_16 as u32
                    },
                    3 => {
                        (*pixel_ptr as u32) |
                        ((*pixel_ptr.offset(1) as u32) << 8) |
                        ((*pixel_ptr.offset(2) as u32) << 16)
                    },
                    4 => {
                        let pixel_ptr_32 = pixel_ptr as *mut u32;
                        *pixel_ptr_32
                    },
                    _ => 0,
                };
                
                self.pixel_to_color(pixel_value)
            }
        } else {
            Color::BLACK
        }
    }
    
    /// Convertir valor de pixel a color
    fn pixel_to_color(&self, pixel_value: u32) -> Color {
        match self.info.pixel_format {
            2 => { // RGBA8888
                let r = ((pixel_value & self.info.red_mask) >> 16) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = (pixel_value & self.info.blue_mask) as u8;
                let a = ((pixel_value & self.info.reserved_mask) >> 24) as u8;
                Color::new(r, g, b, a)
            },
            3 => { // BGRA8888
                let r = ((pixel_value & self.info.red_mask) >> 8) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 16) as u8;
                let b = ((pixel_value & self.info.blue_mask) >> 24) as u8;
                let a = (pixel_value & self.info.reserved_mask) as u8;
                Color::new(r, g, b, a)
            },
            0 => { // RGB888
                let r = ((pixel_value & self.info.red_mask) >> 16) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = (pixel_value & self.info.blue_mask) as u8;
                Color::new(r, g, b, 255)
            },
            1 => { // BGR888
                let r = (pixel_value & self.info.red_mask) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = ((pixel_value & self.info.blue_mask) >> 16) as u8;
                Color::new(r, g, b, 255)
            },
            _ => Color::BLACK, // Formato desconocido
        }
    }
    
    /// Llenar rectángulo con color
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let end_x = core::cmp::min(x + width, self.info.width);
        let end_y = core::cmp::min(y + height, self.info.height);
        
        for py in y..end_y {
            for px in x..end_x {
                self.put_pixel(px, py, color);
            }
        }
    }
    
    /// Limpiar pantalla con color
    /// Limpia toda la pantalla con el color especificado.
    pub fn clear_screen(&mut self, color: Color) {
        let fb_ptr = self.info.base_address as *mut u32;
        let width = self.info.width;
        let height = self.info.height;

        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) as isize;
                unsafe {
                    core::ptr::write_volatile(fb_ptr.add(offset as usize), color.to_u32()); // Azul oscuro
                }
            }
        }
    }
    
    /// Dibujar un círculo usando el algoritmo del punto medio
    pub fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color) {
        let mut x = 0i32;
        let mut y = radius as i32;
        let mut d = 1 - radius as i32;

        while x <= y {
            // Dibujar 8 puntos simétricos del círculo usando put_pixel
            self.put_pixel_safe(center_x + x, center_y + y, color);
            self.put_pixel_safe(center_x - x, center_y + y, color);
            self.put_pixel_safe(center_x + x, center_y - y, color);
            self.put_pixel_safe(center_x - x, center_y - y, color);
            self.put_pixel_safe(center_x + y, center_y + x, color);
            self.put_pixel_safe(center_x - y, center_y + x, color);
            self.put_pixel_safe(center_x + y, center_y - x, color);
            self.put_pixel_safe(center_x - y, center_y - x, color);

            if d < 0 {
                d += 2 * x + 3;
            } else {
                d += 2 * (x - y) + 5;
                y -= 1;
            }
            x += 1;
        }
    }
    
    /// Dibujar un pixel usando acceso directo a memoria (como clear_screen)
    fn draw_pixel_direct(fb_ptr: *mut u8, width: u32, height: u32, pixels_per_scan_line: u32, x: i32, y: i32, color_value: u32, bytes_per_pixel: u32) {
        if x >= 0 && x < width as i32 && y >= 0 && y < height as i32 {
            let offset = (y as u32 * pixels_per_scan_line + x as u32) * bytes_per_pixel;
            unsafe {
                let pixel_ptr = fb_ptr.add(offset as usize);
                match bytes_per_pixel {
                    4 => {
                        // Formato de 32 bpp (ej. RGBA8888 o ARGB8888)
                        core::ptr::write_volatile(pixel_ptr as *mut u32, color_value);
                    }
                    3 => {
                        // Formato de 24 bpp (ej. RGB888 o BGR888)
                        // Suponemos que color_value es 0x00RRGGBB
                        let r = ((color_value >> 16) & 0xFF) as u8;
                        let g = ((color_value >> 8) & 0xFF) as u8;
                        let b = (color_value & 0xFF) as u8;
                        
                        // El orden de bytes puede depender de la configuración del framebuffer (RGB vs BGR)
                        // Aquí asumimos un orden BGR, que es común.
                        core::ptr::write_volatile(pixel_ptr.add(0), b);
                        core::ptr::write_volatile(pixel_ptr.add(1), g);
                        core::ptr::write_volatile(pixel_ptr.add(2), r);
                    }
                    _ => {
                        // Otros formatos como 16 bpp (RGB565) o 8 bpp (indexado) requerirían
                        // una conversión de color más compleja. Por ahora no se soportan.
                    }
                }
            }
        }
    }

    /// Versión segura de put_pixel que no falla en coordenadas inválidas
    fn put_pixel_safe(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && x < self.info.width as i32 && y >= 0 && y < self.info.height as i32 {
            self.put_pixel(x as u32, y as u32, color);
        }
    }
    
    /// Obtener dimensiones del framebuffer
    pub fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }
    
    /// Verificar si las coordenadas están dentro de los límites
    pub fn is_valid_coordinate(&self, x: u32, y: u32) -> bool {
        x < self.info.width && y < self.info.height
    }
    
    /// Obtener el formato de pixel actual
    pub fn pixel_format(&self) -> PixelFormat {
        PixelFormat::from_uefi_format(self.info.pixel_format)
    }
    
    /// Obtener bytes por pixel
    pub fn bytes_per_pixel(&self) -> u8 {
        self.pixel_format().bytes_per_pixel()
    }
    
    /// Obtener pitch (bytes por línea)
    pub fn pitch(&self) -> u32 {
        self.info.pixels_per_scan_line * self.bytes_per_pixel() as u32
    }
    
    /// Llenar rectángulo optimizado (versión rápida para colores sólidos)
    pub fn fill_rect_fast(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return;
        }
        
        let end_x = core::cmp::min(x + width, self.info.width);
        let end_y = core::cmp::min(y + height, self.info.height);
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        // Convertir color a pixel una sola vez
        let pixel_value = match self.info.pixel_format {
            0 => { // RGB888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32)
            },
            1 => { // BGR888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32)
            },
            2 => { // RGBA8888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            },
            3 => { // BGRA8888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32) | ((color.a as u32) << 24)
            },
            _ => { // Por defecto RGBA8888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            }
        };
        
        unsafe {
            for py in y..end_y {
                let row_start = self.buffer.offset((py * pitch + x * bytes_per_pixel as u32) as isize);
                
                match bytes_per_pixel {
                    1 => {
                        core::ptr::write_bytes(row_start, pixel_value as u8, (end_x - x) as usize);
                    },
                    2 => {
                        let row_start_16 = row_start as *mut u16;
                        let pixel_16 = pixel_value as u16;
                        for px in x..end_x {
                            core::ptr::write_volatile(row_start_16.offset((px - x) as isize), pixel_16);
                        }
                    },
                    3 => {
                        for px in x..end_x {
                            let pixel_ptr = row_start.offset((px - x) as isize * 3);
                            core::ptr::write_volatile(pixel_ptr, (pixel_value & 0xFF) as u8);
                            core::ptr::write_volatile(pixel_ptr.offset(1), ((pixel_value >> 8) & 0xFF) as u8);
                            core::ptr::write_volatile(pixel_ptr.offset(2), ((pixel_value >> 16) & 0xFF) as u8);
                        }
                    },
                    4 => {
                        let row_start_32 = row_start as *mut u32;
                        for px in x..end_x {
                            core::ptr::write_volatile(row_start_32.offset((px - x) as isize), pixel_value);
                        }
                    },
                    _ => {},
                }
            }
        }
    }
    
    pub fn draw_char(&mut self, x: u32, y: u32, ch: char, color: Color) {
        let char_width = 8;
        let char_height = 16;
    
        // Obtiene el mapa de bits (bitmap) del carácter.
        let bitmap = self.get_char_bitmap(ch);
        
        // Recorre cada fila y columna del mapa de bits.
        for row in 0..char_height {
            let bits = bitmap[row as usize];
            for col in 0..char_width {
                // Verifica si el bit correspondiente en el mapa está activado.
                if (bits & (1 << (7 - col))) != 0 {
                    let px = x + col;
                    let py = y + row;
                    
                    // Dibuja el píxel en el framebuffer usando tu función segura.
                    self.put_pixel_safe(px as i32, py as i32, color);
                }
            }
        }
    }

    /// Llenar pantalla completa optimizado
    pub fn clear_screen_fast(&mut self, color: Color) {
        self.fill_rect_fast(0, 0, self.info.width, self.info.height, color);
    }
    
    /// Dibujar línea horizontal optimizada
    pub fn draw_hline(&mut self, x: u32, y: u32, width: u32, color: Color) {
        if y < self.info.height {
            self.fill_rect_fast(x, y, width, 1, color);
        }
    }
    
    /// Dibujar línea vertical optimizada
    pub fn draw_vline(&mut self, x: u32, y: u32, height: u32, color: Color) {
        if x < self.info.width {
            self.fill_rect_fast(x, y, 1, height, color);
        }
    }
    
    /// Copiar región de memoria optimizada (para blit rápido)
    pub fn blit_fast(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_fb: &FramebufferDriver) {
        if !self.is_initialized || !src_fb.is_initialized {
            return;
        }
        
        let end_x = core::cmp::min(dst_x + width, self.info.width);
        let end_y = core::cmp::min(dst_y + height, self.info.height);
        let actual_width = end_x - dst_x;
        let actual_height = end_y - dst_y;
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let src_pitch = src_fb.pitch();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..actual_height {
                let src_row = src_fb.buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // Copiar línea completa de una vez
                core::ptr::copy_nonoverlapping(
                    src_row as *const u8,
                    dst_row as *mut u8,
                    (actual_width * bytes_per_pixel as u32) as usize
                );
            }
        }
    }
    
    /// Dibujar línea usando algoritmo de Bresenham
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
        let fb_ptr = self.info.base_address as *mut u8;
        let width = self.info.width;
        let height = self.info.height;
        let pixels_per_scan_line = self.info.pixels_per_scan_line;
        let color_value = color.to_u32();
        let bytes_per_pixel = self.bytes_per_pixel() as u32;

        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;

        let mut x = x1;
        let mut y = y1;

        loop {
            // Usar acceso directo a memoria como clear_screen
            FramebufferDriver::draw_pixel_direct(fb_ptr, width, height, pixels_per_scan_line, x, y, color_value, bytes_per_pixel);
            
            if x == x2 && y == y2 {
                break;
            }
            
            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }
    
    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let x1 = x;
        let y1 = y;
        let x2 = x + width - 1;
        let y2 = y + height - 1;

        self.draw_hline(x1, y1, width, color);
        self.draw_hline(x1, y2, width, color);
        self.draw_vline(x1, y1, height, color);
        self.draw_vline(x2, y1, height, color);
    }
    
    /// Copiar región de framebuffer
    pub fn blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32, src_fb: &FramebufferDriver) {
        let end_x = core::cmp::min(dst_x + width, self.info.width);
        let end_y = core::cmp::min(dst_y + height, self.info.height);
        
        for y in 0..height {
            for x in 0..width {
                let src_px = src_x + x;
                let src_py = src_y + y;
                let dst_px = dst_x + x;
                let dst_py = dst_y + y;
                
                if src_px < src_fb.info.width && src_py < src_fb.info.height &&
                   dst_px < self.info.width && dst_py < self.info.height {
                    let color = src_fb.get_pixel(src_px, src_py);
                    self.put_pixel(dst_px, dst_py, color);
                }
            }
        }
    }

    /// Escribir texto usando fuente simple de 8x16 pixels
    pub fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color) {
        let mut current_x = x;
        let char_width = 8;
        let char_height = 16;

        for ch in text.chars() {
            if current_x + char_width > self.info.width {
                break; // Salir si no cabe más texto
            }

            self.draw_char(current_x, y, ch, color);
            current_x += char_width;
        }
    }

    pub fn write_line(&mut self, text: &str, color: Color) {
        self.write_text(10, 10, text, color);
    }

    /// Obtener bitmap de un carácter (fuente simple)
    fn get_char_bitmap(&self, ch: char) -> [u8; 16] {
        // Aquí se define una fuente simple para caracteres ASCII.
        // Cada elemento del array es una fila de 8 píxeles.
        match ch {
            '0'..='9' => FONT_DATA[(ch as u8 - b'0') as usize],
            'A'..='Z' => FONT_DATA[(ch as u8 - b'A' + 10) as usize],
            'a'..='z' => FONT_DATA[(ch as u8 - b'a' + 10) as usize],
            ' ' => FONT_DATA[36],
            _ => FONT_DATA[36], // Carácter desconocido o no soportado
        }
    }
}

const FONT_DATA: [[u8; 16]; 37] = [
    [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0
    [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 1
    [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 2
    [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 3
    [0x0C, 0x1C, 0x2C, 0x4C, 0x7E, 0x0C, 0x0C, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 4
    [0x7E, 0x60, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 5
    [0x3C, 0x66, 0x60, 0x7C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 6
    [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 7
    [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 8
    [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 9
    [0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // A
    [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // B
    [0x3C, 0x66, 0x60, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // C
    [0x7C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // D
    [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // E
    [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // F
    [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // G
    [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // H
    [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // I
    [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x6C, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // J
    [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // K
    [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // L
    [0x66, 0x7E, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // M
    [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // N
    [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // O
    [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // P
    [0x3C, 0x66, 0x66, 0x66, 0x6E, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Q
    [0x7C, 0x66, 0x66, 0x7C, 0x78, 0x6C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // R
    [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // S
    [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // T
    [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // U
    [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // V
    [0x66, 0x66, 0x66, 0x66, 0x66, 0x7E, 0x7E, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // W
    [0x66, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // X
    [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Y
    [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Z
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Espacio o desconocido
];

// Implementar traits para FramebufferDriver
impl Drawable for FramebufferDriver {
    fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        FramebufferDriver::put_pixel(self, x, y, color);
    }
    
    fn get_pixel(&self, x: u32, y: u32) -> Color {
        FramebufferDriver::get_pixel(self, x, y)
    }
    
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        FramebufferDriver::fill_rect(self, x, y, width, height, color);
    }
    
    fn clear(&mut self, color: Color) {
        self.clear_screen(color);
    }
}

impl TextRenderer for FramebufferDriver {
    fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color) {
        FramebufferDriver::write_text(self, x, y, text, color);
    }
    
    fn char_dimensions(&self) -> (u32, u32) {
        (8, 16) // Tamaño de carácter de la fuente actual
    }
}

impl GeometryRenderer for FramebufferDriver {
    fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
        FramebufferDriver::draw_line(self, x1, y1, x2, y2, color);
    }
    
    fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        FramebufferDriver::draw_rect(self, x, y, width, height, color);
    }
    
    fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color) {
        FramebufferDriver::draw_circle(self, center_x, center_y, radius, color);
    }
}

impl Blittable for FramebufferDriver {
    fn blit_from<T: Drawable>(&mut self, _src: &T, src_x: u32, src_y: u32, 
                              dst_x: u32, dst_y: u32, width: u32, height: u32) {
        // Esta implementación es un placeholder
        // En una implementación real, necesitarías acceso a los datos de src
        // Por ahora, simplemente llenamos con negro
        self.fill_rect(dst_x, dst_y, width, height, Color::BLACK);
    }
}

impl HardwareAccelerated for FramebufferDriver {
    fn acceleration_type(&self) -> HardwareAcceleration {
        self.get_acceleration_type()
    }
    
    fn acceleration_capabilities(&self) -> AccelerationCapabilities {
        self.get_acceleration_capabilities().clone()
    }
    
    fn hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_blit {
            return Err("Hardware blit not supported");
        }
        
        if width > capabilities.max_blit_size.0 || height > capabilities.max_blit_size.1 {
            return Err("Blit size exceeds hardware limits");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_fill {
            return Err("Hardware fill not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_fill(x, y, width, height, color)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_fill(x, y, width, height, color)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_fill(x, y, width, height, color)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_alpha {
            return Err("Hardware alpha blending not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_scaling {
            return Err("Hardware scaling not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
}

// Implementaciones específicas de aceleración de hardware para cada fabricante

impl FramebufferDriver {
    /// Blit acelerado por hardware para Intel Graphics
    fn intel_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                           width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, aquí se configurarían los registros de Intel
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                core::ptr::copy_nonoverlapping(
                    src_row,
                    dst_row as *mut u8,
                    (width * bytes_per_pixel as u32) as usize
                );
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para Intel Graphics
    fn intel_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                for x_offset in 0..width {
                    let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                    match bytes_per_pixel {
                        1 => *pixel_offset = pixel as u8,
                        2 => *(pixel_offset as *mut u16) = pixel as u16,
                        3 => {
                            *pixel_offset = (pixel >> 16) as u8;
                            *pixel_offset.offset(1) = (pixel >> 8) as u8;
                            *pixel_offset.offset(2) = pixel as u8;
                        },
                        4 => *(pixel_offset as *mut u32) = pixel,
                        _ => return Err("Unsupported pixel format for Intel acceleration")
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para Intel Graphics
    fn intel_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                  color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para Intel Graphics
    fn intel_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                            dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
    
    /// Blit acelerado por hardware para NVIDIA
    fn nvidia_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                            width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, aquí se configurarían los registros de NVIDIA
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // NVIDIA puede manejar blits más grandes de una vez
                if width >= 64 {
                    // Blit optimizado para NVIDIA
                    core::ptr::copy_nonoverlapping(
                        src_row,
                        dst_row as *mut u8,
                        (width * bytes_per_pixel as u32) as usize
                    );
                } else {
                    // Blit pixel por pixel para áreas pequeñas
                    for x in 0..width {
                        let src_pixel = src_row.offset((x * bytes_per_pixel as u32) as isize);
                        let dst_pixel = dst_row.offset((x * bytes_per_pixel as u32) as isize);
                        core::ptr::copy_nonoverlapping(src_pixel, dst_pixel as *mut u8, bytes_per_pixel as usize);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para NVIDIA
    fn nvidia_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                // NVIDIA puede llenar líneas completas de una vez
                if width >= 32 {
                    core::ptr::write_bytes(row as *mut u8, pixel as u8, (width * bytes_per_pixel as u32) as usize);
                } else {
                    for x_offset in 0..width {
                        let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                        match bytes_per_pixel {
                            1 => *pixel_offset = pixel as u8,
                            2 => *(pixel_offset as *mut u16) = pixel as u16,
                            3 => {
                                *pixel_offset = (pixel >> 16) as u8;
                                *pixel_offset.offset(1) = (pixel >> 8) as u8;
                                *pixel_offset.offset(2) = pixel as u8;
                            },
                            4 => *(pixel_offset as *mut u32) = pixel,
                            _ => return Err("Unsupported pixel format for NVIDIA acceleration")
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para NVIDIA
    fn nvidia_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                   color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para NVIDIA
    fn nvidia_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                             dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
    
    /// Blit acelerado por hardware para AMD
    fn amd_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                         width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, aquí se configurarían los registros de AMD
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // AMD puede manejar blits medianos de una vez
                if width >= 32 {
                    core::ptr::copy_nonoverlapping(
                        src_row,
                        dst_row as *mut u8,
                        (width * bytes_per_pixel as u32) as usize
                    );
                } else {
                    for x in 0..width {
                        let src_pixel = src_row.offset((x * bytes_per_pixel as u32) as isize);
                        let dst_pixel = dst_row.offset((x * bytes_per_pixel as u32) as isize);
                        core::ptr::copy_nonoverlapping(src_pixel, dst_pixel as *mut u8, bytes_per_pixel as usize);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para AMD
    fn amd_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                // AMD puede llenar líneas medianas de una vez
                if width >= 16 {
                    core::ptr::write_bytes(row as *mut u8, pixel as u8, (width * bytes_per_pixel as u32) as usize);
                } else {
                    for x_offset in 0..width {
                        let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                        match bytes_per_pixel {
                            1 => *pixel_offset = pixel as u8,
                            2 => *(pixel_offset as *mut u16) = pixel as u16,
                            3 => {
                                *pixel_offset = (pixel >> 16) as u8;
                                *pixel_offset.offset(1) = (pixel >> 8) as u8;
                                *pixel_offset.offset(2) = pixel as u8;
                            },
                            4 => *(pixel_offset as *mut u32) = pixel,
                            _ => return Err("Unsupported pixel format for AMD acceleration")
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para AMD
    fn amd_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para AMD
    fn amd_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                          dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
}

/// Framebuffer global del sistema
static mut SYSTEM_FRAMEBUFFER: Option<FramebufferDriver> = None;

/// Inicializar framebuffer del sistema
pub fn init_framebuffer(base_address: u64,
                       width: u32,
                       height: u32,
                       pixels_per_scan_line: u32,
                       pixel_format: u32,
                       pixel_bitmask: u32) -> Result<(), &'static str> {
    let mut fb = FramebufferDriver::new();
    fb.init_from_uefi(base_address, width, height, pixels_per_scan_line, pixel_format, pixel_bitmask)?;
    
    unsafe {
        SYSTEM_FRAMEBUFFER = Some(fb);
    }
    
    Ok(())
}

/// Obtener referencia al framebuffer del sistema
pub fn get_framebuffer() -> Option<&'static mut FramebufferDriver> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_mut()
    }
}

/// Verificar si el framebuffer está disponible
pub fn is_framebuffer_available() -> bool {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map_or(false, |fb| fb.is_initialized())
    }
}

/// Obtener información del framebuffer
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map(|fb| fb.info)
    }
}

/// Inicializar framebuffer con información UEFI
/// Esta función es llamada desde el punto de entrada UEFI
pub fn init_framebuffer_from_uefi(uefi_fb_info: &crate::entry_simple::FramebufferInfo) -> Result<(), &'static str> {
    // ✅ CORREGIR: Mapeo correcto de formatos UEFI
    let pixel_format = match uefi_fb_info.pixel_format {
        0 => 0, // PixelRedGreenBlueReserved8BitPerColor
        1 => 1, // PixelBlueGreenRedReserved8BitPerColor  
        2 => 2, // PixelBitMask
        3 => 3, // PixelBltOnly
        _ => 0, // Default to RGB
    };

    // ✅ CORREGIR: Crear bitmask correcto sin truncar
    let pixel_bitmask = uefi_fb_info.red_mask |
                       (uefi_fb_info.green_mask << 8) |
                       (uefi_fb_info.blue_mask << 16) |
                       (uefi_fb_info.reserved_mask << 24);

    init_framebuffer(
        uefi_fb_info.base_address,
        uefi_fb_info.width,
        uefi_fb_info.height,
        uefi_fb_info.pixels_per_scan_line,
        pixel_format,
        pixel_bitmask
    )
}

/// Escribir texto en el framebuffer usando fuente simple
pub fn write_text(x: u32, y: u32, text: &str, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.write_text(x, y, text, color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Limpiar pantalla del framebuffer
pub fn clear_screen(color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.clear_screen(color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un rectángulo con bordes redondeados
pub fn draw_rounded_rect(x: u32, y: u32, width: u32, height: u32, radius: u32, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        // Implementación simple de rectángulo redondeado
        // Dibujar las esquinas como círculos
        if radius > 0 {
            fb.draw_circle((x + radius) as i32, (y + radius) as i32, radius, color);
            fb.draw_circle((x + width - radius - 1) as i32, (y + radius) as i32, radius, color);
            fb.draw_circle((x + radius) as i32, (y + height - radius - 1) as i32, radius, color);
            fb.draw_circle((x + width - radius - 1) as i32, (y + height - radius - 1) as i32, radius, color);
        }
        
        // Dibujar los lados rectos
        if width > radius * 2 {
            fb.fill_rect(x + radius, y, width - radius * 2, radius, color);
            fb.fill_rect(x + radius, y + height - radius, width - radius * 2, radius, color);
        }
        if height > radius * 2 {
            fb.fill_rect(x, y + radius, radius, height - radius * 2, color);
            fb.fill_rect(x + width - radius, y + radius, radius, height - radius * 2, color);
        }
        
        // Llenar el centro
        if width > radius * 2 && height > radius * 2 {
            fb.fill_rect(x + radius, y + radius, width - radius * 2, height - radius * 2, color);
        }
        
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un gradiente horizontal
pub fn draw_horizontal_gradient(x: u32, y: u32, width: u32, height: u32, 
                               start_color: Color, end_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        for i in 0..width {
            let factor = i as f32 / (width - 1) as f32;
            let r = (start_color.r as f32 * (1.0 - factor) + end_color.r as f32 * factor) as u8;
            let g = (start_color.g as f32 * (1.0 - factor) + end_color.g as f32 * factor) as u8;
            let b = (start_color.b as f32 * (1.0 - factor) + end_color.b as f32 * factor) as u8;
            let a = (start_color.a as f32 * (1.0 - factor) + end_color.a as f32 * factor) as u8;
            
            let color = Color::new(r, g, b, a);
            fb.fill_rect(x + i, y, 1, height, color);
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un gradiente vertical
pub fn draw_vertical_gradient(x: u32, y: u32, width: u32, height: u32, 
                             start_color: Color, end_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        for i in 0..height {
            let factor = i as f32 / (height - 1) as f32;
            let r = (start_color.r as f32 * (1.0 - factor) + end_color.r as f32 * factor) as u8;
            let g = (start_color.g as f32 * (1.0 - factor) + end_color.g as f32 * factor) as u8;
            let b = (start_color.b as f32 * (1.0 - factor) + end_color.b as f32 * factor) as u8;
            let a = (start_color.a as f32 * (1.0 - factor) + end_color.a as f32 * factor) as u8;
            
            let color = Color::new(r, g, b, a);
            fb.fill_rect(x, y + i, width, 1, color);
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar texto con sombra
pub fn write_text_with_shadow(x: u32, y: u32, text: &str, text_color: Color, 
                             shadow_color: Color, shadow_offset: (i32, i32)) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        // Dibujar sombra
        let shadow_x = (x as i32 + shadow_offset.0).max(0) as u32;
        let shadow_y = (y as i32 + shadow_offset.1).max(0) as u32;
        fb.write_text(shadow_x, shadow_y, text, shadow_color);
        
        // Dibujar texto principal
        fb.write_text(x, y, text, text_color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Obtener información detallada del framebuffer
pub fn get_framebuffer_details() -> Option<FramebufferDetails> {
    if let Some(fb) = get_framebuffer() {
        Some(FramebufferDetails {
            width: fb.info.width,
            height: fb.info.height,
            pixel_format: fb.pixel_format(),
            bytes_per_pixel: fb.bytes_per_pixel(),
            pitch: fb.pitch(),
            total_size: (fb.info.height * fb.pitch()) as u64,
            is_initialized: fb.is_initialized(),
        })
    } else {
        None
    }
}

/// Información detallada del framebuffer
#[derive(Debug, Clone, Copy)]
pub struct FramebufferDetails {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub bytes_per_pixel: u8,
    pub pitch: u32,
    pub total_size: u64,
    pub is_initialized: bool,
}

/// Sistema de capas para composición
pub struct LayerManager {
    layers: [Option<Layer>; 8], // Máximo 8 capas
    active_layers: u8,
}

/// Una capa individual
#[derive(Clone, Copy)]
pub struct Layer {
    pub id: u8,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub alpha: u8, // 0-255
    pub buffer: *mut u8,
    pub pitch: u32,
    pub bytes_per_pixel: u8,
}

impl LayerManager {
    pub fn new() -> Self {
        Self {
            layers: [None; 8],
            active_layers: 0,
        }
    }
    
    /// Crear una nueva capa
    pub fn create_layer(&mut self, id: u8, width: u32, height: u32, bytes_per_pixel: u8) -> Result<(), &'static str> {
        if id >= 8 {
            return Err("Layer ID must be less than 8");
        }
        
        if self.layers[id as usize].is_some() {
            return Err("Layer already exists");
        }
        
        let pitch = width * bytes_per_pixel as u32;
        let size = (height * pitch) as usize;
        
        // En un sistema real, aquí asignarías memoria
        // Por ahora, usamos un puntero nulo (esto necesitaría un allocator)
        let buffer = core::ptr::null_mut();
        
        self.layers[id as usize] = Some(Layer {
            id,
            x: 0,
            y: 0,
            width,
            height,
            visible: true,
            alpha: 255,
            buffer,
            pitch,
            bytes_per_pixel,
        });
        
        self.active_layers |= 1 << id;
        Ok(())
    }
    
    /// Eliminar una capa
    pub fn remove_layer(&mut self, id: u8) -> Result<(), &'static str> {
        if id >= 8 {
            return Err("Layer ID must be less than 8");
        }
        
        if self.layers[id as usize].is_none() {
            return Err("Layer does not exist");
        }
        
        self.layers[id as usize] = None;
        self.active_layers &= !(1 << id);
        Ok(())
    }
    
    /// Mostrar/ocultar una capa
    pub fn set_layer_visibility(&mut self, id: u8, visible: bool) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.visible = visible;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Establecer posición de una capa
    pub fn set_layer_position(&mut self, id: u8, x: u32, y: u32) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.x = x;
            layer.y = y;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Establecer transparencia de una capa
    pub fn set_layer_alpha(&mut self, id: u8, alpha: u8) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.alpha = alpha;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Componer todas las capas en el framebuffer principal
    pub fn compose_layers(&self, target_fb: &mut FramebufferDriver) {
        if !target_fb.is_initialized() {
            return;
        }
        
        // Limpiar el framebuffer principal
        target_fb.clear_screen_fast(Color::TRANSPARENT);
        
        // Dibujar capas en orden (capa 0 es la más baja)
        for i in 0..8 {
            if let Some(layer) = &self.layers[i] {
                if layer.visible && !layer.buffer.is_null() {
                    self.blit_layer_to_framebuffer(layer, target_fb);
                }
            }
        }
    }
    
    /// Copiar una capa al framebuffer principal
    fn blit_layer_to_framebuffer(&self, layer: &Layer, target_fb: &mut FramebufferDriver) {
        if layer.alpha == 0 {
            return; // Capa completamente transparente
        }
        
        let end_x = core::cmp::min(layer.x + layer.width, target_fb.info.width);
        let end_y = core::cmp::min(layer.y + layer.height, target_fb.info.height);
        let actual_width = end_x - layer.x;
        let actual_height = end_y - layer.y;
        
        if actual_width == 0 || actual_height == 0 {
            return;
        }
        
        // Si la capa es completamente opaca, usar blit rápido
        if layer.alpha == 255 {
            target_fb.blit_fast(0, 0, layer.x, layer.y, actual_width, actual_height, 
                               &FramebufferDriver {
                                   info: FramebufferInfo {
                                       base_address: layer.buffer as u64,
                                       width: layer.width,
                                       height: layer.height,
                                       pixels_per_scan_line: layer.width,
                                       pixel_format: target_fb.info.pixel_format,
                                       red_mask: 0,
                                       green_mask: 0,
                                       blue_mask: 0,
                                       reserved_mask: 0,
                                   },
                                   buffer: layer.buffer,
                                   is_initialized: true,
                                   hardware_acceleration: HardwareAccelerationManager::new(),
                               });
        } else {
            // Alpha blending pixel por pixel
            for y in 0..actual_height {
                for x in 0..actual_width {
                    // Leer pixel de la capa
                    let layer_pixel = self.get_layer_pixel(layer, x, y);
                    
                    // Leer pixel del framebuffer principal
                    let fb_pixel = target_fb.get_pixel(layer.x + x, layer.y + y);
                    
                    // Aplicar alpha blending
                    let blended = layer_pixel.blend(fb_pixel);
                    
                    // Escribir pixel resultante
                    target_fb.put_pixel(layer.x + x, layer.y + y, blended);
                }
            }
        }
    }
    
    /// Obtener pixel de una capa
    fn get_layer_pixel(&self, layer: &Layer, x: u32, y: u32) -> Color {
        if x >= layer.width || y >= layer.height || layer.buffer.is_null() {
            return Color::TRANSPARENT;
        }
        
        let offset = (y * layer.pitch + x * layer.bytes_per_pixel as u32) as isize;
        let pixel_ptr = unsafe { layer.buffer.offset(offset) };
        
        // Leer pixel según el formato (simplificado)
        unsafe {
            let pixel_value = match layer.bytes_per_pixel {
                1 => *pixel_ptr as u32,
                2 => {
                    let pixel_ptr_16 = pixel_ptr as *mut u16;
                    *pixel_ptr_16 as u32
                },
                3 => {
                    (*pixel_ptr as u32) |
                    ((*pixel_ptr.offset(1) as u32) << 8) |
                    ((*pixel_ptr.offset(2) as u32) << 16)
                },
                4 => {
                    let pixel_ptr_32 = pixel_ptr as *mut u32;
                    *pixel_ptr_32
                },
                _ => 0,
            };
            
            // Convertir a Color (simplificado)
            Color::from_hex_alpha(pixel_value)
        }
    }
}

/// Manager global de capas
static mut LAYER_MANAGER: Option<LayerManager> = None;

/// Inicializar el sistema de capas
pub fn init_layer_system() {
    unsafe {
        LAYER_MANAGER = Some(LayerManager::new());
    }
}

/// Obtener el manager de capas
pub fn get_layer_manager() -> Option<&'static mut LayerManager> {
    unsafe {
        LAYER_MANAGER.as_mut()
    }
}

/// Componer todas las capas
pub fn compose_all_layers() -> Result<(), &'static str> {
    if let Some(layer_mgr) = get_layer_manager() {
        if let Some(fb) = get_framebuffer() {
            layer_mgr.compose_layers(fb);
            Ok(())
        } else {
            Err("Framebuffer not initialized")
        }
    } else {
        Err("Layer system not initialized")
    }
}

/// Sprite o bitmap para dibujar
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub data: &'static [u8], // Datos de pixel en formato RGBA8888
    pub has_alpha: bool,
}

impl Sprite {
    /// Crear sprite desde datos de pixel
    pub fn new(width: u32, height: u32, data: &'static [u8], has_alpha: bool) -> Self {
        Self {
            width,
            height,
            data,
            has_alpha,
        }
    }
    
    /// Obtener pixel en coordenadas (x, y)
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.width || y >= self.height {
            return Color::TRANSPARENT;
        }
        
        let index = ((y * self.width + x) * 4) as usize;
        if index + 3 >= self.data.len() {
            return Color::TRANSPARENT;
        }
        
        Color::new(
            self.data[index],
            self.data[index + 1],
            self.data[index + 2],
            self.data[index + 3],
        )
    }
    
    /// Verificar si el pixel es transparente
    pub fn is_pixel_transparent(&self, x: u32, y: u32) -> bool {
        if !self.has_alpha {
            return false;
        }
        
        let pixel = self.get_pixel(x, y);
        pixel.a == 0
    }
}

/// Dibujar sprite en el framebuffer
pub fn draw_sprite(x: u32, y: u32, sprite: &Sprite) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        draw_sprite_to_framebuffer(fb, x, y, sprite);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar sprite en un framebuffer específico
fn draw_sprite_to_framebuffer(fb: &mut FramebufferDriver, x: u32, y: u32, sprite: &Sprite) {
    let end_x = core::cmp::min(x + sprite.width, fb.info.width);
    let end_y = core::cmp::min(y + sprite.height, fb.info.height);
    
    for sy in 0..(end_y - y) {
        for sx in 0..(end_x - x) {
            let pixel = sprite.get_pixel(sx, y + sy);
            
            if !sprite.is_pixel_transparent(sx, y + sy) {
                fb.put_pixel(x + sx, y + sy, pixel);
            }
        }
    }
}

/// Dibujar sprite con escalado
pub fn draw_sprite_scaled(x: u32, y: u32, sprite: &Sprite, scale: f32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let scaled_width = (sprite.width as f32 * scale) as u32;
        let scaled_height = (sprite.height as f32 * scale) as u32;
        
        let end_x = core::cmp::min(x + scaled_width, fb.info.width);
        let end_y = core::cmp::min(y + scaled_height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let sx = (dx as f32 / scale) as u32;
                let sy = (dy as f32 / scale) as u32;
                
                if sx < sprite.width && sy < sprite.height {
                    let pixel = sprite.get_pixel(sx, sy);
                    
                    if !sprite.is_pixel_transparent(sx, sy) {
                        fb.put_pixel(x + dx, y + dy, pixel);
                    }
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar sprite con rotación (simplificado - solo 90 grados)
pub fn draw_sprite_rotated(x: u32, y: u32, sprite: &Sprite, rotation: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let (width, height) = match rotation % 4 {
            1 | 3 => (sprite.height, sprite.width), // 90 o 270 grados
            _ => (sprite.width, sprite.height),     // 0 o 180 grados
        };
        
        let end_x = core::cmp::min(x + width, fb.info.width);
        let end_y = core::cmp::min(y + height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let (sx, sy) = match rotation % 4 {
                    0 => (dx, dy),                                    // 0 grados
                    1 => (dy, sprite.height - 1 - dx),               // 90 grados
                    2 => (sprite.width - 1 - dx, sprite.height - 1 - dy), // 180 grados
                    3 => (sprite.width - 1 - dy, dx),                // 270 grados
                    _ => (dx, dy),
                };
                
                if sx < sprite.width && sy < sprite.height {
                    let pixel = sprite.get_pixel(sx, sy);
                    
                    if !sprite.is_pixel_transparent(sx, sy) {
                        fb.put_pixel(x + dx, y + dy, pixel);
                    }
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Crear sprite desde patrón de colores
/// Nota: Esta función requiere un allocator para funcionar correctamente
pub fn create_sprite_from_pattern(width: u32, height: u32, pattern: &[Color]) -> Option<Sprite> {
    if pattern.len() != (width * height) as usize {
        return None;
    }
    
    // En un entorno no_std sin allocator, no podemos crear Vec dinámicamente
    // Esta función está aquí para completar la API, pero necesita un allocator
    // para funcionar correctamente en un sistema real
    None
}

/// Dibujar patrón de colores directamente
pub fn draw_pattern(x: u32, y: u32, width: u32, height: u32, pattern: &[Color]) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        if pattern.len() != (width * height) as usize {
            return Err("Pattern size mismatch");
        }
        
        let end_x = core::cmp::min(x + width, fb.info.width);
        let end_y = core::cmp::min(y + height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let pattern_index = ((dy * width + dx) as usize);
                if pattern_index < pattern.len() {
                    fb.put_pixel(x + dx, y + dy, pattern[pattern_index]);
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escribir texto escalado
pub fn write_text_scaled(x: u32, y: u32, text: &str, color: Color, scale: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8 * scale;
        let mut current_x = x;

        for ch in text.chars() {
            if current_x + char_width > fb.dimensions().0 {
                break;
            }
            draw_char_scaled(fb, current_x, y, ch, color, scale);
            current_x += char_width;
        }
        Ok(())
    } else {
        Err("Framebuffer not available")
    }
}

/// Dibujar un carácter escalado
fn draw_char_scaled(fb: &mut FramebufferDriver, x: u32, y: u32, ch: char, color: Color, scale: u32) {
    if scale == 0 {
        return;
    }

    let bitmap = fb.get_char_bitmap(ch);

    for row in 0..16 {
        let bits = bitmap[row];
        for col in 0..8 {
            if (bits & (1 << (7 - col))) != 0 {
                // Dibujar pixel escalado
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x + col * scale + sx;
                        let py = y + row as u32 * scale + sy;
                        if px < fb.info.width && py < fb.info.height {
                            fb.put_pixel(px, py, color);
                        }
                    }
                }
            }
        }
    }
}

/// Escribir texto con fondo
pub fn write_text_with_background(x: u32, y: u32, text: &str, 
                                 text_color: Color, bg_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8;
        let char_height = 16;
        let text_width = text.len() as u32 * char_width;
        
        // Dibujar fondo
        fb.fill_rect(x, y, text_width, char_height, bg_color);
        
        // Dibujar texto
        fb.write_text(x, y, text, text_color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escribir texto centrado
pub fn write_text_centered(y: u32, text: &str, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8;
        let text_width = text.len() as u32 * char_width;
        let x = if text_width < fb.info.width {
            (fb.info.width - text_width) / 2
        } else {
            0
        };
        
        fb.write_text(x, y, text, color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

// Funciones globales para aceleración de hardware

/// Inicializar aceleración de hardware del framebuffer
pub fn init_hardware_acceleration(gpu_info: &GpuInfo) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.init_hardware_acceleration(gpu_info)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Verificar si hay aceleración de hardware disponible
pub fn has_hardware_acceleration() -> bool {
    if let Some(fb) = get_framebuffer() {
        fb.has_hardware_acceleration()
    } else {
        false
    }
}

/// Obtener tipo de aceleración de hardware
pub fn get_acceleration_type() -> HardwareAcceleration {
    if let Some(fb) = get_framebuffer() {
        fb.get_acceleration_type()
    } else {
        HardwareAcceleration::None
    }
}

/// Obtener capacidades de aceleración de hardware
pub fn get_acceleration_capabilities() -> Option<AccelerationCapabilities> {
    if let Some(fb) = get_framebuffer() {
        Some(fb.get_acceleration_capabilities().clone())
    } else {
        None
    }
}

/// Blit acelerado por hardware
pub fn hardware_blit(src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Fill acelerado por hardware
pub fn hardware_fill(x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_fill(x, y, width, height, color)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Alpha blending acelerado por hardware
pub fn hardware_alpha_blend(x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_alpha_blend(x, y, width, height, color, alpha)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escalado acelerado por hardware
pub fn hardware_scale(src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Obtener información detallada de aceleración de hardware
pub fn get_hardware_acceleration_info() -> Option<String> {
    if let Some(fb) = get_framebuffer() {
        let capabilities = fb.get_acceleration_capabilities();
        let accel_type = fb.get_acceleration_type();
        
        let info = format!(
            "Hardware Acceleration: {:?}\n\
             Blit Support: {}\n\
             Fill Support: {}\n\
             Alpha Support: {}\n\
             Gradients Support: {}\n\
             Scaling Support: {}\n\
             Rotation Support: {}\n\
             Max Blit Size: {}x{}\n\
             Memory Bandwidth: {} MB/s",
            accel_type,
            capabilities.supports_hardware_blit,
            capabilities.supports_hardware_fill,
            capabilities.supports_hardware_alpha,
            capabilities.supports_hardware_gradients,
            capabilities.supports_hardware_scaling,
            capabilities.supports_hardware_rotation,
            capabilities.max_blit_size.0,
            capabilities.max_blit_size.1,
            capabilities.memory_bandwidth
        );
        
        Some(info)
    } else {
        None
    }
}
