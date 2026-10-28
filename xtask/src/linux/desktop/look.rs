//! `/usr/local/bin/eclipse-look`: the desktop look, and the one place every
//! look has to be wired up consistently.
//!
//! Lifted out of `desktop.rs` when the two upstream labwc themes arrived
//! (see [`super::upstream`]): the file was 7 lines under the 4000-line
//! ceiling and a look costs 4 lines here, so the script could not grow where
//! it was. Nothing in it changed in the move; the tests next door read the
//! INSTALLED script rather than this source, so they did not care either.

use std::{fs, path::Path};

/// `/usr/local/bin/eclipse-look`: switch the desktop look between `eclipse`
/// (the violet original, what the image ships), `kde` (KDE Breeze Dark),
/// `win11` (Windows 11 dark) and `gnome` (`labAdwaita-dark-xbm`, straight
/// from labwc's own theme repository, see [`super::upstream`]) -- persisted
/// in
/// `/etc/eclipse/look` like `eclipse-kbd`/`eclipse-locale`/`eclipse-tz` do
/// with their own settings. `--boot` is what eclipse-init runs before the
/// compositor starts; `look=` on the kernel cmdline wins over the file.
///
/// What it actually switches, since Plasma itself cannot run here (no session
/// D-Bus, no Qt, no KF6):
///   * the labwc window theme, by rewriting `<name>` in `rc.xml` in place;
///   * the foot palette, by copying `foot.kde.ini` / `foot.eclipse.ini`;
///   * the panel, which reads `/etc/eclipse/look` itself on start -- so the
///     switch restarts lunarbar (eclipse-init respawns it at once).
pub(super) fn write_eclipse_look(rootfs: &Path) {
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let script = localbin.join("eclipse-look");
    fs::write(
        &script,
        b"#!/bin/sh\n\
          # Eclipse OS: desktop look (win11 = Windows 11 dark, kde = KDE\n\
          # Breeze Dark, gnome = labAdwaita-dark-xbm, from labwc's own theme\n\
          # repository, eclipse = the violet original).\n\
          CONF=/etc/eclipse/look\n\
          CFG=\"${HOME:-/root}/.config\"\n\
          RC=\"$CFG/labwc/rc.xml\"\n\
          LOG=\"${HOME:-/root}/.eclipse-look.log\"\n\
          \n\
          look_ok() {\n\
          \x20 case \"$1\" in win11|kde|eclipse|gnome) return 0 ;; *) return 1 ;; esac\n\
          }\n\
          \n\
          theme_for() {\n\
          \x20 case \"$1\" in\n\
          \x20 win11) echo Win11-Dark ;;\n\
          \x20 kde) echo Breeze-Dark ;;\n\
          \x20 gnome) echo labAdwaita-dark-xbm ;;\n\
          \x20 *) echo Eclipse-Dark ;;\n\
          \x20 esac\n\
          }\n\
          \n\
          file_look() {\n\
          \x20 [ -r \"$CONF\" ] || return\n\
          \x20 awk '\n\
          \x20   { sub(/\\r$/, \"\") }\n\
          \x20   /^[[:space:]]*#/ { next }\n\
          \x20   /^[[:space:]]*look[[:space:]]*=/ {\n\
          \x20     sub(/^[^=]*=/, \"\"); gsub(/[[:space:]]/, \"\"); print; exit\n\
          \x20   }\n\
          \x20 ' \"$CONF\"\n\
          }\n\
          \n\
          cmdline_look() {\n\
          \x20 [ -r /proc/cmdline ] || return\n\
          \x20 awk '{\n\
          \x20   n=split($0,a,/[: \\t]/)\n\
          \x20   for(i=1;i<=n;i++) if(a[i] ~ /^look=/){ sub(/^look=/,\"\",a[i]); print a[i]; exit }\n\
          \x20 }' /proc/cmdline\n\
          }\n\
          \n\
          current() {\n\
          \x20 f=$(file_look)\n\
          \x20 if look_ok \"$f\"; then echo \"$f\"; return; fi\n\
          \x20 echo eclipse\n\
          }\n\
          \n\
          resolve_boot() {\n\
          \x20 c=$(cmdline_look)\n\
          \x20 if look_ok \"$c\"; then echo \"$c\"; return; fi\n\
          \x20 current\n\
          }\n\
          \n\
          apply() {\n\
          \x20 look=$1\n\
          \x20 boot=$2\n\
          \x20 theme=$(theme_for \"$look\")\n\
          \x20 mkdir -p /etc/eclipse\n\
          \x20 echo \"look=$look\" > \"$CONF\"\n\
          # labwc theme, rewritten in place. Only the <name> element inside\n\
          # <theme> carries any of these strings, so this cannot hit anything\n\
          # else. One -e per theme the switch might have to come back OUT of:\n\
          # a theme the sed cannot match is a one-way trip into its look.\n\
          \x20 if [ -f \"$RC\" ]; then\n\
          \x20   sed -e \"s|<name>Win11-Dark</name>|<name>$theme</name>|\" \\\n\
          \x20       -e \"s|<name>Breeze-Dark</name>|<name>$theme</name>|\" \\\n\
          \x20       -e \"s|<name>Eclipse-Dark</name>|<name>$theme</name>|\" \\\n\
          \x20       -e \"s|<name>labAdwaita-dark-xbm</name>|<name>$theme</name>|\" \\\n\
          \x20       -e \"s|<name>labAdwaita-dark</name>|<name>$theme</name>|\" \\\n\
          \x20       \"$RC\" > \"$RC.new\" 2>/dev/null && mv \"$RC.new\" \"$RC\"\n\
          \x20 fi\n\
          # foot palette for new terminals (a running foot keeps its colours).\n\
          \x20 src=\"$CFG/foot/foot.$look.ini\"\n\
          \x20 [ -f \"$src\" ] && cp -f \"$src\" \"$CFG/foot/foot.ini\"\n\
          \x20 if [ \"$boot\" != boot ]; then\n\
          \x20   if [ -n \"${LABWC_PID:-}\" ] && kill -0 \"$LABWC_PID\" 2>/dev/null; then\n\
          \x20     kill -HUP \"$LABWC_PID\" 2>>\"$LOG\" || true\n\
          \x20   elif [ -x /usr/bin/labwc ]; then\n\
          \x20     /usr/bin/labwc --reconfigure >>\"$LOG\" 2>&1 || true\n\
          \x20   fi\n\
          # The panel reads /etc/eclipse/look once at start; eclipse-init\n\
          # respawns it with the new look immediately.\n\
          \x20   pkill -x lunarbar 2>/dev/null || true\n\
          \x20 fi\n\
          \x20 echo \"[$(date '+%H:%M:%S')] look=$look theme=$theme boot=$boot\" >>\"$LOG\"\n\
          }\n\
          \n\
          usage() {\n\
          \x20 echo \"usage: eclipse-look [win11|kde|eclipse|gnome|--boot]\" >&2\n\
          \x20 exit 2\n\
          }\n\
          \n\
          case \"${1:-}\" in\n\
          '' ) current; exit 0 ;;\n\
          -h|--help) usage ;;\n\
          --boot)\n\
          \x20 apply \"$(resolve_boot)\" boot\n\
          \x20 current\n\
          \x20 exit 0\n\
          \x20 ;;\n\
          win11|kde|eclipse|gnome)\n\
          \x20 apply \"$1\"\n\
          \x20 echo \"$1\"\n\
          \x20 ;;\n\
          *) usage ;;\n\
          esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let etc = rootfs.join("etc/eclipse");
    let _ = fs::create_dir_all(&etc);
    fs::write(etc.join("look"), b"look=eclipse\n").unwrap();
}
