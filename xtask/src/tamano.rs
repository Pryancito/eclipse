//! El techo de tamaño de un fichero de Rust, con trinquete.
//!
//! Un fichero de 10.000 líneas no lo lee nadie entero, y en este árbol llegó a
//! haber siete. Partirlos es barato (un `#[cfg(test)] mod X { ... }` de primer
//! nivel sale a `X/mod.rs` sin tocar una línea de producción), pero un techo a
//! secas no se puede encender: catorce ficheros pasan ya de 5.000 líneas y
//! bloquear el árbol entero hasta partirlos todos no arregla nada.
//!
//! Así que esto es un trinquete, no una puerta:
//!
//! - Un fichero NUEVO por encima del techo no entra.
//! - Los que ya pasaban están apuntados con su tamaño de hoy, y solo pueden
//!   ENCOGER. Crecer uno es un fallo con su nombre y su cifra.
//! - Cuando uno baja del techo, sale de la lista y ya no puede volver.
//!
//! Lo que mide son LÍNEAS, tests incluidos: lo que cuesta es abrir el fichero.
//! Un fichero de 4.000 líneas de las que 3.000 son tests se parte igual de
//! bien, y más barato.

use crate::PROJECT_DIR;
use std::process::Command;

/// Por encima de esto hay que partir el fichero. Cuatro mil líneas son unas
/// setenta pantallas.
const TECHO: usize = 4000;

/// Lo que no es nuestro: dependencias con su propio estilo y su propio dueño.
const AJENOS: &[&str] = &["vendor/", "smoltcp/", "nvidia-rm-sys/vendor/"];

/// Los que ya pasaban el techo el 9-oct-2026, con su tamaño de ese día.
///
/// **Esta lista solo baja.** Si un número hay que subirlo, lo que hace falta es
/// partir el fichero, no editar la línea.
const PASADOS: &[(&str, usize)] = &[
    ("drivers/src/display/nvidia.rs", 14954),
    (
        "drivers/src/display/nvidia/nouveau_bookkeeping_tests.rs",
        9344,
    ),
    ("linux-object/src/fs/devfs/drm.rs", 8387),
    ("drivers/src/usb/xhci_hid.rs", 7596),
    ("drivers/src/net/e1000e.rs", 7491),
    ("xtask/src/linux/mod.rs", 7452),
    ("linux-object/src/fs/devfs/drm_scheme.rs", 6582),
    ("linux-object/src/fs/procfs.rs", 6307),
    ("tools/eclipse-init/src/main.rs", 5744),
    ("linux-object/src/process.rs", 5643),
    ("xtask/src/linux/desktop.rs", 5510),
    ("zircon-object/src/vm/vmar.rs", 5495),
    ("drivers/src/scheme/syncobj.rs", 5286),
    ("tools/lunarbar/src/main.rs", 5241),
    ("linux-syscall/src/file/file.rs", 4556),
    (
        "linux-object/src/fs/devfs/drm_scheme/kms_scanout_tests.rs",
        4514,
    ),
    ("linux-syscall/src/task.rs", 4196),
    ("linux-object/src/fs/devfs/snd.rs", 4179),
    ("nvidia-rm-sys/src/os_boundary.rs", 4078),
];

/// Las líneas de un fichero, contadas como las cuenta `wc -l` salvo en la
/// última sin `\n`, que aquí SÍ cuenta: es una línea que hay que leer igual.
fn lineas(texto: &str) -> usize {
    texto.lines().count()
}

