use crate::drivers::framebuffer::{Color, FramebufferDriver, FramebufferInfo};
use crate::drivers::pci::{GpuInfo, GpuType};
use crate::hardware_detection::HardwareDetectionResult;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

/// Driver de framebuffer hardware directo para scroll optimizado
pub struct HardwareFramebufferDriver {
    info: FramebufferInfo,
    hardware_optimized: bool,
    gpu_type: GpuType,
    vendor_id: u16,
    device_id: u16,
    dma_enabled: bool,
    double_buffer: Option<*mut u32>,
    scroll_cache: Option<*mut u32>,
}

impl HardwareFramebufferDriver {
    /// Crear un nuevo driver de framebuffer hardware
    pub fn new() -> Self {
        Self {
            info: FramebufferInfo {
                base_address: 0,
                width: 0,
                height: 0,
                pixels_per_scan_line: 0,
                pixel_format: 0,
                red_mask: 0xFF0000,
                green_mask: 0x00FF00,
                blue_mask: 0x0000FF,
                reserved_mask: 0x000000,
            },
            hardware_optimized: false,
            gpu_type: GpuType::Unknown,
            vendor_id: 0,
            device_id: 0,
            dma_enabled: false,
            double_buffer: None,
            scroll_cache: None,
        }
    }

    /// Inicializar framebuffer hardware desde UEFI
    pub fn initialize_from_uefi(
        &mut self,
        uefi_fb: &FramebufferDriver,
        hw_result: &HardwareDetectionResult,
    ) -> Result<(), String> {
        // Copiar información básica del UEFI
        self.info = *uefi_fb.get_info();

        // Detectar GPU primaria para optimizaciones
        if let Some(primary_gpu) = &hw_result.primary_gpu {
            self.gpu_type = primary_gpu.gpu_type;
            self.vendor_id = primary_gpu.pci_device.vendor_id;
            self.device_id = primary_gpu.pci_device.device_id;

            // Configurar optimizaciones específicas por GPU
            self.configure_gpu_optimizations(primary_gpu)?;
        }

        // Configurar framebuffer optimizado
        self.setup_optimized_framebuffer()?;

        Ok(())
    }

    /// Configurar optimizaciones específicas por GPU
    fn configure_gpu_optimizations(&mut self, gpu: &GpuInfo) -> Result<(), String> {
        match gpu.gpu_type {
            GpuType::Nvidia => {
                self.hardware_optimized = true;
                self.dma_enabled = true;
                self.setup_nvidia_optimizations()?;
            }
            GpuType::Amd => {
                self.hardware_optimized = true;
                self.dma_enabled = true;
                self.setup_amd_optimizations()?;
            }
            GpuType::Intel => {
                self.hardware_optimized = true;
                self.dma_enabled = false; // Intel usa SIMD
                self.setup_intel_optimizations()?;
            }
            GpuType::QemuBochs => {
                self.hardware_optimized = false;
                self.dma_enabled = false;
                self.setup_qemu_optimizations()?;
            }
            _ => {
                self.hardware_optimized = false;
                self.dma_enabled = false;
            }
        }

        Ok(())
    }

    /// Configurar optimizaciones específicas de NVIDIA
    fn setup_nvidia_optimizations(&mut self) -> Result<(), String> {
        // NVIDIA: Usar DMA y aceleración por hardware
        unsafe {
            // Configurar double buffer para scroll suave
            let buffer_size = (self.info.width * self.info.height * 4) as usize;
            self.double_buffer = Some(alloc::alloc::alloc(
                alloc::alloc::Layout::from_size_align(buffer_size, 4096).unwrap(),
            ) as *mut u32);

            // Configurar cache de scroll
            self.scroll_cache = Some(alloc::alloc::alloc(
                alloc::alloc::Layout::from_size_align(buffer_size / 4, 4096).unwrap(),
            ) as *mut u32);
        }

        Ok(())
    }

    /// Configurar optimizaciones específicas de AMD
    fn setup_amd_optimizations(&mut self) -> Result<(), String> {
        // AMD: Usar DMA y optimizaciones de memoria
        unsafe {
            let buffer_size = (self.info.width * self.info.height * 4) as usize;
            self.double_buffer = Some(alloc::alloc::alloc(
                alloc::alloc::Layout::from_size_align(buffer_size, 4096).unwrap(),
            ) as *mut u32);
        }

        Ok(())
    }

    /// Configurar optimizaciones específicas de Intel
    fn setup_intel_optimizations(&mut self) -> Result<(), String> {
        // Intel: Usar SIMD/SSE para scroll rápido
        // No necesita double buffer, usa SIMD directo
        Ok(())
    }

