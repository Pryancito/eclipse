//! Eclipse OS desktop for labwc — everything the compositor session needs to
//! look like a real desktop instead of a bare black modeset buffer:
//!
//! - a night-sky wallpaper with the Eclipse logo, rendered at build time by
//!   [`wallpaper`] (pure Rust, no image crates) into
//!   `/usr/share/backgrounds/eclipse/eclipse-night.png`;
//! - an openbox-style dark theme (`Eclipse-Dark`) under `/usr/share/themes`
//!   that labwc picks up for titlebars, menus and OSDs;
//! - labwc config in `/root/.config/labwc/`: `rc.xml` (theme + keybinds),
//!   `menu.xml` (right-click desktop menu), `environment` (cursor/GTK vars)
//!   and `autostart` (wallpaper, panel, first terminal — each guarded so a
//!   missing client never aborts the session);
//! - a waybar bottom panel (dark, purple accent, taskbar + clock + sysinfo)
//!   in `/root/.config/waybar/`;
//! - dark-mode defaults for GTK 3/4 and a matching `foot` terminal palette;
//! - the bulletproof `/usr/local/bin/labwc` wrapper that pins the pixman
//!   renderer environment (login(1) strips arbitrary env vars, so a wrapper
//!   is the only delivery that survives every launch path).
//!
//! Runtime packages (all optional, the session degrades gracefully):
//! `apk add labwc waybar foot swaybg font-dejavu adwaita-icon-theme`.

use std::{fs, path::Path};

/// Install the whole desktop into the rootfs. Called from the rootfs build;
/// everything is plain file writes, so it is safe on incremental rebuilds.
pub fn install(rootfs: &Path) {
    write_wallpaper(rootfs);
    write_theme(rootfs);
    write_labwc_rc(rootfs);
    write_labwc_menu(rootfs);
    write_labwc_environment(rootfs);
    write_labwc_autostart(rootfs);
    write_waybar(rootfs);
    write_gtk_settings(rootfs);
    write_foot_config(rootfs);
    write_labwc_wrapper(rootfs);
    write_terminal_wrapper(rootfs);
}

/// `/usr/local/bin/eclipse-terminal`: launch the first terminal that exists.
/// foot is preferred (pixman/shm, matches this stack); alacritty is the
/// fallback, forced onto software GL (client-side llvmpipe renders via shm,
/// which does not touch the DRM GL path that hangs this box). Keybinds, the
/// desktop menu, the panel launcher and autostart all go through this, so
/// "a terminal" keeps working no matter which one is installed.
fn write_terminal_wrapper(rootfs: &Path) {
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-terminal");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: launch whichever terminal is installed.\n\
          if command -v foot >/dev/null 2>&1; then\n\
          \x20 exec foot \"$@\"\n\
          fi\n\
          if command -v alacritty >/dev/null 2>&1; then\n\
          \x20 LIBGL_ALWAYS_SOFTWARE=1 exec alacritty \"$@\"\n\
          fi\n\
          echo 'eclipse-terminal: no terminal found (apk add foot)' >&2\n\
          exit 127\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn write_wallpaper(rootfs: &Path) {
    let dir = rootfs.join("usr/share/backgrounds/eclipse");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("eclipse-night.png");
    // The render is deterministic, so skip the (CPU-heavy) redraw when the
    // file already exists — incremental rootfs rebuilds stay fast.
    if !path.exists() {
        let png = wallpaper::render_png(1600, 900);
        fs::write(&path, png).unwrap();
    }
}

/// Openbox-3 `themerc` consumed by labwc for window decorations, menus and
/// OSDs. Palette: deep purple night (matches the wallpaper), lavender text,
/// violet accents.
fn write_theme(rootfs: &Path) {
    let dir = rootfs.join("usr/share/themes/Eclipse-Dark/openbox-3");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("themerc"),
        b"# Eclipse OS - dark labwc/openbox theme.\n\
          border.width: 1\n\
          padding.width: 8\n\
          padding.height: 5\n\
          \n\
          window.active.border.color: #6b5aa8\n\
          window.inactive.border.color: #29233f\n\
          window.active.title.bg.color: #191430\n\
          window.inactive.title.bg.color: #131022\n\
          window.active.label.text.color: #e8e4f8\n\
          window.inactive.label.text.color: #837da0\n\
          window.active.button.unpressed.image.color: #e8e4f8\n\
          window.active.button.pressed.image.color: #9b8ae0\n\
          window.active.button.hover.image.color: #ffffff\n\
          window.inactive.button.unpressed.image.color: #837da0\n\
          \n\
          menu.title.bg.color: #191430\n\
          menu.title.text.color: #9b8ae0\n\
          menu.items.bg.color: #14101f\n\
          menu.items.text.color: #e8e4f8\n\
          menu.items.disabled.text.color: #5a5478\n\
          menu.items.active.bg.color: #6b5aa8\n\
          menu.items.active.text.color: #ffffff\n\
          menu.separator.color: #3a3357\n\
          menu.separator.padding.height: 4\n\
          \n\
          osd.bg.color: #191430\n\
          osd.border.color: #6b5aa8\n\
          osd.label.text.color: #e8e4f8\n",
    )
    .unwrap();
}

