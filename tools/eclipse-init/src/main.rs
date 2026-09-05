//! Eclipse OS init — a small, purpose-built PID 1 / service supervisor.
//!
//! Eclipse's kernel already mounts the root, brings up the network and spawns
//! the per-VT shells, so init does NOT need a heavyweight, shell-driven service
//! manager (OpenRC's per-step `busybox sh` fork/exec churn is exactly what
//! stressed the kernel's fragile paths). This init does only what PID 1 must:
//!
//!   * reap orphaned children forever (the defining duty of PID 1),
//!   * mount any pseudo-filesystems that are missing (idempotent, best-effort),
//!   * launch the userspace declared in `/etc/eclipse/services/*.service`
//!     (`oneshot` tasks run to completion in order; `respawn` services are
//!     supervised and restarted if they exit),
//!   * shut the system down on SIGTERM/SIGUSR1/SIGUSR2 (halt/power off) and
//!     SIGINT (reboot). The path is the same as busybox `reboot -f` /
//!     `poweroff -f`: `sync` then `reboot(2)`. A polite kill-all of the
//!     session (labwc, GPU clients) before the syscall hung restart on this
//!     kernel; the syscall itself already quiesces DRM and NVMe. busybox
//!     `halt`/`poweroff`/`reboot` send SIGUSR1/SIGUSR2/SIGTERM; SIGINT is
//!     Ctrl-Alt-Del. The `/usr/local/bin/reboot` wrapper execs
//!     `busybox reboot -f` so a typed `reboot` matches that force path.
//!
//! Design borrowed from runit/s6/dinit (supervision, declarative services,
//! dependency ordering); implementation is our own so every syscall is under
//! our control on the still-maturing kernel. No shell is involved.

