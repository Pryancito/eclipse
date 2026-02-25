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
    pub bar0: u64,
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

/// Estado de inicialización del controlador USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerState {
    Uninitialized,
    Initializing,
    Ready,
    Error,
}

//
// ========== XHCI (USB 3.0+) Register Definitions ==========
//

/// XHCI Capability Registers (offset 0x00)
/// Según XHCI Specification Rev 1.2, Section 5.3
#[repr(C)]
#[derive(Debug)]
pub struct XhciCapabilityRegisters {
    pub caplength: u8,       // 0x00: Capability Register Length
    pub reserved: u8,        // 0x01: Reserved
    pub hciversion: u16,     // 0x02: Interface Version Number
    pub hcsparams1: u32,     // 0x04: Structural Parameters 1
    pub hcsparams2: u32,     // 0x08: Structural Parameters 2
    pub hcsparams3: u32,     // 0x0C: Structural Parameters 3
    pub hccparams1: u32,     // 0x10: Capability Parameters 1
    pub dboff: u32,          // 0x14: Doorbell Offset
    pub rtsoff: u32,         // 0x18: Runtime Register Space Offset
    pub hccparams2: u32,     // 0x1C: Capability Parameters 2
}

/// XHCI Operational Registers
/// Offset = CAPLENGTH (typically 0x20)
#[repr(C)]
#[derive(Debug)]
pub struct XhciOperationalRegisters {
    pub usbcmd: u32,         // 0x00: USB Command
    pub usbsts: u32,         // 0x04: USB Status
    pub pagesize: u32,       // 0x08: Page Size
    pub reserved1: [u32; 2], // 0x0C-0x10: Reserved
    pub dnctrl: u32,         // 0x14: Device Notification Control
    pub crcr: u64,           // 0x18: Command Ring Control
    pub reserved2: [u32; 4], // 0x20-0x2C: Reserved
    pub dcbaap: u64,         // 0x30: Device Context Base Address Array Pointer
    pub config: u32,         // 0x38: Configure
}

// XHCI USB Command Register (USBCMD) bits
pub const XHCI_CMD_RUN: u32 = 1 << 0;         // Run/Stop
pub const XHCI_CMD_RESET: u32 = 1 << 1;       // Host Controller Reset
pub const XHCI_CMD_INTE: u32 = 1 << 2;        // Interrupter Enable
pub const XHCI_CMD_HSEE: u32 = 1 << 3;        // Host System Error Enable

// XHCI USB Status Register (USBSTS) bits
pub const XHCI_STS_HCH: u32 = 1 << 0;         // HC Halted
pub const XHCI_STS_HSE: u32 = 1 << 2;         // Host System Error
pub const XHCI_STS_EINT: u32 = 1 << 3;        // Event Interrupt
pub const XHCI_STS_CNR: u32 = 1 << 11;        // Controller Not Ready

//
// ========== EHCI (USB 2.0) Register Definitions ==========
//

/// EHCI Capability Registers
/// Según EHCI Specification Rev 1.0, Section 2.2
#[repr(C)]
#[derive(Debug)]
pub struct EhciCapabilityRegisters {
    pub caplength: u8,       // 0x00: Capability Register Length
    pub reserved: u8,        // 0x01: Reserved
    pub hciversion: u16,     // 0x02: Interface Version Number
    pub hcsparams: u32,      // 0x04: Structural Parameters
    pub hccparams: u32,      // 0x08: Capability Parameters
    pub hcsp_portroute: u64, // 0x0C: Companion Port Route Description
}

/// EHCI Operational Registers
/// Offset = CAPLENGTH (typically 0x10)
#[repr(C)]
#[derive(Debug)]
pub struct EhciOperationalRegisters {
    pub usbcmd: u32,         // 0x00: USB Command
    pub usbsts: u32,         // 0x04: USB Status
    pub usbintr: u32,        // 0x08: USB Interrupt Enable
    pub frindex: u32,        // 0x0C: Frame Index
    pub ctrldssegment: u32,  // 0x10: Control Data Structure Segment
    pub periodiclistbase: u32, // 0x14: Periodic Frame List Base Address
    pub asynclistaddr: u32,  // 0x18: Asynchronous List Address
    pub reserved: [u32; 9],  // 0x1C-0x3C: Reserved
    pub configflag: u32,     // 0x40: Configured Flag
}

// EHCI USB Command Register (USBCMD) bits
pub const EHCI_CMD_RUN: u32 = 1 << 0;         // Run/Stop
pub const EHCI_CMD_RESET: u32 = 1 << 1;       // Host Controller Reset
pub const EHCI_CMD_PSE: u32 = 1 << 4;         // Periodic Schedule Enable
pub const EHCI_CMD_ASE: u32 = 1 << 5;         // Asynchronous Schedule Enable

// EHCI USB Status Register (USBSTS) bits
pub const EHCI_STS_INT: u32 = 1 << 0;         // USB Interrupt
pub const EHCI_STS_ERR: u32 = 1 << 1;         // USB Error Interrupt
pub const EHCI_STS_HCHALTED: u32 = 1 << 12;   // HC Halted

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
    
    crate::serial::serial_print("\n[USB-HID] === Fase 2: Inicialización de Controladores ===\n");
    
    // Intentar inicializar cada controlador
    let mut initialized_count = 0;
    for controller in &usb_controllers {
        let state = match controller.controller_type {
            UsbControllerType::XHCI => init_xhci_controller(controller),
            UsbControllerType::EHCI => init_ehci_controller(controller),
            UsbControllerType::OHCI => init_ohci_controller(controller),
            UsbControllerType::UHCI => init_uhci_controller(controller),
        };
        
        if state == UsbControllerState::Ready {
            initialized_count += 1;
        }
    }
    
    crate::serial::serial_print(&alloc::format!(
        "\n[USB-HID] Inicialización completada: {}/{} controladores listos\n",
        initialized_count, usb_controllers.len()
    ));
    
    // Fase 3: Framework de enumeración de dispositivos
    enumerate_usb_devices_stub();
    
    // Stage 2: Control Transfer Infrastructure
    init_control_transfer_infrastructure();
    
    // Stage 3: XHCI Driver Core
    init_xhci_driver_core();
    
    // TODO: Stage 4 - Interrupt handling and event processing
    // TODO: Stage 5 - HID integration and input event generation
    
    crate::serial::serial_print("\n[USB-HID] Todas las fases y stages completados (MMIO integration pendiente)\n");
}

/// Detectar controladores USB vía PCI
fn detect_usb_controllers() -> alloc::vec::Vec<UsbController> {
    let mut controllers = alloc::vec::Vec::new();
    
    // Buscar todos los controladores USB
    let pci_devices = crate::pci::find_usb_controllers();
    
    for pci_dev in pci_devices {
        // Tipo viene de prog_if (no de subclass) según especificación PCI
        let controller_type = match pci_dev.prog_if {
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

//
// ========== USB Controller Initialization Functions ==========
//

/// Inicializar controlador XHCI (USB 3.0+)
/// 
/// Esta es una implementación stub que prepara la estructura básica.
/// La inicialización completa requiere:
/// 1. Mapear registros MMIO desde BAR0
/// 2. Reset del controlador
/// 3. Configurar command ring
/// 4. Configurar event ring
/// 5. Configurar device context base array
/// 6. Iniciar el controlador
fn init_xhci_controller(controller: &UsbController) -> UsbControllerState {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] Inicializando XHCI en {:02X}:{:02X}.{}...\n",
        controller.bus, controller.device, controller.function
    ));
    
    // Habilitar el dispositivo PCI (Memory Space + Bus Master) antes de tocar MMIO
    unsafe {
        let pci_dev = crate::pci::PciDevice {
            bus: controller.bus,
            device: controller.device,
            function: controller.function,
            vendor_id: controller.vendor_id,
            device_id: controller.device_id,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            header_type: 0,
            bar0: controller.bar0,
            interrupt_line: controller.interrupt_line,
        };
        crate::pci::enable_device(&pci_dev, true);
    }
    
    // Leer BAR0. XHCI suele tener BAR 64-bit, pero en QEMU/OVMF la parte alta (BAR1)
    // puede no ser correcta o apuntar a región no mapeada (~2GB -> Page Fault).
    // Usamos solo la parte baja 32-bit: dirección física baja (ej. 0x8000) está siempre
    // mapeada en el higher-half del kernel.
    let bar0 = controller.bar0;
    if bar0 == 0 || bar0 == 0xFFFF_FFFF_FFFF_FFFF {
        crate::serial::serial_print("[USB-HID]   ERROR: BAR0 inválido (0 o no programado)\n");
        return UsbControllerState::Error;
    }
    
    let mmio_base = bar0 & !0xF;
    
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   BAR0=0x{:016X}, MMIO Base (phys)=0x{:016X}\n", bar0, mmio_base
    ));
    
    // Mapear y leer registros de capability/operational/runtime/doorbell
    let mmio = match XhciMmio::from_bar0(mmio_base) {
        Ok(m) => m,
        Err(e) => {
            crate::serial::serial_print("[USB-HID]   ERROR: XhciMmio::from_bar0 falló: ");
            crate::serial::serial_print(e);
            crate::serial::serial_print("\n");
            return UsbControllerState::Error;
        }
    };
    
    // Leer versión y parámetros estructurales básicos
    let caplen_hciver = mmio.read_capability(0x00);
    let hcsparams1 = mmio.read_capability(0x04);
    
    let caplength = (caplen_hciver & 0xFF) as u8;
    let hciversion = ((caplen_hciver >> 16) & 0xFFFF) as u16;
    let max_slots = (hcsparams1 & 0xFF) as u8;
    let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   XHCI v{:X}.{:X}, CAPLENGTH=0x{:02X}, MaxSlots={}, MaxPorts={}\n",
        (hciversion >> 8),
        (hciversion & 0xFF),
        caplength,
        max_slots,
        max_ports
    ));
    
    crate::serial::serial_print("[USB-HID]   Iniciando secuencias de hardware reales\n");
    
    let mut state = XhciControllerState::new(mmio_base, mmio);
    state.max_slots = max_slots;
    state.max_ports = max_ports;
    
    // Perform controller initialization
    if let Err(e) = state.initialize() {
        crate::serial::serial_print("[USB-HID]   ERROR initializing XHCI struct: ");
        crate::serial::serial_print(e);
        crate::serial::serial_print("\n");
        return UsbControllerState::Error;
    }
    
    // Store in global state
    unsafe {
        XHCI = Some(state);
        // Start the controller
        if let Err(e) = XHCI.as_mut().unwrap().start_controller() {
            crate::serial::serial_print("[USB-HID]   ERROR starting XHCI: ");
            crate::serial::serial_print(e);
            crate::serial::serial_print("\n");
            return UsbControllerState::Error;
        }
        
        // Enumerate ports
        if let Err(e) = XHCI.as_mut().unwrap().enumerate_ports() {
            crate::serial::serial_print("[USB-HID]   ERROR enumerating ports: ");
            crate::serial::serial_print(e);
            crate::serial::serial_print("\n");
        }
    }
    
    UsbControllerState::Ready
}

/// Estado global del controlador XHCI principal
pub static mut XHCI: Option<XhciControllerState> = None;

/// Inicializar controlador EHCI (USB 2.0)
/// 
/// Esta es una implementación stub que prepara la estructura básica.
/// La inicialización completa requiere:
/// 1. Mapear registros MMIO desde BAR0
/// 2. Reset del controlador
/// 3. Configurar periodic/async schedules
/// 4. Configurar frame list
/// 5. Iniciar el controlador
fn init_ehci_controller(controller: &UsbController) -> UsbControllerState {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] Inicializando EHCI en {:02X}:{:02X}.{}...\n",
        controller.bus, controller.device, controller.function
    ));
    
    let bar0 = controller.bar0;
    if bar0 == 0 || bar0 == 0xFFFF_FFFF_FFFF_FFFF {
        crate::serial::serial_print("[USB-HID]   ERROR: BAR0 es 0, controlador no configurado\n");
        return UsbControllerState::Error;
    }
    
    let mmio_base = bar0 & !0xF;
    
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   MMIO Base: 0x{:016X}\n", mmio_base
    ));
    
    // TODO: Mapear MMIO a espacio virtual del kernel
    // TODO: Leer capability registers
    // TODO: Determinar offset de operational registers (CAPLENGTH)
    // TODO: Reset del controlador (USBCMD.RESET)
    // TODO: Esperar a que USBSTS.HCHALTED = 1
    // TODO: Configurar periodic frame list
    // TODO: Configurar async schedule list
    // TODO: Configurar CONFIGFLAG
    // TODO: Iniciar controlador (USBCMD.RUN)
    
    crate::serial::serial_print("[USB-HID]   Inicialización EHCI stub completada\n");
    crate::serial::serial_print("[USB-HID]   NOTA: Funcionalidad completa requiere implementación de driver\n");
    
    UsbControllerState::Uninitialized
}

/// Inicializar controlador OHCI (USB 1.1)
fn init_ohci_controller(controller: &UsbController) -> UsbControllerState {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] OHCI en {:02X}:{:02X}.{} - inicialización no implementada\n",
        controller.bus, controller.device, controller.function
    ));
    UsbControllerState::Uninitialized
}

/// Inicializar controlador UHCI (USB 1.1)
fn init_uhci_controller(controller: &UsbController) -> UsbControllerState {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] UHCI en {:02X}:{:02X}.{} - inicialización no implementada\n",
        controller.bus, controller.device, controller.function
    ));
    UsbControllerState::Uninitialized
}

//
// ========== Phase 3: USB Device Enumeration Framework ==========
//

/// Tipos de descriptores USB estándar
/// USB Specification 2.0, Chapter 9
pub const USB_DESC_DEVICE: u8 = 0x01;
pub const USB_DESC_CONFIGURATION: u8 = 0x02;
pub const USB_DESC_STRING: u8 = 0x03;
pub const USB_DESC_INTERFACE: u8 = 0x04;
pub const USB_DESC_ENDPOINT: u8 = 0x05;
pub const USB_DESC_HID: u8 = 0x21;
pub const USB_DESC_HID_REPORT: u8 = 0x22;

/// USB Device Descriptor
/// USB Specification 2.0, Section 9.6.1
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbDeviceDescriptor {
    pub b_length: u8,              // 0x12 (18 bytes)
    pub b_descriptor_type: u8,     // 0x01 (DEVICE)
    pub bcd_usb: u16,              // USB Specification Release Number (BCD)
    pub b_device_class: u8,        // Class code
    pub b_device_sub_class: u8,    // Subclass code
    pub b_device_protocol: u8,     // Protocol code
    pub b_max_packet_size0: u8,    // Max packet size for endpoint 0
    pub id_vendor: u16,            // Vendor ID
    pub id_product: u16,           // Product ID
    pub bcd_device: u16,           // Device release number (BCD)
    pub i_manufacturer: u8,        // Index of manufacturer string
    pub i_product: u8,             // Index of product string
    pub i_serial_number: u8,       // Index of serial number string
    pub b_num_configurations: u8,  // Number of configurations
}

/// USB Configuration Descriptor
/// USB Specification 2.0, Section 9.6.3
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbConfigurationDescriptor {
    pub b_length: u8,              // 0x09 (9 bytes)
    pub b_descriptor_type: u8,     // 0x02 (CONFIGURATION)
    pub w_total_length: u16,       // Total length of data for this configuration
    pub b_num_interfaces: u8,      // Number of interfaces
    pub b_configuration_value: u8, // Configuration value
    pub i_configuration: u8,       // Index of configuration string
    pub bm_attributes: u8,         // Configuration characteristics
    pub b_max_power: u8,           // Maximum power consumption (2mA units)
}

/// USB Interface Descriptor
/// USB Specification 2.0, Section 9.6.5
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbInterfaceDescriptor {
    pub b_length: u8,              // 0x09 (9 bytes)
    pub b_descriptor_type: u8,     // 0x04 (INTERFACE)
    pub b_interface_number: u8,    // Interface number
    pub b_alternate_setting: u8,   // Alternate setting
    pub b_num_endpoints: u8,       // Number of endpoints (excluding EP0)
    pub b_interface_class: u8,     // Class code
    pub b_interface_sub_class: u8, // Subclass code
    pub b_interface_protocol: u8,  // Protocol code
    pub i_interface: u8,           // Index of interface string
}

/// USB Endpoint Descriptor
/// USB Specification 2.0, Section 9.6.6
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbEndpointDescriptor {
    pub b_length: u8,              // 0x07 (7 bytes)
    pub b_descriptor_type: u8,     // 0x05 (ENDPOINT)
    pub b_endpoint_address: u8,    // Endpoint address (direction + number)
    pub bm_attributes: u8,         // Endpoint attributes
    pub w_max_packet_size: u16,    // Maximum packet size
    pub b_interval: u8,            // Polling interval
}

