use super::*;

// -- The boot timeline -------------------------------------------------

fn step(what: &str, at_ms: u64, took_ms: u64) -> BootStep {
    BootStep {
        what: String::from(what),
        at: Duration::from_millis(at_ms),
        took: Duration::from_millis(took_ms),
    }
}

#[test]
fn the_timeline_lists_the_worst_stretch_first() {
    // The table answers one question -- what is there to cut -- and the
    // stamped console lines above it already give the order, so sorting it
    // chronologically would bury the answer in the middle.
    let steps = vec![
        step("labwc: wait for /dev/input to settle", 3_000, 1_100),
        step("gtk-caches: run the oneshot", 1_000, 2_000),
        step("labwc: wait for the socket /run/seatd.sock", 2_900, 90),
    ];
    let text = render_boot_timeline(&steps, Duration::from_millis(5_000));
    let order: Vec<&str> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().last())
        .collect();
    assert_eq!(order[0], "oneshot");
    assert!(order[1].ends_with("settle"), "{text}");
}

#[test]
fn a_stretch_under_the_floor_lands_in_the_rest_row_and_not_in_the_table() {
    // A boot has a long tail of sub-millisecond steps; a table that lists
    // them is one nobody reads to the end. They must still be ACCOUNTED
    // for, or the rest row stops meaning "time nothing explains".
    let steps = vec![step("a blink", 10, 1), step("a real wait", 100, 900)];
    let text = render_boot_timeline(&steps, Duration::from_millis(1_000));
    assert!(!text.contains("a blink"), "{text}");
    assert!(text.contains("a real wait"), "{text}");
    // 1000 - 901 = 99 ms unexplained.
    assert!(text.contains("0.099s"), "{text}");
}

#[test]
fn the_rest_row_is_what_the_named_stretches_do_not_account_for() {
    // The number that says whether this instrumentation is still missing
    // something: if `rest` is most of the boot, the waits are not where the
    // time is going and the next measurement has to go somewhere else.
    let steps = vec![step("one", 0, 400), step("two", 400, 400)];
    let text = render_boot_timeline(&steps, Duration::from_millis(1_000));
    assert!(text.contains("0.200s"), "{text}");
    assert!(text.contains("20%"), "{text}");
}

#[test]
fn a_timeline_with_no_stretch_still_says_how_long_the_boot_took() {
    // The console-session boot starts almost nothing: no gate, no oneshot,
    // nothing over the floor. The header is then the whole answer, and a
    // header that says "0.000s" because it was computed from the rows
    // would be a lie about a boot that really did take time.
    let text = render_boot_timeline(&[], Duration::from_millis(1_234));
    assert!(text.contains("1.234s to the supervision loop"), "{text}");
    assert!(text.contains("0.000s of it waiting"), "{text}");
}

#[test]
fn the_rest_row_cannot_wrap_when_the_stretches_outrun_the_total() {
    // `total` is read after the steps, so it cannot be the smaller today.
    // Without the saturating subtraction a future caller that read it
    // first would print a row claiming the boot's remainder took 584
    // million years, which is the kind of number that gets a measurement
    // dismissed as broken.
    let steps = vec![step("one", 0, 2_000)];
    let text = render_boot_timeline(&steps, Duration::from_millis(1_000));
    assert!(text.contains("0.000s    0%"), "{text}");
    // And the header reports the waiting capped at the boot, not 2.000s.
    assert!(text.contains("1.000s of it waiting"), "{text}");
}

#[test]
fn a_percentage_of_a_boot_that_took_no_time_is_zero_and_not_a_division_by_zero() {
    assert_eq!(percent_of(Duration::from_millis(5), Duration::ZERO), 0);
    assert_eq!(
        percent_of(Duration::from_millis(500), Duration::from_millis(1_000)),
        50
    );
    // Capped: a stretch longer than the total reads 100%, never 200%.
    assert_eq!(
        percent_of(Duration::from_millis(3_000), Duration::from_millis(1_000)),
        100
    );
}

#[test]
fn nothing_is_recorded_once_the_timeline_has_been_rendered() {
    // `start_service` serves both the boot and every crash-restart after
    // it, so a respawn that dies an hour later walks the same gates. Those
    // waits are not boot: recording them would grow the vector for the
    // life of the machine and rewrite the history of a boot that is over.
    let before = BOOT_RECORDED.swap(true, Ordering::SeqCst);
    let len = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner()).len();
    note_step("a restart an hour after the boot", Duration::from_secs(5));
    assert_eq!(
        BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner()).len(),
        len
    );
    BOOT_RECORDED.store(before, Ordering::SeqCst);
}

#[test]
fn a_stretch_is_recorded_at_when_it_started_not_when_it_ended() {
    // The `at` column lines a row up against the stamped console lines
    // above it; a wait charged to the moment it FINISHED points the reader
    // at whatever ran next instead of at the thing that waited.
    let before = BOOT_RECORDED.swap(false, Ordering::SeqCst);
    let _ = BOOT_T0.set(Instant::now());
    BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    std::thread::sleep(Duration::from_millis(60));
    note_step(
        "a wait that has already happened",
        Duration::from_millis(50),
    );
    let steps = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    BOOT_RECORDED.store(before, Ordering::SeqCst);
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!(steps[0].at < Duration::from_millis(50), "{steps:?}");
}

// -- The three boot settings -------------------------------------------

#[test]
fn the_three_boot_settings_are_independent_of_each_other() {
    // What makes running them at once sound, pinned where a fourth would be
    // added: each reads its own `/etc/eclipse` file and writes its own, and
    // no two of them name the same program. The one file all three DO write
    // -- labwc's `environment` -- is serialised by the lock inside the
    // scripts, and xtask's tests hold that lock against them.
    let progs: BTreeSet<&str> = BOOT_SETTINGS.iter().map(|(_, prog)| *prog).collect();
    assert_eq!(progs.len(), BOOT_SETTINGS.len(), "{BOOT_SETTINGS:?}");
    let names: BTreeSet<&str> = BOOT_SETTINGS.iter().map(|(what, _)| *what).collect();
    assert_eq!(names.len(), BOOT_SETTINGS.len(), "{BOOT_SETTINGS:?}");
    for (_, prog) in BOOT_SETTINGS {
        assert!(prog.starts_with('/'), "{prog} must be an absolute path");
    }
}

#[test]
fn the_escape_hatch_is_recognised_on_a_colon_joined_command_line() {
    // The Eclipse kernel joins boot arguments with `:`, so a token tested
    // with a space-splitting parser is a hatch that silently is not there.
    let colon = "LOG=warn:init.serial_setup:desktop=labwc";
    assert!(cmdline_has_in(colon, SERIAL_SETUP));
    assert!(cmdline_has_in("LOG=warn init.serial_setup", SERIAL_SETUP));
    assert!(!cmdline_has_in("LOG=warn:desktop=labwc", SERIAL_SETUP));
    // And not matched as a prefix of something else: a token that fires on
    // `init.serial_setupx` would be a hatch nobody can close.
    assert!(!cmdline_has_in("LOG=warn:init.serial_setupx", SERIAL_SETUP));
}

#[test]
fn a_boot_setting_whose_program_is_missing_is_not_fatal() {
    // An image without the desktop stack has no such script, and the boot
    // has always carried on with the compiled defaults. Spawning, not
    // `status()`, must keep that: a `None` child is waited for as a no-op.
    let child = spawn_boot_setting("/nonexistent/eclipse-kbd");
    assert!(child.is_none());
    wait_boot_setting("the keyboard layout", "/nonexistent/eclipse-kbd", child);
}

#[test]
fn the_three_boot_settings_really_do_overlap() {
    // The point of the change: three waits that overlap instead of adding
    // up. Three `sleep`s of 300 ms each take about 300 ms together and about
    // 900 ms one after another, so the distinction is not a matter of
    // measurement noise. Driven through the same spawn/wait pair the boot
    // uses, because a test that re-implemented them would pass over a
    // `apply_boot_settings` that still waited inside its loop.
    let started = std::time::Instant::now();
    let kids: Vec<Option<std::process::Child>> = (0..3)
        .map(|_| std::process::Command::new("sleep").arg("0.3").spawn().ok())
        .collect();
    if kids.iter().any(Option::is_none) {
        eprintln!("skipping: no `sleep` on this host");
        return;
    }
    for kid in kids {
        wait_boot_setting("a test sleep", "sleep", kid);
    }
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(700),
        "three 300 ms waits took {waited:?}; they did not overlap"
    );
}

// -- Boot arguments ----------------------------------------------------
//
// The Eclipse kernel joins boot arguments with `:`, which is why none of
// this can reuse a space-splitting parser.

#[test]
fn a_boot_argument_is_matched_as_a_whole_token_on_either_separator() {
    let colon = "LOG=warn:desktop=labwc:renderer=gl-sw:nvidia.nouveau_uapi";
    assert!(cmdline_has_in(colon, "nvidia.nouveau_uapi"));
    assert!(cmdline_has_in(colon, "renderer=gl-sw"));
    assert!(cmdline_has_in(
        "LOG=warn nvidia.nouveau_uapi",
        "nvidia.nouveau_uapi"
    ));
    assert!(cmdline_has_in(
        "a\tb\nnvidia.nouveau_uapi",
        "nvidia.nouveau_uapi"
    ));
    // A prefix is not the token: this is what keeps `renderer=gl` from
    // firing on `renderer=gl-sw`, and the whole renderer gate rests on it.
    assert!(!cmdline_has_in(colon, "renderer=gl"));
    assert!(!cmdline_has_in(
        "nvidia.nouveau_uapi_off",
        "nvidia.nouveau_uapi"
    ));
    // A token of a LONGER name must not match either.
    assert!(!cmdline_has_in(
        "xnvidia.nouveau_uapi",
        "nvidia.nouveau_uapi"
    ));
}

#[test]
fn an_empty_boot_argument_value_is_not_a_value() {
    // `cmdline_value` reports the empty string rather than `None`, and every
    // caller filters it out. Both halves matter, so both are asserted.
    assert_eq!(cmdline_value("desktop=:LOG=warn", "desktop="), Some(""));
    assert_eq!(cmdline_value("LOG=warn", "desktop="), None);
    assert_eq!(cmdline_value("desktop=xorg", "desktop="), Some("xorg"));
}

// -- Which session boots -----------------------------------------------

#[test]
fn the_session_comes_from_the_cmdline_then_the_file_then_labwc() {
    // 1. the cmdline wins over the file
    assert_eq!(
        selected_desktop_from("LOG=warn:desktop=xorg", Some("labwc\n")),
        "xorg"
    );
    // 2. the file when the cmdline says nothing
    assert_eq!(selected_desktop_from("LOG=warn", Some("xorg\n")), "xorg");
    // 3. labwc when neither does
    assert_eq!(selected_desktop_from("", None), "labwc");
    assert_eq!(selected_desktop_from("", Some("   \n")), "labwc");
    // `desktop=none` is the ISO installer: a real value, not a fallback.
    assert_eq!(selected_desktop_from("desktop=none", None), "none");
    // An EMPTY cmdline value falls through to the file rather than
    // selecting a session named "", which would match no service's
    // `desktop =` and silently boot with no compositor at all.
    assert_eq!(selected_desktop_from("desktop=", Some("xorg\n")), "xorg");
}

// -- The UI language ---------------------------------------------------

#[test]
fn the_ui_language_comes_from_the_cmdline_then_the_file_then_spanish() {
    assert_eq!(ui_lang_from("lang=en", Some("lang=es\n")), "en");
    assert_eq!(ui_lang_from("", Some("lang=en\n")), "en");
    assert_eq!(ui_lang_from("", None), "es");
    // Every accepted spelling, since these come from a human-edited file.
    for spelling in ["en", "EN", "en_US"] {
        assert_eq!(ui_lang_from(&format!("lang={spelling}"), None), "en");
    }
    for spelling in ["es", "ES", "es_ES"] {
        assert_eq!(ui_lang_from(&format!("lang={spelling}"), None), "es");
    }
}

#[test]
fn an_unknown_language_falls_through_instead_of_winning() {
    // There is no French translation, so `lang=fr` must not be honoured as
    // a language -- and must not shadow the file either, which is the part
    // a "first match wins" parser gets wrong.
    assert_eq!(ui_lang_from("lang=fr", Some("lang=en\n")), "en");
    assert_eq!(ui_lang_from("lang=fr", None), "es");
}

#[test]
fn the_locale_file_ignores_blanks_and_comments() {
    let file = "# escrito por eclipse-locale\n\n   \n  lang=en  \n";
    assert_eq!(ui_lang_from("", Some(file)), "en");
    // A commented-out setting is not a setting.
    assert_eq!(ui_lang_from("", Some("#lang=en\n")), "es");
}

#[test]
fn the_language_decides_both_the_posix_locale_and_the_language_list() {
    // foot refuses to render under a non-UTF-8 locale, so the POSIX name
    // matters as much as the choice.
    let mut env = Vec::new();
    overlay_locale_for(&mut env, "en");
    let vars = as_strings(&env);
    assert!(vars.contains(&"LANG=en_US.UTF-8".to_string()), "{vars:?}");
    assert!(vars.contains(&"LANGUAGE=en".to_string()), "{vars:?}");

    let mut env = Vec::new();
    overlay_locale_for(&mut env, "es");
    let vars = as_strings(&env);
    assert!(vars.contains(&"LANG=es_ES.UTF-8".to_string()), "{vars:?}");
    // Spanish falls back to English, not to nothing: a string with no
    // Spanish translation should still come out readable.
    assert!(vars.contains(&"LANGUAGE=es:en".to_string()), "{vars:?}");
}

