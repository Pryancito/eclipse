//! Puts NVIDIA's real GSP-RM firmware image for Turing GPUs into the rootfs,
//! where it's read at runtime (after rootfs mount, from `zCore`'s boot
//! sequence -- `drivers` can't reach the filesystem itself, see
//! `NvidiaGpu::set_gsp_firmware`) and handed to the vendored `kgspInitRm`
//! (nvidia-rm-sys/vendor/eclipse_rm_init.c).
//!
//! Only `gsp.bin` itself is needed here: it's the one genuinely proprietary
//! blob NVIDIA doesn't ship in the open-sourced RM core (the real Linux driver
//! sources it the same way, via `request_firmware`/`nv_get_firmware` in
//! arch/nvalloc/unix/src/osinit.c). The Booter Load/Unload ucodes and the
//! GSP-RM RISC-V bootloader stub that linux-firmware also ships alongside it
//! are NOT needed -- those are already compiled into the vendored RM core as
//! `BINDATA_ARCHIVE` blobs (`generated/g_bindata.c`, fetched via
//! `kgspGetBinArchiveBooterLoadUcode_HAL` / `kgspGetGspRmBootUcodeStorage_HAL`
//! in kernel_gsp_booter.c / kernel_gsp.c), not loaded from external files.
//!
//! # The version must match the vendored RM exactly
//!
//! `kgspInitRm`'s `_kgspFwContainerVerifyVersion` (kernel_gsp.c) hard-fails
//! with `NV_ERR_INVALID_DATA` unless the image's embedded `.fwversion` section
//! equals the RM's `NV_VERSION_STRING` byte for byte. So the version is NOT a
//! constant here: it is read from the pinned submodule's own `version.mk`,
//! which makes it impossible for the two to drift apart when the submodule is
//! re-pinned.
//!
//! # Where the image comes from
//!
//! `NVIDIA/linux-firmware` only carries the GSP versions **Nouveau** supports
//! -- as of writing, 535.113.01 and 570.144 and nothing newer, in both that
//! repo and the canonical `kernel-firmware/linux-firmware`. Eclipse is not
//! Nouveau: it vendors OpenRM itself, so it needs whatever version the
//! submodule is pinned at, which is usually not one of those.
//!
//! The supported way to get that is NVIDIA's own
//! `nouveau/extract-firmware-nouveau.py`, which ships inside the submodule.
//! Given the checked-out tag it downloads the matching `.run` installer and
//! extracts `gsp_tu10x.bin` as `nvidia/tu102/gsp/gsp-<version>.bin`. NVIDIA
//! documents this exact use in `nouveau/extract-firmware-nouveau.txt`:
//! "Linux distro vendors may use Mode #2 to generate firmware packages for
//! GSP-RM versions that are not (yet) available in linux-firmware."
//!
//! Sources are tried cheapest-first: local cache, then `ECLIPSE_GSP_BIN`, then
//! linux-firmware (a plain download, and still correct whenever the pinned
//! version happens to be one Nouveau supports), then the extraction script.
//!
//! # Turing only, for now
//!
//! TU102/TU104/TU106/TU116/TU117 (which includes the RTX 2060 Super's TU106)
//! all share the `gsp_tu10x.bin` bucket -- confirmed via linux-firmware's
//! WHENCE file, which symlinks tu104/tu106 -> tu102 and tu117 -> tu116 for the
//! `gsp/` directory. Ampere-and-later consumer parts need the *other* image
//! the same installer carries, `gsp_ga10x.bin`, and the kernel would have to
//! pick between the two by chip family (`NV_FIRMWARE_CHIP_FAMILY_*`). Since
//! `zCore` reads exactly one path today, only the Turing image is installed.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::PROJECT_DIR;

/// The pinned `open-gpu-kernel-modules` checkout.
fn submodule_dir() -> PathBuf {
    PROJECT_DIR.join("nvidia-rm-sys/vendor/open-gpu-kernel-modules")
}