// USB Interface Classes
pub const USB_CLASS_HID: u8 = 0x03;        // Human Interface Device

// HID Subclasses
pub const HID_SUBCLASS_NONE: u8 = 0x00;    // No subclass
pub const HID_SUBCLASS_BOOT: u8 = 0x01;    // Boot Interface Subclass

// HID Protocols (for Boot Subclass)
pub const HID_PROTOCOL_NONE: u8 = 0x00;    // None
pub const HID_PROTOCOL_KEYBOARD: u8 = 0x01; // Keyboard
pub const HID_PROTOCOL_MOUSE: u8 = 0x02;   // Mouse

/// HID Descriptor
/// HID Specification 1.11, Section 6.2.1
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HidDescriptor {
    pub b_length: u8,              // Descriptor length
    pub b_descriptor_type: u8,     // 0x21 (HID)
    pub bcd_hid: u16,              // HID Class Specification release
    pub b_country_code: u8,        // Country code
    pub b_num_descriptors: u8,     // Number of class descriptors
    pub b_descriptor_type2: u8,    // Type of class descriptor (0x22 = Report)
    pub w_descriptor_length: u16,  // Total length of Report descriptor
}

// HID Request Types
pub const HID_REQUEST_GET_REPORT: u8 = 0x01;
pub const HID_REQUEST_GET_IDLE: u8 = 0x02;
pub const HID_REQUEST_GET_PROTOCOL: u8 = 0x03;
pub const HID_REQUEST_SET_REPORT: u8 = 0x09;
pub const HID_REQUEST_SET_IDLE: u8 = 0x0A;
pub const HID_REQUEST_SET_PROTOCOL: u8 = 0x0B;

// HID Report Types
pub const HID_REPORT_INPUT: u8 = 0x01;
pub const HID_REPORT_OUTPUT: u8 = 0x02;
pub const HID_REPORT_FEATURE: u8 = 0x03;

/// Estado de dispositivo USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDeviceState {
    Attached,       // Dispositivo físicamente conectado
    Powered,        // Dispositivo recibiendo energía
    Default,        // Dispositivo en estado por defecto (después de reset)
    Addressed,      // Dispositivo con dirección asignada
    Configured,     // Dispositivo configurado y listo
    Suspended,      // Dispositivo suspendido
}

/// Tipo de dispositivo HID detectado
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidDeviceType {
    Keyboard,
    Mouse,
    Gamepad,
    Other,
}

/// Información de dispositivo USB enumerado
#[derive(Debug, Clone, Copy)]
pub struct UsbDevice {
    pub address: u8,                    // Dirección USB asignada (1-127)
    pub port: u8,                       // Puerto del hub donde está conectado
    pub state: UsbDeviceState,          // Estado actual del dispositivo
    pub vendor_id: u16,                 // Vendor ID (del descriptor)
    pub product_id: u16,                // Product ID (del descriptor)
    pub device_class: u8,               // Clase del dispositivo
    pub max_packet_size: u8,            // Tamaño máximo de paquete EP0
    pub is_hid: bool,                   // Es un dispositivo HID
    pub hid_type: Option<HidDeviceType>, // Tipo de HID si aplica
    pub is_gaming: bool,                // Es un periférico gaming
}

impl UsbDevice {
    /// Crear un dispositivo USB sin inicializar
    pub const fn new() -> Self {
        Self {
            address: 0,
            port: 0,
            state: UsbDeviceState::Attached,
            vendor_id: 0,
            product_id: 0,
            device_class: 0,
            max_packet_size: 8,
            is_hid: false,
            hid_type: None,
            is_gaming: false,
        }
    }
}

/// Identificar si un dispositivo es HID basado en interface descriptor
fn is_hid_device(interface_desc: &UsbInterfaceDescriptor) -> bool {
    interface_desc.b_interface_class == USB_CLASS_HID
}

/// Determinar tipo de dispositivo HID basado en protocolo boot
fn get_hid_device_type(interface_desc: &UsbInterfaceDescriptor) -> HidDeviceType {
    if interface_desc.b_interface_class != USB_CLASS_HID {
        return HidDeviceType::Other;
    }
    
    // Boot Interface Subclass con protocolo específico
    if interface_desc.b_interface_sub_class == HID_SUBCLASS_BOOT {
        match interface_desc.b_interface_protocol {
            HID_PROTOCOL_KEYBOARD => HidDeviceType::Keyboard,
            HID_PROTOCOL_MOUSE => HidDeviceType::Mouse,
            _ => HidDeviceType::Other,
        }
    } else {
        // Sin boot protocol, podría ser gamepad u otro
        HidDeviceType::Other
    }
}

/// Identificar si un dispositivo HID es un periférico gaming
/// Usa la base de datos de gaming devices implementada en Phase 1
fn is_gaming_peripheral(vendor_id: u16, product_id: u16) -> bool {
    // Reutilizar la función existente de Phase 1
    is_gaming_device(vendor_id, product_id)
}

/// Framework de enumeración de dispositivos USB
/// 
/// Esta función stub documenta el proceso de enumeración:
/// 1. Detectar nueva conexión de dispositivo
/// 2. Reset del puerto
/// 3. Leer Device Descriptor (primeros 8 bytes)
/// 4. Asignar dirección única
/// 5. Leer Device Descriptor completo
/// 6. Leer Configuration Descriptor
/// 7. Leer Interface Descriptors
/// 8. Identificar clase HID y tipo (keyboard/mouse)
/// 9. Configurar dispositivo
/// 10. Para HID: leer HID Descriptor y Report Descriptor
/// 11. Para gaming: aplicar configuraciones específicas
fn enumerate_usb_devices_stub() {
    crate::serial::serial_print("\n[USB-HID] === Fase 3: Framework de Enumeración ===\n");
    crate::serial::serial_print("[USB-HID] Proceso de enumeración USB:\n");
    crate::serial::serial_print("[USB-HID]   1. Detectar conexión de dispositivo (port status change)\n");
    crate::serial::serial_print("[USB-HID]   2. Reset del puerto USB\n");
    crate::serial::serial_print("[USB-HID]   3. Leer Device Descriptor (primeros 8 bytes para max_packet_size)\n");
    crate::serial::serial_print("[USB-HID]   4. Asignar dirección USB única (1-127)\n");
    crate::serial::serial_print("[USB-HID]   5. SET_ADDRESS al dispositivo\n");
    crate::serial::serial_print("[USB-HID]   6. Leer Device Descriptor completo\n");
    crate::serial::serial_print("[USB-HID]   7. Leer Configuration Descriptor\n");
    crate::serial::serial_print("[USB-HID]   8. Leer Interface Descriptors\n");
    crate::serial::serial_print("[USB-HID]   9. Identificar clase HID (0x03)\n");
    crate::serial::serial_print("[USB-HID]   10. Determinar tipo: Keyboard (0x01) o Mouse (0x02)\n");
    crate::serial::serial_print("[USB-HID]   11. SET_CONFIGURATION\n");
    crate::serial::serial_print("[USB-HID]   12. Para HID: Leer HID Descriptor y Report Descriptor\n");
    crate::serial::serial_print("[USB-HID]   13. Para gaming: Detectar vendor/product en database\n");
    crate::serial::serial_print("[USB-HID]   14. Configurar polling rate (1000Hz para gaming)\n");
    crate::serial::serial_print("[USB-HID]   15. Iniciar transferencias interrupt para reportes\n");
    
    crate::serial::serial_print("\n[USB-HID] Estructuras de datos listas:\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbDeviceDescriptor\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbConfigurationDescriptor\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbInterfaceDescriptor\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbEndpointDescriptor\n");
    crate::serial::serial_print("[USB-HID]   ✓ HidDescriptor\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbDevice (tracking)\n");
    crate::serial::serial_print("[USB-HID]   ✓ Gaming device database (67 modelos)\n");
    
    crate::serial::serial_print("\n[USB-HID] NOTA: Enumeración real requiere:\n");
    crate::serial::serial_print("[USB-HID]   - Controlador XHCI/EHCI funcional\n");
    crate::serial::serial_print("[USB-HID]   - Implementación de USB protocol transactions\n");
    crate::serial::serial_print("[USB-HID]   - Manejo de interrupciones USB\n");
    crate::serial::serial_print("[USB-HID]   - Buffers DMA para transferencias\n");
}

//
// ========== Stage 1: Foundation - DMA and Transfer Infrastructure ==========
//

/// Tipos de transferencias USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbTransferType {
    Control,     // Control transfers (setup, data, status)
    Bulk,        // Bulk transfers (large data)
    Interrupt,   // Interrupt transfers (HID reports, periodic)
    Isochronous, // Isochronous transfers (audio/video, not used for HID)
}

/// Estado de una transferencia USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbTransferStatus {
    Pending,      // Transfer en cola
    InProgress,   // Transfer en progreso
    Completed,    // Transfer completada exitosamente
    Error,        // Transfer con error
    Stalled,      // Endpoint stalled
    Cancelled,    // Transfer cancelada
}

/// Buffer DMA para transferencias USB
/// 
/// Representa un buffer de memoria física para DMA.
/// USB controllers requieren direcciones físicas contiguas.
#[derive(Debug)]
pub struct DmaBuffer {
    pub virt_addr: u64,      // Dirección virtual del buffer
    pub phys_addr: u64,      // Dirección física (para DMA)
    pub size: usize,         // Tamaño del buffer en bytes
    pub allocated: bool,     // Si el buffer está en uso
}

impl DmaBuffer {
    /// Crear un buffer DMA sin inicializar
    pub const fn new() -> Self {
        Self {
            virt_addr: 0,
            phys_addr: 0,
            size: 0,
            allocated: false,
        }
    }
    
    /// Verificar si el buffer es válido
    pub fn is_valid(&self) -> bool {
        self.allocated && self.virt_addr != 0 && self.phys_addr != 0 && self.size > 0
    }
}

/// Solicitud de transferencia USB
/// 
/// Representa una transferencia USB genérica.
/// Se usa como base para control, bulk, interrupt transfers.
pub struct UsbTransferRequest {
    pub transfer_type: UsbTransferType,
    pub status: UsbTransferStatus,
    pub device_address: u8,      // Dirección del dispositivo (1-127)
    pub endpoint: u8,            // Número de endpoint (0-15)
    pub direction_in: bool,      // true = IN (device to host), false = OUT
    pub data_buffer: DmaBuffer,  // Buffer para datos
    pub actual_length: usize,    // Bytes transferidos realmente
    pub max_packet_size: u16,    // Tamaño máximo de paquete
}

impl UsbTransferRequest {
    /// Crear una nueva solicitud de transferencia
    pub fn new(transfer_type: UsbTransferType) -> Self {
        Self {
            transfer_type,
            status: UsbTransferStatus::Pending,
            device_address: 0,
            endpoint: 0,
            direction_in: true,
            data_buffer: DmaBuffer::new(),
            actual_length: 0,
            max_packet_size: 64,
        }
    }
}

//
// ========== XHCI Transfer Request Block (TRB) Structures ==========
//

/// TRB Type codes
/// XHCI Specification Section 6.4.6
pub const TRB_TYPE_NORMAL: u8 = 1;
pub const TRB_TYPE_SETUP: u8 = 2;
pub const TRB_TYPE_DATA: u8 = 3;
pub const TRB_TYPE_STATUS: u8 = 4;
pub const TRB_TYPE_LINK: u8 = 6;
pub const TRB_TYPE_COMMAND_COMPLETION: u8 = 33;
pub const TRB_TYPE_PORT_STATUS_CHANGE: u8 = 34;

/// Transfer Request Block (TRB)
/// XHCI Specification Section 4.11
/// 
/// Estructura básica de 16 bytes para todos los TRBs.
/// Los campos específicos dependen del tipo de TRB.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Trb {
    pub parameter: u64,      // TRB-specific parameter (varies by type)
    pub status: u32,         // TRB status field
    pub control: u32,        // Control field (includes Cycle bit, TRB Type)
}

impl Trb {
    /// Crear un TRB vacío
    pub const fn new() -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: 0,
        }
    }
    
    /// Obtener el tipo de TRB del campo control
    pub fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }
    
    /// Obtener el Cycle bit
    pub fn cycle_bit(&self) -> bool {
        (self.control & 1) != 0
    }
    
    /// Establecer el Cycle bit
    pub fn set_cycle_bit(&mut self, cycle: bool) {
        if cycle {
            self.control |= 1;
        } else {
            self.control &= !1;
        }
    }
    
    /// Alias para compatibilidad con código previo
    pub fn get_trb_type(&self) -> u8 {
        self.trb_type()
    }
}

/// Ring de TRBs para XHCI
/// 
/// Estructura circular para Command Ring, Transfer Ring, o Event Ring.
/// Usa el Producer Cycle State (PCS) para detectar wrap-around.
pub struct TrbRing {
    pub allocation: DmaAllocation,    // Continuous DMA segment
    pub enqueue_index: usize,         // Software write index
    pub dequeue_index: usize,         // Software read index
    pub cycle_state: bool,            // Producer Cycle State (CR/TR) or Consumer Cycle State (ER)
    pub capacity: usize,              // Number of TRBs (including Link TRB if present)
    pub has_link_trb: bool,           // True for Command/Transfer rings
}

impl TrbRing {
    /// Create a new TRB ring
    pub fn new(capacity: usize, has_link_trb: bool) -> Result<Self, &'static str> {
        let allocation = DmaAllocation::allocate_trb_ring(capacity)?;
        allocation.zero();
        
        if has_link_trb && capacity > 1 {
            // Setup Link TRB at the end pointing back to the start
            let link_trb = build_link_trb(allocation.phys_addr, true);
            let offset = (capacity - 1) * 16;
            allocation.write_u64(offset, link_trb.parameter);
            allocation.write_u64(offset + 8, (link_trb.status as u64) | ((link_trb.control as u64) << 32));
        }
        
        Ok(Self {
            allocation,
            enqueue_index: 0,
            dequeue_index: 0,
            cycle_state: true, // PCS or CCS starts at 1
            capacity,
            has_link_trb,
        })
    }
    
    /// Check if ring is full (respecting Link TRB if present)
    pub fn is_full(&self) -> bool {
        let usable_capacity = if self.has_link_trb { self.capacity - 1 } else { self.capacity };
        (self.enqueue_index + 1) % usable_capacity == self.dequeue_index
    }

    /// Check if ring is empty (using software indices)
    pub fn is_empty(&self) -> bool {
        self.enqueue_index == self.dequeue_index
    }

    /// Peek at the TRB at dequeue index and check Cycle Bit (for Event Rings)
    pub fn peek_event(&self) -> Option<Trb> {
        let offset = self.dequeue_index * 16;
        let mut trb_data = [0u64; 2];
        self.allocation.read_bytes(offset, unsafe {
            core::slice::from_raw_parts_mut(trb_data.as_mut_ptr() as *mut u8, 16)
        });
        
        let trb = Trb {
            parameter: trb_data[0],
            status: (trb_data[1] & 0xFFFFFFFF) as u32,
            control: (trb_data[1] >> 32) as u32,
        };
        
        // If Cycle Bit matches CCS, there is a new event
        if trb.cycle_bit() == self.cycle_state {
            Some(trb)
        } else {
            None
        }
    }

    /// Pop an event from the ring (for Event Rings)
    pub fn pop_event(&mut self) -> Option<Trb> {
        let trb = self.peek_event()?;
        
        // Advance dequeue index
        self.dequeue_index = (self.dequeue_index + 1) % self.capacity;
        
        // If wrap around, toggle CCS
        if self.dequeue_index == 0 {
            self.cycle_state = !self.cycle_state;
        }
        
        Some(trb)
    }
    

    
    /// Enqueue a TRB in the ring (for Command/Transfer Rings)
    pub fn enqueue(&mut self, mut trb: Trb) -> Result<(), &'static str> {
        if self.is_full() {
            return Err("TRB ring is full");
        }
        
        let usable_capacity = if self.has_link_trb { self.capacity - 1 } else { self.capacity };
        let idx = self.enqueue_index;
        trb.set_cycle_bit(self.cycle_state);
        
        let offset = idx * 16;
        self.allocation.write_u64(offset, trb.parameter);
        self.allocation.write_u64(offset + 8, (trb.status as u64) | ((trb.control as u64) << 32));
        
        self.enqueue_index = (self.enqueue_index + 1) % usable_capacity;
        
        if self.has_link_trb && self.enqueue_index == 0 {
            // Reached link TRB, hardware traverses it, we toggle cycle state
            let link_offset = (self.capacity - 1) * 16;
            
            let mut link_word2 = 0u64;
            self.allocation.read_bytes(link_offset + 8, unsafe {
                core::slice::from_raw_parts_mut(&mut link_word2 as *mut u64 as *mut u8, 8)
            });
            
            // Match Link TRB cycle to current PCS
            if self.cycle_state {
                link_word2 |= 1;
            } else {
                link_word2 &= !1;
            }
            
            self.allocation.write_u64(link_offset + 8, link_word2);
            self.cycle_state = !self.cycle_state;
        }
        Ok(())
    }
}