/// Los `.rs` que versiona git, con su número de líneas.
///
/// Se le pregunta a git y no al sistema de ficheros a propósito: `target/`
/// tiene ficheros generados de cientos de miles de líneas, y un submódulo que
/// no esté sacado no puede hacer fallar el test.
fn ficheros_rs() -> Vec<(String, usize)> {
    let salida = Command::new("git")
        .args(["ls-files", "*.rs"])
        .current_dir(&**PROJECT_DIR)
        .output()
        .expect("git ls-files");
    assert!(salida.status.success(), "git ls-files fallo");
    let lista = String::from_utf8(salida.stdout).expect("git ls-files no es utf-8");
    let mut out = Vec::new();
    for rel in lista.lines() {
        if rel.is_empty() || AJENOS.iter().any(|a| rel.starts_with(a)) {
            continue;
        }
        let ruta = PROJECT_DIR.join(rel);
        // Un fichero que git lista y no está en el disco es un submódulo sin
        // sacar; no es asunto de este test.
        if let Ok(texto) = std::fs::read_to_string(&ruta) {
            out.push((rel.to_string(), lineas(&texto)));
        }
    }
    assert!(
        out.len() > 100,
        "solo {} ficheros: el listado de git no salio bien",
        out.len()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Un fichero nuevo por encima del techo no entra. O se parte, o se apunta
    /// en [`PASADOS`] con un motivo — y apuntarlo es una decisión que se ve en
    /// la revisión, que es justo el punto.
    #[test]
    fn ningun_fichero_nuevo_pasa_del_techo() {
        let apuntados: BTreeMap<_, _> = PASADOS.iter().copied().collect();
        let mut nuevos = Vec::new();
        for (rel, n) in ficheros_rs() {
            if n > TECHO && !apuntados.contains_key(rel.as_str()) {
                nuevos.push(format!("{rel}: {n} lineas"));
            }
        }
        assert!(
            nuevos.is_empty(),
            "pasan del techo de {TECHO} lineas y no estan en PASADOS.\n\
             Partelos (un `#[cfg(test)] mod X {{ ... }}` de primer nivel sale a\n\
             su propio fichero sin tocar produccion) o apuntalos:\n  {}",
            nuevos.join("\n  ")
        );
    }

    /// Y los que ya pasaban solo pueden encoger.
    #[test]
    fn los_que_ya_pasaban_solo_pueden_encoger() {
        let real: BTreeMap<_, _> = ficheros_rs().into_iter().collect();
        let mut crecidos = Vec::new();
        for (rel, apuntado) in PASADOS {
            let Some(&ahora) = real.get(*rel) else {
                continue; // lo cubre el test de la lista rancia
            };
            if ahora > *apuntado {
                crecidos.push(format!("{rel}: {apuntado} -> {ahora}"));
            }
        }
        assert!(
            crecidos.is_empty(),
            "estos ficheros ya pasaban del techo y han CRECIDO.\n\
             El numero de PASADOS no se sube: lo que hace falta es partirlo.\n  {}",
            crecidos.join("\n  ")
        );
    }

    /// Cuando uno baja del techo sale de la lista, y ya no puede volver. Sin
    /// esto el trinquete se queda flojo: un fichero partido de verdad seguiria
    /// teniendo permiso para volver a sus 8.000 líneas.
    #[test]
    fn la_lista_no_guarda_ficheros_que_ya_caben() {
        let real: BTreeMap<_, _> = ficheros_rs().into_iter().collect();
        let mut sobran = Vec::new();
        for (rel, _) in PASADOS {
            match real.get(*rel) {
                None => sobran.push(format!("{rel}: ya no existe")),
                Some(&n) if n <= TECHO => sobran.push(format!("{rel}: {n} lineas, ya cabe")),
                Some(_) => {}
            }
        }
        assert!(
            sobran.is_empty(),
            "sacalos de PASADOS para que no puedan volver a crecer:\n  {}",
            sobran.join("\n  ")
        );
    }

    /// Un fichero apuntado dos veces dejaria que el numero mas alto ganara.
    #[test]
    fn la_lista_no_repite_ficheros() {
        let mut vistos = BTreeMap::new();
        for (rel, n) in PASADOS {
            assert!(
                vistos.insert(*rel, *n).is_none(),
                "{rel} esta dos veces en PASADOS"
            );
        }
        assert_eq!(vistos.len(), PASADOS.len());
    }

    /// La ultima linea sin `\n` cuenta: es una linea que hay que leer. (`wc -l`
    /// diria 1 para "a\nb", y el techo no va de contar saltos de linea.)
    #[test]
    fn la_ultima_linea_sin_salto_tambien_cuenta() {
        assert_eq!(lineas(""), 0);
        assert_eq!(lineas("a\n"), 1);
        assert_eq!(lineas("a\nb"), 2);
        assert_eq!(lineas("a\nb\n"), 2);
    }
}
