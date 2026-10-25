//! Ubuntu 26.10 (`stonking`) apt/dpkg userspace for the Eclipse rootfs.
//!
//! Replaces Alpine/apk as the package tap: Eclipse keeps its static busybox /
//! eclipse-init base; dynamic libraries and apps come from Ubuntu `.deb`s.
//! Bootstrap uses the host's `apt-get download` + `dpkg-deb -x` (no
//! mmdebstrap required) into a staging root, then merges selected trees into
//! the real rootfs — the same throwaway-staging idea as `xorg::install`.
//! Downloads are pinned to [`DISTRO`] via an isolated apt state dir (absolute
//! `Dir::*` paths): the host may still be 24.04/noble and must not resolve
//! package names from its own indexes.
//!
//! Knob: `ECLIPSE_PKG=apt|apk`. Default is **apt** on x86_64 and aarch64;
//! riscv64 stays on apk until ports/size allow (desktop closure already skipped).

use crate::PROJECT_DIR;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Ubuntu suite pin (must match `nvidia_userspace`'s libc6 pocket).
pub(super) const DISTRO: &str = "stonking";

/// Environment hatch.
pub(super) const HATCH: &str = "ECLIPSE_PKG";

/// Mozilla official APT repository signing key (fingerprint:
/// 35BA A0B3 3E9E B396 F59C A838 C0BA 5CE6 DC63 15A3, key ID: C0BA5CE6DC6315A3).
const MOZILLA_REPO_KEY_ASC: &str = "\
-----BEGIN PGP PUBLIC KEY BLOCK-----\n\
\n\
mQENBGCRt7MBCADkYJHHQQoL6tKrW/LbmfR9ljz7ib2aWno4JO3VKQvLwjyUMPpq\n\
/SXXMOnx8jXwgWizpPxQYDRJ0SQXS9ULJ1hXRL/OgMnZAYvYDeV2jBnKsAIEdiG/\n\
e1qm8P4W9qpWJc+hNq7FOT13RzGWRx57SdLWSXo0KeY38r9lvjjOmT/cuOcmjwlD\n\
T9XYf/RSO+yJ/AsyMdAr+ZbDeQUd9HYJiPdI04lGaGM02MjDMnx+monc+y54t+Z+\n\
ry1WtQdzoQt9dHlIPlV1tR+xV5DHHsejCZxu9TWzzSlL5wfBBeEz7R/OIzivGJpW\n\
QdJzd+2QDXSRg9q2XYWP5ZVtSgjVVJjNlb6ZABEBAAG0VEFydGlmYWN0IFJlZ2lz\n\
dHJ5IFJlcG9zaXRvcnkgU2lnbmVyIDxhcnRpZmFjdC1yZWdpc3RyeS1yZXBvc2l0\n\
b3J5LXNpZ25lckBnb29nbGUuY29tPokBTgQTAQoAOBYhBDW6oLM+nrOW9ZyoOMC6\n\
XObcYxWjBQJgkbezAhsDBQsJCAcCBhUKCQgLAgQWAgMBAh4BAheAAAoJEMC6XObc\n\
YxWj+igIAMFh6DrAYMeq9sbZ1ZG6oAMrinUheGQbEqe76nIDQNsZnhDwZ2wWqgVC\n\
7DgOMqlhQmOmzm7M6Nzmq2dvPwq3xC2OeI9fQyzjT72deBTzLP7PJok9PJFOMdLf\n\
ILSsUnmMsheQt4DUO0jYAX2KUuWOIXXJaZ319QyoRNBPYa5qz7qXS7wHLOY89IDq\n\
fHt6Aud8ER5zhyOyhytcYMeaGC1g1IKWmgewnhEq02FantMJGlmmFi2eA0EPD02G\n\
C3742QGqRxLwjWsm5/TpyuU24EYKRGCRm7QdVIo3ugFSetKrn0byOxWGBvtu4fH8\n\
XWvZkRT+u+yzH1s5yFYBqc2JTrrJvRU=\n\
=zwB2\n\
-----END PGP PUBLIC KEY BLOCK-----\n";

const MOZILLA_REPO_KEY_GPG: &[u8] = &[
    0x99, 0x01, 0x0d, 0x04, 0x60, 0x91, 0xb7, 0xb3, 0x01, 0x08, 0x00, 0xe4, 0x60, 0x91, 0xc7, 0x41,
    0x0a, 0x0b, 0xea, 0xd2, 0xab, 0x5b, 0xf2, 0xdb, 0x99, 0xf4, 0x7d, 0x96, 0x3c, 0xfb, 0x89, 0xbd,
    0x9a, 0x5a, 0x7a, 0x38, 0x24, 0xed, 0xd5, 0x29, 0x0b, 0xcb, 0xc2, 0x3c, 0x94, 0x30, 0xfa, 0x6a,
    0xfd, 0x25, 0xd7, 0x30, 0xe9, 0xf1, 0xf2, 0x35, 0xf0, 0x81, 0x68, 0xb3, 0xa4, 0xfc, 0x50, 0x60,
    0x34, 0x49, 0xd1, 0x24, 0x17, 0x4b, 0xd5, 0x0b, 0x27, 0x58, 0x57, 0x44, 0xbf, 0xce, 0x80, 0xc9,
    0xd9, 0x01, 0x8b, 0xd8, 0x0d, 0xe5, 0x76, 0x8c, 0x19, 0xca, 0xb0, 0x02, 0x04, 0x76, 0x21, 0xbf,
    0x7b, 0x5a, 0xa6, 0xf0, 0xfe, 0x16, 0xf6, 0xaa, 0x56, 0x25, 0xcf, 0xa1, 0x36, 0xae, 0xc5, 0x39,
    0x3d, 0x77, 0x47, 0x31, 0x96, 0x47, 0x1e, 0x7b, 0x49, 0xd2, 0xd6, 0x49, 0x7a, 0x34, 0x29, 0xe6,
    0x37, 0xf2, 0xbf, 0x65, 0xbe, 0x38, 0xce, 0x99, 0x3f, 0xdc, 0xb8, 0xe7, 0x26, 0x8f, 0x09, 0x43,
    0x4f, 0xd5, 0xd8, 0x7f, 0xf4, 0x52, 0x3b, 0xec, 0x89, 0xfc, 0x0b, 0x32, 0x31, 0xd0, 0x2b, 0xf9,
    0x96, 0xc3, 0x79, 0x05, 0x1d, 0xf4, 0x76, 0x09, 0x88, 0xf7, 0x48, 0xd3, 0x89, 0x46, 0x68, 0x63,
    0x34, 0xd8, 0xc8, 0xc3, 0x32, 0x7c, 0x7e, 0x9a, 0x89, 0xdc, 0xfb, 0x2e, 0x78, 0xb7, 0xe6, 0x7e,
    0xaf, 0x2d, 0x56, 0xb5, 0x07, 0x73, 0xa1, 0x0b, 0x7d, 0x74, 0x79, 0x48, 0x3e, 0x55, 0x75, 0xb5,
    0x1f, 0xb1, 0x57, 0x90, 0xc7, 0x1e, 0xc7, 0xa3, 0x09, 0x9c, 0x6e, 0xf5, 0x35, 0xb3, 0xcd, 0x29,
    0x4b, 0xe7, 0x07, 0xc1, 0x05, 0xe1, 0x33, 0xed, 0x1f, 0xce, 0x23, 0x38, 0xaf, 0x18, 0x9a, 0x56,
    0x41, 0xd2, 0x73, 0x77, 0xed, 0x90, 0x0d, 0x74, 0x91, 0x83, 0xda, 0xb6, 0x5d, 0x85, 0x8f, 0xe5,
    0x95, 0x6d, 0x4a, 0x08, 0xd5, 0x54, 0x98, 0xcd, 0x95, 0xbe, 0x99, 0x00, 0x11, 0x01, 0x00, 0x01,
    0xb4, 0x54, 0x41, 0x72, 0x74, 0x69, 0x66, 0x61, 0x63, 0x74, 0x20, 0x52, 0x65, 0x67, 0x69, 0x73,
    0x74, 0x72, 0x79, 0x20, 0x52, 0x65, 0x70, 0x6f, 0x73, 0x69, 0x74, 0x6f, 0x72, 0x79, 0x20, 0x53,
    0x69, 0x67, 0x6e, 0x65, 0x72, 0x20, 0x3c, 0x61, 0x72, 0x74, 0x69, 0x66, 0x61, 0x63, 0x74, 0x2d,
    0x72, 0x65, 0x67, 0x69, 0x73, 0x74, 0x72, 0x79, 0x2d, 0x72, 0x65, 0x70, 0x6f, 0x73, 0x69, 0x74,
    0x6f, 0x72, 0x79, 0x2d, 0x73, 0x69, 0x67, 0x6e, 0x65, 0x72, 0x40, 0x67, 0x6f, 0x6f, 0x67, 0x6c,
    0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x3e, 0x89, 0x01, 0x4e, 0x04, 0x13, 0x01, 0x0a, 0x00, 0x38, 0x16,
    0x21, 0x04, 0x35, 0xba, 0xa0, 0xb3, 0x3e, 0x9e, 0xb3, 0x96, 0xf5, 0x9c, 0xa8, 0x38, 0xc0, 0xba,
    0x5c, 0xe6, 0xdc, 0x63, 0x15, 0xa3, 0x05, 0x02, 0x60, 0x91, 0xb7, 0xb3, 0x02, 0x1b, 0x03, 0x05,
    0x0b, 0x09, 0x08, 0x07, 0x02, 0x06, 0x15, 0x0a, 0x09, 0x08, 0x0b, 0x02, 0x04, 0x16, 0x02, 0x03,
    0x01, 0x02, 0x1e, 0x01, 0x02, 0x17, 0x80, 0x00, 0x0a, 0x09, 0x10, 0xc0, 0xba, 0x5c, 0xe6, 0xdc,
    0x63, 0x15, 0xa3, 0xfa, 0x28, 0x08, 0x00, 0xc1, 0x61, 0xe8, 0x3a, 0xc0, 0x60, 0xc7, 0xaa, 0xf6,
    0xc6, 0xd9, 0xd5, 0x91, 0xba, 0xa0, 0x03, 0x2b, 0x8a, 0x75, 0x21, 0x78, 0x64, 0x1b, 0x12, 0xa7,
    0xbb, 0xea, 0x72, 0x03, 0x40, 0xdb, 0x19, 0x9e, 0x10, 0xf0, 0x67, 0x6c, 0x16, 0xaa, 0x05, 0x42,
    0xec, 0x38, 0x0e, 0x32, 0xa9, 0x61, 0x42, 0x63, 0xa6, 0xce, 0x6e, 0xcc, 0xe8, 0xdc, 0xe6, 0xab,
    0x67, 0x6f, 0x3f, 0x0a, 0xb7, 0xc4, 0x2d, 0x8e, 0x78, 0x8f, 0x5f, 0x43, 0x2c, 0xe3, 0x4f, 0xbd,
    0x9d, 0x78, 0x14, 0xf3, 0x2c, 0xfe, 0xcf, 0x26, 0x89, 0x3d, 0x3c, 0x91, 0x4e, 0x31, 0xd2, 0xdf,
    0x20, 0xb4, 0xac, 0x52, 0x79, 0x8c, 0xb2, 0x17, 0x90, 0xb7, 0x80, 0xd4, 0x3b, 0x48, 0xd8, 0x01,
    0x7d, 0x8a, 0x52, 0xe5, 0x8e, 0x21, 0x75, 0xc9, 0x69, 0x9d, 0xf5, 0xf5, 0x0c, 0xa8, 0x44, 0xd0,
    0x4f, 0x61, 0xae, 0x6a, 0xcf, 0xba, 0x97, 0x4b, 0xbc, 0x07, 0x2c, 0xe6, 0x3c, 0xf4, 0x80, 0xea,
    0x7c, 0x7b, 0x7a, 0x02, 0xe7, 0x7c, 0x11, 0x1e, 0x73, 0x87, 0x23, 0xb2, 0x87, 0x2b, 0x5c, 0x60,
    0xc7, 0x9a, 0x18, 0x2d, 0x60, 0xd4, 0x82, 0x96, 0x9a, 0x07, 0xb0, 0x9e, 0x11, 0x2a, 0xd3, 0x61,
    0x5a, 0x9e, 0xd3, 0x09, 0x1a, 0x59, 0xa6, 0x16, 0x2d, 0x9e, 0x03, 0x41, 0x0f, 0x0f, 0x4d, 0x86,
    0x0b, 0x7e, 0xf8, 0xd9, 0x01, 0xaa, 0x47, 0x12, 0xf0, 0x8d, 0x6b, 0x26, 0xe7, 0xf4, 0xe9, 0xca,
    0xe5, 0x36, 0xe0, 0x46, 0x0a, 0x44, 0x60, 0x91, 0x9b, 0xb4, 0x1d, 0x54, 0x8a, 0x37, 0xba, 0x01,
    0x52, 0x7a, 0xd2, 0xab, 0x9f, 0x46, 0xf2, 0x3b, 0x15, 0x86, 0x06, 0xfb, 0x6e, 0xe1, 0xf1, 0xfc,
    0x5d, 0x6b, 0xd9, 0x91, 0x14, 0xfe, 0xbb, 0xec, 0xb3, 0x1f, 0x5b, 0x39, 0xc8, 0x56, 0x01, 0xa9,
    0xcd, 0x89, 0x4e, 0xba, 0xc9, 0xbd, 0x15,
];

/// Mirror for amd64; arm64 uses ports.
fn mirror(arch_deb: &str) -> &'static str {
    if arch_deb == "amd64" {
        "http://archive.ubuntu.com/ubuntu"
    } else {
        "http://ports.ubuntu.com/ubuntu-ports"
    }
}

