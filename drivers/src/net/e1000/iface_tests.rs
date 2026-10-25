//! The smoltcp side of the e1000: the probe, the address slots, the route
//! table the kernel reports, and the interrupt hand-off.
//!
//! `hw_tests` next door drives the card itself. What is here is everything
//! above it, which was the larger half of this file and had no tests at all:
//! the EUI-64 the link-local address is derived with, the four address slots
//! and the rules for filling them, and `add_route`'s purge of the default
//! route -- which used to run for every gatewayed route, so a DHCP lease's
//! static route took the default out of the table the kernel reports while
//! smoltcp went on routing through it.
//!
//! No seam is needed: under test `drivers_phys_to_virt` is the identity, so a
//! leaked heap block is BAR0 and `init`, the whole probe path, runs as it is.

use super::*;
extern crate std;

/// One interface, over its own register window, with the virtual clock moving
/// so that the card's reset waits finish.
///
/// `init` is the probe: it stands the card up, derives the MAC and the IPv6
/// link-local address from `index`, leaks this NIC's route storage and builds
/// the smoltcp interface.
fn interface(index: usize) -> (usize, E1000Interface) {
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1_000);
    let regs = Box::leak(vec![0u32; 0x4000].into_boxed_slice());
    let base = regs.as_ptr() as usize;
    let name = alloc::format!("eth{}", index);
    let iface = init(name, IRQ, base, 0x1_0000, index).expect("the probe did not finish");
    (base, iface)
}

/// The interrupt line the helper probes on.
const IRQ: usize = 43;

fn reg(base: usize, r: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + r * 4) as *const u32) }
}

fn poke(base: usize, r: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + r * 4) as *mut u32, val) };
}

/// The two causes the driver unmasks: RXT0 (a frame arrived) and LSC (the link
/// changed). Spelled out rather than taken from `ims_rearm`, which is what is
/// under test.
const IMS_REARMED: u32 = (1 << 7) | (1 << 2);

fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> IpCidr {
    IpCidr::new(IpAddress::v4(a, b, c, d), prefix)
}

/// The destinations of the reported route table, in order.
fn route_dsts(iface: &E1000Interface) -> Vec<IpCidr> {
    iface.get_routes().iter().map(|r| r.dst).collect()
}

// --- what the probe derives

#[test]
fn the_mac_and_the_link_local_address_are_derived_from_the_probe_index() {
    let (_base, iface) = interface(3);
    assert_eq!(
        iface.get_mac(),
        EthernetAddress([0x54, 0x51, 0x9F, 0x71, 0xC0, 0x03]),
        "the MAC does not end in the probe index"
    );
    // The IPv6 link-local address is the MAC turned into an EUI-64: bit 1 of
    // the first byte is flipped (0x54 -> 0x56), and `ff:fe` is inserted in the
    // middle. Fourteen lines of shifting with nothing checking them, and a
    // wrong one means two NICs can answer on the same link-local address.
    let expected = Ipv6Address::new(0xfe80, 0, 0, 0, 0x5651, 0x9fff, 0xfe71, 0xc003);
    assert!(
        iface
            .get_ip_address()
            .contains(&IpCidr::Ipv6(Ipv6Cidr::new(expected, 64))),
        "the link-local address is not the EUI-64 of the MAC: {:?}",
        iface.get_ip_address()
    );
    // And the four slots the probe leaves: one link-local and three blanks.
    let addrs = iface.get_ip_address();
    assert_eq!(addrs.len(), 4, "the probe did not leave four slots");
    assert_eq!(
        addrs.iter().filter(|c| **c == v4(0, 0, 0, 0, 0)).count(),
        3,
        "the blank slots are not three: {:?}",
        addrs
    );
}

#[test]
fn two_probes_do_not_share_a_mac_a_link_local_address_or_a_route_table() {
    let (_b0, zero) = interface(0);
    let (_b1, one) = interface(1);
    assert_ne!(zero.get_mac(), one.get_mac());
    assert_ne!(
        zero.get_ip_address(),
        one.get_ip_address(),
        "two NICs came up on the same addresses"
    );
    // The route storage is leaked per probe for this reason: a shared
    // `static mut` would alias and both tables would carry both routes.
    zero.add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    assert_eq!(route_dsts(&one).len(), 1, "only its own direct route");
    assert!(
        route_dsts(&one).iter().all(|d| d.prefix_len() != 0),
        "the other interface's default route reached this table: {:?}",
        route_dsts(&one)
    );
}

