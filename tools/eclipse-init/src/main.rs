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

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// Compositor GPU-renderer fallback. With `nvidia.nouveau_uapi` on an NVIDIA
/// GPU the session defaults to GLES2/zink (real GPU). That path can die when a
/// client's EXEC wedges the GPU channel: the compositor is context 0, a
/// per-boot singleton the kernel rebuilds on owner exit (`ctx0_reset`), but a
/// wedged ring still kills labwc in a loop. Once labwc has exited
/// [`COMPOSITOR_DEGRADE_AFTER`] times this boot while the GPU renderer was
/// active, `build_child_env` hands later respawns `WLR_RENDERER=pixman` (the
/// proven software path) instead, so the desktop recovers. Force software for
/// the whole boot with `nvidia.wlr_pixman`; force native Vulkan with
/// `nvidia.wlr_vulkan`.
static COMPOSITOR_DEGRADED: AtomicBool = AtomicBool::new(false);
/// How many times labwc has exited this boot with the GPU renderer requested.
static COMPOSITOR_EXITS: AtomicU32 = AtomicU32::new(0);
/// Exits of the GPU-rendered compositor tolerated before degrading to pixman.
const COMPOSITOR_DEGRADE_AFTER: u32 = 2;

/// Whether this boot wants the GPU-rendered wlroots compositor (GLES2/zink or
/// Vulkan). True for `nvidia.nouveau_uapi` on NVIDIA unless
/// `nvidia.wlr_pixman` kills the path; `nvidia.wlr_vulkan` / `nvidia.wlr_gles2`
/// still force GPU when present.
fn gpu_compositor_requested() -> bool {
    gpu_compositor_requested_in(&read_cmdline())
}

/// [`gpu_compositor_requested`] against a given command line.
fn gpu_compositor_requested_in(cmdline: &str) -> bool {
    if cmdline_has_in(cmdline, "nvidia.wlr_pixman") {
        return false;
    }
    cmdline_has_in(cmdline, "nvidia.wlr_vulkan")
        || cmdline_has_in(cmdline, "nvidia.wlr_gles2")
        || cmdline_has_in(cmdline, "nvidia.nouveau_uapi")
}

/// Has a shutdown signal arrived? Every bounded wait polls this so a
/// Ctrl-Alt-Del lands promptly: the handlers are installed WITHOUT `SA_RESTART`
/// on purpose (see [`install_handler`]), and that only buys anything if the code
/// doing the waiting actually looks.
fn shutdown_requested() -> bool {
    WANT_HALT.load(Ordering::SeqCst) || WANT_REBOOT.load(Ordering::SeqCst)
}