use std::collections::{BTreeMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A respawn service that exits sooner than this after starting is treated as
/// "crashing", not "finished a unit of work", and its restart is delayed.
const HEALTHY_UPTIME: Duration = Duration::from_secs(2);
/// Restart delay for a crashing service: starts here and doubles up to
/// [`MAX_BACKOFF`]. Without it, a service whose binary is missing or that dies
/// on start (labwc before the GPU is ready, udhcpc on a link with no DHCP)
/// would fork/exec at full speed forever, pinning a CPU.
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Set by the SIGTERM/SIGUSR2 handlers: bring the system down (halt/power off).
static WANT_HALT: AtomicBool = AtomicBool::new(false);
/// Set by the SIGINT handler (Ctrl-Alt-Del is delivered to PID 1 as SIGINT).
static WANT_REBOOT: AtomicBool = AtomicBool::new(false);
/// Unique renderer messages once per boot; every respawn used to reprint them.
static RENDERER_MSGS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn log_renderer_once(msg: &str) {
    match RENDERER_MSGS.lock() {
        Ok(mut guard) => {
            let set = guard.get_or_insert_with(HashSet::new);
            if set.insert(msg.to_string()) {
                log(msg);
            }
        }
        Err(_) => log(msg),
    }
}

extern "C" fn on_sigterm(_sig: libc::c_int) {
    WANT_HALT.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigint(_sig: libc::c_int) {
    WANT_REBOOT.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigusr1(_sig: libc::c_int) {
    // busybox `halt` (without -f) signals PID 1 with SIGUSR1.
    WANT_HALT.store(true, Ordering::SeqCst);
}
extern "C" fn on_sigusr2(_sig: libc::c_int) {
    // busybox `poweroff` (without -f) signals PID 1 with SIGUSR2. Without
    // this handler PID 1 ignores the signal (Linux never applies the default
    // terminate action to init) and "Apagar" in lunarbar did nothing.
    WANT_HALT.store(true, Ordering::SeqCst);
}

/// How a service is managed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Run once to completion during boot (mounts, one-time setup).
    Oneshot,
    /// Long-running; supervised and restarted if it exits.
    Respawn,
}

struct Service {
    name: String,
    /// argv (argv[0] is the absolute program path).
    exec: Vec<String>,
    kind: Kind,
    /// Names of services that must be started before this one.
    after: Vec<String>,
    /// Unix socket path to wait for (bounded) before every start of this
    /// service — `after =` only orders the FORK of the dependency, not its
    /// readiness. Waiting natively here (a 10 ms stat poll) replaced the
    /// wrappers' `sleep 0.1`-per-iteration shell loops, which forked a busybox
    /// per poll exactly while the compositor was busy demand-paging itself.
    wait_socket: Option<String>,
    /// Filesystem path (any type) to wait for, bounded, before every start.
    /// labwc uses this for `/dev/input/event0`: without udevd there is NO
    /// input hotplug — libinput scans /dev/input exactly ONCE at compositor
    /// startup, so if the kernel's deferred USB HID enumeration has not
    /// produced the event nodes yet, keyboard and mouse stay dead for the
    /// whole session. Bounded so a genuinely input-less machine still boots.
    wait_path: Option<String>,
    /// Desktop session this service belongs to (`labwc` or `xorg`). `None` means
    /// session-agnostic (always started). A tagged service starts only when the
    /// selected desktop (see [`selected_desktop`]) matches, so the same image
    /// boots labwc on hardware and Xorg under `make qemu` purely from a
    /// `desktop=` boot argument.
    desktop: Option<String>,
    /// If set, child stdout/stderr append here instead of `/dev/null`.
    log: Option<String>,
    /// Live child pid for a running `respawn` service.
    pid: Option<i32>,
    /// When the current child was last started (for crash-loop backoff).
    started_at: Option<Instant>,
    /// Current restart delay for a crashing respawn service; grows on repeated
    /// fast exits, resets once the service stays up past [`HEALTHY_UPTIME`].
    backoff: Duration,
}

/// Default environment handed to every service (and inherited by their
/// children). Includes the Wayland session vars that `/usr/local/bin/labwc`
/// also asserts: init now launches that wrapper too, but keeping the base
/// variables here preserves the boot session if the wrapper ever gets bypassed.
const CHILD_ENV: &[&str] = &[
    "PATH=/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin",
    "HOME=/root",
    "TERM=xterm-256color",
    "LANG=es_ES.UTF-8",
    "LANGUAGE=es:en",
    "TZ=Europe/Madrid",
    "XDG_RUNTIME_DIR=/run/user/0",
    "XDG_CONFIG_HOME=/root/.config",
    "XCURSOR_THEME=Adwaita",
    "XCURSOR_SIZE=24",
    // Xwayland display: pinned again. It was removed while labwc's Xwayland
    // died on spawn — an X11 connect to labwc's socket then blocked FOREVER
    // (the socket EXISTS, labwc listens and parks the client for a spawn that
    // never completes), so the failure mode was an infinite hang rather than
    // a refused connect, and with DISPLAY pinned that hang swallowed every
    // app that merely PROBED X11 (`vulkaninfo` froze right after enumerating
    // the GPU, in its Xlib/xcb surface-support query; glxgears likewise).
    // Two kernel fixes ended that: the fork bug that killed the spawn, and
    // the writev 64 KiB cap that killed GLX clients on their first full-frame
    // flush. X11 is verified working on the RTX now.
    //
    // Services started here do NOT inherit labwc's environment (init execs
    // them itself), so unlike the labwc session env — where labwc's own
    // setenv of the real display number wins — this pin is the only DISPLAY
    // an init-started child and its descendants ever see. That covers the
    // launcher chain: lunarbar spawns apps as ITS children, not labwc's.
    "DISPLAY=:0",
    // NOTE: WLR_RENDERER is NOT here — it is appended at spawn time by
    // `build_child_env` so it can honour the `renderer=` boot arg: pixman by
    // default; `renderer=gl` turns on the NVIDIA experiment knobs but still
    // keeps labwc on the safe software path unless an explicit wlroots
    // compositor token opts into the unstable GPU renderer; `renderer=gl-sw`
    // forces wlroots GLES2 over Mesa llvmpipe (software GL — renders in QEMU
    // where there is no GPU 3D).
    "WLR_BACKENDS=drm,libinput",
    "WLR_DRM_DEVICES=/dev/dri/card0",
    "WLR_LIBINPUT_NO_DEVICES=1",
    // Force LINEAR scanout buffers. Our presentation is a CPU blit that reads
    // the framebuffer linearly, so it can only scan out DRM_FORMAT_MOD_LINEAR.
    // Without this wlroots negotiates a BLOCK-LINEAR swapchain with NVK (PTE
    // kind 0x06), which our VM_BIND refuses (it can only program linear
    // mappings) -- `vkBindImageMemory failed`, `gbm_bo_create failed`,
    // "Swapchain for output failed test", no desktop. `WLR_DRM_NO_MODIFIERS`
    // makes wlroots allocate implicit-modifier (linear) buffers regardless of
    // where it would otherwise source tiled modifiers (the KMS plane OR the
    // renderer's dma-buf feedback), which the `DRM_CAP_ADDFB2_MODIFIERS=0` KMS
    // cap alone may not cover. Belt and braces with that cap.
    "WLR_DRM_NO_MODIFIERS=1",
    // SDL (sdl12-compat / SDL2 / SDL3) backends, the renderer-independent half
    // of the session's SDL policy (the labwc wrapper and /etc/profile assert
    // the same). Video: native Wayland first, X11 as fallback -- Xwayland in
    // the labwc session, Xorg under desktop=xorg, so ONE list serves both.
    // SDL2 otherwise picks X11 whenever DISPLAY is set, which with the pin
    // above is always, sending every SDL app through Xwayland. The comma list
    // needs SDL >= 2.24 (Alpine ships 2.30+); SDL3 reads the underscored
    // names. Audio: SDL stays on ALSA; /etc/asound.conf routes that through
    // PulseAudio so several clients can play at once. Native libpulse clients
    // use PULSE_SERVER (set below). OpenAL prefers Pulse, then ALSA.
    // The renderer half (SDL_RENDER_DRIVER / SDL_FRAMEBUFFER_ACCELERATION) follows
    // the compositor renderer and is appended by `build_child_env`.
    "SDL_VIDEODRIVER=wayland,x11",
    "SDL_VIDEO_DRIVER=wayland,x11",
    "SDL_AUDIODRIVER=alsa",
    "SDL_AUDIO_DRIVER=alsa",
    // OpenAL (openal-soft: supertux2, gzdoom): Pulse first. PI-futexes are
    // implemented, so pa_mutex_new() no longer aborts when libpulse loads.
    "ALSOFT_DRIVERS=pulse,alsa",
    "PULSE_SERVER=unix:/run/pulse/native",
    // No D-Bus session bus on Eclipse OS. An UNSET address makes libdbus
    // `autolaunch:` -- fork dbus-launch, which opens $DISPLAY and spawns a
    // dbus-daemon plus a babysitter behind pipes -- and SDL_Init() walks
    // that chain (SDL_DBus_Init) before anything else; gzdoom hung there.
    // Pinned to the conventional user-bus path, the connect is refused at
    // once when no daemon runs and apps carry on bus-less; a dbus-daemon
    // bound to this path later is picked up by new clients automatically.
    "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
    // apk does not run glib-compile-schemas; even with a compiled blob, dconf
    // over D-Bus is a 25 s g_error for GTK portals. Memory backend + no AT-SPI
    // (we have no a11y session) stops at-spi-bus-launcher / portal-gtk SIGABRT.
    "GSETTINGS_BACKEND=memory",
    "GTK_A11Y=none",
    "NO_AT_BRIDGE=1",
    "GDK_BACKEND=wayland",
];

fn log(msg: &str) {
    // PID 1 has stdout/stderr wired to the console by the kernel.
    println!("[eclipse-init] {msg}");
}

fn main() {
    // Multi-call helper mode (no separate binary): `eclipse-init
    // --exec-on-graphics-vt CMD [ARGS...]` switches the display to the reserved
    // graphics VT (tty7), then execs CMD there. The labwc wrapper uses it so a
    // compositor launched BY HAND from a text VT lands on tty7 too (the boot
    // path already switches below). The flag is distinctive, so it can never
    // collide with the kernel's INIT= argv. Runs before ANY PID1 setup -- no
    // mounts, no signal handlers.
    {
        let argv: Vec<String> = std::env::args().collect();
        if argv.get(1).map(String::as_str) == Some("--exec-on-graphics-vt") {
            activate_graphics_vt();
            exec_argv(&argv[2..]);
            std::process::exit(127); // no command given, or execvp failed
        }
    }

    log("starting");

    mount_pseudo_filesystems();
    install_signal_handlers();

    // Align /proc/kbd, /etc/eclipse/keyboard and labwc's XKB_DEFAULT_LAYOUT
    // before the compositor starts, so the first keymap matches the console.
    apply_keyboard_layout();
    apply_locale();
    apply_timezone();

    let mut services = load_services(Path::new("/etc/eclipse/services"));

    // Pick the desktop session and drop every service tagged for a different
    // one, so only the selected compositor/X stack is supervised.
    let desktop = selected_desktop();
    log(&format!("desktop session: {desktop}"));
    if desktop == "none" {
        log("console/installer session: compositor services skipped");
    }
    services.retain(|_, s| s.desktop.as_deref().map_or(true, |d| d == desktop));

    // Move the display to the dedicated graphics VT (tty7) BEFORE starting the
    // compositor/X, so its libseat binds that reserved, shell-free VT instead of
    // sharing tty1 with the boot shell. The kernel then keeps the display on
    // tty7 once the session sets KD_GRAPHICS and returns to tty1 when it exits.
    if matches!(desktop.as_str(), "labwc" | "xorg") {
        switch_to_graphics_vt();
    }

    let order = ordered_names(&services);

    for name in &order {
        // Re-check shutdown between starts so a SIGTERM during boot is honoured.
        if WANT_HALT.load(Ordering::SeqCst) || WANT_REBOOT.load(Ordering::SeqCst) {
            break;
        }
        start_service(services.get_mut(name).expect("known service"));
    }

    log("entering supervision loop");
    supervise(&mut services);
}

/// Make tty7 (the reserved graphics VT) the active display via the VT ioctls on
/// `/dev/tty0` (the "current VT" control node): `VT_ACTIVATE` makes tty7 active
/// and `VT_WAITACTIVE` blocks until the switch lands, so the graphical session's
/// `libseat` binds it (libseat takes the active VT). Best-effort — on a build
/// without VT support the ioctls fail harmlessly and the session stays on the
/// current VT. Keep the VT number in sync with the kernel's `GRAPHICS_VT` (the
/// last of `NUM_VTS`, currently tty7). Returns whether `/dev/tty0` opened.
fn activate_graphics_vt() -> bool {
    const VT_ACTIVATE: libc::c_ulong = 0x5606;
    const VT_WAITACTIVE: libc::c_ulong = 0x5607;
    const GRAPHICS_VT: libc::c_int = 7; // tty7 == kernel GRAPHICS_VT + 1
    let Ok(path) = CString::new("/dev/tty0") else {
        return false;
    };
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return false;
    }
    // SAFETY: `fd` is a valid open tty; both VT ioctls take the VT number BY
    // VALUE (not a pointer), so passing the int directly is correct.
    unsafe {
        libc::ioctl(fd, VT_ACTIVATE as _, GRAPHICS_VT);
        libc::ioctl(fd, VT_WAITACTIVE as _, GRAPHICS_VT);
        libc::close(fd);
    }
    true
}

/// Boot-path wrapper around [`activate_graphics_vt`] that logs the outcome.
fn switch_to_graphics_vt() {
    if activate_graphics_vt() {
        log("display switched to graphics VT tty7");
    } else {
        log("note: could not open /dev/tty0; session stays on the current VT");
    }
}

/// `execvp` the given `argv` (program name first). Returns ONLY on failure (a
/// missing program, an interior NUL, or `execvp` erroring). Used by the
/// `--exec-on-graphics-vt` helper mode.
fn exec_argv(argv: &[String]) {
    let Some(prog) = argv.first() else {
        return;
    };
    let Ok(c_prog) = CString::new(prog.as_str()) else {
        return;
    };
    let c_args: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    if c_args.len() != argv.len() {
        return; // an argument held an interior NUL -- refuse a truncated argv
    }
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(core::ptr::null());
    // SAFETY: `c_prog` is a valid C string and `ptrs` is a NULL-terminated argv
    // of pointers into `c_args`, which outlive the call.
    unsafe {
        libc::execvp(c_prog.as_ptr(), ptrs.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// Pseudo-filesystems
// ---------------------------------------------------------------------------

/// Mount the standard pseudo-filesystems if they are not already present. The
/// Eclipse kernel already provides procfs/sysfs/devfs and treats these mounts
/// as successful no-ops, so this is cheap and idempotent; it is here so the
/// system is correct even on a kernel build where a mount point is empty.
fn mount_pseudo_filesystems() {
    // (source, target, fstype)
    let mounts = [
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("devtmpfs", "/dev", "devtmpfs"),
        ("tmpfs", "/run", "tmpfs"),
        ("tmpfs", "/tmp", "tmpfs"),
    ];
    for (src, target, fstype) in mounts {
        if !Path::new(target).exists() {
            let _ = fs::create_dir_all(target);
        }
        let c_src = CString::new(src).unwrap();
        let c_target = CString::new(target).unwrap();
        let c_fstype = CString::new(fstype).unwrap();
        // SAFETY: all pointers are valid NUL-terminated strings; data is null.
        let rc = unsafe {
            libc::mount(
                c_src.as_ptr(),
                c_target.as_ptr(),
                c_fstype.as_ptr(),
                0,
                core::ptr::null(),
            )
        };
        if rc != 0 {
            // Already mounted / kernel-provided: not fatal.
            log(&format!("note: mount {fstype} on {target} skipped"));
        }
    }

    // The Eclipse kernel treats the tmpfs mounts above as successful NO-OPS,
    // so on an installed root /run and /tmp are btrfs directories that SURVIVE
    // reboots. Stale sockets from the previous boot (`/run/seatd.sock`,
    // `wayland-0`) then pass the wrappers' `[ -S ]`/wait checks before the
    // daemons are actually listening — clients connect to a dead socket, exit,
    // and burn respawn backoffs; seatd/wlroots may also refuse to bind over a
    // pre-existing path. Clear both trees before any service starts. On a real
    // tmpfs (or the live RAM image) they are already empty and this is a no-op.
    clean_runtime_dir(Path::new("/run"));
    clean_runtime_dir(Path::new("/tmp"));

    // Wayland compositor socket dir (matches CHILD_ENV XDG_RUNTIME_DIR).
    let xdg_run = Path::new("/run/user/0");
    if !xdg_run.exists() {
        let _ = fs::create_dir_all(xdg_run);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(xdg_run, fs::Permissions::from_mode(0o700));
        }
    }
    // PulseAudio system-instance socket dir (PULSE_SERVER=unix:/run/pulse/native).
    // Also the per-user path libpulse looks at if PULSE_SERVER is unset.
    for d in ["/run/pulse", "/run/user/0/pulse"] {
        let p = Path::new(d);
        if !p.exists() {
            let _ = fs::create_dir_all(p);
        }
    }
}

/// Best-effort removal of stale runtime entries INSIDE `dir` (the directory
/// itself stays). Symlinks are removed as entries, never followed.
///
/// Critical: when cleaning `/run`, **preserve `/run/udev`**. The kernel writes
/// a synthetic udev database there (`/run/udev/data/c13:*`) so libudev/libinput
/// treat `/dev/input/event*` as initialized without a running udevd. Wiping it
/// made labwc's libinput backend enumerate zero devices; with
/// `WLR_LIBINPUT_NO_DEVICES=1` the compositor still started — but keyboard and
/// mouse stayed dead for the whole session (VT input kept working because the
/// console bypasses udev).
fn clean_runtime_dir(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let preserve_udev = dir == Path::new("/run");
    let mut removed = 0u32;
    let mut kept_udev = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if preserve_udev && entry.file_name() == *"udev" {
            kept_udev = true;
            continue;
        }
        let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let ok = if is_real_dir {
            fs::remove_dir_all(&path).is_ok()
        } else {
            fs::remove_file(&path).is_ok()
        };
        if ok {
            removed += 1;
        }
    }
    if removed > 0 || kept_udev {
        log(&format!(
            "cleared {removed} stale entr{} under {}{}",
            if removed == 1 { "y" } else { "ies" },
            dir.display(),
            if kept_udev {
                " (kept /run/udev for libinput)"
            } else {
                ""
            }
        ));
    }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

fn install_signal_handlers() {
    install_handler(libc::SIGTERM, on_sigterm as usize);
    install_handler(libc::SIGINT, on_sigint as usize);
    install_handler(libc::SIGUSR1, on_sigusr1 as usize);
    install_handler(libc::SIGUSR2, on_sigusr2 as usize);
    // SIGCHLD is left at its default: the blocking `waitpid` in the supervision
    // loop reaps children directly, so no handler is needed for reaping.
}

fn install_handler(sig: libc::c_int, handler: usize) {
    // SAFETY: zeroed sigaction with a valid handler pointer; standard install.
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = handler;
        libc::sigemptyset(&mut sa.sa_mask);
        // No SA_RESTART: we WANT `waitpid` to return EINTR so the loop notices
        // the shutdown flag promptly.
        sa.sa_flags = 0;
        libc::sigaction(sig, &sa, core::ptr::null_mut());
    }
}

// ---------------------------------------------------------------------------
// Desktop session selection
// ---------------------------------------------------------------------------

/// Which desktop session to start. Resolution order, first hit wins:
///   1. a `desktop=<name>` token on the kernel command line (`/proc/cmdline`) —
///      this is how `make qemu` selects the session while the same image, booted
///      on real hardware with the installed cmdline, gets none and falls through;
///      `desktop=none` (ISO installer) starts no compositor, so the live session
///      is a console plus `install-eclipse`;
///   2. the `/etc/eclipse/desktop` file (first whitespace token) — a persistent
///      per-install override the user can edit;
///   3. `labwc` — the default Eclipse session.
fn selected_desktop() -> String {
    if let Some(d) = cmdline_desktop() {
        return d;
    }
    if let Ok(text) = fs::read_to_string("/etc/eclipse/desktop") {
        if let Some(tok) = text.split_whitespace().next() {
            if !tok.is_empty() {
                return tok.to_string();
            }
        }
    }
    String::from("labwc")
}

/// Extract `desktop=<name>` from `/proc/cmdline`. The Eclipse kernel joins boot
/// arguments with `:` (e.g. `LOG=error:ROOT=/dev/vda:desktop=xorg`), but a plain
/// space-separated cmdline works too — split on both.
fn cmdline_desktop() -> Option<String> {
    let cmdline = fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split(|c: char| c == ':' || c.is_whitespace())
        .find_map(|tok| tok.strip_prefix("desktop="))
        .filter(|d| !d.is_empty())
        .map(String::from)
}

/// Apply the persisted / cmdline keyboard layout before any compositor starts.
/// `eclipse-kbd --boot` writes `/proc/kbd` and `XKB_DEFAULT_LAYOUT` but does
/// not SIGHUP labwc (it is not running yet). Missing script is not fatal: an
/// image built before this tool still boots, just with the compiled default.
fn apply_keyboard_layout() {
    match std::process::Command::new("/usr/local/bin/eclipse-kbd")
        .arg("--boot")
        .status()
    {
        Ok(st) if st.success() => {}
        Ok(st) => log(&format!("eclipse-kbd --boot exited {st}")),
        Err(e) => log(&format!("eclipse-kbd --boot skipped: {e}")),
    }
}

/// Apply `/etc/eclipse/locale` / cmdline `lang=` before any compositor starts.
/// Writes LANG/LANGUAGE into labwc's environment and selects `menu.xml`.
fn apply_locale() {
    match std::process::Command::new("/usr/local/bin/eclipse-locale")
        .arg("--boot")
        .status()
    {
        Ok(st) if st.success() => {}
        Ok(st) => log(&format!("eclipse-locale --boot exited {st}")),
        Err(e) => log(&format!("eclipse-locale --boot skipped: {e}")),
    }
}

/// `es` (default) or `en` from cmdline `lang=` then `/etc/eclipse/locale`.
fn resolved_ui_lang() -> &'static str {
    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline") {
        if let Some(v) = cmdline
            .split(|c: char| c == ':' || c.is_whitespace())
            .find_map(|tok| tok.strip_prefix("lang="))
        {
            match v.trim() {
                "en" | "EN" | "en_US" => return "en",
                "es" | "ES" | "es_ES" => return "es",
                _ => {}
            }
        }
    }
    if let Ok(text) = fs::read_to_string("/etc/eclipse/locale") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("lang=") {
                match v.trim() {
                    "en" | "EN" | "en_US" => return "en",
                    "es" | "ES" | "es_ES" => return "es",
                    _ => {}
                }
            }
        }
    }
    "es"
}

