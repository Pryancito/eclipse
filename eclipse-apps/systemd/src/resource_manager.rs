//! Manager de recursos para Eclipse SystemD
//!
//! Este módulo implementa la gestión avanzada de recursos del sistema
//! incluyendo CPU, memoria, I/O y otros recursos del sistema.

use anyhow::Result;
use log::{info, warn, debug};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Información de uso de CPU
#[derive(Debug, Clone)]
pub struct CpuUsage {
    pub user_time: u64,
    pub system_time: u64,
    pub total_time: u64,
    pub usage_percent: f32,
}

/// Información de uso de memoria
#[derive(Debug, Clone)]
pub struct MemoryUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub cached: u64,
    pub buffers: u64,
    pub usage_percent: f32,
}

/// Información de uso de I/O
#[derive(Debug, Clone)]
pub struct IoUsage {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_operations: u64,
    pub write_operations: u64,
    pub io_time: u64,
}

/// Límites de recursos para un servicio
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    pub cpu_quota: u32,        // Porcentaje de CPU (0-100)
    pub memory_limit: u64,     // Límite de memoria en bytes (0 = ilimitado)
    pub io_weight: u32,        // Peso de I/O (10-1000)
    pub max_processes: u32,    // Número máximo de procesos
    pub nice_level: i32,       // Nivel nice (-20 a 19)
}

/// Estadísticas de recursos de un servicio
#[derive(Debug, Clone)]
pub struct ServiceResourceStats {
    pub service_name: String,
    pub cpu_usage: CpuUsage,
    pub memory_usage: MemoryUsage,
    pub io_usage: IoUsage,
    pub limits: ResourceLimits,
    pub last_update: Instant,
}

/// Manager de recursos del sistema
pub struct ResourceManager {
    /// Estadísticas de servicios
    service_stats: Arc<Mutex<HashMap<String, ServiceResourceStats>>>,
    /// Límites por defecto
    default_limits: ResourceLimits,
    /// Intervalo de monitoreo
    monitor_interval: Duration,
    /// Historial de uso
    usage_history: Arc<Mutex<HashMap<String, Vec<(Instant, CpuUsage, MemoryUsage)>>>>,
    /// Tamaño máximo del historial
    max_history_size: usize,
}

impl ResourceManager {
    /// Crea una nueva instancia del manager de recursos
    pub fn new() -> Self {
        Self {
            service_stats: Arc::new(Mutex::new(HashMap::new())),
            default_limits: ResourceLimits {
                cpu_quota: 100,
                memory_limit: 0, // Sin límite
                io_weight: 100,
                max_processes: 1024,
                nice_level: 0,
            },
            monitor_interval: Duration::from_secs(5),
            usage_history: Arc::new(Mutex::new(HashMap::new())),
            max_history_size: 100,
        }
    }

    /// Registra un servicio para monitoreo de recursos
    pub fn register_service(&self, service_name: &str, pid: Option<u32>) -> Result<()> {
        let mut stats = self.service_stats.lock().unwrap();

        let service_stats = ServiceResourceStats {
            service_name: service_name.to_string(),
            cpu_usage: CpuUsage {
                user_time: 0,
                system_time: 0,
                total_time: 0,
                usage_percent: 0.0,
            },
            memory_usage: MemoryUsage {
                total: 0,
                used: 0,
                free: 0,
                cached: 0,
                buffers: 0,
                usage_percent: 0.0,
            },
            io_usage: IoUsage {
                read_bytes: 0,
                write_bytes: 0,
                read_operations: 0,
                write_operations: 0,
                io_time: 0,
            },
            limits: self.default_limits.clone(),
            last_update: Instant::now(),
        };

        stats.insert(service_name.to_string(), service_stats);
        debug!("Estado Servicio {} registrado para monitoreo de recursos", service_name);
        Ok(())
    }

