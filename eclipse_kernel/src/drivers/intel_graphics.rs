//! Driver Intel Graphics para Eclipse OS
//! 
//! Implementa un driver básico para gráficos integrados Intel
//! con soporte para aceleración 2D y gestión de memoria.

use core::ptr;
use core::mem;
use crate::drivers::pci::{PciDevice, GpuInfo, GpuType};
use crate::drivers::framebuffer::{FramebufferDriver, PixelFormat, Color};
use crate::desktop_ai::{Point, Rect};
use alloc::format;

/// IDs de dispositivos Intel Graphics conocidos
pub const INTEL_HD_GRAPHICS_2000: u16 = 0x0102;
pub const INTEL_HD_GRAPHICS_3000: u16 = 0x0116;
pub const INTEL_HD_GRAPHICS_4000: u16 = 0x0166;
pub const INTEL_HD_GRAPHICS_5000: u16 = 0x0D26;
pub const INTEL_HD_GRAPHICS_6000: u16 = 0x1626;
pub const INTEL_HD_GRAPHICS_620: u16 = 0x5916;
pub const INTEL_HD_GRAPHICS_630: u16 = 0x5912;
pub const INTEL_UHD_GRAPHICS_620: u16 = 0x5917;
pub const INTEL_UHD_GRAPHICS_630: u16 = 0x3E92;

/// Registros de configuración Intel Graphics
const INTEL_GMCH_CTRL: u32 = 0x50;
const INTEL_GMCH_STATUS: u32 = 0x54;
const INTEL_PCH_CTRL: u32 = 0x58;
const INTEL_PCH_STATUS: u32 = 0x5C;

/// Estados del driver Intel
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntelDriverState {
    Uninitialized,
    Initializing,
    Ready,
    Error,
    Suspended,
}

/// Información específica de Intel Graphics
#[derive(Debug, Clone)]
pub struct IntelGraphicsInfo {
    pub device_id: u16,
    pub generation: IntelGeneration,
    pub memory_size: u64,
    pub max_resolution: (u32, u32),
    pub supports_2d: bool,
    pub supports_3d: bool,
    pub supports_hdmi: bool,
    pub supports_dp: bool,
}

/// Generaciones de Intel Graphics
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntelGeneration {
    Gen1, // Iron Lake
    Gen2, // Sandy Bridge
    Gen3, // Ivy Bridge
    Gen4, // Haswell
    Gen5, // Broadwell
    Gen6, // Skylake
    Gen7, // Kaby Lake
    Gen8, // Coffee Lake
    Gen9, // Ice Lake
    Gen10, // Tiger Lake
    Gen11, // Alder Lake
    Gen12, // Raptor Lake
    Unknown,
}

impl IntelGeneration {
    pub fn from_device_id(device_id: u16) -> Self {
        match device_id {
            0x0102 => IntelGeneration::Gen1,
            0x0116 => IntelGeneration::Gen2,
            0x0166 => IntelGeneration::Gen3,
            0x0D26 => IntelGeneration::Gen4,
            0x1626 => IntelGeneration::Gen5,
            0x5916 | 0x5912 | 0x5917 => IntelGeneration::Gen6,
            0x3E92 => IntelGeneration::Gen7,
            _ => IntelGeneration::Unknown,
        }
    }
    
    pub fn as_str(&self) -> &'static str {
        match self {
            IntelGeneration::Gen1 => "Iron Lake",
            IntelGeneration::Gen2 => "Sandy Bridge",
            IntelGeneration::Gen3 => "Ivy Bridge",
            IntelGeneration::Gen4 => "Haswell",
            IntelGeneration::Gen5 => "Broadwell",
            IntelGeneration::Gen6 => "Skylake",
            IntelGeneration::Gen7 => "Kaby Lake",
            IntelGeneration::Gen8 => "Coffee Lake",
            IntelGeneration::Gen9 => "Ice Lake",
            IntelGeneration::Gen10 => "Tiger Lake",
            IntelGeneration::Gen11 => "Alder Lake",
            IntelGeneration::Gen12 => "Raptor Lake",
            IntelGeneration::Unknown => "Unknown",
        }
    }
}

/// Driver Intel Graphics
#[derive(Debug, Clone)]
pub struct IntelGraphicsDriver {
    pub pci_device: PciDevice,
    pub info: IntelGraphicsInfo,
    pub state: IntelDriverState,
    pub framebuffer: Option<FramebufferDriver>,
    pub memory_base: u64,
    pub memory_size: u64,
    pub mmio_base: u64,
    pub mmio_size: u64,
}

