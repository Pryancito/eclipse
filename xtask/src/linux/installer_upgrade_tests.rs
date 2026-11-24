//! `--mode upgrade` del instalador: lo que renueva en la raíz ya instalada.
//!
//! El modo actualización copiaba tres ficheros a la partición EFI y no tocaba
//! la raíz jamás, así que una máquina ya instalada se quedaba para siempre con
//! los envoltorios de `/usr/local/bin`, las filas de `/etc/eclipse/services` y
//! el `eclipse-init` del día en que se instaló. Lo que ahora renueva está en
//! la tabla `ECLIPSE_OWNED` de `tools/install-eclipse/upgrade.h`.
//!
//! Lo que se cruza aquí es lo que ni el compilador de C ni el de Rust pueden
//! ver: que esas rutas VIAJEN EN EL MEDIO. El instalador copia desde su propia
//! raíz live, que es `LIVE_KEEP` y nada más; una ruta que se caiga de esa
//! lista deja la renovación muda --sin un error, sin un aviso, con la
//! actualización diciendo que fue bien-- y el síntoma vuelve a ser el de
//! siempre: un fichero de Eclipse que no existe en una máquina recién
//! actualizada.

use super::*;
use std::process::Command;

fn upgrade_header() -> String {
    fs::read_to_string(PROJECT_DIR.join("tools/install-eclipse/upgrade.h")).unwrap()
}

/// Las rutas de `ECLIPSE_OWNED`, con su `REFRESH_*`.
fn owned_paths() -> Vec<(String, String)> {
    let header = upgrade_header();
    let table = header
        .split_once("static const struct refresh_entry ECLIPSE_OWNED[] = {")
        .expect("la tabla ECLIPSE_OWNED")
        .1;
    let table = table.split_once("\n};").expect("el final de la tabla").0;
    let mut out = Vec::new();
    for line in table.lines() {
        let line = line.trim();
        if !line.starts_with("{ \"") {
            continue;
        }
        let mut parts = line.trim_start_matches('{').split(',');
        let rel = parts.next().unwrap().trim().trim_matches('"').to_string();
        let kind = parts.next().unwrap().trim().to_string();
        out.push((rel, kind));
    }
    assert!(out.len() >= 4, "solo {} rutas", out.len());
    out
}

/// Cada ruta que la actualización renueva tiene que venir en el medio, o sea
/// estar bajo una entrada de [`LIVE_KEEP`]. Si no, no hay nada que copiar.
#[test]
fn la_actualizacion_solo_renueva_rutas_que_el_medio_lleva() {
    for (rel, _) in owned_paths() {
        let cubierta = image::live_keep_covers(&rel);
        assert!(
            cubierta,
            "la actualización renueva /{rel}, que no está en LIVE_KEEP: el medio \
             no lo lleva y la renovación no copiaría nada"
        );
    }
}

/// La política de `/etc/eclipse`: las filas de servicios se pisan (describen
/// los programas de la imagen) y el resto del directorio NO (es el escritorio
/// elegido, el aspecto, el teclado, la zona horaria: lo que el usuario puso).
/// Pisar eso convierte una actualización en una pérdida de ajustes.
#[test]
fn la_configuracion_del_usuario_no_se_pisa_al_actualizar() {
    let paths = owned_paths();
    let services = paths
        .iter()
        .find(|(rel, _)| rel == "etc/eclipse/services")
        .expect("las filas de servicios se renuevan");
    assert_eq!(services.1, "REFRESH_SUFFIX");
    let etc = paths
        .iter()
        .find(|(rel, _)| rel == "etc/eclipse")
        .expect("el resto de /etc/eclipse está en la tabla");
    assert_eq!(
        etc.1, "REFRESH_IF_MISSING",
        "/etc/eclipse es configuración del usuario: solo se escribe lo que falte"
    );
    for (rel, kind) in &paths {
        if rel.starts_with("etc/") && rel != "etc/eclipse/services" && rel != "etc/os-release" {
            assert_eq!(
                kind, "REFRESH_IF_MISSING",
                "/{rel} se pisaría en cada actualización, y /etc es del usuario"
            );
        }
    }
}

/// `upgrade.h` tiene que estar en la lista de fuentes con la que el xtask
/// decide si recompilar el instalador. Si no, un cambio aquí deja el binario
/// del medio con el código viejo y no lo dice ni un aviso --la misma trampa
/// que la tabla de cadenas, que por eso ya está en esa lista.
#[test]
fn el_xtask_recompila_el_instalador_si_cambia_la_actualizacion() {
    let mod_rs = fs::read_to_string(PROJECT_DIR.join("xtask/src/linux/mod.rs")).unwrap();
    let sources = mod_rs
        .split_once("let sources = [")
        .expect("la lista de fuentes de install_eclipse")
        .1;
    let sources = sources.split_once("];").unwrap().0;
    for name in ["i18n.h", "upgrade.h"] {
        assert!(
            sources.contains(name),
            "{name} no está en la lista de fuentes del instalador: {sources}"
        );
    }
}

/// El arnés en C de la actualización, compilado y corrido: la política de la
/// tabla contra directorios de verdad (qué se pisa, qué no, el bit x de un
/// envoltorio, el `.new` que no debe quedarse, dos pasadas seguidas y
/// `--dry-run`). Es el único sitio del árbol donde el código del instalador
/// se EJECUTA; los demás tests leen su fuente.
///
/// Sin un compilador de C no se puede correr, y entonces no se finge que pasó:
/// se dice y se sale.
#[test]
fn el_arnes_de_la_actualizacion_pasa() {
    let dir = PROJECT_DIR.join("tools/install-eclipse");
    let cc = std::env::var("CC").unwrap_or_else(|_| String::from("cc"));
    if Command::new(&cc).arg("--version").output().is_err() {
        eprintln!("sin {cc}: no se corre el arnés del instalador");
        return;
    }
    let out = std::env::temp_dir().join(format!("eclipse-upgrade-harness-{}", std::process::id()));
    let zlib = PROJECT_DIR.join("tools/zlib");
    let mut cmd = Command::new(&cc);
    cmd.current_dir(&dir)
        .args(["-O1", "-D_LARGEFILE64_SOURCE=1", "-DNO_GZCOMPRESS"])
        .arg(format!("-I{}", zlib.display()))
        .arg("-I.")
        .arg("-o")
        .arg(&out)
        .arg("tests/upgrade-harness.c");
    for src in [
        "adler32.c",
        "crc32.c",
        "inflate.c",
        "inffast.c",
        "inftrees.c",
        "zutil.c",
        "gzlib.c",
        "gzread.c",
        "gzclose.c",
    ] {
        cmd.arg(zlib.join(src));
    }
    let build = cmd.output().expect("compilar el arnés");
    assert!(
        build.status.success(),
        "el arnés del instalador no compila:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(&out).output().expect("correr el arnés");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let _ = fs::remove_file(&out);
    assert!(
        run.status.success() && stdout.contains("TODO BIEN"),
        "el arnés del instalador falla:\n{stdout}\n{stderr}"
    );
}
