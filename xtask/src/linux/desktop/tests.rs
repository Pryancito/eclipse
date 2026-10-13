//! Los tests de [`super`], el modulo del escritorio.
//!
//! Estaban en un `#[cfg(test)] mod tests { ... }` de primer nivel dentro de
//! `desktop.rs`, que con ellos dentro llegaba a 6013 lineas y se habia pasado
//! de su techo propio en `xtask/src/tamano.rs`. Sacarlos aqui no toca una
//! linea de produccion y deja el fichero por debajo del **techo general de
//! 4000**, asi que `desktop.rs` sale de la lista de excepciones y, como dice
//! el trinquete, ya no puede volver a ella.
//!
//! Es exactamente el arreglo que el comentario de su entrada en esa lista
//! nombraba, y la alternativa --subir el numero-- es la que el trinquete esta
//! escrito para que se vea en el diff en vez de pasar en silencio.

use super::*;

/// The desktop's connectivity probe was `ip route | grep -q default`,
/// which kills `ip` with `SIGPIPE` the moment grep matches — one
/// `[exit] ... killed by signal SIGPIPE` in the dmesg ring per probe, for
/// a check that worked. `have_default_route` dumps the table to a file
/// first, so `ip` reaches EOF.
#[test]
fn the_x11_prepare_probes_never_pipe_into_a_reader_that_exits_early() {
    let dir = std::env::temp_dir().join(format!(
        "eclipse-x11-prepare-sigpipe-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    write_x11_prepare(&dir);
    let script = fs::read_to_string(dir.join("usr/local/bin/eclipse-x11-prepare")).unwrap();

    for line in script.lines() {
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
    assert!(
        script.contains("have_default_route() {"),
        "the route probe helper is gone from eclipse-x11-prepare:\n{script}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The prefs every profile reads (`defaults/pref/eclipse-os.js`), for
/// both install dirs. The media and HTTP/3 lines are what make YouTube
/// play on a CPU-only machine; see `write_firefox_default_prefs`.
/// Every QEMU boot is a first run (the root is a fresh RAM image), so the
/// first-run pages and Firefox's own background work ran at every launch.
/// Safe Browsing and Fission are protection and must stay at their
/// defaults.
#[test]
fn firefox_skips_first_run_and_background_work_but_keeps_its_protections() {
    let dir = std::env::temp_dir().join(format!("eclipse-ff-firstrun-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (_, app) in FIREFOX_PACKAGES {
        fs::create_dir_all(dir.join(app)).unwrap();
    }
    write_firefox_default_prefs(&dir);
    for (pkg, app) in FIREFOX_PACKAGES {
        let prefs = fs::read_to_string(dir.join(app).join("defaults/pref/eclipse-os.js")).unwrap();
        for line in [
            "pref(\"datareporting.policy.dataSubmissionPolicyBypassNotification\", true);\n",
            "pref(\"datareporting.policy.firstRunURL\", \"\");\n",
            "pref(\"browser.aboutwelcome.enabled\", false);\n",
            "pref(\"browser.startup.homepage_override.mstone\", \"ignore\");\n",
            "pref(\"browser.shell.checkDefaultBrowser\", false);\n",
            "pref(\"datareporting.healthreport.uploadEnabled\", false);\n",
            "pref(\"toolkit.telemetry.unified\", false);\n",
            "pref(\"toolkit.telemetry.archive.enabled\", false);\n",
            "pref(\"app.normandy.enabled\", false);\n",
            "pref(\"app.shield.optoutstudies.enabled\", false);\n",
            "pref(\"browser.newtabpage.activity-stream.feeds.section.topstories\", false);\n",
            "pref(\"dom.ipc.processPrelaunch.enabled\", false);\n",
        ] {
            assert!(prefs.contains(line), "{pkg}: missing {line:?} in\n{prefs}");
        }
        for kept in ["browser.safebrowsing", "fission.autostart"] {
            assert!(
                !prefs.contains(kept),
                "{pkg}: {kept} is protection and stays at Firefox's default"
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn firefox_prefers_h264_and_http2_on_a_cpu_only_machine() {
    let dir = std::env::temp_dir().join(format!("eclipse-ff-prefs-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (_, app) in FIREFOX_PACKAGES {
        fs::create_dir_all(dir.join(app)).unwrap();
    }
    write_firefox_default_prefs(&dir);
    for (pkg, app) in FIREFOX_PACKAGES {
        let prefs = fs::read_to_string(dir.join(app).join("defaults/pref/eclipse-os.js")).unwrap();
        for line in [
            "pref(\"browser.cache.disk.enable\", false);\n",
            "pref(\"browser.cache.memory.capacity\", 98304);\n",
            "pref(\"gfx.webrender.all\", true);\n",
            "pref(\"layers.gpu-process.enabled\", false);\n",
            "pref(\"toolkit.asyncshutdown.crash_timeout\", 300000);\n",
            "pref(\"media.av1.enabled\", false);\n",
            "pref(\"media.mediasource.vp9.enabled\", false);\n",
            "pref(\"network.http.http3.enable\", false);\n",
            "pref(\"browser.sessionstore.interval\", 60000);\n",
        ] {
            assert!(prefs.contains(line), "{pkg}: missing {line:?} in\n{prefs}");
        }
        // H.264 is what is left: nothing here may turn system ffmpeg off.
        assert!(
            !prefs.contains("media.ffmpeg.enabled"),
            "{pkg}: H.264 comes from system libavcodec, keep it on"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The two session wrappers must be valid POSIX shell (`sh -n`), and the
/// GTK-caches oneshot must compile schemas and write the private pixbuf
/// loader cache -- the two files Firefox chrome text and GTK icons hang on.
#[test]
fn gtk_caches_and_firefox_wrappers_parse_and_cover_both_caches() {
    let dir = std::env::temp_dir().join(format!("eclipse-gtkcaches-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_gtk_caches_wrapper(&dir);
    write_firefox_wrapper(&dir);
    for name in ["eclipse-gtk-caches", "eclipse-firefox"] {
        let path = dir.join("usr/local/bin").join(name);
        let src = fs::read_to_string(&path).unwrap();
        assert!(src.starts_with("#!/bin/sh\n"), "{name}: shebang");
        let st = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success(), "{name}: sh -n rejected the script");
    }
    let caches = fs::read_to_string(dir.join("usr/local/bin/eclipse-gtk-caches")).unwrap();
    assert!(caches.contains("glib-compile-schemas \"$sd\""));
    assert!(caches.contains("gschemas.compiled"));
    assert!(caches.contains("gdk-pixbuf-query-loaders > \"$pc.tmp\""));
    assert!(caches.contains("/root/.cache/pixbuf-loaders.cache"));
    let ff = fs::read_to_string(dir.join("usr/local/bin/eclipse-firefox")).unwrap();
    assert!(ff.contains("export NO_AT_BRIDGE=1\n"));
    // Rapid-release is the shipped package; ESR stays as the fallback.
    assert!(
        ff.contains("for c in firefox firefox-esr; do\n"),
        "the wrapper must try firefox before firefox-esr"
    );
    assert!(ff.contains("(apk add firefox)"));
    assert!(ff.contains("export MOZ_ENABLE_WAYLAND=1\n"));
    assert!(
        ff.contains("MOZ_ACCELERATED=1"),
        "GPU path must enable MOZ_ACCELERATED"
    );
    assert!(
        ff.contains("nvidia.wlr_pixman"),
        "software kill-switch must be mentioned"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Firefox must land on its native Wayland backend from EVERY launch
/// path, not only through the wrapper: a `firefox` typed in a terminal
/// reads `/etc/profile`; anything labwc launches (menu, keybind,
/// autostart) reads labwc's environment file; init-started children get
/// eclipse-init's CHILD_ENV (checked in that crate). One pin per copy.
#[test]
fn firefox_runs_on_native_wayland_from_every_launch_path() {
    let dir = std::env::temp_dir().join(format!("eclipse-moz-wayland-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_environment(&dir);
    write_firefox_wrapper(&dir);
    let etc = dir.join("etc");
    fs::create_dir_all(&etc).unwrap();
    super::super::LinuxRootfs::write_profile(&etc);

    let profile = fs::read_to_string(etc.join("profile")).unwrap();
    let environment = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    let wrapper = fs::read_to_string(dir.join("usr/local/bin/eclipse-firefox")).unwrap();
    assert!(
        profile.contains("\nexport MOZ_ENABLE_WAYLAND=1\n"),
        "/etc/profile must export MOZ_ENABLE_WAYLAND=1"
    );
    assert!(
        environment.contains("\nMOZ_ENABLE_WAYLAND=1\n"),
        "labwc's environment file must set MOZ_ENABLE_WAYLAND=1"
    );
    assert!(
        wrapper.contains("\nexport MOZ_ENABLE_WAYLAND=1\n"),
        "the wrapper must export MOZ_ENABLE_WAYLAND=1"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// GTK must find its pixbuf loaders from EVERY launch path too. The
/// registry lives at a private path the gtk-caches oneshot writes at
/// boot, and only labwc's environment file named it: a `firefox` typed
/// into a dock terminal (eclipse-init's CHILD_ENV) or a login shell
/// (/etc/profile) decoded no image, "Could not load a pixbuf from icon
/// theme". Same three copies as the Wayland pin, plus the wrapper, and
/// the path must be the one the oneshot writes.
#[test]
fn gtk_finds_its_pixbuf_loaders_from_every_launch_path() {
    let dir = std::env::temp_dir().join(format!("eclipse-pixbuf-env-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_environment(&dir);
    write_firefox_wrapper(&dir);
    write_gtk_caches_wrapper(&dir);
    let etc = dir.join("etc");
    fs::create_dir_all(&etc).unwrap();
    super::super::LinuxRootfs::write_profile(&etc);

    const REGISTRY: &str = "/root/.cache/pixbuf-loaders.cache";
    let profile = fs::read_to_string(etc.join("profile")).unwrap();
    let environment = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    let wrapper = fs::read_to_string(dir.join("usr/local/bin/eclipse-firefox")).unwrap();
    let caches = fs::read_to_string(dir.join("usr/local/bin/eclipse-gtk-caches")).unwrap();
    assert!(
        caches.contains(&format!("pc=${{GDK_PIXBUF_MODULE_FILE:-{REGISTRY}}}\n")),
        "the oneshot must write the registry at the path the others name"
    );
    assert!(
        profile.contains(&format!("\nexport GDK_PIXBUF_MODULE_FILE={REGISTRY}\n")),
        "/etc/profile must export GDK_PIXBUF_MODULE_FILE"
    );
    assert!(
        profile.contains("\nexport GSETTINGS_BACKEND=memory\n"),
        "/etc/profile must export GSETTINGS_BACKEND=memory"
    );
    assert!(
        environment.contains(&format!("\nGDK_PIXBUF_MODULE_FILE={REGISTRY}\n")),
        "labwc's environment file must set GDK_PIXBUF_MODULE_FILE"
    );
    assert!(environment.contains("\nGSETTINGS_BACKEND=memory\n"));
    assert!(
        wrapper.contains(&format!(
            "\nexport GDK_PIXBUF_MODULE_FILE=\"${{GDK_PIXBUF_MODULE_FILE:-{REGISTRY}}}\"\n"
        )),
        "the wrapper must default GDK_PIXBUF_MODULE_FILE to the registry"
    );
    assert!(wrapper.contains("\nexport GSETTINGS_BACKEND=\"${GSETTINGS_BACKEND:-memory}\"\n"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_file_browser_has_a_menu_entry_that_points_at_the_wrapper() {
    // Through the wrapper, never at `lunarfile` itself: the wrapper is what
    // falls back when the binary is missing, so an entry naming the binary
    // would be a dead row on an image whose build of it failed.
    let dir = std::env::temp_dir().join(format!("eclipse-fileent-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    write_files_desktop_entry(&dir);
    let e = fs::read_to_string(dir.join("root/.local/share/applications/eclipse-files.desktop"))
        .unwrap();
    assert!(e.starts_with("[Desktop Entry]\n"), "{e}");
    assert!(e.contains("\nExec=/usr/local/bin/eclipse-files\n"), "{e}");
    assert!(
        e.contains("\nTryExec=/usr/local/bin/eclipse-files\n"),
        "{e}"
    );
    assert!(!e.contains("Exec=lunarfile"), "{e}");
    // `apps::parse_desktop_text` only keeps Type=Application entries that
    // are not NoDisplay, so a missing Type is a row that never appears.
    assert!(
        e.contains("\nType=Application\n") && !e.contains("NoDisplay"),
        "{e}"
    );
    assert!(e.contains("\nMimeType=inode/directory;\n"), "{e}");
    let _ = fs::remove_dir_all(&dir);
}

/// The `.desktop` override only shadows the packaged entry when it has
/// the SAME id, and `firefox` ships `firefox.desktop` with icons named
/// `firefox`. An override called `firefox-esr.desktop` on a rapid-release
/// image shadows nothing: the menu shows the stock entry (no wrapper)
/// plus ours, with an icon that does not exist. So the override follows
/// the installed package, and switching packages on an incremental build
/// removes the stale one.
#[test]
fn firefox_desktop_override_follows_the_installed_package() {
    let dir = std::env::temp_dir().join(format!("eclipse-ff-desktop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let apps = dir.join("root/.local/share/applications");
    let entry = |id: &str| apps.join(format!("{id}.desktop"));

    // Nothing installed (an offline build): the default package's entry,
    // so a later `apk add firefox` is shadowed from the start.
    fs::create_dir_all(&dir).unwrap();
    write_firefox_desktop_override(&dir);
    assert!(entry("firefox").is_file(), "default entry");
    assert!(
        !entry("firefox-esr").exists(),
        "no entry for a package that is not there"
    );

    // ESR only.
    fs::create_dir_all(dir.join("usr/lib/firefox-esr")).unwrap();
    write_firefox_desktop_override(&dir);
    assert!(entry("firefox-esr").is_file());
    assert!(
        !entry("firefox").exists(),
        "stale rapid-release entry must go"
    );
    let esr = fs::read_to_string(entry("firefox-esr")).unwrap();
    assert!(esr.contains("\nIcon=firefox-esr\n"), "{esr}");

    // Both.
    fs::create_dir_all(dir.join("usr/lib/firefox")).unwrap();
    write_firefox_desktop_override(&dir);
    assert!(entry("firefox").is_file() && entry("firefox-esr").is_file());

    // Rapid release only, the shipped configuration.
    fs::remove_dir_all(dir.join("usr/lib/firefox-esr")).unwrap();
    write_firefox_desktop_override(&dir);
    assert!(!entry("firefox-esr").exists(), "stale ESR entry must go");
    let ff = fs::read_to_string(entry("firefox")).unwrap();
    assert!(ff.contains("\nIcon=firefox\n"), "{ff}");
    assert!(
        ff.contains("\nExec=/usr/local/bin/eclipse-firefox %u\n"),
        "{ff}"
    );
    assert!(
        ff.contains("\nTryExec=/usr/local/bin/eclipse-firefox\n"),
        "{ff}"
    );
    assert!(ff.starts_with("[Desktop Entry]\n"));
    let _ = fs::remove_dir_all(&dir);
}

/// Wads already in the rootfs -- put there by the `freedoom` package, or
/// by hand -- must short-circuit the fetch. Getting this wrong means every
/// image build re-downloads and re-unpacks 57 MiB it already had, and
/// overwrites a wad the user deliberately dropped in.
#[test]
fn freedoom_fetch_leaves_existing_iwads_alone() {
    let dir = std::env::temp_dir().join(format!("eclipse-iwad-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    // The layout Alpine's own package produces: prefix=/usr, waddir=
    // /share/games/doom (freedoom's Makefile).
    let wads = dir.join("usr/share/games/doom");
    fs::create_dir_all(&wads).unwrap();
    for w in ["freedoom1.wad", "freedoom2.wad"] {
        fs::write(wads.join(w), b"not really a wad").unwrap();
    }

    ensure_freedoom_iwads(&dir);

    for w in ["freedoom1.wad", "freedoom2.wad"] {
        assert_eq!(
            fs::read(wads.join(w)).unwrap(),
            b"not really a wad",
            "{w} was overwritten although it was already present"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The Freedoom launcher must be valid shell, must always pass `-iwad`
/// (an engine launched without it opens GZDoom's GTK IWAD picker, which is
/// the one dialog a first-time user should never meet), must prefer a
/// software engine, and must be what the menu and the `.desktop` entries
/// actually run.
#[test]
fn freedoom_wrapper_parses_and_always_picks_an_iwad() {
    let dir = std::env::temp_dir().join(format!("eclipse-freedoom-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_freedoom_wrapper(&dir);
    write_freedoom_desktop_entries(&dir);

    let path = dir.join("usr/local/bin/eclipse-freedoom");
    let src = fs::read_to_string(&path).unwrap();
    assert!(src.starts_with("#!/bin/sh\n"), "shebang");
    let st = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .status()
        .unwrap();
    assert!(st.success(), "sh -n rejected eclipse-freedoom");
    for engine in ["chocolate-doom", "gzdoom"] {
        assert!(src.contains(engine), "{engine} should be tried");
    }
    // Every exec of an ENGINE carries -iwad (`exec >>"$LOG"` is the log
    // redirection, not a program).
    for line in src
        .lines()
        .filter(|l| l.contains("exec ") && !l.contains("exec >>"))
    {
        assert!(
            line.contains("-iwad"),
            "engine launched without an IWAD: {line}"
        );
    }
    // Compare where the engines are RUN, not where their names first
    // appear: a comment mentioning one of them must not decide this.
    assert!(
        src.find("for e in chocolate-doom").unwrap() < src.find("command -v gzdoom").unwrap(),
        "the software engines must be tried before gzdoom"
    );
    assert!(
        src.contains("LIBGL_ALWAYS_SOFTWARE=1"),
        "pixman needs llvmpipe"
    );
    assert!(
        src.contains("/tmp/freedoom.log"),
        "failures must be readable later"
    );
    // "no IWAD found" was a dead end: it named a fix (`apk add freedoom`)
    // that cannot work on a machine with no mirror, and said nothing about
    // where it had looked. The report has to be actionable on its own.
    assert!(
        src.contains("Directories searched:"),
        "the no-IWAD path must name the directories it walked"
    );
    // The engine must not inherit the live session bus: SDL_Init calls
    // SDL_DBus_Init before anything else and libdbus has no timeout on the
    // handshake. This is hygiene, not the fix for the banner hang -- that
    // was two kernel bugs, see docs/README-desktop.md.
    assert!(
        src.contains("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/eclipse-freedoom-no-bus"),
        "the engine must be pointed away from the session bus"
    );
    assert!(
        src.contains("ECLIPSE_FREEDOOM_BUS"),
        "keeping the real bus must stay one env var away"
    );
    assert!(
        !src.contains("apk add freedoom"),
        "the wads ship in the image; `apk add` is not the fix"
    );

    for (file, phase) in [("freedoom1.desktop", "1"), ("freedoom2.desktop", "2")] {
        let entry =
            fs::read_to_string(dir.join("root/.local/share/applications").join(file)).unwrap();
        assert!(
            entry.contains(&format!("Exec=/usr/local/bin/eclipse-freedoom {phase}")),
            "{file} must run the wrapper"
        );
    }
    for menu in [MENU_ES, MENU_EN] {
        let text = std::str::from_utf8(menu).unwrap();
        assert!(
            text.contains("/usr/local/bin/eclipse-freedoom 1")
                && text.contains("/usr/local/bin/eclipse-freedoom 2"),
            "both phases belong in the desktop menu"
        );
        assert!(
            text.contains("/bin/eclipse-dbusd --selftest"),
            "the session-bus probe belongs in the desktop menu"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The two menus must offer the same things, each in its own language.
///
/// The explorer entry shipped labelled "Archivos" in both menus, so
/// switching the session to English left one Spanish word in the root
/// menu. The command list is the half that catches the next item added
/// to one menu and forgotten in the other.
#[test]
fn both_menus_offer_the_same_commands_each_in_its_own_language() {
    fn commands(menu: &[u8]) -> Vec<String> {
        let text = std::str::from_utf8(menu).unwrap();
        text.split("<command>")
            .skip(1)
            .map(|rest| rest.split("</command>").next().unwrap().to_string())
            .collect()
    }
    assert_eq!(
        commands(MENU_ES),
        commands(MENU_EN),
        "an item added to one menu must be added to the other"
    );

    let en = std::str::from_utf8(MENU_EN).unwrap();
    assert!(
        en.contains(r#"<item label="Files">"#),
        "the English menu must label the explorer in English"
    );
    for spanish in ["Archivos", "Ejecutar", "Salir de la sesion", "Teclado"] {
        assert!(
            !en.contains(spanish),
            "the English menu still says {spanish:?}"
        );
    }
    let es = std::str::from_utf8(MENU_ES).unwrap();
    assert!(
        es.contains(r#"<item label="Archivos">"#),
        "the Spanish menu must keep its own label"
    );
}

/// Autostart file is intentionally absent (clients come from init).
#[test]
fn autostart_is_absent() {
    let dir = std::env::temp_dir().join(format!("eclipse-desktop-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_autostart(&dir);
    assert!(
        !dir.join("root/.config/labwc/autostart").exists(),
        "autostart must not exist — labwc would `sh` it and ash SIGSEGVs"
    );
    assert!(
        dir.join("root/.config/labwc/autostart.README").is_file(),
        "breadcrumb README should explain the move to init services"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Run one of the generated boot steps with `--boot` under `HOME`, the way
/// eclipse-init does, and answer whether a shell could be started at all.
fn run_boot_step(dir: &Path, name: &str) -> Option<std::process::Child> {
    std::process::Command::new("sh")
        .arg(dir.join("usr/local/bin").join(name))
        .arg("--boot")
        .env("HOME", dir.join("root"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

#[test]
fn a_boot_step_waits_for_the_lock_on_the_labwc_environment() {
    // eclipse-init starts eclipse-kbd, eclipse-locale and eclipse-tz AT THE
    // SAME TIME (they are independent of each other, and one after another
    // they were 85% of init's boot). All three upsert a key into the same
    // ~/.config/labwc/environment with a read-modify-write, so without
    // mutual exclusion each one reads the file as it was and the last `mv`
    // wins: two of the three keys silently disappear, and the symptom lands
    // a boot later as a keyboard in the wrong layout or an untranslated
    // session.
    //
    // Tested by HOLDING the lock rather than by racing: a race that depends
    // on three host processes interleaving passes on a fast machine whether
    // the lock is there or not, which is a test that checks nothing.
    let dir = std::env::temp_dir().join(format!("eclipse-envlock-held-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_tz(&dir);
    let envf = dir.join("root/.config/labwc/environment");
    let _ = fs::create_dir_all(envf.parent().unwrap());
    let _ = fs::write(&envf, "TZ=Etc/UTC\n");
    let lock = dir.join("root/.config/labwc/environment.lock");
    fs::create_dir_all(&lock).unwrap();
    let Some(mut kid) = run_boot_step(&dir, "eclipse-tz") else {
        eprintln!("skipping: no POSIX sh on this host");
        let _ = fs::remove_dir_all(&dir);
        return;
    };
    // While the lock is held the file must keep the value it had. Half a
    // second is many times the whole uncontended run of this script.
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        fs::read_to_string(&envf).unwrap_or_default(),
        "TZ=Etc/UTC\n",
        "the boot step wrote $ENVF while the lock was held"
    );
    fs::remove_dir(&lock).unwrap();
    let _ = kid.wait();
    let env = fs::read_to_string(&envf).unwrap_or_default();
    assert!(
        env.lines().any(|l| l.starts_with("TZ=")),
        "the boot step never wrote TZ after the lock was dropped:\n{env}"
    );
    assert_eq!(
        env.lines().filter(|l| l.starts_with("TZ=")).count(),
        1,
        "TZ ended up in the file twice:\n{env}"
    );
    assert!(!lock.exists(), "the lock was left behind");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_lock_delays_a_boot_step_and_never_wedges_it() {
    // The property that matters more than the race: a lock nobody will ever
    // drop -- a machine that lost power inside the critical section leaves
    // the directory behind on the installed root -- must not stop the boot.
    // One hanging boot script is how a whole machine ends up wedged (the
    // reason a oneshot has a timeout at all), so the wait is bounded and
    // then the step takes the lock anyway. Losing one key on one boot is the
    // cheaper failure by a wide margin.
    let dir = std::env::temp_dir().join(format!("eclipse-envlock-stale-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_tz(&dir);
    let envf = dir.join("root/.config/labwc/environment");
    let _ = fs::create_dir_all(envf.parent().unwrap());
    let lock = dir.join("root/.config/labwc/environment.lock");
    fs::create_dir_all(&lock).unwrap();
    let Some(mut kid) = run_boot_step(&dir, "eclipse-tz") else {
        eprintln!("skipping: no POSIX sh on this host");
        let _ = fs::remove_dir_all(&dir);
        return;
    };
    // Bounded from Rust as well as in the script: a boot step that really
    // does hang must fail this test, not hang the whole suite with it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match kid.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            Ok(None) => {
                let _ = kid.kill();
                let _ = fs::remove_dir_all(&dir);
                panic!("eclipse-tz --boot never returned with a stale env lock in place");
            }
            Err(e) => panic!("cannot wait for eclipse-tz: {e}"),
        }
    }
    let env = fs::read_to_string(&envf).unwrap_or_default();
    assert!(
        env.lines().any(|l| l.starts_with("TZ=")),
        "the boot step gave up on writing TZ instead of taking the stale lock:\n{env}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_writer_of_the_labwc_environment_takes_the_lock() {
    // The invariant, pinned where a fourth writer would be added: a step
    // that upserts a key into that file without `env_lock` reintroduces the
    // lost-update race above, and a fixed `$ENVF.new` temporary collides
    // with a concurrent writer even under the lock if it is ever stolen.
    let dir = std::env::temp_dir().join(format!("eclipse-envlock-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_kbd(&dir);
    write_eclipse_locale(&dir);
    write_eclipse_tz(&dir);
    for name in ["eclipse-kbd", "eclipse-locale", "eclipse-tz"] {
        let text = fs::read_to_string(dir.join("usr/local/bin").join(name)).unwrap();
        assert!(text.contains("env_lock"), "{name} upserts $ENVF unlocked");
        assert!(text.contains("env_unlock"), "{name} never drops the lock");
        assert!(
            text.contains("tmp=\"$ENVF.$$\""),
            "{name} must write through a pid-unique temporary"
        );
        assert!(
            !text.contains("> \"$ENVF.new\""),
            "{name} still writes the shared $ENVF.new temporary"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eclipse_kbd_script_and_xkb_default() {
    let dir = std::env::temp_dir().join(format!("eclipse-kbd-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_kbd(&dir);
    write_labwc_environment(&dir);
    write_labwc_rc(&dir);
    let script = dir.join("usr/local/bin/eclipse-kbd");
    assert!(script.is_file());
    if let Ok(status) = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
    {
        assert!(status.success(), "eclipse-kbd does not parse as sh");
    }
    let env = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    assert!(
        env.lines().any(|l| l == "XKB_DEFAULT_LAYOUT=es"),
        "labwc environment must default XKB_DEFAULT_LAYOUT=es"
    );
    let conf = fs::read_to_string(dir.join("etc/eclipse/keyboard")).unwrap();
    assert!(conf.contains("layout=es"));
    let rc = fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap();
    assert!(rc.contains("eclipse-kbd toggle"), "W-K keybind missing");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eclipse_locale_script_and_lang_default() {
    let dir = std::env::temp_dir().join(format!("eclipse-locale-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_locale(&dir);
    write_labwc_menu(&dir);
    write_labwc_environment(&dir);
    let script = dir.join("usr/local/bin/eclipse-locale");
    assert!(script.is_file());
    if let Ok(status) = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
    {
        assert!(status.success(), "eclipse-locale does not parse as sh");
    }
    let env = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    assert!(
        env.lines().any(|l| l == "LANG=es_ES.UTF-8"),
        "labwc environment must default LANG=es_ES.UTF-8"
    );
    assert!(env.lines().any(|l| l == "LANGUAGE=es:en"));
    let conf = fs::read_to_string(dir.join("etc/eclipse/locale")).unwrap();
    assert!(conf.contains("lang=es"));
    let menu = fs::read_to_string(dir.join("root/.config/labwc/menu.xml")).unwrap();
    assert!(menu.contains("Teclado (es/us)"));
    let en = fs::read_to_string(dir.join("root/.config/labwc/menu.en.xml")).unwrap();
    assert!(en.contains("Keyboard (es/us)"));
    assert!(!en.contains("Teclado (es/us)"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn kde_look_is_wired_through_theme_rc_and_foot() {
    let dir = std::env::temp_dir().join(format!("eclipse-look-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_theme(&dir);
    write_labwc_rc(&dir);
    write_foot_config(&dir);
    write_eclipse_look(&dir);
    write_kde_helpers(&dir);

    // Both themes ship, so `eclipse-look` can switch either way.
    let breeze =
        fs::read_to_string(dir.join("usr/share/themes/Breeze-Dark/openbox-3/themerc")).unwrap();
    assert!(
        breeze.contains("#3daee9"),
        "Breeze-Dark must use KDE's accent"
    );
    let win11 =
        fs::read_to_string(dir.join("usr/share/themes/Win11-Dark/openbox-3/themerc")).unwrap();
    assert!(
        win11.contains("#0078d4"),
        "Win11-Dark must use Windows' accent"
    );
    assert!(dir
        .join("usr/share/themes/Eclipse-Dark/openbox-3/themerc")
        .is_file());

    // The theme name must be on a line eclipse-look's sed can rewrite,
    // and must name a theme that exists.
    let rc = fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap();
    assert!(
        rc.contains("<name>Eclipse-Dark</name>"),
        "rc.xml must name the shipped theme verbatim for eclipse-look to swap it"
    );

    // Every command a keybind runs has to exist, or the key silently does
    // nothing -- the failure mode that is hardest to notice on hardware.
    for cmd in [
        "/usr/local/bin/eclipse-run",
        "/usr/local/bin/eclipse-files",
        "/usr/local/bin/eclipse-showdesktop",
    ] {
        assert!(rc.contains(cmd), "rc.xml should bind a key to {cmd}");
        let path = dir.join(cmd.trim_start_matches('/'));
        assert!(path.is_file(), "{cmd} is bound but not installed");
        if let Ok(status) = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
        {
            assert!(status.success(), "{cmd} does not parse as sh");
        }
    }

    // KDE's own keys.
    for key in ["A-space", "A-F2", "C-A-T", "W-E", "W-D", "C-F1"] {
        assert!(
            rc.contains(&format!("key=\"{key}\"")),
            "rc.xml missing {key}"
        );
    }

    let script = dir.join("usr/local/bin/eclipse-look");
    if let Ok(status) = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
    {
        assert!(status.success(), "eclipse-look does not parse as sh");
    }
    let conf = fs::read_to_string(dir.join("etc/eclipse/look")).unwrap();
    assert!(
        conf.contains("look=eclipse"),
        "the image ships Eclipse's own look"
    );
    // Every look eclipse-look accepts must have a theme that exists, or
    // switching to it leaves labwc on its built-in defaults.
    let script = fs::read_to_string(dir.join("usr/local/bin/eclipse-look")).unwrap();
    for (name, theme) in [
        ("win11", "Win11-Dark"),
        ("kde", "Breeze-Dark"),
        ("eclipse", "Eclipse-Dark"),
    ] {
        assert!(script.contains(theme), "eclipse-look does not know {theme}");
        assert!(script.contains(name), "eclipse-look does not accept {name}");
        assert!(dir
            .join("usr/share/themes")
            .join(theme)
            .join("openbox-3/themerc")
            .is_file());
        assert!(
            dir.join("root/.config/foot")
                .join(format!("foot.{name}.ini"))
                .is_file()
                || name == "win11" && dir.join("root/.config/foot/foot.win11.ini").is_file(),
            "no foot palette for the {name} look"
        );
    }

    // Both palettes present, and foot.ini a copy of the active one.
    let kde = fs::read_to_string(dir.join("root/.config/foot/foot.kde.ini")).unwrap();
    let win = fs::read_to_string(dir.join("root/.config/foot/foot.win11.ini")).unwrap();
    let ecl = fs::read_to_string(dir.join("root/.config/foot/foot.eclipse.ini")).unwrap();
    let active = fs::read_to_string(dir.join("root/.config/foot/foot.ini")).unwrap();
    assert_eq!(ecl, active, "foot.ini must start as the shipped look");
    assert!(
        kde.contains("background=232629"),
        "KDE terminal palette is Breeze"
    );
    assert!(
        win.contains("background=0c0c0c"),
        "Windows terminal palette is Campbell"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The Qt policy is split between the static session environment and the
/// wrapper (the renderer-dependent half), exactly like SDL's. What must
/// NOT appear is as load-bearing as what must: `QT_QPA_PLATFORMTHEME=kde`
/// and `XDG_CURRENT_DESKTOP=KDE` are the two lines every "KDE on labwc"
/// recipe recommends, and both point at pieces this image does not have.
#[test]
fn qt_policy_is_present_without_claiming_to_be_kde() {
    let dir = std::env::temp_dir().join(format!("eclipse-qt-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_environment(&dir);
    write_labwc_wrapper(&dir);
    let env = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    assert!(env.lines().any(|l| l == "QT_QPA_PLATFORM=wayland;xcb"));
    assert!(env
        .lines()
        .any(|l| l == "XDG_CURRENT_DESKTOP=labwc:wlroots"));
    // Mentioned in a comment saying why it is absent, never as a setting.
    assert!(
        !env.lines().any(|l| {
            let l = l.trim();
            !l.starts_with('#') && l.starts_with("QT_QPA_PLATFORMTHEME=")
        }),
        "no plasma-integration plugin exists here; setting the theme only warns"
    );
    assert!(!env.lines().any(|l| l.trim() == "XDG_CURRENT_DESKTOP=KDE"));
    let wrapper = fs::read_to_string(dir.join("usr/local/bin/labwc")).unwrap();
    assert!(
        wrapper.contains("QT_QUICK_BACKEND:=software"),
        "Qt Quick must fall back to its software raster on the pixman session"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// GTK's file dialog is a portal call only when `GTK_USE_PORTAL=1`, and
/// the var is this session's decision. Exporting it with no portal on the
/// bus is the one way to make things WORSE than having no portal at all:
/// the application skips the dialog it has of its own. So the wrapper sets
/// it only when the stamp says a portal is serving, and the guard is run
/// here -- as the shipped text, not as a copy of it -- in both states.
#[test]
fn gtk_is_only_pointed_at_a_portal_that_is_really_serving() {
    let dir = std::env::temp_dir().join(format!(
        "eclipse-portalenv-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    write_labwc_wrapper(&dir);
    let wrapper = fs::read_to_string(dir.join("usr/local/bin/labwc")).unwrap();
    // The var is never exported outside the test: a single unconditional
    // line anywhere in the wrapper would undo the whole guard.
    assert_eq!(
        wrapper.matches("export GTK_USE_PORTAL").count(),
        1,
        "{wrapper}"
    );
    assert!(!wrapper.contains("GTK_USE_PORTAL=1; export GTK_USE_PORTAL\n          if"));
    // The wait is bounded and only happens when there is something to wait
    // for; an unbounded one would hang the session on a broken portal.
    assert!(wrapper.contains("\"$i\" -lt 3"), "{wrapper}");
    assert!(wrapper.contains("-x /bin/eclipse-portald"), "{wrapper}");

    // Now run the guard itself, sliced out of what is shipped.
    let from = wrapper.find("PORTAL_READY=").expect("el guardia");
    let tail = &wrapper[from..];
    let to = tail.find("sin portal").expect("la rama sin portal");
    let to = to + tail[to..].find("\nfi\n").expect("el fi final") + 4;
    let guard = &tail[..to];
    let run = |stamp: bool| -> String {
        let home = dir.join(if stamp { "con" } else { "sin" });
        let _ = fs::create_dir_all(&home);
        if stamp {
            fs::write(home.join("eclipse-portal.ready"), "x").unwrap();
        }
        let script = format!(
            "XDG_RUNTIME_DIR={}\n{guard}\nprintf '%s' \"${{GTK_USE_PORTAL-sin}}\"\n",
            home.display()
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert_eq!(run(true), "1", "con portal, GTK tiene que usarlo");
    assert_eq!(run(false), "sin", "sin portal, GTK usa su propio dialogo");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eclipse_tz_script_and_spain_default() {
    let dir = std::env::temp_dir().join(format!("eclipse-tz-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_eclipse_tz(&dir);
    write_labwc_environment(&dir);
    let script = dir.join("usr/local/bin/eclipse-tz");
    assert!(script.is_file());
    if let Ok(status) = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
    {
        assert!(status.success(), "eclipse-tz does not parse as sh");
    }
    let env = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();
    assert!(
        env.lines().any(|l| l == "TZ=Europe/Madrid"),
        "labwc environment must default TZ=Europe/Madrid"
    );
    let conf = fs::read_to_string(dir.join("etc/eclipse/timezone")).unwrap();
    assert!(conf.contains("country=ES"));
    assert!(conf.contains("tz=Europe/Madrid"));
    let _ = fs::remove_dir_all(&dir);
}

/// The SDL policy is asserted in THREE generated files -- the labwc
/// wrapper, `/etc/profile` and labwc's `environment` -- plus a fourth copy
/// compiled into eclipse-init (`tools/eclipse-init`, `push_sdl_render_env`).
/// They must agree, or a shell-launched SDL app and an init-launched one
/// would render through different stacks. This pins the two halves:
/// renderer-independent backends everywhere, and the render driver per
/// compositor renderer in every branch of the two shell copies.
/// Cut the renderer gate out of a generated shell file: from the `if` that
/// tests `nvidia.nouveau_uapi` to the `fi` that closes it, tracking depth so
/// the inner `if`s do not end the block early.
fn renderer_gate(text: &str) -> String {
    let mut out = Vec::new();
    let mut depth = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if depth == 0 {
            if t.starts_with("if grep -q 'nvidia\\.nouveau_uapi'") {
                depth = 1;
                out.push(line);
            }
            continue;
        }
        out.push(line);
        // A one-line `if ...; then ...; fi` would need real parsing; the
        // generated gate has none, so counting openers and closers is exact.
        if t.starts_with("if ") || t == "if" {
            depth += 1;
        } else if t == "fi" || t.starts_with("fi ") || t == "fi;" {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
    }
    assert!(depth == 0 && !out.is_empty(), "no renderer gate found");
    out.join("\n")
}

/// Run a renderer gate against a faked `/proc/cmdline` and `card0` vendor,
/// and report the variables it exported. This EXECUTES the branch logic
/// instead of matching its text, so a gate that parses but picks the wrong
/// arm is caught.
fn run_gate(gate: &str, cmdline: &str, vendor: Option<&str>) -> Vec<(String, String)> {
    // No parentheses or spaces in this path: it is substituted into a shell
    // script unquoted, and `ThreadId(3)` made `sh` report a syntax error
    // that looked like a fault in the generated gate.
    let tag: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let dir = std::env::temp_dir().join(format!("eclipse-gate-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sys")).unwrap();
    fs::write(dir.join("cmdline"), cmdline).unwrap();
    if let Some(v) = vendor {
        fs::write(dir.join("sys/vendor"), v).unwrap();
    }
    let base = dir.display().to_string();
    let script = format!(
        "{}\nenv\n",
        gate.replace("/proc/cmdline", &format!("{base}/cmdline"))
            .replace(
                "/sys/class/drm/card0/device/vendor",
                &format!("{base}/sys/vendor")
            )
    );
    // `sh` is POSIX and present on every runner this crate is built on. If
    // it were missing this test must FAIL, not quietly pass: a gate nobody
    // ran is the thing being fixed here.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("sh must be available to run the renderer gate");
    assert!(
        out.status.success(),
        "gate failed for {cmdline:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn gate_var(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// The renderer policy is written THREE times: here in the labwc wrapper, in
/// `/etc/profile`, and in `eclipse-init`'s `build_child_env`. The wrapper and
/// the profile are both generated by this crate, so they can be run
/// side by side -- and they must pick the same renderer for the same machine.
///
/// They did not. The wrapper had TWO top-level arms where the other two have
/// three: it was missing "the `nvidia.nouveau_uapi` flag but NO NVIDIA card",
/// which is the `GL=1` image under QEMU. There the wrapper pinned
/// `WLR_RENDERER=pixman` while the other two pin `gles2`, and
/// `build_child_env`'s own comment records what that mix does: it renders but
/// never composites, so glxgears prints its FPS to the console with no window
/// ever appearing. A labwc started by hand from a non-login shell (no
/// `/etc/profile`, no init) took exactly that arm.
#[test]
fn the_wrapper_and_the_profile_pick_the_same_renderer_for_the_same_machine() {
    let dir = std::env::temp_dir().join(format!("eclipse-renderer-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_wrapper(&dir);
    let etc = dir.join("etc");
    fs::create_dir_all(&etc).unwrap();
    super::super::LinuxRootfs::write_profile(&etc);
    let wrapper_gate = renderer_gate(&fs::read_to_string(dir.join("usr/local/bin/labwc")).unwrap());
    let profile_gate = renderer_gate(&fs::read_to_string(etc.join("profile")).unwrap());

    const NVIDIA: Option<&str> = Some("0x10de\n");
    const VIRTIO: Option<&str> = Some("0x1af4\n");
    // Every machine and boot this image supports, with the renderer each
    // one must land on. The third row is the one that diverged.
    let cases: &[(&str, Option<&str>, &str)] = &[
        // an RTX with nouveau_uapi: GLES2/zink is the default GPU path
        ("nvidia.nouveau_uapi", NVIDIA, "gles2"),
        // kill-switch back to pixman
        ("nvidia.nouveau_uapi:nvidia.wlr_pixman", NVIDIA, "pixman"),
        // an RTX booted without it: the DRM node is not nouveau at all
        ("LOG=warn", NVIDIA, "pixman"),
        // the GL=1 image under QEMU: the flag, but no NVIDIA card
        ("renderer=gl:nvidia.nouveau_uapi", VIRTIO, "gles2"),
        // QEMU with no flag
        ("LOG=warn", VIRTIO, "pixman"),
        // no card0 at all
        ("nvidia.nouveau_uapi", None, "pixman"),
        ("LOG=warn", None, "pixman"),
        // explicit Vulkan; gles2 flag is redundant with the default
        ("nvidia.nouveau_uapi:nvidia.wlr_gles2", NVIDIA, "gles2"),
        ("nvidia.nouveau_uapi:nvidia.wlr_vulkan", NVIDIA, "vulkan"),
    ];
    for (cmdline, vendor, expect) in cases {
        let w = run_gate(&wrapper_gate, cmdline, *vendor);
        let p = run_gate(&profile_gate, cmdline, *vendor);
        let ctx = format!("{cmdline:?} vendor={vendor:?}");
        assert_eq!(
            gate_var(&w, "WLR_RENDERER").as_deref(),
            Some(*expect),
            "wrapper renderer for {ctx}"
        );
        assert_eq!(
            gate_var(&p, "WLR_RENDERER").as_deref(),
            Some(*expect),
            "/etc/profile renderer for {ctx}"
        );
        // The whole point: not merely that each is right on its own, but
        // that the two agree on every variable that decides how a frame is
        // produced. A compositor on one renderer with clients pinned to
        // another composites nothing.
        for key in [
            "WLR_RENDERER",
            "LIBGL_ALWAYS_SOFTWARE",
            "GALLIUM_DRIVER",
            "MESA_LOADER_DRIVER_OVERRIDE",
            "SDL_RENDER_DRIVER",
            "SDL_FRAMEBUFFER_ACCELERATION",
        ] {
            assert_eq!(
                gate_var(&w, key),
                gate_var(&p, key),
                "{key} differs between the wrapper and /etc/profile for {ctx}"
            );
        }
        // A software renderer must never be left with clients probing for a
        // hardware GL driver that is not there.
        if *expect == "pixman" || *vendor != NVIDIA {
            assert_eq!(
                gate_var(&w, "LIBGL_ALWAYS_SOFTWARE").as_deref(),
                Some("1"),
                "software GL not pinned for {ctx}"
            );
            assert_eq!(
                gate_var(&w, "GALLIUM_DRIVER"),
                None,
                "zink pinned for {ctx}"
            );
        } else {
            // and a GPU session must pin its clients to the same stack
            assert_eq!(
                gate_var(&w, "GALLIUM_DRIVER").as_deref(),
                Some("zink"),
                "{ctx}"
            );
            assert_eq!(gate_var(&w, "LIBGL_ALWAYS_SOFTWARE"), None, "{ctx}");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// GL_RENDERER 'zink Vulkan 1.4(llvmpipe ...)' on the RTX: with NVK
/// unusable, zink took lavapipe and Firefox's "GPU" path ran GL on CPU
/// Vulkan. The GPU path must be pinned to NVK and gated on NVK enumerating.
#[test]
fn firefox_gpu_path_never_lands_zink_on_lavapipe() {
    let dir = std::env::temp_dir().join(format!("eclipse-ff-nvk-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_firefox_wrapper(&dir);
    let ff = fs::read_to_string(dir.join("usr/local/bin/eclipse-firefox")).unwrap();
    let gpu = ff.find("export GALLIUM_DRIVER=").unwrap();
    let gate = &ff[..gpu];
    assert!(gate.contains("/usr/share/vulkan/icd.d/nouveau_icd.*.json"));
    assert!(gate.contains("vulkaninfo --summary"));
    assert!(gate.contains("[ -n \"$NVK_ICD\" ]"));
    assert!(gate.contains("export VK_DRIVER_FILES=\"$NVK_ICD\" VK_ICD_FILENAMES=\"$NVK_ICD\""));
    assert!(!ff.contains("lvp_icd"));
    if let Ok(st) = std::process::Command::new("sh")
        .arg("-n")
        .arg(dir.join("usr/local/bin/eclipse-firefox"))
        .status()
    {
        assert!(st.success(), "eclipse-firefox does not parse as sh");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Runs the generated wrapper against a fake `labwc` that fails the way
/// a real one does when zink cannot find the GPU, with every absolute path
/// it touches redirected into `dir`. Returns the fake's call log.
fn run_wrapper_with_fake_labwc(dir: &Path, renderer: &str, fail_on: &str) -> Option<String> {
    let _ = fs::remove_dir_all(dir);
    write_labwc_wrapper(dir);
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let d = dir.display();
    let fake = format!(
        "#!/bin/sh\n\
         echo \"$WLR_RENDERER ${{GALLIUM_DRIVER:-none}}\" >> {d}/calls\n\
         echo x >> {d}/starts\n\
         if [ \"$WLR_RENDERER\" = {fail_on} ] || \\\n\
         \x20 {{ [ {fail_on} = first ] && [ \"$(wc -l < {d}/starts)\" -eq 1 ]; }}; then\n\
         \x20 echo '[../src/server.c:532] unable to create renderer'\n\
         \x20 exit 1\n\
         fi\n\
         exit 0\n"
    );
    fs::write(bin.join("labwc"), fake).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("labwc"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let script = fs::read_to_string(dir.join("usr/local/bin/labwc"))
        .unwrap()
        .replace("/usr/bin /bin /usr/sbin /sbin", &format!("{d}/bin"))
        .replace("/sbin/eclipse-init", &format!("{d}/no-init"))
        .replace("/tmp/labwc", &format!("{d}/labwc"))
        .replace("/run/labwc-renderer-fallback", &format!("{d}/fallback"))
        .replace("/dev/console", "/dev/null");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env("WLR_RENDERER", renderer)
        .env("GALLIUM_DRIVER", "zink")
        .env_remove("LIBGL_ALWAYS_SOFTWARE")
        .env_remove("ECLIPSE_LABWC_NO_FALLBACK")
        .env("ECLIPSE_LABWC_RETRY_DELAY", "0")
        .env("SEATD_SOCK", "/nonexistent")
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(fs::read_to_string(dir.join("calls")).unwrap())
}

/// The real-hardware failure: `nvidia.nouveau_uapi` on the RTX pins
/// GLES2/zink, NVK does not enumerate the GPU ('ZINK: failed to choose
/// pdev'), and labwc exits with 'unable to create renderer'. On hardware
/// only the first start of a boot does that, so the wrapper retries the
/// GPU once, and goes to pixman only when the retry fails too.
#[test]
fn a_gpu_renderer_that_fails_to_init_is_retried_then_falls_back_to_pixman() {
    let dir = std::env::temp_dir().join(format!("eclipse-labwc-fb-{}", std::process::id()));
    // Fails once, then works: the second GPU attempt is the session.
    let Some(calls) = run_wrapper_with_fake_labwc(&dir, "gles2", "first") else {
        return; // no host sh
    };
    assert_eq!(calls, "gles2 zink\ngles2 zink\n");
    assert!(!dir.join("fallback").exists());
    assert!(fs::read_to_string(dir.join("labwc-gles2-1.log"))
        .unwrap()
        .contains("unable to create renderer"));

    // Fails every time: two GPU attempts, then pixman with software GL.
    let calls = run_wrapper_with_fake_labwc(&dir, "gles2", "gles2").unwrap();
    assert_eq!(calls, "gles2 zink\ngles2 zink\npixman none\n");
    assert_eq!(fs::read_to_string(dir.join("fallback")).unwrap(), "gles2\n");
    assert!(dir.join("labwc-gles2-2.log").exists());

    // A GPU renderer that works is run once, and leaves no marker.
    let calls = run_wrapper_with_fake_labwc(&dir, "gles2", "never").unwrap();
    assert_eq!(calls, "gles2 zink\n");
    assert!(!dir.join("fallback").exists());

    // pixman is exec'd directly; nothing to retry.
    let calls = run_wrapper_with_fake_labwc(&dir, "pixman", "never").unwrap();
    assert_eq!(calls, "pixman zink\n");
    let _ = fs::remove_dir_all(&dir);
}

/// A caller's own choice survives the gate: every assignment is `:=`, so
/// what `eclipse-init` already exported (or a person debugging by hand) wins
/// instead of being overwritten by the default for the machine.
#[test]
fn a_renderer_the_caller_already_chose_is_not_overwritten() {
    let dir = std::env::temp_dir().join(format!("eclipse-renderer-ov-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_wrapper(&dir);
    let gate = renderer_gate(&fs::read_to_string(dir.join("usr/local/bin/labwc")).unwrap());
    // Prepend an inherited pin, as an init-launched session carries.
    let with_pin = format!("WLR_RENDERER=gles2\nexport WLR_RENDERER\n{gate}");
    let env = run_gate(
        &with_pin,
        "renderer=gl:nvidia.nouveau_uapi",
        Some("0x1af4\n"),
    );
    assert_eq!(gate_var(&env, "WLR_RENDERER").as_deref(), Some("gles2"));
    let with_pin = format!("WLR_RENDERER=pixman\nexport WLR_RENDERER\n{gate}");
    let env = run_gate(
        &with_pin,
        "nvidia.nouveau_uapi:nvidia.wlr_vulkan",
        Some("0x10de\n"),
    );
    assert_eq!(gate_var(&env, "WLR_RENDERER").as_deref(), Some("pixman"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sdl_policy_is_consistent_across_wrapper_profile_and_environment() {
    let dir = std::env::temp_dir().join(format!("eclipse-sdl-policy-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_labwc_wrapper(&dir);
    write_labwc_environment(&dir);
    let etc = dir.join("etc");
    let _ = fs::create_dir_all(&etc);
    super::super::LinuxRootfs::write_profile(&etc);

    let wrapper = fs::read_to_string(dir.join("usr/local/bin/labwc")).unwrap();
    let profile = fs::read_to_string(etc.join("profile")).unwrap();
    let environment = fs::read_to_string(dir.join("root/.config/labwc/environment")).unwrap();

    // Both shell files must at least PARSE: a syntax slip in the wrapper is
    // "no desktop at boot" with nothing in the log. Skipped where the host
    // has no `sh` (a non-unix build box).
    for (rel, what) in [
        ("usr/local/bin/labwc", "wrapper"),
        ("etc/profile", "/etc/profile"),
    ] {
        if let Ok(status) = std::process::Command::new("sh")
            .arg("-n")
            .arg(dir.join(rel))
            .status()
        {
            assert!(status.success(), "{what} does not parse as sh");
        }
    }

    // Backend half: SDL2 + SDL3 spellings, Wayland-first with X11 fallback,
    // ALSA audio (routed through Pulse) -- plus OpenAL on Pulse then ALSA,
    // PULSE_SERVER for libpulse, and a D-Bus session address so SDL_Init
    // never autolaunches dbus-launch. Static, so all three files.
    for (key, val) in [
        ("SDL_VIDEODRIVER", "wayland,x11"),
        ("SDL_VIDEO_DRIVER", "wayland,x11"),
        ("SDL_AUDIODRIVER", "alsa"),
        ("SDL_AUDIO_DRIVER", "alsa"),
        ("ALSOFT_DRIVERS", "pulse,alsa"),
        ("PULSE_SERVER", "unix:/run/pulse/native"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/0/bus"),
    ] {
        assert!(
            wrapper.contains(&format!(": \"${{{key}:={val}}}\"; export {key}")),
            "wrapper must default {key}={val}"
        );
        assert!(
            profile.contains(&format!("export {key}={val}")),
            "/etc/profile must export {key}={val}"
        );
        assert!(
            environment.lines().any(|l| l == format!("{key}={val}")),
            "labwc environment must set {key}={val}"
        );
    }

    // Renderer half: every WLR_RENDERER branch pins a matching SDL render
    // driver. pixman -> software (+ no GL behind window surfaces); gles2
    // and vulkan -> opengles2 (SDL2 has no Vulkan renderer).
    fn branch_pins(text: &str, wlr_line: &str, expect_driver: &str, expect_fb: &str, what: &str) {
        let mut found = 0;
        for (i, line) in text.lines().enumerate() {
            if !line.contains(wlr_line) {
                continue;
            }
            found += 1;
            // The SDL pins follow within the same branch: before the next
            // `fi`/`else`/`elif` line.
            let branch: Vec<&str> = text
                .lines()
                .skip(i + 1)
                .take_while(|l| {
                    let t = l.trim_start();
                    !(t.starts_with("fi") || t.starts_with("else") || t.starts_with("elif"))
                })
                .collect();
            let joined = branch.join("\n");
            assert!(
                joined.contains(&format!("SDL_RENDER_DRIVER:={expect_driver}"))
                    || joined.contains(&format!("SDL_RENDER_DRIVER={expect_driver}")),
                "{what}: branch `{wlr_line}` must pin SDL_RENDER_DRIVER={expect_driver}:\n{joined}"
            );
            assert!(
                joined.contains(&format!("SDL_FRAMEBUFFER_ACCELERATION:={expect_fb}"))
                    || joined.contains(&format!("SDL_FRAMEBUFFER_ACCELERATION={expect_fb}")),
                "{what}: branch `{wlr_line}` must pin SDL_FRAMEBUFFER_ACCELERATION={expect_fb}:\n{joined}"
            );
        }
        assert!(found > 0, "{what}: no branch sets `{wlr_line}`");
    }
    // The wrapper defaults with `${VAR:=value}`, /etc/profile assigns
    // with `export VAR=value`; match each file in its own spelling.
    for (text, what, pixman, gles2, vulkan) in [
        (
            &wrapper,
            "wrapper",
            "WLR_RENDERER:=pixman",
            "WLR_RENDERER:=gles2",
            "WLR_RENDERER:=vulkan",
        ),
        (
            &profile,
            "/etc/profile",
            "export WLR_RENDERER=pixman",
            "export WLR_RENDERER=gles2",
            "export WLR_RENDERER=vulkan",
        ),
    ] {
        branch_pins(text, pixman, "software", "0", what);
        branch_pins(text, gles2, "opengles2", "opengles2", what);
        branch_pins(text, vulkan, "opengles2", "opengles2", what);
        // Both files have exactly two pixman branches (NVIDIA without a
        // wlr opt-in flag, and everything else); make sure both were seen.
        assert_eq!(text.matches(pixman).count(), 2, "{what}: pixman branches");
    }

    // The menu offers the probe on both draw paths.
    write_labwc_menu(&dir);
    let menu = fs::read_to_string(dir.join("root/.config/labwc/menu.xml")).unwrap();
    assert!(menu.contains("/bin/eclipse-sdl-probe --hold"));
    assert!(menu.contains("/bin/eclipse-sdl-probe --sdl3 --surface --hold"));
    // And on the stepwise path: the one that says WHICH subsystem of
    // SDL_Init hangs, which is what GZDoom needed and no other entry gives.
    assert!(menu.contains("/bin/eclipse-sdl-probe --steps"));
    let _ = fs::remove_dir_all(&dir);
}

/// A scratch rootfs with everything the desktop's look is written into.
/// Separate per tag so the suite can run its tests in parallel.
fn look_rootfs(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "eclipse-look-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    write_eclipse_look(&dir);
    write_theme(&dir);
    write_foot_config(&dir);
    write_labwc_rc(&dir);
    write_labwc_menu(&dir);
    write_kde_helpers(&dir);
    write_gtk_settings(&dir);
    dir
}

/// `(look, labwc theme, the background its terminal palette uses)`. The
/// three names are the ones `eclipse-look` accepts; everything the look
/// touches has to agree on them, which is what the tests below check.
const LOOKS: &[(&str, &str, &str)] = &[
    ("win11", "Win11-Dark", "background=0c0c0c"),
    ("kde", "Breeze-Dark", "background=232629"),
    ("eclipse", "Eclipse-Dark", "background=120f1c"),
];

/// `xml` with its comments taken out. rc.xml's comments name the very
/// tags they explain (`<default/>`, `<mouse>`), so a check run over the
/// raw text passes on a file the tag itself was deleted from.
fn without_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find("-->") {
            Some(j) => &rest[i + j + 3..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn themerc(rootfs: &Path, theme: &str) -> String {
    let p = rootfs
        .join("usr/share/themes")
        .join(theme)
        .join("openbox-3/themerc");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The value of an openbox themerc key, e.g. `window.active.border.color`.
fn themerc_key(rc: &str, key: &str) -> Option<String> {
    rc.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key))
        .map(|(_, v)| v.trim().to_string())
}

/// Every piece that names a look has to name the same three, and each
/// one's theme and terminal palette have to be its own. A look that is
/// accepted but has no theme leaves labwc on its built-in appearance;
/// one whose palette is another look's gives a terminal that does not
/// match the session it was started from.
#[test]
fn every_look_has_a_theme_and_a_palette_of_its_own() {
    let dir = look_rootfs("table");
    let script = fs::read_to_string(dir.join("usr/local/bin/eclipse-look")).expect("eclipse-look");

    // `look_ok` accepts exactly these three and nothing else: a look it
    // accepts without a theme leaves labwc on its built-in appearance.
    let accepted = script
        .split_once("case \"$1\" in ")
        .expect("look_ok is gone from eclipse-look")
        .1
        .split_once(')')
        .unwrap()
        .0;
    assert_eq!(
        accepted.split('|').collect::<Vec<_>>(),
        LOOKS.iter().map(|(l, _, _)| *l).collect::<Vec<_>>(),
        "eclipse-look accepts {accepted:?}"
    );

    for (look, theme, background) in LOOKS {
        // Each look maps to its own theme, which really is installed.
        assert!(
            script.contains(&format!("{look}) echo {theme} ;;"))
                || (*look == "eclipse" && script.contains(&format!("*) echo {theme} ;;"))),
            "{look} does not map to {theme} in eclipse-look:\n{script}"
        );
        let rc = themerc(&dir, theme);
        assert!(
            rc.contains("window.active.title.bg.color:"),
            "{theme}'s themerc has no titlebar colour"
        );
        // ... and to its own terminal palette.
        let ini = fs::read_to_string(
            dir.join("root/.config/foot")
                .join(format!("foot.{look}.ini")),
        )
        .unwrap_or_else(|e| panic!("foot.{look}.ini: {e}"));
        assert!(
            ini.contains(background),
            "foot.{look}.ini is not {look}'s palette ({background}):\n{ini}"
        );
    }

    // And no two looks share a theme or a palette.
    for (i, a) in LOOKS.iter().enumerate() {
        for b in &LOOKS[i + 1..] {
            assert_ne!(a.1, b.1, "two looks share a theme");
            assert_ne!(a.2, b.2, "two looks share a palette");
            assert_ne!(
                themerc(&dir, a.1),
                themerc(&dir, b.1),
                "{} and {} ship the same themerc",
                a.1,
                b.1
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The look a fresh image comes up in is written in four places, and a
/// disagreement between them shows as a session that half-changes: the
/// panel in one look, the window frames in another.
#[test]
fn the_default_look_is_the_same_everywhere_it_is_written() {
    let dir = look_rootfs("default");
    const DEFAULT: &str = "eclipse";
    let (_, theme, _) = LOOKS
        .iter()
        .find(|(l, _, _)| *l == DEFAULT)
        .expect("the default look is not one of the looks");

    assert_eq!(
        fs::read_to_string(dir.join("etc/eclipse/look")).unwrap(),
        format!("look={DEFAULT}\n")
    );
    let rc = fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap();
    assert!(
        rc.contains(&format!("<name>{theme}</name>")),
        "rc.xml does not start on {theme}, so `eclipse-look` has nothing to rewrite"
    );
    let foot = dir.join("root/.config/foot");
    assert_eq!(
        fs::read_to_string(foot.join("foot.ini")).unwrap(),
        fs::read_to_string(foot.join(format!("foot.{DEFAULT}.ini"))).unwrap(),
        "the terminal comes up in a palette the session is not in"
    );
    // The menu's default language is Spanish, as `eclipse-locale` says.
    let menu = dir.join("root/.config/labwc");
    assert_eq!(
        fs::read(menu.join("menu.xml")).unwrap(),
        fs::read(menu.join("menu.es.xml")).unwrap()
    );
    assert_ne!(
        fs::read(menu.join("menu.es.xml")).unwrap(),
        fs::read(menu.join("menu.en.xml")).unwrap(),
        "the two languages ship the same menu"
    );
    // The two differ in their LABELS and in nothing else: a command or an
    // action that drifted in one language is an entry that does something
    // else, or nothing, depending on the language the session came up in.
    let es = fs::read_to_string(menu.join("menu.es.xml")).unwrap();
    let en = fs::read_to_string(menu.join("menu.en.xml")).unwrap();
    let shape = |xml: &str| -> Vec<String> {
        xml.split("<action name=\"")
            .skip(1)
            .map(|a| {
                let name = a.split_once('"').unwrap().0;
                let cmd = a
                    .split_once("<command>")
                    .and_then(|(_, c)| c.split_once("</command>"))
                    .map(|(c, _)| c)
                    .unwrap_or("");
                format!("{name}:{cmd}")
            })
            .collect()
    };
    assert_eq!(
        shape(&es),
        shape(&en),
        "the menu entries do different things in Spanish and in English"
    );
    assert_eq!(
        es.matches("<separator/>").count(),
        en.matches("<separator/>").count(),
        "the two menus are grouped differently"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// With no window shadows on this stack a grey-on-grey border left the
/// focused window unidentifiable (why Breeze's active border carries the
/// accent instead of Breeze's own near-invisible grey). Every theme has
/// to tell the two states apart on more than one cue.
#[test]
fn a_focused_window_is_told_apart_from_an_unfocused_one_in_every_theme() {
    let dir = look_rootfs("focus");
    for (look, theme, _) in LOOKS {
        let rc = themerc(&dir, theme);
        for key in [
            "window.active.border.color",
            "window.active.title.bg.color",
            "window.active.label.text.color",
        ] {
            let inactive = key.replace("active", "inactive");
            let a = themerc_key(&rc, key).unwrap_or_else(|| panic!("{theme} has no {key}"));
            let b =
                themerc_key(&rc, &inactive).unwrap_or_else(|| panic!("{theme} has no {inactive}"));
            assert_ne!(a, b, "{look}: {key} and {inactive} are the same colour");
        }
        // And the focused border is not the unfocused titlebar either,
        // which is the grey-on-grey case the comment above describes.
        assert_ne!(
            themerc_key(&rc, "window.active.border.color"),
            themerc_key(&rc, "window.inactive.title.bg.color"),
            "{look}: the focused border melts into an unfocused titlebar"
        );
        // Selection in the menu has to be visible over the menu itself.
        assert_ne!(
            themerc_key(&rc, "menu.items.active.bg.color"),
            themerc_key(&rc, "menu.items.bg.color"),
            "{look}: the selected menu entry is the colour of the menu"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Each look imitates a desktop that aligns window titles its own way:
/// Windows to the left, Breeze centred. Eclipse's own leaves the key out
/// and takes openbox's default.
#[test]
fn each_theme_keeps_the_title_alignment_of_the_desktop_it_imitates() {
    let dir = look_rootfs("justify");
    let want = [
        ("Win11-Dark", Some("Left")),
        ("Breeze-Dark", Some("Center")),
        ("Eclipse-Dark", None),
    ];
    for (theme, justify) in want {
        let rc = themerc(&dir, theme);
        assert_eq!(
            themerc_key(&rc, "window.label.text.justify").as_deref(),
            justify,
            "{theme} aligns its window titles the wrong way"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// `eclipse-look` is the only thing that changes the look of a running
/// session, and each of these lines is a way for it to half-work.
#[test]
fn the_look_script_rewrites_every_place_the_old_look_is_named() {
    let dir = look_rootfs("script");
    let script = fs::read_to_string(dir.join("usr/local/bin/eclipse-look")).unwrap();

    // The chosen look REPLACES the file: appending leaves the old line
    // first, and `file_look` prints the first match and exits.
    assert!(
        script.contains("echo \"look=$look\" > \"$CONF\""),
        "the look file is not rewritten in place:\n{script}"
    );
    // Every theme name rc.xml could be holding is rewritten, or changing
    // away from a look and back again would stick on the middle one.
    for (_, theme, _) in LOOKS {
        assert!(
            script.contains(&format!("s|<name>{theme}</name>|<name>$theme</name>|")),
            "rc.xml keeps {theme} when the look changes:\n{script}"
        );
    }
    // The palette is copied FROM the look's file over foot.ini.
    assert!(
        script.contains("src=\"$CFG/foot/foot.$look.ini\"")
            && script.contains("cp -f \"$src\" \"$CFG/foot/foot.ini\""),
        "the terminal palette is not copied from the look's own file:\n{script}"
    );
    // At boot nothing is restarted (the session is not up yet); on a
    // later change labwc is reconfigured and the panel respawned.
    assert!(
        script.contains("if [ \"$boot\" != boot ]; then"),
        "the boot path is not told apart from a live change:\n{script}"
    );
    for live in ["labwc --reconfigure", "pkill -x lunarbar"] {
        assert!(
            script.contains(live),
            "a live look change does not {live}:\n{script}"
        );
    }
    // The command line wins over the file, so `look=` on the kernel line
    // can bring a machine up in another look without editing the image.
    let boot = script
        .split_once("resolve_boot() {")
        .expect("resolve_boot is gone")
        .1;
    let boot = boot.split_once("\n}").unwrap().0;
    assert!(
        boot.contains("cmdline_look") && boot.contains("look_ok"),
        "resolve_boot ignores the command line:\n{boot}"
    );
    // And an unreadable or unknown look falls back to the default.
    let current = script
        .split_once("current() {")
        .expect("current is gone")
        .1
        .split_once("\n}")
        .unwrap()
        .0;
    assert!(
        current.contains("echo eclipse"),
        "an unknown look does not fall back to the default:\n{current}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `<desktops number>` and the keys that move between them have to agree:
/// a key for a desktop that does not exist does nothing, and a desktop
/// with no key cannot be reached without a mouse.
#[test]
fn every_desktop_can_be_reached_and_sent_to_from_the_keyboard() {
    let dir = look_rootfs("desktops");
    let rc = fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap();
    let count: usize = rc
        .split_once("<desktops number=\"")
        .expect("no <desktops>")
        .1
        .split_once('"')
        .unwrap()
        .0
        .parse()
        .expect("the desktop count is not a number");
    assert!((2..=9).contains(&count), "{count} desktops");

    for n in 1..=count {
        for action in ["GoToDesktop", "SendToDesktop"] {
            assert!(
                rc.contains(&format!("<action name=\"{action}\"><to>{n}</to></action>")),
                "no key {action} for desktop {n} of {count}"
            );
        }
    }
    for action in ["GoToDesktop", "SendToDesktop"] {
        assert!(
            !rc.contains(&format!("<action name=\"{action}\"><to>{}</to>", count + 1)),
            "a key {action} for desktop {} of {count}",
            count + 1
        );
        // And every desktop is named by the SAME number of keys. Without
        // this a key pointing at the wrong desktop still satisfies the
        // loop above, through another key that happens to agree with it,
        // and its own desktop is left reachable only by mouse.
        let per: Vec<usize> = (1..=count)
            .map(|n| {
                rc.matches(&format!("<action name=\"{action}\"><to>{n}</to></action>"))
                    .count()
            })
            .collect();
        assert!(
            per.iter().all(|c| *c == per[0]),
            "the {action} keys are shared out {per:?} among {count} desktops"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Two settings in rc.xml that cost a whole session when they go: without
/// `<default/>` a custom `<mouse>` section REPLACES labwc's built-in
/// bindings, so windows cannot be moved and the titlebar buttons do
/// nothing; without `xwaylandPersistence` the lazy Xwayland spawn never
/// finishes and the first X11 client hangs for ever.
#[test]
fn the_compositor_keeps_its_builtin_mouse_bindings_and_its_xwayland() {
    let dir = look_rootfs("rcxml");
    let rc = without_comments(&fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap());
    let mouse = rc
        .split_once("<mouse>")
        .expect("no <mouse> section")
        .1
        .split_once("</mouse>")
        .unwrap()
        .0;
    assert!(
        mouse.contains("<default />") || mouse.contains("<default/>"),
        "<mouse> replaces labwc's own bindings instead of adding to them:\n{mouse}"
    );
    assert!(
        rc.contains("<xwaylandPersistence>yes</xwaylandPersistence>"),
        "Xwayland is back to the lazy spawn that hangs the first X11 client"
    );
    let radius: u32 = rc
        .split_once("<cornerRadius>")
        .expect("no <cornerRadius>")
        .1
        .split_once('<')
        .unwrap()
        .0
        .parse()
        .unwrap();
    assert!(radius > 0, "the rounded corners are gone");
    let _ = fs::remove_dir_all(&dir);
}

/// The keys a KDE user presses without thinking, and the launcher behind
/// each. None of these has a KDE binary behind it here, so a wrong
/// command is a key that silently does nothing.
#[test]
fn the_keys_a_kde_user_presses_reach_the_launcher_each_one_is_for() {
    let dir = look_rootfs("keys");
    let rc = fs::read_to_string(dir.join("root/.config/labwc/rc.xml")).unwrap();
    let want: &[(&str, &[&str])] = &[
        ("eclipse-run", &["A-space", "A-F2", "C-A-Delete"]),
        ("eclipse-files", &["W-E"]),
        ("eclipse-clipboard", &["W-V"]),
        ("eclipse-showdesktop", &["W-D"]),
        ("eclipse-terminal", &["W-Return", "A-Return", "C-A-T"]),
    ];
    for (prog, keys) in want {
        for key in *keys {
            assert!(
                rc.contains(&format!(
                    "<keybind key=\"{key}\"><action name=\"Execute\"><command>/usr/local/bin/{prog}"
                )),
                "{key} does not run {prog}"
            );
        }
        // And the launcher the key names is really shipped, executable.
        let p = dir.join("usr/local/bin").join(prog);
        if *prog != "eclipse-terminal" {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&p)
                .unwrap_or_else(|e| panic!("{prog}: {e}"))
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "{prog} is not executable ({mode:o})");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// What the three KDE stand-ins have to do, none of which has a KDE
/// binary behind it: the launcher toggles instead of stacking overlays,
/// Super+D minimises through wlr-foreign-toplevel-management, and the
/// file manager key opens whichever manager IS installed.
#[test]
fn the_kde_standins_toggle_and_fall_back_to_what_is_installed() {
    let dir = look_rootfs("standins");
    let bin = dir.join("usr/local/bin");

    let run = fs::read_to_string(bin.join("eclipse-run")).unwrap();
    assert!(
        run.contains("pkill -x lunarrun") && run.contains("exit 0"),
        "a second press stacks another launcher instead of closing it:\n{run}"
    );
    let clip = fs::read_to_string(bin.join("eclipse-clipboard")).unwrap();
    assert!(
        clip.contains("pkill -f 'lunarclip --pick'") && clip.contains("exit 0"),
        "a second Super+V stacks another picker instead of closing it:\n{clip}"
    );
    // The one way this wrapper can do real damage: `lunarclip --watch` is
    // the same binary, so a match by name alone kills the daemon -- and
    // with it the selection, which on Wayland lives in the process that
    // offers it. One keypress would empty the clipboard.
    assert!(
        !clip.contains("pkill -x lunarclip"),
        "Super+V kills the clipboard daemon it depends on:\n{clip}"
    );
    assert!(
        clip.contains("exec lunarclip --pick"),
        "Super+V does not open the history:\n{clip}"
    );

    let show = fs::read_to_string(bin.join("eclipse-showdesktop")).unwrap();
    assert!(
        show.contains("lunarrun --toggle-desktop"),
        "Super+D does not toggle the desktop, it opens the launcher:\n{show}"
    );
    let files = fs::read_to_string(bin.join("eclipse-files")).unwrap();
    // lunarfile FIRST, and every argument passed through: that is what
    // makes Super+E and the `--pick` chooser the same wrapper.
    assert!(
        files.contains("exec lunarfile \"$@\""),
        "Super+E no longer opens Eclipse's own browser:\n{files}"
    );
    assert!(
        files.find("lunarfile").unwrap() < files.find("for fm in ").unwrap(),
        "the console fallback is tried before lunarfile:\n{files}"
    );
    // A chooser has no fallback: opening a shell would leave the caller
    // waiting for a path that never comes.
    assert!(
        files.contains("--pick") && files.contains("exit 1"),
        "{files}"
    );
    // The editor the opener table names must always find an editor:
    // busybox always has `vi`, which is why it is the last one tried.
    let edit = fs::read_to_string(bin.join("eclipse-edit")).unwrap();
    let eds = edit
        .split_once("for ed in ")
        .unwrap()
        .1
        .split_once(';')
        .unwrap()
        .0;
    assert_eq!(eds.split_whitespace().last(), Some("vi"), "{edit}");
    // The viewer the opener table names: swayimg on the Wayland session,
    // and the browser anywhere else. The display check is the point --
    // swayimg has no X11 backend, so on the X session `command -v` alone
    // would hand it a file it cannot show.
    let view = fs::read_to_string(bin.join("eclipse-view")).unwrap();
    let vs = view
        .split_once("for v in ")
        .expect("the viewer list is gone")
        .1
        .split_once(';')
        .unwrap()
        .0;
    assert_eq!(
        vs.split_whitespace().next(),
        Some("swayimg"),
        "the viewer in the image is no longer tried first: {vs:?}"
    );
    assert!(
        view.find("WAYLAND_DISPLAY").unwrap() < view.find("for v in ").unwrap(),
        "the viewers are tried without checking for a Wayland display:\n{view}"
    );
    assert!(
        view.trim_end().ends_with("eclipse-firefox \"$@\""),
        "the last resort must be the browser, the one viewer X also has:\n{view}"
    );
    // Through the wrapper, never the browser itself.
    assert!(
        !view.contains(" firefox ") && !view.contains("exec firefox"),
        "{view}"
    );
    // And it must parse as sh, like every other wrapper here.
    if let Ok(status) = std::process::Command::new("sh")
        .arg("-n")
        .arg(bin.join("eclipse-view"))
        .status()
    {
        assert!(status.success(), "eclipse-view does not parse as sh");
    }

    let list = files
        .split_once("for fm in ")
        .expect("the file manager list is gone")
        .1
        .split_once(';')
        .unwrap()
        .0;
    assert!(
        list.split_whitespace().count() >= 3,
        "the file manager list is down to {list:?}"
    );
    assert!(
        list.split_whitespace().next() == Some("mc"),
        "the manager actually in the image is no longer tried first: {list:?}"
    );
    // And with none of them installed the key still opens something.
    let last = files.lines().last().unwrap();
    assert!(
        last.contains("eclipse-terminal") && last.contains("exec sh"),
        "with no file manager installed the key does nothing: {last:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Both GTK major versions read their own settings file, so a toolkit
/// left out comes up in the light theme next to a dark session.
#[test]
fn both_gtk_versions_are_asked_for_the_dark_theme() {
    let dir = look_rootfs("gtk");
    for ver in ["gtk-3.0", "gtk-4.0"] {
        let ini = fs::read_to_string(dir.join("root/.config").join(ver).join("settings.ini"))
            .unwrap_or_else(|e| panic!("{ver}: {e}"));
        assert!(
            ini.contains("gtk-application-prefer-dark-theme=1"),
            "{ver} does not prefer the dark theme:\n{ini}"
        );
        assert!(
            ini.contains("gtk-theme-name=Adwaita-dark"),
            "{ver} is not on a dark theme:\n{ini}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// foot defaults to one render thread per CPU, which SEGFAULTs it on
/// Eclipse's young SMP, and the bare `monospace` alias can resolve to a
/// non-mono font on a minimal fontconfig. Both are in every palette's
/// file, so every one of them has to carry the cure.
#[test]
fn the_terminal_starts_on_one_worker_and_a_real_mono_font() {
    let dir = look_rootfs("foot");
    let foot = dir.join("root/.config/foot");
    let mut seen = 0;
    for entry in fs::read_dir(&foot).unwrap().flatten() {
        let ini = fs::read_to_string(entry.path()).unwrap();
        let name = entry.file_name();
        assert!(
            ini.contains("\nworkers=1\n"),
            "{name:?} lets foot start one render thread per CPU:\n{ini}"
        );
        assert!(
            ini.contains("font=DejaVu Sans Mono:"),
            "{name:?} names no real monospace family before the alias:\n{ini}"
        );
        assert!(
            ini.contains("\npad=6x6\n"),
            "{name:?} lost its padding:\n{ini}"
        );
        // The palette is under the section foot reads for a dark theme.
        let colors = ini
            .split_once("[colors-dark]\n")
            .unwrap_or_else(|| panic!("{name:?} has no [colors-dark] section:\n{ini}"))
            .1;
        assert!(
            colors.contains("background=") && colors.contains("foreground="),
            "{name:?}'s palette is empty"
        );
        seen += 1;
    }
    assert_eq!(seen, LOOKS.len() + 1, "a palette file is missing");
    let _ = fs::remove_dir_all(&dir);
}

/// One em-dash written as a double hyphen inside a comment made libxml2
/// discard the WHOLE rc.xml on real hardware, so labwc ran on built-in
/// defaults with no theme and no keybind. The guard that catches it is
/// itself worth a test: it runs at build time and only ever has to fire
/// once.
#[test]
fn a_double_hyphen_in_a_comment_fails_the_build_not_the_desktop() {
    // What ships has to pass, comments and all.
    let dir = look_rootfs("xml");
    let rc = fs::read(dir.join("root/.config/labwc/rc.xml")).unwrap();
    assert_xml_comments_parse(&rc, "the rc.xml that ships");
    assert!(
        rc.windows(4).any(|w| w == b"<!--"),
        "rc.xml has no comments left, so the guard proves nothing"
    );
    for menu in ["menu.es.xml", "menu.en.xml"] {
        assert_xml_comments_parse(
            &fs::read(dir.join("root/.config/labwc").join(menu)).unwrap(),
            menu,
        );
    }
    let _ = fs::remove_dir_all(&dir);

    // And what would be rejected really is.
    for bad in [
        &b"<!-- an em -- dash -->"[..],
        &b"<ok/><!-- fine --><!-- and then -- this -->"[..],
        &b"<!-- never closed"[..],
        &b"<!-- trailing --"[..],
    ] {
        let text = String::from_utf8_lossy(bad).into_owned();
        let caught = std::panic::catch_unwind(move || {
            assert_xml_comments_parse(text.as_bytes(), "a comment libxml2 rejects")
        });
        assert!(
            caught.is_err(),
            "{:?} would have shipped",
            String::from_utf8_lossy(bad)
        );
    }
    // A `--` outside a comment is XML's business, not this guard's.
    assert_xml_comments_parse(
        b"<item label=\"a -- b\"/>",
        "a double hyphen in an attribute",
    );
}