    /// Actualiza las estadísticas de un servicio
    pub fn update_service_stats(&self, service_name: &str, pid: Option<u32>) -> Result<()> {
        let mut stats = self.service_stats.lock().unwrap();

        if let Some(service_stat) = stats.get_mut(service_name) {
            // Actualizar uso de CPU
            if let Some(pid) = pid {
                if let Ok(cpu_usage) = self.get_process_cpu_usage(pid) {
                    service_stat.cpu_usage = cpu_usage;
                }
                if let Ok(memory_usage) = self.get_process_memory_usage(pid) {
                    service_stat.memory_usage = memory_usage;
                }
                if let Ok(io_usage) = self.get_process_io_usage(pid) {
                    service_stat.io_usage = io_usage;
                }
            }

            // Actualizar uso del sistema
            if let Ok(system_memory) = self.get_system_memory_usage() {
                // El uso de memoria del servicio es relativo al sistema
                service_stat.memory_usage.total = system_memory.total;
                service_stat.memory_usage.free = system_memory.free;
                service_stat.memory_usage.cached = system_memory.cached;
                service_stat.memory_usage.buffers = system_memory.buffers;
            }

            service_stat.last_update = Instant::now();

            // Agregar al historial
            self.add_to_history(service_name, &service_stat.cpu_usage, &service_stat.memory_usage);

            debug!("Estado Estadísticas actualizadas para servicio: {}", service_name);
        }

        Ok(())
    }

    /// Establece límites de recursos para un servicio
    pub fn set_service_limits(&self, service_name: &str, limits: ResourceLimits) -> Result<()> {
        let mut stats = self.service_stats.lock().unwrap();

        if let Some(service_stat) = stats.get_mut(service_name) {
            service_stat.limits = limits.clone();
            debug!("Aplicando Límites de recursos actualizados para {}: {:?}", service_name, limits);

            // Aplicar límites al proceso si está ejecutándose
            // Nota: En un sistema real, aquí se aplicarían límites usando cgroups
            self.apply_resource_limits(service_name, &limits)?;
        } else {
            return Err(anyhow::anyhow!("Servicio no encontrado: {}", service_name));
        }

        Ok(())
    }

    /// Obtiene estadísticas de recursos de un servicio
    pub fn get_service_stats(&self, service_name: &str) -> Option<ServiceResourceStats> {
        let stats = self.service_stats.lock().unwrap();
        stats.get(service_name).cloned()
    }

    /// Obtiene estadísticas de todos los servicios
    pub fn get_all_service_stats(&self) -> Vec<ServiceResourceStats> {
        let stats = self.service_stats.lock().unwrap();
        stats.values().cloned().collect()
    }