fn overlay_locale(env: &mut Vec<CString>) {
    env.retain(|e| {
        let s = e.to_str().unwrap_or("");
        !s.starts_with("LANG=") && !s.starts_with("LANGUAGE=") && !s.starts_with("LC_ALL=")
    });
    let (posix, language) = match resolved_ui_lang() {
        "en" => ("en_US.UTF-8", "en"),
        _ => ("es_ES.UTF-8", "es:en"),
    };
    env.push(CString::new(format!("LANG={posix}")).unwrap());
    env.push(CString::new(format!("LANGUAGE={language}")).unwrap());
}

fn apply_timezone() {
    match std::process::Command::new("/usr/local/bin/eclipse-tz")
        .arg("--boot")
        .status()
    {
        Ok(st) if st.success() => {}
        Ok(st) => log(&format!("eclipse-tz --boot exited {st}")),
        Err(e) => log(&format!("eclipse-tz --boot skipped: {e}")),
    }
}

fn tz_for_country(country: &str) -> &'static str {
    match country {
        "US" | "us" | "USA" | "usa" => "America/New_York",
        _ => "Europe/Madrid",
    }
}

/// `tz=` on the cmdline wins, then `country=`, then `/etc/eclipse/timezone`.
fn resolved_tz() -> String {
    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline") {
        if let Some(v) = cmdline
            .split(|c: char| c == ':' || c.is_whitespace())
            .find_map(|tok| tok.strip_prefix("tz=").map(str::trim))
        {
            if !v.is_empty() {
                return v.to_string();
            }
        }
        if let Some(v) = cmdline
            .split(|c: char| c == ':' || c.is_whitespace())
            .find_map(|tok| tok.strip_prefix("country=").map(str::trim))
        {
            if !v.is_empty() {
                return tz_for_country(v).to_string();
            }
        }
    }
    if let Ok(text) = fs::read_to_string("/etc/eclipse/timezone") {
        let mut country = None;
        let mut tz = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("tz=") {
                tz = Some(v.trim().to_string());
            }
            if let Some(v) = line.strip_prefix("country=") {
                country = Some(v.trim().to_string());
            }
        }
        if let Some(z) = tz {
            if !z.is_empty() {
                return z;
            }
        }
        if let Some(c) = country {
            return tz_for_country(&c).to_string();
        }
    }
    "Europe/Madrid".into()
}

