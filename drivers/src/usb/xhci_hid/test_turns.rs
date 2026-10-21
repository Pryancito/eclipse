//! Los torniquetes de los globales que los tests tocan.
//!
//! [`USB_ABS_POINTER`](super::USB_ABS_POINTER) es un global del proceso, y lo
//! que decide es si los informes relativos de un ratón se entregan o se
//! callan. Un test que enumera una tableta la pone a `true`, y mientras lo
//! hace cualquier test de ratón que corra en paralelo vería sus informes
//! descartados: un fallo de uno de cada tres, en un test que no está roto.
//! Por eso el torniquete está debajo de los dos lados, y lo toman los dos.

use super::*;

static ABS_POINTER_TURNSTILE: Mutex<()> = Mutex::new(());

/// Toma el turno y deja la bandera apagada a los dos lados del test.
pub(in crate::usb) struct AbsPointerTurn(#[allow(dead_code)] lock::MutexGuard<'static, ()>);

impl AbsPointerTurn {
    pub(in crate::usb) fn new() -> Self {
        let g = ABS_POINTER_TURNSTILE.lock();
        USB_ABS_POINTER.store(false, Ordering::Relaxed);
        Self(g)
    }
}

impl Drop for AbsPointerTurn {
    fn drop(&mut self) {
        USB_ABS_POINTER.store(false, Ordering::Relaxed);
    }
}

static POLL_TURNSTILE: Mutex<()> = Mutex::new(());

/// Toma el turno de [`POLL_INSTANCES`](super::POLL_INSTANCES) y lo deja vacío
/// a los dos lados del test.
///
/// La lista de controladores que sondea el timer es un global del proceso, y
/// el fake es por hilo: dos tests sondeando a la vez se encontrarían con el
/// controlador del otro, que para el fake no existe.
///
/// Quien necesite también el torniquete de `hotplug` lo toma **después** de
/// éste, siempre en ese orden.
pub(in crate::usb) struct PollTurn(#[allow(dead_code)] lock::MutexGuard<'static, ()>);

impl PollTurn {
    pub(in crate::usb) fn new() -> Self {
        let g = POLL_TURNSTILE.lock();
        set_poll_instance(None);
        Self(g)
    }
}

impl Drop for PollTurn {
    fn drop(&mut self) {
        set_poll_instance(None);
    }
}

static MSI_TURNSTILE: Mutex<()> = Mutex::new(());

/// Toma el turno de la cola de MSI apuntados y la deja vacía a los dos lados
/// del test, junto con el host de interrupciones.
///
/// [`MSI_PENDING`](super::MSI_PENDING) y
/// [`MSI_IRQ_HOST`](super::MSI_IRQ_HOST) son globales del proceso: un test
/// que cuenta lo que hay en la cola y otro que apunta un vector a la vez se
/// pisan, y el que cuenta falla sin estar roto.
///
/// Se toma **después** de [`PollTurn`] cuando hacen falta los dos.
pub(in crate::usb) struct MsiTurn(#[allow(dead_code)] lock::MutexGuard<'static, ()>);

impl MsiTurn {
    pub(in crate::usb) fn new() -> Self {
        let g = MSI_TURNSTILE.lock();
        Self::clear();
        Self(g)
    }

    fn clear() {
        MSI_PENDING.lock().clear();
        *MSI_IRQ_HOST.lock() = None;
    }

    /// Los vectores que hay apuntados ahora mismo.
    pub(in crate::usb) fn vectores(&self) -> Vec<usize> {
        MSI_PENDING.lock().iter().map(|(v, _)| *v).collect()
    }
}

impl Drop for MsiTurn {
    fn drop(&mut self) {
        Self::clear();
    }
}
