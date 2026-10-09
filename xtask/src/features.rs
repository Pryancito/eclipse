//! El libro de cuentas de las features: que ninguna quede sin que algo la
//! construya, y que un `cfg` nuevo no entre sin decidir quien lo compila.
//!
//! Por que existe. Esto ya ha pasado **cuatro veces**, y las cuatro se
//! descubrieron por accidente:
//!
//! - `virtio` puerteaba `drivers/src/virtio/` (696 lineas) y no lo compilaba
//!   nada, ni CI ni `make clippy`.
//! - `xhci-usb-hid` puerteaba `drivers/src/usb/` --- 4.700 lineas con los 24
//!   tests escritos para «el raton ha dejado de funcionar» y «la rueda no
//!   va» --- y esos tests **no habian corrido ni una vez**.
//! - `mock-disk` puerteaba el disco en RAM que `make MOCK=1` pone de raiz, y
//!   habia derivado tanto que `cargo clippy --features mock-disk` ni
//!   construia el crate.
//! - `mem-debug`, la unica herramienta del arbol que caza un desbordamiento
//!   **en la escritura**, no tenia interruptor ninguno y al pedirla a mano
//!   daba 18 errores.
//!
//! Los comentarios de `test.yml` cuentan los tres primeros. El patron es
//! siempre el mismo: una feature se escribe, se usa un dia a mano, y el codigo
//! que hay detras deja de compilarse sin que nada se queje --- hasta que hace
//! falta y no esta.
//!
//! Lo que hace este modulo es resolver **que enciende el arbol de verdad** y
//! obligar a que cada feature declarada tenga un veredicto escrito. Una
//! feature nueva no compila los tests hasta que alguien decide si algo la
//! construye o no, y por que.
//!
//! ## Lo que el resolutor sabe y lo que no
//!
//! Sabe dos cosas, y las dos preguntando en vez de suponiendo:
//!
//! - **Las raices de `make`**: se le pregunta a `make print-features` por cada
//!   combinacion de la rejilla de abajo, que son los interruptores
//!   documentados. Grepear el Makefile no vale: un `ifeq` mal anidado grepea
//!   igual de bien y no añade la feature.
//! - **Las raices de CI**: las lineas **no comentadas** que pasan `--features`
//!   en los workflows, los Makefiles y el propio xtask. Que esto descarte los
//!   comentarios no es adorno: ya sobrevivio un `// self.verify();` a su test
//!   por mirar el fichero entero.
//!
//! Y desde esas raices cierra el grafo de features de los crates del arbol, asi
//! que una feature que solo enciende otra feature cuenta como encendida.
//!
//! **No modela `default-features = false`.** `kernel-hal/default = ["libos"]`
//! y media docena de crates lo piden con los defaults apagados, asi que
//! resolverlo de verdad seria reimplementar a cargo. Las features que viven en
//! una lista `default` llevan por eso un veredicto a mano, y un test comprueba
//! que esa escotilla **solo** la usan ellas.

use crate::PROJECT_DIR;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// Los crates del arbol cuyas features entran en el libro.
///
/// `smoltcp/` queda fuera a proposito: es una bifurcacion de upstream y sus 25
/// features son las de upstream, no decisiones nuestras. Lo que tomamos de el
/// es lo que construye `cargo test --manifest-path smoltcp/Cargo.toml`. Un
/// crate nuevo con `[features]` que no este aqui hace fallar
/// `ningun_crate_con_features_se_queda_fuera_del_libro`.
const CRATES: &[&str] = &[
    "drivers",
    "kernel-hal",
    "linux-object",
    "loader",
    "rboot",
    "zCore",
    "zircon-object",
    "zircon-syscall",
];

/// Los crates con `[features]` que el libro NO cubre, y por que.
const FUERA: &[(&str, &str)] = &[(
    "smoltcp",
    "bifurcacion de upstream: sus features son de upstream, y lo que \
     construimos de el es su `cargo test` con los defaults",
)];

