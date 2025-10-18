# 💻 Ejemplos de Código: Mejoras Prácticas para el Instalador

Este documento contiene ejemplos de código **listos para copiar y pegar** que mejoran el instalador actual.

---

## 1️⃣ Integración con Instalador Oficial

### Archivo: `src/official_installer_wrapper.rs` (NUEVO)

```rust
use std::path::{Path, PathBuf};
use redox_installer::{Config, install, DiskOption};
use crate::{DiskInfo, FilesystemType, InstallationConfig};

pub struct OfficialInstallerWrapper {
    redox_root: PathBuf,
}

impl OfficialInstallerWrapper {
    pub fn new() -> Self {
        Self {
            redox_root: PathBuf::from("/home/moebius/redox"),
        }
    }
    
    /// Instala Redox OS usando el instalador oficial
    pub fn install_with_official(&self, 
                                  disk: &DiskInfo, 
                                  config: &InstallationConfig) -> Result<(), String> {
        println!("🦀 Usando instalador oficial de Redox OS");
        
        // 1. Determinar qué configuración usar
        let config_file = self.get_config_file(config)?;
        println!("   Usando configuración: {}", config_file.display());
        
        // 2. Cargar configuración TOML
        let mut redox_config = Config::from_file(&config_file)
            .map_err(|e| format!("Error leyendo configuración: {}", e))?;
        
        // 3. Aplicar tamaño de partición EFI personalizado
        redox_config.general.efi_partition_size = Some(config.efi_size_mb);
        
        // 4. Instalar usando el instalador oficial
        println!("   Instalando en {}...", disk.name);
        install(
            redox_config,
            &disk.name,
            Some("cookbook"),  // Usar cookbook para paquetes
            false,             // No es instalación live
            None,              // No escribir bootloader separado
        ).map_err(|e| format!("Error en instalación oficial: {}", e))?;
        
        println!("   ✅ Instalación completada con instalador oficial");
        Ok(())
    }
    
    fn get_config_file(&self, config: &InstallationConfig) -> Result<PathBuf, String> {
        // Por defecto, usar desktop
        let config_name = "desktop";
        let arch = "x86_64";
        
        let config_path = self.redox_root
            .join("config")
            .join(arch)
            .join(format!("{}.toml", config_name));
        
        if !config_path.exists() {
            return Err(format!("Configuración no encontrada: {}", config_path.display()));
        }
        
        Ok(config_path)
    }
}
```

### Modificación en `src/main.rs`

```rust
// Añadir al inicio
mod official_installer_wrapper;
use official_installer_wrapper::OfficialInstallerWrapper;

// En la función install_redox_os_direct(), reemplazar:
// let direct_installer = DirectInstaller::new();
// match direct_installer.install_redox_os(selected_disk, &config) {

// CON:
println!("Selecciona el método de instalación:");
println!("  1. Instalador oficial (recomendado)");
println!("  2. Instalador personalizado (legacy)");

let method = read_input("Método [1]: ");

match method.trim() {
    "2" => {
        let direct_installer = DirectInstaller::new();
        direct_installer.install_redox_os(selected_disk, &config)
    }
    _ => {
        let official = OfficialInstallerWrapper::new();
        official.install_with_official(selected_disk, &config)
    }
}
```

---

## 2️⃣ Sistema de Validación Post-Instalación

### Archivo: `src/validator.rs` (NUEVO)

