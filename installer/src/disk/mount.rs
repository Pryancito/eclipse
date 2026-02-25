use std::process::Command;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result, anyhow};
use std::fs;

pub struct MountManager {
    efi_mount: PathBuf,
}

impl MountManager {
    pub fn new() -> Self {
        Self {
            efi_mount: PathBuf::from("/tmp/eclipse_efi_v2"),
        }
    }

    pub fn mount_efi(&self, partition: &str) -> Result<&Path> {
        if !self.efi_mount.exists() {
            fs::create_dir_all(&self.efi_mount).context("Failed to create efi mount point")?;
        }

        // Si es un archivo regular (modo test), saltar el montaje real
        if Path::new(partition).is_file() {
            println!("       🧪 Modo Test: Usando directorio directo para EFI");
            return Ok(&self.efi_mount);
        }

        println!("       💾 Montando {} en {:?}...", partition, self.efi_mount);
        
        // Force unmount if already mounted
        let _ = Command::new("umount").arg("-l").arg(&self.efi_mount).output();

        let output = Command::new("mount")
            .args(&[partition, self.efi_mount.to_str().unwrap()])
            .output()
            .context("Error executing mount command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Failed to mount EFI ({}): {}", partition, stderr));
        }
        Ok(&self.efi_mount)
    }

    pub fn unmount_all(&self) -> Result<()> {
        if self.efi_mount.exists() {
            // Intentar desmontar, pero no fallar si no estaba montado (posible modo test)
            let _ = Command::new("umount")
                .arg(&self.efi_mount)
                .output();
            
            let _ = fs::remove_dir(&self.efi_mount);
        }
        Ok(())
    }
}
