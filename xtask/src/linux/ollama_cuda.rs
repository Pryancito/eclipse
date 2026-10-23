//! Optional official Ollama (glibc + CUDA runners) into the desktop rootfs.
//!
//! **Off by default** — Ollama is not part of the shipped image; users run
//! `/usr/bin/install-ollama-cuda` (or set `ECLIPSE_OLLAMA_CUDA=1` at image
//! build) when they want it. The overlay is `/usr/local/bin/ollama` plus
//! `/usr/local/lib/ollama/cuda_v*` (`ml.LibOllamaPath`).
//!
//! Upstream ships `ollama-linux-amd64.tar.zst` (the old `.tgz` is 404).
//! Never copied into the QEMU live initramfs (no NVIDIA GPU; ~1.5 GiB+).

use crate::PROJECT_DIR;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

/// Hatch to **enable** install at image build (`1`, `on`, `yes`, `true`).
pub(super) const HATCH: &str = "ECLIPSE_OLLAMA_CUDA";

const BASE: &str = "https://ollama.com/download";
const NAME: &str = "ollama-linux-amd64";

/// Whether this build installs the official CUDA Ollama (opt-in).
pub(super) fn wanted() -> bool {
    wanted_by(std::env::var(HATCH).ok().as_deref())
}

fn wanted_by(value: Option<&str>) -> bool {
    value.is_some_and(knob_on)
}

fn knob_on(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "on" | "yes" | "true"
    )
}

fn owned_paths(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("usr/local/bin/ollama"),
        root.join("usr/local/lib/ollama"),
    ]
}

/// Drop what an earlier build left (QEMU live, or hatch off).
pub(super) fn remove(root: &Path, why: &str) {
    let mut n = 0usize;
    for p in owned_paths(root) {
        if p.is_dir() {
            if fs::remove_dir_all(&p).is_ok() {
                n += 1;
            }
        } else if fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    if n > 0 {
        println!(
            "  {}: took out official Ollama CUDA ({n} paths): {why}",
            root.display()
        );
    }
}

/// Best-effort: download (or use cache) and extract into `rootfs` `/usr/local`.
pub(super) fn install(rootfs: &Path) {
    if !wanted() {
        remove(rootfs, &format!("{HATCH} is off"));
        return;
    }
    println!("Installing official Ollama (glibc + CUDA runners) into /usr/local...");
    let cache = PROJECT_DIR.join("ignored").join("ollama-cache");
    if let Err(e) = fs::create_dir_all(&cache) {
        eprintln!("warning: ollama-cuda: mkdir {}: {e}", cache.display());
        return;
    }

    let (archive, kind) = match ensure_archive(&cache) {
        Ok(v) => v,
        Err(why) => {
            eprintln!(
                "warning: ollama-cuda: {why}; put {NAME}.tar.zst in {} by hand, or {HATCH}=0",
                cache.display()
            );
            return;
        }
    };

    let staging = cache.join("extract");
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = fs::create_dir_all(&staging) {
        eprintln!("warning: ollama-cuda: staging: {e}");
        return;
    }
    if let Err(why) = extract(&archive, kind, &staging) {
        eprintln!("warning: ollama-cuda: extract: {why}");
        return;
    }

    let bin_src = staging.join("bin/ollama");
    let lib_src = staging.join("lib/ollama");
    if !bin_src.is_file() || !lib_src.is_dir() {
        eprintln!(
            "warning: ollama-cuda: archive missing bin/ollama or lib/ollama (unexpected layout)"
        );
        return;
    }

    let bin_dst = rootfs.join("usr/local/bin/ollama");
    let lib_dst = rootfs.join("usr/local/lib/ollama");
    if let Err(e) = fs::create_dir_all(bin_dst.parent().unwrap()) {
        eprintln!("warning: ollama-cuda: {e}");
        return;
    }
    let _ = fs::remove_file(&bin_dst);
    let _ = fs::remove_dir_all(&lib_dst);
    if let Err(e) = fs::copy(&bin_src, &bin_dst) {
        eprintln!("warning: ollama-cuda: copy bin: {e}");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bin_dst, fs::Permissions::from_mode(0o755));
    }
    if let Err(e) = copy_dir(&lib_src, &lib_dst) {
        eprintln!("warning: ollama-cuda: copy lib: {e}");
        let _ = fs::remove_file(&bin_dst);
        let _ = fs::remove_dir_all(&lib_dst);
        return;
    }

    let runners: Vec<_> = fs::read_dir(&lib_dst)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with("cuda_v")
        })
        .map(|e| e.path())
        .collect();
    if runners.is_empty() {
        eprintln!(
            "warning: ollama-cuda: no cuda_v* under {}; GPU will not work",
            lib_dst.display()
        );
    } else {
        for r in &runners {
            println!("  runner {}", r.display());
        }
    }
    println!("  done: {} + {}", bin_dst.display(), lib_dst.display());
}

