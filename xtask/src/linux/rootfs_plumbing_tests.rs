//! What `eclipse-init` actually boots, and the plumbing the rootfs needs
//! before it can: the service table crossed against the wrappers the image
//! installs, `PATH` assembly, `/lib` selection, the base accounts and the
//! files under `/etc` that nothing else writes.
//!
//! Moved out of `mod.rs` by the size ratchet: these tests are where this
//! area grows, and `mod.rs` had 35 lines left under its own ceiling.

use super::*;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "eclipse-rootfs-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The applet list is the FALLBACK for when `busybox --list` cannot run on
/// the host, which is every cross build -- so on aarch64 and riscv64 it is
/// the whole list. An applet missing from it has no link at all in those
/// images, and every caller in the rootfs's own scripts swallows the
/// failure: `$(cmd)` substitutes empty and the rest send stderr to
/// /dev/null. That is how eight of these went unnoticed: `tr` in the card0
/// vendor check (so the NVIDIA branch of /etc/profile could never be taken),
/// and `stty`/`printf`/`dd` in the terminal-size probe -- including the
/// final `printf '\e[H'` whose own comment says that without it the
/// installer "would look like a hung black screen".
#[test]
fn applets_cover_what_the_generated_scripts_call() {
    for (applet, used_by) in [
            ("tr", "/etc/profile: the card0 vendor check that picks the renderer"),
            ("printf", "/etc/profile: the cursor query of the TTY-size probe, and the \\e[H that homes the console"),
            ("stty", "/etc/profile: raw mode for the TTY-size probe"),
            ("dd", "/etc/profile: reads the terminal's reply to the probe"),
            ("basename", "eclipse-init: turns the socket path into WAYLAND_DISPLAY"),
            ("dirname", "eclipse-xkbmap and eclipse-look: the mkdir -p of the file they upsert"),
            ("pkill", "eclipse-look and lunarrun: respawn the panel, refuse a second instance"),
            ("setsid", "eclipse-init: detaches a oneshot so it does not hold up the panel"),
            // The ones the list has always had, so the test also notices a
            // deletion. `awk` and `grep` parse /etc/eclipse/*; `sh` is every
            // script's interpreter; `wget` is how apk reaches a mirror.
            ("awk", "/etc/profile: reads the language and timezone out of /etc/eclipse"),
            ("grep", "/etc/profile: reads the nvidia.* flags out of /proc/cmdline"),
            ("sh", "the interpreter of every script written into the rootfs"),
            ("wget", "apk's downloader, and how the IWADs and the CA bundle arrive"),
        ] {
            assert!(
                LinuxRootfs::BASE_APPLETS.contains(&applet),
                "`{applet}` has no applet link on a cross build, and it is used by {used_by}"
            );
        }
}

/// The other half of the pairing above: the test only means something while
/// the probe really is written with those commands. If it is rewritten, this
/// fails and whoever rewrote it updates both halves instead of leaving a
/// list that guards commands nobody calls any more.
#[test]
fn the_terminal_size_probe_still_uses_stty_printf_and_dd() {
    let etc = scratch("profile").join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_profile(&etc);
    let profile = fs::read_to_string(etc.join("profile")).unwrap();
    for fragment in [
        "stty raw -echo min 0 time 3",
        "printf '\\033[999;999H\\033[6n'",
        "dd bs=32 count=1",
        "printf '\\033[H'",
        "tr -d '[:space:]' < /sys/class/drm/card0/device/vendor",
    ] {
        assert!(
            profile.contains(fragment),
            "/etc/profile no longer contains {fragment:?}; \
                 applets_cover_what_the_generated_scripts_call guards it for nothing"
        );
    }
    let _ = fs::remove_dir_all(etc.parent().unwrap());
}

