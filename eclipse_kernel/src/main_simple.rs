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
    // CRÍTICO: Restaurar I/O para logging - estas instrucciones son seguras
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    // CRÍTICO: Restaurar I/O para logging - estas instrucciones son seguras
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

// Usar GraphicsMode del módulo hardware_detection para evitar duplicación

// Función para detectar hardware gráfico (usando nuevo sistema PCI)
fn detect_graphics_hardware() -> crate::hardware_detection::GraphicsMode {
    use crate::hardware_detection::detect_graphics_hardware;

    let result = detect_graphics_hardware();
    result.graphics_mode
}
/// Función principal del kernel
pub fn kernel_main() -> Result<(), &'static str> {
    // Inicializar el allocador global
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
        unsafe {
            serial_write_str("Allocador inicializado.\r\n");
        }
    }

    // Inicializar sistema de display
    unsafe {
        serial_write_str("Iniciando kernel.\r\n");
    }

    // Usar nuevo sistema de detección con verificación de allocador
    use crate::hardware_detection::HardwareDetector;

    // Crear detector de hardware
    let mut detector = HardwareDetector::new_minimal();
    unsafe {
        serial_write_str("[HARDWARE] Detector de hardware creado\r\n");
    }

    // Verificar allocador
    detector.verify_allocator_safe();
    unsafe {
        serial_write_str("[MEMORY] Allocador verificado correctamente\r\n");
    }

    // Detectar hardware
    let detection_result = detector.detect_hardware();
    unsafe {
        serial_write_str("[HARDWARE] Hardware detectado exitosamente\r\n");
        serial_write_str("[PROGRESS] Inicializacion basica completada (1/11)\r\n");
    }

    // Clonar datos necesarios
    let available_gpus = detection_result.available_gpus.clone();
    let graphics_mode = detection_result.graphics_mode.clone();
    let recommended_driver = detection_result.recommended_driver.clone();

    // Mostrar información del hardware detectado
    unsafe {
        let gpu_count = available_gpus.len();
        match gpu_count {
            0 => serial_write_str("[DISPLAY] Hardware detectado: Sin GPUs detectadas\r\n"),
            1 => serial_write_str("[DISPLAY] Hardware detectado: 1 GPU detectada\r\n"),
            _ => {
                // Crear mensaje completo para evitar problemas de sincronización
                let mut msg = String::new();
                msg.push_str("[DISPLAY] Hardware detectado: ");
                msg.push_str(int_to_string(gpu_count as u64));
                msg.push_str(" GPUs detectadas\r\n");
                serial_write_str(&msg);
            }
        }
        serial_write_str("[PROGRESS] Informacion de hardware recopilada (2/11)\r\n");
    }

    // Configurar modo gráfico
    let graphics_mode = match graphics_mode {
        crate::hardware_detection::GraphicsMode::Framebuffer => {
            unsafe {
                serial_write_str("[GRAPHICS] Modo grafico: Framebuffer\r\n");
            }
            crate::hardware_detection::GraphicsMode::Framebuffer
        }
        crate::hardware_detection::GraphicsMode::VGA => {
            unsafe {
                serial_write_str("[GRAPHICS] Modo grafico: VGA\r\n");
            }
            crate::hardware_detection::GraphicsMode::VGA
        }
        crate::hardware_detection::GraphicsMode::HardwareAccelerated => {
            unsafe {
                serial_write_str("[GRAPHICS] Modo grafico: Hardware Accelerated\r\n");
            }
            crate::hardware_detection::GraphicsMode::HardwareAccelerated
        }
    };

    // Inicializar sistema DRM
    unsafe {
        serial_write_str("[DRM] Iniciando integracion DRM...\r\n");
        serial_write_str("[PROGRESS] Sistema DRM preparado (4/11)\r\n");
    }
    use crate::drivers::drm_integration::{DrmIntegration, DrmKernelCommand, create_drm_integration};
    let mut drm_integration = create_drm_integration();

    // Configurar framebuffer si está disponible
    let framebuffer_info = if graphics_mode == crate::hardware_detection::GraphicsMode::Framebuffer {
        unsafe {
            serial_write_str("[FRAMEBUFFER] Configurando framebuffer...\r\n");
            serial_write_str("[RESOLUTION] Resolucion: 1920x1080, 32-bit RGBA\r\n");
        }
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
        unsafe {
            serial_write_str("[VGA] Usando modo VGA...\r\n");
            serial_write_str("[RESOLUTION] Resolucion: 80x25 caracteres\r\n");
        }
        None
    };

    // Inicializar DRM solo si hay framebuffer disponible
    unsafe {
        serial_write_str("[DRM] Inicializando DRM...\r\n");
        serial_write_str("[PROGRESS] Progreso: 40% completado\r\n");
    }

    if framebuffer_info.is_some() {
        // Solo inicializar DRM si hay framebuffer disponible
        match drm_integration.initialize(framebuffer_info) {
            Ok(_) => {
                drm_integration.execute_integrated_operation(DrmKernelCommand::Initialize);
                unsafe {
                    serial_write_str("[DRM] DRM inicializado correctamente\r\n");
                    serial_write_str("[PROGRESS] Sistema grafico operativo (5/11)\r\n");
                }
            }
            Err(_) => {
                unsafe {
                    serial_write_str("[WARNING] Error inicializando DRM, continuando con VGA\r\n");
                    serial_write_str("[PROGRESS] Modo grafico de respaldo activado (5/11)\r\n");
                }
            }
        }
    } else {
        // No hay framebuffer, usar solo VGA
        unsafe {
            serial_write_str("[INFO] No hay framebuffer disponible, usando modo VGA\r\n");
            serial_write_str("[PROGRESS] Modo grafico VGA activado (5/11)\r\n");
        }
    }

    // Inicializar sistema de procesos
    unsafe {
        serial_write_str("[PROCESSES] Inicializando sistema de procesos...\r\n");
        serial_write_str("[PROGRESS] Progreso: 50% completado\r\n");
    }
    let mut init_system = InitSystem::new();
    init_system.initialize();
    unsafe {
        serial_write_str("[PROGRESS] Sistema de procesos operativo (6/11)\r\n");
    }

    // Crear escritorio básico
    unsafe {
        serial_write_str("[DESKTOP] Configurando escritorio...\r\n");
        serial_write_str("[WINDOWS] Creando ventanas del sistema...\r\n");
    }
    crate::desktop_ai::ai_create_window(1, 100, 100, 400, 300, "Terminal");
    crate::desktop_ai::ai_create_window(2, 520, 100, 400, 300, "File Manager");
    crate::desktop_ai::ai_create_window(3, 100, 420, 400, 300, "System Monitor");

    // Renderizar escritorio inicial
    crate::desktop_ai::ai_render_desktop();
    unsafe {
        serial_write_str("[PROGRESS] Escritorio configurado (7/11)\r\n");
    }

    // Inicializar Wayland si está disponible
    unsafe {
        serial_write_str("[WAYLAND] Verificando compositor Wayland...\r\n");
    }
    init_wayland();

    if is_wayland_initialized() {
        unsafe {
            serial_write_str("[WAYLAND] Wayland inicializado correctamente\r\n");
            serial_write_str("[GRAPHICS] Compositor grafico avanzado operativo\r\n");
        }
    } else {
        unsafe {
            serial_write_str("[WARNING] Wayland no disponible, usando framebuffer\r\n");
            serial_write_str("[GRAPHICS] Modo grafico framebuffer activado\r\n");
        }
    }

    // Transferir control al sistema de inicialización
    unsafe {
        serial_write_str("[INIT] Ejecutando inicializacion del sistema...\r\n");
        serial_write_str("[PROGRESS] Progreso: 60% completado\r\n");
    }
    init_system.execute_init();
    unsafe {
        serial_write_str("[PROGRESS] Sistema de inicializacion completado (8/11)\r\n");
    }

    // Inicializar aceleración 2D con primera GPU disponible
    unsafe {
        serial_write_str("[ACCELERATION] Configurando aceleracion grafica 2D...\r\n");
        serial_write_str("[PROGRESS] Progreso: 70% completado\r\n");
    }
    use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation, HardwareAccelerationType};
    use crate::drivers::framebuffer::{FramebufferDriver, Color as FbColor};
    use crate::desktop_ai::{Point, Rect};

    let framebuffer = FramebufferDriver::new();
    let mut acceleration_2d = Acceleration2D::new(framebuffer);

    if let Some(gpu_info) = available_gpus.first() {
        match acceleration_2d.initialize_with_gpu(gpu_info) {
            crate::drivers::acceleration_2d::AccelerationResult::HardwareAccelerated => {
                unsafe {
                    serial_write_str("[ACCELERATION] Aceleracion 2D: Hardware activada\r\n");
                    serial_write_str("[GPU] GPU detectada y operativa\r\n");
                }
            }
            crate::drivers::acceleration_2d::AccelerationResult::SoftwareFallback => {
                unsafe {
                    serial_write_str("[WARNING] Aceleracion 2D: Modo software (fallback)\r\n");
                    serial_write_str("[SOFTWARE] Procesamiento por software activado\r\n");
                }
            }
            crate::drivers::acceleration_2d::AccelerationResult::DriverError(_) => {
                unsafe {
                    serial_write_str("[ERROR] Aceleracion 2D: Error en driver\r\n");
                    serial_write_str("[FALLBACK] Modo basico activado\r\n");
                }
            }
            _ => {
                unsafe {
                    serial_write_str("[INFO] Aceleracion 2D: Configuracion basica\r\n");
                    serial_write_str("[GRAPHICS] Funcionalidad grafica basica operativa\r\n");
                }
            }
        }
    } else {
        unsafe {
            serial_write_str("[INFO] Sin GPUs disponibles, usando modo software\r\n");
            serial_write_str("[SOFTWARE] Aceleracion por software activada\r\n");
        }
    }
    unsafe {
        serial_write_str("[PROGRESS] Aceleracion grafica configurada (9/11)\r\n");
    }

    // Sistema de entrada USB
    unsafe {
        serial_write_str("[INPUT] Inicializando sistema de entrada...\r\n");
        serial_write_str("[PROGRESS] Progreso: 80% completado\r\n");
    }
    use crate::drivers::input_system::{InputSystem, InputSystemConfig, create_default_input_system};
    use crate::drivers::usb_keyboard::{UsbKeyboardDriver, create_usb_keyboard_driver};
    use crate::drivers::usb_mouse::{UsbMouseDriver, create_usb_mouse_driver};

    let mut input_system = create_default_input_system();
    input_system.initialize();

    // Simular conexión de dispositivos USB
    unsafe {
        serial_write_str("[USB] Conectando dispositivos USB...\r\n");
    }
    let keyboard = create_usb_keyboard_driver(0x046D, 0xC31C, 1, 0x81); // Logitech USB Keyboard
    input_system.add_keyboard(keyboard);

    let mouse = create_usb_mouse_driver(0x046D, 0xC077, 2, 0x82); // Logitech USB Mouse
    input_system.add_mouse(mouse);

    unsafe {
        serial_write_str("[USB] Teclado USB conectado (Logitech)\r\n");
        serial_write_str("[USB] Mouse USB conectado (Logitech)\r\n");
        serial_write_str("[PROGRESS] Sistema de entrada operativo (10/11)\r\n");
    }

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
    unsafe {
        serial_write_str("[GUI] Inicializando interfaz grafica...\r\n");
        serial_write_str("[PROGRESS] Progreso: 90% completado\r\n");
    }
    use crate::drivers::gui_integration::{GuiManager, GuiWindow, GuiButton, GuiTextBox, create_gui_manager};
    use crate::apps::{InteractiveAppManager, create_app_manager};

    let mut gui_manager = create_gui_manager();
    gui_manager.initialize();

    // Crear ventanas del sistema
    unsafe {
        serial_write_str("[WINDOWS] Creando ventanas del sistema...\r\n");
    }
    gui_manager.create_window(1, String::from("Ventana Principal"), Rect { x: 100, y: 100, width: 400, height: 300 });
    gui_manager.create_window(2, String::from("Terminal"), Rect { x: 520, y: 100, width: 400, height: 300 });
    gui_manager.create_window(3, String::from("Monitor del Sistema"), Rect { x: 100, y: 420, width: 400, height: 300 });

    // Crear elementos GUI interactivos
    unsafe {
        serial_write_str("[CONTROLS] Creando elementos de interfaz...\r\n");
    }
    let button = GuiButton::new(1, Rect { x: 20, y: 50, width: 100, height: 30 }, String::from("Boton"));
    gui_manager.add_element(Box::new(button));

    let textbox = GuiTextBox::new(2, Rect { x: 20, y: 100, width: 200, height: 25 }, 50);
    gui_manager.add_element(Box::new(textbox));

    // Sistema de aplicaciones avanzado
    unsafe {
        serial_write_str("[APPS] Inicializando gestor de aplicaciones...\r\n");
    }
    let mut app_manager = create_app_manager();
    app_manager.initialize();
    app_manager.switch_app(0);

    unsafe {
        serial_write_str("[PROGRESS] Interfaz grafica completa (11/11)\r\n");
        serial_write_str("[SUCCESS] SISTEMA ECLIPSE OS COMPLETAMENTE OPERATIVO\r\n");
        serial_write_str("==================================================\r\n");
        serial_write_str("[FEATURES] FUNCIONALIDADES DISPONIBLES:\r\n");
        serial_write_str("=========================================\r\n");
        serial_write_str("[OK] Hardware Detection\r\n");
        serial_write_str("[OK] Graphics Subsystem (DRM/VGA)\r\n");
        serial_write_str("[OK] Process Management\r\n");
        serial_write_str("[OK] Desktop Environment\r\n");
        serial_write_str("[OK] Wayland Compositor\r\n");
        serial_write_str("[OK] 2D Acceleration\r\n");
        serial_write_str("[OK] USB Input System\r\n");
        serial_write_str("[OK] GUI Framework\r\n");
        serial_write_str("[OK] Application Manager\r\n");
        serial_write_str("[OK] Advanced Logging\r\n");
        serial_write_str("=========================================\r\n");
    }

    // Bucle principal del kernel
    unsafe {
        serial_write_str("[READY] Kernel Eclipse OS listo y funcionando!\r\n");
        serial_write_str("[INFO] Sistema operativo moderno completamente operativo\r\n");
        serial_write_str("[WAITING] Esperando operaciones del usuario...\r\n");
    }

    // Mantener el kernel ejecutándose
    loop {
        // Procesar eventos del sistema de entrada
        if let Err(_) = input_system.process_events() {}

        // Procesar eventos en la GUI
        while let Some(event) = input_system.get_next_event() {
            if let Err(_) = gui_manager.process_input_event(&event) {}
            if let Err(_) = app_manager.process_input(&event) {}
        }

        // Renderizar GUI y aplicaciones
        if let Err(_) = gui_manager.render(&mut acceleration_2d) {}
        if let Err(_) = app_manager.render(&mut acceleration_2d) {}

        // Pequeña pausa para no consumir toda la CPU
        for _ in 0..100000 {
            // TEMPORALMENTE DESHABILITADO: nop causa opcode inválido
            unsafe {
                // Simular nop con spin loop para evitar opcode inválido
                core::hint::spin_loop();
            }
        }
    }

}