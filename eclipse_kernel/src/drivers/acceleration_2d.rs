#![no_std]

use core::ptr;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::format;

use crate::drivers::framebuffer::{FramebufferDriver, PixelFormat, Color};
use crate::desktop_ai::{Point, Rect};
use crate::drivers::pci::{PciDevice, GpuInfo, GpuType};
use crate::drivers::intel_graphics::IntelGraphicsDriver;
use crate::drivers::nvidia_graphics::NvidiaGraphicsDriver;
use crate::drivers::amd_graphics::AmdGraphicsDriver;

/// Módulo de aceleración 2D para Eclipse OS
/// Proporciona funciones optimizadas de renderizado 2D que aprovechan el hardware gráfico

/// Estructura principal para aceleración 2D
#[derive(Debug, Clone)]
pub struct Acceleration2D {
    pub framebuffer: FramebufferDriver,
    pub intel_driver: Option<IntelGraphicsDriver>,
    pub nvidia_driver: Option<NvidiaGraphicsDriver>,
    pub amd_driver: Option<AmdGraphicsDriver>,
    pub acceleration_enabled: bool,
    pub hardware_type: HardwareAccelerationType,
}

/// Tipo de aceleración hardware disponible
#[derive(Debug, Clone, PartialEq)]
pub enum HardwareAccelerationType {
    None,
    Intel,
    Nvidia,
    Amd,
    Software, // Fallback a software
}

/// Operaciones de aceleración 2D disponibles
#[derive(Debug, Clone)]
pub enum AccelerationOperation {
    FillRect(Rect, Color),
    DrawRect(Rect, Color, u32), // rect, color, thickness
    DrawLine(Point, Point, Color, u32), // start, end, color, thickness
    Blit(Rect, Rect), // source, destination
    ClearScreen(Color),
    DrawCircle(Point, u32, Color, bool), // center, radius, color, filled
    DrawTriangle(Point, Point, Point, Color, bool), // p1, p2, p3, color, filled
}

/// Resultado de operación de aceleración
#[derive(Debug, Clone)]
pub enum AccelerationResult {
    Success,
    HardwareAccelerated,
    SoftwareFallback,
    UnsupportedOperation,
    DriverError(String),
}

impl Acceleration2D {
    /// Crear nueva instancia de aceleración 2D
    pub fn new(framebuffer: FramebufferDriver) -> Self {
        Self {
            framebuffer,
            intel_driver: None,
            nvidia_driver: None,
            amd_driver: None,
            acceleration_enabled: false,
            hardware_type: HardwareAccelerationType::None,
        }
    }

    /// Inicializar aceleración 2D con drivers de GPU
    pub fn initialize_with_gpu(&mut self, gpu_info: &GpuInfo) -> AccelerationResult {
        match gpu_info.gpu_type {
            GpuType::Intel => {
                let mut intel_driver = IntelGraphicsDriver::new(gpu_info.pci_device.clone());
                match intel_driver.initialize() {
                    Ok(_) => {
                        self.intel_driver = Some(intel_driver);
                        self.acceleration_enabled = true;
                        self.hardware_type = HardwareAccelerationType::Intel;
                        AccelerationResult::HardwareAccelerated
                    }
                    Err(e) => {
                        self.hardware_type = HardwareAccelerationType::Software;
                        AccelerationResult::DriverError(format!("Intel driver error: {}", e))
                    }
                }
            }
            GpuType::Nvidia => {
                let mut nvidia_driver = NvidiaGraphicsDriver::new(gpu_info.pci_device.clone(), gpu_info.clone());
                match nvidia_driver.init(None) {
                    Ok(_) => {
                        self.nvidia_driver = Some(nvidia_driver);
                        self.acceleration_enabled = true;
                        self.hardware_type = HardwareAccelerationType::Nvidia;
                        AccelerationResult::HardwareAccelerated
                    }
                    Err(e) => {
                        self.hardware_type = HardwareAccelerationType::Software;
                        AccelerationResult::DriverError(format!("NVIDIA driver error: {}", e))
                    }
                }
            }
            GpuType::Amd => {
                let mut amd_driver = AmdGraphicsDriver::new(gpu_info.pci_device.clone(), gpu_info.clone());
                match amd_driver.init(None) {
                    Ok(_) => {
                        self.amd_driver = Some(amd_driver);
                        self.acceleration_enabled = true;
                        self.hardware_type = HardwareAccelerationType::Amd;
                        AccelerationResult::HardwareAccelerated
                    }
                    Err(e) => {
                        self.hardware_type = HardwareAccelerationType::Software;
                        AccelerationResult::DriverError(format!("AMD driver error: {}", e))
                    }
                }
            }
            _ => {
                self.hardware_type = HardwareAccelerationType::Software;
                AccelerationResult::SoftwareFallback
            }
        }
    }

