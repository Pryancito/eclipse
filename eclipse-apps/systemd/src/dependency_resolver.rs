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
#[allow(dead_code)]
pub enum DependencyType {
    Requires,    // Requerido (si falla, este también falla)
    Wants,       // Deseado (si falla, este continúa)
    After,       // Después de (orden de inicio)
    Before,      // Antes de (orden de inicio)
    Conflicts,   // Conflicto (no pueden ejecutarse juntos)
}

/// Dependencia entre servicios
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Dependency {
    pub from: String,
    pub to: String,
    pub dependency_type: DependencyType,
}

/// Resolvedor de dependencias
#[allow(dead_code)]
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
        let mut required_deps = Vec::new();

        debug!("Buscando Resolviendo dependencias para: {}", service_name);

        // Obtener dependencias de la sección [Unit]
        if let Some(unit_entries) = ServiceParser::get_section_entries(service_file, "Unit") {
            // Requires - dependencias obligatorias
            if let Some(requires) = unit_entries.get("Requires") {
                for dep in requires.split_whitespace() {
                    let dep_name = dep.to_string();
                    dependencies.push(dep_name.clone());
                    required_deps.push(dep_name);
                    debug!("  Requerido Requires: {}", dep);
                }
            }

            // Wants - dependencias opcionales
            if let Some(wants) = unit_entries.get("Wants") {
                for dep in wants.split_whitespace() {
                    dependencies.push(dep.to_string());
                    debug!("  Opcional Wants: {}", dep);
                }
            }

            // After - orden de inicio
            if let Some(after) = unit_entries.get("After") {
                for dep in after.split_whitespace() {
                    dependencies.push(dep.to_string());
                    debug!("  Temporizador After: {}", dep);
                }
            }

            // Before - orden de inicio inverso
            if let Some(before) = unit_entries.get("Before") {
                debug!("  Temporizador Before: {} (no afecta orden de resolución)", before);
            }

            // Conflicts - servicios incompatibles
            if let Some(conflicts) = unit_entries.get("Conflicts") {
                debug!("  Conflicto  Conflicts: {} (no afecta resolución)", conflicts);
            }
        }

        // Resolver dependencias recursivamente
        let mut resolved_deps = Vec::new();
        let mut visited = HashSet::new();

        for dep in dependencies {
            if !visited.contains(&dep) {
                self.resolve_recursive(&dep, &mut resolved_deps, &mut visited, &required_deps)?;
            }
        }

        debug!("Servicio Dependencias resueltas para {}: {:?}", service_name, resolved_deps);
        Ok(resolved_deps)
    }

    /// Resuelve dependencias recursivamente con detección de ciclos
    fn resolve_recursive(&self, service_name: &str, resolved_deps: &mut Vec<String>,
                        visited: &mut HashSet<String>, required_deps: &[String]) -> Result<()> {
        // Evitar ciclos
        if visited.contains(service_name) {
            return Ok(());
        }

        visited.insert(service_name.to_string());

        // Obtener dependencias del servicio
        let deps = self.resolve_service_dependencies(service_name)?;

        // Resolver dependencias de cada dependencia primero
        for dep in deps {
            if !visited.contains(&dep) {
                self.resolve_recursive(&dep, resolved_deps, visited, required_deps)?;
            }
        }

        // Agregar esta dependencia a la lista resuelta
        if !resolved_deps.contains(&service_name.to_string()) {
            resolved_deps.push(service_name.to_string());
        }

        Ok(())
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
            "multi-user.target" => {
                deps.push("basic.target".to_string());
            }
            "graphical.target" => {
                deps.push("multi-user.target".to_string());
            }
            "syslog.service" => {
                // No tiene dependencias
            }
            "basic.target" => {
                deps.push("syslog.service".to_string());
            }
            _ => {
                debug!("  ❓ Dependencias desconocidas para: {}", service_name);
            }
        }

        Ok(deps)
    }

    /// Extrae el nombre del servicio del archivo
    fn extract_service_name(&self, _service_file: &ServiceFile) -> String {
        // En una implementación real, esto se obtendría del nombre del archivo
        // o de una entrada específica en el archivo .service
        "unknown.service".to_string()
    }

    /// Resuelve el orden de inicio de una lista de servicios
    #[allow(dead_code)]
    pub fn resolve_startup_order(&self, services: &[String]) -> Result<Vec<String>> {
        debug!("Reiniciando Resolviendo orden de inicio para: {:?}", services);
        
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
                        if let Some(dep_list) = graph.get_mut(&dep) {
                            dep_list.push(service.clone());
                        }
                        if let Some(degree) = in_degree.get_mut(service) {
                            *degree += 1;
                        }
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
                    if let Some(degree) = in_degree.get_mut(dependent) {
                        *degree -= 1;
                        
                        if *degree == 0 {
                            queue.push_back(dependent.clone());
                        }
                    }
                }
            }
        }
        
        // Verificar si hay ciclos
        if result.len() != services.len() {
            warn!("Advertencia  Ciclo detectado en dependencias de servicios");
            return Err(anyhow::anyhow!("Ciclo detectado en dependencias"));
        }
        
        debug!("Servicio Orden de inicio resuelto: {:?}", result);
        Ok(result)
    }

    /// Verifica si hay conflictos entre servicios
    #[allow(dead_code)]
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
            warn!("Advertencia  Conflictos detectados: {:?}", conflicts);
        }
        
        Ok(conflicts)
    }

    /// Verifica si dos servicios entran en conflicto
    #[allow(dead_code)]
    fn services_conflict(&self, service1: &str, service2: &str) -> bool {
        // En una implementación real, aquí se verificarían las entradas
        // Conflicts en los archivos .service
        // Por ahora, simulamos algunos conflictos conocidos

        match (service1, service2) {
            ("multi-user.target", "graphical.target") => true,
            ("graphical.target", "multi-user.target") => true,
            ("shutdown.target", _) => true,
            (_, "shutdown.target") => true,
            _ => false,
        }
    }

    /// Valida las dependencias de un servicio
    #[allow(dead_code)]
    pub fn validate_dependencies(&self, service_name: &str, available_services: &[String]) -> Result<Vec<String>> {
        let mut warnings = Vec::new();

        let deps = self.resolve_service_dependencies(service_name)?;

        for dep in deps {
            if !available_services.contains(&dep) {
                warnings.push(format!("Dependencia '{}' no encontrada para '{}'", dep, service_name));
            }
        }

        // Verificar conflictos
        for other_service in available_services {
            if self.services_conflict(service_name, other_service) {
                warnings.push(format!("Conflicto detectado entre '{}' y '{}'", service_name, other_service));
            }
        }

        Ok(warnings)
    }

    /// Obtiene el grafo completo de dependencias
    #[allow(dead_code)]
    pub fn build_dependency_graph(&self, services: &[String]) -> Result<HashMap<String, Vec<String>>> {
        let mut graph = HashMap::new();

        for service in services {
            let deps = self.resolve_service_dependencies(service)?;
            graph.insert(service.clone(), deps);
        }

        Ok(graph)
    }

    /// Encuentra servicios huérfanos (sin dependencias)
    #[allow(dead_code)]
    pub fn find_orphan_services(&self, services: &[String]) -> Vec<String> {
        let mut orphans = Vec::new();

        for service in services {
            if let Ok(deps) = self.resolve_service_dependencies(service) {
                if deps.is_empty() {
                    orphans.push(service.clone());
                }
            }
        }

        orphans
    }

    /// Obtiene información de dependencias
    #[allow(dead_code)]
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
#[allow(dead_code)]
pub struct DependencyInfo {
    pub service_name: String,
    pub requires: Vec<String>,
    pub wants: Vec<String>,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub conflicts: Vec<String>,
}

impl DependencyInfo {
    #[allow(dead_code)]
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
