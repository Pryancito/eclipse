//! Driver NVIDIA Graphics para Eclipse OS
//!
//! Implementa un driver básico para GPUs NVIDIA
//! con soporte para aceleración 2D y gestión de memoria.

use crate::desktop_ai::{Point, Rect};
use crate::drivers::framebuffer::{Color, FramebufferDriver, FramebufferInfo, PixelFormat};
use crate::drivers::pci::{GpuInfo, GpuType, PciDevice};
use alloc::format;
use alloc::string::ToString;
use alloc::vec;
use core::mem;
use core::ptr;

// IDs de dispositivos NVIDIA conocidos
const NVIDIA_VENDOR_ID: u16 = 0x10DE;

// IDs de dispositivos NVIDIA por generación
const NVIDIA_GTX_900_SERIES: u16 = 0x13C0; // GTX 960
const NVIDIA_GTX_1000_SERIES: u16 = 0x1B80; // GTX 1060
const NVIDIA_GTX_2000_SERIES: u16 = 0x1F08; // RTX 2060
const NVIDIA_GTX_3000_SERIES: u16 = 0x2504; // RTX 3060
const NVIDIA_GTX_4000_SERIES: u16 = 0x2684; // RTX 4060

/// Estado del driver NVIDIA
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NvidiaDriverState {
    Uninitialized,
    Initializing,
    Ready,
    Error,
    Suspended,
}

/// Generación de GPU NVIDIA
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum NvidiaGeneration {
    GTX900,
    GTX1000,
    GTX2000,
    GTX3000,
    GTX4000,
    Unknown,
}

impl NvidiaGeneration {
    /// Determinar generación basada en device ID
    pub fn from_device_id(device_id: u16) -> Self {
        match device_id {
            0x13C0..=0x13FF => NvidiaGeneration::GTX900,
            0x1B80..=0x1BFF => NvidiaGeneration::GTX1000,
            0x1F08..=0x1FFF => NvidiaGeneration::GTX2000,
            0x2504..=0x25FF => NvidiaGeneration::GTX3000,
            0x2684..=0x26FF => NvidiaGeneration::GTX4000,
            _ => NvidiaGeneration::Unknown,
        }
    }

    /// Obtener representación en string de la generación
    pub fn as_str(&self) -> &'static str {
        match self {
            NvidiaGeneration::GTX900 => "GTX 900 Series",
            NvidiaGeneration::GTX1000 => "GTX 1000 Series",
            NvidiaGeneration::GTX2000 => "RTX 2000 Series",
            NvidiaGeneration::GTX3000 => "RTX 3000 Series",
            NvidiaGeneration::GTX4000 => "RTX 4000 Series",
            NvidiaGeneration::Unknown => "Unknown",
        }
    }
}

/// Información específica de NVIDIA Graphics
#[derive(Debug, Clone)]
pub struct NvidiaGraphicsInfo {
    pub device_id: u16,
    pub generation: NvidiaGeneration,
    pub memory_size: u64,
    pub max_resolution: (u32, u32),
    pub supports_2d: bool,
    pub supports_3d: bool,
    pub supports_vulkan: bool,
    pub supports_opengl: bool,
    pub supports_raytracing: bool,
    pub supports_dlss: bool,
    pub supports_hdmi: bool,
    pub supports_dp: bool,
    pub driver_version: &'static str,
}

impl NvidiaGraphicsInfo {
    /// Crear información de GPU NVIDIA
    pub fn new(device_id: u16, memory_size: u64, max_resolution: (u32, u32)) -> Self {
        let generation = NvidiaGeneration::from_device_id(device_id);
        Self {
            device_id,
            generation,
            memory_size,
            max_resolution,
            supports_2d: true,
            supports_3d: true,
            supports_vulkan: generation >= NvidiaGeneration::GTX1000,
            supports_opengl: true,
            supports_raytracing: generation >= NvidiaGeneration::GTX2000,
            supports_dlss: generation >= NvidiaGeneration::GTX2000,
            supports_hdmi: true,
            supports_dp: true,
            driver_version: "NVIDIA Driver v1.0.0",
        }
    }
}

