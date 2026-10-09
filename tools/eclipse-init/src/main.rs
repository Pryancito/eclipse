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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
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

/// How many starts a respawn service gets while its `exec =` DOES NOT EXIST
/// before init gives up on it for the rest of the boot.
///
/// A missing x bit init repairs ([`repair_exec_mode`]); a missing *file* no
/// amount of retrying can fix, because nothing creates it between two
/// `execve`s. Retrying it anyway is what filled a real console with the same
/// four lines every 8 s for a whole boot, hiding everything else that was
/// printed -- and `/usr/local/bin` is on the installed btrfs root, which a
/// kernel upgrade never rewrites, so "for ever" means exactly that. Three
/// tries, because a path on a filesystem a dependency is still mounting is
/// conceivable and costs about a second to rule out.
const MISSING_EXEC_TRIES: u32 = 3;

/// How long a `oneshot` may run before init stops waiting for it and carries
/// on with the boot. The one limit nothing else in here provides: every
/// bounded wait in this file (`wait_socket`, `wait_path`) has a timeout, but
/// the `waitpid` on a oneshot's child used to have none, so a single wrapper
/// that never exits hung the WHOLE boot -- no later service started, the
/// supervision loop was never reached, and because the shutdown flags are only
/// read in there, Ctrl-Alt-Del did nothing either. The machine was wedged by
/// one hanging script.
///
/// 90 s, the same default as systemd's `TimeoutStartSec`. SMF makes a method
/// timeout mandatory, OpenRC and launchd have start/exit timeouts; of the
/// supervisors this init borrows from, only runit leaves a stuck one-shot
/// alone. Per service with `timeout = <seconds>`, and `timeout = 0` (or
/// `none`) waits for ever, for the one that genuinely needs to.
const DEFAULT_ONESHOT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long an overrun oneshot gets between SIGTERM and SIGKILL, and then
/// between SIGKILL and init giving up on reaping it. Short: by the time this
/// runs the service has already had its whole timeout.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// How many times a respawn service may exit WITHOUT ever staying up
/// [`HEALTHY_UPTIME`] before init stops restarting it for the rest of the
/// boot.
///
/// The backoff ([`MAX_BACKOFF`]) stops a crash loop pinning a CPU, but on its
/// own it never ends: a service that cannot work prints its death every 8 s
/// for as long as the machine is on, and that console is the only diagnostic
/// output the box has -- the storm is what hid everything else on a real boot.
/// Every other supervisor stops: systemd fails the unit past
/// `StartLimitBurst`, SMF moves it to `maintenance` when it is "restarting too
/// quickly", launchd throttles it. Deliberately far more patient than any of
/// them (systemd gives 5 tries in 10 s): with the backoff doubling to 8 s,
/// twenty crashes is about two and a half minutes of trying, so nothing that
/// is merely slow to find its dependency gets written off. The counter resets
/// the moment the service stays up past [`HEALTHY_UPTIME`].
const CRASH_START_LIMIT: u32 = 20;

/// How many times a respawn service may be STARTED inside
/// [`START_LIMIT_INTERVAL`] before init stops restarting it for the rest of
/// the boot -- whatever its uptime and whatever its exit code.
///
/// [`CRASH_START_LIMIT`] only ever fires on a service that never once stayed
/// up [`HEALTHY_UPTIME`], and that leaves a whole family of immortal loops
/// through: a wrapper that SLEEPS before giving up. Every `eclipse-*` wrapper
/// in the images does it -- `eclipse-pulseaudio` is
///
/// ```text
/// command -v pulseaudio >/dev/null 2>&1 || { echo ...; sleep 8; exit 127; }
/// ```
///
/// so each try lives 8 s, reads as a healthy run, clears `crash_starts` and is
/// restarted at once with its backoff reset. On a `minimal` boot (no
/// pulseaudio installed) that is, for as long as the machine is on:
///
/// ```text
/// respawn: pulseaudio exited after 8.125682158s (exit 127 ...), restarting
/// respawn: pulseaudio exited after 8.032575278s (exit 127 ...), restarting
/// ```
///
/// A hand-rolled sleep inside the service is exactly what the supervisor's own
/// backoff is for, and it disables both the backoff and the give-up limit. The
/// wrappers keep their sleeps (they only pace the retries now); what had to
/// change is that the SUPERVISOR counts starts, which no sleep can hide.
const START_LIMIT_BURST: u32 = 20;

/// The window [`START_LIMIT_BURST`] is counted over.
///
/// Long on purpose, because the loops this catches are slow: the wrappers
/// sleep up to 60 s before exiting, so a tighter window than systemd's (5
/// starts in 10 s) would never see them. Twenty starts in half an hour is a
/// service that is not working and will not start working, and a respawn
/// service that is healthy does not restart at all -- every restart is a
/// death. A service that dies once an hour never trips this.
const START_LIMIT_INTERVAL: Duration = Duration::from_secs(1800);

/// Where a respawn service's output goes when its file names no `log =`.
/// `/tmp`, like every `log =` the images ship: a tmpfs, so it costs no disk
/// and is empty again on the next boot.
const DEFAULT_LOG_DIR: &str = "/tmp";

/// How big a service's `log =` may get before init rotates it.
///
/// Every log the images ship lives in `/tmp`, which is a **tmpfs**: its bytes
/// are RAM. Nothing bounded them, so a service that writes on every turn of a
/// crash loop -- or a chatty daemon left running for a day -- ate the
/// machine's memory, and the hole was worst exactly where it hurts most,
/// because the service that cannot stop crashing is the one logging hardest.
/// Every other supervisor caps this: `svlogd` and `s6-log` rotate by size,
/// journald has `SystemMaxUse`.
///
/// 1 MiB, with one old generation kept, so a service costs at most ~2 MiB of
/// RAM however long the machine is up -- and 1 MiB is still tens of thousands
/// of lines, far more than anyone reads.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// How often init looks at the sizes. A minute: a service would have to write
/// a megabyte within it to overshoot, and the sweep is one `stat` per distinct
/// log file.
const LOG_SWEEP_SECS: libc::c_uint = 60;

/// Set by the SIGALRM handler: it is time to look at the log sizes. The
/// supervision loop blocks in `waitpid` for as long as nothing is pending, so
/// the alarm is what gets it to look at all -- and `install_handler` installs
/// no `SA_RESTART`, so the signal interrupts that block instead of being
/// swallowed by it.
static WANT_LOG_SWEEP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigalrm(_sig: libc::c_int) {
    WANT_LOG_SWEEP.store(true, Ordering::SeqCst);
}

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

/// Written when the session's compositor is on pixman although a GPU renderer
/// was asked for; `/run` is wiped at boot, so it lasts one boot. The labwc
/// wrapper writes it too (xtask `write_labwc_wrapper`).
const RENDERER_FALLBACK_MARKER: &str = "/run/labwc-renderer-fallback";

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

/// What a signal asks PID 1 to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Request {
    /// Come back up: busybox `reboot` (SIGTERM) and Ctrl-Alt-Del (SIGINT).
    Reboot,
    /// Stay down: busybox `halt` (SIGUSR1) and `poweroff` (SIGUSR2).
    Halt,
}

/// Every signal PID 1 answers, its handler, and what that handler asks for.
///
/// A table rather than four `install_handler` calls, because the mapping is
/// the whole policy and a swapped row is invisible from anywhere else: this
/// used to have SIGTERM asking for a HALT, so `/bin/reboot`, `busybox reboot`
/// and every script using the absolute path powered the machine off instead of
/// rebooting it. See busybox's halt.c for the sending side.
const SIGNAL_HANDLERS: &[(libc::c_int, extern "C" fn(libc::c_int), Request)] = &[
    (libc::SIGTERM, on_sigterm, Request::Reboot),
    (libc::SIGINT, on_sigint, Request::Reboot),
    (libc::SIGUSR1, on_sigusr1, Request::Halt),
    (libc::SIGUSR2, on_sigusr2, Request::Halt),
];

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
    /// Names of services this one CANNOT work without: systemd's `Requires=`
    /// to `after =`'s `After=`. When a requirement is given up on for the boot
    /// (a missing `exec =`, or [`CRASH_START_LIMIT`] crashes), so is this
    /// service -- see [`blocked_by_requirements`]. Every name here is also an
    /// `after =`, so a requirement is always ordered first.
    requires: Vec<String>,
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
    /// `timeout = <seconds>` from the service file: how long a `oneshot` may
    /// run before init stops waiting for it (see [`start_timeout`]). `None`
    /// means the file said nothing and the default applies.
    timeout: Option<Limit>,
    /// Live child pid for a running `respawn` service.
    pid: Option<i32>,
    /// When the current child was last started (for crash-loop backoff).
    started_at: Option<Instant>,
    /// Current restart delay for a crashing respawn service; grows on repeated
    /// fast exits, resets once the service stays up past [`HEALTHY_UPTIME`].
    backoff: Duration,
    /// Earliest instant this service may be started again, set when it exits.
    ///
    /// A DEADLINE and not a sleep: the backoff used to be served by blocking
    /// PID 1 in `nanosleep` right after reaping the crasher, which stopped it
    /// reaping anything else for the length of the backoff. The uptime of
    /// every other service that died during that window was then measured to
    /// the moment it was finally REAPED, so a service that failed `execve` and
    /// `_exit(127)`ed in a millisecond was credited with the whole backoff,
    /// read as "stayed up past HEALTHY_UPTIME", restarted at once and had its
    /// own backoff reset -- for ever. Seen on hardware as a console filling
    /// with, over and over:
    ///
    /// ```text
    /// respawn: dbus-system exited after 46.334126ms (exit 127, crash), retry in 8s
    /// respawn: oopslog exited after 8.049671271s (exit 127), restarting
    /// ```
    ///
    /// where `oopslog` is an infinite `while :; do ... sleep 10; done` loop
    /// that cannot run for 8 s and exit 127: the 8.049 s is `dbus-system`'s
    /// 8 s backoff plus the 49 ms `oopslog` actually lived.
    restart_at: Option<Instant>,
    /// How many times this service has been started while its `exec =` did not
    /// exist. Counted in [`start_service`]; at [`MISSING_EXEC_TRIES`] the
    /// service is given up on (`given_up`).
    missing_starts: u32,
    /// Given up on for the rest of this boot: never started again, and left
    /// out of the restart pass. Set for an `exec =` that does not exist, and
    /// for a service that has hit [`CRASH_START_LIMIT`].
    given_up: bool,
    /// How many times in a row this service has exited without ever staying
    /// up [`HEALTHY_UPTIME`]. Reset by the first healthy run; at
    /// [`CRASH_START_LIMIT`] the service is given up on (see [`note_crash`]).
    crash_starts: u32,
    /// When this service was started, within the last [`START_LIMIT_INTERVAL`]
    /// (see [`note_start`]). Nothing resets it: a healthy run clears
    /// `crash_starts`, but a service that is restarted twenty times in half an
    /// hour is not healthy however long each try lived.
    starts: VecDeque<Instant>,
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
    // these, so a `firefox` typed into a dock terminal decoded no image
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
    // anything else. That chain was not what hung gzdoom (two kernel bugs
    // were: see docs/README-desktop.md), but pinned, the connect is refused
    // at once and the app skips the fork/exec/pipe detour entirely.
    "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus",
];

