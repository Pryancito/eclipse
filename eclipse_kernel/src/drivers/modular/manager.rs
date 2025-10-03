//! Gestor avanzado de drivers modulares
//!
//! Proporciona herramientas avanzadas para gestionar drivers modulares,
//! incluyendo configuración, monitoreo y control.

use super::{
    get_modular_driver, list_modular_drivers, Capability, DriverError, DriverInfo, ModularDriver,
};

/// Estado de un driver
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriverState {
    Uninitialized,
    Initialized,
    Running,
    Error,
    Stopped,
}

/// Configuración de driver
#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub auto_start: bool,
    pub priority: u8,
    pub timeout_ms: u32,
    pub retry_count: u8,
}

/// Estadísticas de driver
#[derive(Debug, Clone, Copy)]
pub struct DriverStats {
    pub init_time_ms: u32,
    pub last_error: Option<DriverError>,
    pub error_count: u32,
    pub uptime_ms: u32,
}

/// Información completa de driver
#[derive(Debug, Clone)]
pub struct DriverInfoComplete {
    pub basic_info: DriverInfo,
    pub state: DriverState,
    pub config: DriverConfig,
    pub stats: DriverStats,
}

/// Gestor avanzado de drivers
pub struct AdvancedDriverManager {
    driver_states: heapless::Vec<DriverState, 8>,
    driver_configs: heapless::Vec<DriverConfig, 8>,
    driver_stats: heapless::Vec<DriverStats, 8>,
}

impl AdvancedDriverManager {
    /// Crear nuevo gestor avanzado
    pub const fn new() -> Self {
        Self {
            driver_states: heapless::Vec::new(),
            driver_configs: heapless::Vec::new(),
            driver_stats: heapless::Vec::new(),
        }
    }

    /// Inicializar gestor
    pub fn init(&mut self) -> Result<(), DriverError> {
        let drivers = list_modular_drivers();

        // Inicializar estados y configuraciones por defecto
        for _ in 0..drivers.len() {
            let _ = self.driver_states.push(DriverState::Uninitialized);
            let _ = self.driver_configs.push(DriverConfig {
                auto_start: true,
                priority: 5,
                timeout_ms: 5000,
                retry_count: 3,
            });
            let _ = self.driver_stats.push(DriverStats {
                init_time_ms: 0,
                last_error: None,
                error_count: 0,
                uptime_ms: 0,
            });
        }

        Ok(())
    }

    /// Obtener información completa de todos los drivers
    pub fn get_all_driver_info(&self) -> heapless::Vec<DriverInfoComplete, 8> {
        let mut info_list = heapless::Vec::new();
        let drivers = list_modular_drivers();

        for (i, driver_name) in drivers.iter().enumerate() {
            if let Some(driver) = get_modular_driver(driver_name.as_str()) {
                let basic_info = driver.get_info();
                let state = self
                    .driver_states
                    .get(i)
                    .copied()
                    .unwrap_or(DriverState::Uninitialized);
                let config = self.driver_configs.get(i).cloned().unwrap_or(DriverConfig {
                    auto_start: true,
                    priority: 5,
                    timeout_ms: 5000,
                    retry_count: 3,
                });
                let stats = self.driver_stats.get(i).copied().unwrap_or(DriverStats {
                    init_time_ms: 0,
                    last_error: None,
                    error_count: 0,
                    uptime_ms: 0,
                });

                let complete_info = DriverInfoComplete {
                    basic_info,
                    state,
                    config,
                    stats,
                };

                let _ = info_list.push(complete_info);
            }
        }

        info_list
    }

    /// Inicializar driver específico
    pub fn init_driver(&mut self, driver_name: &str) -> Result<(), DriverError> {
        if let Some(driver) = get_modular_driver(driver_name) {
            let start_time = 0; // En una implementación real, esto sería el tiempo actual

            match driver.init() {
                Ok(_) => {
                    self.update_driver_state(driver_name, DriverState::Initialized);
                    self.update_driver_stats(driver_name, |stats| {
                        stats.init_time_ms = 100; // Simulado
                        stats.last_error = None;
                    });
                    Ok(())
                }
                Err(e) => {
                    self.update_driver_state(driver_name, DriverState::Error);
                    self.update_driver_stats(driver_name, |stats| {
                        stats.last_error = Some(e);
                        stats.error_count += 1;
                    });
                    Err(e)
                }
            }
        } else {
            Err(DriverError::NotAvailable)
        }
    }