/// Set by the SIGUSR1/SIGUSR2 handlers: bring the system down (halt/power off).
static WANT_HALT: AtomicBool = AtomicBool::new(false);
/// Set by the SIGTERM/SIGINT handlers: reboot. busybox `reboot` (without
/// `-f`) signals PID 1 with SIGTERM; Ctrl-Alt-Del is delivered as SIGINT.
static WANT_REBOOT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    // busybox `reboot` (without -f) signals PID 1 with SIGTERM — see halt.c:
    // halt/poweroff/reboot → SIGUSR1/SIGUSR2/SIGTERM. This used to request a
    // HALT, so `/bin/reboot`, `busybox reboot` or any script using the
    // absolute path powered the machine off instead of rebooting (only the
    // `/usr/local/bin/reboot` wrapper, which execs `reboot -f`, escaped it).
    WANT_REBOOT.store(true, Ordering::SeqCst);
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// If set, the service starts only when this token is present on the
    /// kernel command line. It is how a diagnostic can ship in every image and
    /// still cost a normal boot nothing: `cmdline = dbus.selftest` runs the
    /// session-bus probe only on a boot that asked for it.
    cmdline: Option<String>,
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
    // Firefox: the native Wayland backend for init-started children and
    // their descendants (lunarbar launches apps as ITS children, so this is
    // the environment a menu-launched browser sees). /etc/profile and the
    // labwc environment file carry the same pin.
    "MOZ_ENABLE_WAYLAND=1",
    // GTK from init-started children (a foot from the dock, and everything
    // typed into it): the gdk-pixbuf loader registry the gtk-caches oneshot
    // writes at boot, and no dconf. Only labwc's environment file carried
    // these, so a `firefox-esr` typed into a dock terminal decoded no image
    // ("Could not load a pixbuf from icon theme": `apk --no-scripts` never
    // wrote the system loaders.cache). /etc/profile and the wrapper carry
    // the same two.
    "GDK_PIXBUF_MODULE_FILE=/root/.cache/pixbuf-loaders.cache",
    "GSETTINGS_BACKEND=memory",
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
    // The D-Bus session bus. `dbus.service` runs a daemon on exactly this
    // path (Alpine's dbus-daemon when the image has it, eclipse-dbusd
    // otherwise), so clients now get a REAL bus: RequestName works, and with
    // it every single-instance check, GtkApplication and portal client.
    //
    // Pinning the address still matters even when the daemon is missing. An
    // UNSET address makes libdbus `autolaunch:` -- fork dbus-launch, which
    // opens $DISPLAY and spawns a dbus-daemon plus a babysitter behind pipes
    // -- and SDL_Init() walks that chain (SDL_DBus_Init) before it does
    // anything else; gzdoom hung there. Pinned, the connect is refused at
    // once and the app carries on bus-less instead of hanging.
    "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
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

    // Handlers first: `mount_pseudo_filesystems` wipes /run and /tmp and can
    // take a while, and a SIGTERM/SIGINT/SIGUSRx arriving before the handlers
    // exist hits SIG_DFL, which this kernel implements as terminate.
    install_signal_handlers();
    mount_pseudo_filesystems();

    // Align /proc/kbd, /etc/eclipse/keyboard and labwc's XKB_DEFAULT_LAYOUT
    // before the compositor starts, so the first keymap matches the console.
    apply_keyboard_layout();
    apply_locale();
    apply_timezone();
    apply_look();

    let mut services = load_services(Path::new("/etc/eclipse/services"));

    // Pick the desktop session and drop every service tagged for a different
    // one, so only the selected compositor/X stack is supervised.
    let desktop = selected_desktop();
    log(&format!("desktop session: {desktop}"));
    if desktop == "none" {
        log("console/installer session: compositor services skipped");
    }
    services.retain(|_, s| s.desktop.as_deref().is_none_or(|d| d == desktop));
    // `cmdline = <token>`: opt-in services (diagnostics) stay out of a normal
    // boot entirely.
    services.retain(|name, s| match s.cmdline.as_deref() {
        None => true,
        Some(token) => {
            let on = cmdline_has(token);
            if !on {
                log(&format!(
                    "{name}: skipped (needs '{token}' on the kernel cmdline)"
                ));
            }
            on
        }
    });

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
    install_handler(libc::SIGTERM, on_sigterm as *const () as usize);
    install_handler(libc::SIGINT, on_sigint as *const () as usize);
    install_handler(libc::SIGUSR1, on_sigusr1 as *const () as usize);
    install_handler(libc::SIGUSR2, on_sigusr2 as *const () as usize);
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
    selected_desktop_from(
        &read_cmdline(),
        fs::read_to_string("/etc/eclipse/desktop").ok().as_deref(),
    )
}

/// [`selected_desktop`] against a given command line and `/etc/eclipse/desktop`.
fn selected_desktop_from(cmdline: &str, file: Option<&str>) -> String {
    if let Some(d) = desktop_from_cmdline(cmdline) {
        return d;
    }
    if let Some(tok) = file.and_then(|t| t.split_whitespace().next()) {
        if !tok.is_empty() {
            return tok.to_string();
        }
    }
    String::from("labwc")
}