#[test]
fn the_locale_overlay_replaces_rather_than_appends() {
    // The base CHILD_ENV already carries a LANG. Two LANG entries in one
    // environment is not "the last one wins" for every libc, so the old
    // one has to go.
    let mut env = vec![
        CString::new("LANG=C").unwrap(),
        CString::new("LANGUAGE=de").unwrap(),
        CString::new("LC_ALL=C").unwrap(),
        CString::new("PATH=/bin").unwrap(),
    ];
    overlay_locale_for(&mut env, "es");
    let vars = as_strings(&env);
    assert_eq!(vars.iter().filter(|v| v.starts_with("LANG=")).count(), 1);
    assert_eq!(
        vars.iter().filter(|v| v.starts_with("LANGUAGE=")).count(),
        1
    );
    // LC_ALL overrides LANG in every libc, so leaving a stale one behind
    // would silently beat the language just chosen.
    assert!(!vars.iter().any(|v| v.starts_with("LC_ALL=")), "{vars:?}");
    assert!(vars.contains(&"PATH=/bin".to_string()), "{vars:?}");
}

// -- The timezone ------------------------------------------------------

#[test]
fn the_timezone_precedence_is_tz_then_country_then_the_file() {
    assert_eq!(
        tz_from("tz=Asia/Tokyo", Some("tz=Europe/Berlin\n")),
        "Asia/Tokyo"
    );
    assert_eq!(tz_from("country=US", None), "America/New_York");
    // `tz=` beats `country=` on the same command line.
    assert_eq!(tz_from("country=US:tz=Asia/Tokyo", None), "Asia/Tokyo");
    assert_eq!(tz_from("", Some("tz=Europe/Berlin\n")), "Europe/Berlin");
    assert_eq!(tz_from("", Some("country=US\n")), "America/New_York");
    // And in the FILE too, whichever line comes first: `country=` is the
    // fallback for a machine that was never told its zone, so a file that
    // carries both is a machine that WAS told.
    assert_eq!(
        tz_from("", Some("country=US\ntz=Europe/Berlin\n")),
        "Europe/Berlin"
    );
    assert_eq!(
        tz_from("", Some("tz=Europe/Berlin\ncountry=US\n")),
        "Europe/Berlin"
    );
    assert_eq!(tz_from("", None), "Europe/Madrid");
}

#[test]
fn an_empty_or_truncated_timezone_falls_through_instead_of_meaning_utc() {
    // `TZ=` is not "unset" to a libc: it reads as UTC. A half-written file
    // or a zero-filled tail after an unclean power cut would otherwise put
    // the clock an hour or two off with nothing to explain it.
    assert_eq!(tz_from("tz=", Some("tz=Europe/Berlin\n")), "Europe/Berlin");
    assert_eq!(tz_from("tz=", None), "Europe/Madrid");
    assert_eq!(tz_from("", Some("tz=\n")), "Europe/Madrid");
    assert_eq!(tz_from("", Some("tz=\ncountry=US\n")), "America/New_York");
    assert_eq!(
        tz_from("country=", Some("tz=Europe/Berlin\n")),
        "Europe/Berlin"
    );
    // And an empty `country=` in the FILE is not a country either.
    assert_eq!(tz_from("", Some("country=\n")), "Europe/Madrid");
}

#[test]
fn the_timezone_file_ignores_blanks_and_comments_and_takes_the_last_setting() {
    let file = "# escrito por eclipse-tz\n\n  tz=Europe/Berlin  \n";
    assert_eq!(tz_from("", Some(file)), "Europe/Berlin");
    assert_eq!(tz_from("", Some("#tz=Asia/Tokyo\n")), "Europe/Madrid");
    // Rewritten in place by `eclipse-tz`, so a duplicated key is the last
    // write, not the first.
    assert_eq!(
        tz_from("", Some("tz=Asia/Tokyo\ntz=Europe/Berlin\n")),
        "Europe/Berlin"
    );
}

#[test]
fn a_country_this_image_does_not_know_lands_on_the_default_zone() {
    assert_eq!(tz_for_country("US"), "America/New_York");
    assert_eq!(tz_for_country("us"), "America/New_York");
    assert_eq!(tz_for_country("ES"), "Europe/Madrid");
    assert_eq!(tz_for_country("FR"), "Europe/Madrid");
}

#[test]
fn a_timezone_with_a_nul_byte_does_not_abort_pid_one() {
    // A zero-filled tail after an unclean power cut reaches `CString::new`
    // as an interior NUL. PID 1 must not die on the first spawn.
    let mut env = vec![CString::new("TZ=UTC").unwrap()];
    overlay_tz_with(&mut env, "Europe/\0Madrid");
    let vars = as_strings(&env);
    assert!(!vars.iter().any(|v| v.starts_with("TZ=")), "{vars:?}");

    let mut env = vec![CString::new("TZ=UTC").unwrap()];
    overlay_tz_with(&mut env, "Asia/Tokyo");
    assert_eq!(as_strings(&env), vec!["TZ=Asia/Tokyo".to_string()]);
}

// -- Service files -----------------------------------------------------

#[test]
fn a_service_file_parses_every_key_it_documents() {
    let svc = parse_service(
        "labwc",
        "# el compositor\n\
         exec = /usr/local/bin/labwc --config /etc/labwc\n\
         type = respawn\n\
         after = seatd gtk-caches dbus\n\
         desktop = labwc\n\
         log = /tmp/labwc.log\n\
         wait_socket = /run/seatd.sock\n\
         wait_path = /dev/input\n",
    )
    .expect("parsea");
    assert_eq!(svc.name, "labwc");
    assert_eq!(
        svc.exec,
        vec!["/usr/local/bin/labwc", "--config", "/etc/labwc"]
    );
    assert_eq!(svc.kind, Kind::Respawn);
    assert_eq!(svc.after, vec!["seatd", "gtk-caches", "dbus"]);
    assert_eq!(svc.desktop.as_deref(), Some("labwc"));
    assert_eq!(svc.log.as_deref(), Some("/tmp/labwc.log"));
    assert_eq!(svc.wait_socket.as_deref(), Some("/run/seatd.sock"));
    assert_eq!(svc.wait_path.as_deref(), Some("/dev/input"));
    // Not set means not set, not an empty string.
    assert_eq!(svc.cmdline, None);
}

#[test]
fn a_service_without_exec_is_refused_rather_than_started_empty() {
    assert!(parse_service("vacio", "type = respawn\n").is_none());
    assert!(parse_service("vacio", "exec =   \n").is_none());
    assert!(parse_service("vacio", "").is_none());
    // A line with no `=` is not a setting, so this has no exec either.
    assert!(parse_service("vacio", "exec /usr/bin/foo\n").is_none());
}

#[test]
fn a_value_may_contain_the_separator() {
    // `split_once`, not `split`: a flag with its own `=` has to survive.
    let svc = parse_service("x", "exec = /bin/sh -c a=b\n").expect("parsea");
    assert_eq!(svc.exec, vec!["/bin/sh", "-c", "a=b"]);
    let svc = parse_service("x", "exec = /b/f\nlog = /tmp/a=b.log\n").expect("parsea");
    assert_eq!(svc.log.as_deref(), Some("/tmp/a=b.log"));
}

#[test]
fn a_type_that_is_not_respawn_is_a_oneshot() {
    // The default, and the documented spelling of it.
    assert_eq!(
        parse_service("x", "exec = /b/f\n").unwrap().kind,
        Kind::Oneshot
    );
    let svc = parse_service("x", "exec = /b/f\ntype = oneshot\n").unwrap();
    assert_eq!(svc.kind, Kind::Oneshot);
    // A typo also lands here -- deliberately, because supervising something
    // forever on a guess is worse -- and now says so in the log.
    let svc = parse_service("x", "exec = /b/f\ntype = respwan\n").unwrap();
    assert_eq!(svc.kind, Kind::Oneshot);
    // Case matters: the format is lowercase.
    let svc = parse_service("x", "exec = /b/f\ntype = Respawn\n").unwrap();
    assert_eq!(svc.kind, Kind::Oneshot);
}

#[test]
fn whitespace_and_comments_around_a_setting_are_not_part_of_it() {
    let svc = parse_service(
        "x",
        "\n   # comentario\n\n   exec   =   /bin/foo   \n  type =  respawn  \n",
    )
    .expect("parsea");
    assert_eq!(svc.exec, vec!["/bin/foo"]);
    assert_eq!(svc.kind, Kind::Respawn);
}

// -- Start order -------------------------------------------------------

fn svc_named(name: &str, after: &[&str]) -> Service {
    let mut text = String::from("exec = /bin/true\n");
    if !after.is_empty() {
        text.push_str(&format!("after = {}\n", after.join(" ")));
    }
    parse_service(name, &text).expect("parsea")
}

/// The backoff is a deadline per service, not a sleep in PID 1, so a
/// service that is waiting one out does not hold back the loop -- and,
/// above all, does not stop it reaping anyone else.
///
/// The bug: the loop used to `nanosleep` the backoff right after reaping
/// the crasher. Every other child that died during that window was reaped
/// only afterwards, and `uptime` is measured at the reap, so a service
/// that failed `execve` and `_exit(127)`ed in a millisecond was credited
/// with the whole backoff, judged healthy, restarted at once and had its
/// own backoff reset -- for ever. On hardware that was
/// `oopslog exited after 8.049671271s (exit 127), restarting`, over and
/// over, for a script that is an infinite loop and cannot exit at all.
#[test]
fn a_service_waiting_out_its_backoff_does_not_hold_back_the_others() {
    let mut map = BTreeMap::new();
    for name in ["crasher", "other"] {
        map.insert(
            name.to_string(),
            parse_service(name, "exec = /bin/true\ntype = respawn\n").expect("parsea"),
        );
    }
    let now = Instant::now();
    map.get_mut("crasher").unwrap().restart_at = Some(now + Duration::from_secs(8));

    let due = due_names(&map, now);
    assert!(
        !due.contains(&"crasher".to_string()),
        "un servicio en su backoff se reinicio antes de tiempo: {due:?}"
    );
    assert!(
        due.contains(&"other".to_string()),
        "el backoff de otro servicio retuvo a este: {due:?}"
    );

    // And once the deadline has passed it comes back by itself.
    let due = due_names(&map, now + Duration::from_secs(8));
    assert!(
        due.contains(&"crasher".to_string()),
        "el servicio no volvio nunca tras su backoff: {due:?}"
    );
}

// -- A oneshot that never exits -----------------------------------------

/// The hole this closes: every bounded wait in this file has a timeout,
/// but the `waitpid` on a oneshot's child had none, so ONE wrapper that
/// never exited hung the whole boot -- no later service was started, the
/// supervision loop was never reached, and the shutdown flags are only
/// read in there, so Ctrl-Alt-Del did nothing either.
#[test]
fn a_oneshot_gets_a_start_timeout_and_a_respawn_service_does_not() {
    let oneshot = parse_service("boot-sound", "exec = /bin/foo\ntype = oneshot\n").expect("parsea");
    assert_eq!(
        start_timeout(oneshot.kind, oneshot.timeout),
        Some(DEFAULT_ONESHOT_TIMEOUT),
        "un oneshot sin 'timeout =' se quedo sin limite: un script colgado cuelga el arranque"
    );

    // A respawn service is never waited for -- the supervision loop owns
    // it -- so it has no start timeout however its file is written.
    let respawn =
        parse_service("labwc", "exec = /bin/foo\ntype = respawn\ntimeout = 5\n").expect("parsea");
    assert_eq!(start_timeout(respawn.kind, respawn.timeout), None);
}

#[test]
fn the_timeout_key_takes_seconds_and_zero_means_wait_for_ever() {
    assert_eq!(parse_limit("5"), Some(Limit::After(Duration::from_secs(5))));
    assert_eq!(parse_limit("0"), Some(Limit::Never));
    assert_eq!(parse_limit("none"), Some(Limit::Never));
    assert_eq!(parse_limit("never"), Some(Limit::Never));
    // Not a number of seconds: `None`, so `parse_service` keeps the
    // default. Accepting a typo as "no limit" would silently restore the
    // hang this key exists to stop.
    assert_eq!(parse_limit("30s"), None);
    assert_eq!(parse_limit(""), None);
    assert_eq!(parse_limit("-1"), None);

    let svc =
        parse_service("x", "exec = /bin/foo\ntype = oneshot\ntimeout = 30s\n").expect("parsea");
    assert_eq!(
        start_timeout(svc.kind, svc.timeout),
        Some(DEFAULT_ONESHOT_TIMEOUT),
        "un 'timeout =' mal escrito dejo al oneshot sin limite"
    );

    let forever =
        parse_service("x", "exec = /bin/foo\ntype = oneshot\ntimeout = 0\n").expect("parsea");
    assert_eq!(start_timeout(forever.kind, forever.timeout), None);
}

/// SIGTERM, then SIGKILL, then boot on regardless: the last step matters
/// as much as the first, because a child wedged inside the kernel never
/// gets reaped and waiting on it is the hang all over again.
#[test]
fn an_overrun_oneshot_is_asked_then_killed_then_left_behind() {
    let limit = Duration::from_secs(90);
    let grace = Duration::from_secs(2);
    let at = |secs| overrun_action(Duration::from_secs(secs), limit, grace);
    assert_eq!(at(0), Overrun::Wait);
    assert_eq!(at(89), Overrun::Wait);
    assert_eq!(at(90), Overrun::Term);
    assert_eq!(at(91), Overrun::Term);
    assert_eq!(at(92), Overrun::Kill);
    assert_eq!(at(93), Overrun::Kill);
    assert_eq!(at(94), Overrun::Abandon);
    assert_eq!(at(600), Overrun::Abandon);
}

