use super::Scheme;
use crate::{DeviceError, DeviceResult};
use alloc::string::String;
use alloc::vec::Vec;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Cidr};

#[derive(Debug, Clone, Default)]
pub struct NetStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    /// RX errors / drops (driver-specific; shown in /proc/net/dev).
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub dst: IpCidr,
    pub gateway: Option<smoltcp::wire::IpAddress>,
}

pub trait NetScheme: Scheme {
    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize>;
    fn send(&self, buf: &[u8]) -> DeviceResult<usize>;
    fn can_recv(&self) -> bool {
        true
    }
    fn can_send(&self) -> bool {
        true
    }
    fn get_mac(&self) -> EthernetAddress;
    fn get_ifname(&self) -> String;
    fn get_ip_address(&self) -> Vec<IpCidr>;
    fn set_ipv4_address(&self, _cidr: Ipv4Cidr) -> DeviceResult {
        Err(DeviceError::NotSupported)
    }
    fn add_ip_address(&self, _cidr: IpCidr) -> DeviceResult {
        Err(DeviceError::NotSupported)
    }
    fn remove_ip_address(&self, _cidr: IpCidr) -> DeviceResult {
        Err(DeviceError::NotSupported)
    }
    fn add_route(&self, _cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        Err(DeviceError::NotSupported)
    }
    fn del_route(&self, _cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        Err(DeviceError::NotSupported)
    }
    fn get_routes(&self) -> Vec<RouteInfo> {
        Vec::new()
    }
    fn get_stats(&self) -> NetStats {
        NetStats::default()
    }
    fn get_mtu(&self) -> usize {
        1500
    }
    fn get_arp_content(&self) -> String {
        String::new()
    }
    /// Import a resolved IPv4/IPv6 neighbor into smoltcp's cache (for TCP/UDP egress).
    fn seed_neighbor(
        &self,
        _protocol: smoltcp::wire::IpAddress,
        _hardware: EthernetAddress,
    ) -> DeviceResult {
        Ok(())
    }
    fn poll(&self) -> DeviceResult;
    /// SIOCSIFFLAGS / admin up — drivers may re-probe link (default no-op).
    fn refresh_link(&self) -> DeviceResult {
        Ok(())
    }
    /// Physical carrier up (for SIOCGIFFLAGS IFF_LOWER_UP / RUNNING).
    fn link_carrier_up(&self) -> bool {
        true
    }
    /// SIOCSIFFLAGS admin-down — quiesce datapath without tearing down the iface.
    fn admin_down(&self) -> DeviceResult {
        Ok(())
    }
    /// IFF_PROMISC — unicast (+ typically multicast) promiscuous receive.
    fn set_promiscuous(&self, _on: bool) -> DeviceResult {
        Ok(())
    }
    /// IFF_ALLMULTI — accept all multicast without programming the MTA.
    fn set_allmulti(&self, _on: bool) -> DeviceResult {
        Ok(())
    }
    /// Replace the hardware multicast filter list (ignored when allmulti/promisc).
    fn set_multicast_list(&self, _addrs: &[[u8; 6]]) -> DeviceResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::{IpAddress, Ipv4Address};

    /// A NIC that implements only what `NetScheme` requires, so every other
    /// answer a test reads is the trait's own default.
    struct BareNic;

    impl Scheme for BareNic {
        fn name(&self) -> &str {
            "bare-nic"
        }
    }

