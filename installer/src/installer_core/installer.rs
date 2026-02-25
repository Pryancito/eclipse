use crate::disk::partitioner::Partitioner;
use crate::disk::formatter::Formatter;
use crate::disk::mount::MountManager;
use crate::distro::preparer::DistroPreparer;
use crate::distro::populator::Populator;
use crate::config::uefi::UefiManager;
use crate::config::fstab::FstabGenerator;
use anyhow::{Result, Context};

pub struct EclipseInstaller {
    partitioner: Partitioner,
    formatter: Formatter,
    mount_manager: MountManager,
    preparer: DistroPreparer,
    populator: Populator,
    uefi_manager: UefiManager,
    fstab_gen: FstabGenerator,
}

impl EclipseInstaller {
    pub fn new() -> Result<Self> {
        Ok(Self {
            partitioner: Partitioner,
            formatter: Formatter,
            mount_manager: MountManager::new(),
            preparer: DistroPreparer::new()?,
            populator: Populator,
            uefi_manager: UefiManager,
            fstab_gen: FstabGenerator,
        })
    }

    pub fn run_install(&self, disk_path: &str) -> Result<()> {
        println!("🚀 Iniciando instalacion de Eclipse OS v0.2.0 en {}...", disk_path);
        
        // 1. Preparar sysroot
        println!("\n[1/5] Preparando distribucion...");
        self.preparer.prepare_sysroot().context("Failed to prepare sysroot")?;
        
        // 2. Particionar
        println!("\n[2/5] Particionando disco...");
        self.partitioner.create_gpt(disk_path)?;
        self.partitioner.create_partitions(disk_path)?;
        
        let efi_part = self.partitioner.get_partition_path(disk_path, 1);
        let root_part = self.partitioner.get_partition_path(disk_path, 2);
        
        // Generar fstab antes de poblar (para que se incluya en la imagen)
        self.fstab_gen.generate(self.preparer.get_sysroot_path(), &efi_part, &root_part)?;
        
        // 3. Formatear
        println!("\n[3/5] Formateando particiones...");
        self.formatter.format_efi(&efi_part)?;
        self.formatter.format_root(&root_part)?;
        
        // 4. Poblar root
        println!("\n[4/5] Instalando sistema...");
        self.populator.populate(&root_part, self.preparer.get_sysroot_path())?;
        
        // 5. Configurar UEFI
        println!("\n[5/5] Configurando arranque UEFI...");
        {
            let efi_mount = self.mount_manager.mount_efi(&efi_part)?;
            self.uefi_manager.install_bootloader(efi_mount)?;
            self.uefi_manager.create_uefi_config(efi_mount)?;
            self.mount_manager.unmount_all()?;
        }
        
        println!("\n✨ Instalacion completada exitosamente!");
        Ok(())
    }
}
