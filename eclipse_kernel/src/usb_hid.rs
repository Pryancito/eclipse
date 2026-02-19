//! USB HID (Human Interface Device) - Gaming Peripherals Support
//!
//! ## Objetivo
//! Soporte para ratones y teclados USB gaming además de PS/2.
//!
//! ## Características Gaming Implementadas
//! ### Ratones Gaming:
//! - **Alta frecuencia**: 1000Hz polling rate (1ms de latencia)
//! - **Alto DPI**: Soporta hasta 16000 DPI ajustable
//! - **Botones extra**: Hasta 8 botones (Back, Forward, DPI+, DPI-, Profile, etc.)
//! - **Aceleración por hardware**: Sensor óptico/láser de alta precisión
//! - **RGB**: Control de iluminación RGB
//!
//! ### Teclados Gaming:
//! - **Alta frecuencia**: 1000Hz polling rate
//! - **N-Key Rollover**: Registro simultáneo de todas las teclas
//! - **Anti-ghosting**: Previene registros fantasma
//! - **Teclas macro**: 6+ teclas programables
//! - **RGB por tecla**: Iluminación RGB individual
//! - **Controles multimedia**: Teclas dedicadas para media
//!
//! ## Plan de implementación
//! 1. **Driver USB host**: EHCI o XHCI para enumerar buses USB
//! 2. **PCI**: Detectar controladores USB (class 0x0C, subclass 0x03)
//! 3. **Enumeración**: Reset, address assignment, descriptor reading
//! 4. **HID boot protocol**: Teclado (interface 1) y ratón (interface 2)
//! 5. **HID gaming extensions**: Detectar vendor-specific features
//! 6. **Polling/Interrupt**: Leer reportes a 1000Hz y convertir a InputEvents
//! 7. **Integración**: input_service drena eventos USB además de PS/2
//!
//! ## Vendedores Gaming Soportados
//! - Logitech (G Series)
//! - Razer (DeathAdder, BlackWidow, etc.)
//! - Corsair (Dark Core, K70, etc.)
//! - SteelSeries (Rival, Apex, etc.)
//! - Roccat (Kone, Vulcan, etc.)
//!
//! ## Dependencias
//! - PCI (ya existe)
//! - IRQ para USB (MSI o legacy)
//! - Memoria para buffers DMA (descriptores)
//!
//! ## Estado actual
//! Stub con especificaciones gaming: sin implementación completa.
//! El input actual usa solo PS/2 + detección USB simulada.

/// Capacidades de dispositivos gaming
#[derive(Debug, Clone, Copy)]
pub struct GamingDeviceCapabilities {
    pub vendor_id: u16,
    pub product_id: u16,
    pub max_polling_rate: u32,  // Hz
    pub max_dpi: u32,            // For mice
    pub adjustable_dpi: bool,
    pub extra_buttons: u8,
    pub n_key_rollover: bool,    // For keyboards
    pub macro_keys: u8,
    pub rgb_support: bool,
}

/// Vendedores gaming conocidos
pub mod vendors {
    pub const LOGITECH: u16 = 0x046D;
    pub const RAZER: u16 = 0x1532;
    pub const CORSAIR: u16 = 0x1B1C;
    pub const STEELSERIES: u16 = 0x1038;
    pub const ROCCAT: u16 = 0x1E7D;
}

/// Product IDs de ratones gaming conocidos
pub mod gaming_mice {
    // Logitech Gaming Mice
    pub const LOGITECH_G502_HERO: u16 = 0xC08B;
    pub const LOGITECH_G502_LIGHTSPEED: u16 = 0xC539;
    pub const LOGITECH_G_PRO_WIRELESS: u16 = 0xC088;
    pub const LOGITECH_G703: u16 = 0xC087;
    pub const LOGITECH_G403: u16 = 0xC083;
    pub const LOGITECH_G603: u16 = 0xC537;
    pub const LOGITECH_G305: u16 = 0xC07E;
    
