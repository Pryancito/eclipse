//! Control de systemd (systemctl) para Eclipse OS
//! 
//! Este módulo implementa los comandos de control de systemd
//! como start, stop, restart, status, etc.

use anyhow::Result;
use log::{info, warn, error};
use std::process::{Command, Stdio};
use std::collections::HashMap;
use serde_json;

/// Control de systemd
pub struct SystemdControl {
    /// Socket de comunicación con el daemon
    daemon_socket: String,
}

impl SystemdControl {
    /// Crea una nueva instancia del control de systemd
    pub fn new() -> Self {
        Self {
            daemon_socket: "/run/eclipse-systemd.sock".to_string(),
        }
    }

    /// Maneja comandos de control
    pub async fn handle_command(&self, args: &[String]) -> Result<()> {
        if args.is_empty() {
            self.show_help();
            return Ok(());
        }

        let command = &args[0];
        let service_name = args.get(1);

        match command.as_str() {
            "start" => {
                if let Some(service) = service_name {
                    self.start_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "stop" => {
                if let Some(service) = service_name {
                    self.stop_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "restart" => {
                if let Some(service) = service_name {
                    self.restart_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "reload" => {
                if let Some(service) = service_name {
                    self.reload_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "status" => {
                if let Some(service) = service_name {
                    self.show_service_status(service).await?;
                } else {
                    self.show_system_status().await?;
                }
            }
            "list-units" => {
                self.list_units().await?;
            }
            "list-services" => {
                self.list_services().await?;
            }
            "enable" => {
                if let Some(service) = service_name {
                    self.enable_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "disable" => {
                if let Some(service) = service_name {
                    self.disable_service(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "is-active" => {
                if let Some(service) = service_name {
                    self.is_service_active(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "is-enabled" => {
                if let Some(service) = service_name {
                    self.is_service_enabled(service).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del servicio");
                    return Ok(());
                }
            }
            "daemon-reload" => {
                self.daemon_reload().await?;
            }
            "get-default" => {
                self.get_default_target().await?;
            }
            "set-default" => {
                if let Some(target) = service_name {
                    self.set_default_target(target).await?;
                } else {
                    eprintln!("❌ Error: Se requiere nombre del target");
                    return Ok(());
                }
            }
            _ => {
                eprintln!("❌ Comando desconocido: {}", command);
                self.show_help();
            }
        }

        Ok(())
    }

    /// Inicia un servicio
    async fn start_service(&self, service_name: &str) -> Result<()> {
        info!("Iniciando Iniciando servicio: {}", service_name);
        
        // En una implementación real, aquí se comunicaría con el daemon
        // Por ahora, ejecutamos el comando directamente
        let result = Command::new("systemctl")
            .arg("start")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio iniciado: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error iniciando servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Detiene un servicio
    async fn stop_service(&self, service_name: &str) -> Result<()> {
        info!("Deteniendo Deteniendo servicio: {}", service_name);
        
        let result = Command::new("systemctl")
            .arg("stop")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio detenido: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error deteniendo servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Reinicia un servicio
    async fn restart_service(&self, service_name: &str) -> Result<()> {
        info!("Reiniciando Reiniciando servicio: {}", service_name);
        
        let result = Command::new("systemctl")
            .arg("restart")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio reiniciado: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error reiniciando servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Recarga un servicio
    async fn reload_service(&self, service_name: &str) -> Result<()> {
        info!("Reiniciando Recargando servicio: {}", service_name);
        
        let result = Command::new("systemctl")
            .arg("reload")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio recargado: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error recargando servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Muestra el estado de un servicio
    async fn show_service_status(&self, service_name: &str) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("status")
            .arg(service_name)
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout);
        println!("{}", output);

        Ok(())
    }

    /// Muestra el estado del sistema
    async fn show_system_status(&self) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("status")
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout);
        println!("{}", output);

        Ok(())
    }

    /// Lista todas las unidades
    async fn list_units(&self) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("list-units")
            .arg("--type=service")
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout);
        println!("{}", output);

        Ok(())
    }

    /// Lista todos los servicios
    async fn list_services(&self) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("list-unit-files")
            .arg("--type=service")
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout);
        println!("{}", output);

        Ok(())
    }

    /// Habilita un servicio
    async fn enable_service(&self, service_name: &str) -> Result<()> {
        info!("Servicio Habilitando servicio: {}", service_name);
        
        let result = Command::new("systemctl")
            .arg("enable")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio habilitado: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error habilitando servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Deshabilita un servicio
    async fn disable_service(&self, service_name: &str) -> Result<()> {
        info!("❌ Deshabilitando servicio: {}", service_name);
        
        let result = Command::new("systemctl")
            .arg("disable")
            .arg(service_name)
            .output()?;

        if result.status.success() {
            println!("Servicio Servicio deshabilitado: {}", service_name);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error deshabilitando servicio {}: {}", service_name, error);
        }

        Ok(())
    }

    /// Verifica si un servicio está activo
    async fn is_service_active(&self, service_name: &str) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("is-active")
            .arg(service_name)
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
        println!("{}", output);

        Ok(())
    }

    /// Verifica si un servicio está habilitado
    async fn is_service_enabled(&self, service_name: &str) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("is-enabled")
            .arg(service_name)
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
        println!("{}", output);

        Ok(())
    }

    /// Recarga la configuración del daemon
    async fn daemon_reload(&self) -> Result<()> {
        info!("Reiniciando Recargando configuración del daemon");
        
        let result = Command::new("systemctl")
            .arg("daemon-reload")
            .output()?;

        if result.status.success() {
            println!("Servicio Configuración del daemon recargada");
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error recargando configuración: {}", error);
        }

        Ok(())
    }

    /// Obtiene el target por defecto
    async fn get_default_target(&self) -> Result<()> {
        let result = Command::new("systemctl")
            .arg("get-default")
            .output()?;

        let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
        println!("{}", output);

        Ok(())
    }

    /// Establece el target por defecto
    async fn set_default_target(&self, target: &str) -> Result<()> {
        info!("Target Estableciendo target por defecto: {}", target);
        
        let result = Command::new("systemctl")
            .arg("set-default")
            .arg(target)
            .output()?;

        if result.status.success() {
            println!("Servicio Target por defecto establecido: {}", target);
        } else {
            let error = String::from_utf8_lossy(&result.stderr);
            eprintln!("❌ Error estableciendo target {}: {}", target, error);
        }

        Ok(())
    }

    /// Muestra la ayuda
    fn show_help(&self) {
        println!("Eclipse SystemD Control v0.2.0");
        println!("Sistema de control de servicios para Eclipse OS");
        println!();
        println!("Uso: systemctl [COMANDO] [SERVICIO/TARGET]");
        println!();
        println!("Comandos de servicios:");
        println!("  start SERVICIO     Inicia un servicio");
        println!("  stop SERVICIO      Detiene un servicio");
        println!("  restart SERVICIO   Reinicia un servicio");
        println!("  reload SERVICIO    Recarga un servicio");
        println!("  status [SERVICIO]  Muestra el estado");
        println!("  enable SERVICIO    Habilita un servicio");
        println!("  disable SERVICIO   Deshabilita un servicio");
        println!("  is-active SERVICIO Verifica si está activo");
        println!("  is-enabled SERVICIO Verifica si está habilitado");
        println!();
        println!("Comandos del sistema:");
        println!("  list-units         Lista todas las unidades");
        println!("  list-services      Lista todos los servicios");
        println!("  daemon-reload      Recarga la configuración");
        println!("  get-default        Obtiene el target por defecto");
        println!("  set-default TARGET Establece el target por defecto");
        println!();
        println!("Ejemplos:");
        println!("  systemctl start eclipse-gui.service");
        println!("  systemctl status network.service");
        println!("  systemctl enable eclipse-shell.service");
        println!("  systemctl set-default graphical.target");
    }
}
