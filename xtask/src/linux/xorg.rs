//! Build-time population of the X.Org stack into the rootfs.
//!
//! Xorg used to be a *runtime* `apk add` chore (the on-screen hints in
//! `desktop.rs` all say "apk add …"): a fresh Eclipse install booted to a bare
//! shell and the user had to fetch xorg-server, an input driver and fonts by
//! hand before `startx` did anything — and even then a real-hardware run showed
//! the input driver (`xf86-input-libinput`) simply missing, so X came up with
//! no keyboard or mouse.
//!
//! This module bakes the whole stack into the image at build time. It installs
//! the packages with `apk add --root` into a THROWAWAY staging root (Alpine's
//! offline root-install pulls the full dependency closure, incl. musl/base,
//! which must NOT clobber Eclipse's hand-staged base), then copies only the
//! X-owned trees (`usr/*`, X fonts/config) into the real rootfs. A built image
//! then has a working X server, the libinput driver, a software-GL renderer,
//! keyboard-map data and the base fonts already present, with the base system
//! untouched. `startx` works out of the box in QEMU (the live initramfs — see
//! `image.rs`, which copies these paths in uncapped) and on real hardware (the
//! installed btrfs root).
//!
//! It is **best-effort**: a missing network, an unreachable mirror or an
//! unavailable package prints a warning and leaves the image buildable (exactly
//! like `nvidia_firmware`). The downloaded `.apk`s are cached under
//! `ignored/apk-cache/<arch>` so a second build — or an offline one — reuses
//! them.
//!
//! Knobs:
//!   * `ECLIPSE_XORG=0|off|no|false` — skip entirely (lean/minimal images).
//!   * `ECLIPSE_XORG_PACKAGES="pkg1 pkg2 …"` — replace the default package set
//!     (e.g. to match a non-Alpine repository whose names differ).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::PROJECT_DIR;

/// Default top-level package set. `apk` resolves the dependency closure
/// (libX11, pixman, libdrm, libinput, …) itself, so this lists only the
/// user-facing pieces. Names track the Alpine repositories written into
/// `/etc/apk/repositories` by `mod.rs`; override with `ECLIPSE_XORG_PACKAGES`
/// for a different provider.
const DEFAULT_PACKAGES: &[&str] = &[
    // The server itself, plus GLX. It also ships the built-in `modesetting`
    // driver (DRM/`/dev/dri/card0`), but Eclipse deliberately drives X through
    // the framebuffer instead (see `xf86-video-fbdev` below and
    // `write_xorg_config` in desktop.rs).
    "xorg-server",
    // The framebuffer video driver. Eclipse's X runs on `/dev/fb0` via this
    // driver, NOT DRM/modesetting: the kernel exposes a plain linear
    // framebuffer (FbDev, linux-object/src/fs/devfs/fbdev.rs) with the fbdev
    // ioctls + mmap the driver needs, and going straight to the framebuffer
    // avoids the DRM dumb-buffer + KMS commit round-trip on every frame, which
    // is pure overhead on a software scanout. So install fbdev explicitly --
    // its absence used to force the modesetting fallback.
    "xf86-video-fbdev",
    // THE piece missing on the real-hardware run: without an input driver X
    // starts with no keyboard or mouse. libinput is what labwc/this kernel's
    // evdev nodes are known to work with.
    "xf86-input-libinput",
    // `startx` / `xinit`.
    "xinit",
    // Software GL (`swrast_dri.so` / llvmpipe): the AIGLX log line
    // "dlopen of /usr/lib/dri/swrast_dri.so failed" came from this being
    // absent, leaving GLX with "no usable GL providers". Needed by GL clients
    // (Firefox). Heavy (~tens of MiB) but the one that makes GL work headless.
    "mesa-dri-gallium",
    "mesa-gl",
    // EGL + GLESv2, declared EXPLICITLY rather than relied on transitively.
    // wlroots' gles2 renderer -- the hardware path the session picks on
    // NVIDIA (`nvidia.wlr_gles2`, and zink+NVK underneath) -- dlopens
    // libEGL.so.1 and libGLESv2.so.2, which in Alpine live in these two
    // packages and NOT in `mesa-gl`. Today they arrive only as a dependency
    // of labwc/wlroots; if that chain ever changes, EGL init fails and
    // wlroots silently falls back to the pixman software renderer, which
    // looks exactly like "the GPU stopped working" (single-digit FPS with
    // no error). One line here removes that failure mode.
    "mesa-egl",
    "mesa-gles",
    // ── Hardware GL on NVIDIA via Zink + NVK (Vulkan) ───────────────────────
    // Since Mesa 25.1 the DEFAULT OpenGL path for NVIDIA GPUs is NO LONGER the
    // classic nvc0 Gallium driver (GEM_PUSHBUF) -- it is Zink (GL-on-Vulkan)
    // running on NVK (the open Vulkan driver for Turing+). Zink itself ships in
    // `mesa-dri-gallium` (`zink_dri.so`), but it needs a Vulkan driver + the
    // loader underneath, and neither was installed -- so on the RTX every GL
    // renderer attempt failed with "DRI2: failed to load driver" / vulkan
    // "ERROR_INCOMPATIBLE_DRIVER" and NOT A SINGLE nouveau ioctl was issued
    // (Mesa never reached the kernel: it was trying the Vulkan path with no
    // Vulkan present). This is also the path Eclipse's kernel already targets:
    // NVK speaks the new nouveau uAPI (VM_INIT/VM_BIND/EXEC), which is exactly
    // what `drivers/src/display/nouveau_uapi.rs` implements.
    //   - vulkan-loader:        libvulkan.so.1, the ICD loader
    //   - mesa-vulkan-nouveau:  NVK, the Vulkan driver (+ its ICD manifest in
    //                           usr/share/vulkan/icd.d/, which LIVE_TREES must
    //                           also carry -- see there)
    //   - mesa-vulkan-swrast:   lavapipe, the CPU Vulkan rasterizer (+ its
    //                           lvp_icd.*.json ICD manifest). THE piece missing
    //                           behind Xwayland dying on real hardware: when
    //                           glamor cannot bring up hardware GL it falls back
    //                           to Zink (GL-on-Vulkan), and Zink needs *a*
    //                           Vulkan device. With only NVK present and NVK
    //                           unusable (the "failed to create timeline
    //                           semaphore" crash), the loader enumerates ZERO
    //                           working devices, Zink fails, glamor fails, and
    //                           Xwayland exits ("no GL providers"). lavapipe is
    //                           a software Vulkan device that always works, so
    //                           Zink/glamor have a floor to fall back to and the
    //                           X session survives even when NVK is broken.
    //   - vulkan-tools:         `vulkaninfo`/`vkcube`, so NVK bring-up can be
    //                           checked from a shell without labwc in the way
    "vulkan-loader",
    "mesa-vulkan-nouveau",
    "mesa-vulkan-swrast",
    "vulkan-tools",
    // OpenGL bring-up probes, the GL counterpart of vulkan-tools: check the
    // nouveau/Zink GL path from a shell before bringing up a compositor.
    //   - mesa-utils:  `glxinfo` (renderer string / GL_RENDERER — says whether
    //                  we got hardware nouveau/Zink or fell back to llvmpipe)
    //                  and `glxgears`, the minimal on-screen GL smoke test.
    //   - mesa-demos:  the wider demo set (`eglinfo`, es2gears, gears, etc.)
    //                  for exercising the pipeline once glxinfo reports HW GL.
    "mesa-utils",
    "mesa-demos",
    // Keyboard: the layout database plus the tools X needs at runtime to
    // compile a keymap and let the user set one.
    "xkeyboard-config",
    "setxkbmap",
    "xkbcomp",
    // Fonts: X refuses to start without its base bitmap fonts (`fixed`) and the
    // cursor font; `encodings` is their companion. DejaVu covers scalable text.
    "font-misc-misc",
    "font-cursor-misc",
    "encodings",
    "font-dejavu",
    // A minimal in-X terminal so `startx` yields a usable session even with no
    // Wayland compositor installed (the `.xinitrc` falls back to `xterm`).
    "xterm",
    // A lightweight window manager for the plain Xorg session (desktop=xorg).
    // Without a WM the `.xinitrc` falls through to a bare xterm with no way to
    // move/resize windows; openbox is what its WM loop tries first. Small, no
    // GTK/desktop dependencies.
    "openbox",
    // Handy CLI knobs many desktops/scripts call (RandR + DPMS/screensaver).
    "xrandr",
    "xset",
    // ── XFCE4 desktop ───────────────────────────────────────────────────────
    // Explicit components rather than the `xfce4` metapackage: the meta also
    // pulls extras we do not want. PulseAudio is first-class now, so the panel
    // plugin is named below; `startxfce4` ships in xfce4-session.
    "xfce4-session",
    "xfwm4",
    "xfce4-panel",
    "xfdesktop",
    "xfce4-settings",
    "xfconf",
    "thunar",
    "garcon",
    "xfce4-terminal",
    "xfce4-appfinder",
    // Panel volume applet: talks to the PulseAudio daemon over the native
    // protocol (`PULSE_SERVER=unix:/run/pulse/native`). Harmless if the
    // user never adds it to the XFCE panel.
    "xfce4-pulseaudio-plugin",
    // xfce4-session aborts without a D-Bus session bus; dbus-x11 provides the
    // `dbus-launch` the `.xinitrc` wraps startxfce4 in.
    "dbus",
    "dbus-x11",
    // GTK's fallback icon theme: without it every unthemed icon in the panel,
    // Thunar and the settings dialogs renders as "missing image".
    "adwaita-icon-theme",
    // THE missing piece behind the session-killing abort.
    //
    // Alpine builds gdk-pixbuf 2.44 with every native loader turned OFF except
    // legacy XPM:
    //
    //   -Dpng=disabled -Djpeg=disabled -Dgif=disabled -Dtiff=disabled
    //   -Dothers=disabled -Dlegacy_xpm=enabled -Dglycin=enabled
    //
    // Decoding is delegated to glycin, which runs a separate loader BINARY per
    // format out of /usr/libexec/glycin-loaders/2+/. Those binaries live in
    // their own subpackages, and this image had none of them -- so gdk-pixbuf
    // could not decode PNG, JPEG or SVG by any path. That is not cosmetic:
    //
    //   Gtk-WARNING: Could not load a pixbuf from icon theme.
    //   Wnck:ERROR:../libwnck/xutils.c:1510:default_icon_at_size:
    //             assertion failed: (base)
    //
    // and libwnck's `base` there is a PNG compiled into libwnck's OWN
    // GResource -- nothing on disk, nothing theme-related. It can only be NULL
    // if PNG decoding is unavailable. -> xfce4-session SIGABRT, session lost.
    //
    // image-rs covers PNG/JPEG/WebP/BMP/GIF; svg covers Adwaita's icons.
    "glycin-image-rs",
    "glycin-svg",
    // `glycin-thumbnailer`, so the session-start probe can actually DECODE a
    // PNG instead of inferring it from which files exist. Every file-presence
    // proxy used so far has been wrong, and this image ships no
    // `gdk-pixbuf-thumbnailer` (the guest reports `png-decode=no-tool`,
    // `tools=gdk-pixbuf-query-loaders`). It earns its place at runtime too:
    // it is what generates Thunar's thumbnails.
    "glycin-thumbnailer",
    // Still wanted: glycin-svg decodes SVG *through* librsvg. It is a library
    // behind glycin here, NOT a `libpixbufloader-svg.so` -- testing for that
    // .so reported "missing" on images where librsvg was installed all along.
    "librsvg",
    // Provides `gdk-pixbuf-query-loaders`, which eclipse-x11-prepare needs to
    // write `loaders.cache`. That step is already unconditional, but it is
    // guarded by `command -v` -- so without this package it silently does
    // nothing and the SVG loader above is never registered.
    "gdk-pixbuf",
    // The mime database GTK names in the same warning; also what GIO needs to
    // return a valid GFileInfo (xfdesktop logs
    // `xfdesktop_regular_file_icon_new: assertion 'G_IS_FILE_INFO(file_info)'
    // failed` without it).
    "shared-mime-info",
    // The base theme every other icon theme inherits from; Adwaita pulls it in
    // as a dependency, but naming it keeps icon lookup working if the theme
    // set is ever trimmed.
    "hicolor-icon-theme",
    // ── labwc Wayland session ───────────────────────────────────────────────
    // Eclipse's own desktop is a labwc/wlroots session (see desktop.rs and
    // README-desktop.md): all its config, wrapper and autostart are generated
    // at build time, but the binaries were never installed -- so `labwc` on
    // the console failed with "real binary not found". These are that stack.
    //
    // labwc pulls its own runtime closure: wlroots (the software-KMS + libinput
    // backend this kernel's /dev/dri/card0 drives via the pixman renderer),
    // wayland-libs, libxkbcommon and pixman. Naming labwc is enough for those.
    "labwc",
    // musl-locales: named locales (es_ES.UTF-8) so LC_TIME/strftime and
    // gettext catalogs resolve. Without it LANG=es_ES.UTF-8 is still UTF-8
    // (musl keys off the name) but month names stay in C.
    "musl-locales",
    // Named timezones (Europe/Madrid, America/New_York) for TZ= and
    // /etc/localtime. Without it musl treats TZ as POSIX and the clock stays UTC.
    "tzdata",
    // NTP client (foreground `ntpd -d`). eclipse-ntpd wraps it so a missing
    // `_ntp` user (apk --no-scripts skips the post-install) does not matter.
    "openntpd",
    // seatd: the seat manager wlroots opens DRM and input devices through. With
    // no logind/elogind here, libseat otherwise has nothing to talk to. The
    // labwc wrapper prefers libseat's daemonless `builtin` backend (works as
    // root, no service), but installing seatd provides `libseat.so` itself --
    // without the package that backend is not even present -- and leaves the
    // daemon path available. `seatd-launch` also ships here.
    "seatd",
    // foot: the native Wayland terminal the autostart and every desktop path
    // launches. It was referenced everywhere and installed nowhere, so the
    // session came up with no usable terminal. Brings its own terminfo.
    "foot",
    // Wayland client/server libs and the protocol data files. labwc/foot pull
    // wayland-libs, but the protocol XML lives in `wayland-protocols`, which a
    // few clients read at runtime; name it so it is never the missing piece.
    "wayland-protocols",
    // XWayland: the rootless X server that lets X11-only clients run inside the
    // labwc/Wayland session. labwc auto-spawns it on demand (`labwc -s`/the XWL
    // path) when an X11 client connects, but only if the `Xwayland` binary is
    // present — without this package that binary is absent and every X11 app
    // (and any toolkit falling back to X11) fails to map. Pulls libxcb and the
    // XWayland-specific bits of the X stack it needs.
    "xwayland",
    // ── ALSA + PulseAudio userspace ─────────────────────────────────────────
    // The kernel PCM is still native ALSA at /dev/snd/ (linux-object snd.rs).
    // PulseAudio runs as a system daemon on top of hw:0,0 (mmap=0, RW+SYNC_PTR)
    // so several clients can play at once — dmix needs SysV shm, which we skip.
    // /etc/asound.conf routes ALSA `default` through the pulse plugin;
    // libpulse clients use PULSE_SERVER=unix:/run/pulse/native.
    "alsa-lib",
    "alsa-utils",
    "pulseaudio",
    "pulseaudio-alsa",
    "pulseaudio-utils",
    "libpulse",
    "alsa-plugins-pulse",
    // Boot chime (`eclipse-boot-sound` plays /usr/share/eclipse/Eclipse_Awakening.mp3).
    "mpg123",
    // ── SDL (1.2 / 2 / 3) ───────────────────────────────────────────────────
    // The SDL family is the toolkit most games, emulators and media players
    // are written against, and none of it was installed: any SDL binary
    // failed at `SDL_Init` with "No available video device". Runtime only
    // (no -dev packages: nothing is compiled on the guest). The session's
    // SDL policy — which video/render driver each session pins — lives in
    // the labwc wrapper, /etc/profile and eclipse-init (see desktop.rs
    // `SDL_ENV_*` and docs/README-desktop.md, "SDL").
    //
    //   - sdl2:          libSDL2-2.0.so.0. Alpine builds it with the wayland,
    //                    x11 AND kmsdrm video drivers, so the same library runs
    //                    natively on labwc (wl_shm/EGL), on Xwayland/Xorg, or
    //                    straight on /dev/dri without a compositor. (Where the
    //                    distro ships `sdl2-compat` -- the SDL2 ABI on top of
    //                    SDL3 -- it `provides` this name, so listing `sdl2`
    //                    resolves either way.)
    //   - sdl3:          libSDL3.so.0, the current API. Matters on the pixman
    //                    session: SDL3's Wayland backend has a native wl_shm
    //                    window framebuffer, so SDL_FRAMEBUFFER_ACCELERATION=0
    //                    + the software renderer touch NO GL at all -- the
    //                    same pure-shm path foot and lunarbg use. SDL2 lacks
    //                    that and always presents through EGL (llvmpipe here).
    //   - sdl12-compat:  the SDL 1.2 ABI (libSDL-1.2.so.0) implemented over
    //                    SDL2, for the long tail of old games/emulators.
    //   - sdl2_image/ttf/mixer/net: the companion libs nearly every SDL2
    //                    program links (image decoding, TrueType text, audio
    //                    mixing, UDP/TCP). sdl2_mixer's output goes through
    //                    SDL's audio layer (SDL_AUDIODRIVER=alsa, routed to
    //                    Pulse by /etc/asound.conf).
    //   - libpng:        named explicitly so PNG loaders (SDL_image, gdk-pixbuf
    //                    fallbacks, games) do not depend on a transitive pull.
    //   - fluidsynth:    software SoundFont synth (libfluidsynth + CLI). MIDI
    //                    in SDL_mixer / gzdoom / scummvm. Alpine's build also
    //                    pulls fluidsynth-libs and a GM soundfont.
    //   - libdecor:      client-side decorations for Wayland. labwc offers
    //                    server-side decorations via xdg-decoration, which SDL
    //                    prefers when present, so this is only the fallback
    //                    (and what SDL_VIDEO_WAYLAND_PREFER_LIBDECOR=1 uses).
    // ── Freedoom ────────────────────────────────────────────────────────────
    //   - freedoom:      the two IWADs (freedoom1.wad, freedoom2.wad). No
    //                    engine of its own; it is the game DATA.
    //   - gzdoom:        the engine the freedoom package's own launchers use.
    //                    It needs OpenGL 3.3+, so on the pixman session it
    //                    runs on llvmpipe -- playable, not fast. A software
    //                    engine (chocolate-doom, crispy-doom) is faster on
    //                    this stack and `eclipse-freedoom` prefers one when
    //                    installed; it is not in the default set because the
    //                    repo docs only ever verified gzdoom's name.
    //   - bash:          the freedoom package installs `dist/freedoom` as
    //                    /usr/bin/freedoom1 and /usr/bin/freedoom2, and that
    //                    script is `#!/usr/bin/env bash` with a genuine bash
    //                    array in it (`PATHS=( ... )` builds DOOMWADPATH), so
    //                    busybox ash cannot run it. Without bash the launcher
    //                    dies as `env: can't execute 'bash': No such file or
    //                    directory` -- observed on real hardware. Alpine puts
    //                    the binary in /bin, which the X_TREES merge below
    //                    deliberately never copies, so it also needs the
    //                    targeted copy further down.
    // These are shipped rather than left to a runtime `apk add`: an installed
    // Eclipse has no mirror in reach on first boot, which is exactly when
    // somebody wants to see whether the desktop can run a game at all.
    "freedoom",
    "gzdoom",
    "bash",
    "sdl2",
    "sdl3",
    "sdl12-compat",
    "sdl2_image",
    "sdl2_ttf",
    "sdl2_mixer",
    "sdl2_net",
    "libpng",
    "fluidsynth",
    "libdecor",
    // ── Firefox ─────────────────────────────────────────────────────────────
    // The browser itself. Everything around it was already here -- the
    // `eclipse-firefox` wrapper and its `.desktop` override (desktop.rs), the
    // software-GL stack above, and `/bin/firefox-probe`, which checks the
    // kernel interfaces it depends on -- but for a while no package was
    // installed, so the wrapper's own "firefox not found" branch was the only
    // thing that ever ran.
    //
    // Rapid-release Firefox. Alpine's `firefox` installs to /usr/lib/firefox/
    // with the binary `/usr/bin/firefox`, `firefox.desktop` and icons named
    // `firefox` (the wrapper and the .desktop override in desktop.rs follow
    // those names). libxul.so alone is ~150 MiB; it reaches the QEMU live
    // image intact because `usr/lib` is one of LIVE_TREES, which
    // `copy_into_live` copies UNCAPPED, so the 16 MiB LIVE_FILE_CAP that
    // governs the rest of the live root does not apply. The live initramfs is
    // a RAM disk, so it does grow by roughly that much.
    //
    // `firefox-esr` remains the fallback the wrapper also accepts; it is a
    // separate package with a separate binary name and does NOT `provides`
    // this one, so a mirror carrying only ESR needs ECLIPSE_XORG_PACKAGES.
    "firefox",
    // What Firefox's GPU probe (`glxtest`, run before the first window) uses
    // to read the graphics card's PCI vendor and device id: it sees
    // `/sys/bus/pci/` and then `dlopen`s `libpci.so.3`, and without the
    // library it logged `[GFX1-]: glxtest: libpci missing` at every start
    // and went on with no PCI ids at all. Alpine ships the library on its
    // own, apart from the `lspci` tool, and the sysfs files libpci reads for
    // an id-and-class scan (`devices/`, `vendor`, `device`, `class`) are the
    // ones the kernel's `/sys/bus/pci` already publishes for libdrm.
    "pciutils-libs",
];