    // Razer Gaming Mice
    pub const RAZER_DEATHADDER_V2: u16 = 0x0084;
    pub const RAZER_DEATHADDER_ELITE: u16 = 0x006E;
    pub const RAZER_VIPER: u16 = 0x0078;
    pub const RAZER_VIPER_ULTIMATE: u16 = 0x007A;
    pub const RAZER_BASILISK_V2: u16 = 0x0085;
    pub const RAZER_NAGA_TRINITY: u16 = 0x0067;
    
    // Corsair Gaming Mice
    pub const CORSAIR_DARK_CORE_RGB_PRO: u16 = 0x1B5E;
    pub const CORSAIR_IRONCLAW_RGB: u16 = 0x1B4C;
    pub const CORSAIR_GLAIVE_RGB: u16 = 0x1B3D;
    pub const CORSAIR_NIGHTSWORD_RGB: u16 = 0x1B5C;
    
    // SteelSeries Gaming Mice
    pub const STEELSERIES_RIVAL_3: u16 = 0x1824;
    pub const STEELSERIES_RIVAL_5: u16 = 0x1850;
    pub const STEELSERIES_RIVAL_600: u16 = 0x1724;
    pub const STEELSERIES_SENSEI_310: u16 = 0x1720;
    
    // Roccat Gaming Mice
    pub const ROCCAT_KONE_AIMO: u16 = 0x2E27;
    pub const ROCCAT_KONE_PRO: u16 = 0x2C8E;
    pub const ROCCAT_BURST_PRO: u16 = 0x2DE1;
}

/// Product IDs de teclados gaming conocidos
pub mod gaming_keyboards {
    // Logitech Gaming Keyboards
    pub const LOGITECH_G915_TKL: u16 = 0xC545;
    pub const LOGITECH_G915: u16 = 0xC541;
    pub const LOGITECH_G513: u16 = 0xC33C;
    pub const LOGITECH_G413: u16 = 0xC33D;
    pub const LOGITECH_G213: u16 = 0xC336;
    
    // Razer Gaming Keyboards
    pub const RAZER_BLACKWIDOW_V3: u16 = 0x024E;
    pub const RAZER_BLACKWIDOW_V3_TKL: u16 = 0x0A24;
    pub const RAZER_HUNTSMAN_ELITE: u16 = 0x0226;
    pub const RAZER_HUNTSMAN_MINI: u16 = 0x0257;
    pub const RAZER_CYNOSA_V2: u16 = 0x025E;
    
    // Corsair Gaming Keyboards
    pub const CORSAIR_K70_RGB_MK2: u16 = 0x1B13;
    pub const CORSAIR_K95_RGB_PLATINUM: u16 = 0x1B2D;
    pub const CORSAIR_K60_RGB_PRO: u16 = 0x1BA0;
    pub const CORSAIR_K65_RGB_MINI: u16 = 0x1BCF;
    
    // SteelSeries Gaming Keyboards
    pub const STEELSERIES_APEX_PRO: u16 = 0x1610;
    pub const STEELSERIES_APEX_7: u16 = 0x1612;
    pub const STEELSERIES_APEX_3: u16 = 0x1614;
    
    // Roccat Gaming Keyboards
    pub const ROCCAT_VULCAN_TKL_PRO: u16 = 0x3098;
    pub const ROCCAT_VULCAN_120_AIMO: u16 = 0x1E7D;
    pub const ROCCAT_PYRO: u16 = 0x2D5C;
}

/// Detectar si un dispositivo USB es un periférico gaming
pub fn is_gaming_device(vendor_id: u16, product_id: u16) -> bool {
    use vendors::*;
    
    match vendor_id {
        LOGITECH => is_logitech_gaming(product_id),
        RAZER => is_razer_gaming(product_id),
        CORSAIR => is_corsair_gaming(product_id),
        STEELSERIES => is_steelseries_gaming(product_id),
        ROCCAT => is_roccat_gaming(product_id),
        _ => false
    }
}