/// Two processes died of `SIGPIPE` on every boot -- `ip` at ~2.7 s and
/// `busybox` at ~4.1 s -- and both came out of this one script: a probe
/// written `writer | grep -q PATTERN` kills the writer the moment `grep`
/// matches, and the kernel records every default-disposition death in the
/// dmesg ring at `error!`. Each probe now reads a file, so the writer
/// reaches EOF and exits 0.
///
/// The assertion is on the shape, not on the two commands: any new
/// `| grep -q` in here brings the error lines straight back.
#[test]
fn the_ntpd_probes_never_pipe_into_a_reader_that_exits_early() {
    let dir = scratch("ntpd");
    fs::create_dir_all(&dir).unwrap();
    LinuxRootfs::write_ntp(&dir);
    let script = fs::read_to_string(dir.join("usr/local/bin/eclipse-ntpd")).unwrap();

    for line in script.lines() {
        // A comment may quote the old form; only real commands matter.
        if line.trim_start().starts_with('#') {
            continue;
        }
        assert!(
            !line.contains('|') || !line.contains("grep -q"),
            "`{}` pipes into `grep -q`: the writer gets SIGPIPE as soon as \
                 grep matches, which the kernel logs as a process killed by a \
                 signal. Redirect to a file and grep the file.",
            line.trim()
        );
    }
    // And the two probes really are still there, so the test above is not
    // passing because the script lost them.
    assert!(
        script.contains("grep -qx ntpd /tmp/ntpd-applets"),
        "the busybox-applet probe is gone from eclipse-ntpd:\n{script}"
    );
    assert!(
        script.contains("grep -q '^default' /tmp/ntpd-routes"),
        "the default-route probe is gone from eclipse-ntpd:\n{script}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Every applet link has to be RELATIVE and name `busybox`: the rootfs is
/// built on the host and mounted at a different root in the guest, and
/// busybox picks its applet from `basename(argv[0])`, so the link's own name
/// is what selects the applet.
#[test]
fn every_applet_gets_a_relative_link_to_busybox() {
    let dir = scratch("applets");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    LinuxRootfs::ensure_busybox_applets(&bin);

    for applet in LinuxRootfs::BASE_APPLETS {
        let link = bin.join(applet);
        assert!(link.is_symlink(), "no link for the `{applet}` applet");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("busybox"),
            "the `{applet}` link must be a relative link to busybox"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The doc comment promises that "existing entries (real binaries like
/// `nl_dump`) are never overwritten", and the whole rootfs depends on it:
/// several applet names are also the names of programs Eclipse builds and
/// installs itself, and replacing one with a busybox link would silently
/// swap the program for a completely different command.
#[test]
fn a_real_binary_keeps_its_place_and_so_does_a_dangling_link() {
    let dir = scratch("keep");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("ls"), b"#!/bin/sh\necho not busybox\n").unwrap();
    unix::fs::symlink("nowhere-at-all", bin.join("ps")).unwrap();

    LinuxRootfs::ensure_busybox_applets(&bin);

    assert!(
        !bin.join("ls").is_symlink(),
        "a real binary was replaced by an applet link"
    );
    assert_eq!(
        fs::read_link(bin.join("ps")).unwrap(),
        Path::new("nowhere-at-all"),
        "an existing symlink was repointed at busybox"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The list is data, and duplicated data hides an edit: the complement loop
/// skips a name it already has, so a duplicate never shows up as a second
/// link and nothing else would ever say it is there.
#[test]
fn the_applet_list_has_no_duplicates() {
    let mut seen = std::collections::BTreeSet::new();
    for applet in LinuxRootfs::BASE_APPLETS {
        assert!(seen.insert(*applet), "`{applet}` is listed twice");
    }
}

/// What goes into the rootfs is the RAW link target, and the rootfs is
/// mounted at a different root in the guest. A relative target survives the
/// move untouched, which is the normal case and must not be rewritten.
#[test]
fn a_relative_symlink_target_is_kept_as_it_was() {
    let dir = scratch("relative");
    fs::write(dir.join("libc.so"), b"").unwrap();
    let link = dir.join("ld-musl-x86_64.so.1");
    unix::fs::symlink("libc.so", &link).unwrap();

    assert_eq!(
        rootfs_link_target(&link),
        Some(PathBuf::from("libc.so")),
        "a relative target is already correct in the guest"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// An absolute target that names a sibling in the same lib directory says
/// the same thing as its file name, and that sibling is installed by the
/// same pass -- so the name is the rewrite, and the link resolves in the
/// guest instead of pointing back at the host's toolchain.
#[test]
fn an_absolute_target_in_the_same_directory_becomes_its_name() {
    let dir = scratch("absolute-sibling");
    fs::write(dir.join("libstdc++.so.6.0.30"), b"").unwrap();
    let link = dir.join("libstdc++.so.6");
    unix::fs::symlink(dir.join("libstdc++.so.6.0.30"), &link).unwrap();

    assert_eq!(
        rootfs_link_target(&link),
        Some(PathBuf::from("libstdc++.so.6.0.30")),
        "an absolute sibling must be rewritten to its name, not copied verbatim"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// And an absolute target that is NOT in the lib directory has nothing to be
/// rewritten to, so it is refused. Installing it verbatim is the bad case
/// this guards: a `/lib` entry that is visibly present and that the loader
/// says is not found, because the path it names only exists on the machine
/// that built the image.
#[test]
fn an_absolute_target_outside_the_directory_is_refused() {
    let dir = scratch("absolute-elsewhere");
    let elsewhere = dir.join("host-only");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("libbar.so.1"), b"").unwrap();
    let link = dir.join("libfoo.so.1");
    unix::fs::symlink(elsewhere.join("libbar.so.1"), &link).unwrap();

    assert_eq!(
        rootfs_link_target(&link),
        None,
        "a host path outside the lib directory must not be installed"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// And the case that caught the first version of the rewrite: when the
/// absolute target's last component is the LINK'S OWN name, rewriting to
/// that name points the link at itself. `exists()` says yes, because it
/// follows the link back to the host path -- so the sibling has to be
/// checked for being a different file, not just for existing. A
/// self-referencing symlink is ELOOP for every reader.
#[test]
fn an_absolute_target_that_would_point_the_link_at_itself_is_refused() {
    let dir = scratch("absolute-self");
    let elsewhere = dir.join("host-only");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("libfoo.so.1"), b"").unwrap();
    let link = dir.join("libfoo.so.1");
    unix::fs::symlink(elsewhere.join("libfoo.so.1"), &link).unwrap();

    assert_eq!(rootfs_link_target(&link), None);
    let _ = fs::remove_dir_all(&dir);
}

/// A dangling absolute link is the same case and must not be installed
/// either: `exists()` follows the link, so the sibling check answers no.
#[test]
fn a_dangling_absolute_target_is_refused() {
    let dir = scratch("absolute-dangling");
    let link = dir.join("libgone.so.1");
    unix::fs::symlink(dir.join("never-existed.so.1"), &link).unwrap();

    assert_eq!(rootfs_link_target(&link), None);
    let _ = fs::remove_dir_all(&dir);
}

/// An empty `PATH` is set-but-empty, and counting it as a first element put
/// a leading `:` in the result -- which every exec reads as THE CURRENT
/// DIRECTORY, so a build would look for its tools in whatever directory it
/// happened to be started from. Unset and empty have to agree.
#[test]
fn an_empty_inherited_path_does_not_become_the_current_directory() {
    let dir = scratch("path-empty");
    let want = dir.canonicalize().unwrap();

    let empty = join_path_env_from(Some(OsString::from("")), [&dir]);
    let unset = join_path_env_from(None, [&dir]);
    assert_eq!(empty, unset, "an empty PATH must behave like an unset one");
    assert_eq!(empty, want.as_os_str());
    assert!(
        !empty.to_string_lossy().starts_with(':'),
        "a leading empty PATH element means the current directory: {empty:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The inherited `PATH` comes first and the added directories after it, each
/// one absolute: they are handed to child builds that run in a different
/// working directory, where a relative entry means somewhere else.
#[test]
fn the_added_entries_follow_the_inherited_path_and_are_absolute() {
    let dir = scratch("path-order");
    let a = dir.join("a");
    let b = dir.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    let joined = join_path_env_from(Some(OsString::from("/inherited")), [&a, &b]);
    let joined = joined.to_string_lossy().into_owned();
    let mut parts = joined.split(':');
    assert_eq!(parts.next(), Some("/inherited"));
    for dir in [&a, &b] {
        let part = parts.next().unwrap();
        assert!(Path::new(part).is_absolute(), "{part} is not absolute");
        assert_eq!(Path::new(part), dir.canonicalize().unwrap());
    }
    assert_eq!(parts.next(), None);
    let _ = fs::remove_dir_all(&dir);
}

/// A directory that does not exist yet used to abort the whole build inside
/// `canonicalize().unwrap()`, naming neither the path nor `PATH` -- just "No
/// such file or directory (os error 2)". exec skips a `PATH` element that is
/// not there, so the build has no reason to stop.
#[test]
fn a_path_entry_that_does_not_exist_does_not_abort_the_build() {
    let dir = scratch("path-missing");
    let missing = dir.join("not-built-yet");

    let joined = join_path_env_from(Some(OsString::from("/inherited")), [&missing]);
    assert_eq!(
        joined.to_string_lossy(),
        format!("/inherited:{}", missing.display()),
        "the entry must still be there, just not canonicalized"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A `PATH` that is not valid UTF-8 is legal on Linux, and reading it as a
/// `String` dropped the whole inherited `PATH` silently -- the child then
/// found none of the host's tools and failed on the first one it needed.
#[test]
fn a_path_that_is_not_utf8_is_carried_through() {
    use std::os::unix::ffi::OsStringExt;
    let dir = scratch("path-bytes");
    let weird = OsString::from_vec(vec![b'/', 0xff, 0xfe, b'/', b'b', b'i', b'n']);

    let joined = join_path_env_from(Some(weird.clone()), [&dir]);
    let bytes = joined.clone().into_vec();
    assert!(
        bytes.starts_with(&weird.clone().into_vec()),
        "the inherited PATH was dropped: {joined:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// And an entry that needs resolving comes out RESOLVED, which is the half
/// of the promise the order test above cannot make: every path it hands in
/// is already canonical, so pushing each entry exactly as it came in
/// passes it. These go to child builds that run in a different working
/// directory, and `canonicalize` is what turns a path that only works from
/// here -- one reached through a `..`, or through a symlinked build
/// directory -- into one that works from anywhere.
#[test]
fn an_entry_that_needs_resolving_comes_out_canonical() {
    let dir = scratch("path-canonical");
    let real = dir.join("real");
    fs::create_dir_all(&real).unwrap();
    let want = real.canonicalize().unwrap();

    // The same directory, spelled two ways no exec resolves for itself.
    let through_parent = dir.join("real").join("..").join("real");
    let through_link = dir.join("link");
    unix::fs::symlink(&real, &through_link).unwrap();

    for spelling in [&through_parent, &through_link] {
        let joined = join_path_env_from(None, [spelling]);
        assert_eq!(
            Path::new(&joined),
            want,
            "{} went into PATH as it came in",
            spelling.display()
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// `check_so` decides what `put_libs` copies into the rootfs's /lib. Every
/// segment after `so` is a version number, so it has to be non-empty as well
/// as all digits: `all` over an empty segment is vacuously true, which made
/// `libfoo.so.` answer yes.
#[test]
fn a_trailing_dot_is_not_a_shared_library() {
    let dir = scratch("check-so");
    for (name, want) in [
        ("libc.so", true),
        ("libgcc_s.so.1", true),
        ("libstdc++.so.6.0.30", true),
        ("libfoo.so.", false),
        ("libfoo.so.6.", false),
        ("libfoo.so.6a", false),
        ("libfoo.so.6.debug", false),
        ("libfoo", false),
        (".so.1", false),
    ] {
        let path = dir.join(name);
        fs::write(&path, b"").unwrap();
        assert_eq!(check_so(&path), want, "check_so({name:?})");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// And a name that is not there at all is not a library, however it is
/// spelled: `put_libs` reads a directory, so a racing delete between the
/// listing and the check must answer no rather than panic.
#[test]
fn a_name_that_does_not_exist_is_not_a_shared_library() {
    let dir = scratch("check-so-missing");
    assert!(!check_so(dir.join("libghost.so.1")));
    let _ = fs::remove_dir_all(&dir);
}

/// A SYMLINK whose target is not there is still a library by name, and
/// that is what the `is_symlink` arm is for: for a symlink, `is_file` and
/// `exists` answer about the TARGET, so a link left dangling by a
/// half-installed toolchain would be invisible to `put_libs` -- and a lib
/// directory is mostly symlinks. What to install for one is
/// `rootfs_link_target`'s decision, and it refuses a dangling target of
/// its own accord, with a line saying which path and why. Skipping it here
/// instead means the name never reaches that decision and nothing is said
/// at all.
#[test]
fn a_dangling_symlink_is_still_a_shared_library_by_name() {
    let dir = scratch("check-so-dangling");
    let link = dir.join("libfoo.so.1");
    unix::fs::symlink(dir.join("never-installed.so.1"), &link).unwrap();
    assert!(
        check_so(&link),
        "a dangling library symlink is dropped before anything can say why"
    );

    // The name still has to look like one, link or no link.
    let bad = dir.join("libfoo.so.");
    unix::fs::symlink(dir.join("never-installed.so.1"), &bad).unwrap();
    assert!(!check_so(&bad));
    let _ = fs::remove_dir_all(&dir);
}

// ---- what eclipse-init actually boots ------------------------------

/// A rootfs with the service table and the wrappers in it, the way
/// `install_eclipse_init` leaves them. Both writers run: `write_oopslog`
/// is called by the wrapper pass and adds a service of its own, so the
/// service directory is only complete once both have.
fn init_rootfs(tag: &str) -> PathBuf {
    let rootfs = scratch(tag);
    let svc = rootfs.join("etc/eclipse/services");
    let localbin = rootfs.join("usr/local/bin");
    fs::create_dir_all(&svc).unwrap();
    fs::create_dir_all(&localbin).unwrap();
    LinuxRootfs::write_init_services(&svc, true);
    // `eclipse-ntpd` is one of the programs the table execs and the only
    // one of them `write_ntp` writes, so the same rootfs needs it here for
    // the same reason `make` writes it there.
    LinuxRootfs::write_ntp(&rootfs);
    LinuxRootfs::write_init_wrappers(&localbin, &svc);
    rootfs
}

/// Every `*.service` of a rootfs as (name without the extension, body),
/// sorted by name.
fn services(rootfs: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = fs::read_dir(rootfs.join("etc/eclipse/services"))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".service")?.to_string();
            Some((stem, fs::read_to_string(e.path()).unwrap()))
        })
        .collect();
    out.sort();
    out
}

/// The `key = value` pairs of a service file, read the way
/// `tools/eclipse-init` reads them: comments and blank lines out, both
/// sides trimmed.
fn fields(body: &str) -> Vec<(&str, &str)> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    fields(body)
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn wrapper(rootfs: &Path, name: &str) -> String {
    let path = rootfs.join("usr/local/bin").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The `exec`s of the table that are written somewhere else, and by whom.
const WRITTEN_BY_DESKTOP_RS: &[&str] = &["labwc", "eclipse-gtk-caches", "eclipse-xkbmap"];

/// `requires =` is the key that lets init stop a service whose dependency
/// it has written off for the boot, instead of letting it burn its own
/// twenty crash-restarts -- each paying its bounded `wait_socket` gate --
/// against something that will never arrive. Two things have to hold of
/// every one of them, and neither is visible from the service file alone:
///
///  * the name is a service this image really writes, because init ignores
///    a requirement that is not in the boot's set (a typo would therefore
///    be silent, which is how `wait_sockt =` raced labwc against seatd for
///    months),
///  * and nothing requires itself, which would write the service off the
///    moment anything behind it failed.
///
/// The ordering half (`requires =` implies `after =`) is init's own job
/// and tested there.
#[test]
fn every_requirement_names_a_service_this_image_writes() {
    let rootfs = init_rootfs("init-requires");
    let all = services(&rootfs);
    let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
    let mut seen = 0;
    for (name, body) in &all {
        for dep in field(body, "requires")
            .unwrap_or_default()
            .split_whitespace()
        {
            seen += 1;
            assert!(
                    names.contains(&dep),
                    "{name}.service requiere '{dep}', que no es ningun servicio de la imagen: {names:?}"
                );
            assert_ne!(dep, name.as_str(), "{name}.service se requiere a si mismo");
        }
    }
    assert!(seen >= 8, "solo {seen} requisitos: alguno se ha perdido");
    let _ = fs::remove_dir_all(&rootfs);
}

/// The requirements that are not a judgement call: a service whose
/// `wait_socket =` is the socket ANOTHER service creates cannot work
/// without it, so when that one is given up on this one is hopeless too.
/// Spelled out because the opposite -- a service missing its `requires =`
/// -- is invisible: it just goes back to crash-looping for two and a half
/// minutes against a socket nobody will bind.
#[test]
fn a_service_gated_on_another_services_socket_requires_it() {
    let rootfs = init_rootfs("init-requires-sockets");
    for (name, dep) in [
        ("labwc", "seatd"),           // /run/seatd.sock
        ("lunarbar", "labwc"),        // /run/user/0/wayland-0
        ("lunarbg", "labwc"),         //      ""
        ("xkbmap", "labwc"),          //      ""
        ("dbus-selftest", "dbus"),    // /run/user/0/bus
        ("boot-sound", "pulseaudio"), // no sound server, no chime
        ("boot-sound-xorg", "pulseaudio"),
    ] {
        let body = fs::read_to_string(
            rootfs
                .join("etc/eclipse/services")
                .join(format!("{name}.service")),
        )
        .unwrap();
        let requires = field(&body, "requires").unwrap_or_default();
        assert!(
            requires.split_whitespace().any(|d| d == dep),
            "{name}.service no requiere '{dep}' (requires = {requires:?}): volvera a \
                 reintentar veinte veces contra algo que nadie va a crear"
        );
    }
    let _ = fs::remove_dir_all(&rootfs);
}

/// Every service the image boots with: the program it runs, whether init
/// supervises it or waits for it to finish, and what it starts after.
///
/// Both columns go wrong quietly. A `type` that is neither word is
/// treated as `oneshot` (the parser says so itself), so a misspelling
/// turns the compositor into something started once and never again. And
/// the set itself is the boot: a service file whose name does not end in
/// `.service` is not read at all, which is how a whole service can
/// disappear with nothing to see.
#[test]
fn every_boot_service_names_its_program_and_how_it_is_supervised() {
    // name, type, exec, after
    const BOOT: &[(&str, &str, &str, &str)] = &[
        (
            "boot-sound",
            "oneshot",
            "/usr/local/bin/eclipse-boot-sound",
            "pulseaudio lunarbar",
        ),
        (
            "boot-sound-xorg",
            "oneshot",
            "/usr/local/bin/eclipse-boot-sound",
            "pulseaudio xorg",
        ),
        ("dbus", "respawn", "/usr/local/bin/eclipse-dbus", ""),
        (
            "dbus-selftest",
            "oneshot",
            "/bin/eclipse-dbusd --selftest",
            "dbus",
        ),
        (
            "dbus-system",
            "respawn",
            "/usr/local/bin/eclipse-dbus-system",
            "",
        ),
        (
            "gtk-caches",
            "oneshot",
            "/usr/local/bin/eclipse-gtk-caches",
            "",
        ),
        (
            "labwc",
            "respawn",
            "/usr/local/bin/labwc",
            "seatd gtk-caches dbus",
        ),
        (
            "lunarbar",
            "respawn",
            "/usr/local/bin/eclipse-lunarbar",
            "labwc",
        ),
        (
            "lunarbg",
            "respawn",
            "/usr/local/bin/eclipse-lunarbg",
            "labwc",
        ),
        (
            "lunarclip",
            "respawn",
            "/usr/local/bin/eclipse-lunarclip",
            "labwc",
        ),
        ("ntpd", "respawn", "/usr/local/bin/eclipse-ntpd", "udhcpc"),
        ("oopslog", "respawn", "/usr/local/bin/eclipse-oopslog", ""),
        // No `after`: `requires = dbus` implies it (#1744).
        ("portal", "respawn", "/usr/local/bin/eclipse-portal", ""),
        (
            "pulseaudio",
            "respawn",
            "/usr/local/bin/eclipse-pulseaudio",
            "dbus-system",
        ),
        ("seatd", "respawn", "/usr/local/bin/eclipse-seatd", ""),
        ("udhcpc", "respawn", "/usr/local/bin/eclipse-udhcpc", ""),
        (
            "xkbmap",
            "oneshot",
            "/usr/local/bin/eclipse-xkbmap",
            "labwc",
        ),
        ("xorg", "respawn", "/usr/local/bin/eclipse-xorg", "dbus"),
    ];
    let rootfs = init_rootfs("services");
    let got = services(&rootfs);
    assert_eq!(
        got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        BOOT.iter().map(|(n, _, _, _)| *n).collect::<Vec<_>>(),
        "the set of services the image boots with changed"
    );
    for ((name, body), (_, kind, exec, after)) in got.iter().zip(BOOT) {
        assert_eq!(field(body, "type"), Some(*kind), "{name}: supervision");
        assert_eq!(field(body, "exec"), Some(*exec), "{name}: program");
        assert_eq!(
            field(body, "after").unwrap_or_default(),
            *after,
            "{name}: what it starts after"
        );
    }
    // The example is documentation, not a service: init reads `*.service`
    // only, so this one has to keep its `.txt`.
    assert!(
        rootfs
            .join("etc/eclipse/services/example.service.txt")
            .is_file(),
        "the documented example is gone, or is now a service init would try to start"
    );
}

/// An `exec` naming a wrapper nothing writes is an `execve` that fails for
/// the whole boot, once per backoff, with the reason only in the log --
/// which is how `eclipse-oopslog` shipped unexecutable for a release.
/// Three of the programs are written by `desktop.rs` instead of here, so
/// the test checks that that is still where they come from.
#[test]
fn every_service_execs_a_wrapper_that_something_really_writes() {
    const DESKTOP_RS: &str = include_str!("desktop.rs");
    let rootfs = init_rootfs("execs");
    for name in WRITTEN_BY_DESKTOP_RS {
        assert!(
            DESKTOP_RS.contains(&format!("localbin.join(\"{name}\")")),
            "the service table execs {name}, and desktop.rs no longer writes it"
        );
    }
    for (name, body) in services(&rootfs) {
        let exec = field(&body, "exec").unwrap();
        let program = exec.split(' ').next().unwrap();
        let Some(stem) = program.strip_prefix("/usr/local/bin/") else {
            assert_eq!(
                program, "/bin/eclipse-dbusd",
                "{name} execs {program}, which is neither a wrapper nor the bus binary"
            );
            continue;
        };
        if WRITTEN_BY_DESKTOP_RS.contains(&stem) {
            continue;
        }
        assert!(
            rootfs.join("usr/local/bin").join(stem).is_file(),
            "{name} execs {program}, which nothing writes"
        );
    }
}

/// `after =` is resolved by name, and a name nothing provides is simply
/// dropped: the service then starts straight away instead of after what
/// it needs, and the only sign is one line in the boot log.
#[test]
fn every_order_names_a_service_that_exists() {
    let rootfs = init_rootfs("after");
    let all = services(&rootfs);
    let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
    for (name, body) in &all {
        for dep in field(body, "after").unwrap_or_default().split_whitespace() {
            assert!(
                names.contains(&dep),
                "{name} is ordered after `{dep}`, which is not a service"
            );
        }
    }
}

/// `after` only orders the fork; `wait_socket` is what actually blocks the
/// start. Waiting on a socket without being ordered after whatever binds
/// it is a race that costs a backoff retry at best, so the two have to
/// name the same service -- `wait_sockt` (sic) once let labwc race seatd
/// on every boot, which is why the parser now logs an unknown key. The
/// ordering may be transitive: init sorts `after` topologically.
#[test]
fn a_service_that_waits_for_a_socket_is_ordered_after_whatever_binds_it() {
    const BOUND_BY: &[(&str, &str)] = &[
        ("/run/seatd.sock", "seatd"),
        ("/run/user/0/wayland-0", "labwc"),
        ("/run/user/0/bus", "dbus"),
    ];
    /// Everything a service is ordered after, directly or through another:
    /// init's start order is a topological sort of `after`, so the chime
    /// being after the panel puts it after the compositor too.
    fn ordered_after(all: &[(String, String)], name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            let Some((_, body)) = all.iter().find(|(s, _)| *s == n) else {
                continue;
            };
            // `requires` implies `after` (#1744): naming only the
            // requirement orders the service just as firmly.
            let deps = format!(
                "{} {}",
                field(body, "after").unwrap_or_default(),
                field(body, "requires").unwrap_or_default()
            );
            for dep in deps.split_whitespace() {
                if !out.iter().any(|d| d == dep) {
                    out.push(dep.to_string());
                    todo.push(dep.to_string());
                }
            }
        }
        out
    }
    let rootfs = init_rootfs("waits");
    let all = services(&rootfs);
    for (name, body) in &all {
        let Some(socket) = field(body, "wait_socket") else {
            continue;
        };
        let (_, binder) = BOUND_BY
            .iter()
            .find(|(s, _)| *s == socket)
            .unwrap_or_else(|| {
                panic!("{name} waits for {socket}, which nothing in the image binds")
            });
        assert!(
            ordered_after(&all, name).iter().any(|d| d == *binder),
            "{name} waits for {socket} but nothing orders it after {binder}, so it can start first"
        );
    }
}

/// What the compositor needs before it forks, in one place. `gtk-caches`
/// has to be the oneshot kind for the ordering to mean anything: a
/// oneshot has COMPLETED before whatever follows it starts, and that is
/// the only reason labwc's clients find the pixbuf loader registry.
#[test]
fn the_compositor_waits_for_the_seat_and_the_caches_before_it_forks() {
    let rootfs = init_rootfs("labwc");
    let all = services(&rootfs);
    let labwc = &all.iter().find(|(n, _)| n == "labwc").unwrap().1;
    let after: Vec<&str> = field(labwc, "after").unwrap().split_whitespace().collect();
    for dep in ["seatd", "gtk-caches", "dbus"] {
        assert!(
            after.contains(&dep),
            "the compositor no longer waits for {dep}"
        );
    }
    let caches = &all.iter().find(|(n, _)| n == "gtk-caches").unwrap().1;
    assert_eq!(
        field(caches, "type"),
        Some("oneshot"),
        "a respawning gtk-caches is not finished when labwc forks"
    );
    assert_eq!(field(labwc, "wait_socket"), Some("/run/seatd.sock"));
    assert_eq!(
            field(labwc, "wait_path"),
            Some("/dev/input"),
            "without a settled /dev/input, libinput's single scan can miss a device for the whole session"
        );
}

/// `cmdline =` keeps a service out of a normal boot entirely, so it is the
/// one key that can disable something by accident. Only the bus probe has
/// it, and its token is the one the menu and the docs tell you to boot
/// with.
#[test]
fn only_the_bus_probe_is_gated_on_the_kernel_cmdline() {
    let rootfs = init_rootfs("cmdline");
    let gated: Vec<(String, String)> = services(&rootfs)
        .into_iter()
        .filter(|(_, b)| field(b, "cmdline").is_some())
        .collect();
    assert_eq!(
        gated.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["dbus-selftest"],
        "a boot service gated on a cmdline token does not run on an ordinary boot"
    );
    let body = &gated[0].1;
    assert_eq!(field(body, "cmdline"), Some("dbus.selftest"));
    assert_eq!(
        field(body, "type"),
        Some("oneshot"),
        "a probe that respawns never stops probing"
    );
}

/// The one `wait_path` in the table is the compositor's `/dev/input`.
/// `wait_path = /dev/snd/pcmC0D0p` was in here once: starts are ordered
/// alphabetically, so it sat ahead of seatd and labwc and spent the full
/// bounded wait on every machine whose sound card this kernel has no
/// driver for -- a silent 8 s added to the boot, before the desktop.
#[test]
fn nothing_on_the_boot_path_waits_for_a_sound_card() {
    let rootfs = init_rootfs("waitpath");
    let waits: Vec<(String, String)> = services(&rootfs)
        .into_iter()
        .filter_map(|(n, b)| field(&b, "wait_path").map(|p| (n, p.to_string())))
        .collect();
    assert_eq!(
        waits,
        [("labwc".to_string(), "/dev/input".to_string())],
        "a service waits for a path that is not the compositor's input directory"
    );
}

/// Every key in the table has to be a key the parser knows: an unknown one
/// is logged and skipped, so a misspelled gate is a gate that does not
/// exist. The example file is the only documentation of that set, which
/// makes it part of the same invariant.
#[test]
fn every_key_a_service_uses_is_parsed_by_the_init_and_documented_in_the_example() {
    let rootfs = init_rootfs("keys");
    let example = example_service(&rootfs);
    let documented = documented_keys(&example);
    for (name, body) in services(&rootfs) {
        for (key, _) in fields(&body) {
            assert!(
                INIT.contains(&format!("\"{key}\" =>")),
                "{name}: `{key}` is not a key tools/eclipse-init parses"
            );
            assert!(
                documented.contains(&key),
                "{name}: `{key}` is used by a service and documented nowhere"
            );
        }
    }
    for key in &documented {
        assert!(
            INIT.contains(&format!("\"{key}\" =>")),
            "the example documents `{key}`, which the init does not parse"
        );
    }
}

const INIT: &str = include_str!("../../../tools/eclipse-init/src/main.rs");

fn example_service(rootfs: &Path) -> String {
    fs::read_to_string(rootfs.join("etc/eclipse/services/example.service.txt")).unwrap()
}

/// The keys `example.service.txt` documents, one `# key = ...` line each.
fn documented_keys(example: &str) -> Vec<&str> {
    example
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("# "))
        .filter_map(|l| l.split_once('='))
        .map(|(k, _)| k.trim())
        .collect()
}

/// `install-ollama-cuda` writes `ollama.service` on the user's machine, after
/// the image is built, so it is in none of the tables the tests above walk.
/// This runs the script's `--service` mode against a scratch root and holds
/// the file to the same rules: keys the init parses and the example
/// documents, supervised, and exec'ing the binary the install put in DEST.
#[test]
fn the_ollama_service_install_ollama_cuda_writes_follows_the_table_rules() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("ollama-service");
    let bin = dir.join("dest/bin/ollama");
    let svc = dir.join("services");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::create_dir_all(&svc).unwrap();
    fs::write(&bin, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let st = std::process::Command::new("sh")
        .arg(PROJECT_DIR.join("tools/cuda-probe/install-ollama-cuda.sh"))
        .arg("--service")
        .env("DEST", dir.join("dest"))
        .env("SERVICE_DIR", &svc)
        .env_remove("OLLAMA_SERVICE")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(st.success(), "install-ollama-cuda --service failed");

    let body = fs::read_to_string(svc.join("ollama.service")).unwrap();
    let example = example_service(&init_rootfs("ollama-keys"));
    let documented = documented_keys(&example);
    for (key, _) in fields(&body) {
        assert!(
            INIT.contains(&format!("\"{key}\" =>")),
            "ollama.service: `{key}` is not a key tools/eclipse-init parses"
        );
        assert!(
            documented.contains(&key),
            "ollama.service: `{key}` is documented nowhere"
        );
    }
    assert_eq!(field(&body, "type"), Some("respawn"));
    let exec = format!("{} serve", bin.display());
    assert_eq!(field(&body, "exec"), Some(exec.as_str()));
    assert!(
        !svc.join("ollama.service.tmp").exists(),
        "the temporary file was left behind"
    );
}

/// `desktop =` is what keeps the two sessions apart, and the interesting
/// column is the empty one: a `desktop` on the bus would deny an Xorg
/// session the thing `SDL_Init` looks for first and `GtkApplication`
/// exits without.
#[test]
fn the_two_sessions_never_start_each_others_clients() {
    const LABWC: &[&str] = &[
        "boot-sound",
        "gtk-caches",
        "labwc",
        "lunarbar",
        "lunarbg",
        "lunarclip",
        "seatd",
        "xkbmap",
    ];
    const XORG: &[&str] = &["boot-sound-xorg", "xorg"];
    const EITHER: &[&str] = &[
        "dbus",
        "dbus-selftest",
        "dbus-system",
        "ntpd",
        "oopslog",
        // A bus service, not a session one: Xorg has GTK apps too.
        "portal",
        "pulseaudio",
        "udhcpc",
    ];
    let rootfs = init_rootfs("desktops");
    for (name, body) in services(&rootfs) {
        let want = if LABWC.contains(&name.as_str()) {
            Some("labwc")
        } else if XORG.contains(&name.as_str()) {
            Some("xorg")
        } else {
            assert!(
                EITHER.contains(&name.as_str()),
                "{name} belongs to no session list"
            );
            None
        };
        assert_eq!(
            field(&body, "desktop"),
            want,
            "{name} is in the wrong session"
        );
    }
}

/// The minimal variant installs no apk closure, so a row whose `exec =`
/// comes out of that closure can never start there. `desktop =` cannot
/// express it -- `pulseaudio` belongs to neither session -- so the table
/// leaves the row out, and this is the assertion that says so.
///
/// Written as the pair it is: the same row IS in the desktop table. A test
/// that only checked the absence would also pass with the row deleted for
/// everyone, which is the opposite of the fix.
#[test]
fn the_minimal_table_has_no_service_the_apk_closure_would_have_to_supply() {
    let desktop = scratch("svc-desktop").join("etc/eclipse/services");
    let minimal = scratch("svc-minimal").join("etc/eclipse/services");
    fs::create_dir_all(&desktop).unwrap();
    fs::create_dir_all(&minimal).unwrap();
    LinuxRootfs::write_init_services(&desktop, true);
    LinuxRootfs::write_init_services(&minimal, false);

    assert!(
        desktop.join("pulseaudio.service").is_file(),
        "the desktop table lost pulseaudio.service, which is where it belongs"
    );
    assert!(
        !minimal.join("pulseaudio.service").is_file(),
        "the minimal image installs no apk closure, so pulseaudio.service is 20 \
             starts and 162 s of console on a binary that is not there"
    );

    // And nothing left in the minimal table may still need it: a
    // `requires =` naming a service that does not exist is a dependency
    // that can never be met. The two that name it (`boot-sound`,
    // `boot-sound-xorg`) are `desktop =`-guarded, so `eclipse-init`
    // discards them before the dependency is ever read -- which is the
    // reason this removal is safe, and the reason it has to be checked
    // rather than assumed.
    for entry in fs::read_dir(&minimal).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".service") {
            continue;
        }
        let body = fs::read_to_string(entry.path()).unwrap();
        let needs_pulse = ["requires", "after"].iter().any(|key| {
            field(&body, key).is_some_and(|v| v.split_whitespace().any(|dep| dep == "pulseaudio"))
        });
        assert!(
            !needs_pulse || field(&body, "desktop").is_some(),
            "{name} depends on pulseaudio and is not session-guarded, so in a \
                 minimal image it waits for a service that is not written"
        );
    }
}

// ---- the wrappers the services exec -------------------------------

/// Every wrapper has to be executable and has to start with a shebang:
/// without the x bit `execve` answers EACCES, without the shebang it
/// answers ENOEXEC, and either way init respawns the service for the
/// whole boot with the reason only in a log.
#[test]
fn every_wrapper_is_executable_and_starts_with_a_shebang() {
    use std::os::unix::fs::PermissionsExt;
    let rootfs = init_rootfs("modes");
    let localbin = rootfs.join("usr/local/bin");
    let mut seen = 0;
    for entry in fs::read_dir(&localbin).unwrap().flatten() {
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mode = fs::metadata(entry.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o111,
            0o111,
            "{name} is {mode:o}, so execve answers EACCES"
        );
        let body = fs::read_to_string(entry.path()).unwrap();
        assert!(
            body.starts_with("#!/bin/sh\n"),
            "{name} has no shebang, so execve answers ENOEXEC"
        );
        seen += 1;
    }
    assert!(seen >= 15, "only {seen} wrappers landed in /usr/local/bin");
}

/// The pass that makes the wrappers executable walks the directory, and it
/// asks the DIRECTORY ENTRY what it is rather than the path: following a
/// symlink would chmod whatever it points at, somewhere else entirely.
#[test]
fn the_chmod_pass_does_not_follow_a_symlink_out_of_the_directory() {
    use std::os::unix::fs::PermissionsExt;
    let rootfs = scratch("stray");
    let outside = rootfs.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("not-a-wrapper");
    fs::write(&secret, b"private\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let svc = rootfs.join("etc/eclipse/services");
    let localbin = rootfs.join("usr/local/bin");
    fs::create_dir_all(&svc).unwrap();
    fs::create_dir_all(&localbin).unwrap();
    unix::fs::symlink(&secret, localbin.join("stray")).unwrap();
    LinuxRootfs::write_init_wrappers(&localbin, &svc);
    let mode = fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the chmod pass followed a symlink and changed a file outside /usr/local/bin"
    );
}

/// `reboot`, `poweroff` and `halt` each have to force their OWN verb.
/// `-f` is sync + the syscall, which is the path that works on this
/// kernel; signalling PID 1 instead ran eclipse-init's kill-all and hung
/// the GPU session. And a copy-paste between the three is a machine that
/// powers off when it was told to halt.
#[test]
fn each_of_the_three_halt_wrappers_forces_its_own_verb() {
    let rootfs = init_rootfs("halts");
    for verb in ["reboot", "poweroff", "halt"] {
        let body = wrapper(&rootfs, verb);
        assert!(
            body.contains(&format!("exec /bin/busybox {verb} -f")),
            "{verb} does not force busybox's own {verb}:\n{body}"
        );
    }
}

/// `-s` is not optional: busybox udhcpc's compiled-in script path is not
/// where Eclipse stages the script, so without it the lease is obtained
/// and never applied -- no address, no resolv.conf, no route. Both execs
/// need it, including the eth0 fallback for when the netlink dump raced
/// the NIC probe and `ip link` listed nothing.
#[test]
fn the_dhcp_wrapper_always_passes_the_script_that_applies_the_lease() {
    let rootfs = init_rootfs("dhcp");
    let body = wrapper(&rootfs, "eclipse-udhcpc");
    let execs: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("exec udhcpc"))
        .collect();
    assert_eq!(
        execs.len(),
        2,
        "expected the listed interface and the eth0 fallback"
    );
    for line in &execs {
        assert!(
            line.contains(" -s \"$SCRIPTv4\""),
            "a lease nothing applies: {line}"
        );
        assert!(
            line.contains(" -f"),
            "a backgrounded client init cannot supervise: {line}"
        );
    }
    assert!(
        execs[1].contains(" -i eth0"),
        "no fallback interface: {}",
        execs[1]
    );
    assert!(
        body.contains("SCRIPTv4=/usr/share/udhcpc/default.script")
            && body.contains("SCRIPTv4=/etc/udhcpc/default.script"),
        "the script path has no second candidate"
    );
}

/// A missing seatd is the most confusing thing this image can do: the
/// whole Wayland session fails and init's `/dev/null` stdio swallows the
/// reason. So the message goes to three places, and the back-off is long
/// -- a package is not going to appear on its own, and the tight loop it
/// used to cause was seatd 100x and labwc 67x per run.
#[test]
fn a_missing_seat_manager_says_so_where_it_can_be_read_and_backs_off_hard() {
    let rootfs = init_rootfs("seatd");
    let body = wrapper(&rootfs, "eclipse-seatd");
    for where_ in [">>\"$LOG\"", "> /dev/console", ">&2"] {
        assert!(
            body.contains(&format!("echo \"$MSG\" {where_}")),
            "the reason never reaches {where_}"
        );
    }
    assert!(
        body.contains("\nsleep 60\nexit 127\n"),
        "the respawn storm is back"
    );
    assert!(
        body.contains("[ -x \"$d/seatd\" ] && exec \"$d/seatd\" -l info"),
        "seatd no longer runs in the foreground with its log turned on"
    );
    // Ubuntu ships seatd in /usr/sbin; the search order must include it.
    assert!(body.contains("for d in /usr/bin /bin /usr/sbin /sbin; do"));
}

/// The wallpaper and the panel share one preamble, and they have to: it is
/// what finds the compositor's socket. On the boot path init has already
/// waited (`wait_socket`), so the loop hits on its first pass; it exists
/// for a manual launch that races a just-started compositor, and it polls
/// once a SECOND because every iteration forks a busybox.
#[test]
fn both_wayland_clients_share_one_wait_and_poll_once_a_second() {
    fn wait_block(script: &str) -> &str {
        let (_, rest) = script.split_once("exec >>\"$LOG\" 2>&1\n").unwrap();
        rest.split_once("command -v").unwrap().0
    }
    let rootfs = init_rootfs("clients");
    let bg = wrapper(&rootfs, "eclipse-lunarbg");
    let bar = wrapper(&rootfs, "eclipse-lunarbar");
    let block = wait_block(&bg);
    assert_eq!(
        block,
        wait_block(&bar),
        "the two clients wait differently now"
    );
    assert!(
        block.lines().any(|l| l.trim() == "sleep 1"),
        "one fork per 0.1 s is back:\n{block}"
    );
    assert!(
        block.contains("chmod 0700 \"$XDG_RUNTIME_DIR\""),
        "the Wayland socket's directory is not private"
    );
    assert!(
        block.contains("WAYLAND_DISPLAY=$(basename \"$s\")"),
        "the client no longer names the socket it found"
    );
}

/// A client whose compositor is not installed is healthy -- there is just
/// nothing to connect to, and no amount of respawning will change that.
/// It backs off for a minute instead of every fifteen seconds.
#[test]
fn a_client_with_no_compositor_installed_backs_off_for_a_minute() {
    let rootfs = init_rootfs("backoff");
    for name in ["eclipse-lunarbg", "eclipse-lunarbar"] {
        let body = wrapper(&rootfs, name);
        assert!(
            body.contains("[ -x \"$d/labwc\" ] && { sleep 2; exit 1; }"),
            "{name}: an installed compositor that is simply not up yet should retry soon"
        );
        assert!(
            body.contains("sleep 60; exit 1"),
            "{name}: respawning against a missing package is pure fork churn"
        );
    }
}

/// The wallpaper is started by init, not as a child of labwc, so it never
/// sees the environment file labwc writes: the defaults it falls back to
/// are the ones a panel with no EDID millimetres gets.
#[test]
fn the_wallpaper_keeps_the_aspect_and_the_frame_rate_it_defaults_to() {
    let rootfs = init_rootfs("aspect");
    let body = wrapper(&rootfs, "eclipse-lunarbg");
    assert!(
        body.contains("${LUNARBG_ASPECT:-16:9}"),
        "the default aspect changed"
    );
    assert!(
        body.contains("--fps \"${LUNARBG_FPS:-8}\""),
        "the default frame rate changed"
    );
}

/// There is no udev or logind seat here, so X takes a VT directly, and
/// `startx` reads the session out of `$HOME` -- which init does not set
/// for it.
#[test]
fn the_x_session_takes_the_first_vt_with_root_as_its_home() {
    let rootfs = init_rootfs("xorg");
    let body = wrapper(&rootfs, "eclipse-xorg");
    assert!(
        body.contains("export HOME=/root\n"),
        "startx would look for .xinitrc elsewhere"
    );
    assert!(
        body.contains("exec startx -- vt1\n"),
        "X no longer takes the first VT"
    );
}

/// A `oneshot` holds up everything ordered after it until it exits, so the
/// one that plays the startup track detaches and returns at once: the
/// track is ~20 s and the panel is behind it.
#[test]
fn a_oneshot_that_plays_a_track_detaches_instead_of_holding_the_boot() {
    let rootfs = init_rootfs("chime");
    let body = wrapper(&rootfs, "eclipse-boot-sound");
    assert!(
        body.contains("setsid /usr/local/bin/eclipse-boot-sound-play"),
        "the oneshot now waits for the whole track:\n{body}"
    );
    assert!(
        body.trim_end().ends_with("exit 0"),
        "the oneshot does not return success"
    );
}

/// A wedged mpg123 never comes back on its own, and it holds the PCM for
/// the whole session (the device is exclusive), so the chime runs every
/// player under a watchdog that kills it. No bare `mpg123` is left.
#[test]
fn every_player_the_chime_starts_is_killed_if_it_wedges() {
    let rootfs = init_rootfs("chime-watchdog");
    let body = wrapper(&rootfs, "eclipse-boot-sound-play");
    assert!(
        body.contains("play() {") && body.contains("MAXPLAY="),
        "the chime lost its bounded player:\n{body}"
    );
    assert!(
        body.contains("kill -KILL \"$p\""),
        "nothing kills a player that ignores SIGTERM:\n{body}"
    );
    for line in body.lines().map(str::trim) {
        // Inside the helper, and the "not installed" probe, are the only
        // places the name may appear bare.
        if line.starts_with("mpg123 ") && !line.starts_with("mpg123 \"$@\"") {
            panic!("a player outside the watchdog: {line}");
        }
    }
    assert!(
        body.contains("play -q -o pulse ") && body.contains("play -q -o alsa,oss "),
        "pulse path must use -o pulse (not alsa-first); bare PCM stays alsa,oss:\n{body}"
    );
    assert!(
        !body.contains("play -q -o alsa --encoding"),
        "with Pulse up, opening ALSA first wedges the exclusive PCM (audio loop):\n{body}"
    );
    assert!(
        body.contains("mpg123.bin"),
        "chime must prefer mpg123.bin so -o flags are not overridden by wrappers:\n{body}"
    );
    // pactl drain can hang forever on a wedged sink; the Ubuntu "final loop"
    // was drain (or ALSA open) blocking after the track finished.
    assert!(
        body.contains("pactl drain") && body.contains("sleep 5; kill \"$d\""),
        "pactl drain must be time-bounded:\n{body}"
    );
}

/// Every oneshot has a bound now (`DEFAULT_ONESHOT_TIMEOUT`, 90 s), but the
/// one that hands an MP3 to mpg123 is the one that froze a machine, and it
/// has nothing to do but fork a detached player: it carries a bound of its
/// own, in seconds rather than the minute and a half everything else gets.
#[test]
fn the_chime_oneshots_carry_a_short_timeout_of_their_own() {
    let rootfs = init_rootfs("chime-timeout");
    for name in ["boot-sound", "boot-sound-xorg"] {
        let body = fs::read_to_string(rootfs.join(format!("etc/eclipse/services/{name}.service")))
            .unwrap();
        let secs: u64 = fields(&body)
            .into_iter()
            .find(|(k, _)| *k == "timeout")
            .unwrap_or_else(|| panic!("{name} sin timeout propio:\n{body}"))
            .1
            .parse()
            .unwrap_or_else(|e| panic!("{name}: timeout no numerico: {e}"));
        assert!(
            secs > 0 && secs <= 30,
            "{name}: timeout = {secs} no acota el chime"
        );
    }
}

/// The watchdog, RUN rather than read: the structural test above cannot
/// tell a working timeout loop from a broken one. Lifts `play()` out of the
/// generated script, points it at a fake mpg123 and shortens the limit to a
/// second, then asks the three questions that matter -- a player that never
/// exits is killed promptly and leaves nothing behind, a clean run still
/// returns 0, and a failing one still returns its own code (the `||`
/// fallback chain in the script depends on that last one).
#[test]
fn the_chime_watchdog_really_kills_a_player_that_hangs() {
    use std::os::unix::fs::PermissionsExt;

    let rootfs = init_rootfs("chime-run");
    let script = rootfs.join("usr/local/bin/eclipse-boot-sound-play");
    let st = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
        .unwrap();
    assert!(st.success(), "sh -n rejected eclipse-boot-sound-play");

    // The helper on its own: from `MAXPLAY=` to the `}` that closes
    // `play()`. Running the whole script would wait on a PCM that is not
    // here and then play a track that is not either.
    let body = fs::read_to_string(&script).unwrap();
    let from = body.find("MAXPLAY=").expect("the helper lost its limit");
    let to = from + body[from..].find("\n}\n").expect("play() is not closed") + 3;
    let helper = &body[from..to];

    let dir = scratch("chime-run-sh");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let pidfile = dir.join("hang.pid");
    // A player that answers to its argument: `hang` never returns (and
    // leaves its pid behind so the test can look for a survivor), `fail`
    // exits 3, anything else succeeds.
    fs::write(
        bin.join("mpg123"),
        format!(
            "#!/bin/sh\n\
                 case \"$1\" in\n\
                 \x20 hang) echo $$ > \"{pid}\"; while :; do sleep 1; done ;;\n\
                 \x20 fail) exit 3 ;;\n\
                 \x20 *) exit 0 ;;\n\
                 esac\n",
            pid = pidfile.display()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(bin.join("mpg123")).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(bin.join("mpg123"), perms).unwrap();

    let driver = dir.join("drive.sh");
    fs::write(
        &driver,
        format!(
            "PATH=\"{bin}\":$PATH\n\
                 {helper}\n\
                 MAXPLAY=1\n\
                 play hang; echo \"hang=$?\"\n\
                 sleep 2\n\
                 if kill -0 \"$(cat '{pid}')\" 2>/dev/null; then echo alive=yes; \
                 else echo alive=no; fi\n\
                 play ok; echo \"ok=$?\"\n\
                 play fail; echo \"fail=$?\"\n",
            bin = bin.display(),
            pid = pidfile.display()
        ),
    )
    .unwrap();

    // Bounded from out here too: a broken watchdog would leave the driver
    // waiting on the hanging player for ever, and a test that HANGS says
    // much less on a CI runner than one that fails.
    let log = dir.join("drive.log");
    let started = std::time::Instant::now();
    let mut child = std::process::Command::new("sh")
        .arg(&driver)
        .stdout(fs::File::create(&log).unwrap())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let limit = std::time::Duration::from_secs(30);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                let said = fs::read_to_string(&log).unwrap_or_default();
                panic!(
                    "the watchdog never bounded the player: still running after \
                         {limit:?}\n{said}"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    let took = started.elapsed();
    let said = fs::read_to_string(&log).unwrap();

    assert!(
        said.contains("alive=no"),
        "a player that never exits survived the watchdog (took {took:?}):\n{said}"
    );
    // Killed, so not a success -- the script's `||` then tries the next
    // output module, which is what it did before this change too.
    assert!(
        said.contains("hang=") && !said.contains("hang=0"),
        "a killed player reported success:\n{said}"
    );
    assert!(
        said.contains("ok=0"),
        "a clean play no longer returns 0:\n{said}"
    );
    assert!(
        said.contains("fail=3"),
        "the player's own exit code is lost, so the fallback path breaks:\n{said}"
    );

    let _ = fs::remove_dir_all(&dir);
}

// ---- the renderer policy a login shell inherits -------------------

/// The value a login shell ends up with, which is not always the rest of
/// the line: `export HOME=/root; mkdir -p ...` assigns `/root` and THEN
/// runs a command, so the assignment ends at the first `;`. The profile no
/// longer chains anything onto an export -- that chaining is what broke this
/// module's own test -- but reading the rest of the line would read any
/// future command as part of the value, and no exported value here contains
/// a semicolon.
fn export<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("export "))
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| match v.split_once(';') {
            Some((value, _)) => value.trim_end(),
            None => v,
        })
}

fn profile_text(tag: &str) -> String {
    let etc = scratch(tag).join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_profile(&etc);
    fs::read_to_string(etc.join("profile")).unwrap()
}

/// Only the two branches that really have a GPU go through zink, and the
/// llvmpipe override at the end of the file stays COMMENTED: it is kept
/// for a machine where even llvmpipe autodetect misbehaves, and active it
/// would put the hardware path on a software rasteriser.
#[test]
fn only_the_two_hardware_branches_ask_for_a_gallium_driver() {
    let profile = profile_text("gallium");
    let active: Vec<&str> = profile
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("export GALLIUM_DRIVER="))
        .collect();
    assert_eq!(
        active,
        ["zink", "zink"],
        "the GPU path no longer goes through zink+NVK"
    );
    assert!(
        profile.contains("#export GALLIUM_DRIVER=llvmpipe"),
        "the commented software-GL override is gone"
    );
    let overrides: Vec<&str> = profile
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("export MESA_LOADER_DRIVER_OVERRIDE="))
        .collect();
    assert_eq!(
        overrides,
        ["zink", "zink"],
        "the Mesa loader override lost its pair"
    );
}

/// Two pins that are easy to lose and expensive to lose. There are two
/// NVIDIA DRM cards on the hardware (console and compute), so without
/// `WLR_DRM_DEVICES` wlroots can bind a phantom connector on the compute
/// GPU. And `WLR_NO_HARDWARE_CURSORS` must NOT be set: the DRM scheme
/// composites the cursor now, and without the hardware path every pointer
/// move re-renders the whole scene.
#[test]
fn the_compositor_gets_the_console_card_and_keeps_its_hardware_cursor() {
    let profile = profile_text("pins");
    assert_eq!(
        export(&profile, "WLR_DRM_DEVICES"),
        Some("/dev/dri/card0"),
        "the compositor is no longer pinned to the console GPU"
    );
    assert_eq!(
        export(&profile, "WLR_LIBINPUT_NO_DEVICES"),
        Some("1"),
        "with no udevd to tag devices, libinput can find none and abort the compositor"
    );
    assert_eq!(
            export(&profile, "WLR_NO_HARDWARE_CURSORS"),
            None,
            "the kernel composites the cursor; turning the hardware path off re-renders the scene on every pointer move"
        );
}

// ---- the /etc the image publishes --------------------------------

/// Does `/etc/profile` export `key` anywhere outside a comment?
///
/// Not [`export`]: a line can be an export AND part of a `case` arm
/// (`*) export LANG=...`), and the two "never export this" rules below are
/// written as comments that name the very variable they forbid.
fn exports_anywhere(profile: &str, key: &str) -> bool {
    profile
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .any(|l| l.contains(&format!("export {key}=")))
}

/// Two things `/etc/profile` must NOT export, both written in it as a
/// comment that names the variable -- which is why a check on the raw text
/// has to skip comments.
///
/// `LC_ALL` would freeze the language for gettext and GTK whatever
/// `/etc/eclipse/locale` says. And `/lib/libeclipse_dns.so` is
/// deliberately not preloaded: musl dropped it silently for years (every
/// process ran in secure mode for want of AT_SECURE in the auxv), and the
/// moment the auxv was fixed and the shim really loaded everywhere, labwc
/// froze within seconds of starting.
#[test]
fn the_login_shell_exports_neither_lc_all_nor_the_dns_shim() {
    let profile = profile_text("forbidden");
    assert!(
        !exports_anywhere(&profile, "LC_ALL"),
        "LC_ALL would freeze LANG for gettext and GTK"
    );
    assert!(
        !exports_anywhere(&profile, "LD_PRELOAD"),
        "the DNS shim froze labwc within seconds of really being loaded"
    );
    assert!(
        profile.contains("#   LD_PRELOAD=/lib/libeclipse_dns.so some-command"),
        "the opt-in-per-command note is what stops the next reader re-adding the preload"
    );
}

/// The language and the timezone are Spanish by default and come out of
/// `/etc/eclipse`, which the desktop's own `eclipse-locale` and
/// `eclipse-tz` scripts write. `LANGUAGE` carries a fallback chain
/// (`es:en`) so a message with no Spanish translation still appears in
/// English instead of as an untranslated key.
#[test]
fn spain_is_the_default_language_and_timezone_and_etc_eclipse_overrides_it() {
    let profile = profile_text("locale");
    assert!(
        profile.contains("_ecl_lang=es\n"),
        "the default language changed"
    );
    assert!(
        profile.contains("_ecl_tz=Europe/Madrid"),
        "the default timezone changed"
    );
    assert!(
        profile.contains("/etc/eclipse/locale") && profile.contains("/etc/eclipse/timezone"),
        "the profile no longer reads what the desktop scripts write"
    );
    assert!(
        profile.contains("en|EN|en_US) export LANG=en_US.UTF-8 LANGUAGE=en ;;"),
        "the English case is gone"
    );
    assert!(
        profile.contains("*) export LANG=es_ES.UTF-8 LANGUAGE=es:en ;;"),
        "the default case is gone, or lost its fallback chain"
    );
}

/// The serial terminal-size probe, which is three rules in one place.
///
/// It runs only on VT 0 (or an unset `ECLIPSE_VT`, e.g. a pty): the kernel
/// deliberately never answers a cursor-position query on a VT, and serial
/// mirrors only the active VT, so probing on VTs 1-5 blocked each of those
/// five shells 0.3 s at boot for an answer that could not come. It rejects
/// a tiny answer and an absurd one -- a bogus `\e[1;1R` or an unanswered
/// `\e[999;999H` echo used to leave nano unusable. And it marks
/// `ECLIPSE_TTY_SIZED` only AFTER `stty` succeeds, so a failed probe can
/// retry.
#[test]
fn the_tty_size_probe_runs_only_on_the_console_vt_and_rejects_absurd_answers() {
    let profile = profile_text("ttysize");
    assert!(
        profile.contains("[ \"${ECLIPSE_VT:-0}\" = \"0\" ]"),
        "the probe now runs on VTs that can never answer it"
    );
    assert!(
        profile.contains("[ \"$__rows\" -gt 1 ] && [ \"$__cols\" -gt 1 ]"),
        "a 1x1 answer is a bogus answer"
    );
    assert!(
        profile.contains("[ \"$__rows\" -le 512 ] && [ \"$__cols\" -le 512 ]"),
        "an absurd size is as bad as a tiny one"
    );
    let marked = profile
        .lines()
        .map(str::trim)
        .find(|l| l.contains("export ECLIPSE_TTY_SIZED=1"))
        .expect("the probe no longer marks that it ran");
    assert!(
        marked.starts_with("&& export"),
        "ECLIPSE_TTY_SIZED must be chained after stty, or a failed probe never retries: {marked}"
    );
}

/// What a login shell gets before anything else. `/usr/local/bin` comes
/// FIRST so the wrappers win over the busybox applets of the same name --
/// `reboot` is the one that matters, since busybox's own applet signals
/// PID 1 and the wrapper forces the path that works. The three certificate
/// variables are three names for one bundle because wget, curl and
/// everything OpenSSL-based each look for a different one.
#[test]
fn the_login_shell_gets_the_path_the_home_and_the_certificate_bundle() {
    let profile = profile_text("env");
    assert_eq!(
        export(&profile, "PATH"),
        Some("/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin"),
        "the wrappers only win over the busybox applets while /usr/local/bin is first"
    );
    assert_eq!(export(&profile, "HOME"), Some("/root"));
    assert_eq!(export(&profile, "TERM"), Some("xterm-256color"));
    for key in ["SSL_CERT_FILE", "CURL_CA_BUNDLE"] {
        assert_eq!(
            export(&profile, key),
            Some("/etc/ssl/certs/ca-certificates.crt"),
            "{key} no longer names the installed bundle"
        );
    }
    assert_eq!(export(&profile, "SSL_CERT_DIR"), Some("/etc/ssl/certs"));
}

/// The Wayland socket's directory is made on demand and made PRIVATE: the
/// socket is the whole session's input and output, and libwayland refuses
/// a runtime dir other users can reach.
#[test]
fn the_wayland_runtime_directory_is_created_private() {
    let profile = profile_text("runtimedir");
    assert_eq!(export(&profile, "XDG_RUNTIME_DIR"), Some("/run/user/0"));
    assert!(
        profile.contains("chmod 0700 \"$XDG_RUNTIME_DIR\""),
        "the runtime directory is no longer private"
    );
}

/// `/etc/passwd` and `/etc/group` are only written when ABSENT, so an
/// account added by a package -- or by `eclipse-useradd` on a running
/// machine -- survives the next incremental rebuild.
#[test]
fn the_base_accounts_are_written_only_when_they_are_absent() {
    let rootfs = scratch("accounts-keep");
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    fs::write(
        etc.join("passwd"),
        "moebius:x:1000:1000::/home/moebius:/bin/sh\n",
    )
    .unwrap();
    LinuxRootfs::install_base_accounts(&rootfs);
    let passwd = fs::read_to_string(etc.join("passwd")).unwrap();
    assert!(
        passwd.contains("moebius:x:1000:"),
        "an account that was already there was clobbered:\n{passwd}"
    );
}

/// `/etc/group`, unlike `/etc/passwd`, is amended rather than left alone:
/// an existing file just gains the `uucp` line it lacks. Twice has to mean
/// the same as once (an incremental rebuild runs this every time), and the
/// amendment has to start on a line of its own -- a group file whose last
/// line had no newline used to come out with the two glued together, which
/// loses both names.
#[test]
fn an_existing_group_file_only_gains_the_uucp_line_and_keeps_its_own() {
    let rootfs = scratch("accounts-amend");
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    fs::write(etc.join("group"), "games:x:35:moebius").unwrap();
    LinuxRootfs::install_base_accounts(&rootfs);
    let once = fs::read_to_string(etc.join("group")).unwrap();
    assert!(
        once.lines().any(|l| l == "games:x:35:moebius"),
        "the last line lost its own identity:\n{once}"
    );
    assert!(
        once.lines().any(|l| l == "uucp:x:14:root"),
        "the uucp group is what a serial/modem device needs:\n{once}"
    );
    LinuxRootfs::install_base_accounts(&rootfs);
    let twice = fs::read_to_string(etc.join("group")).unwrap();
    assert_eq!(
        once, twice,
        "a second rebuild appended the same lines again"
    );
}

/// The check for it is anchored at the START of a line, and that is the
/// point: `uucp` also appears in a group file as a MEMBER
/// (`wheel:x:10:root,uucp` is how a distro hands out dialout rights), and
/// a file that merely mentions the name does not have the group. Counting
/// a mention leaves the rootfs with no gid 14 at all, which is the group
/// a serial or modem device belongs to.
#[test]
fn a_group_file_that_only_mentions_uucp_still_gains_the_group() {
    let rootfs = scratch("group-uucp-member");
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    fs::write(etc.join("group"), "wheel:x:10:root,uucp\n").unwrap();

    LinuxRootfs::install_base_accounts(&rootfs);

    let group = fs::read_to_string(etc.join("group")).unwrap();
    assert!(
        group.lines().any(|l| l == "uucp:x:14:root"),
        "a file that names uucp as a member counted as having the group:\n{group}"
    );
    assert!(
        group.lines().any(|l| l == "wheel:x:10:root,uucp"),
        "the file's own line was not kept:\n{group}"
    );
}

/// The groups a console and a desktop need, and what `root` has to be a
/// member of. `wheel` with no members is a machine where nothing can
/// `su`, and `tty` is what a terminal device belongs to.
#[test]
fn the_base_groups_carry_the_memberships_the_console_needs() {
    let rootfs = scratch("accounts-groups");
    LinuxRootfs::install_base_accounts(&rootfs);
    let group = fs::read_to_string(rootfs.join("etc/group")).unwrap();
    for line in [
        "root:x:0:root",
        "wheel:x:10:root",
        "uucp:x:14:root",
        "tty:x:5:",
    ] {
        assert!(
            group.lines().any(|l| l == line),
            "`{line}` is not in /etc/group:\n{group}"
        );
    }
}

/// Every account's GROUP has to be one the group file provides. `_apt` is
/// the one that bites: APT's sandbox `setgid`s to it before it unpacks
/// anything, so a gid no group names leaves the unpack running as a number
/// nothing can resolve and the files land with an owner `ls -l` prints as
/// a bare digit. The two files are written by the same function and only
/// agree because somebody kept them in step by hand.
#[test]
fn every_account_belongs_to_a_group_that_exists() {
    let rootfs = scratch("accounts-gid");
    LinuxRootfs::install_base_accounts(&rootfs);
    let etc = rootfs.join("etc");
    let passwd = fs::read_to_string(etc.join("passwd")).unwrap();
    let group = fs::read_to_string(etc.join("group")).unwrap();

    let gids: Vec<&str> = group.lines().filter_map(|l| l.split(':').nth(2)).collect();
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next().unwrap_or(line);
        // name:passwd:uid:gid:..., so the gid is two past the uid.
        let gid = fields.nth(2).unwrap_or("");
        assert!(
            gids.contains(&gid),
            "{name} has gid {gid} and no group carries it:\n{group}"
        );
    }
}

/// Neither writer may give `nobody` a shell. It is the account every
/// unprivileged daemon falls back to, and a login shell on it is a login.
#[test]
fn nobody_has_no_shell_to_log_in_with() {
    let base = scratch("nobody-base");
    LinuxRootfs::install_base_accounts(&base);
    let other = scratch("nobody-passwd");
    let etc = other.join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_passwd(&etc, &other);
    for rootfs in [&base, &other] {
        let passwd = fs::read_to_string(rootfs.join("etc/passwd")).unwrap();
        let nobody = passwd
            .lines()
            .find(|l| l.starts_with("nobody:"))
            .expect("no nobody account");
        let shell = nobody.rsplit(':').next().unwrap();
        assert!(
            shell == "/bin/false" || shell == "/sbin/nologin",
            "nobody can log in with {shell}"
        );
    }
}

/// bash resolves `~` through `getpwuid(geteuid())`, i.e. `/etc/passwd`, and
/// greets "I can't find my home directory!" when the entry names a
/// directory that is not there. So the entry and the directory are written
/// together.
#[test]
fn root_gets_a_home_directory_that_exists() {
    let rootfs = scratch("root-home");
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_passwd(&etc, &rootfs);
    let passwd = fs::read_to_string(etc.join("passwd")).unwrap();
    let root = passwd
        .lines()
        .find(|l| l.starts_with("root:"))
        .expect("no root account");
    let home = root.split(':').nth(5).unwrap();
    assert_eq!(home, "/root");
    assert!(
        rootfs.join("root").is_dir(),
        "root's home is in /etc/passwd and not on disk"
    );
}

/// `/etc/group` has TWO writers with DIFFERENT content, each writing only
/// when the file is absent -- so whichever runs first wins, and which one
/// that is depends on the build path. `make` on an existing rootfs (the
/// common case, since a rootfs is checked in) runs `install_base_accounts`
/// first and the `video` group never appears; a from-scratch build runs
/// `write_passwd` first and it does. Everything here runs as root, so
/// nothing has needed `video` yet; this test is here to say the divergence
/// is known rather than to bless it.
#[test]
fn the_two_writers_of_etc_group_disagree_about_the_video_group() {
    let clean = scratch("group-clean");
    let etc = clean.join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_passwd(&etc, &clean);
    assert!(
        fs::read_to_string(etc.join("group"))
            .unwrap()
            .lines()
            .any(|l| l == "video:x:28:"),
        "the from-scratch path is the only one that creates the video group"
    );
    let incremental = scratch("group-incremental");
    LinuxRootfs::install_base_accounts(&incremental);
    assert!(
        !fs::read_to_string(incremental.join("etc/group"))
            .unwrap()
            .lines()
            .any(|l| l.starts_with("video:")),
        "if the base set grew a video group the two writers finally agree, and this test can go"
    );
}

/// The kernel spawns the per-VT shells itself, so busybox init must NOT:
/// a `getty` or an `askfirst` line here means two programs reading the
/// same terminal. What is left is the sysinit hook and the three actions
/// init exists for.
#[test]
fn the_kernel_owns_the_vts_so_the_inittab_has_no_getty() {
    let rootfs = scratch("inittab");
    LinuxRootfs::new(Arch::X86_64).install_busybox_init(&rootfs);
    let raw = fs::read_to_string(rootfs.join("etc/inittab")).unwrap();
    // The file's own comment says "there are NO getty lines here", so a
    // check over the raw text answers about the comment, not the file.
    let inittab: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    for line in [
        "::sysinit:/etc/init.d/rcS",
        "::ctrlaltdel:/bin/busybox reboot",
        "::shutdown:/bin/busybox swapoff -a",
        "::restart:/bin/busybox init",
    ] {
        assert!(
            inittab.lines().any(|l| l == line),
            "`{line}` is gone from the inittab:\n{raw}"
        );
    }
    for word in ["getty", "askfirst", "respawn"] {
        assert!(
            !inittab.contains(word),
            "`{word}` in the inittab fights the kernel for the VTs:\n{raw}"
        );
    }
}

/// `/sbin/init` is a symlink to busybox, and busybox picks its applet from
/// `basename(argv[0])` -- so the link's NAME is what makes it run `init`,
/// and its target has to be busybox and nothing else. The kernel boots
/// `INIT=/sbin/init`.
#[test]
fn sbin_init_is_a_symlink_to_busybox_which_picks_its_applet_from_argv0() {
    let rootfs = scratch("init-link");
    LinuxRootfs::new(Arch::X86_64).install_busybox_init(&rootfs);
    let link = rootfs.join("sbin/init");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("/bin/busybox"),
        "/sbin/init no longer resolves to busybox"
    );
}

/// The sysinit hook is a no-op by design, but it has to be an EXECUTABLE
/// no-op that succeeds: busybox init runs it before anything else, and a
/// hook that cannot be executed or that fails is the first thing a boot
/// reports.
#[test]
fn the_sysinit_hook_is_executable_and_succeeds() {
    use std::os::unix::fs::PermissionsExt;
    let rootfs = scratch("rcs");
    LinuxRootfs::new(Arch::X86_64).install_busybox_init(&rootfs);
    let rcs = rootfs.join("etc/init.d/rcS");
    let mode = fs::metadata(&rcs).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o111, 0o111, "the sysinit hook is {mode:o}");
    let body = fs::read_to_string(&rcs).unwrap();
    assert!(body.starts_with("#!/bin/sh\n"), "the hook has no shebang");
    assert!(
        body.trim_end().ends_with("exit 0"),
        "the hook must succeed; it is the first thing init runs:\n{body}"
    );
}

/// bash does NOT read `/etc/profile` for a non-login interactive shell, so
/// `.bashrc` sources it: without that line a bash opened on a VT has no
/// PATH, no certificate bundle and no session variables. And `/etc/nanorc`
/// is option-only on purpose -- an `include` of the syntax files trips
/// "Mistakes in '/etc/nanorc'" on some nano builds, and nano then refuses
/// to start.
#[test]
fn bash_sources_the_profile_and_nano_loads_with_no_includes() {
    let rootfs = scratch("console");
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_console_configs(&etc, &rootfs);
    let bashrc = fs::read_to_string(rootfs.join("root/.bashrc")).unwrap();
    assert!(
        bashrc.contains("[ -r /etc/profile ] && . /etc/profile"),
        "a non-login bash would start with no environment at all:\n{bashrc}"
    );
    assert!(bashrc.contains("export PS1="), "the prompt is gone");
    assert!(
        bashrc.contains("alias ll='ls -la'"),
        "the one alias the shipped shell has is gone"
    );
    let nanorc = fs::read_to_string(etc.join("nanorc")).unwrap();
    for line in nanorc.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(
            !line.contains("include"),
            "an `include` here makes nano refuse to start on some builds: {line}"
        );
    }
    assert!(
        nanorc.contains("set tabsize 4"),
        "the nano options are gone"
    );
}

/// `/etc/hosts` has to resolve `localhost` on BOTH families and give the
/// machine a name. A missing `::1` line is an IPv6-first resolver timing
/// out on every lookup of its own hostname, and the `127.0.1.1` line is
/// what makes `hostname` resolvable at all -- the Debian convention, and
/// what X and D-Bus clients look up at startup.
#[test]
fn localhost_resolves_on_both_families_and_the_machine_has_its_name() {
    let etc = scratch("hosts").join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_hosts(&etc);
    let hosts = fs::read_to_string(etc.join("hosts")).unwrap();
    let names = |addr: &str| -> Vec<String> {
        hosts
            .lines()
            .filter(|l| l.split_whitespace().next() == Some(addr))
            .flat_map(|l| l.split_whitespace().skip(1).map(String::from))
            .collect()
    };
    assert!(
        names("127.0.0.1").contains(&"localhost".to_string()),
        "localhost has no IPv4 address:\n{hosts}"
    );
    assert!(
        names("::1").contains(&"localhost".to_string()),
        "localhost has no IPv6 address, so an IPv6-first resolver waits for a timeout:\n{hosts}"
    );
    assert!(
        names("127.0.1.1").contains(&"Eclipse".to_string()),
        "the machine's own name does not resolve:\n{hosts}"
    );
}

/// And the IPv6 loopback line carries all three names the convention gives
/// it, not just `localhost`. `/etc/hosts` is the only place
/// `ip6-localhost` and `ip6-loopback` exist at all -- no resolver
/// synthesises either -- so a script or a config that names one gets
/// nothing back, and this file is where a person looks to find out why.
#[test]
fn the_ipv6_loopback_line_carries_the_names_the_convention_gives_it() {
    let etc = scratch("hosts-ip6").join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_hosts(&etc);
    let hosts = fs::read_to_string(etc.join("hosts")).unwrap();
    let line = hosts
        .lines()
        .find(|l| l.split_whitespace().next() == Some("::1"))
        .unwrap_or_else(|| panic!("there is no ::1 line at all:\n{hosts}"));
    for name in ["localhost", "ip6-localhost", "ip6-loopback"] {
        assert!(
            line.split_whitespace().any(|n| n == name),
            "the ::1 line does not name {name}: {line}"
        );
    }
}

/// `/etc/resolv.conf` is written before anything has run DHCP, and it is
/// what every lookup reads until something replaces it. Two DIFFERENT
/// servers is its whole content: with one, a resolver that cannot reach it
/// has nothing to fall back to and every lookup waits out its timeout
/// instead of answering.
#[test]
fn resolv_conf_names_two_different_nameservers() {
    let etc = scratch("resolv-conf").join("etc");
    fs::create_dir_all(&etc).unwrap();
    LinuxRootfs::write_resolv_conf(&etc);

    let conf = fs::read_to_string(etc.join("resolv.conf")).unwrap();
    let servers: Vec<&str> = conf
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver "))
        .collect();
    assert!(
        servers.len() >= 2,
        "one nameserver is no fallback at all:\n{conf}"
    );
    assert_ne!(
        servers[0], servers[1],
        "the fallback is the same server as the first:\n{conf}"
    );
}

/// The NTP wrapper, which is the whole reason the service stays up. busybox
/// `ntpd` first (it needs no privilege-separation user and no `chroot(2)`,
/// neither of which this kernel has), OpenNTPD second -- with the ONLY
/// flags OpenNTPD 6 still accepts. The old `ntpd -d -s -u root` line was a
/// usage error (`-s` went in 6.0, `-u` never existed), so the service died
/// in ~10 ms and init respawned it every 8 s forever, spamming the console.
/// Both run in the FOREGROUND: a client that daemonises exits, and init
/// restarts what it supervises.
#[test]
fn the_ntp_client_runs_in_the_foreground_with_the_flags_openntpd_still_accepts() {
    let rootfs = scratch("ntp");
    LinuxRootfs::write_ntp(&rootfs);
    let conf = fs::read_to_string(rootfs.join("etc/ntpd.conf")).unwrap();
    assert!(
        conf.contains("servers pool.ntp.org"),
        "no server to ask:\n{conf}"
    );
    let wrapper = fs::read_to_string(rootfs.join("usr/local/bin/eclipse-ntpd")).unwrap();
    assert!(
        wrapper.contains("exec /bin/busybox ntpd -n -N -p pool.ntp.org"),
        "busybox ntpd is no longer run in the foreground:\n{wrapper}"
    );
    let openntpd = wrapper
        .lines()
        .map(str::trim)
        .find(|l| l.contains("exec /usr/sbin/ntpd"))
        .expect("the OpenNTPD fallback is gone");
    assert_eq!(
        openntpd, "exec /usr/sbin/ntpd -d",
        "OpenNTPD 6 accepts neither -s nor -u, and a usage error respawns forever"
    );
}

/// The wrapper waits for a default route before it starts: `pool.ntp.org`
/// cannot resolve before DHCP. The wait is bounded (45 tries, 2 s apart) so
/// a machine with no network does not hold the service down forever, and
/// the wrapper is executable -- it is what the service `exec`s.
#[test]
fn the_ntp_wrapper_waits_for_dhcp_within_a_bound_and_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let rootfs = scratch("ntp-wait");
    LinuxRootfs::write_ntp(&rootfs);
    let path = rootfs.join("usr/local/bin/eclipse-ntpd");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o111,
        0o111,
        "the wrapper is {mode:o}, so execve answers EACCES"
    );
    let wrapper = fs::read_to_string(&path).unwrap();
    assert!(
        wrapper.contains("while [ \"$i\" -lt 45 ]; do"),
        "the wait for a default route is gone or unbounded:\n{wrapper}"
    );
    assert!(
        wrapper.contains("ip -4 route show default"),
        "nothing checks for a default route"
    );
}

/// The Rust target triple of each architecture, which is NOT the
/// architecture's own name: rustc knows `riscv64gc`, not `riscv64`, and a
/// triple it does not know is a cross build that fails with eclipse-init
/// missing -- best-effort, so the image silently keeps busybox as PID 1.
#[test]
fn each_architecture_names_the_musl_triple_rustc_knows() {
    for (arch, triple) in [
        (Arch::X86_64, "x86_64-unknown-linux-musl"),
        (Arch::Aarch64, "aarch64-unknown-linux-musl"),
        (Arch::Riscv64, "riscv64gc-unknown-linux-musl"),
    ] {
        assert_eq!(LinuxRootfs::new(arch).musl_rust_triple(), triple);
    }
    assert_ne!(
        LinuxRootfs::new(Arch::Riscv64).musl_rust_triple(),
        format!("{}-unknown-linux-musl", Arch::Riscv64.name()),
        "riscv64 is the one architecture whose triple is not its own name"
    );
}

/// What `install_apk_keys` returns is what apk will actually trust: the
/// caller warns and falls back to `--allow-untrusted` on zero. So the count
/// has to be the number of `.pub` files that really landed, and nothing
/// else in the directory may inflate it.
#[test]
fn the_key_count_is_the_number_of_public_keys_that_landed() {
    let dst = scratch("apk-keys").join("keys");
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("alpine-one.rsa.pub"), b"key\n").unwrap();
    fs::write(dst.join("alpine-two.rsa.pub"), b"key\n").unwrap();
    fs::write(dst.join("README.txt"), b"not a key\n").unwrap();
    let n = LinuxRootfs::install_apk_keys(&dst, "x86_64");
    let pubs = fs::read_dir(&dst)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pub"))
        .count();
    assert_eq!(n, pubs, "the count is not what apk will trust");
    assert!(
        n >= 2,
        "the keys that were already there stopped being counted"
    );
}