impl IntelGraphicsDriver {
    /// Crear nuevo driver Intel Graphics
    pub fn new(pci_device: PciDevice) -> Self {
        let device_id = pci_device.device_id;
        let generation = IntelGeneration::from_device_id(device_id);
        
        // Determinar capacidades basadas en la generación
        let (memory_size, max_resolution, supports_2d, supports_3d, supports_hdmi, supports_dp) = 
            Self::get_capabilities_for_generation(generation);
        
        let info = IntelGraphicsInfo {
            device_id,
            generation,
            memory_size,
            max_resolution,
            supports_2d,
            supports_3d,
            supports_hdmi,
            supports_dp,
        };
        
        Self {
            pci_device,
            info,
            state: IntelDriverState::Uninitialized,
            framebuffer: None,
            memory_base: 0,
            memory_size: 0,
            mmio_base: 0,
            mmio_size: 0,
        }
    }
    
    /// Obtener capacidades basadas en la generación
    fn get_capabilities_for_generation(generation: IntelGeneration) -> (u64, (u32, u32), bool, bool, bool, bool) {
        match generation {
            IntelGeneration::Gen1 => (64 * 1024 * 1024, (1920, 1080), true, false, false, false),
            IntelGeneration::Gen2 => (128 * 1024 * 1024, (2560, 1440), true, true, true, false),
            IntelGeneration::Gen3 => (256 * 1024 * 1024, (2560, 1440), true, true, true, true),
            IntelGeneration::Gen4 => (512 * 1024 * 1024, (3840, 2160), true, true, true, true),
            IntelGeneration::Gen5 => (1024 * 1024 * 1024, (3840, 2160), true, true, true, true),
            IntelGeneration::Gen6 => (1024 * 1024 * 1024, (3840, 2160), true, true, true, true),
            IntelGeneration::Gen7 => (1024 * 1024 * 1024, (3840, 2160), true, true, true, true),
            IntelGeneration::Gen8 => (2048 * 1024 * 1024, (5120, 2880), true, true, true, true),
            IntelGeneration::Gen9 => (2048 * 1024 * 1024, (5120, 2880), true, true, true, true),
            IntelGeneration::Gen10 => (4096 * 1024 * 1024, (7680, 4320), true, true, true, true),
            IntelGeneration::Gen11 => (8192 * 1024 * 1024, (7680, 4320), true, true, true, true),
            IntelGeneration::Gen12 => (16384 * 1024 * 1024, (7680, 4320), true, true, true, true),
            IntelGeneration::Unknown => (256 * 1024 * 1024, (1920, 1080), true, false, true, false),
        }
    }
    
    /// Inicializar el driver Intel Graphics
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.state != IntelDriverState::Uninitialized {
            return Err("Driver ya inicializado");
        }
        
        self.state = IntelDriverState::Initializing;
        
        // Configurar PCI
        self.configure_pci()?;
        
        // Configurar memoria
        self.configure_memory()?;
        
        // Configurar MMIO
        self.configure_mmio()?;
        
        // Inicializar framebuffer
        self.initialize_framebuffer()?;
        
        // Configurar aceleración 2D
        if self.info.supports_2d {
            self.configure_2d_acceleration()?;
        }
        
