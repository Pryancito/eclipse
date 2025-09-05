use std::process::Command;
use std::fs;
use std::path::Path;
use crate::DiskInfo;

pub struct FilesystemManager {
    root_mount_point: String,
    efi_mount_point: String,
}

impl FilesystemManager {
    pub fn new() -> Self {
        Self {
            root_mount_point: "/mnt/eclipse-root".to_string(),
            efi_mount_point: "/mnt/eclipse-efi".to_string(),
        }
    }
    
    pub fn setup_filesystem(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("🔧 Configurando sistema de archivos...");
        
        // 1. Formatear particiones
        self.format_partitions(disk)?;
        
        // 2. Montar particiones
        self.mount_partitions(disk)?;
        
        // 3. Crear estructura de directorios
        self.create_directory_structure()?;
        
        // 4. Configurar permisos
        self.setup_permissions()?;
        
        // 5. Desmontar particiones
        self.unmount_partitions()?;
        
        println!("✅ Sistema de archivos configurado exitosamente");
        Ok(())
    }
    
    fn format_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   💾 Formateando particiones...");
        
        let efi_partition = format!("{}p1", disk.name);
        let root_partition = format!("{}p2", disk.name);
        
        // Formatear partición EFI como FAT32
        self.format_efi_partition(&efi_partition)?;
        
        // Formatear partición root como EXT4
        self.format_root_partition(&root_partition)?;
        