// ── The boot timeline ──────────────────────────────────────────────────────
//
// "Can the boot be made faster?" cannot be answered by reading the code: the
// expensive parts of this init are not the ones that compute, they are the ones
// that WAIT -- a `wait_socket =` for a daemon that is still linking itself, a
// `wait_path =` for device nodes the kernel has not enumerated yet, the settle
// window after them, a `oneshot` that runs a shell script. None of those were
// measured, and none of them are visible in a console log, because until now
// init's lines carried no time at all: a reader could see the ORDER of the boot
// and had no way to see its SHAPE.
//
// So: every line init prints is stamped with its offset from PID 1's first
// statement, every wait is timed, and at the end of the boot the stretches are
// printed worst-first with what they do not account for. The cost is one
// `Instant::now()` per wait and a `Vec` of a few dozen short strings.

/// When PID 1 started: the `+0.000` of every stamped line and of the timeline.
///
/// `OnceLock` and not a plain `static mut`: set once at the top of `main`, read
/// from `log`, which runs on every line including ones printed from inside a
/// `catch_unwind`. A read before it is set (the multi-call helper path, which
/// never gets as far as `main`'s PID 1 setup) prints an unstamped line rather
/// than a wrong one.
static BOOT_T0: OnceLock<Instant> = OnceLock::new();

/// One measured stretch of the boot: a wait, or a step that runs to completion.
///
/// Only LEAVES are recorded -- a gate, a settle window, a oneshot's run -- never
/// a wrapper around several of them. The whole point of the table is that the
/// rows add up to something less than the total and the difference is real
/// unaccounted time; nesting one row inside another would let the rows sum past
/// the boot and make `rest` meaningless.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BootStep {
    /// What was waited for, as the row should read.
    what: String,
    /// When it started, since init's first statement.
    at: Duration,
    /// How long it took.
    took: Duration,
}

/// The stretches recorded so far. Rendered once, at the end of the boot.
static BOOT_STEPS: Mutex<Vec<BootStep>> = Mutex::new(Vec::new());

/// Set once the timeline has been rendered. From then on nothing is recorded:
/// a `respawn` that crashes an hour later goes through the same gates as it did
/// at boot ([`start_service`] serves both), and those waits are not boot --
/// recording them would grow this vector for the life of the machine and
/// rewrite the history of a boot that is already over.
static BOOT_RECORDED: AtomicBool = AtomicBool::new(false);

/// Hard cap on recorded stretches, so a boot that somehow loops through
/// [`start_service`] many times before the timeline is rendered cannot grow
/// this without bound. Generous: a full desktop boot records about thirty.
const BOOT_STEPS_MAX: usize = 512;

/// Stretches shorter than this are left out of the table and land in `rest`.
/// A boot has a long tail of sub-millisecond steps, and a table that lists
/// them is one nobody reads to the end.
const BOOT_STEP_FLOOR: Duration = Duration::from_millis(5);

/// Where the timeline is left for later reading. The console scrolls, and the
/// question "where did my boot go" is usually asked on a machine that is
/// already up. `/run` is a tmpfs init has just mounted.
const BOOT_TIMELINE_PATH: &str = "/run/eclipse-boot-timeline";

/// Time since PID 1's first statement, or `None` before `main` set the origin.
fn since_boot() -> Option<Duration> {
    BOOT_T0.get().map(Instant::elapsed)
}

/// Run `f`, record how long it took as one row of the timeline, and answer its
/// value.
fn timed<T>(what: impl Into<String>, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    note_step(what, start.elapsed());
    value
}

/// Record one stretch of the boot.
fn note_step(what: impl Into<String>, took: Duration) {
    if BOOT_RECORDED.load(Ordering::SeqCst) {
        return;
    }
    let ended_at = since_boot().unwrap_or_default();
    // Poisoned is not a reason to lose the measurement: `unwind` means a panic
    // anywhere in a start can poison this, and a timeline with a hole in it is
    // worse than one taken from a boot that had a bug in it.
    let mut steps = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner());
    if steps.len() >= BOOT_STEPS_MAX {
        return;
    }
    steps.push(BootStep {
        what: what.into(),
        at: ended_at.saturating_sub(took),
        took,
    });
}

/// Render the timeline, stop recording, and leave it in
/// [`BOOT_TIMELINE_PATH`]. Called once, at the end of the start loop.
fn report_boot_timeline() {
    let total = since_boot().unwrap_or_default();
    let steps = {
        let steps = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner());
        steps.clone()
    };
    // After the render, not before it: a step still in flight when this runs
    // (there is none today, but `start_service` is called under `guard`) would
    // otherwise be dropped silently.
    BOOT_RECORDED.store(true, Ordering::SeqCst);
    let text = render_boot_timeline(&steps, total);
    for line in text.lines() {
        log(line);
    }
    let _ = fs::write(BOOT_TIMELINE_PATH, &text);
}

/// The boot timeline as plain text: the stretches that cost something, worst
/// first, and the time no row accounts for.
///
/// Worst-first and not chronological, because the table answers one question --
/// what is there to cut -- and the console lines above it, now stamped, already
/// give the order. `rest` is everything the rows do not cover: the forks and
/// execs, the sub-[`BOOT_STEP_FLOOR`] tail, and anything that was never timed,
/// which is the number that says whether this instrumentation is still missing
/// something.
fn render_boot_timeline(steps: &[BootStep], total: Duration) -> String {
    let named: Duration = steps.iter().map(|s| s.took).sum();
    let mut out = String::new();
    out.push_str(&format!(
        "boot timeline: {} to the supervision loop, {} of it waiting\n",
        secs(total),
        secs(named.min(total))
    ));
    let mut rows: Vec<&BootStep> = steps.iter().filter(|s| s.took >= BOOT_STEP_FLOOR).collect();
    // Longest first; ties broken by when they happened, so a table of equal
    // rows is stable and reads in boot order.
    rows.sort_by(|a, b| b.took.cmp(&a.took).then(a.at.cmp(&b.at)));
    for step in &rows {
        out.push_str(&format!(
            "  {:>8}  {:>3}%  at {:>8}  {}\n",
            secs(step.took),
            percent_of(step.took, total),
            secs(step.at),
            step.what
        ));
    }
    // `saturating_sub`: `total` is read after the steps, so it cannot be the
    // smaller of the two today -- but a future caller that reads it first would
    // otherwise get a row claiming the rest of the boot took 584 million years.
    let rest = total.saturating_sub(named);
    out.push_str(&format!(
        "  {:>8}  {:>3}%  {}\n",
        secs(rest),
        percent_of(rest, total),
        "          everything not timed above (forks, execs, short steps)"
    ));
    out
}

/// Seconds with three decimals: `  1.234s`. Milliseconds would need a second
/// unit for the long waits, and a boot is read in seconds.
fn secs(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

/// `part` as a whole percentage of `total`, saturating at 100 and answering 0
/// for a zero total (a timeline rendered before the clock moved).
fn percent_of(part: Duration, total: Duration) -> u64 {
    let total = total.as_micros();
    if total == 0 {
        return 0;
    }
    ((part.as_micros() * 100) / total).min(100) as u64
}

/// Print one line on the console, stamped with its offset from the start of
/// the boot.
///
/// The stamp is the cheapest half of making the boot measurable: every line
/// this init has ever printed said WHAT happened and nothing about when, so a
/// pasted boot log showed the order of the boot and hid its shape -- a
/// ten-second gate and a ten-millisecond one looked identical. Unstamped only
/// before `main` sets the origin (the `--exec-on-graphics-vt` helper path).
fn log(msg: &str) {
    // PID 1 has stdout/stderr wired to the console by the kernel.
    match since_boot() {
        Some(t) => println!("[eclipse-init] [{:>8}] {msg}", secs(t)),
        None => println!("[eclipse-init] {msg}"),
    }
}

/// How long init waits before re-entering a supervision loop that panicked, so
/// a panic on the very first statement cannot become a hot loop printing
/// itself.
const PANIC_PAUSE: Duration = Duration::from_secs(1);

/// Run `f`; if it panics, say so on the console and answer `None` instead of
/// taking the machine down.
///
/// A panic in PID 1 is the worst outcome in the system: the kernel answers a
/// dead init with "Attempted to kill init" and the whole machine stops, for a
/// bug in one decision -- an index, an arithmetic edge, an `unwrap` on
/// something the disk said. Every other supervisor is a process the kernel can
/// afford to lose; this one is not, which is why the release profile is
/// `panic = "unwind"` and not `abort` (see Cargo.toml) and why the two places
/// that can run arbitrary decision code -- starting a service, and the
/// supervision loop itself -- run inside this.
///
/// `AssertUnwindSafe` because the state that crosses it is init's own service
/// map, with no invariant a half-applied field can break: a service whose
/// `pid` was set but whose `started_at` was not is read as "started just now",
/// which is what the next reap would have concluded anyway. The one shared
/// thing that unwinding really can poison is `RENDERER_SAID`, and
/// [`log_renderer`] already treats a poisoned lock as "say the line".
fn guard<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            // The payload is already on the console: the hook installed in
            // `main` printed the message and the location before unwinding.
            log(&format!(
                "BUG: panicked while {what}; init is still running -- please report this"
            ));
            None
        }
    }
}