/// labwc main config: theme, fonts, four workspaces and enough keybinds to
/// drive the desktop from the keyboard (terminal, menu, tiling, workspaces).
fn write_labwc_rc(rootfs: &Path) {
    let cfg = rootfs.join("root/.config/labwc");
    let _ = fs::create_dir_all(&cfg);
    fs::write(
        cfg.join("rc.xml"),
        br#"<?xml version="1.0"?>
<labwc_config>
  <core><gap>0</gap></core>
  <theme>
    <name>Eclipse-Dark</name>
    <cornerRadius>8</cornerRadius>
    <font place="ActiveWindow"><name>DejaVu Sans</name><size>10</size><weight>bold</weight></font>
    <font place="InactiveWindow"><name>DejaVu Sans</name><size>10</size></font>
    <font place="MenuItem"><name>DejaVu Sans</name><size>10</size></font>
    <font place="OnScreenDisplay"><name>DejaVu Sans</name><size>11</size></font>
  </theme>
  <desktops number="4"/>
  <keyboard>
    <!-- Terminals -->
    <keybind key="W-Return"><action name="Execute"><command>/usr/local/bin/eclipse-terminal</command></action></keybind>
    <keybind key="A-Return"><action name="Execute"><command>/usr/local/bin/eclipse-terminal</command></action></keybind>
    <!-- Desktop menu also on a key, in case the mouse is missing -->
    <keybind key="W-space"><action name="ShowMenu"><menu>root-menu</menu></action></keybind>
    <!-- Window management -->
    <keybind key="A-F4"><action name="Close"/></keybind>
    <keybind key="A-Tab"><action name="NextWindow"/></keybind>
    <keybind key="W-Up"><action name="ToggleMaximize"/></keybind>
    <keybind key="W-Left"><action name="SnapToEdge"><direction>left</direction></action></keybind>
    <keybind key="W-Right"><action name="SnapToEdge"><direction>right</direction></action></keybind>
    <!-- Workspaces -->
    <keybind key="W-1"><action name="GoToDesktop"><to>1</to></action></keybind>
    <keybind key="W-2"><action name="GoToDesktop"><to>2</to></action></keybind>
    <keybind key="W-3"><action name="GoToDesktop"><to>3</to></action></keybind>
    <keybind key="W-4"><action name="GoToDesktop"><to>4</to></action></keybind>
    <keybind key="W-S-1"><action name="SendToDesktop"><to>1</to></action></keybind>
    <keybind key="W-S-2"><action name="SendToDesktop"><to>2</to></action></keybind>
    <keybind key="W-S-3"><action name="SendToDesktop"><to>3</to></action></keybind>
    <keybind key="W-S-4"><action name="SendToDesktop"><to>4</to></action></keybind>
  </keyboard>
  <mouse>
    <context name="Root">
      <mousebind button="Right" action="Press"><action name="ShowMenu"><menu>root-menu</menu></action></mousebind>
    </context>
    <context name="TitleBar">
      <mousebind button="Left" action="DoubleClick"><action name="ToggleMaximize"/></mousebind>
    </context>
  </mouse>
</labwc_config>
"#,
    )
    .unwrap();
}

