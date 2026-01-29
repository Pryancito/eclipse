//! Application Launcher

use heapless::{String, Vec};

/// Maximum number of apps
pub const MAX_APPS: usize = 64;

/// Application entry
pub struct Application {
    pub name: String<64>,
    pub exec_path: String<128>,
    pub icon_path: String<128>,
    pub categories: String<64>,
}

impl Application {
    pub fn new(name: &str, exec_path: &str) -> Self {
        let mut app_name = String::new();
        let mut app_exec = String::new();
        
        for c in name.chars().take(63) {
            let _ = app_name.push(c);
        }
        for c in exec_path.chars().take(127) {
            let _ = app_exec.push(c);
        }

        Self {
            name: app_name,
            exec_path: app_exec,
            icon_path: String::new(),
            categories: String::new(),
        }
    }

    pub fn launch(&self) -> Result<(), &'static str> {
        // In real implementation:
        // 1. Fork process
        // 2. Exec the application
        // 3. Set up environment
        Ok(())
    }
}

/// Application launcher
pub struct AppLauncher {
    pub apps: Vec<Application, MAX_APPS>,
    pub search_query: String<128>,
    pub visible: bool,
}

impl AppLauncher {
    pub fn new() -> Self {
        let mut launcher = Self {
            apps: Vec::new(),
            search_query: String::new(),
            visible: false,
        };

        // Register some default applications
        launcher.register_app("Terminal", "/usr/bin/terminal");
        launcher.register_app("Calculator", "/usr/bin/calculator");
        launcher.register_app("Text Editor", "/usr/bin/editor");
        launcher.register_app("File Manager", "/usr/bin/files");
        launcher.register_app("Settings", "/usr/bin/settings");

        launcher
    }

    pub fn register_app(&mut self, name: &str, exec_path: &str) {
        let app = Application::new(name, exec_path);
        let _ = self.apps.push(app);
    }

    pub fn search(&mut self, query: &str) {
        self.search_query.clear();
        for c in query.chars().take(127) {
            let _ = self.search_query.push(c);
        }
    }

    pub fn get_filtered_apps(&self) -> Vec<&Application, MAX_APPS> {
        let mut filtered = Vec::new();
        
        if self.search_query.is_empty() {
            // Return all apps
            for app in self.apps.iter() {
                let _ = filtered.push(app);
            }
        } else {
            // Filter by search query
            for app in self.apps.iter() {
                if app.name.as_str().to_lowercase().contains(self.search_query.as_str().to_lowercase().as_str()) {
                    let _ = filtered.push(app);
                }
            }
        }

        filtered
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn show(&mut self) {
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }
}
