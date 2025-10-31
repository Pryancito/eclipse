//! Demostración del sistema de drivers modulares
//!
//! Muestra información sobre los drivers modulares registrados
//! y permite probar sus funcionalidades.

use super::{get_modular_driver, list_modular_drivers, DriverError, ModularDriver};

/// Información de demostración de drivers
pub struct DriverDemo {
    demo_active: bool,
    current_driver: Option<heapless::String<32>>,
}

impl DriverDemo {
    /// Crear nueva demostración
    pub const fn new() -> Self {
        Self {
            demo_active: false,
            current_driver: None,
        }
    }

    /// Iniciar demostración
    pub fn start_demo(&mut self) -> Result<(), DriverError> {
        self.demo_active = true;
        Ok(())
    }

    /// Detener demostración
    pub fn stop_demo(&mut self) {
        self.demo_active = false;
        self.current_driver = None;
    }

    /// Mostrar información de todos los drivers
    pub fn show_all_drivers_info(&self) -> heapless::Vec<DriverInfo, 8> {
        let mut info_list = heapless::Vec::new();
        let drivers = list_modular_drivers();

        for driver_name in drivers.iter() {
            if let Some(driver) = get_modular_driver(driver_name.as_str()) {
                let info = driver.get_info();
                let _ = info_list.push(info);
            }
        }

        info_list
    }

    /// Probar driver específico
    pub fn test_driver(&mut self, driver_name: &str) -> Result<DriverTestResult, DriverError> {
        if let Some(driver) = get_modular_driver(driver_name) {
            let mut name = heapless::String::<32>::new();
            let _ = name.push_str(driver_name);
            self.current_driver = Some(name);

            // Probar funcionalidades básicas del driver
            let mut result = DriverTestResult {
                driver_name: {
                    let mut name = heapless::String::<32>::new();
                    let _ = name.push_str(driver_name);
                    name
                },
                is_available: driver.is_available(),
                init_success: false,
                capabilities: heapless::Vec::new(),
                test_passed: 0,
                test_total: 0,
            };

            // Obtener información del driver
            let info = driver.get_info();
            result.capabilities = info.capabilities;

            // Probar inicialización
            if let Ok(_) = driver.init() {
                result.init_success = true;
                result.test_passed += 1;
            }
            result.test_total += 1;

            // Probar disponibilidad
            if driver.is_available() {
                result.test_passed += 1;
            }
            result.test_total += 1;

            Ok(result)
        } else {
            Err(DriverError::NotAvailable)
        }
    }

    /// Ejecutar demostración completa
    pub fn run_full_demo(&mut self) -> Result<DemoResults, DriverError> {
        if !self.demo_active {
            return Err(DriverError::NotAvailable);
        }

        let mut results = DemoResults {
            total_drivers: 0,
            successful_drivers: 0,
            failed_drivers: 0,
            driver_tests: heapless::Vec::new(),
        };

        let drivers = list_modular_drivers();
        results.total_drivers = drivers.len();

        for driver_name in drivers.iter() {
            match self.test_driver(driver_name.as_str()) {
                Ok(test_result) => {
                    if test_result.init_success && test_result.is_available {
                        results.successful_drivers += 1;
                    } else {
                        results.failed_drivers += 1;
                    }
                    let _ = results.driver_tests.push(test_result);
                }
                Err(_) => {
                    results.failed_drivers += 1;
                }
            }
        }

        Ok(results)
    }

    /// Verificar si la demostración está activa
    pub fn is_demo_active(&self) -> bool {
        self.demo_active
    }
}

/// Resultado de prueba de driver
#[derive(Debug, Clone)]
pub struct DriverTestResult {
    pub driver_name: heapless::String<32>,
    pub is_available: bool,
    pub init_success: bool,
    pub capabilities: heapless::Vec<super::Capability, 16>,
    pub test_passed: u32,
    pub test_total: u32,
}

/// Resultados de la demostración
#[derive(Debug, Clone)]
pub struct DemoResults {
    pub total_drivers: usize,
    pub successful_drivers: usize,
    pub failed_drivers: usize,
    pub driver_tests: heapless::Vec<DriverTestResult, 8>,
}

/// Información de driver (alias para evitar conflictos)
pub type DriverInfo = super::DriverInfo;

/// Instancia global de la demostración
static mut DRIVER_DEMO: DriverDemo = DriverDemo::new();

/// Obtener instancia de la demostración
pub fn get_driver_demo() -> &'static mut DriverDemo {
    unsafe { &mut DRIVER_DEMO }
}

/// Iniciar demostración de drivers
pub fn start_driver_demo() -> Result<(), DriverError> {
    unsafe { DRIVER_DEMO.start_demo() }
}

/// Detener demostración de drivers
pub fn stop_driver_demo() {
    unsafe {
        DRIVER_DEMO.stop_demo();
    }
}

/// Ejecutar demostración completa
pub fn run_driver_demo() -> Result<DemoResults, DriverError> {
    unsafe { DRIVER_DEMO.run_full_demo() }
}