/// Right-click desktop menu (openbox menu format). Entries whose binary is
/// not installed simply do nothing, so no guards are needed here.
fn write_labwc_menu(rootfs: &Path) {
    let cfg = rootfs.join("root/.config/labwc");
    let _ = fs::create_dir_all(&cfg);
    fs::write(
        cfg.join("menu.xml"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<openbox_menu>
  <menu id="root-menu" label="Eclipse OS">
    <item label="Terminal"><action name="Execute"><command>/usr/local/bin/eclipse-terminal</command></action></item>
    <item label="Editor (nano)"><action name="Execute"><command>/usr/local/bin/eclipse-terminal nano</command></action></item>
    <item label="Monitor (top)"><action name="Execute"><command>/usr/local/bin/eclipse-terminal top</command></action></item>
    <separator/>
    <item label="Recargar labwc"><action name="Reconfigure"/></item>
    <item label="Salir de la sesion"><action name="Exit"/></item>
  </menu>
</openbox_menu>
"#,
    )
    .unwrap();
}

/// labwc sources `~/.config/labwc/environment` itself, so these survive even
/// when the compositor is launched from a shell that skipped /etc/profile.
fn write_labwc_environment(rootfs: &Path) {
    let cfg = rootfs.join("root/.config/labwc");
    let _ = fs::create_dir_all(&cfg);
    fs::write(
        cfg.join("environment"),
        b"# Eclipse OS - env for the labwc session (sourced by labwc itself).\n\
          # PATH first: the per-VT console shells do NOT source /etc/profile,\n\
          # so a labwc launched from one inherits a PATH without\n\
          # /usr/local/bin - bypassing the labwc wrapper and making keybinds\n\
          # and autostart unable to find eclipse-terminal (seen as the\n\
          # terminal retry loop failing with rc=127 while foot was installed).\n\
          PATH=/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin\n\
          XCURSOR_THEME=Adwaita\n\
          XCURSOR_SIZE=24\n\
          GTK_THEME=Adwaita:dark\n\
          # UTF-8 locale: foot refuses box-drawing/unicode and prints\n\
          # \"error: 'C' is not a UTF-8 locale\" without it. musl accepts any\n\
          # UTF-8 locale name; glibc userspaces ship C.UTF-8 as a builtin.\n\
          LANG=C.UTF-8\n\
          # Private gdk-pixbuf loader registry, regenerated by autostart on\n\
          # every session start (the apk trigger that creates the system one\n\
          # may never have run). Harmless if the file does not exist yet.\n\
          GDK_PIXBUF_MODULE_FILE=/root/.cache/pixbuf-loaders.cache\n\
          # No D-Bus session bus on Eclipse OS: keep GTK's settings out of\n\
          # dconf so apps never try to autolaunch one.\n\
          GSETTINGS_BACKEND=memory\n",
    )
    .unwrap();
}

/// Session autostart: wallpaper, first terminal, then the panel LAST.
/// Everything is logged so a black desktop is diagnosable WITHOUT a reboot
/// (`cat ~/.config/labwc/autostart.log`), and every launch is guarded by
/// `command -v` so a missing client is skipped, never fatal.
///
/// waybar gets two extra layers of protection because it is GTK and this
/// box's GL/GBM path can hang the whole OS (see the labwc wrapper note):
/// - `GDK_GL=disable` keeps GTK off EGL/GBM entirely — waybar renders via
///   cairo/shm just like swaybg and foot, which are known-good here;
/// - a crash-once lock file: the lock is taken before launching waybar and
///   cleared only after it survives 15 s. If the session dies with the lock
///   held (hang, crash, power cycle), the NEXT session skips waybar and
///   says so in the log, so one bad panel can never hang the OS on every
///   boot. `rm ~/.config/labwc/panel.lock` re-arms it.
fn write_labwc_autostart(rootfs: &Path) {
    let cfg = rootfs.join("root/.config/labwc");
    let _ = fs::create_dir_all(&cfg);
    fs::write(
        cfg.join("autostart"),
        b"# Eclipse OS - labwc autostart.\n\
          # Wayland clients used (all optional): apk add foot swaybg waybar\n\
          LOG=\"$HOME/.config/labwc/autostart.log\"\n\
          exec >\"$LOG\" 2>&1\n\
          # Defensive: the VT shells skip /etc/profile, so PATH may lack\n\
          # /usr/local/bin (eclipse-terminal, wrappers) when labwc was\n\
          # launched straight from a console.\n\
          export PATH=/usr/local/bin:/bin:/sbin:/usr/bin:/usr/sbin\n\
          echo \"[autostart] $(date 2>/dev/null || echo boot) begin\"\n\
          echo \"[autostart] XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR WAYLAND_DISPLAY=$WAYLAND_DISPLAY\"\n\
          # gdk-pixbuf loader cache: apk installs it via a trigger that may\n\
          # never have run under Eclipse OS. Without loaders.cache gdk-pixbuf\n\
          # recognises NO image format at all, so swaybg logs \"Failed to\n\
          # load image\" for a perfectly good PNG (and GTK apps lose their\n\
          # icons). Generate it once if absent.\n\
          if command -v gdk-pixbuf-query-loaders >/dev/null 2>&1; then\n\
          \x20 mkdir -p \"$HOME/.cache\"\n\
          \x20 gdk-pixbuf-query-loaders >\"$HOME/.cache/pixbuf-loaders.cache\" 2>&1\n\
          \x20 export GDK_PIXBUF_MODULE_FILE=\"$HOME/.cache/pixbuf-loaders.cache\"\n\
          \x20 echo \"[autostart] pixbuf cache: $(grep -c '^\\\"' \"$GDK_PIXBUF_MODULE_FILE\" 2>/dev/null || echo 0) loaders registered\"\n\
          else\n\
          \x20 echo '[autostart] gdk-pixbuf-query-loaders NOT installed -> swaybg cannot decode images (apk add gdk-pixbuf)'\n\
          fi\n\
          echo \"[autostart] pixbuf modules: $(ls /usr/lib/gdk-pixbuf-2.0/*/loaders/ 2>/dev/null | tr '\\n' ' ')\"\n\
          # Background: lunarbg, Eclipse's own wallpaper client. It renders\n\
          # the night scene procedurally at the output's native resolution --\n\
          # no image files, no gdk-pixbuf. swaybg (image, then solid colour)\n\
          # remains as a fallback chain for systems without it.\n\
          start_swaybg() {\n\
          \x20 WALL=/usr/share/backgrounds/eclipse/eclipse-night.png\n\
          \x20 echo \"[autostart] wallpaper: $(ls -l \"$WALL\" 2>&1)\"\n\
          \x20 if ! command -v swaybg >/dev/null 2>&1; then\n\
          \x20   echo '[autostart] MISSING swaybg -> desktop stays black (apk add swaybg)'\n\
          \x20   return\n\
          \x20 fi\n\
          \x20 if [ -r \"$WALL\" ]; then\n\
          \x20   echo '[autostart] launching swaybg (eclipse-night)'\n\
          \x20   swaybg -i \"$WALL\" -m fill &\n\
          \x20   WALLPID=$!\n\
          \x20   # Watchdog: if swaybg cannot decode the image it exits right\n\
          \x20   # away -- fall back to the solid colour so the desktop never\n\
          \x20   # ends up with no background at all.\n\
          \x20   ( sleep 2\n\
          \x20     if ! kill -0 \"$WALLPID\" 2>/dev/null; then\n\
          \x20       echo '[autostart] swaybg died loading the image -> solid colour fallback'\n\
          \x20       swaybg -c '#1a1440' &\n\
          \x20     fi ) &\n\
          \x20 else\n\
          \x20   echo '[autostart] wallpaper missing, solid colour'; swaybg -c '#1a1440' &\n\
          \x20 fi\n\
          }\n\
          if command -v lunarbg >/dev/null 2>&1; then\n\
          \x20 echo '[autostart] launching lunarbg (native wallpaper)'\n\
          \x20 ( lunarbg; echo \"[autostart] lunarbg exited rc=$?\" ) &\n\
          \x20 # Watchdog by PROCESS NAME: kill -0 on the wrapper-subshell pid\n\
          \x20 # produced a false \"died\" on this kernel, which stacked the\n\
          \x20 # solid-colour swaybg ON TOP of a perfectly alive lunarbg.\n\
          \x20 ( sleep 3\n\
          \x20   if ! pidof lunarbg >/dev/null 2>&1; then\n\
          \x20     echo '[autostart] lunarbg not running after 3s -> swaybg fallback'\n\
          \x20     start_swaybg\n\
          \x20   fi ) &\n\
          else\n\
          \x20 start_swaybg\n\
          fi\n\
          # A terminal FIRST so the desktop is usable even if the panel\n\
          # below takes the session down.\n\
          # THE TERMINAL IS THE PRIORITY: without one there is no way to type\n\
          # commands and watch the desktop come up. Retry loop keyed on pidof\n\
          # (never on wait()/exit codes, see the waybar note below): up to 5\n\
          # attempts through eclipse-terminal (foot, else alacritty on\n\
          # software GL). Attempt 2 runs foot verbosely so a flaky-start foot\n\
          # documents its own failure (seen: silent death, rc=230).\n\
          if command -v foot >/dev/null 2>&1 || command -v alacritty >/dev/null 2>&1; then\n\
          \x20 ( sleep 1\n\
          \x20   n=1\n\
          \x20   while [ \"$n\" -le 5 ]; do\n\
          \x20     if pidof foot alacritty >/dev/null 2>&1; then\n\
          \x20       echo \"[autostart] terminal up (attempt $n)\"\n\
          \x20       exit 0\n\
          \x20     fi\n\
          \x20     echo \"[autostart] terminal attempt $n\"\n\
          \x20     if [ \"$n\" -eq 2 ] && command -v foot >/dev/null 2>&1; then\n\
          \x20       ( foot -d info; echo \"[autostart] foot -d info exited rc=$?\" ) &\n\
          \x20     else\n\
          \x20       ( eclipse-terminal; echo \"[autostart] terminal exited rc=$?\" ) &\n\
          \x20     fi\n\
          \x20     sleep 2\n\
          \x20     n=$((n+1))\n\
          \x20   done\n\
          \x20   if pidof foot alacritty >/dev/null 2>&1; then\n\
          \x20     echo '[autostart] terminal up (last attempt)'\n\
          \x20   else\n\
          \x20     echo '[autostart] terminal FAILED after 5 attempts (apk add foot)'\n\
          \x20   fi ) &\n\
          else echo '[autostart] MISSING foot/alacritty -> no terminal (apk add foot)'; fi\n\
          # Bottom panel: taskbar, clock, sysinfo. GTK app -> keep it off the\n\
          # EGL/GBM path (hangs this box, see /usr/local/bin/labwc) and guard\n\
          # with a crash-once lock so a hang cannot recur on every boot.\n\
          PANEL_LOCK=\"$HOME/.config/labwc/panel.lock\"\n\
          if ! command -v waybar >/dev/null 2>&1; then\n\
          \x20 echo '[autostart] MISSING waybar -> no panel (apk add waybar)'\n\
          elif [ -e \"$PANEL_LOCK\" ]; then\n\
          \x20 echo \"[autostart] panel.lock present: last session died while waybar ran.\"\n\
          \x20 echo \"[autostart] SKIPPING waybar. To retry: rm $PANEL_LOCK\"\n\
          else\n\
          \x20 echo '[autostart] launching waybar (GDK_GL=disable, crash-once lock armed)'\n\
          \x20 touch \"$PANEL_LOCK\"\n\
          \x20 # GSETTINGS_BACKEND=memory: GTK otherwise reads settings through\n\
          \x20 # dconf, whose D-Bus autolaunch is another failure point on a\n\
          \x20 # system with no session bus.\n\
          \x20 # Retry LOOP keyed on `pidof waybar`, with each attempt launched\n\
          \x20 # in the background. Two reasons: waybar's compositor connect\n\
          \x20 # fails transiently during session bring-up (clients started a\n\
          \x20 # moment earlier connect fine), and a wrapper that waits on the\n\
          \x20 # child directly has been seen wedged in wait() on this kernel\n\
          \x20 # after the child died (its exit-rc line never appeared), which\n\
          \x20 # swallowed the retry. pidof needs neither wait() nor exit\n\
          \x20 # codes: it observes the fact we care about - a living panel.\n\
          \x20 ( n=1\n\
          \x20   while [ \"$n\" -le 5 ]; do\n\
          \x20     sleep 2\n\
          \x20     if pidof waybar >/dev/null 2>&1; then\n\
          \x20       echo \"[autostart] waybar up (attempt $n)\"\n\
          \x20       exit 0\n\
          \x20     fi\n\
          \x20     echo \"[autostart] waybar attempt $n\"\n\
          \x20     GDK_GL=disable GDK_BACKEND=wayland GSETTINGS_BACKEND=memory waybar &\n\
          \x20     n=$((n+1))\n\
          \x20   done\n\
          \x20   sleep 2\n\
          \x20   if pidof waybar >/dev/null 2>&1; then\n\
          \x20     echo '[autostart] waybar up (last attempt)'\n\
          \x20   else\n\
          \x20     echo '[autostart] waybar FAILED after 5 attempts'\n\
          \x20   fi ) &\n\
          \x20 ( sleep 15 && rm -f \"$PANEL_LOCK\" && echo '[autostart] waybar survived 15s, lock cleared' >>\"$LOG\" ) &\n\
          fi\n\
          echo \"[autostart] cursor theme dir: $(ls -d /usr/share/icons/*/cursors 2>/dev/null || echo NONE)\"\n\
          # Post-launch health check: which clients are still alive shortly\n\
          # after start. A client that launched but died (bad config, missing\n\
          # lib) shows as DEAD here, with its stderr earlier in this log.\n\
          ( sleep 5\n\
          \x20 echo \"[autostart] after 5s: lunarbg=$(pidof lunarbg >/dev/null 2>&1 && echo ok || echo DEAD) swaybg=$(pidof swaybg >/dev/null 2>&1 && echo ok || echo n/a) waybar=$(pidof waybar >/dev/null 2>&1 && echo ok || echo DEAD) terminal=$(pidof foot alacritty >/dev/null 2>&1 && echo ok || echo DEAD)\" ) &\n\
          echo '[autostart] done'\n",
    )
    .unwrap();
}

/// Waybar bottom panel: launcher + taskbar on the left, cpu/mem/clock on the
/// right. Text-only module formats so no icon font is required beyond DejaVu.
///
/// Deliberately restricted to modules that only need the Wayland socket and
/// /proc: `tray` (dbus), `network` (rtnetlink dumps) and `pulseaudio`
/// (connection retry loop) exercise kernel paths that are partial on
/// Eclipse OS and are the prime suspects for a whole-OS hang the first time
/// the panel ever ran. Re-add them one at a time only after they are proven
/// against this kernel.
fn write_waybar(rootfs: &Path) {
    let cfg = rootfs.join("root/.config/waybar");
    let _ = fs::create_dir_all(&cfg);
    fs::write(
        cfg.join("config"),
        r#"{
    "layer": "top",
    "position": "bottom",
    "height": 34,
    "spacing": 2,
    "modules-left": ["custom/launcher", "wlr/taskbar"],
    "modules-right": ["cpu", "memory", "clock"],

    "custom/launcher": {
        "format": " ◑ ",
        "tooltip-format": "Terminal (Super+Enter)",
        "on-click": "/usr/local/bin/eclipse-terminal"
    },
    "wlr/taskbar": {
        "format": "{icon} {title:.18}",
        "icon-size": 16,
        "tooltip-format": "{title}",
        "on-click": "activate",
        "on-click-right": "close"
    },
    "cpu": { "format": "cpu {usage}%", "interval": 3 },
    "memory": { "format": "mem {percentage}%", "interval": 5 },
    "clock": {
        "format": "{:%H:%M}",
        "format-alt": "{:%a %d %b  %H:%M}",
        "tooltip-format": "{:%A %d %B %Y}"
    }
}
"#,
    )
    .unwrap();
    fs::write(
        cfg.join("style.css"),
        br#"/* Eclipse OS - waybar (dark, purple accent, matches Eclipse-Dark). */
* {
    font-family: "DejaVu Sans", sans-serif;
    font-size: 13px;
    min-height: 0;
}
window#waybar {
    background: rgba(15, 12, 26, 0.92);
    border-top: 2px solid #6b5aa8;
    color: #e8e4f8;
}
#custom-launcher {
    font-size: 18px;
    color: #b9a8ff;
    padding: 0 10px;
}
#custom-launcher:hover { background: #29233f; }
#taskbar button {
    padding: 0 8px;
    margin: 3px 2px;
    border-radius: 6px;
    color: #c9c4e4;
    background: transparent;
}
#taskbar button:hover { background: #29233f; }
#taskbar button.active {
    background: #3a3357;
    color: #ffffff;
}
#cpu, #memory, #clock {
    padding: 0 10px;
    margin: 3px 1px;
    border-radius: 6px;
    color: #c9c4e4;
}
#clock {
    background: #29233f;
    color: #e8e4f8;
    font-weight: bold;
}
"#,
    )
    .unwrap();
}

