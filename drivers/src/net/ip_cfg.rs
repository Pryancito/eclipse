//! Address and route bookkeeping shared by the Ethernet drivers.
//!
//! Every NIC driver in this crate answers the same half of `NetScheme`: the
//! `SIOCSIFADDR` / `SIOCADDRT` family, which is pure bookkeeping over the
//! smoltcp `Interface` and the two shadow tables (`ip_addrs`, `routes`) the
//! kernel reports through `/proc` and `ifconfig`. None of it touches a
//! register, so none of it belongs in a driver.
//!
//! Linux keeps this in the netdev core and leaves the driver with the
//! datapath; here it was copied per driver instead, and the copies drifted:
//! the comments in `e1000.rs` record two bugs (a full address list silently
//! losing its newest entry, a non-default gatewayed route purging the default
//! route from the reported table) that were found and fixed in one copy at a
//! time. These are those fixes, once.
//!
//! `pcnet32` is the first user. `e1000`/`e1000e` still carry their own copies;
//! converting them is a change to a working datapath and gets its own batch.

use alloc::vec::Vec;

use managed::ManagedSlice;
use smoltcp::iface::Interface;
use smoltcp::phy::Device;
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr};

use crate::scheme::RouteInfo;
use crate::{DeviceError, DeviceResult};

/// The placeholder a free address slot holds.
///
/// `InterfaceBuilder::ip_addrs` is given a fixed-length list, so "no address"
/// has to be a value rather than an absent entry. Both spellings below have
/// been used as that placeholder, and `add_ip_address` must recognise either
/// or it overwrites a live address instead of filling a hole.
#[inline]
fn slot_is_free(slot: &IpCidr) -> bool {
    (slot.address().is_unspecified() && slot.prefix_len() == 0)
        || (slot.address() == IpAddress::v4(240, 0, 0, 0) && slot.prefix_len() == 32)
}

/// The value written back into a slot that an address is removed from.
#[inline]
fn vacant_slot() -> IpCidr {
    IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0)
}

/// Decide what `set_ipv4_address` does to an address list, without an
/// `Interface` to hold.
///
/// `cidr` becomes the first IPv4 slot and every other IPv4 slot is vacated:
/// one interface, one IPv4 address, which is what `SIOCSIFADDR` means. If the
/// list holds no IPv4 slot at all, the first slot is taken.
pub fn apply_set_ipv4(addrs: &mut [IpCidr], cidr: Ipv4Cidr) {
    let mut set_primary = false;
    for slot in addrs.iter_mut() {
        if let IpCidr::Ipv4(_) = slot {
            if !set_primary {
                *slot = IpCidr::Ipv4(cidr);
                set_primary = true;
            } else {
                *slot = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0));
            }
        }
    }
    if !set_primary {
        if let Some(slot) = addrs.iter_mut().next() {
            *slot = IpCidr::Ipv4(cidr);
        }
    }
}

/// `SIOCSIFADDR`: make `cidr` the interface's IPv4 address.
pub fn set_ipv4_address<D>(
    iface: &mut Interface<'static, D>,
    shadow: &mut Vec<IpCidr>,
    cidr: Ipv4Cidr,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    iface.update_ip_addrs(|addrs| apply_set_ipv4(addrs, cidr));
    *shadow = iface.ip_addrs().to_vec();
    Ok(())
}

/// `SIOCAIFADDR`: add `cidr`, keeping the addresses already there.
///
/// A free slot is reused; with every slot taken the list GROWS rather than
/// overwriting the newest entry, which silently took an address the interface
/// still answered on. Borrowed storage cannot grow, and there the newest slot
/// is still the least bad place to put it.
pub fn apply_add_ip(addrs: &mut ManagedSlice<'static, IpCidr>, cidr: IpCidr) {
    if addrs.contains(&cidr) {
        return;
    }
    for slot in addrs.iter_mut() {
        if slot_is_free(slot) {
            *slot = cidr;
            return;
        }
    }
    if let ManagedSlice::Owned(addrs) = addrs {
        addrs.push(cidr);
    } else if let Some(slot) = addrs.iter_mut().last() {
        *slot = cidr;
    }
}

/// `SIOCAIFADDR` over an `Interface`.
pub fn add_ip_address<D>(
    iface: &mut Interface<'static, D>,
    shadow: &mut Vec<IpCidr>,
    cidr: IpCidr,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    iface.update_ip_addrs(|addrs| apply_add_ip(addrs, cidr));
    *shadow = iface.ip_addrs().to_vec();
    Ok(())
}