fn overlay_tz(env: &mut Vec<CString>) {
    env.retain(|e| {
        let s = e.to_str().unwrap_or("");
        !s.starts_with("TZ=")
    });
    env.push(CString::new(format!("TZ={}", resolved_tz())).unwrap());
}

// ---------------------------------------------------------------------------
// Service files
// ---------------------------------------------------------------------------

/// Parse every `*.service` file in `dir` into a map keyed by service name (the
/// file stem). Malformed or empty (no `exec`) files are skipped with a warning
/// rather than aborting boot.
fn load_services(dir: &Path) -> BTreeMap<String, Service> {
    let mut out = BTreeMap::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            log(&format!("no service directory {} (nothing to start)", dir.display()));
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("service") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                log(&format!("warning: cannot read {}", path.display()));
                continue;
            }
        };
        match parse_service(&name, &text) {
            Some(svc) => {
                out.insert(name, svc);
            }
            None => log(&format!("warning: {} has no 'exec', skipped", path.display())),
        }
    }
    out
}

/// Parse a single service file. Format is line-based `key = value`, `#`
/// comments and blank lines ignored:
///   exec    = /usr/sbin/foo --flag    (required; whitespace-split into argv)
///   type    = respawn | oneshot       (default: oneshot)
///   after   = bar baz                 (optional; space-separated dep names)
///   desktop = labwc | xorg            (optional; only start under that session)
///   log     = /tmp/foo.log            (optional; capture child stdout/stderr)
fn parse_service(name: &str, text: &str) -> Option<Service> {
    let mut exec: Vec<String> = Vec::new();
    let mut kind = Kind::Oneshot;
    let mut after: Vec<String> = Vec::new();
    let mut desktop: Option<String> = None;
    let mut log_path: Option<String> = None;
    let mut wait_socket: Option<String> = None;
    let mut wait_path: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match key {
            "exec" => exec = value.split_whitespace().map(String::from).collect(),
            "type" => {
                kind = match value {
                    "respawn" => Kind::Respawn,
                    _ => Kind::Oneshot,
                }
            }
            "after" => after = value.split_whitespace().map(String::from).collect(),
            "desktop" => desktop = Some(value.to_string()),
            "log" => log_path = Some(value.to_string()),
            "wait_socket" => wait_socket = Some(value.to_string()),
            "wait_path" => wait_path = Some(value.to_string()),
            _ => {}
        }
    }

    if exec.is_empty() {
        return None;
    }
    Some(Service {
        name: name.to_string(),
        exec,
        kind,
        after,
        desktop,
        log: log_path,
        wait_socket,
        wait_path,
        pid: None,
        started_at: None,
        backoff: MIN_BACKOFF,
    })
}