/// Prefer dark GTK everywhere (file managers, editors, dialogs).
fn write_gtk_settings(rootfs: &Path) {
    for ver in ["gtk-3.0", "gtk-4.0"] {
        let dir = rootfs.join("root/.config").join(ver);
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("settings.ini"),
            b"[Settings]\n\
              gtk-application-prefer-dark-theme=1\n\
              gtk-theme-name=Adwaita-dark\n\
              gtk-icon-theme-name=Adwaita\n\
              gtk-cursor-theme-name=Adwaita\n\
              gtk-font-name=DejaVu Sans 10\n",
        )
        .unwrap();
    }
}

/// foot terminal palette matching the desktop (deep violet background,
/// lavender foreground).
fn write_foot_config(rootfs: &Path) {
    let dir = rootfs.join("root/.config/foot");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("foot.ini"),
        b"# Eclipse OS - foot terminal theme.\n\
          # Name a real monospace family first: the bare 'monospace' alias\n\
          # can resolve to a non-mono font (DejaVuMathTeXGyre) on a minimal\n\
          # fontconfig, which foot warns about on every start.\n\
          font=DejaVu Sans Mono:size=10,monospace:size=10\n\
          pad=6x6\n\
          \n\
          [colors]\n\
          background=120f1c\n\
          foreground=e0dcf4\n\
          regular0=1d1930\n\
          regular1=e07a7a\n\
          regular2=8fd18a\n\
          regular3=e0c07a\n\
          regular4=8a9fe0\n\
          regular5=b98ae0\n\
          regular6=7ac9d1\n\
          regular7=c9c4e4\n\
          bright0=3a3357\n\
          bright1=f09a9a\n\
          bright2=aef0a8\n\
          bright3=f0d89a\n\
          bright4=a8bef0\n\
          bright5=d1a8f0\n\
          bright6=9ae0e8\n\
          bright7=f0eefc\n",
    )
    .unwrap();
}