#[test]
fn the_name_the_probe_was_given_is_the_one_it_answers_with() {
    let (_base, iface) = interface(7);
    assert_eq!(iface.ifname(), "eth7");
    assert_eq!(iface.get_ifname(), "eth7");
    assert_eq!(
        Scheme::name(&iface),
        "e1000",
        "the driver name, not the link"
    );
    assert!(!iface.is_loopback(), "a real NIC is not the loopback");
    assert_eq!(iface.get_mtu(), 1500, "the ethernet MTU");
}

// --- the address slots

#[test]
fn setting_the_primary_address_keeps_one_and_blanks_the_other_v4_slots() {
    let (_base, iface) = interface(0);
    iface
        .set_ipv4_address(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24))
        .unwrap();
    let addrs = iface.get_ip_address();
    assert_eq!(addrs.len(), 4, "a slot was added or lost");
    assert_eq!(addrs[0], v4(10, 0, 2, 15, 24), "the first slot takes it");
    // The remaining v4 slots are blanked rather than left as they were, so the
    // interface answers on exactly one v4 address.
    assert_eq!(
        addrs
            .iter()
            .filter(|c| matches!(c, IpCidr::Ipv4(_)) && !c.address().is_unspecified())
            .count(),
        1,
        "more than one v4 address is in force: {:?}",
        addrs
    );
    // And the link-local address is not a v4 slot, so it survives.
    assert!(
        addrs.iter().any(|c| matches!(c, IpCidr::Ipv6(_))),
        "the IPv6 link-local address was blanked too: {:?}",
        addrs
    );
}

#[test]
fn an_address_goes_into_a_blank_slot_and_is_not_added_twice() {
    let (_base, iface) = interface(0);
    let before = iface.get_ip_address().len();
    iface.add_ip_address(v4(192, 168, 1, 5, 24)).unwrap();
    assert_eq!(
        iface.get_ip_address().len(),
        before,
        "a blank slot was not reused"
    );
    assert!(iface.get_ip_address().contains(&v4(192, 168, 1, 5, 24)));
    iface.add_ip_address(v4(192, 168, 1, 5, 24)).unwrap();
    assert_eq!(
        iface
            .get_ip_address()
            .iter()
            .filter(|c| **c == v4(192, 168, 1, 5, 24))
            .count(),
        1,
        "the same address went in twice"
    );
}

#[test]
fn the_placeholder_address_counts_as_a_blank_slot() {
    let (_base, iface) = interface(0);
    // 240.0.0.0/32 is the other value the probe path leaves behind as "not an
    // address", so a real one has to be allowed to take its slot. Without that
    // arm the list grows instead and the placeholder stays in the table the
    // kernel reports.
    iface.add_ip_address(v4(240, 0, 0, 0, 32)).unwrap();
    let with_placeholder = iface.get_ip_address().len();
    iface.add_ip_address(v4(10, 1, 1, 1, 24)).unwrap();
    assert_eq!(
        iface.get_ip_address().len(),
        with_placeholder,
        "the placeholder's slot was not reused"
    );
    assert!(
        !iface.get_ip_address().contains(&v4(240, 0, 0, 0, 32)),
        "the placeholder is still in the table: {:?}",
        iface.get_ip_address()
    );
}

#[test]
fn with_every_slot_taken_the_list_grows_rather_than_losing_an_address() {
    let (_base, iface) = interface(0);
    // Three blanks and the link-local address, so three go in without growing.
    let wanted = [
        v4(10, 0, 0, 1, 24),
        v4(10, 0, 1, 1, 24),
        v4(10, 0, 2, 1, 24),
    ];
    for cidr in wanted {
        iface.add_ip_address(cidr).unwrap();
    }
    assert_eq!(iface.get_ip_address().len(), 4, "a slot was wasted");
    // The fourth has nowhere to go. Overwriting the newest slot silently takes
    // an address the interface still answers on, so the list grows instead.
    iface.add_ip_address(v4(10, 0, 3, 1, 24)).unwrap();
    let addrs = iface.get_ip_address();
    assert_eq!(addrs.len(), 5, "the list did not grow");
    for cidr in wanted {
        assert!(addrs.contains(&cidr), "{:?} was dropped: {:?}", cidr, addrs);
    }
    assert!(addrs.contains(&v4(10, 0, 3, 1, 24)));
}

