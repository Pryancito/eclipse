//! Los tests de `/var/run`, el `env` de `/usr/bin` y lo que cuelga de ellos.
//!
//! Fuera de `mod.rs` por el techo de lineas: el fichero llego a 6062 con los
//! 6000 de su `PASADOS`, y lo que prescribe el trinquete es sacar un
//! `#[cfg(test)] mod` de primer nivel a su propio fichero. Ni una linea de
//! produccion se mueve, y `use super::*` sigue significando lo mismo porque
//! este modulo sigue siendo hijo de `linux`.

use super::*;

/// `#!/usr/bin/env <cmd>` is the most common shebang in existence, and
/// Eclipse shipped with no `/usr/bin/env` -- only `/bin/env` -- so every
/// script using it failed to exec. The link must be relative (the rootfs is
/// assembled on the host and mounted at a different root in the guest) and
/// must point at busybox, which dispatches on argv[0].
#[test]
fn usr_bin_env_is_linked_to_busybox() {
    let dir = std::env::temp_dir().join(format!("eclipse-usrbinenv-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    LinuxRootfs::ensure_busybox_applets(&bin);

    let link = dir.join("usr/bin/env");
    assert!(
        link.is_symlink(),
        "no /usr/bin/env: every `#!/usr/bin/env ...` script fails to exec"
    );
    let target = fs::read_link(&link).unwrap();
    assert!(
        target.is_relative(),
        "/usr/bin/env must not point at a host-absolute path: {target:?}"
    );
    // Resolved from /usr/bin, the target must name the rootfs's busybox.
    assert_eq!(
        dir.join("usr/bin").join(&target),
        dir.join("usr/bin/../../bin/busybox"),
        "the link must resolve to /bin/busybox inside the rootfs, not {target:?}"
    );
    // And the applet it shadows in /bin is still there.
    assert!(bin.join("env").is_symlink());
    let _ = fs::remove_dir_all(&dir);
}

/// `/usr/local/bin/eclipse-oopslog` shipped **0644** from the day it was
/// added: the script was written next to the other wrappers but never added
/// to the `chmod 0755` list beside them. `oopslog.service` is
/// `type = respawn`, so on every boot eclipse-init forked, `execve` failed
/// with EACCES, the child `_exit(127)`ed in well under a millisecond, and
/// the supervisor respawned it -- backing off to MAX_BACKOFF and then
/// printing an `exit 127` line every 8 s for the rest of the boot. That is
/// the "the service resets over and over" report from real hardware.
///
/// The x bit is the whole fix, so it is what this asserts; the rest checks
/// the script is shell the image's /bin/sh will accept and that the service
/// file actually points at the file being chmodded.
#[test]
fn the_oopslog_wrapper_is_installed_executable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("eclipse-oopslog-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let localbin = dir.join("usr/local/bin");
    let svc_dir = dir.join("etc/eclipse/services");
    fs::create_dir_all(&localbin).unwrap();
    fs::create_dir_all(&svc_dir).unwrap();

    LinuxRootfs::write_oopslog(&localbin, &svc_dir);

    let script = localbin.join("eclipse-oopslog");
    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_ne!(
        mode & 0o111,
        0,
        "eclipse-oopslog is {:04o}: execve fails with EACCES and init respawns it forever",
        mode & 0o7777
    );

    let src = fs::read_to_string(&script).unwrap();
    assert!(src.starts_with("#!/bin/sh\n"), "shebang");
    let st = std::process::Command::new("sh")
        .arg("-n")
        .arg(&script)
        .status()
        .unwrap();
    assert!(st.success(), "sh -n rejected eclipse-oopslog");

    // The loop must never fall out of the bottom: `type = respawn` would
    // make a script that returns look exactly like the bug above.
    assert!(src.contains("while :; do"), "the drain has to be a loop");

    let unit = fs::read_to_string(svc_dir.join("oopslog.service")).unwrap();
    assert!(
        unit.contains("exec = /usr/local/bin/eclipse-oopslog"),
        "the service must point at the script this chmods: {unit}"
    );
    assert!(unit.contains("type = respawn"), "{unit}");

    let _ = fs::remove_dir_all(&dir);
}

/// The drain must write each contained fault ONCE, however many times
/// `/proc/oops` is read and however many reboots the file outlives.
///
/// `/proc/oops` hands back the whole record since boot, not the part that
/// is new, and `/var/log/oops.log` is appended to across reboots. Writing
/// the snapshot whenever it changed therefore wrote fault #1 again when
/// fault #2 arrived, and wrote the lot again after a reboot under a fresh
/// `=== date ===` header. That is not a tidiness problem: two captures of
/// one bug came back carrying a `[null-exec]` block identical down to
/// `r14=0x3b`, from different boots, and were read as one fault beside a
/// `[KERNEL PAGE FAULT]` that had nothing to do with it.
///
/// This drives the shipped script's own loop body over a `/proc/oops` that
/// grows the way a real one does.
#[test]
fn the_drain_never_writes_the_same_fault_twice() {
    let dir = std::env::temp_dir().join(format!("eclipse-oops-dedup-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let localbin = dir.join("usr/local/bin");
    let svc_dir = dir.join("etc/eclipse/services");
    fs::create_dir_all(&localbin).unwrap();
    fs::create_dir_all(&svc_dir).unwrap();
    LinuxRootfs::write_oopslog(&localbin, &svc_dir);

    // The record as the kernel grows it: one fault, then two.
    let first = "[isolate] fault one contained\n";
    let second = "[isolate] fault one contained\n[isolate] fault two contained\n";
    let proc_oops = dir.join("proc_oops");
    let stage2 = dir.join("stage2");
    fs::write(&proc_oops, first).unwrap();
    fs::write(&stage2, second).unwrap();
    let out = dir.join("oops.log");

    // The shipped text, with the three things a test cannot have: the real
    // /proc path, the real /var/log path, and a loop that never ends.
    let src = fs::read_to_string(localbin.join("eclipse-oopslog")).unwrap();
    let driver = src
        .replace("OUT=/var/log/oops.log", &format!("OUT={}", out.display()))
        .replace("mkdir -p /var/log 2>/dev/null", "ITER=0")
        .replace(
            "cat /proc/oops 2>/dev/null",
            &format!("cat {}", proc_oops.display()),
        )
        .replace(
            " sleep 10",
            &format!(
                " ITER=$((ITER+1)); [ $ITER -ge 2 ] && break; cp {} {}",
                stage2.display(),
                proc_oops.display()
            ),
        );
    assert!(
        driver.contains("break"),
        "the sleep was not where this expected it; the driver would loop forever"
    );
    let script = dir.join("driver.sh");
    fs::write(&script, &driver).unwrap();

    let st = std::process::Command::new("sh")
        .arg(&script)
        .status()
        .unwrap();
    assert!(st.success(), "the drain exited non-zero");

    let log = fs::read_to_string(&out).unwrap();
    assert_eq!(
        log.matches("fault one contained").count(),
        1,
        "fault one was written again when fault two arrived:\n{log}"
    );
    assert_eq!(
        log.matches("fault two contained").count(),
        1,
        "fault two is missing or doubled:\n{log}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The session-bus wrapper must be valid POSIX shell, must prefer Alpine's
/// dbus-daemon over Eclipse's own daemon, must bind the SAME address every
/// session already exports (`unix:path=/run/user/0/bus`) -- a wrapper that
/// binds anywhere else is a bus no client will ever find -- and must say
/// something findable when neither daemon is installed, because init wires
/// a service's stdio to /dev/null.
#[test]
fn dbus_wrapper_parses_and_prefers_dbus_daemon() {
    let dir = std::env::temp_dir().join(format!("eclipse-dbuswrap-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    LinuxRootfs::write_dbus_wrapper(&dir);

    let path = dir.join("eclipse-dbus");
    let src = fs::read_to_string(&path).unwrap();
    assert!(src.starts_with("#!/bin/sh\n"), "shebang");
    let st = std::process::Command::new("sh")
        .arg("-n")
        .arg(&path)
        .status()
        .unwrap();
    assert!(st.success(), "sh -n rejected eclipse-dbus");

    assert!(
        src.find("dbus-daemon").unwrap() < src.find("eclipse-dbusd").unwrap(),
        "Alpine's dbus-daemon must be tried first"
    );
    // Both daemons must be launched in the FOREGROUND: init supervises
    // them, and a daemon that forks away is one init would respawn forever.
    assert!(
        src.contains("--nofork"),
        "dbus-daemon must stay in the foreground"
    );
    assert!(!src.contains("--fork"), "no forking daemon under init");
    // The address every session exports, and nothing else.
    assert_eq!(
        src.matches("unix:path=$BUS").count(),
        4,
        "each daemon's log line and its --address use the one address"
    );
    assert!(src.contains("BUS=\"$XDG_RUNTIME_DIR/bus\""));
    assert!(
        src.contains("/etc/machine-id"),
        "dbus validates the machine id"
    );
    assert!(
        src.contains("> /dev/console"),
        "a missing daemon must be findable"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Every `module-alsa-sink` argument the generated system.pa passes must
/// be one the module accepts. pa_modargs rejects a load-module line as a
/// whole on the first unknown key, and the guest log then reads
/// "Failed to parse module arguments" with no sink created -- which is
/// how `mixer_device=` / `use_ucm=` (module-alsa-card keys) silenced
/// PulseAudio entirely. Catch the next one at build time, not in the
/// guest. The list is src/modules/alsa/module-alsa-sink.c's valid_modargs.
#[test]
fn system_pa_alsa_sink_args_are_all_accepted() {
    const ACCEPTED: &[&str] = &[
        "name",
        "sink_name",
        "sink_properties",
        "namereg_fail",
        "device",
        "device_id",
        "format",
        "rate",
        "alternate_rate",
        "channels",
        "channel_map",
        "fragments",
        "fragment_size",
        "mmap",
        "tsched",
        "tsched_buffer_size",
        "tsched_buffer_watermark",
        "ignore_dB",
        "control",
        "rewind_safeguard",
        "deferred_volume",
        "deferred_volume_safety_margin",
        "deferred_volume_extra_delay",
        "fixed_latency_range",
    ];
    let dir = std::env::temp_dir().join(format!("eclipse-pulseconf-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    LinuxRootfs::write_pulse_conf(&dir);
    let pa = fs::read_to_string(dir.join("etc/pulse/system.pa")).unwrap();
    let mut sinks = 0;
    for line in pa
        .lines()
        .filter(|l| l.contains("load-module module-alsa-sink"))
    {
        sinks += 1;
        for arg in line.split_whitespace().skip(2) {
            let key = arg.split('=').next().unwrap();
            assert!(
                ACCEPTED.contains(&key),
                "system.pa passes `{key}` to module-alsa-sink, which does not accept it: {line}"
            );
        }
    }
    assert!(
        sinks >= 1,
        "system.pa must load at least one module-alsa-sink"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `/var/run` must end up as the FHS symlink to `/run` even when an older
/// build left it as a real directory holding `pulse/`; creating
/// `var/run/pulse` afterwards must land in `run/pulse` (one directory
/// behind both paths is the whole point); and a second call is a no-op.
#[test]
fn ensure_var_run_replaces_a_real_dir_with_the_fhs_symlink() {
    let dir = std::env::temp_dir().join(format!("eclipse-varrun-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    // An older build: a real var/run with runtime state inside, no run/.
    fs::create_dir_all(dir.join("var/run/pulse")).unwrap();
    assert!(!dir.join("run").exists());

    LinuxRootfs::ensure_var_run(&dir);

    let link = dir.join("var/run");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "var/run must be a symlink, not a directory"
    );
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
    assert!(
        dir.join("run").is_dir(),
        "run/ must exist so the link never dangles"
    );

    // What write_pulse_conf does next: through the link, into run/pulse.
    fs::create_dir_all(dir.join("var/run/pulse")).unwrap();
    assert!(
        dir.join("run/pulse").is_dir(),
        "var/run/pulse must resolve to run/pulse"
    );

    // Idempotent: the link is kept, not replaced or nested.
    LinuxRootfs::ensure_var_run(&dir);
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
    assert!(dir.join("run/pulse").is_dir());

    // A symlink to the WRONG place (or dangling) is not "already done":
    // it must be replaced by the FHS link, not accepted.
    fs::remove_file(&link).unwrap();
    unix::fs::symlink("../../tmp", &link).unwrap();
    LinuxRootfs::ensure_var_run(&dir);
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));
    fs::remove_file(&link).unwrap();
    unix::fs::symlink("../nowhere", &link).unwrap();
    LinuxRootfs::ensure_var_run(&dir);
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("../run"));

    let _ = fs::remove_dir_all(&dir);
}
