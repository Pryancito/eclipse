// May need move to drivers
use smoltcp::{
    iface::{InterfaceBuilder, Route, Routes},
    phy::Medium,
    wire::{IpAddress, IpCidr},
};

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

// use zcore_drivers::net::get_sockets;
use alloc::sync::Arc;

use alloc::string::String;
use lock::Mutex;

use crate::drivers::add_device;
use crate::drivers::all_net;
use zcore_drivers::net::{LoopbackInterface, LoopbackDevice};
use zcore_drivers::scheme::{NetScheme, NetStats};
use zcore_drivers::Device;

pub fn init() {
    let name = String::from("loopback");
    warn!("name : {}", name);
    // 初始化 一个 协议栈
    // 从外界 接受 一些 配置 参数 如果 没有 选择 默认 的

    let stats = Arc::new(Mutex::new(NetStats::default()));

    // 网络 设备
    // 默认 loopback
    let loopback = LoopbackDevice::new(Medium::Ip, stats.clone());

    // 为 设备 分配 网络 身份

    // ip 地址
    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8),
        IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 128),
    ];
    
    // Loopback does not require any default route/gateway
    static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 4] = [None; 4];
    let routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };

    let ip_addrs_clone = ip_addrs.clone();
    // 设置 主要 设置 iface
    let iface = InterfaceBuilder::new(loopback)
        .ip_addrs(ip_addrs)
        .routes(routes)
        .finalize();

    let loopback_iface = LoopbackInterface {
        iface: Arc::new(Mutex::new(iface)),
        name,
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs_clone)),
    };
    // loopback_iface
    let dev = Device::Net(Arc::new(loopback_iface));
    add_device(dev);
}

pub fn get_net_device() -> Vec<Arc<dyn NetScheme>> {
    let mut devices = all_net().as_vec().clone();
    // Real NICs first; loopback last (matches Linux ifindex 1 = first Ethernet).
    devices.sort_by_key(|d| if d.get_ifname() == "loopback" { 1 } else { 0 });
    devices
}

// ---------------------------------------------------------------------------
// Network RX waker registry
// ---------------------------------------------------------------------------
// TCP read futures register a Waker here before sleeping.
// E1000eInterface::poll() calls wake_net_rx_waiters() after iface.poll()
// so any task waiting for RX data is woken immediately instead of after 5 ms.

use core::task::Waker;
use lazy_static::lazy_static;

lazy_static! {
    static ref NET_RX_WAKERS: Mutex<Vec<Waker>> = Mutex::new(Vec::new());
}

/// Prevent unbounded registry growth if futures are cancelled or repeatedly re-register.
const MAX_NET_RX_WAKERS: usize = 1024;

fn register_waker_once(wakers: &mut Vec<Waker>, waker: &Waker) {
    if wakers.iter().any(|w| w.will_wake(waker)) {
        return;
    }
    if wakers.len() >= MAX_NET_RX_WAKERS {
        wakers.remove(0);
    }
    wakers.push(waker.clone());
}

/// Register the current task's Waker to be notified when RX data arrives.
pub fn register_net_rx_waker(waker: Waker) {
    register_waker_once(&mut NET_RX_WAKERS.lock(), &waker);
}

/// After an IRQ-driven wake: keep the waker for the next sleep cycle.
pub fn retain_net_rx_waker(waker: &Waker) {
    NET_RX_WAKERS.lock().retain(|w| w.will_wake(waker));
}

/// Wake tasks registered for TCP/UDP RX.
pub fn wake_net_rx_waiters() {
    let wakers: Vec<Waker> = core::mem::take(&mut *NET_RX_WAKERS.lock());
    for w in wakers {
        w.wake();
    }
}

/// Future that resolves when either:
///   (a) `wake_net_rx_waiters()` is called (NIC received data), or
///   (b) the timeout expires.
///
/// On first poll it registers the waker in NET_RX_WAKERS **and** installs
/// a fallback timer, so progress is guaranteed even if a wake is missed.
pub struct NetRxOrTimeoutFuture {
    registered: bool,
    deadline: core::time::Duration,
}

impl NetRxOrTimeoutFuture {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            registered: false,
            deadline: crate::timer::timer_now()
                + core::time::Duration::from_millis(timeout_ms),
        }
    }
}

impl core::future::Future for NetRxOrTimeoutFuture {
    type Output = ();

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<()> {
        // Second poll: woken by net data or timer → done.
        if self.registered {
            let waker = cx.waker();
            NET_RX_WAKERS.lock().retain(|w| !w.will_wake(waker));
            return core::task::Poll::Ready(());
        }
        if crate::timer::timer_now() >= self.deadline {
            return core::task::Poll::Ready(());
        }
        // Register waker for immediate NIC notification.
        register_waker_once(&mut NET_RX_WAKERS.lock(), cx.waker());
        // Fallback timer so we don't hang if the NIC wake is missed.
        let waker = cx.waker().clone();
        let dl = self.deadline;
        crate::timer::timer_set(dl, Box::new(move |_| waker.wake_by_ref()));
        self.registered = true;
        core::task::Poll::Pending
    }
}
