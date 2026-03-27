//! IPC async/await: `RecvFuture`, `recv_async()` y `block_on`.
//!
//! El kernel ya hace receive no bloqueante (devuelve 0 si no hay mensaje).
//! Aquí se expone un `Future` que hace poll al `recv()` y devuelve `Pending` si no hay dato;
//! `block_on` repite poll + yield hasta que el future esté listo.
//!
//! Uso con `block_on` (sin executor):
//! ```ignore
//! let mut ch = IpcChannel::new();
//! let mut recv_fut = ch.recv_async();
//! if let Some(msg) = eclipse_ipc::block_on(&mut recv_fut) { ... }
//! ```
//!
//! Uso con executor (si tienes uno que haga `.await`):
//! ```ignore
//! let msg = ch.recv_async().await;
//! ```

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::channel::IpcChannel;
use crate::types::EclipseMessage;

/// Future que completa cuando hay un mensaje IPC disponible.
/// Se crea con [`IpcChannel::recv_async`].
#[must_use = "futures do nothing unless polled"]
pub struct RecvFuture<'a> {
    pub(crate) channel: &'a mut IpcChannel,
}

impl core::marker::Unpin for RecvFuture<'_> {}

impl Future for RecvFuture<'_> {
    type Output = Option<EclipseMessage>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // RecvFuture is Unpin, so we can get &mut self
        let this = unsafe { Pin::get_unchecked_mut(self) };
        match this.channel.recv() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

/// Waker que no hace nada (para executors que solo hacen poll en bucle).
fn noop_waker() -> Waker {
    unsafe {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(core::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE))
    }
}

/// Ejecuta un future hasta que termine, haciendo yield cuando devuelve `Pending`.
///
/// Útil para esperar un mensaje IPC sin un executor completo:
/// ```ignore
/// let mut recv_fut = channel.recv_async();
/// let msg = eclipse_ipc::block_on(&mut recv_fut);
/// ```
pub fn block_on<F>(future: &mut F) -> F::Output
where
    F: Future + Unpin,
{
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match Pin::new(&mut *future).poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => unsafe { crate::libc::yield_cpu() },
        }
    }
}