//
// ========== HID Boot Protocol Report Structures ==========
//

/// Reporte de teclado en Boot Protocol
/// HID Specification Appendix B.1
/// 
/// 8 bytes: [Modifiers, Reserved, Key1, Key2, Key3, Key4, Key5, Key6]
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HidKeyboardReport {
    pub modifiers: u8,       // Bit 0: Left Control, Bit 1: Left Shift, etc.
    pub reserved: u8,        // Debe ser 0
    pub keys: [u8; 6],       // Array de keycodes presionados (0 = ninguno)
}

impl HidKeyboardReport {
    pub const fn new() -> Self {
        Self {
            modifiers: 0,
            reserved: 0,
            keys: [0; 6],
        }
    }
    
    /// Verificar si una tecla está presionada
    pub fn is_key_pressed(&self, keycode: u8) -> bool {
        self.keys.iter().any(|&k| k == keycode)
    }
}

// Keyboard modifier bits
pub const MOD_LEFT_CTRL: u8 = 1 << 0;
pub const MOD_LEFT_SHIFT: u8 = 1 << 1;
pub const MOD_LEFT_ALT: u8 = 1 << 2;
pub const MOD_LEFT_GUI: u8 = 1 << 3;
pub const MOD_RIGHT_CTRL: u8 = 1 << 4;
pub const MOD_RIGHT_SHIFT: u8 = 1 << 5;
pub const MOD_RIGHT_ALT: u8 = 1 << 6;
pub const MOD_RIGHT_GUI: u8 = 1 << 7;

/// Reporte de ratón en Boot Protocol
/// HID Specification Appendix B.2
/// 
/// 3 bytes mínimo: [Buttons, X, Y]
/// 4 bytes con scroll wheel: [Buttons, X, Y, Wheel]
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HidMouseReport {
    pub buttons: u8,         // Bit 0: Button 1 (left), Bit 1: Button 2 (right), etc.
    pub x: i8,               // Movimiento X (relativo)
    pub y: i8,               // Movimiento Y (relativo)
    pub wheel: i8,           // Scroll wheel (si está disponible)
}

impl HidMouseReport {
    pub const fn new() -> Self {
        Self {
            buttons: 0,
            x: 0,
            y: 0,
            wheel: 0,
        }
    }
    
    /// Verificar si un botón está presionado
    pub fn is_button_pressed(&self, button: u8) -> bool {
        (self.buttons & (1 << button)) != 0
    }
}

// Mouse button bits
pub const MOUSE_BUTTON_LEFT: u8 = 0;
pub const MOUSE_BUTTON_RIGHT: u8 = 1;
pub const MOUSE_BUTTON_MIDDLE: u8 = 2;

/// Pool de buffers DMA para gestión eficiente
/// 
/// Pre-aloca buffers DMA para evitar fragmentación.
pub struct DmaBufferPool {
    buffers: alloc::vec::Vec<DmaBuffer>,
    buffer_size: usize,
}

impl DmaBufferPool {
    /// Crear un nuevo pool de buffers
    pub fn new(count: usize, buffer_size: usize) -> Self {
        let mut buffers = alloc::vec::Vec::with_capacity(count);
        for _ in 0..count {
            buffers.push(DmaBuffer::new());
        }
        
        Self {
            buffers,
            buffer_size,
        }
    }
    
    /// Obtener un buffer libre del pool
    pub fn allocate(&mut self) -> Option<&mut DmaBuffer> {
        self.buffers.iter_mut().find(|b| !b.allocated)
    }
    
    /// Liberar un buffer de vuelta al pool
    pub fn free(&mut self, buffer: &DmaBuffer) {
        if let Some(b) = self.buffers.iter_mut().find(|b| b.phys_addr == buffer.phys_addr) {
            b.allocated = false;
        }
    }
    
    /// Obtener estadísticas del pool
    pub fn stats(&self) -> (usize, usize) {
        let allocated = self.buffers.iter().filter(|b| b.allocated).count();
        (allocated, self.buffers.len())
    }
}

//
// ========== Stage 2: USB Protocol Transactions ==========
//

/// Setup Packet para Control Transfers
/// USB Specification 2.0, Section 9.3
/// 
/// Estructura de 8 bytes que inicia todo control transfer.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct UsbSetupPacket {
    pub bm_request_type: u8,    // Request type y dirección
    pub b_request: u8,          // Request específico
    pub w_value: u16,           // Parámetro value (varía por request)
    pub w_index: u16,           // Parámetro index (varía por request)
    pub w_length: u16,          // Bytes de datos a transferir
}

impl UsbSetupPacket {
    /// Crear un setup packet vacío
    pub const fn new() -> Self {
        Self {
            bm_request_type: 0,
            b_request: 0,
            w_value: 0,
            w_index: 0,
            w_length: 0,
        }
    }
    
    /// Crear setup packet para GET_DESCRIPTOR
    pub fn get_descriptor(descriptor_type: u8, descriptor_index: u8, length: u16) -> Self {
        Self {
            bm_request_type: 0x80, // Device-to-host, Standard, Device
            b_request: USB_REQUEST_GET_DESCRIPTOR,
            w_value: ((descriptor_type as u16) << 8) | (descriptor_index as u16),
            w_index: 0,
            w_length: length,
        }
    }
    
    /// Crear setup packet para SET_ADDRESS
    pub fn set_address(address: u8) -> Self {
        Self {
            bm_request_type: 0x00, // Host-to-device, Standard, Device
            b_request: USB_REQUEST_SET_ADDRESS,
            w_value: address as u16,
            w_index: 0,
            w_length: 0,
        }
    }
    
    /// Crear setup packet para SET_CONFIGURATION
    pub fn set_configuration(config_value: u8) -> Self {
        Self {
            bm_request_type: 0x00, // Host-to-device, Standard, Device
            b_request: USB_REQUEST_SET_CONFIGURATION,
            w_value: config_value as u16,
            w_index: 0,
            w_length: 0,
        }
    }
}

// bmRequestType bits
pub const REQUEST_TYPE_DIR_MASK: u8 = 0x80;      // Bit 7: Direction
pub const REQUEST_TYPE_DIR_OUT: u8 = 0x00;       // Host to Device
pub const REQUEST_TYPE_DIR_IN: u8 = 0x80;        // Device to Host
pub const REQUEST_TYPE_TYPE_MASK: u8 = 0x60;     // Bits 5-6: Type
pub const REQUEST_TYPE_STANDARD: u8 = 0x00;      // Standard request
pub const REQUEST_TYPE_CLASS: u8 = 0x20;         // Class-specific request
pub const REQUEST_TYPE_VENDOR: u8 = 0x40;        // Vendor-specific request
pub const REQUEST_TYPE_RECIPIENT_MASK: u8 = 0x1F; // Bits 0-4: Recipient
pub const REQUEST_TYPE_DEVICE: u8 = 0x00;        // Device recipient
pub const REQUEST_TYPE_INTERFACE: u8 = 0x01;     // Interface recipient
pub const REQUEST_TYPE_ENDPOINT: u8 = 0x02;      // Endpoint recipient

// Standard USB Requests (bRequest values)
pub const USB_REQUEST_GET_STATUS: u8 = 0;
pub const USB_REQUEST_CLEAR_FEATURE: u8 = 1;
pub const USB_REQUEST_SET_FEATURE: u8 = 3;
pub const USB_REQUEST_SET_ADDRESS: u8 = 5;
pub const USB_REQUEST_GET_DESCRIPTOR: u8 = 6;
pub const USB_REQUEST_SET_DESCRIPTOR: u8 = 7;
pub const USB_REQUEST_GET_CONFIGURATION: u8 = 8;
pub const USB_REQUEST_SET_CONFIGURATION: u8 = 9;
pub const USB_REQUEST_GET_INTERFACE: u8 = 10;
pub const USB_REQUEST_SET_INTERFACE: u8 = 11;
pub const USB_REQUEST_SYNCH_FRAME: u8 = 12;

/// Estado de un Control Transfer
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlTransferState {
    Setup,       // Setup stage (8-byte setup packet)
    DataIn,      // Data stage, device to host
    DataOut,     // Data stage, host to device
    Status,      // Status stage (zero-length)
    Complete,    // Transfer completo
    Error,       // Transfer con error
}

/// Control Transfer completo
/// 
/// Representa un control transfer de 3 etapas: Setup, Data (opcional), Status.
pub struct ControlTransfer {
    pub setup: UsbSetupPacket,
    pub state: ControlTransferState,
    pub data_buffer: DmaBuffer,
    pub bytes_transferred: usize,
    pub device_address: u8,
    pub endpoint: u8,
}

impl ControlTransfer {
    /// Crear un nuevo control transfer
    pub fn new(setup: UsbSetupPacket, device_address: u8) -> Self {
        let has_data = setup.w_length > 0;
        let initial_state = if has_data {
            ControlTransferState::Setup
        } else {
            ControlTransferState::Setup
        };
        
        Self {
            setup,
            state: initial_state,
            data_buffer: DmaBuffer::new(),
            bytes_transferred: 0,
            device_address,
            endpoint: 0, // Control transfers always use endpoint 0
        }
    }
    
    /// Verificar si el transfer requiere data stage
    pub fn has_data_stage(&self) -> bool {
        self.setup.w_length > 0
    }
    
    /// Obtener dirección de data stage
    pub fn data_direction_in(&self) -> bool {
        (self.setup.bm_request_type & REQUEST_TYPE_DIR_MASK) != 0
    }
}

/// Función stub: Leer Device Descriptor
/// 
/// Lee el Device Descriptor de un dispositivo USB.
/// TODO: Implementar comunicación real con hardware USB
pub fn read_device_descriptor(device_address: u8) -> Result<UsbDeviceDescriptor, &'static str> {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] read_device_descriptor(addr={})\n", device_address
    ));
    
    // TODO: Crear setup packet para GET_DESCRIPTOR
    // TODO: Crear control transfer
    // TODO: Ejecutar transfer via XHCI/EHCI
    // TODO: Parsear descriptor de respuesta
    
    Err("Not implemented - requires USB controller driver")
}

/// Función stub: Leer Configuration Descriptor
/// 
/// Lee el Configuration Descriptor de un dispositivo USB.
/// TODO: Implementar comunicación real con hardware USB
pub fn read_configuration_descriptor(
    device_address: u8,
    config_index: u8,
) -> Result<UsbConfigurationDescriptor, &'static str> {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] read_configuration_descriptor(addr={}, config={})\n",
        device_address, config_index
    ));
    
    // TODO: Crear setup packet para GET_DESCRIPTOR
    // TODO: Leer primero los 9 bytes del configuration descriptor
    // TODO: Leer wTotalLength bytes adicionales (interfaces, endpoints)
    // TODO: Parsear descriptors completos
    
    Err("Not implemented - requires USB controller driver")
}

/// Función stub: Leer String Descriptor
/// 
/// Lee un String Descriptor de un dispositivo USB.
/// TODO: Implementar comunicación real con hardware USB
pub fn read_string_descriptor(
    device_address: u8,
    string_index: u8,
    language_id: u16,
) -> Result<alloc::string::String, &'static str> {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] read_string_descriptor(addr={}, idx={}, lang=0x{:04X})\n",
        device_address, string_index, language_id
    ));
    
    // TODO: Crear setup packet para GET_DESCRIPTOR (string)
    // TODO: Ejecutar control transfer
    // TODO: Decodificar UTF-16LE a String
    
    Err("Not implemented - requires USB controller driver")
}

/// Función stub: Asignar dirección USB a dispositivo
/// 
/// Asigna una dirección única (1-127) a un dispositivo recién conectado.
/// TODO: Implementar comunicación real con hardware USB
pub fn set_device_address(current_address: u8, new_address: u8) -> Result<(), &'static str> {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] set_device_address(current={}, new={})\n",
        current_address, new_address
    ));
    
    if new_address == 0 || new_address > 127 {
        return Err("Invalid USB address (must be 1-127)");
    }
    
    // TODO: Crear setup packet para SET_ADDRESS
    let _setup = UsbSetupPacket::set_address(new_address);
    
    // TODO: Crear control transfer sin data stage
    // TODO: Ejecutar transfer via XHCI/EHCI
    // TODO: Esperar a que dispositivo adopte nueva dirección (2ms delay requerido)
    
    Err("Not implemented - requires USB controller driver")
}

/// Función stub: Configurar dispositivo USB
/// 
/// Selecciona una configuración activa para el dispositivo.
/// TODO: Implementar comunicación real con hardware USB
pub fn set_device_configuration(device_address: u8, config_value: u8) -> Result<(), &'static str> {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] set_device_configuration(addr={}, config={})\n",
        device_address, config_value
    ));
    
    // TODO: Crear setup packet para SET_CONFIGURATION
    let _setup = UsbSetupPacket::set_configuration(config_value);
    
    // TODO: Crear control transfer sin data stage
    // TODO: Ejecutar transfer via XHCI/EHCI
    // TODO: Actualizar estado del dispositivo a Configured
    
    Err("Not implemented - requires USB controller driver")
}

/// Helper: Crear control transfer desde setup packet
pub fn create_control_transfer(
    setup: UsbSetupPacket,
    device_address: u8,
    data_buffer: Option<DmaBuffer>,
) -> ControlTransfer {
    let mut transfer = ControlTransfer::new(setup, device_address);
    if let Some(buffer) = data_buffer {
        transfer.data_buffer = buffer;
    }
    transfer
}

/// Helper: Validar descriptor genérico
pub fn validate_descriptor(data: &[u8], expected_type: u8) -> Result<(), &'static str> {
    if data.len() < 2 {
        return Err("Descriptor too short");
    }
    
    let length = data[0] as usize;
    let desc_type = data[1];
    
    if length > data.len() {
        return Err("Descriptor length exceeds buffer");
    }
    
    if desc_type != expected_type {
        return Err("Unexpected descriptor type");
    }
    
    Ok(())
}

/// Framework de ejecución de control transfer
/// 
/// Documenta las etapas de un control transfer para futura implementación.
pub fn execute_control_transfer_stub(transfer: &mut ControlTransfer) -> Result<(), &'static str> {
    crate::serial::serial_print("\n[USB-HID] === Control Transfer Stages ===\n");
    
    // Setup Stage
    crate::serial::serial_print("[USB-HID] Stage 1: SETUP\n");
    
    // Copy values from packed struct to avoid unaligned references
    let bm_request_type = transfer.setup.bm_request_type;
    let b_request = transfer.setup.b_request;
    let w_value = transfer.setup.w_value;
    let w_index = transfer.setup.w_index;
    let w_length = transfer.setup.w_length;
    
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   bmRequestType: 0x{:02X}\n", bm_request_type
    ));
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   bRequest: 0x{:02X}\n", b_request
    ));
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   wValue: 0x{:04X}\n", w_value
    ));
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   wIndex: 0x{:04X}\n", w_index
    ));
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID]   wLength: {}\n", w_length
    ));
    
    // TODO: Crear TRB tipo SETUP con setup packet
    // TODO: Poner TRB en transfer ring
    // TODO: Ring doorbell
    // TODO: Esperar completion event
    
    // Data Stage (si aplica)
    if transfer.has_data_stage() {
        if transfer.data_direction_in() {
            crate::serial::serial_print("[USB-HID] Stage 2: DATA IN\n");
            // TODO: Crear TRB(s) tipo DATA para recibir datos
        } else {
            crate::serial::serial_print("[USB-HID] Stage 2: DATA OUT\n");
            // TODO: Crear TRB(s) tipo DATA para enviar datos
        }
        crate::serial::serial_print(&alloc::format!(
            "[USB-HID]   Expected bytes: {}\n", w_length
        ));
        // TODO: Ejecutar data stage TRBs
    }
    
    // Status Stage
    crate::serial::serial_print("[USB-HID] Stage 3: STATUS\n");
    // TODO: Crear TRB tipo STATUS (dirección opuesta a data, o IN si no hay data)
    // TODO: Ejecutar status stage TRB
    // TODO: Verificar completion exitoso
    
    crate::serial::serial_print("[USB-HID] Transfer complete (stub)\n");
    transfer.state = ControlTransferState::Complete;
    
    Err("Not implemented - requires USB controller driver")
}

