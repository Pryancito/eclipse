mod service_parser;
mod daemon;
mod target_manager;
mod service_manager;
mod dependency_resolver;
mod journald;
mod control;

use anyhow::Result;
use log::{info, warn};
use std::env;
use std::sync::Arc;
use tokio::signal;

use daemon::SystemdDaemon;
use control::SystemdControl;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    
    info!("🚀 Eclipse SystemD v0.2.0 - Daemon completamente funcional");
    info!("Sistema de inicialización moderno para Eclipse OS");
    
    // Obtener argumentos de línea de comandos
    let args: Vec<String> = env::args().collect();
    
    if args.len() > 1 {
        // Modo de control (systemctl)
        let control = SystemdControl::new();
        return control.handle_command(&args[1..]).await;
    }
    
    // Modo daemon
    run_daemon().await
}

/// Ejecuta el daemon principal de systemd
async fn run_daemon() -> Result<()> {
    info!("🔄 Iniciando daemon systemd");
    
    // Directorio de servicios
    let service_dir = "/etc/eclipse/systemd/system";
    
    // Crear daemon
    let daemon = Arc::new(SystemdDaemon::new(service_dir)?);
    
    // Inicializar daemon
    daemon.initialize().await?;
    
    // Iniciar target por defecto
    let default_target = env::var("DEFAULT_TARGET").unwrap_or_else(|_| "graphical.target".to_string());
    info!("🎯 Iniciando target por defecto: {}", default_target);
    
    if let Err(e) = daemon.start_target(&default_target).await {
        warn!("⚠️  Error iniciando target {}: {}", default_target, e);
        // Intentar con target básico
        if let Err(e) = daemon.start_target("multi-user.target").await {
            warn!("⚠️  Error iniciando target básico: {}", e);
        }
    }
    
    // Mostrar estado del sistema
    let status = daemon.get_system_status().await;
    info!("📊 Estado del sistema: {}", status.get_summary());
    
    // Configurar manejo de señales
    let daemon_clone = Arc::clone(&daemon);
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            eprintln!("Error esperando señal: {}", e);
        }
        info!("🛑 Señal de apagado recibida");
        daemon_clone.shutdown().await;
    });
    
    // Ejecutar loop principal
    daemon.run().await?;
    
    info!("✅ Daemon systemd finalizado");
    Ok(())
}