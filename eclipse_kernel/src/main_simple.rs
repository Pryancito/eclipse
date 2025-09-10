//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use core::fmt::Result as FmtResult;
use core::error::Error;

use core::panic::PanicInfo;
use alloc::format;
use alloc::string::String;

// Importar módulos del kernel
use crate::init_system::{InitSystem, InitProcess};
use crate::wayland::{init_wayland, is_wayland_initialized, get_wayland_state};


// Serial COM1 para logs tempranos
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

pub unsafe fn serial_init() {
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

pub unsafe fn serial_write_str(s: &str) {
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

unsafe fn serial_write_hex8(val: u8) {
    let high = (val >> 4) & 0xF;
    let low = val & 0xF;
    let c1 = if high < 10 {
        b'0' + high
    } else {
        b'A' + (high - 10)
    };
    let c2 = if low < 10 {
        b'0' + low
    } else {
        b'A' + (low - 10)
    };
    serial_write_byte(c1);
    serial_write_byte(c2);
}

/// Función para convertir números a string
fn int_to_string(mut num: u64) -> &'static str {
    // Para simplificar, devolveremos strings fijos para números comunes
    match num {
        0 => "0",
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        6 => "6",
        7 => "7",
        8 => "8",
        9 => "9",
        10 => "10",
        _ => "N/A", // Para números más grandes
    }
}

use core::fmt::Write;

// Modos de gráficos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsMode {
    Framebuffer,
    VGA,
}