#[test]
fn removing_an_address_blanks_its_slot_and_leaves_the_others_alone() {
    let (_base, iface) = interface(0);
    iface.add_ip_address(v4(10, 0, 0, 1, 24)).unwrap();
    iface.add_ip_address(v4(10, 0, 1, 1, 24)).unwrap();
    iface.remove_ip_address(v4(10, 0, 0, 1, 24)).unwrap();
    let addrs = iface.get_ip_address();
    assert_eq!(addrs.len(), 4, "the slot was removed instead of blanked");
    assert!(!addrs.contains(&v4(10, 0, 0, 1, 24)));
    assert!(
        addrs.contains(&v4(10, 0, 1, 1, 24)),
        "the other address went with it: {:?}",
        addrs
    );
    // Removing something that is not there changes nothing.
    iface.remove_ip_address(v4(172, 16, 0, 1, 16)).unwrap();
    assert_eq!(iface.get_ip_address(), addrs);
}

// --- the route table the kernel reports

#[test]
fn a_new_default_route_replaces_the_one_before_it() {
    let (_base, iface) = interface(0);
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 3)))
        .unwrap();
    let defaults: Vec<_> = iface
        .get_routes()
        .into_iter()
        .filter(|r| matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0)
        .collect();
    assert_eq!(defaults.len(), 1, "two default routes are in force");
    assert_eq!(
        defaults[0].gateway,
        Some(IpAddress::v4(10, 0, 2, 3)),
        "the default route kept the old gateway"
    );
}

#[test]
fn a_gatewayed_route_to_a_named_network_does_not_purge_the_default_one() {
    let (_base, iface) = interface(0);
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    // This is the shape of a DHCP lease: a default route, then a static route
    // through a gateway. The purge used to run for every gatewayed route, so
    // this second call took 0.0.0.0/0 out of the table the kernel reports
    // while smoltcp went on routing through it.
    iface
        .add_route(v4(172, 16, 0, 0, 12), Some(IpAddress::v4(10, 0, 2, 9)))
        .unwrap();
    let dsts = route_dsts(&iface);
    assert!(
        dsts.contains(&v4(0, 0, 0, 0, 0)),
        "the default route was purged by a route to a named network: {:?}",
        dsts
    );
    assert!(dsts.contains(&v4(172, 16, 0, 0, 12)));
}

#[test]
fn a_v6_default_route_does_not_take_the_v4_one_with_it() {
    let (_base, iface) = interface(0);
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    // The purge of each family is keyed on the family, so a v6 default route
    // replaces only a v6 one.
    let gw6 = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    iface
        .add_route(
            IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0)),
            Some(IpAddress::Ipv6(gw6)),
        )
        .unwrap();
    let dsts = route_dsts(&iface);
    assert!(
        dsts.contains(&v4(0, 0, 0, 0, 0)),
        "the v6 default route purged the v4 one: {:?}",
        dsts
    );
    assert!(dsts
        .iter()
        .any(|d| matches!(d, IpCidr::Ipv6(_)) && d.prefix_len() == 0));
}

#[test]
fn a_route_with_no_gateway_is_tracked_and_purges_nothing() {
    let (_base, iface) = interface(0);
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    iface.add_route(v4(192, 168, 9, 0, 24), None).unwrap();
    let routes = iface.get_routes();
    let on_link = routes
        .iter()
        .find(|r| r.dst == v4(192, 168, 9, 0, 24))
        .expect("the on-link route was not tracked");
    assert_eq!(on_link.gateway, None, "a gateway was invented for it");
    assert!(
        route_dsts(&iface).contains(&v4(0, 0, 0, 0, 0)),
        "an on-link route purged the default one"
    );
}

#[test]
fn deleting_a_route_takes_that_one_and_only_that_one() {
    let (_base, iface) = interface(0);
    iface
        .add_route(v4(0, 0, 0, 0, 0), Some(IpAddress::v4(10, 0, 2, 2)))
        .unwrap();
    iface.add_route(v4(10, 0, 0, 0, 8), None).unwrap();
    iface.del_route(v4(10, 0, 0, 0, 8), None).unwrap();
    let dsts = route_dsts(&iface);
    assert!(!dsts.contains(&v4(10, 0, 0, 0, 8)));
    assert!(
        dsts.contains(&v4(0, 0, 0, 0, 0)),
        "the default route went with it: {:?}",
        dsts
    );
    // And deleting the default route leaves nothing tracked behind it.
    iface.del_route(v4(0, 0, 0, 0, 0), None).unwrap();
    assert!(
        !route_dsts(&iface).contains(&v4(0, 0, 0, 0, 0)),
        "the default route is still reported after being deleted"
    );
}