/// Extract `desktop=<name>` from a kernel command line. The Eclipse kernel joins
/// boot arguments with `:` (e.g. `LOG=error:ROOT=/dev/vda:desktop=xorg`), but a
/// plain space-separated cmdline works too: [`cmdline_value`] splits on both.
fn desktop_from_cmdline(cmdline: &str) -> Option<String> {
    cmdline_value(cmdline, "desktop=")
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
    ui_lang_from(
        &read_cmdline(),
        fs::read_to_string("/etc/eclipse/locale").ok().as_deref(),
    )
}

/// The two UI languages this image ships, by each spelling accepted for them. An
/// unrecognised value is NOT a language: it falls through to the next source, so
/// a stale `lang=fr` cannot pin the desktop to a locale with no translation.
fn ui_lang_token(value: &str) -> Option<&'static str> {
    match value.trim() {
        "en" | "EN" | "en_US" => Some("en"),
        "es" | "ES" | "es_ES" => Some("es"),
        _ => None,
    }
}

/// [`resolved_ui_lang`] against a given command line and `/etc/eclipse/locale`.
fn ui_lang_from(cmdline: &str, file: Option<&str>) -> &'static str {
    if let Some(lang) = cmdline_value(cmdline, "lang=").and_then(ui_lang_token) {
        return lang;
    }
    for line in file.unwrap_or_default().lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(lang) = line.strip_prefix("lang=").and_then(ui_lang_token) {
            return lang;
        }
    }
    "es"
}

fn overlay_locale(env: &mut Vec<CString>) {
    overlay_locale_for(env, resolved_ui_lang());
}

/// [`overlay_locale`] for an already-resolved language.
///
/// REPLACES rather than appends, and takes `LC_ALL` with it: two `LANG=` entries
/// in one environment are not "the last one wins" for every libc, and an
/// `LC_ALL` left behind beats whatever `LANG` says. The POSIX name has to be a
/// UTF-8 one because foot refuses to render under a plain `C` locale.
fn overlay_locale_for(env: &mut Vec<CString>, lang: &str) {
    env.retain(|e| {
        let s = e.to_str().unwrap_or("");
        !s.starts_with("LANG=") && !s.starts_with("LANGUAGE=") && !s.starts_with("LC_ALL=")
    });
    let (posix, language) = match lang {
        "en" => ("en_US.UTF-8", "en"),
        // Spanish falls back to English rather than to nothing, so a string
        // with no Spanish translation still comes out readable.
        _ => ("es_ES.UTF-8", "es:en"),
    };
    env.push(CString::new(format!("LANG={posix}")).unwrap());
    env.push(CString::new(format!("LANGUAGE={language}")).unwrap());
}

/// Apply `/etc/eclipse/look` / cmdline `look=` before the compositor starts:
/// the labwc theme name in `rc.xml` and the foot palette. The panel reads the
/// same file itself when it starts, so nothing else needs telling.
fn apply_look() {
    match std::process::Command::new("/usr/local/bin/eclipse-look")
        .arg("--boot")
        .status()
    {
        Ok(st) if st.success() => {}
        Ok(st) => log(&format!("eclipse-look --boot exited {st}")),
        Err(e) => log(&format!("eclipse-look --boot skipped: {e}")),
    }
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
    tz_from(
        &read_cmdline(),
        fs::read_to_string("/etc/eclipse/timezone").ok().as_deref(),
    )
}

/// [`resolved_tz`] against a given command line and `/etc/eclipse/timezone`.
///
/// An EMPTY value is not a value, at either source and under either key: a
/// truncated `tz=` line (a zero-filled tail after an unclean power cut, or a
/// half-written file) falls through to the next source instead of handing every
/// service a `TZ=` that libc reads as UTC.
fn tz_from(cmdline: &str, file: Option<&str>) -> String {
    if let Some(v) = cmdline_value(cmdline, "tz=").filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(v) = cmdline_value(cmdline, "country=").filter(|v| !v.is_empty()) {
        return tz_for_country(v).to_string();
    }
    let mut country = None;
    let mut tz = None;
    for line in file.unwrap_or_default().lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("tz=") {
            tz = Some(v.trim());
        }
        if let Some(v) = line.strip_prefix("country=") {
            country = Some(v.trim());
        }
    }
    if let Some(z) = tz.filter(|z| !z.is_empty()) {
        return z.to_string();
    }
    if let Some(c) = country.filter(|c| !c.is_empty()) {
        return tz_for_country(c).to_string();
    }
    "Europe/Madrid".into()
}

