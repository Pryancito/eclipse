// May need move to drivers
use smoltcp::{
    iface::{InterfaceBuilder, NeighborCache, Route, Routes},
    phy::Medium,
    wire::{EthernetAddress, IpAddress, IpCidr},
};

use alloc::collections::BTreeMap;
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
    let loopback = LoopbackDevice::new(Medium::Ethernet, stats.clone());

    // 为 设备 分配 网络 身份

    // 物理地址
    let mac: [u8; 6] = [0x52, 0x54, 0x98, 0x76, 0x54, 0x32];
    let ethernet_addr = EthernetAddress::from_bytes(&mac);
    // ip 地址
    let ip_addrs = [IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)];
    
    // Loopback does not require any default route/gateway
    static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 1] = [None; 1];
    let routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };
    
    // arp缓存
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    // 设置 主要 设置 iface
    let iface = InterfaceBuilder::new(loopback)
        .ethernet_addr(ethernet_addr)
        .ip_addrs(ip_addrs)
        .routes(routes)
        .neighbor_cache(neighbor_cache)
        .finalize();

    let loopback_iface = LoopbackInterface {
        iface: Arc::new(Mutex::new(iface)),
        name,
        stats,
        routes: Arc::new(Mutex::new(vec![])),
    };
    // loopback_iface
    let dev = Device::Net(Arc::new(loopback_iface));
    add_device(dev);
}

pub fn get_net_device() -> Vec<Arc<dyn NetScheme>> {
    let mut devices = all_net().as_vec().clone();
    devices.sort_by_key(|d| d.get_ifname() != "loopback");
    devices
}
