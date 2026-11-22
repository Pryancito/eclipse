//! NVIDIA's own userspace for the vendored RM: `nvidia-smi`, NVML
//! (`libnvidia-ml`) and the CUDA driver (`libcuda` and its PTX JIT), with the
//! glibc runtime they need. They are the real clients of `/dev/nvidiactl`
//! (the `nvidia.nvidiactl` hatch). Installed unless the build is run with
//! `ECLIPSE_NVIDIA_USERSPACE=0`, which also takes out NVIDIA's files an earlier
//! build left in the rootfs. They never reach the QEMU live root, where there
//! is no NVIDIA GPU for them. `/usr/bin/cuda-probe` (`cuda_probe.rs`), a
//! check of CUDA through libcuda, comes and goes with them.
//!
//! # The version must match the vendored RM exactly
//!
//! Every NVIDIA client opens with `NV_ESC_CHECK_VERSION_STR`, and in strict
//! mode the RM refuses a client of any other version. So, as for the GSP
//! firmware, the version is the pinned submodule's own (`version.mk`), and a
//! package for any other version is never taken.
//!
//! # Where it comes from
//!
//! Ubuntu's archive carries NVIDIA's binaries for each driver release as
//! NVIDIA builds them, split into `libnvidia-compute-<major>` (the libraries)
//! and `nvidia-utils-<major>` (`nvidia-smi`): the same files the `.run`
//! installer carries, from a host a build machine can reach whenever it can
//! fetch Ubuntu packages. Which packaging revision of the version is taken
//! does not matter, since the binaries are never rebuilt.
//!
//! The image is musl and NVIDIA's binaries are glibc. Their whole closure is
//! `libc6` (libc, libm, libdl, libpthread, librt and the loader) plus
//! `libgcc_s`, which glibc dlopens to unwind a cancelled thread. Both come
//! from Ubuntu 26.10's (`stonking`) release pocket, pinned by name and
//! SHA-256. The loader goes where the binaries' PT_INTERP names it,
//! `/lib64/ld-linux-x86-64.so.2`, and the libraries go to
//! `/usr/lib/x86_64-linux-gnu`, which that loader searches before `/usr/lib`,
//! where the musl ones live.
//!
//! The packages are kept in `ignored/nvidia-userspace-cache`. A machine that
//! cannot reach the archive can be given them there by hand, and then nothing
//! is downloaded.
//!
//! Unpacking needs `tar` (GNU tar with `zstd`, which Ubuntu's packages are
//! compressed with) and `sha256sum` on the build machine.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::PROJECT_DIR;

/// The environment variable that turns it off (`0`, `off`, `no` or `false`).
pub(super) const HATCH: &str = "ECLIPSE_NVIDIA_USERSPACE";

/// Whether this build installs it.
pub(super) fn wanted() -> bool {
    wanted_by(std::env::var(HATCH).ok().as_deref())
}

/// On unless `value`, the variable's, is one of the off spellings the other
/// build switches take.
fn wanted_by(value: Option<&str>) -> bool {
    !value.is_some_and(super::xorg::knob_off)
}

const POOL: &str = "https://archive.ubuntu.com/ubuntu/pool";

/// Where glibc's libraries go: the loader's first search directory that is
/// not the musl one.
const LIBDIR: &str = "usr/lib/x86_64-linux-gnu";

/// Where glibc's loader goes: the PT_INTERP of every x86_64 glibc binary.
const LOADER: &str = "lib64/ld-linux-x86-64.so.2";

/// How a package's `.deb` is chosen.
#[derive(Clone, Debug, PartialEq)]
enum Deb {
    /// Exactly this file, with this SHA-256.
    Pinned {
        pool: &'static str,
        file: &'static str,
        sha256: &'static str,
    },
    /// Any amd64 build of the pool directory whose name starts with `prefix`
    /// (`<package>_<version>-`).
    Release { pool: String, prefix: String },
}

impl Deb {
    fn pool(&self) -> &str {
        match self {
            Deb::Pinned { pool, .. } => pool,
            Deb::Release { pool, .. } => pool,
        }
    }
}

/// A file taken out of a package: its path inside the `.deb` and where it
/// goes in the rootfs.
#[derive(Clone, Debug, PartialEq)]
struct Take {
    from: String,
    to: String,
}

