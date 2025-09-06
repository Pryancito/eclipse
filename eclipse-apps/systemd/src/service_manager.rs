//! Manager de servicios para Eclipse SystemD
//! 
//! Este módulo gestiona el ciclo de vida de los servicios,
//! incluyendo inicio, parada, reinicio y monitoreo.

use anyhow::Result;
use log::{info, debug, warn, error};
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::thread;

use crate::service_parser::{ServiceFile, ServiceParser};

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
pub struct ServiceInfo {
    pub name: String,
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub start_time: Instant,
    pub restart_count: u32,
    pub last_error: Option<String>,
    pub service_file: ServiceFile,
    pub exit_code: Option<i32>,
}

/// Manager de servicios
pub struct ServiceManager {
    /// Servicios en ejecución
    running_services: Arc<Mutex<HashMap<String, ServiceInfo>>>,
    /// Configuración de servicios
    service_configs: Arc<Mutex<HashMap<String, ServiceFile>>>,
}

impl ServiceManager {
    /// Crea una nueva instancia del manager de servicios
    pub fn new() -> Self {
        Self {
            running_services: Arc::new(Mutex::new(HashMap::new())),
            service_configs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registra un servicio
    pub fn register_service(&self, name: &str, service_file: ServiceFile) {
        self.service_configs.lock().unwrap().insert(name.to_string(), service_file);
        debug!("📝 Servicio registrado: {}", name);
    }

    /// Inicia un servicio
    pub fn start_service(&self, name: &str) -> Result<()> {
        info!("🚀 Iniciando servicio: {}", name);
        
        // Verificar si ya está ejecutándose
        if self.is_service_running(name) {
            return Err(anyhow::anyhow!("Servicio ya está ejecutándose: {}", name));
        }

        // Obtener configuración del servicio
        let service_file = {
            let configs = self.service_configs.lock().unwrap();
            configs.get(name).cloned()
        };

        let service_file = match service_file {
            Some(sf) => sf,
            None => return Err(anyhow::anyhow!("Servicio no encontrado: {}", name)),
        };

        // Ejecutar el servicio
        self.execute_service(name, &service_file)?;
        
        info!("✅ Servicio iniciado: {}", name);
        Ok(())
    }

    /// Detiene un servicio
    pub fn stop_service(&self, name: &str) -> Result<()> {
        info!("🛑 Deteniendo servicio: {}", name);
        
        let mut running = self.running_services.lock().unwrap();
        if let Some(service_info) = running.get_mut(name) {
            service_info.state = ServiceState::Deactivating;
            
            // Terminar proceso si existe
            if let Some(pid) = service_info.pid {
                self.terminate_process(pid)?;
            }
            
            service_info.state = ServiceState::Inactive;
            running.remove(name);
            info!("✅ Servicio detenido: {}", name);
        } else {
            return Err(anyhow::anyhow!("Servicio no está ejecutándose: {}", name));
        }
        
        Ok(())
    }

    /// Reinicia un servicio
    pub fn restart_service(&self, name: &str) -> Result<()> {
        info!("🔄 Reiniciando servicio: {}", name);
        
        // Detener si está ejecutándose
        if self.is_service_running(name) {
            self.stop_service(name)?;
        }
        
        // Pequeña pausa antes de reiniciar
        thread::sleep(Duration::from_millis(100));
        
        // Iniciar nuevamente
        self.start_service(name)?;
        
        info!("✅ Servicio reiniciado: {}", name);
        Ok(())
    }

    /// Recarga un servicio
    pub fn reload_service(&self, name: &str) -> Result<()> {
        info!("🔄 Recargando servicio: {}", name);
        
        let mut running = self.running_services.lock().unwrap();
        if let Some(service_info) = running.get_mut(name) {
            service_info.state = ServiceState::Reloading;
            
            // Enviar señal SIGHUP al proceso
            if let Some(pid) = service_info.pid {
                if let Err(e) = Command::new("kill")
                    .arg("-HUP")
                    .arg(pid.to_string())
                    .output() {
                    warn!("⚠️  Error enviando SIGHUP a {}: {}", pid, e);
                }
            }
            
            service_info.state = ServiceState::Active;
            info!("✅ Servicio recargado: {}", name);
        } else {
            return Err(anyhow::anyhow!("Servicio no está ejecutándose: {}", name));
        }
        
        Ok(())
    }

    /// Verifica si un servicio está ejecutándose
    pub fn is_service_running(&self, name: &str) -> bool {
        let running = self.running_services.lock().unwrap();
        running.contains_key(name)
    }

    /// Obtiene el estado de un servicio
    pub fn get_service_state(&self, name: &str) -> Option<ServiceState> {
        let running = self.running_services.lock().unwrap();
        running.get(name).map(|s| s.state.clone())
    }

    /// Obtiene información de un servicio
    pub fn get_service_info(&self, name: &str) -> Option<ServiceInfo> {
        let running = self.running_services.lock().unwrap();
        running.get(name).cloned()
    }

    /// Lista todos los servicios en ejecución
    pub fn list_running_services(&self) -> Vec<String> {
        let running = self.running_services.lock().unwrap();
        running.keys().cloned().collect()
    }

    /// Ejecuta un servicio
    fn execute_service(&self, name: &str, service_file: &ServiceFile) -> Result<()> {
        // Obtener configuración del servicio
        let exec_start = ServiceParser::get_entry(service_file, "Service", "ExecStart")
            .ok_or_else(|| anyhow::anyhow!("ExecStart no encontrado"))?;
        
        let service_type = ServiceParser::get_entry(service_file, "Service", "Type")
            .unwrap_or(&"simple".to_string());
        
        let root_user = "root".to_string();
        let root_group = "root".to_string();
        let root_dir = "/".to_string();
        
        let user = ServiceParser::get_entry(service_file, "Service", "User")
            .unwrap_or(&root_user);
        
        let group = ServiceParser::get_entry(service_file, "Service", "Group")
            .unwrap_or(&root_group);

        let working_dir = ServiceParser::get_entry(service_file, "Service", "WorkingDirectory")
            .unwrap_or(&root_dir);

        // Crear comando
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
           .arg(exec_start)
           .current_dir(working_dir)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        // Configurar variables de entorno
        if let Some(env_vars) = self.get_environment_variables(service_file) {
            for (key, value) in env_vars {
                cmd.env(key, value);
            }
        }

        debug!("👤 Ejecutando como usuario: {}, grupo: {}", user, group);
        debug!("📁 Directorio de trabajo: {}", working_dir);
        debug!("🚀 Comando: {}", exec_start);

        // Ejecutar comando
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                
                // Crear entrada de servicio en ejecución
                let service_info = ServiceInfo {
                    name: name.to_string(),
                    state: ServiceState::Active,
                    pid: Some(pid),
                    start_time: Instant::now(),
                    restart_count: 0,
                    last_error: None,
                    service_file: service_file.clone(),
                    exit_code: None,
                };
                
                self.running_services.lock().unwrap().insert(name.to_string(), service_info);
                
                debug!("✅ Proceso iniciado con PID: {}", pid);
                
                // Iniciar monitoreo del proceso en un hilo separado
                self.start_process_monitoring(name, pid);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error ejecutando servicio {}: {}", name, e));
            }
        }

        Ok(())
    }

