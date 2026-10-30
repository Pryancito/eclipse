//! Firefox: the wrapper, the names on `PATH`, the default prefs and the
//! `.desktop` override.
//!
//! Lifted out of `desktop.rs` whole, with no change to a line of it beyond
//! the visibility each item needs from here: that file was four lines under
//! the 4000-line ceiling `xtask/src/tamano.rs` enforces, and the browser is
//! the one self-contained subject in it big enough to be worth its own file.

use std::{fs, path::Path};

/// `/usr/local/bin/eclipse-firefox`: launch Firefox with WebRender.
///
/// On an NVIDIA + `nvidia.nouveau_uapi` session (GLES2/zink), use hardware
/// WebRender via zink+NVK. Otherwise force software WebRender (QEMU / no GPU /
/// `nvidia.wlr_pixman`).
///
/// It also asks for a single process, but do not count on getting one. In
/// `BrowserTabsRemoteAutostart` (`toolkit/xre/nsAppRunner.cpp`) Firefox honours
/// `MOZ_FORCE_DISABLE_E10S` only when `allowDisablingE10s` holds, and in a
/// `MOZILLA_OFFICIAL` build that means `xpc::AreNonLocalConnectionsDisabled()`
/// — false for any browser that can reach the network. Alpine sets
/// `MOZILLA_OFFICIAL=1` for both `firefox` and `firefox-esr`, so content
/// processes spawn anyway and the multi-process path (process launch,
/// `SCM_RIGHTS` fd passing, sealed memfd shared memory) is on the critical
/// path. `/bin/firefox-probe` checks exactly that path. The labwc menu, the `firefox.desktop` the panel launcher
/// scans, and any `.desktop` activation all go through this wrapper, so Firefox
/// always starts the same way regardless of launch path; every attempt is
/// logged to `$HOME/.eclipse-firefox.log` so "the browser does not open" is
/// diagnosable without a reboot.
///
/// Crucially this does NOT set `MOZ_WEBRENDER=0`: modern Firefox has no
/// non-WebRender compositor, so that would disable rendering outright (a black
/// window). Software path uses `MOZ_WEBRENDER_SOFTWARE=1`; GPU path leaves
/// WebRender on the hardware backend. Explicit-sync EVENTFD waits for fences
/// to *land* (not merely submit) so labwc does not sample a half-written
/// dmabuf — see `syncobj_eventfd::deliver_ready_waiters`.
pub(crate) fn write_firefox_wrapper(rootfs: &Path) {
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-firefox");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: Firefox with GPU WebRender when the session is on\n\
          # zink+NVK, else software WebRender. See write_firefox_wrapper.\n\
          export HOME=\"${HOME:-/root}\"\n\
          export LANG=\"${LANG:-es_ES.UTF-8}\"\n\
          case \"$LANG\" in *UTF-8|*utf8|*UTF8) ;; *) LANG=es_ES.UTF-8 ;; esac\n\
          export MOZ_ENABLE_WAYLAND=1\n\
          export GDK_PIXBUF_MODULE_FILE=\"${GDK_PIXBUF_MODULE_FILE:-/root/.cache/pixbuf-loaders.cache}\"\n\
          export GSETTINGS_BACKEND=\"${GSETTINGS_BACKEND:-memory}\"\n\
          export MOZ_FORCE_DISABLE_E10S=1\n\
          export MOZ_DISABLE_CONTENT_SANDBOX=1\n\
          export MOZ_DISABLE_GMP_SANDBOX=1\n\
          export MOZ_DISABLE_RDD_SANDBOX=1\n\
          export MOZ_DISABLE_GPU_SANDBOX=1\n\
          export MOZ_DISABLE_SOCKET_PROCESS_SANDBOX=1\n\
          # GPU WebRender when NVIDIA + nouveau_uapi and not forced to pixman;\n\
          # otherwise software (QEMU / no GPU / nvidia.wlr_pixman, or the labwc\n\
          # wrapper fell back to pixman because the GPU renderer failed to init).\n\
          #\n\
          # And only when NVK really exposes the GPU. Zink takes ANY Vulkan\n\
          # device, and lavapipe (mesa-vulkan-swrast) is installed as the\n\
          # floor for Xwayland's glamor. So with NVK unusable, the GPU path\n\
          # silently became GL on zink on lavapipe -- GL_RENDERER 'zink Vulkan\n\
          # 1.4(llvmpipe ...)', seen on the RTX -- slower than software\n\
          # WebRender, and Firefox logs a GraphicsCriticalError 'Couldn't\n\
          # sanitize GL_RENDERER' for every window. So pin the Vulkan loader to\n\
          # NVK's manifest alone, and ask vulkaninfo (vulkan-tools) whether NVK\n\
          # enumerates a device before taking the GPU path at all.\n\
          #\n\
          # One step at a time, RECORDING which one said no. As one chained\n\
          # condition it printed 'NVK exposes no GPU (vulkaninfo)' whatever had\n\
          # failed -- including a glob that matched no ICD, which is not the\n\
          # same diagnosis at all. On a machine where nvidia-smi shows labwc\n\
          # holding 63 MiB on card0, that message sends the reader after NVK\n\
          # for a fault that may be three lines above it.\n\
          NVK_WHY=\n\
          NVK_ICD=\n\
          for f in /usr/share/vulkan/icd.d/nouveau_icd*.json; do\n\
          \x20 [ -r \"$f\" ] && { NVK_ICD=$f; break; }\n\
          done\n\
          # `cat ... 2>/dev/null | tr`, not `tr < ...`: with no card0 the\n\
          # redirection fails in the SHELL, whose message 2>/dev/null on tr\n\
          # never covered -- one 'cannot open /sys/class/drm/card0/device/\n\
          # vendor' on the terminal of every launch in a session without one.\n\
          CARD0_VENDOR=$(cat /sys/class/drm/card0/device/vendor 2>/dev/null | tr -d '[:space:]')\n\
          if ! grep -q 'nvidia\\.nouveau_uapi' /proc/cmdline 2>/dev/null; then\n\
          \x20 NVK_WHY='no nvidia.nouveau_uapi in /proc/cmdline'\n\
          elif [ \"$CARD0_VENDOR\" != \"0x10de\" ]; then\n\
          \x20 NVK_WHY=\"card0 is not NVIDIA (vendor ${CARD0_VENDOR:-unreadable})\"\n\
          elif grep -q 'nvidia\\.wlr_pixman' /proc/cmdline 2>/dev/null; then\n\
          \x20 NVK_WHY='nvidia.wlr_pixman asked for the pixman session'\n\
          elif [ -e /run/labwc-renderer-fallback ]; then\n\
          \x20 NVK_WHY='labwc fell back to pixman (/run/labwc-renderer-fallback)'\n\
          elif [ -z \"$NVK_ICD\" ]; then\n\
          \x20 NVK_WHY='no NVK ICD under /usr/share/vulkan/icd.d (nouveau_icd*.json)'\n\
          elif command -v vulkaninfo >/dev/null 2>&1; then\n\
          \x20 # 2>&1, not 2>/dev/null: when vulkaninfo is the step that says no,\n\
          \x20 # its own error IS the diagnosis, and it was being thrown away.\n\
          \x20 VK_DRIVER_FILES=\"$NVK_ICD\" VK_ICD_FILENAMES=\"$NVK_ICD\" \\\n\
          \x20\x20 vulkaninfo --summary >/tmp/vulkaninfo.summary 2>&1\n\
          \x20 grep -q 'PHYSICAL_DEVICE_TYPE_DISCRETE_GPU\\|PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU' \\\n\
          \x20\x20 /tmp/vulkaninfo.summary || \\\n\
          \x20\x20 NVK_WHY=\"vulkaninfo on $NVK_ICD lists no GPU (/tmp/vulkaninfo.summary)\"\n\
          fi\n\
          if [ -z \"$NVK_WHY\" ]; then\n\
          \x20 export VK_DRIVER_FILES=\"$NVK_ICD\" VK_ICD_FILENAMES=\"$NVK_ICD\"\n\
          \x20 export GALLIUM_DRIVER=\"${GALLIUM_DRIVER:-zink}\"\n\
          \x20 export MESA_LOADER_DRIVER_OVERRIDE=\"${MESA_LOADER_DRIVER_OVERRIDE:-zink}\"\n\
          \x20 unset LIBGL_ALWAYS_SOFTWARE MOZ_WEBRENDER_SOFTWARE 2>/dev/null\n\
          \x20 export MOZ_ACCELERATED=1\n\
          else\n\
          \x20 # Software rendering. NOT MOZ_WEBRENDER=0 (that disables rendering).\n\
          \x20 #\n\
          \x20 # Drop the session's GPU pins FIRST. On a GPU session /etc/profile\n\
          \x20 # and labwc's environment export GALLIUM_DRIVER=zink,\n\
          \x20 # MESA_LOADER_DRIVER_OVERRIDE=zink and VK_DRIVER_FILES pinned to\n\
          \x20 # NVK's ICD alone, and they are INHERITED here. Leave them and\n\
          \x20 # LIBGL_ALWAYS_SOFTWARE=1 contradicts them: Mesa answers 'Not\n\
          \x20 # allowed to force software rendering when API explicitly selects\n\
          \x20 # a hardware device', zink then asks the loader for a CPU device\n\
          \x20 # that the NVK-only ICD list cannot have ('ZINK: CPU device\n\
          \x20 # requested but none found!', 'failed to choose pdev', 'egl:\n\
          \x20 # failed to create dri2 screen'), and Firefox retries forever.\n\
          \x20 # Same unset the labwc renderer fallback does, for the same reason.\n\
          \x20 unset GALLIUM_DRIVER MESA_LOADER_DRIVER_OVERRIDE 2>/dev/null\n\
          \x20 unset VK_DRIVER_FILES VK_ICD_FILENAMES MESA_VK_DEVICE_SELECT 2>/dev/null\n\
          \x20 # llvmpipe by name, not just LIBGL_ALWAYS_SOFTWARE: a software\n\
          \x20 # driver named outright is never the 'hardware device' that\n\
          \x20 # warning is about, and the launch line below then logs GL=llvmpipe\n\
          \x20 # instead of the GL=zink that hid this for a whole session.\n\
          \x20 export GALLIUM_DRIVER=llvmpipe\n\
          \x20 export LIBGL_ALWAYS_SOFTWARE=1\n\
          \x20 export MOZ_WEBRENDER_SOFTWARE=1\n\
          \x20 export MOZ_ACCELERATED=0\n\
          \x20 # The step that said no, verbatim, once per launch.\n\
          \x20 NVK_SW_REASON=\"software WebRender instead of zink+NVK: $NVK_WHY\"\n\
          fi\n\
          export MOZ_CRASHREPORTER_DISABLE=1\n\
          export NO_AT_BRIDGE=1\n\
          export GTK_A11Y=none\n\
          export XDG_CACHE_HOME=\"${XDG_CACHE_HOME:-/tmp/xdg-cache}\"\n\
          mkdir -p \"$XDG_CACHE_HOME\"\n\
          FLOG=\"${HOME:-/root}/.eclipse-firefox.log\"\n\
          # ABSOLUTE paths, never `command -v firefox`: /usr/local/bin/firefox\n\
          # is the shim that got us here and comes FIRST on PATH, so a name\n\
          # lookup would re-enter this script forever. firefox before\n\
          # firefox-esr, install dir before /usr/bin (which on a Debian rootfs\n\
          # is a symlink into the install dir anyway).\n\
          FFBIN=\n\
          for c in /usr/lib/firefox/firefox /usr/lib/firefox-esr/firefox-esr \\\n\
          \x20\x20\x20\x20\x20\x20\x20\x20 /usr/bin/firefox /usr/bin/firefox-esr \\\n\
          \x20\x20\x20\x20\x20\x20\x20\x20 /bin/firefox /bin/firefox-esr; do\n\
          \x20 if [ -x \"$c\" ]; then FFBIN=$c; break; fi\n\
          done\n\
          if [ -z \"$FFBIN\" ]; then\n\
          \x20 echo 'eclipse-firefox: firefox not found (apt-get install firefox)' >&2\n\
          \x20 echo 'eclipse-firefox: firefox not found' >>\"$FLOG\"\n\
          \x20 exit 127\n\
          fi\n\
          echo \"[$(date '+%H:%M:%S')] $FFBIN $* (WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-UNSET} GL=${GALLIUM_DRIVER:-sw} ACCEL=${MOZ_ACCELERATED:-?})\" >>\"$FLOG\"\n\
          [ -n \"${NVK_SW_REASON:-}\" ] && echo \"eclipse-firefox: $NVK_SW_REASON\" >>\"$FLOG\"\n\
          # Launched from a terminal, stderr belongs to the terminal: swallowing\n\
          # it into the log is how `firefox` typed into foot would print nothing\n\
          # at all. Off a tty (.desktop, menu, autostart) the log is the only\n\
          # place it could go, so keep the redirect there.\n\
          if [ -t 2 ]; then\n\
          \x20 exec \"$FFBIN\" \"$@\"\n\
          fi\n\
          exec \"$FFBIN\" \"$@\" 2>>\"$FLOG\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }

    write_firefox_name_shims(&localbin);
}

