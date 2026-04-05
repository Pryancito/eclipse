use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use walkdir::WalkDir;

pub struct DistroPreparer {
    temp_dir: PathBuf,
}

impl DistroPreparer {
    pub fn new() -> Result<Self> {
        let temp_dir = PathBuf::from("/tmp/eclipse_distro_v2_user");
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir).context("Error removing old temp directory")?;
        }
        fs::create_dir_all(&temp_dir).context("Error creating temp directory")?;
        
        Ok(Self { temp_dir })
    }

    pub fn prepare_sysroot(&self) -> Result<()> {
        println!("       📂 Creando estructura de directorios...");
        self.create_directory_structure()?;
        
        println!("       📦 Copiando binarios del sistema...");
        self.copy_binaries()?;
        
        println!("       📜 Copiando configuraciones y servicios...");
        self.copy_configs()?;
        
        Ok(())
    }

    fn create_directory_structure(&self) -> Result<()> {
        let dirs = vec![
            "bin", "sbin", "usr/bin", "usr/sbin", "usr/lib", "usr/share",
            "etc/eclipse/systemd/system", "var/log", "var/lib", "tmp", 
            "proc", "sys", "dev", "boot", "userland/bin", "userland/lib", "userland/config"
        ];

        for dir in dirs {
            let path = self.temp_dir.join(dir);
            fs::create_dir_all(&path).with_context(|| format!("Error creating directory {:?}", path))?;
        }
        Ok(())
    }

    fn copy_binaries(&self) -> Result<()> {
        let binaries = vec![
            ("../eclipse_kernel/target/x86_64-unknown-none/release/eclipse_kernel", "boot/eclipse_kernel"),
            // Init real de Eclipse OS: el kernel lo carga desde /sbin/eclipse-init
            ("../eclipse_kernel/userspace/init/target/x86_64-unknown-eclipse/release/eclipse-init", "sbin/eclipse-init"),
            // Servicios de userspace (cargados desde /sbin por el kernel/init)
            ("../eclipse_kernel/userspace/init/target/x86_64-unknown-eclipse/release/eclipse-init", "sbin/init"),
            ("../eclipse_kernel/userspace/log_service/target/x86_64-unknown-eclipse/release/log_service", "sbin/log_service"),
            ("../eclipse_kernel/userspace/devfs_service/target/x86_64-unknown-eclipse/release/devfs_service", "sbin/devfs_service"),
            ("../eclipse_kernel/userspace/filesystem_service/target/x86_64-unknown-eclipse/release/filesystem_service", "sbin/filesystem_service"),
            ("../eclipse_kernel/userspace/input_service/target/x86_64-unknown-eclipse/release/input_service", "sbin/input_service"),
            ("../eclipse_kernel/userspace/display_service/target/x86_64-unknown-eclipse/release/display_service", "sbin/display_service"),
            ("../eclipse_kernel/userspace/audio_service/target/x86_64-unknown-eclipse/release/audio_service", "sbin/audio_service"),
            ("../eclipse_kernel/userspace/network_service/target/x86_64-unknown-eclipse/release/network_service", "sbin/network_service"),
            ("../eclipse_kernel/userspace/gui_service/target/x86_64-unknown-eclipse/release/gui_service", "sbin/gui_service"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/smithay_app", "usr/bin/smithay_app"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/demo_client", "bin/demo_client"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/lunas", "usr/bin/lunas"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/terminal", "bin/terminal"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/rust-shell", "bin/rust-shell"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/sh", "bin/sh"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/nano", "bin/nano"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/glxgears", "bin/glxgears"),
        ];

        for (src, dest) in binaries {
            let src_path = crate::paths::resolve_path(src);
            if src_path.exists() {
                let dest_path = self.temp_dir.join(dest);
                fs::copy(&src_path, &dest_path).with_context(|| format!("Error copying binary from {:?} to {:?}", src_path, dest))?;
                
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&dest_path)?.permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&dest_path, perms)?;
                }
                println!("         ✓ {} -> {}", src, dest);
            } else {
                println!("         ⚠️  Saltando (no encontrado): {}", src_path.display());
            }
        }
        Ok(())
    }

    fn copy_configs(&self) -> Result<()> {
        // Copiar servicios de systemd
        let services_src = crate::paths::resolve_path("../eclipse-apps/etc/eclipse/systemd/system");
        if services_src.exists() {
            let services_dest = self.temp_dir.join("etc/eclipse/systemd/system");
            for entry in fs::read_dir(&services_src)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let file_name = path.file_name().unwrap();
                    fs::copy(&path, services_dest.join(file_name))?;
                }
            }
            println!("         ✓ Servicios systemd copiados");
        }
        
        Ok(())
    }

    pub fn get_sysroot_path(&self) -> &Path {
        &self.temp_dir
    }
}