        self.state = IntelDriverState::Ready;
        Ok(())
    }
    
    /// Configurar PCI
    fn configure_pci(&mut self) -> Result<(), &'static str> {
        // Habilitar memoria y I/O
        let command = self.read_pci_config(0x04) | 0x07; // MEM, IO, BUS_MASTER
        self.write_pci_config(0x04, command);
        
        // Configurar BAR0 (memoria)
        let bar0 = self.read_pci_config(0x10);
        if bar0 & 0x01 == 0 { // Es memoria
            self.memory_base = (bar0 & 0xFFFFFFF0) as u64;
            self.memory_size = self.info.memory_size;
        }
        
        // Configurar BAR2 (MMIO)
        let bar2 = self.read_pci_config(0x18);
        if bar2 & 0x01 == 0 { // Es memoria
            self.mmio_base = (bar2 & 0xFFFFFFF0) as u64;
            self.mmio_size = 0x100000; // 1MB para MMIO
        }
        
        Ok(())
    }
    
    /// Configurar memoria
    fn configure_memory(&mut self) -> Result<(), &'static str> {
        if self.memory_base == 0 {
            return Err("No se pudo configurar memoria");
        }
        
        // Configurar tamaño de memoria
        self.write_mmio(INTEL_GMCH_CTRL, (self.memory_size / (1024 * 1024)) as u32);
        
        Ok(())
    }
    
    /// Configurar MMIO
    fn configure_mmio(&mut self) -> Result<(), &'static str> {
        if self.mmio_base == 0 {
            return Err("No se pudo configurar MMIO");
        }
        
        // Configurar registros básicos
        self.write_mmio(INTEL_GMCH_STATUS, 0x00000000);
        self.write_mmio(INTEL_PCH_CTRL, 0x00000000);
        self.write_mmio(INTEL_PCH_STATUS, 0x00000000);
        
        Ok(())
    }
    
    /// Inicializar framebuffer
    fn initialize_framebuffer(&mut self) -> Result<(), &'static str> {
        let (width, height) = self.info.max_resolution;
        let bpp = 32;
        let pitch = width * (bpp / 8);
        let size = pitch * height;
        
        // Crear framebuffer
        let framebuffer_info = crate::drivers::framebuffer::FramebufferInfo {
            base_address: self.memory_base,
            width,
            height,
            pixels_per_scan_line: width, // Usar width como valor por defecto
            pixel_format: 1, // BGRA8888
            red_mask: 0x0000FF00,      // BGRA: R en bits 8-15
            green_mask: 0x00FF0000,    // BGRA: G en bits 16-23
            blue_mask: 0xFF000000,     // BGRA: B en bits 24-31
            reserved_mask: 0x000000FF, // BGRA: A en bits 0-7
        };
        
        let framebuffer = FramebufferDriver::new();
        self.framebuffer = Some(framebuffer);
        
        Ok(())
    }
    
    /// Configurar aceleración 2D
    fn configure_2d_acceleration(&mut self) -> Result<(), &'static str> {
        // Configurar registros de aceleración 2D
        self.write_mmio(0x7000, 0x00000001); // Habilitar 2D
        self.write_mmio(0x7004, 0x00000000); // Reset 2D
        self.write_mmio(0x7008, 0x00000001); // Configurar 2D
        
        Ok(())
    }
    
    /// Leer configuración PCI
    fn read_pci_config(&self, offset: u8) -> u32 {
        let address = 0x80000000u32
            | ((self.pci_device.bus as u32) << 16)
            | ((self.pci_device.device as u32) << 11)
            | ((self.pci_device.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        
        unsafe {
            ptr::write_volatile(0xCF8 as *mut u32, address);
            ptr::read_volatile(0xCFC as *mut u32)
        }
    }
    
    /// Escribir configuración PCI
    fn write_pci_config(&self, offset: u8, value: u32) {
        let address = 0x80000000u32
            | ((self.pci_device.bus as u32) << 16)
            | ((self.pci_device.device as u32) << 11)
            | ((self.pci_device.function as u32) << 8)
            | ((offset as u32) & 0xFC);
        
        unsafe {
            ptr::write_volatile(0xCF8 as *mut u32, address);
            ptr::write_volatile(0xCFC as *mut u32, value);
        }
    }
    
    /// Leer MMIO
    fn read_mmio(&self, offset: u32) -> u32 {
        if self.mmio_base == 0 {
            return 0;
        }
        
        unsafe {
            ptr::read_volatile((self.mmio_base + offset as u64) as *const u32)
        }
    }
    
    /// Escribir MMIO
    fn write_mmio(&self, offset: u32, value: u32) {
        if self.mmio_base == 0 {
            return;
        }
        
        unsafe {
            ptr::write_volatile((self.mmio_base + offset as u64) as *mut u32, value);
        }
    }
    
    /// Obtener información del driver
    pub fn get_info(&self) -> &IntelGraphicsInfo {
        &self.info
    }
    
    /// Obtener estado del driver
    pub fn get_state(&self) -> IntelDriverState {
        self.state
    }
    
    /// Obtener framebuffer
    pub fn get_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        self.framebuffer.as_mut()
    }
    
    /// Verificar si está listo
    pub fn is_ready(&self) -> bool {
        self.state == IntelDriverState::Ready
    }
    
    /// Obtener información de memoria
    pub fn get_memory_info(&self) -> (u64, u64) {
        (self.memory_base, self.memory_size)
    }
    
    /// Obtener información de MMIO
    pub fn get_mmio_info(&self) -> (u64, u64) {
        (self.mmio_base, self.mmio_size)
    }
    
    /// Convertir color a pixel
    fn color_to_pixel(&self, color: Color, format: PixelFormat) -> u32 {
        match format {
            PixelFormat::BGRA8888 => {
                ((color.a as u32) << 24) | 
                ((color.b as u32) << 16) | 
                ((color.g as u32) << 8) | 
                (color.r as u32)
            },
            PixelFormat::RGBA8888 => {
                ((color.a as u32) << 24) | 
                ((color.r as u32) << 16) | 
                ((color.g as u32) << 8) | 
                (color.b as u32)
            },
            _ => {
                // Fallback a BGRA8888
                ((color.a as u32) << 24) | 
                ((color.b as u32) << 16) | 
                ((color.g as u32) << 8) | 
                (color.r as u32)
            }
        }
    }
    
    /// Renderizar operación 2D acelerada
    pub fn render_2d(&mut self, operation: Intel2DOperation, fb: &mut FramebufferDriver) -> Result<(), &'static str> {
        if !self.is_ready() || !self.info.supports_2d {
            return Err("2D no disponible");
        }
        
        match operation {
            Intel2DOperation::FillRect(rect, color) => {
                self.fill_rect_2d(rect, color)?;
            },
            Intel2DOperation::DrawRect(rect, color, thickness) => {
                self.draw_rect_2d(rect, color, thickness)?;
            },
            Intel2DOperation::DrawLine(start, end, color, thickness) => {
                self.draw_line_2d(start, end, color, thickness)?;
            },
            Intel2DOperation::Blit(src_rect, dst_rect) => {
                self.blit_2d(src_rect, dst_rect)?;
            },
            Intel2DOperation::DrawCircle(center, radius, color, filled) => {
                self.draw_circle_2d(center, radius, color, filled)?;
            },
            Intel2DOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                self.draw_triangle_2d(p1, p2, p3, color, filled)?;
            },
        }
        
        Ok(())
    }
    
    /// Rellenar rectángulo con aceleración 2D
    fn fill_rect_2d(&mut self, rect: Rect, color: Color) -> Result<(), &'static str> {
        // Configurar operación 2D
        self.write_mmio(0x7100, rect.x as u32); // X
        self.write_mmio(0x7104, rect.y as u32); // Y
        self.write_mmio(0x7108, rect.width as u32); // Width
        self.write_mmio(0x710C, rect.height as u32); // Height
        self.write_mmio(0x7110, self.color_to_pixel(color, PixelFormat::BGRA8888)); // Color
        
        // Ejecutar operación
        self.write_mmio(0x7000, 0x00000002); // FillRect command
        
        // Esperar completado
        while self.read_mmio(0x7000) & 0x80000000 != 0 {
            // Busy wait
        }
        
        Ok(())
    }
    
    /// Blit con aceleración 2D
    fn blit_2d(&mut self, src_rect: Rect, dst_rect: Rect) -> Result<(), &'static str> {
        // Configurar operación Blit
        self.write_mmio(0x7200, src_rect.x as u32); // Src X
        self.write_mmio(0x7204, src_rect.y as u32); // Src Y
        self.write_mmio(0x7208, dst_rect.x as u32); // Dst X
        self.write_mmio(0x720C, dst_rect.y as u32); // Dst Y
        self.write_mmio(0x7210, src_rect.width as u32); // Width
        self.write_mmio(0x7214, src_rect.height as u32); // Height
        
        // Ejecutar operación
        self.write_mmio(0x7000, 0x00000004); // Blit command
        
        // Esperar completado
        while self.read_mmio(0x7000) & 0x80000000 != 0 {
            // Busy wait
        }
        
        Ok(())
    }
    
    /// Dibujar línea con aceleración 2D
    fn draw_line_2d(&mut self, start: Point, end: Point, color: Color, thickness: u32) -> Result<(), &'static str> {
        // Configurar operación Line
        self.write_mmio(0x7300, start.x as u32); // Start X
        self.write_mmio(0x7304, start.y as u32); // Start Y
        self.write_mmio(0x7308, end.x as u32); // End X
        self.write_mmio(0x730C, end.y as u32); // End Y
        self.write_mmio(0x7310, self.color_to_pixel(color, PixelFormat::BGRA8888)); // Color
        self.write_mmio(0x7314, thickness); // Thickness
        
        // Ejecutar operación
        self.write_mmio(0x7000, 0x00000008); // Line command
        
        // Esperar completado
        while self.read_mmio(0x7000) & 0x80000000 != 0 {
            // Busy wait
        }
        
        Ok(())
    }
    
    /// Dibujar rectángulo con aceleración 2D
    fn draw_rect_2d(&mut self, rect: Rect, color: Color, thickness: u32) -> Result<(), &'static str> {
        // Dibujar los 4 lados del rectángulo
        let top_left = Point { x: rect.x, y: rect.y };
        let top_right = Point { x: rect.x + rect.width, y: rect.y };
        let bottom_left = Point { x: rect.x, y: rect.y + rect.height };
        let bottom_right = Point { x: rect.x + rect.width, y: rect.y + rect.height };
        
        // Línea superior
        self.draw_line_2d(top_left, top_right, color, thickness)?;
        // Línea derecha
        self.draw_line_2d(top_right, bottom_right, color, thickness)?;
        // Línea inferior
        self.draw_line_2d(bottom_right, bottom_left, color, thickness)?;
        // Línea izquierda
        self.draw_line_2d(bottom_left, top_left, color, thickness)?;
        
        Ok(())
    }
    
    /// Dibujar círculo con aceleración 2D
    fn draw_circle_2d(&mut self, center: Point, radius: u32, color: Color, filled: bool) -> Result<(), &'static str> {
        // Configurar operación de círculo
        self.write_mmio(0x7400, center.x as u32); // Center X
        self.write_mmio(0x7404, center.y as u32); // Center Y
        self.write_mmio(0x7408, radius); // Radius
        self.write_mmio(0x740C, self.color_to_pixel(color, PixelFormat::BGRA8888)); // Color
        self.write_mmio(0x7410, if filled { 1 } else { 0 }); // Filled flag
        
        // Ejecutar operación
        self.write_mmio(0x7000, 0x00000010); // DrawCircle command
        
        // Esperar completado
        while self.read_mmio(0x7000) & 0x80000000 != 0 {
            // Busy wait
        }
        
        Ok(())
    }
    
    /// Dibujar triángulo con aceleración 2D
    fn draw_triangle_2d(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool) -> Result<(), &'static str> {
        if filled {
            // Rellenar triángulo usando scanline
            self.fill_triangle_2d(p1, p2, p3, color)?;
        } else {
            // Dibujar líneas del triángulo
            self.draw_line_2d(p1, p2, color, 1)?;
            self.draw_line_2d(p2, p3, color, 1)?;
            self.draw_line_2d(p3, p1, color, 1)?;
        }
        
        Ok(())
    }
    
    /// Rellenar triángulo con aceleración 2D
    fn fill_triangle_2d(&mut self, p1: Point, p2: Point, p3: Point, color: Color) -> Result<(), &'static str> {
        // Configurar operación de triángulo
        self.write_mmio(0x7500, p1.x as u32); // P1 X
        self.write_mmio(0x7504, p1.y as u32); // P1 Y
        self.write_mmio(0x7508, p2.x as u32); // P2 X
        self.write_mmio(0x750C, p2.y as u32); // P2 Y
        self.write_mmio(0x7510, p3.x as u32); // P3 X
        self.write_mmio(0x7514, p3.y as u32); // P3 Y
        self.write_mmio(0x7518, self.color_to_pixel(color, PixelFormat::BGRA8888)); // Color
        
        // Ejecutar operación
        self.write_mmio(0x7000, 0x00000020); // FillTriangle command
        
        // Esperar completado
        while self.read_mmio(0x7000) & 0x80000000 != 0 {
            // Busy wait
        }
        
        Ok(())
    }
}

/// Operaciones 2D soportadas
#[derive(Debug, Clone)]
pub enum Intel2DOperation {
    FillRect(Rect, Color),
    DrawRect(Rect, Color, u32), // rect, color, thickness
    DrawLine(Point, Point, Color, u32), // start, end, color, thickness
    Blit(Rect, Rect), // source, destination
    DrawCircle(Point, u32, Color, bool), // center, radius, color, filled
    DrawTriangle(Point, Point, Point, Color, bool), // p1, p2, p3, color, filled
}

/// Función de conveniencia para crear driver Intel
pub fn create_intel_driver(pci_device: PciDevice) -> IntelGraphicsDriver {
    IntelGraphicsDriver::new(pci_device)
}