/// Whether the build is running as root (euid 0), via `id -u` — no extra crate
/// dependency. If `id` can't be run we assume NON-root: that is the common
/// developer-build case, and it makes apk take the `--usermode` path (which a
/// genuine root build would then reject loudly rather than silently mis-owning
/// files).
fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// Whether a knob's value is one of the spellings that mean off. One function
/// rather than a copy per knob: the two lists were identical, and a spelling
/// added to one of them and not the other is a knob that answers differently
/// from its twin for no reason a reader could find.
fn knob_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "no" | "false" | ""
    )
}

/// Returns `true` unless `ECLIPSE_XORG` is explicitly set to a falsey value.
fn enabled() -> bool {
    match std::env::var("ECLIPSE_XORG") {
        Ok(v) => !knob_off(&v),
        Err(_) => true,
    }
}

/// Donde se cachean los `.apk`, **por arco**.
///
/// apk nombra el fichero cacheado `<nombre>-<version>.apk`, sin el arco
/// dentro, asi que una sola cache compartida le sirve al aarch64 el paquete de
/// x86_64 que ya estaba ahi con ese nombre exacto, sin decir nada.
fn apk_cache_dir(arch: &str) -> PathBuf {
    PROJECT_DIR.join("ignored").join("apk-cache").join(arch)
}

/// El cargador dinamico de musl, cuyo nombre lleva el arco dentro.
fn musl_loader_name(arch: &str) -> String {
    format!("ld-musl-{arch}.so.1")
}

/// El soname contra el que estan enlazados los binarios de Alpine; es un alias
/// del cargador, y tambien lleva el arco dentro.
fn musl_libc_alias_name(arch: &str) -> String {
    format!("libc.musl-{arch}.so.1")
}

/// Build one `apk add` invocation against the staging root. Factored out so the
/// bulk install and the per-package retry below cannot drift apart in their
/// flags — a retry that differed by one argument would "fail" for reasons that
/// have nothing to do with the package being tested.
#[allow(clippy::too_many_arguments)]
fn mk_apk_add(
    apk_bin: &Path,
    stage: &Path,
    arch: &str,
    repos: &Path,
    cache: &Path,
    keys: &Path,
    initdb: bool,
    update_cache: bool,
) -> Command {
    let mut cmd = Command::new(apk_bin);
    cmd.arg("add").arg("--root").arg(stage);
    // apk-tools 3.x (Chimera static build) needs --initdb to create its
    // database in the (empty) staging root — but only the first time; a later
    // add into the now-populated root must not re-init it.
    if initdb {
        cmd.arg("--initdb");
    }
    cmd.arg("--arch")
        .arg(arch)
        .arg("--repositories-file")
        .arg(repos)
        // Absolute, persistent cache. apk fetches a missing repository index
        // automatically and reuses a cached one, so NOT forcing --update-cache
        // lets an OFFLINE rebuild succeed off the .apk/index a prior online
        // build cached here (a forced refresh would hard-fail with no network).
        .arg("--cache-dir")
        .arg(cache)
        // Post-install scripts would need to chroot into the target; skip them
        // (font caches regenerate on first use).
        .arg("--no-scripts");
    // ... but a cached index also goes STALE, and a stale one is worse than no
    // index: it still lists every package name, so resolution succeeds and the
    // FETCH is what 404s, because the mirror has since moved on to newer
    // versions. A name added to DEFAULT_PACKAGES after the last online build
    // then never installs, however many times the image is rebuilt on a
    // perfectly good network -- which is how `freedoom` shipped absent. So the
    // caller asks for a refresh first and falls back to the cached index only
    // when that fails, which is the offline case the comment above is about.
    if update_cache {
        cmd.arg("--update-cache");
    }
    // apk 3.x refuses to create a database as a non-root user without
    // --usermode, and refuses --usermode AS root ("--usermode not allowed as
    // root"). The build normally runs as an unprivileged user (`make` on the
    // developer's box); CI/sudo runs as root. Pass the flag only when non-root.
    if !running_as_root() {
        cmd.arg("--usermode");
    }
    if keys.is_dir() {
        cmd.arg("--keys-dir").arg(keys);
        // Empty keys-dir still yields UNTRUSTED on every APKINDEX and a
        // 60-package install that commits nothing. --allow-untrusted lets the
        // build finish from the Alpine CDN; prefer shipping keys (see
        // tools/apk/keys).
        let has_pub = fs::read_dir(keys)
            .ok()
            .map(|it| {
                it.flatten()
                    .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pub"))
            })
            .unwrap_or(false);
        if !has_pub {
            cmd.arg("--allow-untrusted");
        }
    } else {
        cmd.arg("--allow-untrusted");
    }
    cmd
}

/// The bare package name inside an apk dependency atom, which is what `apk
/// info` prints and what apk itself keys `world` on.
///
/// apk's own grammar (`apk_dep_parse` in tools/apk/src/package.c) is
/// `[!]name[@tag][<op>version]`, where an op is one or more of `<`, `=`, `>`,
/// `~`: the name ends at the first of those characters, and the `@tag` is then
/// split off what is left. So `firefox>102`, `mesa@edge` and `mesa@edge>=24`
/// are all the name `mesa`/`firefox` to apk, and comparing the whole atom to a
/// name never matches.
///
/// This exists because `ECLIPSE_XORG_PACKAGES` is documented for exactly the
/// cases that need an atom rather than a name -- a repository whose names
/// differ, a mirror carrying only rapid-release `firefox` -- and every comparison below used to be
/// on the whole atom.
fn apk_atom_name(atom: &str) -> &str {
    let atom = atom.trim();
    let atom = atom.strip_prefix('!').unwrap_or(atom);
    let name = match atom.find(['<', '=', '>', '~']) {
        Some(i) => &atom[..i],
        None => atom,
    };
    match name.find('@') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// The requested atoms that `apk info` does not account for, in the order they
/// were asked for. Empty means every request resolved.
///
/// A free function and not a closure at the one call site, because the call
/// site is inside a 750-line `install()` that needs a real apk binary and a
/// network to reach: the comparison could go back to matching whole atoms
/// against bare names and no test could tell. This is that comparison, and
/// [`merge_apk_world`] answers the same question the same way.
fn not_installed<'a>(requested: &'a [String], installed: &[String]) -> Vec<&'a String> {
    requested
        .iter()
        .filter(|p| {
            let name = apk_atom_name(p);
            !installed.iter().any(|i| apk_atom_name(i) == name)
        })
        .collect()
}

/// Add the requested top-level packages that actually installed to `world_path`
/// (the rootfs's `/etc/apk/world`), deduplicated and additive — existing base
/// entries are preserved. `requested` is what we asked `apk add` for;
/// `installed` is the audited closure (`apk info`), used so a name that failed
/// to resolve is not written into world as if it were present.
///
/// Both comparisons are on the NAME inside the atom, never on the atom: `apk
/// info` prints bare names, so a requested `firefox>102` matched nothing and
/// was dropped on the floor -- installed, but absent from `world`, which is
/// what `apk fix` and `apk upgrade` read to decide what to keep. And an
/// existing `firefox>102` in the base world has to block appending a bare
/// `firefox`, or world ends up with two entries for one name.
fn merge_apk_world(world_path: &Path, requested: &[String], installed: &[String]) {
    let mut world: Vec<String> = std::fs::read_to_string(world_path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let before = world.len();
    for p in requested {
        let name = apk_atom_name(p);
        if installed.iter().any(|i| apk_atom_name(i) == name)
            && !world.iter().any(|w| apk_atom_name(w) == name)
        {
            world.push(p.clone());
        }
    }
    if world.len() == before {
        return;
    }
    if let Some(parent) = world_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = world.join("\n");
    body.push('\n');
    match std::fs::write(world_path, body) {
        Ok(()) => println!(
            "Xorg stack: recorded {} package(s) in {}",
            world.len() - before,
            world_path.display()
        ),
        Err(e) => eprintln!("warning: could not update {}: {e}", world_path.display()),
    }
}

/// Union the `world` at `src` into the one at `dst`, additive and deduplicated
/// BY NAME: every package named in `src` but not in `dst` is appended, base
/// entries kept. Used to carry the full rootfs's apk-world additions into the
/// live root, whose etc/apk LIVE_TREES does not copy.
///
/// By name and not by line, because the two worlds can spell the same package
/// differently -- one `firefox`, the other `firefox>102` -- and appending both
/// leaves two entries for one name.
fn union_apk_world(src: &Path, dst: &Path) {
    let src_lines: Vec<String> = match std::fs::read_to_string(src) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => return, // no source world to carry
    };
    let mut dst_lines: Vec<String> = std::fs::read_to_string(dst)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let before = dst_lines.len();
    for l in src_lines {
        let name = apk_atom_name(&l);
        if !dst_lines.iter().any(|d| apk_atom_name(d) == name) {
            dst_lines.push(l);
        }
    }
    if dst_lines.len() == before {
        return;
    }
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = dst_lines.join("\n");
    body.push('\n');
    let _ = std::fs::write(dst, body);
}

