//! The XDG desktop portal, as the image ships it: the wrapper
//! `eclipse-init` runs and the service file that starts it.
//!
//! It lives apart from `linux/mod.rs` because that file is already most of a
//! thousand lines of shell quoted inside Rust, and because the portal is one
//! idea with two halves: nothing starts it but this service, and nothing in
//! the session trusts it until the stamp this wrapper manages exists.
//!
//! It also holds the cross-build of `tools/eclipse-dbusd`, because the portal
//! is the SECOND BINARY of the bus's own package: one cargo invocation
//! produces `eclipse-dbusd` and `eclipse-portald`, and what the two share is a
//! D-Bus marshaller.
//!
//! What the portal itself does is in `tools/eclipse-dbusd/src/portal.rs`.

use super::LinuxRootfs;
use crate::PROJECT_DIR;
use os_xtask_utils::CommandExt;
use os_xtask_utils::Ext;
use std::fs;
use std::path::{Path, PathBuf};

impl LinuxRootfs {
    /// Cross-compile `tools/eclipse-dbusd` (Rust) as a static, non-PIE musl
    /// binary and return its path. Best-effort, like every other tool here: a
    /// failed build just means the image relies on Alpine's `dbus-daemon`.
    pub(super) fn eclipse_dbusd(&self) -> (PathBuf, PathBuf) {
        let dir = PROJECT_DIR.join("tools").join("eclipse-dbusd");
        let triple = self.musl_rust_triple();
        let out = dir.join("target").join(triple).join("release");
        let executable = out.join("eclipse-dbusd");
        // The portal is the second binary of the same package: it speaks
        // D-Bus, so it shares the marshaller and the client with the daemon
        // rather than carrying a copy.
        let portald = out.join("eclipse-portald");
        let newest_src = [
            "src/main.rs",
            "src/lib.rs",
            "src/bus.rs",
            "src/message.rs",
            "src/client.rs",
            "src/portal.rs",
            "src/bin/eclipse-portald.rs",
            "Cargo.toml",
        ]
        .iter()
        .filter_map(|rel| fs::metadata(dir.join(rel)).ok()?.modified().ok())
        .max();
        // BOTH have to be newer than the newest source: one missing or stale
        // is a rebuild, not a cache hit.
        let oldest_bin = [&executable, &portald]
            .iter()
            .map(|b| fs::metadata(b).ok()?.modified().ok())
            .collect::<Option<Vec<_>>>()
            .and_then(|v| v.into_iter().min());
        if let (Some(bin_mtime), Some(src_mtime)) = (oldest_bin, newest_src) {
            if bin_mtime >= src_mtime
                && self.cached_for_target(&executable)
                && self.cached_for_target(&portald)
            {
                return (executable, portald);
            }
        }

        println!("Compiling eclipse-dbusd (Rust, {triple})...");
        let _ = Ext::new("rustup")
            .arg("target")
            .arg("add")
            .arg(triple)
            .status();
        let mut cargo = Ext::new("cargo");
        cargo
            .current_dir(&dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg(triple);
        // Static non-PIE, plus the cross linker when the host's `cc` cannot
        // link this target at all: see `cross_build_env`.
        for (k, v) in self.cross_build_env() {
            cargo.env(k, v);
        }
        let status = cargo.status();
        if !status.success() {
            eprintln!("warning: eclipse-dbusd build failed; only dbus-daemon can serve the bus");
        }
        (executable, portald)
    }
}

/// `/usr/local/bin/eclipse-portal`.
pub fn write_wrapper(localbin: &Path) {
    // The portal, in the foreground so eclipse-init supervises it. It is
    // Eclipse's own `eclipse-portald` unless Alpine's
    // `xdg-desktop-portal` is installed, which is the better portal and
    // wins: both take the same name on the bus, and `eclipse-portald`
    // stands aside by itself if it loses the race, but not starting it at
    // all is cheaper than a daemon that exits.
    fs::write(
        localbin.join("eclipse-portal"),
        b"#!/bin/sh\n\
          # Eclipse OS: XDG desktop portal for eclipse-init.\n\
          : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
          BUS=\"$XDG_RUNTIME_DIR/bus\"\n\
          : \"${DBUS_SESSION_BUS_ADDRESS:=unix:path=$BUS}\"\n\
          export DBUS_SESSION_BUS_ADDRESS\n\
          # The stamp says the portal is SERVING, and the session reads it\n\
          # to decide whether to set GTK_USE_PORTAL. A stale one from a\n\
          # previous run would advertise a portal that is not there, so it\n\
          # goes before the daemon is started, not after.\n\
          READY=\"$XDG_RUNTIME_DIR/eclipse-portal.ready\"\n\
          rm -f \"$READY\"\n\
          for d in /usr/libexec /usr/lib/xdg-desktop-portal /usr/lib; do\n\
          \x20 if [ -x \"$d/xdg-desktop-portal\" ]; then\n\
          \x20 \x20 echo \"eclipse-portal: $d/xdg-desktop-portal\" > /dev/console 2>/dev/null\n\
          \x20 \x20 exec \"$d/xdg-desktop-portal\" -r\n\
          \x20 fi\n\
          done\n\
          for d in /usr/local/bin /usr/bin /bin; do\n\
          \x20 if [ -x \"$d/eclipse-portald\" ]; then\n\
          \x20 \x20 echo \"eclipse-portal: $d/eclipse-portald on $DBUS_SESSION_BUS_ADDRESS\" > /dev/console 2>/dev/null\n\
          \x20 \x20 exec \"$d/eclipse-portald\" --ready=\"$READY\"\n\
          \x20 fi\n\
          done\n\
          MSG='eclipse-portal: no xdg-desktop-portal and no eclipse-portald\n\
          -- an application whose file dialog is a portal call will fall\n\
          back to its own. Rebuild the image so\n\
          /usr/local/bin/eclipse-portald is installed.'\n\
          echo \"$MSG\" > /dev/console 2>/dev/null || true\n\
          echo \"$MSG\" >&2\n\
          sleep 60\n\
          exit 127\n",
    )
    .unwrap();
}

/// `/etc/eclipse/services/portal.service`.
pub fn write_service(svc_dir: &Path) {
    // The XDG desktop portal: the file chooser other applications call
    // over the bus. `requires = dbus` implies `after = dbus` (see #1744),
    // and `wait_socket` keeps it from being respawned in a loop while the
    // bus is still binding. Not desktop-restricted: a portal is as useful
    // to an Xorg session as to a labwc one, and the chooser it shows
    // (`lunarfile`) is a Wayland client either way, which is the one
    // limitation worth knowing.
    fs::write(
        svc_dir.join("portal.service"),
        b"# XDG desktop portal (FileChooser). See /usr/local/bin/eclipse-portal.\n\
          exec = /usr/local/bin/eclipse-portal\n\
          type = respawn\n\
          requires = dbus\n\
          wait_socket = /run/user/0/bus\n\
          log = /tmp/portal.log\n",
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-portal-xtask-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The wrapper is a shell script `eclipse-init` execs, so the things that
    /// can only be got wrong here are: it has to parse, it has to stay in the
    /// FOREGROUND (a daemon that forks away is one init respawns for ever),
    /// and it has to prefer the real xdg-desktop-portal when the image has it.
    #[test]
    fn the_portal_wrapper_parses_and_prefers_the_real_portal() {
        let dir = scratch("wrapper");
        write_wrapper(&dir);
        let path = dir.join("eclipse-portal");
        let src = fs::read_to_string(&path).unwrap();
        assert!(src.starts_with("#!/bin/sh\n"), "shebang");
        let st = std::process::Command::new("sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success(), "sh -n rechazo eclipse-portal");
        // Alpine's portal first: it is the complete one, and two owners of one
        // bus name is a coin toss over which serves each call.
        assert!(
            src.find("xdg-desktop-portal").unwrap() < src.find("eclipse-portald").unwrap(),
            "el portal de Alpine va primero"
        );
        // And it looks where Alpine actually puts it: xdg-desktop-portal is
        // a libexec helper, not something on PATH, so a list without
        // /usr/libexec never finds it however well it is ordered.
        assert!(src.contains("/usr/libexec"), "{src}");
        // Both are exec'd, never backgrounded.
        assert_eq!(src.matches("exec \"").count(), 2, "{src}");
        assert!(!src.contains(" &\n"), "nada en segundo plano bajo init");
        // The stale stamp goes BEFORE anything is started: a leftover one from
        // a previous boot would tell the session a portal is serving when
        // nothing is.
        let rm = src.find("rm -f \"$READY\"").expect("borra el sello");
        assert!(rm < src.find("exec \"").unwrap(), "el sello se borra tarde");
        assert!(src.contains("--ready=\"$READY\""), "no pasa --ready");
        assert!(
            src.contains("eclipse-portal.ready"),
            "el sello tiene que ser el que lee la sesion"
        );
        // And a missing portal has to be findable: init wires a service's
        // stdio to /dev/null, so a bare `>&2` vanishes.
        assert!(src.contains("> /dev/console"), "{src}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The service is the only thing that starts the portal, so what it says
    /// about ordering is the whole ordering.
    #[test]
    fn the_portal_service_waits_for_the_bus_it_needs() {
        let dir = scratch("service");
        write_service(&dir);
        let src = fs::read_to_string(dir.join("portal.service")).unwrap();
        assert!(
            src.contains("exec = /usr/local/bin/eclipse-portal"),
            "{src}"
        );
        // A portal with no bus is nothing: `requires` implies `after` (#1744),
        // and `wait_socket` keeps init from respawning it in a loop while the
        // bus is still binding.
        assert!(src.contains("requires = dbus"), "{src}");
        assert!(src.contains("wait_socket = /run/user/0/bus"), "{src}");
        // Supervised, and with somewhere to say what went wrong.
        assert!(src.contains("type = respawn"), "{src}");
        assert!(src.contains("log = "), "{src}");
        // NOT tied to one session: an Xorg session wants a portal too.
        assert!(!src.contains("desktop = "), "{src}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `/etc/profile` as the image ships it.
    fn profile_text() -> String {
        let etc = scratch("profile").join("etc");
        fs::create_dir_all(&etc).unwrap();
        LinuxRootfs::write_profile(&etc);
        fs::read_to_string(etc.join("profile")).unwrap()
    }

    /// One `export KEY=VALUE` of a profile, or nothing.
    fn export<'a>(profile: &'a str, key: &str) -> Option<&'a str> {
        profile
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("export "))
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// A login shell on a VT does not inherit the labwc session's
    /// environment, so `/etc/profile` carries the same portal decision. What
    /// matters is that it is a DECISION and not a pin: with no portal serving,
    /// `GTK_USE_PORTAL=1` makes a GTK application skip its own dialog for a
    /// portal that will never answer.
    #[test]
    fn the_profile_points_gtk_at_the_portal_only_when_one_is_serving() {
        let profile = profile_text();
        let line = profile
            .lines()
            .map(str::trim)
            .find(|l| l.contains("GTK_USE_PORTAL") && !l.starts_with('#'))
            .expect("/etc/profile no dice nada del portal");
        assert!(
            line.starts_with("[ -e /run/user/0/eclipse-portal.ready ]"),
            "esta puesto sin condicion: {line}"
        );
        assert!(line.contains("export GTK_USE_PORTAL=1"), "{line}");
        // And nowhere else, unconditionally.
        assert_eq!(profile.matches("GTK_USE_PORTAL").count(), 2, "{profile}");
        assert_eq!(export(&profile, "GTK_USE_PORTAL"), None, "{profile}");
    }
}