/// Lines about the renderer policy already printed this boot.
static RENDERER_SAID: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Log a renderer-policy line the FIRST time it is decided, and never again.
///
/// The policy is computed per `execve` ([`build_child_env`] runs inside
/// [`spawn`]), so these lines used to be reprinted for every service start --
/// and a service stuck in a respawn loop reprinted the whole renderer block
/// every backoff, which is what buried the one line that said what was
/// actually wrong. The decision itself is what is worth reading, so each
/// distinct line is printed once: a decision that CHANGES (the compositor
/// degrading to pixman) is a different line and still gets said.
fn log_renderer(msg: &str) -> bool {
    let fresh = RENDERER_SAID
        .lock()
        .map(|mut seen| seen.insert(msg.to_string()))
        // A poisoned mutex must not cost the line: say it.
        .unwrap_or(true);
    if fresh {
        log(msg);
    }
    fresh
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

    // The origin of every stamped line and of the boot timeline. Taken before
    // the first line is printed, so nothing in the boot is outside the clock.
    let _ = BOOT_T0.set(Instant::now());
    log("starting");

    // Name the panic on the console before it unwinds: the default hook writes
    // to stderr, which for PID 1 is the console too, but without saying who
    // panicked -- and on a console-only machine that line is the whole bug
    // report. `guard` catches it right after this runs.
    std::panic::set_hook(Box::new(|info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| String::from("unknown location"));
        log(&format!("BUG: panic at {where_}: {info}"));
    }));

    // Handlers first: `mount_pseudo_filesystems` wipes /run and /tmp and can
    // take a while, and a SIGTERM/SIGINT/SIGUSRx arriving before the handlers
    // exist hits SIG_DFL, which this kernel implements as terminate.
    install_signal_handlers();
    // Timed, all of it: `mount_pseudo_filesystems` wipes /run and /tmp, and the
    // four `apply_*` steps read and rewrite files under /etc and /proc. None of
    // it had ever been measured, and "init took a second before it started
    // anything" is exactly the kind of thing that hides there.
    timed(
        "mount the pseudo-filesystems, wipe /run and /tmp",
        mount_pseudo_filesystems,
    );

    // Align /proc/kbd, /etc/eclipse/keyboard and labwc's XKB_DEFAULT_LAYOUT
    // before the compositor starts, so the first keymap matches the console --
    // and the locale and the timezone with them, since every service init
    // spawns from here on inherits them (see `child_env_for`).
    apply_boot_settings();
    timed("apply the look", apply_look);

    let mut services = timed("read the service files", || {
        load_services(Path::new("/etc/eclipse/services"))
    });

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
        // `VT_WAITACTIVE` blocks until the switch lands, so this is a wait and
        // belongs in the timeline like any other.
        timed(
            "switch the display to the graphics VT",
            switch_to_graphics_vt,
        );
    }

    let order = ordered_names(&services);

    for name in &order {
        // Re-check shutdown between starts so a SIGTERM during boot is honoured.
        if WANT_HALT.load(Ordering::SeqCst) || WANT_REBOOT.load(Ordering::SeqCst) {
            break;
        }
        // A requirement given up on during this very loop (a missing `exec =`
        // is noticed at its first start) writes off what is behind it before
        // the boot gets there.
        report_given_up(&mut services);
        if services.get(name).is_some_and(|s| s.given_up) {
            continue;
        }
        // Guarded: a bug in one service's start must cost that service, not the
        // boot. `get_mut` rather than an `expect`, so a name that somehow is
        // not in the map is a line on the console and not a dead machine.
        let Some(svc) = services.get_mut(name) else {
            log(&format!(
                "BUG: {name} vanished from the service table; skipped"
            ));
            continue;
        };
        guard(&format!("starting {name}"), || start_service(svc));
    }

    // Everything declared has been started (or given up on): the boot proper is
    // over, so this is where its timeline is complete.
    report_boot_timeline();
    log("entering supervision loop");
    // PID 1 may not return and may not die, so a panic in here is caught and
    // the loop re-entered. The pause is what keeps a panic on the first
    // statement from becoming a console-filling hot loop.
    while guard("supervising", || supervise(&mut services)).is_none() {
        log("re-entering the supervision loop after a panic");
        sleep_interruptible(PANIC_PAUSE);
    }
}

/// How many text consoles the kernel opens. The session has to land past them.
const TEXT_VTS: libc::c_int = 6;

/// The VT the session runs on: tty7, the first one clear of the text consoles
/// (tty7 == the kernel's own GRAPHICS_VT + 1). Written as the derivation
/// rather than as a 7, and at module scope because [`activate_graphics_vt`]
/// opens `/dev/tty0`, which a test has not got.
const GRAPHICS_VT: libc::c_int = TEXT_VTS + 1;

/// Checked here rather than in a test because it is decidable at compile time:
/// a session sharing a VT with a text console puts a getty's output on top of
/// the compositor.
const _: () = assert!(GRAPHICS_VT > TEXT_VTS);

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
    let Some(c_args) = argv_for_exec(argv) else {
        return;
    };
    let ptrs = exec_ptrs(&c_args);
    // SAFETY: `c_args[0]` is a valid C string and `ptrs` is a NULL-terminated
    // argv of pointers into `c_args`, which outlives the call.
    unsafe {
        libc::execvp(c_args[0].as_ptr(), ptrs.as_ptr());
    }
}

/// `argv` as C strings, or `None` if it cannot be passed to `execvp` whole.
///
/// An argument holding an interior NUL is refused rather than dropped: a
/// truncated argv is a DIFFERENT command, and running it would be worse than
/// running nothing. Split out because [`exec_argv`] only returns when it has
/// failed, so no test can call it and come back.
fn argv_for_exec(argv: &[String]) -> Option<Vec<CString>> {
    if argv.is_empty() {
        return None;
    }
    let c_args: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    (c_args.len() == argv.len()).then_some(c_args)
}

/// The NULL-terminated pointer array `execvp` reads. The terminator is the
/// whole point: without it `execvp` walks off the end of the allocation.
fn exec_ptrs(args: &[CString]) -> Vec<*const libc::c_char> {
    let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(core::ptr::null());
    ptrs
}

// ---------------------------------------------------------------------------
// Pseudo-filesystems
// ---------------------------------------------------------------------------

/// The pseudo-filesystems PID 1 mounts, as `(source, target, fstype)`. A
/// const rather than a local, because [`mount_pseudo_filesystems`] MOUNTS: no
/// test can call it, so the table is the only part of it a test can hold.
const PSEUDO_MOUNTS: &[(&str, &str, &str)] = &[
    ("proc", "/proc", "proc"),
    ("sysfs", "/sys", "sysfs"),
    ("devtmpfs", "/dev", "devtmpfs"),
    ("tmpfs", "/run", "tmpfs"),
    ("tmpfs", "/tmp", "tmpfs"),
];

/// The trees wiped before any service starts, for the reason below.
const RUNTIME_DIRS: &[&str] = &["/run", "/tmp"];

/// `XDG_RUNTIME_DIR`'s mode. The specification requires it be reachable by
/// its owner and nobody else, so no group or other bit may be set here.
const XDG_RUNTIME_MODE: u32 = 0o700;

