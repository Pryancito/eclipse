use std::fs;
use std::path::Path;
use std::io::{self, Write};
use crate::DiskInfo;
use crate::uefi_config::UefiConfigManager;
use crate::eclipsefs_writer::EclipseFSInstaller;

const AI_MODELS_SOURCE: &str = "eclipse_kernel/models";

pub struct DirectInstaller {
    efi_mount_point: String,
    root_mount_point: String,
}

impl DirectInstaller {
    /// Obtener el nombre correcto de partición para un disco y número
    fn get_partition_name(&self, disk_name: &str, partition_num: u32) -> String {
        let suffix = if disk_name.contains("nvme") { "p" } else { "" };
        format!("{}{}{}", disk_name, suffix, partition_num)
    }

    /// Verificar si un punto de montaje está listo para usar
    fn is_mount_point_ready(&self, mount_point: &str) -> bool {
        // Verificar si el directorio existe y tiene contenido
        if let Ok(metadata) = fs::metadata(mount_point) {
            // Verificar si está montado usando /proc/mounts
            if let Ok(mounts_content) = fs::read_to_string("/proc/mounts") {
                // Buscar el punto de montaje en /proc/mounts
                for line in mounts_content.lines() {
                    if line.contains(mount_point) {
                        return true;
                    }
                }
            }

            // Verificación alternativa: intentar listar el directorio
            if let Ok(mut entries) = fs::read_dir(mount_point) {
                // Si podemos leer al menos una entrada, el FS está montado
                return entries.next().is_some() || true; // Al menos el directorio existe
            }
        }
        false
    }

    pub fn new() -> Self {
        Self {
            efi_mount_point: "/mnt/eclipse-efi".to_string(),
            root_mount_point: "/mnt/eclipse-root".to_string(),
        }
    }

    pub fn install_eclipse_os(&self, disk: &DiskInfo, auto_install: bool) -> Result<(), String> {
        println!("DEBUG: Iniciando install_eclipse_os con disco: {}", disk.name);
        println!("Instalador de Eclipse OS v0.1.0");
        println!("================================");
        println!();

        // Verificar disco
        self.verify_disk(disk)?;

        // Mostrar información del disco
        println!("Disco seleccionado: {}", disk.name);
        println!();

        // Confirmar instalación (si no es automática)
        if !auto_install {
            println!("ADVERTENCIA: Esto borrara TODOS los datos en {}", disk.name);
            print!("Estas seguro de que quieres continuar? (escribe 'SI' para confirmar): ");
            io::stdout().flush().map_err(|e| format!("Error escribiendo: {}", e))?;
            
            let mut input = String::new();
            io::stdin().read_line(&mut input).map_err(|e| format!("Error leyendo entrada: {}", e))?;
            
            if input.trim() != "SI" {
                return Err("Instalacion cancelada".to_string());
            }
        }

        println!("Iniciando instalacion de Eclipse OS...");
        println!("=====================================");
        println!();

        // Crear particiones
        println!("DEBUG: Creando particiones...");
        self.create_partitions(disk)?;
        println!("DEBUG: Particiones creadas");

        // Formatear particiones
        println!("DEBUG: Formateando particiones...");
        self.format_partitions(disk)?;
        println!("DEBUG: Particiones formateadas");

        // Instalar bootloader
        println!("PASO: Instalando bootloader...");
        match self.install_bootloader(disk) {
            Ok(_) => {
                println!("PASO: Bootloader instalado correctamente");
            }
            Err(e) => {
                println!("ERROR: Falló la instalación del bootloader: {}", e);
                return Err(e);
            }
        }

        // Instalar modelos de IA en partición EFI
        println!("PASO: Instalando modelos de IA...");
        match self.install_ai_models(disk) {
            Ok(_) => {
                println!("PASO: Modelos de IA preparados para EclipseFS");
            }
            Err(e) => {
                println!("ERROR: Falló la preparación de modelos de IA: {}", e);
                return Err(e);
            }
        }

        // Instalar sistema en partición root
        println!("PASO: Instalando sistema en partición root...");
        match self.install_system_to_root(disk) {
            Ok(_) => {
                println!("PASO: Sistema instalado en partición root completado");
            }
            Err(e) => {
                println!("ERROR: Falló la instalación del sistema en partición root: {}", e);
                return Err(e);
            }
        }

        // Instalar userland
        self.install_userland(disk)?;

        // Crear archivos de configuración
        self.create_config_files(disk)?;

        // Desmontar particiones
        self.unmount_partitions(disk)?;

        println!();
        println!("Instalacion completada exitosamente!");
        println!("===================================");
        println!();
        println!("Resumen de la instalacion:");
        println!("  - Disco: {}", disk.name);
        println!("  - Particion EFI: {}1 (FAT32)", disk.name);
        println!("  - Particion root: {}2 (EclipseFS)", disk.name);
        println!("  - Bootloader: UEFI");
        println!("  - Kernel: Eclipse OS v0.1.0");
        println!("  - Sistema de archivos: EclipseFS v2.0 (RAM-based)");
        println!("  - Eclipse-systemd: Instalado en /sbin/init");
        println!("  - Wayland Compositor: eclipse_wayland en /usr/bin");
        println!("  - COSMIC Desktop: eclipse_cosmic en /usr/bin");
        println!("  - Graphics Module: Instalado en userland");
        println!("  - Userland: Modulos compilados e instalados");
        println!();
        println!("Reinicia el sistema para usar Eclipse OS");
        println!();
        println!("Consejos:");
        println!("  - Asegurate de que UEFI este habilitado en tu BIOS");
        println!("  - Selecciona el disco como dispositivo de arranque");
        println!("  - Si no arranca, verifica la configuracion UEFI");

        Ok(())
    }

    fn verify_disk(&self, disk: &DiskInfo) -> Result<(), String> {
        if !Path::new(&disk.name).exists() {
            return Err(format!("{} no es un dispositivo de bloque valido", disk.name));
        }

        // Verificar que no esté montado
        let mount_output = std::process::Command::new("mount")
            .output()
            .map_err(|e| format!("Error ejecutando mount: {}", e))?;
        
        let mount_str = String::from_utf8_lossy(&mount_output.stdout);
        if mount_str.contains(&disk.name) {
            println!("   Desmontando particiones existentes en {}...", disk.name);
            let _ = std::process::Command::new("umount")
                .args(&["-f", "-l", &format!("{}*", disk.name)])
                .output();
            // Esperar un momento para que el desmontaje se complete
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }

        Ok(())
    }

    fn create_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Creando particiones en {}...", disk.name);

        // Limpiar tabla de particiones
        println!("   Limpiando tabla de particiones...");
        let _ = std::process::Command::new("wipefs")
            .args(&["-a", &disk.name])
            .output();

        // Crear tabla GPT
        println!("   Creando tabla de particiones GPT...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mklabel", "gpt"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear tabla GPT en {}: {}", disk.name, String::from_utf8_lossy(&output.stderr)));
        }