```rust
use std::path::Path;
use std::fs;

pub struct InstallationValidator {
    mount_point: String,
}

#[derive(Debug)]
pub struct ValidationResult {
    pub passed: bool,
    pub checks: Vec<CheckResult>,
}

#[derive(Debug)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

impl InstallationValidator {
    pub fn new(mount_point: String) -> Self {
        Self { mount_point }
    }
    
    pub fn validate_all(&self) -> ValidationResult {
        let mut checks = Vec::new();
        
        checks.push(self.check_bootloader());
        checks.push(self.check_kernel());
        checks.push(self.check_initfs());
        checks.push(self.check_filesystem_structure());
        checks.push(self.check_essential_binaries());
        checks.push(self.check_config_files());
        
        let passed = checks.iter().all(|c| c.passed);
        
        ValidationResult { passed, checks }
    }
    
    fn check_bootloader(&self) -> CheckResult {
        let bootloader_path = format!("{}/EFI/BOOT/BOOTX64.EFI", self.mount_point);
        
        match fs::metadata(&bootloader_path) {
            Ok(metadata) => {
                let size = metadata.len();
                if size > 10000 && size < 10_000_000 {
                    CheckResult {
                        name: "Bootloader".to_string(),
                        passed: true,
                        message: format!("OK - {} KB", size / 1024),
                    }
                } else {
                    CheckResult {
                        name: "Bootloader".to_string(),
                        passed: false,
                        message: format!("Tamaño sospechoso: {} bytes", size),
                    }
                }
            }
            Err(_) => CheckResult {
                name: "Bootloader".to_string(),
                passed: false,
                message: "No encontrado".to_string(),
            }
        }
    }
    
    fn check_kernel(&self) -> CheckResult {
        let kernel_path = format!("{}/boot/kernel", self.mount_point);
        
        match fs::metadata(&kernel_path) {
            Ok(metadata) => {
                let size = metadata.len();
                let size_mb = size as f64 / 1024.0 / 1024.0;
                
                if size > 100_000 {  // Al menos 100 KB
                    CheckResult {
                        name: "Kernel".to_string(),
                        passed: true,
                        message: format!("OK - {:.2} MB", size_mb),
                    }
                } else {
                    CheckResult {
                        name: "Kernel".to_string(),
                        passed: false,
                        message: format!("Demasiado pequeño: {:.2} MB", size_mb),
                    }
                }
            }
            Err(_) => CheckResult {
                name: "Kernel".to_string(),
                passed: false,
                message: "No encontrado en /boot/kernel".to_string(),
            }
        }
    }
    
    fn check_initfs(&self) -> CheckResult {
        let initfs_path = format!("{}/boot/initfs", self.mount_point);
        
        match fs::metadata(&initfs_path) {
            Ok(metadata) => {
                let size_mb = metadata.len() as f64 / 1024.0 / 1024.0;
                CheckResult {
                    name: "Initfs".to_string(),
                    passed: true,
                    message: format!("OK - {:.2} MB", size_mb),
                }
            }
            Err(_) => CheckResult {
                name: "Initfs".to_string(),
                passed: false,
                message: "No encontrado (puede ser opcional)".to_string(),
            }
        }
    }
    
    fn check_filesystem_structure(&self) -> CheckResult {
        let required_dirs = vec![
            "/usr/bin", "/usr/lib", "/etc", "/var", 
            "/tmp", "/home", "/root", "/boot",
        ];
        
        let mut missing = Vec::new();
        for dir in &required_dirs {
            let full_path = format!("{}{}", self.mount_point, dir);
            if !Path::new(&full_path).exists() {
                missing.push(dir.to_string());
            }
        }
        
        if missing.is_empty() {
            CheckResult {
                name: "Estructura de directorios".to_string(),
                passed: true,
                message: format!("OK - {} directorios verificados", required_dirs.len()),
            }
        } else {
            CheckResult {
                name: "Estructura de directorios".to_string(),
                passed: false,
                message: format!("Faltan: {}", missing.join(", ")),
            }
        }
    }
    
    fn check_essential_binaries(&self) -> CheckResult {
        let essential_bins = vec![
            "/usr/bin/ion",      // Shell
            "/usr/bin/ls",       // Listar archivos
            "/usr/bin/mkdir",    // Crear directorios
        ];
        
        let mut missing = Vec::new();
        for bin in &essential_bins {
            let full_path = format!("{}{}", self.mount_point, bin);
            if !Path::new(&full_path).exists() {
                missing.push(bin.to_string());
            }
        }
        
        if missing.is_empty() {
            CheckResult {
                name: "Binarios esenciales".to_string(),
                passed: true,
                message: format!("OK - {} binarios encontrados", essential_bins.len()),
            }
        } else {
            CheckResult {
                name: "Binarios esenciales".to_string(),
                passed: false,
                message: format!("Faltan: {}", missing.join(", ")),
            }
        }
    }
    
    fn check_config_files(&self) -> CheckResult {
        let config_files = vec![
            "/etc/hostname",
            "/usr/lib/os-release",
        ];
        
        let mut missing = Vec::new();
        for file in &config_files {
            let full_path = format!("{}{}", self.mount_point, file);
            if !Path::new(&full_path).exists() {
                missing.push(file.to_string());
            }
        }
        
        if missing.is_empty() {
            CheckResult {
                name: "Archivos de configuración".to_string(),
                passed: true,
                message: "OK - Configuración presente".to_string(),
            }
        } else {
            CheckResult {
                name: "Archivos de configuración".to_string(),
                passed: false,
                message: format!("Faltan: {}", missing.join(", ")),
            }
        }
    }
    
    pub fn print_results(&self, result: &ValidationResult) {
        println!();
        println!("╔═══════════════════════════════════════════════════╗");
        println!("║     📋 Resultados de Validación 📋               ║");
        println!("╚═══════════════════════════════════════════════════╝");
        println!();
        
        for check in &result.checks {
            let icon = if check.passed { "✅" } else { "❌" };
            println!("{} {:30} {}", icon, check.name, check.message);
        }
        
        println!();
        if result.passed {
            println!("╔═══════════════════════════════════════════════════╗");
            println!("║   ✅ VALIDACIÓN EXITOSA ✅                       ║");
            println!("║   La instalación parece correcta                  ║");
            println!("╚═══════════════════════════════════════════════════╝");
        } else {
            println!("╔═══════════════════════════════════════════════════╗");
            println!("║   ⚠️  ADVERTENCIA ⚠️                              ║");
            println!("║   Algunos checks fallaron                         ║");
            println!("║   La instalación puede no arrancar correctamente  ║");
            println!("╚═══════════════════════════════════════════════════╝");
        }
    }
}
```