/// Eclipse arch name → Debian arch.
pub(super) fn deb_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Whether this build uses apt (vs apk).
pub(super) fn use_apt(arch: &str) -> bool {
    match std::env::var(HATCH) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "apt" => true,
            "apk" => false,
            other => {
                eprintln!(
                    "warning: {HATCH}={other:?} unknown; expected apt|apk — using default for {arch}"
                );
                default_apt(arch)
            }
        },
        Err(_) => default_apt(arch),
    }
}

fn default_apt(arch: &str) -> bool {
    matches!(arch, "x86_64" | "aarch64")
}

pub(super) fn cache_dir(arch: &str) -> PathBuf {
    // Suite in the path: a host that previously cached noble `.deb`s would
    // otherwise satisfy `cache_has_deb_for("labwc")` with an old binary that
    // wants `libwlroots.so.12` while stonking labwc wants `libwlroots-0.20`.
    PROJECT_DIR
        .join("ignored")
        .join("apt-cache")
        .join(DISTRO)
        .join(arch)
}

/// Isolated apt state under the download cache so host indexes (e.g. Pop/noble)
/// never decide what `apt-get download` / `apt-cache` resolve. Without this,
/// a host on 24.04 cannot fetch `libapt-pkg7.0` from Ubuntu 26.10.
fn apt_state_dir(cache: &Path) -> PathBuf {
    cache.join(".apt")
}

