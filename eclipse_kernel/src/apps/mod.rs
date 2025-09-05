//! Módulo de aplicaciones de usuario para Eclipse OS
//! 
//! Este módulo contiene aplicaciones básicas del sistema operativo
//! que los usuarios pueden ejecutar.

pub mod shell;
pub mod file_manager;
pub mod system_info;
pub mod text_editor;
pub mod calculator;
pub mod task_manager;

use alloc::{vec, vec::Vec};
use alloc::string::{String, ToString};
use alloc::format;

/// Tipo de aplicación
#[derive(Debug, Clone, PartialEq)]
pub enum AppType {
    System,
    User,
    Utility,
    Development,
    Game,
}

/// Estado de la aplicación
#[derive(Debug, Clone, PartialEq)]
pub enum AppStatus {
    Running,
    Stopped,
    Paused,
    Error,
}

/// Información de una aplicación
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub app_type: AppType,
    pub status: AppStatus,
    pub memory_usage: usize,
    pub cpu_usage: f32,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Gestor de aplicaciones
pub struct AppManager {
    apps: Vec<AppInfo>,
    running_apps: Vec<String>,
    next_id: usize,
}

impl AppManager {
    pub fn new() -> Self {
        Self {
            apps: Vec::new(),
            running_apps: Vec::new(),
            next_id: 1,
        }
    }

    /// Registrar una nueva aplicación
    pub fn register_app(&mut self, app: AppInfo) -> Result<String, &'static str> {
        let id = format!("app_{}", self.next_id);
        self.next_id += 1;
        
        // Aquí se registraría la aplicación en el sistema
        // Por ahora solo simulamos el registro
        
        Ok(id)
    }

    /// Iniciar una aplicación
    pub fn start_app(&mut self, app_id: &str) -> Result<(), &'static str> {
        if self.running_apps.contains(&app_id.to_string()) {
            return Err("Aplicación ya está ejecutándose");
        }
        
        self.running_apps.push(app_id.to_string());
        Ok(())
    }

    /// Detener una aplicación
    pub fn stop_app(&mut self, app_id: &str) -> Result<(), &'static str> {
        if let Some(pos) = self.running_apps.iter().position(|x| x == app_id) {
            self.running_apps.remove(pos);
            Ok(())
        } else {
            Err("Aplicación no está ejecutándose")
        }
    }

    /// Listar aplicaciones disponibles
    pub fn list_apps(&self) -> &Vec<AppInfo> {
        &self.apps
    }

    /// Listar aplicaciones en ejecución
    pub fn list_running_apps(&self) -> &Vec<String> {
        &self.running_apps
    }

    /// Obtener información de una aplicación
    pub fn get_app_info(&self, app_id: &str) -> Option<&AppInfo> {
        self.apps.iter().find(|app| app.id == app_id)
    }
}

/// Inicializar el gestor de aplicaciones
pub fn init_app_manager() -> AppManager {
    let mut manager = AppManager::new();
    
    // Registrar aplicaciones del sistema
    let system_apps = vec![
        AppInfo {
            id: "shell".to_string(),
            name: "Eclipse Shell".to_string(),
            description: "Terminal avanzado del sistema".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::System,
            status: AppStatus::Stopped,
            memory_usage: 1024,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
        AppInfo {
            id: "file_manager".to_string(),
            name: "File Manager".to_string(),
            description: "Gestor de archivos del sistema".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::System,
            status: AppStatus::Stopped,
            memory_usage: 2048,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
        AppInfo {
            id: "system_info".to_string(),
            name: "System Information".to_string(),
            description: "Información del sistema".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::System,
            status: AppStatus::Stopped,
            memory_usage: 512,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
        AppInfo {
            id: "text_editor".to_string(),
            name: "Text Editor".to_string(),
            description: "Editor de texto básico".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::Utility,
            status: AppStatus::Stopped,
            memory_usage: 1536,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
        AppInfo {
            id: "calculator".to_string(),
            name: "Calculator".to_string(),
            description: "Calculadora científica".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::Utility,
            status: AppStatus::Stopped,
            memory_usage: 256,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
        AppInfo {
            id: "task_manager".to_string(),
            name: "Task Manager".to_string(),
            description: "Gestor de tareas del sistema".to_string(),
            version: "1.0.0".to_string(),
            app_type: AppType::System,
            status: AppStatus::Stopped,
            memory_usage: 768,
            cpu_usage: 0.0,
            created_at: 0,
            updated_at: 0,
        },
    ];

    for app in system_apps {
        let _ = manager.register_app(app);
    }

    manager
}

/// Función principal para ejecutar aplicaciones
pub fn run_app(app_name: &str) -> Result<(), &'static str> {
    match app_name {
        "shell" => shell::run(),
        "file_manager" => file_manager::run(),
        "system_info" => system_info::run(),
        "text_editor" => text_editor::run(),
        "calculator" => calculator::run(),
        "task_manager" => task_manager::run(),
        _ => Err("Aplicación no encontrada"),
    }
}

/// Listar todas las aplicaciones disponibles
pub fn list_available_apps() -> Vec<&'static str> {
    vec![
        "shell",
        "file_manager", 
        "system_info",
        "text_editor",
        "calculator",
        "task_manager",
    ]
}
