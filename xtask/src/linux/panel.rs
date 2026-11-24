//! "Panel de control" in the launcher: `eclipse-panel`, its entry, and the
//! claves the panel is built from.
//!
//! The panel (tools/lunarbar/src/bin/lunarpanel.rs) has no settings of its
//! own: it draws whatever blocks it finds in `/usr/share/eclipse/claves.d`
//! (and `/etc/eclipse/claves.d`, for the ones a machine adds). Those blocks
//! live in the tree as `tools/lunarbar/claves.d/*.claves` and are copied here
//! as they are, so a new setting is a new block and nothing else.

use crate::PROJECT_DIR;
use std::fs;
use std::path::Path;

/// Where the panel looks for the shipped blocks (`claves::DIRS[0]`).
const CLAVES_DIR: &str = "usr/share/eclipse/claves.d";

pub(super) fn write_panel(rootfs: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let wrapper = localbin.join("eclipse-panel");
    fs::write(
        &wrapper,
        b"#!/bin/sh\n\
          # Eclipse OS: the control panel, computed from the blocks in\n\
          # /usr/share/eclipse/claves.d. See tools/lunarbar/src/bin/lunarpanel.rs.\n\
          if command -v lunarpanel >/dev/null 2>&1; then\n\
          \x20 exec lunarpanel \"$@\"\n\
          fi\n\
          exec /usr/local/bin/eclipse-terminal sh -c \"ls /etc/eclipse; echo; echo '[enter]'; read _\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let dir = rootfs.join("root/.local/share/applications");
    let _ = fs::create_dir_all(&dir);
    fs::write(
        dir.join("lunarpanel.desktop"),
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name=Control panel\n\
         Name[es]=Panel de control\n\
         GenericName=Settings\n\
         Comment=Look, language, keyboard, time zone, network and system\n\
         Comment[es]=Aspecto, idioma, teclado, zona horaria, red y sistema\n\
         Exec=/usr/local/bin/eclipse-panel\n\
         TryExec=/usr/local/bin/eclipse-panel\n\
         Terminal=false\n\
         Icon=preferences-system\n\
         Categories=System;Settings;\n\
         Keywords=panel;control;ajustes;settings;configuracion;preferencias;aspecto;idioma;teclado;\n\
         StartupNotify=false\n",
    )
    .unwrap();

    install_claves(
        &PROJECT_DIR.join("tools/lunarbar/claves.d"),
        &rootfs.join(CLAVES_DIR),
    );
    write_entorno(&localbin);
}

/// `eclipse-entorno NAME VALUE`: writes one setting into
/// `/etc/eclipse/entorno` (lunartext::entorno reads it) and restarts the
/// client that read it at start. Only the names below are taken, and only
/// a value made of letters, digits and `_.:+@,-` (the panel's own rule):
/// whatever reads the file reads plain words. `auto` removes the line, so
/// the program's own default holds again. `ECLIPSE_ENTORNO`,
/// `ECLIPSE_INIT_CTL` and `ECLIPSE_INIT_PID` are for the tests.
fn write_entorno(localbin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = localbin.join("eclipse-entorno");
    fs::write(
        &script,
        b"#!/bin/sh\n\
          # Eclipse OS: one desktop setting into /etc/eclipse/entorno.\n\
          # Written by xtask/src/linux/panel.rs; read by lunartext::entorno.\n\
          F=\"${ECLIPSE_ENTORNO:-/etc/eclipse/entorno}\"\n\
          CTL=\"${ECLIPSE_INIT_CTL:-/run/eclipse-init/ctl}\"\n\
          name=\"$1\"; value=\"$2\"\n\
          case \"$name\" in\n\
          \x20 LUNARBG_FPS|LUNARBG_STATIC) restart=lunarbg ;;\n\
          \x20 LUNARBAR_HEIGHT) restart=lunarbar ;;\n\
          \x20 LUNAR_FX|LUNAR_TEXT) restart='lunarbar lunarbg' ;;\n\
          \x20 SILVERSURFER_ADBLOCK) restart= ;;\n\
          \x20 *) echo \"eclipse-entorno: $name no es un ajuste del entorno\" >&2; exit 2 ;;\n\
          esac\n\
          case \"$value\" in\n\
          \x20 ''|*[!A-Za-z0-9_.:+@,-]*) echo \"eclipse-entorno: valor no admitido: $value\" >&2; exit 2 ;;\n\
          esac\n\
          [ ${#value} -le 64 ] || { echo 'eclipse-entorno: valor demasiado largo' >&2; exit 2; }\n\
          mkdir -p \"$(dirname \"$F\")\" || exit 1\n\
          tmp=\"$F.$$\"\n\
          { grep -v \"^$name=\" \"$F\" 2>/dev/null || true\n\
          \x20 [ \"$value\" = auto ] || echo \"$name=$value\"\n\
          } > \"$tmp\" && mv \"$tmp\" \"$F\" || { rm -f \"$tmp\"; exit 1; }\n\
          # eclipse-init restarts a respawn service asked for in its ctl\n\
          # directory (tools/eclipse-init/src/control.rs). Without init, or\n\
          # with the service not running, the next start reads the file.\n\
          if [ -n \"$restart\" ] && [ -d \"$CTL\" ]; then\n\
          \x20 for s in $restart; do : > \"$CTL/restart.$s\"; done\n\
          \x20 kill -HUP \"${ECLIPSE_INIT_PID:-1}\" 2>/dev/null || true\n\
          fi\n\
          exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every `*.claves` in `from` into `to`, and nothing else: a block that was
/// removed from the tree must not linger in an image built over an old one.
fn install_claves(from: &Path, to: &Path) {
    let _ = fs::create_dir_all(to);
    if let Ok(old) = fs::read_dir(to) {
        for e in old.flatten() {
            if e.path().extension().is_some_and(|x| x == "claves") {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let Ok(entries) = fs::read_dir(from) else {
        eprintln!(
            "warning: {} missing; the control panel uses its built-in claves",
            from.display()
        );
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "claves") {
            if let Some(name) = p.file_name() {
                fs::copy(&p, to.join(name)).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_claves_are_installed_and_stale_ones_removed() {
        let tmp = std::env::temp_dir().join(format!("xtask-claves-{}", std::process::id()));
        let to = tmp.join("claves.d");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("99-vieja.claves"), "[x]\n").unwrap();
        install_claves(&PROJECT_DIR.join("tools/lunarbar/claves.d"), &to);
        let mut names: Vec<String> = fs::read_dir(&to)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(
            names.iter().any(|n| n == "10-apariencia.claves"),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n == "99-vieja.claves"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn eclipse_entorno_writes_one_line_restarts_its_client_and_refuses_the_rest() {
        let tmp = std::env::temp_dir().join(format!("xtask-entorno-{}", std::process::id()));
        let bin = tmp.join("bin");
        let ctl = tmp.join("ctl");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&ctl).unwrap();
        write_entorno(&bin);
        let file = tmp.join("etc/entorno");
        let run = |name: &str, value: &str| {
            std::process::Command::new("/bin/sh")
                .arg(bin.join("eclipse-entorno"))
                .args([name, value])
                .env("ECLIPSE_ENTORNO", &file)
                .env("ECLIPSE_INIT_CTL", &ctl)
                // No such process: the HUP goes nowhere.
                .env("ECLIPSE_INIT_PID", "2147483646")
                .status()
                .unwrap()
                .success()
        };
        assert!(run("LUNARBAR_HEIGHT", "44"));
        assert!(run("LUNAR_FX", "off"));
        assert!(run("LUNARBAR_HEIGHT", "48"));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "LUNAR_FX=off\nLUNARBAR_HEIGHT=48\n"
        );
        assert!(ctl.join("restart.lunarbar").is_file());
        assert!(ctl.join("restart.lunarbg").is_file());
        // `auto` hands the setting back to the program.
        assert!(run("LUNARBAR_HEIGHT", "auto"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "LUNAR_FX=off\n");
        // Fail-closed: another name, or a value that is not a plain word.
        assert!(!run("LD_PRELOAD", "/tmp/x.so"));
        assert!(!run("LUNAR_FX", "off;reboot"));
        assert!(!run("LUNAR_FX", "$(id)"));
        assert!(!run("LUNAR_FX", ""));
        assert_eq!(fs::read_to_string(&file).unwrap(), "LUNAR_FX=off\n");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn every_command_a_block_runs_is_one_the_image_has() {
        // The blocks name scripts by absolute path; each must be one xtask
        // writes, or the row would fail the day it is used.
        let dir = PROJECT_DIR.join("tools/lunarbar/claves.d");
        let linux = PROJECT_DIR.join("xtask/src/linux");
        let mut sources = String::new();
        for f in [
            "desktop.rs",
            "desktop/look.rs",
            "desktop/locale.rs",
            "network.rs",
            "monitor.rs",
            "panel.rs",
        ] {
            sources += &fs::read_to_string(linux.join(f)).unwrap();
        }
        for e in fs::read_dir(dir).unwrap().flatten() {
            let text = fs::read_to_string(e.path()).unwrap();
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                if !matches!(k.trim(), "aplicar" | "leer" | "orden") {
                    continue;
                }
                let prog = v.split_whitespace().next().unwrap_or("");
                let name = prog.rsplit('/').next().unwrap_or(prog);
                assert!(
                    sources.contains(&format!("localbin.join(\"{name}\")")),
                    "{}: {prog} is not written by xtask",
                    e.path().display()
                );
            }
        }
    }
}
