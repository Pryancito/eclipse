//! What the bootstrap says when a package does not reach the rootfs.
//!
//! The failure this covers: `make image` on a desktop rootfs downloaded 68
//! essential `.deb`s ("68 package(s) ok"), printed "merged Ubuntu stonking
//! runtime", and then panicked a thousand lines later in `verify.rs` with
//! "falta /usr/bin/apt o apt-get tras el bootstrap" — the one sentence that
//! cannot say *why*. `extract_debs` collapsed 68 outcomes into one `bool`
//! that was true as soon as a single `.deb` unpacked, so a package that was
//! never in the cache and a package whose `dpkg-deb -x` died looked the same
//! as one that worked.
//!
//! These tests pin the three verdicts and the gap report that prints them
//! next to the extraction, where the reason is still on screen.

use super::*;

/// A directory of its own for one test, named after it.
fn a_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("eclipse-apt-report-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).expect("a directory of its own");
    p
}

/// `path`, with the parents it needs, holding something.
fn file_with(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("a parent directory");
    }
    fs::write(path, "x").expect("a file");
}

fn report_of(extracted: &[&str], missing: &[&str], failed: &[(&str, &str, &str)]) -> ExtractReport {
    ExtractReport {
        extracted: extracted.iter().map(|s| (*s).to_string()).collect(),
        missing: missing.iter().map(|s| (*s).to_string()).collect(),
        failed: failed
            .iter()
            .map(|(p, f, e)| ((*p).to_string(), (*f).to_string(), (*e).to_string()))
            .collect(),
    }
}

#[test]
fn verdict_tells_the_three_outcomes_apart() {
    let r = report_of(
        &["dpkg"],
        &["hello"],
        &[("apt", "apt_3.1.5_amd64.deb", "tar: unexpected EOF")],
    );

    let apt = r.verdict("apt");
    assert!(
        apt.contains("apt_3.1.5_amd64.deb") && apt.contains("tar: unexpected EOF"),
        "a failed extraction names the file and what dpkg-deb said: {apt}"
    );
    assert!(
        r.verdict("hello").contains("cache"),
        "a package with no .deb points at the cache, not at the merge"
    );
    assert!(
        r.verdict("dpkg").contains("merge"),
        "a package that unpacked and still has no file lost it in the merge"
    );
    assert!(
        r.verdict("gpgv").contains("gpgv"),
        "and a package nobody asked for says so by name"
    );
}

#[test]
fn any_is_false_only_when_nothing_unpacked() {
    assert!(!report_of(&[], &["apt"], &[]).any());
    assert!(report_of(&["dpkg"], &["apt"], &[]).any());
}

#[test]
fn gaps_name_every_missing_file_and_count_them() {
    let root = a_dir("gaps");
    // dpkg landed whole; apt and hello did not.
    file_with(&root.join("usr/bin/dpkg"));
    file_with(&root.join("usr/share/dpkg/cputable"));

    let r = report_of(
        &["dpkg"],
        &["hello"],
        &[("apt", "apt_3.1.5_amd64.deb", "boom")],
    );
    assert_eq!(
        report_bootstrap_gaps(&root, &root.join("cache"), &r),
        2,
        "/usr/bin/apt and hello are the two holes; dpkg's two files are there"
    );

    file_with(&root.join("usr/bin/apt-get"));
    assert_eq!(
        report_bootstrap_gaps(&root, &root.join("cache"), &r),
        1,
        "apt-get satisfies the apt entry on its own, the way verify reads it"
    );

    file_with(&root.join("bin/hello"));
    assert_eq!(
        report_bootstrap_gaps(&root, &root.join("cache"), &r),
        0,
        "and hello counts from /bin too"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_gap_names_every_path_the_entry_would_have_accepted() {
    // Naming only the first alternative sends the reader looking for a file
    // that was never required on its own.
    assert_eq!(any_of(&["usr/bin/dpkg"]), "/usr/bin/dpkg");
    assert_eq!(
        any_of(&["usr/bin/apt", "usr/bin/apt-get"]),
        "cualquiera de /usr/bin/apt o /usr/bin/apt-get"
    );
}

#[test]
fn every_must_have_file_is_one_verify_checks() {
    // The point of the list is to pre-empt `verify::check_apt`. If that
    // function grows a check, this list is where the reason comes from.
    let paths: Vec<&str> = BOOTSTRAP_MUST_HAVE
        .iter()
        .flat_map(|(p, _)| p.iter().copied())
        .collect();
    for want in ["usr/bin/apt", "usr/bin/apt-get", "usr/bin/dpkg"] {
        assert!(paths.contains(&want), "{want} is a verify check");
    }
    for (_, pkg) in BOOTSTRAP_MUST_HAVE {
        assert!(
            BOOTSTRAP_PKGS.contains(pkg),
            "{pkg} ships a must-have file, so the bootstrap has to download it"
        );
    }
}

#[test]
fn deb_owner_does_not_confuse_a_prefix_with_a_package() {
    let pkgs = ["apt", "apt-utils", "libssl4"];
    assert_eq!(deb_owner("apt_3.1.5_amd64.deb", &pkgs), Some("apt"));
    assert_eq!(
        deb_owner("apt-utils_3.1.5_amd64.deb", &pkgs),
        Some("apt-utils"),
        "apt-utils is its own package, not a download of apt"
    );
    assert_eq!(
        deb_owner("apt-transport-https_3.1.5_all.deb", &pkgs),
        None,
        "and a .deb nobody asked for belongs to nobody"
    );
}

#[test]
fn first_line_survives_an_empty_stderr() {
    assert_eq!(first_line("\n\n  tar: boom\nmas\n"), "tar: boom");
    assert_eq!(first_line("   \n"), "sin mensaje");
}

/// The real bootstrap, against the real Ubuntu archive. Network + `dpkg-deb`,
/// so it never runs in CI: `cargo test -p xtask -- --ignored bootstrap_de_verdad`.
#[test]
#[ignore]
fn bootstrap_de_verdad_deja_apt_en_la_rootfs() {
    let root = a_dir("real");
    bootstrap(&root, "x86_64");
    let apt = root.join("usr/bin/apt").is_file() || root.join("usr/bin/apt-get").is_file();
    // A whole rootfs is hundreds of MiB: clear it before the assert, or a
    // failing run leaves it in /tmp for good.
    let _ = fs::remove_dir_all(&root);
    assert!(apt, "no hay apt en {}", root.display());
}