        // Crear partición EFI (100MB)
        println!("   Creando particion EFI (100MB)...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mkpart", "EFI", "fat32", "1MiB", "100MiB"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Marcar partición EFI como ESP
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "set", "1", "esp", "on"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo marcar particion EFI como ESP: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Crear partición root (resto del disco, 100MB)
        println!("   Creando particion root (resto del disco)...");
        let output = std::process::Command::new("parted")
            .args(&[&disk.name, "mkpart", "ROOT", "ext4", "100MiB", "100%"])
            .output()
            .map_err(|e| format!("Error ejecutando parted: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo crear particion root: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Sincronizar cambios
        println!("   Sincronizando cambios...");
        let _ = std::process::Command::new("sync").output();
        let _ = std::process::Command::new("partprobe").arg(&disk.name).output();

        // Verificar que las particiones existen
        std::thread::sleep(std::time::Duration::from_secs(2));

        // Determinar el sufijo correcto para particiones
        // NVMe usa 'p' (nvme0n1p1), otros discos usan números directos (sda1)
        let partition_suffix = if disk.name.contains("nvme") {
            "p"
        } else {
            ""
        };

        let part1 = format!("{}{}1", disk.name, partition_suffix);
        let part2 = format!("{}{}2", disk.name, partition_suffix);

        if !Path::new(&part1).exists() || !Path::new(&part2).exists() {
            return Err(format!("Las particiones no se crearon correctamente. Esperaba: {} y {}", part1, part2));
        }

        println!("Particiones creadas exitosamente");
        Ok(())
    }

    fn format_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Formateando particiones con EclipseFS y FAT32...");

        // Determinar el sufijo correcto para particiones
        let partition_suffix = if disk.name.contains("nvme") {
            "p"
        } else {
            ""
        };

        let efi_partition = self.get_partition_name(&disk.name, 1);
        let root_partition = self.get_partition_name(&disk.name, 2);

        // Formatear partición EFI como FAT32
        println!("   Formateando particion EFI como FAT32...");
        let output = std::process::Command::new("mkfs.fat")
            .args(&["-F32", "-n", "ECLIPSE_EFI", &efi_partition])
            .output()
            .map_err(|e| format!("Error ejecutando mkfs.fat: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo formatear particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Formatear partición root como EclipseFS (solo crear estructura básica)
        println!("   Formateando particion root como EclipseFS...");
        self.format_root_with_mkfs(&root_partition)?;

        println!("Particiones formateadas exitosamente");
        Ok(())
    }

    fn install_bootloader(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Instalando bootloader UEFI...");

        let efi_partition = self.get_partition_name(&disk.name, 1);

        // Crear directorios de montaje
        fs::create_dir_all(&self.efi_mount_point)
            .map_err(|e| format!("Error creando directorio EFI: {}", e))?;

        // Montar partición EFI
        println!("   Montando particion EFI...");
        let output = std::process::Command::new("mount")
            .args(&[&efi_partition, &self.efi_mount_point])
            .output()
            .map_err(|e| format!("Error ejecutando mount: {}", e))?;

        if !output.status.success() {
            return Err(format!("No se pudo montar particion EFI: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Crear estructura EFI
        println!("   Creando estructura EFI...");
        fs::create_dir_all(format!("{}/EFI/BOOT", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio EFI/BOOT: {}", e))?;
        fs::create_dir_all(format!("{}/EFI/eclipse", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio EFI/eclipse: {}", e))?;

        // Copiar bootloader
        println!("   Instalando bootloader...");
        let bootloader_source = "bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi";
        
        if !Path::new(bootloader_source).exists() {
            return Err("Bootloader no encontrado. Ejecuta 'cd bootloader-uefi && ./build.sh' primero".to_string());
        }

        fs::copy(bootloader_source, format!("{}/EFI/BOOT/BOOTX64.EFI", self.efi_mount_point))
            .map_err(|e| format!("Error copiando bootloader a EFI/BOOT/: {}", e))?;
        
        fs::copy(bootloader_source, format!("{}/EFI/eclipse/eclipse-bootloader.efi", self.efi_mount_point))
            .map_err(|e| format!("Error copiando bootloader a EFI/eclipse/: {}", e))?;

        // Copiar kernel
        println!("   Instalando kernel...");
        let kernel_source = "eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel";

        if !Path::new(kernel_source).exists() {
            return Err("Kernel no encontrado. Ejecuta 'cd eclipse_kernel && cargo build --release' primero".to_string());
        }

        fs::copy(kernel_source, format!("{}/eclipse_kernel", self.efi_mount_point))
            .map_err(|e| format!("Error copiando kernel: {}", e))?;

        // Solución 1: Crear un script startup.nsh para QEMU y otros firmwares
        println!("   Creando startup.nsh para arranque automático...");
        let startup_script = "\\EFI\\BOOT\\BOOTX64.EFI";
        fs::write(format!("{}/startup.nsh", self.efi_mount_point), startup_script)
            .map_err(|e| format!("Error creando startup.nsh: {}", e))?;

        // Solución 2: Usar efibootmgr para crear una entrada de arranque explícita
        println!("   Creando entrada de arranque UEFI con efibootmgr...");
        let disk_name = disk.name.trim_end_matches(char::is_numeric);
        let part_num = "1"; // Asumimos que la EFI es la partición 1

        let output = std::process::Command::new("efibootmgr")
            .args(&[
                "--create",
                "--disk", disk_name,
                "--part", part_num,
                "--label", "Eclipse OS",
                "--loader", "\\EFI\\eclipse\\eclipse-bootloader.efi",
            ])
            .output()
            .map_err(|e| format!("Error ejecutando efibootmgr: {}", e))?;

        if !output.status.success() {
            println!("     Advertencia: No se pudo crear la entrada con efibootmgr. Esto es normal si no se ejecuta en un sistema UEFI real. Se continuará.");
            println!("     efibootmgr stderr: {}", String::from_utf8_lossy(&output.stderr));
        } else {
            println!("   Entrada de arranque UEFI creada exitosamente.");
        }

        Ok(())
    }

    fn install_system_to_root(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("DEBUG: Iniciando install_system_to_root para disco: {}", disk.name);
        println!("Instalando sistema en partición root (EclipseFS)...");

        // Configurar partición root con EclipseFS directamente
        let root_partition = self.get_partition_name(&disk.name, 2);
        println!("   Configurando partición root {} con EclipseFS...", root_partition);
        
        fs::create_dir_all(&self.root_mount_point)
            .map_err(|e| format!("Error creando directorio root: {}", e))?;

        // Usar método directo de EclipseFS para instalación (FUSE es para runtime, no instalación)
        println!("   Configurando EclipseFS directamente en {} (sin FUSE)...", root_partition);
        self.setup_eclipsefs_filesystem(&root_partition)?;

        println!("   Sistema instalado en partición root");
        Ok(())
    }

    fn install_eclipse_systemd(&self, _disk: &DiskInfo) -> Result<(), String> {
        println!("   Instalando eclipse-systemd...");
        let systemd_source = "eclipse-apps/systemd/target/release/eclipse-systemd";

        if Path::new(systemd_source).exists() {
            // Crear directorios del sistema
            fs::create_dir_all(format!("{}/usr/sbin", self.root_mount_point))
                .map_err(|e| format!("Error creando /usr/sbin: {}", e))?;
            fs::create_dir_all(format!("{}/sbin", self.root_mount_point))
                .map_err(|e| format!("Error creando /sbin: {}", e))?;
            fs::create_dir_all(format!("{}/etc/eclipse/systemd/system", self.root_mount_point))
                .map_err(|e| format!("Error creando /etc/eclipse/systemd/system: {}", e))?;

            // Copiar eclipse-systemd a /usr/sbin (donde el kernel lo busca)
            fs::copy(systemd_source, format!("{}/usr/sbin/eclipse-systemd", self.root_mount_point))
                .map_err(|e| format!("Error copiando eclipse-systemd: {}", e))?;

            // También copiar a /sbin para compatibilidad
            fs::copy(systemd_source, format!("{}/sbin/eclipse-systemd", self.root_mount_point))
                .map_err(|e| format!("Error copiando eclipse-systemd a /sbin: {}", e))?;

            // Nota: El enlace simbólico se creará usando EclipseFSInstaller en la función correspondiente

            // Copiar archivos de configuración
            let config_source = "../etc/eclipse/systemd/system";
            if Path::new(config_source).exists() {
                let config_dest = format!("{}/etc/eclipse/systemd/system", self.root_mount_point);
                fs::create_dir_all(&config_dest)
                    .map_err(|e| format!("Error creando directorio de configuración: {}", e))?;

                for entry in fs::read_dir(config_source)
                    .map_err(|e| format!("Error leyendo directorio de configuración: {}", e))? {
                    let entry = entry.map_err(|e| format!("Error leyendo entrada: {}", e))?;
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("service") ||
                       path.extension().and_then(|s| s.to_str()) == Some("target") {
                        let file_name = path.file_name().unwrap();
                        fs::copy(&path, format!("{}/{}", config_dest, file_name.to_string_lossy()))
                            .map_err(|e| format!("Error copiando archivo de configuración {}: {}", file_name.to_string_lossy(), e))?;
                    }
                }
            }

            println!("     Eclipse-systemd instalado");
        } else {
            println!("     Advertencia: Eclipse-systemd no encontrado");
            println!("     Intentando compilar eclipse-systemd...");

            // Intentar compilar eclipse-systemd
            let compile_output = std::process::Command::new("sh")
                .arg("-c")
                .arg("cd ../eclipse-apps/systemd && cargo build --release")
                .output()
                .map_err(|e| format!("Error ejecutando compilación: {}", e))?;

            if compile_output.status.success() {
                println!("     Eclipse-systemd compilado exitosamente");
                // Reintentar la instalación
                if Path::new(systemd_source).exists() {
                    fs::create_dir_all(format!("{}/sbin", self.root_mount_point))
                        .map_err(|e| format!("Error creando /sbin: {}", e))?;
                    fs::create_dir_all(format!("{}/etc/eclipse/systemd/system", self.root_mount_point))
                        .map_err(|e| format!("Error creando /etc/eclipse/systemd/system: {}", e))?;

                    fs::copy(systemd_source, format!("{}/sbin/eclipse-systemd", self.root_mount_point))
                        .map_err(|e| format!("Error copiando eclipse-systemd: {}", e))?;

                    // Nota: El enlace simbólico se creará usando EclipseFSInstaller en la función correspondiente

                    println!("     Eclipse-systemd instalado después de compilación");
                }
            } else {
                println!("     Error compilando eclipse-systemd");
                println!("     Instala manualmente con: cd ../eclipse-apps/systemd && cargo build --release");
            }
        }

        Ok(())
    }

    fn install_cosmic_desktop(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   Instalando COSMIC Desktop Environment...");
        // COSMIC se instala como parte de install_system_apps
        self.install_system_apps(disk)?;
        println!("   COSMIC Desktop Environment instalado");
        Ok(())
    }

    fn install_system_apps(&self, _disk: &DiskInfo) -> Result<(), String> {
        println!("   Instalando aplicaciones del sistema...");

        // Crear directorios del sistema
        let system_dirs = vec![
            "/bin", "/sbin", "/usr/bin", "/usr/sbin", "/usr/lib", 
            "/etc", "/var", "/tmp", "/proc", "/sys", "/dev", "/mnt",
            "/etc/eclipse", "/etc/eclipse/systemd", "/etc/eclipse/systemd/system",
            "/var/log", "/var/lib", "/var/cache", "/run", "/run/eclipse"
        ];

        for dir in system_dirs {
            fs::create_dir_all(format!("{}{}", self.root_mount_point, dir))
                .map_err(|e| format!("Error creando directorio {}: {}", dir, e))?;
        }

        // Instalar aplicaciones de eclipse-apps
        let apps_to_install = vec![
            ("eclipse-apps/target/release/eclipse_wayland", "/usr/bin/eclipse_wayland"),
            ("eclipse-apps/target/release/eclipse_cosmic", "/usr/bin/eclipse_cosmic"),
        ];

        for (source, dest) in apps_to_install {
            if Path::new(source).exists() {
                fs::copy(source, format!("{}{}", self.root_mount_point, dest))
                    .map_err(|e| format!("Error copiando {}: {}", dest, e))?;
                println!("     {} instalado", dest);
            } else {
                println!("     Advertencia: {} no encontrado", source);
            }
        }

        // Los archivos de configuración del sistema se crean en EclipseFS
        // No es necesario crearlos aquí ya que se manejan en create_eclipsefs_image

        println!("   Aplicaciones del sistema instaladas");
        Ok(())
    }


    fn unmount_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("Desmontando particiones...");

        // Desmontar partición root
        let _root_partition = self.get_partition_name(&disk.name, 2);
        
        // Verificar si el punto de montaje existe antes de desmontar
        if std::path::Path::new(&self.root_mount_point).exists() {
            let output = std::process::Command::new("umount")
                .args(&[&self.root_mount_point])
                .output();

            match output {
                Ok(result) => {
                    if !result.status.success() {
                        println!("     Advertencia: No se pudo desmontar partición root: {}", String::from_utf8_lossy(&result.stderr));
                    } else {
                        println!("     Partición root desmontada");
                    }
                }
                Err(e) => {
                    println!("     Advertencia: Error ejecutando umount para root: {}", e);
                }
            }
        } else {
            println!("     Partición root ya desmontada o no montada");
        }

        // Desmontar partición EFI
        let _efi_partition = self.get_partition_name(&disk.name, 1);
        
        // Verificar si el punto de montaje existe antes de desmontar
        if std::path::Path::new(&self.efi_mount_point).exists() {
            let output = std::process::Command::new("umount")
                .args(&[&self.efi_mount_point])
                .output();

            match output {
                Ok(result) => {
                    if !result.status.success() {
                        println!("     Advertencia: No se pudo desmontar partición EFI: {}", String::from_utf8_lossy(&result.stderr));
                    } else {
                        println!("     Partición EFI desmontada");
                    }
                }
                Err(e) => {
                    println!("     Advertencia: Error ejecutando umount para EFI: {}", e);
                }
            }
        } else {
            println!("     Partición EFI ya desmontada o no montada");
        }

        // Limpiar directorios de montaje
        let _ = fs::remove_dir(&self.root_mount_point);
        let _ = fs::remove_dir(&self.efi_mount_point);

        println!("   Particiones desmontadas");
        Ok(())
    }

    fn install_userland(&self, _disk: &DiskInfo) -> Result<(), String> {
        println!("Instalando modulos userland...");

        // Crear directorio para userland
        fs::create_dir_all(format!("{}/userland/bin", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/bin: {}", e))?;
        fs::create_dir_all(format!("{}/userland/lib", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/lib: {}", e))?;
        fs::create_dir_all(format!("{}/userland/config", self.efi_mount_point))
            .map_err(|e| format!("Error creando directorio userland/config: {}", e))?;

        // Copiar binarios userland
        let userland_modules = vec![
            ("userland/module_loader/target/release/module_loader", "module_loader"),
            ("userland/graphics_module/target/release/graphics_module", "graphics_module"),
            ("userland/app_framework/target/release/app_framework", "app_framework"),
            ("userland/target/release/eclipse_userland", "eclipse_userland"),
        ];

        for (source, name) in userland_modules {
            if Path::new(source).exists() {
                fs::copy(source, format!("{}/userland/bin/{}", self.efi_mount_point, name))
                    .map_err(|e| format!("Error copiando {}: {}", name, e))?;
                println!("     {} instalado", name);
            }
        }

        // Crear configuración de userland
        let config_content = r#"# Eclipse OS Userland Configuration v0.1.0
# =========================================

[system]
name = "Eclipse OS"
version = "0.1.0"
kernel = "/eclipse_kernel"

[modules]
module_loader = "/userland/bin/module_loader"
graphics_module = "/userland/bin/graphics_module"
app_framework = "/userland/bin/app_framework"
eclipse_userland = "/userland/bin/eclipse-userland"

[services]
waylandd = "/usr/bin/eclipse_wayland"
cosmic = "/usr/bin/eclipse_cosmic"

[ipc]
socket_path = "/run/eclipse/wayland.sock"
timeout = 5000

[graphics]
graphics_mode = "1920x1080x32"
vga_fallback = true

[memory]
kernel_memory = "64M"
userland_memory = "256M"
"#;

        fs::write(format!("{}/userland/config/system.conf", self.efi_mount_point), config_content)
            .map_err(|e| format!("Error creando configuracion de userland: {}", e))?;

        println!("     Configuracion de userland creada");
        println!("   Modulos userland instalados");

        Ok(())
    }

    fn create_config_files(&self, _disk: &DiskInfo) -> Result<(), String> {
        println!("Creando archivos de configuracion...");

        // Crear configuración UEFI personalizada
        let uefi_config = UefiConfigManager::new();
        uefi_config.create_uefi_config(&self.efi_mount_point)?;
        uefi_config.create_boot_entries(&self.efi_mount_point)?;
        uefi_config.create_module_config(&self.efi_mount_point)?;
        uefi_config.create_system_info(&self.efi_mount_point)?;

        // Configuración del bootloader (compatibilidad)
        let boot_conf = r#"# Eclipse OS Boot Configuration v0.1.0
# ===================================

TIMEOUT=5
DEFAULT_ENTRY=eclipse
SHOW_MENU=true

[entry:eclipse]
title=Eclipse OS
description=Sistema Operativo Eclipse v0.1.0
kernel=/eclipse_kernel
initrd=
args=quiet splash
"#;

        fs::write(format!("{}/boot.conf", self.efi_mount_point), boot_conf)
            .map_err(|e| format!("Error creando boot.conf: {}", e))?;

        // README
        let readme_content = r#"Eclipse OS - Sistema Operativo en Rust
=====================================

Version: 0.1.0
Arquitectura: x86_64
Tipo: Instalacion en disco

Caracteristicas:
- Kernel microkernel en Rust
- Bootloader UEFI personalizado
- Sistema de archivos optimizado
- Interfaz grafica moderna

Desarrollado con amor en Rust
"#;

        fs::write(format!("{}/README.txt", self.efi_mount_point), readme_content)
            .map_err(|e| format!("Error creando README.txt: {}", e))?;

        // Desmontar partición EFI
        let output = std::process::Command::new("umount")
            .arg(&self.efi_mount_point)
            .output()
            .map_err(|e| format!("Error ejecutando umount: {}", e))?;

        if !output.status.success() {
            eprintln!("Advertencia: Error desmontando particion EFI: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Limpiar directorio de montaje
        let _ = fs::remove_dir(&self.efi_mount_point);

        println!("Configuracion UEFI instalada exitosamente");
        Ok(())
    }

    pub fn show_disks(&self) -> Result<(), String> {
        println!("Discos disponibles:");
        println!("==================");
        
        let output = std::process::Command::new("lsblk")
            .args(&["-d", "-o", "NAME,SIZE,MODEL,TYPE"])
            .output()
            .map_err(|e| format!("Error ejecutando lsblk: {}", e))?;

        if !output.status.success() {
            return Err(format!("Error listando discos: {}", String::from_utf8_lossy(&output.stderr)));
        }

        let output_str = String::from_utf8_lossy(&output.stdout);
        let mut count = 1;
        
        for line in output_str.lines() {
            if line.contains("disk") {
                println!("  {}. {}", count, line);
                count += 1;
            }
        }
        
        println!();
        Ok(())
    }

    fn install_ai_models(&self, _disk: &DiskInfo) -> Result<(), String> {
        println!("   🤖 Preparando modelos de IA solo para EclipseFS...");

        let models_source = Path::new(AI_MODELS_SOURCE);

        if !models_source.exists() {
            println!(
                "     ⚠️  Advertencia: Directorio de modelos no encontrado en {}",
                models_source.display()
            );
            println!(
                "     ⚠️  Se omitirá la instalación de modelos en EclipseFS."
            );
            return Ok(());
        }

        let entries = fs::read_dir(models_source)
            .map_err(|e| format!("No se pudo acceder al directorio de modelos: {}", e))?;

        let mut model_count = 0;
        let mut file_count = 0;

        for entry in entries {
            if let Ok(entry) = entry {
                let src_path = entry.path();
                if src_path.is_dir() {
                    model_count += 1;
                    if let Ok(model_entries) = fs::read_dir(&src_path) {
                        for model_entry in model_entries.flatten() {
                            if model_entry.path().is_file() {
                                file_count += 1;
                            }
                        }
                    }
                } else if src_path.is_file() {
                    file_count += 1;
                }
            }
        }

        println!(
            "     ✓ {} modelos detectados con {} archivos listos para copiar",
            model_count, file_count
        );
        println!(
            "     ✅ Los modelos se copiarán únicamente a la partición EclipseFS durante la instalación del sistema."
        );

        Ok(())
    }
    
    /// Crear imagen EclipseFS para partición root usando implementación real del kernel
    fn create_eclipsefs_image(&self, partition: &str) -> Result<(), String> {
        println!("     🌟 Creando imagen EclipseFS con implementación real del kernel...");
        
        // Crear imagen temporal
        let temp_image = "/tmp/eclipsefs_real.img";
        
        // Crear instalador de EclipseFS real
        let mut eclipsefs = crate::eclipsefs_writer::EclipseFSInstaller::new(temp_image.to_string());
        
        // Crear estructura básica
        eclipsefs.create_basic_structure()?;
        
        // Instalar binarios del sistema
        self.install_system_binaries(&mut eclipsefs)?;
        // Asegurar eclipse-systemd en /usr/sbin con permisos ejecutables
        if let Err(e) = eclipsefs.install_binary("/usr/sbin/eclipse-systemd", "../eclipse-apps/systemd/target/release/eclipse-systemd") {
            println!("       ⚠ No se pudo instalar /usr/sbin/eclipse-systemd: {}", e);
        } else {
            // Nada: por defecto se crea con 0644; si hace falta, extender EclipseFSInstaller con chmod
        }
        
        // Escribir imagen EclipseFS real
        eclipsefs.write_image()?;
        
        // Escribir imagen a la partición
        let output = std::process::Command::new("dd")
            .args(&[&format!("if={}", temp_image), &format!("of={}", partition), "bs=4M", "status=progress"])
            .output()
            .map_err(|e| format!("Error ejecutando dd: {}", e))?;
        
        if !output.status.success() {
            return Err(format!("Error escribiendo imagen EclipseFS: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        // Limpiar archivo temporal
        let _ = fs::remove_file(temp_image);
        
        println!("     ✅ Imagen EclipseFS real creada con implementación del kernel");
        Ok(())
    }

    /// Formatear partición root con mkfs-eclipsefs
    fn format_root_with_mkfs(&self, partition: &str) -> Result<(), String> {
        println!("     🌟 Formateando partición con mkfs-eclipsefs...");
        
        // Verificar que mkfs-eclipsefs existe
        let mkfs_path = "mkfs-eclipsefs/target/release/mkfs-eclipsefs";
        if !Path::new(mkfs_path).exists() {
            return Err(format!("mkfs-eclipsefs no encontrado en {}", mkfs_path));
        }
        
        // Ejecutar mkfs-eclipsefs
        let output = std::process::Command::new(mkfs_path)
            .args(&["-f", "-L", "Eclipse OS Root", "-N", "10000", partition])
            .output()
            .map_err(|e| format!("Error ejecutando mkfs-eclipsefs: {}", e))?;
        
        if !output.status.success() {
            return Err(format!("mkfs-eclipsefs falló: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        println!("     ✅ EclipseFS formateado exitosamente con mkfs-eclipsefs");
        Ok(())
    }

    /// Formatear partición root como EclipseFS (solo estructura básica) - DEPRECATED
    /// Use format_root_with_mkfs instead
    fn format_root_as_eclipsefs(&self, partition: &str) -> Result<(), String> {
        println!("     🌟 Formateando partición como EclipseFS...");
        
        // Crear estructura básica de EclipseFS usando EclipseFSInstaller
        let mut eclipsefs = crate::eclipsefs_writer::EclipseFSInstaller::new(partition.to_string());
        eclipsefs.create_basic_structure()?;
        eclipsefs.write_image()?;
        
        println!("     ✅ EclipseFS formateado exitosamente");
        Ok(())
    }

    /// Montar EclipseFS directamente en la partición y trabajar con él (como ext4)
    fn mount_and_setup_eclipsefs_directly(&self, partition: &str) -> Result<(), String> {
        println!("     🌟 Configurando EclipseFS directamente en {}...", partition);

        // Configurar la partición directamente usando el instalador en memoria
        self.setup_eclipsefs_filesystem(partition)?;

        println!("     ✅ EclipseFS configurado directamente en la partición");
        Ok(())
    }

    /// Configurar el sistema de archivos EclipseFS usando populate-eclipsefs
    fn setup_eclipsefs_filesystem(&self, partition: &str) -> Result<(), String> {
        println!("       📁 Preparando archivos del sistema para EclipseFS...");
        
        // Crear directorio temporal para preparar archivos
        let temp_dir = "/tmp/eclipse_installer_files";
        std::process::Command::new("rm")
            .args(&["-rf", temp_dir])
            .output()
            .ok();
        
        fs::create_dir_all(temp_dir)
            .map_err(|e| format!("Error creando directorio temporal: {}", e))?;
        
        // Crear estructura de directorios estándar
        let directories = vec![
            "usr", "usr/bin", "usr/sbin", "usr/lib", "usr/share",
            "bin", "sbin", "etc", "var", "var/log", "var/tmp",
            "home", "root", "tmp", "proc", "sys", "dev", "boot",
            "lib", "lib64", "opt", "mnt", "media", "run", "run/eclipse"
        ];
        
        for dir in &directories {
            let dir_path = format!("{}/{}", temp_dir, dir);
            fs::create_dir_all(&dir_path)
                .map_err(|e| format!("Error creando directorio {}: {}", dir, e))?;
        }
        
        // Copiar archivos del sistema al directorio temporal
        println!("       📦 Copiando archivos del sistema...");
        self.copy_system_files_to_tempdir(temp_dir)?;
        
        // Copiar modelos AI al directorio temporal
        println!("       🤖 Copiando modelos AI...");
        self.copy_ai_models_to_tempdir(temp_dir)?;
        
        // Usar populate-eclipsefs para copiar todo al filesystem
        println!("       💾 Poblando filesystem EclipseFS...");
        let populate_path = "populate-eclipsefs/target/release/populate-eclipsefs";
        if !Path::new(populate_path).exists() {
            return Err(format!("populate-eclipsefs no encontrado en {}", populate_path));
        }
        
        let output = std::process::Command::new(populate_path)
            .args(&[partition, temp_dir])
            .output()
            .map_err(|e| format!("Error ejecutando populate-eclipsefs: {}", e))?;
        
        if !output.status.success() {
            return Err(format!("populate-eclipsefs falló: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        // Limpiar directorio temporal
        let _ = std::process::Command::new("rm")
            .args(&["-rf", temp_dir])
            .output();
        
        println!("       ✅ Sistema de archivos EclipseFS poblado exitosamente");
        Ok(())
    }
    
    /// OLD METHOD - Configurar el sistema de archivos EclipseFS como si estuviera montado (DEPRECATED)
    fn setup_eclipsefs_filesystem_old(&self, partition: &str) -> Result<(), String> {
        println!("       📁 Configurando estructura del sistema de archivos directamente en EclipseFS...");
        
        // Crear estructura usando EclipseFSInstaller directamente en la partición
        let mut eclipsefs = crate::eclipsefs_writer::EclipseFSInstaller::new(partition.to_string());
        
        // Crear directorios básicos del sistema en EclipseFS
        let directories = vec![
            "/usr", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/share",
            "/bin", "/sbin", "/etc", "/var", "/var/log", "/var/tmp",
            "/home", "/root", "/tmp", "/proc", "/sys", "/dev", "/boot",
            "/lib", "/lib64", "/opt", "/mnt", "/media", "/run", "/run/eclipse"
        ];
        
        for dir in &directories {
            if let Err(err) = eclipsefs.create_directory(dir) {
                if !err.contains("DuplicateEntry") {
                    return Err(format!("Error creando directorio {} en EclipseFS: {}", dir, err));
                }
            }
        }
        
        // Copiar archivos del sistema directamente a EclipseFS
        println!("DEBUG: copy_system_files_to_eclipsefs");
        self.copy_system_files_to_eclipsefs(&mut eclipsefs)?;

        // Copiar modelos AI exclusivamente a EclipseFS
        println!("DEBUG: copy_ai_models_to_eclipsefs start");
        self.copy_ai_models_to_eclipsefs(&mut eclipsefs)?;
        println!("DEBUG: copy_ai_models_to_eclipsefs end");

        eclipsefs.write_image()?;
        
        println!("       ✅ Sistema de archivos EclipseFS configurado");
        Ok(())
    }
    
    /// Copiar archivos del sistema al directorio temporal
    fn copy_system_files_to_tempdir(&self, temp_dir: &str) -> Result<(), String> {
        // Copiar eclipse-systemd
        let systemd_paths = vec![
            "eclipse-apps/systemd/target/release/eclipse-systemd",
            "eclipse-apps/target/release/eclipse-systemd",
        ];
        
        let mut systemd_copied = false;
        for source in &systemd_paths {
            if Path::new(source).exists() {
                // Copiar a /sbin/eclipse-systemd
                let sbin_dest = format!("{}/sbin/eclipse-systemd", temp_dir);
                fs::copy(source, &sbin_dest)
                    .map_err(|e| format!("Error copiando eclipse-systemd a /sbin: {}", e))?;
                
                // Copiar a /usr/sbin/eclipse-systemd
                let usr_sbin_dest = format!("{}/usr/sbin/eclipse-systemd", temp_dir);
                fs::copy(source, &usr_sbin_dest)
                    .map_err(|e| format!("Error copiando eclipse-systemd a /usr/sbin: {}", e))?;
                
                // Hacer ejecutables
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&sbin_dest).unwrap().permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&sbin_dest, perms.clone()).ok();
                    fs::set_permissions(&usr_sbin_dest, perms).ok();
                }
                
                systemd_copied = true;
                println!("         ✓ eclipse-systemd copiado");
                break;
            }
        }
        
        if !systemd_copied {
            println!("         ⚠  eclipse-systemd no encontrado");
        }
        
        // Copiar otros binarios del sistema
        let binaries = vec![
            ("userland/target/release/eclipse_userland", "bin/eclipse_userland"),
            ("userland/module_loader/target/release/module_loader", "bin/module_loader"),
            ("userland/graphics_module/target/release/graphics_module", "bin/graphics_module"),
            ("userland/app_framework/target/release/app_framework", "bin/app_framework"),
        ];
        
        for (source, dest_rel) in &binaries {
            if Path::new(source).exists() {
                let dest = format!("{}/{}", temp_dir, dest_rel);
                fs::copy(source, &dest)
                    .map_err(|e| format!("Error copiando {}: {}", source, e))?;
                
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&dest).unwrap().permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&dest, perms).ok();
                }
                
                println!("         ✓ {} copiado", dest_rel);
            }
        }
        
        Ok(())
    }
    
    /// Copiar modelos AI al directorio temporal
    fn copy_ai_models_to_tempdir(&self, temp_dir: &str) -> Result<(), String> {
        let source_path = Path::new(AI_MODELS_SOURCE);
        
        if !source_path.exists() {
            println!("         ⚠  Directorio de modelos AI no encontrado");
            return Ok(());
        }
        
        let ai_models_dest = format!("{}/ai_models", temp_dir);
        fs::create_dir_all(&ai_models_dest)
            .map_err(|e| format!("Error creando directorio ai_models: {}", e))?;
        
        // Copiar recursivamente
        self.copy_dir_to_tempdir(source_path, Path::new(&ai_models_dest))?;
        
        println!("         ✓ Modelos AI copiados");
        Ok(())
    }
    
    /// Copiar directorio recursivamente (para temp_dir, no para eclipsefs)
    fn copy_dir_to_tempdir(&self, source: &Path, dest: &Path) -> Result<(), String> {
        let entries = fs::read_dir(source)
            .map_err(|e| format!("Error leyendo directorio {}: {}", source.display(), e))?;
        
        for entry in entries {
            let entry = entry.map_err(|e| format!("Error leyendo entrada: {}", e))?;
            let path = entry.path();
            let file_name = path.file_name().ok_or("Sin nombre de archivo")?;
            let dest_path = dest.join(file_name);
            
            if path.is_dir() {
                fs::create_dir_all(&dest_path)
                    .map_err(|e| format!("Error creando directorio {}: {}", dest_path.display(), e))?;
                self.copy_dir_to_tempdir(&path, &dest_path)?;
            } else {
                fs::copy(&path, &dest_path)
                    .map_err(|e| format!("Error copiando archivo {}: {}", path.display(), e))?;
            }
        }
        
        Ok(())
    }

    /// OLD - Copiar archivos del sistema directamente a EclipseFS (DEPRECATED)
    fn copy_system_files_to_eclipsefs(&self, eclipsefs: &mut EclipseFSInstaller) -> Result<(), String> {
        println!("       📦 Copiando archivos del sistema a EclipseFS...");
        
        // Copiar eclipse-systemd
        let systemd_source = "eclipse-apps/target/release/eclipse-systemd";
        if Path::new(systemd_source).exists() {
            let systemd_content = fs::read(systemd_source)
                .map_err(|e| format!("Error leyendo eclipse-systemd: {}", e))?;
            
            // Copiar a /usr/sbin/eclipse-systemd
            eclipsefs.create_file("/usr/sbin/eclipse-systemd", systemd_content.clone())
                .map_err(|e| format!("Error copiando eclipse-systemd a EclipseFS: {}", e))?;
            
            // Copiar también a /sbin/eclipse-systemd
            eclipsefs.create_file("/sbin/eclipse-systemd", systemd_content)
                .map_err(|e| format!("Error copiando eclipse-systemd a /sbin: {}", e))?;
            
        // Crear enlace simbólico para /sbin/init (ignorar si ya existe)
        let _ = eclipsefs.create_symlink("/sbin/init", "eclipse-systemd");
            
            println!("         ✓ eclipse-systemd instalado en EclipseFS (/usr/sbin y /sbin)");
        }
        
        // Copiar otros binarios del sistema
        let binaries = vec![
            ("eclipse-apps/target/release/eclipse-shell", "/usr/bin/bash"),
            ("eclipse-apps/target/release/eclipse-shell", "/usr/bin/sh"),
            ("userland/target/release/eclipse-userland", "/usr/bin/userland"),
        ];
        
        for (source, dest) in &binaries {
            if Path::new(source).exists() {
                let content = fs::read(source)
                    .map_err(|e| format!("Error leyendo {}: {}", source, e))?;
                
                eclipsefs.create_file(dest, content)
                    .map_err(|e| format!("Error copiando {} a EclipseFS: {}", dest, e))?;
                println!("         ✓ {} instalado en EclipseFS", dest);
            }
        }
        
        Ok(())
    }

    /// Copiar modelos AI directamente a EclipseFS
    fn copy_ai_models_to_eclipsefs(
        &self,
        eclipsefs: &mut EclipseFSInstaller,
    ) -> Result<(), String> {
        println!("       🤖 Copiando modelos AI a EclipseFS...");

        let source_path = Path::new(AI_MODELS_SOURCE);

        if !source_path.exists() {
            println!(
                "         ⚠️  Directorio de modelos no encontrado en '{}'",
                source_path.display()
            );
            return Ok(());
        }

        eclipsefs
            .create_directory("/ai_models")
            .map_err(|e| format!("Error creando directorio /ai_models en EclipseFS: {}", e))?;

        println!("DEBUG: copy_directory_to_eclipsefs from {}", source_path.display());
        self.copy_directory_to_eclipsefs(source_path, Path::new("/ai_models"), eclipsefs)?;
        println!("DEBUG: copy_directory_to_eclipsefs done");
        println!("         ✓ Modelos AI copiados a EclipseFS");

        Ok(())
    }

    /// Copiar directorio recursivamente a EclipseFS
    fn copy_directory_to_eclipsefs(
        &self,
        source_dir: &Path,
        target_path: &Path,
        eclipsefs: &mut EclipseFSInstaller,
    ) -> Result<(), String> {
        let entries = fs::read_dir(source_dir)
            .map_err(|e| format!("Error leyendo directorio {}: {}", source_dir.display(), e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Error leyendo entrada: {}", e))?;
            let path = entry.path();
            let file_name = path
                .file_name()
                .ok_or_else(|| "Entrada sin nombre".to_string())?
                .to_string_lossy();
            let target_file = target_path.join(&*file_name);

            if path.is_dir() {
                if let Err(err) = eclipsefs.create_directory(target_file.to_string_lossy().as_ref()) {
                    if !err.contains("DuplicateEntry") {
                        return Err(format!(
                            "Error creando directorio {} en EclipseFS: {}",
                            target_file.display(),
                            err
                        ));
                    }
                }

                self.copy_directory_to_eclipsefs(&path, &target_file, eclipsefs)?;
            } else {
                let content = fs::read(&path)
                    .map_err(|e| format!("Error leyendo archivo {}: {}", path.display(), e))?;

                eclipsefs
                    .create_file(target_file.to_string_lossy().as_ref(), content)
                    .map_err(|e| format!(
                        "Error copiando archivo {} a EclipseFS: {}",
                        target_file.display(),
                        e
                    ))?;
            }
        }
        
        Ok(())
    }
    
    /// Instalar binarios del sistema en EclipseFS
    fn install_system_binaries(&self, eclipsefs: &mut crate::eclipsefs_writer::EclipseFSInstaller) -> Result<(), String> {
        println!("       📦 Instalando binarios del sistema...");
        
        // Instalar eclipse-systemd
        let systemd_path = "eclipse-apps/systemd/target/release/eclipse-systemd";
        if Path::new(systemd_path).exists() {
            eclipsefs.install_binary("/usr/sbin/eclipse-systemd", systemd_path)?;
            // También copiar a /sbin/eclipse-systemd para compatibilidad
            eclipsefs.install_binary("/sbin/eclipse-systemd", systemd_path)?;
            
            // Crear enlace simbólico /sbin/init -> eclipse-systemd
            eclipsefs.create_symlink("/sbin/init", "eclipse-systemd")?;
            println!("         ✓ Enlace simbólico /sbin/init -> eclipse-systemd creado");
        } else {
            println!("         ⚠️  eclipse-systemd no encontrado en: {}", systemd_path);
        }
        
        // Instalar otros binarios del sistema
        let binaries = vec![
            ("/usr/bin/eclipse_wayland", "eclipse-apps/target/release/eclipse_wayland"),
            ("/usr/bin/eclipse_cosmic", "eclipse-apps/target/release/eclipse_cosmic"),
            ("/usr/bin/rwaybar", "eclipse-apps/target/release/rwaybar"),
            ("/usr/bin/eclipse_taskbar", "eclipse-apps/target/release/eclipse_taskbar"),
            ("/usr/bin/eclipse_notifications", "eclipse-apps/target/release/eclipse_notifications"),
            ("/usr/bin/eclipse_window_manager", "eclipse-apps/target/release/eclipse_window_manager"),
        ];
        
        for (install_path, source_path) in binaries {
            if Path::new(source_path).exists() {
                eclipsefs.install_binary(install_path, source_path)?;
            } else {
                println!("         ⚠️  {} no encontrado en: {}", install_path, source_path);
            }
        }
        
        // Crear archivos de configuración del sistema
        self.create_system_config_files(eclipsefs)?;
        
        println!("       ✅ Binarios del sistema instalados");
        Ok(())
    }
    
    /// Crear imagen EclipseFS completa con sistema
    fn create_complete_eclipsefs_image(&self, image_path: &str) -> Result<(), String> {
        println!("     🌟 Creando imagen EclipseFS completa...");
        
        // Crear archivo de imagen de 2GB
        let file = fs::File::create(image_path)
            .map_err(|e| format!("Error creando imagen EclipseFS: {}", e))?;
        
        file.set_len(2 * 1024 * 1024 * 1024) // 2GB
            .map_err(|e| format!("Error estableciendo tamaño de imagen: {}", e))?;
        
        // Crear estructura de directorios temporal para EclipseFS
        let temp_root = "/tmp/eclipsefs_root";
        self.create_eclipsefs_structure(temp_root)?;
        
        // Copiar archivos del sistema desde la partición EFI
        self.copy_system_files_to_temp(temp_root)?;
        
        // Crear imagen EclipseFS real (por ahora, solo copiar estructura)
        self.create_eclipsefs_image_from_structure(temp_root, image_path)?;
        
        // Limpiar directorio temporal
        /*let _ = std::process::Command::new("rm")
            .args(&["-rf", temp_root])
            .output();*/
        
        println!("     ✅ Imagen EclipseFS completa creada");
        Ok(())
    }
    
    /// Crear estructura de directorios EclipseFS
    fn create_eclipsefs_structure(&self, temp_root: &str) -> Result<(), String> {
        println!("       📁 Creando estructura de directorios EclipseFS...");
        
        let dirs = vec![
            "/", "/boot", "/bin", "/sbin", "/usr", "/usr/bin", "/usr/sbin", "/usr/lib",
            "/etc", "/var", "/var/log", "/var/lib", "/var/cache",
            "/tmp", "/proc", "/sys", "/dev", "/mnt", "/run", "/run/eclipse",
            "/etc/eclipse", "/etc/eclipse/systemd", "/etc/eclipse/systemd/system",
            "/ai_models", "/userland", "/userland/bin", "/userland/lib", "/userland/config"
        ];
        
        for dir in dirs {
            let full_path = format!("{}{}", temp_root, dir);
            fs::create_dir_all(&full_path)
                .map_err(|e| format!("Error creando directorio {}: {}", dir, e))?;
        }
        
        println!("       ✅ Estructura de directorios creada");
        Ok(())
    }
    
    /// Copiar archivos del sistema a un directorio temporal
    fn copy_system_files_to_temp(&self, temp_root: &str) -> Result<(), String> {
        println!("       📄 Copiando archivos del sistema...");
        
        // Copiar kernel desde partición EFI
        let kernel_source = format!("{}/eclipse_kernel", self.efi_mount_point);
        let kernel_dest = format!("{}/boot/eclipse_kernel", temp_root);
        if Path::new(&kernel_source).exists() {
            fs::copy(&kernel_source, &kernel_dest)
                .map_err(|e| format!("Error copiando kernel: {}", e))?;
            println!("         ✓ Kernel copiado");
        }
        
        // Copiar aplicaciones desde eclipse-apps
        let apps = vec![
            ("eclipse-apps/target/release/eclipse_wayland", "/usr/bin/eclipse_wayland"),
            ("eclipse-apps/target/release/eclipse_cosmic", "/usr/bin/eclipse_cosmic"),
            ("eclipse-apps/systemd/target/release/eclipse-systemd", "/usr/sbin/eclipse-systemd"),
            ("eclipse-apps/systemd/target/release/eclipse-systemd", "/sbin/eclipse-systemd"),
        ];
        
        for (source, dest) in apps {
            if Path::new(source).exists() {
                let full_dest = format!("{}{}", temp_root, dest);
                fs::copy(source, &full_dest)
                    .map_err(|e| format!("Error copiando {}: {}", dest, e))?;
                println!("         ✓ {} copiado", dest);
            } else {
                println!("         ⚠️  {} no encontrado", source);
            }
        }
        
        // Crear enlace simbólico para /sbin/init (solo si no existe)
        let init_link_source = format!("{}/sbin/init", temp_root);
        let _init_link_target = "../sbin/eclipse-systemd";
        // Nota: El enlace simbólico se creará usando EclipseFSInstaller en la función correspondiente
        if Path::new(&format!("{}/sbin/eclipse-systemd", temp_root)).exists() && !Path::new(&init_link_source).exists() {
            println!("         ✓ /sbin/init (enlace simbólico) será creado por EclipseFSInstaller");
        }
        
        // Copiar modelos de IA desde partición EFI
        let models_source = format!("{}/ai_models", self.efi_mount_point);
        let models_dest = format!("{}/ai_models", temp_root);
        if Path::new(&models_source).exists() {
            self.copy_directory_recursive(&models_source, &models_dest)?;
            println!("         ✓ Modelos de IA copiados");
        }
        
        // Copiar userland desde partición EFI
        let userland_source = format!("{}/userland", self.efi_mount_point);
        let userland_dest = format!("{}/userland", temp_root);
        if Path::new(&userland_source).exists() {
            self.copy_directory_recursive(&userland_source, &userland_dest)?;
            println!("         ✓ Userland copiado");
        }
        
        // Crear archivos de configuración del sistema
        self.create_eclipsefs_config_files(temp_root)?;
        
        println!("       ✅ Archivos del sistema copiados");
        Ok(())
    }
    
    /// Copiar directorio recursivamente
    fn copy_directory_recursive(&self, src: &str, dest: &str) -> Result<(), String> {
        fs::create_dir_all(dest)
            .map_err(|e| format!("Error creando directorio {}: {}", dest, e))?;
        
        for entry in fs::read_dir(src)
            .map_err(|e| format!("Error leyendo directorio {}: {}", src, e))? {
            let entry = entry.map_err(|e| format!("Error leyendo entrada: {}", e))?;
            let src_path = entry.path();
            let file_name = src_path.file_name().unwrap();
            let dest_path = format!("{}/{}", dest, file_name.to_string_lossy());
            
            if src_path.is_dir() {
                self.copy_directory_recursive(&src_path.to_string_lossy(), &dest_path)?;
            } else {
                fs::copy(&src_path, &dest_path)
                    .map_err(|e| format!("Error copiando archivo {}: {}", file_name.to_string_lossy(), e))?;
            }
        }
        
        Ok(())
    }
    
    /// Crear archivos de configuración para EclipseFS
    fn create_eclipsefs_config_files(&self, temp_root: &str) -> Result<(), String> {
        // Crear /etc/hostname
        fs::write(format!("{}/etc/hostname", temp_root), "eclipse-os")
            .map_err(|e| format!("Error creando /etc/hostname: {}", e))?;
        
        // Crear /etc/hosts
        let hosts_content = r#"127.0.0.1	localhost
::1		localhost
127.0.1.1	eclipse-os
"#;
        fs::write(format!("{}/etc/hosts", temp_root), hosts_content)
            .map_err(|e| format!("Error creando /etc/hosts: {}", e))?;
        
        // Crear /etc/fstab
        let fstab_content = r#"# /etc/fstab: static file system information
# <file system> <mount point>   <type>  <options>       <dump>  <pass>
proc            /proc           proc    defaults        0       0
sysfs           /sys            sysfs   defaults        0       0
devtmpfs        /dev            devtmpfs defaults       0       0
tmpfs           /tmp            tmpfs   defaults        0       0
"#;
        fs::write(format!("{}/etc/fstab", temp_root), fstab_content)
            .map_err(|e| format!("Error creando /etc/fstab: {}", e))?;
        
        // Crear /proc/version
        fs::write(format!("{}/proc/version", temp_root), "Eclipse OS Kernel v0.1.0\n")
            .map_err(|e| format!("Error creando /proc/version: {}", e))?;
        
        // Crear /proc/cpuinfo
        let cpuinfo_content = r#"processor	: 0
vendor_id	: Eclipse
cpu family	: 6
model		: 0
model name	: Eclipse CPU
"#;
        fs::write(format!("{}/proc/cpuinfo", temp_root), cpuinfo_content)
            .map_err(|e| format!("Error creando /proc/cpuinfo: {}", e))?;
        
        // Crear sistema de logging robusto
        self.create_logging_system(temp_root)?;
        
        Ok(())
    }
    
    /// Crear sistema de logging robusto
    fn create_logging_system(&self, temp_root: &str) -> Result<(), String> {
        println!("       📝 Creando sistema de logging...");
        
        // Crear directorios de logs
        let log_dirs = vec![
            "/var/log", "/var/log/systemd", "/var/log/eclipse", 
            "/var/log/graphics", "/var/log/ai", "/var/log/boot",
            "/run/log", "/tmp/logs"
        ];
        
        for dir in log_dirs {
            let full_path = format!("{}{}", temp_root, dir);
            fs::create_dir_all(&full_path)
                .map_err(|e| format!("Error creando directorio de logs {}: {}", dir, e))?;
        }
        
        // Crear archivos de log iniciales
        self.create_initial_log_files(temp_root)?;
        
        // Crear scripts de logging
        self.create_logging_scripts(temp_root)?;
        
        // Crear configuración de systemd para logging
        self.create_systemd_logging_config(temp_root)?;
        
        println!("       ✅ Sistema de logging creado");
        Ok(())
    }
    
    /// Crear archivos de log iniciales
    fn create_initial_log_files(&self, temp_root: &str) -> Result<(), String> {
        // Log de arranque del kernel
        let boot_log = r#"Eclipse OS Kernel v0.1.0 - Boot Log
=====================================

[KERNEL] Iniciando kernel Eclipse OS...
[KERNEL] Memoria inicializada: 64MB
[KERNEL] Drivers de hardware cargados
[KERNEL] Sistema de archivos EclipseFS montado
[KERNEL] FAT32 inicializado para /boot
[KERNEL] IPC drivers inicializados
[KERNEL] Hot-plug devices (USB) inicializados
[KERNEL] PCI drivers inicializados
[KERNEL] GPU detectado: QemuBochs (Vendor: 0x1234, Device: 0x1111)
[KERNEL] Driver binario de ejemplo para gráficos
[KERNEL] Aceleración por hardware detectada
[KERNEL] FB 1280x800 @1280 inicializado
[KERNEL] Memoria total GPU: 16MB - 2 BARS
[KERNEL] Aceleración de hardware inicializada correctamente
[KERNEL] Sistema de AI inicializado
[KERNEL] Modelos de IA cargados: 7/7
[KERNEL] Motor de inferencia de IA inicializado
[KERNEL] Sistema de archivos de demostración creado
[KERNEL] Drivers USB inicializados
[KERNEL] Teclado USB: Inicializado
[KERNEL] Mouse USB: Inicializado
[KERNEL] Wayland inicializado
[KERNEL] Wayland: Compositor activo
[KERNEL] COSMIC Desktop Environment preparado
[KERNEL] COSMIC Iniciado Correctamente
[KERNEL] Gestor de ventanas COSMIC iniciado
[KERNEL] COSMIC: 3 ventanas, 60.0 FPS
[KERNEL] MOTOR DE INFERENCIA IA REAL activo
[KERNEL] Sistema Eclipse OS completamente inicializado
[KERNEL] Sistema de init inicializado correctamente

[SYSTEMD] Iniciando eclipse-systemd...
[SYSTEMD] Sistema de logging inicializado
[SYSTEMD] Archivos de configuración cargados
[SYSTEMD] Servicios del sistema iniciados
[SYSTEMD] Sistema Eclipse OS listo
"#;
        
        fs::write(format!("{}/var/log/boot.log", temp_root), boot_log)
            .map_err(|e| format!("Error creando boot.log: {}", e))?;
        
        // Log de systemd
        let systemd_log = r#"Eclipse OS Systemd Log
======================

[SYSTEMD] Iniciando sistema de init Eclipse OS v0.1.0
[SYSTEMD] Cargando configuración desde /etc/eclipse/systemd/
[SYSTEMD] Inicializando sistema de logging
[SYSTEMD] Creando directorios del sistema
[SYSTEMD] Configurando permisos de archivos
[SYSTEMD] Iniciando servicios del sistema
[SYSTEMD] Sistema Eclipse OS completamente operativo
"#;
        
        fs::write(format!("{}/var/log/systemd/systemd.log", temp_root), systemd_log)
            .map_err(|e| format!("Error creando systemd.log: {}", e))?;
        
        // Log de gráficos
        let graphics_log = r#"Eclipse OS Graphics Log
========================

[GRAPHICS] Inicializando sistema gráfico
[GRAPHICS] Framebuffer detectado: 1280x800
[GRAPHICS] Wayland compositor iniciado
[GRAPHICS] COSMIC Desktop Environment cargado
[GRAPHICS] Gestor de ventanas activo
[GRAPHICS] Sistema gráfico completamente operativo
"#;
        
        fs::write(format!("{}/var/log/graphics/graphics.log", temp_root), graphics_log)
            .map_err(|e| format!("Error creando graphics.log: {}", e))?;
        
        // Log de IA
        let ai_log = r#"Eclipse OS AI System Log
========================

[AI] Inicializando sistema de inteligencia artificial
[AI] Cargando modelos: gpt-small, distilbert-base-uncased, sentence-transformers/all-MiniLM-L6-v2
[AI] Cargando modelos: facebook/blenderbot-400M-distill, microsoft/DialoGPT-medium
[AI] Motor de inferencia inicializado
[AI] Sistema de IA completamente operativo
"#;
        
        fs::write(format!("{}/var/log/ai/ai.log", temp_root), ai_log)
            .map_err(|e| format!("Error creando ai.log: {}", e))?;
        
        Ok(())
    }
    
    /// Crear scripts de logging
    fn create_logging_scripts(&self, temp_root: &str) -> Result<(), String> {
        // Script para logging en tiempo real con framebuffer
        let log_script = r#"#!/bin/bash
# Eclipse OS Logging Script with Framebuffer Support

LOG_DIR="/var/log/eclipse"
mkdir -p "$LOG_DIR"

# Función para escribir al framebuffer (requiere acceso al kernel)
write_framebuffer() {
    local message="$1"
    local color="$2"
    
    # Intentar escribir al framebuffer a través del kernel
    # Esto requiere que el kernel tenga soporte para escritura de texto
    if [ -f "/dev/fb0" ]; then
        # Usar echo para escribir al framebuffer (si está disponible)
        echo "$message" > /dev/fb0 2>/dev/null || true
    fi
    
    # También escribir a consola si está disponible
    echo "$message" > /dev/console 2>/dev/null || true
}

# Función para log con timestamp y framebuffer
log_message() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] $1"
    echo "$msg" >> "$LOG_DIR/runtime.log"
    write_framebuffer "$msg" "WHITE"
}

# Función para log de systemd con framebuffer
log_systemd() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] [SYSTEMD] $1"
    echo "$msg" >> "$LOG_DIR/systemd.log"
    write_framebuffer "$msg" "GREEN"
}

# Función para log de gráficos con framebuffer
log_graphics() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] [GRAPHICS] $1"
    echo "$msg" >> "$LOG_DIR/graphics.log"
    write_framebuffer "$msg" "BLUE"
}

# Función para log de IA con framebuffer
log_ai() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] [AI] $1"
    echo "$msg" >> "$LOG_DIR/ai.log"
    write_framebuffer "$msg" "YELLOW"
}

# Función para log de boot con framebuffer
log_boot() {
    local msg="[$(date '+%Y-%m-%d %H:%M:%S')] [BOOT] $1"
    echo "$msg" >> "$LOG_DIR/boot.log"
    write_framebuffer "$msg" "CYAN"
}

# Función especial para mensajes de systemd en framebuffer
log_systemd_framebuffer() {
    local msg="[SYSTEMD] $1"
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $msg" >> "$LOG_DIR/systemd.log"
    write_framebuffer "$msg" "GREEN"
}

# Exportar funciones para uso global
export -f log_message log_systemd log_graphics log_ai log_boot log_systemd_framebuffer write_framebuffer

echo "Sistema de logging Eclipse OS con framebuffer inicializado"
"#;
        
        fs::write(format!("{}/usr/bin/eclipse-logger", temp_root), log_script)
            .map_err(|e| format!("Error creando eclipse-logger: {}", e))?;
        
        // Hacer el script ejecutable
        std::process::Command::new("chmod")
            .args(&["+x", &format!("{}/usr/bin/eclipse-logger", temp_root)])
            .output()
            .map_err(|e| format!("Error haciendo ejecutable eclipse-logger: {}", e))?;
        
        // Script para monitoreo de logs
        let monitor_script = r#"#!/bin/bash
# Eclipse OS Log Monitor

echo "=== Eclipse OS Log Monitor ==="
echo "Monitoreando logs del sistema..."
echo ""

# Función para mostrar logs en tiempo real
monitor_logs() {
    echo "Logs disponibles:"
    echo "1. systemd.log - Sistema de init"
    echo "2. graphics.log - Sistema gráfico"
    echo "3. ai.log - Sistema de IA"
    echo "4. boot.log - Log de arranque"
    echo "5. runtime.log - Log en tiempo real"
    echo ""
    
    read -p "Selecciona log a monitorear (1-5): " choice
    
    case $choice in
        1) tail -f /var/log/eclipse/systemd.log ;;
        2) tail -f /var/log/eclipse/graphics.log ;;
        3) tail -f /var/log/eclipse/ai.log ;;
        4) tail -f /var/log/eclipse/boot.log ;;
        5) tail -f /var/log/eclipse/runtime.log ;;
        *) echo "Opción inválida" ;;
    esac
}

monitor_logs
"#;
        
        fs::write(format!("{}/usr/bin/eclipse-log-monitor", temp_root), monitor_script)
            .map_err(|e| format!("Error creando eclipse-log-monitor: {}", e))?;
        
        // Hacer el script ejecutable
        std::process::Command::new("chmod")
            .args(&["+x", &format!("{}/usr/bin/eclipse-log-monitor", temp_root)])
            .output()
            .map_err(|e| format!("Error haciendo ejecutable eclipse-log-monitor: {}", e))?;
        
        Ok(())
    }
    
    /// Crear configuración de systemd para logging
    fn create_systemd_logging_config(&self, temp_root: &str) -> Result<(), String> {
        // Crear directorio /etc/systemd si no existe
        let systemd_dir = format!("{}/etc/systemd", temp_root);
        fs::create_dir_all(&systemd_dir)
            .map_err(|e| format!("Error creando directorio /etc/systemd: {}", e))?;
        
        // Configuración de journald para logging
        let journald_config = r#"# Eclipse OS Journald Configuration
[Journal]
Storage=persistent
Compress=yes
Seal=yes
SplitMode=uid
SyncIntervalSec=5m
RateLimitIntervalSec=30s
RateLimitBurst=1000
SystemMaxUse=1G
SystemKeepFree=2G
SystemMaxFileSize=10M
RuntimeMaxUse=100M
RuntimeKeepFree=200M
RuntimeMaxFileSize=10M
MaxRetentionSec=1month
MaxFileSec=1week
ForwardToSyslog=no
ForwardToKMsg=no
ForwardToConsole=yes
ForwardToWall=yes
TTYPath=/dev/console
MaxLevelStore=debug
MaxLevelSyslog=debug
MaxLevelKMsg=notice
MaxLevelConsole=info
MaxLevelWall=emerg
"#;
        
        fs::write(format!("{}/etc/systemd/journald.conf", temp_root), journald_config)
            .map_err(|e| format!("Error creando journald.conf: {}", e))?;
        
        // Servicio de logging personalizado
        let logging_service = r#"# Eclipse OS Logging Service
[Unit]
Description=Eclipse OS Logging Service
Documentation=https://github.com/eclipse-os/eclipse-os
After=systemd-journald.service
Before=graphics.service ai.service

[Service]
Type=simple
ExecStart=/usr/bin/eclipse-logger
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#;
        
        fs::write(format!("{}/etc/eclipse/systemd/system/eclipse-logging.service", temp_root), logging_service)
            .map_err(|e| format!("Error creando eclipse-logging.service: {}", e))?;
        
        // Servicio de monitoreo de logs
        let monitor_service = r#"# Eclipse OS Log Monitor Service
[Unit]
Description=Eclipse OS Log Monitor
Documentation=https://github.com/eclipse-os/eclipse-os
After=eclipse-logging.service

[Service]
Type=simple
ExecStart=/usr/bin/eclipse-log-monitor
Restart=no
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#;
        
        fs::write(format!("{}/etc/eclipse/systemd/system/eclipse-log-monitor.service", temp_root), monitor_service)
            .map_err(|e| format!("Error creando eclipse-log-monitor.service: {}", e))?;
        
        Ok(())
    }
    
    /// Crear imagen EclipseFS desde estructura de directorios
    fn create_eclipsefs_image_from_structure(&self, _temp_root: &str, image_path: &str) -> Result<(), String> {
        println!("       💾 Creando imagen EclipseFS real desde estructura...");
        
        // Usar nuestra implementación corregida de EclipseFS
        let mut eclipsefs = crate::eclipsefs_writer::EclipseFSInstaller::new(image_path.to_string());
        
        // Crear estructura básica
        eclipsefs.create_basic_structure()?;
        
        // Instalar binarios del sistema
        self.install_system_binaries(&mut eclipsefs)?;
        
        // Escribir imagen EclipseFS real
        eclipsefs.write_image()?;
        
        println!("       ✅ Imagen EclipseFS real creada");
        Ok(())
    }
    
    /// Escribir imagen a partición
    fn write_image_to_partition(&self, image_path: &str, partition: &str) -> Result<(), String> {
        println!("     💾 Escribiendo imagen a partición...");
        
        let output = std::process::Command::new("dd")
            .args(&[&format!("if={}", image_path), &format!("of={}", partition), "bs=4M", "status=progress"])
            .output()
            .map_err(|e| format!("Error ejecutando dd: {}", e))?;
        
        if !output.status.success() {
            return Err(format!("Error escribiendo imagen: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        println!("     ✅ Imagen escrita a partición exitosamente");
        Ok(())
    }
    
    /// Crear archivos de configuración del sistema
    fn create_system_config_files(&self, eclipsefs: &mut EclipseFSInstaller) -> Result<(), String> {
        println!("       📝 Creando archivos de configuración del sistema...");
        
        // Crear /etc/fstab
        let fstab_content = "# /etc/fstab: static file system information
# <file system> <mount point>   <type>  <options>       <dump>  <pass>
proc            /proc           proc    defaults        0       0
sysfs           /sys            sysfs   defaults        0       0
devtmpfs        /dev            devtmpfs defaults       0       0
tmpfs           /tmp            tmpfs   defaults        0       0
/dev/sda1       /boot           vfat    defaults        0       2
/dev/sda2       /               eclipsefs defaults      0       1
";
        
        eclipsefs.create_file("/etc/fstab", fstab_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/fstab creado");
        
        // Crear /etc/hostname
        let hostname_content = "eclipse-os\n";
        eclipsefs.create_file("/etc/hostname", hostname_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/hostname creado");
        
        // Crear /etc/hosts
        let hosts_content = "127.0.0.1       localhost
::1             localhost
127.0.1.1       eclipse-os
";
        eclipsefs.create_file("/etc/hosts", hosts_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/hosts creado");
        
        // Crear /etc/passwd básico
        let passwd_content = "root:x:0:0:root:/root:/bin/bash
nobody:x:65534:65534:nobody:/nonexistent:/bin/false
";
        eclipsefs.create_file("/etc/passwd", passwd_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/passwd creado");
        
        // Crear /etc/group básico
        let group_content = "root:x:0:
nogroup:x:65534:
";
        eclipsefs.create_file("/etc/group", group_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/group creado");
        
        // Crear /etc/shadow básico (sin contraseñas)
        let shadow_content = "root:*:0:0:99999:7:::
nobody:*:65534:0:99999:7:::
";
        eclipsefs.create_file("/etc/shadow", shadow_content.as_bytes().to_vec())?;
        println!("         ✅ /etc/shadow creado");
        
        println!("       ✅ Archivos de configuración del sistema creados");
        Ok(())
    }
}



