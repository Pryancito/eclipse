//! Abyss, the second (optional) desktop session: the Wayland compositor and
//! its shell from the `desktop` submodule (Pryancito/desktop, a fork of
//! chasebrowndev/EclipseOS). labwc is the default; Abyss runs only when the
//! boot picks it, with `desktop=abyss` on the command line or `abyss` in
//! `/etc/eclipse/desktop` (eclipse-init's `selected_desktop`).
//!
//! Licence: everything under `desktop/` is AGPL-3.0-only (its protocol crates
//! and SDKs are Apache-2.0). It is built and shipped as separate programs; no
//! code from it is linked into anything under this repository's MIT licence,
//! and the source of exactly what ships is the pinned submodule commit.
//!
//! Like SilverSurfer, it is a HOST build: Abyss links the system libseat,
//! libinput, libudev, GBM/EGL and xkbcommon, so it is only built when the host
//! architecture is the image's, and every step is best-effort. A missing
//! submodule, a missing `-dev` package or a failed build just leaves the image
//! without the second session; labwc is untouched either way.
//!
//! It lives apart from `linux/mod.rs` because that file sits at its size
//! ceiling (see `xtask/src/tamano.rs`).

use super::LinuxRootfs;
use crate::PROJECT_DIR;
use os_xtask_utils::CommandExt;
use os_xtask_utils::Ext;
use std::fs;
use std::path::{Path, PathBuf};

/// What the `abyss/` workspace builds: the compositor and its control CLI.
const ABYSS_BINS: &[&str] = &["ec-abyss", "ec-ctl"];

/// What the `shell/` workspace builds: bar, launcher and Settings. The
/// wallpaper is lunarbg, as in labwc, so `ec-wallpaper` is not built. Not yet
/// `ec-toasts`/`ec-services`: they own `org.freedesktop.Notifications` and
/// would fight `eclipse-notifyd` for it on the shared session bus.
const SHELL_BINS: &[&str] = &["ec-hyperion-bar", "ec-launcher", "ec-settings"];

/// The submodule's checkout.
fn desktop_dir() -> PathBuf {
    PROJECT_DIR.join("desktop")
}

/// The look the image ships for `session`: the Abyss session wears the
/// `abyss` look (warm-black glass, see `tools/lunarbar/src/glass.rs`), so the
/// lunar apps match the compositor around them. Only the image's own default
/// (`look=tecno`, written by `eclipse-look`'s installer) is replaced; a
/// labwc image keeps tecno.
pub(super) fn default_look(rootfs: &Path, session: &str) {
    let path = rootfs.join("etc/eclipse/look");
    if session != "abyss" {
        return;
    }
    let shipped = fs::read_to_string(&path).unwrap_or_default();
    if shipped.trim().is_empty() || shipped.trim() == "look=tecno" {
        if let Err(e) = fs::write(&path, "look=abyss\n") {
            eprintln!("warning: could not write {path:?}: {e}");
        }
    }
}

impl LinuxRootfs {
    /// Build Abyss and its shell from `desktop/` and copy the binaries into
    /// `usr/bin`, plus the default `/etc/eclipse/abyss.kdl`. Best-effort.
    pub(super) fn install_abyss(&self, rootfs: &Path) {
        let dir = desktop_dir();
        if !dir.join("abyss").join("Cargo.toml").is_file() {
            eprintln!(
                "warning: abyss: desktop/ is empty (git submodule update --init desktop); \
                 the image has labwc only"
            );
            return;
        }
        if self.needs_cross_linker() {
            eprintln!(
                "warning: abyss: not built for a {} image on a {} host (it links the \
                 host's libseat/libinput/GBM/EGL and there is no cross sysroot); the \
                 image has labwc only",
                self.0.name(),
                std::env::consts::ARCH
            );
            return;
        }
        // Each workspace pins its own toolchain (rust-toolchain.toml) and the
        // submodule's .cargo/config.toml puts every binary in desktop/target.
        // cargo's own fingerprinting makes a second run a no-op.
        let built = cargo_build(&dir.join("abyss"), ABYSS_BINS, Some("drm"))
            & cargo_build(&dir.join("shell"), SHELL_BINS, None);
        if !built {
            eprintln!(
                "warning: abyss: build failed (needs libseat-dev libinput-dev libudev-dev \
                 libgbm-dev libegl-dev libgles-dev libxkbcommon-dev libwayland-dev \
                 libdrm-dev); installing whatever was built"
            );
        }
        let out = dir.join("target").join("release");
        let usr_bin = rootfs.join("usr/bin");
        let _ = fs::create_dir_all(&usr_bin);
        let mut installed = 0;
        for name in ABYSS_BINS.iter().chain(SHELL_BINS) {
            let src = out.join(name);
            if !src.is_file() || !self.cached_for_target(&src) {
                continue;
            }
            let dest = usr_bin.join(name);
            let _ = fs::remove_file(&dest);
            match fs::copy(&src, &dest) {
                Ok(_) => installed += 1,
                Err(err) => eprintln!("warning: abyss: could not install {name}: {err}"),
            }
        }
        // System tier of Abyss's config search path; the user tier
        // (~/.config/eclipse/abyss.kdl) overrides it key by key.
        let etc = rootfs.join("etc/eclipse");
        let _ = fs::create_dir_all(&etc);
        let _ = fs::write(etc.join("abyss.kdl"), DEFAULT_CONFIG);
        let _ = fs::write(etc.join("policy.kdl"), DEFAULT_POLICY);
        println!("abyss: {installed} binaries in /usr/bin (session: desktop=abyss)");
    }
}

