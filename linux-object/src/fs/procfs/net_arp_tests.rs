//! `/proc/net/arp`: the neighbours of IPv4 the stack said of its cards.

use super::*;
use zcore_drivers::scheme::NeighbourInfo;
use zcore_drivers::wire::{Ipv4Address, Ipv6Address};

#[test]
fn the_table_has_a_row_for_each_neighbour_of_ipv4() {
    let heading =
        "IP address       HW type     Flags       HW address            Mask     Device\n";
    // Nobody met: the heading alone.
    assert_eq!(proc_net_arp_of(core::iter::empty()), heading);
    let met = |address, last: u8| NeighbourInfo {
        address,
        hardware: [0x52, 0x55, 0x0a, 0x00, 0x02, last],
        until_ms: 0,
    };
    let cards = alloc::vec![
        (
            String::from("eth0"),
            alloc::vec![
                met(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2)), 2),
                // One of IPv6 is no row of this table.
                met(
                    IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2)),
                    2
                ),
            ],
        ),
        (
            String::from("eth1"),
            alloc::vec![met(
                IpAddress::Ipv4(Ipv4Address::new(192, 168, 100, 254)),
                3
            )],
        ),
    ];
    // Column for column what Linux writes.
    assert_eq!(
        proc_net_arp_of(cards.into_iter()),
        alloc::format!(
            "{heading}\
             10.0.2.2         0x1         0x2         52:55:0a:00:02:02     *        eth0\n\
             192.168.100.254  0x1         0x2         52:55:0a:00:02:03     *        eth1\n"
        )
    );
}
