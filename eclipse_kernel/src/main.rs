//! Punto de entrada principal del kernel Eclipse OS

#![no_std]
#![no_main]

// use core::panic::PanicInfo;
use core::error::Error;
extern crate alloc;
use alloc::boxed::Box;
use alloc::format;

// Importar funciones necesarias
use eclipse_kernel::main_simple::kernel_main;
use eclipse_kernel::allocator;
use eclipse_kernel::drivers::framebuffer::{
    FramebufferDriver, Color, FramebufferInfo, 
    init_framebuffer, init_hardware_acceleration,
    has_hardware_acceleration, get_acceleration_type,
    get_hardware_acceleration_info, hardware_fill,
    write_text, clear_screen, draw_rounded_rect,
    is_framebuffer_available
};
use eclipse_kernel::drivers::pci::{GpuType, GpuInfo};

// Estructuras para paginación x86-64
#[repr(C, align(4096))]
#[derive(Debug, Clone, Copy)]
pub struct PageTable {
    entries: [u64; 512],
}

impl PageTable {
    pub const fn new() -> Self {
        Self {
            entries: [0; 512],
        }
    }

    pub fn set_entry(&mut self, index: usize, entry: u64) {
        if index < 512 {
            self.entries[index] = entry;
        }
    }

    pub fn get_entry(&self, index: usize) -> u64 {
        if index < 512 {
            self.entries[index]
        } else {
            0
        }
    }
}

// Bits de las entradas de tabla de páginas
const PAGE_PRESENT: u64 = 1 << 0;           // Presente en memoria
const PAGE_WRITABLE: u64 = 1 << 1;          // Permiso de escritura
const PAGE_USER: u64 = 1 << 2;              // Acceso desde modo usuario
const PAGE_HUGE: u64 = 1 << 7;              // Página grande (2MB/1GB)
const PAGE_NO_EXECUTE: u64 = 1 << 63;       // No ejecutar

// Salida serie COM1 para diagnóstico temprano
// Salida serie COM1 para diagnóstico temprano
// Serial COM1 para logs tempranos
#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let mut val: u8;
    core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
    val
}

unsafe fn serial_init() {
    let base: u16 = 0x3F8;
    outb(base + 1, 0x00);
    outb(base + 3, 0x80);
    outb(base + 0, 0x01);
    outb(base + 1, 0x00);
    outb(base + 3, 0x03);
    outb(base + 2, 0xC7);
    outb(base + 4, 0x0B);
}

unsafe fn serial_write_byte(b: u8) {
    let base: u16 = 0x3F8;
    while (inb(base + 5) & 0x20) == 0 {}
    outb(base, b);
}

unsafe fn serial_write_str(s: &str) {
    for &c in s.as_bytes() { serial_write_byte(c); }
}

