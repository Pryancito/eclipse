//! Registrar dos veces la misma tarjeta no la pone dos veces.
//!
//! `DRM_STATE.drivers` es una lista a la que sólo se añade, y a propósito:
//! aquí no se desenchufa ninguna GPU. Lo que no era a propósito es que
//! `create_root_fs` recorre `drivers::all_drm()` y registra cada tarjeta que
//! encuentra, y `LinuxProcess::new` llama a `create_root_fs` --para eso existe
//! `with_root`, para no «re-scanning disks / re-mounting»--, así que cada
//! llamada metía una SEGUNDA entrada por cada tarjeta ya registrada.
//!
//! Lo que cuesta: `build_gpu_nodes` hace un nodo por entrada, así que una
//! tarjeta duplicada se lleva un `/dev/dri/cardN` duplicado y empuja los
//! minors de las que vengan detrás; `software_kms_active()` pregunta lo mismo
//! dos veces; y cada duplicado se queda con un clon del `Arc` para el resto
//! del arranque.

use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Cuántas entradas de la lista son `driver`, por identidad.
fn copies(driver: &Arc<dyn DrmScheme>) -> usize {
    DRM_STATE
        .lock()
        .drivers
        .iter()
        .filter(|d| Arc::ptr_eq(d, driver))
        .count()
}

/// El caso que pasaba de verdad: el mismo `Arc` registrado otra vez, que es
/// lo que hace `create_root_fs` en cada `LinuxProcess::new`.
#[test]
fn registrar_la_misma_tarjeta_otra_vez_no_la_duplica() {
    // `attach` toma el cerrojo de los globales de DRM y la `Screen` lo
    // retiene: sin él esto contaría las tarjetas de otro test.
    let screen = kms_emu::attach(16, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-dedupe").console_at(0x01, 0x00));
    let driver = DRM_STATE
        .lock()
        .drivers
        .iter()
        .find(|d| gpu.is_driver(d))
        .cloned()
        .expect("attach_gpu la registra");

    assert_eq!(copies(&driver), 1, "attach_gpu la registra una vez");
    register_driver(driver.clone());
    register_driver(driver.clone());
    assert_eq!(
        copies(&driver),
        1,
        "la lista se duplicaba en cada create_root_fs, y nada quitaba las copias"
    );
    drop(gpu);
    drop(screen);
}
