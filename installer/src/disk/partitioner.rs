use std::process::Command;
use anyhow::{Context, Result, anyhow};

pub struct Partitioner;

impl Partitioner {
    pub fn create_gpt(&self, disk: &str) -> Result<()> {
        println!("       🏗️  Creando tabla de particiones GPT en {}...", disk);
        
        // Wipe existing signatures
        let _ = Command::new("wipefs").args(&["-a", disk]).output();

        let output = Command::new("parted")
            .args(&[disk, "--script", "mklabel", "gpt"])
            .output()
            .context("Error executing parted mklabel")?;

        if !output.status.success() {
            return Err(anyhow!("Failed to create GPT: {}", String::from_utf8_lossy(&output.stderr)));
        }
        Ok(())
    }

    pub fn create_partitions(&self, disk: &str) -> Result<()> {
        println!("       📏 Creando particiones (EFI 100MiB, ROOT 100%)...");
        
        // EFI Partition
        let output = Command::new("parted")
            .args(&[disk, "--script", "mkpart", "EFI", "fat32", "1MiB", "101MiB"])
            .output()
            .context("Error creating EFI partition")?;
        if !output.status.success() {
            return Err(anyhow!("Failed to create EFI partition: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Set ESP flag
        let _ = Command::new("parted").args(&[disk, "--script", "set", "1", "esp", "on"]).output();

        // Root Partition
        let output = Command::new("parted")
            .args(&[disk, "--script", "mkpart", "ROOT", "ext4", "101MiB", "100%"])
            .output()
            .context("Error creating Root partition")?;
        if !output.status.success() {
            return Err(anyhow!("Failed to create ROOT partition: {}", String::from_utf8_lossy(&output.stderr)));
        }

        // Sync and notify kernel
        let _ = Command::new("sync").output();
        let _ = Command::new("partprobe").arg(disk).output();
        
        Ok(())
    }
    
    pub fn get_partition_path(&self, disk: &str, num: u32) -> String {
        if disk.contains("nvme") || disk.contains("loop") {
            format!("{}p{}", disk, num)
        } else {
            format!("{}{}", disk, num)
        }
    }
}
