//! Which archive the apt bootstrap downloads from, and whose indexes decide.
//!
//! Every `apt-get` and `apt-cache` the bootstrap runs is pinned to [`DISTRO`]
//! by the `-o Dir::*` flags that `apt_suite_opts` builds, against an isolated
//! state directory under the download cache. Almost none of that had a test:
//! the one assertion on the list checked that a value starting with `/` is
//! absolute, which is true of any string starting with `/`.
//!
//! The flags are not decoration. The host running the build is normally a
//! different Ubuntu, or a derivative's rebuild of one, and every flag left
//! out hands one decision back to it: which suite a package name resolves
//! in, which versions already count as installed, where a downloaded `.deb`
//! lands. None of those shows up as a build error. What comes out is a
//! rootfs full of the host's packages, or an empty one.

use super::*;

/// A download cache of its own for one test, named after it.
fn a_cache(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("eclipse-apt-pin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).expect("a cache directory of its own");
    p
}

/// `target` written relative to the process working directory, so that
/// anything handed to apt has to be absolutised before it is any use.
fn relative_to_cwd(target: &Path) -> PathBuf {
    let cwd = std::env::current_dir().expect("a working directory");
    let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut rel = PathBuf::new();
    for _ in cwd.components().skip(1) {
        rel.push("..");
    }
    for c in target.components().skip(1) {
        rel.push(c);
    }
    assert!(
        !rel.is_absolute(),
        "the whole point of this path is that it is relative: {}",
        rel.display()
    );
    rel
}

/// The value of one `-o` option, by its key.
fn opt<'a>(opts: &'a [String], key: &str) -> &'a str {
    let prefix = format!("{key}=");
    opts.iter()
        .find_map(|o| o.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {key} among {opts:?}"))
}

/// amd64 comes from the main archive and everything else from ports, which
/// is where Ubuntu actually keeps arm64. Swapping the two does not fail the
/// build: `apt-get download` simply finds no candidate, and the arm64 rootfs
/// comes out with no Ubuntu userspace in it at all.
#[test]
fn arm64_is_fetched_from_ports_and_amd64_from_the_main_archive() {
    assert_eq!(mirror("amd64"), "http://archive.ubuntu.com/ubuntu");
    assert_eq!(mirror("arm64"), "http://ports.ubuntu.com/ubuntu-ports");
    assert_ne!(mirror("amd64"), mirror("arm64"));
}

/// And the sources list written for an architecture names that mirror, so
/// the choice reaches apt instead of stopping at the helper.
#[test]
fn the_sources_list_names_the_mirror_of_its_own_architecture() {
    let cache = a_cache("mirror-amd64");
    let amd = fs::read_to_string(write_suite_sources(&cache, "amd64")).expect("a sources list");
    assert!(amd.contains("archive.ubuntu.com"), "{amd}");
    assert!(!amd.contains("ports.ubuntu.com"), "{amd}");
    assert!(amd.contains("arch=amd64"), "{amd}");
    assert!(amd.contains(DISTRO), "{amd}");
    let _ = fs::remove_dir_all(&cache);

    let cache = a_cache("mirror-arm64");
    let arm = fs::read_to_string(write_suite_sources(&cache, "arm64")).expect("a sources list");
    assert!(arm.contains("ports.ubuntu.com"), "{arm}");
    assert!(!arm.contains("archive.ubuntu.com"), "{arm}");
    assert!(arm.contains("arch=arm64"), "{arm}");
    assert!(!arm.contains("arch=amd64"), "{arm}");
    let _ = fs::remove_dir_all(&cache);
}

/// A relative `Dir::` value is not ignored by apt, it is resolved under
/// `/etc/apt` or `/var/lib/apt` by apt itself, which is how a host on
/// another suite once silently ignored the sources list written here.
#[test]
fn a_relative_path_is_absolutised_and_an_absolute_one_is_left_alone() {
    let already = Path::new("/var/tmp/eclipse-apt-cache");
    assert_eq!(abs(already), already);

    let relative = Path::new("ignored/apt-cache");
    let made = abs(relative);
    assert!(made.is_absolute(), "{}", made.display());
    assert!(made.ends_with(relative), "{}", made.display());
}

