use std::process::Command;
use std::path::Path;
use anyhow::{Context, Result, anyhow};

pub struct Populator;

impl Populator {
    pub fn populate(&self, partition: &str, source_dir: &Path) -> Result<()> {
        println!("       💾 Poblando filesystem EclipseFS en {}...", partition);
        let populate_path = crate::paths::resolve_path("../populate-eclipsefs/target/release/populate-eclipsefs");
        
        if !populate_path.exists() {
            return Err(anyhow!("populate-eclipsefs binary not found at {:?}", populate_path));
        }

        let output = Command::new(&populate_path)
            .args(&["-v", partition, source_dir.to_str().unwrap()])
            .output()
            .with_context(|| format!("Error executing {:?}", populate_path))?;

        if !output.status.success() {
            return Err(anyhow!("populate-eclipsefs failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        println!("       ✅ Filesystem poblado correctamente");
        Ok(())
    }
}
