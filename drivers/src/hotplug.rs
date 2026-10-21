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

/// Un torniquete para los tests que instalan destino, y la forma de dejar el
/// hueco vacio otra vez.
///
/// `SINK` es un global del proceso entero, asi que no basta con que los tests
/// de este modulo tomen turno entre ellos: el driver de xHCI publica discos
/// por aqui y sus tests tambien instalan destino. Dos suites distintas
/// pisandose el destino es un fallo de uno de cada tres en paralelo, y de los
/// que ensenan un test roto que no lo esta. Por eso el torniquete vive aqui,
/// debajo de los dos, y no en el modulo de tests de ninguno.
#[cfg(test)]
pub(crate) static TEST_TURNSTILE: Mutex<()> = Mutex::new(());

/// Desinstala el destino. Solo para los tests: en marcha el destino lo pone
/// `kernel-hal` una vez y no se quita.
#[cfg(test)]
pub(crate) fn clear_sink() {
    *SINK.lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::{BlockScheme, Scheme};
    use crate::DeviceResult;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// What the installed sink saw. The sink is a plain `fn` pointer (that is
    /// what lets the static be built in `const`), so it cannot capture: the
    /// bookkeeping has to be global too, and is reset by the guard below.
    static ADDED: AtomicUsize = AtomicUsize::new(0);
    static REMOVED: AtomicUsize = AtomicUsize::new(0);
    /// A tag for the last device the sink was given, so a test can tell WHICH
    /// device arrived rather than just how many. A `u64` cannot hold a name,
    /// and the sink cannot capture one, so the name is folded into a number.
    static LAST_ADDED: AtomicUsize = AtomicUsize::new(0);

    /// Clears the installed sink and the counters on both sides of a test, so
    /// one that installs a sink cannot leave it behind for the next.
    struct Alone(#[allow(dead_code)] crate::sync::MutexGuard<'static, ()>);

    impl Alone {
        fn new() -> Self {
            let g = TEST_TURNSTILE.lock();
            clear();
            Self(g)
        }
    }

    impl Drop for Alone {
        fn drop(&mut self) {
            clear();
        }
    }

    fn clear() {
        clear_sink();
        ADDED.store(0, Ordering::SeqCst);
        REMOVED.store(0, Ordering::SeqCst);
        LAST_ADDED.store(0, Ordering::SeqCst);
    }

    fn record_add(dev: Device) {
        ADDED.fetch_add(1, Ordering::SeqCst);
        LAST_ADDED.store(tag(&dev), Ordering::SeqCst);
    }

    /// A sink that claims it found whatever it was asked to remove.
    fn record_remove_found(_dev: &Device) -> bool {
        REMOVED.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// A sink that was given a device it does not have.
    fn record_remove_missing(_dev: &Device) -> bool {
        REMOVED.fetch_add(1, Ordering::SeqCst);
        false
    }

    /// A number that stands for the driver inside a `Device`, folded from the
    /// name it reports.
    fn tag(dev: &Device) -> usize {
        dev.inner()
            .name()
            .bytes()
            .fold(0usize, |acc, b| acc * 31 + b as usize)
    }

    /// A stand-in for a USB disk: something that can be put in a `Device`.
    struct Pendrive(&'static str);

    impl Scheme for Pendrive {
        fn name(&self) -> &str {
            self.0
        }
    }

    impl BlockScheme for Pendrive {
        fn read_block(&self, _id: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _id: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            128
        }
    }

    fn disk(name: &'static str) -> Device {
        Device::Block(Arc::new(Pendrive(name)))
    }

    /// Before `kernel-hal` fills the hole, there is nowhere for a device to
    /// go. Saying so is the whole point: a driver that believed it had
    /// registered its disk would sit waiting for reads that never arrive.
    #[test]
    fn without_a_sink_an_added_device_is_reported_as_discarded() {
        let _alone = Alone::new();
        assert!(!sink_installed());
        assert!(!add(disk("sda")), "add claimed success with no sink");
        assert!(!remove(&disk("sda")), "remove claimed success with no sink");
    }

    /// With a sink installed, the device reaches it -- and the SAME device,
    /// which matters because the sink is handed it by value.
    #[test]
    fn an_added_device_reaches_the_installed_sink() {
        let _alone = Alone::new();
        set_sink(record_add, record_remove_found);
        assert!(sink_installed());

        let dev = disk("sdb");
        let expected = tag(&dev);
        assert!(add(dev));
        assert_eq!(ADDED.load(Ordering::SeqCst), 1);
        assert_eq!(
            LAST_ADDED.load(Ordering::SeqCst),
            expected,
            "another device arrived at the sink"
        );
    }

    /// A removal is answered by the SINK, not by this module: a device the
    /// kernel does not have registered must come back `false` even with a
    /// sink installed, because that is a different situation from "nowhere to
    /// ask" and whoever is debugging needs them apart.
    #[test]
    fn a_removal_answers_what_the_sink_found() {
        let _alone = Alone::new();

        set_sink(record_add, record_remove_found);
        assert!(remove(&disk("sdc")));
        assert_eq!(REMOVED.load(Ordering::SeqCst), 1);

        set_sink(record_add, record_remove_missing);
        assert!(!remove(&disk("sdc")));
        assert_eq!(REMOVED.load(Ordering::SeqCst), 2);
    }

    /// Installing twice is a caller bug, but the LAST one has to win: an
    /// image that restarts its drivers must not keep handing devices to the
    /// sink of the previous run.
    #[test]
    fn installing_a_second_sink_replaces_the_first() {
        let _alone = Alone::new();

        /// A sink that counts into `REMOVED` on an ADD, so a device that went
        /// to the old sink instead of the new one is visible.
        fn wrong_add(_dev: Device) {
            REMOVED.fetch_add(100, Ordering::SeqCst);
        }

        set_sink(wrong_add, record_remove_found);
        set_sink(record_add, record_remove_found);
        assert!(add(disk("sdd")));
        assert_eq!(ADDED.load(Ordering::SeqCst), 1);
        assert_eq!(
            REMOVED.load(Ordering::SeqCst),
            0,
            "the device went to the replaced sink"
        );
    }

    /// `add` is `#[must_use]`, and the reason is that a caller which ignores
    /// it has silently dropped a device; the handful of devices in a row case
    /// must each be reported.
    #[test]
    fn several_devices_in_a_row_each_reach_the_sink() {
        let _alone = Alone::new();
        set_sink(record_add, record_remove_found);
        for name in ["sde", "sdf", "sdg"] {
            assert!(add(disk(name)));
        }
        assert_eq!(ADDED.load(Ordering::SeqCst), 3);
    }

    /// The hole is a pair of plain `fn` pointers so the static can be built
    /// in `const` with nothing allocated: pin that it is still `None` before
    /// anyone installs anything, which is what makes `sink_installed` a
    /// usable question for a driver deciding whether to build a device at
    /// all.
    #[test]
    fn the_hole_starts_empty_and_asking_does_not_fill_it() {
        let _alone = Alone::new();
        assert!(!sink_installed());
        assert!(!sink_installed());
    }
}
