// smoltcp
use smoltcp::{iface::Interface, phy::{self, DeviceCapabilities, Medium}, time::Instant, Result};
use alloc::collections::VecDeque;

use crate::net::get_sockets;
use alloc::sync::Arc;

use alloc::string::String;
use lock::Mutex;

use crate::scheme::{NetScheme, Scheme, RouteInfo, NetStats};
use crate::{DeviceError, DeviceResult};

use alloc::vec::Vec;
use smoltcp::wire::EthernetAddress;
use smoltcp::wire::{IpCidr, Ipv4Cidr};

pub struct LoopbackDevice {
    queue: VecDeque<Vec<u8>>,
    medium: Medium,
    stats: Arc<Mutex<NetStats>>,
}

impl LoopbackDevice {
    pub fn new(medium: Medium, stats: Arc<Mutex<NetStats>>) -> Self {
        Self {
            queue: VecDeque::new(),
            medium,
            stats,
        }
    }
}

impl<'a> phy::Device<'a> for LoopbackDevice {
    type RxToken = LoopbackRxToken;
    type TxToken = LoopbackTxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 65535;
        caps.medium = self.medium;
        caps
    }

    fn receive(&'a mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let stats = self.stats.clone();
        self.queue.pop_front().map(move |buffer| {
            let rx = LoopbackRxToken { buffer, stats: stats.clone() };
            let tx = LoopbackTxToken {
                queue: &mut self.queue,
                stats,
            };
            (rx, tx)
        })
    }

    fn transmit(&'a mut self) -> Option<Self::TxToken> {
        Some(LoopbackTxToken {
            queue: &mut self.queue,
            stats: self.stats.clone(),
        })
    }
}

pub struct LoopbackRxToken {
    buffer: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::RxToken for LoopbackRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut stats = self.stats.lock();
        stats.rx_packets += 1;
        stats.rx_bytes += self.buffer.len() as u64;
        drop(stats);

        f(&mut self.buffer)
    }
}

pub struct LoopbackTxToken<'a> {
    queue: &'a mut VecDeque<Vec<u8>>,
    stats: Arc<Mutex<NetStats>>,
}

impl<'a> phy::TxToken for LoopbackTxToken<'a> {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut buffer = alloc::vec![0u8; len];
        let result = f(&mut buffer);

        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        drop(stats);

        self.queue.push_back(buffer);
        result
    }
}

#[derive(Clone)]
pub struct LoopbackInterface {
    pub iface: Arc<Mutex<Interface<'static, LoopbackDevice>>>,
    pub name: String,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
}

impl Scheme for LoopbackInterface {
    fn name(&self) -> &str {
        "loopback"
    }

    fn handle_irq(&self, _cause: usize) {}
}

impl NetScheme for LoopbackInterface {
    fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
        unimplemented!()
    }
    fn send(&self, _buf: &[u8]) -> DeviceResult<usize> {
        unimplemented!()
    }
    fn poll(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(crate::net::timer_now_as_micros() as i64);
        let sockets = get_sockets();
        let mut sockets = sockets.lock();
        match self.iface.lock().poll(&mut sockets, timestamp) {
            Ok(_) => Ok(()),
            Err(err) => {
                debug!("poll got err {}", err);
                Err(DeviceError::IoError)
            }
        }
    }

    fn get_mac(&self) -> EthernetAddress {
        EthernetAddress::default()
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        Vec::from(self.iface.lock().ip_addrs())
    }
    
    fn add_route(&self, cidr: IpCidr, gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        self.routes.lock().push(RouteInfo { dst: cidr, gateway });
        Ok(())
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        let mut res = Vec::new();
        
        // 1. Add tracked routes
        res.extend(self.routes.lock().clone());

        // 2. Add direct routes
        for cidr in iface.ip_addrs() {
            if let IpCidr::Ipv4(v4) = cidr {
                if v4.prefix_len() > 0 {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv4(v4.network()),
                        gateway: None,
                    });
                }
            }
        }
        res
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
    }
    fn get_mtu(&self) -> usize {
        65535
    }
}