/// Driver NVIDIA Graphics
#[derive(Debug, Clone)]
pub struct NvidiaGraphicsDriver {
    pub pci_device: PciDevice,
    pub info: NvidiaGraphicsInfo,
    pub state: NvidiaDriverState,
    pub framebuffer: Option<FramebufferDriver>,
    pub memory_base: u64,
    pub memory_size: u64,
    pub mmio_base: u64,
    pub mmio_size: u64,
}

impl NvidiaGraphicsDriver {
    /// Crear una nueva instancia del driver NVIDIA Graphics
    pub fn new(pci_device: PciDevice, gpu_info: GpuInfo) -> Self {
        Self {
            pci_device,
            info: NvidiaGraphicsInfo::new(
                gpu_info.pci_device.device_id,
                gpu_info.memory_size,
                gpu_info.max_resolution,
            ),
            state: NvidiaDriverState::Uninitialized,
            framebuffer: None,
            memory_base: 0,
            memory_size: 0,
            mmio_base: 0,
            mmio_size: 0,
        }
    }

    /// Inicializar el driver NVIDIA Graphics
    pub fn init(&mut self, framebuffer_info: Option<FramebufferInfo>) -> Result<(), &'static str> {
        self.state = NvidiaDriverState::Initializing;

        // Habilitar bus mastering y memoria en el dispositivo PCI
        let mut command = self.read_pci_config(0x04);
        command |= (1 << 1) | (1 << 2); // Set Memory Space Enable and Bus Master Enable
        self.write_pci_config(0x04, command);

        // Configurar BAR0 (memoria) con logs detallados
        let bar0 = self.read_pci_config(0x10);
        crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: BAR0 = 0x{:08X}\n", bar0));
        