/// Vacate the slot holding `cidr`, if any. Only the FIRST match is vacated:
/// the same address twice in the list is a bug upstream, not a reason to clear
/// two slots.
pub fn apply_remove_ip(addrs: &mut [IpCidr], cidr: IpCidr) {
    for slot in addrs.iter_mut() {
        if *slot == cidr {
            *slot = vacant_slot();
            return;
        }
    }
}

/// `SIOCDIFADDR` over an `Interface`.
pub fn remove_ip_address<D>(
    iface: &mut Interface<'static, D>,
    shadow: &mut Vec<IpCidr>,
    cidr: IpCidr,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    iface.update_ip_addrs(|addrs| apply_remove_ip(addrs, cidr));
    *shadow = iface.ip_addrs().to_vec();
    Ok(())
}

/// Does adding a route to `cidr` replace the default route of its family?
///
/// Only a `/0` does. This ran for every gatewayed route once, so the static
/// route in a DHCP lease took `0.0.0.0/0` out of the table the kernel reports
/// while smoltcp went on routing through it.
#[inline]
pub fn replaces_default(cidr: &IpCidr) -> bool {
    cidr.prefix_len() == 0
}

/// Drop the reported default route of `cidr`'s family from `routes`.
pub fn purge_reported_default(routes: &mut Vec<RouteInfo>, cidr: &IpCidr) {
    match cidr {
        IpCidr::Ipv4(_) => {
            routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0))
        }
        IpCidr::Ipv6(_) => {
            routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0))
        }
        _ => {}
    }
}

/// `SIOCADDRT`.
pub fn add_route<D>(
    iface: &mut Interface<'static, D>,
    routes: &mut Vec<RouteInfo>,
    cidr: IpCidr,
    gateway: Option<IpAddress>,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    match gateway {
        Some(IpAddress::Ipv4(gw)) => {
            if replaces_default(&cidr) {
                let _ = iface.routes_mut().remove_default_ipv4_route();
                iface
                    .routes_mut()
                    .add_default_ipv4_route(gw)
                    .map_err(|_| DeviceError::IoError)?;
                purge_reported_default(routes, &cidr);
            }
            routes.push(RouteInfo {
                dst: cidr,
                gateway: Some(IpAddress::Ipv4(gw)),
            });
        }
        Some(IpAddress::Ipv6(gw)) => {
            if replaces_default(&cidr) {
                let _ = iface.routes_mut().remove_default_ipv6_route();
                iface
                    .routes_mut()
                    .add_default_ipv6_route(gw)
                    .map_err(|_| DeviceError::IoError)?;
                purge_reported_default(routes, &cidr);
            }
            routes.push(RouteInfo {
                dst: cidr,
                gateway: Some(IpAddress::Ipv6(gw)),
            });
        }
        None => routes.push(RouteInfo { dst: cidr, gateway }),
        _ => {}
    }
    Ok(())
}

/// `SIOCDELRT`.
pub fn del_route<D>(
    iface: &mut Interface<'static, D>,
    routes: &mut Vec<RouteInfo>,
    cidr: IpCidr,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    if replaces_default(&cidr) {
        match cidr {
            IpCidr::Ipv4(_) => {
                let _ = iface.routes_mut().remove_default_ipv4_route();
            }
            IpCidr::Ipv6(_) => {
                let _ = iface.routes_mut().remove_default_ipv6_route();
            }
            _ => {}
        }
    }
    routes.retain(|r| r.dst != cidr);
    Ok(())
}

/// The on-link route every configured address implies, as `route -n` shows it.
pub fn direct_routes(addrs: &[IpCidr]) -> Vec<RouteInfo> {
    let mut res = Vec::new();
    for cidr in addrs {
        match cidr {
            IpCidr::Ipv4(v4) if v4.prefix_len() > 0 => res.push(RouteInfo {
                dst: IpCidr::Ipv4(v4.network()),
                gateway: None,
            }),
            IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => res.push(RouteInfo {
                dst: IpCidr::Ipv6(v6.network()),
                gateway: None,
            }),
            _ => {}
        }
    }
    res
}

