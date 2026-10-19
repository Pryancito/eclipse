//! `/usr/bin/cuda-probe`: CUDA through NVIDIA's own libcuda (see
//! tools/cuda-probe). A glibc program like libcuda itself, so it is built
//! with the build machine's C compiler, not musl, and goes in with NVIDIA's
//! userspace, whose glibc it runs on. Own file for the size ceiling of
//! `nvidia_userspace.rs`.

use crate::PROJECT_DIR;
use std::{fs, path::Path, process::Command};

/// Where it goes in the root.
pub(super) const PATH: &str = "usr/bin/cuda-probe";

/// Builds the probe and puts it in `rootfs`. Best-effort, like the rest of
/// NVIDIA's userspace: a build machine that cannot make an x86_64 glibc
/// program leaves the image without it.
pub(super) fn install(rootfs: &Path) {
    if !cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        eprintln!(
            "warning: cuda-probe needs an x86_64 glibc build machine; no CUDA probe in the image"
        );
        return;
    }
    let dir = PROJECT_DIR.join("tools").join("cuda-probe");
    let executable = dir.join("cuda-probe");
    let built = Command::new("cc")
        .args(["-O2", "-s", "-o"])
        .arg(&executable)
        .arg(dir.join("cuda-probe.c"))
        .arg("-ldl")
        .status()
        .is_ok_and(|status| status.success());
    if !built {
        eprintln!("warning: cuda-probe not built (cc); no CUDA probe in the image");
        return;
    }
    let dst = rootfs.join(PATH);
    let _ = fs::remove_file(&dst);
    if let Err(e) = fs::copy(&executable, &dst) {
        eprintln!("warning: cuda-probe: {}: {e}", dst.display());
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
    }
}