    /// Obtiene el historial de uso de un servicio
    pub fn get_service_history(&self, service_name: &str, limit: Option<usize>) -> Vec<(Instant, CpuUsage, MemoryUsage)> {
        let history = self.usage_history.lock().unwrap();
        if let Some(service_history) = history.get(service_name) {
            let limit = limit.unwrap_or(service_history.len());
            service_history.iter().rev().take(limit).cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Verifica si un servicio excede sus límites
    pub fn check_resource_limits(&self, service_name: &str) -> Result<Vec<String>> {
        let mut violations = Vec::new();
        let stats = self.service_stats.lock().unwrap();

        if let Some(service_stat) = stats.get(service_name) {
            let limits = &service_stat.limits;

            // Verificar límite de CPU
            if limits.cpu_quota > 0 && service_stat.cpu_usage.usage_percent > limits.cpu_quota as f32 {
                violations.push(format!(
                    "Uso de CPU {:.1}% excede límite de {}%",
                    service_stat.cpu_usage.usage_percent,
                    limits.cpu_quota
                ));
            }

            // Verificar límite de memoria
            if limits.memory_limit > 0 && service_stat.memory_usage.used > limits.memory_limit {
                violations.push(format!(
                    "Uso de memoria {} MB excede límite de {} MB",
                    service_stat.memory_usage.used / 1024 / 1024,
                    limits.memory_limit / 1024 / 1024
                ));
            }

            // Verificar límite de procesos
            // Nota: En una implementación real, contaríamos los procesos del servicio
        }

        Ok(violations)
    }

    /// Obtiene estadísticas del sistema
    pub fn get_system_stats(&self) -> Result<SystemStats> {
        let cpu_usage = self.get_system_cpu_usage()?;
        let memory_usage = self.get_system_memory_usage()?;
        let load_average = self.get_system_load_average()?;

        Ok(SystemStats {
            cpu_usage,
            memory_usage,
            load_average,
            uptime: self.get_system_uptime(),
        })
    }

    /// Inicia el monitoreo continuo de recursos
    pub async fn start_monitoring(&self) -> Result<()> {
        info!("Buscando Iniciando monitoreo de recursos del sistema");

        loop {
            // Actualizar estadísticas de todos los servicios
            let service_names: Vec<String> = {
                let stats = self.service_stats.lock().unwrap();
                stats.keys().cloned().collect()
            };

            for service_name in &service_names {
                // Nota: En una implementación real, obtendríamos el PID del servicio
                if let Err(e) = self.update_service_stats(service_name, None) {
                    warn!("Advertencia  Error actualizando estadísticas de {}: {}", service_name, e);
                }

                // Verificar límites
                if let Ok(violations) = self.check_resource_limits(service_name) {
                    for violation in violations {
                        warn!("Advertencia  Violación de límite en {}: {}", service_name, violation);
                    }
                }
            }

            sleep(self.monitor_interval).await;
        }
    }

    /// Obtiene uso de CPU de un proceso
    fn get_process_cpu_usage(&self, pid: u32) -> Result<CpuUsage> {
        let stat_path = format!("/proc/{}/stat", pid);
        let stat_content = std::fs::read_to_string(&stat_path)?;

        let parts: Vec<&str> = stat_content.split_whitespace().collect();
        if parts.len() < 17 {
            return Err(anyhow::anyhow!("Formato de /proc/stat inválido"));
        }

        let utime: u64 = parts[13].parse().unwrap_or(0);
        let stime: u64 = parts[14].parse().unwrap_or(0);
        let total_time = utime + stime;

        // Calcular porcentaje de CPU (simplificado)
        let usage_percent = if total_time > 0 {
            (total_time as f32 / 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(CpuUsage {
            user_time: utime,
            system_time: stime,
            total_time,
            usage_percent,
        })
    }

    /// Obtiene uso de memoria de un proceso
    fn get_process_memory_usage(&self, pid: u32) -> Result<MemoryUsage> {
        let statm_path = format!("/proc/{}/statm", pid);
        let statm_content = std::fs::read_to_string(&statm_path)?;

        let parts: Vec<&str> = statm_content.split_whitespace().collect();
        if parts.len() < 1 {
            return Err(anyhow::anyhow!("Formato de /proc/statm inválido"));
        }

        let total_pages: u64 = parts[0].parse().unwrap_or(0);
        let resident_pages: u64 = parts[1].parse().unwrap_or(0);

        // Convertir páginas a bytes (asumiendo 4KB por página)
        let page_size = 4096;
        let used = resident_pages * page_size;

        Ok(MemoryUsage {
            total: total_pages * page_size,
            used,
            free: 0, // No aplicable para proceso individual
            cached: 0,
            buffers: 0,
            usage_percent: 0.0, // Calcular relativo al sistema
        })
    }

    /// Obtiene uso de I/O de un proceso
    fn get_process_io_usage(&self, pid: u32) -> Result<IoUsage> {
        let io_path = format!("/proc/{}/io", pid);
        if let Ok(io_content) = std::fs::read_to_string(&io_path) {
            let mut read_bytes = 0u64;
            let mut write_bytes = 0u64;
            let mut read_operations = 0u64;
            let mut write_operations = 0u64;

            for line in io_content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() == 2 {
                    match parts[0] {
                        "rchar:" => read_bytes = parts[1].parse().unwrap_or(0),
                        "wchar:" => write_bytes = parts[1].parse().unwrap_or(0),
                        "syscr:" => read_operations = parts[1].parse().unwrap_or(0),
                        "syscw:" => write_operations = parts[1].parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }

            Ok(IoUsage {
                read_bytes,
                write_bytes,
                read_operations,
                write_operations,
                io_time: 0, // No disponible en /proc/<pid>/io
            })
        } else {
            // Si no podemos leer /proc/<pid>/io, devolver valores por defecto
            Ok(IoUsage {
                read_bytes: 0,
                write_bytes: 0,
                read_operations: 0,
                write_operations: 0,
                io_time: 0,
            })
        }
    }

    /// Obtiene uso de CPU del sistema
    fn get_system_cpu_usage(&self) -> Result<CpuUsage> {
        let stat_content = std::fs::read_to_string("/proc/stat")?;
        let first_line = stat_content.lines().next()
            .ok_or(anyhow::anyhow!("No se pudo leer /proc/stat"))?;

        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 8 {
            return Err(anyhow::anyhow!("Formato de /proc/stat inválido"));
        }

        let user: u64 = parts[1].parse().unwrap_or(0);
        let nice: u64 = parts[2].parse().unwrap_or(0);
        let system: u64 = parts[3].parse().unwrap_or(0);
        let idle: u64 = parts[4].parse().unwrap_or(0);
        let iowait: u64 = parts[5].parse().unwrap_or(0);

        let total_time = user + nice + system + idle + iowait;
        let usage_percent = if total_time > 0 {
            ((total_time - idle) as f32 / total_time as f32 * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(CpuUsage {
            user_time: user,
            system_time: system,
            total_time,
            usage_percent,
        })
    }

    /// Obtiene uso de memoria del sistema
    fn get_system_memory_usage(&self) -> Result<MemoryUsage> {
        let meminfo_content = std::fs::read_to_string("/proc/meminfo")?;
        let mut total = 0u64;
        let mut free = 0u64;
        let mut cached = 0u64;
        let mut buffers = 0u64;

        for line in meminfo_content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let value: u64 = parts[1].parse().unwrap_or(0) * 1024; // Convertir KB a bytes
                match parts[0] {
                    "MemTotal:" => total = value,
                    "MemFree:" => free = value,
                    "Cached:" => cached = value,
                    "Buffers:" => buffers = value,
                    _ => {}
                }
            }
        }

        let used = total - free;
        let usage_percent = if total > 0 {
            (used as f32 / total as f32 * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(MemoryUsage {
            total,
            used,
            free,
            cached,
            buffers,
            usage_percent,
        })
    }

    /// Obtiene promedio de carga del sistema
    fn get_system_load_average(&self) -> Result<(f32, f32, f32)> {
        let loadavg_content = std::fs::read_to_string("/proc/loadavg")?;
        let parts: Vec<&str> = loadavg_content.split_whitespace().collect();

        if parts.len() >= 3 {
            let load1: f32 = parts[0].parse().unwrap_or(0.0);
            let load5: f32 = parts[1].parse().unwrap_or(0.0);
            let load15: f32 = parts[2].parse().unwrap_or(0.0);

            Ok((load1, load5, load15))
        } else {
            Ok((0.0, 0.0, 0.0))
        }
    }

    /// Obtiene tiempo de actividad del sistema
    fn get_system_uptime(&self) -> Duration {
        if let Ok(uptime_content) = std::fs::read_to_string("/proc/uptime") {
            let parts: Vec<&str> = uptime_content.split_whitespace().collect();
            if let Some(uptime_str) = parts.get(0) {
                if let Ok(uptime_secs) = uptime_str.parse::<f64>() {
                    return Duration::from_secs_f64(uptime_secs);
                }
            }
        }
        Duration::from_secs(0)
    }

    /// Aplica límites de recursos a un servicio
    fn apply_resource_limits(&self, service_name: &str, limits: &ResourceLimits) -> Result<()> {
        // Nota: En una implementación real, aquí se aplicarían límites usando:
        // - cgroups para CPU, memoria e I/O
        // - nice/renice para prioridad de CPU
        // - prlimit para límites de procesos

        debug!("Aplicando Aplicando límites de recursos a {}: {:?}", service_name, limits);
        Ok(())
    }

    /// Agrega entrada al historial de uso
    fn add_to_history(&self, service_name: &str, cpu: &CpuUsage, memory: &MemoryUsage) {
        let mut history = self.usage_history.lock().unwrap();

        let service_history = history.entry(service_name.to_string())
            .or_insert_with(Vec::new);

        service_history.push((Instant::now(), cpu.clone(), memory.clone()));

        // Mantener tamaño máximo del historial
        if service_history.len() > self.max_history_size {
            let excess = service_history.len() - self.max_history_size;
            service_history.drain(0..excess);
        }
    }
}

/// Estadísticas del sistema
#[derive(Debug, Clone)]
pub struct SystemStats {
    pub cpu_usage: CpuUsage,
    pub memory_usage: MemoryUsage,
    pub load_average: (f32, f32, f32),
    pub uptime: Duration,
}

impl SystemStats {
    pub fn get_summary(&self) -> String {
        format!(
            "Sistema - CPU: {:.1}%, Memoria: {:.1}%, Load: {:.2} {:.2} {:.2}, Uptime: {:.0}s",
            self.cpu_usage.usage_percent,
            self.memory_usage.usage_percent,
            self.load_average.0,
            self.load_average.1,
            self.load_average.2,
            self.uptime.as_secs()
        )
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_quota: 100,
            memory_limit: 0,
            io_weight: 100,
            max_processes: 1024,
            nice_level: 0,
        }
    }
}