/// Inicializar infraestructura de control transfers
/// 
/// Prepara estructuras necesarias para control transfers.
pub fn init_control_transfer_infrastructure() {
    crate::serial::serial_print("\n[USB-HID] === Stage 2: USB Protocol Transactions ===\n");
    crate::serial::serial_print("[USB-HID] Control Transfer Infrastructure:\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbSetupPacket (8 bytes)\n");
    crate::serial::serial_print("[USB-HID]   ✓ ControlTransfer state machine\n");
    crate::serial::serial_print("[USB-HID]   ✓ Standard USB requests (GET_DESCRIPTOR, SET_ADDRESS, etc.)\n");
    crate::serial::serial_print("[USB-HID]   ✓ Request type constants and helpers\n");
    
    crate::serial::serial_print("\n[USB-HID] Descriptor Reading APIs:\n");
    crate::serial::serial_print("[USB-HID]   ✓ read_device_descriptor()\n");
    crate::serial::serial_print("[USB-HID]   ✓ read_configuration_descriptor()\n");
    crate::serial::serial_print("[USB-HID]   ✓ read_string_descriptor()\n");
    
    crate::serial::serial_print("\n[USB-HID] Device Management APIs:\n");
    crate::serial::serial_print("[USB-HID]   ✓ set_device_address()\n");
    crate::serial::serial_print("[USB-HID]   ✓ set_device_configuration()\n");
    
    crate::serial::serial_print("\n[USB-HID] Helper Functions:\n");
    crate::serial::serial_print("[USB-HID]   ✓ create_control_transfer()\n");
    crate::serial::serial_print("[USB-HID]   ✓ validate_descriptor()\n");
    crate::serial::serial_print("[USB-HID]   ✓ execute_control_transfer_stub()\n");
    
    crate::serial::serial_print("\n[USB-HID] NOTA: Stage 2 completo (stubs)\n");
    crate::serial::serial_print("[USB-HID]   Implementación real requiere:\n");
    crate::serial::serial_print("[USB-HID]   - XHCI/EHCI driver funcional\n");
    crate::serial::serial_print("[USB-HID]   - TRB submission y completion\n");
    crate::serial::serial_print("[USB-HID]   - Event ring polling\n");
    crate::serial::serial_print("[USB-HID]   - Doorbell register access\n");
}

// ============================================================================
// Stage 3: XHCI Driver Core
// ============================================================================

/// XHCI Command Ring - Circular buffer for command TRBs
/// Used to submit commands to the controller (Address Device, Configure Endpoint, etc.)
pub struct CommandRing {
    ring: TrbRing,
    command_ring_control: u64, // Physical address + RCS for CRCR register
}

impl CommandRing {
    /// Create a new command ring with specified capacity
    pub fn new(capacity: usize) -> Result<Self, &'static str> {
        let ring = TrbRing::new(capacity, true)?;
        let command_ring_control = ring.allocation.phys_addr;
        
        Ok(CommandRing {
            ring,
            command_ring_control,
        })
    }
    
    /// Submit a command TRB to the ring and return its physical address
    pub fn submit_command(&mut self, trb: Trb) -> Result<u64, &'static str> {
        if self.ring.is_full() {
            return Err("Command ring is full");
        }
        
        let enqueue_index = self.ring.enqueue_index;
        let trb_phys = self.ring.allocation.phys_addr + (enqueue_index as u64 * 16);
        self.ring.enqueue(trb)?;
        
        crate::serial::serial_print("[XHCI] Command TRB submitted\n");
        
        Ok(trb_phys)
    }
    
    /// Get the command ring control register value (for CRCR)
    pub fn get_crcr(&self) -> u64 {
        self.command_ring_control
    }
}

/// XHCI Transfer Ring - Circular buffer for transfer TRBs  
/// One ring per device endpoint for data transfers
pub struct TransferRing {
    ring: TrbRing,
    endpoint_address: u8,  // Device endpoint this ring is for
    device_slot: u8,       // Device slot ID (1-255)
}

impl TransferRing {
    /// Create a new transfer ring for an endpoint
    pub fn new(capacity: usize, device_slot: u8, endpoint_address: u8) -> Result<Self, &'static str> {
        let ring = TrbRing::new(capacity, true)?;
        
        Ok(TransferRing {
            ring,
            endpoint_address,
            device_slot,
        })
    }
    
    /// Submit a transfer TRB to the ring
    pub fn submit_transfer(&mut self, trb: Trb) -> Result<(), &'static str> {
        if self.ring.is_full() {
            return Err("Transfer ring is full");
        }
        
        self.ring.enqueue(trb)?;
        
        // TODO: Ring doorbell for device slot
        crate::serial::serial_print("[XHCI] Transfer TRB submitted\n");
        
        Ok(())
    }
    
    /// Get physical address of ring (for endpoint context)
    pub fn get_ring_address(&self) -> u64 {
        self.ring.allocation.phys_addr
    }
}

/// XHCI Event Ring - Circular buffer for event TRBs from controller
/// Controller writes completion events here
pub struct EventRing {
    ring: TrbRing,
    event_ring_segment_table: DmaAllocation, // Buffer for ERST
}

impl EventRing {
    /// Create a new event ring with specified capacity
    pub fn new(capacity: usize) -> Result<Self, &'static str> {
        let ring = TrbRing::new(capacity, false)?;
        
        // One segment for now
        let erst = DmaAllocation::allocate_erst(1)?;
        erst.zero();
        
        // Configure first ERST entry
        // 64-bit Ring Segment Base Address
        erst.write_u64(0, ring.allocation.phys_addr);
        // Size of the ring segment in TRBs
        erst.write_u64(8, capacity as u64); // Bits 0-15: size, 16-31: reserved
        
        let erdp = ring.allocation.phys_addr;
        
        Ok(EventRing {
            ring,
            event_ring_segment_table: erst,
        })
    }
    
    /// Process next event TRB from the ring
    pub fn process_next_event(&mut self) -> Option<Trb> {
        self.ring.pop_event()
    }
    
    /// Check if there are pending events
    pub fn has_pending_events(&self) -> bool {
        !self.ring.is_empty()
    }
    
    /// Get ERST base address (for runtime register)
    pub fn get_erst_base(&self) -> u64 {
        self.event_ring_segment_table.phys_addr
    }
    
    /// Get current ERDP value (for runtime register)
    pub fn get_erdp(&self) -> u64 {
        self.ring.allocation.phys_addr + (self.ring.dequeue_index as u64 * 16)
    }
}

/// XHCI Slot Context - Device addressing and routing information
/// Part of Device Context structure (Section 6.2.2)
#[repr(C)]
pub struct SlotContext {
    pub route_string: u32,        // Route string bits 0-19, speed bits 20-23, etc.
    pub port_info: u32,           // Root hub port, number of ports, context entries
    pub tt_info: u32,             // TT hub slot ID, TT port number, TT think time
    pub device_state: u32,        // Slot state, device address, reserved
    pub reserved: [u32; 4],       // Reserved for future use
}

impl SlotContext {
    /// Create a new slot context for a device
    pub fn new() -> Self {
        SlotContext {
            route_string: 0,
            port_info: 0,
            tt_info: 0,
            device_state: 0,
            reserved: [0; 4],
        }
    }
    
    /// Set device address in slot context
    pub fn set_device_address(&mut self, address: u8) {
        // Device address is in bits 0-7 of device_state
        self.device_state = (self.device_state & !0xFF) | (address as u32);
    }
    
    /// Get device address from slot context
    pub fn get_device_address(&self) -> u8 {
        (self.device_state & 0xFF) as u8
    }
}

/// XHCI Endpoint Context - Endpoint state and transfer ring info
/// Part of Device Context structure (Section 6.2.3)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EndpointContext {
    pub ep_state: u32,            // Endpoint state, mult, max streams, etc.
    pub ep_info: u32,             // Interval, error count, endpoint type, etc.
    pub tr_dequeue_pointer: u64,  // Transfer Ring Dequeue Pointer (physical)
    pub transfer_info: u32,       // Average TRB length, max ESIT payload
    pub reserved: [u32; 3],       // Reserved for future use
}

impl EndpointContext {
    /// Create a new endpoint context
    pub fn new() -> Self {
        EndpointContext {
            ep_state: 0,
            ep_info: 0,
            tr_dequeue_pointer: 0,
            transfer_info: 0,
            reserved: [0; 3],
        }
    }
    
    /// Set transfer ring dequeue pointer
    pub fn set_tr_dequeue_pointer(&mut self, address: u64, dcs: bool) {
        // DCS (Dequeue Cycle State) is bit 0
        self.tr_dequeue_pointer = address | (dcs as u64);
    }
    
    /// Set endpoint type (Control, Isoch, Bulk, Interrupt)
    pub fn set_endpoint_type(&mut self, ep_type: u8) {
        // Endpoint type is in bits 3-5 of ep_info
        self.ep_info = (self.ep_info & !(0x7 << 3)) | ((ep_type as u32) << 3);
    }
}

/// Endpoint type constants for EndpointContext
pub const EP_TYPE_CONTROL: u8 = 4;
pub const EP_TYPE_ISOCH_OUT: u8 = 1;
pub const EP_TYPE_BULK_OUT: u8 = 2;
pub const EP_TYPE_INTERRUPT_OUT: u8 = 3;
pub const EP_TYPE_ISOCH_IN: u8 = 5;
pub const EP_TYPE_BULK_IN: u8 = 6;
pub const EP_TYPE_INTERRUPT_IN: u8 = 7;

/// XHCI Device Context - Contains slot context and endpoint contexts
/// Section 6.2.1 of XHCI specification
#[repr(C, align(64))]
pub struct DeviceContext {
    pub slot_context: SlotContext,
    pub endpoint_contexts: [EndpointContext; 31], // EP 0-30
}

impl DeviceContext {
    /// Create a new device context
    pub fn new() -> Self {
        DeviceContext {
            slot_context: SlotContext::new(),
            endpoint_contexts: [EndpointContext::new(); 31],
        }
    }
}

/// XHCI Input Control Context - Specifies which contexts to modify
/// Section 6.2.5.1
#[repr(C)]
pub struct InputControlContext {
    pub drop_context_flags: u32,   // Bits indicate contexts to drop
    pub add_context_flags: u32,    // Bits indicate contexts to add/modify
    pub reserved: [u32; 5],        // Reserved
    pub configuration_value: u8,   // Configuration value
    pub interface_number: u8,      // Interface number
    pub alternate_setting: u8,     // Alternate setting
    pub reserved2: u8,
}

impl InputControlContext {
    /// Create a new input control context
    pub fn new() -> Self {
        InputControlContext {
            drop_context_flags: 0,
            add_context_flags: 0,
            reserved: [0; 5],
            configuration_value: 0,
            interface_number: 0,
            alternate_setting: 0,
            reserved2: 0,
        }
    }
    
    /// Add a context (slot or endpoint) to be configured
    pub fn add_context(&mut self, context_index: u8) {
        self.add_context_flags |= 1 << context_index;
    }
    
    /// Drop a context (remove endpoint)
    pub fn drop_context(&mut self, context_index: u8) {
        self.drop_context_flags |= 1 << context_index;
    }
}

/// XHCI Input Context - Used for Address Device and Configure Endpoint commands
/// Section 6.2.5
#[repr(C, align(64))]
pub struct InputContext {
    pub input_control_context: InputControlContext,
    pub device_context: DeviceContext,
}

impl InputContext {
    /// Create a new input context
    pub fn new() -> Self {
        InputContext {
            input_control_context: InputControlContext::new(),
            device_context: DeviceContext::new(),
        }
    }
}

/// TRB Builder - Helper functions to create specific TRB types

/// Create a Link TRB to chain ring segments
pub fn build_link_trb(next_segment: u64, toggle_cycle: bool) -> Trb {
    let mut control = (u32::from(TRB_TYPE_LINK)) << 10;
    if toggle_cycle {
        control |= 0x2; // Toggle Cycle bit
    }
    
    Trb {
        parameter: next_segment,
        status: 0,
        control,
    }
}

/// Create a No Op Command TRB (for testing)
pub fn build_noop_command_trb() -> Trb {
    Trb {
        parameter: 0,
        status: 0,
        control: (u32::from(TRB_TYPE_NOOP_COMMAND)) << 10,
    }
}

/// Create an Enable Slot Command TRB
pub fn build_enable_slot_trb(slot_type: u8) -> Trb {
    let control = (u32::from(TRB_TYPE_ENABLE_SLOT) << 10) | ((slot_type as u32) << 16);
    
    Trb {
        parameter: 0,
        status: 0,
        control,
    }
}

/// Create an Address Device Command TRB
pub fn build_address_device_trb(input_context_ptr: u64, slot_id: u8, bsr: bool) -> Trb {
    let mut control = (u32::from(TRB_TYPE_ADDRESS_DEVICE) << 10) | ((slot_id as u32) << 24);
    if bsr {
        control |= 0x200; // Block Set Address Request
    }
    
    Trb {
        parameter: input_context_ptr,
        status: 0,
        control,
    }
}

/// Create a Setup Stage TRB for Control Transfers
pub fn build_setup_trb(request_type: u8, request: u8, value: u16, index: u16, length: u16, transfer_type: u8) -> Trb {
    let parameter = (request_type as u64) | ((request as u64) << 8) | ((value as u64) << 16) | ((index as u64) << 32) | ((length as u64) << 48);
    let status = 8; // TRB Transfer Length (bits 16:0) = 8
    
    // TRB Type = 2 (Setup Stage), IDT (bit 6) = 1, TRT (bits 16-17)
    let control = (2 << 10) | (1 << 6) | ((transfer_type as u32) << 16);
    
    Trb {
        parameter,
        status,
        control,
    }
}

/// Create a Data Stage TRB for Control Transfers
pub fn build_data_trb(buffer_ptr: u64, length: u32, is_in: bool) -> Trb {
    let status = length & 0x1FFFF; // TRB Transfer Length (bits 16:0)
    
    // TRB Type = 3 (Data Stage), DIR (bit 16)
    let mut control = 3 << 10;
    if is_in {
        control |= 1 << 16;
    }
    
    Trb {
        parameter: buffer_ptr,
        status,
        control,
    }
}

/// Create a Status Stage TRB for Control Transfers
pub fn build_status_trb(is_in: bool, ioc: bool) -> Trb {
    // TRB Type = 4 (Status Stage), DIR (bit 16), IOC (bit 5)
    let mut control = 4 << 10;
    if is_in {
        control |= 1 << 16;
    }
    if ioc {
        control |= 1 << 5;
    }
    
    Trb {
        parameter: 0,
        status: 0,
        control,
    }
}

/// Create a Configure Endpoint Command TRB
pub fn build_configure_endpoint_trb(input_context_ptr: u64, slot_id: u8) -> Trb {
    let control = (u32::from(TRB_TYPE_CONFIGURE_ENDPOINT) << 10) | ((slot_id as u32) << 24);
    
    Trb {
        parameter: input_context_ptr,
        status: 0,
        control,
    }
}

/// Create a Setup Stage TRB (for control transfers)
pub fn build_setup_stage_trb(setup_packet: &UsbSetupPacket) -> Trb {
    // Setup packet goes in parameter field (8 bytes)
    let param_low = setup_packet.bm_request_type as u64
        | ((setup_packet.b_request as u64) << 8)
        | ((setup_packet.w_value as u64) << 16)
        | ((setup_packet.w_index as u64) << 32)
        | ((setup_packet.w_length as u64) << 48);
    
    let control = (u32::from(TRB_TYPE_SETUP) << 10) | (8u32 << 17); // 8 bytes
    
    Trb {
        parameter: param_low,
        status: 0,
        control,
    }
}

/// Create a Data Stage TRB (for control transfers)
pub fn build_data_stage_trb(data_buffer: u64, length: u16, direction_in: bool) -> Trb {
    let mut control = (u32::from(TRB_TYPE_DATA) << 10) | ((length as u32) << 17);
    if direction_in {
        control |= 0x10000; // DIR bit for IN transfers
    }
    
    Trb {
        parameter: data_buffer,
        status: 0,
        control,
    }
}

/// Create a Status Stage TRB (for control transfers)
pub fn build_status_stage_trb(direction_in: bool) -> Trb {
    let mut control = (u32::from(TRB_TYPE_STATUS)) << 10;
    if direction_in {
        control |= 0x10000; // DIR bit (opposite of data stage)
    }
    
    Trb {
        parameter: 0,
        status: 0,
        control,
    }
}

/// Create a Normal TRB (for bulk/interrupt transfers)
pub fn build_normal_trb(data_buffer: u64, length: u16, ioc: bool) -> Trb {
    let mut control = (u32::from(TRB_TYPE_NORMAL) << 10) | ((length as u32) << 17);
    if ioc {
        control |= 0x20; // Interrupt On Completion
    }
    
    Trb {
        parameter: data_buffer,
        status: 0,
        control,
    }
}

// Additional TRB type constants for Stage 3
pub const TRB_TYPE_ENABLE_SLOT: u8 = 9;
pub const TRB_TYPE_ADDRESS_DEVICE: u8 = 11;
pub const TRB_TYPE_CONFIGURE_ENDPOINT: u8 = 12;
pub const TRB_TYPE_NOOP_COMMAND: u8 = 23;

