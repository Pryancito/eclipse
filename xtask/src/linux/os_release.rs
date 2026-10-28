//! `/etc/os-release`: Eclipse's own, not the one its package tap drops.
//!
//! The rootfs is built on top of someone else's userspace -- Ubuntu `.deb`s on
//! x86_64/aarch64, Alpine `.apk`s elsewhere -- and both of those ship an
//! `os-release` of their own inside `base-files`/`alpine-release`. Whatever
//! lands there is what every program that asks "what system am I on?" believes:
//! `/etc/os-release` is the one interface Linux userspace agreed on for that
//! question, read by glibc's `gnu_get_libc_version` callers, by
//! `neofetch`/`fastfetch`, by `lsb_release` fallbacks, by GTK's about dialogs,
//! by crash reporters and by any installer script that branches on `ID`. An
//! Eclipse that answers "Ubuntu" there is an Eclipse that cannot be told apart
//! from its own build dependency.
//!
//! So this writes ours, last, over whatever the tap left. The fields and their
//! quoting are the os-release specification (systemd/freedesktop,
//! `os-release(5)`); nothing here is invented.
//!
//! **The version is not typed here.** It is read out of
//! [`VERSION_SOURCE`](self::VERSION_SOURCE), the same `ECLIPSE_VERSION` that
//! `uname(2)`, `/proc/version` and `/proc/sys/kernel/osrelease` report, so a
//! release bump moves one constant and `uname -r` and `os-release` cannot
//! disagree. A second copy of a version number is a second copy that goes
//! stale, and this one would go stale *silently*: nothing fails when
//! `/etc/os-release` lies.
//!
//! **One deliberate divergence from the spec's recommendation.** The spec
//! suggests the real file live in `/usr/lib/os-release` with `/etc/os-release`
//! as a symlink to it. Here it is the other way round: the real file is
//! `/etc/os-release` and `/usr/lib/os-release` is the symlink. `/etc` is the
//! path every reader actually opens, and this tree boots with no udev and with
//! several image packers between the rootfs and the guest; a plain file on the
//! path that matters cannot be broken by a symlink that one of them flattens.
//! Both paths exist and both resolve, which is all any reader needs.

use super::apt_rootfs;
use crate::{variant::Variant, PROJECT_DIR};
use std::{fs, os::unix, path::Path, process::Command};

/// Where the product version lives, as a path relative to the repository root.
///
/// `linux-object::uname::ECLIPSE_VERSION` is the kernel's own idea of which
/// Eclipse this is; see the module docs for why it is read instead of copied.
pub(super) const VERSION_SOURCE: &str = "linux-object/src/uname.rs";

/// The constant whose string literal is the version.
const VERSION_CONST: &str = "ECLIPSE_VERSION";

/// `NAME`: the operating system without its version. Spec: "a string
/// identifying the operating system, without a version component, and suitable
/// for presentation to the user".
const NAME: &str = "Eclipse OS";

/// `ID`: lowercase, no spaces, the machine-readable name programs switch on.
const ID: &str = "eclipse";

const HOME_URL: &str = "https://github.com/Pryancito/eclipse";
const BUG_REPORT_URL: &str = "https://github.com/Pryancito/eclipse/issues";

/// `ANSI_COLOR`: how a terminal should tint the logo/name. Magenta, as the
/// boot console and the wallpaper already are.
pub(super) const ANSI_COLOR: &str = "0;35";

/// `LOGO`: the name of an ICON, not of the distribution.
///
/// This is the one field whose value is not ours to choose. The spec says it
/// names an icon "following the Icon Theme Specification", conventionally
/// `distributor-logo-<LOGO>`, and readers resolve it through the installed
/// icon theme: `fastfetch`, GTK's about dialogs and the desktop's own about
/// pages all look up that name. So the value is only as true as the icon
/// behind it: this said `linux` for as long as no theme in the image had an
/// `eclipse` icon, because naming one that is not there buys a blank where
/// the logo goes, not a logo.
///
/// [`logo`](super::logo) now installs
/// `usr/share/icons/hicolor/scalable/apps/distributor-logo-eclipse.svg`, so
/// `eclipse` resolves. The two are one decision: whoever stops shipping that
/// icon has to bring this back to `linux` in the same change, and the test
/// below is what makes that the only way out.
const LOGO: &str = "eclipse";

