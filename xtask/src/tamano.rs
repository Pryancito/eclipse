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
//! - Los que ya pasaban llevan cada uno su propio techo, redondeado un poco
//!   por encima de su tamaño del día en que entró. Pasarse de él es un fallo
//!   con su nombre y su cifra.
//! - Cuando uno baja del techo general, sale de la lista y ya no puede volver.
//!
//! **Por qué un techo por fichero y no «solo puede encoger»**: la primera
//! versión de esto apuntaba el tamaño exacto del día, y doce horas después ya
//! bloqueaba el árbol — un merge ajeno le había metido 42 líneas a `drm.rs`, y
//! con «solo encoger» eso enrojece el PR siguiente, que no tiene nada que ver.
//! El margen (hasta el siguiente múltiplo de 250) deja respirar un cambio
//! honesto y sigue impidiendo que un fichero doble. Subir un techo se puede,
//! pero es una línea en el diff que el revisor ve, no un silencio.
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

/// Los que ya pasaban el techo el 9-oct-2026, con el techo propio de cada uno
/// (su tamaño de ese día redondeado al siguiente múltiplo de 250).
///
/// `xtask/src/linux/desktop.rs` **salió de aquí** el 13-oct-2026 y no puede
/// volver: su entrada avisaba de que estaba a cinco líneas del techo y por tanto
/// «bloqueando al siguiente que lo toque por algo que no es suyo», y es lo que
/// pasó — el visor de imágenes lo dejó en 6013 y el rojo salía en todos los
/// PRs del árbol. Sacar su `mod tests` a `desktop/tests.rs` lo dejó en 3912
/// sin tocar una línea de producción, que es el arreglo que su propio
/// comentario nombraba.
///
/// Antes de subir un número: mirar si el fichero tiene un `#[cfg(test)] mod X
/// { ... }` de primer nivel. Si lo tiene, sale a su propio fichero sin tocar
/// una línea de producción —un `mod X;` lo lee igual de `X.rs` que de
/// `X/mod.rs`— y lo más probable es que el fichero baje del techo general y
/// salga de esta lista. Eso es lo que le pasó a
/// `tools/eclipse-init/src/main.rs` el mismo día que entró este test: creció
/// 280 líneas, este test las vio, y partirlo lo dejó en 3.313.
/// **`linux-object/src/process.rs`, subido a 6250 el 14-oct-2026**: el #1886
/// metio 131 lineas de produccion y lo dejo en 6129 sobre un techo de 6000,
/// o sea MASTER ROJO en este test para todo el mundo. Sube al siguiente
/// multiplo de 250, que es la regla de arriba, y no a la cifra del dia. Pero
/// es el fichero que mas veces ha tocado su techo: lo que toca es partirlo, y
/// aqui ya no vale lo de sacar un `#[cfg(test)] mod` de primer nivel porque
/// sus tests viven todos en ficheros aparte desde hace tandas. Lo que queda
/// dentro y es un tema entero son las credenciales (`setuid`/`setgid`/
/// `setgroups` y sus comprobaciones) y el control de trabajos.
///
/// `linux-syscall/src/file/file.rs` **salio de aqui** el 20-oct-2026, por la
/// misma via que `desktop.rs`: el #1986 lo dejo en 4795 sobre un techo de
/// 4750 (master rojo en este test para todo el arbol), y sacar sus catorce
/// `#[cfg(test)] mod X { ... }` de primer nivel a `file/X.rs` lo dejo en 3860
/// sin tocar una linea de produccion. Ya no puede volver.
const PASADOS: &[(&str, usize)] = &[
    // 15000: subido el 25-oct-2026 por la tanda del cliente del RM por
    // contexto, que lleva el `ctx_idx` a los ocho sitios que liberan un handle
    // del RM (un handle solo existe en SU cliente, asi que el contexto viaja
    // con el). Son 29 lineas de hilvanado de argumentos sobre un techo que
    // estaba a 0 de distancia -- master llego justo a 14750. El 18-oct ya
    // paso lo mismo con los comptags, por lo mismo: aqui ya no vale sacar un
    // `#[cfg(test)] mod` de primer nivel, porque los diez que tiene son todos
    // `mod X;` a ficheros aparte desde hace tandas. **Lo que toca es partir el
    // fichero**, como dice el comentario de arriba, y no es la tanda que
    // hilvana un argumento.
    //
    // 13500: BAJADO el 28-oct-2026 por la limpieza del codigo muerto (los
    // pasos gpustep2-4 y 11-13, la rama quiet del arranque del GSP), que lo
    // dejo en 13472.
    ("drivers/src/display/nvidia.rs", 13500),
    (
        "drivers/src/display/nvidia/nouveau_bookkeeping_tests.rs",
        9500,
    ),
    // 8750: el trabajo multi-GPU (CRTC, framebuffer y presents por tarjeta)
    // lo dejo en 8614 con el techo en 8500, y master estaba rojo por ello.
    // Sus tests ya estan todos fuera; lo que toca es sacar produccion a
    // submodulos, no volver a subir esto.
    ("linux-object/src/fs/devfs/drm.rs", 8750),
    // 8000: la recuperacion del transporte de almacenamiento masivo (reset
    // Bulk-Only, reintento del CSW y reintento por sentido) lo dejo en 7754
    // con el techo en 7750. Sus tests estan TODOS fuera ya --veintitres
    // `mod *_tests` en ficheros propios-- asi que aqui no queda nada barato
    // que sacar: lo que toca la vez que haya que volver a subirlo es partir
    // produccion (el camino de los hubs y el de almacenamiento masivo son dos
    // submodulos que no se hablan), no subir el numero otra vez.
    ("drivers/src/usb/xhci_hid.rs", 8000),
    ("drivers/src/net/e1000e.rs", 7500),
    // 6000: BAJADO el 25-oct-2026, que es para lo que está este test. El
    // fichero llegó a 7965 con seis tests nuevos y 35 líneas de holgura, así
    // que `rootfs_plumbing_tests` salió a `linux/rootfs_plumbing_tests.rs`
    // (2181 líneas, ni una de producción) y quedó en 5786. El techo baja con
    // él al siguiente múltiplo de 250. De paso la extracción destapó un
    // cruce que pasaba en falso: `install_tests` leía este fichero buscando
    // quién arranca cada wrapper, y lo que lo satisfacía era el texto de los
    // tests que ahora están fuera.
    ("xtask/src/linux/mod.rs", 6000),
    // 7000: master llego a 6753 con el techo en 6750, asi que el trinquete
    // estaba rojo antes de esta tanda y no por ella. 7250: la busqueda por
    // tarjeta del nodo (multi-GPU) lo dejo en 7025; mismo caso que drm.rs.
    ("linux-object/src/fs/devfs/drm_scheme.rs", 7250),
    ("linux-object/src/fs/procfs.rs", 6500),
    // Subido el 14-oct y otra vez el 16; lo que toca es partirlo, ver arriba.
    ("linux-object/src/process.rs", 6500),
    ("zircon-object/src/vm/vmar.rs", 5500),
    // 4750 desde el 28-oct-2026: sus dos `mod tests` salieron a
    // `main_tests/` (730 líneas, ni una de producción) para que cupiera el
    // menú de hunter, y quedó en 4576.
    ("tools/lunarbar/src/main.rs", 4750),
    (
        "linux-object/src/fs/devfs/drm_scheme/kms_scanout_tests.rs",
        4750,
    ),
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

    /// Y los que ya pasaban no pueden pasarse de su propio techo.
    #[test]
    fn ninguno_de_los_pasados_se_sale_de_su_techo() {
        let real: BTreeMap<_, _> = ficheros_rs().into_iter().collect();
        let mut crecidos = Vec::new();
        for (rel, techo) in PASADOS {
            let Some(&ahora) = real.get(*rel) else {
                continue; // lo cubre el test de la lista rancia
            };
            if ahora > *techo {
                crecidos.push(format!("{rel}: {ahora} lineas, su techo es {techo}"));
            }
        }
        assert!(
            crecidos.is_empty(),
            "estos ficheros se han pasado de su techo.\n\
             Mira primero si tienen un `#[cfg(test)] mod X {{ ... }}` de primer nivel:\n\
             sacarlo a su propio fichero no toca produccion y suele bajar del techo\n\
             general. Subir el numero se puede, pero es una decision que se ve.\n  {}",
            crecidos.join("\n  ")
        );
    }

    /// El margen es a propósito, y acotado: un techo muy por encima de lo que
    /// el fichero mide hoy no sujeta nada. Medio millar de líneas de aire es
    /// el doble del redondeo, así que un techo que se pase de eso es uno que
    /// alguien subió sin partir el fichero.
    #[test]
    fn ningun_techo_lleva_mas_aire_del_que_hace_falta() {
        let real: BTreeMap<_, _> = ficheros_rs().into_iter().collect();
        let mut flojos = Vec::new();
        for (rel, techo) in PASADOS {
            let Some(&ahora) = real.get(*rel) else {
                continue;
            };
            if techo.saturating_sub(ahora) > 500 {
                flojos.push(format!("{rel}: {ahora} lineas y un techo de {techo}"));
            }
        }
        assert!(
            flojos.is_empty(),
            "estos techos ya no sujetan nada: bajalos a lo que mide el fichero\n\
             (redondeado al siguiente multiplo de 250).\n  {}",
            flojos.join("\n  ")
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
        // Lo que le paso a `tools/eclipse-init/src/main.rs` el 9-oct: entro en
        // la lista con 5.744 lineas, crecio 280 en la misma tanda, este test
        // lo vio, y partir su modulo de tests lo dejo en 3.313 y fuera.
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