    /// Obtiene variables de entorno del servicio
    fn get_environment_variables(&self, service_file: &ServiceFile) -> Option<Vec<(String, String)>> {
        let mut env_vars = Vec::new();
        
        // Buscar entradas Environment en la sección Service
        if let Some(service_entries) = ServiceParser::get_section_entries(service_file, "Service") {
            for (key, value) in service_entries {
                if key.starts_with("Environment") {
                    if let Some((env_key, env_value)) = self.parse_environment_entry(value) {
                        env_vars.push((env_key, env_value));
                    }
                }
            }
        }
        
        if env_vars.is_empty() {
            None
        } else {
            Some(env_vars)
        }
    }

    /// Parsea una entrada de variable de entorno
    fn parse_environment_entry(&self, entry: &str) -> Option<(String, String)> {
        if let Some(eq_pos) = entry.find('=') {
            let key = entry[..eq_pos].trim().to_string();
            let value = entry[eq_pos + 1..].trim().to_string();
            Some((key, value))
        } else {
            None
        }
    }

    /// Inicia el monitoreo de un proceso
    fn start_process_monitoring(&self, service_name: &str, pid: u32) {
        let service_name = service_name.to_string();
        let running_services = Arc::clone(&self.running_services);
        
        thread::spawn(move || {
            debug!("🔍 Iniciando monitoreo del proceso {} (PID: {})", service_name, pid);
            
            // En una implementación real, aquí se usaría waitpid o similar
            // para monitorear el proceso
            loop {
                thread::sleep(Duration::from_secs(1));
                
                // Verificar si el proceso sigue ejecutándose
                if !Self::is_process_running(pid) {
                    warn!("⚠️  Proceso terminado: {} (PID: {})", service_name, pid);
                    
                    // Actualizar estado del servicio
                    if let Ok(mut running) = running_services.lock() {
                        if let Some(service_info) = running.get_mut(&service_name) {
                            service_info.state = ServiceState::Failed;
                            service_info.pid = None;
                        }
                    }
                    
                    break;
                }
            }
        });
    }

    /// Verifica si un proceso está ejecutándose
    fn is_process_running(pid: u32) -> bool {
        // En una implementación real, aquí se verificaría el estado del proceso
        // usando syscalls del sistema operativo
        // Por ahora, simulamos que siempre está ejecutándose
        true
    }

    /// Termina un proceso
    fn terminate_process(&self, pid: u32) -> Result<()> {
        // Enviar señal SIGTERM
        if let Err(e) = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output() {
            warn!("⚠️  Error enviando SIGTERM a {}: {}", pid, e);
        }
        
        // Esperar un poco para que el proceso termine
        thread::sleep(Duration::from_millis(500));
        
        // Si aún está ejecutándose, enviar SIGKILL
        if Self::is_process_running(pid) {
            if let Err(e) = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .output() {
                warn!("⚠️  Error enviando SIGKILL a {}: {}", pid, e);
            }
        }
        
        Ok(())
    }

    /// Obtiene estadísticas del manager
    pub fn get_stats(&self) -> ServiceManagerStats {
        let running = self.running_services.lock().unwrap();
        let configs = self.service_configs.lock().unwrap();
        
        ServiceManagerStats {
            total_services: configs.len(),
            running_services: running.len(),
            failed_services: running.values().filter(|s| s.state == ServiceState::Failed).count(),
            active_services: running.values().filter(|s| s.state == ServiceState::Active).count(),
        }
    }
}

/// Estadísticas del manager de servicios
#[derive(Debug, Clone)]
pub struct ServiceManagerStats {
    pub total_services: usize,
    pub running_services: usize,
    pub failed_services: usize,
    pub active_services: usize,
}

impl ServiceManagerStats {
    pub fn get_summary(&self) -> String {
        format!(
            "Servicios: {}/{} ejecutándose ({} activos, {} fallidos)",
            self.running_services, self.total_services, self.active_services, self.failed_services
        )
    }
}