/// Verificar si un product_id de Logitech es gaming
fn is_logitech_gaming(product_id: u16) -> bool {
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    matches!(product_id,
        // Mice
        LOGITECH_G502_HERO | LOGITECH_G502_LIGHTSPEED | LOGITECH_G_PRO_WIRELESS |
        LOGITECH_G703 | LOGITECH_G403 | LOGITECH_G603 | LOGITECH_G305 |
        // Keyboards
        LOGITECH_G915_TKL | LOGITECH_G915 | LOGITECH_G513 |
        LOGITECH_G413 | LOGITECH_G213
    )
}

/// Verificar si un product_id de Razer es gaming
fn is_razer_gaming(product_id: u16) -> bool {
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    matches!(product_id,
        // Mice
        RAZER_DEATHADDER_V2 | RAZER_DEATHADDER_ELITE | RAZER_VIPER |
        RAZER_VIPER_ULTIMATE | RAZER_BASILISK_V2 | RAZER_NAGA_TRINITY |
        // Keyboards
        RAZER_BLACKWIDOW_V3 | RAZER_BLACKWIDOW_V3_TKL | RAZER_HUNTSMAN_ELITE |
        RAZER_HUNTSMAN_MINI | RAZER_CYNOSA_V2
    )
}

/// Verificar si un product_id de Corsair es gaming
fn is_corsair_gaming(product_id: u16) -> bool {
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    matches!(product_id,
        // Mice
        CORSAIR_DARK_CORE_RGB_PRO | CORSAIR_IRONCLAW_RGB |
        CORSAIR_GLAIVE_RGB | CORSAIR_NIGHTSWORD_RGB |
        // Keyboards
        CORSAIR_K70_RGB_MK2 | CORSAIR_K95_RGB_PLATINUM |
        CORSAIR_K60_RGB_PRO | CORSAIR_K65_RGB_MINI
    )
}

/// Verificar si un product_id de SteelSeries es gaming
fn is_steelseries_gaming(product_id: u16) -> bool {
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    matches!(product_id,
        // Mice
        STEELSERIES_RIVAL_3 | STEELSERIES_RIVAL_5 |
        STEELSERIES_RIVAL_600 | STEELSERIES_SENSEI_310 |
        // Keyboards
        STEELSERIES_APEX_PRO | STEELSERIES_APEX_7 | STEELSERIES_APEX_3
    )
}

/// Verificar si un product_id de Roccat es gaming
fn is_roccat_gaming(product_id: u16) -> bool {
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    matches!(product_id,
        // Mice
        ROCCAT_KONE_AIMO | ROCCAT_KONE_PRO | ROCCAT_BURST_PRO |
        // Keyboards
        ROCCAT_VULCAN_TKL_PRO | ROCCAT_VULCAN_120_AIMO | ROCCAT_PYRO
    )
}

/// Tipo de dispositivo de entrada
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputDeviceType {
    Mouse,
    Keyboard,
}