/// Populate `rootfs` with the X.Org stack. `apk_bin` is the (host-runnable)
/// apk binary already staged into the rootfs by `mod.rs`, `arch` the target
/// arch name (e.g. "x86_64"). Best-effort: never panics, never fails the build.
pub(super) fn install(rootfs: &Path, apk_bin: &Path, arch: &str) {
    if !enabled() {
        println!("Xorg stack: skipped (ECLIPSE_XORG is off)");
        return;
    }

    // `apk_bin` es el apk del HOST (`LinuxRootfs::apk_host`), no el estatico
    // del arco objetivo que se copia al rootfs: ese no arranca aqui en una
    // compilacion cruzada, y por eso este paso se saltaba entero en todo lo que
    // no fuera x86_64 — un `make release` de aarch64 salia con la variante
    // desktop sin NADA del cierre de apk, solo con un println. El arco objetivo
    // viaja en `--arch`, que es como Alpine misma cruza, y `--no-scripts` ya
    // evita el unico paso que necesitaria ejecutar binarios del objetivo.
    if !apk_bin.is_file() {
        eprintln!(
            "warning: apk binary {apk_bin:?} not found; skipping Xorg install \
             (startx will need a runtime `apk add`)"
        );
        return;
    }

    let repos = rootfs.join("etc/apk/repositories");
    let keys = rootfs.join("etc/apk/keys");
    if !repos.is_file() {
        eprintln!(
            "warning: {repos:?} missing; skipping Xorg install \
             (apk repositories not set up)"
        );
        return;
    }

    // Persistent, gitignored cache so a re-build or an OFFLINE build reuses the
    // .apk files fetched by an earlier online build instead of hitting the
    // mirror again.
    // Por ARCO. apk nombra el `.apk` cacheado `<nombre>-<version>.apk`, sin el
    // arco dentro, asi que una cache compartida le daba al aarch64 el paquete
    // de x86_64 que ya estaba ahi con ese mismo nombre.
    let cache = apk_cache_dir(arch);
    let _ = std::fs::create_dir_all(&cache);

    let packages: Vec<String> = match std::env::var("ECLIPSE_XORG_PACKAGES") {
        Ok(list) if !list.trim().is_empty() => {
            list.split_whitespace().map(str::to_string).collect()
        }
        _ => DEFAULT_PACKAGES.iter().map(|s| s.to_string()).collect(),
    };

    println!(
        "Xorg stack: installing {} package(s) via apk into a staging root ...",
        packages.len(),
    );

    // CRITICAL: install into a THROWAWAY staging root, NOT the real rootfs.
    // apk --initdb starts from an empty database, so to satisfy xorg-server it
    // pulls the ENTIRE dependency closure — including musl/libc and other base
    // packages — and writes them over whatever is at the target. Aimed at the
    // real rootfs that clobbered the hand-staged busybox/musl/ld-musl base and
    // made every shell SIGSEGV at boot (jump to a garbage PC). Install into a
    // scratch dir, then copy ONLY the X-owned trees (usr/*, X fonts, X config)
    // into the real rootfs — never /bin, /lib, /sbin or base /etc — so the base
    // system is untouched and X is purely additive.
    let stage = PROJECT_DIR.join("ignored").join("xorg-stage");
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::create_dir_all(&stage);

    let mut cmd = mk_apk_add(apk_bin, &stage, arch, &repos, &cache, &keys, true, true);
    for p in &packages {
        cmd.arg(p);
    }

    let mut outcome = cmd.status();
    // A refreshed index is the normal case; a refresh that fails means no
    // network, so fall back to whatever the cache already holds before
    // concluding anything about the packages themselves.
    if !matches!(&outcome, Ok(s) if s.success()) {
        eprintln!(
            "warning: `apk add` with a refreshed index failed; retrying off the \
             cached index (this is the offline path)"
        );
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::create_dir_all(&stage);
        let mut cached = mk_apk_add(apk_bin, &stage, arch, &repos, &cache, &keys, true, false);
        for p in &packages {
            cached.arg(p);
        }
        outcome = cached.status();
    }
    // `apk add` is ONE transaction: a single unresolvable name aborts all of
    // it, and the failure branch below only warns and skips the merge — so the
    // rootfs silently keeps whatever a PREVIOUS build left there. That reads as
    // success (X still starts, from the old files) while every package added
    // since is quietly absent. It is how `librsvg` was added to the list,
    // shipped in three builds, and never appeared in the image: the guest had
    // exactly one pixbuf loader, libpixbufloader-xpm.so.
    //
    // So on failure, retry package-by-package: the resolvable ones still land,
    // and the ones that do not get NAMED instead of taking the rest down with
    // them.
    let mut unresolved: Vec<String> = Vec::new();
    if !matches!(&outcome, Ok(s) if s.success()) {
        eprintln!(
            "warning: bulk `apk add` failed; retrying package-by-package so one \
             unresolvable name cannot void the whole X stack"
        );
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::create_dir_all(&stage);
        let mut first = true;
        let mut any_ok = false;
        for p in &packages {
            let mut c = mk_apk_add(apk_bin, &stage, arch, &repos, &cache, &keys, first, false);
            c.arg(p);
            match c.status() {
                Ok(s) if s.success() => {
                    any_ok = true;
                    first = false;
                }
                _ => unresolved.push(p.clone()),
            }
        }
        if !unresolved.is_empty() {
            eprintln!(
                "warning: these packages could NOT be installed: {}",
                unresolved.join(" ")
            );
        }
        if any_ok {
            // Something installed, so the merge below is worth doing. Synthesise
            // a success status rather than restructuring the match: this is a
            // host-only (unix) build tool.
            use std::os::unix::process::ExitStatusExt;
            outcome = Ok(std::process::ExitStatus::from_raw(0));
        }
    }
    // Audit the staging root's apk database against what was ASKED for, every
    // build, success or not. `apk add` reports failure for the transaction as a
    // whole; it does not tell you which name it could not resolve, and the
    // package-by-package retry above only runs when the bulk call fails. So a
    // requested package could be quietly absent with nothing in the output
    // naming it -- which is exactly how `glycin-image-rs` (the loader that
    // decodes PNG, and therefore the difference between a working desktop and
    // an aborting one) went missing across several builds while the console
    // showed no error at all.
    //
    // Asking apk itself rather than parsing `lib/apk/db/installed`: this build
    // uses apk-tools 3.x, whose on-disk database format is not the 2.x text
    // file, and an audit that silently reads nothing would be worse than none
    // at all.
    let installed: Vec<String> = Command::new(apk_bin)
        .arg("info")
        .arg("--root")
        .arg(&stage)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if installed.is_empty() {
        eprintln!("warning: Xorg stack: `apk info` returned nothing; cannot audit what installed");
    } else {
        // By name, like `merge_apk_world`: `apk info` prints bare names, so a
        // constrained or repository-tagged request that installed perfectly
        // well used to be reported here as NOT installed -- and this log line
        // is the only place a package going missing ever shows up.
        let missing = not_installed(&packages, &installed);
        if missing.is_empty() {
            println!(
                "Xorg stack: all {} requested packages are installed ({} in the closure)",
                packages.len(),
                installed.len()
            );
        } else {
            eprintln!(
                "warning: Xorg stack: requested but NOT installed: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    match &outcome {
        Ok(s) if s.success() => {
            // Merge ONLY the X-owned trees from the staging root into the real
            // rootfs. Never copy bin/ lib/ sbin/ or base /etc: those are the
            // hand-staged base the closure would otherwise clobber. usr/lib
            // holds the X + mesa libs (musl lives in /lib, which we skip), so
            // the base loader/libc is preserved.
            const X_TREES: &[&str] = &[
                "usr/bin",
                "usr/lib",
                "usr/libexec",
                "usr/share",
                "etc/fonts",
                "etc/X11",
                // XFCE/GTK: session defaults (xfce4/xfconf per-channel XML,
                // autostart, menus) and the D-Bus policy files.
                "etc/xdg",
                "etc/dbus-1",
            ];
            let skip_nothing = stage.join("\0does-not-exist");
            for rel in X_TREES {
                copy_uncapped(&stage.join(rel), &rootfs.join(rel), &skip_nothing);
            }
            // Record the requested top-level packages in the rootfs's apk
            // `world`. The staging install updates only the THROWAWAY root's
            // world (base /etc is deliberately never copied over), so without
            // this the X/mesa binaries are present but `apk world` /
            // `/etc/apk/world` never lists them — they read as un-owned files,
            // and an `apk fix`/`upgrade` would not know to keep them. Merge,
            // deduplicated, into whatever base world already exists: only ADD
            // the names that actually installed (per the audit above), never
            // drop or rewrite the base's own entries.
            merge_apk_world(&rootfs.join("etc/apk/world"), &packages, &installed);
            // `bash`, file by file. X_TREES deliberately never merges `bin/`
            // (the hand-staged busybox base would be clobbered), and Alpine's
            // bash package puts its only binary at /bin/bash -- so asking for
            // the package is not enough on its own: it installs into the
            // staging root and is then dropped on the floor. That is why
            // /usr/bin/freedoom2 shipped while `bash` did not, and the
            // launcher died with `env: can't execute 'bash'`. Copy just the
            // one binary, additively, and give it a /usr/bin/bash alias so
            // both spellings of the shebang resolve.
            {
                let staged = ["bin/bash", "usr/bin/bash"]
                    .into_iter()
                    .map(|rel| stage.join(rel))
                    .find(|p| p.is_file());
                if let Some(src) = staged {
                    let dst = rootfs.join("bin/bash");
                    let _ = std::fs::create_dir_all(rootfs.join("bin"));
                    let _ = std::fs::remove_file(&dst);
                    if std::fs::copy(&src, &dst).is_ok() {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
                        let alias = rootfs.join("usr/bin/bash");
                        if !alias.exists() && !alias.is_symlink() {
                            let _ = std::fs::create_dir_all(rootfs.join("usr/bin"));
                            let _ = std::os::unix::fs::symlink("../../bin/bash", &alias);
                        }
                        println!(
                            "Xorg stack: installed /bin/bash (+ /usr/bin/bash) from the closure"
                        );
                    } else {
                        eprintln!("warning: could not copy bash out of the staging root");
                    }
                } else if installed.iter().any(|i| i == "bash") {
                    eprintln!(
                        "warning: apk reports `bash` installed but neither bin/bash nor \
                         usr/bin/bash is in the staging root"
                    );
                }
            }
            // Alpine's X binaries (Xorg, mcookie, xterm, …) are dynamically
            // linked against Alpine's musl. The hand-staged base ships Eclipse's
            // own (musl-cross) `ld-musl-x86_64.so.1`, and an Alpine binary run
            // against it jumps to a bogus low PC (observed: `mcookie` SIGSEGV at
            // pc=0x1bd0, "Couldn't create cookie"). musl keeps a stable,
            // BACKWARD-compatible ABI, so installing Alpine's (newer) loader as
            // the one `/lib/ld-musl-x86_64.so.1` makes BOTH sets work: Eclipse's
            // older-musl base binaries keep running on the newer loader, and the
            // Alpine X binaries get the musl they were built against. Copy ONLY
            // this single file from the closure (not the whole base), with an
            // explicit executable mode. ECLIPSE_XORG_MUSL=0 keeps Eclipse's
            // loader (X binaries then need a matching runtime musl instead).
            let use_alpine_musl = !matches!(
                std::env::var("ECLIPSE_XORG_MUSL")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "0" | "off" | "no" | "false"
            );
            // Por arco: en aarch64 el cargador se llama
            // `ld-musl-aarch64.so.1` y con el nombre de x86_64 a pelo no se
            // copiaba nada (y el alias apuntaba a un fichero inexistente).
            let ld_name = musl_loader_name(arch);
            let stage_ld = stage.join("lib").join(&ld_name);
            let ld = rootfs.join("lib").join(&ld_name);
            let libc_alias = rootfs.join("lib").join(musl_libc_alias_name(arch));
            if use_alpine_musl && stage_ld.is_file() {
                if let Some(parent) = ld.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::remove_file(&ld);
                if std::fs::copy(&stage_ld, &ld).is_ok() {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&ld, std::fs::Permissions::from_mode(0o755));
                    println!(
                        "Xorg stack: installed Alpine musl as /lib/{ld_name} \
                         (one coherent loader for base + X; ECLIPSE_XORG_MUSL=0 to keep Eclipse's)"
                    );
                }
            }
            // `libc.musl-<arch>.so.1` is the soname the binaries NEED; it is an
            // alias of the loader now in place. Create it additively.
            if ld.is_file() && !libc_alias.exists() {
                let _ = std::os::unix::fs::symlink(&ld_name, &libc_alias);
            }
            // Xorg opens its logfile (`/var/log/Xorg.0.log`) very early — before
            // probing any device — and dies with a fatal "Cannot open log file"
            // if the directory is absent. The staged base has no `/var/log`, so
            // create it. `/tmp/.X11-unix` (the X socket dir) is created by the
            // server at runtime, and `/tmp` already exists, so we leave that be.
            let _ = std::fs::create_dir_all(rootfs.join("var/log"));
            // dbus writes its machine-id under /var/lib/dbus (seeded at first
            // boot by eclipse-x11-prepare from /etc/machine-id).
            let _ = std::fs::create_dir_all(rootfs.join("var/lib/dbus"));
            // Fontconfig scan scope — the single biggest labwc startup cost.
            //
            // The stock Alpine fonts.conf scans `/usr/share/fonts` RECURSIVELY,
            // which drags in the ~400+ X11 bitmap fonts (font-misc-misc, cursor,
            // 75dpi/100dpi, encodings, cyrillic) that Xorg reaches through its
            // OWN FontPath and that no fontconfig/pango client ever wants. A boot
            // trace of labwc (/proc/bootprofile) showed the first fontconfig user
            // opening + gunzipping + parsing every one of those files COLD — the
            // scan alone was ~110s of a ~229s startup, per-file amplified by the
            // slow cold-metadata path AND thrashing the dcache for everything
            // that ran after it.
            //
            // A prior `<rejectfont>` glob did NOT fix this and the comment that
            // claimed it did was wrong: reject filters at MATCH time, not SCAN
            // time — fontconfig still opens and parses every rejected file to
            // build its cache. The only thing that stops the scan is not listing
            // those directories. So ship a fonts.conf that scans ONLY the
            // scalable dirs the desktop actually uses (DejaVu for pango/foot);
            // the bitmap packages stay on disk for Xorg. The conf.d rendering
            // defaults are still included.
            //
            // Adwaita Sans/Mono (the `adwaita-fonts` variable fonts) are OUT
            // of the scan and deleted below, and the generic families are
            // pinned to DejaVu with strong aliases. With Adwaita in the scan,
            // `fc-match sans-serif` answered AdwaitaSans, and Firefox's chrome
            // -- which asks GTK for the system font and gets "Sans" -- drew NO
            // text at all: no tab titles, no URL-bar placeholder, no menu
            // labels, while page content (Firefox's own default list starts
            // with DejaVu Sans) rendered fine. Firefox does not rasterize that
            // variable font on this stack; nothing on the desktop asks for it
            // by name (labwc, foot, GTK settings all say DejaVu), so the only
            // way it was ever reached was through the generic alias.
            let fc_confd = rootfs.join("etc/fonts/conf.d");
            let _ = std::fs::create_dir_all(&fc_confd);
            let _ = std::fs::write(
                rootfs.join("etc/fonts/fonts.conf"),
                b"<?xml version=\"1.0\"?>\n\
                  <!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n\
                  <fontconfig>\n\
                  \x20 <description>Eclipse: scan only the scalable fonts the desktop uses; X11 bitmap fonts stay on disk for Xorg's FontPath but out of the fontconfig scan (see xtask/src/linux/xorg.rs)</description>\n\
                  \x20 <dir>/usr/share/fonts/dejavu</dir>\n\
                  \x20 <dir>/usr/local/share/fonts</dir>\n\
                  \x20 <dir prefix=\"xdg\">fonts</dir>\n\
                  \x20 <dir>~/.fonts</dir>\n\
                  \x20 <cachedir>/var/cache/fontconfig</cachedir>\n\
                  \x20 <cachedir prefix=\"xdg\">fontconfig</cachedir>\n\
                  \x20 <!-- Generic families resolve to DejaVu, ahead of anything conf.d prefers. -->\n\
                  \x20 <alias binding=\"strong\"><family>sans-serif</family><prefer><family>DejaVu Sans</family></prefer></alias>\n\
                  \x20 <alias binding=\"strong\"><family>serif</family><prefer><family>DejaVu Serif</family></prefer></alias>\n\
                  \x20 <alias binding=\"strong\"><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>\n\
                  \x20 <alias binding=\"strong\"><family>system-ui</family><prefer><family>DejaVu Sans</family></prefer></alias>\n\
                  \x20 <alias binding=\"strong\"><family>Adwaita Sans</family><prefer><family>DejaVu Sans</family></prefer></alias>\n\
                  \x20 <alias binding=\"strong\"><family>Adwaita Mono</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>\n\
                  \x20 <include ignore_missing=\"yes\">/etc/fonts/conf.d</include>\n\
                  </fontconfig>\n",
            );
            // Where fontconfig persists its per-directory caches at first use.
            // On the installed btrfs root this makes the first-boot scan a
            // one-time cost instead of silently failing the cache write.
            let _ = std::fs::create_dir_all(rootfs.join("var/cache/fontconfig"));
            // Bulletproof the narrow scan: physically delete the X11 bitmap font
            // directories. The narrow fonts.conf above stops fontconfig from
            // listing them, but a boot trace showed /usr/share/fonts STILL being
            // scanned recursively — some conf.d fragment (or a package default)
            // re-adds the parent dir, dragging the ~400+ bitmap fonts back into
            // the scan (~110s of cold open+gunzip+parse on the critical path to
            // labwc's first frame). Removing the directories makes that scan find
            // nothing regardless of which config re-adds the parent. The desktop
            // uses only the scalable DejaVu + Adwaita fonts (kept); Xorg's `fixed`
            // core font goes with them, which is acceptable for the Wayland
            // desktop. Best-effort: a missing dir is fine.
            for bitmap_dir in [
                "usr/share/fonts/misc",
                "usr/share/fonts/75dpi",
                "usr/share/fonts/100dpi",
                "usr/share/fonts/cyrillic",
                "usr/share/fonts/encodings",
                "usr/share/fonts/Type1",
                "usr/share/fonts/util",
                // Variable fonts Firefox's chrome cannot draw; see fonts.conf.
                "usr/share/fonts/Adwaita",
            ] {
                let _ = std::fs::remove_dir_all(rootfs.join(bitmap_dir));
            }
            // The etc/xdg merge above may have overwritten Eclipse's xfconf
            // defaults with Alpine's stock ones (desktop::install runs BEFORE
            // this function) — most importantly the xfwm4 channel that turns
            // the compositor OFF for the software framebuffer. Re-assert them.
            super::desktop::write_xfce_defaults(rootfs);
            // The usr/share merge above lands Alpine's icon themes on top of
            // ours, so re-assert the PNG fallbacks afterwards (they are only
            // written where no real icon exists).
            super::desktop::write_fallback_icons(rootfs);
            let _ = std::fs::remove_dir_all(&stage);
        }
        Ok(s) => {
            eprintln!(
                "warning: `apk add` for the Xorg stack exited {s:?} (mirror \
                 unreachable, or a package name not in the configured repos?). \
                 The image is still usable; startx will need a runtime \
                 `apk add`, or set ECLIPSE_XORG_PACKAGES to match your repos."
            );
        }
        Err(e) => {
            eprintln!(
                "warning: could not run apk ({e}); skipping Xorg install. \
                 The image is still usable; startx will need a runtime `apk add`."
            );
        }
    }

    // Verify and report LOUDLY either way: the whole point is that `startx`
    // works, so a build that silently shipped without the server (a warned-past
    // apk failure) must be unmistakable, not a surprise at boot.
    let xserver = ["usr/bin/Xorg", "usr/bin/X"]
        .iter()
        .find(|p| rootfs.join(p).is_file());
    let startx = rootfs.join("usr/bin/startx").is_file();
    let libinput = rootfs
        .join("usr/lib/xorg/modules/input/libinput_drv.so")
        .is_file()
        || rootfs
            .join("usr/lib/xorg/modules/input/libinput_drv.la")
            .is_file();
    match (xserver, startx) {
        (Some(_), true) => {
            println!(
                "Xorg stack: OK — X server + startx present, input driver {}.",
                if libinput {
                    "present"
                } else {
                    "MISSING (no libinput_drv.so — X will have no input!)"
                }
            );
            // The X server starting is NOT the same as the desktop starting.
            // adwaita-icon-theme is SVG, gdk-pixbuf has no built-in SVG loader,
            // and libwnck's default_icon_at_size g_asserts on a NULL pixbuf
            // rather than degrading — so a missing librsvg does not degrade the
            // icons, it kills xfce4-session and the whole session with it. The
            // check above would happily report OK for that image, and did.
            let loaders = std::fs::read_dir(rootfs.join("usr/lib/gdk-pixbuf-2.0"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path().join("loaders"))
                .find(|p| p.is_dir());
            let names: Vec<String> = loaders
                .iter()
                .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            // SVG support is EITHER a pixbuf loader module OR a glycin
            // loader. Alpine moved image decoding out of
            // `libpixbufloader-*.so` into glycin (gdk-pixbuf 2.44 pulls in
            // libglycin + glycin-svg, and librsvg becomes a library behind it
            // rather than a pixbuf module), so testing only for the .so
            // reported "NO SVG pixbuf loader" on an image that DID install
            // librsvg -- which is what this banner said for four builds.
            let glycin_svg = ["usr/lib/glycin-loaders", "usr/libexec/glycin"]
                .iter()
                .filter_map(|d| std::fs::read_dir(rootfs.join(d)).ok())
                .flatten()
                .flatten()
                .any(|e| {
                    let p = e.path();
                    p.file_name()
                        .map(|n| n.to_string_lossy().contains("svg"))
                        .unwrap_or(false)
                        || std::fs::read_dir(&p)
                            .into_iter()
                            .flatten()
                            .flatten()
                            .any(|c| c.file_name().to_string_lossy().contains("svg"))
                });
            let has_svg = names.iter().any(|n| n.contains("svg")) || glycin_svg;

            // Inventory what ACTUALLY landed in the rootfs, every build.
            //
            // Five rounds went into guessing why the desktop had no usable
            // icon: a missing package, a wrong package name, a stale loader
            // cache, glycin instead of pixbuf modules. Each guess cost a full
            // rebuild and a boot to disprove. All of it is answerable from the
            // staged tree at build time, so print it and stop guessing:
            // whether apk installed the thing is visible in whether its files
            // are here.
            let list = |rel: &str, limit: usize| -> String {
                match std::fs::read_dir(rootfs.join(rel)) {
                    Ok(rd) => {
                        let mut v: Vec<String> = rd
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect();
                        v.sort();
                        let n = v.len();
                        v.truncate(limit);
                        if n > limit {
                            format!("{} (+{} more)", v.join(" "), n - limit)
                        } else if v.is_empty() {
                            "<empty>".to_string()
                        } else {
                            v.join(" ")
                        }
                    }
                    Err(_) => "<missing>".to_string(),
                }
            };
            println!("Xorg stack: icon themes: {}", list("usr/share/icons", 12));
            // The loaders live under a VERSIONED directory --
            // usr/libexec/glycin-loaders/2+/glycin-image-rs -- so listing
            // `usr/libexec/glycin` (as this did) always answered "<missing>",
            // including on builds where glycin-svg was installed the whole
            // time. Walk one level down instead.
            let glycin_loaders: Vec<String> =
                ["usr/libexec/glycin-loaders", "usr/lib/glycin-loaders"]
                    .iter()
                    .flat_map(|base| std::fs::read_dir(rootfs.join(base)).into_iter().flatten())
                    .flatten()
                    .flat_map(|ver| std::fs::read_dir(ver.path()).into_iter().flatten())
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
            println!(
                "Xorg stack: glycin loaders: {}",
                if glycin_loaders.is_empty() {
                    "<none>".to_string()
                } else {
                    glycin_loaders.join(" ")
                }
            );
            // Did librsvg's own files arrive at all? If not, apk never
            // installed it despite it being in DEFAULT_PACKAGES; if yes, the
            // question is only which decode path uses it.
            let rsvg: Vec<String> = std::fs::read_dir(rootfs.join("usr/lib"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("rsvg"))
                .collect();
            println!(
                "Xorg stack: librsvg files in usr/lib: {}",
                if rsvg.is_empty() {
                    "<none -- apk did NOT install librsvg>".to_string()
                } else {
                    rsvg.join(" ")
                }
            );
            if has_svg {
                println!(
                    "Xorg stack: SVG decode present ({} pixbuf loader(s){}).",
                    names.len(),
                    if glycin_svg { ", via glycin" } else { "" }
                );
            } else {
                eprintln!(
                    "======================================================================\n\
                     Xorg stack: NO SVG pixbuf loader ({} loader(s): {}).\n\
                     adwaita-icon-theme is SVG, so every icon lookup returns NULL and\n\
                     libwnck aborts xfce4-session on the first one — `startx` will bring\n\
                     up X and then lose the session to SIGABRT.\n\
                     `librsvg` is in DEFAULT_PACKAGES; if it is not here, apk did not\n\
                     install it (see any per-package warning above).\n\
                     ======================================================================",
                    names.len(),
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(" ")
                    },
                );
            }
        }
        _ => {
            eprintln!(
                "======================================================================\n\
                 Xorg stack: NOT installed — the built image will say \
                 `sh: startx: not found`.\n\
                 apk could not fetch the packages (result: {outcome:?}).\n\
                 Most likely: no network to the mirror in /etc/apk/repositories, \
                 or the package\n names differ from your repo. To fix on a machine \
                 WITH internet:\n\
                 \x20 * run the build again (this step is best-effort and skips \
                 when offline), or\n\
                 \x20 * set ECLIPSE_XORG_PACKAGES=\"...\" to match your repo's names, or\n\
                 \x20 * `apk add` the stack once at runtime (it is cached).\n\
                 ======================================================================"
            );
        }
    }

    // Xwayland report. labwc (Alpine's build) auto-spawns a rootless Xwayland
    // server on demand so X11-only clients (glxgears, eglgears_x11, xterm, and
    // any toolkit falling back to X11) can run inside the Wayland session — but
    // ONLY if the `Xwayland` binary is present. It is in DEFAULT_PACKAGES, and
    // usr/bin is copied into both the installed root and the live/QEMU root
    // (LIVE_TREES), so a missing binary means apk did not resolve the package.
    // DISPLAY is pinned again in the session env (write_labwc_environment)
    // and in eclipse-init's CHILD_ENV, now that Xwayland survives its spawn
    // on hardware; it was unpinned for a while because a hung spawn made
    // every X11-probing app -- full vulkaninfo included -- block forever on
    // labwc's socket. Still report the binary's presence LOUDLY so a build
    // that shipped without Xwayland is unmistakable.
    if rootfs.join("usr/bin/Xwayland").is_file() {
        println!("Xorg stack: Xwayland present (usr/bin/Xwayland) — X11 support installed (DISPLAY=:0 pinned in the session env; see desktop.rs).");
    } else {
        eprintln!(
            "warning: Xorg stack: Xwayland NOT present (usr/bin/Xwayland missing). \
             X11-only clients (glxgears, eglgears_x11, xterm) will fail to connect \
             under labwc. `xwayland` is in DEFAULT_PACKAGES; if it is absent, apk \
             did not install it (see any per-package warning above)."
        );
    }
}

// ─── Live/QEMU initramfs inclusion ──────────────────────────────────────────
//
// The installed system (real hardware) runs the FULL btrfs rootfs, so the
// `apk add --root` above is all it needs. QEMU, however, boots the *minimal
// live initramfs* (see image.rs `build_live_rootfs`), whose `LIVE_KEEP`
// deliberately omits `usr/bin` / `usr/lib` and whose per-file cap drops big
// files — so without help X would be present on disk but absent in QEMU.
//
// `copy_into_live` copies the X-owned trees into the live root UNCAPPED so
// `startx` works in QEMU. The installed ESP's bootstrap initramfs and the
// ISO installer SFS are snapshotted *before* this copy (see image.rs), so
// Mesa/LLVM never land on the EFI partition or the ISO El Torito ESP. It
// now INCLUDES `usr/lib/dri` (see the note at `copy_into_live` itself): the
// Mesa 26.x DRI entries are just symlinks into the megadriver libraries
// already copied uncapped under `usr/lib`, so excluding them saved no space
// and only broke GL (labwc "virtio_gpu: driver missing", no desktop on the
// GL path). With them, Mesa loads virtio_gpu_dri.so (QEMU) and nouveau_dri.so
// (real hardware). Turn the QEMU copy off with `ECLIPSE_XORG_LIVE=0`.

/// X-owned trees copied verbatim (uncapped) from the full rootfs into the live
/// root. Missing entries are silently skipped, so this is safe whether or not
/// the `apk` install above actually ran.
const LIVE_TREES: &[&str] = &[
    "usr/bin", // X, Xorg, startx, xinit, xterm, xkbcomp, setxkbmap, xrandr, xset
    // seatd lives in /usr/sbin on some providers (Ubuntu); harmless when empty.
    "usr/sbin",
    // glibc runtime paths. The Alpine/musl stack never populates these, but a
    // glibc-built desktop stack (e.g. Ubuntu-packaged labwc/Xorg staged into
    // the rootfs) puts its loader in /lib64 and its libraries in
    // /lib/x86_64-linux-gnu; without copying them into the live root those
    // binaries are unrunnable in QEMU. Empty on pure-musl builds, so this
    // costs nothing there.
    "lib64",
    "lib/x86_64-linux-gnu",
    "usr/lib", // libX11/xcb/pixman/drm/input/xkbcommon + usr/lib/xorg modules (minus dri) + libvulkan*/NVK
    // Vulkan ICD manifests (usr/share/vulkan/icd.d/nouveau_icd.*.json for NVK,
    // lvp_icd.*.json for lavapipe). The loader (`libvulkan.so.1`, from usr/lib
    // above) finds each driver ONLY through its JSON; without it in the QEMU
    // live root the driver is invisible even though its .so is present --
    // and Zink (GL-on-Vulkan) then has no Vulkan to run on. lavapipe's manifest
    // is what gives Zink/glamor a software Vulkan floor when NVK is unusable.
    "usr/share/vulkan",
    "usr/libexec",     // Xorg.wrap on some layouts
    "usr/share/X11",   // xkb data, xorg.conf.d defaults, rgb.txt
    "usr/share/fonts", // base bitmap fonts X refuses to start without
    "usr/share/fontconfig",
    // libinput's device-quirks database. Without it X logs "failed to find
    // data files" and libinput falls back to degraded device behavior.
    "usr/share/libinput",
    // alsa-lib's topology: alsa.conf defines the `hw` plugin that `aplay -l`
    // and every ALSA client resolve. usr/bin (aplay) and usr/lib (libasound)
    // are already copied; without this tree QEMU boots with aplay present
    // and /dev/snd/controlC0 live, then fails with
    // "Cannot access file /usr/share/alsa/alsa.conf" / "Invalid CTL hw:0".
    "usr/share/alsa",
    // PulseAudio mixer paths / profile-sets (module-alsa-card) and locale.
    // The daemon itself is usr/bin + usr/lib (already copied); without this
    // tree a QEMU live boot has pulseaudio(1) but no alsa-mixer data.
    "usr/share/pulseaudio",
    // IANA tzdata. lunarbar's clock uses localtime_r; musl needs the zone file.
    "usr/share/zoneinfo",
    // ICU's locale data, when the build packages it as an archive rather than
    // linking it into libicudata.so: `icu-data-en`/`icu-data-full` then put the
    // blob in usr/share/icu/<ver>/icudt<maj>l.dat and only the libraries land
    // in usr/lib, which LIVE_TREES already copies. That is the same split that
    // bit alsa.conf and the glycin conf.d above -- every .so present, the data
    // the .so opens at runtime absent, failing exactly like a missing package.
    //
    // Which of the two layouts this rootfs actually has is not something the
    // build can assume, so `audit_icu_data` below prints it and warns when the
    // live root ends up with neither. This entry makes the archive layout
    // survive the copy; it is a no-op for the libicudata.so one.
    //
    // The reason to care: SpiderMonkey's JS_Init
    // (`JS::detail::InitWithFailureDiagnostic`) runs a fixed sequence of
    // RETURN_IF_FAIL checks, one of which is `ICU4CLibrary::Initialize()` ->
    // `u_init()`, and with no data blob to open that fails. JS_Init then hands
    // its caller a diagnostic string and the caller answers with
    // MOZ_CRASH_UNSAFE: a deliberate store through a null pointer. All the
    // kernel gets to print is
    //   unhandled page fault @ 0x0(WRITE | USER) ... proc=firefox
    //   pc=<libxul.so+0x1b8e250>
    // with no allocation failure logged before it -- which is the fault the
    // QEMU runs show. This entry is the hypothesis that missing ICU data is
    // what produced it, NOT a confirmed fix: the same fault is what every
    // other check in that sequence produces too.
    //
    // Uncapped like the rest of LIVE_TREES: the full blob is ~30 MiB, well
    // over LIVE_FILE_CAP.
    "usr/share/icu",
    // Boot chime MP3 + any other Eclipse-owned share files.
    "usr/share/eclipse",
    // The Freedoom IWADs (`usr/share/games/doom/freedoom{1,2}.wad`, ~27 MiB
    // each) and the `usr/share/doom` spelling some ports use. Without this
    // tree the QEMU live root carries /usr/bin/freedoom2 and gzdoom but NO
    // game data, so `eclipse-freedoom` prints "no IWAD found" and the Alpine
    // launcher finds an empty DOOMWADPATH -- the game data was the one part
    // of the stack that never reached the image. LIVE_KEEP omits usr/share
    // wholesale, so this list is the only way in.
    "usr/share/games",
    "usr/share/doom",
    "etc/fonts",
    "etc/libinput", // local-overrides.quirks (if present)
    // ── XFCE4 in QEMU ───────────────────────────────────────────────────────
    // The XFCE/GTK data the session reads at runtime. usr/bin and usr/lib
    // above already carry the binaries and libraries (gdk-pixbuf loaders, GTK
    // modules); these are the /usr/share + /etc trees that make them work.
    "usr/share/xfce4",
    "usr/share/xfwm4",
    "usr/share/themes",
    "usr/share/icons",
    "usr/share/glib-2.0", // GSettings schemas (compiled at first boot)
    // glycin's conf.d: one .conf per loader, mapping a mime type to the
    // loader binary under usr/libexec (which `usr/libexec` above already
    // brings). Without these glycin has NO loader registry, so a present
    // loader binary is never invoked and every decode fails exactly as if it
    // were absent. The booted image showed precisely that: a glycin-svg
    // binary in usr/libexec and `glycin-conf=0`.
    "usr/share/glycin-loaders",
    "usr/share/thumbnailers",
    "usr/share/dbus-1",
    "usr/share/mime",
    "usr/share/applications",
    "usr/share/desktop-directories",
    "etc/xdg",
    "etc/dbus-1",
];

fn live_enabled() -> bool {
    match std::env::var("ECLIPSE_XORG_LIVE") {
        Ok(v) => !knob_off(&v),
        Err(_) => true,
    }
}

/// Recursively copy `src` into `dst`, uncapped, preserving symlinks and
/// permissions, skipping any path under `skip`. Missing `src` is a no-op.
fn copy_uncapped(src: &Path, dst: &Path, skip: &Path) {
    if src == skip {
        return;
    }
    let md = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(_) => return,
    };
    if md.file_type().is_symlink() {
        if let Ok(target) = std::fs::read_link(src) {
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(dst);
            let _ = std::os::unix::fs::symlink(target, dst);
        }
        return;
    }
    if md.is_dir() {
        let _ = std::fs::create_dir_all(dst);
        if let Ok(rd) = std::fs::read_dir(src) {
            for entry in rd.flatten() {
                copy_uncapped(&entry.path(), &dst.join(entry.file_name()), skip);
            }
        }
        return;
    }
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(src, dst);
}

/// Print whether the Freedoom IWADs and `bash` are present under `root`, naming
/// `what` (the rootfs, the live root). Purely informational: the build never
/// fails on it, but a missing line in the log is what the next "no IWAD found"
/// report will be checked against.
pub(super) fn report_freedoom(root: &Path, what: &str) {
    let wad = |name: &str| {
        [
            "usr/share/games/doom",
            "usr/share/doom",
            "usr/share/freedoom",
        ]
        .iter()
        .map(|d| root.join(d).join(name))
        .find(|p| p.is_file())
    };
    let one = wad("freedoom1.wad");
    let two = wad("freedoom2.wad");
    let bash = root.join("bin/bash").is_file() || root.join("usr/bin/bash").is_file();
    match (&one, &two) {
        (Some(a), Some(b)) => println!(
            "Freedoom: {what} has both IWADs ({}, {}), bash={}",
            a.strip_prefix(root).unwrap_or(a).display(),
            b.strip_prefix(root).unwrap_or(b).display(),
            if bash { "yes" } else { "NO" }
        ),
        _ => eprintln!(
            "warning: Freedoom: {what} is missing {} -- `eclipse-freedoom` there will \
             say \"no IWAD found\" (bash={})",
            [
                ("freedoom1.wad", one.is_some()),
                ("freedoom2.wad", two.is_some())
            ]
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(" "),
            if bash { "yes" } else { "NO" }
        ),
    }
}

/// Total size in bytes of a directory tree (for the size notice). Best-effort.
fn tree_size(p: &Path) -> u64 {
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if md.file_type().is_symlink() {
        return 0;
    }
    if md.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
            .unwrap_or(0)
    } else {
        md.len()
    }
}