fn overlay_tz(env: &mut Vec<CString>) {
    overlay_tz_with(env, &resolved_tz());
}

/// [`overlay_tz`] for an already-resolved zone. The value comes from
/// `/etc/eclipse/timezone` or the command line, so a NUL byte in it (a
/// zero-filled tail after an unclean power cut) must not abort PID 1 on the
/// first spawn: leave `TZ` unset instead.
fn overlay_tz_with(env: &mut Vec<CString>, tz: &str) {
    env.retain(|e| {
        let s = e.to_str().unwrap_or("");
        !s.starts_with("TZ=")
    });
    if let Ok(tz) = CString::new(format!("TZ={tz}")) {
        env.push(tz);
    }
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
            log(&format!(
                "no service directory {} (nothing to start)",
                dir.display()
            ));
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
            None => log(&format!(
                "warning: {} has no 'exec', skipped",
                path.display()
            )),
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
///   cmdline = dbus.selftest           (optional; only start when the kernel
///                                      command line carries this token)
///   log     = /tmp/foo.log            (optional; capture child stdout/stderr)
fn parse_service(name: &str, text: &str) -> Option<Service> {
    let mut exec: Vec<String> = Vec::new();
    let mut kind = Kind::Oneshot;
    let mut after: Vec<String> = Vec::new();
    let mut desktop: Option<String> = None;
    let mut cmdline: Option<String> = None;
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
                    "oneshot" => Kind::Oneshot,
                    // A value that is neither still means oneshot, because
                    // defaulting to "supervise it forever" would be worse. But
                    // say so: `type = respwan` turns the compositor into a
                    // oneshot, so the desktop dies for good the first time it
                    // exits and NOTHING restarts it. Silently, before this line.
                    other => {
                        log(&format!(
                            "warning: {name}: 'type = {other}' is not respawn or oneshot;                              treating it as oneshot (it will NOT be restarted if it exits)"
                        ));
                        Kind::Oneshot
                    }
                }
            }
            "after" => after = value.split_whitespace().map(String::from).collect(),
            "desktop" => desktop = Some(value.to_string()),
            "cmdline" => cmdline = Some(value.to_string()),
            "log" => log_path = Some(value.to_string()),
            "wait_socket" => wait_socket = Some(value.to_string()),
            "wait_path" => wait_path = Some(value.to_string()),
            // Same reason: a misspelled key is a gate that does not exist.
            // `wait_sockt = /run/seatd.sock` used to be accepted in silence, and
            // then labwc raced seatd on every boot.
            other => log(&format!("warning: {name}: unknown key '{other}', ignored")),
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
        cmdline,
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
    if wait_until(timeout, || is_unix_socket(path), shutdown_requested) == Wait::TimedOut {
        log(&format!("warning: {path} not ready after {timeout:?}"));
    }
}

/// How a bounded wait ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    /// `ready` returned true.
    Ready,
    /// `stop` returned true: a shutdown was requested, so waiting is pointless.
    Stopped,
    /// `timeout` elapsed.
    TimedOut,
}

/// Poll `ready` until it holds, `stop` says to give up, or `timeout` elapses.
///
/// The three waits below share this so their pacing cannot drift apart, and so
/// they all honour a shutdown the same way. `sleep_interruptible`, NOT
/// `std::thread::sleep`: the latter RESTARTS itself on EINTR, which silently
/// undid the deliberate absence of `SA_RESTART` (see [`install_handler`]) and
/// left a Ctrl-Alt-Del during boot unanswered for as long as every wait took.
fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool, stop: impl Fn() -> bool) -> Wait {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if ready() {
            return Wait::Ready;
        }
        if stop() {
            return Wait::Stopped;
        }
        sleep_interruptible(poll_step(start.elapsed()));
    }
    // One last look: `ready` may have become true during the final sleep, and
    // reporting a timeout for something that IS there would send a service into
    // its backoff for nothing.
    if ready() {
        Wait::Ready
    } else {
        Wait::TimedOut
    }
}