/// Determinar el tipo de dispositivo basado en product_id
fn get_device_type(vendor_id: u16, product_id: u16) -> Option<InputDeviceType> {
    use vendors::*;
    use gaming_mice::*;
    use gaming_keyboards::*;
    
    match vendor_id {
        LOGITECH => {
            if matches!(product_id, LOGITECH_G502_HERO | LOGITECH_G502_LIGHTSPEED | 
                       LOGITECH_G_PRO_WIRELESS | LOGITECH_G703 | LOGITECH_G403 | 
                       LOGITECH_G603 | LOGITECH_G305) {
                Some(InputDeviceType::Mouse)
            } else if matches!(product_id, LOGITECH_G915_TKL | LOGITECH_G915 | 
                              LOGITECH_G513 | LOGITECH_G413 | LOGITECH_G213) {
                Some(InputDeviceType::Keyboard)
            } else {
                None
            }
        }
        RAZER => {
            if matches!(product_id, RAZER_DEATHADDER_V2 | RAZER_DEATHADDER_ELITE | 
                       RAZER_VIPER | RAZER_VIPER_ULTIMATE | RAZER_BASILISK_V2 | 
                       RAZER_NAGA_TRINITY) {
                Some(InputDeviceType::Mouse)
            } else if matches!(product_id, RAZER_BLACKWIDOW_V3 | RAZER_BLACKWIDOW_V3_TKL | 
                              RAZER_HUNTSMAN_ELITE | RAZER_HUNTSMAN_MINI | RAZER_CYNOSA_V2) {
                Some(InputDeviceType::Keyboard)
            } else {
                None
            }
        }
        CORSAIR => {
            if matches!(product_id, CORSAIR_DARK_CORE_RGB_PRO | CORSAIR_IRONCLAW_RGB | 
                       CORSAIR_GLAIVE_RGB | CORSAIR_NIGHTSWORD_RGB) {
                Some(InputDeviceType::Mouse)
            } else if matches!(product_id, CORSAIR_K70_RGB_MK2 | CORSAIR_K95_RGB_PLATINUM | 
                              CORSAIR_K60_RGB_PRO | CORSAIR_K65_RGB_MINI) {
                Some(InputDeviceType::Keyboard)
            } else {
                None
            }
        }
        STEELSERIES => {
            if matches!(product_id, STEELSERIES_RIVAL_3 | STEELSERIES_RIVAL_5 | 
                       STEELSERIES_RIVAL_600 | STEELSERIES_SENSEI_310) {
                Some(InputDeviceType::Mouse)
            } else if matches!(product_id, STEELSERIES_APEX_PRO | STEELSERIES_APEX_7 | 
                              STEELSERIES_APEX_3) {
                Some(InputDeviceType::Keyboard)
            } else {
                None
            }
        }
        ROCCAT => {
            if matches!(product_id, ROCCAT_KONE_AIMO | ROCCAT_KONE_PRO | ROCCAT_BURST_PRO) {
                Some(InputDeviceType::Mouse)
            } else if matches!(product_id, ROCCAT_VULCAN_TKL_PRO | ROCCAT_VULCAN_120_AIMO | 
                              ROCCAT_PYRO) {
                Some(InputDeviceType::Keyboard)
            } else {
                None
            }
        }
        _ => None
    }
}

/// Obtener capacidades de dispositivo gaming
pub fn get_gaming_capabilities(vendor_id: u16, product_id: u16) -> Option<GamingDeviceCapabilities> {
    if !is_gaming_device(vendor_id, product_id) {
        return None;
    }
    
    let device_type = get_device_type(vendor_id, product_id)?;
    
    // Configuraciones específicas por vendor y producto
    match (vendor_id, device_type) {
        (vendors::LOGITECH, InputDeviceType::Mouse) => Some(get_logitech_mouse_caps(product_id)),
        (vendors::LOGITECH, InputDeviceType::Keyboard) => Some(get_logitech_keyboard_caps(product_id)),
        (vendors::RAZER, InputDeviceType::Mouse) => Some(get_razer_mouse_caps(product_id)),
        (vendors::RAZER, InputDeviceType::Keyboard) => Some(get_razer_keyboard_caps(product_id)),
        (vendors::CORSAIR, InputDeviceType::Mouse) => Some(get_corsair_mouse_caps(product_id)),
        (vendors::CORSAIR, InputDeviceType::Keyboard) => Some(get_corsair_keyboard_caps(product_id)),
        (vendors::STEELSERIES, InputDeviceType::Mouse) => Some(get_steelseries_mouse_caps(product_id)),
        (vendors::STEELSERIES, InputDeviceType::Keyboard) => Some(get_steelseries_keyboard_caps(product_id)),
        (vendors::ROCCAT, InputDeviceType::Mouse) => Some(get_roccat_mouse_caps(product_id)),
        (vendors::ROCCAT, InputDeviceType::Keyboard) => Some(get_roccat_keyboard_caps(product_id)),
        _ => None
    }
}