/// Copy the X.Org stack from the `full` rootfs into the `live` (QEMU) root so
/// `startx` works in QEMU. Best-effort; no-op when disabled, when Xorg was not
/// installed, or per-tree when a source path is absent. The ISO installer SFS
/// is fused before this runs.
pub(super) fn copy_into_live(full: &Path, live: &Path) {
    // `live_enabled()` (ECLIPSE_XORG_LIVE) is the master switch for staging the
    // desktop into the RAM live root; the Xorg-specific `enabled()` only gates
    // the Xorg half below, so a labwc-only build (ECLIPSE_XORG=0) still gets its
    // compositor copied in.
    if !live_enabled() {
        return;
    }
    // Copy the desktop stack into the live root when EITHER the Xorg server OR
    // the labwc/Wayland compositor is installed in the full rootfs. Both are
    // desktop binaries under usr/bin + usr/lib (+ the glibc loader trees) that
    // LIVE_KEEP deliberately omits, so the QEMU live boot cannot run the desktop
    // without them.
    //
    // labwc previously reached the live initramfs ONLY as a side effect of this
    // Xorg copy sweeping all of usr/bin+usr/lib -- so a labwc-only build (or one
    // where the Xorg apk failed) booted with the compositor binary and
    // libwlroots ABSENT: the /usr/local/bin/labwc wrapper is kept (usr/local/bin
    // is in LIVE_KEEP) but finds no /usr/bin/labwc, prints "real binary not
    // found (apk add labwc)" and exits 127, and eclipse-init just respawns it
    // forever. That is a black screen with the compositor never actually there.
    //
    // `symlink_metadata` and not `exists()`: these paths are inside a staged
    // rootfs, not a chroot, so a symlink there (Alpine ships `usr/bin/X` as
    // one) points at an ABSOLUTE path that resolves against the build HOST,
    // where it does not exist. `exists()` follows the link and answers no, and
    // answering no here returns before copying anything -- the black screen
    // above, reached a second way. Asking about the entry itself cannot be
    // fooled by where the link points.
    let staged = |p: &str| full.join(p).symlink_metadata().is_ok();
    let have_xorg = enabled()
        && ["usr/bin/Xorg", "usr/bin/X", "usr/lib/xorg"]
            .iter()
            .any(|p| staged(p));
    let have_labwc = staged("usr/bin/labwc");
    if !have_xorg && !have_labwc {
        return;
    }

    // INCLUDE mesa's DRI drivers now (previously excluded). The old rationale
    // ("too heavy for the RAM initramfs") was wrong: the heavy libraries
    // (libgallium-*.so, libLLVM-*.so) live in `usr/lib` and are ALREADY copied
    // uncapped by the `usr/lib` tree above — the `usr/lib/dri` entries are just
    // symlinks into them (Mesa 26.x megadriver). Excluding them saved nothing
    // and only broke GL: labwc logged "virtio_gpu: driver missing" / "DRI2:
    // failed to create screen" and fell back to a non-working kms_swrast, so
    // the desktop could not render on the GL path at all. Keeping them lets
    // Mesa load virtio_gpu_dri.so (QEMU + virgl) and nouveau_dri.so (real
    // hardware). `skip` is pointed at a sentinel that matches no real entry.
    let skip: PathBuf = full.join("usr/lib/__eclipse_include_all__");

    println!(
        "Desktop stack: copying into QEMU live initramfs (xorg={have_xorg} labwc={have_labwc}, including DRI drivers for GL) ..."
    );
    for rel in LIVE_TREES {
        copy_uncapped(&full.join(rel), &live.join(rel), &skip);
    }
    // Carry the apk `world` additions into the live root too. LIVE_TREES omits
    // etc/apk (base config is already present in the live rootfs), so without
    // this the desktop binaries reach the QEMU image but `apk world` on that
    // live boot would not list them. Union full's world into live's, additive.
    union_apk_world(&full.join("etc/apk/world"), &live.join("etc/apk/world"));
    let mib = tree_size(&live.join("usr")) / (1024 * 1024);
    println!("Xorg stack: live root usr/ is now ~{mib} MiB");
    // Say out loud whether the game DATA and the shell its launcher needs made
    // the crossing. Both went missing silently before -- the binaries shipped
    // and only the boot showed it -- and a build log line is the cheapest place
    // to catch it again.
    report_freedoom(live, "live root");

    // Inventory the LIVE root, not just the rootfs. The two have diverged:
    // the rootfs reports `icon themes: Adwaita hicolor` while the booted
    // guest reports one theme, which means packaging fixes have been landing
    // in a tree the RAM image never carries. Printing both sides says which
    // of the two is wrong without another boot.
    let themes = match std::fs::read_dir(live.join("usr/share/icons")) {
        Ok(rd) => {
            let mut v: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            if v.is_empty() {
                "<empty>".to_string()
            } else {
                v.join(" ")
            }
        }
        Err(_) => "<missing>".to_string(),
    };
    let fallback = live
        .join("usr/share/icons/hicolor/48x48/apps/application-x-executable.png")
        .is_file();
    println!("Xorg stack: LIVE root icon themes: {themes}");
    println!(
        "Xorg stack: LIVE root fallback PNG: {}",
        if fallback { "present" } else { "MISSING" }
    );
    if live.join("usr/share/alsa/alsa.conf").is_file() {
        println!("Xorg stack: LIVE root /usr/share/alsa/alsa.conf present (aplay/mpg123)");
    } else {
        eprintln!(
            "warning: LIVE root missing /usr/share/alsa/alsa.conf — `aplay -l` will fail \
             with Invalid CTL hw:0 even if /dev/snd/controlC0 exists. \
             `alsa-lib` must be in the apk set and usr/share/alsa in LIVE_TREES."
        );
    }
    if live.join("usr/bin/pulseaudio").is_file() {
        println!("Xorg stack: LIVE root /usr/bin/pulseaudio present");
    } else {
        eprintln!(
            "warning: LIVE root missing /usr/bin/pulseaudio — libpulse clients and \
             ALSA-via-pulse will be silent. `pulseaudio` must be in the apk set."
        );
    }
    audit_icu_data(full, live);
}