/// Produce a start order honouring `after =` dependencies: a service is only
/// emitted once every dependency it lists has been emitted. Remaining services
/// (missing deps or dependency cycles) are appended in name order so a bad
/// `after =` never wedges boot.
fn ordered_names(services: &BTreeMap<String, Service>) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut pending: Vec<String> = services.keys().cloned().collect();

    loop {
        let mut progressed = false;
        let mut still_pending: Vec<String> = Vec::new();
        for name in pending {
            let deps = &services[&name].after;
            let ready = deps
                .iter()
                // A dep that doesn't exist can never be satisfied: ignore it
                // (treat as already-met) rather than deadlock.
                .all(|d| !services.contains_key(d) || order.contains(d));
            if ready {
                order.push(name);
                progressed = true;
            } else {
                still_pending.push(name);
            }
        }
        pending = still_pending;
        if pending.is_empty() {
            break;
        }
        if !progressed {
            // Cycle or unsatisfiable deps: emit the rest in name order.
            pending.sort();
            order.extend(pending);
            break;
        }
    }
    order
}

// ---------------------------------------------------------------------------
// Launching & supervision
// ---------------------------------------------------------------------------

/// Start a service. `oneshot` runs to completion (blocking) before returning;
/// `respawn` is forked and its pid recorded for the supervision loop.
fn start_service(svc: &mut Service) {
    // `after =` only orders the dependency's FORK; its socket may lag. Wait
    // natively here (both first start and crash-restarts pass through) so the
    // service doesn't die before its dependency is ready — and so no shell
    // wrapper has to fork a `sleep 0.1` busybox per poll instead. labwc keeps
    // its historical seatd wait even if its service file lacks the key.
    let wait = svc
        .wait_socket
        .clone()
        .or_else(|| (svc.name == "labwc").then(|| String::from("/run/seatd.sock")));
    if let Some(path) = wait {
        wait_for_socket(&path, Duration::from_secs(10));
    }
    // See `Service::wait_path`: input nodes for labwc. Always wait for
    // `/dev/input` when starting labwc, even if the service file is an older
    // image without `wait_path =` — without udevd there is no input hotplug.
    let wait_path = svc
        .wait_path
        .clone()
        .or_else(|| (svc.name == "labwc").then(|| String::from("/dev/input")));
    if let Some(path) = wait_path {
        wait_for_path(&path, Duration::from_secs(8));
        if Path::new(&path).is_dir() {
            wait_for_dir_settled(&path, Duration::from_secs(8), Duration::from_secs(1));
        }
    }
    match svc.kind {
        Kind::Oneshot => {
            log(&format!("oneshot: {}", svc.name));
            if let Some(pid) = spawn(&svc.exec, svc.log.as_deref()) {
                // Wait specifically for this child to finish.
                let mut status = 0;
                // SAFETY: pid is a child of ours.
                unsafe { libc::waitpid(pid, &mut status, 0) };
            }
        }
        Kind::Respawn => {
            log(&format!("respawn: {} (starting)", svc.name));
            svc.pid = spawn(&svc.exec, svc.log.as_deref());
            svc.started_at = Some(Instant::now());
        }
    }
}

/// Poll until `path` is a socket or `timeout` elapses (best-effort).
///
/// Fine-grained at first (10 ms) so the common case — seatd binds its socket
/// a few tens of ms after forking — releases the dependent service almost
/// immediately instead of rounding the wait up to a 100 ms slot; backs off to
/// 100 ms after the first second so a missing daemon costs no busy churn.
fn wait_for_socket(path: &str, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if is_unix_socket(path) {
            return;
        }
        let step = if start.elapsed() < Duration::from_secs(1) {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        };
        std::thread::sleep(step);
    }
    log(&format!("warning: {path} not ready after {timeout:?}"));
}

/// Poll `dir`'s listing until it has been NON-EMPTY and UNCHANGED for
/// `settle`, or `timeout` elapses. This is "device enumeration finished" for
/// a hotplug-less consumer: libinput scans `/dev/input` exactly once at
/// compositor startup, so labwc must not start while the kernel is still
/// mid-enumeration adding nodes one by one — waiting for the FIRST node let
/// labwc start between the keyboard (event0) and a slower-enumerating mouse,
/// which then stayed invisible for the whole session.
fn wait_for_dir_settled(dir: &str, timeout: Duration, settle: Duration) {
    let list = |d: &str| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(d)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort_unstable();
        names
    };
    let start = Instant::now();
    let mut last = list(dir);
    let mut stable_since = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(100));
        let now = list(dir);
        if now != last {
            last = now;
            stable_since = Instant::now();
            continue;
        }
        if !last.is_empty() && stable_since.elapsed() >= settle {
            log(&format!(
                "{dir} settled with {} entr{}",
                last.len(),
                if last.len() == 1 { "y" } else { "ies" }
            ));
            return;
        }
    }
    log(&format!(
        "warning: {dir} did not settle non-empty after {timeout:?} ({} entries)",
        last.len()
    ));
}

/// Poll until `path` exists (any file type) or `timeout` elapses. Same pacing
/// as [`wait_for_socket`]; used for device nodes (`wait_path =`).
fn wait_for_path(path: &str, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if Path::new(path).exists() {
            return;
        }
        let step = if start.elapsed() < Duration::from_secs(1) {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        };
        std::thread::sleep(step);
    }
    log(&format!("warning: {path} not present after {timeout:?}"));
}

