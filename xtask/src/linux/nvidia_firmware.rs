//! Fetches NVIDIA's real GSP-RM firmware for Turing GPUs (source: NVIDIA's
//! own public `linux-firmware` distribution) into the rootfs, where
//! `drivers::display::nvidia` reads it at runtime through the mounted
//! rootfs VFS and hands it to the vendored `kgspInitRm` (see
//! nvidia-rm-sys/vendor/eclipse_rm_init.c).
//!
//! TU102/TU104/TU106/TU116/TU117 (which includes the RTX 2060 Super's
//! TU106) all share one GSP firmware bucket -- confirmed via
//! linux-firmware's WHENCE file, which symlinks tu104/tu106 -> tu102 and
//! tu117 -> tu116 for the `gsp/` directory.
use std::{fs, path::Path};

const NVIDIA_FW_VERSION: &str = "535.113.01";
const NVIDIA_FW_BASE: &str =
    "https://raw.githubusercontent.com/NVIDIA/linux-firmware/main/nvidia/tu102/gsp";

/// (upstream filename stem, stable destination filename Eclipse looks for)
const NVIDIA_FW_FILES: [(&str, &str); 4] = [
    ("gsp", "gsp.bin"),
    ("bootloader", "bootloader.bin"),
    ("booter_load", "booter_load.bin"),
    ("booter_unload", "booter_unload.bin"),
];

/// Best-effort: a missing/failed download just means the real GPU driver
/// finds no firmware at runtime and reports that (same as upstream
/// `nvidia.ko` without `/lib/firmware/nvidia` installed) -- it must never
/// fail the whole OS image build, since GSP firmware is irrelevant to
/// every non-NVIDIA-GPU build and boot path.
pub(super) fn install(rootfs: &Path) {
    let dest_dir = rootfs.join("lib/firmware/nvidia/gsp");
    if dest_dir.join("gsp.bin").is_file() {
        return;
    }
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        eprintln!("warning: could not create {dest_dir:?}: {e}; skipping NVIDIA GSP firmware");
        return;
    }
    for (src_stem, dst_name) in NVIDIA_FW_FILES {
        let url = format!("{NVIDIA_FW_BASE}/{src_stem}-{NVIDIA_FW_VERSION}.bin");
        let dst = dest_dir.join(dst_name);
        println!("Fetching NVIDIA GSP firmware {dst_name} from linux-firmware...");
        let status = std::process::Command::new("wget")
            .arg(&url)
            .arg("-O")
            .arg(&dst)
            .status();
        let ok = matches!(&status, Ok(s) if s.success());
        if !ok {
            eprintln!(
                "warning: failed to fetch {url} ({status:?}); NVIDIA GSP firmware will be unavailable"
            );
            let _ = fs::remove_dir_all(&dest_dir);
            return;
        }
    }
}