/// Whether an ICU data blob is reachable under `root`, and how.
///
/// ICU4C can be built two ways and Alpine's choice decides which tree carries
/// the data: `--with-data-packaging=archive` writes
/// `usr/share/icu/<ver>/icudt<maj>l.dat`, the default links it into
/// `usr/lib/libicudata.so.<maj>`. Only the second is covered by the `usr/lib`
/// entry in [`LIVE_TREES`], so report which layout this build actually has.
fn icu_data_layout(root: &Path) -> Option<String> {
    // Archive packaging: usr/share/icu/<ver>/*.dat.
    if let Ok(rd) = std::fs::read_dir(root.join("usr/share/icu")) {
        for ver in rd.flatten() {
            if let Ok(files) = std::fs::read_dir(ver.path()) {
                for f in files.flatten() {
                    let name = f.file_name();
                    let name = name.to_string_lossy();
                    if name.ends_with(".dat") {
                        return Some(format!(
                            "usr/share/icu/{}/{name}",
                            ver.file_name().to_string_lossy()
                        ));
                    }
                }
            }
        }
    }
    // Shared-library packaging: usr/lib/libicudata.so.<maj>[.<min>].
    if let Ok(rd) = std::fs::read_dir(root.join("usr/lib")) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // Skip the bare `libicudata.so` dev symlink: it carries no data of
            // its own and is absent from the -libs package anyway.
            if name.starts_with("libicudata.so.") {
                return Some(format!("usr/lib/{name}"));
            }
        }
    }
    None
}