    /// Detener driver específico
    pub fn stop_driver(&mut self, driver_name: &str) -> Result<(), DriverError> {
        if let Some(driver) = get_modular_driver(driver_name) {
            driver.close();
            self.update_driver_state(driver_name, DriverState::Stopped);
            Ok(())
        } else {
            Err(DriverError::NotAvailable)
        }
    }

    /// Configurar driver específico
    pub fn configure_driver(
        &mut self,
        driver_name: &str,
        config: DriverConfig,
    ) -> Result<(), DriverError> {
        let drivers = list_modular_drivers();
        if let Some(index) = drivers.iter().position(|name| name.as_str() == driver_name) {
            if let Some(driver_config) = self.driver_configs.get_mut(index) {
                *driver_config = config;
                Ok(())
            } else {
                Err(DriverError::NotAvailable)
            }
        } else {
            Err(DriverError::NotAvailable)
        }
    }

    /// Obtener estado de driver
    pub fn get_driver_state(&self, driver_name: &str) -> Option<DriverState> {
        let drivers = list_modular_drivers();
        if let Some(index) = drivers.iter().position(|name| name.as_str() == driver_name) {
            self.driver_states.get(index).copied()
        } else {
            None
        }
    }

    /// Actualizar estado de driver
    fn update_driver_state(&mut self, driver_name: &str, state: DriverState) {
        let drivers = list_modular_drivers();
        if let Some(index) = drivers.iter().position(|name| name.as_str() == driver_name) {
            if let Some(driver_state) = self.driver_states.get_mut(index) {
                *driver_state = state;
            }
        }
    }

    /// Actualizar estadísticas de driver
    fn update_driver_stats<F>(&mut self, driver_name: &str, updater: F)
    where
        F: FnOnce(&mut DriverStats),
    {
        let drivers = list_modular_drivers();
        if let Some(index) = drivers.iter().position(|name| name.as_str() == driver_name) {
            if let Some(stats) = self.driver_stats.get_mut(index) {
                updater(stats);
            }
        }
    }

    /// Obtener resumen del sistema
    pub fn get_system_summary(&self) -> SystemSummary {
        let mut total_drivers = 0;
        let mut initialized_drivers = 0;
        let mut error_drivers = 0;
        let mut total_errors = 0;

        for state in self.driver_states.iter() {
            total_drivers += 1;
            match state {
                DriverState::Initialized | DriverState::Running => initialized_drivers += 1,
                DriverState::Error => error_drivers += 1,
                _ => {}
            }
        }

        for stats in self.driver_stats.iter() {
            total_errors += stats.error_count;
        }

        SystemSummary {
            total_drivers,
            initialized_drivers,
            error_drivers,
            total_errors,
        }
    }
}

/// Resumen del sistema
#[derive(Debug, Clone, Copy)]
pub struct SystemSummary {
    pub total_drivers: usize,
    pub initialized_drivers: usize,
    pub error_drivers: usize,
    pub total_errors: u32,
}

/// Instancia global del gestor avanzado
static mut ADVANCED_DRIVER_MANAGER: AdvancedDriverManager = AdvancedDriverManager::new();

/// Obtener instancia del gestor avanzado
pub fn get_advanced_driver_manager() -> &'static mut AdvancedDriverManager {
    unsafe { &mut ADVANCED_DRIVER_MANAGER }
}

/// Inicializar gestor avanzado
pub fn init_advanced_driver_manager() -> Result<(), DriverError> {
    unsafe { ADVANCED_DRIVER_MANAGER.init() }
}

/// Obtener resumen del sistema
pub fn get_system_summary() -> SystemSummary {
    unsafe { ADVANCED_DRIVER_MANAGER.get_system_summary() }
}