#[test]
fn the_reported_table_carries_the_direct_network_of_every_address() {
    let (_base, iface) = interface(0);
    iface.add_ip_address(v4(192, 168, 1, 5, 24)).unwrap();
    let dsts = route_dsts(&iface);
    // The network, not the address: a route to 192.168.1.5/24 would be a host
    // route written as a network one.
    assert!(
        dsts.contains(&v4(192, 168, 1, 0, 24)),
        "the direct network of the address is missing: {:?}",
        dsts
    );
    assert!(
        !dsts.contains(&v4(192, 168, 1, 5, 24)),
        "the address was reported instead of its network: {:?}",
        dsts
    );
    // A blank slot is not a route to everything.
    assert!(
        !dsts
            .iter()
            .any(|d| matches!(d, IpCidr::Ipv4(_)) && d.prefix_len() == 0),
        "a blank address slot became a default route: {:?}",
        dsts
    );
    // The link-local address brings its own /64.
    assert!(
        dsts.iter()
            .any(|d| matches!(d, IpCidr::Ipv6(_)) && d.prefix_len() == 64),
        "the link-local network is missing: {:?}",
        dsts
    );
}

// --- the interrupt hand-off

#[test]
fn an_interrupt_on_another_line_is_not_this_interfaces() {
    let (base, iface) = interface(0);
    poke(base, E1000_ICR, 0x1234);
    poke(base, E1000_IMS, 0);
    iface.handle_irq(IRQ + 1);
    assert_eq!(
        reg(base, E1000_ICR),
        0x1234,
        "it read-cleared the cause register of a line that is not its own"
    );
    assert_eq!(reg(base, E1000_IMS), 0, "it touched the mask");
    assert_eq!(crate::utils::deferred_job::pending_deferred_jobs(), 0);
}

#[test]
fn an_interrupt_with_no_cause_only_puts_the_mask_back() {
    let (base, iface) = interface(0);
    poke(base, E1000_ICR, 0);
    poke(base, E1000_IMS, 0);
    let pending = crate::utils::deferred_job::pending_deferred_jobs();
    iface.handle_irq(IRQ);
    assert_eq!(
        reg(base, E1000_IMS),
        IMS_REARMED,
        "a spurious interrupt did not leave the two causes unmasked"
    );
    assert_eq!(
        crate::utils::deferred_job::pending_deferred_jobs(),
        pending,
        "a poll was queued for an interrupt with no cause"
    );
}

#[test]
fn an_interrupt_with_a_cause_masks_the_card_and_queues_one_poll() {
    let (base, iface) = interface(0);
    poke(base, E1000_ICR, 1 << 7);
    poke(base, E1000_IMS, 0);
    poke(base, E1000_IMC, 0);
    let pending = crate::utils::deferred_job::pending_deferred_jobs();
    iface.handle_irq(IRQ);
    assert_eq!(
        reg(base, E1000_IMC),
        0xffff_ffff,
        "the card was left unmasked with a poll outstanding"
    );
    assert_eq!(
        crate::utils::deferred_job::pending_deferred_jobs(),
        pending + 1,
        "the bottom half was not queued"
    );
    // A second interrupt while the first poll is still outstanding queues
    // nothing more and leaves the card masked: re-arming here would let later
    // edges clear causes the queued poll never sees.
    poke(base, E1000_ICR, 1 << 7);
    iface.handle_irq(IRQ);
    assert_eq!(
        crate::utils::deferred_job::pending_deferred_jobs(),
        pending + 1,
        "a second poll was queued on top of the first"
    );
    assert_eq!(reg(base, E1000_IMS), 0, "the mask was put back too early");
}

#[test]
fn the_queued_poll_puts_the_mask_back_even_if_it_never_runs() {
    let (base, iface) = interface(0);
    poke(base, E1000_ICR, 1 << 7);
    poke(base, E1000_IMS, 0);
    iface.handle_irq(IRQ);
    // The guard is built outside the closure and moved in, so dropping the job
    // unexecuted clears the flag and re-arms all the same. Leaving that to the
    // closure's body meant an evicted bottom half left the NIC masked for the
    // life of the kernel.
    crate::utils::deferred_job::drain_deferred_jobs();
    assert_eq!(
        reg(base, E1000_IMS),
        IMS_REARMED,
        "the bottom half ran and the card is still masked"
    );
    // And the flag is clear, so the next interrupt queues a fresh poll.
    poke(base, E1000_ICR, 1 << 7);
    let pending = crate::utils::deferred_job::pending_deferred_jobs();
    iface.handle_irq(IRQ);
    assert_eq!(
        crate::utils::deferred_job::pending_deferred_jobs(),
        pending + 1,
        "poll_pending was left true, so the interface is stuck on the periodic path"
    );
    crate::utils::deferred_job::drain_deferred_jobs();
}