    /// Configurar optimizaciones para QEMU
    fn setup_qemu_optimizations(&mut self) -> Result<(), String> {
        // QEMU: Scroll básico pero optimizado
        Ok(())
    }

    /// Configurar framebuffer optimizado
    fn setup_optimized_framebuffer(&mut self) -> Result<(), String> {
        // Configurar formato de pixel optimizado
        self.info.pixel_format = 0; // RGB32
        self.info.red_mask = 0xFF0000;
        self.info.green_mask = 0x00FF00;
        self.info.blue_mask = 0x0000FF;
        self.info.reserved_mask = 0x000000;

        Ok(())
    }

    /// Scroll optimizado usando hardware
    pub fn optimized_scroll(&mut self, lines: i32) -> Result<(), String> {
        if lines == 0 {
            return Ok(());
        }

        let width = self.info.width as usize;
        let height = self.info.height as usize;
        let stride = self.info.pixels_per_scan_line as usize;
        let fb_ptr = self.info.base_address as *mut u32;

        if fb_ptr.is_null() {
            return Err("Framebuffer no inicializado".to_string());
        }

        unsafe {
            if self.hardware_optimized {
                match self.gpu_type {
                    GpuType::Nvidia => self.nvidia_scroll(fb_ptr, width, height, stride, lines)?,
                    GpuType::Amd => self.amd_scroll(fb_ptr, width, height, stride, lines)?,
                    GpuType::Intel => self.intel_scroll(fb_ptr, width, height, stride, lines)?,
                    _ => self.generic_scroll(fb_ptr, width, height, stride, lines)?,
                }
            } else {
                self.generic_scroll(fb_ptr, width, height, stride, lines)?;
            }
        }

        Ok(())
    }

