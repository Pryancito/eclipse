//! Locale data: generate the locales the image declares, instead of declaring
//! one that is not there.
//!
//! The rootfs is built by unpacking `.deb`s with `dpkg-deb -x`
//! ([`super::apt_rootfs::extract_debs`]); no maintainer script ever runs. For
//! almost every package that is fine -- the files are the package. `locales`
//! is the exception: the `.deb` carries only the *sources* of a locale
//! (`/usr/share/i18n/locales/es_ES` plus a charmap), and the compiled form
//! glibc actually opens, `/usr/lib/locale/locale-archive`, is produced by the
//! postinst running `locale-gen`. Skip that step and the image ships the
//! recipe without the dish.
//!
//! What the guest showed (29-oct, Moebius's machine): `/etc/profile` exports
//! `LANG=es_ES.UTF-8`, `locale` prints every `LC_*` as `es_ES.UTF-8`, and
//! glibc answers each `setlocale` with
//!
//! ```text
//! locale: Cannot set LC_CTYPE to default locale: No such file or directory
//! warning: foot: invalid locale, falling back to 'C.UTF-8'
//! ```
//!
//! because the only thing under `/usr/lib/locale` was the `C.utf8` directory
//! that `libc-bin` ships prebuilt. Every program that asks for a named locale
//! -- foot, GTK, anything calling `strftime` or `gettext` -- silently drops to
//! `C`: English month names, `.` as the decimal separator, no translations.
//!
//! So this compiles them at build time. It is `locale-gen` done without a
//! chroot: the *guest's* `localedef` is run on the host through the guest's
//! own `ld.so`, so the archive is written by the same glibc that will read it.
//! Doing it with the host's `localedef` would be the subtle bug -- the archive
//! format is tied to the glibc that produced it, and the host here is 2.39
//! while the guest tap is 2.44.
//!
//! **This is glibc-only, and on purpose.** On the apt rootfs
//! ([`super::apt_rootfs::use_apt`]: x86_64 and aarch64) the C library is
//! Ubuntu's glibc and named locales mean something. On the apk rootfs
//! (riscv64) it is musl, which has no `locale-archive` and no `localedef`:
//! musl's `setlocale` accepts any name, treats everything as UTF-8 and has no
//! `LC_TIME`/`LC_MONETARY` data at all. There is nothing to generate there, so
//! this writes the configuration files and says so rather than pretending.

use super::apt_rootfs;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Default UI language of the image, the same one `/etc/profile` and
/// `eclipse-locale` export. Changing it here does not change those: it is the
/// locale that gets *generated*, and the two have to agree or we are back to
/// the bug this module exists for.
pub(super) const DEFAULT_LANG: &str = "es_ES.UTF-8";

/// `LANGUAGE` is a gettext-only list of message languages, tried in order. The
/// Spanish desktop falls back to English catalogs rather than to msgids.
pub(super) const DEFAULT_LANGUAGE: &str = "es:en";

/// The locales compiled into the image, as `(name, charmap)` pairs naming a
/// file in `/usr/share/i18n/locales` and one in `/usr/share/i18n/charmaps`.
///
/// Kept short on purpose: each one costs ~2.5 MiB in the archive, and Ubuntu's
/// `SUPPORTED` list has 500+ entries. `C.UTF-8` is not here because
/// `libc-bin` ships it already compiled under `/usr/lib/locale/C.utf8`.
pub(super) const WANTED: &[(&str, &str)] = &[("es_ES", "UTF-8"), ("en_US", "UTF-8")];

/// `/etc/locale.gen`, in the format Debian's `locale-gen` parses: one
/// `<name>.<charmap> <charmap>` line per generated locale, commented lines
/// ignored. We do not run `locale-gen` at boot, but the file is the place a
/// user looks to find out what this image generated, and the place they edit
/// before running `locale-gen` themselves inside the guest.
pub(super) fn locale_gen_body() -> String {
    let mut out = String::from(
        "# Locales compiled into this image at build time (xtask locales).\n\
         # Edit and run `locale-gen` in the guest to add more.\n",
    );
    for (name, charmap) in WANTED {
        out.push_str(&format!("{name}.{charmap} {charmap}\n"));
    }
    out
}

/// `/etc/locale.conf` (the systemd/`localectl` spelling) and
/// `/etc/default/locale` (the Debian spelling, what PAM's `pam_env` reads)
/// get the same body, so a guest cannot read two different answers.
pub(super) fn locale_conf_body() -> String {
    format!("LANG={DEFAULT_LANG}\nLANGUAGE={DEFAULT_LANGUAGE}\n")
}

/// Dynamic loader of the guest userspace, relative to the rootfs.
///
/// The guest binaries are not runnable by path here: their `PT_INTERP` is an
/// absolute `/lib64/...` that resolves to the *host's* loader, which would
/// then load the host's glibc. Invoking the loader explicitly with
/// `--library-path` is what pins both to the rootfs.
fn loader_rel(deb: &str) -> Option<&'static str> {
    match deb {
        "amd64" => Some("usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"),
        "arm64" => Some("usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1"),
        _ => None,
    }
}

