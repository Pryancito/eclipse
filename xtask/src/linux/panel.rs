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
    write_udriver(&localbin);
}

/// `eclipse-udriver instalar VVVV DDDD FICHERO` and `eclipse-udriver quitar
/// VVVV DDDD`: the panel's "Instalar driver" for the PCI device with that
/// vendor and device. Installing copies the file to
/// `/usr/lib/eclipse/udrivers/udrv-VVVV-DDDD`, writes its eclipse-init
/// service (`udriver = pci-VVVV-DDDD@0`: started once the kernel leaves it
/// that device) and the `[driver:udrv-VVVV-DDDD]` block the devices page
/// reads; removing deletes the three. The panel then adds the device to
/// (or takes it from) `udriver=` in rboot.conf, so it takes effect at the
/// next boot. The ids are four lowercase hex digits each, and nothing else
/// is taken. `ECLIPSE_UDRIVERS`, `ECLIPSE_SERVICES` and `ECLIPSE_CLAVES`
/// are for the tests.
fn write_udriver(localbin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = localbin.join("eclipse-udriver");
    fs::write(
        &script,
        b"#!/bin/sh\n\
          # Eclipse OS: installs or removes a userspace driver for one PCI device.\n\
          # Written by xtask/src/linux/panel.rs; run by lunarpanel.\n\
          LIB=\"${ECLIPSE_UDRIVERS:-/usr/lib/eclipse/udrivers}\"\n\
          SVC=\"${ECLIPSE_SERVICES:-/etc/eclipse/services}\"\n\
          CLV=\"${ECLIPSE_CLAVES:-/etc/eclipse/claves.d}\"\n\
          verb=\"$1\"; v=\"$2\"; d=\"$3\"; file=\"$4\"\n\
          for x in \"$v\" \"$d\"; do\n\
          \x20 case \"$x\" in\n\
          \x20   [0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;\n\
          \x20   *) echo \"eclipse-udriver: $x no es un id de cuatro cifras hex\" >&2; exit 2 ;;\n\
          \x20 esac\n\
          done\n\
          name=\"udrv-$v-$d\"; item=\"pci-$v-$d@0\"\n\
          case \"$verb\" in\n\
          \x20 instalar)\n\
          \x20   [ -f \"$file\" ] || { echo \"eclipse-udriver: $file no es un fichero\" >&2; exit 2; }\n\
          \x20   mkdir -p \"$LIB\" \"$SVC\" \"$CLV\" || exit 1\n\
          \x20   cp \"$file\" \"$LIB/$name.tmp\" && chmod 755 \"$LIB/$name.tmp\" && mv \"$LIB/$name.tmp\" \"$LIB/$name\" \\\n\
          \x20     || { rm -f \"$LIB/$name.tmp\"; echo \"eclipse-udriver: no se pudo copiar $file\" >&2; exit 1; }\n\
          \x20   printf '# Driver del dispositivo PCI %s:%s, instalado desde el panel de control.\\n# Lo quita: eclipse-udriver quitar %s %s\\nexec = %s %s\\ntype = respawn\\nudriver = %s\\n' \\\n\
          \x20     \"$v\" \"$d\" \"$v\" \"$d\" \"$LIB/$name\" \"$item\" \"$item\" > \"$SVC/$name.service\" || exit 1\n\
          \x20   printf '# Instalado por eclipse-udriver.\\n[driver:%s]\\nes = %s\\nids = %s:%s\\nnucleo = no\\nudriver = pci-%s-%s\\ninstalado = si\\n' \\\n\
          \x20     \"$name\" \"$name\" \"$v\" \"$d\" \"$v\" \"$d\" > \"$CLV/90-$name.claves\" || exit 1\n\
          \x20   ;;\n\
          \x20 quitar)\n\
          \x20   rm -f \"$LIB/$name\" \"$SVC/$name.service\" \"$CLV/90-$name.claves\" || exit 1\n\
          \x20   ;;\n\
          \x20 *) echo 'uso: eclipse-udriver instalar VVVV DDDD FICHERO | quitar VVVV DDDD' >&2; exit 2 ;;\n\
          esac\n\
          exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
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
          \x20 LUNARBG_FPS|LUNARBG_STATIC|LUNARBG_RELOJ|LUNARBG_FECHA|LUNARBG_LECTURAS) restart=lunarbg ;;\n\
          \x20 LUNARBG_LECTURA_CARGA|LUNARBG_LECTURA_CPU|LUNARBG_LECTURA_MEMORIA|LUNARBG_LECTURA_RED) restart=lunarbg ;;\n\
          \x20 LUNARBG_LECTURA_DISCO|LUNARBG_LECTURA_TEMPERATURA|LUNARBG_LECTURA_EN_MARCHA|LUNARBG_LECTURA_PROCESOS) restart=lunarbg ;;\n\
          \x20 LUNARBAR_HEIGHT|LUNARBAR_ESCRITORIOS|LUNARBAR_TITULO|LUNARBAR_CPU|LUNARBAR_MEMORIA) restart=lunarbar ;;\n\
          \x20 LUNARBAR_RED|LUNARBAR_TEMPERATURA|LUNARBAR_BATERIA|LUNARBAR_DISCO|LUNARBAR_EN_MARCHA|LUNARBAR_CARGA) restart=lunarbar ;;\n\
          \x20 LUNAR_FX|LUNAR_TEXT) restart='lunarbar lunarbg' ;;\n\
          \x20 LUNAR_ACCENT) restart=lunarbar ;;\n\
          \x20 LUNAR_CLOCK) restart=lunarbar ;;\n\
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

    /// `(clave, defecto)` of every block in the shipped claves that lives in
    /// `/etc/eclipse/entorno`.
    fn claves_del_entorno() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let dir = PROJECT_DIR.join("tools/lunarbar/claves.d");
        for e in fs::read_dir(&dir).unwrap().flatten() {
            let text = fs::read_to_string(e.path()).unwrap();
            for block in text.split("\n[") {
                let get = |k: &str| {
                    block.lines().find_map(|l| {
                        let (a, b) = l.split_once('=')?;
                        (a.trim() == k).then(|| b.trim().to_string())
                    })
                };
                if get("fichero").as_deref() == Some("/etc/eclipse/entorno") {
                    out.push((get("clave").unwrap(), get("defecto").unwrap()));
                }
            }
        }
        out
    }

    #[test]
    fn eclipse_entorno_takes_every_clave_the_panel_offers_and_each_program_reads() {
        let tmp = std::env::temp_dir().join(format!("xtask-entorno-todas-{}", std::process::id()));
        let bin = tmp.join("bin");
        fs::create_dir_all(&bin).unwrap();
        write_entorno(&bin);
        let claves = claves_del_entorno();
        for (name, value) in &claves {
            let ok = std::process::Command::new("/bin/sh")
                .arg(bin.join("eclipse-entorno"))
                .args([name, value])
                .env("ECLIPSE_ENTORNO", tmp.join("entorno"))
                .env("ECLIPSE_INIT_CTL", tmp.join("no-ctl"))
                .status()
                .unwrap()
                .success();
            assert!(ok, "eclipse-entorno refuses {name}={value}");
        }
        // Every clave the wallpaper and the bar read has its block in the
        // panel, so none of them is a setting only a text editor reaches.
        for (src, prefix) in [
            ("tools/lunarbg/src/scene/lecturas.rs", "\"LUNARBG_"),
            ("tools/lunarbar/src/barra.rs", "\"LUNARBAR_"),
        ] {
            let text = fs::read_to_string(PROJECT_DIR.join(src)).unwrap();
            for part in text.split(prefix).skip(1) {
                let rest = part.split('"').next().unwrap();
                if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
                    continue;
                }
                let name = format!("{}{rest}", &prefix[1..]);
                assert!(claves.iter().any(|(n, _)| *n == name), "{name} ({src}) has no clave");
            }
        }
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
    fn eclipse_udriver_installs_three_files_and_removes_them() {
        let tmp = std::env::temp_dir().join(format!("xtask-udriver-{}", std::process::id()));
        let bin = tmp.join("bin");
        fs::create_dir_all(&bin).unwrap();
        write_udriver(&bin);
        let drv = tmp.join("udrv-mt7922");
        fs::write(&drv, "#!/bin/sh\n").unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("/bin/sh")
                .arg(bin.join("eclipse-udriver"))
                .args(args)
                .env("ECLIPSE_UDRIVERS", tmp.join("lib"))
                .env("ECLIPSE_SERVICES", tmp.join("svc"))
                .env("ECLIPSE_CLAVES", tmp.join("claves"))
                .status()
                .unwrap()
                .success()
        };
        let d = drv.to_str().unwrap();
        assert!(run(&["instalar", "14c3", "0616", d]));
        let lib = tmp.join("lib/udrv-14c3-0616");
        assert!(lib.is_file());
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&lib).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let svc = fs::read_to_string(tmp.join("svc/udrv-14c3-0616.service")).unwrap();
        assert!(svc.contains("type = respawn\n"), "{svc}");
        assert!(svc.contains("\nudriver = pci-14c3-0616@0\n"), "{svc}");
        assert!(
            svc.contains(&format!("exec = {} pci-14c3-0616@0\n", lib.display())),
            "{svc}"
        );
        let block = fs::read_to_string(tmp.join("claves/90-udrv-14c3-0616.claves")).unwrap();
        assert!(block.contains("[driver:udrv-14c3-0616]\n"), "{block}");
        assert!(block.contains("ids = 14c3:0616\n"), "{block}");
        assert!(block.contains("udriver = pci-14c3-0616\n"), "{block}");
        assert!(block.contains("instalado = si\n"), "{block}");
        // Fail-closed: ids that are not four hex digits, a missing file.
        assert!(!run(&["instalar", "14C3", "0616", d]));
        assert!(!run(&["instalar", "14c3", "06;6", d]));
        assert!(!run(&["instalar", "14c3", "0617", "/no/such/file"]));
        assert!(!run(&["borrar", "14c3", "0616"]));
        assert!(run(&["quitar", "14c3", "0616"]));
        assert!(!lib.exists());
        assert!(!tmp.join("svc/udrv-14c3-0616.service").exists());
        assert!(!tmp.join("claves/90-udrv-14c3-0616.claves").exists());
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
