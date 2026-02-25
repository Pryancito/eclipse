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
            ("../eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd", "usr/sbin/eclipse-systemd"),
            ("../eclipse-apps/systemd/target/x86_64-unknown-none/release/eclipse-systemd", "sbin/init"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/smithay_app", "usr/bin/smithay_app"),
            ("../eclipse-apps/target/x86_64-unknown-eclipse/release/demo_client", "usr/bin/demo_client"),
            ("../userland/target/release/eclipse_userland", "userland/bin/eclipse_userland"),
            ("../userland/module_loader/target/release/module_loader", "userland/bin/module_loader"),
            ("../userland/graphics_module/target/release/graphics_module", "userland/bin/graphics_module"),
            ("../userland/app_framework/target/release/app_framework", "userland/bin/app_framework"),
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