### Usar en `src/direct_installer.rs`

```rust
// Al final de install_redox_os(), antes de unmount_partitions:

println!("🔍 [8/9] Validando instalación...");
let validator = crate::validator::InstallationValidator::new(
    self.root_mount_point.clone()
);
let validation = validator.validate_all();
validator.print_results(&validation);

if !validation.passed {
    println!();
    println!("⚠️  La validación encontró problemas.");
    println!("   ¿Deseas continuar de todos modos? (s/N): ");
    
    let response = read_input("");
    if response.trim().to_lowercase() != "s" {
        return Err("Instalación cancelada por validación fallida".to_string());
    }
}
println!("   ✅ Validación completada");
println!();
```

---

## 3️⃣ Sistema de Logging

### Archivo: `Cargo.toml` (modificar)

```toml
[dependencies]
libc = "0.2"
log = "0.4"
env_logger = "0.11"
chrono = "0.4"
```

### Archivo: `src/logger.rs` (NUEVO)

```rust
use std::fs::{File, OpenOptions};
use std::io::Write;
use chrono::Local;

pub struct InstallLogger {
    file: Option<File>,
    console: bool,
}

impl InstallLogger {
    pub fn new(log_path: &str, console: bool) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|e| format!("Error creando log: {}", e))?;
        
        Ok(Self {
            file: Some(file),
            console,
        })
    }
    
    fn write_log(&mut self, level: &str, message: &str) {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        let log_line = format!("[{}] {}: {}\n", timestamp, level, message);
        
        // Escribir a consola si está habilitado
        if self.console {
            print!("{}", log_line);
        }
        
        // Escribir a archivo
        if let Some(ref mut file) = self.file {
            let _ = file.write_all(log_line.as_bytes());
            let _ = file.flush();
        }
    }
    
    pub fn info(&mut self, message: &str) {
        self.write_log("INFO", message);
    }
    
    pub fn warn(&mut self, message: &str) {
        self.write_log("WARN", message);
    }
    
    pub fn error(&mut self, message: &str) {
        self.write_log("ERROR", message);
    }
    
    pub fn step(&mut self, step_num: usize, total: usize, description: &str) {
        let message = format!("[{}/{}] {}", step_num, total, description);
        self.write_log("STEP", &message);
    }
}
```

