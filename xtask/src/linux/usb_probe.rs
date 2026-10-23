//! `/bin/usb-probe`: input / USB diagnostic probe testing /proc/usbhid,
//! /dev/input/event*, sysfs, synthetic udev db, and libinput compatibility
//! (see tools/usb-probe). Own file for the size ceiling of `linux/mod.rs`.

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
    pub(super) fn install_usb_probe(&self, musl: &Path, bin: &Path) {
        let probe = self.usb_probe(musl);
        if probe.is_file() {
            let dst = bin.join("usb-probe");
            let _ = dir::rm(&dst);
            fs::copy(&probe, &dst).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
            }
        } else {
            eprintln!("warning: usb-probe not built; no usb probe in the image");
        }
    }

    fn usb_probe(&self, musl: &Path) -> PathBuf {
        let dir = PROJECT_DIR.join("tools").join("usb-probe");
        let executable = dir.join("usb-probe");
        let source = dir.join("usb-probe.c");
        if executable.is_file() && self.cached_for_target(&executable) {
            if let (Ok(bin), Ok(src)) = (fs::metadata(&executable), fs::metadata(&source)) {
                if let (Ok(bin), Ok(src)) = (bin.modified(), src.modified()) {
                    if bin >= src {
                        return executable;
                    }
                }
            }
        }

        println!("Compiling usb-probe...");
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
            println!("Failed to compile usb-probe");
        }
        executable
    }
}