/// The tracked routes followed by the on-link ones: what `get_routes` reports.
pub fn reported_routes(tracked: &[RouteInfo], addrs: &[IpCidr]) -> Vec<RouteInfo> {
    let mut res = tracked.to_vec();
    res.extend(direct_routes(addrs));
    res
}

/// Seed smoltcp's neighbour cache, the `SchemeUpcast`-free part of it.
pub fn seed_neighbor<D>(
    iface: &mut Interface<'static, D>,
    protocol: IpAddress,
    hardware: smoltcp::wire::EthernetAddress,
    timestamp: smoltcp::time::Instant,
) -> DeviceResult
where
    D: for<'d> Device<'d>,
{
    iface.seed_neighbor(protocol, hardware, timestamp);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! These exercise the pure halves of the helpers: the decisions, with no
    //! `Interface` in the way. Each one is a rule a per-driver copy of this
    //! code got wrong at least once.

    use super::*;
    use alloc::vec;
    use smoltcp::wire::{Ipv6Address, Ipv6Cidr};

    fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> IpCidr {
        IpCidr::new(IpAddress::v4(a, b, c, d), prefix)
    }

    fn owned(addrs: Vec<IpCidr>) -> ManagedSlice<'static, IpCidr> {
        ManagedSlice::Owned(addrs)
    }

    #[test]
    fn setting_the_ipv4_address_leaves_exactly_one_ipv4_slot_in_force() {
        // Two live IPv4 addresses after a single `SIOCSIFADDR` is the bug: the
        // interface answers ARP for an address the admin just replaced.
        let mut addrs = vec![
            v4(10, 0, 0, 1, 24),
            v4(192, 168, 1, 5, 24),
            IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::LOOPBACK, 64)),
        ];
        apply_set_ipv4(
            &mut addrs,
            Ipv4Cidr::new(Ipv4Address::new(172, 16, 0, 9), 16),
        );
        assert_eq!(addrs[0], v4(172, 16, 0, 9, 16));
        assert!(
            addrs[1].address().is_unspecified(),
            "the second IPv4 slot must be vacated, not kept"
        );
        assert!(
            matches!(addrs[2], IpCidr::Ipv6(_)),
            "the IPv6 address is none of SIOCSIFADDR's business"
        );
    }

    #[test]
    fn setting_the_ipv4_address_with_no_ipv4_slot_takes_the_first_slot() {
        // A list that is all IPv6 (or all placeholders of the other spelling)
        // would otherwise swallow the address with no error anywhere.
        let mut addrs = vec![IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::LOOPBACK, 64))];
        apply_set_ipv4(&mut addrs, Ipv4Cidr::new(Ipv4Address::new(10, 1, 2, 3), 8));
        assert_eq!(addrs[0], v4(10, 1, 2, 3, 8));
    }

    #[test]
    fn adding_an_address_fills_a_free_slot_and_keeps_the_live_ones() {
        let mut addrs = owned(vec![v4(10, 0, 0, 1, 24), v4(0, 0, 0, 0, 0)]);
        apply_add_ip(&mut addrs, v4(192, 168, 0, 2, 24));
        assert_eq!(addrs[0], v4(10, 0, 0, 1, 24), "the live address stays");
        assert_eq!(addrs[1], v4(192, 168, 0, 2, 24));
    }

    #[test]
    fn the_other_placeholder_spelling_counts_as_a_free_slot_too() {
        // 240.0.0.0/32 is the placeholder some builders use. Not recognising
        // it meant `add_ip_address` walked past a hole and overwrote a live
        // address at the end of the list.
        let mut addrs = owned(vec![v4(240, 0, 0, 0, 32), v4(10, 0, 0, 1, 24)]);
        apply_add_ip(&mut addrs, v4(192, 168, 0, 2, 24));
        assert_eq!(addrs[0], v4(192, 168, 0, 2, 24));
        assert_eq!(addrs[1], v4(10, 0, 0, 1, 24), "untouched");
    }

    #[test]
    fn an_address_already_present_is_not_added_twice() {
        let mut addrs = owned(vec![v4(10, 0, 0, 1, 24), v4(0, 0, 0, 0, 0)]);
        apply_add_ip(&mut addrs, v4(10, 0, 0, 1, 24));
        assert_eq!(addrs[1], v4(0, 0, 0, 0, 0), "the free slot stays free");
    }

    #[test]
    fn a_full_owned_list_grows_rather_than_losing_an_address() {
        // This is the bug the comment in `e1000.rs` records: with every slot
        // taken, the newest entry was overwritten and the interface quietly
        // stopped answering on it.
        let mut addrs = owned(vec![v4(10, 0, 0, 1, 24), v4(10, 0, 0, 2, 24)]);
        apply_add_ip(&mut addrs, v4(10, 0, 0, 3, 24));
        assert_eq!(addrs.len(), 3, "the list must grow");
        assert_eq!(addrs[0], v4(10, 0, 0, 1, 24));
        assert_eq!(addrs[1], v4(10, 0, 0, 2, 24));
        assert_eq!(addrs[2], v4(10, 0, 0, 3, 24));
    }

    #[test]
    fn a_full_borrowed_list_cannot_grow_and_takes_the_last_slot() {
        // Borrowed storage has nowhere to grow into, so the last slot is the
        // best there is -- but the FIRST addresses must still survive.
        let storage: &'static mut [IpCidr] = vec![v4(10, 0, 0, 1, 24), v4(10, 0, 0, 2, 24)].leak();
        let mut addrs = ManagedSlice::Borrowed(storage);
        apply_add_ip(&mut addrs, v4(10, 0, 0, 3, 24));
        assert_eq!(addrs[0], v4(10, 0, 0, 1, 24));
        assert_eq!(addrs[1], v4(10, 0, 0, 3, 24));
    }

    #[test]
    fn removing_an_address_vacates_its_slot_and_only_its_slot() {
        let mut addrs = vec![v4(10, 0, 0, 1, 24), v4(10, 0, 0, 2, 24)];
        apply_remove_ip(&mut addrs, v4(10, 0, 0, 2, 24));
        assert_eq!(addrs[0], v4(10, 0, 0, 1, 24));
        assert_eq!(addrs[1], vacant_slot());

        // An address that is not there changes nothing.
        let before = addrs.clone();
        apply_remove_ip(&mut addrs, v4(172, 16, 0, 1, 16));
        assert_eq!(addrs, before);
    }

    #[test]
    fn only_a_default_route_replaces_the_default_route() {
        assert!(replaces_default(&v4(0, 0, 0, 0, 0)));
        assert!(!replaces_default(&v4(10, 0, 0, 0, 8)));
        assert!(!replaces_default(&v4(10, 0, 0, 1, 32)));
    }

    #[test]
    fn purging_the_reported_default_leaves_the_static_routes_alone() {
        // The DHCP lease case: a static route plus a default route. Purging on
        // every gatewayed route took 0.0.0.0/0 out of the reported table while
        // smoltcp went on routing through it.
        let mut routes = vec![
            RouteInfo {
                dst: v4(0, 0, 0, 0, 0),
                gateway: Some(IpAddress::v4(10, 0, 0, 1)),
            },
            RouteInfo {
                dst: v4(172, 16, 0, 0, 16),
                gateway: Some(IpAddress::v4(10, 0, 0, 254)),
            },
            RouteInfo {
                dst: IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0)),
                gateway: None,
            },
        ];
        purge_reported_default(&mut routes, &v4(0, 0, 0, 0, 0));
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].dst, v4(172, 16, 0, 0, 16));
        assert!(
            matches!(routes[1].dst, IpCidr::Ipv6(_)),
            "the IPv6 default is a different family and must survive"
        );
    }

    #[test]
    fn an_unconfigured_address_implies_no_on_link_route() {
        // A /0 placeholder is not a network, and reporting 0.0.0.0/0 with no
        // gateway as an on-link route makes `route -n` claim the whole
        // internet is on this wire.
        assert!(direct_routes(&[v4(0, 0, 0, 0, 0)]).is_empty());
        let direct = direct_routes(&[v4(192, 168, 7, 33, 24)]);
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].dst, v4(192, 168, 7, 0, 24));
        assert!(direct[0].gateway.is_none());
    }

    #[test]
    fn the_reported_table_is_the_tracked_routes_then_the_on_link_ones() {
        let tracked = vec![RouteInfo {
            dst: v4(0, 0, 0, 0, 0),
            gateway: Some(IpAddress::v4(192, 168, 7, 1)),
        }];
        let reported = reported_routes(&tracked, &[v4(192, 168, 7, 33, 24)]);
        assert_eq!(reported.len(), 2);
        assert_eq!(reported[0].dst, v4(0, 0, 0, 0, 0));
        assert_eq!(reported[1].dst, v4(192, 168, 7, 0, 24));
    }
}