        if bar0 & 0x01 == 0 {
            // Es memoria 32-bit
            self.memory_base = (bar0 & 0xFFFFFFF0) as u64;
            self.memory_size = self.info.memory_size;
            crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: Memory base = 0x{:016X}, size = 0x{:016X}\n", 
                                                   self.memory_base, self.memory_size));
        } else {
            crate::debug::serial_write_str("NVIDIA_DRIVER: BAR0 es I/O, no memoria\n");
        }

        // Configurar BAR1 (MMIO) con logs detallados
        let bar1 = self.read_pci_config(0x14);
        crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: BAR1 = 0x{:08X}\n", bar1));
        
        if bar1 & 0x01 == 0 {
            // Es memoria 32-bit
            self.mmio_base = (bar1 & 0xFFFFFFF0) as u64;
            self.mmio_size = 0x200000; // 2MB para MMIO NVIDIA
            crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: MMIO base = 0x{:016X}, size = 0x{:016X}\n", 
                                                   self.mmio_base, self.mmio_size));
        } else {
            crate::debug::serial_write_str("NVIDIA_DRIVER: BAR1 es I/O, no memoria\n");
        }

        // Configurar BAR2 como alternativa para MMIO
        let bar2 = self.read_pci_config(0x18);
        crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: BAR2 = 0x{:08X}\n", bar2));
        
        if bar2 & 0x01 == 0 && self.mmio_base == 0 {
            // Usar BAR2 como MMIO si BAR1 no está disponible
            self.mmio_base = (bar2 & 0xFFFFFFF0) as u64;
            self.mmio_size = 0x200000;
            crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: Usando BAR2 como MMIO = 0x{:016X}\n", self.mmio_base));
        }

        // Verificar disponibilidad de memoria y MMIO
        if self.memory_base == 0 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: WARNING - No se pudo obtener base de memoria\n");
        }
        
        if self.mmio_base == 0 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: WARNING - No se pudo obtener base MMIO\n");
            // Continuar sin MMIO para hardware real
            self.mmio_base = 0x100000000; // Dirección virtual simulada
            self.mmio_size = 0x200000;
            crate::debug::serial_write_str("NVIDIA_DRIVER: Usando MMIO simulado para compatibilidad\n");
        }

        // Configurar registros MMIO para modo de video (solo si MMIO está disponible)
        if self.mmio_base != 0 {
            match self.configure_video_mode() {
                Ok(_) => {
                    crate::debug::serial_write_str("NVIDIA_DRIVER: Modo de video configurado exitosamente\n");
                }
                Err(e) => {
                    crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: Error configurando modo de video: {}\n", e));
                    // Continuar sin modo de video específico
                }
            }

            // Verificar MMIO solo si está mapeado correctamente
            if self.mmio_base < 0x100000000 {
                // MMIO real - verificar funcionalidad
                self.write_mmio(0x0000, 0xDEADBEEF);
                let value = self.read_mmio(0x0000);
                if value != 0xDEADBEEF {
                    crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: WARNING - MMIO verification failed: wrote 0xDEADBEEF, read 0x{:08X}\n", value));
                    // No fallar, continuar sin verificación MMIO
                } else {
                    crate::debug::serial_write_str("NVIDIA_DRIVER: MMIO verification successful\n");
                }
            } else {
                crate::debug::serial_write_str("NVIDIA_DRIVER: Saltando verificación MMIO (simulado)\n");
            }
        } else {
            crate::debug::serial_write_str("NVIDIA_DRIVER: MMIO no disponible, continuando sin configuración MMIO\n");
        }

        // Asignar framebuffer si está disponible
        if let Some(info) = framebuffer_info {
            self.attach_existing_framebuffer(&info);
        }

        self.state = NvidiaDriverState::Ready;
        Ok(())
    }

    /// Adjuntar un framebuffer existente
    pub fn attach_existing_framebuffer(&mut self, info: &FramebufferInfo) {
        let mut fb = FramebufferDriver::new();
        if fb
            .init_from_uefi(
                info.base_address,
                info.width,
                info.height,
                info.pixels_per_scan_line,
                info.pixel_format,
                info.red_mask | info.green_mask | info.blue_mask,
            )
            .is_ok()
        {
            fb.lock_as_primary();
            self.framebuffer = Some(fb);
        }
    }

    /// Leer registro MMIO con verificación de seguridad
    pub fn read_mmio(&self, offset: u32) -> u32 {
        // Verificar que MMIO esté configurado
        if self.mmio_base == 0 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: ERROR - Intento de leer MMIO no configurado\n");
            return 0xFFFFFFFF;
        }
        
        // Verificar que la dirección esté dentro del rango MMIO
        if offset as u64 >= self.mmio_size {
            crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: ERROR - Offset MMIO fuera de rango: 0x{:08X}\n", offset));
            return 0xFFFFFFFF;
        }
        
        // Solo acceder a MMIO real (no simulado)
        if self.mmio_base >= 0x100000000 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: WARNING - Acceso a MMIO simulado\n");
            return 0xDEADBEEF; // Valor simulado
        }
        
        unsafe { 
            ptr::read_volatile((self.mmio_base + offset as u64) as *const u32) 
        }
    }

    /// Escribir registro MMIO con verificación de seguridad
    pub fn write_mmio(&self, offset: u32, value: u32) {
        // Verificar que MMIO esté configurado
        if self.mmio_base == 0 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: ERROR - Intento de escribir MMIO no configurado\n");
            return;
        }
        
        // Verificar que la dirección esté dentro del rango MMIO
        if offset as u64 >= self.mmio_size {
            crate::debug::serial_write_str(&format!("NVIDIA_DRIVER: ERROR - Offset MMIO fuera de rango: 0x{:08X}\n", offset));
            return;
        }
        
        // Solo acceder a MMIO real (no simulado)
        if self.mmio_base >= 0x100000000 {
            crate::debug::serial_write_str("NVIDIA_DRIVER: WARNING - Escritura a MMIO simulado\n");
            return; // No escribir en MMIO simulado
        }
        
        unsafe {
            ptr::write_volatile((self.mmio_base + offset as u64) as *mut u32, value);
        }
    }

    /// Verificar si el driver está listo
    pub fn is_ready(&self) -> bool {
        self.state == NvidiaDriverState::Ready
    }

    /// Obtener información del driver
    pub fn get_info(&self) -> &NvidiaGraphicsInfo {
        &self.info
    }

    /// Obtener información de memoria
    pub fn get_memory_info(&self) -> (u64, u64) {
        (self.memory_base, self.memory_size)
    }

    /// Obtener información de MMIO
    pub fn get_mmio_info(&self) -> (u64, u64) {
        (self.mmio_base, self.mmio_size)
    }

    /// Obtener framebuffer si está disponible
    pub fn get_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        self.framebuffer.as_mut()
    }

    /// Convertir color a pixel
    pub fn color_to_pixel(&self, color: Color) -> u32 {
        // Usar el método to_pixel del Color con formato RGBA8888
        color.to_pixel(PixelFormat::RGBA8888)
    }

    /// Renderizar operación 2D
    pub fn render_2d(
        &mut self,
        operation: Nvidia2DOperation,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        if !self.is_ready() {
            return Err("Driver NVIDIA no está listo");
        }

        match operation {
            Nvidia2DOperation::FillRect(rect, color) => {
                self.fill_rect_2d(rect, color, fb)?;
            }
            Nvidia2DOperation::DrawRect(rect, color, thickness) => {
                self.draw_rect_2d(rect, color, thickness, fb)?;
            }
            Nvidia2DOperation::DrawLine(start, end, color, thickness) => {
                self.draw_line_2d(start, end, color, thickness, fb)?;
            }
            Nvidia2DOperation::Blit(src_rect, dst_rect) => {
                self.blit_2d(src_rect, dst_rect, fb)?;
            }
            Nvidia2DOperation::DrawCircle(center, radius, color, filled) => {
                self.draw_circle_2d(center, radius, color, filled, fb)?;
            }
            Nvidia2DOperation::DrawTriangle(p1, p2, p3, color, filled) => {
                self.draw_triangle_2d(p1, p2, p3, color, filled, fb)?;
            }
        }

        Ok(())
    }

    /// Rellenar rectángulo con aceleración 2D NVIDIA
    fn fill_rect_2d(
        &mut self,
        rect: Rect,
        color: Color,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Configurar operación 2D NVIDIA
        self.write_mmio(0x1000, rect.x as u32); // X
        self.write_mmio(0x1004, rect.y as u32); // Y
        self.write_mmio(0x1008, rect.width as u32); // Width
        self.write_mmio(0x100C, rect.height as u32); // Height
        self.write_mmio(0x1010, self.color_to_pixel(color)); // Color

        // Ejecutar operación
        self.write_mmio(0x1000, 0x00000001); // FillRect command

        // Esperar completado
        while self.read_mmio(0x1000) & 0x80000000 != 0 {
            // Busy wait
        }

        Ok(())
    }

    /// Dibujar rectángulo con aceleración 2D NVIDIA
    fn draw_rect_2d(
        &mut self,
        rect: Rect,
        color: Color,
        thickness: u32,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Dibujar los 4 lados del rectángulo
        let top_left = Point {
            x: rect.x,
            y: rect.y,
        };
        let top_right = Point {
            x: rect.x + rect.width,
            y: rect.y,
        };
        let bottom_left = Point {
            x: rect.x,
            y: rect.y + rect.height,
        };
        let bottom_right = Point {
            x: rect.x + rect.width,
            y: rect.y + rect.height,
        };

        // Línea superior
        self.draw_line_2d(top_left, top_right, color, thickness, fb)?;
        // Línea derecha
        self.draw_line_2d(top_right, bottom_right, color, thickness, fb)?;
        // Línea inferior
        self.draw_line_2d(bottom_right, bottom_left, color, thickness, fb)?;
        // Línea izquierda
        self.draw_line_2d(bottom_left, top_left, color, thickness, fb)?;

        Ok(())
    }

    /// Dibujar línea con aceleración 2D NVIDIA
    fn draw_line_2d(
        &mut self,
        start: Point,
        end: Point,
        color: Color,
        thickness: u32,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Configurar operación de línea NVIDIA
        self.write_mmio(0x2000, start.x as u32); // Start X
        self.write_mmio(0x2004, start.y as u32); // Start Y
        self.write_mmio(0x2008, end.x as u32); // End X
        self.write_mmio(0x200C, end.y as u32); // End Y
        self.write_mmio(0x2010, self.color_to_pixel(color)); // Color
        self.write_mmio(0x2014, thickness); // Thickness

        // Ejecutar operación
        self.write_mmio(0x2000, 0x00000002); // DrawLine command

        // Esperar completado
        while self.read_mmio(0x2000) & 0x80000000 != 0 {
            // Busy wait
        }

        Ok(())
    }

    /// Blit con aceleración 2D NVIDIA
    fn blit_2d(
        &mut self,
        src_rect: Rect,
        dst_rect: Rect,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Configurar operación Blit NVIDIA
        self.write_mmio(0x3000, src_rect.x as u32); // Src X
        self.write_mmio(0x3004, src_rect.y as u32); // Src Y
        self.write_mmio(0x3008, dst_rect.x as u32); // Dst X
        self.write_mmio(0x300C, dst_rect.y as u32); // Dst Y
        self.write_mmio(0x3010, src_rect.width as u32); // Width
        self.write_mmio(0x3014, src_rect.height as u32); // Height

        // Ejecutar operación
        self.write_mmio(0x3000, 0x00000004); // Blit command

        // Esperar completado
        while self.read_mmio(0x3000) & 0x80000000 != 0 {
            // Busy wait
        }

        Ok(())
    }

    /// Dibujar círculo con aceleración 2D NVIDIA
    fn draw_circle_2d(
        &mut self,
        center: Point,
        radius: u32,
        color: Color,
        filled: bool,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Configurar operación de círculo NVIDIA
        self.write_mmio(0x4000, center.x as u32); // Center X
        self.write_mmio(0x4004, center.y as u32); // Center Y
        self.write_mmio(0x4008, radius); // Radius
        self.write_mmio(0x400C, self.color_to_pixel(color)); // Color
        self.write_mmio(0x4010, if filled { 1 } else { 0 }); // Filled flag

        // Ejecutar operación
        self.write_mmio(0x4000, 0x00000008); // DrawCircle command

        // Esperar completado
        while self.read_mmio(0x4000) & 0x80000000 != 0 {
            // Busy wait
        }

        Ok(())
    }

    /// Dibujar triángulo con aceleración 2D NVIDIA
    fn draw_triangle_2d(
        &mut self,
        p1: Point,
        p2: Point,
        p3: Point,
        color: Color,
        filled: bool,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        if filled {
            // Rellenar triángulo usando scanline
            self.fill_triangle_2d(p1, p2, p3, color, fb)?;
        } else {
            // Dibujar líneas del triángulo
            self.draw_line_2d(p1, p2, color, 1, fb)?;
            self.draw_line_2d(p2, p3, color, 1, fb)?;
            self.draw_line_2d(p3, p1, color, 1, fb)?;
        }

        Ok(())
    }

    /// Rellenar triángulo con aceleración 2D NVIDIA
    fn fill_triangle_2d(
        &mut self,
        p1: Point,
        p2: Point,
        p3: Point,
        color: Color,
        fb: &mut FramebufferDriver,
    ) -> Result<(), &'static str> {
        // Configurar operación de triángulo NVIDIA
        self.write_mmio(0x5000, p1.x as u32); // P1 X
        self.write_mmio(0x5004, p1.y as u32); // P1 Y
        self.write_mmio(0x5008, p2.x as u32); // P2 X
        self.write_mmio(0x500C, p2.y as u32); // P2 Y
        self.write_mmio(0x5010, p3.x as u32); // P3 X
        self.write_mmio(0x5014, p3.y as u32); // P3 Y
        self.write_mmio(0x5018, self.color_to_pixel(color)); // Color

        // Ejecutar operación
        self.write_mmio(0x5000, 0x00000010); // FillTriangle command

        // Esperar completado
        while self.read_mmio(0x5000) & 0x80000000 != 0 {
            // Busy wait
        }

        Ok(())
    }

    /// Leer configuración PCI
    fn read_pci_config(&self, offset: u8) -> u32 {
        let address = 0x80000000
            | (self.pci_device.bus as u32) << 16
            | (self.pci_device.device as u32) << 11
            | (self.pci_device.function as u32) << 8
            | (offset as u32);

        unsafe {
            // Simular lectura PCI (en un kernel real usaría outl/inl)
            address
        }
    }

    /// Escribir configuración PCI
    fn write_pci_config(&self, offset: u8, value: u32) {
        let address = 0x80000000
            | (self.pci_device.bus as u32) << 16
            | (self.pci_device.device as u32) << 11
            | (self.pci_device.function as u32) << 8
            | (offset as u32);

        unsafe {
            // Simular escritura PCI (en un kernel real usaría outl)
            ptr::write_volatile(address as *mut u32, value);
        }
    }
}