/// Everything installed, package by package, and the links made after.
#[derive(Clone, Debug, PartialEq)]
struct Plan {
    /// The glibc packages, installed only on a rootfs with no glibc loader.
    runtime: Vec<(Deb, Vec<Take>)>,
    /// NVIDIA's packages.
    nvidia: Vec<(Deb, Vec<Take>)>,
    /// `(link, target)`, the target relative to the link's directory.
    links: Vec<(String, String)>,
    /// Programs, which must carry the version string they were built for.
    programs: Vec<String>,
}

fn same(path: &str) -> Take {
    Take {
        from: path.to_string(),
        to: path.to_string(),
    }
}

/// What is installed for RM `version`.
fn plan(version: &str) -> Plan {
    let major = version.split('.').next().unwrap_or(version);
    let nvidia = |package: &str| Deb::Release {
        pool: format!("restricted/n/nvidia-graphics-drivers-{major}"),
        prefix: format!("{package}-{major}_{version}-"),
    };
    let glibc = Deb::Pinned {
        pool: "main/g/glibc",
        file: "libc6_2.44-1ubuntu2_amd64.deb",
        sha256: "e4871fc9d5bf5fe0f384c6b90f70e64e9c9bbcb84927aa97c7f70ec6bf05b8c0",
    };
    let libgcc = Deb::Pinned {
        pool: "main/g/gcc-16",
        file: "libgcc-s1_16.2.0-3ubuntu1_amd64.deb",
        sha256: "6d0b3779d578ffc598c26678ea26228b20880044742f44dcd78b2556fdc97d07",
    };
    let mut libc = vec![Take {
        from: format!("{LIBDIR}/ld-linux-x86-64.so.2"),
        to: LOADER.to_string(),
    }];
    for lib in [
        "libc.so.6",
        "libm.so.6",
        "libdl.so.2",
        "libpthread.so.0",
        "librt.so.1",
        // Official Ollama (and other glibc Go binaries) need these; without
        // them the loader fails on `libresolv.so.2` and users reach for
        // Alpine `gcompat`, which overwrites `/lib64/ld-linux-x86-64.so.2`
        // and then dies on `fcntl64`.
        "libresolv.so.2",
        "libnss_dns.so.2",
        "libnss_files.so.2",
    ] {
        libc.push(same(&format!("{LIBDIR}/{lib}")));
    }
    let libs = ["libnvidia-ml", "libcuda", "libnvidia-ptxjitcompiler"];
    let mut links = Vec::new();
    for lib in libs {
        links.push((
            format!("{LIBDIR}/{lib}.so.1"),
            format!("{lib}.so.{version}"),
        ));
    }
    links.push((format!("{LIBDIR}/libcuda.so"), "libcuda.so.1".to_string()));
    Plan {
        runtime: vec![
            (glibc, libc),
            (libgcc, vec![same(&format!("{LIBDIR}/libgcc_s.so.1"))]),
        ],
        nvidia: vec![
            (
                nvidia("libnvidia-compute"),
                libs.iter()
                    .map(|lib| same(&format!("{LIBDIR}/{lib}.so.{version}")))
                    .collect(),
            ),
            (nvidia("nvidia-utils"), vec![same("usr/bin/nvidia-smi")]),
        ],
        links,
        programs: vec!["usr/bin/nvidia-smi".to_string()],
    }
}

/// The members of a `!<arch>` archive, which is what a `.deb` is, as
/// `(name, contents)`; `None` if it is not one or is cut short.
fn ar_members(bytes: &[u8]) -> Option<Vec<(&str, &[u8])>> {
    if bytes.get(..8)? != b"!<arch>\n" {
        return None;
    }
    let mut members = Vec::new();
    let mut at = 8;
    while at < bytes.len() {
        let header = bytes.get(at..at + 60)?;
        if &header[58..] != b"`\n" {
            return None;
        }
        let name = std::str::from_utf8(&header[..16]).ok()?.trim_end();
        let size: usize = std::str::from_utf8(&header[48..58])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let start = at + 60;
        members.push((
            name.trim_end_matches('/'),
            bytes.get(start..start.checked_add(size)?)?,
        ));
        // Each member starts on an even offset.
        at = start + size + size % 2;
    }
    Some(members)
}