/// Report whether SpiderMonkey will find ICU data in the QEMU live root.
///
/// Firefox reaching `JS_Init` and dying there is indistinguishable, from the
/// outside, from Firefox not being installed: the failing check
/// (`ICU4CLibrary::Initialize()`) reports itself by returning a diagnostic
/// string that the caller turns into a `MOZ_CRASH_UNSAFE` null store, so all
/// the kernel ever prints is an unhandled write to address 0. Saying at BUILD
/// time which ICU layout landed where is much cheaper than reading that fault
/// afterwards.
fn audit_icu_data(full: &Path, live: &Path) {
    // Only meaningful once something in the image actually links against ICU.
    // Firefox is the consumer this matters for; skip the noise otherwise.
    //
    // Both install paths: `firefox-esr` is a separate Alpine package with its
    // own tree, and the `eclipse-firefox` wrapper accepts either. Keying off
    // `usr/lib/firefox` alone would silently skip the audit on an ESR-only
    // image -- exactly the build that most needs the warning.
    let installed = |root: &Path| {
        root.join("usr/lib/firefox").is_dir() || root.join("usr/lib/firefox-esr").is_dir()
    };
    if !installed(full) && !installed(live) {
        return;
    }
    match (icu_data_layout(full), icu_data_layout(live)) {
        (_, Some(found)) => {
            println!("Xorg stack: LIVE root ICU data: {found}");
        }
        (Some(found), None) => {
            eprintln!(
                "warning: LIVE root has NO ICU data but the full rootfs has {found} — the \
                 copy into the live root dropped it. SpiderMonkey's u_init() then fails, \
                 JS_Init returns \"ICU4CLibrary::Initialize() failed\" and Firefox aborts \
                 through MOZ_CRASH (a store to address 0) before opening a window. The \
                 tree holding that file must be in LIVE_TREES."
            );
        }
        (None, None) => {
            eprintln!(
                "warning: no ICU data blob found in either root, yet Firefox is \
                 installed. SpiderMonkey's ICU4CLibrary::Initialize() will fail and \
                 Firefox will abort before opening a window. Add an `icu-data-*` package \
                 to the apk set (Alpine splits ICU into icu-libs and icu-data-en / \
                 icu-data-full)."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// apk's own grammar, taken from `apk_dep_parse` in
    /// tools/apk/src/package.c: the name ends at the first of `< = > ~`, a
    /// leading `!` is a conflict marker, and `@tag` is split off what remains.
    /// Every comparison in this module used to be on the whole atom, which
    /// matches `apk info`'s bare names only when nobody asked for a version.
    #[test]
    fn a_package_atom_is_reduced_to_the_name_apk_keys_on() {
        for (atom, name) in [
            ("xorg-server", "xorg-server"),
            ("firefox>102", "firefox"),
            ("firefox<128", "firefox"),
            ("busybox=1.36.1-r0", "busybox"),
            ("mesa-gl>=24", "mesa-gl"),
            ("mesa-gl<=25", "mesa-gl"),
            ("icu-data-full~76", "icu-data-full"),
            ("mesa@edge", "mesa"),
            ("mesa@edge>=24", "mesa"),
            ("!xf86-video-vesa", "xf86-video-vesa"),
            ("  labwc  ", "labwc"),
        ] {
            assert_eq!(apk_atom_name(atom), name, "atom {atom:?}");
        }
    }

    /// The name is everything BEFORE the operator, so a package whose name
    /// merely contains a digit or a dash keeps all of it. Getting this wrong in
    /// the other direction -- truncating at the first dash, say -- would make
    /// two different packages look like one.
    #[test]
    fn a_name_is_not_cut_anywhere_but_at_the_operator() {
        assert_eq!(apk_atom_name("sdl12-compat"), "sdl12-compat");
        assert_eq!(apk_atom_name("mesa-vulkan-nouveau"), "mesa-vulkan-nouveau");
        assert_eq!(apk_atom_name("font-misc-misc"), "font-misc-misc");
        assert_eq!(apk_atom_name("libpng"), "libpng");
        assert_ne!(apk_atom_name("mesa-gl"), apk_atom_name("mesa-egl"));
    }

    fn scratch(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eclipse-xorg-{what}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn world_of(p: &Path) -> Vec<String> {
        fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// The whole point of writing `world`: `apk fix` and `apk upgrade` read it
    /// to decide what to keep, so a package that installed and is missing from
    /// it reads as an un-owned pile of files.
    #[test]
    fn a_package_that_installed_is_recorded_and_one_that_did_not_is_not() {
        let d = scratch("record");
        let w = d.join("etc/apk/world");
        merge_apk_world(
            &w,
            &["xorg-server".into(), "xf86-video-vesa".into()],
            &["xorg-server".into(), "libx11".into()],
        );
        assert_eq!(world_of(&w), vec!["xorg-server".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    /// The build log is the only place a package going missing ever shows up,
    /// so the audit behind it has to be right in both directions: name a
    /// request that really did not resolve, and stay quiet about one that
    /// installed under a constraint or a repository tag. It used to compare the
    /// whole atom against `apk info`'s bare names, so every constrained request
    /// was reported missing and the real ones drowned in the noise.
    #[test]
    fn the_audit_names_what_did_not_install_and_nothing_else() {
        let requested: Vec<String> = ["xorg-server", "firefox>102", "mesa@edge", "xf86-video-vesa"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let installed: Vec<String> = ["xorg-server", "firefox", "mesa", "libx11"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            not_installed(&requested, &installed),
            vec![&"xf86-video-vesa".to_string()]
        );
        assert!(
            not_installed(&requested, &requested).is_empty(),
            "everything asked for is accounted for"
        );
        assert_eq!(
            not_installed(&requested, &[]).len(),
            4,
            "an audit that returned nothing accounts for nothing"
        );
    }

    /// The bug this batch is about. `apk info` prints bare names, so the atom
    /// `firefox>102` -- which is what the module's own documentation tells you
    /// to put in `ECLIPSE_XORG_PACKAGES` for a mirror carrying only one
    /// version -- matched nothing and was silently dropped from `world`,
    /// although apk had installed it.
    #[test]
    fn a_constrained_or_tagged_request_that_installed_is_still_recorded() {
        let d = scratch("constrained");
        let w = d.join("etc/apk/world");
        merge_apk_world(
            &w,
            &["firefox>102".into(), "mesa@edge".into()],
            &["firefox".into(), "mesa".into()],
        );
        assert_eq!(
            world_of(&w),
            vec!["firefox>102".to_string(), "mesa@edge".to_string()],
            "the atom is what goes into world -- apk world holds atoms -- but \
             whether it installed is asked by name"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// And the same comparison in the other direction: a name already in the
    /// base world under a different spelling must block the append, or world
    /// carries two entries for one package.
    #[test]
    fn a_name_already_in_world_is_not_added_again_under_another_spelling() {
        let d = scratch("dedup");
        let w = d.join("etc/apk/world");
        fs::create_dir_all(w.parent().unwrap()).unwrap();
        fs::write(&w, "busybox\nfirefox>102\n").unwrap();
        merge_apk_world(
            &w,
            &["firefox".into(), "labwc".into()],
            &["firefox".into(), "labwc".into()],
        );
        assert_eq!(
            world_of(&w),
            vec![
                "busybox".to_string(),
                "firefox>102".to_string(),
                "labwc".to_string()
            ],
            "firefox is already there with a constraint; labwc is new"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Additive, never destructive: the base's own entries are what keeps the
    /// hand-staged base system owned, and this function has no business
    /// rewriting them.
    #[test]
    fn the_base_entries_survive_untouched_and_in_order() {
        let d = scratch("base");
        let w = d.join("etc/apk/world");
        fs::create_dir_all(w.parent().unwrap()).unwrap();
        fs::write(&w, "alpine-base\nbusybox>=1.36\nmusl\n").unwrap();
        merge_apk_world(&w, &["labwc".into()], &["labwc".into()]);
        let got = world_of(&w);
        assert_eq!(&got[..3], &["alpine-base", "busybox>=1.36", "musl"]);
        assert_eq!(got.len(), 4);
        let _ = fs::remove_dir_all(&d);
    }

    /// Nothing to add means nothing written, so a build that installs no new
    /// package cannot reformat a world file it did not need to touch.
    #[test]
    fn a_run_that_adds_nothing_leaves_the_file_exactly_as_it_was() {
        let d = scratch("noop");
        let w = d.join("etc/apk/world");
        fs::create_dir_all(w.parent().unwrap()).unwrap();
        // Deliberately ragged: blank line, trailing spaces, no final newline.
        let raw = "alpine-base \n\nbusybox";
        fs::write(&w, raw).unwrap();
        merge_apk_world(&w, &["labwc".into()], &["xorg-server".into()]);
        assert_eq!(
            fs::read_to_string(&w).unwrap(),
            raw,
            "no additions means no write at all"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// apk reads `world` line by line, so the last entry needs its newline or
    /// the next tool to append lands on the same line and makes one nonsense
    /// package name out of two real ones.
    #[test]
    fn every_entry_ends_on_its_own_line_including_the_last() {
        let d = scratch("newline");
        let w = d.join("etc/apk/world");
        merge_apk_world(
            &w,
            &["labwc".into(), "foot".into()],
            &["labwc".into(), "foot".into()],
        );
        assert_eq!(fs::read_to_string(&w).unwrap(), "labwc\nfoot\n");
        let _ = fs::remove_dir_all(&d);
    }

    /// `apk info` returning nothing is the audited-nothing case, and the caller
    /// warns about it. What must not happen is recording the request anyway.
    #[test]
    fn an_empty_audit_records_nothing_rather_than_everything() {
        let d = scratch("noaudit");
        let w = d.join("etc/apk/world");
        merge_apk_world(&w, &["xorg-server".into(), "labwc".into()], &[]);
        assert!(
            world_of(&w).is_empty(),
            "without an audit there is no evidence anything installed"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The world file lives at `etc/apk/world` in a rootfs this function may be
    /// the first to write into.
    #[test]
    fn the_parent_directory_is_created_when_it_is_missing() {
        let d = scratch("mkdir");
        let w = d.join("etc/apk/world");
        merge_apk_world(&w, &["labwc".into()], &["labwc".into()]);
        assert!(w.is_file());
        assert!(
            fs::read_to_string(&w).unwrap().ends_with('\n'),
            "apk reads it line by line"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// `etc/apk` is not in LIVE_TREES, so the live root can reach this point
    /// with no `etc/apk` directory at all -- and a write into a directory that
    /// is not there fails silently, because every write here is a `let _ =`.
    #[test]
    fn the_union_creates_the_live_etc_apk_it_may_be_the_first_to_need() {
        let d = scratch("union-mkdir");
        let src = d.join("full/etc/apk/world");
        let dst = d.join("live/etc/apk/world");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, "labwc\nfoot\n").unwrap();
        assert!(
            !dst.parent().unwrap().exists(),
            "the live root has no etc/apk yet"
        );
        union_apk_world(&src, &dst);
        assert_eq!(
            world_of(&dst),
            vec!["labwc".to_string(), "foot".to_string()]
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// LIVE_TREES does not carry `etc/apk`, so the live root's world is the
    /// base one and the full rootfs's additions have to be unioned in.
    #[test]
    fn the_live_world_gains_what_the_full_one_added_and_keeps_its_own() {
        let d = scratch("union");
        let src = d.join("full/etc/apk/world");
        let dst = d.join("live/etc/apk/world");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, "alpine-base\nlabwc\nfoot\n").unwrap();
        fs::write(&dst, "alpine-base\nbusybox\n").unwrap();
        union_apk_world(&src, &dst);
        assert_eq!(
            world_of(&dst),
            vec![
                "alpine-base".to_string(),
                "busybox".to_string(),
                "labwc".to_string(),
                "foot".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Same defect as in `merge_apk_world`: the two worlds can spell one
    /// package differently, and a line-by-line union then writes it twice.
    #[test]
    fn the_union_does_not_duplicate_a_name_spelled_two_ways() {
        let d = scratch("union-dup");
        let src = d.join("full/etc/apk/world");
        let dst = d.join("live/etc/apk/world");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, "firefox>102\nmesa@edge\n").unwrap();
        fs::write(&dst, "firefox\nmesa\n").unwrap();
        union_apk_world(&src, &dst);
        assert_eq!(
            world_of(&dst),
            vec!["firefox".to_string(), "mesa".to_string()]
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// No source world is the "Xorg never ran" case, and it is a no-op rather
    /// than a truncation of the destination.
    #[test]
    fn a_missing_source_world_leaves_the_destination_alone() {
        let d = scratch("union-nosrc");
        let dst = d.join("live/etc/apk/world");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&dst, "alpine-base\nbusybox\n").unwrap();
        union_apk_world(&d.join("full/etc/apk/world"), &dst);
        assert_eq!(
            world_of(&dst),
            vec!["alpine-base".to_string(), "busybox".to_string()]
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The other half of this batch. `copy_into_live` returns without copying
    /// anything unless it can see Xorg or the compositor, and it used to look
    /// with `exists()`, which FOLLOWS a symlink. These paths are inside a
    /// staged rootfs rather than a chroot, so a symlink there resolves against
    /// the build host: `usr/bin/labwc -> /usr/bin/labwc.real` is present in the
    /// image and absent on the host, `exists()` says no, and the whole desktop
    /// silently never reaches the live root -- the boot then finds the wrapper,
    /// no binary behind it, and respawns forever on a black screen.
    #[test]
    fn a_symlinked_compositor_still_counts_as_installed() {
        let d = scratch("symlink-gate");
        let full = d.join("full");
        let live = d.join("live");
        fs::create_dir_all(full.join("usr/bin")).unwrap();
        fs::create_dir_all(full.join("usr/share/games/doom")).unwrap();
        fs::create_dir_all(&live).unwrap();
        // Absolute target, the way a package's own symlink is written: broken
        // when read from the host, correct once this tree is the root.
        std::os::unix::fs::symlink("/usr/bin/labwc.real", full.join("usr/bin/labwc")).unwrap();
        assert!(
            !full.join("usr/bin/labwc").exists(),
            "the fixture only means anything while the link does not resolve here"
        );
        fs::write(full.join("usr/share/games/doom/freedoom1.wad"), b"IWAD").unwrap();

        copy_into_live(&full, &live);

        assert!(
            live.join("usr/share/games/doom/freedoom1.wad").is_file(),
            "a symlinked compositor must not make the whole copy a no-op"
        );
        assert_eq!(
            fs::read_link(live.join("usr/bin/labwc")).unwrap(),
            Path::new("/usr/bin/labwc.real"),
            "and the link itself crosses as a link, not as its target"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// With neither Xorg nor a compositor there is nothing to stage, and
    /// copying the rootfs into the RAM root anyway would be the whole image
    /// twice over.
    #[test]
    fn a_rootfs_with_no_desktop_at_all_is_still_skipped() {
        let d = scratch("no-desktop");
        let full = d.join("full");
        let live = d.join("live");
        fs::create_dir_all(full.join("usr/share/games/doom")).unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::write(full.join("usr/share/games/doom/freedoom1.wad"), b"IWAD").unwrap();

        copy_into_live(&full, &live);

        assert!(
            !live.join("usr/share/games/doom").exists(),
            "no server and no compositor means nothing to copy"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The knob spellings, in one place because both knobs now share them. A
    /// substring test here would be the bug this repo has already had twice:
    /// `ECLIPSE_XORG=off-by-default` is not off.
    #[test]
    fn a_knob_is_off_only_for_the_spellings_it_documents() {
        for off in ["0", "off", "no", "false", "OFF", "False", " 0 ", "", "  "] {
            assert!(knob_off(off), "{off:?} is documented as off");
        }
        for on in [
            "1",
            "yes",
            "true",
            "on",
            "off-by-default",
            "no-really",
            "0x0",
            "00",
        ] {
            assert!(!knob_off(on), "{on:?} is not one of the off spellings");
        }
    }

    /// The game DATA has to reach the image, not just the binaries. Freedoom
    /// shipped as `/usr/bin/freedoom2` + gzdoom with no wad behind them because
    /// `LIVE_KEEP` omits `usr/share` wholesale and `LIVE_TREES` never listed the
    /// directory the wads land in, so the live root carried the launchers and
    /// nothing to launch. `bash` is the other half: the package's launchers are
    /// `#!/usr/bin/env bash`.
    #[test]
    fn the_live_root_carries_the_freedoom_wads_and_bash() {
        assert!(
            LIVE_TREES.contains(&"usr/share/games"),
            "usr/share/games holds freedoom{{1,2}}.wad; without it the live root \
             has the launchers and no game data"
        );
        for p in ["freedoom", "gzdoom", "bash"] {
            assert!(
                DEFAULT_PACKAGES.contains(&p),
                "{p} is part of the shipped Freedoom stack"
            );
        }
    }

    /// End to end over the one step that was losing them: a rootfs holding a
    /// wad and a compositor must hand both the launcher AND the wad to the
    /// live root. Listing the tree is not the same as copying it -- this runs
    /// the copy.
    #[test]
    fn copy_into_live_takes_the_wads_across() {
        let base = std::env::temp_dir().join(format!("eclipse-live-wad-{}", std::process::id()));
        let full = base.join("full");
        let live = base.join("live");
        let _ = std::fs::remove_dir_all(&base);
        for d in ["usr/bin", "usr/share/games/doom"] {
            std::fs::create_dir_all(full.join(d)).unwrap();
        }
        // `copy_into_live` only runs when a compositor or Xorg is installed.
        std::fs::write(full.join("usr/bin/labwc"), b"#!/bin/sh\n").unwrap();
        std::fs::write(full.join("usr/bin/freedoom2"), b"#!/usr/bin/env bash\n").unwrap();
        std::fs::write(
            full.join("usr/share/games/doom/freedoom2.wad"),
            b"IWAD not really",
        )
        .unwrap();
        std::fs::create_dir_all(&live).unwrap();

        copy_into_live(&full, &live);

        assert!(
            live.join("usr/share/games/doom/freedoom2.wad").is_file(),
            "the wad must reach the live root, not just the launcher"
        );
        assert!(live.join("usr/bin/freedoom2").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The shipped browser is the ESR line: lighter on RAM than rapid release
    /// (Firefox is the hungriest thing in the image, and on real hardware it
    /// was slow enough to drag labwc down) and a feature set that stands
    /// still for a year. The two are separate Alpine packages with separate
    /// binary, install dir, `.desktop` id and icon names, and neither
    /// `provides` the other, so this is the name everything else in the image
    /// (the wrapper's search order, the `.desktop` override) is keyed on.
    /// `glxtest`, Firefox's GPU probe, `dlopen`s `libpci.so.3` as soon as it
    /// sees `/sys/bus/pci/` (which the kernel publishes for libdrm) and logs
    /// `[GFX1-]: glxtest: libpci missing` when the library is not there. The
    /// library comes in its own Alpine package, separate from `pciutils`
    /// (the `lspci` tool, which nothing in the image needs).
    #[test]
    fn firefox_gpu_probe_finds_libpci() {
        assert!(
            DEFAULT_PACKAGES.contains(&"pciutils-libs"),
            "pciutils-libs (libpci.so.3, what glxtest dlopens) must be in the default package set"
        );
        assert!(
            !DEFAULT_PACKAGES.contains(&"pciutils"),
            "the lspci tool is not what Firefox needs; only the library package"
        );
    }

    #[test]
    fn the_shipped_browser_is_firefox_not_esr() {
        assert!(
            DEFAULT_PACKAGES.contains(&"firefox"),
            "firefox must be in the default package set"
        );
        assert!(
            !DEFAULT_PACKAGES.contains(&"firefox-esr"),
            "ESR must not be installed next to rapid-release: two 150 MiB \
             libxul.so in a RAM-backed image, and two menu entries"
        );
    }

    /// Un `apk add` para un arco cualquiera.
    fn apk_add_for(arch: &str) -> Vec<String> {
        apk_args(&mk_apk_add(
            Path::new("/apk"),
            Path::new("/stage"),
            arch,
            Path::new("/stage/etc/apk/repositories"),
            &apk_cache_dir(arch),
            Path::new("/no/such/keys"),
            true,
            true,
        ))
    }

    /// El `--arch` que se le pasa a apk es el del OBJETIVO, no una constante.
    /// Es lo que decide de que indice de Alpine sale el cierre de paquetes, y
    /// hasta el #1758 este paso se saltaba entero en todo lo que no fuera
    /// x86_64, asi que la variante desktop de aarch64 de `make release` salia
    /// sin un solo paquete.
    #[test]
    fn el_arco_de_apk_es_el_del_objetivo() {
        for arch in ["x86_64", "aarch64", "riscv64"] {
            let args = apk_add_for(arch);
            let i = args
                .iter()
                .position(|a| a == "--arch")
                .unwrap_or_else(|| panic!("falta --arch para {arch}: {args:?}"));
            assert_eq!(
                args.get(i + 1).map(String::as_str),
                Some(arch),
                "apk tiene que resolver para {arch}, no para el arco del host"
            );
        }
    }

    /// La cache de `.apk` va por arco. apk nombra el fichero cacheado
    /// `<nombre>-<version>.apk`, SIN el arco, asi que una cache compartida le
    /// da al aarch64 el binario de x86_64 que dejo ahi la tirada anterior de
    /// `make release`.
    #[test]
    fn la_cache_de_apk_no_se_comparte_entre_arcos() {
        let x86 = apk_cache_dir("x86_64");
        let arm = apk_cache_dir("aarch64");
        assert_ne!(x86, arm, "dos arcos no pueden compartir cache de .apk");
        assert!(
            x86.ends_with("x86_64") && arm.ends_with("aarch64"),
            "la hoja de la cache es el arco: {x86:?} / {arm:?}"
        );
        // Y lo que apk recibe es esa ruta, no otra.
        let args = apk_add_for("aarch64");
        let i = args.iter().position(|a| a == "--cache-dir").unwrap();
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some(arm.display().to_string().as_str()),
            "el --cache-dir tiene que ser el del arco"
        );
    }

    /// El cargador de musl lleva el arco en el nombre. Con `ld-musl-x86_64`
    /// escrito a pelo, en aarch64 no se copiaba nada del cierre y el alias
    /// `libc.musl-*` quedaba apuntando a un fichero que no existe.
    #[test]
    fn el_cargador_de_musl_se_nombra_por_arco() {
        assert_eq!(musl_loader_name("aarch64"), "ld-musl-aarch64.so.1");
        assert_eq!(musl_libc_alias_name("aarch64"), "libc.musl-aarch64.so.1");
        assert_eq!(musl_loader_name("x86_64"), "ld-musl-x86_64.so.1");
        assert_eq!(musl_libc_alias_name("x86_64"), "libc.musl-x86_64.so.1");
    }

    /// `install` no puede volver a rendirse por el arco. El motivo original
    /// (el estatico de apk era del objetivo y no arranca en el host) ya no
    /// aplica: lo que recibe es el apk del host.
    #[test]
    fn el_paso_de_paquetes_no_se_rinde_por_el_arco() {
        let src = include_str!("xorg.rs");
        let body = src
            .split_once("pub(super) fn install(")
            .expect("install sigue existiendo")
            .1;
        for linea in body.lines() {
            let codigo = linea.split("//").next().unwrap_or("");
            assert!(
                !codigo.contains("arch != \"x86_64\""),
                "ha vuelto el corte por arco en install: {linea}"
            );
        }
    }

    /// Every argument of one `apk add`, in order, as strings.
    fn apk_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// One `apk add` against `keys`, with everything else fixed.
    fn apk_add(keys: &Path, initdb: bool, update_cache: bool) -> Vec<String> {
        apk_args(&mk_apk_add(
            Path::new("/apk"),
            Path::new("/stage"),
            "x86_64",
            Path::new("/stage/etc/apk/repositories"),
            Path::new("/cache"),
            keys,
            initdb,
            update_cache,
        ))
    }

    /// The flags that say where apk installs and from what, each of them a
    /// reason a build broke once: no `--root` and the packages land on the
    /// build host, no `--arch` and apk resolves for the builder's own
    /// architecture, no `--repositories-file` and it reads the host's
    /// `/etc/apk/repositories`, no `--cache-dir` and an offline rebuild has
    /// nothing to reuse. They are checked as flag-and-value pairs because a
    /// misspelled flag is not a flag apk ignores -- it is an argument it takes
    /// for a package name.
    #[test]
    fn every_apk_add_says_where_to_install_for_what_and_from_where() {
        let args = apk_add(Path::new("/no/such/keys"), true, true);
        for (flag, value) in [
            ("--root", "/stage"),
            ("--arch", "x86_64"),
            ("--repositories-file", "/stage/etc/apk/repositories"),
            ("--cache-dir", "/cache"),
        ] {
            let i = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("apk add must pass {flag}: {args:?}"));
            assert_eq!(args.get(i + 1).map(String::as_str), Some(value), "{flag}");
        }
        assert_eq!(
            args.first().map(String::as_str),
            Some("add"),
            "the subcommand is `add`, not one that could remove something"
        );
        assert!(
            args.iter().any(|a| a == "--no-scripts"),
            "a post-install script would need to chroot into the target: {args:?}"
        );
    }

    /// apk-tools 3.x creates its database only when asked with `--initdb`, and
    /// only the first add into the empty staging root may ask: a later one into
    /// the now-populated root that re-inits throws away everything installed so
    /// far.
    #[test]
    fn the_package_database_is_created_once_and_never_re_initialised() {
        let keys = Path::new("/no/such/keys");
        assert!(apk_add(keys, true, true).iter().any(|a| a == "--initdb"));
        assert!(!apk_add(keys, false, true).iter().any(|a| a == "--initdb"));
    }

    /// A cached index goes stale, and a stale one is worse than none: it still
    /// lists every package name, so resolution succeeds and the FETCH is what
    /// 404s. That is how `freedoom` shipped absent from build after build on a
    /// perfectly good network. So the caller asks for a refresh first and falls
    /// back to the cached index only when that fails -- the offline build, and
    /// the only run that must not force a refresh.
    #[test]
    fn a_refreshed_index_is_asked_for_only_when_the_caller_wants_one() {
        let keys = Path::new("/no/such/keys");
        assert!(apk_add(keys, true, true)
            .iter()
            .any(|a| a == "--update-cache"));
        assert!(!apk_add(keys, true, false)
            .iter()
            .any(|a| a == "--update-cache"));
    }

    /// apk 3.x refuses to create a database as a non-root user without
    /// `--usermode`, and refuses `--usermode` AS root ("--usermode not allowed
    /// as root"). One command serves both, so the flag has to follow who is
    /// running the build: `make` on a developer's box, root under sudo or in
    /// CI.
    #[test]
    fn the_usermode_flag_follows_who_is_running_the_build() {
        let args = apk_add(Path::new("/no/such/keys"), true, true);
        assert_eq!(
            args.iter().any(|a| a == "--usermode"),
            !running_as_root(),
            "apk refuses the flag as root and refuses its absence as a user"
        );
    }

    /// What signing keys buy and what their absence costs. The trap is an
    /// EMPTY keys directory: apk takes `--keys-dir`, finds no key, calls every
    /// APKINDEX untrusted and commits nothing of a sixty-package install. So a
    /// keys directory with no `.pub` in it has to be treated exactly like no
    /// keys directory at all.
    ///
    /// Worth saying out loud: `tools/apk/keys`, which the comment on
    /// `mk_apk_add` points at, does not exist in this tree and nothing in it
    /// ships an Alpine `.rsa.pub`, so every `apk add` of a build today takes
    /// the third branch below. This test is what will notice the day keys land.
    #[test]
    fn a_keys_directory_with_no_public_key_buys_nothing_over_having_none() {
        let d = scratch("apk-keys");

        let empty = d.join("empty");
        fs::create_dir_all(&empty).unwrap();
        let args = apk_add(&empty, true, true);
        assert!(args.iter().any(|a| a == "--keys-dir"));
        assert!(
            args.iter().any(|a| a == "--allow-untrusted"),
            "an empty keys-dir leaves every index untrusted and installs nothing"
        );

        let keyed = d.join("keyed");
        fs::create_dir_all(&keyed).unwrap();
        fs::write(keyed.join("alpine-devel@example-4a6a0840.rsa.pub"), b"k").unwrap();
        let args = apk_add(&keyed, true, true);
        assert!(args.iter().any(|a| a == "--keys-dir"));
        assert!(
            !args.iter().any(|a| a == "--allow-untrusted"),
            "with a key present the signatures are what gets checked"
        );

        let args = apk_add(&d.join("absent"), true, true);
        assert!(
            !args.iter().any(|a| a == "--keys-dir"),
            "there is no directory to point apk at"
        );
        assert!(args.iter().any(|a| a == "--allow-untrusted"));

        let _ = fs::remove_dir_all(&d);
    }

    /// A name that is a prefix of another is still its own package. The set
    /// ships `mesa-gl` next to `mesa-gles` and `sdl2` next to `sdl2_image`, so
    /// a comparison that matched prefixes would report `sdl2` installed off the
    /// back of `sdl2_image`: the audit would go quiet, `world` would gain an
    /// entry for a package that is not there, and `apk fix` would read it. All
    /// three comparisons answer the same question and are checked together.
    #[test]
    fn a_name_that_is_a_prefix_of_another_is_still_its_own_package() {
        let requested = ["sdl2".to_string(), "mesa-gl".to_string()];
        let installed = ["sdl2_image".to_string(), "mesa-gles".to_string()];
        assert_eq!(
            not_installed(&requested, &installed),
            [&requested[0], &requested[1]],
            "neither request resolved; the longer names are other packages"
        );
        // The other direction of the same comparison: a name ending in a digit
        // is that name, not that name with the digit shaved off. Half this set
        // ends in one (`sdl2`, `sdl3`, `mpg123`, `xfwm4`), so getting it wrong
        // would report the whole lot missing while they sit there installed.
        assert!(
            not_installed(&requested[..1], &requested[..1]).is_empty(),
            "sdl2 installed is sdl2 asked for"
        );

        let d = scratch("prefix");
        let w = d.join("world");
        fs::write(&w, "sdl2_image\n").unwrap();
        merge_apk_world(&w, &requested, &installed);
        assert_eq!(
            world_of(&w),
            ["sdl2_image"],
            "nothing installed, so nothing is recorded"
        );

        let src = d.join("src-world");
        fs::write(&src, "sdl2\nmesa-gl\n").unwrap();
        union_apk_world(&src, &w);
        assert_eq!(
            world_of(&w),
            ["sdl2_image", "sdl2", "mesa-gl"],
            "and the union adds them rather than seeing them already there"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A blank line in an existing `world` is not a package. Carrying one
    /// through puts an empty entry in the file apk reads to decide what to
    /// keep, and makes the count in the log line wrong besides.
    #[test]
    fn a_blank_line_in_world_is_not_a_package() {
        let d = scratch("world-blanks");
        let w = d.join("world");
        fs::write(&w, "busybox\n\n  \nmusl\n").unwrap();
        merge_apk_world(&w, &["labwc".into()], &["labwc".into()]);
        assert_eq!(
            fs::read_to_string(&w).unwrap(),
            "busybox\nmusl\nlabwc\n",
            "the blank lines go, the order stays, and the file ends on a newline"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A symlink crosses AS a symlink, and has to be able to land where
    /// something already is: the live root is not empty when this runs -- the
    /// base rootfs is already fused into it -- so a package whose `usr/bin/X`
    /// is a link to `Xorg` arrives on top of whatever the base left there.
    /// `symlink` onto an existing path is EEXIST, so without clearing the
    /// destination the link is silently not made and the live root keeps the
    /// stale file.
    #[test]
    fn a_link_crosses_as_a_link_and_takes_the_place_of_what_was_there() {
        let d = scratch("copy-symlink");
        let src = d.join("src");
        let dst = d.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        std::os::unix::fs::symlink("/usr/bin/Xorg", src.join("X")).unwrap();
        fs::write(dst.join("X"), b"the base rootfs got here first").unwrap();

        copy_uncapped(&src, &dst, &d.join("matches-nothing"));

        assert_eq!(
            fs::read_link(dst.join("X")).unwrap(),
            Path::new("/usr/bin/Xorg"),
            "the link must replace the stale file, not fail on it"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A tree arrives with the shape it had, every file under the relative path
    /// it came from. Flattening it would put `usr/lib/dri/*.so` straight into
    /// `usr/lib`, where Mesa's loader does not look. A single file is the same
    /// question with nothing to recurse through: its parent has to be created
    /// too, since `fs::copy` into a directory that is not there just fails.
    #[test]
    fn a_tree_arrives_with_the_shape_it_had() {
        let d = scratch("copy-shape");
        let src = d.join("src");
        let dst = d.join("dst");
        fs::create_dir_all(src.join("dri")).unwrap();
        fs::create_dir_all(src.join("xorg/modules/drivers")).unwrap();
        fs::write(src.join("dri/nouveau_dri.so"), b"so").unwrap();
        fs::write(src.join("xorg/modules/drivers/fbdev_drv.so"), b"so").unwrap();

        copy_uncapped(&src, &dst, &d.join("matches-nothing"));

        assert!(dst.join("dri/nouveau_dri.so").is_file());
        assert!(dst.join("xorg/modules/drivers/fbdev_drv.so").is_file());

        let lone = d.join("deeper/still/alsa.conf");
        copy_uncapped(&src.join("dri/nouveau_dri.so"), &lone, &d.join("no"));
        assert!(lone.is_file(), "a file makes its own parent on the way");
        let _ = fs::remove_dir_all(&d);
    }

    /// `skip` is the one path that does not cross, and it prunes what is under
    /// it rather than only the entry itself.
    #[test]
    fn the_skipped_subtree_is_the_only_thing_left_behind() {
        let d = scratch("copy-skip");
        let src = d.join("src");
        let dst = d.join("dst");
        fs::create_dir_all(src.join("dri")).unwrap();
        fs::write(src.join("dri/swrast_dri.so"), b"so").unwrap();
        fs::write(src.join("libEGL.so.1"), b"so").unwrap();

        copy_uncapped(&src, &dst, &src.join("dri"));

        assert!(dst.join("libEGL.so.1").is_file(), "everything else crosses");
        assert!(
            !dst.join("dri").exists(),
            "the skipped directory takes its contents with it"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Mesa 26's DRI drivers are symlinks into the megadriver that `usr/lib`
    /// already carries, so they cost nothing -- and their absence is what broke
    /// GL: labwc logged "virtio_gpu: driver missing" / "DRI2: failed to create
    /// screen" and fell back to a kms_swrast that does not work. The skip is
    /// pointed at a sentinel name for exactly that reason, so a skip naming a
    /// real directory would take `usr/lib/dri` back out of the live root.
    #[test]
    fn the_dri_drivers_cross_with_the_rest_of_the_libraries() {
        let d = scratch("live-dri");
        let full = d.join("full");
        let live = d.join("live");
        fs::create_dir_all(full.join("usr/lib/dri")).unwrap();
        fs::create_dir_all(full.join("usr/bin")).unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::write(full.join("usr/bin/labwc"), b"#!/bin/sh\n").unwrap();
        fs::write(full.join("usr/lib/dri/virtio_gpu_dri.so"), b"so").unwrap();
        fs::write(full.join("usr/lib/libgallium-26.so"), b"so").unwrap();

        copy_into_live(&full, &live);

        assert!(
            live.join("usr/lib/dri/virtio_gpu_dri.so").is_file(),
            "excluding dri/ saved nothing and left GL with no driver"
        );
        assert!(live.join("usr/lib/libgallium-26.so").is_file());
        let _ = fs::remove_dir_all(&d);
    }

    /// The size notice is the only thing in the log that says how big the RAM
    /// root got, so it has to be the whole tree: files summed, a symlink
    /// counted as the nothing it is -- what it points at is already counted
    /// where it lives, and an absolute one resolves against the build host
    /// anyway -- and a path that is not there answered with 0 rather than a
    /// panic in the middle of a build.
    #[test]
    fn the_size_of_a_tree_is_its_files_summed_and_a_link_weighs_nothing() {
        let d = scratch("tree-size");
        let root = d.join("usr");
        fs::create_dir_all(root.join("lib/dri")).unwrap();
        fs::write(root.join("lib/libGL.so"), vec![0u8; 1000]).unwrap();
        fs::write(root.join("lib/dri/swrast_dri.so"), vec![0u8; 24]).unwrap();
        std::os::unix::fs::symlink("/usr/lib/libGL.so", root.join("lib/dri/alias.so")).unwrap();

        assert_eq!(tree_size(&root), 1024, "every file once, and no link");
        assert_eq!(tree_size(&root.join("lib/libGL.so")), 1000);
        assert_eq!(tree_size(&d.join("not-here")), 0);
        let _ = fs::remove_dir_all(&d);
    }

    /// ICU ships its data one of two ways and the build does not get to pick:
    /// `--with-data-packaging=archive` writes a blob under
    /// `usr/share/icu/<ver>/`, the default links it into
    /// `libicudata.so.<maj>`. Both have to be recognised, because the warning
    /// that fires when neither is reachable is all there is between a working
    /// Firefox and an unhandled write to address 0 -- SpiderMonkey's `u_init()`
    /// failing inside `JS_Init`, whose caller answers with a deliberate store
    /// through a null pointer.
    #[test]
    fn both_ways_icu_can_ship_its_data_are_recognised() {
        let d = scratch("icu-layout");

        let archive = d.join("archive");
        fs::create_dir_all(archive.join("usr/share/icu/76.1")).unwrap();
        fs::write(archive.join("usr/share/icu/76.1/icudt76l.dat"), b"blob").unwrap();
        assert_eq!(
            icu_data_layout(&archive).as_deref(),
            Some("usr/share/icu/76.1/icudt76l.dat"),
            "the archive layout is named by the blob itself, version and all"
        );

        let lib = d.join("lib");
        fs::create_dir_all(lib.join("usr/lib")).unwrap();
        fs::write(lib.join("usr/lib/libicudata.so.76.1"), b"so").unwrap();
        assert_eq!(
            icu_data_layout(&lib).as_deref(),
            Some("usr/lib/libicudata.so.76.1")
        );

        assert_eq!(
            icu_data_layout(&d.join("neither")),
            None,
            "nothing anywhere is the case the warning exists for"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// `libicudata.so` with no version behind it is the -dev symlink: it
    /// carries no data of its own and is absent from the package that ships the
    /// library. Answering with it would be worse than answering nothing,
    /// because it is the answer that silences the warning.
    #[test]
    fn the_bare_development_symlink_is_not_an_icu_data_blob() {
        let d = scratch("icu-dev");
        fs::create_dir_all(d.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("libicudata.so.76", d.join("usr/lib/libicudata.so")).unwrap();
        assert_eq!(icu_data_layout(&d), None);

        // Nor is a `.dat` outside `usr/share/icu`: that path is where ICU's
        // own data loader looks, and a blob anywhere else is not reachable.
        fs::create_dir_all(d.join("usr/share/icu-data/76.1")).unwrap();
        fs::write(d.join("usr/share/icu-data/76.1/icudt76l.dat"), b"blob").unwrap();
        assert_eq!(icu_data_layout(&d), None);
        let _ = fs::remove_dir_all(&d);
    }

    /// The failure this file has hit five times over: the package installs, its
    /// binary and its `.so` reach the live root through `usr/bin` and
    /// `usr/lib`, and the DATA it opens at runtime does not, because
    /// `LIVE_KEEP` omits `usr/share` wholesale and `LIVE_TREES` is the only way
    /// in. It then fails exactly like a missing package, with the build saying
    /// nothing: `alsa.conf` absent and `aplay -l` answering "Invalid CTL hw:0",
    /// the Vulkan ICD manifests absent and the loader enumerating zero devices,
    /// the Freedoom wads absent and the launcher finding no IWAD, glycin's
    /// conf.d absent and every image decode failing with the loader sitting
    /// right there. So for each of these: both halves, or neither.
    #[test]
    fn the_data_each_shipped_program_opens_at_runtime_has_a_tree_of_its_own() {
        for (pkg, tree, opens) in [
            (
                "alsa-lib",
                "usr/share/alsa",
                "alsa.conf, which defines the `hw` plugin",
            ),
            (
                "pulseaudio",
                "usr/share/pulseaudio",
                "the mixer paths and the profile sets",
            ),
            (
                "vulkan-loader",
                "usr/share/vulkan",
                "the ICD manifests, the only way a driver is found",
            ),
            (
                "tzdata",
                "usr/share/zoneinfo",
                "the zone files musl's localtime_r reads",
            ),
            (
                "xkeyboard-config",
                "usr/share/X11",
                "the xkb database xkbcomp compiles",
            ),
            (
                "font-misc-misc",
                "usr/share/fonts",
                "the `fixed` font X refuses to start without",
            ),
            (
                "glycin-svg",
                "usr/share/glycin-loaders",
                "the conf.d that maps a mime type to a loader",
            ),
            ("freedoom", "usr/share/games", "the IWADs"),
            ("adwaita-icon-theme", "usr/share/icons", "the icon theme"),
            ("shared-mime-info", "usr/share/mime", "the mime database"),
            ("xfce4-session", "usr/share/xfce4", "the session's own data"),
            ("dbus", "usr/share/dbus-1", "the service activation files"),
            (
                "dbus",
                "etc/dbus-1",
                "the bus configuration, without which dbus-daemon will not start",
            ),
            (
                "firefox",
                "usr/share/icu",
                "ICU's data blob, when the build packages it as an archive",
            ),
        ] {
            assert!(
                DEFAULT_PACKAGES.contains(&pkg),
                "{pkg} is part of the shipped stack"
            );
            assert!(
                LIVE_TREES.contains(&tree),
                "{pkg} is installed but the live root would not carry {tree} -- \
                 {opens}. The program reaches QEMU and fails there as if absent"
            );
        }
    }

    /// Where the programs and the libraries themselves come from. `lib64` and
    /// `lib/x86_64-linux-gnu` are empty on a pure-musl build and are what makes
    /// a glibc-built desktop stack runnable at all: its loader lives there, and
    /// without it every one of those binaries is unrunnable in QEMU.
    #[test]
    fn every_program_the_live_root_runs_comes_from_a_tree_it_copies() {
        for tree in [
            "usr/bin",
            "usr/sbin",
            "usr/lib",
            "usr/libexec",
            "lib64",
            "lib/x86_64-linux-gnu",
        ] {
            assert!(
                LIVE_TREES.contains(&tree),
                "{tree} holds programs, libraries or the loader behind them"
            );
        }
    }

    /// Every tree is a relative path -- `Path::join` with an absolute one drops
    /// the live root entirely and reads the build host's own `/usr` -- and no
    /// tree is listed twice or sits inside another, either of which copies it
    /// twice.
    #[test]
    fn no_tree_is_absolute_nor_listed_twice_nor_inside_another() {
        for (i, a) in LIVE_TREES.iter().enumerate() {
            assert!(
                !a.starts_with('/'),
                "{a} is absolute, so joining it onto the live root gives the host's own path"
            );
            for b in &LIVE_TREES[i + 1..] {
                assert_ne!(a, b, "{a} is listed twice");
                assert!(
                    !Path::new(b).starts_with(a) && !Path::new(a).starts_with(b),
                    "{a} and {b} are nested, so one of them is copied twice"
                );
            }
        }
    }

    /// What the real-hardware run found missing. X with no input driver comes
    /// up with no keyboard and no mouse, which from the outside is a hung
    /// machine; Eclipse drives X through the framebuffer rather than DRM, so
    /// fbdev has to be installed explicitly or the server falls back to
    /// `modesetting`; `startx` itself comes from `xinit`; and the server
    /// refuses to start at all without its base bitmap fonts and the cursor
    /// font.
    #[test]
    fn the_x_server_its_two_drivers_and_the_fonts_it_needs_ship_together() {
        for pkg in [
            "xorg-server",
            "xf86-video-fbdev",
            "xf86-input-libinput",
            "xinit",
            "font-misc-misc",
            "font-cursor-misc",
            "encodings",
        ] {
            assert!(
                DEFAULT_PACKAGES.contains(&pkg),
                "{pkg} is one of the pieces a usable X session needs"
            );
        }
    }

    /// Since Mesa 25.1 the default GL path on NVIDIA is Zink (GL on Vulkan)
    /// over NVK, so GL now needs a Vulkan driver and a loader underneath it:
    /// absent, every renderer failed with "DRI2: failed to load driver" and not
    /// one nouveau ioctl was ever issued. And under the hardware path there has
    /// to be a SOFTWARE floor -- with only NVK present and NVK broken the
    /// loader enumerates zero devices, Zink fails, glamor fails, and Xwayland
    /// exits with "no GL providers". lavapipe always works, so the session
    /// survives a broken NVK.
    #[test]
    fn the_gl_stack_has_a_software_floor_under_every_hardware_path() {
        for pkg in [
            "mesa-dri-gallium",
            "vulkan-loader",
            "mesa-vulkan-nouveau",
            "mesa-vulkan-swrast",
        ] {
            assert!(
                DEFAULT_PACKAGES.contains(&pkg),
                "{pkg} is part of the GL path"
            );
        }
        // wlroots' gles2 renderer dlopens libEGL.so.1 and libGLESv2.so.2,
        // which in Alpine live in these two packages and NOT in `mesa-gl`.
        // They arrive transitively through labwc today; if that chain changes,
        // EGL init fails and wlroots falls back to the pixman software renderer
        // with no error at all -- single-digit FPS that looks exactly like the
        // GPU having stopped working.
        for pkg in ["mesa-egl", "mesa-gles"] {
            assert!(
                DEFAULT_PACKAGES.contains(&pkg),
                "{pkg} is declared explicitly rather than relied on transitively"
            );
        }
    }

    /// The compositor, the seat manager it waits for and the X bridge it starts
    /// clients under. Each is the real binary behind a wrapper that
    /// `write_init_wrappers` lays down in `/usr/local/bin`, and a wrapper whose
    /// binary never installed prints "real binary not found (apk add …)", exits
    /// 127, and gets respawned by the init for the whole boot -- a black screen
    /// that looks nothing like a missing package.
    #[test]
    fn the_wayland_session_ships_the_binaries_its_wrappers_exec() {
        for pkg in ["labwc", "seatd", "xwayland"] {
            assert!(
                DEFAULT_PACKAGES.contains(&pkg),
                "an init wrapper execs {pkg}, so it has to be installed"
            );
        }
    }

    /// No package is asked for twice, counting the spellings apk collapses:
    /// `firefox` and `firefox>102` are one name to it, and a set holding both
    /// resolves the same package twice and leaves two `world` entries for it.
    #[test]
    fn no_package_is_asked_for_twice_under_any_spelling() {
        let mut seen: Vec<&str> = Vec::new();
        for atom in DEFAULT_PACKAGES {
            let name = apk_atom_name(atom);
            assert!(
                !seen.contains(&name),
                "{name} is in the default set twice (as {atom:?})"
            );
            seen.push(name);
        }
        assert_eq!(seen.len(), DEFAULT_PACKAGES.len());
    }

    /// Either desktop on its own is enough to stage, and all three spellings of
    /// an installed server count -- including the module directory, since
    /// `usr/bin/X` is a symlink the staging step may not have made yet. labwc
    /// used to reach the live root only as a side effect of the Xorg copy
    /// sweeping all of `usr/bin`, so a labwc-only build, or one whose Xorg apk
    /// failed, booted with the compositor absent: the wrapper is kept, finds no
    /// `/usr/bin/labwc`, exits 127, and the init respawns it forever.
    #[test]
    fn either_desktop_on_its_own_is_enough_to_stage_the_live_root() {
        for (tag, installed) in [
            ("xorg-bin", "usr/bin/Xorg"),
            ("xorg-x", "usr/bin/X"),
            ("xorg-modules", "usr/lib/xorg/modules/libfb.so"),
            ("labwc-only", "usr/bin/labwc"),
        ] {
            let d = scratch(tag);
            let full = d.join("full");
            let live = d.join("live");
            let one = full.join(installed);
            fs::create_dir_all(one.parent().unwrap()).unwrap();
            fs::create_dir_all(full.join("usr/share/games/doom")).unwrap();
            fs::create_dir_all(&live).unwrap();
            fs::write(&one, b"#!/bin/sh\n").unwrap();
            fs::write(full.join("usr/share/games/doom/freedoom1.wad"), b"IWAD").unwrap();

            copy_into_live(&full, &live);

            assert!(
                live.join("usr/share/games/doom/freedoom1.wad").is_file(),
                "{installed} on its own has to stage the desktop"
            );
            let _ = fs::remove_dir_all(&d);
        }
    }

    /// The union goes INTO the live root's world, which is the one the QEMU
    /// boot reads; the full rootfs's own world is the source and comes out of
    /// this untouched. Backwards, the live boot lists nothing it has and the
    /// installed image gains entries for packages that only ever lived in RAM.
    #[test]
    fn the_desktop_packages_are_recorded_in_the_live_world_not_the_other_way() {
        let d = scratch("live-world");
        let full = d.join("full");
        let live = d.join("live");
        fs::create_dir_all(full.join("usr/bin")).unwrap();
        fs::create_dir_all(full.join("etc/apk")).unwrap();
        fs::create_dir_all(live.join("etc/apk")).unwrap();
        fs::write(full.join("usr/bin/labwc"), b"#!/bin/sh\n").unwrap();
        fs::write(full.join("etc/apk/world"), "busybox\nlabwc\n").unwrap();
        fs::write(live.join("etc/apk/world"), "busybox\n").unwrap();

        copy_into_live(&full, &live);

        assert_eq!(world_of(&live.join("etc/apk/world")), ["busybox", "labwc"]);
        assert_eq!(
            world_of(&full.join("etc/apk/world")),
            ["busybox", "labwc"],
            "the full rootfs's world is the source here, not the destination"
        );
        let _ = fs::remove_dir_all(&d);
    }
}
