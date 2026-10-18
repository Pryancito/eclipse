//! `/bin/nvctl-probe`: the steps `nvidia-smi -L` takes through
//! `/dev/nvidiactl`, with no NVIDIA library (see tools/nvctl-probe). Own file
//! for the size ceiling of `linux/mod.rs`.

use super::LinuxRootfs;
use crate::{Arch, PROJECT_DIR};
use os_xtask_utils::{dir, CommandExt, Ext};
use std::{
    fs,
    path::{Path, PathBuf},
};

impl LinuxRootfs {
    /// Builds the probe and puts it in `bin`. x86_64 only, where the NVIDIA
    /// driver is; best-effort like the other probes: a missing musl gcc
    /// leaves the image without it.
    pub(super) fn install_nvctl_probe(&self, musl: &Path, bin: &Path) {
        let Arch::X86_64 = self.0 else {
            return;
        };
        let probe = self.nvctl_probe(musl);
        if probe.is_file() {
            let dst = bin.join("nvctl-probe");
            let _ = dir::rm(&dst);
            fs::copy(&probe, &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        } else {
            eprintln!("warning: nvctl-probe not built; no /dev/nvidiactl probe in the image");
        }
    }

    fn nvctl_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("nvctl-probe");
        let executable = dir.join("nvctl-probe");
        let source = dir.join("nvctl-probe.c");
        if executable.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin), Ok(src)) = (fs::metadata(&executable), fs::metadata(&source)) {
                if let (Ok(bin), Ok(src)) = (bin.modified(), src.modified()) {
                    if bin >= src {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling nvctl-probe...");
        let musl = musl.canonicalize().unwrap();
        let cc = format!(
            "{}/{}-linux-musl-gcc",
            musl.join("bin").display(),
            self.0.name()
        );
        let status = Ext::new(&cc)
            .current_dir(&dir)
            .arg("-static")
            .arg("-O2")
            .arg("-s")
            .arg("-o")
            .arg(&executable)
            .arg(&source)
            .status();
        if !status.success() {
            println!("Failed to compile nvctl-probe");
        }
        executable
    }
}