/// A oneshot wrapper that exits leaving a grandchild running is normal and
/// not interfered with (`eclipse-boot-sound` forks `mpg123` and exits on
/// purpose). But once init has SIGTERMed the whole group because the
/// service overran, a shell that dies on that signal while its grandchild
/// ignores it must NOT end the wait: that would leave running exactly the
/// workload the timeout exists to stop, and skip the promised SIGKILL.
#[test]
fn a_reaped_shell_does_not_end_the_wait_once_the_group_is_being_killed() {
    assert_eq!(after_child_exit(false), Watch::Done);
    assert_eq!(after_child_exit(true), Watch::Group);
}

// -- A respawn service that can never work ------------------------------

/// The backoff makes a crash loop cheap but never ends it: before this, a
/// service that could not work printed its death every `MAX_BACKOFF` for
/// as long as the machine was on, and that console is the only diagnostic
/// output the box has.
#[test]
fn a_service_that_never_comes_up_is_given_up_on_instead_of_retried_for_ever() {
    let mut svc = parse_service("crasher", "exec = /bin/foo\ntype = respawn\n").expect("parsea");
    let crash = Duration::from_millis(40);
    for n in 1..CRASH_START_LIMIT {
        assert!(
            !note_crash(&mut svc, crash, Some(1)),
            "se rindio en el intento {n}, antes del limite"
        );
        assert!(!svc.given_up);
    }
    assert!(
        note_crash(&mut svc, crash, Some(1)),
        "nunca se rindio: la consola sigue llenandose cada {MAX_BACKOFF:?} para siempre"
    );
    assert!(svc.given_up);
    assert!(svc.restart_at.is_none());

    // And the restart pass leaves it alone from then on.
    let mut map = BTreeMap::new();
    map.insert("crasher".to_string(), svc);
    assert!(due_names(&map, Instant::now()).is_empty());
}

// -- Reaping during the boot's bounded gates -----------------------------

/// A respawn service that dies during a `wait_socket` gate must be
/// credited with the time IT lived, not with the gate as well.
///
/// This is the same mistake the backoff made when it slept inside the
/// supervision loop (the `oopslog exited after 8.049671271s` storm): the
/// uptime used to be measured when the loop got round to reaping, so a
/// service that failed `execve` in a millisecond during labwc's 10 s gate
/// was read as having stayed up, judged healthy, restarted at once and had
/// its backoff reset -- for ever.
#[test]
fn an_exit_collected_during_a_gate_keeps_the_uptime_it_really_had() {
    let mut map = BTreeMap::new();
    let mut svc = respawn_svc("crasher", "");
    let started = Instant::now();
    svc.pid = Some(4242);
    svc.started_at = Some(started);
    map.insert("crasher".to_string(), svc);

    // Reaped 40 ms after it started, handed over much later (the gate).
    let reaped = started + Duration::from_millis(40);
    note_exit(&mut map, 4242, 0, reaped);

    let svc = &map["crasher"];
    assert_eq!(svc.pid, None, "el pid no se limpio: nadie lo reiniciaria");
    assert_eq!(
        svc.crash_starts, 1,
        "la caida de 40 ms se conto como una vuelta sana porque se midio \
         hasta el final de la puerta"
    );
    assert!(
        svc.backoff > MIN_BACKOFF,
        "el backoff se quedo en el minimo: la caida paso por sana"
    );
    assert_eq!(
        svc.restart_at,
        Some(reaped + MIN_BACKOFF),
        "el plazo se calculo desde ahora y no desde la muerte"
    );
}

/// A healthy run still is one: the same instant arithmetic has to say yes
/// when the service really did stay up.
#[test]
fn a_service_that_really_stayed_up_is_restarted_at_once() {
    let mut map = BTreeMap::new();
    let mut svc = respawn_svc("worker", "");
    let started = Instant::now();
    svc.pid = Some(77);
    svc.started_at = Some(started);
    svc.backoff = MAX_BACKOFF;
    map.insert("worker".to_string(), svc);

    note_exit(&mut map, 77, 0, started + HEALTHY_UPTIME);
    assert_eq!(map["worker"].crash_starts, 0);
    assert_eq!(
        map["worker"].backoff, MIN_BACKOFF,
        "no se reinicio el backoff"
    );
    assert_eq!(map["worker"].restart_at, Some(started + HEALTHY_UPTIME));
}

/// A pid that belongs to no service was a oneshot's leftover or an orphan
/// reparented to init: reaping it was the whole job, and it must not
/// disturb anybody.
#[test]
fn reaping_an_orphan_changes_nothing() {
    let mut map = BTreeMap::new();
    let mut svc = respawn_svc("worker", "");
    svc.pid = Some(5);
    map.insert("worker".to_string(), svc);
    note_exit(&mut map, 99999, 0, Instant::now());
    assert_eq!(map["worker"].pid, Some(5));
    assert!(map["worker"].restart_at.is_none());
}

/// Serializes the tests that touch the global `PENDING_EXITS`. Poisoning
/// is stepped over here too: one failing test must not take the rest of
/// them down with it.
fn queue_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static QUEUE_TESTS: Mutex<()> = Mutex::new(());
    QUEUE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The queue is what makes reaping inside a gate safe: a bare
/// `waitpid(-1)` in there would swallow a respawn service's death, the
/// loop would never see that pid, and nothing would ever restart it.
#[test]
fn what_a_gate_reaps_is_handed_to_the_loop_and_handed_over_once() {
    // `PENDING_EXITS` is global and the suite runs in parallel, so any
    // test that touches the queue has to hold this first or the counts
    // below belong to whoever else was pushing.
    let _exclusive = queue_test_lock();
    // Start from empty: another test in this binary may have run a gate.
    let _ = take_pending();
    let at = Instant::now();
    queue_exit(Exit {
        pid: 11,
        status: 0,
        at,
    });

    let taken = take_pending();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].pid, 11);
    assert_eq!(taken[0].at, at, "se perdio el instante de la muerte");
    assert!(
        take_pending().is_empty(),
        "la misma muerte se entrego dos veces"
    );
}

/// Poisoning is reachable since #1748 (a panic in PID 1 unwinds now), and
/// a dropped exit is a respawn service stranded with `pid = Some(..)` for
/// ever -- so the queue has to survive it.
#[test]
fn a_poisoned_queue_still_hands_the_death_over() {
    let _exclusive = queue_test_lock();
    let _ = take_pending();

    // Poison it the only way it can be poisoned: panic while holding it.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| {
        let _held = PENDING_EXITS.lock().unwrap();
        panic!("boom");
    });
    std::panic::set_hook(previous);
    assert!(PENDING_EXITS.is_poisoned(), "no se llego a envenenar");

    queue_exit(Exit {
        pid: 12,
        status: 0,
        at: Instant::now(),
    });
    let taken = take_pending();
    assert_eq!(taken.len(), 1, "la muerte se perdio con el envenenamiento");
    assert_eq!(taken[0].pid, 12);
}

// -- PID 1 may not die --------------------------------------------------

/// The worst outcome in the system is a dead PID 1: the kernel answers it
/// with "Attempted to kill init" and the whole machine stops, for a bug in
/// one decision. So the two places that run decision code -- a service
/// start and the supervision loop -- run inside `guard`, and the release
/// profile unwinds instead of aborting (a test cannot read Cargo.toml's
/// profile, but `catch_unwind` returning here at all is the half that
/// would be impossible under `abort`).
#[test]
fn a_panic_is_caught_instead_of_taking_pid_1_down() {
    // The hook would print the panic; silence it for the test so the
    // suite's output stays readable, then put the default back.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    assert_eq!(guard("doing something fine", || 7), Some(7));
    let after = std::cell::Cell::new(false);
    assert_eq!(
        guard("doing something broken", || -> i32 { panic!("boom") }),
        None,
        "un panic no se contuvo: PID 1 se moriria y con el la maquina"
    );
    // And init goes on to do the next thing.
    guard("carrying on", || after.set(true));
    assert!(after.get());

    // An index out of range is the realistic shape of it, not a `panic!`.
    let empty: Vec<u32> = Vec::new();
    assert_eq!(guard("indexing", || empty[1]), None);

    std::panic::set_hook(previous);
}

/// The loop in `main` re-enters `supervise` for as long as it keeps
/// panicking, because PID 1 may not return either. What stops that being a
/// console-filling hot loop is the pause, so it has to be a real one.
#[test]
fn the_pause_before_re_entering_the_loop_is_long_enough_to_not_be_a_hot_loop() {
    assert!(PANIC_PAUSE >= Duration::from_millis(500), "{PANIC_PAUSE:?}");
    // And short enough that a machine whose supervision loop is panicking
    // still answers a shutdown promptly: the pause is interruptible, and
    // POLL_SLICE is the loop's own idea of prompt.
    assert!(PANIC_PAUSE <= Duration::from_secs(5), "{PANIC_PAUSE:?}");
}

// -- Logs that live in RAM ----------------------------------------------

/// Every `log =` the images ship is under `/tmp`, a tmpfs, so its bytes are
/// the machine's memory. The cap is what stops a service that writes on
/// every turn of a crash loop -- the one logging hardest is the one that
/// cannot stop crashing -- from eating it.
#[test]
fn a_log_is_rotated_once_it_outgrows_the_cap_and_the_writers_fd_survives() {
    assert!(!log_overflow(0));
    assert!(!log_overflow(MAX_LOG_BYTES - 1));
    assert!(log_overflow(MAX_LOG_BYTES));

    let dir = scratch("log-cap");
    let path = dir.join("chatty.log");
    let name = path.to_str().unwrap();

    // Under the cap: untouched, and no stale generation left behind.
    fs::write(&path, b"corto\n").unwrap();
    assert!(rotate_log(name).is_none());
    assert!(!dir.join("chatty.log.1").exists());

    // Over it: the contents move aside and the file starts again.
    fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
    // The writer holds an O_APPEND fd on this inode, exactly as a service
    // does, and must still be writing to the SAME file afterwards -- which
    // is why this is a copy and a truncate and not a rename.
    use std::io::Write;
    let mut writer = fs::File::options().append(true).open(&path).unwrap();

    let line = rotate_log(name).expect("deberia rotar");
    assert!(line.contains("chatty.log.1"), "{line}");
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        fs::metadata(dir.join("chatty.log.1")).unwrap().len(),
        MAX_LOG_BYTES,
        "la generacion anterior no se guardo entera"
    );

    writer.write_all(b"despues\n").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "despues\n",
        "el servicio siguio escribiendo en otro sitio: su fd ya no apunta al log"
    );

    // And now it is under the cap again, so the next sweep leaves it be.
    assert!(rotate_log(name).is_none());
    let _ = fs::remove_dir_all(&dir);
}

