//! Parser de archivos .service de systemd
//! 
//! Este módulo parsea archivos de configuración .service
//! siguiendo el formato estándar de systemd

use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Sección de un archivo .service
#[derive(Debug, Clone)]
pub struct ServiceSection {
    pub name: String,
    pub entries: HashMap<String, String>,
}

/// Archivo .service parseado
#[derive(Debug, Clone)]
pub struct ServiceFile {
    pub sections: Vec<ServiceSection>,
}

/// Parser de archivos .service
pub struct ServiceParser;

impl ServiceParser {
    /// Parsea un archivo .service
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<ServiceFile> {
        let content = fs::read_to_string(path)?;
        Self::parse_content(&content)
    }

    /// Parsea el contenido de un archivo .service
    pub fn parse_content(content: &str) -> Result<ServiceFile> {
        let mut sections = Vec::new();
        let mut current_section = None;
        let mut current_entries = HashMap::new();

        for line in content.lines() {
            let line = line.trim();
            
            // Saltar líneas vacías y comentarios
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Nueva sección
            if line.starts_with('[') && line.ends_with(']') {
                // Guardar sección anterior
                if let Some(section_name) = current_section {
                    sections.push(ServiceSection {
                        name: section_name,
                        entries: current_entries,
                    });
                }

                // Iniciar nueva sección
                let section_name = line[1..line.len()-1].to_string();
                current_section = Some(section_name);
                current_entries = HashMap::new();
            }
            // Entrada de configuración
            else if let Some((key, value)) = Self::parse_entry(line) {
                current_entries.insert(key, value);
            }
        }

        // Guardar última sección
        if let Some(section_name) = current_section {
            sections.push(ServiceSection {
                name: section_name,
                entries: current_entries,
            });
        }

        Ok(ServiceFile { sections })
    }

    /// Parsea una línea de entrada
    fn parse_entry(line: &str) -> Option<(String, String)> {
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let value = line[eq_pos + 1..].trim().to_string();
            Some((key, value))
        } else {
            None
        }
    }

    /// Obtiene una entrada de una sección específica
    pub fn get_entry<'a>(service_file: &'a ServiceFile, section: &str, key: &str) -> Option<&'a String> {
        service_file.sections
            .iter()
            .find(|s| s.name == section)
            .and_then(|s| s.entries.get(key))
    }

    /// Obtiene todas las entradas de una sección
    pub fn get_section_entries<'a>(service_file: &'a ServiceFile, section: &str) -> Option<&'a HashMap<String, String>> {
        service_file.sections
            .iter()
            .find(|s| s.name == section)
            .map(|s| &s.entries)
    }

    /// Verifica si una sección existe
    pub fn has_section(service_file: &ServiceFile, section: &str) -> bool {
        service_file.sections
            .iter()
            .any(|s| s.name == section)
    }

    /// Lista todas las secciones
    pub fn list_sections(service_file: &ServiceFile) -> Vec<&str> {
        service_file.sections
            .iter()
            .map(|s| s.name.as_str())
            .collect()
    }
}

/// Validador de archivos .service
pub struct ServiceValidator;

impl ServiceValidator {
    /// Valida un archivo .service
    pub fn validate(service_file: &ServiceFile) -> Result<Vec<String>> {
        let mut errors = Vec::new();

        // Verificar que tiene sección [Unit]
        if !ServiceParser::has_section(service_file, "Unit") {
            errors.push("Missing [Unit] section".to_string());
        }

        // Verificar que tiene sección [Service] o [Install]
        if !ServiceParser::has_section(service_file, "Service") && 
           !ServiceParser::has_section(service_file, "Install") {
            errors.push("Missing [Service] or [Install] section".to_string());
        }

        // Validar sección [Unit]
        if let Some(unit_entries) = ServiceParser::get_section_entries(service_file, "Unit") {
            Self::validate_unit_section(unit_entries, &mut errors);
        }

        // Validar sección [Service]
        if let Some(service_entries) = ServiceParser::get_section_entries(service_file, "Service") {
            Self::validate_service_section(service_entries, &mut errors);
        }

        // Validar sección [Install]
        if let Some(install_entries) = ServiceParser::get_section_entries(service_file, "Install") {
            Self::validate_install_section(install_entries, &mut errors);
        }

        Ok(errors)
    }

    /// Valida la sección [Unit]
    fn validate_unit_section(entries: &HashMap<String, String>, errors: &mut Vec<String>) {
        // Validar Description
        if !entries.contains_key("Description") {
            errors.push("[Unit] section missing Description".to_string());
        }

        // Validar After (si existe)
        if let Some(after) = entries.get("After") {
            if after.is_empty() {
                errors.push("[Unit] After cannot be empty".to_string());
            }
        }

        // Validar Requires (si existe)
        if let Some(requires) = entries.get("Requires") {
            if requires.is_empty() {
                errors.push("[Unit] Requires cannot be empty".to_string());
            }
        }
    }

    /// Valida la sección [Service]
    fn validate_service_section(entries: &HashMap<String, String>, errors: &mut Vec<String>) {
        // Validar Type
        if let Some(service_type) = entries.get("Type") {
            let valid_types = ["simple", "forking", "oneshot", "dbus", "notify", "idle"];
            if !valid_types.contains(&service_type.as_str()) {
                errors.push(format!("[Service] Invalid Type: {}", service_type));
            }
        }

        // Validar ExecStart
        if !entries.contains_key("ExecStart") {
            errors.push("[Service] section missing ExecStart".to_string());
        }

        // Validar User
        if let Some(user) = entries.get("User") {
            if user.is_empty() {
                errors.push("[Service] User cannot be empty".to_string());
            }
        }

        // Validar Group
        if let Some(group) = entries.get("Group") {
            if group.is_empty() {
                errors.push("[Service] Group cannot be empty".to_string());
            }
        }

        // Validar Restart
        if let Some(restart) = entries.get("Restart") {
            let valid_restarts = ["no", "on-success", "on-failure", "on-abnormal", "on-watchdog", "on-abort", "always"];
            if !valid_restarts.contains(&restart.as_str()) {
                errors.push(format!("[Service] Invalid Restart: {}", restart));
            }
        }
    }

    /// Valida la sección [Install]
    fn validate_install_section(entries: &HashMap<String, String>, errors: &mut Vec<String>) {
        // Validar WantedBy
        if let Some(wanted_by) = entries.get("WantedBy") {
            if wanted_by.is_empty() {
                errors.push("[Install] WantedBy cannot be empty".to_string());
            }
        }
    }
}