/// Mount the standard pseudo-filesystems if they are not already present. The
/// Eclipse kernel already provides procfs/sysfs/devfs and treats these mounts
/// as successful no-ops, so this is cheap and idempotent; it is here so the
/// system is correct even on a kernel build where a mount point is empty.
fn mount_pseudo_filesystems() {
    for (src, target, fstype) in PSEUDO_MOUNTS.iter().copied() {
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
    for d in RUNTIME_DIRS {
        clean_runtime_dir(Path::new(d));
    }

    // Wayland compositor socket dir (matches CHILD_ENV XDG_RUNTIME_DIR).
    let xdg_run = Path::new("/run/user/0");
    if !xdg_run.exists() {
        let _ = fs::create_dir_all(xdg_run);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(xdg_run, fs::Permissions::from_mode(XDG_RUNTIME_MODE));
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
    clean_runtime_dir_keeping(dir, keep_under(dir));
}

/// The one entry of `/run` that must survive the wipe. Named here rather than
/// inside the loop so the rule is a value a test can hold: the decision used
/// to be tied to the literal `/run`, which no test can clean.
const KEEP_UNDER_RUN: Option<&str> = Some("udev");

/// What [`clean_runtime_dir`] keeps when sweeping `dir`. A function rather
/// than a line inside the sweep, because the sweep REMOVES: a test that
/// checked this rule in place would have to wipe the real `/run`.
fn keep_under(dir: &Path) -> Option<&'static str> {
    KEEP_UNDER_RUN.filter(|_| dir == Path::new("/run"))
}

/// [`clean_runtime_dir`] with the entry to keep given rather than derived, so
/// the sweep can be pointed at a scratch directory.
fn clean_runtime_dir_keeping(dir: &Path, keep: Option<&str>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut removed = 0u32;
    let mut kept_udev = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if keep.is_some_and(|k| entry.file_name() == *k) {
            kept_udev = true;
            continue;
        }
        // The `false` fallback is unreachable in practice (`file_type` reads
        // the `d_type` readdir already returned, and falls back to an `lstat`
        // of an entry that was just listed), so `true` here behaves the same;
        // it is the safe side anyway, since a plain `remove_file` leaves a
        // directory in place instead of taking a tree with it.
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
            // Plural only: swapping the two arms changes the log line and
            // nothing else.
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
    for (sig, handler, _) in SIGNAL_HANDLERS {
        install_handler(*sig, *handler as *const () as usize);
    }
    // SIGALRM: the only clock PID 1 has. It is what wakes the supervision loop
    // to rotate the logs (see MAX_LOG_BYTES); the handler does nothing but set
    // the flag, and the loop re-arms it.
    install_handler(libc::SIGALRM, on_sigalrm as *const () as usize);
    arm_log_sweep();
    // SIGCHLD is left at its default: the blocking `waitpid` in the supervision
    // loop reaps children directly, so no handler is needed for reaping.
}

/// Ask for the next log sweep. One-shot, so the loop re-arms it after each one:
/// an interval timer would keep firing while init is mid-shutdown.
fn arm_log_sweep() {
    // SAFETY: `alarm` only schedules a SIGALRM for this process.
    unsafe { libc::alarm(LOG_SWEEP_SECS) };
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

/// Kernel command line token that puts the three boot settings back to one
/// after another. The escape hatch for [`apply_boot_settings`]: if running them
/// at once ever misbehaves on a machine, `init.serial_setup` restores exactly
/// the old order without a rebuild.
const SERIAL_SETUP: &str = "init.serial_setup";

/// The three boot settings, as (what the timeline calls it, the program).
///
/// A table and not three calls because they are run as a group, and because the
/// one thing that must stay true of them -- that they are INDEPENDENT of each
/// other -- is easier to see as a list than as three statements that happen to
/// be adjacent.
const BOOT_SETTINGS: [(&str, &str); 3] = [
    ("the keyboard layout", "/usr/local/bin/eclipse-kbd"),
    ("the locale", "/usr/local/bin/eclipse-locale"),
    ("the timezone", "/usr/local/bin/eclipse-tz"),
];

/// Apply the keyboard layout, the locale and the timezone, all three at once.
///
/// These are three shell scripts, each of which forks eight or ten busybox
/// applets of its own (`awk`, `tr`, `dd`, `grep`, `mv`), and run one after
/// another they were **85% of init's whole boot**: 1.13 s of 1.46 s on the
/// measured QEMU boot that `docs/README-boot.md` records, against 150 ms for
/// everything else init does before the first service. On this kernel a
/// `fork`/`execve` is the expensive operation, and this is thirty of them in a
/// row on the critical path, with PID 1 blocked in `waitpid` for all of it.
///
/// Nothing orders them: each reads its own `/etc/eclipse` file and the kernel
/// command line, each writes its own file, and none reads anything another
/// writes. The one thing they share is labwc's `environment`, which all three
/// upsert a key into -- and three read-modify-writes of one file at once lose
/// keys, so the scripts now take a bounded lock around exactly that (see
/// `write_eclipse_kbd` and its siblings in xtask, and the tests that hold the
/// lock against them). That lock is what makes this sound; without it this
/// function would be a silent keyboard-layout bug one boot later.
///
/// They still all finish before this returns, because the first service init
/// starts must already see the locale and the timezone in its environment.
/// What changes is only that the three waits overlap.
fn apply_boot_settings() {
    if cmdline_has(SERIAL_SETUP) {
        log(&format!(
            "{SERIAL_SETUP}: applying the boot settings one at a time"
        ));
        for (what, prog) in BOOT_SETTINGS {
            timed(format!("apply {what}"), || {
                wait_boot_setting(what, prog, spawn_boot_setting(prog))
            });
        }
        return;
    }
    // Fork all three, THEN wait: a loop that waited inside it would be the
    // serial version with extra words.
    let started: Vec<(&str, &str, Option<std::process::Child>)> = BOOT_SETTINGS
        .iter()
        .map(|(what, prog)| (*what, *prog, spawn_boot_setting(prog)))
        .collect();
    timed(
        "apply the keyboard layout, the locale and the timezone (at once)",
        || {
            for (what, prog, child) in started {
                wait_boot_setting(what, prog, child);
            }
        },
    );
}

/// Start one boot setting, or say on the console why it could not start.
fn spawn_boot_setting(prog: &str) -> Option<std::process::Child> {
    match std::process::Command::new(prog).arg("--boot").spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            // Not fatal and never has been: an image without the desktop stack
            // has no such script, and the boot carries on with the defaults.
            log(&format!("{prog} --boot skipped: {e}"));
            None
        }
    }
}