/// Bulletproof `labwc` launcher. wlroots picks its renderer from
/// WLR_RENDERER *at exec time*, and it does NOT auto-fall-back from gles2 to
/// pixman. On this box the nvidia DRM node is a stub with no usable
/// GLES2/GBM: the gles2/GBM path hangs the whole OS at GL FBO creation.
///
/// We set the vars in the kernel init env and /etc/profile, but login(1)
/// rebuilds the environment and strips arbitrary vars, so a compositor
/// started from a post-login shell can lose them. A wrapper is the only
/// delivery that cannot be stripped: it re-exports what is missing in
/// labwc's own process and execs the real binary. Placed in /usr/local/bin,
/// which is prepended to PATH (kernel init env + /etc/profile) so it wins.
fn write_labwc_wrapper(rootfs: &Path) {
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("labwc");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: labwc launcher. See xtask/src/linux/desktop.rs.\n\
          # A Wayland compositor needs XDG_RUNTIME_DIR for its socket; set it\n\
          # here too in case labwc was started from a non-login shell that\n\
          # never sourced /etc/profile (otherwise clients can't connect and\n\
          # the desktop stays black with no autostart).\n\
          : \"${XDG_RUNTIME_DIR:=/run/user/0}\"\n\
          export XDG_RUNTIME_DIR\n\
          [ -d \"$XDG_RUNTIME_DIR\" ] || { mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; }\n\
          # Software cursor needs an XCURSOR theme on disk; wlroots draws NO\n\
          # pointer without one (apk add adwaita-icon-theme).\n\
          : \"${XCURSOR_THEME:=Adwaita}\"; export XCURSOR_THEME\n\
          : \"${XCURSOR_SIZE:=24}\"; export XCURSOR_SIZE\n\
          for d in /usr/bin /bin /usr/sbin /sbin; do\n\
          \x20 if [ -x \"$d/labwc\" ]; then exec \"$d/labwc\" \"$@\"; fi\n\
          done\n\
          echo 'labwc: real binary not found (apk add labwc)' >&2\n\
          exit 127\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The autostart carries real shell logic (crash-once lock) — make sure
    /// what we generate actually parses as POSIX sh.
    #[test]
    fn autostart_is_valid_sh() {
        let dir = std::env::temp_dir().join(format!("eclipse-desktop-test-{}", std::process::id()));
        write_labwc_autostart(&dir);
        let script = dir.join("root/.config/labwc/autostart");
        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg(&script)
            .status()
            .expect("run sh -n");
        let _ = fs::remove_dir_all(&dir);
        assert!(status.success(), "generated autostart is not valid sh");
    }
}