// Logitech mouse capabilities
fn get_logitech_mouse_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_mice::*;
    
    let (max_dpi, extra_buttons) = match product_id {
        LOGITECH_G502_HERO => (25600, 11),        // G502 Hero: 25600 DPI, 11 botones
        LOGITECH_G502_LIGHTSPEED => (25600, 11),  // G502 Lightspeed: 25600 DPI, 11 botones
        LOGITECH_G_PRO_WIRELESS => (25600, 8),    // G Pro Wireless: 25600 DPI, 8 botones
        LOGITECH_G703 => (25600, 6),              // G703: 25600 DPI, 6 botones
        LOGITECH_G403 => (16000, 6),              // G403: 16000 DPI, 6 botones
        LOGITECH_G603 => (12000, 6),              // G603: 12000 DPI, 6 botones
        LOGITECH_G305 => (12000, 6),              // G305: 12000 DPI, 6 botones
        _ => (16000, 8),
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::LOGITECH,
        product_id,
        max_polling_rate: 1000,
        max_dpi,
        adjustable_dpi: true,
        extra_buttons,
        n_key_rollover: false,
        macro_keys: 0,
        rgb_support: true,
    }
}

// Logitech keyboard capabilities
fn get_logitech_keyboard_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_keyboards::*;
    
    let macro_keys = match product_id {
        LOGITECH_G915_TKL => 0,      // TKL no tiene macros
        LOGITECH_G915 => 5,           // G915: 5 teclas G
        LOGITECH_G513 => 0,           // G513: sin macros dedicadas
        LOGITECH_G413 => 0,           // G413: sin macros dedicadas
        LOGITECH_G213 => 0,           // G213: sin macros dedicadas
        _ => 0,
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::LOGITECH,
        product_id,
        max_polling_rate: 1000,
        max_dpi: 0,
        adjustable_dpi: false,
        extra_buttons: 0,
        n_key_rollover: true,
        macro_keys,
        rgb_support: true,
    }
}

// Razer mouse capabilities
fn get_razer_mouse_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_mice::*;
    
    let (max_dpi, extra_buttons) = match product_id {
        RAZER_DEATHADDER_V2 => (20000, 8),      // DeathAdder V2: 20000 DPI
        RAZER_DEATHADDER_ELITE => (16000, 7),   // DeathAdder Elite: 16000 DPI
        RAZER_VIPER => (16000, 8),              // Viper: 16000 DPI
        RAZER_VIPER_ULTIMATE => (20000, 8),     // Viper Ultimate: 20000 DPI
        RAZER_BASILISK_V2 => (20000, 11),       // Basilisk V2: 20000 DPI, 11 botones
        RAZER_NAGA_TRINITY => (16000, 19),      // Naga Trinity: 16000 DPI, 19 botones!
        _ => (16000, 8),
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::RAZER,
        product_id,
        max_polling_rate: 1000,
        max_dpi,
        adjustable_dpi: true,
        extra_buttons,
        n_key_rollover: false,
        macro_keys: 0,
        rgb_support: true,
    }
}

// Razer keyboard capabilities
fn get_razer_keyboard_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_keyboards::*;
    
    let macro_keys = match product_id {
        RAZER_BLACKWIDOW_V3 => 5,           // BlackWidow V3: 5 teclas macro
        RAZER_BLACKWIDOW_V3_TKL => 0,       // TKL: sin macros
        RAZER_HUNTSMAN_ELITE => 5,          // Huntsman Elite: 5 macros
        RAZER_HUNTSMAN_MINI => 0,           // Mini: sin macros (60%)
        RAZER_CYNOSA_V2 => 0,               // Cynosa V2: sin macros dedicadas
        _ => 0,
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::RAZER,
        product_id,
        max_polling_rate: 1000,
        max_dpi: 0,
        adjustable_dpi: false,
        extra_buttons: 0,
        n_key_rollover: true,
        macro_keys,
        rgb_support: true,
    }
}

