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

/// Reads an ELF64 little-endian section's contents by name, or `None` if the
/// file is not such an ELF or has no section with that name. Deliberately a
/// hand-rolled reader rather than a new dependency: this needs one section out
/// of one well-formed file, and every offset it trusts is bounds-checked.
fn elf64_section<'a>(bytes: &'a [u8], want: &str) -> Option<&'a [u8]> {
    let at_u16 = |off: usize| -> Option<usize> {
        let b = bytes.get(off..off + 2)?;
        Some(u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let at_u32 = |off: usize| -> Option<usize> {
        let b = bytes.get(off..off + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let at_u64 = |off: usize| -> Option<usize> {
        let b = bytes.get(off..off + 8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?) as usize)
    };

    // e_ident: magic, then EI_CLASS == 2 (64-bit) and EI_DATA == 1 (LE).
    if bytes.get(..4)? != b"\x7fELF" || *bytes.get(4)? != 2 || *bytes.get(5)? != 1 {
        return None;
    }
    let e_shoff = at_u64(0x28)?;
    let e_shentsize = at_u16(0x3A)?;
    let e_shnum = at_u16(0x3C)?;
    let e_shstrndx = at_u16(0x3E)?;
    if e_shentsize < 0x40 || e_shstrndx >= e_shnum {
        return None;
    }
    let shdr = |i: usize| -> Option<(usize, usize, usize)> {
        let base = e_shoff.checked_add(i.checked_mul(e_shentsize)?)?;
        Some((at_u32(base)?, at_u64(base + 0x18)?, at_u64(base + 0x20)?))
    };

    // The section-name string table, then a linear scan for `want`.
    let (_, strtab_off, strtab_size) = shdr(e_shstrndx)?;
    let strtab = bytes.get(strtab_off..strtab_off.checked_add(strtab_size)?)?;
    for i in 0..e_shnum {
        let (sh_name, sh_offset, sh_size) = shdr(i)?;
        let name = strtab.get(sh_name..)?;
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        if name == want.as_bytes() {
            return bytes.get(sh_offset..sh_offset.checked_add(sh_size)?);
        }
    }
    None
}

/// Mirrors what `_kgspFwContainerVerifyVersion` (kernel_gsp.c) does at runtime:
/// pull the `.fwversion` section out of the container and require it to be
/// exactly the version string plus its NUL terminator. Running the real check
/// at build time is the point -- a mismatched image otherwise only shows up as
/// a GSP that refuses to boot on real hardware.
///
/// Searching the whole blob for the version bytes would not do: an image for
/// another version can carry this one's string in some other section or in
/// metadata and would sail through.
fn looks_like_version(image: &Path, version: &str) -> bool {
    let Ok(bytes) = fs::read(image) else {
        return false;
    };
    let Some(section) = elf64_section(&bytes, ".fwversion") else {
        return false;
    };
    // fwversionSize == strlen(NV_VERSION_STRING) + 1, and the bytes before the
    // terminator must match -- the same two conditions the RM applies.
    section.len() == version.len() + 1
        && section.last() == Some(&0)
        && &section[..version.len()] == version.as_bytes()
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

/// Hands back the extractor to run, preferring `origin/main`'s copy over the
/// one checked out at the pinned tag.
///
/// This is not gratuitous: NVIDIA says to do it, and 580.178.04 is exactly why.
/// From `nouveau/extract-firmware-nouveau.txt`: "Unfortunately, [checking out
/// a version tag] has the side-effect of also checking out an outdated version
/// of the script. To ensure that the latest version is used: git checkout main
/// -- nouveau/extract-firmware-nouveau.py".
///
/// Concretely, the script shipped at the 580.178.04 tag cannot read its own
/// tree: it looks for `<sym>_image_prod_data` while the generated bindata
/// declares `<sym>_BINDATA_LABEL_IMAGE_PROD_data`, and dies with "array
/// kgspBinArchiveBooterLoadUcode_TU102_image_prod_data not found". The copy on
/// `main` tries both spellings -- its own comment dates the rename to r575 --
/// so it handles the pinned tree and older ones alike.
///
/// Falls back to the checked-out copy when the fetch fails (no network, or a
/// submodule clone without that ref), since for a pre-r575 pin it is correct.
fn newest_extract_script(work: &Path) -> Option<PathBuf> {
    const REL: &str = "nouveau/extract-firmware-nouveau.py";
    let in_tree = submodule_dir().join(REL);

    let fetched = (|| {
        let ok = Command::new("git")
            .args(["fetch", "--depth", "1", "origin", "main"])
            .current_dir(submodule_dir())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        let out = Command::new("git")
            .args(["show", &format!("FETCH_HEAD:{REL}")])
            .current_dir(submodule_dir())
            .output()
            .ok()?;
        if !out.status.success() || out.stdout.is_empty() {
            return None;
        }
        let path = work.join("extract-firmware-nouveau.py");
        fs::write(&path, &out.stdout).ok()?;
        Some(path)
    })();

    match fetched {
        Some(path) => {
            println!("  using origin/main's extractor (the tag ships an older one)");
            Some(path)
        }
        None if in_tree.is_file() => {
            println!(
                "  could not fetch origin/main's extractor; falling back to the one at the \
                 pinned tag, which may not understand its own bindata naming"
            );
            Some(in_tree)
        }
        None => {
            println!(
                "  {} is missing (submodule not checked out?)",
                in_tree.display()
            );
            None
        }
    }
}

/// Runs NVIDIA's own extractor, which downloads the matching `.run` installer
/// and pulls `gsp_tu10x.bin` out of it. Needs real network access to
/// `download.nvidia.com` and a few hundred MB of scratch space.
fn try_extract_script(version: &str, out: &Path) -> bool {
    let Some(work) = out.parent().map(|p| p.join("extract")) else {
        return false;
    };
    let _ = fs::remove_dir_all(&work);
    if fs::create_dir_all(&work).is_err() {
        return false;
    }
    let Some(script) = newest_extract_script(&work) else {
        return false;
    };
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

/// Puts the image for the pinned RM version in the cache and hands back its
/// path, or `None` with an explanation already printed. Every caller wants the
/// same chain, cheapest source first, so it lives here rather than in
/// `install` -- `cargo nvidia-firmware` runs exactly this and nothing else.
///
/// `force` re-fetches even when the cache already has the file, for the case
/// where a cached image is suspect.
pub(crate) fn obtain(force: bool) -> Option<PathBuf> {
    let version = pinned_rm_version()?;

    // Persistent cache, so a second build -- or an offline one -- does not
    // re-download a few hundred megabytes. Same convention as the apk cache.
    let cache_dir = PROJECT_DIR.join("ignored").join("nvidia-gsp-cache");
    let cached = cache_dir.join(format!("gsp-{version}.bin"));
    if let Err(e) = fs::create_dir_all(&cache_dir) {
        eprintln!("warning: could not create {cache_dir:?}: {e}");
    }
    if force && cached.is_file() {
        println!("Discarding cached {}", cached.display());
        let _ = fs::remove_file(&cached);
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
             Extracting it needs to reach download.nvidia.com for the matching .run \
             installer. If this machine cannot, run this where it can:\n    \
             cargo nvidia-firmware --out gsp-{version}.bin\n\
             bring that file over, and then here:\n    \
             ECLIPSE_GSP_BIN=gsp-{version}.bin cargo nvidia-firmware"
        );
        return None;
    }
    Some(cached)
}

/// `cargo nvidia-firmware`: obtain the image and report where it is, without
/// building a rootfs. Unlike `install`, this is the user asking for the
/// firmware on purpose, so an unobtainable image is an error, not a warning.
pub(crate) fn make(out: Option<PathBuf>, force: bool) -> bool {
    let Some(version) = pinned_rm_version() else {
        eprintln!(
            "error: could not read NVIDIA_VERSION from \
             nvidia-rm-sys/vendor/open-gpu-kernel-modules/version.mk -- is the submodule \
             checked out? (git submodule update --init --recursive)"
        );
        return false;
    };
    println!("GSP-RM firmware for the pinned RM version {version}");
    let Some(cached) = obtain(force) else {
        return false;
    };
    println!("  {}", cached.display());
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent) {
                    eprintln!("error: could not create {}: {e}", parent.display());
                    return false;
                }
            }
        }
        if let Err(e) = fs::copy(&cached, &out) {
            eprintln!("error: could not copy to {}: {e}", out.display());
            return false;
        }
        println!("  copied to {}", out.display());
    }
    true
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
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        eprintln!("warning: could not create {dest_dir:?}: {e}; skipping NVIDIA GSP firmware");
        return;
    }
    let Some(cached) = obtain(false) else {
        return;
    };
    if let Err(e) = fs::copy(&cached, &dst) {
        eprintln!("warning: could not install {cached:?} -> {dst:?}: {e}");
        let _ = fs::remove_file(&dst);
        return;
    }
    println!(
        "Installed GSP-RM firmware into the rootfs from {}",
        cached.display()
    );
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

    /// Builds the smallest ELF64 that the reader accepts: a header, a
    /// `.fwversion` section holding `fwversion`, a `.shstrtab`, and `filler`
    /// appended after everything so a test can plant bytes that are in the
    /// file but NOT in `.fwversion`.
    fn fake_gsp_elf(fwversion: &[u8], filler: &[u8]) -> Vec<u8> {
        const EHDR: usize = 64;
        const SHENT: usize = 64;
        let shstrtab: &[u8] = b"\0.fwversion\0.shstrtab\0";

        let fw_off = EHDR;
        let str_off = fw_off + fwversion.len();
        let sh_off = str_off + shstrtab.len();

        let mut v = vec![0u8; EHDR];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[5] = 1; // ELFDATA2LSB
        v[0x28..0x30].copy_from_slice(&(sh_off as u64).to_le_bytes()); // e_shoff
        v[0x3A..0x3C].copy_from_slice(&(SHENT as u16).to_le_bytes()); // e_shentsize
        v[0x3C..0x3E].copy_from_slice(&3u16.to_le_bytes()); // e_shnum
        v[0x3E..0x40].copy_from_slice(&2u16.to_le_bytes()); // e_shstrndx
        v.extend_from_slice(fwversion);
        v.extend_from_slice(shstrtab);

        let mut shdr = |name: u32, off: usize, size: usize| {
            let mut h = vec![0u8; SHENT];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[0x18..0x20].copy_from_slice(&(off as u64).to_le_bytes());
            h[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            v.extend_from_slice(&h);
        };
        shdr(0, 0, 0); // SHN_UNDEF
        shdr(1, fw_off, fwversion.len()); // ".fwversion"
        shdr(12, str_off, shstrtab.len()); // ".shstrtab"
        v.extend_from_slice(filler);
        v
    }

    fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join("eclipse-gsp-version-check");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join(name);
        fs::write(&f, bytes).unwrap();
        f
    }

    #[test]
    fn version_check_reads_the_fwversion_section() {
        let f = scratch("good.bin", &fake_gsp_elf(b"580.178.04\0", b""));
        assert!(looks_like_version(&f, "580.178.04"));
        assert!(!looks_like_version(&f, "570.144"));
    }

    /// The reason this reads the section instead of searching the file: an
    /// image built for another version can carry the string we want somewhere
    /// else entirely -- in a log section, a path, some metadata -- and a
    /// whole-blob search would accept it and install a firmware the RM then
    /// rejects at boot with NV_ERR_INVALID_DATA.
    #[test]
    fn version_check_ignores_the_string_outside_fwversion() {
        let f = scratch(
            "decoy.bin",
            &fake_gsp_elf(b"570.144\0", b"built from 580.178.04 sources"),
        );
        assert!(!looks_like_version(&f, "580.178.04"));
        assert!(looks_like_version(&f, "570.144"));
    }

    /// A prefix must not pass: the RM requires the length to match too.
    #[test]
    fn version_check_rejects_a_prefix() {
        let f = scratch("prefix.bin", &fake_gsp_elf(b"580.178.04\0", b""));
        assert!(!looks_like_version(&f, "580.178"));
    }

    #[test]
    fn version_check_rejects_a_non_elf() {
        let f = scratch("notelf.bin", b"580.178.04\0 but not an ELF at all");
        assert!(!looks_like_version(&f, "580.178.04"));
    }
}
