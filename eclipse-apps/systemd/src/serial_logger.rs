//! Serial Logger para Eclipse SystemD
//! 
//! Este módulo implementa el logging a serial para systemd
//! para que los logs también aparezcan en la consola serial.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use chrono::{DateTime, Utc};

/// Logger serial para systemd
pub struct SerialLogger {
    /// Puerto serial (simulado)
    serial_port: Arc<Mutex<u16>>,
    /// Habilitado
    enabled: bool,
}

impl SerialLogger {
    /// Crea una nueva instancia del logger serial
    pub fn new() -> Self {
        Self {
            serial_port: Arc::new(Mutex::new(0x3F8)), // COM1
            enabled: true,
        }
    }

    /// Habilita o deshabilita el logging serial
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Verifica si está habilitado
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Escribe un mensaje a serial
    pub async fn write_message(&self, level: &str, service: &str, message: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let log_line = format!(
            "[{}] {} {}: {}\n",
            timestamp,
            level,
            service,
            message
        );

        // Simular escritura a serial
        self.write_to_serial(&log_line).await?;
        
        Ok(())
    }

    /// Escribe un mensaje de error
    pub async fn write_error(&self, service: &str, message: &str) -> Result<()> {
        self.write_message("ERROR", service, message).await
    }

    /// Escribe un mensaje de advertencia
    pub async fn write_warning(&self, service: &str, message: &str) -> Result<()> {
        self.write_message("WARN", service, message).await
    }

    /// Escribe un mensaje informativo
    pub async fn write_info(&self, service: &str, message: &str) -> Result<()> {
        self.write_message("INFO", service, message).await
    }

    /// Escribe un mensaje de debug
    pub async fn write_debug(&self, service: &str, message: &str) -> Result<()> {
        self.write_message("DEBUG", service, message).await
    }

    /// Escribe un mensaje de inicio del sistema
    pub async fn write_system_startup(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let startup_message = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                        ECLIPSE-SYSTEMD INICIADO                              ║
╚══════════════════════════════════════════════════════════════════════════════╝

Eclipse SystemD v0.5.0 - Sistema de inicialización moderno
Sistema de logging habilitado (Archivo + Serial)
Estado: Iniciando servicios del sistema...

"#;

        self.write_to_serial(startup_message).await?;
        Ok(())
    }

    /// Escribe un mensaje de estado del sistema
    pub async fn write_system_status(&self, status: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let status_message = format!(
            "\nEstado Estado del sistema:\n{}\n",
            status
        );

        self.write_to_serial(&status_message).await?;
        Ok(())
    }

    /// Escribe información de un servicio
    pub async fn write_service_info(&self, service_name: &str, state: &str, pid: Option<u32>) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let pid_info = if let Some(pid) = pid {
            format!(" (PID: {})", pid)
        } else {
            String::new()
        };

        let service_message = format!(
            "Aplicando Servicio {}: {} {}\n",
            service_name,
            state,
            pid_info
        );

        self.write_to_serial(&service_message).await?;
        Ok(())
    }

    /// Escribe un mensaje de error del sistema
    pub async fn write_system_error(&self, error: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let error_message = format!(
            "❌ ERROR DEL SISTEMA: {}\n",
            error
        );

        self.write_to_serial(&error_message).await?;
        Ok(())
    }

    /// Escribe un mensaje de advertencia del sistema
    pub async fn write_system_warning(&self, warning: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let warning_message = format!(
            "Advertencia  ADVERTENCIA: {}\n",
            warning
        );

        self.write_to_serial(&warning_message).await?;
        Ok(())
    }

    /// Escribe un mensaje de apagado del sistema
    pub async fn write_system_shutdown(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let shutdown_message = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                        ECLIPSE-SYSTEMD APAGANDO                              ║
╚══════════════════════════════════════════════════════════════════════════════╝

Deteniendo servicios del sistema...
Sistema Eclipse OS apagando correctamente.

"#;

        self.write_to_serial(shutdown_message).await?;
        Ok(())
    }

    /// Simula la escritura a serial
    async fn write_to_serial(&self, message: &str) -> Result<()> {
        // En un sistema real, aquí se escribiría al puerto serial
        // Por ahora, simulamos la escritura
        
        // Simular delay de escritura serial
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        
        // En un sistema real, esto sería:
        // unsafe {
        //     for byte in message.bytes() {
        //         outb(*self.serial_port.lock().await, byte);
        //     }
        // }
        
        // Por ahora, solo imprimimos a stdout para simular
        println!("[SERIAL] {}", message.trim());
        
        Ok(())
    }
}

impl Default for SerialLogger {
    fn default() -> Self {
        Self::new()
    }
}

/// Macro para logging fácil a serial
#[macro_export]
macro_rules! serial_log {
    ($logger:expr, $level:expr, $service:expr, $($arg:tt)*) => {
        if let Err(e) = $logger.write_message($level, $service, &format!($($arg)*)).await {
            eprintln!("Error escribiendo a serial: {}", e);
        }
    };
}

/// Macro para logging de errores a serial
#[macro_export]
macro_rules! serial_error {
    ($logger:expr, $service:expr, $($arg:tt)*) => {
        if let Err(e) = $logger.write_error($service, &format!($($arg)*)).await {
            eprintln!("Error escribiendo error a serial: {}", e);
        }
    };
}

/// Macro para logging de advertencias a serial
#[macro_export]
macro_rules! serial_warning {
    ($logger:expr, $service:expr, $($arg:tt)*) => {
        if let Err(e) = $logger.write_warning($service, &format!($($arg)*)).await {
            eprintln!("Error escribiendo advertencia a serial: {}", e);
        }
    };
}

/// Macro para logging informativo a serial
#[macro_export]
macro_rules! serial_info {
    ($logger:expr, $service:expr, $($arg:tt)*) => {
        if let Err(e) = $logger.write_info($service, &format!($($arg)*)).await {
            eprintln!("Error escribiendo info a serial: {}", e);
        }
    };
}

/// Macro para logging de debug a serial
#[macro_export]
macro_rules! serial_debug {
    ($logger:expr, $service:expr, $($arg:tt)*) => {
        if let Err(e) = $logger.write_debug($service, &format!($($arg)*)).await {
            eprintln!("Error escribiendo debug a serial: {}", e);
        }
    };
}
