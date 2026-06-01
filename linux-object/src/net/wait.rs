//! Wait for NIC RX, a tty interrupt (Ctrl+C), or a timeout.

use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

/// Resolves when any of: pending Ctrl+C, `wake_net_rx_waiters()`, or deadline.
pub struct NetOrTtyWait {
    deadline: Duration,
    armed: bool,
}

impl NetOrTtyWait {
    pub fn new_after_ms(ms: u64) -> Self {
        Self {
            deadline: kernel_hal::timer::timer_now() + Duration::from_millis(ms),
            armed: false,
        }
    }
}

impl Future for NetOrTtyWait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if crate::fs::stdio::ctrl_c_pending_peek() {
            return Poll::Ready(());
        }
        if kernel_hal::timer::timer_now() >= self.deadline {
            return Poll::Ready(());
        }
        if self.armed {
            kernel_hal::net::retain_net_rx_waker(cx.waker());
            return Poll::Ready(());
        }
        crate::fs::stdio::register_tty_intr_waker(cx.waker().clone());
        kernel_hal::net::register_net_rx_waker(cx.waker().clone());
        let waker = cx.waker().clone();
        let dl = self.deadline;
        kernel_hal::timer::timer_set(dl, Box::new(move |_| waker.wake_by_ref()));
        self.armed = true;
        Poll::Pending
    }
}