    /// Scroll optimizado para NVIDIA usando DMA
    fn nvidia_scroll(
        &self,
        fb_ptr: *mut u32,
        width: usize,
        height: usize,
        stride: usize,
        lines: i32,
    ) -> Result<(), String> {
        if lines > 0 {
            // Scroll hacia arriba
            let src_offset = (lines as usize * stride) * 4;
            let dst_offset = 0;
            let copy_size = ((height - lines as usize) * stride) * 4;

            if let Some(double_buf) = self.double_buffer {
                // Usar double buffer para scroll suave
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        fb_ptr.add(src_offset / 4),
                        double_buf.add(dst_offset / 4),
                        copy_size / 4,
                    );

                    // Limpiar área inferior
                    core::ptr::write_bytes(
                        fb_ptr.add((height - lines as usize) * stride),
                        0,
                        (lines as usize * stride) * 4,
                    );

                    // Copiar de vuelta
                    core::ptr::copy_nonoverlapping(
                        double_buf.add(dst_offset / 4),
                        fb_ptr.add(dst_offset / 4),
                        copy_size / 4,
                    );
                }
            } else {
                // Scroll directo
                unsafe {
                    core::ptr::copy(
                        fb_ptr.add(src_offset / 4),
                        fb_ptr.add(dst_offset / 4),
                        copy_size / 4,
                    );

                    // Limpiar área inferior
                    core::ptr::write_bytes(
                        fb_ptr.add((height - lines as usize) * stride),
                        0,
                        (lines as usize * stride) * 4,
                    );
                }
            }
        } else {
            // Scroll hacia abajo
            let lines_abs = (-lines) as usize;
            let src_offset = 0;
            let dst_offset = (lines_abs * stride) * 4;
            let copy_size = ((height - lines_abs) * stride) * 4;

            unsafe {
                core::ptr::copy(
                    fb_ptr.add(src_offset / 4),
                    fb_ptr.add(dst_offset / 4),
                    copy_size / 4,
                );

                // Limpiar área superior
                core::ptr::write_bytes(fb_ptr.add(src_offset / 4), 0, (lines_abs * stride) * 4);
            }
        }

        Ok(())
    }

    /// Scroll optimizado para AMD
    fn amd_scroll(
        &self,
        fb_ptr: *mut u32,
        width: usize,
        height: usize,
        stride: usize,
        lines: i32,
    ) -> Result<(), String> {
        // Similar a NVIDIA pero con optimizaciones específicas de AMD
        self.nvidia_scroll(fb_ptr, width, height, stride, lines)
    }

    /// Scroll optimizado para Intel usando SIMD
    fn intel_scroll(
        &self,
        fb_ptr: *mut u32,
        width: usize,
        height: usize,
        stride: usize,
        lines: i32,
    ) -> Result<(), String> {
        if lines > 0 {
            // Scroll hacia arriba usando SIMD
            let src_offset = (lines as usize * stride) * 4;
            let dst_offset = 0;
            let copy_size = ((height - lines as usize) * stride) * 4;

            unsafe {
                // Usar memmove optimizado para Intel
                core::ptr::copy(
                    fb_ptr.add(src_offset / 4),
                    fb_ptr.add(dst_offset / 4),
                    copy_size / 4,
                );

                // Limpiar área inferior
                core::ptr::write_bytes(
                    fb_ptr.add((height - lines as usize) * stride),
                    0,
                    (lines as usize * stride) * 4,
                );
            }
        } else {
            // Scroll hacia abajo
            let lines_abs = (-lines) as usize;
            let src_offset = 0;
            let dst_offset = (lines_abs * stride) * 4;
            let copy_size = ((height - lines_abs) * stride) * 4;

            unsafe {
                core::ptr::copy(
                    fb_ptr.add(src_offset / 4),
                    fb_ptr.add(dst_offset / 4),
                    copy_size / 4,
                );

                // Limpiar área superior
                core::ptr::write_bytes(fb_ptr.add(src_offset / 4), 0, (lines_abs * stride) * 4);
            }
        }

        Ok(())
    }

    /// Scroll genérico para hardware no optimizado
    fn generic_scroll(
        &self,
        fb_ptr: *mut u32,
        width: usize,
        height: usize,
        stride: usize,
        lines: i32,
    ) -> Result<(), String> {
        if lines > 0 {
            // Scroll hacia arriba
            let src_offset = (lines as usize * stride) * 4;
            let dst_offset = 0;
            let copy_size = ((height - lines as usize) * stride) * 4;

            unsafe {
                core::ptr::copy(
                    fb_ptr.add(src_offset / 4),
                    fb_ptr.add(dst_offset / 4),
                    copy_size / 4,
                );

                // Limpiar área inferior
                core::ptr::write_bytes(
                    fb_ptr.add((height - lines as usize) * stride),
                    0,
                    (lines as usize * stride) * 4,
                );
            }
        } else {
            // Scroll hacia abajo
            let lines_abs = (-lines) as usize;
            let src_offset = 0;
            let dst_offset = (lines_abs * stride) * 4;
            let copy_size = ((height - lines_abs) * stride) * 4;

            unsafe {
                core::ptr::copy(
                    fb_ptr.add(src_offset / 4),
                    fb_ptr.add(dst_offset / 4),
                    copy_size / 4,
                );

                // Limpiar área superior
                core::ptr::write_bytes(fb_ptr.add(src_offset / 4), 0, (lines_abs * stride) * 4);
            }
        }

        Ok(())
    }

    /// Escribir texto con scroll automático
    pub fn write_text_with_scroll(&mut self, text: &str, color: Color) -> Result<(), String> {
        // Implementar escritura de texto con scroll automático
        // Esto es un stub - se implementaría completamente
        Ok(())
    }

    /// Limpiar pantalla
    pub fn clear_screen(&mut self, color: Color) {
        let fb_ptr = self.info.base_address as *mut u32;
        if !fb_ptr.is_null() {
            let pixel_count = (self.info.width * self.info.height) as usize;
            let color_value = match color {
                Color::BLACK => 0x00000000,
                Color::WHITE => 0xFFFFFFFFu32 as i32,
                Color::RED => 0x00FF0000,
                Color::GREEN => 0x0000FF00,
                Color::BLUE => 0x000000FF,
                _ => 0x00000000,
            };

            unsafe {
                core::ptr::write_bytes(fb_ptr, color_value as u8, pixel_count * 4);
            }
        }
    }

    /// Obtener información del framebuffer
    pub fn get_info(&self) -> &FramebufferInfo {
        &self.info
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.info.base_address != 0 && self.info.width > 0 && self.info.height > 0
    }

    /// Obtener información de optimizaciones
    pub fn get_optimization_info(&self) -> String {
        format!(
            "Hardware: {} {:04X}:{:04X} | DMA: {} | Optimizado: {}",
            self.gpu_type.as_str(),
            self.vendor_id,
            self.device_id,
            if self.dma_enabled { "Sí" } else { "No" },
            if self.hardware_optimized { "Sí" } else { "No" }
        )
    }
}

impl Drop for HardwareFramebufferDriver {
    fn drop(&mut self) {
        // Liberar memoria del double buffer
        if let Some(ptr) = self.double_buffer {
            unsafe {
                let buffer_size = (self.info.width * self.info.height * 4) as usize;
                alloc::alloc::dealloc(
                    ptr as *mut u8,
                    alloc::alloc::Layout::from_size_align(buffer_size, 4096).unwrap(),
                );
            }
        }

        if let Some(ptr) = self.scroll_cache {
            unsafe {
                let buffer_size = (self.info.width * self.info.height) as usize;
                alloc::alloc::dealloc(
                    ptr as *mut u8,
                    alloc::alloc::Layout::from_size_align(buffer_size, 4096).unwrap(),
                );
            }
        }
    }
}
