use std::fs;
use std::path::Path;

use crate::disk::partitioner::Partitioner;
use crate::disk::formatter::Formatter;
use crate::disk::mount::MountManager;
use crate::distro::preparer::DistroPreparer;
use crate::distro::populator::Populator;
use crate::config::uefi::UefiManager;
use crate::config::fstab::FstabGenerator;
use anyhow::{Result, Context};
use crate::paths;

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
        
        let build_tree = paths::resolve_path("eclipse-os-build");
        let kernel_marker = build_tree.join("boot/eclipse_kernel");
        let use_full_build_tree = kernel_marker.is_file();

        // 1. Preparar sysroot (o usar eclipse-os-build completo, misma fuente que populate-eclipsefs en build.sh)
        println!("\n[1/5] Preparando distribucion...");
        let root_source: &Path = if use_full_build_tree {
            println!("       📂 Origen: eclipse-os-build completo (populate-eclipsefs copia todo el arbol al disco).");
            &build_tree
        } else {
            println!("       📂 Origen: sysroot minimo en /tmp (ejecuta ./build.sh para generar eclipse-os-build).");
            self.preparer.prepare_sysroot().context("Failed to prepare sysroot")?;
            self.preparer.get_sysroot_path()
        };

        if use_full_build_tree {
            fs::create_dir_all(build_tree.join("etc"))
                .context("crear eclipse-os-build/etc para fstab")?;
        }
        
        // 2. Particionar
        println!("\n[2/5] Particionando disco...");
        self.partitioner.create_gpt(disk_path)?;
        self.partitioner.create_partitions(disk_path)?;
        
        let efi_part = self.partitioner.get_partition_path(disk_path, 1);
        let root_part = self.partitioner.get_partition_path(disk_path, 2);
        
        // Generar fstab antes de poblar (para que se incluya en la imagen)
        self.fstab_gen.generate(root_source, &efi_part, &root_part)?;
        
        // 3. Formatear
        println!("\n[3/5] Formateando particiones...");
        self.formatter.format_efi(&efi_part)?;
        self.formatter.format_root(&root_part)?;
        
        // 4. Poblar root
        println!("\n[4/5] Instalando sistema...");
        self.populator.populate(&root_part, root_source)?;
        
        // 5. Configurar UEFI
        println!("\n[5/5] Configurando arranque UEFI...");
        {
            let efi_mount = self.mount_manager.mount_efi(&efi_part)?;
            self.uefi_manager.install_bootloader(efi_mount, root_source)?;
            self.uefi_manager.create_uefi_config(efi_mount)?;
            self.mount_manager.unmount_all()?;
        }
        
        println!("\n✨ Instalacion completada exitosamente!");
        Ok(())
    }
}
