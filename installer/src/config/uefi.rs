use std::fs;
use std::path::Path;
use anyhow::Result;

pub struct UefiManager;

impl UefiManager {
    pub fn install_bootloader(&self, mount_point: &Path) -> Result<()> {
        println!("       🚀 Instalando bootloader UEFI...");
        let boot_dir = mount_point.join("EFI/BOOT");
        let eclipse_dir = mount_point.join("EFI/eclipse");
        
        fs::create_dir_all(&boot_dir)?;
        fs::create_dir_all(&eclipse_dir)?;
        
        let src = crate::paths::resolve_path("../bootloader-uefi/target/x86_64-unknown-uefi/release/eclipse-bootloader.efi");
        let debug_src = crate::paths::resolve_path("../bootloader-uefi/target/x86_64-unknown-uefi/debug/eclipse-bootloader.efi");
        
        let bootloader_src = if src.exists() {
            src
        } else if debug_src.exists() {
            println!("         ⚠️  Bootloader no encontrado en release, usando version debug");
            debug_src
        } else {
            return Err(anyhow::anyhow!("Bootloader not found at {:?} or {:?}", src, debug_src));
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