/// The amd64 `.deb`s a pool directory's index page lists whose names start
/// with `prefix`, in name order.
fn listed_debs(html: &str, prefix: &str) -> Vec<String> {
    let mut found: Vec<String> = html
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|name| {
            name.starts_with(prefix) && name.ends_with("_amd64.deb") && !name.contains('/')
        })
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The file's SHA-256, by `sha256sum`.
fn sha256_of(path: &Path) -> Option<String> {
    let out = Command::new("sha256sum").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.split_whitespace().next()?.to_string())
}

fn wget(url: &str, out: &Path) -> bool {
    let tmp = out.with_extension("part");
    let ok = Command::new("wget")
        .arg("-q")
        .arg(url)
        .arg("-O")
        .arg(&tmp)
        .status()
        .is_ok_and(|s| s.success())
        && fs::rename(&tmp, out).is_ok();
    let _ = fs::remove_file(&tmp);
    ok
}

/// Fetches a package from the archive into `cache`, or says why not.
fn download(deb: &Deb, cache: &Path) -> Result<(), String> {
    let base = format!("{POOL}/{}", deb.pool());
    let file = match deb {
        Deb::Pinned { file, .. } => file.to_string(),
        Deb::Release { prefix, .. } => {
            let index = cache.join("index.html");
            if !wget(&format!("{base}/"), &index) {
                return Err(format!("could not list {base}/"));
            }
            let html = fs::read_to_string(&index).unwrap_or_default();
            let _ = fs::remove_file(&index);
            listed_debs(&html, prefix)
                .pop()
                .ok_or_else(|| format!("{base}/ has no {prefix}*_amd64.deb"))?
        }
    };
    println!("Downloading {file}...");
    if wget(&format!("{base}/{file}"), &cache.join(&file)) {
        Ok(())
    } else {
        Err(format!("could not download {base}/{file}"))
    }
}

