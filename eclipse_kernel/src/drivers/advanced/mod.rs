//! Drivers Avanzados para ReactOS Rust
//! 
//! Implementa drivers avanzados para USB, Audio y Video
//! con soporte completo para hardware moderno.

pub mod usb;
pub mod audio;
pub mod video;
pub mod pci;
pub mod acpi;

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

/// Estado del sistema de drivers avanzados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AdvancedDriverState {
    Uninitialized,
    Initializing,
    Initialized,
    Error,
    Shutdown,
}

/// Configuración de drivers avanzados
#[derive(Debug, Clone, Copy)]
pub struct AdvancedDriverConfig {
    pub enable_usb: bool,
    pub enable_audio: bool,
    pub enable_video: bool,
    pub enable_pci: bool,
    pub enable_acpi: bool,
    pub usb_polling_interval_ms: u32,
    pub audio_buffer_size: usize,
    pub video_acceleration: bool,
    pub debug_mode: bool,
}

impl AdvancedDriverConfig {
    pub fn default() -> Self {
        Self {
            enable_usb: true,
            enable_audio: true,
            enable_video: true,
            enable_pci: true,
            enable_acpi: true,
            usb_polling_interval_ms: 10,
            audio_buffer_size: 4096,
            video_acceleration: true,
            debug_mode: false,
        }
    }
    
    pub fn minimal() -> Self {
        Self {
            enable_usb: false,
            enable_audio: false,
            enable_video: false,
            enable_pci: true,
            enable_acpi: true,
            usb_polling_interval_ms: 100,
            audio_buffer_size: 1024,
            video_acceleration: false,
            debug_mode: false,
        }
    }
    
    pub fn high_performance() -> Self {
        Self {
            enable_usb: true,
            enable_audio: true,
            enable_video: true,
            enable_pci: true,
            enable_acpi: true,
            usb_polling_interval_ms: 5,
            audio_buffer_size: 8192,
            video_acceleration: true,
            debug_mode: false,
        }
    }
}

/// Estadísticas de drivers avanzados
#[derive(Debug, Clone, Copy)]
pub struct AdvancedDriverStats {
    pub usb_devices_connected: u32,
    pub audio_streams_active: u32,
    pub video_displays_active: u32,
    pub pci_devices_found: u32,
    pub acpi_tables_loaded: u32,
    pub total_interrupts: u64,
    pub total_errors: u64,
    pub last_error_code: u32,
}

/// Gestor de drivers avanzados
pub struct AdvancedDriverManager {
    pub state: AtomicU32,
    pub config: AdvancedDriverConfig,
    pub stats: AdvancedDriverStats,
    pub is_initialized: AtomicBool,
    pub error_count: AtomicU32,
}