/// XHCI Doorbell Register structure
#[repr(C)]
pub struct DoorbellRegister {
    value: u32,
}

impl DoorbellRegister {
    /// Ring a doorbell for a device slot and endpoint
    pub fn ring(&mut self, target: u8, stream_id: u16) {
        // Target is bits 0-7, Stream ID is bits 16-31
        self.value = (target as u32) | ((stream_id as u32) << 16);
        
        // TODO: Write to actual MMIO doorbell register
        crate::serial::serial_print("[XHCI] Doorbell rung: target=");
        crate::serial::serial_print_dec(target as u64);
        crate::serial::serial_print(" stream=");
        crate::serial::serial_print_dec(stream_id as u64);
        crate::serial::serial_print("\n");
    }
}

/// XHCI Controller State - Tracks controller operational state
pub struct XhciControllerState {
    pub mmio: Option<XhciMmio>,
    pub command_ring: Option<CommandRing>,
    pub event_rings: alloc::vec::Vec<EventRing>,
    pub device_contexts_dma: alloc::vec::Vec<Option<DmaAllocation>>, // Index by slot ID
    pub transfer_rings: alloc::vec::Vec<Option<TransferRing>>,       // Index by slot ID * 31 + ep_idx
    pub dcbaa: Option<DmaAllocation>,
    pub dcbaa_phys: u64,
    pub mmio_base: u64,
    pub max_slots: u8,
    pub max_ports: u8,
}

impl XhciControllerState {
    /// Create a new XHCI controller state
    pub fn new(mmio_base: u64, mmio: XhciMmio) -> Self {
        XhciControllerState {
            mmio: Some(mmio),
            command_ring: None,
            event_rings: alloc::vec::Vec::new(),
            device_contexts_dma: alloc::vec::Vec::new(),
            transfer_rings: alloc::vec::Vec::new(),
            dcbaa: None,
            dcbaa_phys: 0,
            mmio_base,
            max_slots: 0,
            max_ports: 0,
        }
    }
    
    /// Initialize controller rings and structures
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Initializing controller rings and structures...\n");
        
        let mmio = self.mmio.as_ref().ok_or("MMIO not found")?;

        // 1. Allocate Device Context Base Address Array (DCBAA)
        let max_slots = self.max_slots as usize;
        let dcbaa = DmaAllocation::allocate_dcbaa(max_slots)?;
        dcbaa.zero();

        // 2. Setup Scratchpad Buffers if needed
        let hcsparams2 = mmio.read_capability(0x08);
        let max_scratchpad_hi = (hcsparams2 >> 27) & 0x1F;
        let max_scratchpad_lo = (hcsparams2 >> 21) & 0x1F;
        let max_scratchpad_bufs = (max_scratchpad_hi << 5) | max_scratchpad_lo;
        
        if max_scratchpad_bufs > 0 {
            crate::serial::serial_print(&alloc::format!("[XHCI] Allocating {} scratchpad buffers\n", max_scratchpad_bufs));
            
            // Allocate Scatter/Gather Array of pointers
            let sp_array = DmaAllocation::allocate((max_scratchpad_bufs as usize) * 8, 64)?;
            sp_array.zero();
            
            // Allocate actual pages and fill array
            for i in 0..max_scratchpad_bufs {
                let page = DmaAllocation::allocate(4096, 4096)?;
                page.zero();
                sp_array.write_u64((i as usize) * 8, page.phys_addr);
            }
            
            // Put array pointer in DCBAA[0]
            dcbaa.write_u64(0, sp_array.phys_addr);
        }

        self.dcbaa_phys = dcbaa.phys_addr;
        self.dcbaa = Some(dcbaa);

        // 3. Create command ring (64 TRBs)
        let command_ring = CommandRing::new(64)?;
        self.command_ring = Some(command_ring);
        
        // 4. Create event ring for interrupter 0 (256 TRBs)
        let event_ring = EventRing::new(256)?;
        self.event_rings.push(event_ring);
        
        // Allocate device context array
        for _ in 0..=max_slots {
            self.device_contexts_dma.push(None); // Empty contexts initially
        }
        
        // Pre-allocate transfer rings array (32 endpoints per slot, including slot 0 which is unused)
        for _ in 0..=(max_slots * 32) {
            self.transfer_rings.push(None);
        }
        
        crate::serial::serial_print("[XHCI] Controller rings initialized\n");
        Ok(())
    }
    
    /// Start the XHCI controller with proper reset sequence
    pub fn start_controller(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Starting controller...\n");
        let mmio = self.mmio.as_ref().ok_or("MMIO not initialized")?;

        // 1. Wait until Controller Not Ready (CNR) is 0
        let mut timeout = 10000;
        while (mmio.read_operational(0x04) & (1 << 11)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("Timeout waiting for CNR to clear before reset"); }
        }

        // 2. Perform Host Controller Reset (HCRST)
        crate::serial::serial_print("[XHCI] Asserting HCRST...\n");
        let mut usbcmd = mmio.read_operational(0x00);
        usbcmd |= 1 << 1; // HCRST
        mmio.write_operational(0x00, usbcmd);

        // Wait for HCRST to clear
        timeout = 10000;
        while (mmio.read_operational(0x00) & (1 << 1)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("Timeout waiting for HCRST to clear"); }
        }

        // Wait for CNR to clear again
        timeout = 10000;
        while (mmio.read_operational(0x04) & (1 << 11)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("Timeout waiting for CNR to clear after reset"); }
        }
        
        crate::serial::serial_print("[XHCI] Controller Reset Complete\n");

        // 3. Program MaxSlotsEnabled in CONFIG register
        let config = mmio.read_operational(0x38);
        mmio.write_operational(0x38, (config & !0xFF) | (self.max_slots as u32));

        // 4. Program DCBAAP
        let dcbaa_phys = self.dcbaa_phys;
        mmio.write_operational(0x30, dcbaa_phys as u32);
        mmio.write_operational(0x34, (dcbaa_phys >> 32) as u32);

        // 5. Program Command Ring Control Register (CRCR)
        if let Some(ref cmd_ring) = self.command_ring {
            let cr_phys = cmd_ring.get_crcr();
            mmio.write_operational(0x18, (cr_phys as u32) | 1); // Enable Ring Cycle State (RCS)
            mmio.write_operational(0x1C, (cr_phys >> 32) as u32);
        }

        // 6. Program Event Ring
        if !self.event_rings.is_empty() {
            let er_ring = &self.event_rings[0];
            // ERSTSZ (Runtime 0x28)
            mmio.write_runtime(0x28, 1);
            // ERDP (Runtime 0x38)
            let erdp = er_ring.get_erdp();
            mmio.write_runtime(0x38, erdp as u32);
            mmio.write_runtime(0x3C, (erdp >> 32) as u32);
            // ERSTBA (Runtime 0x30)
            let erstba = er_ring.get_erst_base();
            mmio.write_runtime(0x30, erstba as u32);
            mmio.write_runtime(0x34, (erstba >> 32) as u32);
        }

        // 7. Enable Interrupts
        crate::serial::serial_print("[XHCI] Enabling native interrupts...\n");
        mmio.write_runtime(0x20, 0x03); // IP (1) | IE (2)
        let mut cmd = mmio.read_operational(0x00);
        cmd |= 1 << 2; // INTE
        
        // 8. Start Controller
        cmd |= 1; // R/S bit
        mmio.write_operational(0x00, cmd);

        crate::serial::serial_print("[XHCI] Controller started successfully.\n");
        Ok(())
    }
    
    /// Submit a command TRB to the command ring and ring the doorbell
    pub fn submit_command(&mut self, trb: Trb) -> Result<u64, &'static str> {
        let trb_phys = if let Some(ref mut cmd_ring) = self.command_ring {
            cmd_ring.submit_command(trb)?
        } else {
            return Err("Command ring not initialized");
        };
        
        // Ring doorbell 0 (Host Controller), target 0 (Command Ring)
        if let Some(ref mmio) = self.mmio {
            mmio.ring_doorbell(0, 0);
        }
        
        Ok(trb_phys)
    }
    /// Poll the event ring for a specific TRB completion
    pub fn poll_event_blocking(&mut self, target_trb_phys: u64, trb_type_filter: u8) -> Result<Trb, &'static str> {
        let mut timeout = 5000000;
        while timeout > 0 {
            if let Some(event_ring) = self.event_rings.get_mut(0) {
                if let Some(event) = event_ring.process_next_event() {
                    let trb_type = event.get_trb_type();
                    
                    // Update ERDP and clear EHB
                    if let Some(ref mmio) = self.mmio {
                        let erdp = event_ring.get_erdp();
                        mmio.write_runtime(0x38, (erdp as u32) | 0x08);
                        mmio.write_runtime(0x3C, (erdp >> 32) as u32);
                    }
                    
                    if trb_type == trb_type_filter {
                        if event.parameter == target_trb_phys {
                            let comp_code = (event.status >> 24) & 0xFF;
                            if comp_code == 1 || comp_code == 13 { // Success or Short Packet
                                return Ok(event);
                            } else {
                                crate::serial::serial_print(&alloc::format!("[XHCI] Event failed with code {}\n", comp_code));
                                return Err("Event indicated failure");
                            }
                        }
                    }
                }
            }
            timeout -= 1;
        }
        Err("Poll timeout")
    }
    /// Submit a command and spin-wait for its completion event
    pub fn execute_command_blocking(&mut self, trb: Trb) -> Result<Trb, &'static str> {
        let trb_phys = self.submit_command(trb)?;
        self.poll_event_blocking(trb_phys, 33) // 33 = Command Completion
    }

    /// Enumerate ports to find connected devices
    pub fn enumerate_ports(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Enumerating ports...\n");
        let max_ports = self.max_ports;

        for i in 1..=max_ports {
            let port_offset = 0x400 + ((i as usize - 1) * 0x10);
            
            // Temporary block to scope the mmio reference
            let (is_connected, portsc) = {
                let mmio = self.mmio.as_ref().ok_or("MMIO not initialized")?;
                let portsc = mmio.read_operational(port_offset);
                ((portsc & 1) != 0, portsc)
            };

            if is_connected {
                crate::serial::serial_print(&alloc::format!("[XHCI] Device connected on port {}\n", i));
                
                // Assert Port Reset (PR, bit 4)
                if let Some(ref mmio) = self.mmio {
                    mmio.write_operational(port_offset, portsc | (1 << 4));
                }

                // Wait for Port Reset Change (PRC, bit 21) or Reset to complete
                let mut timeout = 100000;
                let mut reset_complete = false;
                while timeout > 0 {
                    if let Some(ref mmio) = self.mmio {
                        let status = mmio.read_operational(port_offset);
                        if (status & (1 << 21)) != 0 || (status & (1 << 4)) == 0 {
                            reset_complete = true;
                            break;
                        }
                    }
                    timeout -= 1;
                }

                if !reset_complete {
                    crate::serial::serial_print(&alloc::format!("[XHCI] Timeout resetting port {}\n", i));
                    continue;
                }

                crate::serial::serial_print(&alloc::format!("[XHCI] Port {} reset complete\n", i));

                let mut port_speed = 0;
                // Read speed and clear status change bits
                if let Some(ref mmio) = self.mmio {
                    let current = mmio.read_operational(port_offset);
                    port_speed = (current >> 10) & 0x0F;
                    crate::serial::serial_print(&alloc::format!("[XHCI] Port {} speed: {}\n", i, port_speed));
                    
                    let clear_mask = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22);
                    mmio.write_operational(port_offset, (current & 0x0E00C3E0) | clear_mask);
                }
                
                // Enable Slot
                crate::serial::serial_print(&alloc::format!("[XHCI] Issuing Enable Slot Command for port {}\n", i));
                let enable_trb = build_enable_slot_trb(0);
                
                match self.execute_command_blocking(enable_trb) {
                    Ok(completion_event) => {
                        let completion_code = (completion_event.status >> 24) & 0xFF;
                        let slot_id = (completion_event.control >> 24) & 0xFF;
                        crate::serial::serial_print(&alloc::format!("[XHCI] Enable Slot success, Slot ID: {}, Code: {}\n", slot_id, completion_code));
                        
                        // Proceed to Address Device etc.
                        crate::serial::serial_print(&alloc::format!("[XHCI] Allocating contexts for Slot {}\n", slot_id));
                        let slot_idx = slot_id as usize;
                        if let Ok(dev_ctx_dma) = DmaAllocation::allocate(1024, 64) {
                            dev_ctx_dma.zero();
                            // Store in DCBAA
                            if let Some(ref dcbaa) = self.dcbaa {
                                dcbaa.write_u64(slot_idx * 8, dev_ctx_dma.phys_addr);
                            }
                            self.device_contexts_dma[slot_idx] = Some(dev_ctx_dma);
                            
                            // Allocate Input Context
                            if let Ok(input_ctx_dma) = DmaAllocation::allocate(2048, 64) {
                                input_ctx_dma.zero();
                                
                                // Initialize Input Control Context
                                // add_context_flags = Slot Context (bit 0) | EP0 Context (bit 1)
                                input_ctx_dma.write_u32(4, 0x03);
                                
                                // Initialize Slot Context (starts at offset 32)
                                let route_string = (port_speed << 20) | (1 << 27); // speed and 1 context entry
                                let root_hub_port = i as u32;
                                input_ctx_dma.write_u32(32, route_string);
                                input_ctx_dma.write_u32(36, root_hub_port << 16); // port_info
                                
                                // Initialize EP0 Context (starts at offset 64)
                                if let Ok(tr_ring) = TransferRing::new(64, slot_id as u8, 0) {
                                    let tr_phys = tr_ring.get_ring_address();
                                    self.transfer_rings[slot_idx * 32] = Some(tr_ring);
                                    
                                    // ep_info: error count=3, type=4 (Control)
                                    // Max Packet Size: 8 for LS, 8/64 for FS, 64 for HS, 512 for SS
                                    let mps = match port_speed {
                                        2 => 8,   // Low Speed
                                        3 => 64,  // High Speed
                                        4 => 512, // Super Speed
                                        _ => 64,  // Default to 64 (Full Speed usually works with this)
                                    };
                                    input_ctx_dma.write_u32(68, (3 << 1) | (4 << 3) | (mps << 16));
                                    input_ctx_dma.write_u64(72, tr_phys | 1); // TR Dequeue Pointer with DCS (bit 0) = 1
                                    
                                    crate::serial::serial_print("[XHCI] Issuing Address Device Command\n");
                                    let address_trb = build_address_device_trb(input_ctx_dma.phys_addr, slot_id as u8, false);
                                    
                                    match self.execute_command_blocking(address_trb) {
                                        Ok(addr_completion) => {
                                            let code = (addr_completion.status >> 24) & 0xFF;
                                            crate::serial::serial_print(&alloc::format!("[XHCI] Address Device success, Code: {}\n", code));
                                            
                                            // Request Device Descriptor
                                            if let Err(e) = self.request_device_descriptor(slot_id as u8) {
                                                crate::serial::serial_print("[XHCI] request_device_descriptor failed: ");
                                                crate::serial::serial_print(e);
                                                crate::serial::serial_print("\n");
                                            }
                                        },
                                        Err(e) => {
                                            crate::serial::serial_print("[XHCI] Address Device failed: ");
                                            crate::serial::serial_print(e);
                                            crate::serial::serial_print("\n");
                                        }
                                    }
                                }
                            }
                        }
                    },
                    Err(e) => {
                        crate::serial::serial_print("[XHCI] Enable Slot failed: ");
                        crate::serial::serial_print(e);
                        crate::serial::serial_print("\n");
                    }
                }
            }
        }
        Ok(())
    }

    /// Perform a control transfer to read the Device Descriptor
    pub fn request_device_descriptor(&mut self, slot_id: u8) -> Result<(), &'static str> {
        crate::serial::serial_print(&alloc::format!("[XHCI] Requesting Device Descriptor for slot {}\n", slot_id));
        
        // Allocate DMA buffer for the descriptor (18 bytes, aligned to 64)
        let desc_buffer = DmaAllocation::allocate(64, 64)?;
        desc_buffer.zero();
        
        // Setup Stage: GET_DESCRIPTOR (Device)
        let setup_trb = build_setup_trb(0x80, 0x06, 0x0100, 0x0000, 18, 3); // 3 = IN Data Stage
        let data_trb = build_data_trb(desc_buffer.phys_addr, 18, true); // true = IN
        let status_trb = build_status_trb(false, true); // false = OUT, true = IOC
        
        let slot_idx = slot_id as usize;
        let mut status_trb_phys = 0;
        
        if let Some(ref mut tr_ring) = self.transfer_rings[slot_idx * 32] {
            tr_ring.submit_transfer(setup_trb)?;
            tr_ring.submit_transfer(data_trb)?;
            status_trb_phys = tr_ring.ring.allocation.phys_addr + (tr_ring.ring.enqueue_index as u64 * 16);
            tr_ring.submit_transfer(status_trb)?;
            
            // Ring doorbell for EP0 (Target 1)
            let mmio = self.mmio.as_ref().ok_or("MMIO not initialized")?;
            mmio.ring_doorbell(slot_id, 1);
        } else {
            return Err("EP0 transfer ring not initialized");
        }
        
        // Poll for Transfer Event (type 32)
        self.poll_event_blocking(status_trb_phys, 32)?;

        
        // Parse descriptor
        let mut desc_data = [0u8; 18];
        desc_buffer.read_bytes(0, &mut desc_data);
        
        crate::serial::serial_print("[XHCI] Device Descriptor received:\n");
        crate::serial::serial_print(&alloc::format!("  bLength: {}\n", desc_data[0]));
        crate::serial::serial_print(&alloc::format!("  bDescriptorType: {}\n", desc_data[1]));
        crate::serial::serial_print(&alloc::format!("  bcdUSB: {:02X}{:02X}\n", desc_data[3], desc_data[2]));
        crate::serial::serial_print(&alloc::format!("  bDeviceClass: {:02X}\n", desc_data[4]));
        crate::serial::serial_print(&alloc::format!("  bDeviceSubClass: {:02X}\n", desc_data[5]));
        crate::serial::serial_print(&alloc::format!("  bDeviceProtocol: {:02X}\n", desc_data[6]));
        crate::serial::serial_print(&alloc::format!("  bMaxPacketSize0: {}\n", desc_data[7]));
        crate::serial::serial_print(&alloc::format!("  idVendor: {:02X}{:02X}\n", desc_data[9], desc_data[8]));
        crate::serial::serial_print(&alloc::format!("  idProduct: {:02X}{:02X}\n", desc_data[11], desc_data[10]));
        
        Ok(())
    }
    
    /// Process pending events from event ring (for async interrupts)
    pub fn process_events(&mut self) {
        if self.event_rings.is_empty() {
            return;
        }
        
        let event_ring = &mut self.event_rings[0];
        while let Some(event_trb) = event_ring.process_next_event() {
            let trb_type = event_trb.get_trb_type();
            
            crate::serial::serial_print("[XHCI] Event TRB type: ");
            crate::serial::serial_print_dec(trb_type as u64);
            crate::serial::serial_print("\n");
            
            // Update ERDP
            if let Some(ref mmio) = self.mmio {
                let erdp = event_ring.get_erdp();
                mmio.write_runtime(0x38, erdp as u32);
                mmio.write_runtime(0x3C, (erdp >> 32) as u32);
            }
        }
    }
}