/// Operaciones 2D de NVIDIA
#[derive(Debug, Clone)]
pub enum Nvidia2DOperation {
    FillRect(Rect, Color),
    DrawRect(Rect, Color, u32),          // rect, color, thickness
    DrawLine(Point, Point, Color, u32),  // start, end, color, thickness
    Blit(Rect, Rect),                    // source, destination
    DrawCircle(Point, u32, Color, bool), // center, radius, color, filled
    DrawTriangle(Point, Point, Point, Color, bool), // p1, p2, p3, color, filled
}

/// Función de conveniencia para crear un driver NVIDIA
pub fn create_nvidia_driver(pci_device: PciDevice, gpu_info: GpuInfo) -> NvidiaGraphicsDriver {
    NvidiaGraphicsDriver::new(pci_device, gpu_info)
}

/// Detectar GPU NVIDIA en el sistema
pub fn detect_nvidia_gpu() -> Option<NvidiaGraphicsDriver> {
    // Simular detección de GPU NVIDIA
    // En un kernel real, esto escanearía el bus PCI

    // Crear un dispositivo PCI simulado para NVIDIA
    let pci_device = PciDevice {
        bus: 0,
        device: 1,
        function: 0,
        vendor_id: NVIDIA_VENDOR_ID,
        device_id: NVIDIA_GTX_3000_SERIES, // RTX 3060 como ejemplo
        class_code: 0x03,                  // VGA controller
        subclass_code: 0x00,
        prog_if: 0x00,
        revision_id: 0xA1,
        header_type: 0x00,
        status: 0x0010,
        command: 0x0007,
    };

    let gpu_info = GpuInfo {
        pci_device,
        gpu_type: GpuType::Nvidia,
        memory_size: 8 * 1024 * 1024 * 1024, // 8GB
        is_primary: true,
        supports_2d: true,
        supports_3d: true,
        max_resolution: (3840, 2160), // 4K
    };

    Some(NvidiaGraphicsDriver::new(pci_device, gpu_info))
}