/// How long to sleep between polls, given how long the wait has already run:
/// fine-grained (10 ms) for the first second so the common case releases almost
/// immediately, then 100 ms so a daemon that never arrives costs no churn. One
/// function, so the three waits below cannot drift apart.
fn poll_step(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(1) {
        Duration::from_millis(10)
    } else {
        Duration::from_millis(100)
    }
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
        if shutdown_requested() {
            log(&format!(
                "shutdown requested while waiting for {dir} to settle"
            ));
            return;
        }
        sleep_interruptible(Duration::from_millis(100));
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
    if wait_until(timeout, || Path::new(path).exists(), shutdown_requested) == Wait::TimedOut {
        log(&format!("warning: {path} not present after {timeout:?}"));
    }
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
    /// Enable the NVIDIA nouveau uAPI. On real NVIDIA hardware the session
    /// defaults to GLES2/zink; `nvidia.wlr_pixman` forces software and
    /// `nvidia.wlr_vulkan` selects native Vulkan. In QEMU our virtio-gpu is
    /// 2D-only (no virgl), so this mode degrades to software GL there too.
    Gl,
    /// wlroots GLES2 over Mesa's software rasterizer (llvmpipe). Exercises the
    /// real GL/EGL/GLES2 path with no GPU 3D: it renders in QEMU (on the CPU, so
    /// slowly), and proves the compositor's whole GL stack end-to-end before
    /// hardware GL (virgl / nouveau) is wired underneath it.
    GlSw,
}

/// Pick the renderer from a given command line and `card0` PCI vendor. `vendor`
/// is `None` when there is no `card0` at all.
fn renderer_mode_from(cmdline: &str, vendor: Option<&str>) -> Renderer {
    // An explicit `renderer=` token always wins (checked most-specific first, so
    // `gl-sw` is not shadowed by `gl`). With no token, or `renderer=auto`, pick
    // from the GPU that is actually present.
    if cmdline_has_in(cmdline, "renderer=pixman") {
        Renderer::Pixman
    } else if cmdline_has_in(cmdline, "renderer=gl-sw") {
        Renderer::GlSw
    } else if cmdline_has_in(cmdline, "renderer=gl") {
        Renderer::Gl
    } else {
        detect_renderer_from(vendor, cmdline)
    }
}

