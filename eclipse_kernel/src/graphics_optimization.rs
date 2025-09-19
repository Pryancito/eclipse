//! Optimizaciones de gráficos para hardware real
//! 
//! Este módulo implementa optimizaciones específicas para mejorar el rendimiento
//! gráfico en hardware real, especialmente para operaciones de scroll.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::window_system::geometry::{Point, Size, Rectangle};
use alloc::vec::Vec;
use core::arch::asm;

/// Configuración de optimización gráfica
#[derive(Debug, Clone)]
pub struct GraphicsOptimizationConfig {
    pub enable_dma: bool,
    pub enable_double_buffering: bool,
    pub enable_region_scroll: bool,
    pub enable_memory_optimization: bool,
    pub scroll_region_size: u32,
    pub double_buffer_size: u32,
}

impl Default for GraphicsOptimizationConfig {
    fn default() -> Self {
        Self {
            enable_dma: true,
            enable_double_buffering: true,
            enable_region_scroll: true,
            enable_memory_optimization: true,
            scroll_region_size: 64, // Scroll en bloques de 64 píxeles
            double_buffer_size: 1024 * 1024, // 1MB de buffer doble
        }
    }
}

/// Optimizador de gráficos
pub struct GraphicsOptimizer {
    config: GraphicsOptimizationConfig,
    double_buffer: Option<Vec<u32>>,
    scroll_regions: Vec<ScrollRegion>,
    last_scroll_position: Point,
    dma_enabled: bool,
}

/// Región de scroll optimizada
#[derive(Debug, Clone)]
struct ScrollRegion {
    rect: Rectangle,
    dirty: bool,
    last_update: u64,
}

impl GraphicsOptimizer {
    pub fn new(config: GraphicsOptimizationConfig) -> Self {
        Self {
            double_buffer: if config.enable_double_buffering {
                Some(Vec::with_capacity(config.double_buffer_size as usize))
            } else {
                None
            },
            scroll_regions: Vec::new(),
            last_scroll_position: Point::new(0, 0),
            dma_enabled: false,
            config,
        }
    }

    /// Inicializar el optimizador
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Inicializar DMA si está disponible
        if self.config.enable_dma {
            self.initialize_dma()?;
        }

        // Inicializar regiones de scroll
        if self.config.enable_region_scroll {
            self.initialize_scroll_regions()?;
        }