/// The value of `ARCHITECTURE`, in the spelling the spec prescribes (systemd's
/// own architecture ids), not in ours.
///
/// This is a translation and not `arch` verbatim on purpose: `x86_64` is the
/// kernel's spelling, `x86-64` is os-release's, and a reader that compares
/// against the spec's table would miss ours.
fn architecture(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x86-64"),
        "aarch64" => Some("arm64"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }
}

/// `ID_LIKE`: the systems ours is close enough to that a script written for
/// them will mostly work. It is the package tap talking, and it changes with
/// the tap: a script that reads `ID_LIKE=ubuntu debian` and reaches for `apt`
/// is right on the apt rootfs and wrong on the apk one.
fn id_like(arch: &str) -> &'static str {
    if apt_rootfs::use_apt(arch) {
        "ubuntu debian"
    } else {
        "alpine"
    }
}

/// `VARIANT` / `VARIANT_ID`: which image this is. `VARIANT_ID` is the
/// lowercase token (the spec's rules for `ID`), `VARIANT` the human name.
fn variant_name(variant: Variant) -> &'static str {
    match variant {
        Variant::Desktop => "Desktop",
        Variant::Minimal => "Minimal",
    }
}

/// The version literal inside `src`, which is the body of [`VERSION_SOURCE`].
///
/// Deliberately a string scan and not a dependency on `linux-object`: that
/// crate is `no_std` and built for the guest, and making the host-side build
/// tool link it to read one `&str` would drag the kernel's whole dependency
/// tree into `cargo test -p xtask`.
fn parse_version(src: &str) -> Option<String> {
    src.lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .filter_map(|l| l.split_once(VERSION_CONST))
        .filter_map(|(before, after)| {
            // `pub const ECLIPSE_VERSION: &str = "0.6.2";` -- and not, say, a
            // doc link or a use statement that merely names it.
            before.contains("const").then_some(after)
        })
        .filter_map(|after| {
            let (_, value) = after.split_once('=')?;
            let value = value.trim().strip_suffix(';')?.trim();
            let value = value.strip_prefix('"')?.strip_suffix('"')?;
            (!value.is_empty()).then(|| value.to_string())
        })
        .next()
}

/// The product version, read from the tree.
///
/// Panics rather than defaulting: an image whose `/etc/os-release` claims a
/// version nobody shipped is worse than a build that stops and says why, and
/// [`VERSION_SOURCE`] is a file git tracks -- it is missing only if the build
/// is running somewhere it has no business running.
pub(super) fn eclipse_version() -> String {
    let path = PROJECT_DIR.join(VERSION_SOURCE);
    let src = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("os-release: cannot read {}: {e}", path.display()));
    parse_version(&src).unwrap_or_else(|| {
        panic!(
            "os-release: no `const {VERSION_CONST}: &str = \"...\"` in {}",
            path.display()
        )
    })
}