/// Auto-pick the renderer from the GPU behind `/dev/dri/card0`, so ONE image
/// does the right thing in QEMU and on real hardware without a build flag
/// (`renderer=auto`, and the default when the cmdline names no renderer).
///
/// Per-GPU choice: NVIDIA (`0x10de`) enters the NVIDIA GPU path — but ONLY when
/// `nvidia.nouveau_uapi` is also on the cmdline, because that flag is what
/// turns the kernel's nouveau uAPI on (without it the DRM node identifies as
/// "zcore" and NVK can never enumerate; NVIDIA without the flag goes to
/// pixman). With the flag, labwc defaults to GLES2/zink (kill-switch
/// `nvidia.wlr_pixman`; `nvidia.wlr_vulkan` for native Vulkan).
///
/// Everything else — QEMU virtio-gpu (`0x1af4`), VirtualBox SVGA (`0x15ad`),
/// or no card — lands on **pixman**. The previous auto pick was
/// GLES2/llvmpipe (`gl-sw`), which painted menus with tile garbage: `glFlush`
/// returns before llvmpipe's workers finish, this kernel has no timeline
/// syncobj for them to wait on, and the present scanned out half-drawn frames.
/// Pixman settles on the calling thread, so the buffer the kernel copies is
/// whole. Opt into the old software-GL stack with `renderer=gl-sw`, or virgl
/// with `renderer=gl`.
fn detect_renderer_from(vendor: Option<&str>, cmdline: &str) -> Renderer {
    match vendor {
        Some(v) if vendor_is_nvidia(Some(v)) => {
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
            if cmdline_has_in(cmdline, "nvidia.nouveau_uapi") {
                log(&format!(
                    "renderer=auto: NVIDIA GPU {} + nvidia.nouveau_uapi -> gl \
                     (GLES2/zink by default; nvidia.wlr_pixman for software)",
                    v.trim()
                ));
                Renderer::Gl
            } else {
                log(&format!(
                    "renderer=auto: NVIDIA GPU {} but nvidia.nouveau_uapi is OFF (kernel uAPI \
                     disabled; DRM node is \"zcore\") -> pixman. Boot with GL=1 (or add \
                     nvidia.nouveau_uapi + renderer=gl to the cmdline) for hardware GL",
                    v.trim()
                ));
                Renderer::Pixman
            }
        }
        Some(v) if !v.trim().is_empty() => {
            log(&format!(
                "renderer=auto: GPU vendor {} -> pixman (pass renderer=gl-sw for GLES2/llvmpipe, \
                 renderer=gl for virgl)",
                v.trim()
            ));
            Renderer::Pixman
        }
        _ => {
            log("renderer=auto: no GPU visible -> pixman");
            Renderer::Pixman
        }
    }
}

/// The PCI vendor id behind `/dev/dri/card0`, as sysfs spells it (`0x10de`).
/// `None` when there is no card0, which is not the same as a card that reports
/// an empty vendor: [`detect_renderer_from`] treats those differently.
fn card0_vendor() -> Option<String> {
    fs::read_to_string("/sys/class/drm/card0/device/vendor").ok()
}

/// Is `vendor` NVIDIA (PCI vendor `0x10de`)? One place, so the compositor gate
/// and the client-side zink pin can never disagree about what an NVIDIA card is.
fn vendor_is_nvidia(vendor: Option<&str>) -> bool {
    vendor.is_some_and(|v| v.trim().eq_ignore_ascii_case("0x10de"))
}

/// Does the kernel command line carry `token`? Same `:`/whitespace splitting
/// as [`renderer_mode`]. Used for opt-in knobs like `nvidia.wlr_vulkan`.
fn cmdline_has(token: &str) -> bool {
    cmdline_has_in(&read_cmdline(), token)
}

/// The kernel command line, or an empty string when `/proc` is not mounted yet.
/// One place to read it so every caller splits it the same way.
fn read_cmdline() -> String {
    fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

/// Does `cmdline` carry `token` as a WHOLE token? The Eclipse kernel joins boot
/// arguments with `:` (`LOG=warn:desktop=labwc:renderer=gl`); a plain
/// space-separated command line works too, so both separate. Whole-token
/// matching is what keeps `renderer=gl` from also firing on `renderer=gl-sw`.
fn cmdline_has_in(cmdline: &str, token: &str) -> bool {
    cmdline.split([':', ' ', '\t', '\n']).any(|t| t == token)
}

/// The value of the first `<key>=` token on `cmdline`, trimmed. `None` when the
/// key is absent; `Some("")` when it is present but empty, which every caller
/// treats as "not set" rather than as a value.
fn cmdline_value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline
        .split([':', ' ', '\t', '\n'])
        .find_map(|tok| tok.strip_prefix(key))
        .map(str::trim)
}

/// The environment handed to every spawned service: the static [`CHILD_ENV`]
/// base plus the renderer pin. Pixman (CPU software) is the default because with
/// no working GL driver wlroots' GLES2 path leaves the desktop black — exactly
/// what happened when pixman was dropped unconditionally.
fn build_child_env() -> Vec<CString> {
    let cmdline = read_cmdline();
    let vendor = card0_vendor();
    let mut env = child_env_for(
        renderer_mode_from(&cmdline, vendor.as_deref()),
        vendor.as_deref(),
        &cmdline,
        COMPOSITOR_DEGRADED.load(Ordering::Relaxed),
    );
    overlay_locale(&mut env);
    overlay_tz(&mut env);
    env
}

