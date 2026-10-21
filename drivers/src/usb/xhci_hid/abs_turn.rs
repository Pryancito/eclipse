//! El torniquete de la bandera del puntero absoluto.
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