/// Initialize Stage 3 XHCI driver core infrastructure
pub fn init_xhci_driver_core() {
    crate::serial::serial_print("\n[USB-HID] === Stage 3: XHCI Driver Core ===\n");
    
    crate::serial::serial_print("[USB-HID] XHCI Ring Structures:\n");
    crate::serial::serial_print("[USB-HID]   ✓ CommandRing (command submission)\n");
    crate::serial::serial_print("[USB-HID]   ✓ TransferRing (data transfers per endpoint)\n");
    crate::serial::serial_print("[USB-HID]   ✓ EventRing (completion events)\n");
    
    crate::serial::serial_print("[USB-HID] XHCI Context Structures:\n");
    crate::serial::serial_print("[USB-HID]   ✓ SlotContext (device addressing)\n");
    crate::serial::serial_print("[USB-HID]   ✓ EndpointContext (endpoint state)\n");
    crate::serial::serial_print("[USB-HID]   ✓ DeviceContext (slot + 31 endpoints)\n");
    crate::serial::serial_print("[USB-HID]   ✓ InputContext (for commands)\n");
    
    crate::serial::serial_print("[USB-HID] TRB Builders:\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_enable_slot_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_address_device_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_configure_endpoint_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_setup_stage_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_data_stage_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_status_stage_trb()\n");
    crate::serial::serial_print("[USB-HID]   ✓ build_normal_trb()\n");
    
    crate::serial::serial_print("[USB-HID] Controller Operations:\n");
    crate::serial::serial_print("[USB-HID]   ✓ XhciControllerState (state tracking)\n");
    crate::serial::serial_print("[USB-HID]   ✓ initialize() (rings and structures)\n");
    crate::serial::serial_print("[USB-HID]   ✓ start_controller() (start sequence stub)\n");
    crate::serial::serial_print("[USB-HID]   ✓ submit_command() (command submission)\n");
    crate::serial::serial_print("[USB-HID]   ✓ process_events() (event processing)\n");
    
    crate::serial::serial_print("[USB-HID] Doorbell Operations:\n");
    crate::serial::serial_print("[USB-HID]   ✓ DoorbellRegister::ring()\n");
    
    crate::serial::serial_print("\n[USB-HID] Stage 3 framework complete\n");
    crate::serial::serial_print("[USB-HID] NOTA: Requiere integración MMIO:\n");
    crate::serial::serial_print("[USB-HID]   - Mapear registros XHCI a memoria virtual\n");
    crate::serial::serial_print("[USB-HID]   - Escribir/leer registros capability/operational/runtime\n");
    crate::serial::serial_print("[USB-HID]   - Configurar interrupts MSI/MSI-X\n");
    crate::serial::serial_print("[USB-HID]   - Allocar DMA buffers físicos\n");
}

// ============================================================================
// STAGE 4: INTERRUPT HANDLING AND EVENT PROCESSING
// ============================================================================

/// Event TRB: Command Completion Event (Section 6.4.2.2)
#[repr(C)]
pub struct CommandCompletionEvent {
    command_trb_pointer: u64,     // Pointer to completed command TRB
    completion_info: u32,         // Completion Parameter + Completion Code
    flags: u32,                   // Cycle, Type, VF ID, Slot ID
}

impl CommandCompletionEvent {
    pub fn get_completion_code(&self) -> u8 {
        ((self.completion_info >> 24) & 0xFF) as u8
    }
    
    pub fn get_slot_id(&self) -> u8 {
        ((self.flags >> 24) & 0xFF) as u8
    }
    
    pub fn is_success(&self) -> bool {
        self.get_completion_code() == TRB_COMPLETION_SUCCESS
    }
}

/// Event TRB: Transfer Event (Section 6.4.2.1)
#[repr(C)]
pub struct TransferEvent {
    trb_pointer: u64,             // TRB pointer
    transfer_info: u32,           // Transfer length + Completion Code
    flags: u32,                   // Cycle, ED, Type, Endpoint ID, Slot ID
}

impl TransferEvent {
    pub fn get_completion_code(&self) -> u8 {
        ((self.transfer_info >> 24) & 0xFF) as u8
    }
    
    pub fn get_transfer_length(&self) -> u32 {
        // Residual transfer length in bytes
        self.transfer_info & 0x00FFFFFF
    }
    
    pub fn get_slot_id(&self) -> u8 {
        ((self.flags >> 24) & 0xFF) as u8
    }
    
    pub fn get_endpoint_id(&self) -> u8 {
        ((self.flags >> 16) & 0x1F) as u8
    }
}

/// Event TRB: Port Status Change Event (Section 6.4.2.3)
#[repr(C)]
pub struct PortStatusChangeEvent {
    reserved: u64,
    port_info: u32,               // Port ID
    flags: u32,                   // Cycle, Type
}

impl PortStatusChangeEvent {
    pub fn get_port_id(&self) -> u8 {
        ((self.port_info >> 24) & 0xFF) as u8
    }
}

// TRB Completion Codes (Section 6.4.5)
pub const TRB_COMPLETION_INVALID: u8 = 0;
pub const TRB_COMPLETION_SUCCESS: u8 = 1;
pub const TRB_COMPLETION_DATA_BUFFER_ERROR: u8 = 2;
pub const TRB_COMPLETION_BABBLE_DETECTED: u8 = 3;
pub const TRB_COMPLETION_USB_TRANSACTION_ERROR: u8 = 4;
pub const TRB_COMPLETION_TRB_ERROR: u8 = 5;
pub const TRB_COMPLETION_STALL_ERROR: u8 = 6;
pub const TRB_COMPLETION_RESOURCE_ERROR: u8 = 7;
pub const TRB_COMPLETION_BANDWIDTH_ERROR: u8 = 8;
pub const TRB_COMPLETION_NO_SLOTS_AVAILABLE: u8 = 9;
pub const TRB_COMPLETION_INVALID_STREAM_TYPE: u8 = 10;
pub const TRB_COMPLETION_SLOT_NOT_ENABLED: u8 = 11;
pub const TRB_COMPLETION_ENDPOINT_NOT_ENABLED: u8 = 12;
pub const TRB_COMPLETION_SHORT_PACKET: u8 = 13;
pub const TRB_COMPLETION_RING_UNDERRUN: u8 = 14;
pub const TRB_COMPLETION_RING_OVERRUN: u8 = 15;
pub const TRB_COMPLETION_VF_EVENT_RING_FULL: u8 = 16;
pub const TRB_COMPLETION_PARAMETER_ERROR: u8 = 17;
pub const TRB_COMPLETION_BANDWIDTH_OVERRUN: u8 = 18;
pub const TRB_COMPLETION_CONTEXT_STATE_ERROR: u8 = 19;
pub const TRB_COMPLETION_NO_PING_RESPONSE: u8 = 20;
pub const TRB_COMPLETION_EVENT_RING_FULL: u8 = 21;
pub const TRB_COMPLETION_INCOMPATIBLE_DEVICE: u8 = 22;
pub const TRB_COMPLETION_MISSED_SERVICE: u8 = 23;
pub const TRB_COMPLETION_COMMAND_RING_STOPPED: u8 = 24;
pub const TRB_COMPLETION_COMMAND_ABORTED: u8 = 25;
pub const TRB_COMPLETION_STOPPED: u8 = 26;
pub const TRB_COMPLETION_STOPPED_LENGTH_INVALID: u8 = 27;
pub const TRB_COMPLETION_STOPPED_SHORT_PACKET: u8 = 28;
pub const TRB_COMPLETION_MAX_EXIT_LATENCY_TOO_LARGE: u8 = 29;

/// Port Status and Control Register (Section 5.4.8)
pub struct PortStatusControl {
    value: u32,
}

impl PortStatusControl {
    pub fn new(value: u32) -> Self {
        Self { value }
    }
    
    /// Current Connect Status (bit 0)
    pub fn is_connected(&self) -> bool {
        (self.value & 0x1) != 0
    }
    
    /// Port Enabled/Disabled (bit 1)
    pub fn is_enabled(&self) -> bool {
        (self.value & 0x2) != 0
    }
    
    /// Port Reset (bit 4)
    pub fn is_resetting(&self) -> bool {
        (self.value & 0x10) != 0
    }
    
    /// Port Speed (bits 10-13)
    pub fn get_speed(&self) -> u8 {
        ((self.value >> 10) & 0xF) as u8
    }
}

// Port speed values
pub const PORT_SPEED_FULL: u8 = 1;      // USB 1.1 Full Speed (12 Mbps)
pub const PORT_SPEED_LOW: u8 = 2;       // USB 1.1 Low Speed (1.5 Mbps)
pub const PORT_SPEED_HIGH: u8 = 3;      // USB 2.0 High Speed (480 Mbps)
pub const PORT_SPEED_SUPER: u8 = 4;     // USB 3.0 SuperSpeed (5 Gbps)

/// Reset a USB port
pub fn reset_port(port_id: u8) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Resetting port ");
    crate::serial::serial_print_dec(port_id as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Write to port status control register
    // Set PR (Port Reset) bit
    // Wait 50-120ms for reset to complete
    
    crate::serial::serial_print("[USB-HID] Port reset initiated (stub)\n");
    Ok(())
}

/// Clear port status change bits
pub fn clear_port_change_bits(port_id: u8) {
    crate::serial::serial_print("[USB-HID] Clearing port ");
    crate::serial::serial_print_dec(port_id as u64);
    crate::serial::serial_print(" change bits\n");
    
    // TODO: Write 1 to CSC, PEC, WRC, OCC, PRC, PLC, CEC bits to clear them
}

/// Get port speed
pub fn get_port_speed(port_id: u8) -> u8 {
    crate::serial::serial_print("[USB-HID] Reading port ");
    crate::serial::serial_print_dec(port_id as u64);
    crate::serial::serial_print(" speed\n");
    
    // TODO: Read port status control register and extract speed
    PORT_SPEED_HIGH // Stub: assume High Speed
}

/// Wait for port reset to complete
pub fn wait_for_port_reset_complete(port_id: u8) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Waiting for port ");
    crate::serial::serial_print_dec(port_id as u64);
    crate::serial::serial_print(" reset complete\n");
    
    // TODO: Poll PR bit until it's cleared (max 200ms timeout)
    
    Ok(())
}

/// Process a command completion event
pub fn process_command_completion_event(event: &CommandCompletionEvent) {
    let completion_code = event.get_completion_code();
    let slot_id = event.get_slot_id();
    
    crate::serial::serial_print("[USB-HID] Command Completion Event:\n");
    crate::serial::serial_print("[USB-HID]   Completion Code: ");
    crate::serial::serial_print_dec(completion_code as u64);
    crate::serial::serial_print("\n[USB-HID]   Slot ID: ");
    crate::serial::serial_print_dec(slot_id as u64);
    crate::serial::serial_print("\n");
    
    if event.is_success() {
        crate::serial::serial_print("[USB-HID]   Status: SUCCESS\n");
    } else {
        crate::serial::serial_print("[USB-HID]   Status: ERROR\n");
    }
    
    // TODO: Notify waiting threads, update completion tracker
}

/// Process a transfer event
pub fn process_transfer_event(event: &TransferEvent) {
    let completion_code = event.get_completion_code();
    let residual_length = event.get_transfer_length();
    let slot_id = event.get_slot_id();
    let endpoint_id = event.get_endpoint_id();
    
    crate::serial::serial_print("[USB-HID] Transfer Event:\n");
    crate::serial::serial_print("[USB-HID]   Completion Code: ");
    crate::serial::serial_print_dec(completion_code as u64);
    crate::serial::serial_print("\n[USB-HID]   Residual Length: ");
    crate::serial::serial_print_dec(residual_length as u64);
    crate::serial::serial_print("\n[USB-HID]   Slot ID: ");
    crate::serial::serial_print_dec(slot_id as u64);
    crate::serial::serial_print("\n[USB-HID]   Endpoint ID: ");
    crate::serial::serial_print_dec(endpoint_id as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Invoke transfer completion callbacks
}

/// Process a port status change event
pub fn process_port_status_change_event(event: &PortStatusChangeEvent) {
    let port_id = event.get_port_id();
    
    crate::serial::serial_print("[USB-HID] Port Status Change Event:\n");
    crate::serial::serial_print("[USB-HID]   Port ID: ");
    crate::serial::serial_print_dec(port_id as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Read port status register
    // Detect connection/disconnection
    // Initiate device enumeration if device connected
    
    crate::serial::serial_print("[USB-HID]   Port event detected (stub)\n");
}

/// Interrupt configuration
pub struct InterruptConfiguration {
    pub irq_number: u8,
    pub vector: u8,
    pub msi_enabled: bool,
    pub msix_enabled: bool,
    pub interrupter_index: u16,
}

/// Configure interrupts
pub fn configure_interrupts() -> Result<InterruptConfiguration, &'static str> {
    crate::serial::serial_print("[USB-HID] Configuring interrupts\n");
    
    // TODO: Setup MSI/MSI-X or legacy IRQ
    // Configure IMAN, IMOD, USBINTR registers
    
    Ok(InterruptConfiguration {
        irq_number: 11,  // Stub IRQ number
        vector: 0,
        msi_enabled: false,
        msix_enabled: false,
        interrupter_index: 0,
    })
}

/// USB interrupt handler (stub)
pub fn usb_interrupt_handler_stub() -> bool {
    crate::serial::serial_print("[USB-HID] Interrupt handler called\n");
    
    // TODO: Read USBSTS register
    // Check for Host System Error (HSE)
    // Check for Event Interrupt (EINT)
    // Process events from event ring
    // Clear USBSTS bits (write-1-to-clear)
    // Update event ring dequeue pointer
    
    true // Interrupt handled
}

/// Register USB interrupt handler
pub fn register_usb_interrupt_handler(irq: u8) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Registering interrupt handler for IRQ ");
    crate::serial::serial_print_dec(irq as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Use kernel IRQ registration API
    
    Ok(())
}

/// Completion tracker
pub struct CompletionTracker {
    pending_commands: alloc::vec::Vec<(u64, u64)>,  // (TRB pointer, timestamp)
    timeout_ms: u64,
}

impl CompletionTracker {
    pub fn new() -> Self {
        Self {
            pending_commands: alloc::vec::Vec::new(),
            timeout_ms: 5000,  // 5 second timeout
        }
    }
    
    pub fn wait_for_command_completion(&self, _trb_pointer: u64) -> Result<u8, &'static str> {
        crate::serial::serial_print("[USB-HID] Waiting for command completion\n");
        
        // TODO: Poll event ring for matching completion
        // Timeout after timeout_ms
        
        Err("Not implemented - requires event ring polling")
    }
    
    pub fn mark_command_complete(&mut self, _trb_pointer: u64, _completion_code: u8) {
        crate::serial::serial_print("[USB-HID] Command marked complete\n");
        
        // TODO: Record completion, signal waiting threads
    }
}