/// `cargo build --release` of some packages of one workspace.
fn cargo_build(workspace: &Path, packages: &[&str], features: Option<&str>) -> bool {
    println!(
        "Compiling {} (host)...",
        workspace.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut cargo = Ext::new("cargo");
    // Upstream's release profile keeps full debuginfo: 130-190 MiB a binary.
    // The image wants them stripped (a few MiB each).
    cargo
        .current_dir(workspace)
        .env("CARGO_PROFILE_RELEASE_STRIP", "true")
        .arg("build")
        .arg("--release");
    for p in packages {
        cargo.arg("-p").arg(p);
    }
    if let Some(f) = features {
        cargo.arg("--no-default-features").arg("--features").arg(f);
    }
    cargo.status().success()
}

/// `/etc/eclipse/abyss.kdl`. Upstream defaults, except what Eclipse OS needs:
/// a desktop in the KDE manner (titlebars, maximize) like the labwc session,
/// foot as the terminal (Apotheosis is not built), and the labwc session's
/// own shortcuts for the lunar apps (see `rc.xml` in `desktop.rs`). Abyss's
/// launcher keeps Super+R; Super+E is the file manager, as in labwc.
///
/// `render-device` pins the console GPU, as `WLR_DRM_DEVICES` does for labwc.
/// Abyss's automatic pick counts each GPU's connectors from the
/// `/sys/class/drm/cardN-<connector>` directories, and Eclipse's sysfs lists
/// only the `cardN`/`renderDN` nodes, so on the RTX machine it found two GPUs
/// with no connector and refused to start. `card0` is the GPU the screen hangs
/// off (`devfs::drm::build_gpu_nodes`); its connectors are read through KMS
/// once it is open, which works.
///
/// `gpu-warmup`, `frame-pacing-off` and `explicit-sync-off`: without them,
/// `vblank_mode=0 es2gears_wayland` on the RTX machine fell from ~4000 FPS to
/// a frame every few seconds for tens of seconds at a time, with a hardware
/// fence counted landed 93 s after its submit (`/proc/gpudbg`); with them it
/// holds 3000-3200 FPS. The kernel side of explicit sync is what is suspect
/// there (a fence held behind its chain; the `[syncobj] hw fence` dmesg
/// line names it), so it stays off until that is found.
const DEFAULT_CONFIG: &str = "\
// Eclipse OS: system defaults for the Abyss session (desktop=abyss).
// Your own changes go in ~/.config/eclipse/abyss.kdl (or Settings).
// Every key: https://github.com/Pryancito/desktop/blob/main/docs/CONFIG.md
mode \"de\"
misc {
    render-device \"/dev/dri/card0\"
    terminal-command \"foot\"
    gpu-warmup #true
    frame-pacing-off #true
    explicit-sync-off #true
}
launcher {
    bind {
        open \"none\"
        run \"Super+R\"
    }
}
bind \"SUPER\" \"Return\" { spawn \"/usr/local/bin/eclipse-terminal\"; }
bind \"ALT\" \"Return\" { spawn \"/usr/local/bin/eclipse-terminal\"; }
bind \"ALT\" \"space\" { spawn \"/usr/local/bin/eclipse-run\"; }
bind \"ALT\" \"F2\" { spawn \"/usr/local/bin/eclipse-run\"; }
bind \"SUPER\" \"e\" { spawn \"/usr/local/bin/eclipse-files\"; }
bind \"SUPER\" \"v\" { spawn \"/usr/local/bin/eclipse-clipboard\"; }
bind \"SUPER\" \"d\" { spawn \"/usr/local/bin/eclipse-showdesktop\"; }
bind \"SUPER\" \"l\" { spawn \"lunarlock\"; }
bind \"SUPER\" \"Tab\" { spawn \"lunardesk\"; }
bind \"SUPER\" \"n\" { spawn \"lunarnotes\"; }
bind \"SUPER\" \"k\" { spawn \"/usr/local/bin/eclipse-kbd toggle\"; }
bind \"Print\" { spawn \"lunarshot\"; }
bind \"SHIFT\" \"Print\" { spawn \"lunarshot --region\"; }
bind \"CTRL SHIFT\" \"Escape\" { spawn \"/usr/local/bin/eclipse-monitor\"; }
";

/// `/etc/eclipse/policy.kdl`: upstream's `secret` windows and input methods,
/// plus the lunar apps' admissions. Abyss hides every privileged global from
/// a program not named here (ADR 0094), so each lunar app that needs one is
/// listed: the clipboard manager, the screenshot tool, and the clients that
/// list and control windows and desktops (Super+Tab, Super+D, lunarbar).
const DEFAULT_POLICY: &str = "\
// Eclipse OS: who may use Abyss's privileged protocols (desktop=abyss).
windowrule \"sensitivity secret\" { app-id \"^ec-secret-prompt$\"; }
windowrule \"sensitivity secret\" { app-id \"^(?i:org\\\\.keepassxc\\\\.KeePassXC|keepassxc)$\"; }
windowrule \"sensitivity secret\" { app-id \"^(?i:bitwarden|com\\\\.bitwarden\\\\.desktop)$\"; }
input-method { allow \"fcitx5\" \"kime\" \"squeekboard\"; }
clipboard { data-control-allow \"/bin/lunarclip\"; }
capture { allow \"/bin/lunarshot\"; }
toplevel-management { allow \"/bin/lunarbar\" \"/bin/lunarrun\" \"/bin/lunardesk\"; }
";

/// `/usr/local/bin/eclipse-abyss`: the session's environment, then Abyss.
/// The GPU choice (zink+NVK, or software) is NOT made here: eclipse-init's
/// `build_child_env` already exports it to every service, labwc's included.
pub(super) fn write_wrapper(localbin: &Path) {
    fs::write(
        localbin.join("eclipse-abyss"),
        b"#!/bin/sh\n\
          # Eclipse OS: the Abyss session (desktop=abyss). See xtask/src/linux/abyss.rs.\n\
          : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
          [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
          : \"${XDG_CONFIG_HOME:=$HOME/.config}\"; export XDG_CONFIG_HOME\n\
          : \"${XCURSOR_THEME:=Adwaita}\"; export XCURSOR_THEME\n\
          : \"${XCURSOR_SIZE:=24}\"; export XCURSOR_SIZE\n\
          # Ubuntu's libseat would try logind first; there is none. Pin seatd.\n\
          : \"${LIBSEAT_BACKEND:=seatd}\"; export LIBSEAT_BACKEND\n\
          : \"${LANG:=es_ES.UTF-8}\"; export LANG\n\
          : \"${TZ:=Europe/Madrid}\"; export TZ\n\
          # Xwayland's sockets go in /tmp/.X11-unix, which Smithay does not create.\n\
          [ -d /tmp/.X11-unix ] || { mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix; }\n\
          if [ ! -x /usr/bin/ec-abyss ]; then\n\
          \x20 echo 'eclipse-abyss: /usr/bin/ec-abyss is not in this image (desktop/ submodule not built); boot with desktop=labwc' >/dev/console 2>/dev/null\n\
          \x20 sleep 60\n\
          \x20 exit 127\n\
          fi\n\
          # `--session` exports WAYLAND_DISPLAY to systemd --user when there is\n\
          # one; here there is none and it skips that. eclipse-init starts the\n\
          # shell (abyss-*.service) instead.\n\
          exec /usr/bin/ec-abyss --backend drm --session\n",
    )
    .unwrap();
    set_exec(&localbin.join("eclipse-abyss"));
    // The shell clients. Abyss binds `wayland-1`, never `wayland-0` (Smithay's
    // `new_auto` skips 0 so a client cannot reach the wrong compositor), and
    // init's environment is the labwc one, so the display is set here.
    fs::write(
        localbin.join("eclipse-abyss-client"),
        b"#!/bin/sh\n\
          # Eclipse OS: one Abyss shell client (`eclipse-abyss-client ec-hyperion-bar`).\n\
          : \"${XDG_RUNTIME_DIR:=/run/user/0}\"; export XDG_RUNTIME_DIR\n\
          WAYLAND_DISPLAY=wayland-1; export WAYLAND_DISPLAY\n\
          : \"${LANG:=es_ES.UTF-8}\"; export LANG\n\
          : \"${TZ:=Europe/Madrid}\"; export TZ\n\
          if [ ! -x \"/usr/bin/$1\" ]; then\n\
          \x20 echo \"eclipse-abyss-client: /usr/bin/$1 is not in this image\" >&2\n\
          \x20 sleep 60\n\
          \x20 exit 127\n\
          fi\n\
          exec \"/usr/bin/$1\"\n",
    )
    .unwrap();
    set_exec(&localbin.join("eclipse-abyss-client"));
}

/// The services of the Abyss session, all `desktop = abyss`: its own seatd
/// (seatd.service is `desktop = labwc`, and a service has one desktop), the
/// compositor, and the shell clients, parked on Abyss's control socket.
pub(super) fn write_services(svc_dir: &Path) {
    let mut files: Vec<(String, String)> = vec![
        (
            "abyss-seatd.service".into(),
            "# Seat manager for the Abyss session. See /usr/local/bin/eclipse-seatd.\n\
             exec = /usr/local/bin/eclipse-seatd\n\
             type = respawn\n\
             desktop = abyss\n"
                .into(),
        ),
        (
            "abyss.service".into(),
            "# Abyss Wayland session. See /usr/local/bin/eclipse-abyss.\n\
             # Same gates as labwc.service: seatd's socket, a settled /dev/input.\n\
             exec = /usr/local/bin/eclipse-abyss\n\
             type = respawn\n\
             after = abyss-seatd dbus\n\
             requires = abyss-seatd\n\
             wait_socket = /run/seatd.sock\n\
             wait_path = /dev/input\n\
             desktop = abyss\n\
             log = /tmp/abyss.log\n"
                .into(),
        ),
    ];
    // Abyss's bar; lunarbg and lunarclip through the same wrappers as under
    // labwc (they take whichever wayland-N socket exists).
    for (name, exec) in [
        (
            "hyperion-bar",
            "/usr/local/bin/eclipse-abyss-client ec-hyperion-bar",
        ),
        ("lunarbg", "/usr/local/bin/eclipse-lunarbg"),
        ("lunarclip", "/usr/local/bin/eclipse-lunarclip"),
    ] {
        files.push((
            format!("abyss-{name}.service"),
            format!(
                "# {name} for the Abyss session.\n\
                 exec = {exec}\n\
                 type = respawn\n\
                 after = abyss\n\
                 requires = abyss\n\
                 wait_socket = /run/user/0/eclipse/abyss.sock\n\
                 desktop = abyss\n"
            ),
        ));
    }
    for (name, body) in files {
        fs::write(svc_dir.join(name), body).unwrap();
    }
}

fn set_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_is_abyss_only() {
        let dir = std::env::temp_dir().join(format!("abyss-svc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_services(&dir);
        let mut n = 0;
        for e in fs::read_dir(&dir).unwrap() {
            let body = fs::read_to_string(e.unwrap().path()).unwrap();
            assert!(body.contains("desktop = abyss\n"), "{body}");
            assert!(!body.contains("desktop = labwc"), "{body}");
            n += 1;
        }
        assert_eq!(n, 5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_abyss_session_ships_the_abyss_look() {
        let dir = std::env::temp_dir().join(format!("abyss-look-{}", std::process::id()));
        let etc = dir.join("etc/eclipse");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("look"), "look=tecno\n").unwrap();
        default_look(&dir, "labwc");
        assert_eq!(
            fs::read_to_string(etc.join("look")).unwrap(),
            "look=tecno\n"
        );
        default_look(&dir, "abyss");
        assert_eq!(
            fs::read_to_string(etc.join("look")).unwrap(),
            "look=abyss\n"
        );
        // A look somebody chose is left alone.
        fs::write(etc.join("look"), "look=kde\n").unwrap();
        default_look(&dir, "abyss");
        assert_eq!(fs::read_to_string(etc.join("look")).unwrap(), "look=kde\n");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same pin as labwc's `WLR_DRM_DEVICES`: without it Abyss looks for
    /// connectors in sysfs, finds none on Eclipse, and does not start.
    #[test]
    fn config_pins_the_console_gpu() {
        assert!(DEFAULT_CONFIG.contains("render-device \"/dev/dri/card0\"\n"));
        // Without these a GPU client stalled for tens of seconds on the RTX box.
        for key in ["gpu-warmup", "frame-pacing-off", "explicit-sync-off"] {
            assert!(
                DEFAULT_CONFIG.contains(&format!("    {key} #true\n")),
                "{key} is not on in the system config"
            );
        }
    }

    #[test]
    fn wrapper_execs_the_drm_backend() {
        let dir = std::env::temp_dir().join(format!("abyss-bin-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_wrapper(&dir);
        let s = fs::read_to_string(dir.join("eclipse-abyss")).unwrap();
        assert!(s.contains("exec /usr/bin/ec-abyss --backend drm --session\n"));
        assert!(s.contains("LIBSEAT_BACKEND:=seatd"));
        assert!(s.contains("mkdir -p /tmp/.X11-unix"));
        let c = fs::read_to_string(dir.join("eclipse-abyss-client")).unwrap();
        assert!(c.contains("WAYLAND_DISPLAY=wayland-1;"));
        let _ = fs::remove_dir_all(&dir);
    }
}