/// The renderer half of the child environment, as plain strings so a test can
/// read it. Every arm is a decision the `/etc/profile` block and the
/// `/usr/local/bin/labwc` wrapper (both written by `xtask`) must make the same
/// way: those two re-assert this policy for a session that init did NOT launch,
/// and a session that renders with a different renderer than its clients expect
/// composites nothing. Keep the three in step.
fn child_env_for(
    renderer: Renderer,
    vendor: Option<&str>,
    cmdline: &str,
    degraded: bool,
) -> Vec<CString> {
    let mut env: Vec<CString> = CHILD_ENV
        .iter()
        .map(|e| CString::new(*e).unwrap())
        .collect();
    match renderer {
        Renderer::Pixman => {
            env.push(CString::new("WLR_RENDERER=pixman").unwrap());
            env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
            push_sdl_render_env(&mut env, SdlRender::Software);
        }
        Renderer::Gl => {
            if vendor_is_nvidia(vendor) {
                // Real GPU path: `nvidia.nouveau_uapi` on an NVIDIA card defaults
                // to GLES2/zink (NVK). Kill-switch: `nvidia.wlr_pixman`. Native
                // Vulkan compositor: `nvidia.wlr_vulkan`. After
                // COMPOSITOR_DEGRADE_AFTER deaths this boot, respawns go to
                // pixman so the desktop recovers (see COMPOSITOR_DEGRADED).
                let force_pixman = degraded || cmdline_has_in(cmdline, "nvidia.wlr_pixman");
                let want_vulkan = cmdline_has_in(cmdline, "nvidia.wlr_vulkan");
                if !force_pixman
                    && (want_vulkan
                        || cmdline_has_in(cmdline, "nvidia.wlr_gles2")
                        || cmdline_has_in(cmdline, "nvidia.nouveau_uapi"))
                {
                    let wlr = if want_vulkan { "vulkan" } else { "gles2" };
                    env.push(CString::new(format!("WLR_RENDERER={wlr}")).unwrap());
                    env.push(CString::new("WLR_DRM_NO_MODIFIERS=1").unwrap());
                    log(&format!(
                        "renderer=gl: NVIDIA GPU -> WLR_RENDERER={wlr} (zink+NVK{}; degrade with nvidia.wlr_pixman)",
                        if want_vulkan {
                            ", nvidia.wlr_vulkan"
                        } else if cmdline_has_in(cmdline, "nvidia.wlr_gles2") {
                            ", nvidia.wlr_gles2"
                        } else {
                            ", default with nouveau_uapi"
                        }
                    ));
                    // Pin OpenGL clients to zink: our nouveau uAPI implements
                    // VM_BIND/EXEC (NVK) but not classic nvc0 GEM_PUSHBUF.
                    env.push(CString::new("GALLIUM_DRIVER=zink").unwrap());
                    env.push(CString::new("MESA_LOADER_DRIVER_OVERRIDE=zink").unwrap());
                    push_sdl_render_env(&mut env, SdlRender::Gles2);
                    log(
                        "renderer=gl: NVIDIA GPU -> pinning GL clients to zink+NVK",
                    );
                } else {
                    env.push(CString::new("WLR_RENDERER=pixman").unwrap());
                    env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
                    env.push(CString::new("LIBGL_ALWAYS_SOFTWARE=1").unwrap());
                    push_sdl_render_env(&mut env, SdlRender::Software);
                    if degraded {
                        log(
                            "renderer=gl: NVIDIA GPU -> compositor DEGRADED to pixman for the rest of this boot (labwc kept dying on the GPU renderer; the GPU channel is likely wedged -- see `dmesg | grep nouveau-uapi` and /tmp/labwc.log)",
                        );
                    } else {
                        log(
                            "renderer=gl: NVIDIA GPU -> pixman (nvidia.wlr_pixman); remove that flag for GLES2/zink",
                        );
                    }
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
                log("renderer=gl: no NVIDIA GPU (QEMU/virtio) -> degrading to software GL (gl-sw stack: labwc GLES2 + llvmpipe clients)");
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
            log("renderer=gl-sw: wlroots GLES2 over Mesa llvmpipe (software GL)");
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
    // Same treatment as argv[0]: an interior NUL in an `exec =` argument
    // (corrupted .service file) fails this spawn instead of aborting init.
    let c_args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(a.as_str()).ok())
        .collect::<Option<Vec<_>>>()?;
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
            // The Rust runtime sets SIGPIPE to SIG_IGN before `main`, and an
            // ignored disposition survives execve: without this every
            // supervised daemon and `eclipse-*` wrapper script ran with
            // SIGPIPE ignored (pipelines got EPIPE instead of terminating).
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
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
            // GPU-renderer fallback for the compositor (see COMPOSITOR_DEGRADED):
            // count labwc's exits while nvidia.wlr_gles2/vulkan is requested and,
            // past the tolerance, flip later respawns to pixman. Counted on EVERY
            // exit, not only fast ones: the first labwc instance can live for
            // minutes (the desktop works until a client wedges the GPU channel)
            // and the respawns on the dead channel may also linger before dying,
            // so an uptime-gated "crash" count would never trip.
            if svc.name == "labwc"
                && !COMPOSITOR_DEGRADED.load(Ordering::Relaxed)
                && gpu_compositor_requested()
            {
                let n = COMPOSITOR_EXITS.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= COMPOSITOR_DEGRADE_AFTER {
                    COMPOSITOR_DEGRADED.store(true, Ordering::Relaxed);
                    log(&format!(
                        "respawn: labwc died {n}x this boot on the GPU renderer ({how}); \
                         degrading the compositor to pixman for the rest of this boot -- \
                         the GPU channel is likely wedged (a client's EXEC hung it and \
                         ctx 0 is never rebuilt); see `dmesg | grep nouveau-uapi` and /tmp/labwc.log"
                    ));
                } else {
                    log(&format!(
                        "respawn: labwc died ({how}) on the GPU renderer; retrying it \
                         ({n}/{COMPOSITOR_DEGRADE_AFTER} exits before degrading to pixman)"
                    ));
                }
            }
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
        // Restart pass: any respawn service now without a live pid is restarted
        // through the normal launcher so crash-restarts re-apply wait_socket /
        // wait_path gates exactly like the first boot start.
        // Walk in dependency order (`after =`), not BTreeMap alphabetical
        // order: "labwc" < "seatd", so a crash of both restarted labwc first,
        // which then parked ~10 s on the seatd socket gate (or launched
        // against a dead seatd and crashed again) before seatd was retried.
        for name in ordered_names(services) {
            if let Some(svc) = services.get_mut(&name) {
                if svc.kind == Kind::Respawn && svc.pid.is_none() {
                    start_service(svc);
                }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// This binary is PID 1 of the installed system: it mounts, picks the session,
// orders and supervises every service, and builds the environment the compositor
// runs in. It is a standalone package (its own `[workspace]`, its own musl
// target, built best-effort by `xtask`), so no `cargo` line in the tree ever
// compiled it, let alone tested it -- a build failure here silently leaves
// busybox init as PID 1 and the desktop never starts.
//
// What is covered is the decision-making: the boot-argument parsing, the
// session/locale/timezone precedence, the service-file parser, the dependency
// order, and the renderer policy. What is NOT is everything that talks to the
// kernel (mounts, VT ioctls, fork/exec, reboot) -- that needs to BE init.
#[cfg(test)]
mod tests {
    use super::*;

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
    /// static base too, so a `firefox-esr` typed into foot decodes images.
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
}