/// The same thing where it matters: with a relative cache directory, every
/// path in the option list still comes out absolute. This is the assertion
/// the old one only looked like -- it was conditioned on the value already
/// starting with a slash, so a helper that absolutised nothing passed it.
#[test]
fn every_apt_directory_option_is_absolute_even_from_a_relative_cache() {
    let cache = a_cache("relative-cache");
    let opts = apt_suite_opts(&relative_to_cwd(&cache), "amd64");
    let dirs: Vec<&String> = opts.iter().filter(|o| o.starts_with("Dir:")).collect();
    assert!(dirs.len() >= 6, "{opts:?}");
    for o in dirs {
        let (_, path) = o.split_once('=').expect("an option is key=value");
        assert!(
            Path::new(path).is_absolute(),
            "apt resolves a relative Dir value under its own etc: {o}"
        );
    }
    let _ = fs::remove_dir_all(&cache);
}

/// And they all point inside the isolated state, not at the host's own apt.
/// `Dir::State::status` is the one that bites hardest: left at its default
/// the host's installed versions become the candidates, and those are
/// rebuilds the Ubuntu archive has never heard of.
#[test]
fn the_options_point_at_the_isolated_state_and_not_at_the_host() {
    let cache = a_cache("isolated-state");
    let opts = apt_suite_opts(&cache, "amd64");
    let state = abs(&apt_state_dir(&cache));
    for key in [
        "Dir::Etc::sourcelist",
        "Dir::Etc::sourceparts",
        "Dir::Etc::preferencesparts",
        "Dir::State::lists",
        "Dir::State::status",
        "Dir::Cache",
    ] {
        let path = opt(&opts, key);
        assert!(
            Path::new(path).starts_with(&state),
            "{key} escapes the state under {}: {path}",
            state.display()
        );
    }
    assert_ne!(opt(&opts, "Dir::State::status"), "/var/lib/dpkg/status");
    let _ = fs::remove_dir_all(&cache);
}

/// Only the architecture being built is pinned. A second one left in the
/// list would let apt satisfy a dependency with a package the target cannot
/// run, and the build would say nothing about it.
#[test]
fn only_the_target_architecture_is_pinned() {
    let cache = a_cache("arch-pin");
    let opts = apt_suite_opts(&cache, "arm64");
    assert_eq!(opt(&opts, "APT::Architecture"), "arm64");
    assert_eq!(opt(&opts, "APT::Architectures::"), "arm64");
    assert!(
        !opts.iter().any(|o| o.contains("amd64")),
        "an architecture the target cannot run must not resolve: {opts:?}"
    );
    assert!(
        opts.iter().any(|o| o == "Acquire::Languages=none"),
        "translation indexes are megabytes of nothing: {opts:?}"
    );
    let _ = fs::remove_dir_all(&cache);
}

/// Every invocation carries the whole list, in `-o` pairs, and runs in the C
/// locale. The locale is load bearing: apt-cache translates the `Depends:`
/// label, and the parser downstream keys on the English one, so a localised
/// host turns a dependency closure into an empty one.
#[test]
fn the_apt_command_runs_in_the_c_locale_and_carries_the_whole_pin() {
    let cache = a_cache("apt-cmd");
    let opts = apt_suite_opts(&cache, "amd64");
    let cmd = apt_cmd("apt-cache", &cache, "amd64");
    assert_eq!(cmd.get_program().to_string_lossy(), "apt-cache");

    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for key in ["LC_ALL", "LANG"] {
        assert!(
            envs.contains(&(key.to_string(), Some(String::from("C")))),
            "{key} must be C or the Depends label comes back translated: {envs:?}"
        );
    }

    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args.len(), opts.len() * 2, "{args:?}");
    for (i, o) in opts.iter().enumerate() {
        assert_eq!(args[i * 2], "-o", "{args:?}");
        assert_eq!(&args[i * 2 + 1], o, "{args:?}");
    }
    let _ = fs::remove_dir_all(&cache);
}

/// When the pin changes, the cache filled under the old one is dropped. A
/// `.deb` downloaded for another suite stays a valid file forever otherwise,
/// and keeps answering yes to "is this package already here?" while naming a
/// library version the new suite does not ship.
#[test]
fn changing_the_pin_drops_the_debs_cached_under_the_old_one() {
    let cache = a_cache("pin-change");
    write_suite_sources(&cache, "amd64");
    let stale = cache.join("labwc_0.8.4-1_amd64.deb");
    fs::write(&stale, b"built against another wlroots").expect("a cached deb");
    let keep = cache.join("notes.txt");
    fs::write(&keep, b"not a package").expect("a file that is not a deb");

    write_suite_sources(&cache, "arm64");

    assert!(
        !stale.exists(),
        "a deb fetched under the old pin must not satisfy the new one"
    );
    assert!(keep.is_file(), "and only the packages are dropped");
    assert!(
        cache.join(".apt/lists/partial").is_dir(),
        "the index is rebuilt, not left missing"
    );
    let _ = fs::remove_dir_all(&cache);
}