fn is_unix_socket(path: &str) -> bool {
    let c_path = match CString::new(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    // SAFETY: path is a valid C string; st is stack-allocated.
    unsafe {
        let mut st: libc::stat = core::mem::zeroed();
        if libc::stat(c_path.as_ptr(), &mut st) != 0 {
            return false;
        }
        (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    }
}

/// fork + execv the given argv. Returns the child pid in the parent, or `None`
/// if the fork failed. In the child, signal dispositions are reset to default
/// and a fresh session is started before exec.
/// Sleep for `d`, returning early if a signal (a shutdown request) interrupts
/// it. `nanosleep` returns EINTR on a delivered signal, which is exactly what
/// lets a Ctrl-Alt-Del during a service backoff bring the system down promptly.
// `libc::time_t` is a deprecated alias (musl 1.2 widened it) but it is still the
// exact field type of `libc::timespec`, so the cast requires it; the value fits
// regardless of width.
#[allow(deprecated)]
fn sleep_interruptible(d: Duration) {
    let req = libc::timespec {
        tv_sec: d.as_secs() as libc::time_t,
        tv_nsec: d.subsec_nanos() as libc::c_long,
    };
    // SAFETY: nanosleep with a valid timespec and a null remainder pointer.
    unsafe {
        libc::nanosleep(&req, core::ptr::null_mut());
    }
}

/// How the desktop renders, chosen by the `renderer=` boot arg. The Eclipse
/// kernel joins boot args with `:` (e.g. `LOG=warn:desktop=labwc:renderer=gl`);
/// a plain space-separated cmdline works too.
enum Renderer {
    /// CPU 2D software (default, and the fallback for any unknown value): the
    /// wlroots pixman renderer. Always composites a frame.
    Pixman,
    /// Enable the NVIDIA nouveau experiment knobs. On current real hardware the
    /// default SESSION under this mode is still the safe software path; explicit
    /// `nvidia.wlr_gles2` / `nvidia.wlr_vulkan` cmdline flags opt into the
    /// unstable GPU-rendered compositor paths. In QEMU our virtio-gpu is 2D-only
    /// (no virgl), so this mode degrades to software GL there too.
    Gl,
    /// wlroots GLES2 over Mesa's software rasterizer (llvmpipe). Exercises the
    /// real GL/EGL/GLES2 path with no GPU 3D: it renders in QEMU (on the CPU, so
    /// slowly), and proves the compositor's whole GL stack end-to-end before
    /// hardware GL (virgl / nouveau) is wired underneath it.
    GlSw,
}

fn renderer_mode() -> Renderer {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let has = |tok: &str| cmdline.split([':', ' ', '\t', '\n']).any(|t| t == tok);
    // An explicit `renderer=` token always wins (checked most-specific first, so
    // `gl-sw` is not shadowed by `gl`). With no token, or `renderer=auto`, pick
    // from the GPU that is actually present.
    if has("renderer=pixman") {
        Renderer::Pixman
    } else if has("renderer=gl-sw") {
        Renderer::GlSw
    } else if has("renderer=gl") {
        Renderer::Gl
    } else {
        detect_renderer()
    }
}

/// Auto-pick the renderer from the GPU behind `/dev/dri/card0`, so ONE image
/// does the right thing in QEMU and on real hardware without a build flag
/// (`renderer=auto`, and the default when the cmdline names no renderer).
///
/// Per-GPU choice: NVIDIA (`0x10de`) enters the NVIDIA experiment mode — but
/// ONLY when `nvidia.nouveau_uapi` is also on the cmdline, because that flag is
/// what turns the kernel's nouveau uAPI on (without it the DRM node identifies
/// as "zcore" and NVK can never enumerate; NVIDIA without the flag goes to
/// pixman, the proven software default there). In that experiment mode labwc now
/// still defaults to software unless `nvidia.wlr_gles2` / `nvidia.wlr_vulkan`
/// opt into the unstable GPU compositor. Everything else — notably QEMU's
/// virtio-gpu (`0x1af4`), whose GL is virgl and is not wired into
/// auto-detection — stays on software GL (llvmpipe), which is safe everywhere
/// and never leaves a black screen. Only when no GPU is visible do we fall back
/// to pixman. To use virgl in QEMU, pass `renderer=gl` explicitly.
fn detect_renderer() -> Renderer {
    match fs::read_to_string("/sys/class/drm/card0/device/vendor") {
        Ok(v) if v.trim().eq_ignore_ascii_case("0x10de") => {
            // NVIDIA: nouveau GL composites on real hardware via zink+NVK (the
            // path this uAPI implements). build_child_env's Gl arm additionally
            // pins GL clients to zink so they take the same NVK path instead of
            // the unimplemented classic nvc0 GEM_PUSHBUF one (which would drop
            // them to llvmpipe, whose buffers are not nouveau objects).
            //
            // Same TWO-condition rule as the kernel and /etc/profile: the
            // NVIDIA GPU is the capability, `nvidia.nouveau_uapi` on the
            // cmdline is the request that actually TURNS THE KERNEL uAPI ON.
            // Without the flag the DRM node identifies as "zcore", NVK finds
            // 0 GPUs, and returning Gl here only bought a doomed zink probe
            // (labwc: EGL fails -> wlroots falls back to pixman anyway; GL
            // clients: a zink pin that can never work). In practice this arm
            // only runs WITHOUT the flag -- `GL=1` stamps `renderer=gl`
            // alongside the flag, so an explicit token wins before auto ever
            // gets asked -- but keying on the flag keeps a hand-written
            // `renderer=auto:nvidia.nouveau_uapi` cmdline honest too.
            let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
            if cmdline
                .split([':', ' ', '\t', '\n'])
                .any(|t| t == "nvidia.nouveau_uapi")
            {
                log_renderer_once(&format!(
                    "renderer=auto: NVIDIA GPU {} + nvidia.nouveau_uapi -> gl \
                     (NVIDIA experiment mode; labwc stays software unless explicitly opted into wlroots GPU rendering)",
                    v.trim()
                ));
                Renderer::Gl
            } else {
                log_renderer_once(&format!(
                    "renderer=auto: NVIDIA GPU {} but nvidia.nouveau_uapi is OFF (kernel uAPI \
                     disabled; DRM node is \"zcore\") -> pixman. Boot with GL=1 (or add \
                     nvidia.nouveau_uapi + renderer=gl to the cmdline) for hardware GL",
                    v.trim()
                ));
                Renderer::Pixman
            }
        }
        Ok(v) if !v.trim().is_empty() => {
            log_renderer_once(&format!(
                "renderer=auto: GPU vendor {} -> gl-sw (software GL; pass renderer=gl for virgl)",
                v.trim()
            ));
            Renderer::GlSw
        }
        _ => {
            log_renderer_once("renderer=auto: no GPU visible -> pixman");
            Renderer::Pixman
        }
    }
}

/// Is the GPU behind `/dev/dri/card0` an NVIDIA card (PCI vendor `0x10de`)?
/// Used to pin GL clients to zink+NVK on real hardware WITHOUT touching QEMU's
/// virtio-gpu (`0x1af4`), whose GL runs through virgl and has no Vulkan for
/// zink to sit on.
fn gpu_is_nvidia() -> bool {
    fs::read_to_string("/sys/class/drm/card0/device/vendor")
        .map(|v| v.trim().eq_ignore_ascii_case("0x10de"))
        .unwrap_or(false)
}

/// Does the kernel command line carry `token`? Same `:`/whitespace splitting
/// as [`renderer_mode`]. Used for opt-in knobs like `nvidia.wlr_vulkan`.
fn cmdline_has(token: &str) -> bool {
    fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .split([':', ' ', '\t', '\n'])
        .any(|t| t == token)
}

/// Inject `/lib/libeclipse_nvkick.so` at the front of `LD_PRELOAD` so GL/NVK
/// clients (lunarbar → glxgears) inherit the usermode kick without a login
/// shell sourcing `/etc/profile`. Does not replace an existing preload list.
fn prepend_nvkick_preload(env: &mut Vec<CString>) {
    const LIB: &str = "/lib/libeclipse_nvkick.so";
    if !Path::new(LIB).exists() {
        return;
    }
    if let Some(pos) = env.iter().position(|e| e.to_bytes().starts_with(b"LD_PRELOAD=")) {
        let cur = env[pos].to_string_lossy();
        let val = cur.strip_prefix("LD_PRELOAD=").unwrap_or("");
        if val.split(':').any(|p| p == LIB) {
            return;
        }
        let merged = if val.is_empty() {
            format!("LD_PRELOAD={LIB}")
        } else {
            format!("LD_PRELOAD={LIB}:{val}")
        };
        env[pos] = CString::new(merged).unwrap();
    } else {
        env.push(CString::new(format!("LD_PRELOAD={LIB}")).unwrap());
    }
}

/// The environment handed to every spawned service: the static [`CHILD_ENV`]
/// base plus the renderer pin. Pixman (CPU software) is the default because with
/// no working GL driver wlroots' GLES2 path leaves the desktop black — exactly
/// what happened when pixman was dropped unconditionally.
fn build_child_env() -> Vec<CString> {
    let mut env: Vec<CString> = CHILD_ENV
        .iter()
        .map(|e| CString::new(*e).unwrap())
        .collect();
    overlay_locale(&mut env);
    overlay_tz(&mut env);
    prepend_nvkick_preload(&mut env);
    match renderer_mode() {
        Renderer::Pixman => {
            env.push(CString::new("WLR_RENDERER=pixman").unwrap());
            env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
            push_sdl_render_env(&mut env, SdlRender::Software);
        }
        Renderer::Gl => {
            if gpu_is_nvidia() {
                // Current real-hardware status: labwc's default GPU-rendered path
                // still crashes inside Mesa/NVK and floods `/tmp/labwc.log` with
                // "zink: failed to create timeline semaphore", then the compositor
                // dies during teardown. Keep the kernel nouveau uAPI enabled (for
                // continued bring-up/debugging) but default the SESSION to the
                // proven software path so labwc actually reaches the desktop.
                //
                // Opt back into the unstable compositor GPU paths explicitly:
                //   * `nvidia.wlr_gles2`   -> wlroots GLES2 on zink+NVK
                //   * `nvidia.wlr_vulkan` -> wlroots native Vulkan/NVK
                //
                // Absent those flags, use pixman for the compositor and software
                // GL for clients. That avoids zink/NVK entirely on boot.
                if cmdline_has("nvidia.wlr_vulkan") || cmdline_has("nvidia.wlr_gles2") {
                    let wlr = if cmdline_has("nvidia.wlr_vulkan") {
                        "vulkan"
                    } else {
                        "gles2"
                    };
                    env.push(CString::new(format!("WLR_RENDERER={wlr}")).unwrap());
                    log_renderer_once(&format!(
                        "renderer=gl: NVIDIA GPU -> experimental WLR_RENDERER={wlr} \
                         (via nvidia.wlr_{})",
                        if wlr == "vulkan" { "vulkan" } else { "gles2" }
                    ));
                    // On real NVIDIA hardware, pin the OpenGL Gallium driver to
                    // zink (GL-on-Vulkan over NVK) only on the explicit
                    // experimental path. Our nouveau uAPI implements the zink/NVK
                    // submission path (VM_BIND/EXEC) but NOT classic nvc0
                    // GEM_PUSHBUF, so hardware GL clients need the pin whenever we
                    // intentionally exercise the GPU path.
                    env.push(CString::new("GALLIUM_DRIVER=zink").unwrap());
                    env.push(CString::new("MESA_LOADER_DRIVER_OVERRIDE=zink").unwrap());
                    push_sdl_render_env(&mut env, SdlRender::Gles2);
                    log_renderer_once(
                        "renderer=gl: NVIDIA GPU -> pinning GL clients to zink+NVK on explicit experimental path",
                    );
                } else {
                    env.push(CString::new("WLR_RENDERER=pixman").unwrap());
                    env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
                    env.push(CString::new("LIBGL_ALWAYS_SOFTWARE=1").unwrap());
                    push_sdl_render_env(&mut env, SdlRender::Software);
                    log_renderer_once(
                        "renderer=gl: NVIDIA GPU -> defaulting labwc to pixman and clients to software GL; opt into GPU rendering with nvidia.wlr_gles2 or nvidia.wlr_vulkan",
                    );
                }
            } else {
                // `renderer=gl` (GL=1) on a machine with NO NVIDIA GPU -- the
                // GL=1 image booted under QEMU. The hardware-GL path cannot
                // exist here (the kernel's own two-condition gate already left
                // the nouveau uAPI off; our virtio-gpu is 2D-only, no virgl),
                // and leaving the environment unpinned was WORSE than either
                // explicit mode: labwc's wrapper then defaulted WLR_RENDERER to
                // pixman while GL clients kept hardware-probing Mesa defaults,
                // and that mix rendered but never composited -- glxgears
                // printed its FPS to the console with no window ever appearing
                // (frames swapped into buffers the pixman compositor does not
                // take). Degrade to the SAME software-GL stack as
                // `renderer=gl-sw` (labwc on GLES2/llvmpipe, clients on
                // llvmpipe), which is exactly the QEMU configuration that
                // renders gears -- so the ONE `GL=1` image does the right thing
                // on both machines, mirroring what `renderer=auto` picks here.
                env.push(CString::new("WLR_RENDERER=gles2").unwrap());
                env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
                env.push(CString::new("LIBGL_ALWAYS_SOFTWARE=1").unwrap());
                push_sdl_render_env(&mut env, SdlRender::Gles2);
                log_renderer_once("renderer=gl: no NVIDIA GPU (QEMU/virtio) -> degrading to software GL (gl-sw stack: labwc GLES2 + llvmpipe clients)");
            }
        }
        Renderer::GlSw => {
            // Software GL: wlroots' GLES2 renderer over Mesa's llvmpipe.
            // LIBGL_ALWAYS_SOFTWARE sends Mesa straight to the software
            // rasterizer, so it never probes the 2D virtio-gpu for virgl (no
            // "virtio_gpu: driver missing"); WLR_RENDERER_ALLOW_SOFTWARE lets
            // wlroots accept the software GL context it otherwise rejects. If EGL
            // still fails to init, the next knobs are GALLIUM_DRIVER=llvmpipe and
            // MESA_LOADER_DRIVER_OVERRIDE=kms_swrast.
            env.push(CString::new("WLR_RENDERER=gles2").unwrap());
            env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
            env.push(CString::new("LIBGL_ALWAYS_SOFTWARE=1").unwrap());
            push_sdl_render_env(&mut env, SdlRender::Gles2);
            log_renderer_once("renderer=gl-sw: wlroots GLES2 over Mesa llvmpipe (software GL)");
        }
    }
    env
}

/// Which SDL render path the session's SDL clients (sdl12-compat, SDL2, SDL3)
/// should take. Follows the compositor renderer chosen above one-to-one.
#[derive(Clone, Copy)]
enum SdlRender {
    /// pixman session: SDL's CPU renderer, and NO GL behind
    /// `SDL_GetWindowSurface` (`SDL_FRAMEBUFFER_ACCELERATION=0`). SDL3 then
    /// blits over wl_shm exactly like foot/lunarbg; SDL2's Wayland backend has
    /// no shm framebuffer and still presents through EGL, which the session's
    /// `LIBGL_ALWAYS_SOFTWARE` keeps on llvmpipe.
    Software,
    /// gles2 / vulkan sessions: SDL's GLES2 renderer, on the same GL stack the
    /// compositor uses (zink+NVK on real hardware, llvmpipe under gl-sw). SDL2
    /// has no Vulkan renderer, so the vulkan session maps here as well.
    Gles2,
}

/// Append the renderer half of the SDL policy (the backend half is static, in
/// [`CHILD_ENV`]). Mirrors the labwc wrapper and /etc/profile: the three copies
/// must stay in step, or a shell-launched SDL app and an init-launched one would
/// render through different stacks.
fn push_sdl_render_env(env: &mut Vec<CString>, mode: SdlRender) {
    let (driver, fb) = match mode {
        SdlRender::Software => ("software", "0"),
        SdlRender::Gles2 => ("opengles2", "opengles2"),
    };
    env.push(CString::new(format!("SDL_RENDER_DRIVER={driver}")).unwrap());
    env.push(CString::new(format!("SDL_FRAMEBUFFER_ACCELERATION={fb}")).unwrap());
}

fn spawn(argv: &[String], log_path: Option<&str>) -> Option<i32> {
    let prog = CString::new(argv[0].as_str()).ok()?;
    let c_args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let mut p_args: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
    p_args.push(core::ptr::null());

    let c_env: Vec<CString> = build_child_env();
    let mut p_env: Vec<*const libc::c_char> = c_env.iter().map(|e| e.as_ptr()).collect();
    p_env.push(core::ptr::null());

    // SAFETY: standard fork/exec. The child only calls async-signal-safe libc
    // functions (signal reset, setsid, open/dup2, execve) before exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        log(&format!("error: fork failed for {}", argv[0]));
        return None;
    }
    if pid == 0 {
        unsafe {
            // Reset signals to default so the child isn't born with init's
            // handlers, and give it its own session/process group.
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGUSR1, libc::SIG_DFL);
            libc::signal(libc::SIGUSR2, libc::SIG_DFL);
            libc::setsid();
            // Detach from the console: stdin → /dev/null; stdout/stderr →
            // optional log file or /dev/null so service chatter never hits
            // the screen. init keeps the real console for its own lines.
            silence_stdio(log_path);
            libc::execve(prog.as_ptr(), p_args.as_ptr(), p_env.as_ptr());
            // execve only returns on failure.
            libc::_exit(127);
        }
    }
    Some(pid)
}

