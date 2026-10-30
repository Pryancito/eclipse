//! What a test can check here without a rootfs: that the files we write say
//! the same thing the rest of the image says, and that the `localedef`
//! invocation points at the guest's data rather than the host's. Whether the
//! archive actually resolves is a guest-side fact; it is checked by booting
//! the image (`locale` printing no "Cannot set LC_CTYPE").

use super::*;

/// Every generated locale has to appear in `/etc/locale.gen` in the format
/// Debian's `locale-gen` parses, or a user editing that file to add a third
/// one gets a file that contradicts the image.
#[test]
fn locale_gen_lists_every_wanted_locale_in_debian_format() {
    let body = locale_gen_body();
    for (name, charmap) in WANTED {
        let line = format!("{name}.{charmap} {charmap}");
        assert!(
            body.lines().any(|l| l == line),
            "/etc/locale.gen is missing {line:?}:\n{body}"
        );
    }
    for line in body
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let (locale, charmap) = line.split_once(' ').expect("`<locale> <charmap>`");
        assert!(
            locale.ends_with(charmap),
            "{line:?}: the locale name must carry its own charmap suffix"
        );
    }
}

/// The locale we *generate* and the locale `/etc/profile` and `eclipse-locale`
/// *export* are two different places in this tree, and the bug being fixed is
/// precisely what happens when they disagree. Read the shipped script instead
/// of trusting a second copy of the string.
#[test]
fn the_generated_lang_is_the_one_the_desktop_exports() {
    let src = include_str!("../desktop.rs");
    assert!(
        src.contains(DEFAULT_LANG),
        "desktop.rs no longer exports {DEFAULT_LANG}; the image would declare a \
         locale that nothing generates"
    );
    assert!(
        WANTED
            .iter()
            .any(|(name, charmap)| DEFAULT_LANG == format!("{name}.{charmap}")),
        "{DEFAULT_LANG} is not in WANTED, so it is declared but never compiled"
    );
}

/// `LANGUAGE` is a colon-separated gettext list, and each entry a language
/// code -- not a locale name. `es:en`, never `es_ES.UTF-8`.
#[test]
fn locale_conf_carries_lang_and_a_gettext_language_list() {
    let body = locale_conf_body();
    assert!(body.contains(&format!("LANG={DEFAULT_LANG}\n")), "{body}");
    for part in DEFAULT_LANGUAGE.split(':') {
        assert!(
            !part.is_empty() && !part.contains('.') && !part.contains('_'),
            "LANGUAGE entry {part:?} is a locale name, not a language code"
        );
    }
}

/// The whole point of invoking the loader by hand is that nothing resolves to
/// the host: the loader, the library path, the binary, `I18NPATH` and
/// `GCONV_PATH` all have to sit under the rootfs, and the output has to go
/// there too via `--prefix`.
#[test]
fn localedef_runs_entirely_inside_the_rootfs() {
    let rootfs = Path::new("/tmp/eclipse-locales-test-root");
    let run = localedef_command(rootfs, "amd64", "es_ES", "UTF-8").expect("amd64 has a loader");
    assert!(
        run.loader.starts_with(rootfs),
        "the loader is the host's: {}",
        run.loader.display()
    );
    let joined = run.args.join(" ");
    assert!(
        joined.contains(&format!("--prefix={}", rootfs.display())),
        "without --prefix the archive lands on the host: {joined}"
    );
    for want in ["--library-path", "usr/bin/localedef", "es_ES.UTF-8"] {
        assert!(joined.contains(want), "missing {want:?} in: {joined}");
    }
    let keys: Vec<&str> = run.envs.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"I18NPATH"), "{keys:?}");
    assert!(keys.contains(&"GCONV_PATH"), "{keys:?}");
    for (key, path) in &run.envs {
        assert!(
            path.starts_with(rootfs),
            "{key} points outside the rootfs: {}",
            path.display()
        );
    }
}

/// The loader's name is architecture-specific and getting it wrong is a silent
/// skip, not an error. riscv64 is the musl rootfs and deliberately has none.
#[test]
fn every_apt_arch_has_a_loader_and_the_musl_one_does_not() {
    for arch in ["x86_64", "aarch64"] {
        let deb = apt_rootfs::deb_arch(arch).expect("apt arch");
        let loader = loader_rel(deb).unwrap_or_else(|| panic!("no loader for {arch}"));
        assert!(
            loader.starts_with("usr/lib/") && loader.contains("ld-linux-"),
            "{arch}: {loader} is not a glibc loader path"
        );
    }
    assert!(loader_rel("riscv64").is_none());
}

/// A cross build cannot run the guest's `localedef`, and claiming otherwise
/// would write an archive in the host's byte order.
#[test]
fn generation_is_refused_when_the_host_arch_differs() {
    assert!(host_can_run(std::env::consts::ARCH));
    let other = if std::env::consts::ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    assert!(!host_can_run(other));
}

/// Writing the configuration is not conditional on being able to generate:
/// even the musl rootfs gets a coherent `/etc/locale.conf` and the Debian
/// spelling of it, and the two must never say different things.
#[test]
fn the_two_spellings_of_the_config_are_written_and_identical() {
    let dir = std::env::temp_dir().join(format!("eclipse-locales-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    install(&dir, "riscv64");
    let conf = fs::read_to_string(dir.join("etc/locale.conf")).unwrap();
    let debian = fs::read_to_string(dir.join("etc/default/locale")).unwrap();
    assert_eq!(
        conf, debian,
        "/etc/locale.conf and /etc/default/locale differ"
    );
    assert!(conf.contains(DEFAULT_LANG), "{conf}");
    assert!(dir.join("etc/locale.gen").is_file());
    let _ = fs::remove_dir_all(&dir);
}