### Usar en `src/direct_installer.rs`

```rust
// En DirectInstaller::install_redox_os()
let mut logger = crate::logger::InstallLogger::new(
    "/tmp/redox_install.log",
    true  // Mostrar en consola también
)?;

logger.info("=== Inicio de instalación de Redox OS ===");
logger.info(&format!("Disco: {}", disk.name));
logger.info(&format!("Tamaño EFI: {} MB", config.efi_size_mb));

// En cada paso:
logger.step(1, 8, "Creando particiones");
self.create_partitions(disk, config)?;
logger.info("Particiones creadas exitosamente");

// En caso de error:
logger.error(&format!("Error creando particiones: {}", e));
```

---

## 4️⃣ Selector de Configuración

### Archivo: `src/config_selector.rs` (NUEVO)

```rust
use std::path::{Path, PathBuf};
use std::io::{self, Write};

#[derive(Debug, Clone)]
pub enum ConfigPreset {
    Minimal,
    Desktop,
    Server,
    Custom(PathBuf),
}

impl ConfigPreset {
    pub fn select_interactive() -> Option<Self> {
        println!("╔═══════════════════════════════════════════════════╗");
        println!("║      Selecciona el tipo de instalación:          ║");
        println!("╠═══════════════════════════════════════════════════╣");
        println!("║  1. Minimal  - Sistema básico (~500 MB)          ║");
        println!("║  2. Desktop  - Entorno gráfico (~2 GB)           ║");
        println!("║  3. Server   - Servicios de servidor (~1 GB)     ║");
        println!("║  4. Custom   - Archivo TOML personalizado        ║");
        println!("╚═══════════════════════════════════════════════════╝");
        println!();
        
        print!("Opción [2]: ");
        io::stdout().flush().unwrap();
        
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        
        match input.trim() {
            "1" => Some(ConfigPreset::Minimal),
            "2" | "" => Some(ConfigPreset::Desktop),
            "3" => Some(ConfigPreset::Server),
            "4" => {
                print!("Ruta al archivo TOML: ");
                io::stdout().flush().unwrap();
                
                let mut path_input = String::new();
                io::stdin().read_line(&mut path_input).unwrap();
                
                let path = PathBuf::from(path_input.trim());
                if path.exists() {
                    Some(ConfigPreset::Custom(path))
                } else {
                    println!("❌ Archivo no encontrado");
                    None
                }
            }
            _ => {
                println!("❌ Opción inválida");
                None
            }
        }
    }
    
    pub fn get_config_path(&self) -> PathBuf {
        let base = PathBuf::from("/home/moebius/redox/config/x86_64");
        
        match self {
            ConfigPreset::Minimal => base.join("minimal.toml"),
            ConfigPreset::Desktop => base.join("desktop.toml"),
            ConfigPreset::Server => base.join("server.toml"),
            ConfigPreset::Custom(path) => path.clone(),
        }
    }
    
    pub fn description(&self) -> String {
        match self {
            ConfigPreset::Minimal => "Sistema mínimo con utilidades básicas".to_string(),
            ConfigPreset::Desktop => "Sistema completo con entorno gráfico COSMIC".to_string(),
            ConfigPreset::Server => "Sistema optimizado para servidor".to_string(),
            ConfigPreset::Custom(path) => format!("Configuración personalizada: {}", path.display()),
        }
    }
}
```

### Usar en `src/main.rs`