/// `/usr/local/bin/firefox` and `/usr/local/bin/firefox-esr`: the browser's
/// OWN names, pointing at [`write_firefox_wrapper`]'s script.
///
/// Until these existed the wrapper was reachable only through a `.desktop`
/// file — the panel launcher, the labwc menu, `eclipse-view`. A `firefox`
/// typed into a terminal ran the packaged binary directly and got NONE of
/// the wrapper's environment, which is not a cosmetic difference:
///
/// * `MOZ_CRASHREPORTER_DISABLE=1` is unset, so a fault is swallowed by
///   Breakpad into a `.dmp` nobody can read here instead of reaching the
///   kernel, where `[exit] ... at pc=` names the library and offset. That is
///   what `ExceptionHandler::GenerateDump attempting to generate:` followed
///   by a row of `Exiting due to channel error.` is: the parent faulted,
///   Breakpad ate the signal, the children only saw the channel close.
/// * `MOZ_DISABLE_RDD_SANDBOX` and its four siblings are unset, so the media
///   decoder and the socket process start sandboxed.
/// * The zink+NVK / software-WebRender decision never runs, so a session
///   where NVK enumerates no device falls into GL on lavapipe.
/// * `$HOME/.eclipse-firefox.log` gets no line.
///
/// `/usr/local/bin` is first on `PATH` (`/etc/profile`, eclipse-init's
/// `CHILD_ENV`, the labwc environment), so these shadow `/usr/bin/firefox`
/// the way `/usr/local/bin/labwc` already shadows the packaged compositor.
/// The wrapper resolves the real binary by absolute path for this reason: a
/// `command -v firefox` would now find the shim and loop.
fn write_firefox_name_shims(localbin: &Path) {
    use std::os::unix::fs::PermissionsExt;

    for name in ["firefox", "firefox-esr"] {
        let shim = localbin.join(name);
        fs::write(
            &shim,
            b"#!/bin/sh\n\
              # Eclipse OS: the browser's own name on PATH, so a `firefox`\n\
              # typed into a terminal starts the same way the menu does.\n\
              # See write_firefox_name_shims in xtask/src/linux/desktop.rs.\n\
              exec /usr/local/bin/eclipse-firefox \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Default prefs shipped next to the Firefox install (`defaults/pref/*.js`
/// is read for every profile, the same slot `channel-prefs.js` uses).
///
/// The QEMU root is an SFS image in RAM, sized with ~40% headroom over its
/// payload and with no `/home` split; `HOME=/root` lives on it. Firefox's
/// disk cache defaults to "smart size" -- up to 1 GiB, chosen from the free
/// space it sees -- and writes it under `~/.cache/mozilla`. A cache on a RAM
/// disk buys nothing and, on this image, fills the root in a couple of
/// minutes of browsing: the SFS then reports `unused_blocks: 0`, and until
/// the vendored `alloc_block` was wired in that was a kernel panic rather
/// than ENOSPC. Keep the memory cache; turn the disk cache off.
///
/// Video is the other thing tuned here. Everything in this image runs on the
/// CPU: there is no hardware video decoder, WebRender is the software one, and
/// the kernel's own overhead lands on the same cores. YouTube asks the browser
/// which codecs Media Source Extensions can play and picks the most efficient
/// for bandwidth -- AV1, then VP9 -- which are also the most expensive to
/// decode in software. With the CPU pinned at 97% YouTube Music never got past
/// the buffering spinner. With AV1 off and VP9 off for MSE, YouTube falls back
/// to H.264, which Firefox decodes through the system `libavcodec`
/// (`ffmpeg7-libavcodec`, a runtime dependency of Alpine's `firefox`, so it
/// is always there). A `<video>` pointing at a plain VP9 or AV1 file still
/// plays: only what MSE advertises changes.
///
/// HTTP/3 is off for the same machine. QUIC runs over UDP, and a UDP socket
/// here holds a fixed 64 KiB while TCP's holds 512 KiB; when the CPU is
/// saturated, the thread that drains the socket runs late and QUIC's packets
/// are dropped at the socket, which QUIC reads as congestion and slows down.
/// HTTP/2 over TCP carries the same streams without that cliff.
///
/// The last block is what Firefox does on its own at start-up. The QEMU root
/// is a RAM image loaded fresh at every boot, so the profile never survives
/// and EVERY launch is a first run: the privacy-notice tab, about:welcome,
/// telemetry, Normandy and a prelaunched spare content process all ran each
/// time, and Firefox sat at 50-80% of the six CPUs with nothing open. Safe
/// Browsing and site isolation (Fission) stay on: they are protection, not
/// overhead to trim here. (`MOZ_FORCE_DISABLE_E10S` in the wrapper does not
/// help: an official build ignores it unless non-local connections are
/// disabled, see `BrowserTabsRemoteAutostart` in `nsAppRunner.cpp`.)
///
/// Both package names are covered because `firefox` and `firefox-esr` are
/// separate Alpine packages with separate install dirs (see the wrapper).
/// Nothing is written when neither is installed in the rootfs -- which is
/// why this is NOT part of [`install`]: on a from-scratch build that runs
/// before `xorg::install` has fetched the package. The rootfs build calls
/// this after the package step, on both the fresh and the incremental path.
pub(crate) fn write_firefox_default_prefs(rootfs: &Path) {
    for (_, dir) in FIREFOX_PACKAGES {
        let app = rootfs.join(dir);
        if !app.is_dir() {
            continue;
        }
        let pref_dir = app.join("defaults/pref");
        let _ = fs::create_dir_all(&pref_dir);
        fs::write(
            pref_dir.join("eclipse-os.js"),
            b"// Eclipse OS defaults -- see write_firefox_default_prefs in\n\
              // xtask/src/linux/desktop.rs.\n\
              // The root filesystem is a RAM-backed SFS image with little\n\
              // headroom: no on-disk cache (the memory cache stays on).\n\
              pref(\"browser.cache.disk.enable\", false);\n\
              pref(\"browser.cache.disk.smart_size.enabled\", false);\n\
              pref(\"browser.cache.disk.capacity\", 0);\n\
              // Cap the memory cache so a long session cannot eat the live\n\
              // image; 96 MiB is enough for a few tabs under software WR.\n\
              pref(\"browser.cache.memory.capacity\", 98304);\n\
              // Prefer GPU WebRender when the session exposes zink+NVK; the\n\
              // eclipse-firefox wrapper forces software when there is no GPU.\n\
              pref(\"gfx.webrender.all\", true);\n\
              pref(\"layers.acceleration.force-enabled\", true);\n\
              // Keep WR in-process: a separate GPU process often stalls on\n\
              // Wayland/zink teardown here and trips nsTerminator's shutdown\n\
              // hang crash (intentional null write of reason 0xf2).\n\
              pref(\"layers.gpu-process.enabled\", false);\n\
              // zink+NVK / profile IO on a RAM rootfs can make a quit phase\n\
              // exceed the stock 60s nsTerminator budget; raise it so closing\n\
              // Firefox does not MOZ_CRASH mid-teardown.\n\
              pref(\"toolkit.asyncshutdown.crash_timeout\", 300000);\n\
              // No hardware video decoder on many Eclipse builds: make\n\
              // YouTube and other MSE players pick H.264 (system libavcodec)\n\
              // over AV1 and VP9, the costliest codecs to decode in software.\n\
              pref(\"media.hardware-video-decoding.enabled\", false);\n\
              pref(\"media.av1.enabled\", false);\n\
              pref(\"media.mediasource.vp9.enabled\", false);\n\
              // QUIC over a 64 KiB UDP socket drops packets when the CPU is\n\
              // busy; HTTP/2 over TCP carries the same streams.\n\
              pref(\"network.http.http3.enable\", false);\n\
              // Session store less often: each write is msync + profile IO on\n\
              // a RAM rootfs that still pays the syscall path.\n\
              pref(\"browser.sessionstore.interval\", 60000);\n\
              // The QEMU root is a RAM image loaded fresh at every boot, so\n\
              // every launch is a first run. Skip the first-run pages (the\n\
              // privacy notice, about:welcome, the what's-new page) and the\n\
              // default-browser check: each is a page load on a CPU-only\n\
              // renderer before the user has opened anything.\n\
              pref(\"datareporting.policy.dataSubmissionPolicyBypassNotification\", true);\n\
              pref(\"datareporting.policy.firstRunURL\", \"\");\n\
              pref(\"browser.aboutwelcome.enabled\", false);\n\
              pref(\"browser.startup.homepage_override.mstone\", \"ignore\");\n\
              pref(\"browser.shell.checkDefaultBrowser\", false);\n\
              // Background work that runs whether or not a page is open:\n\
              // telemetry collection and upload, Normandy studies, and the\n\
              // Pocket stories feed on the new-tab page.\n\
              pref(\"datareporting.healthreport.uploadEnabled\", false);\n\
              pref(\"toolkit.telemetry.unified\", false);\n\
              pref(\"toolkit.telemetry.archive.enabled\", false);\n\
              pref(\"app.normandy.enabled\", false);\n\
              pref(\"app.shield.optoutstudies.enabled\", false);\n\
              pref(\"browser.newtabpage.activity-stream.feeds.section.topstories\", false);\n\
              // No spare content process launched ahead of time: here a new\n\
              // process is a full fork+exec that maps libxul and starts a JS\n\
              // engine, so a spare costs seconds of CPU at every launch.\n\
              pref(\"dom.ipc.processPrelaunch.enabled\", false);\n",
        )
        .unwrap();
    }
}

/// The two Alpine packages that can put a Firefox in the image, as
/// (package = `.desktop` id = icon name, install dir under the rootfs). Both
/// names come from the package: `firefox` installs
/// `/usr/share/applications/firefox.desktop`, icons named `firefox.png` and
/// `/usr/lib/firefox/`; `firefox-esr` the same with its own name. The first
/// one is the default package; see DEFAULT_PACKAGES.
pub(crate) const FIREFOX_PACKAGES: &[(&str, &str)] = &[
    ("firefox", "usr/lib/firefox"),
    ("firefox-esr", "usr/lib/firefox-esr"),
];

/// User-level `.desktop` override so lunarbar's launcher menu (and any XDG
/// activation) runs Firefox through `eclipse-firefox` instead of the stock
/// package's `Exec=`, which tries hardware GL/WebRender and paints black on
/// this GPU-less stack. lunarbar scans `$XDG_DATA_HOME/applications` before
/// `/usr/share/applications` and keeps the first entry per desktop-file id, so
/// this shadows the packaged file without touching it.
///
/// The id has to be the installed package's: `firefox` ships `firefox.desktop`
/// and icons named `firefox`, so an override called `firefox-esr.desktop`
/// would not shadow anything -- the menu would show the packaged entry (stock
/// `Exec`, no wrapper) AND ours, the second with an icon that does not exist.
/// One override per package that is actually in the rootfs, and the one for a
/// package that is not there is removed, so an incremental build that switched
/// packages does not keep a stale entry. With no Firefox installed at all (an
/// offline build) the default package's entry is written, so a later `apt-get install`
/// is shadowed from the start. Runs after apk, next to
/// [`write_firefox_default_prefs`], for that reason.
pub(crate) fn write_firefox_desktop_override(rootfs: &Path) {
    let dir = rootfs.join("root/.local/share/applications");
    let _ = fs::create_dir_all(&dir);
    let installed: Vec<&str> = FIREFOX_PACKAGES
        .iter()
        .filter(|(_, app)| rootfs.join(app).is_dir())
        .map(|(id, _)| *id)
        .collect();
    let wanted: Vec<&str> = if installed.is_empty() {
        vec![FIREFOX_PACKAGES[0].0]
    } else {
        installed
    };
    for (id, _) in FIREFOX_PACKAGES {
        let entry = dir.join(format!("{id}.desktop"));
        if !wanted.contains(id) {
            let _ = fs::remove_file(&entry);
            continue;
        }
        fs::write(
            &entry,
            format!(
                "[Desktop Entry]\n\
                 Version=1.0\n\
                 Type=Application\n\
                 Name=Firefox\n\
                 GenericName=Web Browser\n\
                 Comment=Browse the Web (Eclipse OS software-rendering wrapper)\n\
                 Exec=/usr/local/bin/eclipse-firefox %u\n\
                 TryExec=/usr/local/bin/eclipse-firefox\n\
                 Terminal=false\n\
                 Icon={id}\n\
                 Categories=Network;WebBrowser;\n\
                 MimeType=text/html;x-scheme-handler/http;x-scheme-handler/https;\n\
                 StartupNotify=false\n"
            ),
        )
        .unwrap();
    }
}
