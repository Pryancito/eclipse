//! Lo que todo cliente Wayland de `tools/lunarbar` tiene que llevar.
//!
//! Fuera de `mod.rs` por el techo de lineas, que es lo que prescribe el
//! trinquete (`xtask/src/tamano.rs`): sacar un `#[cfg(test)] mod` de primer
//! nivel a su propio fichero, sin tocar ni una linea de produccion y sin
//! subir ningun techo. Lo pide el arreglo de formato del array de mtimes de
//! `lunar_tools`, que rustfmt quiere vertical (su `array_width` son 60
//! columnas, no 100) y que por si solo dejaba este fichero en 5756 lineas
//! con 5750 de techo. `use super::*` sigue significando lo mismo: este
//! modulo sigue siendo hijo de `linux`.

use super::*;

/// Every Wayland client that binds the foreign-toplevel manager must tell
/// wayland-client what object its `toplevel` event creates. Without that
/// specialization the client aborts on the first window it is told about,
/// which is a crash that only shows up under a real compositor: it needs
/// another window to exist, so neither a build nor a `--dump` catches it.
#[test]
fn wayland_clients_specialize_the_toplevel_event() {
    let dir = PROJECT_DIR.join("tools").join("lunarbar").join("src");
    for rel in ["main.rs", "bin/lunarrun.rs"] {
        let src = fs::read_to_string(dir.join(rel)).unwrap();
        if !src.contains("ZwlrForeignToplevelManagerV1") {
            continue;
        }
        assert!(
            src.contains("event_created_child!"),
            "{rel} binds zwlr_foreign_toplevel_manager_v1 without an \
             event_created_child! specialization; it will abort as soon \
             as a window exists"
        );
    }
}