/// Wait for one boot setting and report a non-zero exit, exactly as the three
/// separate `status()` calls used to.
fn wait_boot_setting(what: &str, prog: &str, child: Option<std::process::Child>) {
    let Some(mut child) = child else { return };
    match child.wait() {
        Ok(st) if st.success() => {}
        Ok(st) => log(&format!("{prog} --boot exited {st} ({what} may be wrong)")),
        Err(e) => log(&format!("{prog} --boot could not be waited for: {e}")),
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
        // As in `parse_service`, the `#` arm is belt and braces: `#lang=en`
        // does not start with `lang=` either.
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
    let mut requires: Vec<String> = Vec::new();
    let mut desktop: Option<String> = None;
    let mut cmdline: Option<String> = None;
    let mut log_path: Option<String> = None;
    let mut wait_socket: Option<String> = None;
    let mut wait_path: Option<String> = None;
    let mut timeout: Option<Limit> = None;

    for line in text.lines() {
        let line = line.trim();
        // The `#` arm is belt and braces: a commented-out setting still has
        // its `#` glued to the key (`#exec`, `# exec`), so it falls through the
        // match below as an unknown key either way.
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
            "requires" => requires = value.split_whitespace().map(String::from).collect(),
            "desktop" => desktop = Some(value.to_string()),
            "cmdline" => cmdline = Some(value.to_string()),
            "log" => log_path = Some(value.to_string()),
            "timeout" => match parse_limit(value) {
                Some(limit) => timeout = Some(limit),
                // Keep the default rather than guess: a typo here would
                // otherwise silently remove the only limit on a oneshot that
                // hangs, which is the whole point of the key.
                None => log(&format!(
                    "warning: {name}: 'timeout = {value}' is not a number of seconds \
                     (or 0/none for no limit); using the default"
                )),
            },
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
    // A requirement is an ordering too, so `requires =` alone is enough and
    // cannot be written in a way that starts the dependent first. systemd keeps
    // `Requires=` and `After=` independent and this is the trap it leaves: a
    // unit that requires another but is not ordered after it starts beside it.
    for name in &requires {
        if !after.iter().any(|a| a == name) {
            after.push(name.clone());
        }
    }
    // A supervised service with nowhere to write keeps its reason to itself.
    // Its stdout/stderr go to /dev/null (see `silence_stdio`), so when it dies
    // the only thing anybody has is the number on the console -- and the
    // wrapper scripts' own diagnostics go to `/dev/console`, which the forked
    // child may not be able to open at all. `oopslog.service` shipped with no
    // `log =` and that is exactly how it went: `exit 127` on repeat with
    // nothing anywhere saying which command was not found. Default one rather
    // than leave the hole open for the next service file too; an explicit
    // `log =` still wins, and a `log = /dev/null` still opts out.
    //
    // Respawn only: a oneshot runs once and its failure is reported by the
    // boot step that waited for it.
    if kind == Kind::Respawn && log_path.is_none() {
        log_path = Some(format!("{DEFAULT_LOG_DIR}/{name}.log"));
    }
    Some(Service {
        name: name.to_string(),
        exec,
        kind,
        after,
        requires,
        desktop,
        cmdline,
        log: log_path,
        timeout,
        wait_socket,
        wait_path,
        pid: None,
        started_at: None,
        backoff: MIN_BACKOFF,
        restart_at: None,
        missing_starts: 0,
        given_up: false,
        starts: VecDeque::new(),
        crash_starts: 0,
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
            // Cycle or unsatisfiable deps: emit the rest in name order. The
            // sort is already satisfied by construction -- `pending` starts as
            // a `BTreeMap`'s keys and only ever has entries removed -- and is
            // kept so the guarantee does not rest on the map's type.
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

/// Why an absolute `exec =` cannot be run, if it cannot, as a line to log.
///
/// `execve` failures happen in the forked child, which can only `_exit(127)`:
/// its stdio is already `/dev/null`, so the reason never reaches the console
/// and all the supervisor sees is "exited after 400us (exit 127, crash)" every
/// MAX_BACKOFF for the rest of the boot. That exact storm shipped -- the
/// installer wrote `/usr/local/bin/eclipse-oopslog` 0644, so the service
/// respawned forever on EACCES with nothing saying "not executable" anywhere.
/// Checking before the fork costs one `stat` and names the cause.
///
/// Only absolute paths are checked: a bare `exec = seatd` goes through
/// `execvp`'s PATH search, which this cannot replicate, and a wrong answer
/// there would be worse than none. Returns `None` when there is nothing to
/// report, including every case this cannot decide.
///
/// See [`repair_exec_mode`] for the one case init does not merely report.
fn exec_problem(prog: &str) -> Option<String> {
    if !prog.starts_with('/') {
        return None;
    }
    let path = Path::new(prog);
    let Ok(meta) = fs::metadata(path) else {
        return Some(format!("{prog} does not exist"));
    };
    if meta.is_dir() {
        return Some(format!("{prog} is a directory"));
    }
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 == 0 {
        return Some(format!(
            "{prog} is not executable (mode {:04o}) -- `chmod +x` it; \
             execve will fail with EACCES and this service will respawn forever",
            meta.permissions().mode() & 0o7777
        ));
    }
    None
}

/// Count a start of a service whose `exec =` does not exist, and say whether
/// init has given up on it.
///
/// The counting lives here, out of [`start_service`], because that one forks:
/// the policy was untestable inside it. Returns `true` when the service must
/// not be started -- this try and every later one -- having said once what
/// would fix it. A service whose program IS there resets the count, so a path
/// that appears late (a filesystem mounted by an earlier service) costs
/// nothing.
fn note_missing_exec(svc: &mut Service) -> bool {
    let Some(prog) = svc.exec.first().cloned() else {
        return false;
    };
    if !exec_is_missing(&prog) {
        svc.missing_starts = 0;
        return false;
    }
    svc.missing_starts += 1;
    if svc.missing_starts < MISSING_EXEC_TRIES {
        return false;
    }
    svc.given_up = true;
    svc.pid = None;
    svc.restart_at = None;
    log(&format!(
        "{}: giving up after {} tries -- {} is still not there. Nothing on this \
         machine creates it: /usr/local/bin is on the installed root, which a kernel \
         upgrade does not rewrite. Reinstall the image (install-eclipse, mode `new`), \
         or write the wrapper by hand, to get this service back.",
        svc.name, svc.missing_starts, prog
    ));
    true
}

/// Which services can never work for the rest of this boot because something
/// they `requires =` has been given up on, and which requirement it is.
///
/// Transitive, so a chain goes with it: with `seatd` given up, `labwc` is
/// hopeless, and so are `lunarbar` and `lunarbg` behind it. Without this,
/// every one of them burned [`CRASH_START_LIMIT`] starts -- each paying its
/// bounded `wait_socket` gate, 10 s of it for labwc -- against a socket
/// nothing was ever going to create, and the console carried their deaths
/// instead of the one failure that mattered.
///
/// A name that is not a service in this boot's set is ignored, exactly as
/// [`ordered_names`] ignores it: a requirement that does not exist cannot be
/// reported as failed, and treating it as failure would disable services on
/// an image whose session dropped the dependency.
fn blocked_by_requirements(services: &BTreeMap<String, Service>) -> BTreeMap<String, String> {
    let mut blocked: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut added = false;
        for (name, svc) in services {
            if svc.given_up || blocked.contains_key(name) {
                continue;
            }
            let failed = svc.requires.iter().find(|dep| {
                services
                    .get(*dep)
                    .is_some_and(|d| d.given_up || blocked.contains_key(*dep))
            });
            if let Some(dep) = failed {
                blocked.insert(name.clone(), dep.clone());
                added = true;
            }
        }
        if !added {
            return blocked;
        }
    }
}

/// Give up on every service whose requirement has been given up on, and return
/// what to log: `(service, the requirement that failed)` for each one newly
/// written off. Idempotent -- a service already given up on is not reported
/// twice.
fn propagate_given_up(services: &mut BTreeMap<String, Service>) -> Vec<(String, String)> {
    let blocked = blocked_by_requirements(services);
    for name in blocked.keys() {
        if let Some(svc) = services.get_mut(name) {
            svc.given_up = true;
            svc.restart_at = None;
        }
    }
    blocked.into_iter().collect()
}

/// Log, once each, the services [`propagate_given_up`] has just written off.
fn report_given_up(services: &mut BTreeMap<String, Service>) {
    for (name, dep) in propagate_given_up(services) {
        log(&format!(
            "{name}: not starting it -- it requires {dep}, which init has given up on for \
             this boot. Fix {dep} and reboot."
        ));
    }
}

/// Count a respawn service's exit and say whether init has just given up on
/// it for the rest of the boot.
///
/// The missing piece next to the backoff: the backoff makes a crash loop cheap
/// but never ends it, so a service that cannot work keeps printing its death
/// every [`MAX_BACKOFF`] for as long as the machine is on -- see
/// [`CRASH_START_LIMIT`] for why that matters on a console-only box and what
/// the other supervisors do. A single healthy run (past [`HEALTHY_UPTIME`])
/// clears the count -- but a 126 or a 127 is never a healthy run, however
/// long the service lived before reporting it, so this also fires on the
/// wrappers that sleep before giving up. The general case, any exit code at
/// all, is [`note_start`].
///
/// Its own function, out of [`supervise`], because that one blocks in
/// `waitpid` for the life of the machine.
/// Record a start in `starts` and say whether this service has now been
/// started [`START_LIMIT_BURST`] times inside [`START_LIMIT_INTERVAL`].
///
/// The net under [`note_crash`], which only catches a service that never
/// stays up [`HEALTHY_UPTIME`]: this one counts STARTS, so no amount of
/// sleeping inside the service can hide the loop. Pure, so the window can be
/// tested without a child.
///
/// `now` is passed in rather than read here for the same reason uptime is
/// measured to the reap instant: a caller that already has the instant must
/// not get a different one.
fn note_start(starts: &mut VecDeque<Instant>, now: Instant) -> bool {
    // Drop what has aged out of the window. `saturating_duration_since`
    // because a monotonic clock that goes backwards must not resurrect
    // entries.
    while let Some(&oldest) = starts.front() {
        if now.saturating_duration_since(oldest) > START_LIMIT_INTERVAL {
            starts.pop_front();
        } else {
            break;
        }
    }
    // Full: nothing is started, so nothing is recorded. Called BEFORE the
    // spawn, so the window holds starts that happened and no more --
    // recording a refusal would push the real ones out of it.
    if starts.len() >= START_LIMIT_BURST as usize {
        return true;
    }
    starts.push_back(now);
    false
}

fn note_crash(svc: &mut Service, uptime: Duration, exit_code: Option<i32>) -> bool {
    // 126 and 127 are the two failures no retry can repair: the program is
    // not there, or it is there and cannot be executed. A long life does not
    // turn one of those into a healthy run -- and a wrapper that sleeps
    // before reporting one is exactly how `eclipse-pulseaudio` kept its
    // counter at zero for ever (see [`START_LIMIT_BURST`]).
    let unrepairable = matches!(exit_code, Some(126) | Some(127));
    if uptime >= HEALTHY_UPTIME && !unrepairable {
        svc.crash_starts = 0;
        return false;
    }
    svc.crash_starts += 1;
    if svc.crash_starts < CRASH_START_LIMIT {
        return false;
    }
    svc.given_up = true;
    svc.pid = None;
    svc.restart_at = None;
    true
}

/// Is this `exec =` an absolute path with no file behind it?
///
/// The one failure [`exec_problem`] reports that no retry and no repair can
/// change, so the one that [`start_service`] stops retrying. Deliberately
/// narrow: a bare `exec = seatd` goes through `execvp`'s PATH search, which
/// this cannot replicate, and a wrong answer here would disable a service that
/// works.
fn exec_is_missing(prog: &str) -> bool {
    prog.starts_with('/') && fs::metadata(Path::new(prog)).is_err()
}

/// Add the missing x bits to a service's own `exec =` and say whether it worked.
///
/// Reporting is not enough for this one failure, because of where the file
/// lives. `/usr/local/bin` is part of `rootfs.btrfs.gz`, which only the
/// *installer* writes to the disk: upgrading the kernel on a machine that is
/// already installed does not rewrite it. So the image that shipped
/// `eclipse-oopslog` 0644 leaves every such disk with a service that respawns
/// forever on EACCES, and no new kernel can fix it -- the user would have to
/// reinstall, or know to `chmod +x` a file they have never heard of.
///
/// One `chmod` from init fixes it on the next boot instead, and is safe to do
/// unconditionally: this runs only for a path named by an `exec =` in
/// /etc/eclipse/services, a file whose entire purpose is to be executed, and
/// only when it is a regular file with no x bit at all -- a state in which the
/// service cannot work however long it is left alone. It is logged either way,
/// so a repaired boot is still a boot that says what was wrong.
///
/// Returns `None` if nothing was attempted, otherwise the line to log.
fn repair_exec_mode(prog: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(prog);
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 != 0 {
        return None;
    }
    // Mirror `chmod +x`: add x wherever the file is already readable, which for
    // a 0644 wrapper means 0755. Never touches setuid/setgid or the read and
    // write bits.
    let old = meta.permissions().mode() & 0o7777;
    let add = ((old & 0o444) >> 2) & 0o111;
    let new = old | if add == 0 { 0o100 } else { add };
    match fs::set_permissions(path, fs::Permissions::from_mode(new)) {
        Ok(()) => Some(format!(
            "{prog} was {old:04o} (not executable); chmod'ed it to {new:04o} and starting it"
        )),
        Err(e) => Some(format!(
            "{prog} is {old:04o} (not executable) and chmod to {new:04o} failed: {e}; \
             the service cannot start -- is the root filesystem read-only?"
        )),
    }
}

/// A `timeout =` as written in a service file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Limit {
    /// Give up waiting after this long.
    After(Duration),
    /// Wait as long as it takes (`timeout = 0`, `timeout = none`).
    Never,
}

/// Parse a `timeout =` value: whole seconds, or `0`/`none`/`never`/`infinity`
/// for no limit. `None` for anything else, so a typo keeps the default instead
/// of removing the limit.
fn parse_limit(value: &str) -> Option<Limit> {
    match value {
        "none" | "never" | "infinity" => return Some(Limit::Never),
        _ => {}
    }
    let secs: u64 = value.parse().ok()?;
    if secs == 0 {
        Some(Limit::Never)
    } else {
        Some(Limit::After(Duration::from_secs(secs)))
    }
}

/// How long init waits for a service's child, given what its file asked for.
///
/// A `respawn` service is never waited for (the supervision loop owns it), so
/// it has no start timeout however its file is written -- answering `None`
/// here rather than letting a `timeout =` on a respawn service look like it
/// does something. A `oneshot` gets what it asked for, or
/// [`DEFAULT_ONESHOT_TIMEOUT`].
fn start_timeout(kind: Kind, asked: Option<Limit>) -> Option<Duration> {
    if kind != Kind::Oneshot {
        return None;
    }
    match asked {
        Some(Limit::Never) => None,
        Some(Limit::After(d)) => Some(d),
        None => Some(DEFAULT_ONESHOT_TIMEOUT),
    }
}

/// What init does about a oneshot still running after `waited`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Overrun {
    /// Still inside its timeout: keep waiting.
    Wait,
    /// Over it: ask it to go (SIGTERM to its process group).
    Term,
    /// It ignored SIGTERM: SIGKILL the group.
    Kill,
    /// Even SIGKILL has not been reaped (a child stuck in the kernel): stop
    /// waiting and boot on. The supervision loop reaps it if it ever dies.
    Abandon,
}

/// The escalation for an overrun oneshot, as a function of how long it has
/// been running. Signals its process GROUP, not just the child: every child is
/// a session leader (`setsid` in [`spawn`]), and the hanging one-shots here are
/// shell wrappers whose real work is a grandchild, which a kill of the shell
/// alone would leave running.
///
/// Split out of the wait loop, which blocks on a real child, so the policy can
/// be tested.
fn overrun_action(waited: Duration, limit: Duration, grace: Duration) -> Overrun {
    if waited < limit {
        Overrun::Wait
    } else if waited < limit + grace {
        Overrun::Term
    } else if waited < limit + grace * 2 {
        Overrun::Kill
    } else {
        Overrun::Abandon
    }
}

/// Whether [`await_oneshot`] is finished once its own child has been reaped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Watch {
    /// Nothing left to do: the child ran to completion on its own.
    Done,
    /// The child was reaped while init was terminating it, so keep watching
    /// its process GROUP: the thing the SIGTERM was for may be a grandchild.
    Group,
}

/// What is left to do once the oneshot's own child has been reaped.
///
/// A oneshot wrapper that exits leaving a grandchild running is NORMAL here
/// and deliberately not interfered with -- `eclipse-boot-sound` forks `mpg123`
/// and exits exactly so init is not held for the length of the track. But once
/// init has decided the service overran and has SIGTERMed its whole group,
/// that is no longer the case: a shell that dies on the SIGTERM while a
/// grandchild ignores it would, if init returned here, leave running precisely
/// the workload the timeout exists to stop, and skip the promised SIGKILL.
fn after_child_exit(terminating: bool) -> Watch {
    if terminating {
        Watch::Group
    } else {
        Watch::Done
    }
}

/// Wait for a oneshot's child, bounded by `limit`, and say nothing unless
/// something is wrong.
///
/// The bounded version of the plain blocking `waitpid` this used to be. Why it
/// has to be bounded at all: see [`DEFAULT_ONESHOT_TIMEOUT`]. Polls rather
/// than arming a timer because the pacing is already written
/// ([`poll_step`]) and because a poll is also where a shutdown request gets
/// noticed -- a Ctrl-Alt-Del during a long oneshot used to wait the oneshot
/// out, which for a hanging one meant for ever.
fn await_oneshot(name: &str, pid: i32, limit: Option<Duration>) {
    let start = Instant::now();
    let mut termed = false;
    let mut killed = false;
    // Set once the child itself has been reaped while init was terminating it:
    // from then on what is watched is the process group (see
    // [`after_child_exit`]), because the escalation is not finished.
    let mut reaped = false;
    loop {
        if reaped {
            // SAFETY: signal 0 only tests whether the group still has members.
            if unsafe { libc::kill(-pid, 0) } != 0 {
                // ESRCH: the group is empty, so the escalation worked.
                return;
            }
        } else {
            let mut status = 0;
            // SAFETY: pid is a child of ours; WNOHANG so the loop keeps looking.
            let done = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if done == pid {
                if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
                    let code = libc::WEXITSTATUS(status);
                    // A oneshot that failed used to be completely silent:
                    // nothing waited on its result and its stdio is its
                    // `log =` at best.
                    log(&format!(
                        "oneshot: {name} failed (exit {code}{})",
                        exit_note(code)
                    ));
                } else if libc::WIFSIGNALED(status) && !termed {
                    log(&format!(
                        "oneshot: {name} was killed by signal {}",
                        libc::WTERMSIG(status)
                    ));
                }
                match after_child_exit(termed) {
                    Watch::Done => return,
                    Watch::Group => reaped = true,
                }
            } else if done < 0 && errno() != libc::EINTR {
                // ECHILD: already reaped elsewhere. Anything else is not
                // something more waiting can fix.
                return;
            }
        }
        if shutdown_requested() {
            return;
        }
        let Some(limit) = limit else {
            sleep_interruptible(poll_step(start.elapsed()));
            continue;
        };
        match overrun_action(start.elapsed(), limit, STOP_GRACE) {
            Overrun::Wait => {}
            Overrun::Term if !termed => {
                termed = true;
                log(&format!(
                    "oneshot: {name} is still running after {limit:?}; terminating it and \
                     carrying on with the boot (raise or remove the limit with \
                     'timeout = <seconds>' / 'timeout = 0' in its .service file)"
                ));
                // SAFETY: signalling the child's own process group.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
            }
            Overrun::Kill if !killed => {
                killed = true;
                log(&format!("oneshot: {name} ignored SIGTERM; killing it"));
                // SAFETY: as above.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            Overrun::Term | Overrun::Kill => {}
            Overrun::Abandon => {
                log(&format!(
                    "oneshot: {name} survived SIGKILL (a process stuck in the kernel, one \
                     that left its process group, or a zombie in it nobody has reaped \
                     yet); booting on without it"
                ));
                return;
            }
        }
        sleep_interruptible(poll_step(start.elapsed()));
    }
}

/// Start a service. `oneshot` runs to completion (bounded, see
/// [`await_oneshot`]) before returning; `respawn` is forked and its pid
/// recorded for the supervision loop.
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
        timed(format!("{}: wait for the socket {path}", svc.name), || {
            wait_for_socket(&path, Duration::from_secs(10))
        });
    }
    // See `Service::wait_path`: input nodes for labwc. Always wait for
    // `/dev/input` when starting labwc, even if the service file is an older
    // image without `wait_path =` — without udevd there is no input hotplug.
    let wait_path = svc
        .wait_path
        .clone()
        .or_else(|| (svc.name == "labwc").then(|| String::from("/dev/input")));
    if let Some(path) = wait_path {
        timed(format!("{}: wait for {path} to appear", svc.name), || {
            wait_for_path(&path, Duration::from_secs(8))
        });
        if Path::new(&path).is_dir() {
            // Timed SEPARATELY from the appearance: they are different costs
            // with different cures -- one is the kernel enumerating devices,
            // the other is a fixed settle window this init chooses.
            timed(format!("{}: wait for {path} to settle", svc.name), || {
                wait_for_dir_settled(&path, Duration::from_secs(8), Duration::from_secs(1))
            });
        }
    }
    // Name an unrunnable `exec =` on the console: the child that fails execve
    // cannot (see `exec_problem`). A missing x bit is also repaired in place,
    // because no kernel upgrade can reach the installed /usr/local/bin that
    // carries it (see `repair_exec_mode`).
    if let Some(prog) = svc.exec.first() {
        if let Some(problem) = exec_problem(prog) {
            log(&format!("error: {}: {}", svc.name, problem));
            if let Some(repair) = repair_exec_mode(prog) {
                log(&format!("{}: {}", svc.name, repair));
            }
        }
        // A file that is not there cannot become there between two `execve`s,
        // so retrying it is a console that scrolls for the whole boot and a
        // service that is no closer to running. Count the tries and stop,
        // saying once what would actually fix it.
        if note_missing_exec(svc) {
            return;
        }
    }
    match svc.kind {
        Kind::Oneshot => {
            log(&format!("oneshot: {}", svc.name));
            if let Some(pid) = spawn(&svc.exec, svc.log.as_deref()) {
                // Wait for this child specifically -- but not for ever: a
                // oneshot that never exits used to hang the whole boot.
                let limit = start_timeout(svc.kind, svc.timeout);
                timed(format!("{}: run the oneshot", svc.name), || {
                    await_oneshot(&svc.name, pid, limit)
                });
            }
        }
        Kind::Respawn => {
            let now = Instant::now();
            // Counted here and not at the exit, because this is the one place
            // every start goes through: a service whose every try sleeps past
            // HEALTHY_UPTIME looks healthy on the way out (see
            // [`START_LIMIT_BURST`]) and perfectly ordinary on the way in.
            if note_start(&mut svc.starts, now) {
                svc.given_up = true;
                svc.pid = None;
                svc.restart_at = None;
                log(&format!(
                    "respawn: {} has been started {} times in the last {:?} and is \
                     still not working; giving up on it for the rest of this boot, so \
                     the console stays readable for everything else.{} Fix the cause \
                     and reboot.",
                    svc.name,
                    START_LIMIT_BURST,
                    START_LIMIT_INTERVAL,
                    svc.log
                        .as_deref()
                        .map(|p| format!(" Its output is in {p}."))
                        .unwrap_or_default(),
                ));
                return;
            }
            log(&format!("respawn: {} (starting)", svc.name));
            svc.pid = spawn(&svc.exec, svc.log.as_deref());
            svc.started_at = Some(now);
            svc.restart_at = None;
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
    // `<` rather than `<=`: the one instant it excludes is covered by the last
    // look below, so the two spellings cannot be told apart from the outside.
    while start.elapsed() < timeout {
        if ready() {
            return Wait::Ready;
        }
        if stop() {
            return Wait::Stopped;
        }
        poll_sleep(poll_step(start.elapsed()));
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
    wait_for_dir_settled_until(dir, timeout, settle, shutdown_requested)
}

/// [`wait_for_dir_settled`] with the give-up test given rather than read from
/// the global flags, exactly as [`wait_until`] already takes its `stop`.
fn wait_for_dir_settled_until(
    dir: &str,
    timeout: Duration,
    settle: Duration,
    stop: impl Fn() -> bool,
) {
    let list = |d: &str| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(d)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        // Sorted so that two listings of the same set of names compare
        // equal: `read_dir` gives no order, and an unsorted listing would read
        // as a change and restart the settle window. Not reachable from a
        // test, which cannot make the kernel hand the names back shuffled.
        names.sort_unstable();
        names
    };
    let start = Instant::now();
    let mut last = list(dir);
    let mut stable_since = Instant::now();
    while start.elapsed() < timeout {
        if stop() {
            log(&format!(
                "shutdown requested while waiting for {dir} to settle"
            ));
            return;
        }
        poll_sleep(Duration::from_millis(100));
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
                // Plural only, like the sweep's: the line, nothing else.
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
        // `& S_IFMT` is the correct mask, though `& S_IFSOCK` would answer
        // the same: S_IFSOCK is 0o140000 and no other file type sets both
        // 0o100000 and 0o040000 (a symlink is 0o120000, a regular file
        // 0o100000, a block device 0o060000).
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
                let _ = log_renderer(&format!(
                    "renderer=auto: NVIDIA GPU {} + nvidia.nouveau_uapi -> gl \
                     (GLES2/zink by default; nvidia.wlr_pixman for software)",
                    v.trim()
                ));
                Renderer::Gl
            } else {
                let _ = log_renderer(&format!(
                    "renderer=auto: NVIDIA GPU {} but nvidia.nouveau_uapi is OFF (kernel uAPI \
                     disabled; DRM node is \"zcore\") -> pixman. Boot with GL=1 (or add \
                     nvidia.nouveau_uapi + renderer=gl to the cmdline) for hardware GL",
                    v.trim()
                ));
                Renderer::Pixman
            }
        }
        Some(v) if !v.trim().is_empty() => {
            let _ = log_renderer(&format!(
                "renderer=auto: GPU vendor {} -> pixman (pass renderer=gl-sw for GLES2/llvmpipe, \
                 renderer=gl for virgl)",
                v.trim()
            ));
            Renderer::Pixman
        }
        _ => {
            let _ = log_renderer("renderer=auto: no GPU visible -> pixman");
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
                    let _ = log_renderer(&format!(
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
                    let _ =
                        log_renderer("renderer=gl: NVIDIA GPU -> pinning GL clients to zink+NVK");
                } else {
                    env.push(CString::new("WLR_RENDERER=pixman").unwrap());
                    env.push(CString::new("WLR_RENDERER_ALLOW_SOFTWARE=1").unwrap());
                    env.push(CString::new("LIBGL_ALWAYS_SOFTWARE=1").unwrap());
                    push_sdl_render_env(&mut env, SdlRender::Software);
                    if degraded {
                        let _ = log_renderer(
                            "renderer=gl: NVIDIA GPU -> compositor DEGRADED to pixman for the rest of this boot (labwc kept dying on the GPU renderer; the GPU channel is likely wedged -- see `dmesg | grep nouveau-uapi` and /tmp/labwc.log)",
                        );
                    } else {
                        let _ = log_renderer(
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
                let _ = log_renderer("renderer=gl: no NVIDIA GPU (QEMU/virtio) -> degrading to software GL (gl-sw stack: labwc GLES2 + llvmpipe clients)");
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
            let _ = log_renderer("renderer=gl-sw: wlroots GLES2 over Mesa llvmpipe (software GL)");
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

/// Whether a log file of this size has to be rotated.
fn log_overflow(size: u64) -> bool {
    size >= MAX_LOG_BYTES
}

/// Rotate `path` if it has outgrown [`MAX_LOG_BYTES`], and say so if it did.
///
/// Copy-then-truncate, the way logrotate's `copytruncate` works, and for the
/// same reason: the service writing here holds an open fd, so renaming the file
/// would leave it writing to the renamed inode for ever and the new file empty.
/// Copying the contents to `<path>.1` and truncating the original keeps the
/// writer's fd valid -- it was opened `O_APPEND`, so its next write lands at
/// the new end of file rather than a megabyte into a sparse hole.
///
/// What this loses is a line written between the copy and the truncate, which
/// is the same race `copytruncate` has had for twenty years and the price of
/// not having to reopen another process's fd. Returns `None` when there is
/// nothing to do, including every error: a log that cannot be rotated must not
/// stop a boot.
fn rotate_log(path: &str) -> Option<String> {
    let size = fs::metadata(path).ok()?.len();
    if !log_overflow(size) {
        return None;
    }
    let old = format!("{path}.1");
    let contents = fs::read(path).ok()?;
    fs::write(&old, &contents).ok()?;
    // `truncate`, not a remove and recreate: the service's fd must keep
    // pointing at this very inode.
    fs::File::options()
        .write(true)
        .open(path)
        .ok()?
        .set_len(0)
        .ok()?;
    Some(format!(
        "log: {path} reached {size} bytes; moved it to {old} and started it again \
         (tmpfs is RAM, so an unbounded log is a machine that runs out of memory)"
    ))
}

/// Rotate every log the service table names that has outgrown the cap.
///
/// By distinct path, because two services can share one `log =`
/// (`boot-sound` and `boot-sound-xorg` both write `/tmp/boot-sound.log`) and
/// rotating it twice would throw away the generation just kept.
fn sweep_logs(services: &BTreeMap<String, Service>) {
    let paths: BTreeSet<&str> = services.values().filter_map(|s| s.log.as_deref()).collect();
    for path in paths {
        if let Some(line) = rotate_log(path) {
            log(&line);
        }
    }
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

/// The PID 1 main loop: reap every child, restarting the `respawn` services;
/// orphans reparented to init are simply reaped. A pending shutdown/reboot
/// signal breaks out to `shutdown`.
///
/// Blocks in `waitpid` while nothing is backing off, and polls only while a
/// crashed service is waiting out its [`Service::restart_at`] deadline — the
/// loop must stay able to reap during a backoff, or every other service's
/// uptime is measured to the end of that backoff instead of to its own death.
fn supervise(services: &mut BTreeMap<String, Service>) {
    loop {
        // Children reaped during a bounded gate (see `reap_pending`): account
        // for them here, each with the instant it was really reaped.
        for exit in take_pending() {
            note_exit(services, exit.pid, exit.status, exit.at);
        }
        // The alarm is PID 1's only clock, and this is all it is for: keep the
        // services' logs from eating the tmpfs they live in.
        if WANT_LOG_SWEEP.swap(false, Ordering::SeqCst) {
            sweep_logs(services);
            arm_log_sweep();
        }
        if WANT_HALT.load(Ordering::SeqCst) {
            return shutdown(false, services);
        }
        if WANT_REBOOT.load(Ordering::SeqCst) {
            return shutdown(true, services);
        }

        // The nearest backoff deadline still in the future, if any. It decides
        // whether this iteration may block: with one pending, a blocking
        // `waitpid` would sit there until some OTHER child happened to die and
        // the crashed service would never be restarted at all.
        let now = Instant::now();
        let next_due = services
            .values()
            .filter(|s| s.kind == Kind::Respawn && s.pid.is_none())
            .filter_map(|s| s.restart_at)
            .filter(|t| *t > now)
            .min();

        let mut status = 0;
        // SAFETY: wait for any child; non-blocking while a backoff is pending.
        let pid = unsafe {
            libc::waitpid(
                -1,
                &mut status,
                if next_due.is_some() { libc::WNOHANG } else { 0 },
            )
        };
        if pid == 0 {
            // WNOHANG: nothing has exited yet. Sleep at most to the nearest
            // deadline, capped so a shutdown signal is still answered promptly,
            // then run the restart pass.
            if let Some(due) = next_due {
                sleep_interruptible(
                    due.saturating_duration_since(Instant::now())
                        .min(POLL_SLICE),
                );
            }
            restart_due(services);
            continue;
        }
        if pid < 0 {
            let err = errno();
            if err == libc::EINTR {
                // A signal arrived; loop to re-check the shutdown flags.
                continue;
            }
            if err == libc::ECHILD {
                // No children to wait on. With a backoff pending that is the
                // normal state (the crasher was the last child), so wait it out
                // and restart; otherwise pause until the next signal so we are
                // not a busy loop. `pause` returns on EINTR.
                if let Some(due) = next_due {
                    sleep_interruptible(
                        due.saturating_duration_since(Instant::now())
                            .min(POLL_SLICE),
                    );
                    restart_due(services);
                } else {
                    unsafe { libc::pause() };
                }
                continue;
            }
            // Unexpected: avoid spinning.
            unsafe { libc::pause() };
            continue;
        }

        note_exit(services, pid, status, Instant::now());
        restart_due(services);
    }
}

/// Account for one child that has been reaped: if it was a supervised respawn
/// service, clear its pid, say how it ended and decide when (or whether) it is
/// started again. `at` is when it was reaped.
///
/// Split out of [`supervise`] because the exits no longer all arrive there: the
/// boot's bounded gates reap too (see [`reap_pending`]), and an exit collected
/// in one of them is handed over with the instant it happened, which is the
/// whole point -- the uptime has to be measured to that instant and not to
/// whenever the loop gets round to it.
///
/// A pid that belongs to no service was a oneshot's leftover or an orphan
/// reparented to init: reaping it was the whole job.
fn note_exit(services: &mut BTreeMap<String, Service>, pid: i32, status: i32, at: Instant) {
    if let Some(svc) = services.values_mut().find(|s| s.pid == Some(pid)) {
        // Measured to `at`, the instant the child was REAPED, not to now: an
        // exit collected during one of the boot's bounded gates is handed over
        // with its own timestamp, and crediting it with the gate as well would
        // read a 40 ms crash as a healthy ten-second run. That is the same
        // mistake the backoff made when it slept inside this loop.
        let uptime = svc
            .started_at
            .map(|t| at.saturating_duration_since(t))
            .unwrap_or_default();
        svc.pid = None;
        // HOW it ended, not just when: a service that keeps "exiting after
        // 8 s" reads completely differently as `exit 0`, `exit 1` or
        // `signal 9`, and this line is the only record on a console-only
        // box. Uses libc's status decoding so a signal death is named.
        // Kept out of `how`'s branch because `note_crash` needs it too: a 126
        // or a 127 is not a healthy run whatever the uptime says.
        let exit_code = if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        };
        let how = if libc::WIFEXITED(status) {
            let code = libc::WEXITSTATUS(status);
            let mut how = format!("exit {code}{}", exit_note(code));
            // Those two codes are the ones a reader can actually chase, and
            // the service's own output is where the name of the missing
            // command is. Say where it landed, on the line that reports the
            // death, or the reader has to know that `log =` exists at all.
            if !exit_note(code).is_empty() {
                if let Some(path) = &svc.log {
                    how.push_str(&format!("; see {path}"));
                }
            }
            how
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
        if let Some((n, out_of_tries)) = compositor_exit(
            &svc.name,
            COMPOSITOR_DEGRADED.load(Ordering::Relaxed),
            gpu_compositor_requested(),
            || COMPOSITOR_EXITS.fetch_add(1, Ordering::Relaxed),
        ) {
            if out_of_tries {
                COMPOSITOR_DEGRADED.store(true, Ordering::Relaxed);
                // Same marker the labwc wrapper writes when it falls back
                // itself: the GL wrappers (eclipse-firefox) read it and stay
                // off zink, which with no GPU path lands on lavapipe.
                let _ = std::fs::write(RENDERER_FALLBACK_MARKER, "init-degraded\n");
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
        // Has it now crashed so many times in a row that retrying it is
        // only costing the console? Checked before the restart is even
        // scheduled, so a service given up on here never gets a deadline.
        if note_crash(svc, uptime, exit_code) {
            log(&format!(
                "respawn: {} exited after {:?} ({}) and has now failed {} times in a \
                 row without ever staying up {:?}; giving up on it for the rest of \
                 this boot, so the console stays readable for everything else.{} \
                 Fix the cause and reboot.",
                svc.name,
                uptime,
                how,
                svc.crash_starts,
                HEALTHY_UPTIME,
                svc.log
                    .as_deref()
                    .map(|p| format!(" Its own output is in {p}."))
                    .unwrap_or_default(),
            ));
        } else {
            let (wait, next) = restart_delay(uptime, svc.backoff);
            svc.backoff = next;
            svc.restart_at = Some(at + wait);
            if wait.is_zero() {
                log(&format!(
                    "respawn: {} exited after {:?} ({}), restarting",
                    svc.name, uptime, how
                ));
            } else {
                log(&format!(
                    "respawn: {} exited after {:?} ({}, crash), retry in {:?}",
                    svc.name, uptime, how, wait
                ));
            }
        }
    }
}

/// What a bare exit code means when it is one of the two the shell reserves,
/// as a suffix for the supervisor's line (empty for every other code).
///
/// A respawn service's stdio is `/dev/null` unless its file sets `log =`, so
/// for most of them the number on the console is ALL there is -- and 126/127
/// are the two numbers that are not the program's own opinion but a report
/// that it never ran. Spelling them out is the difference between "it keeps
/// exiting 127" and a reader who knows to go looking for a missing command
/// inside the wrapper script.
fn exit_note(code: i32) -> &'static str {
    match code {
        // `execve` failed for the program itself, or a command the wrapper
        // script ran was not found. `exec_problem` names the first case at
        // start time; nothing can name the second from out here.
        127 => " -- command not found, or execve failed",
        // Found but not runnable: no x bit, or a bad interpreter line.
        126 => " -- found but not executable",
        _ => "",
    }
}

/// A child reaped somewhere other than the supervision loop, with the instant
/// it was reaped.
struct Exit {
    pid: i32,
    status: i32,
    at: Instant,
}

/// Children reaped by [`reap_pending`] during one of the boot's bounded gates,
/// waiting to be accounted for by the supervision loop.
///
/// A `Mutex` for the type's sake, not for contention: init is single-threaded.
static PENDING_EXITS: Mutex<Vec<Exit>> = Mutex::new(Vec::new());

/// Reap every child that has exited, without blocking, and queue what was
/// reaped for the supervision loop.
///
/// Called from the bounded waits ([`poll_sleep`]), where init used to reap
/// nothing at all: a `wait_socket` gate is 10 s of a boot in which any child
/// that died stayed a zombie, and -- worse -- its uptime was then measured to
/// the end of the gate instead of to its own death, so a service that failed
/// `execve` in a millisecond could be read as having stayed up past
/// [`HEALTHY_UPTIME`], judged healthy and restarted with its backoff reset.
///
/// The queue is what makes this safe. A bare `waitpid(-1)` in here would
/// swallow a respawn service's death: the loop would never see that pid, the
/// service's `pid` would stay `Some` for ever and nothing would restart it.
fn reap_pending() {
    loop {
        let mut status = 0;
        // SAFETY: non-blocking wait for any child.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid == 0 {
            // Children exist, none has exited.
            return;
        }
        if pid < 0 {
            // Our handlers are installed WITHOUT `SA_RESTART` on purpose (see
            // `install_handlers`), so a signal delivered right here comes back
            // as EINTR having reaped nothing. Giving up on it would leave the
            // death to be collected after the gate, with the late `at` this
            // whole queue exists to avoid -- so retry instead.
            if errno() == libc::EINTR {
                continue;
            }
            // ECHILD, or nothing more to collect.
            return;
        }
        queue_exit(Exit {
            pid,
            status,
            at: Instant::now(),
        });
    }
}

/// Push onto the queue, through a poisoned lock if it comes to that.
///
/// `PENDING_EXITS` is only ever held for a `push` or a `take`, so a poisoning
/// means some *other* code panicked while this lock happened to be held -- and
/// since #1748 a panic in PID 1 unwinds and is caught instead of killing the
/// machine, which makes poisoning reachable rather than theoretical. Dropping
/// an exit on it would strand a respawn service with `pid = Some(..)` for ever,
/// exactly the failure this queue is here to prevent, so take the data back out
/// of the poisoned guard and carry on.
fn queue_exit(exit: Exit) {
    let mut queue = PENDING_EXITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    queue.push(exit);
}

/// Everything [`reap_pending`] has collected since the last call.
fn take_pending() -> Vec<Exit> {
    // Through a poisoned lock as well, for the reason in `queue_exit`.
    let mut queue = PENDING_EXITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *queue)
}

/// Reap, then sleep: the one poll step of every bounded wait.
///
/// Not [`await_oneshot`]'s, deliberately -- that one is waiting for a specific
/// child of its own, and a reap of "any child" in there would take it out from
/// under the `waitpid` that is watching for it.
fn poll_sleep(d: Duration) {
    reap_pending();
    sleep_interruptible(d);
}

/// How long the loop may sleep in one go while waiting out a backoff. Short
/// enough that a shutdown signal and a child that dies meanwhile are both
/// noticed promptly; long enough that PID 1 costs nothing while it waits.
const POLL_SLICE: Duration = Duration::from_millis(50);

/// Restart every respawn service that has no live child and whose backoff
/// deadline has passed.
///
/// Through the normal launcher, so crash-restarts re-apply the wait_socket /
/// wait_path gates exactly like the first boot start. Walks in dependency
/// order (`after =`), not BTreeMap alphabetical order: "labwc" < "seatd", so a
/// crash of both restarted labwc first, which then parked ~10 s on the seatd
/// socket gate (or launched against a dead seatd and crashed again) before
/// seatd was retried.
fn restart_due(services: &mut BTreeMap<String, Service>) {
    if WANT_HALT.load(Ordering::SeqCst) || WANT_REBOOT.load(Ordering::SeqCst) {
        return;
    }
    // A service whose requirement has just been given up on is hopeless too:
    // write it off here rather than let it burn its own CRASH_START_LIMIT
    // starts, each paying its bounded wait gate, against something that will
    // never arrive.
    report_given_up(services);
    for name in due_names(services, Instant::now()) {
        if let Some(svc) = services.get_mut(&name) {
            start_service(svc);
        }
    }
}

/// Which respawn services are due to be (re)started at `now`, in dependency
/// order: those with no live child whose backoff deadline has passed.
///
/// Split out of [`restart_due`], which forks, so the policy can be tested.
fn due_names(services: &BTreeMap<String, Service>, now: Instant) -> Vec<String> {
    ordered_names(services)
        .into_iter()
        .filter(|name| {
            services.get(name).is_some_and(|svc| {
                svc.kind == Kind::Respawn
                    && !svc.given_up
                    && svc.pid.is_none()
                    && svc.restart_at.is_none_or(|t| t <= now)
            })
        })
        .collect()
}

/// How long to wait before restarting a respawn service that just exited, and
/// what its backoff becomes: `(wait now, next backoff)`.
///
/// A service that stayed up past [`HEALTHY_UPTIME`] did a unit of work and is
/// restarted at once with its backoff reset. One that died almost immediately
/// is a crash loop, and waits its current backoff, which then doubles up to
/// [`MAX_BACKOFF`] -- without that, a service whose binary is missing forks
/// and execs at full speed forever and pins a CPU.
///
/// Split out of [`supervise`], which blocks in `waitpid` for the life of the
/// machine: the policy was unreachable from a test while it lived in there.
fn restart_delay(uptime: Duration, backoff: Duration) -> (Duration, Duration) {
    if uptime >= HEALTHY_UPTIME {
        (Duration::ZERO, MIN_BACKOFF)
    } else {
        (backoff, (backoff * 2).min(MAX_BACKOFF))
    }
}

/// Whether a child's exit counts against the compositor's GPU-renderer
/// tolerance, and if so what the count becomes and whether it has run out.
///
/// Counted on EVERY exit of the GPU-rendered compositor, not only fast ones:
/// the first labwc instance can live for minutes and the respawns on a dead
/// GPU channel may also linger, so an uptime-gated "crash" count would never
/// trip. Returns `None` when the exit is not the compositor's, when it is
/// already degraded, or when no GPU renderer was asked for.
///
/// `bump` yields the count BEFORE this exit and is called only when the exit
/// counts, so a service other than the compositor dying does not consume the
/// compositor's tolerance.
fn compositor_exit(
    name: &str,
    degraded: bool,
    gpu: bool,
    bump: impl FnOnce() -> u32,
) -> Option<(u32, bool)> {
    if name != "labwc" || degraded || !gpu {
        return None;
    }
    let n = bump() + 1;
    Some((n, n >= COMPOSITOR_DEGRADE_AFTER))
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
mod tests;