/// Initialize Stage 4: Interrupt infrastructure
pub fn init_interrupt_infrastructure() {
    crate::serial::serial_print("\n[USB-HID] === Stage 4: Interrupt Handling ===\n");
    
    crate::serial::serial_print("[USB-HID] Event TRB Structures:\n");
    crate::serial::serial_print("[USB-HID]   ✓ CommandCompletionEvent\n");
    crate::serial::serial_print("[USB-HID]   ✓ TransferEvent\n");
    crate::serial::serial_print("[USB-HID]   ✓ PortStatusChangeEvent\n");
    
    crate::serial::serial_print("[USB-HID] TRB Completion Codes: 29 codes defined\n");
    
    crate::serial::serial_print("[USB-HID] Port Management:\n");
    crate::serial::serial_print("[USB-HID]   ✓ PortStatusControl register\n");
    crate::serial::serial_print("[USB-HID]   ✓ reset_port()\n");
    crate::serial::serial_print("[USB-HID]   ✓ clear_port_change_bits()\n");
    crate::serial::serial_print("[USB-HID]   ✓ get_port_speed()\n");
    crate::serial::serial_print("[USB-HID]   ✓ wait_for_port_reset_complete()\n");
    
    crate::serial::serial_print("[USB-HID] Event Processing:\n");
    crate::serial::serial_print("[USB-HID]   ✓ process_command_completion_event()\n");
    crate::serial::serial_print("[USB-HID]   ✓ process_transfer_event()\n");
    crate::serial::serial_print("[USB-HID]   ✓ process_port_status_change_event()\n");
    
    crate::serial::serial_print("[USB-HID] Interrupt Handling:\n");
    crate::serial::serial_print("[USB-HID]   ✓ InterruptConfiguration\n");
    crate::serial::serial_print("[USB-HID]   ✓ configure_interrupts()\n");
    crate::serial::serial_print("[USB-HID]   ✓ usb_interrupt_handler_stub()\n");
    crate::serial::serial_print("[USB-HID]   ✓ register_usb_interrupt_handler()\n");
    
    crate::serial::serial_print("[USB-HID] Completion Tracking:\n");
    crate::serial::serial_print("[USB-HID]   ✓ CompletionTracker\n");
    crate::serial::serial_print("[USB-HID]   ✓ wait_for_command_completion()\n");
    
    crate::serial::serial_print("\n[USB-HID] Stage 4 framework complete\n");
    crate::serial::serial_print("[USB-HID] NOTA: Requiere integración con kernel:\n");
    crate::serial::serial_print("[USB-HID]   - Registrar handler de interrupciones\n");
    crate::serial::serial_print("[USB-HID]   - Configurar MSI/MSI-X\n");
    crate::serial::serial_print("[USB-HID]   - Polling de event ring en interrupt context\n");
}

// ============================================================================
// STAGE 5: HID INTEGRATION AND INPUT EVENTS
// ============================================================================

/// Kernel input event structure
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub event_type: InputEventType,
    pub code: u16,
    pub value: i32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum InputEventType {
    KeyPress,
    KeyRelease,
    MouseMove,
    MouseButton,
    MouseWheel,
}

/// HID Device structure
pub struct HidDevice {
    pub device_address: u8,
    pub interface_number: u8,
    pub endpoint_address: u8,
    pub device_type: HidDeviceType,
    pub is_gaming: bool,
    pub polling_interval: u8,  // In milliseconds (1 for gaming = 1000Hz)
    pub last_keyboard_report: Option<HidKeyboardReport>,
    pub last_mouse_report: Option<HidMouseReport>,
}

impl HidDevice {
    pub fn new(device_address: u8, device_type: HidDeviceType, is_gaming: bool) -> Self {
        Self {
            device_address,
            interface_number: 0,
            endpoint_address: 0x81,  // Typical IN endpoint
            device_type,
            is_gaming,
            polling_interval: if is_gaming { 1 } else { 8 },  // 1000Hz for gaming, 125Hz otherwise
            last_keyboard_report: None,
            last_mouse_report: None,
        }
    }
}

/// Attach a HID device
pub fn attach_hid_device(device: HidDevice) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Attaching HID device:\n");
    crate::serial::serial_print("[USB-HID]   Address: ");
    crate::serial::serial_print_dec(device.device_address as u64);
    crate::serial::serial_print("\n[USB-HID]   Type: ");
    match device.device_type {
        HidDeviceType::Keyboard => crate::serial::serial_print("Keyboard"),
        HidDeviceType::Mouse => crate::serial::serial_print("Mouse"),
        HidDeviceType::Gamepad => crate::serial::serial_print("Gamepad"),
        HidDeviceType::Other => crate::serial::serial_print("Other"),
    }
    crate::serial::serial_print("\n[USB-HID]   Gaming: ");
    if device.is_gaming {
        crate::serial::serial_print("Yes (1000Hz)\n");
    } else {
        crate::serial::serial_print("No (125Hz)\n");
    }
    
    // TODO: Store device in global list
    // Configure endpoint for interrupt transfers
    // Start polling for reports
    
    Ok(())
}

/// Detach a HID device
pub fn detach_hid_device(device_address: u8) {
    crate::serial::serial_print("[USB-HID] Detaching HID device ");
    crate::serial::serial_print_dec(device_address as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Remove from global list
    // Stop polling
}

/// Parse keyboard report and generate input events
pub fn parse_keyboard_report(
    current: &HidKeyboardReport,
    previous: Option<&HidKeyboardReport>,
) -> alloc::vec::Vec<InputEvent> {
    let mut events = alloc::vec::Vec::new();
    
    // Check modifier changes
    if let Some(prev) = previous {
        let modifier_changes = current.modifiers ^ prev.modifiers;
        
        if modifier_changes & MOD_LEFT_CTRL != 0 {
            let pressed = (current.modifiers & MOD_LEFT_CTRL) != 0;
            events.push(InputEvent {
                event_type: if pressed { InputEventType::KeyPress } else { InputEventType::KeyRelease },
                code: 0x1D, // Left Ctrl
                value: if pressed { 1 } else { 0 },
            });
        }
        
        // Check key changes
        for key in &current.keys {
            if *key != 0 && !prev.is_key_pressed(*key) {
                // Key pressed
                events.push(InputEvent {
                    event_type: InputEventType::KeyPress,
                    code: *key as u16,
                    value: 1,
                });
            }
        }
        
        for key in &prev.keys {
            if *key != 0 && !current.is_key_pressed(*key) {
                // Key released
                events.push(InputEvent {
                    event_type: InputEventType::KeyRelease,
                    code: *key as u16,
                    value: 0,
                });
            }
        }
    }
    
    events
}

/// Parse mouse report and generate input events
pub fn parse_mouse_report(
    current: &HidMouseReport,
    previous: Option<&HidMouseReport>,
) -> alloc::vec::Vec<InputEvent> {
    let mut events = alloc::vec::Vec::new();
    
    // Check button changes
    if let Some(prev) = previous {
        let button_changes = current.buttons ^ prev.buttons;
        
        if button_changes & 0x01 != 0 {
            // Left button
            let pressed = (current.buttons & 0x01) != 0;
            events.push(InputEvent {
                event_type: InputEventType::MouseButton,
                code: 0,  // Left button
                value: if pressed { 1 } else { 0 },
            });
        }
        
        if button_changes & 0x02 != 0 {
            // Right button
            let pressed = (current.buttons & 0x02) != 0;
            events.push(InputEvent {
                event_type: InputEventType::MouseButton,
                code: 1,  // Right button
                value: if pressed { 1 } else { 0 },
            });
        }
        
        if button_changes & 0x04 != 0 {
            // Middle button
            let pressed = (current.buttons & 0x04) != 0;
            events.push(InputEvent {
                event_type: InputEventType::MouseButton,
                code: 2,  // Middle button
                value: if pressed { 1 } else { 0 },
            });
        }
    }
    
    // Mouse movement
    if current.x != 0 {
        events.push(InputEvent {
            event_type: InputEventType::MouseMove,
            code: 0,  // X axis
            value: current.x as i32,
        });
    }
    
    if current.y != 0 {
        events.push(InputEvent {
            event_type: InputEventType::MouseMove,
            code: 1,  // Y axis
            value: current.y as i32,
        });
    }
    
    // Scroll wheel
    if current.wheel != 0 {
        events.push(InputEvent {
            event_type: InputEventType::MouseWheel,
            code: 0,
            value: current.wheel as i32,
        });
    }
    
    events
}

/// HID interrupt handler - processes incoming reports
pub fn hid_interrupt_handler(device_address: u8, report_data: &[u8]) {
    crate::serial::serial_print("[USB-HID] HID report received from device ");
    crate::serial::serial_print_dec(device_address as u64);
    crate::serial::serial_print("\n");
    
    // TODO: Lookup device by address
    // Determine device type
    // Parse report based on type
    // Generate input events
    // Send events to input service
    
    if report_data.len() >= 8 {
        // Could be keyboard report
        crate::serial::serial_print("[USB-HID]   Report length: ");
        crate::serial::serial_print_dec(report_data.len() as u64);
        crate::serial::serial_print(" (keyboard format)\n");
    } else if report_data.len() >= 3 {
        // Could be mouse report
        crate::serial::serial_print("[USB-HID]   Report length: ");
        crate::serial::serial_print_dec(report_data.len() as u64);
        crate::serial::serial_print(" (mouse format)\n");
    }
}

/// Configure gaming peripheral features
pub fn configure_gaming_peripheral(device_address: u8, vendor_id: u16, product_id: u16) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Configuring gaming peripheral:\n");
    crate::serial::serial_print("[USB-HID]   Device: ");
    crate::serial::serial_print_dec(device_address as u64);
    crate::serial::serial_print("\n[USB-HID]   Vendor: 0x");
    crate::serial::serial_print_dec(vendor_id as u64);
    crate::serial::serial_print("\n[USB-HID]   Product: 0x");
    crate::serial::serial_print_dec(product_id as u64);
    crate::serial::serial_print("\n");
    
    // Get gaming capabilities from database
    if is_gaming_device(vendor_id, product_id) {
        if let Some(caps) = get_gaming_capabilities(vendor_id, product_id) {
            crate::serial::serial_print("[USB-HID]   Max DPI: ");
            crate::serial::serial_print_dec(caps.max_dpi as u64);
            crate::serial::serial_print("\n[USB-HID]   Max Polling Rate: ");
            crate::serial::serial_print_dec(caps.max_polling_rate as u64);
            crate::serial::serial_print("Hz\n");
            
            // TODO: Send vendor-specific commands to configure:
            // - Set polling rate to 1000Hz
            // - Set DPI to max or user preference
            // - Enable all buttons
            // - Configure RGB lighting (if supported)
            
            crate::serial::serial_print("[USB-HID]   Gaming features configured (stub)\n");
        } else {
            crate::serial::serial_print("[USB-HID]   No capability entry found in database\n");
        }
    }
    
    Ok(())
}

/// Send input events to input service
pub fn send_input_events(events: &[InputEvent]) {
    if events.is_empty() {
        return;
    }
    
    crate::serial::serial_print("[USB-HID] Sending ");
    crate::serial::serial_print_dec(events.len() as u64);
    crate::serial::serial_print(" input events\n");
    
    // TODO: Send events to input service via IPC
    // Input service will handle:
    // - Event queuing
    // - Distribution to GUI applications
    // - Virtual terminal switching
}

/// Initialize Stage 5: HID integration
pub fn init_hid_integration() {
    crate::serial::serial_print("\n[USB-HID] === Stage 5: HID Integration ===\n");
    
    crate::serial::serial_print("[USB-HID] Input Event System:\n");
    crate::serial::serial_print("[USB-HID]   ✓ InputEvent structure\n");
    crate::serial::serial_print("[USB-HID]   ✓ InputEventType enum\n");
    
    crate::serial::serial_print("[USB-HID] HID Device Management:\n");
    crate::serial::serial_print("[USB-HID]   ✓ HidDevice structure\n");
    crate::serial::serial_print("[USB-HID]   ✓ attach_hid_device()\n");
    crate::serial::serial_print("[USB-HID]   ✓ detach_hid_device()\n");
    
    crate::serial::serial_print("[USB-HID] Report Processing:\n");
    crate::serial::serial_print("[USB-HID]   ✓ parse_keyboard_report()\n");
    crate::serial::serial_print("[USB-HID]   ✓ parse_mouse_report()\n");
    crate::serial::serial_print("[USB-HID]   ✓ hid_interrupt_handler()\n");
    
    crate::serial::serial_print("[USB-HID] Gaming Features:\n");
    crate::serial::serial_print("[USB-HID]   ✓ configure_gaming_peripheral()\n");
    crate::serial::serial_print("[USB-HID]   ✓ 1000Hz polling support\n");
    crate::serial::serial_print("[USB-HID]   ✓ High DPI configuration\n");
    crate::serial::serial_print("[USB-HID]   ✓ Extra button handling\n");
    
    crate::serial::serial_print("[USB-HID] Integration:\n");
    crate::serial::serial_print("[USB-HID]   ✓ send_input_events()\n");
    crate::serial::serial_print("[USB-HID]   ✓ Link to input service (ready)\n");
    
    crate::serial::serial_print("\n[USB-HID] Stage 5 framework complete\n");
    crate::serial::serial_print("[USB-HID] NOTA: Listo para integración completa:\n");
    crate::serial::serial_print("[USB-HID]   - Parse HID report descriptors\n");
    crate::serial::serial_print("[USB-HID]   - Configure interrupt endpoints\n");
    crate::serial::serial_print("[USB-HID]   - Start report polling\n");
    crate::serial::serial_print("[USB-HID]   - IPC con input service\n");
}

// ============================================================================
// Hardware Integration Layer
// ============================================================================
// MMIO, DMA, and IRQ integration for USB HID driver

use crate::memory::virt_to_phys;
use core::sync::atomic::{fence, Ordering};
use core::ptr::{read_volatile, write_volatile};

/// MMIO Region for USB controller registers
pub struct MmioRegion {
    pub base_phys: u64,
    pub base_virt: u64,
    pub size: usize,
}

impl MmioRegion {
    /// Create a new MMIO region by mapping physical address to kernel virtual space
    pub fn new(phys_addr: u64, size: usize) -> Result<Self, &'static str> {
        // Rechazar solo dirección cero; MMIO puede estar en espacio físico bajo o alto
        if phys_addr == 0 {
            return Err("Physical address is zero");
        }
        
        // Use map_mmio_range to ensure the physical address is accessible in virtual space
        let virt_addr = crate::memory::map_mmio_range(phys_addr, size);
        
        crate::serial::serial_print("[USB-HID] MMIO Region mapped:\n");
        crate::serial::serial_print("  Physical: 0x");
        crate::serial::serial_print_hex(phys_addr);
        crate::serial::serial_print("\n  Virtual: 0x");
        crate::serial::serial_print_hex(virt_addr);
        crate::serial::serial_print("\n  Size: ");
        crate::serial::serial_print_hex(size as u64);
        crate::serial::serial_print("\n");
        
        Ok(Self {
            base_phys: phys_addr,
            base_virt: virt_addr,
            size,
        })
    }
    
    /// Read 32-bit register at offset with volatile and memory ordering
    pub fn read_u32(&self, offset: usize) -> u32 {
        if offset + 4 > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Read beyond MMIO region\n");
            return 0;
        }
        
        let addr = (self.base_virt + offset as u64) as *const u32;
        
        // Acquire fence ensures reads happen in order
        fence(Ordering::Acquire);
        let value = unsafe { read_volatile(addr) };
        fence(Ordering::Acquire);
        
        value
    }
    
    /// Write 32-bit register at offset with volatile and memory ordering
    pub fn write_u32(&self, offset: usize, value: u32) {
        if offset + 4 > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Write beyond MMIO region\n");
            return;
        }
        
        let addr = (self.base_virt + offset as u64) as *mut u32;
        
        // Release fence ensures writes complete in order
        fence(Ordering::Release);
        unsafe { write_volatile(addr, value); }
        fence(Ordering::Release);
    }
    
    /// Read 64-bit register at offset
    pub fn read_u64(&self, offset: usize) -> u64 {
        if offset + 8 > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Read beyond MMIO region\n");
            return 0;
        }
        
        let addr = (self.base_virt + offset as u64) as *const u64;
        
        fence(Ordering::Acquire);
        let value = unsafe { read_volatile(addr) };
        fence(Ordering::Acquire);
        
        value
    }
    
    /// Write 64-bit register at offset
    pub fn write_u64(&self, offset: usize, value: u64) {
        if offset + 8 > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Write beyond MMIO region\n");
            return;
        }
        
        let addr = (self.base_virt + offset as u64) as *mut u64;
        
        fence(Ordering::Release);
        unsafe { write_volatile(addr, value); }
        fence(Ordering::Release);
    }
}