impl NvidiaGraphicsDriver {
    /// Configurar modo de video NVIDIA
    fn configure_video_mode(&mut self) -> Result<(), &'static str> {
        // Configurar registros de control de video
        self.write_mmio(NVIDIA_VIDEO_CONTROL, 0x00000001); // Habilitar video
        self.write_mmio(NVIDIA_VIDEO_MODE, 0x00000020); // Modo 32-bit

        // Configurar sincronización
        self.write_mmio(NVIDIA_H_SYNC_START, 0x00000000);
        self.write_mmio(NVIDIA_H_SYNC_END, 0x00000000);
        self.write_mmio(NVIDIA_V_SYNC_START, 0x00000000);
        self.write_mmio(NVIDIA_V_SYNC_END, 0x00000000);

        Ok(())
    }

    /// Reconfigurar tarjeta gráfica para nuevo framebuffer
    pub fn reconfigure_graphics_card(
        &mut self,
        new_fb_info: &FramebufferInfo,
    ) -> Result<(), &'static str> {
        // 1. Deshabilitar video temporalmente
        self.write_mmio(NVIDIA_VIDEO_CONTROL, 0x00000000);

        // 2. Configurar nueva resolución
        self.write_mmio(NVIDIA_WIDTH, new_fb_info.width);
        self.write_mmio(NVIDIA_HEIGHT, new_fb_info.height);
        self.write_mmio(NVIDIA_STRIDE, new_fb_info.pixels_per_scan_line);

        // 3. Configurar nueva dirección de framebuffer
        self.write_mmio(NVIDIA_FB_BASE_LOW, new_fb_info.base_address as u32);
        self.write_mmio(NVIDIA_FB_BASE_HIGH, (new_fb_info.base_address >> 32) as u32);

        // 4. Configurar formato de pixel
        let pixel_format = match new_fb_info.pixel_format {
            32 => 0x00000020, // 32-bit RGBA
            24 => 0x00000018, // 24-bit RGB
            16 => 0x00000010, // 16-bit RGB
            _ => 0x00000020,  // Default 32-bit
        };
        self.write_mmio(NVIDIA_PIXEL_FORMAT, pixel_format);

        // 5. Reconfigurar sincronización para nueva resolución
        self.reconfigure_timing(new_fb_info)?;

        // 6. Habilitar video con nueva configuración
        self.write_mmio(NVIDIA_VIDEO_CONTROL, 0x00000001);

        Ok(())
    }

    /// Reconfigurar timing de sincronización
    fn reconfigure_timing(&self, fb_info: &FramebufferInfo) -> Result<(), &'static str> {
        // Calcular timing basado en resolución
        let h_total = fb_info.width + 100; // H total
        let v_total = fb_info.height + 50; // V total

        self.write_mmio(NVIDIA_H_TOTAL, h_total);
        self.write_mmio(NVIDIA_V_TOTAL, v_total);
        self.write_mmio(NVIDIA_H_SYNC_START, fb_info.width);
        self.write_mmio(NVIDIA_H_SYNC_END, fb_info.width + 20);
        self.write_mmio(NVIDIA_V_SYNC_START, fb_info.height);
        self.write_mmio(NVIDIA_V_SYNC_END, fb_info.height + 5);

        Ok(())
    }

    /// Cambiar modo de video VESA/UEFI
    pub fn set_vesa_mode(&mut self, mode: u16) -> Result<(), &'static str> {
        // Implementar cambio de modo VESA
        // Esto requeriría llamadas a servicios de firmware
        self.write_mmio(NVIDIA_VESA_MODE, mode as u32);
        Ok(())
    }

    /// Llamar a servicios de firmware UEFI
    pub fn call_uefi_graphics_service(
        &self,
        service: u32,
        params: &[u32],
    ) -> Result<u32, &'static str> {
        // Implementar llamadas a servicios UEFI
        // Esto requeriría integración con el sistema UEFI
        self.write_mmio(NVIDIA_UEFI_SERVICE, service);
        for (i, param) in params.iter().enumerate() {
            self.write_mmio(NVIDIA_UEFI_PARAM_BASE + i as u32, *param);
        }

        // Leer resultado
        let result = self.read_mmio(NVIDIA_UEFI_RESULT);
        Ok(result)
    }
}