/// Build-time wallpaper renderer. Draws the Eclipse OS night scene (gradient
/// sky, stars, crescent moon, mountain silhouettes, the Eclipse disc with
/// its three white stripes and the "Eclipse OS" wordmark) into an RGB
/// buffer, then encodes a valid PNG with no external crates: stored-deflate
/// zlib blocks + hand-rolled CRC32/Adler32. Fully deterministic.
mod wallpaper {
    /// Render the scene and return the encoded PNG bytes.
    pub fn render_png(w: usize, h: usize) -> Vec<u8> {
        let img = render(w, h);
        encode_png(w as u32, h as u32, &img)
    }

    // ---------------------------------------------------------------- scene

    fn render(w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![0f32; w * h * 3]; // linear-ish RGB accumulation

        let fw = w as f32;
        let fh = h as f32;
        // Logo geometry, shared by several passes (stars avoid it).
        let cx = fw * 0.5;
        let cy = fh * 0.43;
        let radius = fh * 0.19;

        // Sky: vertical gradient, deep navy to violet with a warm glow band
        // near the horizon (behind the mountains).
        for y in 0..h {
            let t = y as f32 / fh;
            let (r, g, b) = sky_color(t);
            // Horizon glow: gaussian band centred just above the ridge line.
            let glow = (-((y as f32 - fh * 0.78) / (fh * 0.10)).powi(2)).exp() * 0.30;
            for x in 0..w {
                let i = (y * w + x) * 3;
                buf[i] = r + glow * 0.75;
                buf[i + 1] = g + glow * 0.35;
                buf[i + 2] = b + glow * 0.60;
            }
        }

        draw_stars(&mut buf, w, h, cx, cy, radius);
        draw_crescent_moon(&mut buf, w, h, fw * 0.83, fh * 0.16, fh * 0.05);
        draw_mountains(&mut buf, w, h);
        draw_logo(&mut buf, w, h, cx, cy, radius);
        draw_wordmark(&mut buf, w, h, cx, cy + radius + fh * 0.075);

        // Quantise with a hair of deterministic noise so the smooth gradients
        // do not band at 8 bits.
        let mut out = vec![0u8; w * h * 3];
        for (i, v) in buf.iter().enumerate() {
            let n = (hash2(i as u32, 0x9e3779b9) as f32 / u32::MAX as f32 - 0.5) * 1.5;
            out[i] = (v.clamp(0.0, 1.0) * 255.0 + n).round().clamp(0.0, 255.0) as u8;
        }
        out
    }

    fn sky_color(t: f32) -> (f32, f32, f32) {
        // Stops: deep navy -> indigo -> violet.
        let a = (0.051, 0.043, 0.118); // #0d0b1e
        let b = (0.102, 0.078, 0.251); // #1a1440
        let c = (0.239, 0.165, 0.388); // #3d2a63
        if t < 0.55 {
            lerp3(a, b, t / 0.55)
        } else {
            lerp3(b, c, (t - 0.55) / 0.45)
        }
    }

    fn lerp3(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
        let t = t.clamp(0.0, 1.0);
        (
            a.0 + (b.0 - a.0) * t,
            a.1 + (b.1 - a.1) * t,
            a.2 + (b.2 - a.2) * t,
        )
    }