// Corsair mouse capabilities
fn get_corsair_mouse_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_mice::*;
    
    let (max_dpi, extra_buttons) = match product_id {
        CORSAIR_DARK_CORE_RGB_PRO => (18000, 9),  // Dark Core: 18000 DPI, 9 botones
        CORSAIR_IRONCLAW_RGB => (18000, 7),       // Ironclaw: 18000 DPI, 7 botones
        CORSAIR_GLAIVE_RGB => (16000, 6),         // Glaive: 16000 DPI, 6 botones
        CORSAIR_NIGHTSWORD_RGB => (18000, 10),    // Nightsword: 18000 DPI, 10 botones
        _ => (16000, 8),
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::CORSAIR,
        product_id,
        max_polling_rate: 1000,
        max_dpi,
        adjustable_dpi: true,
        extra_buttons,
        n_key_rollover: false,
        macro_keys: 0,
        rgb_support: true,
    }
}

// Corsair keyboard capabilities
fn get_corsair_keyboard_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_keyboards::*;
    
    let macro_keys = match product_id {
        CORSAIR_K70_RGB_MK2 => 0,           // K70: sin macros dedicadas
        CORSAIR_K95_RGB_PLATINUM => 6,      // K95: 6 teclas macro
        CORSAIR_K60_RGB_PRO => 0,           // K60: sin macros
        CORSAIR_K65_RGB_MINI => 0,          // K65 Mini: sin macros (60%)
        _ => 0,
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::CORSAIR,
        product_id,
        max_polling_rate: 1000,
        max_dpi: 0,
        adjustable_dpi: false,
        extra_buttons: 0,
        n_key_rollover: true,
        macro_keys,
        rgb_support: true,
    }
}

// SteelSeries mouse capabilities
fn get_steelseries_mouse_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_mice::*;
    
    let (max_dpi, extra_buttons) = match product_id {
        STEELSERIES_RIVAL_3 => (8500, 6),       // Rival 3: 8500 DPI
        STEELSERIES_RIVAL_5 => (18000, 9),      // Rival 5: 18000 DPI, 9 botones
        STEELSERIES_RIVAL_600 => (12000, 7),    // Rival 600: 12000 DPI
        STEELSERIES_SENSEI_310 => (12000, 8),   // Sensei 310: 12000 DPI
        _ => (12000, 8),
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::STEELSERIES,
        product_id,
        max_polling_rate: 1000,
        max_dpi,
        adjustable_dpi: true,
        extra_buttons,
        n_key_rollover: false,
        macro_keys: 0,
        rgb_support: true,
    }
}

// SteelSeries keyboard capabilities
fn get_steelseries_keyboard_caps(product_id: u16) -> GamingDeviceCapabilities {
    GamingDeviceCapabilities {
        vendor_id: vendors::STEELSERIES,
        product_id,
        max_polling_rate: 1000,
        max_dpi: 0,
        adjustable_dpi: false,
        extra_buttons: 0,
        n_key_rollover: true,
        macro_keys: 0,  // SteelSeries usa todas las teclas sin macros dedicadas
        rgb_support: true,
    }
}

// Roccat mouse capabilities
fn get_roccat_mouse_caps(product_id: u16) -> GamingDeviceCapabilities {
    use gaming_mice::*;
    
    let (max_dpi, extra_buttons) = match product_id {
        ROCCAT_KONE_AIMO => (16000, 10),    // Kone Aimo: 16000 DPI, 10 botones
        ROCCAT_KONE_PRO => (19000, 5),      // Kone Pro: 19000 DPI
        ROCCAT_BURST_PRO => (16000, 5),     // Burst Pro: 16000 DPI
        _ => (16000, 8),
    };
    
    GamingDeviceCapabilities {
        vendor_id: vendors::ROCCAT,
        product_id,
        max_polling_rate: 1000,
        max_dpi,
        adjustable_dpi: true,
        extra_buttons,
        n_key_rollover: false,
        macro_keys: 0,
        rgb_support: true,
    }
}

// Roccat keyboard capabilities
fn get_roccat_keyboard_caps(product_id: u16) -> GamingDeviceCapabilities {
    GamingDeviceCapabilities {
        vendor_id: vendors::ROCCAT,
        product_id,
        max_polling_rate: 1000,
        max_dpi: 0,
        adjustable_dpi: false,
        extra_buttons: 0,
        n_key_rollover: true,
        macro_keys: 0,  // Roccat usa Easy-Shift+ sin macros dedicadas
        rgb_support: true,
    }
}