#[derive(Clone, Copy)]
enum Kind {
    Zst,
    Tgz,
}

fn ensure_archive(cache: &Path) -> Result<(PathBuf, Kind), String> {
    let zst = cache.join(format!("{NAME}.tar.zst"));
    let tgz = cache.join(format!("{NAME}.tgz"));
    if zst.is_file() && file_big_enough(&zst) {
        println!("  using cached {}", zst.display());
        return Ok((zst, Kind::Zst));
    }
    if tgz.is_file() && file_big_enough(&tgz) {
        println!("  using cached {}", tgz.display());
        return Ok((tgz, Kind::Tgz));
    }

    let url_zst = format!("{BASE}/{NAME}.tar.zst");
    println!("  downloading {url_zst} → {}", zst.display());
    match download(&url_zst, &zst) {
        Ok(()) if file_big_enough(&zst) => Ok((zst, Kind::Zst)),
        Ok(()) | Err(_) => {
            let _ = fs::remove_file(&zst);
            let url_tgz = format!("{BASE}/{NAME}.tgz");
            println!("  falling back to {url_tgz}");
            download(&url_tgz, &tgz)?;
            if !file_big_enough(&tgz) {
                let _ = fs::remove_file(&tgz);
                return Err(format!(
                    "download too small or 404 (tried .tar.zst and .tgz from {BASE})"
                ));
            }
            Ok((tgz, Kind::Tgz))
        }
    }
}

fn file_big_enough(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.len() >= 1_000_000).unwrap_or(false)
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("part");
    let _ = fs::remove_file(&tmp);
    let status = Command::new("curl")
        .args(["-fL", "--progress-bar", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if !status.success() {
        let status = Command::new("wget")
            .args(["-O"])
            .arg(&tmp)
            .arg(url)
            .status()
            .map_err(|e| format!("wget: {e}"))?;
        if !status.success() {
            let _ = fs::remove_file(&tmp);
            return Err(format!("download failed from {url}"));
        }
    }
    fs::rename(&tmp, dest).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

fn extract(archive: &Path, kind: Kind, dest: &Path) -> Result<(), String> {
    match kind {
        Kind::Zst => {
            // zstd -d -c archive | tar -xf - -C dest
            let zstd = Command::new("zstd")
                .args(["-d", "-c"])
                .arg(archive)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("zstd: {e} (install zstd on the build host)"))?;
            let out = zstd.stdout.ok_or_else(|| "zstd: no stdout".to_string())?;
            let status = Command::new("tar")
                .args(["-xf", "-", "-C"])
                .arg(dest)
                .stdin(out)
                .status()
                .map_err(|e| format!("tar: {e}"))?;
            if !status.success() {
                return Err(format!("tar exit {status}"));
            }
            Ok(())
        }
        Kind::Tgz => {
            let status = Command::new("tar")
                .args(["-xzf"])
                .arg(archive)
                .arg("-C")
                .arg(dest)
                .status()
                .map_err(|e| format!("tar: {e}"))?;
            if !status.success() {
                return Err(format!("tar exit {status}"));
            }
            Ok(())
        }
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for ent in fs::read_dir(src)? {
        let ent = ent?;
        let from = ent.path();
        let to = dst.join(ent.file_name());
        if ent.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
            if let Ok(meta) = fs::metadata(&from) {
                let _ = fs::set_permissions(&to, meta.permissions());
            }
        }
    }
    let mut mark = fs::File::create(dst.join(".eclipse-ollama-cuda"))?;
    writeln!(mark, "official {NAME}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hatch_is_opt_in() {
        assert!(!wanted_by(None));
        assert!(!wanted_by(Some("0")));
        assert!(!wanted_by(Some("off")));
        assert!(wanted_by(Some("1")));
        assert!(wanted_by(Some("on")));
        assert!(wanted_by(Some("yes")));
        assert!(wanted_by(Some("true")));
    }
}
