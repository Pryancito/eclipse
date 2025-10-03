#![allow(dead_code)]
//! Información del sistema para Eclipse OS
//!
//! Muestra información detallada sobre el hardware y software del sistema.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::{vec, vec::Vec};

/// Información del sistema
pub struct SystemInfo {
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    pub architecture: String,
    pub cpu_info: CpuInfo,
    pub memory_info: MemoryInfo,
    pub disk_info: Vec<DiskInfo>,
    pub network_info: Vec<NetworkInfo>,
    pub uptime: u64,
}

#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub model: String,
    pub cores: u32,
    pub threads: u32,
    pub frequency: u32,
    pub cache_size: u32,
    pub usage: f32,
}

#[derive(Debug, Clone)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub cached: u64,
    pub buffers: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub swap_free: u64,
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub device: String,
    pub mount_point: String,
    pub filesystem: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub usage_percent: f32,
}

#[derive(Debug, Clone)]
pub struct NetworkInfo {
    pub interface: String,
    pub ip_address: String,
    pub mac_address: String,
    pub status: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl SystemInfo {
    pub fn new() -> Self {
        Self {
            os_name: "Eclipse OS".to_string(),
            os_version: "1.0.0".to_string(),
            kernel_version: "1.0.0".to_string(),
            architecture: "x86_64".to_string(),
            cpu_info: CpuInfo {
                model: "Intel Core i7-12700K".to_string(),
                cores: 8,
                threads: 16,
                frequency: 3200,
                cache_size: 25600,
                usage: 15.5,
            },
            memory_info: MemoryInfo {
                total: 8589934592,      // 8GB
                used: 4294967296,       // 4GB
                free: 4294967296,       // 4GB
                cached: 1073741824,     // 1GB
                buffers: 536870912,     // 512MB
                swap_total: 8589934592, // 8GB
                swap_used: 0,
                swap_free: 8589934592, // 8GB
            },
            disk_info: vec![
                DiskInfo {
                    device: "/dev/sda1".to_string(),
                    mount_point: "/".to_string(),
                    filesystem: "ext4".to_string(),
                    total: 107374182400, // 100GB
                    used: 53687091200,   // 50GB
                    free: 53687091200,   // 50GB
                    usage_percent: 50.0,
                },
                DiskInfo {
                    device: "/dev/sda2".to_string(),
                    mount_point: "/home".to_string(),
                    filesystem: "ext4".to_string(),
                    total: 214748364800, // 200GB
                    used: 107374182400,  // 100GB
                    free: 107374182400,  // 100GB
                    usage_percent: 50.0,
                },
            ],
            network_info: vec![
                NetworkInfo {
                    interface: "eth0".to_string(),
                    ip_address: "192.168.1.100".to_string(),
                    mac_address: "00:11:22:33:44:55".to_string(),
                    status: "up".to_string(),
                    rx_bytes: 1073741824, // 1GB
                    tx_bytes: 536870912,  // 512MB
                },
                NetworkInfo {
                    interface: "wlan0".to_string(),
                    ip_address: "192.168.1.101".to_string(),
                    mac_address: "00:11:22:33:44:56".to_string(),
                    status: "up".to_string(),
                    rx_bytes: 2147483648, // 2GB
                    tx_bytes: 1073741824, // 1GB
                },
            ],
            uptime: 86400, // 1 día
        }
    }

    pub fn run(&self) -> Result<(), &'static str> {
        self.show_welcome();
        self.show_general_info();
        self.show_cpu_info();
        self.show_memory_info();
        self.show_disk_info();
        self.show_network_info();
        self.show_processes();
        self.show_services();
        Ok(())
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE SYSTEM INFO                       ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Información detallada del sistema operativo                ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_general_info(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                        INFORMACIÓN GENERAL");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info(&format!(
            "Sistema Operativo: {} {}",
            self.os_name, self.os_version
        ));
        self.print_info(&format!("Kernel: {}", self.kernel_version));
        self.print_info(&format!("Arquitectura: {}", self.architecture));
        self.print_info(&format!("Tiempo de actividad: {} segundos", self.uptime));
        self.print_info("");
    }

    fn show_cpu_info(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                           PROCESADOR");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info(&format!("Modelo: {}", self.cpu_info.model));
        self.print_info(&format!("Núcleos: {}", self.cpu_info.cores));
        self.print_info(&format!("Hilos: {}", self.cpu_info.threads));
        self.print_info(&format!("Frecuencia: {} MHz", self.cpu_info.frequency));
        self.print_info(&format!("Cache: {} KB", self.cpu_info.cache_size));
        self.print_info(&format!("Uso: {:.1}%", self.cpu_info.usage));
        self.print_info("");
    }

    fn show_memory_info(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                            MEMORIA");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info(&format!(
            "Total: {} MB",
            self.memory_info.total / 1024 / 1024
        ));
        self.print_info(&format!(
            "Usada: {} MB",
            self.memory_info.used / 1024 / 1024
        ));
        self.print_info(&format!(
            "Libre: {} MB",
            self.memory_info.free / 1024 / 1024
        ));
        self.print_info(&format!(
            "Cache: {} MB",
            self.memory_info.cached / 1024 / 1024
        ));
        self.print_info(&format!(
            "Buffers: {} MB",
            self.memory_info.buffers / 1024 / 1024
        ));
        self.print_info("");
        self.print_info("Swap:");
        self.print_info(&format!(
            "  Total: {} MB",
            self.memory_info.swap_total / 1024 / 1024
        ));
        self.print_info(&format!(
            "  Usada: {} MB",
            self.memory_info.swap_used / 1024 / 1024
        ));
        self.print_info(&format!(
            "  Libre: {} MB",
            self.memory_info.swap_free / 1024 / 1024
        ));
        self.print_info("");
    }

    fn show_disk_info(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                            DISCOS");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info(
            "Dispositivo    Punto de montaje  Sistema  Tamaño    Usado    Libre    Uso%",
        );
        self.print_info(
            "─────────────  ────────────────  ───────  ────────  ───────  ───────  ─────",
        );

        for disk in &self.disk_info {
            self.print_info(&format!(
                "{:<13} {:<17} {:<7} {:<8} {:<7} {:<7} {:.1}%",
                disk.device,
                disk.mount_point,
                disk.filesystem,
                format_size(disk.total),
                format_size(disk.used),
                format_size(disk.free),
                disk.usage_percent
            ));
        }
        self.print_info("");
    }

    fn show_network_info(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                           RED");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("Interfaz    Dirección IP      MAC              Estado  RX      TX");
        self.print_info("──────────  ────────────────  ───────────────  ──────  ───────  ───────");

        for net in &self.network_info {
            self.print_info(&format!(
                "{:<10} {:<17} {:<16} {:<7} {:<7} {:<7}",
                net.interface,
                net.ip_address,
                net.mac_address,
                net.status,
                format_size(net.rx_bytes),
                format_size(net.tx_bytes)
            ));
        }
        self.print_info("");
    }

    fn show_processes(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                          PROCESOS");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("PID    Nombre           Estado    CPU%   Memoria");
        self.print_info("─────  ───────────────  ────────  ─────  ───────");
        self.print_info("1      kernel           Running   0.1    1024");
        self.print_info("2      shell            Running   0.5    2048");
        self.print_info("3      file_manager     Stopped   0.0    0");
        self.print_info("4      system_info      Running   0.2    512");
        self.print_info("5      calculator       Stopped   0.0    0");
        self.print_info("");
    }

    fn show_services(&self) {
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("                         SERVICIOS");
        self.print_info("═══════════════════════════════════════════════════════════════");
        self.print_info("Servicio           Estado    Puerto  Descripción");
        self.print_info("─────────────────  ────────  ──────  ─────────────────────────");
        self.print_info("ssh                Running   22      Servidor SSH");
        self.print_info("http               Running   80      Servidor HTTP");
        self.print_info("https              Running   443     Servidor HTTPS");
        self.print_info("ftp                Stopped   -       Servidor FTP");
        self.print_info("smtp               Running   25      Servidor SMTP");
        self.print_info("");
    }

    fn print_info(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola
        // Por ahora solo simulamos
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    format!("{:.1}{}", size, UNITS[unit_index])
}

/// Función principal para ejecutar la información del sistema
pub fn run() -> Result<(), &'static str> {
    let system_info = SystemInfo::new();
    system_info.run()
}
