//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use core::fmt::Result as FmtResult;
use core::error::Error;
use core::fmt::Write;

use core::panic::PanicInfo;
use alloc::format;
use alloc::string::String;

// Importar módulos del kernel
use crate::init_system::{InitSystem, InitProcess};
use crate::wayland::{init_wayland, is_wayland_initialized, get_wayland_state};


use crate::drivers::framebuffer::{Color, get_framebuffer,
    FramebufferDriver, FramebufferInfo, clear_screen
};
use crate::ai_typing_system::{AiTypingSystem, AiTypingConfig, TypingEffect,
    create_ai_typing_system};
// Módulo ai_font_generator removido
use crate::drivers::pci::{GpuType, GpuInfo};
use crate::hardware_detection::{GraphicsMode, detect_graphics_hardware};

/// Función principal del kernel
pub fn kernel_main() {
    // Verificar si hay framebuffer disponible
    if let Some(fb) = get_framebuffer() {
        match detect_graphics_hardware().graphics_mode {
            GraphicsMode::Framebuffer => {
                fb.clear_screen(Color::BLACK);
            }
            GraphicsMode::VGA => {
                fb.clear_screen(Color::BLACK);
            }
            GraphicsMode::HardwareAccelerated => {
                use crate::drivers::pci::PciDevice;
                use crate::drivers::pci::GpuInfo;
                use crate::drivers::pci::GpuType;
                use crate::drivers::pci::PciManager;
                let mut pci_manager = PciManager::new();
                pci_manager.scan_devices_safe();
                let gpu_info = pci_manager.get_primary_gpu();
                let Some(gpu_info) = gpu_info else {
                    panic!("No se pudo crear la GPU info");
                };
                fb.clear_screen(Color::BLACK);
                fb.init_hardware_acceleration(gpu_info);
            }
        }

        // Crear sistema de AI para escritura
        let mut ai_system = create_ai_typing_system();
        
        // Configurar efecto de escritura
        let mut config = AiTypingConfig::default();
        config.effect = TypingEffect::Typewriter;
        config.color = Color::WHITE;
        ai_system.set_config(config);
        
        // Escribir mensaje especial con efecto rainbow
        let special_message = String::from("Eclipse OS Kernel con AI");
        ai_system.write_message(fb, &special_message);
        
        // Demostrar función optimizada directa
        ai_system.set_position(20, 200);
        ai_system.write_message_direct(fb, "Funcion optimizada con punteros directos");
        
        // Escribir mensaje de bienvenida
        ai_system.write_welcome_message(fb);
        
        // Escribir mensajes del sistema
        ai_system.write_system_message(fb, 0); // "Cargando sistema de archivos..."
        ai_system.write_system_message(fb, 1); // "Inicializando drivers de hardware..."
        ai_system.write_system_message(fb, 2); // "Configurando red..."
        
        // Escribir mensaje de éxito
        ai_system.write_success_message(fb, 0); // "Operacion completada exitosamente"
        
        // Cambiar a efecto rainbow para mensaje especial
        let mut rainbow_config = AiTypingConfig::default();
        rainbow_config.effect = TypingEffect::Rainbow;
        ai_system.set_config(rainbow_config);
         unsafe {
             loop {
                 // Sistema de aprendizaje continuo de la IA
                 ai_system.feed_framebuffer(fb);
                 
                 // Aprendizaje adaptativo avanzado
                 ai_system.adaptive_learning(fb);
                 
                 // Mostrar estadísticas de aprendizaje
                 ai_system.display_learning_stats(fb);
                 
                 // Pausa optimizada para el loop
                 for _ in 0..100000 {
                     core::hint::spin_loop();
                 }
             }
         }
    } else {
        panic!("No hay framebuffer disponible");
    }
}
    /*
    // El dibujo del framebuffer se movió al bucle principal para evitar parpadeo
    // Usar nuevo sistema de detección con verificación de allocador
    use crate::hardware_detection::HardwareDetector;

    // Crear detector de hardware
    let mut detector = HardwareDetector::new_minimal();
    unsafe {
        // Logging removido temporalmente para evitar breakpoint
    }

    // Verificar allocador
    detector.verify_allocator_safe();
    unsafe {
        // Logging removido temporalmente para evitar breakpoint
    }

    // Detectar hardware
    let detection_result = detector.detect_hardware();
    unsafe {
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    }

    // Clonar datos necesarios
    let available_gpus = detection_result.available_gpus.clone();
    let graphics_mode = detection_result.graphics_mode.clone();
    let recommended_driver = detection_result.recommended_driver.clone();

    // Mostrar información del hardware detectado
    let gpu_count = available_gpus.len();
    match gpu_count {
        0 => {
            // Logging removido temporalmente para evitar breakpoint
        },
        1 => {
            // Logging removido temporalmente para evitar breakpoint
        },
        _ => {
            // Logging removido temporalmente para evitar breakpoint
        },
    }
    unsafe {
        // Logging removido temporalmente para evitar breakpoint
    }

    // Configurar modo gráfico
    let graphics_mode = match graphics_mode {
        crate::hardware_detection::GraphicsMode::Framebuffer => {
            // Logging removido temporalmente para evitar breakpoint
            crate::hardware_detection::GraphicsMode::Framebuffer
        }
        crate::hardware_detection::GraphicsMode::VGA => {
            // Logging removido temporalmente para evitar breakpoint
            crate::hardware_detection::GraphicsMode::VGA
        }
        crate::hardware_detection::GraphicsMode::HardwareAccelerated => {
            // Logging removido temporalmente para evitar breakpoint
            crate::hardware_detection::GraphicsMode::HardwareAccelerated
        }
    };

    // Inicializar sistema DRM
    unsafe {
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    }
    use crate::drivers::drm_integration::{DrmIntegration, DrmKernelCommand, create_drm_integration};
    let mut drm_integration = create_drm_integration();

    // Crear instancia del FramebufferDriver para verificar si está inicializado
    let framebuffer_driver = crate::drivers::framebuffer::FramebufferDriver::new();

    if framebuffer_driver.is_initialized() {
        // Solo inicializar DRM si hay framebuffer disponible
        let framebuffer_info = framebuffer_driver.get_info();
        match drm_integration.initialize(Some(*framebuffer_info)) {
            Ok(_) => {
                drm_integration.execute_integrated_operation(DrmKernelCommand::Initialize);
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
            Err(_) => {
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
        }
    } else {
        // No hay framebuffer, usar solo VGA
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    }

    // Inicializar sistema de archivos ANTES de los procesos
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    crate::filesystem::init();
    // Logging removido temporalmente para evitar breakpoint

    // Inicializar sistema de procesos
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint

    // Logging removido temporalmente para evitar breakpoint

    // Crear escritorio básico
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    crate::desktop_ai::ai_create_window(1, 100, 100, 400, 300, "Terminal");
    crate::desktop_ai::ai_create_window(2, 520, 100, 400, 300, "File Manager");
    crate::desktop_ai::ai_create_window(3, 100, 420, 400, 300, "System Monitor");

    // Renderizar escritorio inicial
    crate::desktop_ai::ai_render_desktop();
    // Logging removido temporalmente para evitar breakpoint

    // Inicializar Wayland si está disponible
    // Logging removido temporalmente para evitar breakpoint
    init_wayland();

    if is_wayland_initialized() {
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    } else {
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    }

    // Transferir control al sistema de inicialización
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint

    // Logging removido temporalmente para evitar breakpoint

    // Inicializar aceleración 2D con primera GPU disponible
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation, HardwareAccelerationType};
    use crate::drivers::framebuffer::{FramebufferDriver, Color as FbColor};
    use crate::desktop_ai::{Point, Rect};

    let mut framebuffer = FramebufferDriver::new();
    let mut acceleration_2d = Acceleration2D::new(framebuffer.clone());

    if let Some(gpu_info) = available_gpus.first() {
        match acceleration_2d.initialize_with_gpu(gpu_info) {
            crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
            crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
            crate::drivers::acceleration_2d::AccelerationResult::DriverError(_) => {
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
            _ => {
                // Logging removido temporalmente para evitar breakpoint
                // Logging removido temporalmente para evitar breakpoint
            }
        }
    } else {
        // Logging removido temporalmente para evitar breakpoint
        // Logging removido temporalmente para evitar breakpoint
    }

    use crate::drivers::input_system::{InputSystem, InputSystemConfig, create_default_input_system};
    use crate::drivers::usb_keyboard::{UsbKeyboardDriver, create_usb_keyboard_driver};
    use crate::drivers::usb_mouse::{UsbMouseDriver, create_usb_mouse_driver};

    let mut input_system = create_default_input_system();
    input_system.initialize();

    // Simular conexión de dispositivos USB
    // Logging removido temporalmente para evitar breakpoint
    let keyboard = create_usb_keyboard_driver(0x046D, 0xC31C, 1, 0x81); // Logitech USB Keyboard
    input_system.add_keyboard(keyboard);

    let mouse = create_usb_mouse_driver(0x046D, 0xC077, 2, 0x82); // Logitech USB Mouse
    input_system.add_mouse(mouse);

    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint

    // USB Hub
    use crate::drivers::usb_hub::{UsbHubDriver, UsbHubInfo, UsbHubType, UsbPowerSwitching, UsbOverCurrentProtection, create_standard_usb_hub};
    use crate::drivers::usb_hid::{HidDriver, HidDeviceInfo, create_hid_driver};

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
    usb_hub.initialize();

    // Dispositivo HID
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

    let mut hid_driver = create_hid_driver(hid_info, 2, 0x81);
    hid_driver.initialize();

    // Sistema GUI avanzado
    let mut desktop_renderer = crate::desktop_ai::DesktopRenderer::new();
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    use crate::drivers::gui_integration::{GuiManager, GuiWindow, GuiButton, GuiTextBox, create_gui_manager};
    use crate::apps::{InteractiveAppManager, create_app_manager};

    let mut gui_manager = create_gui_manager();
    gui_manager.initialize();

    // Crear ventanas del sistema
    // Logging removido temporalmente para evitar breakpoint
    gui_manager.create_window(1, String::from("Ventana Principal"), Rect { x: 100, y: 100, width: 400, height: 300 });
    gui_manager.create_window(2, String::from("Terminal"), Rect { x: 520, y: 100, width: 400, height: 300 });
    gui_manager.create_window(3, String::from("Monitor del Sistema"), Rect { x: 100, y: 420, width: 400, height: 300 });

    // Crear elementos GUI interactivos
    // Logging removido temporalmente para evitar breakpoint
    let button = GuiButton::new(1, Rect { x: 20, y: 50, width: 100, height: 30 }, String::from("Boton"));
    gui_manager.add_element(Box::new(button));

    let textbox = GuiTextBox::new(2, Rect { x: 20, y: 100, width: 200, height: 25 }, 50);
    gui_manager.add_element(Box::new(textbox));

    // Sistema de aplicaciones avanzado
    // Logging removido temporalmente para evitar breakpoint
    let mut app_manager = create_app_manager();
    app_manager.initialize();
    app_manager.switch_app(0);

    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint

    // Bucle principal del kernel
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
    let mut init_system = InitSystem::new();
    init_system.initialize();
    init_system.execute_init();
    // Dibujar elementos UNA SOLA VEZ al inicio
    if let Some(fb) = get_framebuffer() {
        unsafe {
            let fb_info = *fb.get_info();
            
            // Limpiar pantalla con color negro UNA VEZ
            clear_screen_direct(&fb_info, Color::BLACK);
            
            draw_character_direct(&fb_info, 10, 10, 'A', Color::WHITE);

            // Dibujar texto "Hola Eclipse OS!" con efectos
            draw_text_advanced_direct(&fb_info, 10, 10, "Hola Eclipse OS!", 
                                    Color::WHITE, 
                                    Some(Color::new(0, 0, 0, 128)), // Sombra negra
                                    Some(Color::new(0, 0, 255, 255))); // Contorno azul
            
            // Dibujar algunos rectángulos de colores para demostrar las funciones
            draw_rect_direct(&fb_info, 50, 50, 200, 100, Color::RED);
            draw_rect_direct(&fb_info, 300, 50, 200, 100, Color::GREEN);
            draw_rect_direct(&fb_info, 550, 50, 200, 100, Color::BLUE);
            
            // Dibujar más texto con diferentes efectos
            draw_text_advanced_direct(&fb_info, 10, 200, "Sistema de Graficos Moderno", 
                                    Color::CYAN, 
                                    Some(Color::new(0, 0, 0, 64)), // Sombra sutil
                                    None); // Sin contorno
            
            draw_text_advanced_direct(&fb_info, 10, 220, "Funciones Hibridas Funcionando!", 
                                    Color::YELLOW, 
                                    None, // Sin sombra
                                    Some(Color::new(255, 0, 0, 255))); // Contorno rojo
            
            // Demostrar diferentes caracteres
            draw_text_direct(&fb_info, 10, 250, "ABCDEFGHIJKLMNOPQRSTUVWXYZ", Color::GREEN);
            draw_text_direct(&fb_info, 10, 270, "abcdefghijklmnopqrstuvwxyz", Color::MAGENTA);
            draw_text_direct(&fb_info, 10, 290, "0123456789!@#$%^&*()", Color::ORANGE);
            
        }
    }

    // Mantener el kernel ejecutándose SIN REDIBUJAR
    // Solo bucle infinito sin tocar el framebuffer
    loop {
        // Pausa muy larga para evitar consumo excesivo de CPU
        for _ in 0..1000000 {
            core::hint::spin_loop();
        }
    }
}*/

/*fn draw_interface() {
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
        crate::drivers::framebuffer::HardwareAcceleration::Intel2D => "Intel Graphics 2D",
        crate::drivers::framebuffer::HardwareAcceleration::Nvidia2D => "NVIDIA 2D",
        crate::drivers::framebuffer::HardwareAcceleration::Amd2D => "AMD 2D",
        crate::drivers::framebuffer::HardwareAcceleration::Generic2D => "Genérico 2D",
        crate::drivers::framebuffer::HardwareAcceleration::None => "Sin aceleración",
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

*/
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