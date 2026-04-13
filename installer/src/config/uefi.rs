use std::fs;
use std::path::Path;
use anyhow::{Result, Context};

pub struct UefiManager;

impl UefiManager {
    /// Instala bootloader y kernel en la partición EFI montada.
    /// `sysroot`: directorio ya preparado por el preparer (contiene boot/eclipse_kernel). Se usa como origen del kernel.
    pub fn install_bootloader(&self, mount_point: &Path, sysroot: &Path) -> Result<()> {
        // 1) Copiar kernel a /boot/ de la EFI primero (el bootloader busca \boot\eclipse_kernel)
        println!("       📦 Copiando kernel a EFI/boot...");
        let efi_boot_dir = mount_point.join("boot");
        fs::create_dir_all(&efi_boot_dir).context("crear directorio boot en EFI")?;
        let kernel_src = sysroot.join("boot").join("eclipse_kernel");
        if !kernel_src.exists() {
            return Err(anyhow::anyhow!(
                "eclipse_kernel no encontrado en sysroot: {:?}. Compila el kernel antes de instalar.",
                kernel_src
            ));
        }
        let kernel_dest = efi_boot_dir.join("eclipse_kernel");
        fs::copy(&kernel_src, &kernel_dest).context("copiar eclipse_kernel a EFI/boot")?;
        println!("         ✓ eclipse_kernel copiado a /boot/ de la particion EFI");

        // 2) Bootloader UEFI
        println!("       🚀 Instalando bootloader UEFI...");
        let boot_dir = mount_point.join("EFI/BOOT");
        let eclipse_dir = mount_point.join("EFI/eclipse");
        fs::create_dir_all(&boot_dir)?;
        fs::create_dir_all(&eclipse_dir)?;
        
        // Priorizar bootloader desde el sysroot (eclipse-os-build) si existe
        let sysroot_bootloader = sysroot.join("efi/boot/bootx64.efi");
        let src = crate::paths::resolve_path("../bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi");
        let debug_src = crate::paths::resolve_path("../bootloader-uefi/target/x86_64-unknown-uefi/debug/eclipse-bootloader.efi");
        
        let bootloader_src = if sysroot_bootloader.exists() {
            println!("         📂 Origen: Bootloader encontrado en el build tree.");
            sysroot_bootloader
        } else if src.exists() {
            src
        } else if debug_src.exists() {
            println!("         ⚠️  Bootloader no encontrado en release, usando version debug");
            debug_src
        } else {
            return Err(anyhow::anyhow!("Bootloader not found at {:?}, {:?} or {:?}", sysroot_bootloader, src, debug_src));
        };
        
        fs::copy(&bootloader_src, boot_dir.join("BOOTX64.EFI"))?;
        fs::copy(&bootloader_src, eclipse_dir.join("eclipse-bootloader.efi"))?;
        println!("         ✓ Bootloader copiado");

        Ok(())
    }

    pub fn create_uefi_config(&self, mount_point: &Path) -> Result<()> {
        let config_src = r#"[system]
kernel_path = "/eclipse_kernel"
userland_path = "/userland/bin/eclipse_userland"
bootloader_path = "/EFI/BOOT/BOOTX64.EFI"

[debug]
serial_logging = true
framebuffer_logging = true
verbose_output = true

[memory]
kernel_heap_size = "32M"
userland_heap_size = "128M"

[graphics]
mode = "1024x768x32"
vga_fallback = true
"#;
        fs::write(mount_point.join("uefi_config.txt"), config_src)?;
        
        // modules.conf
        let modules_src = r#"[eclipse_userland]
enabled = true
priority = 4
path = "/userland/bin/eclipse_userland"
args = "--start-services"
"#;
        fs::write(mount_point.join("modules.conf"), modules_src)?;
        
        println!("         ✓ Configuracion UEFI creada");
        Ok(())
    }
}
