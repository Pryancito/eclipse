//! # Aplicaciones del Sistema Userland

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Application {
    pub app_id: u32,
    pub name: String,
    pub executable_path: String,
    pub working_directory: String,
    pub arguments: Vec<String>,
    pub running: bool,
}

pub struct ApplicationManager {
    applications: Vec<Application>,
    next_app_id: u32,
}

impl ApplicationManager {
    pub fn new() -> Self {
        Self {
            applications: Vec::new(),
            next_app_id: 1,
        }
    }
    
    pub fn initialize(&mut self) -> Result<()> {
        // Crear aplicaciones del sistema
        self.create_system_applications()?;
        Ok(())
    }
    
    fn create_system_applications(&mut self) -> Result<()> {
        // Notepad
        let notepad = Application {
            app_id: 1,
            name: "Notepad".to_string(),
            executable_path: "C:\\Windows\\System32\\notepad.exe".to_string(),
            working_directory: "C:\\Windows\\System32".to_string(),
            arguments: Vec::new(),
            running: false,
        };
        self.applications.push(notepad);
        
        // Calculator
        let calculator = Application {
            app_id: 2,
            name: "Calculator".to_string(),
            executable_path: "C:\\Windows\\System32\\calc.exe".to_string(),
            working_directory: "C:\\Windows\\System32".to_string(),
            arguments: Vec::new(),
            running: false,
        };
        self.applications.push(calculator);
        
        // Command Prompt
        let cmd = Application {
            app_id: 3,
            name: "Command Prompt".to_string(),
            executable_path: "C:\\Windows\\System32\\cmd.exe".to_string(),
            working_directory: "C:\\".to_string(),
            arguments: Vec::new(),
            running: false,
        };
        self.applications.push(cmd);
        
        Ok(())
    }
    
    pub fn launch_application(&mut self, app_name: &str) -> Result<()> {
        if let Some(app) = self.applications.iter_mut().find(|a| a.name == app_name) {
            app.running = true;
            println!("Launching {}...", app.name);
        }
        Ok(())
    }
    
    pub fn terminate_application(&mut self, app_name: &str) -> Result<()> {
        if let Some(app) = self.applications.iter_mut().find(|a| a.name == app_name) {
            app.running = false;
            println!("Terminating {}...", app.name);
        }
        Ok(())
    }
    
    pub fn get_applications(&self) -> &Vec<Application> {
        &self.applications
    }
    
    pub fn get_running_applications(&self) -> Vec<&Application> {
        self.applications.iter().filter(|a| a.running).collect()
    }
}

static mut APPLICATION_MANAGER: Option<ApplicationManager> = None;

pub fn initialize() -> Result<()> {
    unsafe {
        APPLICATION_MANAGER = Some(ApplicationManager::new());
        if let Some(ref mut manager) = APPLICATION_MANAGER {
            manager.initialize()?;
        }
    }
    Ok(())
}

pub fn launch_application(app_name: &str) -> Result<()> {
    unsafe {
        if let Some(ref mut manager) = APPLICATION_MANAGER {
            manager.launch_application(app_name)
        } else {
            Err(anyhow::anyhow!("Application manager not initialized"))
        }
    }
}

pub fn terminate_application(app_name: &str) -> Result<()> {
    unsafe {
        if let Some(ref mut manager) = APPLICATION_MANAGER {
            manager.terminate_application(app_name)
        } else {
            Err(anyhow::anyhow!("Application manager not initialized"))
        }
    }
}

pub fn get_applications() -> Vec<Application> {
    unsafe {
        if let Some(ref manager) = APPLICATION_MANAGER {
            manager.get_applications().clone()
        } else {
            Vec::new()
        }
    }
}