/// Información de controlador USB detectado
#[derive(Debug, Clone, Copy)]
pub struct UsbController {
    pub controller_type: UsbControllerType,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bar0: u32,
    pub interrupt_line: u8,
}

/// Tipos de controladores USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerType {
    UHCI,  // USB 1.1
    OHCI,  // USB 1.1 (alternativo)
    EHCI,  // USB 2.0
    XHCI,  // USB 3.0+
}

impl UsbControllerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbControllerType::UHCI => "UHCI (USB 1.1)",
            UsbControllerType::OHCI => "OHCI (USB 1.1)",
            UsbControllerType::EHCI => "EHCI (USB 2.0)",
            UsbControllerType::XHCI => "XHCI (USB 3.0+)",
        }
    }
}

/// Inicializar soporte USB HID.
/// Detecta controladores USB vía PCI y prepara estructuras para futura inicialización.
pub fn init() {
    crate::serial::serial_print("[USB-HID] Inicializando soporte USB HID...\n");
    
    // Detectar controladores USB vía PCI
    let usb_controllers = detect_usb_controllers();
    
    if usb_controllers.is_empty() {
        crate::serial::serial_print("[USB-HID] No se encontraron controladores USB\n");
        return;
    }
    
    crate::serial::serial_print("[USB-HID] Controladores USB detectados:\n");
    for controller in &usb_controllers {
        crate::serial::serial_print(&alloc::format!(
            "[USB-HID]   {} en {:02X}:{:02X}.{} (Vendor: 0x{:04X}, Device: 0x{:04X})\n",
            controller.controller_type.as_str(),
            controller.bus,
            controller.device,
            controller.function,
            controller.vendor_id,
            controller.device_id
        ));
        crate::serial::serial_print(&alloc::format!(
            "[USB-HID]     BAR0: 0x{:08X}, IRQ: {}\n",
            controller.bar0,
            controller.interrupt_line
        ));
    }
    
    // Contar por tipo
    let xhci_count = usb_controllers.iter().filter(|c| c.controller_type == UsbControllerType::XHCI).count();
    let ehci_count = usb_controllers.iter().filter(|c| c.controller_type == UsbControllerType::EHCI).count();
    let ohci_count = usb_controllers.iter().filter(|c| c.controller_type == UsbControllerType::OHCI).count();
    let uhci_count = usb_controllers.iter().filter(|c| c.controller_type == UsbControllerType::UHCI).count();
    
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] Total: {} controladores (XHCI: {}, EHCI: {}, OHCI: {}, UHCI: {})\n",
        usb_controllers.len(), xhci_count, ehci_count, ohci_count, uhci_count
    ));
    
    // TODO: Inicializar controladores XHCI/EHCI
    // TODO: Enumerar dispositivos HID conectados
    // TODO: Identificar periféricos gaming y configurar polling rate alto
    // TODO: Configurar buffers DMA para transferencias de alta frecuencia
    
    crate::serial::serial_print("[USB-HID] Detección completada (inicialización pendiente)\n");
}

/// Detectar controladores USB vía PCI
fn detect_usb_controllers() -> alloc::vec::Vec<UsbController> {
    let mut controllers = alloc::vec::Vec::new();
    
    // Buscar todos los controladores USB
    let pci_devices = crate::pci::find_usb_controllers();
    
    for pci_dev in pci_devices {
        let controller_type = match pci_dev.subclass {
            0x00 => UsbControllerType::UHCI,
            0x10 => UsbControllerType::OHCI,
            0x20 => UsbControllerType::EHCI,
            0x30 => UsbControllerType::XHCI,
            _ => continue,
        };
        
        let controller = UsbController {
            controller_type,
            bus: pci_dev.bus,
            device: pci_dev.device,
            function: pci_dev.function,
            vendor_id: pci_dev.vendor_id,
            device_id: pci_dev.device_id,
            bar0: pci_dev.bar0,
            interrupt_line: pci_dev.interrupt_line,
        };
        
        controllers.push(controller);
    }
    
    controllers
}
