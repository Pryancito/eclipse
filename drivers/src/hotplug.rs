//! Alta y baja de dispositivos que aparecen DESPUÉS del sondeo del bus.
//!
//! En bare metal, todo dispositivo sale de los sondeos de bus de este crate y
//! `kernel-hal` lo da de alta una sola vez al arrancar: `pci::init` devuelve
//! una lista de [`Device`] y el llamante la recorre. Un disco USB no cabe en
//! ese modelo por dos razones, y las dos importan:
//!
//! * Aparece cuando lo enchufan, que puede ser horas después del arranque.
//! * Y **se va**, que es lo que de verdad no encajaba: las listas de
//!   `kernel-hal` eran solo-añadir, de modo que incluso dando de alta el disco
//!   se quedaba ahí para siempre, y cualquier lectura posterior iría contra un
//!   `slot` que el controlador ya reasignó a otra cosa.
//!
//! La dependencia va de `kernel-hal` a este crate, no al revés, así que este
//! módulo es el hueco y `kernel-hal` lo rellena en su init
//! (`install_hotplug_sink`). Mismo patrón que `pci_set_irq_host`, en la otra
//! dirección.
//!
//! Son punteros a función y no cierres a propósito: el destino es un par de
//! `fn` planas de `kernel-hal`, así el estático se construye en `const` y no
//! hace falta asignar nada para instalarlo.

use crate::sync::Mutex;
use crate::Device;

/// Da de alta un dispositivo. Devuelve el mismo `Device` si no hay destino.
type AddFn = fn(Device);
/// Da de baja el dispositivo que se le pase, por identidad (no por valor), y
/// dice si lo encontró.
type RemoveFn = fn(&Device) -> bool;

static SINK: Mutex<Option<(AddFn, RemoveFn)>> = Mutex::new(None);

/// Instala el destino. Lo llama `kernel-hal` una vez, en su init de drivers.
///
/// Instalar dos veces es un error del llamante y se avisa, pero el último gana:
/// un init que corra dos veces (una imagen que reinicia los drivers) no debe
/// quedarse con un destino viejo.
pub fn set_sink(add: AddFn, remove: RemoveFn) {
    let mut g = SINK.lock();
    if g.is_some() {
        warn!("[hotplug] el destino ya estaba instalado; se reemplaza");
    }
    *g = Some((add, remove));
}

/// Si hay destino instalado. Un driver puede usarlo para no molestarse en
/// construir el dispositivo cuando nadie lo va a recoger.
pub fn sink_installed() -> bool {
    SINK.lock().is_some()
}

/// Da de alta `dev`. `false` cuando no hay destino, y entonces **el
/// dispositivo se descarta**: decirlo es lo único honesto, porque un driver
/// que crea que lo dio de alta se queda esperando lecturas que nunca llegan.
#[must_use]
pub fn add(dev: Device) -> bool {
    let f = SINK.lock().as_ref().map(|(a, _)| *a);
    match f {
        Some(add) => {
            add(dev);
            true
        }
        None => {
            warn!("[hotplug] sin destino: se descarta {:?}", dev);
            false
        }
    }
}

/// Da de baja `dev`, por identidad. `false` si no había destino o si el
/// dispositivo no estaba dado de alta.
pub fn remove(dev: &Device) -> bool {
    let f = SINK.lock().as_ref().map(|(_, r)| *r);
    match f {
        Some(remove) => remove(dev),
        None => false,
    }
}
