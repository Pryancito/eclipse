//! # Servicios del Sistema Userland

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Service {
    pub service_id: u32,
    pub name: String,
    pub display_name: String,
    pub status: ServiceStatus,
    pub service_type: ServiceType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    KernelDriver,
    FileSystemDriver,
    Adapter,
    RecognizerDriver,
    Win32OwnProcess,
    Win32ShareProcess,
    InteractiveProcess,
}

pub struct ServiceManager {
    services: Vec<Service>,
    next_service_id: u32,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
            next_service_id: 1,
        }
    }
    
    pub fn initialize(&mut self) -> Result<()> {
        // Crear servicios del sistema
        self.create_system_services()?;
        Ok(())
    }
    
    fn create_system_services(&mut self) -> Result<()> {
        // Servicio de Event Log
        let event_log = Service {
            service_id: 1,
            name: "EventLog".to_string(),
            display_name: "Windows Event Log".to_string(),
            status: ServiceStatus::Running,
            service_type: ServiceType::Win32OwnProcess,
        };
        self.services.push(event_log);
        
        // Servicio de Plug and Play
        let plug_play = Service {
            service_id: 2,
            name: "PlugPlay".to_string(),
            display_name: "Plug and Play".to_string(),
            status: ServiceStatus::Running,
            service_type: ServiceType::KernelDriver,
        };
        self.services.push(plug_play);
        
        // Servicio de RPC
        let rpc = Service {
            service_id: 3,
            name: "RpcSs".to_string(),
            display_name: "Remote Procedure Call (RPC)".to_string(),
            status: ServiceStatus::Running,
            service_type: ServiceType::Win32OwnProcess,
        };
        self.services.push(rpc);
        
        Ok(())
    }
    
    pub fn start_service(&mut self, service_name: &str) -> Result<()> {
        if let Some(service) = self.services.iter_mut().find(|s| s.name == service_name) {
            service.status = ServiceStatus::Starting;
            // Simular inicio del servicio
            service.status = ServiceStatus::Running;
        }
        Ok(())
    }
    
    pub fn stop_service(&mut self, service_name: &str) -> Result<()> {
        if let Some(service) = self.services.iter_mut().find(|s| s.name == service_name) {
            service.status = ServiceStatus::Stopping;
            // Simular parada del servicio
            service.status = ServiceStatus::Stopped;
        }
        Ok(())
    }
    
    pub fn get_services(&self) -> &Vec<Service> {
        &self.services
    }
    
    pub fn get_service(&self, service_name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == service_name)
    }
}

static mut SERVICE_MANAGER: Option<ServiceManager> = None;

pub fn initialize() -> Result<()> {
    unsafe {
        SERVICE_MANAGER = Some(ServiceManager::new());
        if let Some(ref mut manager) = SERVICE_MANAGER {
            manager.initialize()?;
        }
    }
    Ok(())
}

pub fn start_service(service_name: &str) -> Result<()> {
    unsafe {
        if let Some(ref mut manager) = SERVICE_MANAGER {
            manager.start_service(service_name)
        } else {
            Err(anyhow::anyhow!("Service manager not initialized"))
        }
    }
}

pub fn stop_service(service_name: &str) -> Result<()> {
    unsafe {
        if let Some(ref mut manager) = SERVICE_MANAGER {
            manager.stop_service(service_name)
        } else {
            Err(anyhow::anyhow!("Service manager not initialized"))
        }
    }
}

pub fn get_services() -> Vec<Service> {
    unsafe {
        if let Some(ref manager) = SERVICE_MANAGER {
            manager.get_services().clone()
        } else {
            Vec::new()
        }
    }
}