/// La rejilla de interruptores que se le pregunta a `make`.
///
/// Son los documentados en `zCore/Makefile`. Cada fila es un `make
/// print-features` entero; la union de todas es «lo que un desarrollador puede
/// encender desde la linea de ordenes».
const REJILLA: &[&[&str]] = &[
    &[],
    &["LIBOS=1"],
    &["LINUX=0"],
    &["LINUX=0", "LIBOS=1"],
    &["GRAPHIC=on"],
    &["MOCK=1"],
    &["TEST=1"],
    &["MEM_DEBUG=1"],
    &["ULEAK_SCAN=1"],
    &["HYPERVISOR=1"],
    &["NET=loopback"],
    &["PLATFORM=d1"],
    &["PLATFORM=fu740"],
    &["PLATFORM=c910light"],
];

/// Las arquitecturas con las que se recorre la rejilla.
const ARCOS: &[&str] = &["x86_64", "aarch64", "riscv64"];

/// Lo que el libro dice de una feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Veredicto {
    /// Algo la compila, pero ninguna linea de `cargo test` la enciende: el
    /// codigo de detras se construye y sus tests, si los tiene, no corren.
    ///
    /// Esta es la distincion que importa, y la que costo caro: `xhci-usb-hid`
    /// **si** se compilaba, via `zcore/linux`, y sus 24 tests --- los escritos
    /// para «el raton ha dejado de funcionar» y «la rueda no va» --- no habian
    /// corrido ni una vez.
    Construida,
    /// Alguna linea de `cargo test` la enciende: lo de detras se compila Y se
    /// prueba.
    Probada,
    /// Vive en una lista `default`, que el resolutor no modela. El texto dice
    /// quien la enciende de verdad.
    PorDefecto(&'static str),
    /// Nada la enciende, a proposito, y por que.
    Nadie(&'static str),
}

use Veredicto::{Construida, Nadie, PorDefecto, Probada};

/// El libro: una fila por feature declarada, con su veredicto.
///
/// El nombre del crate es el de su `[package] name`, no el del directorio.
pub(crate) const LIBRO: &[(&str, &str, Veredicto)] = &[
    // --- zcore: el binario del nucleo ---
    ("zcore", "linux", Probada),
    ("zcore", "zircon", Construida),
    ("zcore", "libos", Probada),
    ("zcore", "graphic", Construida),
    ("zcore", "mock-disk", Construida),
    ("zcore", "mem-debug", Construida),
    ("zcore", "uleak-scan", Construida),
    ("zcore", "baremetal-test", Construida),
    ("zcore", "hypervisor", Construida),
    ("zcore", "loopback", Construida),
    ("zcore", "no-pci", Construida),
    ("zcore", "link-user-img", Construida),
    ("zcore", "board-d1", Construida),
    ("zcore", "board-c910light", Construida),
    ("zcore", "board-fu740", Construida),
    (
        "zcore",
        "colorless-log",
        Nadie(
            "no hay interruptor: el color del log solo se puede quitar editando \
             el --features a mano. Candidata a un LOG_COLOR=off, igual que el \
             MEM_DEBUG=1 que esta feature-sin-interruptor acabo necesitando",
        ),
    ),
    (
        "zcore",
        "thead-maee",
        Nadie(
            "no hay interruptor, y board-d1 NO la enciende aunque el D1 sea un \
             T-Head: si las extensiones MAEE hacen falta en esa placa, esto es \
             un hueco de verdad y no una feature muerta. Decision de hardware",
        ),
    ),
    (
        "zcore",
        "qemu-debug-console",
        Nadie(
            "reenvio a kernel-hal/qemu-debug-console, que SI se enciende por \
             baremetal-test (TEST=1). La de aqui no la enciende nada: es un \
             duplicado del nombre, no codigo sin compilar",
        ),
    ),
    (
        "zcore",
        "allwinner-drivers",
        Nadie(
            "duplicado del nombre: board-d1 enciende kernel-hal/allwinner-drivers \
             directamente, sin pasar por esta",
        ),
    ),
    (
        "zcore",
        "fu740-drivers",
        Nadie(
            "duplicado del nombre: board-fu740 enciende kernel-hal/fu740-drivers \
             directamente, sin pasar por esta",
        ),
    ),
    // --- kernel-hal ---
    (
        "kernel-hal",
        "default",
        PorDefecto(
            "la lista default de kernel-hal, que es [\"libos\"]: en un build \
             baremetal la apagan con default-features = false",
        ),
    ),
    (
        "kernel-hal",
        "libos",
        PorDefecto(
            "la enciende zcore/libos, y tambien es el default de kernel-hal \
             --- que los crates del nucleo apagan con default-features = false \
             justamente para no arrastrar libos a un build baremetal",
        ),
    ),
    ("kernel-hal", "graphic", Construida),
    ("kernel-hal", "loopback", Construida),
    ("kernel-hal", "no-pci", Construida),
    ("kernel-hal", "uleak-scan", Construida),
    ("kernel-hal", "xhci-usb-hid", Probada),
    ("kernel-hal", "qemu-debug-console", Construida),
    ("kernel-hal", "board-c910light", Construida),
    ("kernel-hal", "allwinner-drivers", Construida),
    ("kernel-hal", "fu740-drivers", Construida),
    (
        "kernel-hal",
        "board-fu740",
        Nadie(
            "declarada y vacia: zcore/board-fu740 reenvia a \
             kernel-hal/fu740-drivers, no a esta, y en kernel-hal solo la nombra \
             un comentario de hart_walk.rs. No puertea codigo",
        ),
    ),
    (
        "kernel-hal",
        "link-user-img",
        Nadie(
            "declarada y vacia: la que puertea codigo es zcore/link-user-img \
             (zCore/src/fs.rs), que es otra feature con el mismo nombre. Esta no \
             la enciende nada y no hay nada detras",
        ),
    ),
    (
        "kernel-hal",
        "thead-maee",
        Nadie("la enciende zcore/thead-maee, que tampoco tiene interruptor"),
    ),
    // --- zcore-drivers ---
    ("zcore-drivers", "graphic", Probada),
    ("zcore-drivers", "virtio", Probada),
    ("zcore-drivers", "mock", Probada),
    ("zcore-drivers", "loopback", Construida),
    ("zcore-drivers", "no-pci", Construida),
    ("zcore-drivers", "xhci-usb-hid", Probada),
    ("zcore-drivers", "allwinner", Construida),
    ("zcore-drivers", "fu740", Construida),
    (
        "zcore-drivers",
        "legacy-usb-hid",
        Nadie(
            "la ruta HID vieja, alternativa a xhci-usb-hid, que es la que se \
             construye. Nada la enciende: o se le pone interruptor para poder \
             comparar las dos, o se quita. Decision suya",
        ),
    ),
    (
        "zcore-drivers",
        "board_malta",
        Nadie(
            "su unico uso es cfg(all(feature = \"board_malta\", target_arch = \
             \"mips\")) y el arbol no construye para MIPS: muerta por plataforma, \
             no por olvido",
        ),
    ),
    // --- linux-object ---
    ("linux-object", "mock-disk", Probada),
    // --- zircon-object / zircon-syscall ---
    ("zircon-object", "libos", Probada),
    ("zircon-object", "elf", Probada),
    ("zircon-object", "aspace-separate", Probada),
    (
        "zircon-object",
        "hypervisor",
        Nadie(
            "zcore/hypervisor NO reenvia a esta, asi que HYPERVISOR=1 enciende \
             la feature del binario y deja sin compilar el codigo de hipervisor \
             de aqui. Es el mismo hueco que el de zircon-syscall",
        ),
    ),
    ("zircon-syscall", "libos", Probada),
    (
        "zircon-syscall",
        "hypervisor",
        Nadie(
            "igual que la de zircon-object: zcore/hypervisor no reenvia, y aqui \
             hay ocho usos en lib.rs que no compila nadie",
        ),
    ),
    (
        "zircon-syscall",
        "deny-page-fault",
        Nadie(
            "solo se mira con cfg!(any(feature = \"deny-page-fault\", \
             not(target_os = \"none\"))) en vmar.rs: en un build de host la otra \
             rama ya es cierta, y en baremetal nada la enciende",
        ),
    ),
    // --- loader ---
    (
        "zcore-loader",
        "default",
        PorDefecto(
            "la lista default de zcore-loader, [\"libos\", \"linux\", \"zircon\"]: \
             zcore pide las tres por su cuenta segun el modo",
        ),
    ),
    ("zcore-loader", "linux", Probada),
    ("zcore-loader", "zircon", Construida),
    ("zcore-loader", "libos", Probada),
    // --- rboot ---
    (
        "rboot",
        "default",
        PorDefecto(
            "la lista default de rboot, que es [\"rboot\"]: asi es como se \
             construye el cargador",
        ),
    ),
    (
        "rboot",
        "rboot",
        PorDefecto("el default de rboot, que es como se construye el cargador"),
    ),
    (
        "rboot",
        "boot-ui",
        Nadie(
            "ningun build del cargador la enciende. El codigo de detras no queda \
             sin compilar de milagro: va con cfg(any(feature = \"boot-ui\", \
             test)), asi que lo compilan los tests de rboot. Esa `test` es la \
             unica razon de que esto no sea el caso de virtio otra vez",
        ),
    ),
];

/// La tabla `[features]` de un `Cargo.toml`, tal cual.
///
/// Parser a mano, y a proposito: meter un crate de TOML para ocho tablas de
/// este tamaño es mas dependencia que ayuda. Lo que **no** hace es saltarse una
/// linea que no entiende: revienta. Saltarsela es justo como una feature se
/// escapa del libro, que es lo que esto viene a impedir.
fn tabla_de_features(dir: &Path) -> BTreeMap<String, Vec<String>> {
    let texto = std::fs::read_to_string(dir.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("no se pudo leer {}/Cargo.toml: {e}", dir.display()));
    let mut out = BTreeMap::new();
    let mut dentro = false;
    let mut pendiente: Option<(String, String)> = None;
    for linea in texto.lines() {
        let t = linea.trim();
        if let Some((nombre, mut acumulado)) = pendiente.take() {
            acumulado.push(' ');
            acumulado.push_str(t);
            if acumulado.contains(']') {
                out.insert(nombre, entradas(&acumulado));
            } else {
                pendiente = Some((nombre, acumulado));
            }
            continue;
        }
        if t.starts_with('[') {
            dentro = t == "[features]";
            continue;
        }
        if !dentro || t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (nombre, resto) = t.split_once('=').unwrap_or_else(|| {
            panic!(
                "linea de [features] que no entiendo en {}/Cargo.toml: {t:?}; \
                 si es valida, enseñale a este parser a leerla --- saltarsela \
                 es como una feature se escapa del libro",
                dir.display()
            )
        });
        let nombre = nombre.trim().trim_matches('"').to_string();
        let resto = resto.trim().to_string();
        if resto.contains(']') {
            out.insert(nombre, entradas(&resto));
        } else {
            pendiente = Some((nombre, resto));
        }
    }
    assert!(
        pendiente.is_none(),
        "{}/Cargo.toml: una lista de features sin cerrar",
        dir.display()
    );
    out
}

/// Los `"..."` de una lista de features.
fn entradas(lista: &str) -> Vec<String> {
    lista
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// El `[package] name` de un crate.
fn nombre_del_crate(dir: &Path) -> String {
    let texto = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    for linea in texto.lines() {
        if let Some(resto) = linea.trim().strip_prefix("name") {
            if let Some((_, v)) = resto.split_once('=') {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    panic!("{}/Cargo.toml sin name", dir.display())
}

/// El grafo de features del arbol: `(crate, feature) -> las que enciende`.
fn grafo() -> BTreeMap<(String, String), Vec<(String, String)>> {
    let mut nombres = BTreeMap::new();
    for dir in CRATES {
        let d = PROJECT_DIR.join(dir);
        nombres.insert(nombre_del_crate(&d), d);
    }
    let mut g = BTreeMap::new();
    for (crate_, dir) in &nombres {
        for (feature, deps) in tabla_de_features(dir) {
            let mut aristas = Vec::new();
            for dep in deps {
                // `dep:x` activa una dependencia opcional, no una feature.
                if dep.starts_with("dep:") {
                    continue;
                }
                match dep.split_once('/') {
                    Some((c, f)) => {
                        let c = c.trim_end_matches('?');
                        if nombres.contains_key(c) {
                            aristas.push((c.to_string(), f.to_string()));
                        }
                    }
                    None => aristas.push((crate_.clone(), dep)),
                }
            }
            g.insert((crate_.clone(), feature), aristas);
        }
    }
    g
}

/// Las features que `make` enciende, preguntandole por cada fila de la rejilla.
fn raices_de_make() -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for arco in ARCOS {
        for fila in REJILLA {
            let salida = Command::new("make")
                .current_dir(PROJECT_DIR.join("zCore"))
                .arg("--no-print-directory")
                .arg("--eval=print-%: ; @echo $($*)")
                .arg("print-features")
                .arg(format!("ARCH={arco}"))
                .args(*fila)
                .output()
                .expect("make no se ha podido ejecutar");
            // Una fila que no vale para este arco (LIBOS en otro arco, una
            // PLATFORM de riscv en x86_64) sale con $(error): no es un fallo
            // del test, es que esa combinacion no existe.
            if !salida.status.success() {
                continue;
            }
            for f in String::from_utf8_lossy(&salida.stdout).split_whitespace() {
                out.insert(("zcore".to_string(), f.to_string()));
            }
        }
    }
    out
}

/// Las features que una linea de construccion o de test enciende con
/// `--features`, en los workflows, los Makefiles y el propio xtask.
/// Los ficheros del arbol que contienen lineas de construccion o de test.
///
/// Se salta `features.rs`: es el que LEE lineas de construccion, asi que las
/// que use de ejemplo en sus tests se contarian como reales. Lo cazo el propio
/// libro --- la linea de ejemplo de
/// `un_comentario_que_nombra_una_feature_no_la_construye` resucito a
/// `legacy-usb-hid` ---, y aqui no se construye nada: es una tabla.
fn ficheros_de_construccion() -> Vec<std::path::PathBuf> {
    let mut ficheros: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(PROJECT_DIR.join(".github/workflows")) {
        ficheros.extend(dir.filter_map(Result::ok).map(|e| e.path()));
    }
    ficheros.push(PROJECT_DIR.join("Makefile"));
    ficheros.push(PROJECT_DIR.join("zCore/Makefile"));
    let mut pila = vec![PROJECT_DIR.join("xtask/src")];
    while let Some(d) = pila.pop() {
        if let Ok(dir) = std::fs::read_dir(&d) {
            for e in dir.filter_map(Result::ok) {
                let p = e.path();
                if p.is_dir() {
                    pila.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    ficheros.push(p);
                }
            }
        }
    }
    ficheros.retain(|f| !f.ends_with("features.rs"));
    ficheros
}

fn raices_de_ci() -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for f in ficheros_de_construccion() {
        let Ok(texto) = std::fs::read_to_string(&f) else {
            continue;
        };
        for linea in texto.lines() {
            out.extend(raices_de_linea(linea));
        }
    }
    out
}

/// Las features que enciende UNA linea, o nada si no es una linea que
/// construya.
///
/// Las lineas comentadas no construyen nada, y este arbol nombra features en
/// los comentarios a punta pala para explicar por que existen --- `test.yml`
/// tiene seis comentarios con `--features` dentro, uno de ellos para decir que
/// ahi NO se pasa. Contarlos daria por construida cualquier feature que alguien
/// se haya molestado en explicar, que es exactamente como un
/// `// self.verify();` sobrevivio a su test.
fn raices_de_linea(linea: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let t = linea.trim();
    if t.starts_with('#') || t.starts_with("//") || !t.contains("--features") {
        return out;
    }
    let crate_ = t.split("-p ").nth(1).and_then(|r| {
        r.split_whitespace()
            .next()
            .filter(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
    });
    for trozo in t.split("--features").skip(1) {
        let lista = trozo
            .trim_start()
            .trim_start_matches('=')
            .trim_start()
            .trim_start_matches('"')
            .split(['"', ' ', '\''])
            .next()
            .unwrap_or("");
        for feature in lista.split(',').filter(|x| !x.is_empty()) {
            match feature.split_once('/') {
                Some((c, f)) => {
                    out.insert((c.to_string(), f.to_string()));
                }
                None => {
                    if let Some(c) = crate_ {
                        out.insert((c.to_string(), feature.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Las features que enciende una linea de `cargo test`, y solo esas.
fn raices_de_test() -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for f in ficheros_de_construccion() {
        let Ok(texto) = std::fs::read_to_string(&f) else {
            continue;
        };
        for linea in texto.lines().filter(|l| l.contains("cargo test")) {
            out.extend(raices_de_linea(linea));
        }
    }
    out
}

/// Lo que el arbol PRUEBA: las raices de `cargo test`, cerradas sobre el grafo.
fn probadas() -> BTreeSet<(String, String)> {
    cierre(raices_de_test())
}

/// Todo lo que el arbol enciende: las raices, cerradas sobre el grafo.
fn alcanzables() -> BTreeSet<(String, String)> {
    cierre(raices_de_make().into_iter().chain(raices_de_ci()).collect())
}

fn cierre(raices: BTreeSet<(String, String)>) -> BTreeSet<(String, String)> {
    let g = grafo();
    let mut pila: Vec<(String, String)> = raices.into_iter().collect();
    let mut vistas = BTreeSet::new();
    while let Some(n) = pila.pop() {
        if !vistas.insert(n.clone()) {
            continue;
        }
        if let Some(hijas) = g.get(&n) {
            pila.extend(hijas.iter().cloned());
        }
    }
    vistas
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La red principal: una feature declarada y sin fila en el libro no
    /// compila los tests. Es lo que convierte «a nadie se le ocurrio
    /// compilarla» en una decision escrita.
    #[test]
    fn toda_feature_declarada_tiene_veredicto() {
        let g = grafo();
        let libro: BTreeSet<(String, String)> = LIBRO
            .iter()
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .collect();
        let faltan: Vec<_> = g.keys().filter(|k| !libro.contains(k)).collect();
        assert!(
            faltan.is_empty(),
            "features declaradas y sin fila en LIBRO: {faltan:?}. \
             Decide quien las construye --- o escribe por que nadie --- y añadelas"
        );
    }

    /// Y al reves: una fila que nombra una feature que ya no existe es un
    /// veredicto sobre nada, y lo peor que puede tener el libro es ruido que
    /// nadie se cree.
    #[test]
    fn el_libro_no_habla_de_features_que_no_existen() {
        let g = grafo();
        let sobran: Vec<_> = LIBRO
            .iter()
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| !g.contains_key(k))
            .collect();
        assert!(
            sobran.is_empty(),
            "filas de LIBRO que no corresponden a ninguna feature declarada: \
             {sobran:?}"
        );
    }

    /// La etiqueta tiene que ser exacta en los dos sentidos: lo que el libro da
    /// por `Probada` lo enciende una linea de `cargo test`, y lo que da por
    /// `Construida` **no**. Si alguien añade la feature a un `cargo test`, esto
    /// se queja --- y esa queja es la unica forma de enterarse de que una
    /// feature paso de solo-compilada a probada, que es el cambio que nadie
    /// mira.
    #[test]
    fn la_etiqueta_de_probada_dice_la_verdad() {
        let prob = probadas();
        let mal_construida: Vec<_> = LIBRO
            .iter()
            .filter(|(_, _, v)| *v == Construida)
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| prob.contains(k))
            .collect();
        assert!(
            mal_construida.is_empty(),
            "el libro las da por solo-construidas y un `cargo test` las \
             enciende: {mal_construida:?}. Subelas a Probada"
        );
        let mal_probada: Vec<_> = LIBRO
            .iter()
            .filter(|(_, _, v)| *v == Probada)
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| !prob.contains(k))
            .collect();
        assert!(
            mal_probada.is_empty(),
            "el libro las da por probadas y ninguna linea de `cargo test` las \
             enciende: {mal_probada:?}. O se ha caido esa linea, o bajan a \
             Construida"
        );
    }

    /// Lo que el libro da por encendido, el resolutor lo encuentra. Asi una
    /// fila no puede quedarse rancia cuando alguien quita la linea que la
    /// construia.
    #[test]
    fn lo_que_el_libro_da_por_encendido_lo_enciende_algo() {
        let viva = alcanzables();
        let mentiras: Vec<_> = LIBRO
            .iter()
            .filter(|(_, _, v)| *v == Construida)
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| !viva.contains(k))
            .collect();
        assert!(
            mentiras.is_empty(),
            "el libro las da por encendidas y nada del arbol las enciende: \
             {mentiras:?}. O se ha caido la linea que las construia, o la fila \
             tiene que pasar a Nadie con su motivo"
        );
    }

    /// Y lo que el libro da por muerto sigue muerto. Si alguien le pone
    /// interruptor a una, esto se queja: el libro deja de ser verdad y hay que
    /// subirla a `Encendida`.
    #[test]
    fn lo_que_el_libro_da_por_muerto_sigue_sin_construirse() {
        let viva = alcanzables();
        let resucitadas: Vec<_> = LIBRO
            .iter()
            .filter(|(_, _, v)| matches!(v, Nadie(_)))
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| viva.contains(k))
            .collect();
        assert!(
            resucitadas.is_empty(),
            "el libro dice que nadie las construye y el arbol si: \
             {resucitadas:?}. Subelas a Encendida"
        );
    }

    /// Un motivo vacio no es un motivo. La fila existe para que el siguiente
    /// que la lea no tenga que volver a medirlo.
    #[test]
    fn ningun_veredicto_va_sin_explicacion() {
        for (c, f, v) in LIBRO {
            let motivo = match v {
                Construida | Probada => continue,
                Nadie(m) | PorDefecto(m) => *m,
            };
            assert!(
                motivo.len() > 20,
                "{c}/{f}: el motivo es demasiado corto para servirle a nadie: \
                 {motivo:?}"
            );
        }
    }

    /// La escotilla `PorDefecto` existe porque el resolutor no modela
    /// `default-features = false`. Solo puede usarla una feature que este de
    /// verdad en una lista `default`, o seria la puerta por la que se cuela
    /// cualquier cosa sin medirla.
    #[test]
    fn la_escotilla_por_defecto_solo_vale_para_las_de_default() {
        let g = grafo();
        let mut en_default: BTreeSet<(String, String)> = BTreeSet::new();
        for ((c, f), hijas) in &g {
            if f == "default" {
                en_default.insert((c.clone(), f.clone()));
                en_default.extend(hijas.iter().cloned());
            }
        }
        let colados: Vec<_> = LIBRO
            .iter()
            .filter(|(_, _, v)| matches!(v, PorDefecto(_)))
            .map(|(c, f, _)| (c.to_string(), f.to_string()))
            .filter(|k| !en_default.contains(k))
            .collect();
        assert!(
            colados.is_empty(),
            "usan la escotilla PorDefecto sin estar en ninguna lista default: \
             {colados:?}"
        );
    }

    /// Y que la lista de crates cubiertos no se quede corta: un crate nuevo con
    /// `[features]` tiene que entrar en el libro o quedar excluido por escrito.
    #[test]
    fn ningun_crate_con_features_se_queda_fuera_del_libro() {
        let dentro: BTreeSet<&str> = CRATES.iter().copied().collect();
        let fuera: BTreeSet<&str> = FUERA.iter().map(|(c, _)| *c).collect();
        let mut huerfanos = Vec::new();
        for entrada in std::fs::read_dir(&**PROJECT_DIR)
            .unwrap()
            .filter_map(Result::ok)
        {
            let dir = entrada.path();
            if !dir.join("Cargo.toml").is_file() {
                continue;
            }
            let nombre = entrada.file_name().to_string_lossy().to_string();
            if dentro.contains(nombre.as_str()) || fuera.contains(nombre.as_str()) {
                continue;
            }
            if !tabla_de_features(&dir).is_empty() {
                huerfanos.push(nombre);
            }
        }
        assert!(
            huerfanos.is_empty(),
            "crates con [features] que el libro no cubre ni excluye: \
             {huerfanos:?}. Añadelos a CRATES, o a FUERA con su motivo"
        );
        for (_, motivo) in FUERA {
            assert!(motivo.len() > 20, "una exclusion sin motivo util");
        }
    }

    /// El resolutor descarta las lineas comentadas, y eso hay que fijarlo: los
    /// comentarios de este arbol nombran features a punta pala para explicar
    /// por que existen, asi que un lector que no las descarte da por
    /// construida cualquiera que alguien se moleste en explicar. Es como
    /// `// self.verify();` sobrevivio a su test.
    #[test]
    fn un_comentario_que_nombra_una_feature_no_la_construye() {
        let linea = "          cargo test -p zcore-drivers --lib --features legacy-usb-hid";
        assert_eq!(
            raices_de_linea(linea),
            [("zcore-drivers".to_string(), "legacy-usb-hid".to_string())]
                .into_iter()
                .collect(),
            "la linea de verdad tiene que contar"
        );
        for comentada in [format!("# {linea}"), format!("// {linea}")] {
            assert!(
                raices_de_linea(&comentada).is_empty(),
                "una linea comentada no construye nada: {comentada:?}"
            );
        }
        // Y el arbol tiene de eso: seis comentarios con `--features` solo en
        // test.yml, uno para decir que ahi no se pasa ninguna.
        let yml = std::fs::read_to_string(PROJECT_DIR.join(".github/workflows/test.yml")).unwrap();
        let comentarios = yml
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('#') && t.contains("--features")
            })
            .count();
        assert!(
            comentarios > 0,
            "si ya no quedan comentarios con --features, este test se queda sin \
             caso real y hay que decidir si sigue valiendo"
        );
    }

    /// El parser revienta con lo que no entiende, en vez de saltarselo.
    ///
    /// Saltarse una linea es justo como una feature se escapa del libro, que es
    /// lo que todo esto viene a impedir. No se puede comprobar ensuciando un
    /// `Cargo.toml` del arbol --- cargo rechaza el fichero antes de que corra
    /// un test ---, asi que se le da uno de mentira.
    #[test]
    #[should_panic(expected = "linea de [features] que no entiendo")]
    fn el_parser_revienta_con_lo_que_no_entiende() {
        let dir = std::env::temp_dir().join("eclipse-libro-features");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[features]\nturbo\n",
        )
        .unwrap();
        tabla_de_features(&dir);
    }

    /// Y lee lo que si entiende: una entrada en una linea, una en varias, y una
    /// con el nombre entre comillas.
    #[test]
    fn el_parser_lee_las_tres_formas_de_escribir_una_feature() {
        let dir = std::env::temp_dir().join("eclipse-libro-features-ok");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[features]\n\
             corta = [\"a\", \"dep/b\"]\n\
             \"entre-comillas\" = []\n\
             larga = [\n  \"c\",\n  \"otro/d\",\n]\n\
             \n[dependencies]\nnada = \"1\"\n",
        )
        .unwrap();
        let t = tabla_de_features(&dir);
        assert_eq!(t.len(), 3, "{t:?}");
        assert_eq!(t["corta"], vec!["a", "dep/b"]);
        assert_eq!(t["entre-comillas"], Vec::<String>::new());
        assert_eq!(t["larga"], vec!["c", "otro/d"]);
    }

    /// `ULEAK_SCAN=1` tiene que llegar a la feature, y no estar puesta cuando
    /// no se pide. El escaner llevaba desde que se escribio sin manera de
    /// encenderlo, y su propio comentario te dice que lo enciendas.
    #[test]
    fn uleak_scan_enciende_la_feature_y_solo_cuando_se_pide() {
        let con = features_de_make(&["ARCH=x86_64", "ULEAK_SCAN=1"]);
        assert!(
            con.iter().any(|f| f == "uleak-scan"),
            "ULEAK_SCAN=1 no llego a la feature: {con:?}"
        );
        let sin = features_de_make(&["ARCH=x86_64"]);
        assert!(
            !sin.iter().any(|f| f == "uleak-scan"),
            "uleak-scan aparece sin pedirla: {sin:?}"
        );
    }

    /// Y en LIBOS es un error, no un silencio: el escaner es
    /// `cfg(not(feature = "libos"))`, asi que ahi daria un nucleo sin escaner
    /// con pinta de tenerlo.
    #[test]
    fn uleak_scan_en_libos_es_un_error() {
        let salida = Command::new("make")
            .current_dir(PROJECT_DIR.join("zCore"))
            .arg("--no-print-directory")
            .arg("--eval=print-%: ; @echo $($*)")
            .arg("print-features")
            .args(["ARCH=x86_64", "LIBOS=1", "ULEAK_SCAN=1"])
            .output()
            .expect("make no se ha podido ejecutar");
        assert!(
            !salida.status.success(),
            "ULEAK_SCAN=1 LIBOS=1 tenia que fallar y salio bien: {}",
            String::from_utf8_lossy(&salida.stdout)
        );
        let err = String::from_utf8_lossy(&salida.stderr);
        assert!(
            err.contains("ULEAK_SCAN"),
            "el error no nombra ULEAK_SCAN: {err}"
        );
    }

    fn features_de_make(args: &[&str]) -> Vec<String> {
        let salida = Command::new("make")
            .current_dir(PROJECT_DIR.join("zCore"))
            .arg("--no-print-directory")
            .arg("--eval=print-%: ; @echo $($*)")
            .arg("print-features")
            .args(args)
            .output()
            .expect("make no se ha podido ejecutar");
        assert!(
            salida.status.success(),
            "make print-features {args:?} fallo: {}",
            String::from_utf8_lossy(&salida.stderr)
        );
        String::from_utf8_lossy(&salida.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }
}
