//! Resolvedor de dependencias para Eclipse SystemD
//! 
//! Este módulo resuelve las dependencias entre servicios y targets,
//! determinando el orden correcto de inicio.

use anyhow::Result;
use log::{debug, warn};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::service_parser::{ServiceFile, ServiceParser};

/// Tipo de dependencia
#[derive(Debug, Clone, PartialEq)]
pub enum DependencyType {
    Requires,    // Requerido (si falla, este también falla)
    Wants,       // Deseado (si falla, este continúa)
    After,       // Después de (orden de inicio)
    Before,      // Antes de (orden de inicio)
    Conflicts,   // Conflicto (no pueden ejecutarse juntos)
}

/// Dependencia entre servicios
#[derive(Debug, Clone)]
pub struct Dependency {
    pub from: String,
    pub to: String,
    pub dependency_type: DependencyType,
}

/// Resolvedor de dependencias
pub struct DependencyResolver {
    /// Grafo de dependencias
    dependencies: HashMap<String, Vec<Dependency>>,
    /// Servicios visitados durante la resolución
    visited: HashSet<String>,
    /// Servicios en proceso de resolución (para detectar ciclos)
    resolving: HashSet<String>,
}

impl DependencyResolver {
    /// Crea una nueva instancia del resolvedor de dependencias
    pub fn new() -> Self {
        Self {
            dependencies: HashMap::new(),
            visited: HashSet::new(),
            resolving: HashSet::new(),
        }
    }

    /// Resuelve las dependencias de un servicio
    pub fn resolve_dependencies(&self, service_file: &ServiceFile) -> Result<Vec<String>> {
        let service_name = self.extract_service_name(service_file);
        let mut dependencies = Vec::new();
        
        debug!("🔍 Resolviendo dependencias para: {}", service_name);
        
        // Obtener dependencias de la sección [Unit]
        if let Some(unit_entries) = ServiceParser::get_section_entries(service_file, "Unit") {
            // Requires
            if let Some(requires) = unit_entries.get("Requires") {
                for dep in requires.split_whitespace() {
                    dependencies.push(dep.to_string());
                    debug!("  📋 Requires: {}", dep);
                }
            }
            
            // Wants
            if let Some(wants) = unit_entries.get("Wants") {
                for dep in wants.split_whitespace() {
                    dependencies.push(dep.to_string());
                    debug!("  💭 Wants: {}", dep);
                }
            }
            
            // After
            if let Some(after) = unit_entries.get("After") {
                for dep in after.split_whitespace() {
                    dependencies.push(dep.to_string());
                    debug!("  ⏰ After: {}", dep);
                }
            }
        }
        
        // Resolver dependencias recursivamente
        let mut resolved_deps = Vec::new();
        for dep in dependencies {
            if !resolved_deps.contains(&dep) {
                resolved_deps.push(dep.clone());
                
                // Resolver dependencias de la dependencia
                if let Ok(sub_deps) = self.resolve_service_dependencies(&dep) {
                    for sub_dep in sub_deps {
                        if !resolved_deps.contains(&sub_dep) {
                            resolved_deps.push(sub_dep);
                        }
                    }
                }
            }
        }
        
        debug!("✅ Dependencias resueltas para {}: {:?}", service_name, resolved_deps);
        Ok(resolved_deps)
    }

    /// Resuelve dependencias de un servicio específico
    fn resolve_service_dependencies(&self, service_name: &str) -> Result<Vec<String>> {
        // En una implementación real, aquí se cargaría el archivo .service
        // y se resolverían sus dependencias
        // Por ahora, devolvemos dependencias básicas conocidas
        
        let mut deps = Vec::new();
        
        match service_name {
            "eclipse-gui.service" => {
                deps.push("multi-user.target".to_string());
                deps.push("network.service".to_string());
            }
            "eclipse-shell.service" => {
                deps.push("basic.target".to_string());
                deps.push("syslog.service".to_string());
            }
            "network.service" => {
                deps.push("basic.target".to_string());
            }
            "syslog.service" => {
                // No tiene dependencias
            }
            _ => {
                debug!("  ❓ Dependencias desconocidas para: {}", service_name);
            }
        }
        
        Ok(deps)
    }

