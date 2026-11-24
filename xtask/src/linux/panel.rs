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
