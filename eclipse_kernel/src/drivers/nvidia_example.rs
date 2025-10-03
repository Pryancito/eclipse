use alloc::string::{String, ToString};

/// Ejemplo de uso de las integraciones NVIDIA
pub struct NvidiaExample {
    pub initialized: bool,
}

impl NvidiaExample {
    /// Crear nuevo ejemplo
    pub fn new() -> Self {
        Self { initialized: false }
    }

    /// Inicializar ejemplo
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized = true;
        Ok(())
    }

    /// Obtener información del sistema
    pub fn get_system_info(&self) -> String {
        if self.initialized {
            "Sistema NVIDIA inicializado correctamente".to_string()
        } else {
            "Sistema NVIDIA no inicializado".to_string()
        }
    }
}