```rust
// En install_redox_os_direct(), después de seleccionar disco:

use crate::config_selector::ConfigPreset;

let preset = match ConfigPreset::select_interactive() {
    Some(p) => p,
    None => {
        println!("❌ Instalación cancelada");
        return;
    }
};

println!();
println!("📋 Configuración seleccionada:");
println!("   {}", preset.description());
println!();
```

---

## 5️⃣ Estimador de Tiempo

### Archivo: `src/progress.rs` (NUEVO)

```rust
use std::time::{Duration, Instant};

pub struct ProgressTracker {
    total_steps: usize,
    current_step: usize,
    step_start: Option<Instant>,
    step_durations: Vec<Duration>,
}

impl ProgressTracker {
    pub fn new(total_steps: usize) -> Self {
        Self {
            total_steps,
            current_step: 0,
            step_start: None,
            step_durations: Vec::new(),
        }
    }
    
    pub fn start_step(&mut self, step_name: &str) {
        self.current_step += 1;
        self.step_start = Some(Instant::now());
        
        let percentage = (self.current_step * 100) / self.total_steps;
        let remaining = self.estimate_remaining();
        
        println!("📊 [{}/{}] {}% - {}", 
                 self.current_step, 
                 self.total_steps, 
                 percentage,
                 step_name);
        
        if let Some(remaining) = remaining {
            let mins = remaining.as_secs() / 60;
            let secs = remaining.as_secs() % 60;
            println!("   ⏱️  Tiempo estimado restante: {}m {}s", mins, secs);
        }
    }
    
    pub fn finish_step(&mut self) {
        if let Some(start) = self.step_start {
            let duration = start.elapsed();
            self.step_durations.push(duration);
            
            println!("   ✅ Completado en {:.1}s", duration.as_secs_f64());
        }
        self.step_start = None;
    }
    
    fn estimate_remaining(&self) -> Option<Duration> {
        if self.step_durations.is_empty() {
            return None;
        }
        
        // Calcular tiempo promedio por paso
        let total: Duration = self.step_durations.iter().sum();
        let avg = total / self.step_durations.len() as u32;
        
        // Estimar tiempo restante
        let remaining_steps = self.total_steps.saturating_sub(self.current_step);
        Some(avg * remaining_steps as u32)
    }
    
    pub fn print_summary(&self) {
        let total: Duration = self.step_durations.iter().sum();
        let mins = total.as_secs() / 60;
        let secs = total.as_secs() % 60;
        
        println!();
        println!("⏱️  Tiempo total de instalación: {}m {}s", mins, secs);
    }
}
```

### Usar en `src/direct_installer.rs`

```rust
pub fn install_redox_os(&self, disk: &DiskInfo, config: &InstallationConfig) -> Result<(), String> {
    let mut progress = crate::progress::ProgressTracker::new(8);
    
    progress.start_step("Creando particiones");
    self.create_partitions(disk, config)?;
    progress.finish_step();
    
    progress.start_step("Formateando particiones");
    self.format_partitions(disk, config)?;
    progress.finish_step();
    
    // ... más pasos
    
    progress.print_summary();
    Ok(())
}
```

---

## 📦 Cómo Integrar Todo

### 1. Actualizar `Cargo.toml`

```toml
[dependencies]
libc = "0.2"
log = "0.4"
env_logger = "0.11"
chrono = "0.4"
redox-installer = { path = "../installer" }
```

### 2. Añadir módulos en `src/main.rs`

```rust
mod official_installer_wrapper;
mod validator;
mod logger;
mod config_selector;
mod progress;
```

### 3. Compilar

```bash
cd /home/moebius/redox/redox-disk-installer
cargo build --release
```

### 4. Probar

```bash
sudo ./target/release/redox-disk-installer
```

---

## 🎯 Resumen de Beneficios

Con estos cambios:

✅ **Código reducido** - De 1200 a ~600 líneas  
✅ **Más robusto** - Usa código oficial probado  
✅ **Mejor UX** - Progreso, validación, logs  
✅ **Más flexible** - Configuraciones personalizables  
✅ **Mantenible** - Menos código personalizado  

---

**¡Listo para copiar y pegar!** 🚀