/// Redirect fds 0/1/2. stdin always `/dev/null`; stdout/stderr go to `log_path`
/// (append, create) when set, otherwise `/dev/null`. Async-signal-safe
/// (`open`/`dup2`/`close`); failures are ignored.
unsafe fn silence_stdio(log_path: Option<&str>) {
    let devnull = b"/dev/null\0";
    let null_fd = libc::open(devnull.as_ptr() as *const libc::c_char, libc::O_RDWR);
    if null_fd >= 0 {
        libc::dup2(null_fd, libc::STDIN_FILENO);
        if null_fd > libc::STDERR_FILENO {
            libc::close(null_fd);
        }
    }

    let out_fd = if let Some(path) = log_path {
        if let Ok(c_path) = CString::new(path) {
            libc::open(
                c_path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o644,
            )
        } else {
            -1
        }
    } else {
        -1
    };
    let out_fd = if out_fd >= 0 {
        out_fd
    } else {
        libc::open(devnull.as_ptr() as *const libc::c_char, libc::O_RDWR)
    };
    if out_fd >= 0 {
        libc::dup2(out_fd, libc::STDOUT_FILENO);
        libc::dup2(out_fd, libc::STDERR_FILENO);
        if out_fd > libc::STDERR_FILENO {
            libc::close(out_fd);
        }
    }
}

