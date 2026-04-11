use std::fs;
use std::path::Path;
use anyhow::{Context, Result};

pub struct FstabGenerator;

impl FstabGenerator {
    pub fn generate(&self, sysroot: &Path, efi_part: &str, root_part: &str) -> Result<()> {
        let fstab_path = sysroot.join("etc/fstab");
        let content = format!(
            r#"# /etc/fstab: static file system information
# <file system> <mount point>   <type>  <options>       <dump>  <pass>
proc            /proc           proc    defaults        0       0
sysfs           /sys            sysfs   defaults        0       0
devtmpfs        /dev            devtmpfs defaults       0       0
tmpfs           /tmp            tmpfs   defaults        0       0
{}       /boot           vfat    defaults        0       2
{}       /               eclipsefs defaults      0       1
"#,
            efi_part, root_part
        );

        if let Some(parent) = fstab_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Failed to create {:?}", parent))?;
        }
        fs::write(&fstab_path, content).with_context(|| format!("Failed to write fstab to {:?}", fstab_path))?;
        println!("         ✓ /etc/fstab generado");
        Ok(())
    }
}