#[test]
fn the_periodic_poll_puts_the_mask_back_itself() {
    let (base, iface) = interface(0);
    poke(base, E1000_IMS, 0);
    // The IRQ path re-arms from the guard; this one has no guard, so it has to
    // do it on the way out or the card never speaks again.
    iface.poll().expect("the periodic poll failed");
    assert_eq!(reg(base, E1000_IMS), IMS_REARMED);
}

// --- what goes out and what comes in

#[test]
fn a_frame_that_cannot_fit_a_buffer_is_refused_rather_than_clipped() {
    let (_base, iface) = interface(0);
    // The caller is never told that a corrupt, truncated packet went out in
    // full, so the length is checked before the ring is touched.
    assert_eq!(
        iface.send(&[0u8; RX_BUF_SIZE + 1]),
        Err(DeviceError::InvalidParam)
    );
    assert_eq!(iface.send(&[]), Err(DeviceError::InvalidParam));
    // And one that does fit goes, with its whole length reported.
    let frame = [0xabu8; 64];
    assert_eq!(iface.send(&frame), Ok(frame.len()));
}

#[test]
fn an_empty_ring_is_not_ready_rather_than_an_empty_frame() {
    let (_base, iface) = interface(0);
    let mut buf = [0xffu8; 64];
    assert_eq!(iface.recv(&mut buf), Err(DeviceError::NotReady));
    assert_eq!(
        buf, [0xffu8; 64],
        "it wrote into the caller's buffer anyway"
    );
    assert!(iface.can_recv(), "callers always get to try a receive");
    assert!(iface.can_send(), "a fresh ring has room");
}

#[test]
fn the_statistics_handed_out_are_a_copy_and_not_the_counters_themselves() {
    let (_base, iface) = interface(0);
    let before = iface.get_stats();
    iface.stats.lock().tx_bytes += 1234;
    assert_eq!(
        iface.get_stats().tx_bytes,
        before.tx_bytes + 1234,
        "the counters handed out are not the driver's own"
    );
    let mut snapshot = iface.get_stats();
    snapshot.tx_bytes = 0;
    assert_ne!(
        iface.get_stats().tx_bytes,
        0,
        "writing to the snapshot reached the counters"
    );
}

// --- what smoltcp is told about the card

#[test]
fn smoltcp_is_told_the_whole_frame_and_the_kernel_the_payload() {
    let (_base, iface) = interface(0);
    let caps = phy::Device::capabilities(&iface.driver);
    // The two MTUs of this driver are not the same number and must not be made
    // to agree: smoltcp's `max_transmission_unit` is the whole ethernet frame,
    // header included, while the `get_mtu` userspace reads is the payload.
    assert_eq!(caps.max_transmission_unit, 1514, "frame, not payload");
    assert_eq!(iface.get_mtu(), 1500, "payload, not frame");
    assert_eq!(
        caps.max_transmission_unit - iface.get_mtu(),
        14,
        "the gap between them is the ethernet header"
    );
    // A burst limit must stay unset: smoltcp clamps the TCP window to
    // `burst * MSS` in a u16, so burst=64 wraps 94336 to 28800 and the link
    // tops out at a few Mbps.
    assert_eq!(
        caps.max_burst_size, None,
        "a burst limit wraps the TCP window and caps the link at a few Mbps"
    );
}

#[test]
fn the_pci_driver_claims_the_two_device_ids_qemu_offers_and_no_other() {
    let driver = E1000DriverPci;
    assert_eq!(PciDriver::name(&driver), "e1000");
    assert!(
        driver.matched(0x8086, 0x100e),
        "the 82540EM QEMU defaults to"
    );
    assert!(driver.matched(0x8086, 0x100f), "the 82545EM");
    assert!(
        !driver.matched(0x8086, 0x10d3),
        "that is the e1000e, which has its own driver"
    );
    assert!(
        !driver.matched(0x10ec, 0x100e),
        "the device id was matched without the vendor"
    );
}
