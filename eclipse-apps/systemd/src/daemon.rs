//! Daemon systemd para Eclipse OS
//! 
//! Este módulo implementa el daemon principal de systemd que gestiona
//! servicios, targets y dependencias en tiempo real.

use anyhow::Result;
use log::{info, warn, debug};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::path::Path;
use tokio::time::sleep;

use crate::service_parser::{ServiceFile, ServiceParser, ServiceValidator};
use crate::target_manager::TargetManager;
use crate::service_manager::ServiceManager;
use crate::dependency_resolver::DependencyResolver;
use crate::journald::JournalManager;
use crate::serial_logger::SerialLogger;
use crate::notifications::NotificationManager;
use crate::resource_manager::ResourceManager;
use eclipse_ipc::{UnixBus, IpcMessage, socket_path_from_env};
use std::time::Duration as StdDuration;

/// Estado de un servicio
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceState {
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
    Reloading,
}

/// Información de un servicio en ejecución
#[derive(Debug, Clone)]
pub struct RunningService {
    pub name: String,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub start_time: Instant,
    pub restart_count: u32,
    pub last_error: Option<String>,
    pub service_file: ServiceFile,
}

/// Daemon principal de systemd
pub struct SystemdDaemon {
    /// Servicios cargados
    services: Arc<RwLock<HashMap<String, ServiceFile>>>,
    /// Servicios en ejecución
    running_services: Arc<RwLock<HashMap<String, RunningService>>>,
    /// Manager de targets
    target_manager: Arc<RwLock<TargetManager>>,
    /// Manager de servicios
    service_manager: Arc<ServiceManager>,
    /// Resolvedor de dependencias
    dependency_resolver: Arc<DependencyResolver>,
    /// Manager del journal
    journal_manager: Arc<JournalManager>,
    /// Logger serial
    pub serial_logger: Arc<SerialLogger>,
    /// Manager de notificaciones
    notification_manager: Arc<NotificationManager>,
    /// Manager de recursos
    resource_manager: Arc<ResourceManager>,
    /// Estado del daemon
    is_running: Arc<RwLock<bool>>,
    /// Directorio de servicios
    service_dir: String,
}

impl SystemdDaemon {
    /// Crea una nueva instancia del daemon systemd
    pub fn new(service_dir: &str) -> Result<Self> {
        let journal_manager = Arc::new(JournalManager::new("/var/log/eclipse-systemd/journal.json")?);
        let serial_logger = Arc::new(SerialLogger::new());
        let notification_manager = Arc::new(NotificationManager::new());
        let resource_manager = Arc::new(ResourceManager::new());

        Ok(Self {
            services: Arc::new(RwLock::new(HashMap::new())),
            running_services: Arc::new(RwLock::new(HashMap::new())),
            target_manager: Arc::new(RwLock::new(TargetManager::new())),
            service_manager: Arc::new(ServiceManager::new()),
            dependency_resolver: Arc::new(DependencyResolver::new()),
            journal_manager,
            serial_logger,
            notification_manager,
            resource_manager,
            is_running: Arc::new(RwLock::new(false)),
            service_dir: service_dir.to_string(),
        })
    }

    /// Inicializa el daemon systemd
    pub async fn initialize(&self) -> Result<()> {
        info!("Inicializando Eclipse SystemD Daemon v0.2.0");
        
        // Escribir mensaje de inicio a serial
        self.serial_logger.write_system_startup().await?;
        
        // Asegurar directorios básicos para logs/runtime
        self.serial_logger.write_info("systemd", "📁 Creando directorios del sistema...").await?;
        let _ = std::fs::create_dir_all("/var/log");
        let _ = std::fs::create_dir_all("/run");
        self.serial_logger.write_info("systemd", "✅ Directorios del sistema creados").await?;

        // Registrar inicio en el journal
        self.journal_manager.log_info("systemd", "Iniciando Eclipse SystemD Daemon v0.2.0")?;
        
        // Cargar todos los archivos .service
        self.serial_logger.write_info("systemd", "📋 Cargando archivos de servicios...").await?;
        self.load_service_files().await?;
        
        // Inicializar targets
        self.serial_logger.write_info("systemd", "🎯 Inicializando targets del sistema...").await?;
        self.target_manager.write().await.initialize()?;
        self.serial_logger.write_info("systemd", "✅ Targets inicializados").await?;

        // Crear canal de notificaciones
        self.serial_logger.write_info("systemd", "🔔 Configurando sistema de notificaciones...").await?;
        self.notification_manager.create_channel("systemd", 100)?;
        self.notification_manager.create_channel("services", 100)?;
        self.serial_logger.write_info("systemd", "✅ Sistema de notificaciones configurado").await?;

        // Inicializar manager de recursos
        self.serial_logger.write_info("systemd", "💾 Inicializando gestor de recursos...").await?;
        self.resource_manager.register_service("systemd", Some(std::process::id()))?;
        self.serial_logger.write_info("systemd", "✅ Gestor de recursos inicializado").await?;

        // Marcar como ejecutándose
        *self.is_running.write().await = true;
        
        self.serial_logger.write_info("systemd", "🎉 Eclipse SystemD completamente inicializado y listo").await?;
        self.journal_manager.log_info("systemd", "Daemon systemd inicializado correctamente")?;
        info!("Daemon systemd inicializado correctamente");
        Ok(())
    }