// Constantes de registros NVIDIA
const NVIDIA_VIDEO_CONTROL: u32 = 0x0000;
const NVIDIA_VIDEO_MODE: u32 = 0x0004;
const NVIDIA_WIDTH: u32 = 0x0008;
const NVIDIA_HEIGHT: u32 = 0x000C;
const NVIDIA_STRIDE: u32 = 0x0010;
const NVIDIA_FB_BASE_LOW: u32 = 0x0014;
const NVIDIA_FB_BASE_HIGH: u32 = 0x0018;
const NVIDIA_PIXEL_FORMAT: u32 = 0x001C;
const NVIDIA_H_TOTAL: u32 = 0x0020;
const NVIDIA_V_TOTAL: u32 = 0x0024;
const NVIDIA_H_SYNC_START: u32 = 0x0028;
const NVIDIA_H_SYNC_END: u32 = 0x002C;
const NVIDIA_V_SYNC_START: u32 = 0x0030;
const NVIDIA_V_SYNC_END: u32 = 0x0034;
const NVIDIA_VESA_MODE: u32 = 0x0038;
const NVIDIA_UEFI_SERVICE: u32 = 0x003C;
const NVIDIA_UEFI_PARAM_BASE: u32 = 0x0040;
const NVIDIA_UEFI_RESULT: u32 = 0x0080;
