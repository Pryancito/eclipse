//! Manager de targets para Eclipse SystemD
//! 
//! Este módulo gestiona los targets (objetivos) de systemd como
//! multi-user.target, graphical.target, etc.

use anyhow::Result;
use log::{info, debug, warn};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Target de systemd
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub description: String,
    pub services: Vec<String>,
    pub dependencies: Vec<String>,
    pub conflicts: Vec<String>,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// Manager de targets
pub struct TargetManager {
    targets: HashMap<String, Target>,
    target_dir: String,
}

impl TargetManager {
    /// Crea una nueva instancia del manager de targets
    pub fn new() -> Self {
        Self {
            targets: HashMap::new(),
            target_dir: "/etc/eclipse/systemd/system".to_string(),
        }
    }

    /// Inicializa el manager de targets
    pub fn initialize(&mut self) -> Result<()> {
        info!("Target Inicializando Target Manager");
        
        // Cargar targets predefinidos
        self.load_predefined_targets()?;
        
        // Cargar targets desde archivos
        self.load_target_files()?;
        
        info!("Servicio Target Manager inicializado con {} targets", self.targets.len());
        Ok(())
    }

    /// Carga targets predefinidos del sistema
    fn load_predefined_targets(&mut self) -> Result<()> {
        // Target básico
        let basic_target = Target {
            name: "basic.target".to_string(),
            description: "Basic System Target".to_string(),
            services: vec![
                "syslog.service".to_string(),
                "network.service".to_string(),
            ],
            dependencies: vec![],
            conflicts: vec![],
            before: vec![],
            after: vec![],
        };
        
        // Target multi-usuario
        let multi_user_target = Target {
            name: "multi-user.target".to_string(),
            description: "Multi-User System Target".to_string(),
            services: vec![
                "basic.target".to_string(),
                "eclipse-shell.service".to_string(),
            ],
            dependencies: vec!["basic.target".to_string()],
            conflicts: vec!["graphical.target".to_string()],
            before: vec![],
            after: vec!["basic.target".to_string()],
        };
        
        // Target gráfico
        let graphical_target = Target {
            name: "graphical.target".to_string(),
            description: "Graphical System Target".to_string(),
            services: vec![
                "multi-user.target".to_string(),
                "eclipse-gui.service".to_string(),
            ],
            dependencies: vec!["multi-user.target".to_string()],
            conflicts: vec!["multi-user.target".to_string()],
            before: vec![],
            after: vec!["multi-user.target".to_string()],
        };

        // Agregar targets predefinidos
        self.targets.insert("basic.target".to_string(), basic_target);
        self.targets.insert("multi-user.target".to_string(), multi_user_target);
        self.targets.insert("graphical.target".to_string(), graphical_target);
        
        debug!("📦 Targets predefinidos cargados: basic, multi-user, graphical");
        Ok(())
    }

    /// Carga targets desde archivos .target
    fn load_target_files(&mut self) -> Result<()> {
        let target_dir_path = Path::new(&self.target_dir);
        if !target_dir_path.exists() {
            warn!("Advertencia  Directorio de targets no encontrado: {}", self.target_dir);
            return Ok(());
        }

        if let Ok(entries) = fs::read_dir(target_dir_path) {
            for entry in entries {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    
                    if path.extension().and_then(|s| s.to_str()) == Some("target") {
                        let target_name = path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        
                        debug!("Buscando Cargando target: {}", target_name);
                        
                        match self.parse_target_file(&path) {
                            Ok(target) => {
                                let target_name_clone = target_name.clone();
                                self.targets.insert(target_name, target);
                                debug!("  Servicio Target cargado: {}", target_name_clone);
                            }
                            Err(e) => {
                                warn!("  Error Error cargando target {}: {}", target_name, e);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Parsea un archivo .target
    fn parse_target_file(&self, path: &Path) -> Result<Target> {
        let content = fs::read_to_string(path)?;
        self.parse_target_content(&content)
    }

    /// Parsea el contenido de un archivo .target
    fn parse_target_content(&self, content: &str) -> Result<Target> {
        let mut name = String::new();
        let mut description = String::new();
        let mut services = Vec::new();
        let mut dependencies = Vec::new();
        let mut conflicts = Vec::new();
        let mut before = Vec::new();
        let mut after = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            
            // Saltar líneas vacías y comentarios
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parsear entradas
            if let Some((key, value)) = self.parse_entry(line) {
                match key.as_str() {
                    "Description" => description = value,
                    "Wants" => {
                        services.extend(value.split_whitespace().map(|s| s.to_string()));
                    }
                    "Requires" => {
                        dependencies.extend(value.split_whitespace().map(|s| s.to_string()));
                    }
                    "Conflicts" => {
                        conflicts.extend(value.split_whitespace().map(|s| s.to_string()));
                    }
                    "Before" => {
                        before.extend(value.split_whitespace().map(|s| s.to_string()));
                    }
                    "After" => {
                        after.extend(value.split_whitespace().map(|s| s.to_string()));
                    }
                    _ => {
                        debug!("  Registrando Entrada desconocida: {} = {}", key, value);
                    }
                }
            }
        }

        Ok(Target {
            name,
            description,
            services,
            dependencies,
            conflicts,
            before,
            after,
        })
    }

    /// Parsea una línea de entrada
    fn parse_entry(&self, line: &str) -> Option<(String, String)> {
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let value = line[eq_pos + 1..].trim().to_string();
            Some((key, value))
        } else {
            None
        }
    }

    /// Obtiene los servicios de un target
    pub fn get_target_services(&self, target_name: &str) -> Result<Vec<String>> {
        if let Some(target) = self.targets.get(target_name) {
            let mut all_services = Vec::new();
            
            // Agregar servicios del target
            all_services.extend(target.services.clone());
            
            // Agregar servicios de dependencias
            for dep in &target.dependencies {
                if let Ok(dep_services) = self.get_target_services(dep) {
                    all_services.extend(dep_services);
                }
            }
            
            // Remover duplicados
            all_services.sort();
            all_services.dedup();
            
            Ok(all_services)
        } else {
            Err(anyhow::anyhow!("Target no encontrado: {}", target_name))
        }
    }

    /// Obtiene un target por nombre
    pub fn get_target(&self, target_name: &str) -> Option<&Target> {
        self.targets.get(target_name)
    }

    /// Lista todos los targets disponibles
    pub fn list_targets(&self) -> Vec<&str> {
        self.targets.keys().map(|s| s.as_str()).collect()
    }

    /// Verifica si un target existe
    pub fn target_exists(&self, target_name: &str) -> bool {
        self.targets.contains_key(target_name)
    }

    /// Obtiene información de un target
    pub fn get_target_info(&self, target_name: &str) -> Option<TargetInfo> {
        if let Some(target) = self.targets.get(target_name) {
            Some(TargetInfo {
                name: target.name.clone(),
                description: target.description.clone(),
                service_count: target.services.len(),
                dependency_count: target.dependencies.len(),
                conflict_count: target.conflicts.len(),
            })
        } else {
            None
        }
    }
}

/// Información de un target
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub name: String,
    pub description: String,
    pub service_count: usize,
    pub dependency_count: usize,
    pub conflict_count: usize,
}

impl TargetInfo {
    pub fn get_summary(&self) -> String {
        format!(
            "{}: {} ({} servicios, {} dependencias, {} conflictos)",
            self.name, self.description, self.service_count, 
            self.dependency_count, self.conflict_count
        )
    }
}