        Ok(())
    }

    /// Inicializar DMA para transferencias de memoria
    fn initialize_dma(&mut self) -> Result<(), &'static str> {
        // En una implementación real, esto configuraría el controlador DMA
        // Por ahora simulamos la inicialización
        self.dma_enabled = true;
        Ok(())
    }

    /// Inicializar regiones de scroll
    fn initialize_scroll_regions(&mut self) -> Result<(), &'static str> {
        self.scroll_regions.clear();
        
        // Crear regiones de scroll más grandes para evitar problemas de rendimiento
        let region_size = core::cmp::max(self.config.scroll_region_size, 128);
        
        // Dividir la pantalla en regiones de scroll más grandes
        for y in (0..768).step_by(region_size as usize) {
            for x in (0..1024).step_by(region_size as usize) {
                let width = core::cmp::min(region_size, 1024 - x);
                let height = core::cmp::min(region_size, 768 - y);
                
                // Solo crear regiones si tienen un tamaño mínimo
                if width >= 64 && height >= 64 {
                    self.scroll_regions.push(ScrollRegion {
                        rect: Rectangle::new(x as i32, y as i32, width, height),
                        dirty: false,
                        last_update: 0,
                    });
                }
            }
        }
        
        Ok(())
    }

    /// Scroll optimizado con aceleración por hardware
    pub fn optimized_scroll(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        scroll_delta: Point,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if scroll_delta.x == 0 && scroll_delta.y == 0 {
            return Ok(());
        }

        // Usar scroll por hardware si está disponible
        if self.config.enable_memory_optimization {
            self.hardware_scroll(framebuffer, scroll_delta, scroll_rect)?;
        } else if self.config.enable_region_scroll {
            self.scroll_by_regions(framebuffer, scroll_delta, scroll_rect)?;
        } else {
            self.scroll_full_screen(framebuffer, scroll_delta, scroll_rect)?;
        }

        self.last_scroll_position = Point::new(
            self.last_scroll_position.x + scroll_delta.x,
            self.last_scroll_position.y + scroll_delta.y,
        );

        Ok(())
    }

    /// Scroll optimizado usando métodos compatibles
    fn hardware_scroll(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        scroll_delta: Point,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if scroll_delta.y == 0 && scroll_delta.x == 0 {
            return Ok(());
        }

        // Usar scroll optimizado por hardware para mejor rendimiento
        if scroll_delta.y != 0 {
            self.optimized_scroll_vertical(framebuffer, scroll_delta.y, scroll_rect)?;
        }
        
        if scroll_delta.x != 0 {
            self.optimized_scroll_horizontal(framebuffer, scroll_delta.x, scroll_rect)?;
        }

        Ok(())
    }

    /// Scroll vertical por hardware usando instrucciones SIMD
    fn hardware_scroll_vertical(
        &self,
        fb_ptr: *mut u32,
        fb_width: u32,
        fb_height: u32,
        stride: u32,
        delta_y: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_y == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(fb_height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(fb_width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        if delta_y > 0 {
            // Scroll hacia abajo - mover líneas hacia arriba
            for y in (start_y..end_y - delta_y as u32).rev() {
                let src_y = y + delta_y as u32;
                if src_y < end_y {
                    self.hardware_copy_line(
                        fb_ptr, stride,
                        start_x, src_y,
                        start_x, y,
                        end_x - start_x
                    )?;
                }
            }
            
            // Limpiar líneas inferiores
            for y in (end_y - delta_y as u32)..end_y {
                self.hardware_clear_line(fb_ptr, stride, start_x, y, end_x - start_x)?;
            }
        } else {
            // Scroll hacia arriba - mover líneas hacia abajo
            let abs_delta = (-delta_y) as u32;
            for y in (start_y + abs_delta)..end_y {
                let src_y = y - abs_delta;
                self.hardware_copy_line(
                    fb_ptr, stride,
                    start_x, src_y,
                    start_x, y,
                    end_x - start_x
                )?;
            }
            
            // Limpiar líneas superiores
            for y in start_y..(start_y + abs_delta) {
                self.hardware_clear_line(fb_ptr, stride, start_x, y, end_x - start_x)?;
            }
        }

        Ok(())
    }

    /// Scroll horizontal por hardware usando instrucciones SIMD
    fn hardware_scroll_horizontal(
        &self,
        fb_ptr: *mut u32,
        fb_width: u32,
        fb_height: u32,
        stride: u32,
        delta_x: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_x == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(fb_height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(fb_width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        if delta_x > 0 {
            // Scroll hacia la derecha - usar copia de líneas optimizada
            let delta = delta_x as u32;
            for y in start_y..end_y {
                // Copiar línea completa con offset
                self.hardware_copy_line(
                    fb_ptr, stride,
                    start_x + delta, y,
                    start_x, y,
                    end_x - start_x - delta
                )?;
                
                // Limpiar píxeles de la derecha
                self.hardware_clear_line(
                    fb_ptr, stride,
                    end_x - delta, y,
                    delta
                )?;
            }
        } else {
            // Scroll hacia la izquierda - usar copia de líneas optimizada
            let abs_delta = (-delta_x) as u32;
            for y in start_y..end_y {
                // Copiar línea completa con offset
                self.hardware_copy_line(
                    fb_ptr, stride,
                    start_x, y,
                    start_x + abs_delta, y,
                    end_x - start_x - abs_delta
                )?;
                
                // Limpiar píxeles de la izquierda
                self.hardware_clear_line(
                    fb_ptr, stride,
                    start_x, y,
                    abs_delta
                )?;
            }
        }

        Ok(())
    }

    /// Copia de línea usando instrucciones SIMD optimizadas
    fn hardware_copy_line(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        src_x: u32,
        src_y: u32,
        dst_x: u32,
        dst_y: u32,
        width: u32,
    ) -> Result<(), &'static str> {
        if width == 0 {
            return Ok(());
        }

        unsafe {
            let src_offset = (src_y * stride + src_x) as usize;
            let dst_offset = (dst_y * stride + dst_x) as usize;
            
            // Usar instrucciones SIMD para copia rápida
            let mut remaining = width as usize;
            let mut src_idx = 0;
            let mut dst_idx = 0;
            
            // Copiar en bloques de 8 píxeles (256 bits) usando AVX2
            while remaining >= 8 {
                asm!(
                    "vmovdqu ymm0, [{src_ptr}]",
                    "vmovdqu [{dst_ptr}], ymm0",
                    src_ptr = in(reg) (fb_ptr as *const u32).add(src_idx),
                    dst_ptr = in(reg) (fb_ptr as *mut u32).add(dst_idx),
                    out("ymm0") _,
                    options(nostack, preserves_flags)
                );
                
                src_idx += 8;
                dst_idx += 8;
                remaining -= 8;
            }
            
            // Copiar píxeles restantes
            while remaining > 0 {
                *fb_ptr.add(dst_idx) = *fb_ptr.add(src_idx);
                src_idx += 1;
                dst_idx += 1;
                remaining -= 1;
            }
        }

        Ok(())
    }

    /// Limpia una línea completa
    fn hardware_clear_line(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        x: u32,
        y: u32,
        width: u32,
    ) -> Result<(), &'static str> {
        if width == 0 {
            return Ok(());
        }

        unsafe {
            let offset = (y * stride + x) as usize;
            
            // Limpiar en bloques de 8 píxeles usando AVX2
            let mut remaining = width as usize;
            let mut idx = 0;
            
            while remaining >= 8 {
                asm!(
                    "vpxor ymm0, ymm0, ymm0",
                    "vmovdqu [{ptr}], ymm0",
                    ptr = in(reg) (fb_ptr as *mut u32).add(idx),
                    out("ymm0") _,
                    options(nostack, preserves_flags)
                );
                
                idx += 8;
                remaining -= 8;
            }
            
            // Limpiar píxeles restantes
            while remaining > 0 {
                *fb_ptr.add(idx) = 0;
                idx += 1;
                remaining -= 1;
            }
        }

        Ok(())
    }

    /// Copia un píxel individual
    fn hardware_copy_pixel(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        src_x: u32,
        src_y: u32,
        dst_x: u32,
        dst_y: u32,
    ) -> Result<(), &'static str> {
        unsafe {
            let src_offset = (src_y * stride + src_x) as usize;
            let dst_offset = (dst_y * stride + dst_x) as usize;
            *fb_ptr.add(dst_offset) = *fb_ptr.add(src_offset);
        }
        Ok(())
    }

    /// Limpia un píxel individual
    fn hardware_clear_pixel(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        x: u32,
        y: u32,
    ) -> Result<(), &'static str> {
        unsafe {
            let offset = (y * stride + x) as usize;
            *fb_ptr.add(offset) = 0;
        }
        Ok(())
    }

    /// Scroll vertical optimizado usando técnicas de Linux
    fn optimized_scroll_vertical(
        &self,
        framebuffer: &mut FramebufferDriver,
        delta_y: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_y == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(framebuffer.info.height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(framebuffer.info.width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        let stride = framebuffer.info.pixels_per_scan_line;
        let fb_ptr = framebuffer.info.base_address as *mut u32;
        let width = end_x - start_x;

        // Usar scroll estilo Linux - mucho más eficiente
        self.linux_style_scroll_vertical(fb_ptr, stride, start_y, end_y, start_x, width, delta_y)?;

        Ok(())
    }

    /// Scroll vertical estilo Linux - muy eficiente
    fn linux_style_scroll_vertical(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        start_y: u32,
        end_y: u32,
        start_x: u32,
        width: u32,
        delta_y: i32,
    ) -> Result<(), &'static str> {
        if delta_y == 0 {
            return Ok(());
        }

        unsafe {
            if delta_y > 0 {
                // Scroll hacia abajo - estilo Linux optimizado
                let delta = delta_y as u32;
                let lines_to_move = end_y - start_y - delta;
                
                if lines_to_move > 0 {
                    // Calcular tamaños para operaciones de memoria
                    let bytes_per_line = width * 4; // 4 bytes por píxel
                    let total_bytes = lines_to_move * bytes_per_line;
                    
                    // Usar memmove para mover todo el bloque de una vez
                    let src_start = (start_y * stride + start_x) as usize;
                    let dst_start = ((start_y + delta) * stride + start_x) as usize;
                    
                    // Mover líneas de abajo hacia arriba - una sola operación
                    core::ptr::copy(
                        fb_ptr.add(src_start),
                        fb_ptr.add(dst_start),
                        total_bytes as usize
                    );
                }
                
                // Limpiar líneas superiores - usar memset para mejor rendimiento
                let clear_bytes = delta * width * 4;
                let clear_start = (start_y * stride + start_x) as usize;
                core::ptr::write_bytes(
                    fb_ptr.add(clear_start),
                    0,
                    clear_bytes as usize
                );
            } else {
                // Scroll hacia arriba - estilo Linux optimizado
                let abs_delta = (-delta_y) as u32;
                let lines_to_move = end_y - start_y - abs_delta;
                
                if lines_to_move > 0 {
                    // Calcular tamaños para operaciones de memoria
                    let bytes_per_line = width * 4; // 4 bytes por píxel
                    let total_bytes = lines_to_move * bytes_per_line;
                    
                    // Usar memmove para mover todo el bloque de una vez
                    let src_start = ((start_y + abs_delta) * stride + start_x) as usize;
                    let dst_start = (start_y * stride + start_x) as usize;
                    
                    // Mover líneas de arriba hacia abajo - una sola operación
                    core::ptr::copy(
                        fb_ptr.add(src_start),
                        fb_ptr.add(dst_start),
                        total_bytes as usize
                    );
                }
                
                // Limpiar líneas inferiores - usar memset para mejor rendimiento
                let clear_bytes = abs_delta * width * 4;
                let clear_start = ((end_y - abs_delta) * stride + start_x) as usize;
                core::ptr::write_bytes(
                    fb_ptr.add(clear_start),
                    0,
                    clear_bytes as usize
                );
            }
        }

        Ok(())
    }

    /// Scroll vertical usando instrucciones SSE para mejor rendimiento
    fn try_sse_scroll_vertical(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        start_y: u32,
        end_y: u32,
        start_x: u32,
        width: u32,
        delta_y: i32,
    ) -> bool {
        if width < 4 {
            return false; // SSE necesita al menos 4 píxeles
        }

        unsafe {
            if delta_y > 0 {
                // Scroll hacia abajo usando SSE
                let delta = delta_y as u32;
                for y in (start_y..(end_y - delta)).rev() {
                    let src_offset = (y * stride + start_x) as usize;
                    let dst_offset = ((y + delta) * stride + start_x) as usize;
                    
                    // Usar SSE para copiar 4 píxeles a la vez
                    let mut x = 0;
                    while x + 4 <= width {
                        let src = fb_ptr.add(src_offset + x as usize);
                        let dst = fb_ptr.add(dst_offset + x as usize);
                        
                        // Cargar 4 píxeles (16 bytes) usando SSE
                        let pixels = core::ptr::read_unaligned(src as *const [u32; 4]);
                        core::ptr::write_unaligned(dst as *mut [u32; 4], pixels);
                        
                        x += 4;
                    }
                    
                    // Copiar píxeles restantes
                    while x < width {
                        let src_offset_px = src_offset + x as usize;
                        let dst_offset_px = dst_offset + x as usize;
                        *fb_ptr.add(dst_offset_px) = *fb_ptr.add(src_offset_px);
                        x += 1;
                    }
                }
                
                // Limpiar líneas superiores
                for y in start_y..(start_y + delta) {
                    let offset = (y * stride + start_x) as usize;
                    let mut x = 0;
                    while x + 4 <= width {
                        let dst = fb_ptr.add(offset + x as usize);
                        core::ptr::write_unaligned(dst as *mut [u32; 4], [0; 4]);
                        x += 4;
                    }
                    
                    // Limpiar píxeles restantes
                    while x < width {
                        *fb_ptr.add(offset + x as usize) = 0;
                        x += 1;
                    }
                }
            } else {
                // Scroll hacia arriba usando SSE
                let abs_delta = (-delta_y) as u32;
                for y in start_y..(end_y - abs_delta) {
                    let src_offset = ((y + abs_delta) * stride + start_x) as usize;
                    let dst_offset = (y * stride + start_x) as usize;
                    
                    // Usar SSE para copiar 4 píxeles a la vez
                    let mut x = 0;
                    while x + 4 <= width {
                        let src = fb_ptr.add(src_offset + x as usize);
                        let dst = fb_ptr.add(dst_offset + x as usize);
                        
                        // Cargar 4 píxeles (16 bytes) usando SSE
                        let pixels = core::ptr::read_unaligned(src as *const [u32; 4]);
                        core::ptr::write_unaligned(dst as *mut [u32; 4], pixels);
                        
                        x += 4;
                    }
                    
                    // Copiar píxeles restantes
                    while x < width {
                        let src_offset_px = src_offset + x as usize;
                        let dst_offset_px = dst_offset + x as usize;
                        *fb_ptr.add(dst_offset_px) = *fb_ptr.add(src_offset_px);
                        x += 1;
                    }
                }
                
                // Limpiar líneas inferiores
                for y in (end_y - abs_delta)..end_y {
                    let offset = (y * stride + start_x) as usize;
                    let mut x = 0;
                    while x + 4 <= width {
                        let dst = fb_ptr.add(offset + x as usize);
                        core::ptr::write_unaligned(dst as *mut [u32; 4], [0; 4]);
                        x += 4;
                    }
                    
                    // Limpiar píxeles restantes
                    while x < width {
                        *fb_ptr.add(offset + x as usize) = 0;
                        x += 1;
                    }
                }
            }
        }

        true
    }

    /// Fallback de scroll vertical usando métodos tradicionales optimizados
    fn fallback_scroll_vertical(
        &self,
        framebuffer: &mut FramebufferDriver,
        delta_y: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_y == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(framebuffer.info.height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(framebuffer.info.width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        // Usar acceso directo al framebuffer para mejor rendimiento
        let stride = framebuffer.info.pixels_per_scan_line;
        let fb_ptr = framebuffer.info.base_address as *mut u32;

        unsafe {
            if delta_y > 0 {
                // Scroll hacia abajo - copiar líneas de arriba hacia abajo
                let delta = delta_y as u32;
                for y in (start_y..(end_y - delta)).rev() {
                    for x in start_x..end_x {
                        let src_offset = (y * stride + x) as usize;
                        let dst_offset = ((y + delta) * stride + x) as usize;
                        *fb_ptr.add(dst_offset) = *fb_ptr.add(src_offset);
                    }
                }
                
                // Limpiar líneas superiores
                for y in start_y..(start_y + delta) {
                    for x in start_x..end_x {
                        let offset = (y * stride + x) as usize;
                        *fb_ptr.add(offset) = 0; // Color negro
                    }
                }
            } else {
                // Scroll hacia arriba - copiar líneas de abajo hacia arriba
                let abs_delta = (-delta_y) as u32;
                for y in start_y..(end_y - abs_delta) {
                    for x in start_x..end_x {
                        let src_offset = ((y + abs_delta) * stride + x) as usize;
                        let dst_offset = (y * stride + x) as usize;
                        *fb_ptr.add(dst_offset) = *fb_ptr.add(src_offset);
                    }
                }
                
                // Limpiar líneas inferiores
                for y in (end_y - abs_delta)..end_y {
                    for x in start_x..end_x {
                        let offset = (y * stride + x) as usize;
                        *fb_ptr.add(offset) = 0; // Color negro
                    }
                }
            }
        }

        Ok(())
    }

    /// Scroll horizontal optimizado usando técnicas de Linux
    fn optimized_scroll_horizontal(
        &self,
        framebuffer: &mut FramebufferDriver,
        delta_x: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_x == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(framebuffer.info.height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(framebuffer.info.width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        let stride = framebuffer.info.pixels_per_scan_line;
        let fb_ptr = framebuffer.info.base_address as *mut u32;
        let width = end_x - start_x;

        // Usar scroll estilo Linux - mucho más eficiente
        self.linux_style_scroll_horizontal(fb_ptr, stride, start_y, end_y, start_x, width, delta_x)?;

        Ok(())
    }

    /// Scroll horizontal estilo Linux - muy eficiente
    fn linux_style_scroll_horizontal(
        &self,
        fb_ptr: *mut u32,
        stride: u32,
        start_y: u32,
        end_y: u32,
        start_x: u32,
        width: u32,
        delta_x: i32,
    ) -> Result<(), &'static str> {
        if delta_x == 0 {
            return Ok(());
        }

        unsafe {
            if delta_x > 0 {
                // Scroll hacia la derecha - estilo Linux
                let delta = delta_x as u32;
                let pixels_to_move = width - delta;
                
                if pixels_to_move > 0 {
                    // Calcular bytes para mover
                    let bytes_to_move = pixels_to_move * 4; // 4 bytes por píxel
                    
                    for y in start_y..end_y {
                        let src_start = (y * stride + start_x + delta) as usize;
                        let dst_start = (y * stride + start_x) as usize;
                        
                        // Mover píxeles de derecha a izquierda
                        core::ptr::copy(
                            fb_ptr.add(src_start),
                            fb_ptr.add(dst_start),
                            bytes_to_move as usize
                        );
                        
                        // Limpiar píxeles de la derecha
                        let clear_start = (y * stride + start_x + width - delta) as usize;
                        core::ptr::write_bytes(
                            fb_ptr.add(clear_start),
                            0,
                            (delta * 4) as usize
                        );
                    }
                } else {
                    // Si no hay píxeles que mover, solo limpiar
                    for y in start_y..end_y {
                        let clear_start = (y * stride + start_x) as usize;
                        core::ptr::write_bytes(
                            fb_ptr.add(clear_start),
                            0,
                            (width * 4) as usize
                        );
                    }
                }
            } else {
                // Scroll hacia la izquierda - estilo Linux
                let abs_delta = (-delta_x) as u32;
                let pixels_to_move = width - abs_delta;
                
                if pixels_to_move > 0 {
                    // Calcular bytes para mover
                    let bytes_to_move = pixels_to_move * 4; // 4 bytes por píxel
                    
                    for y in start_y..end_y {
                        let src_start = (y * stride + start_x + abs_delta) as usize;
                        let dst_start = (y * stride + start_x) as usize;
                        
                        // Mover píxeles de izquierda a derecha
                        core::ptr::copy(
                            fb_ptr.add(src_start),
                            fb_ptr.add(dst_start),
                            bytes_to_move as usize
                        );
                        
                        // Limpiar píxeles de la izquierda
                        let clear_start = (y * stride + start_x) as usize;
                        core::ptr::write_bytes(
                            fb_ptr.add(clear_start),
                            0,
                            (abs_delta * 4) as usize
                        );
                    }
                } else {
                    // Si no hay píxeles que mover, solo limpiar
                    for y in start_y..end_y {
                        let clear_start = (y * stride + start_x) as usize;
                        core::ptr::write_bytes(
                            fb_ptr.add(clear_start),
                            0,
                            (width * 4) as usize
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Fallback de scroll horizontal usando métodos tradicionales optimizados
    fn fallback_scroll_horizontal(
        &self,
        framebuffer: &mut FramebufferDriver,
        delta_x: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_x == 0 {
            return Ok(());
        }

        let start_y = core::cmp::max(0, scroll_rect.y) as u32;
        let end_y = core::cmp::min(framebuffer.info.height as i32, scroll_rect.y + scroll_rect.height as i32) as u32;
        let start_x = core::cmp::max(0, scroll_rect.x) as u32;
        let end_x = core::cmp::min(framebuffer.info.width as i32, scroll_rect.x + scroll_rect.width as i32) as u32;

        // Usar acceso directo al framebuffer para mejor rendimiento
        let stride = framebuffer.info.pixels_per_scan_line;
        let fb_ptr = framebuffer.info.base_address as *mut u32;

        unsafe {
            if delta_x > 0 {
                // Scroll hacia la derecha - copiar píxeles de izquierda a derecha
                let delta = delta_x as u32;
                for y in start_y..end_y {
                    for x in (start_x..end_x - delta).rev() {
                        let src_offset = (y * stride + x + delta) as usize;
                        let dst_offset = (y * stride + x) as usize;
                        *fb_ptr.add(dst_offset) = *fb_ptr.add(src_offset);
                    }
                    
                    // Limpiar píxeles de la derecha
                    for x in (end_x - delta)..end_x {
                        let offset = (y * stride + x) as usize;
                        *fb_ptr.add(offset) = 0; // Color negro
                    }
                }
            } else {
                // Scroll hacia la izquierda - copiar píxeles de derecha a izquierda
                let abs_delta = (-delta_x) as u32;
                for y in start_y..end_y {
                    for x in (start_x + abs_delta)..end_x {
                        let src_offset = (y * stride + x - abs_delta) as usize;
                        let dst_offset = (y * stride + x) as usize;
                        *fb_ptr.add(dst_offset) = *fb_ptr.add(src_offset);
                    }
                    
                    // Limpiar píxeles de la izquierda
                    for x in start_x..(start_x + abs_delta) {
                        let offset = (y * stride + x) as usize;
                        *fb_ptr.add(offset) = 0; // Color negro
                    }
                }
            }
        }

        Ok(())
    }

    /// Scroll por regiones (más eficiente)
    fn scroll_by_regions(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        scroll_delta: Point,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        // Identificar regiones afectadas por el scroll
        let affected_regions: Vec<usize> = self.scroll_regions
            .iter()
            .enumerate()
            .filter(|(_, region)| {
                region.rect.intersects(&scroll_rect)
            })
            .map(|(i, _)| i)
            .collect();

        // Procesar cada región afectada
        let timestamp = self.get_timestamp();
        for &region_idx in &affected_regions {
            if let Some(region) = self.scroll_regions.get_mut(region_idx) {
                // Hacer scroll de la región
                Self::scroll_region_static(framebuffer, region, scroll_delta)?;
                region.dirty = true;
                region.last_update = timestamp;
            }
        }

        Ok(())
    }

    /// Scroll de una región específica (método estático)
    fn scroll_region_static(
        framebuffer: &mut FramebufferDriver,
        region: &mut ScrollRegion,
        scroll_delta: Point,
    ) -> Result<(), &'static str> {
        let rect = region.rect;
        
        // Calcular nueva posición de la región
        let new_rect = Rectangle::new(
            rect.x + scroll_delta.x,
            rect.y + scroll_delta.y,
            rect.width,
            rect.height,
        );

        // Verificar que la nueva posición esté dentro de los límites
        if new_rect.x < 0 || new_rect.y < 0 || 
           new_rect.x + new_rect.width as i32 > 1024 || 
           new_rect.y + new_rect.height as i32 > 768 {
            return Ok(());
        }

        // Usar copia de memoria optimizada
        Self::memory_copy_region_static(framebuffer, rect, new_rect)?;

        // Actualizar la posición de la región
        region.rect = new_rect;

        Ok(())
    }

    /// Scroll de una región específica
    fn scroll_region(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        region: &mut ScrollRegion,
        scroll_delta: Point,
    ) -> Result<(), &'static str> {
        let rect = region.rect;
        
        // Calcular nueva posición de la región
        let new_rect = Rectangle::new(
            rect.x + scroll_delta.x,
            rect.y + scroll_delta.y,
            rect.width,
            rect.height,
        );

        // Verificar que la nueva posición esté dentro de los límites
        if new_rect.x < 0 || new_rect.y < 0 || 
           new_rect.x + new_rect.width as i32 > 1024 || 
           new_rect.y + new_rect.height as i32 > 768 {
            return Ok(());
        }

        // Usar DMA si está disponible para la transferencia
        if self.dma_enabled {
            self.dma_copy_region(framebuffer, rect, new_rect)?;
        } else {
            self.memory_copy_region(framebuffer, rect, new_rect)?;
        }

        Ok(())
    }

    /// Scroll de pantalla completa (fallback)
    fn scroll_full_screen(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        scroll_delta: Point,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        // Implementación de scroll de pantalla completa
        // Esto es menos eficiente pero más simple
        
        if scroll_delta.y > 0 {
            // Scroll hacia abajo
            for y in (scroll_rect.y..scroll_rect.y + scroll_rect.height as i32 - scroll_delta.y).rev() {
                for x in scroll_rect.x..scroll_rect.x + scroll_rect.width as i32 {
                    let color = framebuffer.get_pixel(x as u32, y as u32);
                    framebuffer.put_pixel(x as u32, (y + scroll_delta.y) as u32, color);
                }
            }
        } else if scroll_delta.y < 0 {
            // Scroll hacia arriba
            for y in scroll_rect.y..scroll_rect.y + scroll_rect.height as i32 + scroll_delta.y {
                for x in scroll_rect.x..scroll_rect.x + scroll_rect.width as i32 {
                    let color = framebuffer.get_pixel(x as u32, (y - scroll_delta.y) as u32);
                    framebuffer.put_pixel(x as u32, y as u32, color);
                }
            }
        }

        if scroll_delta.x != 0 {
            // Scroll horizontal (similar al vertical)
            self.scroll_horizontal(framebuffer, scroll_delta.x, scroll_rect)?;
        }

        Ok(())
    }

    /// Scroll horizontal
    fn scroll_horizontal(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        delta_x: i32,
        scroll_rect: Rectangle,
    ) -> Result<(), &'static str> {
        if delta_x > 0 {
            // Scroll hacia la derecha
            for x in (scroll_rect.x..scroll_rect.x + scroll_rect.width as i32 - delta_x).rev() {
                for y in scroll_rect.y..scroll_rect.y + scroll_rect.height as i32 {
                    let color = framebuffer.get_pixel(x as u32, y as u32);
                    framebuffer.put_pixel((x + delta_x) as u32, y as u32, color);
                }
            }
        } else if delta_x < 0 {
            // Scroll hacia la izquierda
            for x in scroll_rect.x..scroll_rect.x + scroll_rect.width as i32 + delta_x {
                for y in scroll_rect.y..scroll_rect.y + scroll_rect.height as i32 {
                    let color = framebuffer.get_pixel((x - delta_x) as u32, y as u32);
                    framebuffer.put_pixel(x as u32, y as u32, color);
                }
            }
        }

        Ok(())
    }

    /// Copia de región usando DMA
    fn dma_copy_region(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        src_rect: Rectangle,
        dst_rect: Rectangle,
    ) -> Result<(), &'static str> {
        // En una implementación real, esto usaría DMA para copiar memoria
        // Por ahora simulamos con copia de memoria optimizada
        
        let width = core::cmp::min(src_rect.width, dst_rect.width);
        let height = core::cmp::min(src_rect.height, dst_rect.height);
        
        // Usar operaciones de memoria optimizadas
        unsafe {
            self.optimized_memory_copy(
                framebuffer,
                src_rect.x, src_rect.y,
                dst_rect.x, dst_rect.y,
                width, height,
            )?;
        }

        Ok(())
    }

    /// Copia de región usando memoria
    /// Copia de memoria de región (método estático)
    fn memory_copy_region_static(
        framebuffer: &mut FramebufferDriver,
        src_rect: Rectangle,
        dst_rect: Rectangle,
    ) -> Result<(), &'static str> {
        let width = core::cmp::min(src_rect.width, dst_rect.width);
        let height = core::cmp::min(src_rect.height, dst_rect.height);
        
        for y in 0..height {
            for x in 0..width {
                let color = framebuffer.get_pixel(
                    (src_rect.x + x as i32) as u32,
                    (src_rect.y + y as i32) as u32,
                );
                framebuffer.put_pixel(
                    (dst_rect.x + x as i32) as u32,
                    (dst_rect.y + y as i32) as u32,
                    color,
                );
            }
        }

        Ok(())
    }

    fn memory_copy_region(
        &mut self,
        framebuffer: &mut FramebufferDriver,
        src_rect: Rectangle,
        dst_rect: Rectangle,
    ) -> Result<(), &'static str> {
        Self::memory_copy_region_static(framebuffer, src_rect, dst_rect)
    }

    /// Copia de memoria optimizada usando instrucciones de ensamblador
    unsafe fn optimized_memory_copy(
        &self,
        framebuffer: &mut FramebufferDriver,
        src_x: i32, src_y: i32,
        dst_x: i32, dst_y: i32,
        mut width: u32, height: u32,
    ) -> Result<(), &'static str> {
        // Obtener punteros a los datos del framebuffer
        let fb_ptr = framebuffer.info.base_address;
        if fb_ptr == 0 {
            return Err("Framebuffer no disponible");
        }

        let bytes_per_pixel = 4; // ARGB
        let stride = framebuffer.info.pixels_per_scan_line * bytes_per_pixel;
        
        let src_offset = (src_y * stride as i32 + src_x * bytes_per_pixel as i32) as isize;
        let dst_offset = (dst_y * stride as i32 + dst_x * bytes_per_pixel as i32) as isize;
        
        let src_ptr = (fb_ptr as *mut u8).offset(src_offset);
        let dst_ptr = (fb_ptr as *mut u8).offset(dst_offset);
        
        // Copiar línea por línea usando instrucciones optimizadas
        for y in 0..height {
            let mut line_src = src_ptr.offset((y * stride) as isize);
            let mut line_dst = dst_ptr.offset((y * stride) as isize);
            
            // Usar rep movsd para copia rápida de 32 bits
            asm!(
                "rep movsd",
                inout("edi") line_dst,
                inout("esi") line_src,
                inout("ecx") width,
                options(nostack, preserves_flags)
            );
        }

        Ok(())
    }

    /// Limpiar regiones sucias
    pub fn clear_dirty_regions(&mut self, framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
        for region in &mut self.scroll_regions {
            if region.dirty {
                // Limpiar la región con color de fondo
                framebuffer.draw_rect(
                    region.rect.x as u32,
                    region.rect.y as u32,
                    region.rect.width,
                    region.rect.height,
                    Color::BLACK
                );
                region.dirty = false;
            }
        }
        Ok(())
    }

    /// Obtener timestamp actual
    fn get_timestamp(&self) -> u64 {
        // En una implementación real, esto usaría un timer del sistema
        0 // Placeholder
    }

    /// Obtener estadísticas de optimización
    pub fn get_stats(&self) -> OptimizationStats {
        let dirty_regions = self.scroll_regions.iter().filter(|r| r.dirty).count();
        let total_regions = self.scroll_regions.len();
        
        OptimizationStats {
            dma_enabled: self.dma_enabled,
            double_buffering_enabled: self.double_buffer.is_some(),
            region_scroll_enabled: self.config.enable_region_scroll,
            total_scroll_regions: total_regions,
            dirty_regions,
            last_scroll_position: self.last_scroll_position,
        }
    }

    /// Aplicar doble buffering
    pub fn apply_double_buffering(&mut self, framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
        if !self.config.enable_double_buffering {
            return Ok(());
        }

        // Implementación básica de doble buffering
        // En una implementación real, esto copiaría el buffer de trabajo al buffer de pantalla
        // Por ahora, solo marcamos que el doble buffering está activo
        Ok(())
    }

    /// Sincronizar con el hardware
    pub fn sync_with_hardware(&mut self, framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
        // Forzar sincronización con el hardware de gráficos
        // En una implementación real, esto sincronizaría con el hardware
        
        // Si estamos usando DMA, sincronizar las transferencias
        if self.config.enable_dma {
            // En una implementación real, esto esperaría a que las transferencias DMA terminen
        }
        
        Ok(())
    }
}

/// Estadísticas de optimización
#[derive(Debug, Clone)]
pub struct OptimizationStats {
    pub dma_enabled: bool,
    pub double_buffering_enabled: bool,
    pub region_scroll_enabled: bool,
    pub total_scroll_regions: usize,
    pub dirty_regions: usize,
    pub last_scroll_position: Point,
}

/// Instancia global del optimizador
static mut GRAPHICS_OPTIMIZER: Option<GraphicsOptimizer> = None;

/// Inicializar el optimizador global
pub fn init_graphics_optimizer() -> Result<(), &'static str> {
    unsafe {
        if GRAPHICS_OPTIMIZER.is_some() {
            return Ok(());
        }

        // Detectar si estamos usando hardware real
        let using_real_hardware = crate::gpu_fallback::is_using_real_hardware();
        
        let mut config = GraphicsOptimizationConfig::default();
        
        // Configuración optimizada para ambos tipos de hardware
        // Tanto hardware real como QEMU pueden beneficiarse del scroll optimizado
        config.enable_dma = false; // Deshabilitar DMA temporalmente para evitar problemas
        config.enable_double_buffering = false; // Deshabilitar doble buffer temporalmente
        config.enable_region_scroll = true;
        config.enable_memory_optimization = true; // HABILITAR scroll por hardware para todos
        
        if using_real_hardware {
            // Configuración para hardware real
            config.scroll_region_size = 256; // Regiones más grandes para hardware real
            config.double_buffer_size = 256 * 1024; // 256KB de doble buffer
        } else {
            // Configuración para QEMU/emuladores
            config.scroll_region_size = 128; // Regiones más pequeñas para QEMU
            config.double_buffer_size = 128 * 1024; // 128KB de doble buffer
        }
        
        let mut optimizer = GraphicsOptimizer::new(config);
        optimizer.initialize()?;
        GRAPHICS_OPTIMIZER = Some(optimizer);
    }
    Ok(())
}

/// Obtener referencia al optimizador
pub fn get_graphics_optimizer() -> Result<&'static mut GraphicsOptimizer, &'static str> {
    unsafe {
        GRAPHICS_OPTIMIZER.as_mut().ok_or("Optimizador no inicializado")
    }
}

/// Scroll optimizado globalmente
pub fn optimized_scroll(
    framebuffer: &mut FramebufferDriver,
    scroll_delta: Point,
    scroll_rect: Rectangle,
) -> Result<(), &'static str> {
    let optimizer = get_graphics_optimizer()?;
    optimizer.optimized_scroll(framebuffer, scroll_delta, scroll_rect)
}

/// Limpiar regiones sucias globalmente
pub fn clear_dirty_regions(framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
    let optimizer = get_graphics_optimizer()?;
    optimizer.clear_dirty_regions(framebuffer)
}

/// Obtener estadísticas globales
pub fn get_optimization_stats() -> Option<OptimizationStats> {
    unsafe {
        GRAPHICS_OPTIMIZER.as_ref().map(|o| o.get_stats())
    }
}

/// Forzar actualización del framebuffer para hardware real
pub fn force_framebuffer_update() -> Result<(), &'static str> {
    unsafe {
        if let Some(ref mut optimizer) = GRAPHICS_OPTIMIZER {
            // Obtener el framebuffer actual
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                // Solo hacer clear de las regiones sucias (operación simple)
                optimizer.clear_dirty_regions(&mut *fb)?;
                
                // Aplicar optimizaciones básicas sin operaciones complejas
                if optimizer.config.enable_double_buffering {
                    // Aplicar doble buffer de forma simplificada
                    optimizer.apply_double_buffering(&mut *fb)?;
                }
            }
        }
    }
    Ok(())
}