/// The other half, which is what makes a second build cheap: an unchanged
/// pin keeps everything already downloaded.
#[test]
fn rewriting_the_same_pin_keeps_the_cache_it_already_filled() {
    let cache = a_cache("pin-same");
    write_suite_sources(&cache, "amd64");
    let deb = cache.join("dpkg_1.22.11ubuntu4_amd64.deb");
    fs::write(&deb, b"a whole package").expect("a cached deb");

    write_suite_sources(&cache, "amd64");

    assert!(deb.is_file(), "every build would re-download the lot");
}

/// The isolated dpkg status starts empty, and is not emptied again: what the
/// staging root has already unpacked is recorded there, and a build that
/// forgot it between packages would reinstall the whole closure each time.
#[test]
fn the_isolated_dpkg_status_is_created_empty_and_then_left_alone() {
    let cache = a_cache("dpkg-status");
    write_suite_sources(&cache, "amd64");
    let status = cache.join(".apt/dpkg/status");
    assert_eq!(
        fs::read_to_string(&status).expect("a status file"),
        "",
        "the host's installed packages are not ours"
    );

    fs::write(&status, "Package: dpkg\nStatus: install ok installed\n").expect("a record");
    write_suite_sources(&cache, "amd64");
    assert!(
        fs::read_to_string(&status)
            .expect("a status file")
            .contains("Package: dpkg"),
        "what the staging root unpacked has to survive the next call"
    );
    assert_eq!(
        fs::read_to_string(cache.join(".apt/dpkg/arch"))
            .expect("an arch file")
            .trim(),
        "amd64"
    );
    let _ = fs::remove_dir_all(&cache);
}

/// The cache census is by name and extension, and both halves matter: a
/// sibling package whose name merely starts the same way is not this one,
/// and a half-written download is not a package.
#[test]
fn only_a_deb_named_for_the_package_itself_counts_as_cached() {
    let cache = a_cache("cache-census");
    for name in [
        "labwc_0.9.1-1_amd64.deb",
        "labwc_0.9.0-1_amd64.deb",
        "labwc-tweaks_0.1-1_amd64.deb",
        "liblabwc_1_amd64.deb",
        "labwc_0.9.1-1_amd64.deb.part",
        "labwc",
    ] {
        fs::write(cache.join(name), b"x").expect("a file in the cache");
    }

    let mut found: Vec<String> = cached_debs_for(&cache, "labwc")
        .iter()
        .map(|p| {
            p.file_name()
                .expect("a cached file has a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        ["labwc_0.9.0-1_amd64.deb", "labwc_0.9.1-1_amd64.deb"],
        "neither a sibling package nor a half-written download is this package"
    );
    assert!(cached_debs_for(&cache, "firefox").is_empty());
    let _ = fs::remove_dir_all(&cache);
}

/// And the "is there anything at all here?" question, which decides whether
/// a failed download run still has something to extract.
#[test]
fn a_cache_with_no_package_in_it_says_so_and_so_does_one_that_is_not_there() {
    let cache = a_cache("cache-empty");
    assert!(!cache_has_any_deb(&cache), "an empty directory has nothing");
    fs::write(cache.join("gpgv_2.4.4_amd64.deb.tmp"), b"x").expect("a partial download");
    assert!(
        !cache_has_any_deb(&cache),
        "a partial download is not a deb"
    );
    fs::write(cache.join("gpgv_2.4.4_amd64.deb"), b"x").expect("a package");
    assert!(cache_has_any_deb(&cache));
    assert!(
        !cache_has_any_deb(&cache.join("never-created")),
        "a cache that was never made is empty, not an error"
    );
    let _ = fs::remove_dir_all(&cache);
}

/// The multiarch tuple names the directory the glibc loader is hunted for in
/// before the bootstrap gives up on it. Getting arm64 wrong leaves the arm64
/// rootfs with no `ld-linux`, so every dynamic binary in it fails to start.
#[test]
fn the_multiarch_tuple_follows_the_target_and_falls_back_to_x86() {
    assert_eq!(gnu_tuple("amd64"), "x86_64-linux-gnu");
    assert_eq!(gnu_tuple("arm64"), "aarch64-linux-gnu");
    assert_eq!(gnu_tuple("riscv64"), "x86_64-linux-gnu");
}