/// Everything needed to run the guest's `localedef` for one locale: the
/// loader, its arguments, and the two environment variables that point glibc
/// at the rootfs's data instead of the host's.
pub(super) struct LocaledefRun {
    pub(super) loader: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) envs: Vec<(String, PathBuf)>,
}

/// Build the invocation for one locale.
///
/// Split out of [`generate_one`] so a test can check the wiring without a
/// 2.5 MiB rootfs: getting `I18NPATH` or `GCONV_PATH` wrong does not fail,
/// it quietly compiles against the host's charmaps.
pub(super) fn localedef_command(
    rootfs: &Path,
    deb: &str,
    name: &str,
    charmap: &str,
) -> Option<LocaledefRun> {
    let tuple = apt_rootfs::gnu_tuple(deb);
    let loader = rootfs.join(loader_rel(deb)?);
    let args = vec![
        "--library-path".to_string(),
        rootfs.join("usr/lib").join(tuple).display().to_string(),
        rootfs.join("usr/bin/localedef").display().to_string(),
        "-i".to_string(),
        name.to_string(),
        "-f".to_string(),
        charmap.to_string(),
        format!("--prefix={}", rootfs.display()),
        format!("{name}.{charmap}"),
    ];
    let envs = vec![
        ("I18NPATH".to_string(), rootfs.join("usr/share/i18n")),
        (
            "GCONV_PATH".to_string(),
            rootfs.join("usr/lib").join(tuple).join("gconv"),
        ),
    ];
    Some(LocaledefRun { loader, args, envs })
}

/// Whether the guest's `localedef` can run here at all: it is a guest-arch
/// ELF, so only a host of the same architecture executes it. Cross builds
/// (aarch64 image on an x86_64 host) cannot generate, and there is no
/// `qemu-user` in this tree to lean on.
fn host_can_run(arch: &str) -> bool {
    std::env::consts::ARCH == arch
}

/// Run the guest's `localedef` once. Returns the error line to report, if any.
fn generate_one(rootfs: &Path, deb: &str, name: &str, charmap: &str) -> Result<(), String> {
    let Some(run) = localedef_command(rootfs, deb, name, charmap) else {
        return Err(format!("no loader known for {deb}"));
    };
    let mut cmd = Command::new(&run.loader);
    cmd.args(&run.args);
    for (key, value) in &run.envs {
        cmd.env(key, value);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "localedef {name}.{charmap} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("localedef {name}.{charmap} did not start: {e}")),
    }
}

/// Write the configuration files and compile [`WANTED`] into
/// `/usr/lib/locale/locale-archive`.
///
/// Best-effort like the rest of the rootfs steps: it never panics and never
/// fails the build, but it is loud about every reason it could not generate,
/// because a silent skip here is exactly the failure mode it fixes.
pub(super) fn install(rootfs: &Path, arch: &str) {
    let etc = rootfs.join("etc");
    let _ = fs::create_dir_all(&etc);
    let _ = fs::write(etc.join("locale.gen"), locale_gen_body());
    let conf = locale_conf_body();
    let _ = fs::write(etc.join("locale.conf"), &conf);
    let _ = fs::create_dir_all(etc.join("default"));
    let _ = fs::write(etc.join("default/locale"), &conf);

    if !apt_rootfs::use_apt(arch) {
        println!(
            "locales: {arch} userspace is musl -- no locale-archive exists there. \
             Wrote /etc/locale.conf ({DEFAULT_LANG}); named locales stay UTF-8-but-C."
        );
        return;
    }
    let Some(deb) = apt_rootfs::deb_arch(arch) else {
        eprintln!("warning: locales: unsupported arch {arch}");
        return;
    };
    if !rootfs.join("usr/bin/localedef").is_file() {
        eprintln!(
            "warning: locales: no /usr/bin/localedef in the rootfs (the `locales` \
             package is in the desktop set only) -- {DEFAULT_LANG} will NOT resolve"
        );
        return;
    }
    if !rootfs.join("usr/share/i18n/locales").is_dir() {
        eprintln!(
            "warning: locales: no /usr/share/i18n/locales in the rootfs -- \
             {DEFAULT_LANG} will NOT resolve"
        );
        return;
    }
    if !host_can_run(arch) {
        eprintln!(
            "warning: locales: cannot generate {arch} locales on a {} host \
             (the guest localedef is a {arch} ELF and there is no qemu-user here) -- \
             {DEFAULT_LANG} will NOT resolve in this image",
            std::env::consts::ARCH
        );
        return;
    }

    let mut made = Vec::new();
    for (name, charmap) in WANTED {
        match generate_one(rootfs, deb, name, charmap) {
            Ok(()) => made.push(format!("{name}.{charmap}")),
            Err(msg) => eprintln!("warning: locales: {msg}"),
        }
    }
    let archive = rootfs.join("usr/lib/locale/locale-archive");
    let size = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    if made.is_empty() || size == 0 {
        eprintln!("warning: locales: no locale-archive written -- {DEFAULT_LANG} will NOT resolve");
        return;
    }
    println!(
        "locales: generated {} ({} KiB archive), LANG={DEFAULT_LANG}",
        made.join(", "),
        size / 1024
    );
}

#[cfg(test)]
mod tests;