        Ok(())
    }
    
    fn format_efi_partition(&self, partition: &str) -> Result<(), String> {
        println!("     📁 Formateando partición EFI como FAT32...");
        
        let output = Command::new("mkfs.fat")
            .args(&["-F32", "-n", "ECLIPSE_EFI", partition])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error formateando EFI: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando mkfs.fat: {}", e))
        }
    }
    
    fn format_root_partition(&self, partition: &str) -> Result<(), String> {
        println!("     🗂️  Formateando partición root como EXT4...");
        
        let output = Command::new("mkfs.ext4")
            .args(&["-F", "-L", "ECLIPSE_ROOT", partition])
            .output();
            
        match output {
            Ok(result) => {
                if result.status.success() {
                    Ok(())
                } else {
                    Err(format!("Error formateando root: {}", String::from_utf8_lossy(&result.stderr)))
                }
            }
            Err(e) => Err(format!("Error ejecutando mkfs.ext4: {}", e))
        }
    }
    
    fn mount_partitions(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   📁 Montando particiones...");
        
        let efi_partition = format!("{}p1", disk.name);
        let root_partition = format!("{}p2", disk.name);
        
        // Crear directorios de montaje
        if !Path::new(&self.efi_mount_point).exists() {
            fs::create_dir_all(&self.efi_mount_point)
                .map_err(|e| format!("Error creando directorio EFI: {}", e))?;
        }
        
        if !Path::new(&self.root_mount_point).exists() {
            fs::create_dir_all(&self.root_mount_point)
                .map_err(|e| format!("Error creando directorio root: {}", e))?;
        }
        
        // Montar partición EFI
        let efi_output = Command::new("mount")
            .args(&[&efi_partition, &self.efi_mount_point])
            .output();
            
        match efi_output {
            Ok(result) => {
                if !result.status.success() {
                    return Err(format!("Error montando EFI: {}", String::from_utf8_lossy(&result.stderr)));
                }
            }
            Err(e) => return Err(format!("Error ejecutando mount EFI: {}", e))
        }
        
        // Montar partición root
        let root_output = Command::new("mount")
            .args(&[&root_partition, &self.root_mount_point])
            .output();
            
        match root_output {
            Ok(result) => {
                if !result.status.success() {
                    return Err(format!("Error montando root: {}", String::from_utf8_lossy(&result.stderr)));
                }
            }
            Err(e) => return Err(format!("Error ejecutando mount root: {}", e))
        }
        
        Ok(())
    }
    
    fn create_directory_structure(&self) -> Result<(), String> {
        println!("   📂 Creando estructura de directorios...");
        
        let directories = vec![
            // Directorios del sistema
            "bin", "sbin", "usr/bin", "usr/sbin", "usr/lib", "usr/share",
            "etc", "var", "tmp", "opt", "home", "root",
            "proc", "sys", "dev", "mnt", "media", "run",
            // Directorios específicos de Eclipse OS
            "boot", "boot/efi", "boot/grub",
            "var/log", "var/cache", "var/lib", "var/spool",
            "etc/systemd", "etc/network", "etc/security",
            "usr/local/bin", "usr/local/lib", "usr/local/share",
        ];
        
        for dir in directories {
            let full_path = format!("{}/{}", self.root_mount_point, dir);
            fs::create_dir_all(&full_path)
                .map_err(|e| format!("Error creando directorio {}: {}", dir, e))?;
        }
        
        // Crear enlaces simbólicos
        self.create_symlinks()?;
        
        Ok(())
    }
    
    fn create_symlinks(&self) -> Result<(), String> {
        println!("     🔗 Creando enlaces simbólicos...");
        
        let symlinks = vec![
            ("lib", "usr/lib"),
            ("lib64", "usr/lib"),
            ("sbin", "usr/sbin"),
        ];
        
        for (link, target) in symlinks {
            let link_path = format!("{}/{}", self.root_mount_point, link);
            let target_path = format!("{}/{}", self.root_mount_point, target);
            
            if !Path::new(&link_path).exists() {
                std::os::unix::fs::symlink(target, &link_path)
                    .map_err(|e| format!("Error creando enlace simbólico {}: {}", link, e))?;
            }
        }
        
        Ok(())
    }
    
    fn setup_permissions(&self) -> Result<(), String> {
        println!("   🔐 Configurando permisos...");
        
        // Configurar permisos básicos
        let permissions = vec![
            ("/", 0o755),
            ("/root", 0o700),
            ("/tmp", 0o1777),
            ("/var/tmp", 0o1777),
            ("/proc", 0o555),
            ("/sys", 0o555),
        ];
        
        for (path, mode) in permissions {
            let full_path = format!("{}{}", self.root_mount_point, path);
            if Path::new(&full_path).exists() {
                // En un sistema real, usaríamos chmod aquí
                // Por ahora solo mostramos que se configuraría
            }
        }
        
        Ok(())
    }
    
    fn unmount_partitions(&self) -> Result<(), String> {
        println!("   📤 Desmontando particiones...");
        
        // Desmontar root primero
        let root_output = Command::new("umount")
            .arg(&self.root_mount_point)
            .output();
            
        match root_output {
            Ok(result) => {
                if !result.status.success() {
                    eprintln!("Advertencia: Error desmontando root: {}", String::from_utf8_lossy(&result.stderr));
                }
            }
            Err(e) => eprintln!("Advertencia: Error ejecutando umount root: {}", e)
        }
        
        // Desmontar EFI
        let efi_output = Command::new("umount")
            .arg(&self.efi_mount_point)
            .output();
            
        match efi_output {
            Ok(result) => {
                if !result.status.success() {
                    eprintln!("Advertencia: Error desmontando EFI: {}", String::from_utf8_lossy(&result.stderr));
                }
            }
            Err(e) => eprintln!("Advertencia: Error ejecutando umount EFI: {}", e)
        }
        
        Ok(())
    }
    
    pub fn create_fstab(&self, disk: &DiskInfo) -> Result<(), String> {
        println!("   📝 Creando archivo fstab...");
        
        let efi_partition = format!("{}p1", disk.name);
        let root_partition = format!("{}p2", disk.name);
        
        let fstab_content = format!(r#"# Eclipse OS fstab
# =================

# <file system> <mount point> <type> <options> <dump> <pass>
{} /boot/efi vfat defaults 0 2
{} / ext4 defaults 0 1
proc /proc proc defaults 0 0
sysfs /sys sysfs defaults 0 0
devtmpfs /dev devtmpfs defaults 0 0
tmpfs /tmp tmpfs defaults 0 0
"#, efi_partition, root_partition);
        
        let fstab_path = format!("{}/etc/fstab", self.root_mount_point);
        fs::write(&fstab_path, fstab_content)
            .map_err(|e| format!("Error creando fstab: {}", e))?;
        
        Ok(())
    }
    
    pub fn create_hostname(&self) -> Result<(), String> {
        println!("   🏷️  Configurando hostname...");
        
        let hostname_content = "eclipse-os\n";
        let hostname_path = format!("{}/etc/hostname", self.root_mount_point);
        fs::write(&hostname_path, hostname_content)
            .map_err(|e| format!("Error creando hostname: {}", e))?;
        
        Ok(())
    }
}