/// The cached `.deb` for `deb`, if there is one to use.
fn cached(deb: &Deb, cache: &Path) -> Result<Option<PathBuf>, String> {
    match deb {
        Deb::Pinned { file, sha256, .. } => {
            let path = cache.join(file);
            if !path.is_file() {
                return Ok(None);
            }
            match sha256_of(&path) {
                Some(sum) if sum == *sha256 => Ok(Some(path)),
                // Cut short, or not the file: fetched again.
                Some(sum) => {
                    println!("  {file} has SHA-256 {sum}, not {sha256}: discarded");
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
                None => Err(format!("could not run sha256sum on {file}")),
            }
        }
        Deb::Release { prefix, .. } => {
            let names: Vec<String> = fs::read_dir(cache)
                .map(|dir| {
                    dir.filter_map(|e| e.ok()?.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            let listing: String = names.iter().map(|n| format!("href=\"{n}\"")).collect();
            Ok(listed_debs(&listing, prefix).pop().map(|f| cache.join(f)))
        }
    }
}

/// Unpacks `takes` out of the `.deb` at `deb` into `stage`, by their paths in
/// the package.
fn unpack(deb: &Path, takes: &[Take], stage: &Path) -> Result<(), String> {
    let name = deb.display();
    let bytes = fs::read(deb).map_err(|e| format!("{name}: {e}"))?;
    let members = ar_members(&bytes).ok_or_else(|| format!("{name} is not a .deb"))?;
    let (_, data) = members
        .iter()
        .find(|(member, _)| member.starts_with("data.tar"))
        .ok_or_else(|| format!("{name} has no data.tar"))?;
    // Not by the member's name, which the package chooses. GNU tar finds the
    // compression from the contents.
    let tarball = stage.join("package-data");
    fs::write(&tarball, data).map_err(|e| format!("{}: {e}", tarball.display()))?;
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(stage)
        .args(takes.iter().map(|take| format!("./{}", take.from)))
        .status();
    let _ = fs::remove_file(&tarball);
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!(
            "could not unpack {name} (GNU tar with zstd is needed)"
        )),
    }
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_: &str, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("the rootfs is only built on unix"))
}

/// Puts `plan` into `rootfs`, taking each package from `cache` or, if it is
/// not there, from `fetch`. Nothing is installed unless every file is there:
/// half a glibc, or NVML without the program that uses it, is worse than
/// none.
fn install_with(
    rootfs: &Path,
    cache: &Path,
    version: &str,
    plan: &Plan,
    fetch: &dyn Fn(&Deb, &Path) -> Result<(), String>,
) -> Result<(), String> {
    fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let stage = cache.join(format!("stage-{version}"));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let result = install_staged(rootfs, cache, &stage, version, plan, fetch);
    let _ = fs::remove_dir_all(&stage);
    result
}

/// `install_with`, unpacking into `stage` first.
fn install_staged(
    rootfs: &Path,
    cache: &Path,
    stage: &Path,
    version: &str,
    plan: &Plan,
    fetch: &dyn Fn(&Deb, &Path) -> Result<(), String>,
) -> Result<(), String> {
    // A rootfs that already has a glibc keeps it whole: its loader with our
    // libc, or the other way round, runs nothing.
    let runtime = !rootfs.join(LOADER).exists();
    let packages = plan
        .runtime
        .iter()
        .filter(|_| runtime)
        .chain(plan.nvidia.iter());
    let mut takes = Vec::new();
    for (deb, files) in packages {
        let path = match cached(deb, cache)? {
            Some(path) => path,
            None => {
                fetch(deb, cache)?;
                cached(deb, cache)?.ok_or("the download is not where it should be")?
            }
        };
        unpack(&path, files, stage)?;
        takes.extend(files.iter().cloned());
    }
    for program in &plan.programs {
        let bytes = fs::read(stage.join(program)).unwrap_or_default();
        if !bytes
            .windows(version.len())
            .any(|w| w == version.as_bytes())
        {
            return Err(format!("{program} is not the {version} build"));
        }
    }

    for take in &takes {
        let to = rootfs.join(&take.to);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let _ = fs::remove_file(&to);
        fs::copy(stage.join(&take.from), &to).map_err(|e| format!("{}: {e}", to.display()))?;
    }
    for (link, target) in &plan.links {
        let link = rootfs.join(link);
        let _ = fs::remove_file(&link);
        symlink(target, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    if !runtime {
        println!("  the rootfs already has a glibc loader: kept, with its libraries");
    }
    Ok(())
}

/// Takes NVIDIA's files and links out of `root`, and says how many there were.
/// The glibc stays: it is small, and the loader may be another stack's.
fn remove_with(root: &Path, plan: &Plan) -> usize {
    plan.nvidia
        .iter()
        .flat_map(|(_, takes)| takes.iter().map(|take| take.to.as_str()))
        .chain(plan.links.iter().map(|(link, _)| link.as_str()))
        .filter(|path| fs::remove_file(root.join(path)).is_ok())
        .count()
}

/// `remove_with` for this RM's version, saying why when anything goes.
pub(super) fn remove(root: &Path, why: &str) {
    let Some(version) = super::nvidia_firmware::pinned_rm_version() else {
        return;
    };
    let probe = fs::remove_file(root.join(super::cuda_probe::PATH)).is_ok() as usize
        + fs::remove_file(root.join(super::cuda_probe::STATUS_PATH)).is_ok() as usize
        + fs::remove_file(root.join(super::cuda_probe::INSTALL_OLLAMA_PATH)).is_ok() as usize
        + fs::remove_file(root.join(super::cuda_probe::ECLIPSE_OLLAMA_PATH)).is_ok() as usize;
    let removed = remove_with(root, &plan(&version)) + probe;
    if removed > 0 {
        println!(
            "  {}: took out NVIDIA's userspace ({removed} files): {why}",
            root.display()
        );
    }
}

/// Best-effort, like the GSP firmware: a failure is reported and the build
/// goes on without NVIDIA's userspace.
pub(super) fn install(rootfs: &Path) {
    let Some(version) = super::nvidia_firmware::pinned_rm_version() else {
        eprintln!(
            "warning: no NVIDIA_VERSION in the submodule's version.mk, so no NVIDIA \
             userspace"
        );
        return;
    };
    println!("Installing NVIDIA's userspace {version} (nvidia-smi, NVML, libcuda) and glibc...");
    let cache = PROJECT_DIR.join("ignored").join("nvidia-userspace-cache");
    match install_with(rootfs, &cache, &version, &plan(&version), &download) {
        Ok(()) => {
            super::cuda_probe::install(rootfs);
            println!("  done");
        }
        Err(why) => eprintln!(
            "warning: NVIDIA userspace: {why}; the image has none. A machine that \
             cannot reach {POOL} can be given the packages in {}, and {HATCH}=0 \
             leaves them out",
            cache.display()
        ),
    }
}

#[cfg(test)]
mod tests;