    /// Carga todos los archivos .service del directorio
    async fn load_service_files(&self) -> Result<()> {
        info!("Cargando archivos .service desde: {}", self.service_dir);

        let service_dir_path = Path::new(&self.service_dir);
        if !service_dir_path.exists() {
            warn!("Directorio de servicios no encontrado: {}", self.service_dir);
            self.journal_manager.log_warning("systemd", &format!("Directorio de servicios no encontrado: {}", self.service_dir))?;
            return Ok(());
        }

        let mut loaded_count = 0;
        let mut error_count = 0;

        if let Ok(entries) = std::fs::read_dir(service_dir_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    
                    if path.extension().and_then(|s| s.to_str()) == Some("service") {
                        let service_name = path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        
                        debug!("Cargando servicio: {}", service_name);
                        
                        match ServiceParser::parse_file(&path) {
                            Ok(service_file) => {
                                // Validar archivo
                                match ServiceValidator::validate(&service_file) {
                                    Ok(errors) => {
                                        if errors.is_empty() {
                                            self.services.write().await.insert(service_name.clone(), service_file);
                                            loaded_count += 1;
                                            debug!("  Servicio cargado: {}", service_name);
                                            self.journal_manager.log_info("systemd", &format!("Servicio cargado: {}", service_name))?;
                                        } else {
                                            error_count += 1;
                                            warn!("  ❌ Servicio inválido {}: {:?}", service_name, errors);
                                            self.journal_manager.log_error("systemd", &format!("Servicio inválido {}: {:?}", service_name, errors))?;
                                        }
                                    }
                                    Err(e) => {
                                        error_count += 1;
                                        warn!("  ❌ Error validando {}: {}", service_name, e);
                                        self.journal_manager.log_error("systemd", &format!("Error validando {}: {}", service_name, e))?;
                                    }
                                }
                            }
                            Err(e) => {
                                error_count += 1;
                                warn!("  ❌ Error parseando {}: {}", service_name, e);
                                self.journal_manager.log_error("systemd", &format!("Error parseando {}: {}", service_name, e))?;
                            }
                        }
                    }
                }
            }
        }

        self.journal_manager.log_info("systemd", &format!("Servicios cargados: {} exitosos, {} con errores", loaded_count, error_count))?;
        self.serial_logger.write_info("systemd", &format!("Servicios cargados: {} exitosos, {} con errores", loaded_count, error_count)).await?;
        info!("Estado Servicios cargados: {} exitosos, {} con errores", loaded_count, error_count);
        Ok(())
    }

    /// Inicia un servicio
    pub fn start_service<'a>(&'a self, service_name: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
        info!("Iniciando Iniciando servicio: {}", service_name);
        
        // Escribir a serial que estamos iniciando el servicio
        self.serial_logger.write_info("systemd", &format!("🔧 Preparando servicio: {}", service_name)).await?;
        
        // Verificar si el servicio existe
        let service_file = {
            let services = self.services.read().await;
            services.get(service_name).cloned()
        };
        
        let service_file = match service_file {
            Some(sf) => sf,
            None => {
                self.serial_logger.write_error("systemd", &format!("❌ Servicio no encontrado: {}", service_name)).await?;
                return Err(anyhow::anyhow!("Servicio no encontrado: {}", service_name));
            }
        };

        // Verificar si ya está ejecutándose
        {
            let running = self.running_services.read().await;
            if running.contains_key(service_name) {
                self.serial_logger.write_info("systemd", &format!("✅ Servicio ya ejecutándose: {}", service_name)).await?;
                return Ok(());
            }
        }

        // Resolver dependencias
        let dependencies = self.dependency_resolver.resolve_dependencies(&service_file)?;
        if !dependencies.is_empty() {
            self.serial_logger.write_info("systemd", &format!("🔗 Resolviendo dependencias para {}: {:?}", service_name, dependencies)).await?;
        }
        
        for dep in &dependencies {
            if !self.is_service_running(dep).await {
                self.serial_logger.write_info("systemd", &format!("📦 Iniciando dependencia: {} -> {}", dep, service_name)).await?;
                info!("Dependencia Iniciando dependencia: {}", dep);
                self.start_service(dep).await?;
            }
        }

        // Ejecutar el servicio
        self.serial_logger.write_info("systemd", &format!("🚀 Ejecutando servicio: {}", service_name)).await?;
        self.execute_service(service_name, &service_file).await?;

        // Orquestación simple: no bloquear por clientes.
        // Wayland puede arrancar solo; COSMIC reconectará por su cuenta.
        // Dejamos el pequeño retraso, pero no exigimos dependencia fuerte.
        if service_name == "eclipse-wayland.service" {
            self.serial_logger.write_info("systemd", "⏳ Esperando estabilización de Wayland...").await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        
        self.serial_logger.write_info("systemd", &format!("✅ Servicio iniciado exitosamente: {}", service_name)).await?;
        info!("Servicio Servicio iniciado: {}", service_name);
        Ok(())
        })
    }

    /// Detiene un servicio
    pub async fn stop_service(&self, service_name: &str) -> Result<()> {
        info!("Deteniendo Deteniendo servicio: {}", service_name);
        
        let mut running = self.running_services.write().await;
        if let Some(running_service) = running.get_mut(service_name) {
            running_service.state = ServiceState::Deactivating;
            
            // Terminar proceso si existe
            if let Some(pid) = running_service.pid {
                if let Err(e) = std::process::Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .output() {
                    warn!("Advertencia  Error terminando proceso {}: {}", pid, e);
                }
            }
            
            let pid = running_service.pid;
            running_service.state = ServiceState::Inactive;
            running.remove(service_name);

            // Enviar notificación de servicio detenido
            if let Err(e) = self.notification_manager.notify_service_stopping(service_name, pid) {
                warn!("Advertencia  Error enviando notificación de servicio detenido: {}", e);
            }

            info!("Servicio Servicio detenido: {}", service_name);
        } else {
            return Err(anyhow::anyhow!("Servicio no está ejecutándose: {}", service_name));
        }
        
        Ok(())
    }

    /// Reinicia un servicio
    pub async fn restart_service(&self, service_name: &str) -> Result<()> {
        info!("Reiniciando Reiniciando servicio: {}", service_name);
        
        // Detener si está ejecutándose
        if self.is_service_running(service_name).await {
            self.stop_service(service_name).await?;
        }
        
        // Iniciar nuevamente
        self.start_service(service_name).await?;
        
        info!("Servicio Servicio reiniciado: {}", service_name);
        Ok(())
    }

    /// Obtiene el estado de un servicio
    pub async fn get_service_status(&self, service_name: &str) -> Option<ServiceState> {
        let running = self.running_services.read().await;
        running.get(service_name).map(|s| s.state.clone())
    }

    /// Verifica si un servicio está ejecutándose
    pub async fn is_service_running(&self, service_name: &str) -> bool {
        let running = self.running_services.read().await;
        running.contains_key(service_name)
    }

    /// Ejecuta un servicio
    async fn execute_service(&self, service_name: &str, service_file: &ServiceFile) -> Result<()> {
        // Obtener configuración del servicio
        let exec_start = ServiceParser::get_entry(service_file, "Service", "ExecStart")
            .ok_or_else(|| anyhow::anyhow!("ExecStart no encontrado"))?;
        
        let simple_type = "simple".to_string();
        let root_user = "root".to_string();
        let root_group = "root".to_string();
        
        let service_type = ServiceParser::get_entry(service_file, "Service", "Type")
            .unwrap_or(&simple_type);
        
        let user = ServiceParser::get_entry(service_file, "Service", "User")
            .unwrap_or(&root_user);
        
        let group = ServiceParser::get_entry(service_file, "Service", "Group")
            .unwrap_or(&root_group);

        // Escribir información del servicio a serial
        self.serial_logger.write_info("systemd", &format!("📝 Configuración del servicio {}: Tipo={}, Usuario={}, Grupo={}", service_name, service_type, user, group)).await?;
        self.serial_logger.write_info("systemd", &format!("💻 Comando a ejecutar: {}", exec_start)).await?;

        // Crear comando
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
           .arg(exec_start)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        // Configurar usuario y grupo (simulado)
        debug!("Usuario Ejecutando como usuario: {}, grupo: {}", user, group);

        // Ejecutar comando
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                
                // Escribir a serial que el proceso se inició
                self.serial_logger.write_info("systemd", &format!("🎯 Proceso iniciado con PID: {} para servicio: {}", pid, service_name)).await?;
                
                // Crear entrada de servicio en ejecución
                let running_service = RunningService {
                    name: service_name.to_string(),
                    state: ServiceState::Active,
                    pid: Some(pid),
                    start_time: Instant::now(),
                    restart_count: 0,
                    last_error: None,
                    service_file: service_file.clone(),
                };
                
                self.running_services.write().await.insert(service_name.to_string(), running_service);

                // Registrar servicio en el manager de recursos
                if let Err(e) = self.resource_manager.register_service(service_name, Some(pid)) {
                    warn!("Advertencia  Error registrando servicio en resource manager: {}", e);
                    self.serial_logger.write_warning("systemd", &format!("⚠️ Error registrando servicio en resource manager: {}", e)).await?;
                }

                // Enviar notificación de servicio listo
                if let Err(e) = self.notification_manager.notify_service_ready(service_name, pid) {
                    warn!("Advertencia  Error enviando notificación de servicio listo: {}", e);
                    self.serial_logger.write_warning("systemd", &format!("⚠️ Error enviando notificación de servicio listo: {}", e)).await?;
                }

                debug!("Servicio Proceso iniciado con PID: {}", pid);
            }
            Err(e) => {
                // Escribir error a serial
                self.serial_logger.write_error("systemd", &format!("💥 Error ejecutando servicio {}: {}", service_name, e)).await?;
                
                // Enviar notificación de error
                if let Err(ne) = self.notification_manager.notify_service_error(service_name, &e.to_string(), None) {
                    warn!("Advertencia  Error enviando notificación de error: {}", ne);
                }
                return Err(anyhow::anyhow!("Error ejecutando servicio {}: {}", service_name, e));
            }
        }

        Ok(())
    }

    /// Inicia un target
    pub async fn start_target(&self, target_name: &str) -> Result<()> {
        info!("Target Iniciando target: {}", target_name);
        
        // Escribir a serial que estamos iniciando el target
        self.serial_logger.write_info("systemd", &format!("🚀 Iniciando target: {}", target_name)).await?;
        
        let services = self.target_manager.read().await.get_target_services(target_name)?;
        
        self.serial_logger.write_info("systemd", &format!("📋 Servicios a iniciar: {}", services.join(", "))).await?;
        
        for service_name in &services {
            if !self.is_service_running(service_name).await {
                self.serial_logger.write_info("systemd", &format!("⚡ Iniciando servicio: {}", service_name)).await?;
                self.start_service(service_name).await?;
            } else {
                self.serial_logger.write_info("systemd", &format!("✅ Servicio ya ejecutándose: {}", service_name)).await?;
            }
        }
        
        self.serial_logger.write_info("systemd", &format!("🎉 Target completado: {}", target_name)).await?;
        info!("Servicio Target iniciado: {}", target_name);
        Ok(())
    }

    /// Obtiene el estado del sistema
    pub async fn get_system_status(&self) -> SystemStatus {
        let running = self.running_services.read().await;
        let services = self.services.read().await;
        
        SystemStatus {
            total_services: services.len(),
            running_services: running.len(),
            failed_services: running.values().filter(|s| s.state == ServiceState::Failed).count(),
            uptime: Instant::now(), // En una implementación real, esto sería el tiempo de inicio del sistema
        }
    }

    /// Ejecuta el loop principal del daemon
    pub async fn run(&self) -> Result<()> {
        info!("Reiniciando Iniciando loop principal del daemon systemd");
        self.journal_manager.log_info("systemd", "Iniciando loop principal del daemon")?;
        // IPC: monitorización básica de ready/health
        let ipc_path = socket_path_from_env();
        let ipc = UnixBus::connect_with_retry(&ipc_path, 10, StdDuration::from_millis(200))
            .ok();

        while *self.is_running.read().await {
            // Monitorear servicios en ejecución
            self.monitor_services().await?;
            
            // Procesar cola de eventos
            self.process_event_queue().await?;

            // Leer señales ready/health de servicios
            if let Some(ref bus) = ipc {
                let mut buf = [0u8; 2048];
                if let Ok(Some(msg)) = bus.recv_timeout(&mut buf, StdDuration::from_millis(10)) {
                    match msg {
                        IpcMessage::Ready { service, .. } => {
                            let _ = self.journal_manager.log_info("systemd", &format!("Servicio listo: {}", service));
                        }
                        IpcMessage::Health { service, ok } => {
                            let lvl = if ok { "ok" } else { "fail" };
                            let _ = self.journal_manager.log_info("systemd", &format!("Health {}: {}", service, lvl));
                        }
                        IpcMessage::Pong { service } => {
                            let _ = self.journal_manager.log_debug("systemd", &format!("Pong de {}", service));
                        }
                        _ => {}
                    }
                }
            }
            
            // Sincronizar journal
            self.journal_manager.sync()?;
            
            // Dormir un poco para evitar uso excesivo de CPU
            sleep(Duration::from_millis(100)).await;
        }
        
        self.journal_manager.log_info("systemd", "Daemon systemd detenido")?;
        info!("Deteniendo Daemon systemd detenido");
        Ok(())
    }

    /// Monitorea servicios en ejecución
    async fn monitor_services(&self) -> Result<()> {
        let mut running = self.running_services.write().await;
        let mut to_remove = Vec::new();
        
        for (name, service) in running.iter_mut() {
            // Verificar si el proceso sigue ejecutándose
            if let Some(pid) = service.pid {
                if !self.is_process_running(pid) {
                    warn!("Advertencia  Proceso terminado inesperadamente: {} (PID: {})", name, pid);
                    self.journal_manager.log_warning("systemd", &format!("Proceso terminado inesperadamente: {} (PID: {})", name, pid))?;
                    service.state = ServiceState::Failed;
                    to_remove.push(name.clone());
                }
            }
        }
        
        // Remover servicios fallidos
        for name in to_remove {
            running.remove(&name);
        }
        
        Ok(())
    }

    /// Verifica si un proceso está ejecutándose
    fn is_process_running(&self, _pid: u32) -> bool {
        // En una implementación real, aquí se verificaría el estado del proceso
        // Por ahora, simulamos que siempre está ejecutándose
        true
    }

    /// Procesa la cola de eventos
    async fn process_event_queue(&self) -> Result<()> {
        // En una implementación real, aquí se procesarían eventos del sistema
        // como señales, cambios de archivos, etc.
        Ok(())
    }

    /// Detiene el daemon
    pub async fn shutdown(&self) {
        info!("Deteniendo Iniciando apagado del daemon systemd");
        self.journal_manager.log_info("systemd", "Iniciando apagado del daemon systemd").ok();
        self.serial_logger.write_system_shutdown().await.ok();
        
        *self.is_running.write().await = false;
        
        // Detener todos los servicios
        let running_services: Vec<String> = {
            let running = self.running_services.read().await;
            running.keys().cloned().collect()
        };
        
        for service_name in &running_services {
            if let Err(e) = self.stop_service(service_name).await {
                warn!("Advertencia  Error deteniendo servicio {}: {}", service_name, e);
                self.journal_manager.log_error("systemd", &format!("Error deteniendo servicio {}: {}", service_name, e)).ok();
            }
        }
        
        self.journal_manager.log_info("systemd", "Daemon systemd apagado").ok();
        info!("Servicio Daemon systemd apagado");
    }
}

/// Estado del sistema
#[derive(Debug, Clone)]
pub struct SystemStatus {
    pub total_services: usize,
    pub running_services: usize,
    pub failed_services: usize,
    pub uptime: Instant,
}

impl SystemStatus {
    pub fn get_summary(&self) -> String {
        format!(
            "Servicios: {}/{} ejecutándose, {} fallidos",
            self.running_services, self.total_services, self.failed_services
        )
    }
}