    fn draw_stars(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32, radius: f32) {
        for i in 0..240u32 {
            let x = (hash2(i, 1) % w as u32) as f32;
            let y = (hash2(i, 2) % (h as u32 * 6 / 10)) as f32;
            // Keep the area around the logo clean.
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            if d < radius + 70.0 {
                continue;
            }
            let bright = 0.25 + (hash2(i, 3) % 1000) as f32 / 1000.0 * 0.75;
            add_px(buf, w, h, x as i32, y as i32, (bright, bright, bright * 0.95));
            // A cross twinkle on the brightest few.
            if bright > 0.85 {
                let half = bright * 0.35;
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    add_px(buf, w, h, x as i32 + dx, y as i32 + dy, (half, half, half));
                }
            }
        }
    }

    fn draw_crescent_moon(buf: &mut [f32], w: usize, h: usize, mx: f32, my: f32, r: f32) {
        // Crescent = disc minus an offset disc; soft-edged via coverage.
        let bite_x = mx + r * 0.45;
        let bite_y = my - r * 0.18;
        let (x0, x1) = span(mx, r + 2.0, w);
        let (y0, y1) = span(my, r + 2.0, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = dist(x as f32, y as f32, mx, my);
                let db = dist(x as f32, y as f32, bite_x, bite_y);
                let cover = coverage(r, d) * (1.0 - coverage(r * 0.92, db));
                if cover > 0.0 {
                    blend_px(buf, w, x, y, (0.85, 0.83, 0.94), cover * 0.9);
                }
            }
        }
    }

    fn draw_mountains(buf: &mut [f32], w: usize, h: usize) {
        let fh = h as f32;
        // Two silhouette layers; the front one is darker.
        let layers: [(f32, [f32; 6], (f32, f32, f32)); 2] = [
            (
                fh * 0.780,
                [0.0040, 1.7, 0.011, 0.4, 0.027, 2.2],
                (0.133, 0.102, 0.220), // #221a38
            ),
            (
                fh * 0.845,
                [0.0060, 4.1, 0.015, 1.1, 0.033, 0.0],
                (0.090, 0.067, 0.161), // #171129
            ),
        ];
        for (base, p, color) in layers {
            for x in 0..w {
                let fx = x as f32;
                let ridge = base
                    + (fx * p[0] + p[1]).sin() * fh * 0.055
                    + (fx * p[2] + p[3]).sin() * fh * 0.028
                    + (fx * p[4] + p[5]).sin() * fh * 0.012;
                let start = ridge.max(0.0) as usize;
                for y in start..h {
                    // Soft top edge on the first row.
                    let cover = if y == start {
                        1.0 - (ridge - ridge.floor())
                    } else {
                        1.0
                    };
                    blend_px(buf, w, x, y, color, cover);
                }
            }
        }
    }

    fn draw_logo(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32, r: f32) {
        // Outer glow halo.
        let halo = r * 0.55;
        let (x0, x1) = span(cx, r + halo, w);
        let (y0, y1) = span(cy, r + halo, h);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = dist(x as f32, y as f32, cx, cy);
                if d > r && d < r + halo {
                    let t = 1.0 - (d - r) / halo;
                    let a = t * t * 0.45;
                    add_px(
                        buf,
                        w,
                        h,
                        x as i32,
                        y as i32,
                        (0.55 * a, 0.47 * a, 0.86 * a),
                    );
                }
            }
        }
        // Dark disc with a subtle vertical shade, then a lavender rim.
        for y in y0..y1 {
            for x in x0..x1 {
                let d = dist(x as f32, y as f32, cx, cy);
                let cover = coverage(r, d);
                if cover > 0.0 {
                    let shade = 0.5 + (y as f32 - cy) / (2.0 * r); // 0 top .. 1 bottom
                    let base = lerp3((0.110, 0.086, 0.208), (0.055, 0.043, 0.118), shade);
                    blend_px(buf, w, x, y, base, cover);
                }
                let rim = (1.0 - ((d - r).abs() / 3.0)).clamp(0.0, 1.0);
                if rim > 0.0 {
                    blend_px(buf, w, x, y, (0.66, 0.60, 0.91), rim * 0.9);
                }
            }
        }
        // The three white stripes (rounded bars across the disc).
        for (off_frac, bar_frac) in [(-0.36f32, 0.085f32), (-0.02, 0.085), (0.32, 0.085)] {
            let yb = cy + r * off_frac;
            let bar_r = r * bar_frac;
            let dy = yb - cy;
            let chord = (r * r - dy * dy).max(0.0).sqrt();
            let half = chord - r * 0.16;
            if half <= 0.0 {
                continue;
            }
            let (bx0, bx1) = span(cx, half + bar_r + 2.0, w);
            let (by0, by1) = span(yb, bar_r + 2.0, h);
            for y in by0..by1 {
                for x in bx0..bx1 {
                    let d = capsule_dist(x as f32, y as f32, cx - half, yb, cx + half, yb);
                    let cover = coverage(bar_r, d);
                    if cover > 0.0 {
                        blend_px(buf, w, x, y, (0.95, 0.94, 0.98), cover);
                    }
                }
            }
        }
    }

    /// "Eclipse OS" in a 5x7 bitmap font, scaled up with soft edges.
    fn draw_wordmark(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32) {
        let text = "Eclipse OS";
        let scale = (h as f32 * 0.008).round().max(3.0) as usize; // ~7px at 900
        let advance = (6 * scale) as i32;
        let total = text.len() as i32 * advance - scale as i32;
        // Signed coords: on small renders the text may start off-canvas.
        let left = cx as i32 - total / 2;
        let top = cy as i32;
        for (ci, ch) in text.chars().enumerate() {
            let glyph = glyph5x7(ch);
            let gx = left + ci as i32 * advance;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0b10000 >> col) == 0 {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let x = gx + (col * scale + sx) as i32;
                            let y = top + (row * scale + sy) as i32;
                            if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                                blend_px(buf, w, x as usize, y as usize, (0.85, 0.82, 0.94), 0.95);
                            }
                        }
                    }
                }
            }
        }
    }

    fn glyph5x7(c: char) -> [u8; 7] {
        match c {
            'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
            'c' => [0b00000, 0b00000, 0b01111, 0b10000, 0b10000, 0b10000, 0b01111],
            'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
            'i' => [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
            'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b11110, 0b10000],
            's' => [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
            'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01111],
            'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
            'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
            _ => [0; 7],
        }
    }

    // ------------------------------------------------------- draw utilities

    fn dist(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
        ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
    }

    /// Anti-aliased coverage of a disc of radius `r` at distance `d`.
    fn coverage(r: f32, d: f32) -> f32 {
        (r - d + 0.5).clamp(0.0, 1.0)
    }

    /// Distance from a point to the segment (ax,ay)-(bx,by); a capsule is
    /// all points within `bar_r` of the segment.
    fn capsule_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        let t = if len2 > 0.0 {
            (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        dist(px, py, ax + dx * t, ay + dy * t)
    }

    /// Pixel range [c-r, c+r] clamped to [0, limit).
    fn span(c: f32, r: f32, limit: usize) -> (usize, usize) {
        let lo = (c - r).floor().max(0.0) as usize;
        let hi = ((c + r).ceil() as usize + 1).min(limit);
        (lo, hi)
    }

    fn add_px(buf: &mut [f32], w: usize, h: usize, x: i32, y: i32, c: (f32, f32, f32)) {
        if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
            return;
        }
        let i = (y as usize * w + x as usize) * 3;
        buf[i] += c.0;
        buf[i + 1] += c.1;
        buf[i + 2] += c.2;
    }

    fn blend_px(buf: &mut [f32], w: usize, x: usize, y: usize, c: (f32, f32, f32), a: f32) {
        let i = (y * w + x) * 3;
        buf[i] = buf[i] * (1.0 - a) + c.0 * a;
        buf[i + 1] = buf[i + 1] * (1.0 - a) + c.1 * a;
        buf[i + 2] = buf[i + 2] * (1.0 - a) + c.2 * a;
    }

    /// Deterministic 2-input integer hash (xorshift-style avalanche).
    fn hash2(a: u32, b: u32) -> u32 {
        let mut x = a.wrapping_mul(0x85eb_ca6b) ^ b.wrapping_mul(0xc2b2_ae35);
        x ^= x >> 16;
        x = x.wrapping_mul(0x7feb_352d);
        x ^= x >> 15;
        x = x.wrapping_mul(0x846c_a68b);
        x ^ (x >> 16)
    }

    // ---------------------------------------------------------- PNG encoder

    /// Minimal PNG encoder: 8-bit RGB, zlib stream made of *stored* (i.e.
    /// uncompressed) deflate blocks. Larger on disk than real compression
    /// but standards-compliant, dependency-free and trivially correct.
    fn encode_png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
        assert_eq!(rgb.len(), (w * h * 3) as usize);
        let mut png = Vec::with_capacity(rgb.len() + rgb.len() / 32 + 1024);
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, none, no interlace
        write_chunk(&mut png, b"IHDR", &ihdr);

        // Raw scanlines: filter byte 0 (None) + RGB row.
        let stride = (w * 3) as usize;
        let mut raw = Vec::with_capacity((stride + 1) * h as usize);
        for row in rgb.chunks_exact(stride) {
            raw.push(0);
            raw.extend_from_slice(row);
        }

        // zlib wrapper with stored deflate blocks (max 65535 bytes each).
        let mut z = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
        z.extend_from_slice(&[0x78, 0x01]); // CMF/FLG, 32K window, check ok
        let mut chunks = raw.chunks(65535).peekable();
        while let Some(block) = chunks.next() {
            let last = chunks.peek().is_none();
            z.push(last as u8); // BFINAL, BTYPE=00 (stored)
            let len = block.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(block);
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());
        write_chunk(&mut png, b"IDAT", &z);
        write_chunk(&mut png, b"IEND", &[]);
        png
    }

    fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = Crc32::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(&crc.finish().to_be_bytes());
    }

    struct Crc32(u32);

    impl Crc32 {
        fn new() -> Self {
            Crc32(0xffff_ffff)
        }
        fn update(&mut self, data: &[u8]) {
            // Canonical bit-at-a-time reflected CRC32 (poly 0xEDB88320).
            for &b in data {
                self.0 ^= b as u32;
                for _ in 0..8 {
                    self.0 = if self.0 & 1 != 0 {
                        0xedb8_8320 ^ (self.0 >> 1)
                    } else {
                        self.0 >> 1
                    };
                }
            }
        }
        fn finish(self) -> u32 {
            self.0 ^ 0xffff_ffff
        }
    }

    fn adler32(data: &[u8]) -> u32 {
        const MOD: u32 = 65521;
        let (mut a, mut b) = (1u32, 0u32);
        for chunk in data.chunks(5552) {
            for &byte in chunk {
                a += byte as u32;
                b += a;
            }
            a %= MOD;
            b %= MOD;
        }
        (b << 16) | a
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn crc32_matches_reference() {
            // CRC32 of "123456789" is the classic check value 0xCBF43926.
            let mut crc = Crc32::new();
            crc.update(b"123456789");
            assert_eq!(crc.finish(), 0xcbf4_3926);
        }

        #[test]
        fn adler32_matches_reference() {
            // Adler32 of "Wikipedia" per the algorithm's documentation.
            assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        }

        /// Manual aid: render the real wallpaper to $ECLIPSE_WALLPAPER_OUT
        /// (or the temp dir) so it can be eyeballed without a full build.
        /// Run with: `cargo test -p xtask dump_wallpaper -- --ignored`.
        #[test]
        #[ignore]
        fn dump_wallpaper() {
            let out = std::env::var_os("ECLIPSE_WALLPAPER_OUT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("eclipse-night.png"));
            std::fs::write(&out, render_png(1600, 900)).unwrap();
            eprintln!("wallpaper written to {}", out.display());
        }

        #[test]
        fn png_structure_is_valid() {
            let png = render_png(64, 36);
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
            assert_eq!(&png[12..16], b"IHDR");
            assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        }
    }
}