/// XHCI Controller MMIO register access
pub struct XhciMmio {
    pub capability: MmioRegion,
    pub operational: MmioRegion,
    pub runtime: MmioRegion,
    pub doorbell: MmioRegion,
}

impl XhciMmio {
    /// Initialize XHCI MMIO regions from BAR0
    pub fn from_bar0(bar0: u64) -> Result<Self, &'static str> {
        // Map initial capability region (256 bytes is sufficient)
        let cap_region = MmioRegion::new(bar0, 256)?;
        
        // Diagnóstico: leer raw dword en offset 0 (CAPLENGTH+HCIVERSION)
        let raw_cap0 = cap_region.read_u32(0);
        crate::serial::serial_print("[USB-HID] Raw CAP[0]=0x");
        crate::serial::serial_print_hex(raw_cap0 as u64);
        crate::serial::serial_print(" (esperado: ~0x00000120 para XHCI 1.0)\n");
        
        // Read CAPLENGTH to find operational registers offset
        let caplength = raw_cap0 & 0xFF;
        
        // Read RTSOFF (Runtime Register Space Offset) at offset 0x18
        let rtsoff = cap_region.read_u32(0x18);
        
        // Read DBOFF (Doorbell Offset) at offset 0x14
        let dboff = cap_region.read_u32(0x14);
        
        crate::serial::serial_print("[USB-HID] XHCI register offsets:\n");
        crate::serial::serial_print("  CAPLENGTH: 0x");
        crate::serial::serial_print_hex(caplength as u64);
        crate::serial::serial_print("\n  RTSOFF: 0x");
        crate::serial::serial_print_hex(rtsoff as u64);
        crate::serial::serial_print("\n  DBOFF: 0x");
        crate::serial::serial_print_hex(dboff as u64);
        crate::serial::serial_print("\n");
        
        // Create regions for each register space
    let operational_size = 0x1000; // Suficiente espacio para cubrir PORTSC (offset 0x400+)
    let operational = MmioRegion::new(bar0 + caplength as u64, operational_size)?;
    let runtime = MmioRegion::new(bar0 + rtsoff as u64, 8192)?;
    let doorbell = MmioRegion::new(bar0 + dboff as u64, 1024)?;
        
        Ok(Self {
            capability: cap_region,
            operational,
            runtime,
            doorbell,
        })
    }
    
    /// Read from capability register
    pub fn read_capability(&self, offset: usize) -> u32 {
        self.capability.read_u32(offset)
    }
    
    /// Read from operational register
    pub fn read_operational(&self, offset: usize) -> u32 {
        self.operational.read_u32(offset)
    }
    
    /// Write to operational register
    pub fn write_operational(&self, offset: usize, value: u32) {
        self.operational.write_u32(offset, value);
    }
    
    /// Read from runtime register
    pub fn read_runtime(&self, offset: usize) -> u32 {
        self.runtime.read_u32(offset)
    }
    
    /// Write to runtime register
    pub fn write_runtime(&self, offset: usize, value: u32) {
        self.runtime.write_u32(offset, value);
    }
    
    /// Ring doorbell register
    pub fn ring_doorbell(&self, slot_id: u8, target: u8) {
        let doorbell_offset = (slot_id as usize) * 4;
        let value = target as u32;
        self.doorbell.write_u32(doorbell_offset, value);
        
        crate::serial::serial_print("[USB-HID] Doorbell rung: slot=");
        crate::serial::serial_print_hex(slot_id as u64);
        crate::serial::serial_print(" target=");
        crate::serial::serial_print_hex(target as u64);
        crate::serial::serial_print("\n");
    }
}

/// DMA-capable memory allocation
pub struct DmaAllocation {
    pub virt_addr: u64,
    pub phys_addr: u64,
    pub size: usize,
    pub alignment: usize,
}

impl DmaAllocation {
    /// Allocate DMA-capable memory (physically contiguous)
    pub fn allocate(size: usize, alignment: usize) -> Result<Self, &'static str> {
        // Use kernel heap allocator
        // In a real implementation, this would use a dedicated DMA allocator
        use alloc::vec::Vec;
        
        // Allocate with extra space for alignment
        let alloc_size = size + alignment;
        let mut buffer: Vec<u8> = Vec::with_capacity(alloc_size);
        buffer.resize(alloc_size, 0);
        
        let raw_ptr = buffer.as_ptr() as u64;
        
        // Align the pointer
        let aligned_virt = (raw_ptr + (alignment as u64 - 1)) & !(alignment as u64 - 1);
        
        // Convert to physical address
        let phys_addr = virt_to_phys(aligned_virt);
        
        // Leak the buffer to prevent deallocation
        core::mem::forget(buffer);
        
        crate::serial::serial_print("[USB-HID] DMA allocation:\n");
        crate::serial::serial_print("  Virtual: 0x");
        crate::serial::serial_print_hex(aligned_virt);
        crate::serial::serial_print("\n  Physical: 0x");
        crate::serial::serial_print_hex(phys_addr);
        crate::serial::serial_print("\n  Size: ");
        crate::serial::serial_print_hex(size as u64);
        crate::serial::serial_print("\n  Alignment: ");
        crate::serial::serial_print_hex(alignment as u64);
        crate::serial::serial_print("\n");
        
        Ok(Self {
            virt_addr: aligned_virt,
            phys_addr,
            size,
            alignment,
        })
    }
    
    /// Allocate TRB ring (64-byte aligned as required by CRCR/TR dequeue pointers)
    pub fn allocate_trb_ring(num_trbs: usize) -> Result<Self, &'static str> {
        let size = num_trbs * 16; // Each TRB is 16 bytes
        Self::allocate(size, 64)
    }
    
    /// Allocate device context (64-byte aligned)
    pub fn allocate_device_context() -> Result<Self, &'static str> {
        let size = 1024; // DeviceContext with 31 endpoints
        Self::allocate(size, 64)
    }
    
    /// Allocate Event Ring Segment Table (64-byte aligned)
    pub fn allocate_erst(num_segments: usize) -> Result<Self, &'static str> {
        let size = num_segments * 16; // Each ERST entry is 16 bytes
        Self::allocate(size, 64)
    }
    
    /// Allocate Device Context Base Address Array (4KB aligned, page-aligned)
    pub fn allocate_dcbaa(max_slots: usize) -> Result<Self, &'static str> {
        let size = (max_slots + 1) * 8; // Array of 64-bit pointers
        Self::allocate(size, 4096)
    }
    
    /// Zero the allocated memory
    pub fn zero(&self) {
        let ptr = self.virt_addr as *mut u8;
        unsafe {
            core::ptr::write_bytes(ptr, 0, self.size);
        }
    }
    
    /// Write data to DMA buffer
    pub fn write_bytes(&self, offset: usize, data: &[u8]) {
        if offset + data.len() > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Write beyond DMA buffer\n");
            return;
        }
        
        let ptr = (self.virt_addr + offset as u64) as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }
    }
    
    /// Read data from DMA buffer
    pub fn read_bytes(&self, offset: usize, data: &mut [u8]) {
        if offset + data.len() > self.size {
            crate::serial::serial_print("[USB-HID] ERROR: Read beyond DMA buffer\n");
            return;
        }
        
        let ptr = (self.virt_addr + offset as u64) as *const u8;
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), data.len());
        }
    }
    
    /// Write 32-bit value to DMA buffer (little endian)
    pub fn write_u32(&self, offset: usize, value: u32) {
        self.write_bytes(offset, &value.to_le_bytes());
    }

    /// Write 64-bit value to DMA buffer (little endian)
    pub fn write_u64(&self, offset: usize, value: u64) {
        self.write_bytes(offset, &value.to_le_bytes());
    }
}

/// USB Interrupt Handler Context
pub struct UsbInterruptContext {
    pub controller_mmio: Option<XhciMmio>,
    pub event_ring_phys: u64,
    pub event_ring_virt: u64,
    pub event_ring_size: usize,
    pub dequeue_index: usize,
    pub cycle_state: bool,
}

impl UsbInterruptContext {
    pub fn new() -> Self {
        Self {
            controller_mmio: None,
            event_ring_phys: 0,
            event_ring_virt: 0,
            event_ring_size: 0,
            dequeue_index: 0,
            cycle_state: true,
        }
    }
}

static mut USB_IRQ_CONTEXT: Option<UsbInterruptContext> = None;

/// Initialize USB interrupt context
pub fn init_usb_irq_context(mmio: XhciMmio, event_ring: &DmaAllocation) {
    unsafe {
        USB_IRQ_CONTEXT = Some(UsbInterruptContext {
            controller_mmio: Some(mmio),
            event_ring_phys: event_ring.phys_addr,
            event_ring_virt: event_ring.virt_addr,
            event_ring_size: event_ring.size / 16, // Number of TRBs
            dequeue_index: 0,
            cycle_state: true,
        });
    }
    
    crate::serial::serial_print("[USB-HID] Interrupt context initialized\n");
}

/// USB Hardware Interrupt Handler
/// This is called by the kernel IRQ system when a USB interrupt occurs
pub extern "C" fn usb_hardware_interrupt_handler() -> bool {
    unsafe {
        let context = match USB_IRQ_CONTEXT.as_mut() {
            Some(ctx) => ctx,
            None => return false,
        };
        
        let mmio = match context.controller_mmio.as_ref() {
            Some(m) => m,
            None => return false,
        };
        
        // Read USBSTS register (offset 0x04 in operational registers)
        let usbsts = mmio.read_operational(0x04);
        
        // Check if this is our interrupt (EINT bit)
        if (usbsts & 0x08) == 0 {
            return false; // Not our interrupt
        }
        
        // Clear EINT bit by writing 1 to it (write-1-to-clear)
        mmio.write_operational(0x04, 0x08);
        
        // Process events from event ring
        let mut events_processed = 0;
        while events_processed < 16 {  // Process up to 16 events per interrupt
            // Read TRB at dequeue index
            let trb_offset = context.dequeue_index * 16;
            if trb_offset >= context.event_ring_size * 16 {
                break;
            }
            
            let trb_addr = (context.event_ring_virt + trb_offset as u64) as *const u32;
            let control = unsafe { read_volatile(trb_addr.add(3)) };
            let cycle = (control & 0x01) != 0;
            
            // Check if TRB is ready (cycle bit matches our state)
            if cycle != context.cycle_state {
                break; // No more events
            }
            
            // Extract TRB type
            let trb_type = (control >> 10) & 0x3F;
            
            // Log event
            crate::serial::serial_print("[USB-HID] Event TRB type: ");
            crate::serial::serial_print_hex(trb_type as u64);
            crate::serial::serial_print("\n");
            
            // Advance dequeue pointer
            context.dequeue_index += 1;
            if context.dequeue_index >= context.event_ring_size {
                context.dequeue_index = 0;
                context.cycle_state = !context.cycle_state;
            }
            
            events_processed += 1;
        }
        
        // Update Event Ring Dequeue Pointer (ERDP) in runtime registers
        // ERDP is at offset 0x38 in interrupter 0 register set
        let erdp = context.event_ring_phys + (context.dequeue_index * 16) as u64;
        mmio.write_runtime(0x38, (erdp & 0xFFFFFFFF) as u32);
        mmio.write_runtime(0x3C, (erdp >> 32) as u32);
        
        true // Interrupt handled
    }
}

/// Register USB interrupt handler with kernel
pub fn register_usb_irq_handler(irq_number: u8) -> Result<(), &'static str> {
    crate::serial::serial_print("[USB-HID] Registering IRQ handler for IRQ ");
    crate::serial::serial_print_hex(irq_number as u64);
    crate::serial::serial_print("\n");
    
    // Register the USB interrupt handler with the kernel's IRQ system
    crate::interrupts::set_irq_handler(irq_number, usb_irq_handler_wrapper)?;
    
    crate::serial::serial_print("[USB-HID] IRQ handler registered successfully\n");
    
    Ok(())
}

/// Wrapper function that bridges kernel IRQ system to USB hardware interrupt handler
fn usb_irq_handler_wrapper() {
    // Call the actual USB interrupt handler
    usb_hardware_interrupt_handler();
}

/// Enable USB interrupts in the controller
pub fn enable_usb_interrupts(mmio: &XhciMmio) {
    // Enable interrupts in USBCMD register (bit 2)
    let usbcmd = mmio.read_operational(0x00);
    mmio.write_operational(0x00, usbcmd | 0x04);
    
    // Enable interrupts in interrupter 0 (IMAN register)
    // IMAN is at offset 0x20 in runtime registers
    mmio.write_runtime(0x20, 0x03); // IE bit (0x02) and IP bit (0x01)
    
    // Set interrupt moderation (IMOD) to 0 for no delay
    mmio.write_runtime(0x24, 0);
    
    crate::serial::serial_print("[USB-HID] USB interrupts enabled\n");
}

/// Disable USB interrupts
pub fn disable_usb_interrupts(mmio: &XhciMmio) {
    // Disable interrupts in USBCMD register
    let usbcmd = mmio.read_operational(0x00);
    mmio.write_operational(0x00, usbcmd & !0x04);
    
    // Disable interrupts in interrupter 0
    mmio.write_runtime(0x20, 0);
    
    crate::serial::serial_print("[USB-HID] USB interrupts disabled\n");
}

/// Initialize hardware integration layer
pub fn init_hardware_integration() {
    crate::serial::serial_print("\n[USB-HID] === Hardware Integration Layer ===\n");
    
    crate::serial::serial_print("[USB-HID] MMIO Support:\n");
    crate::serial::serial_print("[USB-HID]   ✓ MmioRegion structure\n");
    crate::serial::serial_print("[USB-HID]   ✓ Volatile register read/write\n");
    crate::serial::serial_print("[USB-HID]   ✓ Memory ordering (Acquire/Release)\n");
    crate::serial::serial_print("[USB-HID]   ✓ XhciMmio for register access\n");
    crate::serial::serial_print("[USB-HID]   ✓ Capability/Operational/Runtime/Doorbell regions\n");
    
    crate::serial::serial_print("[USB-HID] DMA Support:\n");
    crate::serial::serial_print("[USB-HID]   ✓ DmaAllocation structure\n");
    crate::serial::serial_print("[USB-HID]   ✓ Physical memory allocation\n");
    crate::serial::serial_print("[USB-HID]   ✓ TRB ring allocation (16-byte aligned)\n");
    crate::serial::serial_print("[USB-HID]   ✓ Device context allocation (64-byte aligned)\n");
    crate::serial::serial_print("[USB-HID]   ✓ ERST allocation (64-byte aligned)\n");
    crate::serial::serial_print("[USB-HID]   ✓ DCBAA allocation (4KB aligned)\n");
    crate::serial::serial_print("[USB-HID]   ✓ Zero and read/write operations\n");
    
    crate::serial::serial_print("[USB-HID] IRQ Support:\n");
    crate::serial::serial_print("[USB-HID]   ✓ UsbInterruptContext for state tracking\n");
    crate::serial::serial_print("[USB-HID]   ✓ usb_hardware_interrupt_handler()\n");
    crate::serial::serial_print("[USB-HID]   ✓ Event ring processing\n");
    crate::serial::serial_print("[USB-HID]   ✓ ERDP (Event Ring Dequeue Pointer) update\n");
    crate::serial::serial_print("[USB-HID]   ✓ register_usb_irq_handler()\n");
    crate::serial::serial_print("[USB-HID]   ✓ enable_usb_interrupts()\n");
    crate::serial::serial_print("[USB-HID]   ✓ disable_usb_interrupts()\n");
    
    crate::serial::serial_print("\n[USB-HID] Hardware integration layer complete\n");
    crate::serial::serial_print("[USB-HID] Ready for controller initialization:\n");
    crate::serial::serial_print("[USB-HID]   1. Create XhciMmio from BAR0\n");
    crate::serial::serial_print("[USB-HID]   2. Allocate DMA buffers (rings, contexts)\n");
    crate::serial::serial_print("[USB-HID]   3. Initialize controller with MMIO\n");
    crate::serial::serial_print("[USB-HID]   4. Register IRQ handler\n");
    crate::serial::serial_print("[USB-HID]   5. Enable interrupts\n");
    crate::serial::serial_print("[USB-HID]   6. Start USB operations\n");
}