    /// Extrae el nombre del servicio del archivo
    fn extract_service_name(&self, service_file: &ServiceFile) -> String {
        // En una implementación real, esto se obtendría del nombre del archivo
        // o de una entrada específica en el archivo .service
        "unknown.service".to_string()
    }

    /// Resuelve el orden de inicio de una lista de servicios
    pub fn resolve_startup_order(&self, services: &[String]) -> Result<Vec<String>> {
        debug!("🔄 Resolviendo orden de inicio para: {:?}", services);
        
        let mut graph = HashMap::new();
        let mut in_degree = HashMap::new();
        
        // Inicializar grafo
        for service in services {
            graph.insert(service.clone(), Vec::new());
            in_degree.insert(service.clone(), 0);
        }
        
        // Construir grafo de dependencias
        for service in services {
            if let Ok(deps) = self.resolve_service_dependencies(service) {
                for dep in deps {
                    if services.contains(&dep) {
                        graph.get_mut(&dep).unwrap().push(service.clone());
                        *in_degree.get_mut(service).unwrap() += 1;
                    }
                }
            }
        }
        
        // Ordenamiento topológico
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        
        // Agregar servicios sin dependencias
        for (service, &degree) in &in_degree {
            if degree == 0 {
                queue.push_back(service.clone());
            }
        }
        
        while let Some(service) = queue.pop_front() {
            result.push(service.clone());
            
            // Reducir grado de dependientes
            if let Some(dependents) = graph.get(&service) {
                for dependent in dependents {
                    let degree = in_degree.get_mut(dependent).unwrap();
                    *degree -= 1;
                    
                    if *degree == 0 {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }
        
        // Verificar si hay ciclos
        if result.len() != services.len() {
            warn!("⚠️  Ciclo detectado en dependencias de servicios");
            return Err(anyhow::anyhow!("Ciclo detectado en dependencias"));
        }
        
        debug!("✅ Orden de inicio resuelto: {:?}", result);
        Ok(result)
    }

    /// Verifica si hay conflictos entre servicios
    pub fn check_conflicts(&self, services: &[String]) -> Result<Vec<(String, String)>> {
        let mut conflicts = Vec::new();
        
        for i in 0..services.len() {
            for j in i + 1..services.len() {
                if self.services_conflict(&services[i], &services[j]) {
                    conflicts.push((services[i].clone(), services[j].clone()));
                }
            }
        }
        
        if !conflicts.is_empty() {
            warn!("⚠️  Conflictos detectados: {:?}", conflicts);
        }
        
        Ok(conflicts)
    }

    /// Verifica si dos servicios entran en conflicto
    fn services_conflict(&self, service1: &str, service2: &str) -> bool {
        // En una implementación real, aquí se verificarían las entradas
        // Conflicts en los archivos .service
        // Por ahora, simulamos algunos conflictos conocidos
        
        match (service1, service2) {
            ("multi-user.target", "graphical.target") => true,
            ("graphical.target", "multi-user.target") => true,
            _ => false,
        }
    }

    /// Obtiene información de dependencias
    pub fn get_dependency_info(&self, service_name: &str) -> DependencyInfo {
        let mut info = DependencyInfo {
            service_name: service_name.to_string(),
            requires: Vec::new(),
            wants: Vec::new(),
            after: Vec::new(),
            before: Vec::new(),
            conflicts: Vec::new(),
        };
        
        // En una implementación real, aquí se cargaría el archivo .service
        // y se extraerían las dependencias
        
        match service_name {
            "eclipse-gui.service" => {
                info.requires.push("multi-user.target".to_string());
                info.wants.push("network.service".to_string());
                info.after.push("multi-user.target".to_string());
                info.conflicts.push("multi-user.target".to_string());
            }
            "eclipse-shell.service" => {
                info.requires.push("basic.target".to_string());
                info.wants.push("syslog.service".to_string());
                info.after.push("basic.target".to_string());
            }
            _ => {}
        }
        
        info
    }
}

/// Información de dependencias de un servicio
#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub service_name: String,
    pub requires: Vec<String>,
    pub wants: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub conflicts: Vec<String>,
}

impl DependencyInfo {
    pub fn get_summary(&self) -> String {
        format!(
            "{}: {} requires, {} wants, {} after, {} before, {} conflicts",
            self.service_name,
            self.requires.len(),
            self.wants.len(),
            self.after.len(),
            self.before.len(),
            self.conflicts.len()
        )
    }
}