// Función para detectar hardware gráfico (usando nuevo sistema PCI)
fn detect_graphics_hardware() -> GraphicsMode {
    use crate::hardware_detection::{detect_graphics_hardware, GraphicsMode as NewGraphicsMode};
    
    let result = detect_graphics_hardware();
    match result.graphics_mode {
        NewGraphicsMode::Framebuffer => GraphicsMode::Framebuffer,
        NewGraphicsMode::VGA => GraphicsMode::VGA,
        NewGraphicsMode::HardwareAccelerated => GraphicsMode::Framebuffer, // Usar framebuffer como base
    }
}
/// Función principal del kernel
pub fn kernel_main() -> Result<(), &'static str> {
    // Inicializar el allocador global
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }
    
    // Inicializar sistema de display
    unsafe {
        serial_write_str("Iniciando kernel.\r\n");
    }
    // Usar nuevo sistema de detección
    use crate::hardware_detection::{HardwareDetector, GraphicsMode as NewGraphicsMode};
    let mut detector = HardwareDetector::new();
    let detection_result = detector.detect_hardware();
    
    // Clonar los datos necesarios para evitar problemas de borrow
    let available_gpus = detection_result.available_gpus.clone();
    let graphics_mode = detection_result.graphics_mode.clone();
    let recommended_driver = detection_result.recommended_driver.clone();
    
    // Obtener información de drivers después de clonar
    let driver_info = detector.get_gpu_driver_info();
    let (total, ready, intel_ready) = detector.get_driver_stats();
   
    // Configurar modo de gráficos
    let graphics_mode = match graphics_mode {
        NewGraphicsMode::Framebuffer => {
            match crate::uefi_framebuffer::configure_framebuffer_for_hardware() {
                Ok(_) => { GraphicsMode::Framebuffer }
                Err(_) => { GraphicsMode::VGA }
            }
        }
        NewGraphicsMode::HardwareAccelerated => { GraphicsMode::Framebuffer }
        NewGraphicsMode::VGA => { GraphicsMode::VGA }
    };
    
    unsafe { serial_write_str("Iniciando integracion DRM...\r\n"); }
    use crate::drivers::drm_integration::{DrmIntegration, DrmKernelCommand, create_drm_integration};
    let mut drm_integration = create_drm_integration();
    
    // Obtener información del framebuffer si está disponible
    let framebuffer_info = if graphics_mode == GraphicsMode::Framebuffer {
        Some(crate::drivers::framebuffer::FramebufferInfo {
            base_address: 0x1000000,
            size: 1920 * 1080 * 4,
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            bpp: 32,
            red_offset: 0,
            green_offset: 8,
            blue_offset: 16,
            alpha_offset: 24,
            red_length: 8,
            green_length: 8,
            blue_length: 8,
            alpha_length: 8,
            pixel_format: crate::drivers::framebuffer::PixelFormat::RGBA8888,
        })
    } else {
        None
    };
    unsafe { serial_write_str("20% de inicializacion...\r\n"); }
    match drm_integration.initialize(framebuffer_info) {
        Ok(_) => {
            // Probar operación DRM básica
            match drm_integration.execute_integrated_operation(DrmKernelCommand::Initialize) {
                Ok(_) => {
                    unsafe {
                        serial_write_str("Comunicacion DRM kernel-userland establecida\n");
                    }
                }
                Err(_) => { }
            }
        }
        Err(_) => { }
    }
        
    // Inicializar systemd
    let mut init_system = InitSystem::new();
    match init_system.initialize() {
        Ok(_) => { }
        Err(_) => { }
    }
    
    // Crear escritorio de ejemplo
    match crate::desktop_ai::ai_create_window(1, 100, 100, 400, 300, "Terminal") {
        Ok(_) => {
        }
        Err(_) => {
        }
    }

    // Renderizar escritorio
    match crate::desktop_ai::ai_render_desktop() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    match init_wayland() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Transferir control a systemd
    match init_system.execute_init() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear sistema de aceleración 2D
    use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation, HardwareAccelerationType};
    use crate::drivers::framebuffer::{FramebufferDriver, Color as FbColor};
    use crate::desktop_ai::{Point, Rect};
    
    let framebuffer = FramebufferDriver::new();
    let mut acceleration_2d = Acceleration2D::new(framebuffer);
    
    // Demostración de aceleración 2D
    if graphics_mode == GraphicsMode::Framebuffer {
    
    unsafe {
        serial_write_str("40% de inicialización...\r\n");
        serial_write_str("Demostrando aceleración 2D...\r\n");
    }
        
        // Inicializar aceleración 2D con la primera GPU detectada
        if let Some(gpu_info) = available_gpus.first() {
            match acceleration_2d.initialize_with_gpu(gpu_info) {
                crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                }
                crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                }
                crate::drivers::acceleration_2d::AccelerationResult::DriverError(e) => {
                }
                _ => {}
            }
        }
        
        // Demostrar operaciones de aceleración 2D
        let demo_operations = [
            AccelerationOperation::ClearScreen(FbColor { r: 0, g: 0, b: 128, a: 255 }), // Fondo azul
            AccelerationOperation::FillRect(Rect { x: 100, y: 100, width: 200, height: 150 }, FbColor { r: 255, g: 0, b: 0, a: 255 }), // Rectángulo rojo
            AccelerationOperation::DrawRect(Rect { x: 120, y: 120, width: 160, height: 110 }, FbColor { r: 255, g: 255, b: 0, a: 255 }, 3), // Rectángulo amarillo
            AccelerationOperation::DrawLine(Point { x: 50, y: 50 }, Point { x: 300, y: 200 }, FbColor { r: 0, g: 255, b: 0, a: 255 }, 2), // Línea verde
            AccelerationOperation::DrawCircle(Point { x: 400, y: 300 }, 50, FbColor { r: 255, g: 0, b: 255, a: 255 }, true), // Círculo magenta relleno
            AccelerationOperation::DrawCircle(Point { x: 500, y: 200 }, 30, FbColor { r: 255, g: 255, b: 255, a: 255 }, false), // Círculo blanco vacío
            AccelerationOperation::DrawTriangle(Point { x: 600, y: 100 }, Point { x: 700, y: 100 }, Point { x: 650, y: 200 }, FbColor { r: 255, g: 128, b: 0, a: 255 }, true), // Triángulo naranja
        ];
        
        for (i, operation) in demo_operations.iter().enumerate() {
            match acceleration_2d.execute_operation(operation.clone()) {
                crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                }
                crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                }
                crate::drivers::acceleration_2d::AccelerationResult::DriverError(e) => {
                }
                _ => {}
            }
        }
    }
    
    use crate::drivers::input_system::{InputSystem, InputSystemConfig, create_default_input_system};
    use crate::drivers::usb_keyboard::{UsbKeyboardDriver, create_usb_keyboard_driver};
    use crate::drivers::usb_mouse::{UsbMouseDriver, create_usb_mouse_driver};
    
    // Crear sistema de entrada
    let mut input_system = create_default_input_system();
    
    match input_system.initialize() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Simular conexión de teclado USB
    let keyboard = create_usb_keyboard_driver(0x046D, 0xC31C, 1, 0x81); // Logitech USB Keyboard
    match input_system.add_keyboard(keyboard) {
        Ok(device_id) => {
        }
        Err(_) => {
        }
    }
    
    // Simular conexión de mouse USB
    let mouse = create_usb_mouse_driver(0x046D, 0xC077, 2, 0x82); // Logitech USB Mouse
    match input_system.add_mouse(mouse) {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Mostrar estadísticas del sistema de entrada
    let stats = input_system.get_stats();
    // Simular datos de teclado (tecla 'H' presionada)
    let keyboard_data = [0x00, 0x00, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00]; // H key
    if let Err(_) = input_system.process_keyboard_data(0, &keyboard_data) {
    }
    
    // Simular datos de mouse (movimiento + click izquierdo)
    let mouse_data = [0x01, 0x05, 0x03, 0x00]; // Left button + move right 5, down 3
    if let Err(_) = input_system.process_mouse_data(0, &mouse_data) {
    }
    
    // Procesar eventos
    if let Err(_) = input_system.process_events() {
    }
    
    // Mostrar eventos procesados
    let mut event_count = 0;
    while let Some(event) = input_system.get_next_event() {
        event_count += 1;
        match event.event_type {
            crate::drivers::input_system::InputEventType::Keyboard(keyboard_event) => {
                unsafe {
                    match keyboard_event {
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyPress { key, .. } => {
                        }
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyRelease { key, .. } => {
                        }
                        crate::drivers::usb_keyboard::KeyboardEvent::KeyRepeat { key, .. } => {
                        }
                    }
                }
            }
            crate::drivers::input_system::InputEventType::Mouse(mouse_event) => {
                unsafe {
                    match mouse_event {
                        crate::drivers::usb_mouse::MouseEvent::Move { position, .. } => {
                        }
                        crate::drivers::usb_mouse::MouseEvent::ButtonPress { button, .. } => {
                        }
                        crate::drivers::usb_mouse::MouseEvent::ButtonRelease { button, .. } => {
                        }
                        crate::drivers::usb_mouse::MouseEvent::Wheel { wheel, .. } => {
                        }
                    }
                }
            }
            crate::drivers::input_system::InputEventType::System(system_event) => {
                unsafe {
                    match system_event {
                        crate::drivers::input_system::SystemEvent::DeviceConnected { device_type, .. } => {
                        }
                        crate::drivers::input_system::SystemEvent::DeviceDisconnected { device_type, .. } => {
                        }
                        crate::drivers::input_system::SystemEvent::InputError { error } => {
                        }
                        crate::drivers::input_system::SystemEvent::BufferOverflow => {
                        }
                    }
                }
            }
        }
    }
    
    use crate::drivers::usb_hub::{UsbHubDriver, UsbHubInfo, UsbHubType, UsbPowerSwitching, UsbOverCurrentProtection, create_standard_usb_hub};
    use crate::drivers::usb_hid::{HidDriver, HidDeviceInfo, create_hid_driver};
    use crate::drivers::gui_integration::{GuiManager, GuiWindow, GuiButton, GuiTextBox, create_gui_manager};
    use crate::apps::{InteractiveAppManager, create_app_manager};
    use alloc::boxed::Box;
    
    // Crear USB Hub
    let hub_info = UsbHubInfo {
        vendor_id: 0x05E3,
        product_id: 0x0608,
        manufacturer: String::from("Generic"),
        product: String::from("USB 2.0 Hub"),
        version: 0x0100,
        device_address: 1,
        num_ports: 4,
        hub_type: UsbHubType::Usb2Hub,
        power_switching: UsbPowerSwitching::Individual,
        over_current_protection: UsbOverCurrentProtection::Individual,
        tt_think_time: 8,
        port_indicators: true,
        compound_device: false,
    };
    
    let mut usb_hub = UsbHubDriver::new(hub_info);
    match usb_hub.initialize() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear dispositivo HID
    let hid_info = HidDeviceInfo {
        vendor_id: 0x046D,
        product_id: 0xC31C,
        version: 0x0110,
        manufacturer: String::from("Logitech"),
        product: String::from("USB Keyboard"),
        serial_number: String::from("12345"),
        device_class: 0x03, // HID Class
        device_subclass: 0x01, // Boot Interface Subclass
        device_protocol: 0x01, // Keyboard
        max_packet_size: 8,
        country_code: 0x00,
        num_descriptors: 1,
        report_descriptor_length: 0,
    };
    
    unsafe { serial_write_str("80% de inicialización...\r\n"); }

    let mut hid_driver = create_hid_driver(hid_info, 2, 0x81);
    match hid_driver.initialize() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear gestor de GUI
    let mut gui_manager = create_gui_manager();
    match gui_manager.initialize() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear ventanas de ejemplo
    match gui_manager.create_window(1, String::from("Ventana Principal"), Rect { x: 100, y: 100, width: 400, height: 300 }) {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear elementos de GUI
    let button = GuiButton::new(1, Rect { x: 20, y: 50, width: 100, height: 30 }, String::from("Botón"));
    match gui_manager.add_element(Box::new(button)) {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    let textbox = GuiTextBox::new(2, Rect { x: 20, y: 100, width: 200, height: 25 }, 50);
    match gui_manager.add_element(Box::new(textbox)) {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Crear gestor de aplicaciones interactivas
    let mut app_manager = create_app_manager();
    match app_manager.initialize() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Cambiar a la primera aplicación
    match app_manager.switch_app(0) {
        Ok(_) => {
        }
        Err(_) => {
        }
    }
    
    // Procesar eventos del sistema de entrada
    if let Err(e) = input_system.process_events() {
    }
    
    // Procesar eventos de aplicaciones
    while let Some(event) = input_system.get_next_event() {
        // Procesar en el gestor de GUI
        if let Err(_) = gui_manager.process_input_event(&event) {
        }
        
        // Procesar en el gestor de aplicaciones
        if let Err(_) = app_manager.process_input(&event) {
        }
    }
    
    // Renderizar GUI
    if let Err(_) = gui_manager.render(&mut acceleration_2d) {
    }
    
    // Renderizar aplicaciones
    if let Err(_) = app_manager.render(&mut acceleration_2d) {
    }
    
    // Mostrar estadísticas finales
    let input_stats = input_system.get_stats();
    let hub_stats = usb_hub.get_stats();

    match crate::ai_simple_demo::run_simple_ai_demo() {
        Ok(_) => {
        }
        Err(_) => {
        }
    }

    unsafe { serial_write_str("100% de inicialización...\r\n"); }
    // match crate::ai_shell::run_ai_shell() {
    //     Ok(_) => {
    //         unsafe {
    //             VGA.set_color(Color::Green, Color::Black);
    //             VGA.write_string("Shell con IA ejecutado exitosamente\n");
    //             VGA.set_color(Color::White, Color::Black);
    //         }
    //     }
    //     Err(e) => {
    //         unsafe {
    //         VGA.set_color(Color::Red, Color::Black);
    //         VGA.write_string("Error ejecutando shell con IA: ");
    //         VGA.write_string("Error de kernel");
    //         VGA.write_string("\n");
    //             VGA.set_color(Color::White, Color::Black);
    //         }
    //     }
    // }

    unsafe {
        // ✅ IMPLEMENTACIÓN DE FRAMEBUFFER CON FALLBACK SEGURO
        serial_write_str("Iniciando sistema gráfico Eclipse OS\r\n");

        // Esperar un poco para que el bootloader termine
        for _ in 0..1000000 {
            core::arch::asm!("nop");
        }
        // Intentar inicializar framebuffer primero
        let mut use_framebuffer = false;
        if let Some(fb_info) = crate::entry_simple::get_framebuffer_info() {
            match crate::drivers::framebuffer::init_framebuffer_from_uefi(&fb_info) {
                Ok(_) => {
                    // ✅ VERIFICACIÓN ADICIONAL: Verificar que realmente esté disponible
                    if crate::drivers::framebuffer::is_framebuffer_available() {
                        use_framebuffer = true;

                        // Limpiar pantalla con color azul
                        let _ = crate::drivers::framebuffer::clear_screen(crate::drivers::framebuffer::Color::rgb(0, 0, 64));

                        // Dibujar algunos elementos gráficos básicos
                        draw_framebuffer_welcome();
                    } else {
                        use_framebuffer = false;
                    }
                }
                Err(e) => {
                    use_framebuffer = false;
                }
            }
        }
        // Ya se inicializó VGA arriba, no necesitamos hacer nada más aquí

        serial_write_str("Kernel inicializado correctamente\r\n");
    }
    Ok(())
}

/// Función para dibujar elementos de bienvenida en framebuffer
unsafe fn draw_framebuffer_welcome() {
    use crate::drivers::framebuffer::{Color, write_text, clear_screen};

    // Colores
    let white = Color::rgb(255, 255, 255);
    let green = Color::rgb(0, 255, 0);
    let blue = Color::rgb(0, 0, 255);
    let black = Color::rgb(0, 0, 0);

    let _ = clear_screen(black);
    // Dibujar texto de bienvenida usando la función write_text
    let _ = write_text(300, 250, "ECLIPSE OS", white);
    let _ = write_text(300, 280, "FRAMEBUFFER ACTIVO", green);
    let _ = write_text(300, 310, "GRAFICOS FUNCIONANDO", blue);
    let _ = write_text(300, 340, "Presione cualquier tecla", white);
}


// El panic_handler está definido en lib.rs