/// Two services can name the same `log =` -- `boot-sound` and
/// `boot-sound-xorg` both write `/tmp/boot-sound.log` -- and rotating it
/// twice in one sweep would throw away the generation just kept.
#[test]
fn a_log_two_services_share_is_rotated_once_per_sweep() {
    let dir = scratch("log-shared");
    let path = dir.join("shared.log");
    fs::write(&path, vec![b'y'; MAX_LOG_BYTES as usize]).unwrap();

    let mut map = BTreeMap::new();
    for name in ["boot-sound", "boot-sound-xorg"] {
        map.insert(
            name.to_string(),
            parse_service(
                name,
                &format!("exec = /bin/foo\nlog = {}\n", path.to_str().unwrap()),
            )
            .expect("parsea"),
        );
    }
    sweep_logs(&map);
    assert_eq!(
        fs::metadata(dir.join("shared.log.1")).unwrap().len(),
        MAX_LOG_BYTES,
        "la segunda rotacion de la misma pasada se llevo la generacion guardada"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A log that cannot be rotated must not stop anything: init reports
/// nothing and carries on.
#[test]
fn a_log_that_is_not_there_is_not_a_failure() {
    let dir = scratch("log-missing");
    assert!(rotate_log(dir.join("ghost.log").to_str().unwrap()).is_none());
    // A `log = /dev/null` (the documented way to opt out) is not a file
    // with a length to outgrow either.
    assert!(rotate_log("/dev/null").is_none());
    let _ = fs::remove_dir_all(&dir);
}

// -- A requirement that was given up on ---------------------------------

fn respawn_svc(name: &str, extra: &str) -> Service {
    parse_service(name, &format!("exec = /bin/foo\ntype = respawn\n{extra}")).expect("parsea")
}

/// With `seatd` written off, labwc is hopeless and so is everything behind
/// it. Before this, each of them burned its own CRASH_START_LIMIT starts --
/// labwc paying a 10 s `wait_socket` gate every time -- against a socket
/// nothing was ever going to create, and the console carried their deaths
/// instead of the one failure that mattered.
#[test]
fn giving_up_on_a_requirement_gives_up_on_the_chain_behind_it() {
    let mut map = BTreeMap::new();
    map.insert("seatd".to_string(), respawn_svc("seatd", ""));
    map.insert(
        "labwc".to_string(),
        respawn_svc("labwc", "requires = seatd\n"),
    );
    map.insert(
        "lunarbar".to_string(),
        respawn_svc("lunarbar", "requires = labwc\n"),
    );
    // Ordered after labwc but not requiring it: unaffected.
    map.insert(
        "oopslog".to_string(),
        respawn_svc("oopslog", "after = labwc\n"),
    );

    // Nothing has failed yet, so nothing is blocked.
    assert!(propagate_given_up(&mut map).is_empty());

    map.get_mut("seatd").unwrap().given_up = true;
    let blocked = propagate_given_up(&mut map);
    assert_eq!(
        blocked,
        vec![
            ("labwc".to_string(), "seatd".to_string()),
            ("lunarbar".to_string(), "labwc".to_string()),
        ],
        "la cadena detras de un requisito perdido no se propago: {blocked:?}"
    );
    assert!(map["labwc"].given_up && map["lunarbar"].given_up);
    assert!(
        !map["oopslog"].given_up,
        "un 'after =' que no es requisito no debe arrastrar a nadie"
    );

    // Reported once: the second pass has nothing to say.
    assert!(propagate_given_up(&mut map).is_empty());
    // And the restart pass leaves the written-off ones alone.
    let due = due_names(&map, Instant::now());
    assert_eq!(due, vec!["oopslog".to_string()], "{due:?}");
}

/// A `requires =` is an ordering too, so the trap systemd leaves -- a unit
/// that requires another without being ordered after it, and so starts
/// beside it -- cannot be written here.
#[test]
fn a_requirement_is_ordered_first_even_with_no_after_line() {
    let svc = respawn_svc("labwc", "requires = seatd\n");
    assert!(svc.after.contains(&"seatd".to_string()));

    let mut map = BTreeMap::new();
    map.insert("labwc".to_string(), svc);
    map.insert("seatd".to_string(), respawn_svc("seatd", ""));
    let order = ordered_names(&map);
    assert_eq!(
        order,
        vec!["seatd".to_string(), "labwc".to_string()],
        "{order:?}"
    );

    // Named twice, it is still one entry.
    let both = respawn_svc("labwc", "after = seatd dbus\nrequires = seatd\n");
    assert_eq!(both.after, vec!["seatd".to_string(), "dbus".to_string()]);
}

/// A requirement that is not a service of this boot is ignored, exactly as
/// `ordered_names` ignores it: the session that dropped it would otherwise
/// lose every service behind it.
#[test]
fn a_requirement_that_does_not_exist_blocks_nobody() {
    let mut map = BTreeMap::new();
    map.insert(
        "lunarbar".to_string(),
        respawn_svc("lunarbar", "requires = labwc\n"),
    );
    assert!(propagate_given_up(&mut map).is_empty());
    assert!(!map["lunarbar"].given_up);
}

/// One healthy run clears the count, so a service that is merely slow to
/// find its dependency is never written off: this only ever fires on a
/// service that has not once come up since boot.
#[test]
fn one_healthy_run_clears_the_crash_count() {
    let mut svc = parse_service("slow", "exec = /bin/foo\ntype = respawn\n").expect("parsea");
    for _ in 0..CRASH_START_LIMIT - 1 {
        note_crash(&mut svc, Duration::from_millis(40), Some(1));
    }
    assert_eq!(svc.crash_starts, CRASH_START_LIMIT - 1);
    assert!(!note_crash(&mut svc, HEALTHY_UPTIME, Some(0)));
    assert_eq!(svc.crash_starts, 0, "una vuelta sana no reinicio la cuenta");
    assert!(!svc.given_up);
    // Exactly HEALTHY_UPTIME counts as healthy, the same boundary
    // `restart_delay` uses, or the two would disagree about one instant.
    assert_eq!(restart_delay(HEALTHY_UPTIME, MAX_BACKOFF).0, Duration::ZERO);
}

/// The `eclipse-pulseaudio` loop, which ran for the whole of every
/// `minimal` boot. Its wrapper is
///
/// ```text
/// command -v pulseaudio >/dev/null 2>&1 || { echo ...; sleep 8; exit 127; }
/// ```
///
/// so each try lived 8 s, cleared `crash_starts` as a healthy run, and was
/// restarted at once with its backoff reset -- for ever:
///
/// ```text
/// respawn: pulseaudio exited after 8.125682158s (exit 127 ...), restarting
/// respawn: pulseaudio exited after 8.032575278s (exit 127 ...), restarting
/// ```
///
/// A sleep inside the service is what the supervisor's own backoff is for,
/// and it disabled both the backoff and the give-up limit.
#[test]
fn un_127_tras_ocho_segundos_no_es_una_vuelta_sana() {
    let mut svc = parse_service("pulseaudio", "exec = /bin/foo\ntype = respawn\n").expect("parsea");
    let vida = Duration::from_millis(8_125);
    assert!(
        vida > HEALTHY_UPTIME,
        "la premisa del test: la vuelta pasa de HEALTHY_UPTIME"
    );
    for n in 1..CRASH_START_LIMIT {
        assert!(
            !note_crash(&mut svc, vida, Some(127)),
            "se rindio en el intento {n}, antes del limite"
        );
        assert_eq!(
            svc.crash_starts, n,
            "la cuenta se reinicio: vuelve el bucle"
        );
    }
    assert!(
        note_crash(&mut svc, vida, Some(127)),
        "nunca se rindio: la consola se llena cada 8 s para siempre"
    );
    assert!(svc.given_up);
    assert!(svc.restart_at.is_none());
}

/// Y lo mismo con 126 (el bit x), que es el otro fallo que ningun
/// reintento arregla.
#[test]
fn un_126_tras_una_vuelta_larga_tampoco_es_sana() {
    let mut svc = parse_service("x", "exec = /bin/foo\ntype = respawn\n").expect("parsea");
    assert!(!note_crash(&mut svc, Duration::from_secs(30), Some(126)));
    assert_eq!(svc.crash_starts, 1);
}

/// Lo que NO puede cambiar: una vuelta larga que acaba en cualquier otro
/// codigo sigue siendo sana. Si esto se rompe, un servicio que se reinicia
/// por su cuenta cada hora se da por perdido.
#[test]
fn una_vuelta_larga_con_cualquier_otro_codigo_sigue_siendo_sana() {
    for code in [
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(125),
        Some(128),
        Some(255),
    ] {
        let mut svc = parse_service("ok", "exec = /bin/foo\ntype = respawn\n").expect("parsea");
        svc.crash_starts = 5;
        assert!(
            !note_crash(&mut svc, Duration::from_secs(30), code),
            "{code:?} se conto como caida"
        );
        assert_eq!(svc.crash_starts, 0, "{code:?} no reinicio la cuenta");
    }
}

/// La red general, debajo de `note_crash`: se cuentan los ARRANQUES, asi
/// que ningun `sleep` dentro del servicio puede esconder el bucle --
/// tampoco los `sleep 60; exit 1` de los wrappers, que ningun codigo de
/// salida delata.
#[test]
fn la_ventana_de_arranques_se_llena_y_entonces_se_rinde() {
    let mut ventana = VecDeque::new();
    let t0 = Instant::now();
    for n in 0..START_LIMIT_BURST {
        assert!(
            !note_start(&mut ventana, t0 + Duration::from_secs(n as u64 * 8)),
            "se nego el arranque {n}, dentro del cupo"
        );
    }
    assert_eq!(ventana.len(), START_LIMIT_BURST as usize);
    let ultimo = t0 + Duration::from_secs(START_LIMIT_BURST as u64 * 8);
    assert!(note_start(&mut ventana, ultimo), "el cupo no corto nada");
    assert_eq!(
        ventana.len(),
        START_LIMIT_BURST as usize,
        "una negativa no se apunta: apuntarla echaria de la ventana un arranque de verdad"
    );
}

/// Y la ventana olvida: un servicio que se cae despacio durante horas, sin
/// llegar al cupo dentro del intervalo, no se da por perdido nunca.
#[test]
fn la_ventana_olvida_lo_que_sale_del_intervalo() {
    let mut ventana = VecDeque::new();
    let mut t = Instant::now();
    // Un arranque por intervalo, mas un poco: nunca hay dos a la vez
    // dentro de la ventana.
    for _ in 0..START_LIMIT_BURST * 3 {
        assert!(
            !note_start(&mut ventana, t),
            "se rindio con un arranque por ventana"
        );
        assert_eq!(ventana.len(), 1);
        t += START_LIMIT_INTERVAL + Duration::from_secs(1);
    }
}

/// El borde: justo en el intervalo la marca SIGUE dentro (se tira lo que
/// pasa de el), igual que `HEALTHY_UPTIME` cuenta como sana.
#[test]
fn el_borde_del_intervalo_cuenta_como_dentro() {
    let mut ventana = VecDeque::new();
    let t0 = Instant::now();
    assert!(!note_start(&mut ventana, t0));
    assert!(!note_start(&mut ventana, t0 + START_LIMIT_INTERVAL));
    assert_eq!(ventana.len(), 2, "la marca del borde se tiro");
    let mut ventana = VecDeque::new();
    assert!(!note_start(&mut ventana, t0));
    assert!(!note_start(
        &mut ventana,
        t0 + START_LIMIT_INTERVAL + Duration::from_nanos(1)
    ));
    assert_eq!(ventana.len(), 1, "un nanosegundo despues sigue dentro");
}

/// Y el cable: que `start_service` cuente de verdad. Sin esta llamada la
/// ventana es una funcion que nadie usa, y el bucle de `pulseaudio` vuelve
/// entero. Arranca `/bin/true` de verdad (es lo que hace PID 1), con
/// `log = /dev/null` para no dejar un fichero por el camino.
#[test]
fn start_service_cuenta_los_arranques_y_acaba_rindiendose() {
    let mut svc = parse_service(
        "contados",
        "exec = /bin/true\ntype = respawn\nlog = /dev/null\n",
    )
    .expect("parsea");
    for n in 0..START_LIMIT_BURST {
        start_service(&mut svc);
        assert!(
            !svc.given_up,
            "se rindio en el arranque {n}, dentro del cupo"
        );
        assert_eq!(svc.starts.len() as u32, n + 1);
    }
    start_service(&mut svc);
    assert!(
        svc.given_up,
        "start_service no cuenta los arranques: el bucle de pulseaudio sigue vivo"
    );
    assert!(svc.pid.is_none());
    assert!(svc.restart_at.is_none());
}

/// 126 and 127 are the only exit codes that are not the program's opinion
/// but a report that it never ran, and a respawn service's stdio is
/// `/dev/null` unless its file sets `log =` -- so the number on the
/// console is all a reader gets.
#[test]
fn the_two_codes_that_mean_the_program_never_ran_are_spelled_out() {
    assert!(
        exit_note(127).contains("command not found"),
        "127 no dice que el programa no llego a correr: {:?}",
        exit_note(127)
    );
    assert!(
        exit_note(126).contains("not executable"),
        "126 no dice que no se pudo ejecutar: {:?}",
        exit_note(126)
    );
    for code in [0, 1, 2, 125, 128, 255] {
        assert_eq!(exit_note(code), "", "exit {code} no necesita nota");
    }
}

/// A supervised service with no `log =` used to write its output into
/// `/dev/null`, so when it died the number on the console was everything
/// anybody had. `oopslog.service` shipped exactly like that and spent a
/// whole boot repeating `exit 127` with the name of the missing command
/// nowhere on the machine.
#[test]
fn a_respawn_service_without_a_log_gets_one_rather_than_dev_null() {
    let defaulted = parse_service(
        "oopslog",
        "exec = /usr/local/bin/eclipse-oopslog\ntype = respawn\n",
    )
    .expect("parsea");
    assert_eq!(
        defaulted.log.as_deref(),
        Some("/tmp/oopslog.log"),
        "un servicio respawn sin `log =` sigue escribiendo a /dev/null"
    );

    // An explicit one still wins, including the opt-out.
    let explicit = parse_service(
        "dbus-system",
        "exec = /usr/local/bin/eclipse-dbus-system\ntype = respawn\nlog = /tmp/dbus-system.log\n",
    )
    .expect("parsea");
    assert_eq!(explicit.log.as_deref(), Some("/tmp/dbus-system.log"));
    let opted_out = parse_service(
        "quiet",
        "exec = /bin/true\ntype = respawn\nlog = /dev/null\n",
    )
    .expect("parsea");
    assert_eq!(opted_out.log.as_deref(), Some("/dev/null"));

    // A oneshot keeps none: it runs once and the boot step that waited for
    // it is what reports its failure.
    let once = parse_service("once", "exec = /bin/true\n").expect("parsea");
    assert_eq!(once.log, None, "un oneshot no necesita fichero propio");
}

fn order_of(defs: &[(&str, &[&str])]) -> Vec<String> {
    let mut map = BTreeMap::new();
    for (name, after) in defs {
        map.insert(name.to_string(), svc_named(name, after));
    }
    ordered_names(&map)
}

fn before(order: &[String], first: &str, second: &str) -> bool {
    let pos = |n: &str| order.iter().position(|x| x == n);
    match (pos(first), pos(second)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

#[test]
fn a_dependency_starts_before_the_service_that_lists_it() {
    // "labwc" < "seatd" alphabetically, so a map-order walk gets this
    // backwards -- and did: labwc then parked ~10 s on the seatd socket.
    let order = order_of(&[("labwc", &["seatd"]), ("seatd", &[])]);
    assert!(before(&order, "seatd", "labwc"), "{order:?}");
    assert_eq!(order.len(), 2);
}

#[test]
fn a_whole_dependency_chain_comes_out_in_order() {
    // Named so the alphabetical order is the exact reverse of the required
    // one: nothing but the dependency walk can produce this.
    let order = order_of(&[("c", &["b"]), ("b", &["a"]), ("a", &[]), ("d", &["b", "c"])]);
    assert_eq!(order, vec!["a", "b", "c", "d"]);
}

#[test]
fn the_real_service_graph_orders_seatd_and_dbus_before_the_compositor() {
    let order = order_of(&[
        ("labwc", &["seatd", "gtk-caches", "dbus"]),
        ("seatd", &[]),
        ("dbus", &[]),
        ("gtk-caches", &["dbus"]),
        ("lunarbg", &["labwc"]),
        ("lunarbar", &["labwc"]),
        ("udhcpc", &[]),
        ("ntpd", &["udhcpc"]),
    ]);
    for dep in ["seatd", "gtk-caches", "dbus"] {
        assert!(before(&order, dep, "labwc"), "{dep} tras labwc: {order:?}");
    }
    assert!(before(&order, "dbus", "gtk-caches"), "{order:?}");
    assert!(before(&order, "labwc", "lunarbg"), "{order:?}");
    assert!(before(&order, "labwc", "lunarbar"), "{order:?}");
    assert!(before(&order, "udhcpc", "ntpd"), "{order:?}");
    assert_eq!(order.len(), 8);
}

#[test]
fn a_dependency_cycle_still_boots_everything() {
    // A bad `after =` must never wedge boot: PID 1 has nothing to fall back
    // on. Every service is emitted exactly once, cycle or not.
    let order = order_of(&[("a", &["b"]), ("b", &["a"]), ("c", &[])]);
    assert_eq!(order.len(), 3);
    assert!(order.contains(&"a".to_string()));
    assert!(order.contains(&"b".to_string()));
    // The one service outside the cycle still gets its turn.
    assert!(order.contains(&"c".to_string()));
}

#[test]
fn a_service_that_lists_itself_is_not_a_deadlock() {
    let order = order_of(&[("a", &["a"]), ("b", &[])]);
    assert_eq!(order.len(), 2);
}

#[test]
fn a_dependency_that_is_not_installed_is_not_waited_for() {
    // `desktop =` and `cmdline =` drop services BEFORE the order is
    // computed, so a surviving service can list one that is gone. It must
    // start in its normal turn: the dep will never arrive, so nothing is
    // gained by holding it back. The EXACT order matters, not just that
    // nothing was dropped -- a version that waits for the missing dep still
    // emits everything, via the cycle fallback, only with labwc shoved to
    // the end behind services that were supposed to follow it.
    assert_eq!(
        order_of(&[("labwc", &["xorg"]), ("seatd", &[])]),
        vec!["labwc", "seatd"]
    );
    // And the same with a real dep alongside the missing one: the real one
    // is still honoured.
    assert_eq!(
        order_of(&[("labwc", &["xorg", "seatd"]), ("seatd", &[])]),
        vec!["seatd", "labwc"]
    );
}

#[test]
fn the_order_is_the_same_every_boot() {
    // Two services with no relation between them must not swap places from
    // one boot to the next, or a boot-order bug is unreproducible.
    let defs: &[(&str, &[&str])] = &[("b", &[]), ("a", &[]), ("c", &["a"])];
    let once = order_of(defs);
    for _ in 0..8 {
        assert_eq!(order_of(defs), once);
    }
}

// -- Bounded waits -----------------------------------------------------

#[test]
fn the_poll_pacing_is_fine_grained_only_at_the_start() {
    // seatd binds its socket a few tens of ms after forking, so the first
    // second is polled at 10 ms to release the dependent service almost at
    // once; after that a daemon that never arrives must cost no churn.
    assert_eq!(poll_step(Duration::ZERO), Duration::from_millis(10));
    assert_eq!(
        poll_step(Duration::from_millis(999)),
        Duration::from_millis(10)
    );
    assert_eq!(
        poll_step(Duration::from_secs(1)),
        Duration::from_millis(100)
    );
    assert_eq!(
        poll_step(Duration::from_secs(9)),
        Duration::from_millis(100)
    );
}

#[test]
fn a_wait_that_is_already_satisfied_returns_at_once() {
    let start = Instant::now();
    assert_eq!(
        wait_until(Duration::from_secs(30), || true, || false),
        Wait::Ready
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_shutdown_request_ends_a_wait_instead_of_serving_out_its_timeout() {
    // This is the whole point of installing the handlers WITHOUT SA_RESTART.
    // Boot parks up to 10 s on the seatd socket and 8 s each on /dev/input
    // and its settle, all before the compositor starts: a Ctrl-Alt-Del in
    // there used to go unanswered for the sum of them, because
    // `std::thread::sleep` restarts itself on EINTR.
    let start = Instant::now();
    assert_eq!(
        wait_until(Duration::from_secs(30), || false, || true),
        Wait::Stopped
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_wait_that_is_never_satisfied_reports_a_timeout() {
    let start = Instant::now();
    assert_eq!(
        wait_until(Duration::from_millis(120), || false, || false),
        Wait::TimedOut
    );
    // It really waited, rather than falling straight through.
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn something_that_arrives_during_the_last_sleep_is_not_reported_missing() {
    // A service sent into its backoff over a socket that IS there is a
    // 10-second stall for nothing.
    let calls = std::cell::Cell::new(0);
    let outcome = wait_until(
        Duration::from_millis(40),
        || {
            calls.set(calls.get() + 1);
            // false while the loop runs, true by the final look
            calls.get() > 4
        },
        || false,
    );
    assert_eq!(outcome, Wait::Ready, "{} llamadas", calls.get());
}

#[test]
fn the_settle_wait_holds_on_for_a_device_node_that_arrives_late() {
    // Without udevd there is NO input hotplug: libinput scans /dev/input
    // exactly once, at compositor startup. Waiting for the FIRST node let
    // labwc start between the keyboard (event0) and a slower-enumerating
    // mouse, which then stayed invisible for the whole session. So the
    // settle clock has to RESTART every time the listing changes, not run
    // from the first look.
    let dir = std::env::temp_dir().join(format!("eclipse-settle-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let d = dir.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        fs::write(d.join("event0"), b"teclado").unwrap();
        std::thread::sleep(Duration::from_millis(250));
        fs::write(d.join("event1"), b"raton").unwrap();
    });

    let start = Instant::now();
    wait_for_dir_settled(
        &dir.display().to_string(),
        Duration::from_secs(5),
        Duration::from_millis(300),
    );
    let waited = start.elapsed();
    writer.join().unwrap();

    // The mouse was there when the wait returned: that is the bug, stated
    // as an observation rather than as a duration.
    let entries = fs::read_dir(&dir).unwrap().count();
    assert_eq!(entries, 2, "volvio con {entries} nodos tras {waited:?}");
    // And it returned because it settled, not because it timed out.
    assert!(
        waited < Duration::from_secs(5),
        "agoto el plazo: {waited:?}"
    );
    // The settle really was observed after the LAST change, so the wait
    // cannot be shorter than the last arrival plus the settle.
    assert!(
        waited >= Duration::from_millis(600),
        "volvio pronto: {waited:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_settle_wait_gives_up_on_a_machine_with_no_input_at_all() {
    // Bounded, so a genuinely input-less machine still boots.
    let dir = std::env::temp_dir().join(format!("eclipse-settle-none-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let start = Instant::now();
    wait_for_dir_settled(
        &dir.display().to_string(),
        Duration::from_millis(400),
        Duration::from_millis(100),
    );
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(350),
        "no espero: {waited:?}"
    );
    assert!(waited < Duration::from_secs(3), "no se rindio: {waited:?}");
    let _ = fs::remove_dir_all(&dir);
}

// -- The renderer policy -----------------------------------------------
//
// Three places implement this: here, the `/etc/profile` block and the
// `/usr/local/bin/labwc` wrapper (the last two written by `xtask`, and
// checked against each other there). A session whose compositor renders
// with one renderer while its clients target another composites nothing.

const NVIDIA: Option<&str> = Some("0x10de\n");
const VIRTIO: Option<&str> = Some("0x1af4\n");

fn renderer_env(cmdline: &str, vendor: Option<&str>, degraded: bool) -> Vec<String> {
    let r = renderer_mode_from(cmdline, vendor);
    as_strings(&child_env_for(r, vendor, cmdline, degraded))
}

fn var<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|e| e.strip_prefix(key))
        .map(|v| v.trim_start_matches('='))
}

#[test]
fn an_explicit_renderer_token_beats_the_detected_gpu() {
    // And `renderer=gl` must not swallow `renderer=gl-sw`.
    assert!(matches!(
        renderer_mode_from("renderer=pixman", NVIDIA),
        Renderer::Pixman
    ));
    assert!(matches!(
        renderer_mode_from("renderer=gl-sw", NVIDIA),
        Renderer::GlSw
    ));
    assert!(matches!(
        renderer_mode_from("renderer=gl", VIRTIO),
        Renderer::Gl
    ));
    // Most-specific first: with both spellings present gl-sw wins, which is
    // the safer of the two.
    assert!(matches!(
        renderer_mode_from("renderer=gl:renderer=gl-sw", NVIDIA),
        Renderer::GlSw
    ));
}

#[test]
fn an_nvidia_card_without_the_kernel_flag_is_not_a_gpu_session() {
    // Without `nvidia.nouveau_uapi` the DRM node identifies as "zcore" and
    // NVK enumerates nothing, so auto-detection must pick pixman. Returning
    // Gl here only bought a doomed zink probe.
    assert!(matches!(
        renderer_mode_from("LOG=warn", NVIDIA),
        Renderer::Pixman
    ));
    assert!(matches!(
        renderer_mode_from("nvidia.nouveau_uapi", NVIDIA),
        Renderer::Gl
    ));
}

#[test]
fn autodetection_covers_the_three_machines_this_image_boots_on() {
    // QEMU virtio-gpu / VirtualBox SVGA: pixman. gl-sw (GLES2/llvmpipe)
    // left menu garbage because the kernel presents before the workers
    // finish; opt in with renderer=gl-sw if that stack is wanted.
    assert!(matches!(renderer_mode_from("", VIRTIO), Renderer::Pixman));
    assert!(matches!(
        renderer_mode_from("renderer=auto", Some("0x15ad\n")),
        Renderer::Pixman
    ));
    // No card at all: pixman, which never leaves a black screen.
    assert!(matches!(renderer_mode_from("", None), Renderer::Pixman));
    // A card that reports an empty vendor is not a card.
    assert!(matches!(
        renderer_mode_from("", Some("  \n")),
        Renderer::Pixman
    ));
    // Case-insensitive, because sysfs spelling is not ours to assume -- and
    // it has to hold through the WHOLE policy, not only the detection: the
    // client-side zink pin asks the same question a second time, and the two
    // answers disagreeing is a compositor and its clients on different
    // stacks.
    assert!(matches!(
        renderer_mode_from("nvidia.nouveau_uapi", Some("0X10DE\n")),
        Renderer::Gl
    ));
    let upper = renderer_env(
        "nvidia.nouveau_uapi:nvidia.wlr_gles2",
        Some("0X10DE\n"),
        false,
    );
    let lower = renderer_env("nvidia.nouveau_uapi:nvidia.wlr_gles2", NVIDIA, false);
    assert_eq!(var(&upper, "WLR_RENDERER"), Some("gles2"), "{upper:?}");
    assert_eq!(var(&upper, "GALLIUM_DRIVER"), Some("zink"), "{upper:?}");
    assert_eq!(upper, lower, "la caja del vendor cambia la politica");
}

#[test]
fn the_gpu_session_defaults_to_gles2_and_pins_clients_to_the_same_stack() {
    // zink+NVK is the only GL this uAPI implements, so the compositor and
    // its clients have to be pinned to it together. With nouveau_uapi on
    // NVIDIA, GLES2 is the default; vulkan stays explicit; pixman is the
    // kill-switch.
    let env = renderer_env("renderer=gl:nvidia.wlr_vulkan", NVIDIA, false);
    assert_eq!(var(&env, "WLR_RENDERER"), Some("vulkan"), "{env:?}");
    assert_eq!(var(&env, "GALLIUM_DRIVER"), Some("zink"), "{env:?}");
    assert_eq!(
        var(&env, "MESA_LOADER_DRIVER_OVERRIDE"),
        Some("zink"),
        "{env:?}"
    );

    let env = renderer_env("renderer=gl:nvidia.nouveau_uapi", NVIDIA, false);
    assert_eq!(var(&env, "WLR_RENDERER"), Some("gles2"), "{env:?}");
    assert_eq!(var(&env, "GALLIUM_DRIVER"), Some("zink"), "{env:?}");
    assert_eq!(var(&env, "LIBGL_ALWAYS_SOFTWARE"), None, "{env:?}");

    let env = renderer_env("renderer=gl:nvidia.wlr_gles2", NVIDIA, false);
    assert_eq!(var(&env, "WLR_RENDERER"), Some("gles2"), "{env:?}");
    assert_eq!(var(&env, "GALLIUM_DRIVER"), Some("zink"), "{env:?}");

    // Kill-switch: force the proven software path for the whole boot.
    let env = renderer_env(
        "renderer=gl:nvidia.nouveau_uapi:nvidia.wlr_pixman",
        NVIDIA,
        false,
    );
    assert_eq!(var(&env, "WLR_RENDERER"), Some("pixman"), "{env:?}");
    assert_eq!(var(&env, "GALLIUM_DRIVER"), None, "{env:?}");
    assert_eq!(var(&env, "LIBGL_ALWAYS_SOFTWARE"), Some("1"), "{env:?}");

    // Without nouveau_uapi the DRM node is not nouveau: stay on software.
    let env = renderer_env("renderer=gl", NVIDIA, false);
    assert_eq!(var(&env, "WLR_RENDERER"), Some("pixman"), "{env:?}");
    assert_eq!(var(&env, "GALLIUM_DRIVER"), None, "{env:?}");
    assert_eq!(var(&env, "LIBGL_ALWAYS_SOFTWARE"), Some("1"), "{env:?}");
}

#[test]
fn the_gl_image_on_a_machine_with_no_nvidia_card_pins_software_gl() {
    // This is the `GL=1` image under QEMU: `renderer=gl` on virtio. Leaving
    // the environment unpinned made labwc default to pixman while clients
    // probed Mesa's own defaults, and that mix rendered without ever
    // compositing -- glxgears printed FPS with no window ever appearing.
    // It has to land on the SAME stack as `renderer=gl-sw`.
    let gl = renderer_env("renderer=gl", VIRTIO, false);
    let gl_sw = renderer_env("renderer=gl-sw", VIRTIO, false);
    assert_eq!(var(&gl, "WLR_RENDERER"), Some("gles2"), "{gl:?}");
    assert_eq!(var(&gl, "LIBGL_ALWAYS_SOFTWARE"), Some("1"), "{gl:?}");
    assert_eq!(var(&gl, "WLR_RENDERER_ALLOW_SOFTWARE"), Some("1"), "{gl:?}");
    for key in [
        "WLR_RENDERER",
        "LIBGL_ALWAYS_SOFTWARE",
        "WLR_RENDERER_ALLOW_SOFTWARE",
        "SDL_RENDER_DRIVER",
    ] {
        assert_eq!(var(&gl, key), var(&gl_sw, key), "{key} difiere");
    }
}

#[test]
fn the_compositor_renderer_and_the_client_gl_never_disagree() {
    // The invariant behind all of the above, over every combination this
    // image can boot with: pixman clients must be on software GL, and a
    // hardware-GL compositor must not be handed software-GL clients.
    for cmdline in [
        "",
        "renderer=pixman",
        "renderer=gl",
        "renderer=gl-sw",
        "nvidia.nouveau_uapi",
        "renderer=gl:nvidia.wlr_gles2",
        "renderer=gl:nvidia.wlr_vulkan",
    ] {
        for vendor in [NVIDIA, VIRTIO, None] {
            for degraded in [false, true] {
                let env = renderer_env(cmdline, vendor, degraded);
                let wlr = var(&env, "WLR_RENDERER").unwrap_or("");
                let soft = var(&env, "LIBGL_ALWAYS_SOFTWARE") == Some("1");
                let zink = var(&env, "GALLIUM_DRIVER") == Some("zink");
                let ctx = format!("{cmdline:?} {vendor:?} degraded={degraded}: {env:?}");
                assert!(!wlr.is_empty(), "sin WLR_RENDERER en {ctx}");
                // A hardware-GL compositor and a software-GL client pin
                // cannot both be right.
                assert!(!(zink && soft), "zink y software GL a la vez en {ctx}");
                // The zink pin only ever goes with a GPU renderer.
                if zink {
                    assert!(wlr == "vulkan" || wlr == "gles2", "zink con {wlr} en {ctx}");
                }
                // pixman composites on the CPU, so its clients must not be
                // left probing for hardware GL.
                if wlr == "pixman" {
                    assert!(!zink, "pixman con zink en {ctx}");
                }
            }
        }
    }
}

#[test]
fn a_degraded_compositor_drops_to_pixman_even_though_the_flag_asked_for_gpu() {
    // After COMPOSITOR_DEGRADE_AFTER exits on the GPU renderer the desktop
    // has to come back on pixman, or the machine never reaches a desktop
    // again this boot.
    let asked = "renderer=gl:nvidia.wlr_gles2";
    assert_eq!(
        var(&renderer_env(asked, NVIDIA, false), "WLR_RENDERER"),
        Some("gles2")
    );
    let degraded = renderer_env(asked, NVIDIA, true);
    assert_eq!(
        var(&degraded, "WLR_RENDERER"),
        Some("pixman"),
        "{degraded:?}"
    );
    assert_eq!(var(&degraded, "GALLIUM_DRIVER"), None, "{degraded:?}");
    // And the counter only counts exits when the GPU was actually asked for.
    assert!(gpu_compositor_requested_in(asked));
    assert!(gpu_compositor_requested_in("nvidia.wlr_vulkan"));
    assert!(gpu_compositor_requested_in(
        "renderer=gl:nvidia.nouveau_uapi"
    ));
    assert!(!gpu_compositor_requested_in(
        "renderer=gl:nvidia.nouveau_uapi:nvidia.wlr_pixman"
    ));
}

#[test]
fn every_service_gets_the_runtime_dir_the_wayland_socket_lives_in() {
    // init does not source /etc/profile, so this base set is ALL a service
    // gets. There is deliberately no WAYLAND_DISPLAY: the compositor creates
    // the socket, and a client with none set looks for `wayland-0` inside
    // XDG_RUNTIME_DIR. That is why this directory is not a free choice --
    // the service files wait on /run/user/0/wayland-0 before starting the
    // panel, so a different XDG_RUNTIME_DIR would leave init waiting on a
    // path no client ever uses.
    let env = renderer_env("", None, false);
    assert_eq!(var(&env, "XDG_RUNTIME_DIR"), Some("/run/user/0"), "{env:?}");
    for key in ["PATH", "HOME", "XDG_CONFIG_HOME"] {
        assert!(var(&env, key).is_some(), "falta {key}: {env:?}");
    }
    // A relative PATH entry in PID 1's environment is every child's `.` in
    // its search path.
    let path = var(&env, "PATH").expect("PATH");
    for seg in path.split(':') {
        assert!(seg.starts_with('/'), "PATH relativo {seg:?} en {path:?}");
    }
    // Every entry is a NAME=VALUE pair; a bare name would be dropped by
    // execve on some libcs and inherited on others.
    for e in &env {
        assert!(e.contains('='), "{e:?} no es NAME=VALUE");
        assert!(!e.starts_with('='), "{e:?} no tiene nombre");
    }
}

// -- Helpers -----------------------------------------------------------

fn as_strings(env: &[CString]) -> Vec<String> {
    env.iter()
        .map(|e| e.to_str().expect("utf-8").to_string())
        .collect()
}

/// Firefox on its native Wayland backend for init-started children and
/// their descendants: lunarbar launches apps as ITS children, so this
/// static base is the environment a menu-launched browser sees (labwc's
/// environment file and /etc/profile carry the same pin, checked in
/// xtask). Static, so it must be in CHILD_ENV itself and survive
/// `build_child_env` on every renderer.
/// GTK from a dock terminal: the pixbuf loader registry and the
/// GSettings backend that labwc's environment file already names, in the
/// static base too, so a `firefox` typed into foot decodes images.
#[test]
fn gtk_finds_its_pixbuf_loaders_from_init_started_children() {
    for var in [
        "GDK_PIXBUF_MODULE_FILE=/root/.cache/pixbuf-loaders.cache",
        "GSETTINGS_BACKEND=memory",
    ] {
        assert!(CHILD_ENV.contains(&var), "{var} must be in CHILD_ENV");
        for cmdline in ["LOG=warn", "renderer=gl", "renderer=gl-sw"] {
            let env = renderer_env(cmdline, None, false);
            assert_eq!(
                env.iter().filter(|v| v.as_str() == var).count(),
                1,
                "{cmdline}: {env:?}"
            );
        }
    }
}

// -- Unrunnable `exec =` ------------------------------------------------

/// The bug this exists for: `/usr/local/bin/eclipse-oopslog` shipped 0644,
/// so its service respawned forever on EACCES and the console only ever
/// said "exit 127". The mode must be named, and so must the fix.
#[test]
fn a_non_executable_exec_is_named_with_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("eclipse-init-exec-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let script = dir.join("eclipse-oopslog");
    fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    let msg = exec_problem(script.to_str().unwrap()).expect("0644 must be reported");
    assert!(msg.contains("not executable"), "{msg}");
    assert!(
        msg.contains("0644"),
        "the mode itself has to be in the line: {msg}"
    );

    // And the same file, once executable, is reported as fine.
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(exec_problem(script.to_str().unwrap()), None);

    // Group- or other-only x still execs for those users: not our call.
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644 | 0o010)).unwrap();
    assert_eq!(exec_problem(script.to_str().unwrap()), None);

    let _ = fs::remove_dir_all(&dir);
}

/// An image already installed on a disk keeps the 0644 wrapper forever: a
/// new kernel does not rewrite `/usr/local/bin`, only the installer does.
/// So init repairs the mode itself rather than only naming it.
#[test]
fn a_non_executable_exec_is_chmoded_so_an_installed_disk_heals_itself() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("eclipse-init-heal-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Exactly what the image shipped.
    let script = dir.join("eclipse-oopslog");
    fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();

    let msg = repair_exec_mode(script.to_str().unwrap()).expect("0644 must be repaired");
    assert!(msg.contains("0644"), "{msg}");
    assert!(msg.contains("0755"), "{msg}");
    let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o755,
        "chmod +x on a 0644 wrapper is 0755, got {mode:04o}"
    );
    // And `exec_problem` now has nothing to say about it, so the service runs.
    assert_eq!(exec_problem(script.to_str().unwrap()), None);

    // Idempotent: an already-executable file is left alone, mode untouched.
    assert_eq!(repair_exec_mode(script.to_str().unwrap()), None);
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o7777,
        0o755
    );

    // A root-only 0600 file gains only owner-x, and the setuid bit of a
    // 04644 file survives the repair -- `chmod +x`, not `chmod 755`.
    let private = dir.join("private");
    fs::write(&private, b"x").unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(repair_exec_mode(private.to_str().unwrap()).is_some());
    assert_eq!(
        fs::metadata(&private).unwrap().permissions().mode() & 0o7777,
        0o700
    );

    let suid = dir.join("suid");
    fs::write(&suid, b"x").unwrap();
    fs::set_permissions(&suid, fs::Permissions::from_mode(0o4644)).unwrap();
    assert!(repair_exec_mode(suid.to_str().unwrap()).is_some());
    assert_eq!(
        fs::metadata(&suid).unwrap().permissions().mode() & 0o7777,
        0o4755
    );

    // Nothing to repair for a directory or a path that is not there.
    assert_eq!(repair_exec_mode(dir.to_str().unwrap()), None);
    assert_eq!(repair_exec_mode(dir.join("absent").to_str().unwrap()), None);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_or_directory_exec_is_named_and_a_relative_one_is_not_judged() {
    let dir = std::env::temp_dir().join(format!("eclipse-init-exec2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let missing = dir.join("no-such-binary");
    let msg = exec_problem(missing.to_str().unwrap()).expect("a missing exec is reportable");
    assert!(msg.contains("does not exist"), "{msg}");

    let msg = exec_problem(dir.to_str().unwrap()).expect("a directory is reportable");
    assert!(msg.contains("is a directory"), "{msg}");

    // A bare name is resolved by execvp's PATH search, which this cannot
    // replicate: staying silent beats guessing wrong.
    assert_eq!(exec_problem("seatd"), None);
    assert_eq!(exec_problem("no-such-binary-anywhere"), None);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn firefox_is_pinned_to_native_wayland_for_init_started_children() {
    assert!(
        CHILD_ENV.contains(&"MOZ_ENABLE_WAYLAND=1"),
        "MOZ_ENABLE_WAYLAND=1 must be in the static child environment"
    );
    for cmdline in ["LOG=warn", "renderer=gl", "renderer=gl-sw"] {
        let env = renderer_env(cmdline, None, false);
        assert!(
            env.iter().any(|v| v == "MOZ_ENABLE_WAYLAND=1"),
            "{cmdline}: {env:?}"
        );
    }
}
// ── signals: what each one asks PID 1 for ────────────────────────────────

/// A scratch directory of this test's own, cleared first.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("eclipse-init-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every signal PID 1 answers asks for what busybox means by it, and the
/// handler wired to it sets the matching flag.
///
/// Nothing else in the system can tell: a swapped row shows up as the
/// machine powering off when the user asked it to reboot, and it DID --
/// SIGTERM used to request a HALT, so `/bin/reboot`, `busybox reboot` and
/// every script using the absolute path powered the box off instead.
#[test]
fn every_signal_asks_for_what_busybox_sends_it_for() {
    // busybox halt.c sends halt/poweroff/reboot as SIGUSR1/SIGUSR2/
    // SIGTERM; the kernel delivers Ctrl-Alt-Del as SIGINT. The table is
    // hard-coded here on purpose: this is the other side of it.
    let want: &[(libc::c_int, Request, &str)] = &[
        (libc::SIGTERM, Request::Reboot, "busybox reboot"),
        (libc::SIGINT, Request::Reboot, "Ctrl-Alt-Del"),
        (libc::SIGUSR1, Request::Halt, "busybox halt"),
        (libc::SIGUSR2, Request::Halt, "busybox poweroff"),
    ];
    assert_eq!(
        SIGNAL_HANDLERS.len(),
        want.len(),
        "the table no longer covers the four signals PID 1 must answer"
    );

    let held = (
        WANT_HALT.load(Ordering::SeqCst),
        WANT_REBOOT.load(Ordering::SeqCst),
    );
    for (sig, req, sender) in want {
        let row = SIGNAL_HANDLERS
            .iter()
            .find(|(s, _, _)| s == sig)
            .unwrap_or_else(|| panic!("no row for the signal {sender} sends"));
        assert_eq!(row.2, *req, "{sender} asks for the wrong thing");
        // And the handler on that row really does what the row promises.
        WANT_HALT.store(false, Ordering::SeqCst);
        WANT_REBOOT.store(false, Ordering::SeqCst);
        (row.1)(*sig);
        let got = match (
            WANT_HALT.load(Ordering::SeqCst),
            WANT_REBOOT.load(Ordering::SeqCst),
        ) {
            (true, false) => Some(Request::Halt),
            (false, true) => Some(Request::Reboot),
            _ => None,
        };
        assert_eq!(
            got,
            Some(*req),
            "the handler for {sender} set the wrong flag, or both, or neither"
        );
    }
    WANT_HALT.store(held.0, Ordering::SeqCst);
    WANT_REBOOT.store(held.1, Ordering::SeqCst);
}

/// Either flag alone ends a bounded wait. Both are needed together only
/// to decide WHICH way the machine goes down, never whether it does.
#[test]
fn either_flag_alone_stops_a_bounded_wait() {
    let held = (
        WANT_HALT.load(Ordering::SeqCst),
        WANT_REBOOT.load(Ordering::SeqCst),
    );
    for (halt, reboot, want) in [
        (false, false, false),
        (true, false, true),
        (false, true, true),
        (true, true, true),
    ] {
        WANT_HALT.store(halt, Ordering::SeqCst);
        WANT_REBOOT.store(reboot, Ordering::SeqCst);
        assert_eq!(
            shutdown_requested(),
            want,
            "halt={halt} reboot={reboot} read as {}",
            shutdown_requested()
        );
    }
    WANT_HALT.store(held.0, Ordering::SeqCst);
    WANT_REBOOT.store(held.1, Ordering::SeqCst);
}

// ── the stale-runtime sweep ──────────────────────────────────────────────

/// The sweep of `/run` and `/tmp` removes what the previous boot left and
/// keeps the ONE entry it is told to keep.
///
/// Both halves matter and neither had a test. The removal is what stops a
/// stale `wayland-0` or `seatd.sock` passing a readiness check before the
/// daemon is listening. What it keeps is `/run/udev`: the kernel writes a
/// synthetic udev database there so libinput treats `/dev/input/event*` as
/// initialized without a udevd, and wiping it left the compositor running
/// with no keyboard and no mouse for the whole session.
#[test]
fn the_runtime_sweep_keeps_the_one_entry_it_is_told_to_keep() {
    let dir = scratch("sweep");
    fs::write(dir.join("wayland-0"), b"stale").unwrap();
    fs::create_dir_all(dir.join("udev/data")).unwrap();
    fs::write(dir.join("udev/data/c13:64"), b"E:ID_INPUT=1").unwrap();
    fs::create_dir_all(dir.join("pulse")).unwrap();
    fs::write(dir.join("pulse/pid"), b"123").unwrap();
    std::os::unix::fs::symlink("/nonexistent", dir.join("dangling")).unwrap();

    clean_runtime_dir_keeping(&dir, Some("udev"));

    let left: Vec<String> = {
        let mut n: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    };
    assert_eq!(left, vec!["udev".to_string()], "left behind: {left:?}");
    // Kept whole, not emptied: the database inside it is the point.
    assert!(
        dir.join("udev/data/c13:64").exists(),
        "the udev database was emptied"
    );

    // With nothing to keep, nothing is kept -- which is what `/tmp` gets.
    clean_runtime_dir_keeping(&dir, None);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "udev survived /tmp");
    let _ = fs::remove_dir_all(&dir);

    // And the rule that picks between the two. `/run/udev` holds the
    // device database libinput reads: wiping it once left the session with
    // no keyboard and no mouse until the next boot, so the one tree that
    // keeps something is `/run`, and `/tmp` keeps nothing.
    assert_eq!(keep_under(Path::new("/run")), Some("udev"));
    assert_eq!(keep_under(Path::new("/tmp")), None);
    assert_eq!(keep_under(Path::new("/run/user/0")), None);
    for d in RUNTIME_DIRS {
        let swept = Path::new(d);
        assert_eq!(
            keep_under(swept) == Some("udev"),
            swept == Path::new("/run"),
            "{d} keeps the wrong thing"
        );
    }
}

/// A dangling symlink is removed as an entry and never followed: the
/// sweep must not reach outside the tree it was pointed at.
#[test]
fn the_sweep_removes_a_symlink_without_following_it() {
    let dir = scratch("link");
    let outside = scratch("link-target");
    fs::write(outside.join("keep-me"), b"not yours").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("elsewhere")).unwrap();

    clean_runtime_dir_keeping(&dir, None);

    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "the link stayed");
    assert!(
        outside.join("keep-me").exists(),
        "the sweep followed the link and emptied the target"
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&outside);
}

// ── the socket gate ──────────────────────────────────────────────────────

/// `wait_socket =` is the gate that stops labwc racing seatd, and it asks
/// exactly one question: is there a UNIX SOCKET at that path? A regular
/// file, a directory or nothing at all are all "not yet" -- a stale
/// regular file passing for a socket is how a client connects to a
/// daemon that is not listening.
#[test]
fn only_a_unix_socket_satisfies_the_socket_gate() {
    let dir = scratch("sock");
    let sock = dir.join("seatd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    fs::write(dir.join("plain"), b"not a socket").unwrap();
    fs::create_dir_all(dir.join("adir")).unwrap();

    assert!(is_unix_socket(&sock.display().to_string()), "the socket");
    assert!(
        !is_unix_socket(&dir.join("plain").display().to_string()),
        "a regular file passed for a socket"
    );
    assert!(
        !is_unix_socket(&dir.join("adir").display().to_string()),
        "a directory passed for a socket"
    );
    assert!(
        !is_unix_socket(&dir.join("absent").display().to_string()),
        "a path that is not there passed for a socket"
    );
    // A path with an interior NUL cannot be asked about, so it is not one.
    assert!(!is_unix_socket("/run/se\0atd.sock"), "a NUL in the path");

    drop(listener);
    let _ = fs::remove_dir_all(&dir);
}

// ── the tables PID 1 boots from ──────────────────────────────────────────

/// The pseudo-filesystems, each mounted where its own name says. The
/// function that uses this MOUNTS, so no test can call it: the table is
/// the only part of the decision a test can reach, and a missing row is a
/// kernel interface the whole session then does without.
#[test]
fn the_pseudo_filesystems_cover_the_four_the_session_needs() {
    let at = |target: &str| {
        PSEUDO_MOUNTS
            .iter()
            .find(|(_, t, _)| *t == target)
            .unwrap_or_else(|| panic!("nothing is mounted on {target}"))
    };
    assert_eq!(at("/proc").2, "proc", "/proc is not procfs");
    assert_eq!(at("/sys").2, "sysfs", "/sys is not sysfs");
    assert_eq!(at("/dev").2, "devtmpfs", "/dev is not devtmpfs");
    assert_eq!(at("/run").2, "tmpfs", "/run is not a tmpfs");
    assert_eq!(at("/tmp").2, "tmpfs", "/tmp is not a tmpfs");
    // No target twice: the second mount of a path shadows the first.
    for (i, (_, t, _)) in PSEUDO_MOUNTS.iter().enumerate() {
        assert!(
            !PSEUDO_MOUNTS[i + 1..].iter().any(|(_, u, _)| u == t),
            "{t} is mounted twice"
        );
    }
}

/// Both runtime trees are swept, and the session directory is the owner's
/// alone. `/run` AND `/tmp`: on an installed root the kernel treats the
/// tmpfs mounts as no-ops, so both are btrfs directories that SURVIVE a
/// reboot, and a stale socket in either passes a readiness check.
#[test]
fn both_runtime_trees_are_swept_and_the_session_dir_is_private() {
    assert!(
        RUNTIME_DIRS.contains(&"/run") && RUNTIME_DIRS.contains(&"/tmp"),
        "a tree that survives a reboot is not swept: {RUNTIME_DIRS:?}"
    );
    // XDG_RUNTIME_DIR: reachable by its owner and by nobody else, which is
    // what the specification requires of it and what the Wayland socket
    // inside it relies on.
    assert_eq!(
        XDG_RUNTIME_MODE & 0o077,
        0,
        "the session directory is readable off-owner: {XDG_RUNTIME_MODE:04o}"
    );
    assert_eq!(
        XDG_RUNTIME_MODE & 0o700,
        0o700,
        "its owner cannot reach it: {XDG_RUNTIME_MODE:04o}"
    );
}

/// The session lands on a VT clear of the text consoles. On one of them
/// the compositor and a getty fight over the same screen, and the boot
/// messages scroll over the desktop.
#[test]
fn the_session_lands_on_a_vt_clear_of_the_text_consoles() {
    // The relation is a `const` assertion in production (it is decidable
    // at compile time); what is left for a test is the two numbers, which
    // have to agree with the kernel's own VT layout.
    assert_eq!(TEXT_VTS, 6, "the kernel no longer opens six text consoles");
    assert_eq!(GRAPHICS_VT, 7, "the session is not on tty7");
}

/// The marker lives under a tree a boot wipes, so it lasts exactly one
/// boot. `/tmp` would not do: on an installed root it is a btrfs
/// directory that survives, so a machine that degraded once would come
/// up on software rendering for ever after.
#[test]
fn the_renderer_fallback_marker_lasts_exactly_one_boot() {
    assert!(
        RUNTIME_DIRS
            .iter()
            .any(|d| RENDERER_FALLBACK_MARKER.starts_with(&format!("{d}/"))),
        "{RENDERER_FALLBACK_MARKER} is not under a tree the boot sweeps"
    );
    assert!(
        RENDERER_FALLBACK_MARKER.starts_with("/run/"),
        "a marker outside /run can outlive its boot: {RENDERER_FALLBACK_MARKER}"
    );
}

// ── the respawn policy ───────────────────────────────────────────────────

/// A service that did a unit of work restarts at once with its backoff
/// reset; one that died on start waits, and the wait doubles to a ceiling.
///
/// This policy lived inside `supervise`, which blocks in `waitpid` for the
/// life of the machine, so none of it was reachable from a test -- and it
/// is the difference between a missing binary costing one log line every
/// eight seconds and it pinning a CPU for the whole boot.
#[test]
fn a_crashing_service_backs_off_and_a_working_one_does_not() {
    // Healthy: no wait, and the next crash starts from the minimum again.
    let (wait, next) = restart_delay(HEALTHY_UPTIME, MAX_BACKOFF);
    assert_eq!(wait, Duration::ZERO, "a healthy exit waited");
    assert_eq!(next, MIN_BACKOFF, "a healthy exit kept its backoff");

    // A crash: wait what we had, then double.
    let (wait, next) = restart_delay(Duration::ZERO, MIN_BACKOFF);
    assert_eq!(wait, MIN_BACKOFF, "the first retry did not wait");
    assert_eq!(next, MIN_BACKOFF * 2, "the backoff did not grow");

    // Doubling stops at the ceiling, and the walk up to it is short
    // enough that a service which starts working comes back quickly.
    let mut b = MIN_BACKOFF;
    let mut steps = 0;
    while b < MAX_BACKOFF {
        let (wait, next) = restart_delay(Duration::ZERO, b);
        assert_eq!(wait, b, "the retry waited something other than its backoff");
        assert!(next > b, "the backoff stopped growing at {b:?}");
        b = next;
        steps += 1;
        assert!(
            steps < 20,
            "the backoff takes {steps} crashes to reach its ceiling"
        );
    }
    assert_eq!(b, MAX_BACKOFF, "the ceiling is not a power of two away");
    assert_eq!(
        restart_delay(Duration::ZERO, MAX_BACKOFF),
        (MAX_BACKOFF, MAX_BACKOFF),
        "the backoff grew past its ceiling"
    );
    // And the ceiling is a wait a person would sit through.
    assert!(MAX_BACKOFF <= Duration::from_secs(30), "{MAX_BACKOFF:?}");
    assert!(
        MIN_BACKOFF < HEALTHY_UPTIME,
        "the first retry outlasts health"
    );
    // The floor is long enough that a service failing to exec cannot spin
    // PID 1: at a millisecond it would be retried a thousand times a
    // second, with a log line each time.
    assert!(
        MIN_BACKOFF >= Duration::from_millis(100),
        "a crash loop would spin PID 1 at {MIN_BACKOFF:?}"
    );
    // And the health window sits between the two: longer than the first
    // retry (or every retry would look healthy) and shorter than the
    // ceiling (or a service could never be asked to run long enough to
    // clear a backoff it is already being held back by).
    assert!(
        HEALTHY_UPTIME < MAX_BACKOFF,
        "a service must run {HEALTHY_UPTIME:?} to clear a {MAX_BACKOFF:?} wait"
    );

    // A freshly parsed service starts at the minimum, so its FIRST crash
    // is retried promptly rather than after the ceiling.
    let svc = parse_service("labwc", "exec = /usr/local/bin/labwc\ntype = respawn\n").unwrap();
    assert_eq!(
        restart_delay(Duration::ZERO, svc.backoff).0,
        MIN_BACKOFF,
        "a service's first crash waited {:?}",
        svc.backoff
    );
}

/// The compositor is dropped to software rendering only after its
/// tolerance runs out, and only when it is the compositor, only while a
/// GPU renderer was asked for, and only once.
#[test]
fn the_compositor_degrades_only_when_its_tolerance_runs_out() {
    // Counted from the first exit, and the tolerance is reached, not
    // passed, before degrading.
    for n in 0..COMPOSITOR_DEGRADE_AFTER - 1 {
        let (count, out) = compositor_exit("labwc", false, true, || n).unwrap();
        assert_eq!(count, n + 1, "the exit was not counted");
        assert!(
            !out,
            "degraded on exit {count} of {COMPOSITOR_DEGRADE_AFTER}"
        );
    }
    let (count, out) = compositor_exit("labwc", false, true, || COMPOSITOR_DEGRADE_AFTER - 1)
        .expect("the last exit of the tolerance was not counted");
    assert_eq!(count, COMPOSITOR_DEGRADE_AFTER);
    assert!(out, "the tolerance ran out and it did not degrade");

    // Not counted at all when it is not the compositor, when no GPU
    // renderer was asked for, or when it has already degraded -- the last
    // one is what stops the count climbing for the rest of the boot.
    // The counter must not even be READ in those cases: it is a global
    // `fetch_add`, so consulting it is what spends the tolerance.
    let asked = std::cell::Cell::new(0u32);
    let count = || {
        asked.set(asked.get() + 1);
        9
    };
    assert_eq!(
        compositor_exit("seatd", false, true, count),
        None,
        "seatd counted"
    );
    assert_eq!(
        compositor_exit("labwc", false, false, count),
        None,
        "pixman counted"
    );
    assert_eq!(
        compositor_exit("labwc", true, true, count),
        None,
        "counted twice"
    );
    assert_eq!(
        asked.get(),
        0,
        "an exit that does not count spent the tolerance"
    );

    // And a tolerance nobody would wait out is not a tolerance.
    assert!(
        (1..=3).contains(&COMPOSITOR_DEGRADE_AFTER),
        "the desktop dies {COMPOSITOR_DEGRADE_AFTER} times before it recovers"
    );
}

// ── exec ─────────────────────────────────────────────────────────────────

/// An argv with an interior NUL is refused WHOLE, and what does go to
/// `execvp` is NULL-terminated.
///
/// Both are the kind of thing only a test sees: a truncated argv is a
/// different command, run with no sign that anything was dropped, and a
/// pointer array with no terminator sends `execvp` off the end of the
/// allocation.
#[test]
fn an_argv_is_passed_whole_and_null_terminated_or_not_at_all() {
    let argv: Vec<String> = ["/usr/local/bin/labwc", "-C", "/root/.config/labwc"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let c_args = argv_for_exec(&argv).expect("a clean argv was refused");
    assert_eq!(c_args.len(), argv.len(), "an argument was dropped");
    for (c, s) in c_args.iter().zip(&argv) {
        assert_eq!(c.to_str().unwrap(), s, "an argument changed");
    }

    let ptrs = exec_ptrs(&c_args);
    assert_eq!(ptrs.len(), c_args.len() + 1, "no room for the terminator");
    assert!(ptrs[ptrs.len() - 1].is_null(), "execvp gets no terminator");
    assert!(
        ptrs[..c_args.len()].iter().all(|p| !p.is_null()),
        "an argument came through as NULL, which ends the argv early"
    );

    // A NUL anywhere in it, and nothing is run.
    let dirty: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "echo\0rm -rf /".into()];
    assert!(
        argv_for_exec(&dirty).is_none(),
        "an argv with an interior NUL was accepted, truncated"
    );
    assert!(argv_for_exec(&[]).is_none(), "an empty argv was accepted");
}

// ── the renderer environment ─────────────────────────────────────────────

/// SDL's renderer follows the compositor's one-to-one, and the two modes
/// are different environments. The same three copies of this policy live
/// in the labwc wrapper and /etc/profile, so a swap here means a
/// shell-launched SDL app and an init-launched one render differently.
#[test]
fn the_sdl_render_path_follows_the_compositor_renderer() {
    let env_for = |mode| {
        let mut env: Vec<CString> = Vec::new();
        push_sdl_render_env(&mut env, mode);
        env.iter()
            .map(|e| e.to_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let soft = env_for(SdlRender::Software);
    let gl = env_for(SdlRender::Gles2);
    assert_ne!(soft, gl, "both SDL modes set the same environment");
    // A CPU session must not advertise an accelerated framebuffer: SDL
    // would hand the app a surface with no GL behind it.
    assert!(
        soft.contains(&"SDL_RENDER_DRIVER=software".to_string())
            && soft.contains(&"SDL_FRAMEBUFFER_ACCELERATION=0".to_string()),
        "the pixman session does not pin SDL to the CPU: {soft:?}"
    );
    assert!(
        gl.contains(&"SDL_RENDER_DRIVER=opengles2".to_string())
            && gl.contains(&"SDL_FRAMEBUFFER_ACCELERATION=opengles2".to_string()),
        "the GL session does not pin SDL to GLES2: {gl:?}"
    );
}

/// A pixman session says so, and says software is allowed. wlroots
/// REFUSES a software renderer without that second variable, so a
/// compositor told to use pixman and not told it may would not start.
#[test]
fn a_pixman_session_names_pixman_and_allows_software() {
    let env = child_env_for(Renderer::Pixman, None, "", false);
    let have: Vec<String> = env
        .iter()
        .map(|e| e.to_str().unwrap().to_string())
        .collect();
    assert!(
        have.contains(&"WLR_RENDERER=pixman".to_string()),
        "a pixman session does not name pixman: {have:?}"
    );
    assert!(
        have.contains(&"WLR_RENDERER_ALLOW_SOFTWARE=1".to_string()),
        "wlroots will refuse the software renderer it was given"
    );
}

// ── the settle wait ──────────────────────────────────────────────────────

/// A shutdown ends the settle wait instead of serving out its timeout.
/// This is the longest wait on the boot path, so a Ctrl-Alt-Del during it
/// is the one most likely to look ignored.
#[test]
fn a_shutdown_ends_the_settle_wait_instead_of_serving_it_out() {
    let dir = scratch("settle-stop");
    let start = Instant::now();
    wait_for_dir_settled_until(
        &dir.display().to_string(),
        Duration::from_secs(30),
        Duration::from_secs(10),
        || true,
    );
    let waited = start.elapsed();
    assert!(
        waited < Duration::from_secs(1),
        "it served out {waited:?} of a thirty second wait"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Something that is already there, on a wait that is also being stopped,
/// is reported as HAVING ARRIVED. The other way round the caller logs a
/// warning about a socket that is sitting right there, and sends the
/// service into its backoff for nothing.
#[test]
fn a_wait_both_satisfied_and_stopped_reports_what_arrived() {
    assert_eq!(
        wait_until(Duration::from_secs(5), || true, || true),
        Wait::Ready,
        "a wait reported a shutdown for something that had arrived"
    );
    assert_eq!(
        wait_until(Duration::from_secs(5), || false, || true),
        Wait::Stopped
    );
}

// ── the rest of the service file, and the chmod ──────────────────────────

/// Every key a service file may carry lands in its own field, and a key
/// that is not one of them changes nothing. A misspelling is a gate that
/// does not exist: `wait_sockt = /run/seatd.sock` was accepted in silence,
/// and labwc then raced seatd on every boot.
#[test]
fn every_key_a_service_file_carries_lands_in_its_own_field() {
    let text = "\
exec = /usr/local/bin/labwc -C /root/.config/labwc
type = respawn
after = seatd dbus
desktop = labwc
cmdline = dbus.selftest
log = /tmp/labwc.log
wait_socket = /run/seatd.sock
wait_path = /dev/input/event0
";
    let svc = parse_service("labwc", text).expect("the service was refused");
    assert_eq!(svc.name, "labwc");
    assert_eq!(
        svc.exec,
        ["/usr/local/bin/labwc", "-C", "/root/.config/labwc"]
    );
    assert_eq!(svc.kind, Kind::Respawn);
    assert_eq!(svc.after, ["seatd", "dbus"]);
    assert_eq!(svc.desktop.as_deref(), Some("labwc"));
    assert_eq!(
        svc.cmdline.as_deref(),
        Some("dbus.selftest"),
        "the cmdline gate"
    );
    assert_eq!(svc.log.as_deref(), Some("/tmp/labwc.log"));
    assert_eq!(svc.wait_socket.as_deref(), Some("/run/seatd.sock"));
    assert_eq!(svc.wait_path.as_deref(), Some("/dev/input/event0"));

    // A misspelled key sets nothing at all -- not the field it nearly
    // names, and not any other.
    let typo = parse_service("labwc", "exec = /bin/true\nwait_sockt = /run/seatd.sock\n")
        .expect("the service was refused");
    assert_eq!(typo.wait_socket, None, "a misspelled key set the real one");
    assert_eq!(typo.wait_path, None);
    assert_eq!(typo.cmdline, None);
    assert_eq!(typo.desktop, None);
    assert_eq!(typo.log, None);
}

/// A file with no read bit at all still gets its owner's x bit. The
/// `chmod +x` this mirrors adds x wherever the file is readable, which
/// for a 0644 wrapper means 0755 -- but for a 0200 one it would mean
/// adding nothing, and the service would still not start.
#[test]
fn an_exec_with_no_read_bit_still_gets_its_owner_x_bit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("chmod");
    for (before, after) in [(0o644u32, 0o755u32), (0o600, 0o700), (0o200, 0o300)] {
        let prog = dir.join(format!("w{before:04o}"));
        fs::write(&prog, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&prog, fs::Permissions::from_mode(before)).unwrap();
        let said = repair_exec_mode(&prog.display().to_string());
        assert!(said.is_some(), "{before:04o} was left alone");
        let now = fs::metadata(&prog).unwrap().permissions().mode() & 0o7777;
        assert_eq!(now, after, "{before:04o} became {now:04o}");
        assert_ne!(now & 0o111, 0, "{before:04o} is still not executable");
        // And a second pass leaves it be: it is already executable.
        assert_eq!(
            repair_exec_mode(&prog.display().to_string()),
            None,
            "an executable file was chmoded again"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A symlinked `exec =` is repaired through the link. The wrappers under
/// /usr/local/bin can be links, and it is the file at the end of one that
/// `execve` checks the mode of.
#[test]
fn a_symlinked_exec_is_repaired_through_the_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("chmod-link");
    let real = dir.join("eclipse-oopslog");
    fs::write(&real, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
    let link = dir.join("oopslog");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    assert!(
        repair_exec_mode(&link.display().to_string()).is_some(),
        "a link to a non-executable file was left alone"
    );
    let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "the target is {mode:04o}");
    let _ = fs::remove_dir_all(&dir);
}

/// Every spelling of the two languages this image ships. The locale file
/// is written by `eclipse-locale` and by hand, so `es_ES` and `ES` reach
/// here as readily as `es` does -- and a spelling that falls through
/// silently leaves a user who asked for English with a Spanish desktop.
#[test]
fn every_spelling_of_the_two_languages_is_accepted() {
    for (spelt, want) in [
        ("en", "en"),
        ("EN", "en"),
        ("en_US", "en"),
        ("es", "es"),
        ("ES", "es"),
        ("es_ES", "es"),
    ] {
        assert_eq!(ui_lang_token(spelt), Some(want), "lang={spelt}");
        assert_eq!(
            ui_lang_from(&format!("LOG=warn:lang={spelt}"), None),
            want,
            "lang={spelt} on the cmdline"
        );
        assert_eq!(
            ui_lang_from("", Some(&format!("lang={spelt}\n"))),
            want,
            "lang={spelt} in the file"
        );
    }
    // And nothing else is a language, at either source.
    for other in ["fr", "en_GB", "espanol", "e", ""] {
        assert_eq!(ui_lang_token(other), None, "lang={other} was accepted");
    }
}

// -- an `exec =` that is not there -------------------------------------

/// A service file naming a program that does not exist is not a crash
/// loop: it is a thing that cannot start, ever. Init tries it
/// [`MISSING_EXEC_TRIES`] times and then stops, because nothing creates
/// that file between two `execve`s.
///
/// The console this came from was `dbus-system` on real hardware:
/// `/usr/local/bin/eclipse-dbus-system does not exist`, four lines every
/// 8 s, for the whole boot, with everything else scrolled off the screen.
#[test]
fn a_service_whose_program_is_missing_is_given_up_on_instead_of_retried_for_ever() {
    let mut svc = parse_service(
        "dbus-system",
        "exec = /usr/local/bin/eclipse-dbus-system-that-is-not-there\ntype = respawn\n",
    )
    .expect("parsea");
    for try_n in 1..MISSING_EXEC_TRIES {
        assert!(
            !note_missing_exec(&mut svc),
            "given up on try {try_n}, before the tries ran out"
        );
        assert!(!svc.given_up);
    }
    assert!(note_missing_exec(&mut svc), "it is still being retried");
    assert!(svc.given_up);
}

/// Given up means out of the restart pass as well: leaving it `due` would
/// put the same storm back through a different door.
#[test]
fn a_service_given_up_on_is_not_due_to_restart() {
    let ghost =
        || parse_service("ghost", "exec = /bin/no-such-program\ntype = respawn\n").expect("parsea");
    let mut map = BTreeMap::new();
    map.insert(String::from("ghost"), ghost());
    assert_eq!(due_names(&map, Instant::now()), vec![String::from("ghost")]);
    let mut svc = ghost();
    svc.given_up = true;
    map.insert(String::from("ghost"), svc);
    assert!(due_names(&map, Instant::now()).is_empty());
}

/// Only an ABSOLUTE path with nothing behind it counts: a bare `exec =
/// seatd` is resolved by `execvp` against PATH, which this cannot
/// replicate, and calling that one missing would disable a service that
/// works.
#[test]
fn only_an_absolute_path_with_no_file_behind_it_counts_as_missing() {
    assert!(exec_is_missing("/usr/local/bin/eclipse-no-such-wrapper"));
    assert!(!exec_is_missing("/bin"));
    assert!(!exec_is_missing("seatd"));
    assert!(!exec_is_missing("eclipse-no-such-wrapper"));
}

/// A program that turns up resets the count: a path that only appears once
/// an earlier service has mounted its filesystem must not spend its tries.
#[test]
fn a_program_that_turns_up_resets_the_count() {
    let mut svc = parse_service("late", "exec = /bin/sh\ntype = respawn\n").expect("parsea");
    svc.missing_starts = MISSING_EXEC_TRIES - 1;
    assert!(!note_missing_exec(&mut svc));
    assert_eq!(svc.missing_starts, 0);
    assert!(!svc.given_up);
}

/// The renderer policy is recomputed for every `execve`, so its lines used
/// to be reprinted once per service start -- and a service respawning
/// every 8 s reprinted the whole block every 8 s, which is what buried the
/// line that said what was wrong. Each distinct line is said once.
#[test]
fn a_renderer_line_is_said_once_and_a_different_one_is_still_said() {
    let line = "renderer=test: first time only";
    assert!(log_renderer(line), "the first time has to be said");
    assert!(!log_renderer(line), "the same line was said twice");
    assert!(
        log_renderer("renderer=test: a different decision"),
        "a decision that changed was swallowed"
    );
}