unsafe fn serial_write_hex32(val: u32) {
    for i in (0..8).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

unsafe fn serial_write_hex64(val: u64) {
    for i in (0..16).rev() {
        let nibble = (val >> (i * 4)) & 0xF;
        let c = if nibble < 10 {
            b'0' + nibble as u8
        } else {
            b'A' + (nibble - 10) as u8
        };
        serial_write_byte(c);
    }
}

// Usamos el panic handler definido en lib.rs
// Punto de entrada principal del kernel (con parámetros del framebuffer)
#[no_mangle]
pub extern "C" fn _start(framebuffer_info_ptr: *const FramebufferInfo) -> ! {
    unsafe {

        // Leer la información del framebuffer de manera segura
        let fb_info = core::ptr::read_volatile(framebuffer_info_ptr);
        // Inicializar el framebuffer usando la nueva API
        match init_framebuffer(
            fb_info.base_address,
            fb_info.width,
            fb_info.height,
            fb_info.pixels_per_scan_line,
            fb_info.pixel_format,
            fb_info.red_mask | fb_info.green_mask | fb_info.blue_mask
        ) {
            Ok(()) => {
                // Limpiar pantalla con color de fondo
                clear_screen(Color::new(20, 20, 40, 255));
                // Crear información de GPU simulada para demostrar la aceleración
                // Nota: En un sistema real, esto vendría de la detección PCI
                let gpu_info = GpuInfo {
                    pci_device: eclipse_kernel::drivers::pci::PciDevice {
                        bus: 0,
                        device: 2,
                        function: 0,
                        vendor_id: 0x8086,
                        device_id: 0x5916,
                        class_code: 0x03,
                        subclass_code: 0x00,
                        prog_if: 0x00,
                        revision_id: 0x02,
                        header_type: 0x00,
                        status: 0x0010,
                        command: 0x0007,
                    },
                    gpu_type: GpuType::Nvidia, // Simular Intel Graphics
                    memory_size: 1024 * 1024 * 1024 * 8, // 8GB
                    is_primary: true,
                    supports_2d: true,
                    supports_3d: true,
                    max_resolution: (3840, 2160),
                };

                // Inicializar aceleración de hardware
                init_hardware_acceleration(&gpu_info);
                // Dibujar interfaz usando las nuevas funcionalidades
                draw_interface();
                
                // También intentar dibujo directo como fallback
                draw_direct_fallback(fb_info);
            }
            Err(e) => {
                // Fallback: dibujo directo en memoria
                draw_fallback_pattern(fb_info);
            }
        }

        loop {
            core::hint::spin_loop();
        }
    }
}

/// Dibujar interfaz principal del kernel
unsafe fn draw_interface() {
    // Título principal con fondo redondeado
    draw_rounded_rect(10, 10, 400, 60, 10, Color::new(30, 30, 60, 255)).unwrap_or_default();
    write_text(20, 30, "Eclipse OS Kernel", Color::WHITE).unwrap_or_default();
    write_text(20, 50, "Version 0.5.0 - Con Aceleración de Hardware", Color::CYAN).unwrap_or_default();

    // Información del sistema
    draw_rounded_rect(10, 80, 600, 120, 8, Color::new(40, 40, 40, 255)).unwrap_or_default();
    write_text(20, 100, "Sistema Operativo en Desarrollo", Color::GREEN).unwrap_or_default();
    
    // Mostrar tipo de aceleración disponible
    let accel_type = get_acceleration_type();
    let accel_text = match accel_type {
        eclipse_kernel::drivers::framebuffer::HardwareAcceleration::Intel2D => "Intel Graphics 2D",
        eclipse_kernel::drivers::framebuffer::HardwareAcceleration::Nvidia2D => "NVIDIA 2D",
        eclipse_kernel::drivers::framebuffer::HardwareAcceleration::Amd2D => "AMD 2D",
        eclipse_kernel::drivers::framebuffer::HardwareAcceleration::Generic2D => "Genérico 2D",
        eclipse_kernel::drivers::framebuffer::HardwareAcceleration::None => "Sin aceleración",
    };
    
    write_text(20, 120, "Aceleración de Hardware:", Color::YELLOW).unwrap_or_default();
    write_text(20, 140, accel_text, Color::ORANGE).unwrap_or_default();
    
    // Demostrar aceleración de hardware si está disponible
    if has_hardware_acceleration() {
        write_text(20, 160, "Probando aceleración de hardware...", Color::LIME).unwrap_or_default();
        
        // Usar hardware_fill para demostrar aceleración
        hardware_fill(20, 180, 200, 50, Color::new(255, 100, 100, 255)).unwrap_or_default();
        write_text(30, 200, "Rectángulo acelerado por hardware", Color::WHITE).unwrap_or_default();
    }

    // Barra de estado
    draw_rounded_rect(10, 220, 600, 40, 5, Color::new(60, 60, 60, 255)).unwrap_or_default();
    write_text(20, 240, "Sistema listo - Presiona cualquier tecla para continuar", Color::LIGHT_GRAY).unwrap_or_default();
}

/// Dibujo directo en memoria del framebuffer
unsafe fn draw_direct_fallback(fb_info: FramebufferInfo) {
    
    let fb_ptr = fb_info.base_address as *mut u32;
    let width = fb_info.width.min(1280);
    let height = fb_info.height.min(720);
    
    // Limpiar pantalla con color azul oscuro
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) as isize;
            core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00101040); // Azul oscuro
        }
    }
    
    // Dibujar rectángulo rojo en la esquina superior izquierda
    for y in 0..100 {
        for x in 0..400 {
            if y < height && x < width {
                let offset = (y * width + x) as isize;
                core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x00FF0000); // Rojo
            }
        }
    }
    
    // Dibujar rectángulo verde debajo del rojo
    for y in 100..200 {
        for x in 0..400 {
            if y < height && x < width {
                let offset = (y * width + x) as isize;
                core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x0000FF00); // Verde
            }
        }
    }
    
    // Dibujar rectángulo azul debajo del verde
    for y in 200..300 {
        for x in 0..400 {
            if y < height && x < width {
                let offset = (y * width + x) as isize;
                core::ptr::write_volatile(fb_ptr.add(offset as usize), 0x000000FF); // Azul
            }
        }
    }
}

/// Dibujo de fallback si el framebuffer no se inicializa correctamente
unsafe fn draw_fallback_pattern(fb_info: FramebufferInfo) {
    let fb_ptr = fb_info.base_address as *mut u32;
    let width = fb_info.width.min(1280);
    let height = fb_info.height.min(720);
    
    // Dibujar patrón de prueba
    for y in 0..height {
        for x in 0..width {
            let color = if (x + y) % 2 == 0 {
                0x00FF0000 // Rojo
            } else {
                0x0000FF00 // Verde
            };
            let offset = (y * width + x) as isize;
            core::ptr::write_volatile(fb_ptr.add(offset as usize), color);
        }
    }
}

unsafe fn kernel_call() -> Result<(), Box<dyn Error>> {
    kernel_main()?;
    Ok(())
}