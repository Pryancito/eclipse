//! `/usr/bin/cuda-probe`: CUDA through NVIDIA's own libcuda (see
//! tools/cuda-probe). A glibc program like libcuda itself, so it is built
//! with the build machine's C compiler, not musl, and goes in with NVIDIA's
//! userspace, whose glibc it runs on. Own file for the size ceiling of
//! `nvidia_userspace.rs`.
//!
//! Also installs `/usr/bin/cuda-status` (shell): hatch + nodes + libcuda +
//! Ollama CUDA runners, and `/usr/bin/install-ollama-cuda` to fetch the
//! official tarball that actually contains `lib/ollama/cuda_v*`.

use crate::PROJECT_DIR;
use std::{fs, path::Path, process::Command};

/// Where the glibc probe goes in the root.
pub(super) const PATH: &str = "usr/bin/cuda-probe";

/// Hatch / device / libcuda / ollama checklist (shell).
pub(super) const STATUS_PATH: &str = "usr/bin/cuda-status";

/// Official Ollama tarball installer (CUDA runners).
pub(super) const INSTALL_OLLAMA_PATH: &str = "usr/bin/install-ollama-cuda";

/// Wrapper that prefers `cuda_v*` when `cuda-status` is green.
pub(super) const ECLIPSE_OLLAMA_PATH: &str = "usr/bin/eclipse-ollama";

/// Builds the probe and puts it in `rootfs`. Best-effort, like the rest of
/// NVIDIA's userspace: a build machine that cannot make an x86_64 glibc
/// program leaves the image without it.
pub(super) fn install(rootfs: &Path) {
    install_shell(rootfs, "cuda-status.sh", STATUS_PATH);
    install_shell(rootfs, "install-ollama-cuda.sh", INSTALL_OLLAMA_PATH);
    install_shell(rootfs, "eclipse-ollama.sh", ECLIPSE_OLLAMA_PATH);
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

fn install_shell(rootfs: &Path, src_name: &str, dest: &str) {
    let src = PROJECT_DIR.join("tools").join("cuda-probe").join(src_name);
    let dst = rootfs.join(dest);
    let _ = fs::remove_file(&dst);
    if let Err(e) = fs::copy(&src, &dst) {
        eprintln!("warning: {dest}: {}: {e}", dst.display());
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
    }
}