    /// Ejecutar operación de aceleración 2D
    pub fn execute_operation(&mut self, operation: AccelerationOperation) -> AccelerationResult {
        if self.acceleration_enabled {
            match self.hardware_type {
                HardwareAccelerationType::Intel => {
                    if let Some(ref mut driver) = self.intel_driver {
                        match operation {
                            AccelerationOperation::FillRect(rect, color) => {
                                driver.fill_rect(rect, color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawRect(rect, color, thickness) => {
                                driver.draw_rect(rect, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawLine(start, end, color, thickness) => {
                                driver.draw_line(start, end, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::Blit(src, dst) => {
                                driver.blit(src, dst, &mut self.framebuffer)
                            }
                            AccelerationOperation::ClearScreen(color) => {
                                driver.clear_screen(color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawCircle(center, radius, color, filled) => {
                                driver.draw_circle(center, radius, color, filled, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                                driver.draw_triangle(p1, p2, p3, color, filled, &mut self.framebuffer)
                            }
                        }
                    } else {
                        self.execute_software_operation(operation)
                    }
                }
                HardwareAccelerationType::Nvidia => {
                    if let Some(ref mut driver) = self.nvidia_driver {
                        match operation {
                            AccelerationOperation::FillRect(rect, color) => {
                                driver.fill_rect(rect, color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawRect(rect, color, thickness) => {
                                driver.draw_rect(rect, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawLine(start, end, color, thickness) => {
                                driver.draw_line(start, end, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::Blit(src, dst) => {
                                driver.blit(src, dst, &mut self.framebuffer)
                            }
                            AccelerationOperation::ClearScreen(color) => {
                                driver.clear_screen(color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawCircle(center, radius, color, filled) => {
                                driver.draw_circle(center, radius, color, filled, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                                driver.draw_triangle(p1, p2, p3, color, filled, &mut self.framebuffer)
                            }
                        }
                    } else {
                        self.execute_software_operation(operation)
                    }
                }
                HardwareAccelerationType::Amd => {
                    if let Some(ref mut driver) = self.amd_driver {
                        match operation {
                            AccelerationOperation::FillRect(rect, color) => {
                                driver.fill_rect(rect, color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawRect(rect, color, thickness) => {
                                driver.draw_rect(rect, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawLine(start, end, color, thickness) => {
                                driver.draw_line(start, end, color, thickness, &mut self.framebuffer)
                            }
                            AccelerationOperation::Blit(src, dst) => {
                                driver.blit(src, dst, &mut self.framebuffer)
                            }
                            AccelerationOperation::ClearScreen(color) => {
                                driver.clear_screen(color, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawCircle(center, radius, color, filled) => {
                                driver.draw_circle(center, radius, color, filled, &mut self.framebuffer)
                            }
                            AccelerationOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                                driver.draw_triangle(p1, p2, p3, color, filled, &mut self.framebuffer)
                            }
                        }
                    } else {
                        self.execute_software_operation(operation)
                    }
                }
                _ => {
                    self.execute_software_operation(operation)
                }
            }
        } else {
            // Fallback a software
            self.execute_software_operation(operation)
        }
    }


    /// Ejecutar operación usando software (fallback)
    fn execute_software_operation(&mut self, operation: AccelerationOperation) -> AccelerationResult {
        match operation {
            AccelerationOperation::FillRect(rect, color) => {
                self.framebuffer.fill_rect(rect.x, rect.y, rect.width, rect.height, color);
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::DrawRect(rect, color, thickness) => {
                self.framebuffer.draw_rect(rect.x, rect.y, rect.width, rect.height, color);
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::DrawLine(start, end, color, thickness) => {
                self.framebuffer.draw_line(start.x as i32, start.y as i32, end.x as i32, end.y as i32, color);
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::Blit(src, dst) => {
                // Simular blit copiando pixel por pixel
                for y in 0..src.height {
                    for x in 0..src.width {
                        let color = self.framebuffer.get_pixel(src.x + x, src.y + y);
                        self.framebuffer.put_pixel(dst.x + x, dst.y + y, color);
                    }
                }
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::ClearScreen(color) => {
                self.framebuffer.fill_rect(0, 0, self.framebuffer.info.width, self.framebuffer.info.height, color);
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::DrawCircle(center, radius, color, filled) => {
                self.draw_circle_software(center, radius, color, filled);
                AccelerationResult::SoftwareFallback
            }
            AccelerationOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                self.draw_triangle_software(p1, p2, p3, color, filled);
                AccelerationResult::SoftwareFallback
            }
        }
    }

    /// Dibujar círculo usando software
    fn draw_circle_software(&mut self, center: Point, radius: u32, color: Color, filled: bool) {
        let radius_i = radius as i32;
        let center_x = center.x as i32;
        let center_y = center.y as i32;

        for y in -radius_i..=radius_i {
            for x in -radius_i..=radius_i {
                let distance_squared = x * x + y * y;
                let radius_squared = radius_i * radius_i;

                let should_draw = if filled {
                    distance_squared <= radius_squared
                } else {
                    let inner_radius = (radius_i - 1).max(0);
                    let inner_radius_squared = inner_radius * inner_radius;
                    distance_squared <= radius_squared && distance_squared > inner_radius_squared
                };

                if should_draw {
                    let pixel_x = (center_x + x) as u32;
                    let pixel_y = (center_y + y) as u32;
                    
                    if pixel_x < self.framebuffer.info.width && pixel_y < self.framebuffer.info.height {
                        self.framebuffer.put_pixel(pixel_x, pixel_y, color);
                    }
                }
            }
        }
    }

    /// Dibujar triángulo usando software
    fn draw_triangle_software(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool) {
        if filled {
            self.fill_triangle_software(p1, p2, p3, color);
        } else {
            // Dibujar líneas del triángulo
            self.framebuffer.draw_line(p1.x as i32, p1.y as i32, p2.x as i32, p2.y as i32, color);
            self.framebuffer.draw_line(p2.x as i32, p2.y as i32, p3.x as i32, p3.y as i32, color);
            self.framebuffer.draw_line(p3.x as i32, p3.y as i32, p1.x as i32, p1.y as i32, color);
        }
    }

    /// Rellenar triángulo usando software (algoritmo de scanline)
    fn fill_triangle_software(&mut self, p1: Point, p2: Point, p3: Point, color: Color) {
        // Ordenar puntos por Y
        let mut points = [p1, p2, p3];
        points.sort_by_key(|p| p.y);

        let (top, middle, bottom) = (points[0], points[1], points[2]);

        // Rellenar desde top hasta middle
        self.fill_triangle_half(top, middle, bottom, color, true);
        
        // Rellenar desde middle hasta bottom
        self.fill_triangle_half(top, middle, bottom, color, false);
    }

    /// Rellenar la mitad superior o inferior del triángulo
    fn fill_triangle_half(&mut self, top: Point, middle: Point, bottom: Point, color: Color, upper_half: bool) {
        let start_y = if upper_half { top.y } else { middle.y };
        let end_y = if upper_half { middle.y } else { bottom.y };

        for y in start_y..=end_y {
            let mut x1 = 0u32;
            let mut x2 = 0u32;

            if upper_half {
                // Calcular intersecciones con las líneas top-middle y top-bottom
                if middle.y != top.y {
                    x1 = top.x + ((middle.x as i32 - top.x as i32) * (y as i32 - top.y as i32) / (middle.y as i32 - top.y as i32)) as u32;
                }
                if bottom.y != top.y {
                    x2 = top.x + ((bottom.x as i32 - top.x as i32) * (y as i32 - top.y as i32) / (bottom.y as i32 - top.y as i32)) as u32;
                }
            } else {
                // Calcular intersecciones con las líneas middle-bottom y top-bottom
                if bottom.y != middle.y {
                    x1 = middle.x + ((bottom.x as i32 - middle.x as i32) * (y as i32 - middle.y as i32) / (bottom.y as i32 - middle.y as i32)) as u32;
                }
                if bottom.y != top.y {
                    x2 = top.x + ((bottom.x as i32 - top.x as i32) * (y as i32 - top.y as i32) / (bottom.y as i32 - top.y as i32)) as u32;
                }
            }

            // Dibujar línea horizontal entre x1 y x2
            if x1 > x2 {
                core::mem::swap(&mut x1, &mut x2);
            }
            for x in x1..=x2 {
                if x < self.framebuffer.info.width && y < self.framebuffer.info.height {
                    self.framebuffer.put_pixel(x, y, color);
                }
            }
        }
    }

    /// Obtener información de aceleración
    pub fn get_acceleration_info(&self) -> String {
        format!(
            "Aceleración 2D: {} | Hardware: {:?} | Habilitada: {}",
            if self.acceleration_enabled { "Activa" } else { "Inactiva" },
            self.hardware_type,
            self.acceleration_enabled
        )
    }

    /// Verificar si una operación está soportada por hardware
    pub fn is_operation_supported(&self, operation: &AccelerationOperation) -> bool {
        self.acceleration_enabled && match operation {
            AccelerationOperation::FillRect(_, _) => true,
            AccelerationOperation::DrawRect(_, _, _) => true,
            AccelerationOperation::DrawLine(_, _, _, _) => true,
            AccelerationOperation::Blit(_, _) => true,
            AccelerationOperation::ClearScreen(_) => true,
            AccelerationOperation::DrawCircle(_, _, _, _) => true,
            AccelerationOperation::DrawTriangle(_, _, _, _, _) => true,
        }
    }
}

/// Trait para drivers de gráficos que soportan aceleración 2D
pub trait GraphicsDriver {
    fn fill_rect(&mut self, rect: Rect, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn draw_rect(&mut self, rect: Rect, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn draw_line(&mut self, start: Point, end: Point, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn blit(&mut self, src: Rect, dst: Rect, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn clear_screen(&mut self, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn draw_circle(&mut self, center: Point, radius: u32, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult;
    fn draw_triangle(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult;
}

/// Implementación del trait para Intel Graphics
impl GraphicsDriver for IntelGraphicsDriver {
    fn fill_rect(&mut self, rect: Rect, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::FillRect(rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel fill_rect failed".to_string()),
        }
    }

    fn draw_rect(&mut self, rect: Rect, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::DrawRect(rect, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel draw_rect failed".to_string()),
        }
    }

    fn draw_line(&mut self, start: Point, end: Point, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::DrawLine(start, end, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel draw_line failed".to_string()),
        }
    }

    fn blit(&mut self, src: Rect, dst: Rect, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::Blit(src, dst), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel blit failed".to_string()),
        }
    }

    fn clear_screen(&mut self, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        let screen_rect = Rect {
            x: 0,
            y: 0,
            width: fb.info.width,
            height: fb.info.height,
        };
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::FillRect(screen_rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel clear_screen failed".to_string()),
        }
    }

    fn draw_circle(&mut self, center: Point, radius: u32, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::DrawCircle(center, radius, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel draw_circle failed".to_string()),
        }
    }

    fn draw_triangle(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::intel_graphics::Intel2DOperation::DrawTriangle(p1, p2, p3, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("Intel draw_triangle failed".to_string()),
        }
    }
}

/// Implementación del trait para NVIDIA Graphics
impl GraphicsDriver for NvidiaGraphicsDriver {
    fn fill_rect(&mut self, rect: Rect, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::FillRect(rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA fill_rect failed".to_string()),
        }
    }

    fn draw_rect(&mut self, rect: Rect, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::DrawRect(rect, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA draw_rect failed".to_string()),
        }
    }

    fn draw_line(&mut self, start: Point, end: Point, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::DrawLine(start, end, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA draw_line failed".to_string()),
        }
    }

    fn blit(&mut self, src: Rect, dst: Rect, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::Blit(src, dst), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA blit failed".to_string()),
        }
    }

    fn clear_screen(&mut self, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        let screen_rect = Rect {
            x: 0,
            y: 0,
            width: fb.info.width,
            height: fb.info.height,
        };
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::FillRect(screen_rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA clear_screen failed".to_string()),
        }
    }

    fn draw_circle(&mut self, center: Point, radius: u32, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::DrawCircle(center, radius, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA draw_circle failed".to_string()),
        }
    }

    fn draw_triangle(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::nvidia_graphics::Nvidia2DOperation::DrawTriangle(p1, p2, p3, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("NVIDIA draw_triangle failed".to_string()),
        }
    }
}

/// Implementación del trait para AMD Graphics
impl GraphicsDriver for AmdGraphicsDriver {
    fn fill_rect(&mut self, rect: Rect, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::FillRect(rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD fill_rect failed".to_string()),
        }
    }

    fn draw_rect(&mut self, rect: Rect, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::DrawRect(rect, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD draw_rect failed".to_string()),
        }
    }

    fn draw_line(&mut self, start: Point, end: Point, color: Color, thickness: u32, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::DrawLine(start, end, color, thickness), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD draw_line failed".to_string()),
        }
    }

    fn blit(&mut self, src: Rect, dst: Rect, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::Blit(src, dst), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD blit failed".to_string()),
        }
    }

    fn clear_screen(&mut self, color: Color, fb: &mut FramebufferDriver) -> AccelerationResult {
        let screen_rect = Rect {
            x: 0,
            y: 0,
            width: fb.info.width,
            height: fb.info.height,
        };
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::FillRect(screen_rect, color), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD clear_screen failed".to_string()),
        }
    }

    fn draw_circle(&mut self, center: Point, radius: u32, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::DrawCircle(center, radius, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD draw_circle failed".to_string()),
        }
    }

    fn draw_triangle(&mut self, p1: Point, p2: Point, p3: Point, color: Color, filled: bool, fb: &mut FramebufferDriver) -> AccelerationResult {
        match self.render_2d(crate::drivers::amd_graphics::Amd2DOperation::DrawTriangle(p1, p2, p3, color, filled), fb) {
            Ok(_) => AccelerationResult::HardwareAccelerated,
            Err(_) => AccelerationResult::DriverError("AMD draw_triangle failed".to_string()),
        }
    }
}
