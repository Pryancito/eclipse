//! `/bin/fp-probe`: whether a thread's x87, SSE and AVX registers come back
//! from the kernel as it left them (system calls, switches, `fork`, signals;
//! see tools/fp-probe). x86_64 only: the probe is written in its registers.
//! Own file for the size ceiling of `linux/mod.rs`.

use super::LinuxRootfs;
use crate::PROJECT_DIR;
use os_xtask_utils::{dir, CommandExt, Ext};
use std::{
    fs,
    path::{Path, PathBuf},
};

impl LinuxRootfs {
    /// Builds the probe and puts it in `bin`. Best-effort like the other
    /// probes: a missing musl gcc leaves the image without it.
    pub(super) fn install_fp_probe(&self, musl: &Path, bin: &Path) {
        if self.0.name() != "x86_64" {
            return;
        }
        let probe = self.fp_probe(musl);
        if probe.is_file() {
            let dst = bin.join("fp-probe");
            let _ = dir::rm(&dst);
            fs::copy(&probe, &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        } else {
            eprintln!("warning: fp-probe not built; no vector register probe in the image");
        }
    }

    fn fp_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("fp-probe");
        let executable = dir.join("fp-probe");
        let source = dir.join("fp-probe.c");
        if executable.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin), Ok(src)) = (fs::metadata(&executable), fs::metadata(&source)) {
                if let (Ok(bin), Ok(src)) = (bin.modified(), src.modified()) {
                    if bin >= src {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling fp-probe...");
        let musl = musl.canonicalize().unwrap();
        let cc = format!(
            "{}/{}-linux-musl-gcc",
            musl.join("bin").display(),
            self.0.name()
        );
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-pthread")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile fp-probe");
        }
        executable
    }
}