fn write_suite_sources(cache: &Path, deb: &str) -> PathBuf {
    let state = apt_state_dir(cache);
    let etc = state.join("etc");
    let _ = fs::create_dir_all(etc.join("sourceparts"));
    let _ = fs::create_dir_all(etc.join("preferences.d"));
    let _ = fs::create_dir_all(state.join("lists/partial"));
    let _ = fs::create_dir_all(state.join("cache/archives/partial"));
    // Empty dpkg status: without this, apt-get download picks the host's
    // installed Pop!_OS versions (…~24.04~pop0…) which are not in the
    // Ubuntu archive and fail with "no source to download version".
    let dpkg = state.join("dpkg");
    let _ = fs::create_dir_all(&dpkg);
    let status = dpkg.join("status");
    if !status.exists() {
        let _ = fs::write(&status, "");
    }
    let _ = fs::write(dpkg.join("arch"), format!("{deb}\n"));
    let sources = etc.join("sources.list");
    let keyrings = etc.join("keyrings");
    let _ = fs::create_dir_all(&keyrings);
    let key_asc = keyrings.join("packages.mozilla.org.asc");
    let key_gpg = keyrings.join("packages.mozilla.org.gpg");
    let _ = fs::write(&key_asc, MOZILLA_REPO_KEY_ASC);
    let _ = fs::write(&key_gpg, MOZILLA_REPO_KEY_GPG);
    let trusted_d = state.join("etc/trusted.gpg.d");
    let _ = fs::create_dir_all(&trusted_d);
    let _ = fs::write(
        trusted_d.join("packages.mozilla.org.gpg"),
        MOZILLA_REPO_KEY_GPG,
    );

    let m = mirror(deb);
    // `trusted=yes`: host may lack the Ubuntu archive keyring for this suite;
    // we only extract with dpkg-deb -x, same trust model as before.
    //
    // Mozilla's apt repo: Ubuntu's `firefox` in main is a snap transitional
    // stub (`1snap1`) with no browser binary — useless for Eclipse. The
    // real `.deb` ships from packages.mozilla.org, signed with Mozilla's key.
    let key_path = abs(&key_asc);
    let body = format!(
        "deb [arch={deb} trusted=yes] {m} {DISTRO} main universe\n\
         deb [arch={deb} trusted=yes] {m} {DISTRO}-updates main universe\n\
         deb [arch={deb} trusted=yes] {m} {DISTRO}-security main universe\n\
         deb [arch={deb} trusted=yes signed-by={}] https://packages.mozilla.org/apt mozilla main\n",
        key_path.display()
    );
    // Prefer Mozilla firefox over the Ubuntu snap stub (priority 500).
    let pin = state.join("etc/preferences.d/mozilla-firefox");
    let _ = fs::write(
        &pin,
        "Package: firefox*\n\
         Pin: origin packages.mozilla.org\n\
         Pin-Priority: 1001\n",
    );
    let prev = fs::read_to_string(&sources).unwrap_or_default();
    if prev != body {
        let _ = fs::write(&sources, &body);
        // Force re-index when the suite pin / mirrors change.
        let _ = fs::remove_dir_all(state.join("lists"));
        let _ = fs::create_dir_all(state.join("lists/partial"));
        // Drop cached `.deb`s so a Pop host build or the Ubuntu snap stub
        // cannot satisfy `cache_has_deb_for("firefox")` forever.
        if let Ok(entries) = fs::read_dir(cache) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("deb") {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
    sources
}

/// Absolutize a path for apt `-o Dir::…=` values. Relative paths are resolved
/// under `/etc/apt` or `/var/lib/apt` by apt itself — which is how a host on
/// noble silently ignored our stonking sources.list.
fn abs(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// `-o` flags that pin every apt/apt-cache invocation to [`DISTRO`].
fn apt_suite_opts(cache: &Path, deb: &str) -> Vec<String> {
    let state = abs(&apt_state_dir(cache));
    let sources = abs(&write_suite_sources(cache, deb));
    vec![
        format!("Dir::Etc::sourcelist={}", sources.display()),
        format!(
            "Dir::Etc::sourceparts={}",
            state.join("etc/sourceparts").display()
        ),
        format!(
            "Dir::Etc::preferencesparts={}",
            state.join("etc/preferences.d").display()
        ),
        format!("Dir::State::lists={}", state.join("lists").display()),
        // Critical on Pop/Ubuntu hosts: default status is /var/lib/dpkg/status,
        // so candidates become the host's Pop rebuilds (…pop0~24.04…) that the
        // Ubuntu archive cannot fetch.
        format!("Dir::State::status={}", state.join("dpkg/status").display()),
        format!("Dir::Cache={}", state.join("cache").display()),
        format!("APT::Architecture={deb}"),
        format!("APT::Architectures::={deb}"),
        "Acquire::Languages=none".into(),
        "APT::Get::AllowUnauthenticated=true".into(),
    ]
}

fn apt_cmd(bin: &str, cache: &Path, deb: &str) -> Command {
    let mut cmd = Command::new(bin);
    // apt-cache localises "Depends:" → "Depende:" etc.; our parser keys on the
    // English labels. Without C locale we also swallow every alternative of a
    // virtual package (all musescore-* soundfonts) as if they were hard deps.
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");
    for o in apt_suite_opts(cache, deb) {
        cmd.arg("-o").arg(o);
    }
    cmd
}

/// Refresh [`DISTRO`] package indexes into the isolated list dir (once per
/// build when missing/stale). Returns false if update fails (offline, …).
fn ensure_suite_index(cache: &Path, deb: &str) -> bool {
    let _ = write_suite_sources(cache, deb);
    let lists = apt_state_dir(cache).join("lists");
    let has_packages = fs::read_dir(&lists)
        .ok()
        .map(|it| {
            it.flatten()
                .any(|e| e.file_name().to_string_lossy().contains("_Packages"))
        })
        .unwrap_or(false);
    if has_packages {
        return true;
    }
    println!("apt: updating {DISTRO} package indexes ({deb})…");
    let status = apt_cmd("apt-get", cache, deb)
        .arg("-qq")
        .arg("update")
        .status();
    match status {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!(
                "warning: apt-get update for Ubuntu {DISTRO} failed (exit {})",
                s.code().unwrap_or(-1)
            );
            false
        }
        Err(e) => {
            eprintln!("warning: apt-get update for Ubuntu {DISTRO}: {e}");
            false
        }
    }
}

/// Packages needed so the guest can run `apt-get` / `dpkg` (plus libc).
const BOOTSTRAP_PKGS: &[&str] = &[
    "dpkg",
    "apt",
    "apt-utils",
    "gpgv",
    "ubuntu-keyring",
    "ca-certificates",
    "libapt-pkg7.0",
    "libstdc++6",
    "libgcc-s1",
    "libc6",
    "libseccomp2",
    "libsystemd0",
    "libgcrypt20",
    "libgpg-error0",
    "liblz4-1",
    "libzstd1",
    "libxxhash0",
    "libbz2-1.0",
    "liblzma5",
    "zlib1g",
    "libselinux1",
    "libpcre2-8-0",
    "libmd0",
    "libcap2",
    "libcrypt1",
    "libudev1",
    "libgssapi-krb5-2",
    "libkrb5-3",
    "libk5crypto3",
    "libkrb5support0",
    "libkeyutils1",
    "libcom-err2",
    // Ubuntu 26.10: apt/gnu-coreutils link libssl4; keep t64 for older deps.
    "libssl4",
    "libssl3t64",
    "libgnutls30t64",
    "libhogweed6t64",
    "libnettle8t64",
    "libgmp10",
    "libidn2-0",
    "libunistring5",
    "libtasn1-6",
    "libp11-kit0",
    "libffi8",
    "openssl",
    "adduser",
    "passwd",
    "libaudit1",
    "libpam0g",
    "libpam-modules",
    "debianutils",
    "bash",
    // 26.10 splits GNU coreutils out of the `coreutils` meta (which prefers
    // uutils). Without these, `/usr/bin/cp` is a dynamic GNU binary missing
    // its Pre-Depends — classic `libattr.so.1: cannot open shared object`.
    "gnu-coreutils",
    "coreutils-from-gnu",
    "libattr1",
    "libacl1",
    "tar",
    "gzip",
    "xz-utils",
    "findutils",
    "grep",
    "sed",
    "mawk",
    "libncurses6",
    "libtinfo6",
    "init-system-helpers",
    "netbase",
    "perl-base",
    // Smoke package: proves dpkg extract + merge left a runnable glibc binary.
    "hello",
];

/// Trees copied from an apt staging root into the Eclipse rootfs.
/// Never `/bin` or `/sbin` wholesale — busybox owns those applets.
const MERGE_TREES: &[&str] = &[
    "usr/bin",
    "usr/sbin",
    "usr/lib",
    "usr/lib64",
    "usr/libexec",
    "usr/share",
    "etc/alsa",
    "etc/apt",
    "etc/dpkg",
    "var/lib/dpkg",
    "var/cache/apt",
    "var/lib/apt",
    "lib",
];

/// Write `/etc/apt` + empty dpkg admin dir. Safe to call on every build.
pub(super) fn write_layout(root: &Path, arch: &str) {
    let Some(deb) = deb_arch(arch) else {
        eprintln!("warning: apt layout: unsupported arch {arch}");
        return;
    };
    let mirror = mirror(deb);
    let etc_apt = root.join("etc/apt");
    let _ = fs::create_dir_all(etc_apt.join("apt.conf.d"));
    let _ = fs::create_dir_all(etc_apt.join("preferences.d"));
    let _ = fs::create_dir_all(etc_apt.join("trusted.gpg.d"));
    let _ = fs::create_dir_all(etc_apt.join("sources.list.d"));
    let keyrings = etc_apt.join("keyrings");
    let _ = fs::create_dir_all(&keyrings);
    let _ = fs::write(
        keyrings.join("packages.mozilla.org.asc"),
        MOZILLA_REPO_KEY_ASC,
    );
    let _ = fs::write(
        keyrings.join("packages.mozilla.org.gpg"),
        MOZILLA_REPO_KEY_GPG,
    );
    let _ = fs::write(
        etc_apt.join("trusted.gpg.d/packages.mozilla.org.gpg"),
        MOZILLA_REPO_KEY_GPG,
    );
    let sources = format!(
        "deb [arch={deb} signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] {mirror} {DISTRO} main restricted universe multiverse\n\
         deb [arch={deb} signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] {mirror} {DISTRO}-updates main restricted universe multiverse\n\
         deb [arch={deb} signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] {mirror} {DISTRO}-security main restricted universe multiverse\n\
         deb [arch={deb} signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main\n"
    );
    let _ = fs::write(etc_apt.join("sources.list"), sources);
    let _ = fs::write(
        etc_apt.join("preferences.d/mozilla-firefox"),
        "Package: firefox*\n\
         Pin: origin packages.mozilla.org\n\
         Pin-Priority: 1001\n",
    );
    let _ = fs::write(
        etc_apt.join("apt.conf.d/00eclipse"),
        "APT::Install-Recommends \"false\";\n\
         APT::Install-Suggests \"false\";\n\
         APT::Sandbox::User \"root\";\n\
         Binary::apt::Pager \"cat\";\n\
         APT::Cache-Start 250000000;\n\
         APT::Cache-Grow 50000000;\n\
         APT::Cache-Limit 250000000;\n\
         APT::Get::Inhibit-Shutdown \"false\";\n",
    );

    let dpkg = root.join("var/lib/dpkg");
    let _ = fs::create_dir_all(dpkg.join("info"));
    let _ = fs::create_dir_all(dpkg.join("updates"));
    let _ = fs::create_dir_all(dpkg.join("triggers"));
    let status = dpkg.join("status");
    if !status.exists() {
        let _ = fs::write(&status, "");
    }
    let _ = fs::write(dpkg.join("available"), "");
    let _ = fs::create_dir_all(root.join("var/cache/apt/archives/partial"));
    let _ = fs::create_dir_all(root.join("var/lib/apt/lists/partial"));
    let _ = fs::write(root.join("var/lib/dpkg/arch"), format!("{deb}\n"));

    // Ubuntu stonking / merged-/usr (DEP17): /lib64 must be a symlink to usr/lib64.
    // If /lib64 is a real directory, libc6.preinst refuses to unpack.
    if arch == "x86_64" {
        let usr_lib64 = root.join("usr/lib64");
        let _ = fs::create_dir_all(&usr_lib64);
        let lib64 = root.join("lib64");
        if lib64.is_dir() && !lib64.is_symlink() {
            copy_tree(&lib64, &usr_lib64);
            let _ = fs::remove_dir_all(&lib64);
        }
        if !lib64.exists() && !lib64.is_symlink() {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink("usr/lib64", &lib64);
        }
    }

    println!(
        "apt layout: Ubuntu {DISTRO} ({deb}) → {}",
        etc_apt.join("sources.list").display()
    );
}

/// Bootstrap apt/dpkg + glibc into `root` (best-effort).
pub(super) fn bootstrap(root: &Path, arch: &str) {
    let Some(deb) = deb_arch(arch) else {
        return;
    };
    write_layout(root, arch);
    let cache = cache_dir(arch);
    let _ = fs::create_dir_all(&cache);
    let stage = PROJECT_DIR
        .join("ignored")
        .join(format!("apt-bootstrap-{arch}"));
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::create_dir_all(&stage);
    write_layout(&stage, arch);

    println!(
        "apt bootstrap: downloading {} essential package(s) for {deb}...",
        BOOTSTRAP_PKGS.len()
    );
    if !download_packages(&cache, deb, BOOTSTRAP_PKGS) {
        eprintln!(
            "warning: apt bootstrap: download failed (offline?). Guest may lack /usr/bin/apt."
        );
        let _ = fs::remove_dir_all(&stage);
        return;
    }
    let root_status = root.join("var/lib/dpkg/status");
    let stage_status = stage.join("var/lib/dpkg/status");
    if root_status.is_file() {
        let _ = fs::copy(&root_status, &stage_status);
    }
    if !extract_debs(&cache, BOOTSTRAP_PKGS, &stage) {
        eprintln!("warning: apt bootstrap: extract failed");
        let _ = fs::remove_dir_all(&stage);
        return;
    }
    merge_stage(&stage, root);
    // Prefer Ubuntu bash when present; busybox ash remains via applets.
    copy_one_bin(&stage, root, "bin/bash");
    copy_one_bin(&stage, root, "usr/bin/dpkg");
    copy_one_bin(&stage, root, "usr/bin/apt");
    copy_one_bin(&stage, root, "usr/bin/apt-get");
    copy_one_bin(&stage, root, "usr/bin/apt-cache");
    copy_one_bin(&stage, root, "usr/bin/gpgv");
    // Loader path every glibc ELF names.
    let loader_src = stage.join("usr/lib64/ld-linux-x86-64.so.2");
    let loader_alt = stage.join(format!("usr/lib/{}/ld-linux-x86-64.so.2", gnu_tuple(deb)));
    let loader_old = stage.join("lib64/ld-linux-x86-64.so.2");
    for src in [loader_src, loader_alt, loader_old] {
        if src.is_file() {
            let dst = root.join("usr/lib64/ld-linux-x86-64.so.2");
            let _ = fs::create_dir_all(root.join("usr/lib64"));
            let _ = fs::remove_file(&dst);
            let _ = fs::copy(&src, &dst);
            break;
        }
    }
    if deb == "arm64" {
        let src = stage.join("lib/ld-linux-aarch64.so.1");
        if src.is_file() {
            let _ = fs::create_dir_all(root.join("lib"));
            let _ = fs::copy(&src, root.join("lib/ld-linux-aarch64.so.1"));
        }
    }
    let _ = fs::remove_dir_all(&stage);
    fix_common_alternatives(root);
    let hello = root.join("usr/bin/hello").is_file() || root.join("bin/hello").is_file();
    if hello {
        println!("apt bootstrap: smoke OK — /usr/bin/hello present");
    } else {
        eprintln!(
            "warning: apt bootstrap: `hello` missing (offline download?). \
             apt/dpkg may still be present."
        );
    }
    println!(
        "apt bootstrap: merged Ubuntu {DISTRO} runtime into {}",
        root.display()
    );
}

/// Debian/Ubuntu `update-alternatives` entries that never appear because we
/// extract `.deb`s with `dpkg-deb -x` (no postinst). Without these, scripts
/// that call the generic name (`mpg123`, …) fail even though the real binary
/// is on disk.
fn fix_common_alternatives(root: &Path) {
    // mpg123: the package ships mpg123.bin + output wrappers; `/usr/bin/mpg123`
    // is normally an alternative. Prefer the real binary so callers can pass
    // their own `-o` without fighting a wrapper's preset.
    let mpg = root.join("usr/bin/mpg123");
    if !mpg.exists() {
        for name in ["mpg123.bin", "mpg123-pulse", "mpg123-alsa"] {
            let target = root.join("usr/bin").join(name);
            if target.is_file() {
                let _ = fs::remove_file(&mpg);
                if std::os::unix::fs::symlink(name, &mpg).is_ok() {
                    println!("apt alternatives: /usr/bin/mpg123 -> {name}");
                }
                break;
            }
        }
    }
    // seatd: Ubuntu puts the daemon in /usr/sbin; some scripts expect /usr/bin.
    let seatd_bin = root.join("usr/bin/seatd");
    let seatd_sbin = root.join("usr/sbin/seatd");
    if !seatd_bin.exists() && seatd_sbin.is_file() {
        let _ = fs::create_dir_all(root.join("usr/bin"));
        if std::os::unix::fs::symlink("../sbin/seatd", &seatd_bin).is_ok() {
            println!("apt alternatives: /usr/bin/seatd -> ../sbin/seatd");
        }
    }

    // pager / less / more: Debian/Ubuntu tools (and `apt search`) expect
    // `/usr/bin/pager` or `/usr/bin/less`. Point them to `/bin/less` (busybox) if missing.
    let usr_bin = root.join("usr/bin");
    let _ = fs::create_dir_all(&usr_bin);
    let pager = usr_bin.join("pager");
    if !pager.exists() {
        let _ = std::os::unix::fs::symlink("../../bin/less", &pager);
    }
    let less = usr_bin.join("less");
    if !less.exists() {
        let _ = std::os::unix::fs::symlink("../../bin/less", &less);
    }
    let more = usr_bin.join("more");
    if !more.exists() {
        let _ = std::os::unix::fs::symlink("../../bin/more", &more);
    }

    // Drop dummy systemd system-services with Exec=/bin/false. When systemd is
    // not PID 1, D-Bus tries to activate these via servicehelper, stalls for
    // 25 seconds waiting for the service to appear, then logs a timeout error.
    // Without these files, D-Bus replies ServiceUnknown immediately.
    let sys_services = root.join("usr/share/dbus-1/system-services");
    if let Ok(entries) = fs::read_dir(&sys_services) {
        for ent in entries.flatten() {
            let path = ent.path();
            if let Ok(content) = fs::read_to_string(&path) {
                if content.contains("Exec=/bin/false") {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

fn gnu_tuple(deb: &str) -> &'static str {
    match deb {
        "amd64" => "x86_64-linux-gnu",
        "arm64" => "aarch64-linux-gnu",
        _ => "x86_64-linux-gnu",
    }
}

fn copy_one_bin(stage: &Path, root: &Path, rel: &str) {
    let src = stage.join(rel);
    if !src.is_file() {
        return;
    }
    let dst = root.join(rel);
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::copy(&src, &dst);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
    }
}

/// Install the named Ubuntu packages into `root` via staging merge (best-effort).
pub(super) fn install_packages(root: &Path, arch: &str, packages: &[String]) {
    let Some(deb) = deb_arch(arch) else {
        eprintln!("warning: apt install: unsupported arch {arch}");
        return;
    };
    if packages.is_empty() {
        return;
    }
    write_layout(root, arch);
    let cache = cache_dir(arch);
    let _ = fs::create_dir_all(&cache);
    let stage = PROJECT_DIR.join("ignored").join(format!("apt-xorg-{arch}"));
    let _ = fs::remove_dir_all(&stage);
    let _ = fs::create_dir_all(&stage);
    write_layout(&stage, arch);

    // Flatten Depends, map virtual/t64 Provides to a real package, drop the rest.
    // apt-cache --recurse emits names like `libssl3` / `awk` that download cannot fetch.
    let raw = depends_recursive(&cache, deb, packages).unwrap_or_else(|| packages.to_vec());
    let (to_fetch, skipped_virtual): (Vec<String>, Vec<String>) = {
        let mut ok = Vec::new();
        let mut skip = Vec::new();
        for p in raw {
            match resolve_real_package(&cache, deb, &p) {
                Some(real) => ok.push(real),
                None => skip.push(p),
            }
        }
        ok.sort();
        ok.dedup();
        (ok, skip)
    };
    if !skipped_virtual.is_empty() {
        let preview: Vec<_> = skipped_virtual
            .iter()
            .take(12)
            .map(String::as_str)
            .collect();
        println!(
            "apt install: skipping {} virtual/unresolvable name(s) (e.g. {})",
            skipped_virtual.len(),
            preview.join(", ")
        );
    }
    if to_fetch.len() > 900 {
        eprintln!(
            "warning: apt install: dependency closure is {} pkgs — still fetching (may take a while)",
            to_fetch.len()
        );
    }

    println!(
        "apt install: {} top-level → {} downloadable package(s) for {deb}...",
        packages.len(),
        to_fetch.len()
    );
    let names: Vec<&str> = to_fetch.iter().map(String::as_str).collect();
    if names.is_empty() {
        eprintln!("warning: apt install: nothing downloadable");
        let _ = fs::remove_dir_all(&stage);
        return;
    }
    if !download_packages(&cache, deb, &names) {
        eprintln!("warning: apt install: download produced no .debs");
        let _ = fs::remove_dir_all(&stage);
        return;
    }
    let root_status = root.join("var/lib/dpkg/status");
    let stage_status = stage.join("var/lib/dpkg/status");
    if root_status.is_file() {
        let _ = fs::copy(&root_status, &stage_status);
    }
    // Extract every .deb in the cache that we just needed (filter by name list).
    if !extract_debs(&cache, &names, &stage) {
        eprintln!("warning: apt install: extract failed");
        let _ = fs::remove_dir_all(&stage);
        return;
    }
    merge_stage(&stage, root);
    for p in ["bin/bash", "usr/bin/bash"] {
        copy_one_bin(&stage, root, p);
    }
    fix_common_alternatives(root);
    let _ = fs::remove_dir_all(&stage);
    println!(
        "apt install: merged {} package tree(s) into {}",
        packages.len(),
        root.display()
    );
}

fn download_packages(cache: &Path, deb: &str, packages: &[&str]) -> bool {
    if packages.is_empty() {
        return true;
    }
    if !ensure_suite_index(cache, deb) {
        eprintln!(
            "warning: apt-get download: no {DISTRO} index (host is not the guest suite; \
             cannot resolve packages like libapt-pkg7.0 from the host's noble lists)"
        );
        return cache_has_any_deb(cache);
    }
    // One package per apt-get download: a single missing name must not abort
    // a batch of 40 real packages (that was flooding the log with false errors).
    let mut ok = 0usize;
    let mut fail = 0usize;
    for p in packages {
        // Drop truncated leftovers (Ctrl-C mid-download) before trusting cache.
        purge_corrupt_debs_for(cache, deb, p);
        if cache_has_deb_for(cache, deb, p) {
            ok += 1;
            continue;
        }
        // Prefer the suite candidate (`pkg/stonking`). Without the slash,
        // a host that still bleeds Pop status can ask for …pop0~24.04…
        // versions that Ubuntu's archive does not carry. Firefox comes from
        // Mozilla (pinned above) — bare name is correct there.
        let target = if *p == "firefox" {
            p.to_string()
        } else {
            format!("{p}/{DISTRO}")
        };
        let status = apt_cmd("apt-get", cache, deb)
            .arg("-qq")
            .arg("download")
            .arg(&target)
            .current_dir(cache)
            .status();
        match status {
            Ok(s) if s.success() => {
                purge_corrupt_debs_for(cache, deb, p);
                if cache_has_deb_for(cache, deb, p) {
                    ok += 1;
                } else {
                    fail += 1;
                }
            }
            _ => {
                // Fallback: some packages only exist in `-updates`/`universe`
                // without a clean suite target name.
                let retry = apt_cmd("apt-get", cache, deb)
                    .arg("-qq")
                    .arg("download")
                    .arg(p)
                    .current_dir(cache)
                    .status();
                match retry {
                    Ok(s) if s.success() => {
                        purge_corrupt_debs_for(cache, deb, p);
                        if cache_has_deb_for(cache, deb, p) {
                            ok += 1;
                        } else {
                            fail += 1;
                        }
                    }
                    _ => fail += 1,
                }
            }
        }
    }
    if fail > 0 {
        eprintln!("warning: apt-get download: {ok} ok, {fail} failed");
    } else {
        println!("apt-get download: {ok} package(s) ok");
    }
    ok > 0 || cache_has_any_deb(cache)
}

fn cached_debs_for(cache: &Path, pkg: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(cache) else {
        return Vec::new();
    };
    let prefix = format!("{pkg}_");
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".deb"))
                .unwrap_or(false)
        })
        .collect()
}

/// Version string embedded in a `.deb` file name (`pkg_1.2-3_amd64.deb` →
/// `1.2-3`). Handles the `%3a` epoch encoding apt uses in filenames.
fn version_from_deb_name(pkg: &str, file_name: &str) -> Option<String> {
    let prefix = format!("{pkg}_");
    let rest = file_name.strip_prefix(&prefix)?;
    let ver = rest.rsplit_once('_')?.0; // drop `_amd64.deb` / `_all.deb`
    let ver = ver.strip_suffix(".deb").unwrap_or(ver);
    Some(ver.replace("%3a", ":"))
}

/// `Size:` for a specific package version from `apt-cache show` output
/// (multi-stanza: Ubuntu stub + Mozilla firefox share the name).
fn size_from_apt_show(show: &str, version: &str) -> Option<u64> {
    let mut ver: Option<&str> = None;
    let mut size: Option<u64> = None;
    let flush = |ver: &mut Option<&str>, size: &mut Option<u64>| -> Option<u64> {
        if ver.as_ref().is_some_and(|v| *v == version) {
            return size.take();
        }
        *ver = None;
        *size = None;
        None
    };
    for line in show.lines() {
        if line.is_empty() {
            if let Some(s) = flush(&mut ver, &mut size) {
                return Some(s);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Version:") {
            if let Some(s) = flush(&mut ver, &mut size) {
                return Some(s);
            }
            ver = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("Size:") {
            size = rest.trim().parse().ok();
        }
    }
    flush(&mut ver, &mut size)
}

fn apt_show_text(cache: &Path, deb: &str, pkg: &str) -> Option<String> {
    let output = apt_cmd("apt-cache", cache, deb)
        .arg("show")
        .arg(pkg)
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `path` looks like a complete download of `pkg`.
///
/// Only `got < want` is treated as truncated. Comparing against the *first*
/// `Size:` from `apt-cache show` was wrong for firefox: Ubuntu's snap stub is
/// ~68 KiB while Mozilla's real `.deb` is ~90 MiB — we were deleting the good
/// file ("have 93MB, want 68KB").
fn deb_is_complete(cache: &Path, deb: &str, pkg: &str, path: &Path) -> bool {
    let got = match fs::metadata(path) {
        Ok(m) if m.len() > 0 => m.len(),
        _ => return false,
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let Some(ver) = version_from_deb_name(pkg, name) else {
        return got > 0;
    };
    let Some(show) = apt_show_text(cache, deb, pkg) else {
        return got > 0;
    };
    match size_from_apt_show(&show, &ver) {
        Some(want) => got >= want, // equal = complete; larger shouldn't happen
        None => got > 0,           // version not in index (pinned foreign) — keep
    }
}

/// Remove cached `.deb`s that are strictly smaller than their index `Size:`
/// (Ctrl-C / network drop). Never delete a larger file: that is usually a
/// different origin of the same name (Mozilla firefox vs Ubuntu stub).
fn purge_corrupt_debs_for(cache: &Path, deb: &str, pkg: &str) {
    let Some(show) = apt_show_text(cache, deb, pkg) else {
        return;
    };
    for path in cached_debs_for(cache, pkg) {
        let got = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if got == 0 {
            let _ = fs::remove_file(&path);
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(ver) = version_from_deb_name(pkg, name) else {
            continue;
        };
        let Some(want) = size_from_apt_show(&show, &ver) else {
            continue;
        };
        if got < want {
            eprintln!("apt cache: dropping truncated {name} (have {got} B, want {want} B)");
            let _ = fs::remove_file(&path);
        }
    }
}

fn cache_has_deb_for(cache: &Path, deb: &str, pkg: &str) -> bool {
    cached_debs_for(cache, pkg)
        .iter()
        .any(|p| deb_is_complete(cache, deb, pkg, p))
}

/// True if `apt-cache show` (against the guest suite index) returns a real package.
fn has_download_candidate(cache: &Path, deb: &str, pkg: &str) -> bool {
    if !ensure_suite_index(cache, deb) {
        return false;
    }
    let output = apt_cmd("apt-cache", cache, deb)
        .arg("show")
        .arg(pkg)
        .output();
    match output {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.contains("Package:") && text.contains("Version:")
        }
        _ => false,
    }
}

/// Hard renames that no longer `Provides:` the old name (Debian/Ubuntu
/// transitions). Mozilla's firefox still Depends on `libgdk-pixbuf2.0-0`;
/// `mime-support` became `media-types` with only Breaks/Replaces.
const PACKAGE_ALIASES: &[(&str, &str)] = &[
    ("libgdk-pixbuf2.0-0", "libgdk-pixbuf-2.0-0"),
    ("mime-support", "media-types"),
    ("libgcc1", "libgcc-s1"),
];

/// Candidates to try when `pkg` itself is not downloadable.
fn package_rename_candidates(pkg: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (from, to) in PACKAGE_ALIASES {
        if pkg == *from {
            out.push((*to).to_string());
        }
    }
    // t64 ABI rename: libssl3 → libssl3t64, libasound2 → libasound2t64
    if !pkg.ends_with("t64") {
        out.push(format!("{pkg}t64"));
    }
    // gdk-pixbuf style: libfoo2.0-0 → libfoo-2.0-0 (hyphen before the SONAME).
    if let Some(i) = pkg.find(|c: char| c.is_ascii_digit()) {
        if i > 0 && !pkg.as_bytes()[i - 1].is_ascii_digit() && pkg.as_bytes()[i - 1] != b'-' {
            let mut hyphenated = String::with_capacity(pkg.len() + 1);
            hyphenated.push_str(&pkg[..i]);
            hyphenated.push('-');
            hyphenated.push_str(&pkg[i..]);
            if hyphenated != pkg {
                out.push(hyphenated);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Map a Depends name to something `apt-get download` can fetch.
/// Handles Debian/Ubuntu t64 renames (`libssl3` → `libssl3t64`) via Reverse
/// Provides, plus hard renames that dropped the old Provides entirely.
fn resolve_real_package(cache: &Path, deb: &str, pkg: &str) -> Option<String> {
    if has_download_candidate(cache, deb, pkg) {
        return Some(pkg.to_string());
    }
    if let Some((_, prefer)) = VIRTUAL_PREFERS.iter().find(|(v, _)| *v == pkg) {
        if has_download_candidate(cache, deb, prefer) {
            return Some((*prefer).to_string());
        }
    }
    for alt in package_rename_candidates(pkg) {
        if has_download_candidate(cache, deb, &alt) {
            return Some(alt);
        }
    }
    let output = apt_cmd("apt-cache", cache, deb)
        .arg("showpkg")
        .arg(pkg)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut in_rev = false;
    for line in text.lines() {
        if line.starts_with("Reverse Provides:") {
            in_rev = true;
            continue;
        }
        if in_rev {
            if line.is_empty() || !line.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                break;
            }
            // "libglib2.0-0t64 2.80.0-6ubuntu3.10 (= ...)"
            let provider = line.split_whitespace().next()?;
            // Prefer the host/target arch; skip foreign "gawk:i386" style.
            if let Some((name, arch_tag)) = provider.split_once(':') {
                if arch_tag != deb {
                    continue;
                }
                if has_download_candidate(cache, deb, name) {
                    return Some(name.to_string());
                }
            } else if has_download_candidate(cache, deb, provider) {
                return Some(provider.to_string());
            }
        }
    }
    None
}

fn cache_has_any_deb(cache: &Path) -> bool {
    fs::read_dir(cache)
        .ok()
        .map(|it| {
            it.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("deb"))
        })
        .unwrap_or(false)
}

fn extract_debs(cache: &Path, packages: &[&str], stage: &Path) -> bool {
    let mut any = false;
    let Ok(entries) = fs::read_dir(cache) else {
        return false;
    };
    let debs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("deb"))
        .collect();
    // Per-.deb empty tmp: Ubuntu usrmerge ships `bin`→`usr/bin` symlinks. Extracting
    // two packages straight into the same stage fails with
    // "Cannot create symlink to 'usr/bin': File exists" once `bin` is already a
    // real directory (or the other way around). Unpack alone, then merge.
    let tmp = stage.join(".deb-extract-tmp");
    for deb in &debs {
        let name = deb
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !packages.is_empty() {
            let stem = name.split('_').next().unwrap_or("");
            let wanted = packages.iter().any(|p| {
                stem == *p
                    || stem.starts_with(&format!("{p}:"))
                    || name.starts_with(&format!("{p}_"))
            });
            if !wanted {
                continue;
            }
        }
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::create_dir_all(&tmp);
        let status = Command::new("dpkg-deb")
            .arg("-x")
            .arg(deb)
            .arg(&tmp)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!(
                "apt extract: dpkg-deb failed on {} — removing (likely truncated)",
                name
            );
            let _ = fs::remove_file(deb);
            continue;
        }
        merge_usrmerge_tree(&tmp, stage);
        record_deb_status(deb, &stage.join("var/lib/dpkg/status"));
        any = true;
    }
    let _ = fs::remove_dir_all(&tmp);
    any
}

fn record_deb_status(deb: &Path, status_file: &Path) {
    let output = match Command::new("dpkg-deb").arg("-f").arg(deb).output() {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pkg_name = None;
    let mut lines = Vec::new();
    let mut inserted_status = false;

    for line in text.lines() {
        if line.starts_with("Package: ") {
            let name = line["Package: ".len()..].trim();
            pkg_name = Some(name.to_string());
            lines.push(line.to_string());
            lines.push("Status: install ok installed".to_string());
            inserted_status = true;
            continue;
        }
        if line.starts_with("Status: ") {
            continue;
        }
        lines.push(line.to_string());
    }
    if !inserted_status {
        lines.push("Status: install ok installed".to_string());
    }
    let Some(pkg) = pkg_name else {
        return;
    };

    if let Ok(existing) = fs::read_to_string(status_file) {
        let needle = format!("Package: {pkg}\n");
        if existing.starts_with(&needle) || existing.contains(&format!("\n{needle}")) {
            return;
        }
    }

    if let Some(parent) = status_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(status_file)
    {
        let _ = writeln!(f, "{}", lines.join("\n"));
        let _ = writeln!(f);
    }
}

/// Copy one unpacked `.deb` into `stage`, flattening Ubuntu's merged-/usr layout.
///
/// Top-level `bin`/`sbin`/`lib`/`lib64` **symlinks** are skipped (their payload
/// lives under `usr/…`). If they are real directories (pre-usrmerge packages),
/// their contents are folded into `usr/bin`, `usr/sbin`, `usr/lib`, `usr/lib64`.
fn merge_usrmerge_tree(from: &Path, stage: &Path) {
    let Ok(entries) = fs::read_dir(from) else {
        return;
    };
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let src = ent.path();
        match name_str {
            "bin" | "sbin" | "lib" | "lib64" => {
                if src.is_symlink() {
                    // usrmerge marker — real files are under usr/ or already linked.
                    continue;
                }
                if src.is_dir() {
                    let dest = match name_str {
                        "bin" => stage.join("usr/bin"),
                        "sbin" => stage.join("usr/sbin"),
                        "lib" => stage.join("usr/lib"),
                        "lib64" => stage.join("usr/lib64"),
                        _ => unreachable!(),
                    };
                    copy_tree(&src, &dest);
                }
            }
            _ => {
                let dst = stage.join(name_str);
                if src.is_dir() && !src.is_symlink() {
                    copy_tree(&src, &dst);
                } else if src.is_symlink() {
                    #[cfg(unix)]
                    {
                        if let Ok(target) = fs::read_link(&src) {
                            // Don't replace an existing directory with a symlink.
                            if dst.is_dir() && !dst.is_symlink() {
                                continue;
                            }
                            let _ = fs::remove_file(&dst);
                            let _ = std::os::unix::fs::symlink(target, &dst);
                        }
                    }
                } else if src.is_file() {
                    if let Some(parent) = dst.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::remove_file(&dst);
                    let _ = fs::copy(&src, &dst);
                }
            }
        }
    }
}

fn merge_stage(stage: &Path, root: &Path) {
    for rel in MERGE_TREES {
        let from = stage.join(rel);
        if !from.exists() {
            continue;
        }
        copy_tree(&from, &root.join(rel));
    }
}

fn copy_tree(from: &Path, to: &Path) {
    if from.is_file() {
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::remove_file(to);
        let _ = fs::copy(from, to);
        return;
    }
    let Ok(entries) = fs::read_dir(from) else {
        return;
    };
    let _ = fs::create_dir_all(to);
    for ent in entries.flatten() {
        let src = ent.path();
        let dst = to.join(ent.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else if src.is_symlink() {
            #[cfg(unix)]
            {
                if let Ok(target) = fs::read_link(&src) {
                    let _ = fs::remove_file(&dst);
                    let _ = std::os::unix::fs::symlink(target, &dst);
                }
            }
        } else {
            let _ = fs::remove_file(&dst);
            let _ = fs::copy(&src, &dst);
        }
    }
}

/// Preferred real package for a virtual Depends name. Without this,
/// `apt-cache depends` lists every provider under `<sf3-soundfont-gm>` and a
/// naive parser downloads hundreds of MiB of MuseScore fonts.
const VIRTUAL_PREFERS: &[(&str, &str)] = &[
    ("sf3-soundfont-gm", "timgm6mb-soundfont"),
    ("sf2-soundfont-gm", "timgm6mb-soundfont"),
    ("libjack-0.125", "libjack-jackd2-0"),
];

/// Parse `LC_ALL=C apt-cache depends` stdout into package names.
///
/// Only `Depends:` / `PreDepends:` (and the `|Depends:` first alternative)
/// count. Bare indented names under a virtual package are providers — take
/// none of them here; [`VIRTUAL_PREFERS`] / [`resolve_real_package`] pick one.
fn parse_depends_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let rest = if let Some(r) = t.strip_prefix("|Depends:") {
            r
        } else if let Some(r) = t.strip_prefix("Depends:") {
            r
        } else if let Some(r) = t.strip_prefix("|PreDepends:") {
            r
        } else if let Some(r) = t.strip_prefix("PreDepends:") {
            r
        } else {
            // Package headers and indented provider lists — ignore.
            continue;
        };
        let raw = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('<')
            .trim_end_matches('>')
            .split(':')
            .next()
            .unwrap_or("");
        if raw.is_empty() {
            continue;
        }
        if let Some((_, prefer)) = VIRTUAL_PREFERS.iter().find(|(v, _)| *v == raw) {
            out.push((*prefer).to_string());
        } else {
            out.push(raw.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Best-effort recursive Depends via `apt-cache depends --recurse` on the
/// guest suite index (not the host's).
fn depends_recursive(cache: &Path, deb: &str, packages: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = packages.to_vec();
    if !ensure_suite_index(cache, deb) {
        return Some(out);
    }
    let mut cmd = apt_cmd("apt-cache", cache, deb);
    cmd.arg("depends")
        .arg("--recurse")
        .arg("--no-recommends")
        .arg("--no-suggests")
        .arg("--no-conflicts")
        .arg("--no-breaks")
        .arg("--no-replaces")
        .arg("--no-enhances");
    for p in packages {
        cmd.arg(p);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return Some(out);
    }
    out.extend(parse_depends_lines(&String::from_utf8_lossy(
        &output.stdout,
    )));
    out.sort();
    out.dedup();
    Some(out)
}

/// Ubuntu stonking (26.10) names for the Eclipse desktop set (Alpine DEFAULT_PACKAGES map).
pub(super) fn desktop_packages_ubuntu() -> Vec<String> {
    DESKTOP_UBUNTU.iter().map(|s| (*s).to_string()).collect()
}

const DESKTOP_UBUNTU: &[&str] = &[
    "xserver-xorg-core",
    "xserver-xorg-video-fbdev",
    "xserver-xorg-input-libinput",
    "xinit",
    "libgl1-mesa-dri",
    "libgl1",
    "libegl1",
    "libgles2",
    "libvulkan1",
    "mesa-vulkan-drivers",
    "vulkan-tools",
    "mesa-utils",
    "xkb-data",
    "x11-xkb-utils",
    "xfonts-base",
    "xfonts-encodings",
    "fonts-dejavu",
    "xterm",
    "openbox",
    "x11-xserver-utils",
    "xfce4-session",
    "xfwm4",
    "xfce4-panel",
    "xfdesktop4",
    "xfce4-settings",
    "xfconf",
    "thunar",
    "libgarcon-1-0",
    "libgarcon-common",
    "xfce4-terminal",
    "xfce4-appfinder",
    "xfce4-pulseaudio-plugin",
    "dbus",
    "dbus-x11",
    "adwaita-icon-theme",
    "librsvg2-common",
    "libgdk-pixbuf-2.0-0",
    "libgdk-pixbuf2.0-bin",
    // Successor of transitional `mime-support` (Firefox/mailcap Depends).
    "media-types",
    "shared-mime-info",
    "hicolor-icon-theme",
    "labwc",
    // Pin wlroots to the SONAME labwc links: stonking uses 0.20 (not
    // libwlroots.so.12 from noble). Explicit so a stale transitive miss
    // cannot leave the compositor with "libwlroots.so.12: not found".
    "libwlroots-0.20",
    "libsfdo0",
    "locales",
    "tzdata",
    "openntpd",
    "seatd",
    "foot",
    "wayland-protocols",
    "xwayland",
    "libasound2t64",
    "libasound2-plugins",
    "alsa-utils",
    "pulseaudio",
    "pulseaudio-utils",
    "libpulse0",
    "gstreamer1.0-pulseaudio",
    "mpg123",
    "freedoom",
    "chocolate-doom",
    "bash",
    "libsdl2-2.0-0",
    "libsdl3-0",
    "libsdl1.2debian",
    "libsdl2-image-2.0-0",
    "libsdl2-ttf-2.0-0",
    "libsdl2-mixer-2.0-0",
    "libsdl2-net-2.0-0",
    "libpng16-16t64",
    "libfluidsynth3",
    // Tiny GM font (~5 MiB). Without an explicit pick, fluidsynth's virtual
    // `sf3-soundfont-gm` used to pull every MuseScore soundfont (100+ MiB each).
    "timgm6mb-soundfont",
    "libdecor-0-0",
    "firefox",
    // SpiderMonkey needs ICU data at JS_Init. Ubuntu links it into
    // libicudata.so (multiarch); without this package Firefox aborts before
    // opening a window. Do not rely on a transitive firefox Depends alone.
    "libicu78",
    "zstd",
    "curl",
    "ca-certificates",
    "libpci3",
    "swayimg",
];

/// Verify helpers for `verify.rs`.
pub(super) fn check_apt(root: &Path, arch: &str) -> Vec<String> {
    let mut out = Vec::new();
    let sources = root.join("etc/apt/sources.list");
    match fs::read_to_string(&sources) {
        Ok(s) if s.contains(DISTRO) => {}
        Ok(_) => out.push(format!("/etc/apt/sources.list does not mention {DISTRO}")),
        Err(_) => out.push("falta /etc/apt/sources.list".into()),
    }
    let arch_file = root.join("var/lib/dpkg/arch");
    if let Some(want) = deb_arch(arch) {
        match fs::read_to_string(&arch_file) {
            Ok(s) if s.trim() == want => {}
            Ok(s) => out.push(format!(
                "/var/lib/dpkg/arch dice {:?} y la rootfs es {arch} ({want})",
                s.trim()
            )),
            Err(_) => out.push("falta /var/lib/dpkg/arch".into()),
        }
    }
    let apt_bin = root.join("usr/bin/apt-get");
    let dpkg_bin = root.join("usr/bin/dpkg");
    if !apt_bin.is_file() && !root.join("usr/bin/apt").is_file() {
        out.push("falta /usr/bin/apt o apt-get tras el bootstrap".into());
    }
    if !dpkg_bin.is_file() {
        out.push("falta /usr/bin/dpkg tras el bootstrap".into());
    }
    let status = root.join("var/lib/dpkg/status");
    if !status.exists() {
        out.push("falta /var/lib/dpkg/status".into());
    }
    let cputable = root.join("usr/share/dpkg/cputable");
    if !cputable.is_file() {
        out.push("falta /usr/share/dpkg/cputable tras el bootstrap (apt falla con 'Error reading the CPU table')".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deb_arch_maps() {
        assert_eq!(deb_arch("x86_64"), Some("amd64"));
        assert_eq!(deb_arch("aarch64"), Some("arm64"));
        assert_eq!(deb_arch("riscv64"), None);
    }

    #[test]
    fn default_uses_apt_on_primary_arches() {
        // Without hatch — unit test cannot clear env reliably if set; just map.
        assert!(default_apt("x86_64"));
        assert!(default_apt("aarch64"));
        assert!(!default_apt("riscv64"));
    }

    #[test]
    fn desktop_list_is_non_empty() {
        assert!(desktop_packages_ubuntu().len() > 40);
        assert!(DESKTOP_UBUNTU.contains(&"firefox"));
        assert!(DESKTOP_UBUNTU.contains(&"labwc"));
        assert!(DESKTOP_UBUNTU.contains(&"libgarcon-1-0"));
        assert!(!DESKTOP_UBUNTU.contains(&"garcon"));
        assert!(DESKTOP_UBUNTU.contains(&"libsdl3-0"));
        assert!(DESKTOP_UBUNTU.contains(&"libicu78"));
    }

    /// Input/audio packages the Ubuntu desktop set must keep naming explicitly:
    /// transitive Depends are easy to lose when the closure filter drops
    /// virtuals, and without these labwc has no seat/libinput and the chime
    /// has no player.
    #[test]
    fn desktop_set_names_input_and_audio_packages() {
        // Top-level only: libinput10 arrives as a Depends of the X driver.
        for pkg in [
            "seatd",
            "labwc",
            "libwlroots-0.20",
            "libsfdo0",
            "xserver-xorg-input-libinput",
            "xkb-data",
            "adwaita-icon-theme", // software cursor needs a theme on disk
            "mpg123",
            "libasound2t64",
            "libasound2-plugins", // libasound_module_pcm_pulse.so (Recommends-only in Ubuntu)
            "pulseaudio",
            "pulseaudio-utils", // pactl unmute/drain for the boot chime
        ] {
            assert!(
                DESKTOP_UBUNTU.contains(&pkg),
                "{pkg} must be top-level in DESKTOP_UBUNTU (input/audio)"
            );
        }
        assert!(
            MERGE_TREES.contains(&"etc/alsa"),
            "etc/alsa must be in MERGE_TREES so /etc/alsa/conf.d reaches the rootfs"
        );
    }

    #[test]
    fn alternatives_synthesize_mpg123_and_seatd_symlinks() {
        let dir = tempfile_dir("apt-alts");
        fs::create_dir_all(dir.join("usr/bin")).unwrap();
        fs::create_dir_all(dir.join("usr/sbin")).unwrap();
        fs::write(dir.join("usr/bin/mpg123.bin"), b"fake").unwrap();
        fs::write(dir.join("usr/sbin/seatd"), b"fake").unwrap();
        fix_common_alternatives(&dir);
        assert!(
            dir.join("usr/bin/mpg123").exists(),
            "mpg123 alternative missing"
        );
        assert!(
            dir.join("usr/bin/seatd").exists(),
            "seatd -> sbin symlink missing"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_layout_smoke() {
        let dir = tempfile_dir("apt-layout");
        write_layout(&dir, "x86_64");
        let s = fs::read_to_string(dir.join("etc/apt/sources.list")).unwrap();
        assert!(s.contains("stonking"));
        assert!(s.contains("amd64"));
        assert!(s.contains("packages.mozilla.org"));
        assert!(s.contains("signed-by=/etc/apt/keyrings/packages.mozilla.org.asc"));
        assert!(dir.join("etc/apt/preferences.d/mozilla-firefox").is_file());
        assert!(dir
            .join("etc/apt/keyrings/packages.mozilla.org.asc")
            .is_file());
        assert!(dir
            .join("etc/apt/keyrings/packages.mozilla.org.gpg")
            .is_file());
        assert!(dir
            .join("etc/apt/trusted.gpg.d/packages.mozilla.org.gpg")
            .is_file());
        assert_eq!(
            fs::read_to_string(dir.join("var/lib/dpkg/arch"))
                .unwrap()
                .trim(),
            "amd64"
        );
        #[cfg(unix)]
        {
            assert!(dir.join("lib64").is_symlink());
            assert_eq!(
                fs::read_link(dir.join("lib64")).unwrap(),
                std::path::Path::new("usr/lib64")
            );
        }
    }

    #[test]
    fn deb_version_and_size_match_the_right_firefox_stanza() {
        assert_eq!(
            version_from_deb_name("firefox", "firefox_158.0.1~build1_amd64.deb").as_deref(),
            Some("158.0.1~build1")
        );
        assert_eq!(
            version_from_deb_name("firefox", "firefox_1%3a1snap1-0ubuntu11_amd64.deb").as_deref(),
            Some("1:1snap1-0ubuntu11")
        );
        let show = "\
Package: firefox\n\
Version: 1:1snap1-0ubuntu11\n\
Size: 68410\n\
\n\
Package: firefox\n\
Version: 158.0.1~build1\n\
Size: 93046960\n\
";
        assert_eq!(size_from_apt_show(show, "1:1snap1-0ubuntu11"), Some(68410));
        assert_eq!(size_from_apt_show(show, "158.0.1~build1"), Some(93046960));
        // Must NOT use the stub size for the Mozilla version.
        assert_ne!(size_from_apt_show(show, "158.0.1~build1"), Some(68410));
    }

    #[test]
    fn depends_parser_ignores_virtual_provider_lists() {
        let text = "\
libfluidsynth3\n\
  Depends: timgm6mb-soundfont\n\
  Depends: <sf3-soundfont-gm>\n\
    fluid-soundfont-gm\n\
    musescore-general-soundfont\n\
    musescore-general-soundfont-lossless\n\
 |Depends: libjack-jackd2-0\n\
  Depends: <libjack-0.125>\n\
    libjack-jackd2-0\n\
    libjack0\n\
";
        let got = parse_depends_lines(text);
        assert!(got.contains(&"timgm6mb-soundfont".into()), "{got:?}");
        assert!(got.contains(&"libjack-jackd2-0".into()), "{got:?}");
        assert!(
            !got.iter().any(|p| p.contains("musescore")),
            "must not pull every soundfont provider: {got:?}"
        );
        assert!(!got.contains(&"libjack0".into()), "{got:?}");
    }

    #[test]
    fn package_renames_cover_gdk_pixbuf_and_mime_support() {
        let c = package_rename_candidates("libgdk-pixbuf2.0-0");
        assert!(
            c.iter().any(|s| s == "libgdk-pixbuf-2.0-0"),
            "gdk-pixbuf rename missing: {c:?}"
        );
        let c = package_rename_candidates("mime-support");
        assert!(
            c.iter().any(|s| s == "media-types"),
            "mime-support → media-types missing: {c:?}"
        );
        let c = package_rename_candidates("libssl3");
        assert!(c.iter().any(|s| s == "libssl3t64"), "{c:?}");
    }

    #[test]
    fn suite_sources_pin_distro_not_host() {
        // Host may be noble; downloads must still target DISTRO.
        let dir = tempfile_dir("apt-suite");
        let sources = write_suite_sources(&dir, "amd64");
        let s = fs::read_to_string(&sources).unwrap();
        assert!(s.contains(DISTRO), "{s}");
        assert!(s.contains("trusted=yes"), "{s}");
        assert!(!s.contains("noble"), "must not pin host suite:\n{s}");
        assert!(
            s.contains("packages.mozilla.org"),
            "Mozilla firefox repo missing:\n{s}"
        );
        assert!(
            s.contains("signed-by="),
            "Mozilla repo must carry signed-by:\n{s}"
        );
        assert!(dir
            .join(".apt/etc/keyrings/packages.mozilla.org.asc")
            .is_file());
        assert!(BOOTSTRAP_PKGS.contains(&"libapt-pkg7.0"));
        assert!(BOOTSTRAP_PKGS.contains(&"gpgv"));
        assert!(BOOTSTRAP_PKGS.contains(&"libattr1"));
        assert!(BOOTSTRAP_PKGS.contains(&"gnu-coreutils"));
        assert!(BOOTSTRAP_PKGS.contains(&"libssl4"));
        assert!(
            cache_dir("x86_64").ends_with(format!("{DISTRO}/x86_64")),
            "cache must be suite-scoped, got {}",
            cache_dir("x86_64").display()
        );
        let opts = apt_suite_opts(&dir, "amd64");
        assert!(
            opts.iter().any(|o| o.starts_with("Dir::State::status=")),
            "must isolate dpkg status from host Pop packages: {opts:?}"
        );
        // apt resolves relative Dir::* under /etc/apt — opts must be absolute.
        for o in &opts {
            if let Some((_, path)) = o.split_once('=') {
                if path.starts_with('/') {
                    assert!(
                        Path::new(path).is_absolute(),
                        "apt Dir path must be absolute: {o}"
                    );
                }
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_asound_conf_routes_to_pulse_on_both_alpine_and_ubuntu_multiarch_layouts() {
        for (tag, plugin_rel) in [
            (
                "asound-alpine",
                "usr/lib/alsa-lib/libasound_module_pcm_pulse.so",
            ),
            (
                "asound-ubuntu-amd64",
                "usr/lib/x86_64-linux-gnu/alsa-lib/libasound_module_pcm_pulse.so",
            ),
            (
                "asound-ubuntu-arm64",
                "usr/lib/aarch64-linux-gnu/alsa-lib/libasound_module_pcm_pulse.so",
            ),
        ] {
            let rootfs = tempfile_dir(tag);
            let bin = rootfs.join("usr/bin");
            let plugin = rootfs.join(plugin_rel);
            fs::create_dir_all(&bin).unwrap();
            fs::create_dir_all(plugin.parent().unwrap()).unwrap();
            fs::write(bin.join("pulseaudio"), b"fake").unwrap();
            fs::write(&plugin, b"fake").unwrap();

            super::super::LinuxRootfs::write_asound_conf(&rootfs);
            let conf = fs::read_to_string(rootfs.join("etc/asound.conf")).unwrap();
            assert!(
                conf.contains("type pulse") && conf.contains("server unix:/run/pulse/native"),
                "{tag} ({plugin_rel}) did not route ALSA default to PulseAudio:\n{conf}"
            );
            let _ = fs::remove_dir_all(&rootfs);
        }

        let no_pulse = tempfile_dir("asound-no-pulse");
        super::super::LinuxRootfs::write_asound_conf(&no_pulse);
        let fallback = fs::read_to_string(no_pulse.join("etc/asound.conf")).unwrap();
        assert!(
            !fallback.contains("type pulse") && fallback.contains("slave.pcm \"eclipse_hw\""),
            "fallback without PulseAudio must route default through plug->eclipse_hw:\n{fallback}"
        );
        let _ = fs::remove_dir_all(&no_pulse);
    }

    #[test]
    fn labwc_input_stack_pins_seatd_and_waits_for_evdev() {
        let rootfs = tempfile_dir("input-stack");
        let svc_dir = rootfs.join("etc/eclipse/services");
        let localbin = rootfs.join("usr/local/bin");
        fs::create_dir_all(&svc_dir).unwrap();
        fs::create_dir_all(&localbin).unwrap();
        super::super::LinuxRootfs::write_init_services(&svc_dir, true);
        super::super::LinuxRootfs::write_init_wrappers(&localbin, &svc_dir);

        let labwc_svc = fs::read_to_string(svc_dir.join("labwc.service")).unwrap();
        assert!(labwc_svc.contains("wait_socket = /run/seatd.sock"));
        assert!(labwc_svc.contains("wait_path = /dev/input"));
        assert!(labwc_svc.contains("requires = seatd"));

        super::super::desktop::install(&rootfs);
        let labwc = fs::read_to_string(localbin.join("labwc")).unwrap();
        assert!(labwc.contains("LIBSEAT_BACKEND:=seatd"), "{labwc}");
        assert!(labwc.contains("WLR_BACKENDS:=drm,libinput"), "{labwc}");
        assert!(labwc.contains("WLR_LIBINPUT_NO_DEVICES:=1"), "{labwc}");
        let _ = fs::remove_dir_all(&rootfs);
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("eclipse-apt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
