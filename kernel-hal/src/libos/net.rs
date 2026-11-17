use alloc::sync::Arc;
use alloc::vec::Vec;
use core::task::Waker;

use crate::drivers::add_device;
use crate::drivers::all_net;
use zcore_drivers::net::ShownLoopback;
use zcore_drivers::scheme::NetScheme;
use zcore_drivers::Device;

/// `lo`, behind which there is no stack: it is only shown.
pub fn init() {
    add_device(Device::Net(Arc::new(ShownLoopback::new())));
}

/// libOS host build: RX wakers are no-ops (real NIC path is bare-metal only).
pub fn register_net_rx_waker(_waker: Waker) {}

/// libOS host build: RX wakers are no-ops.
pub fn retain_net_rx_waker(_waker: &Waker) {}

/// libOS host build: RX wakers are no-ops.
pub fn clear_net_rx_waker(_waker: &Waker) {}

/// libOS host build: RX wakers are no-ops.
pub fn wake_net_rx_waiters() {}

/// libOS host build: RX wakers are no-ops.
pub fn wake_net_rx_waiters_now() {}

pub fn get_net_device() -> Vec<Arc<dyn NetScheme>> {
    all_net().as_vec().clone()
}

/// Run a closure over every network device without allocating a `Vec` on the heap.
/// Real NICs are visited first, loopback last.
pub fn for_each_net_device<F: FnMut(&dyn NetScheme)>(mut f: F) {
    let devices = all_net().as_vec();
    for dev in devices.iter() {
        f(dev.as_ref());
    }
}

pub struct NetRxOrTimeoutFuture {
    deadline: core::time::Duration,
}

impl NetRxOrTimeoutFuture {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            deadline: crate::timer::timer_now() + core::time::Duration::from_millis(timeout_ms),
        }
    }
}

impl core::future::Future for NetRxOrTimeoutFuture {
    type Output = ();

    fn poll(
        self: core::pin::Pin<&mut Self>,
        _cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<()> {
        if crate::timer::timer_now() >= self.deadline {
            core::task::Poll::Ready(())
        } else {
            core::task::Poll::Pending
        }
    }
}