/// The PID 1 main loop: block in `waitpid`, reaping every child. A reaped
/// `respawn` service is restarted; orphans reparented to init are simply
/// reaped. A pending shutdown/reboot signal breaks out to `shutdown`.
fn supervise(services: &mut BTreeMap<String, Service>) {
    loop {
        if WANT_HALT.load(Ordering::SeqCst) {
            return shutdown(false, services);
        }
        if WANT_REBOOT.load(Ordering::SeqCst) {
            return shutdown(true, services);
        }

        let mut status = 0;
        // SAFETY: blocking wait for any child.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            let err = errno();
            if err == libc::EINTR {
                // A signal arrived; loop to re-check the shutdown flags.
                continue;
            }
            if err == libc::ECHILD {
                // No children to wait on: pause until the next signal so we are
                // not a busy loop. Returns on EINTR (a delivered signal).
                unsafe { libc::pause() };
                continue;
            }
            // Unexpected: avoid spinning.
            unsafe { libc::pause() };
            continue;
        }

        // Did a supervised respawn service just exit? Decide its restart delay,
        // then clear its pid; a single restart pass below respawns it. Splitting
        // "decide" from "restart" keeps the mutable borrow off the sleep.
        let mut delay = Duration::ZERO;
        if let Some(svc) = services.values_mut().find(|s| s.pid == Some(pid)) {
            let uptime = svc.started_at.map(|t| t.elapsed()).unwrap_or_default();
            svc.pid = None;
            // HOW it ended, not just when: a service that keeps "exiting after
            // 8 s" reads completely differently as `exit 0`, `exit 1` or
            // `signal 9`, and this line is the only record on a console-only
            // box. Uses libc's status decoding so a signal death is named.
            let how = if libc::WIFEXITED(status) {
                format!("exit {}", libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                format!("signal {}", libc::WTERMSIG(status))
            } else {
                format!("status {:#x}", status)
            };
            if uptime >= HEALTHY_UPTIME {
                // Up long enough to be healthy: restart now, reset the backoff.
                svc.backoff = MIN_BACKOFF;
                log(&format!(
                    "respawn: {} exited after {:?} ({}), restarting",
                    svc.name, uptime, how
                ));
            } else {
                // Exited almost immediately: back off so a broken or
                // not-yet-ready service cannot pin a CPU.
                delay = svc.backoff;
                svc.backoff = (svc.backoff * 2).min(MAX_BACKOFF);
                log(&format!(
                    "respawn: {} exited after {:?} ({}, crash), retry in {:?}",
                    svc.name, uptime, how, delay
                ));
            }
        }
        // Otherwise it was a oneshot's leftover or a reparented orphan: reaped.
        if !delay.is_zero() {
            // Interruptible by a shutdown signal; if one arrived, honour it
            // instead of respawning. Otherwise fall through to the restart pass
            // (NOT `continue`: with no other children the next waitpid would
            // ECHILD-pause and the backed-off service would never come back).
            sleep_interruptible(delay);
            if WANT_HALT.load(Ordering::SeqCst) || WANT_REBOOT.load(Ordering::SeqCst) {
                continue;
            }
        }
        // Restart pass: any respawn service now without a live pid is respawned.
        for svc in services.values_mut() {
            if svc.kind == Kind::Respawn && svc.pid.is_none() {
                svc.pid = spawn(&svc.exec, svc.log.as_deref());
                svc.started_at = Some(Instant::now());
            }
        }
    }
}

/// Ask the kernel to reboot (`reboot == true`) or power off.
///
/// This is the busybox `reboot -f` / `poweroff -f` path: `sync` then
/// `reboot(2)`. We deliberately do **not** `kill(-1, SIGTERM/SIGKILL)` first.
/// Tearing down labwc and the GPU clients before the syscall is what hung
/// "Reiniciar" on this kernel; `reboot -f` skipped that and worked. Device
/// quiesce (GSP-RM / WPR2, NVMe CC.SHN) already happens inside
/// `kernel_hal::cpu::reset` / `power_off`.
///
/// If the kernel cannot reboot, halt in a pause loop.
fn shutdown(reboot: bool, _services: &mut BTreeMap<String, Service>) {
    log(if reboot {
        "rebooting (force)"
    } else {
        "powering off (force)"
    });

    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGUSR2] {
            libc::signal(sig, libc::SIG_IGN);
        }

        libc::sync();

        let cmd = if reboot {
            libc::RB_AUTOBOOT
        } else {
            libc::RB_POWER_OFF
        };
        // A successful reboot(2) never returns.
        libc::reboot(cmd);
        log(&format!(
            "reboot syscall returned (errno {}); halting",
            errno()
        ));
        loop {
            libc::pause();
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn errno() -> libc::c_int {
    // SAFETY: __errno_location returns a valid pointer on musl/glibc.
    unsafe { *libc::__errno_location() }
}