    impl NetScheme for BareNic {
        fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
            Ok(0)
        }
        fn send(&self, _buf: &[u8]) -> DeviceResult<usize> {
            Ok(0)
        }
        fn get_mac(&self) -> EthernetAddress {
            EthernetAddress([0x02, 0, 0, 0, 0, 1])
        }
        fn get_ifname(&self) -> String {
            String::from("bare0")
        }
        fn get_ip_address(&self) -> Vec<IpCidr> {
            Vec::new()
        }
        fn poll(&self) -> DeviceResult {
            Ok(())
        }
    }

    fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> IpCidr {
        IpCidr::new(IpAddress::Ipv4(Ipv4Address::new(a, b, c, d)), prefix)
    }

    /// Reconfiguring the addresses or routes of a NIC that does not implement
    /// it must FAIL, not silently succeed: `ifconfig` reporting success on an
    /// address the device never took is worse than an error, because the
    /// routing table and the hardware then disagree with no way to tell.
    #[test]
    fn address_and_route_changes_are_refused_by_default() {
        let nic = BareNic;
        assert_eq!(
            nic.set_ipv4_address(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24)),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(
            nic.add_ip_address(v4(10, 0, 0, 3, 24)),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(
            nic.remove_ip_address(v4(10, 0, 0, 3, 24)),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(
            nic.add_route(v4(0, 0, 0, 0, 0), None),
            Err(DeviceError::NotSupported)
        );
        assert_eq!(
            nic.del_route(v4(0, 0, 0, 0, 0), None),
            Err(DeviceError::NotSupported)
        );
    }

    /// The read-only queries, by contrast, answer emptily rather than fail:
    /// `/proc/net/route` and `/proc/net/arp` are read on a healthy system and
    /// an error there would be reported as a broken procfs.
    #[test]
    fn the_read_only_queries_answer_empty_by_default() {
        let nic = BareNic;
        assert!(nic.get_routes().is_empty());
        assert!(nic.get_arp_content().is_empty());
    }

    /// 1500 is Ethernet's own MTU, which is the only safe guess: a default
    /// larger than the link would make every full-size frame fail to go out.
    #[test]
    fn the_default_mtu_is_the_ethernet_one() {
        assert_eq!(BareNic.get_mtu(), 1500);
    }

    /// A driver that counts nothing must report zeros, not stale or random
    /// numbers; `/proc/net/dev` is read by userspace tools that diff it.
    #[test]
    fn the_default_statistics_are_all_zero() {
        let s = BareNic.get_stats();
        assert_eq!(
            (s.rx_bytes, s.rx_packets, s.tx_bytes, s.tx_packets),
            (0, 0, 0, 0)
        );
        assert_eq!(
            (s.rx_errors, s.rx_dropped, s.tx_errors, s.tx_dropped),
            (0, 0, 0, 0)
        );
    }

    /// A device with no queue bookkeeping must say it can do both: claiming
    /// it cannot receive would park the stack on a NIC that works.
    #[test]
    fn a_device_without_queue_bookkeeping_claims_it_can_do_both() {
        assert!(BareNic.can_recv());
        assert!(BareNic.can_send());
    }

    /// Carrier is assumed up by default, because a driver that does not read
    /// the PHY cannot tell -- and reporting the link down would stop DHCP on
    /// a cable that is plugged in.
    #[test]
    fn carrier_is_assumed_up_when_the_driver_cannot_tell() {
        assert!(BareNic.link_carrier_up());
    }

    /// The flag/filter setters are no-ops that SUCCEED: a NIC with no
    /// hardware filter receives everything anyway, so promiscuous mode is
    /// already in effect and an error would fail `tcpdump` for nothing.
    #[test]
    fn the_filter_and_link_knobs_succeed_as_no_ops() {
        let nic = BareNic;
        assert_eq!(nic.refresh_link(), Ok(()));
        assert_eq!(nic.admin_down(), Ok(()));
        assert_eq!(nic.set_promiscuous(true), Ok(()));
        assert_eq!(nic.set_allmulti(true), Ok(()));
        assert_eq!(nic.set_multicast_list(&[[1, 0, 0x5e, 0, 0, 1]]), Ok(()));
        assert_eq!(
            nic.seed_neighbor(
                IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1)),
                EthernetAddress([0x02, 0, 0, 0, 0, 2])
            ),
            Ok(())
        );
    }

    /// `NetStats` is handed around as `Default` in the default `get_stats`;
    /// the derive must stay all-zero.
    #[test]
    fn default_netstats_is_all_zero() {
        let s = NetStats::default();
        assert_eq!(s.rx_bytes + s.rx_packets + s.tx_bytes + s.tx_packets, 0);
        assert_eq!(
            s.rx_errors + s.rx_dropped + s.tx_errors + s.tx_dropped,
            0
        );
    }

    /// A route with no gateway is on-link, which is a different thing from a
    /// route through 0.0.0.0; the `Option` must survive a clone.
    #[test]
    fn a_route_without_a_gateway_stays_without_one() {
        let r = RouteInfo {
            dst: v4(192, 168, 1, 0, 24),
            gateway: None,
        };
        let c = r.clone();
        assert!(c.gateway.is_none());
        assert_eq!(c.dst, v4(192, 168, 1, 0, 24));
    }
}