impl AdvancedDriverManager {
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(AdvancedDriverState::Uninitialized as u32),
            config: AdvancedDriverConfig::default(),
            stats: AdvancedDriverStats {
                usb_devices_connected: 0,
                audio_streams_active: 0,
                video_displays_active: 0,
                pci_devices_found: 0,
                acpi_tables_loaded: 0,
                total_interrupts: 0,
                total_errors: 0,
                last_error_code: 0,
            },
            is_initialized: AtomicBool::new(false),
            error_count: AtomicU32::new(0),
        }
    }
    
    /// Inicializar drivers avanzados
    pub fn init(&mut self) -> Result<(), &'static str> {
        if self.is_initialized.load(Ordering::Relaxed) {
            return Ok(());
        }
        
        self.state.store(AdvancedDriverState::Initializing as u32, Ordering::Relaxed);
        
        // Inicializar PCI primero (necesario para otros drivers)
        if self.config.enable_pci {
            match pci::init_pci() {
                Ok(device_count) => {
                    self.stats.pci_devices_found = device_count;
                }
                Err(e) => {
                    self.state.store(AdvancedDriverState::Error as u32, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        
        // Inicializar ACPI
        if self.config.enable_acpi {
            match acpi::init_acpi() {
                Ok(table_count) => {
                    self.stats.acpi_tables_loaded = table_count;
                }
                Err(e) => {
                    self.state.store(AdvancedDriverState::Error as u32, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        
        // Inicializar USB
        if self.config.enable_usb {
            match usb::init_usb() {
                Ok(device_count) => {
                    self.stats.usb_devices_connected = device_count;
                }
                Err(e) => {
                    self.state.store(AdvancedDriverState::Error as u32, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        
        // Inicializar Audio
        if self.config.enable_audio {
            match audio::init_audio() {
                Ok(stream_count) => {
                    self.stats.audio_streams_active = stream_count;
                }
                Err(e) => {
                    self.state.store(AdvancedDriverState::Error as u32, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        
        // Inicializar Video
        if self.config.enable_video {
            match video::init_video() {
                Ok(display_count) => {
                    self.stats.video_displays_active = display_count;
                }
                Err(e) => {
                    self.state.store(AdvancedDriverState::Error as u32, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }
        
        self.state.store(AdvancedDriverState::Initialized as u32, Ordering::Relaxed);
        self.is_initialized.store(true, Ordering::Relaxed);
        
        Ok(())
    }
    
    /// Procesar eventos de drivers avanzados
    pub fn process_events(&mut self) {
        if !self.is_initialized.load(Ordering::Relaxed) {
            return;
        }
        
        // Procesar eventos USB
        if self.config.enable_usb {
            if let Err(e) = usb::process_usb_events() {
                self.handle_error(e);
            }
        }
        
        // Procesar eventos de Audio
        if self.config.enable_audio {
            if let Err(e) = audio::process_audio_events() {
                self.handle_error(e);
            }
        }
        
        // Procesar eventos de Video
        if self.config.enable_video {
            if let Err(e) = video::process_video_events() {
                self.handle_error(e);
            }
        }
        
        // Procesar eventos PCI
        if self.config.enable_pci {
            if let Err(e) = pci::process_pci_events() {
                self.handle_error(e);
            }
        }
        
        // Procesar eventos ACPI
        if self.config.enable_acpi {
            if let Err(e) = acpi::process_acpi_events() {
                self.handle_error(e);
            }
        }
    }
    
    /// Manejar errores
    fn handle_error(&mut self, error_code: u32) {
        self.stats.total_errors += 1;
        self.stats.last_error_code = error_code;
        self.error_count.fetch_add(1, Ordering::Relaxed);
        
        if self.config.debug_mode {
            // TODO: Implementar logging de errores
        }
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> AdvancedDriverStats {
        self.stats
    }
    
    /// Establecer configuración
    pub fn set_config(&mut self, config: AdvancedDriverConfig) {
        self.config = config;
    }
    
    /// Obtener estado
    pub fn get_state(&self) -> AdvancedDriverState {
        match self.state.load(Ordering::Relaxed) {
            0 => AdvancedDriverState::Uninitialized,
            1 => AdvancedDriverState::Initializing,
            2 => AdvancedDriverState::Initialized,
            3 => AdvancedDriverState::Error,
            4 => AdvancedDriverState::Shutdown,
            _ => AdvancedDriverState::Error,
        }
    }
    
    /// Shutdown de drivers avanzados
    pub fn shutdown(&mut self) {
        if !self.is_initialized.load(Ordering::Relaxed) {
            return;
        }
        
        self.state.store(AdvancedDriverState::Shutdown as u32, Ordering::Relaxed);
        
        // Shutdown en orden inverso
        if self.config.enable_video {
            let _ = video::shutdown_video();
        }
        
        if self.config.enable_audio {
            let _ = audio::shutdown_audio();
        }
        
        if self.config.enable_usb {
            let _ = usb::shutdown_usb();
        }
        
        if self.config.enable_acpi {
            let _ = acpi::shutdown_acpi();
        }
        
        if self.config.enable_pci {
            let _ = pci::shutdown_pci();
        }
        
        self.is_initialized.store(false, Ordering::Relaxed);
    }
}

/// Gestor global de drivers avanzados
static mut ADVANCED_DRIVER_MANAGER: Option<AdvancedDriverManager> = None;

/// Inicializar drivers avanzados
pub fn init_advanced_drivers() -> Result<(), &'static str> {
    let mut manager = AdvancedDriverManager::new();
    manager.init()?;
    
    unsafe {
        ADVANCED_DRIVER_MANAGER = Some(manager);
    }
    
    Ok(())
}

/// Obtener gestor de drivers avanzados
pub fn get_advanced_driver_manager() -> Option<&'static mut AdvancedDriverManager> {
    unsafe {
        ADVANCED_DRIVER_MANAGER.as_mut()
    }
}

/// Procesar eventos de drivers avanzados
pub fn process_advanced_driver_events() {
    if let Some(manager) = get_advanced_driver_manager() {
        manager.process_events();
    }
}

/// Obtener estadísticas de drivers avanzados
pub fn get_advanced_driver_stats() -> Option<AdvancedDriverStats> {
    if let Some(manager) = get_advanced_driver_manager() {
        Some(manager.get_stats())
    } else {
        None
    }
}

/// Establecer configuración de drivers avanzados
pub fn set_advanced_driver_config(config: AdvancedDriverConfig) {
    if let Some(manager) = get_advanced_driver_manager() {
        manager.set_config(config);
    }
}

/// Obtener estado de drivers avanzados
pub fn get_advanced_driver_state() -> Option<AdvancedDriverState> {
    if let Some(manager) = get_advanced_driver_manager() {
        Some(manager.get_state())
    } else {
        None
    }
}

/// Shutdown de drivers avanzados
pub fn shutdown_advanced_drivers() {
    if let Some(manager) = get_advanced_driver_manager() {
        manager.shutdown();
    }
}
