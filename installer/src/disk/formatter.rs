use std::process::Command;
use anyhow::{Context, Result, anyhow};

pub struct Formatter;

impl Formatter {
    pub fn format_efi(&self, partition: &str) -> Result<()> {
        println!("       💾 Formateando particion EFI ({}) como FAT32...", partition);
        let output = Command::new("mkfs.fat")
            .args(&["-F32", "-n", "ECLIPSE_EFI", partition])
            .output()
            .context("Error executing mkfs.fat")?;

        if !output.status.success() {
            return Err(anyhow!("mkfs.fat failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        Ok(())
    }

    pub fn format_root(&self, partition: &str) -> Result<()> {
        println!("       🌟 Formateando particion root ({}) con EclipseFS...", partition);
        let mkfs_path = crate::paths::resolve_path("../mkfs-eclipsefs/target/release/mkfs-eclipsefs");
        
        let output = Command::new(&mkfs_path)
            .args(&["-f", "-L", "Eclipse Root", "-N", "10000", partition])
            .output()
            .with_context(|| format!("Error executing {:?}", mkfs_path))?;

        if !output.status.success() {
            return Err(anyhow!("mkfs-eclipsefs failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        Ok(())
    }
}