/// Reads `NVIDIA_VERSION = <x>` out of the submodule's `version.mk` -- the
/// same file NVIDIA's own build and extraction script read, and the source of
/// the `NV_VERSION_STRING` the RM will demand of the firmware at runtime.
fn pinned_rm_version() -> Option<String> {
    let path = submodule_dir().join("version.mk");
    let text = fs::read_to_string(&path).ok()?;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("NVIDIA_VERSION") else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(value) = rest.strip_prefix('=') {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Necessary-condition check on a candidate image: the RM compares the ELF's
/// `.fwversion` section against its own version string, and that section's
/// content is exactly that string, so a blob for the right version must
/// contain those bytes somewhere. Cheap enough to run on every candidate, and
/// it catches the one mistake that otherwise only shows up as a GSP that
/// refuses to boot on real hardware.
fn looks_like_version(image: &Path, version: &str) -> bool {
    let Ok(bytes) = fs::read(image) else {
        return false;
    };
    let needle = version.as_bytes();
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// Downloads linux-firmware's copy, if it has one for this version.
fn try_linux_firmware(version: &str, out: &Path) -> bool {
    let url = format!(
        "https://raw.githubusercontent.com/NVIDIA/linux-firmware/main/nvidia/tu102/gsp/gsp-{version}.bin"
    );
    println!("Trying linux-firmware for GSP-RM {version}...");
    let status = Command::new("wget")
        .arg("-q")
        .arg(&url)
        .arg("-O")
        .arg(out)
        .status();
    if matches!(&status, Ok(s) if s.success()) && looks_like_version(out, version) {
        return true;
    }
    // A 404 still leaves a zero-length (or HTML) file behind.
    let _ = fs::remove_file(out);
    println!("  not published there (expected for versions Nouveau does not support)");
    false
}

/// Runs NVIDIA's own extractor, which downloads the matching `.run` installer
/// and pulls `gsp_tu10x.bin` out of it. Needs real network access to
/// `download.nvidia.com` and a few hundred MB of scratch space.
fn try_extract_script(version: &str, out: &Path) -> bool {
    let script = submodule_dir().join("nouveau/extract-firmware-nouveau.py");
    if !script.is_file() {
        println!(
            "  {} is missing (submodule not checked out?)",
            script.display()
        );
        return false;
    }
    let Some(work) = out.parent().map(|p| p.join("extract")) else {
        return false;
    };
    let _ = fs::remove_dir_all(&work);
    if fs::create_dir_all(&work).is_err() {
        return false;
    }
    println!("Extracting GSP-RM {version} from NVIDIA's .run installer (this downloads a few hundred MB)...");
    let status = Command::new("python3")
        .arg(&script)
        .arg("-i")
        .arg(submodule_dir())
        .arg("-o")
        .arg(&work)
        .arg("-d")
        .status();
    let extracted = work.join(format!("nvidia/tu102/gsp/gsp-{version}.bin"));
    let ok = matches!(&status, Ok(s) if s.success())
        && extracted.is_file()
        && fs::copy(&extracted, out).is_ok()
        && looks_like_version(out, version);
    if !ok {
        let _ = fs::remove_file(out);
    }
    let _ = fs::remove_dir_all(&work);
    ok
}

/// Best-effort: a missing or failed image just means the real GPU driver finds
/// no firmware at runtime and reports that (same as upstream `nvidia.ko`
/// without `/lib/firmware/nvidia` installed) -- it must never fail the whole
/// OS image build, since GSP firmware is irrelevant to every non-NVIDIA-GPU
/// build and boot path.
pub(super) fn install(rootfs: &Path) {
    let dest_dir = rootfs.join("lib/firmware/nvidia/gsp");
    let dst = dest_dir.join("gsp.bin");
    if dst.is_file() {
        return;
    }
    let Some(version) = pinned_rm_version() else {
        eprintln!(
            "warning: could not read NVIDIA_VERSION from the open-gpu-kernel-modules \
             submodule; skipping GSP firmware"
        );
        return;
    };
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        eprintln!("warning: could not create {dest_dir:?}: {e}; skipping NVIDIA GSP firmware");
        return;
    }

    // Persistent cache, so a second build -- or an offline one -- does not
    // re-download a few hundred megabytes. Same convention as the apk cache.
    let cache_dir = PROJECT_DIR.join("ignored").join("nvidia-gsp-cache");
    let cached = cache_dir.join(format!("gsp-{version}.bin"));
    if let Err(e) = fs::create_dir_all(&cache_dir) {
        eprintln!("warning: could not create {cache_dir:?}: {e}");
    }

    if !cached.is_file() {
        // An image the caller extracted themselves -- the escape hatch for an
        // air-gapped build, or a CI runner that cannot reach NVIDIA.
        if let Ok(supplied) = std::env::var("ECLIPSE_GSP_BIN") {
            let supplied = PathBuf::from(supplied);
            if looks_like_version(&supplied, &version) {
                if let Err(e) = fs::copy(&supplied, &cached) {
                    eprintln!("warning: could not copy {supplied:?}: {e}");
                }
            } else {
                eprintln!(
                    "warning: ECLIPSE_GSP_BIN={} does not look like a GSP image for {version} \
                     (no .fwversion match); ignoring it",
                    supplied.display()
                );
            }
        }
    }
    if !cached.is_file() {
        try_linux_firmware(&version, &cached);
    }
    if !cached.is_file() {
        try_extract_script(&version, &cached);
    }

    if !cached.is_file() {
        eprintln!(
            "warning: no GSP-RM firmware for {version} could be obtained, so the NVIDIA GPU \
             will not initialise.\n\
             \n\
             linux-firmware only publishes the versions Nouveau supports. To produce the \
             matching image yourself:\n    \
             nvidia-rm-sys/vendor/open-gpu-kernel-modules/nouveau/extract-firmware-nouveau.py \
             -i nvidia-rm-sys/vendor/open-gpu-kernel-modules -o <dir> -d\n\
             then point ECLIPSE_GSP_BIN at <dir>/nvidia/tu102/gsp/gsp-{version}.bin and \
             re-run this build."
        );
        return;
    }
    if let Err(e) = fs::copy(&cached, &dst) {
        eprintln!("warning: could not install {cached:?} -> {dst:?}: {e}");
        let _ = fs::remove_file(&dst);
        return;
    }
    println!("Installed GSP-RM firmware {version} into the rootfs");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version the firmware is fetched for must be the version the
    /// vendored RM will ask for -- that is the whole point of reading
    /// version.mk instead of hardcoding it. If the submodule is not checked
    /// out there is nothing to assert.
    #[test]
    fn pinned_version_matches_the_rm_source() {
        let Some(version) = pinned_rm_version() else {
            return;
        };
        let header = submodule_dir().join("src/common/inc/nvUnixVersion.h");
        let Ok(text) = fs::read_to_string(&header) else {
            return;
        };
        assert!(
            text.contains(&format!("\"{version}\"")),
            "version.mk says {version} but {} does not define NV_VERSION_STRING to match",
            header.display()
        );
    }

    #[test]
    fn version_check_rejects_a_mismatched_image() {
        let dir = std::env::temp_dir().join("eclipse-gsp-version-check");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake.bin");
        fs::write(&f, b"\x7fELF....570.144\0....").unwrap();
        assert!(looks_like_version(&f, "570.144"));
        assert!(!looks_like_version(&f, "580.178.04"));
        let _ = fs::remove_dir_all(&dir);
    }
}