/// `BUILD_ID`: which build of that version this is. The commit is the only
/// honest answer here, and the spec's own example is a vcs id.
///
/// `None` when git cannot answer (a tarball, a shallow copy with no HEAD): the
/// field is optional, and an absent `BUILD_ID` says "unknown" where a made-up
/// one would not.
fn build_id() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&**PROJECT_DIR)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// One `KEY=VALUE` line, quoted the way the spec requires.
///
/// os-release is "a newline-separated list of environment-like
/// shell-compatible variable assignments", so a value with a space in it that
/// is not quoted does not parse -- `. /etc/os-release` from a shell script is
/// a documented way to read the file, and `PRETTY_NAME=Eclipse OS 0.6.2` would
/// run `OS` as a command. Bare values are left bare because that is what every
/// distribution's file looks like and what a naive `cut -d= -f2` reader
/// expects.
fn line(key: &str, value: &str) -> String {
    let bare = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b':'));
    if bare {
        format!("{key}={value}\n")
    } else {
        // The spec allows "\\", "\$", "\"", "\`" inside double quotes; nothing
        // we write contains any of them, but a version string from the tree is
        // still input, so escape rather than trust.
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`");
        format!("{key}=\"{escaped}\"\n")
    }
}

/// The whole file.
///
/// Pure on purpose: everything that could be wrong about this file (a missing
/// mandatory field, an unquoted space, a version that is not the tree's) is
/// decided here, where a test can read the answer without building an image.
fn render(
    version: &str,
    build_id: Option<&str>,
    arch: &str,
    variant: Variant,
    id_like: &str,
) -> String {
    let mut out = String::new();
    // Order follows os-release(5)'s own presentation: identification first,
    // then version, then the optional metadata.
    out.push_str(&line("NAME", NAME));
    out.push_str(&line("ID", ID));
    out.push_str(&line("ID_LIKE", id_like));
    out.push_str(&line("VERSION", version));
    out.push_str(&line("VERSION_ID", version));
    out.push_str(&line("PRETTY_NAME", &format!("{NAME} {version}")));
    if let Some(build) = build_id {
        out.push_str(&line("BUILD_ID", build));
    }
    out.push_str(&line("VARIANT", variant_name(variant)));
    out.push_str(&line("VARIANT_ID", variant.name()));
    if let Some(a) = architecture(arch) {
        out.push_str(&line("ARCHITECTURE", a));
    }
    out.push_str(&line("LOGO", LOGO));
    out.push_str(&line("ANSI_COLOR", ANSI_COLOR));
    out.push_str(&line("HOME_URL", HOME_URL));
    out.push_str(&line("DOCUMENTATION_URL", HOME_URL));
    out.push_str(&line("SUPPORT_URL", BUG_REPORT_URL));
    out.push_str(&line("BUG_REPORT_URL", BUG_REPORT_URL));
    out
}

/// Writes Eclipse's `/etc/os-release` into `rootfs`, replacing whatever the
/// package tap left there, and points `/usr/lib/os-release` at it.
///
/// Call this AFTER the tap and the desktop stack: both extract packages that
/// carry their own file, and the last writer is the one the guest reads.
pub(super) fn install(rootfs: &Path, arch: &str, variant: Variant) {
    let version = eclipse_version();
    let build = build_id();
    let body = render(&version, build.as_deref(), arch, variant, id_like(arch));

    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    let real = etc.join("os-release");
    // Remove, don't overwrite: Ubuntu's `base-files` leaves `/etc/os-release`
    // as a symlink into `/usr/lib`, and writing through it would put our text
    // in their file and leave `/etc` pointing out of itself.
    let _ = fs::remove_file(&real);
    fs::write(&real, &body)
        .unwrap_or_else(|e| panic!("os-release: cannot write {}: {e}", real.display()));

    let usr_lib = rootfs.join("usr").join("lib");
    fs::create_dir_all(&usr_lib).unwrap();
    let link = usr_lib.join("os-release");
    let _ = fs::remove_file(&link);
    if let Err(e) = unix::fs::symlink("../../etc/os-release", &link) {
        // A copy is a worse answer than a symlink (it can drift), but a
        // missing `/usr/lib/os-release` is worse than both: it is the spec's
        // fallback path, and a reader that only knows it would find nothing.
        eprintln!(
            "warning: os-release: symlink {} failed: {e}",
            link.display()
        );
        let _ = fs::write(&link, &body);
    }

    println!(
        "os-release: {NAME} {version} ({arch}, {}{})",
        variant.name(),
        build
            .as_deref()
            .map(|b| format!(", build {b}"))
            .unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-osrelease-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `KEY=VALUE` as a shell would see it, with the quotes already removed.
    fn fields(body: &str) -> Vec<(String, String)> {
        body.lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (k, v) = l.split_once('=').unwrap_or_else(|| {
                    panic!("os-release line is not an assignment: {l:?}");
                });
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v);
                (k.to_string(), v.to_string())
            })
            .collect()
    }

    fn get(body: &str, key: &str) -> Option<String> {
        fields(body)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn sample() -> String {
        render(
            "1.2.3",
            Some("abcdef123456"),
            "x86_64",
            Variant::Minimal,
            "ubuntu debian",
        )
    }

    /// The four fields the spec calls for in practice: `ID` and `NAME` for
    /// "which system", `VERSION_ID` for "which release", `PRETTY_NAME` for
    /// "what to show the user" (it is the one field with a defined default, and
    /// every fetch tool prints it).
    #[test]
    fn the_mandatory_fields_are_there() {
        let body = sample();
        for key in ["NAME", "ID", "VERSION_ID", "PRETTY_NAME"] {
            let v = get(&body, key).unwrap_or_else(|| panic!("{key} missing:\n{body}"));
            assert!(!v.is_empty(), "{key} is empty:\n{body}");
        }
        assert_eq!(get(&body, "ID").unwrap(), "eclipse");
        assert_eq!(get(&body, "NAME").unwrap(), "Eclipse OS");
    }

    /// `ID` has to be lowercase with no spaces: it is the token scripts
    /// compare against, and the spec restricts it to lowercase alphanumerics
    /// plus `.`, `_` and `-` for exactly that reason.
    #[test]
    fn the_machine_readable_ids_are_tokens() {
        let body = sample();
        for key in ["ID", "VARIANT_ID", "VERSION_ID"] {
            let v = get(&body, key).unwrap();
            assert!(
                v.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, '.' | '_' | '-')),
                "{key}={v:?} is not a lowercase token"
            );
        }
    }

    /// `/etc/os-release` is documented as sourceable, and plenty of scripts do
    /// exactly that. A value with a space in it that we forgot to quote turns
    /// the second word into a command, so this asks a real shell instead of
    /// trusting the renderer: `PRETTY_NAME` is the field that always has a
    /// space, and the one that would break first.
    #[test]
    fn a_shell_can_source_it() {
        let dir = scratch("source");
        let f = dir.join("os-release");
        fs::write(&f, sample()).unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!(
                ". \"{}\" && printf '%s|%s|%s' \"$ID\" \"$PRETTY_NAME\" \"$VERSION_ID\"",
                f.display()
            ))
            .output()
            .expect("sh did not run");
        assert!(
            out.status.success(),
            "sh could not source it: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "eclipse|Eclipse OS 1.2.3|1.2.3"
        );
    }

    /// The version in the file is the tree's, and it gets there by being read.
    ///
    /// Both halves matter: that the scan finds it in the real `uname.rs` (so a
    /// rename of the constant fails here and not in a shipped image), and that
    /// the version does NOT appear as a literal in this module -- which is the
    /// only way to catch somebody "fixing" a release bump by typing the number
    /// in a second place.
    #[test]
    fn the_version_comes_from_the_tree_and_is_not_copied_here() {
        let version = eclipse_version();
        assert!(
            version.split('.').count() >= 2
                && version
                    .split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "{version:?} does not look like a version"
        );
        let body = render(&version, None, "x86_64", Variant::Desktop, "alpine");
        assert_eq!(get(&body, "VERSION_ID").unwrap(), version);
        assert_eq!(
            get(&body, "PRETTY_NAME").unwrap(),
            format!("Eclipse OS {version}")
        );

        // Only the production half of this file, and only its code: a comment
        // (or a test's own sample) may legitimately quote a version-shaped
        // string, and flagging those would make this test cry wolf on a
        // docs-only edit. What must not happen is the number being *used*.
        let me = fs::read_to_string(PROJECT_DIR.join("xtask/src/linux/os_release.rs")).unwrap();
        let code = me.split("\n#[cfg(test)]").next().unwrap();
        let culprits: Vec<&str> = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(&version))
            .collect();
        assert!(
            culprits.is_empty(),
            "the tree's version {version:?} is typed into os_release.rs, which is \
             the one thing reading {VERSION_SOURCE} exists to prevent: {culprits:?}"
        );
    }

    #[test]
    fn the_version_scan_reads_the_literal_and_rejects_junk() {
        assert_eq!(
            parse_version("pub const ECLIPSE_VERSION: &str = \"0.9.1\";").as_deref(),
            Some("0.9.1")
        );
        // A doc comment that names the constant is not the constant.
        assert_eq!(
            parse_version("/// [`ECLIPSE_VERSION`] keeps it available.\n").as_deref(),
            None
        );
        // Nor is a mention with no `const` before it.
        assert_eq!(
            parse_version("use crate::ECLIPSE_VERSION;").as_deref(),
            None
        );
        assert_eq!(
            parse_version("pub const ECLIPSE_VERSION: &str = \"\";").as_deref(),
            None
        );
        assert_eq!(parse_version("").as_deref(), None);
        // The real declaration, found among the rest of a file.
        let src =
            "//! docs\nconst OTHER: usize = 1;\npub const ECLIPSE_VERSION: &str = \"7.7.7\";\n";
        assert_eq!(parse_version(src).as_deref(), Some("7.7.7"));
    }

    /// `LOGO` names an icon, so it has to name one the image actually
    /// INSTALLS. This is the test that ties the field to the file: it reads
    /// the logo module for the icon it ships and asks that `LOGO` resolve to
    /// it, so dropping the icon and leaving the field, or renaming either one
    /// alone, fails here instead of in a reader that quietly draws a blank.
    #[test]
    fn the_logo_names_an_icon_the_image_installs() {
        let body = sample();
        let value = get(&body, "LOGO").unwrap();
        // The whole literal, quotes included: a substring match would be
        // satisfied by `distributor-logo-eclipseos`, which is a different icon
        // and would leave this field naming one that does not exist.
        let wanted = format!("\"distributor-logo-{value}\"");
        let logo_rs = fs::read_to_string(PROJECT_DIR.join("xtask/src/linux/logo.rs")).unwrap();
        assert!(
            logo_rs.contains(&wanted),
            "LOGO={value:?} needs an icon named distributor-logo-{value}, \
             and logo.rs installs no such file"
        );
    }

    /// The spec's spelling of the architecture, not the kernel's.
    #[test]
    fn the_architecture_uses_the_specs_names() {
        assert_eq!(architecture("x86_64"), Some("x86-64"));
        assert_eq!(architecture("aarch64"), Some("arm64"));
        assert_eq!(architecture("riscv64"), Some("riscv64"));
        // An arch we do not know is a field we leave out, not a guess.
        assert_eq!(architecture("m68k"), None);
        let body = render("1.2.3", None, "m68k", Variant::Minimal, "alpine");
        assert!(get(&body, "ARCHITECTURE").is_none(), "{body}");
    }

    /// The two variants are two different images, and `VARIANT_ID` is how the
    /// guest (and anyone reading a bug report) tells them apart.
    #[test]
    fn each_variant_says_which_one_it_is() {
        for (v, want) in [(Variant::Desktop, "desktop"), (Variant::Minimal, "minimal")] {
            let body = render("1.2.3", None, "x86_64", v, "alpine");
            assert_eq!(get(&body, "VARIANT_ID").unwrap(), want);
            assert_eq!(get(&body, "VARIANT").unwrap(), variant_name(v));
        }
    }

    /// `ID_LIKE` follows the package tap, because that is what makes it true:
    /// a script that reads it and reaches for `apt` must only do so on the
    /// rootfs that has apt.
    #[test]
    fn id_like_follows_the_package_tap() {
        // `use_apt` reads an env hatch, so this asks for both answers through
        // the same function the build uses rather than hardcoding per-arch.
        for arch in ["x86_64", "aarch64", "riscv64"] {
            let want = if apt_rootfs::use_apt(arch) {
                "ubuntu debian"
            } else {
                "alpine"
            };
            assert_eq!(id_like(arch), want, "{arch}");
        }
    }

    /// An absent `BUILD_ID` is a field that is not written, not an empty one:
    /// `BUILD_ID=""` reads as "the build id is the empty string".
    #[test]
    fn an_unknown_build_id_is_left_out() {
        let body = render("1.2.3", None, "x86_64", Variant::Minimal, "alpine");
        assert!(!body.contains("BUILD_ID"), "{body}");
        let body = render(
            "1.2.3",
            Some("deadbeef"),
            "x86_64",
            Variant::Minimal,
            "alpine",
        );
        assert_eq!(get(&body, "BUILD_ID").unwrap(), "deadbeef");
    }

    /// The whole point: the tap's file loses.
    #[test]
    fn it_replaces_the_file_the_package_tap_left() {
        let root = scratch("replace");
        let etc = root.join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("os-release"),
            "PRETTY_NAME=\"Ubuntu Stonking\"\nNAME=\"Ubuntu\"\nID=ubuntu\n",
        )
        .unwrap();

        install(&root, "x86_64", Variant::Minimal);

        let body = fs::read_to_string(etc.join("os-release")).unwrap();
        assert_eq!(get(&body, "ID").unwrap(), "eclipse");
        assert!(!body.contains("Ubuntu"), "{body}");
    }

    /// Ubuntu's `base-files` ships `/etc/os-release` as a symlink into
    /// `/usr/lib`. Writing through that link would have left our text in
    /// *their* file and `/etc/os-release` pointing out of `/etc` -- and the
    /// file would still have read correctly, so nothing would have complained.
    #[test]
    fn it_does_not_write_through_the_taps_symlink() {
        let root = scratch("symlink");
        let etc = root.join("etc");
        let usr_lib = root.join("usr/lib");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&usr_lib).unwrap();
        fs::write(usr_lib.join("os-release"), "ID=ubuntu\n").unwrap();
        unix::fs::symlink("../usr/lib/os-release", etc.join("os-release")).unwrap();

        install(&root, "x86_64", Variant::Desktop);

        let meta = fs::symlink_metadata(etc.join("os-release")).unwrap();
        assert!(
            meta.file_type().is_file(),
            "/etc/os-release is still a symlink into /usr/lib"
        );
        let body = fs::read_to_string(etc.join("os-release")).unwrap();
        assert_eq!(get(&body, "ID").unwrap(), "eclipse");
    }

    /// `/usr/lib/os-release` is the spec's fallback path, so it has to exist
    /// and resolve -- and it has to resolve to OUR file, not be a second copy
    /// that can drift from it.
    #[test]
    fn the_fallback_path_points_at_our_file() {
        let root = scratch("fallback");
        install(&root, "aarch64", Variant::Minimal);

        let link = root.join("usr/lib/os-release");
        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "/usr/lib/os-release is not a link"
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../../etc/os-release")
        );
        // And it resolves, relatively, from where it lives.
        assert_eq!(
            fs::read_to_string(&link).unwrap(),
            fs::read_to_string(root.join("etc/os-release")).unwrap()
        );
    }

    /// Running the install twice (an incremental `make image` over a rootfs
    /// that already has ours) must leave the same thing, not a link pointing
    /// at a link or a file full of two copies.
    #[test]
    fn installing_twice_is_the_same_as_once() {
        let root = scratch("twice");
        install(&root, "x86_64", Variant::Minimal);
        let first = fs::read_to_string(root.join("etc/os-release")).unwrap();
        install(&root, "x86_64", Variant::Minimal);
        let second = fs::read_to_string(root.join("etc/os-release")).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read_link(root.join("usr/lib/os-release")).unwrap(),
            PathBuf::from("../../etc/os-release")
        );
    }

    /// The quoting rule, both ways: a bare token stays bare (every
    /// distribution's file looks like that, and a `cut -d= -f2` reader expects
    /// it), and anything with a space gets quotes.
    #[test]
    fn values_are_quoted_only_when_they_need_to_be() {
        assert_eq!(line("ID", "eclipse"), "ID=eclipse\n");
        assert_eq!(line("VERSION_ID", "0.6.2"), "VERSION_ID=0.6.2\n");
        assert_eq!(line("NAME", "Eclipse OS"), "NAME=\"Eclipse OS\"\n");
        assert_eq!(
            line("ID_LIKE", "ubuntu debian"),
            "ID_LIKE=\"ubuntu debian\"\n"
        );
        // A URL carries `:` and `/`; `/` is not in the bare set, so it quotes.
        assert_eq!(
            line("HOME_URL", "https://example.invalid/x"),
            "HOME_URL=\"https://example.invalid/x\"\n"
        );
        // And the four characters the spec says must be escaped inside quotes.
        assert_eq!(line("K", "a $b `c\" \\d"), "K=\"a \\$b \\`c\\\" \\\\d\"\n");
    }

    /// Every line of the rendered file is one assignment with an uppercase
    /// key: the shape the spec describes, and what a parser that is not a
    /// shell (most of them) assumes.
    #[test]
    fn every_line_is_one_uppercase_assignment() {
        let body = sample();
        assert!(body.ends_with('\n'), "no trailing newline:\n{body}");
        for (k, _) in fields(&body) {
            assert!(
                !k.is_empty()
                    && k.bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    && !k.starts_with(|c: char| c.is_ascii_digit()),
                "{k:?} is not an os-release key"
            );
        }
        // No key twice: a duplicate makes shell and non-shell readers disagree
        // about which one wins.
        let mut keys: Vec<String> = fields(&body).into_iter().map(|(k, _)| k).collect();
        let total = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), total, "a key is written twice:\n{body}");
    }
}
