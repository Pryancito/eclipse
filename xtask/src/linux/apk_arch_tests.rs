//! De que arquitectura baja apk los paquetes.
//!
//! Sacado de `mod.rs`, que volvio a romper su techo de lineas al anadirle la
//! fontaneria de la segunda pareja de sondas del `PT_INTERP`. El remedio que
//! prescribe el trinquete es este --- un `#[cfg(test)] mod X` de primer nivel
//! a su propio fichero --- y no subir el numero.
//!
//! Lo decide `/etc/apk/arch` dentro del rootfs, no `/etc/apk/repositories`:
//! ese solo nombra la rama (`.../v3.24/main`) y apk le pega el arco detras.
//! Sin ese fichero apk cae a su arco compilado --- el del HOST en una
//! compilacion cruzada.

use super::*;

/// Misma convencion que `xorg::scratch`: el arbol en /tmp va etiquetado
/// por proceso e hilo para que dos tests en paralelo no se pisen.
fn scratch(que: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "eclipse-apk-arch-{que}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Lo que se escribe es el arco objetivo, tal cual y con salto de linea,
/// que es como lo lee apk.
#[test]
fn el_arco_escrito_es_el_del_objetivo() {
    for arch in ["x86_64", "aarch64", "riscv64"] {
        let etc = scratch(arch).join("etc");
        LinuxRootfs::write_apk_arch(&etc, arch);
        let leido = fs::read_to_string(etc.join("apk").join("arch")).unwrap();
        assert_eq!(leido, format!("{arch}\n"), "arco de {arch}");
    }
}

/// Se escribe aunque `etc/apk` no exista todavia: en la ruta desde cero el
/// directorio lo crea el bloque de apk, pero en la incremental no hay quien
/// lo cree, y un `return` silencioso ahi dejaba el rootfs sin arco.
#[test]
fn crea_el_directorio_si_hace_falta() {
    let etc = scratch("crea").join("etc");
    assert!(!etc.exists());
    LinuxRootfs::write_apk_arch(&etc, "aarch64");
    assert!(etc.join("apk").join("arch").is_file());
}

/// Un segundo `cargo rootfs` para otro arco tiene que PISAR el valor, no
/// dejar el de la tirada anterior de `make release`.
#[test]
fn una_segunda_tirada_pisa_el_arco_de_la_primera() {
    let etc = scratch("pisa").join("etc");
    LinuxRootfs::write_apk_arch(&etc, "x86_64");
    LinuxRootfs::write_apk_arch(&etc, "aarch64");
    assert_eq!(
        fs::read_to_string(etc.join("apk").join("arch")).unwrap(),
        "aarch64\n"
    );
}

/// La configuracion de apk no puede volver a colgar de que exista el
/// estatico del objetivo: ese es best-effort (aqui mismo el de aarch64 no
/// se baja) y lo que arma el cierre de paquetes es el apk del host. Iban
/// juntos dentro de un `if apk.is_file()`, asi que un objetivo sin
/// estatico salia ademas sin `/etc/apk/repositories`.
#[test]
fn la_configuracion_de_apk_no_cuelga_del_estatico_del_objetivo() {
    let src = include_str!("mod.rs");
    let i = src
        .find("if apk.is_file() {")
        .expect("sigue habiendo una comprobacion del estatico");
    let bloque = &src[i..];
    let fin = bloque
        .find("\n            }")
        .or_else(|| bloque.find("\n        }"))
        .unwrap_or(bloque.len());
    let dentro = &bloque[..fin];
    for aguja in ["repositories", "write_apk_arch", "install_apk_keys"] {
        assert!(
            !dentro.contains(aguja),
            "{aguja} ha vuelto a quedar dentro del `if apk.is_file()`"
        );
    }
}
