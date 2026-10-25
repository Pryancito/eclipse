//! The three PCI device-id tables of the e1000e, the trait method that wires
//! them to a vendor id, and the receive-mode calls the kernel drives the
//! filter with.
//!
//! The tables decide which branches `reset_and_init` takes: a PCH part needs
//! the ULP exit, the LANPHYPC toggle and the queue-enable dance, and a Sunrise
//! Point part needs RXDCTL.QUEUE_ENABLE cleared before CTRL.RST and reads its
//! MAC out of the flash at BAR0+0xE000 instead of the EEPROM. An id in the
//! wrong table is a card that either never comes up or comes up at 10 Mbit/s,
//! and nothing here is reachable from the probe on this host, so the tables
//! were only ever exercised with the three ids the other suites happen to use.
//! A mutation lot confirmed it: swapping one id for another survived.
//!
//! So the tables are stated a second time here, independently, as ranges with
//! the family each one belongs to, and compared against the whole 16-bit id
//! space. Two spellings of one table is the point: a change to either side
//! fails.
//!
//! The receive-mode calls are the other half. Each of the three takes one flag
//! from the caller and the other from what the card already has, and that
//! pairing had no test either -- swapping the two arguments survived the lot
//! too.

use super::rx_ring_tests::make_hw;
use super::scheme_tests::interface;
use super::*;
extern crate std;

/// Every device id the PCH table claims, by family. Written out here from the
/// Intel and Linux names rather than taken from the driver, so that the two
/// spellings have to agree.
const PCH: &[(u16, u16)] = &[
    (0x1502, 0x1503), // 82579, pch2lan
    (0x153a, 0x153b), // I217-LM / I217-V
    (0x1559, 0x155a), // I218-V / I218-LM
    (0x15a0, 0x15a3), // I218 on Lynx Point and Wildcat Point
    (0x156f, 0x1570), // I219 on Sunrise Point
    (0x15b7, 0x15be), // I219 on Sunrise Point-H and Cannon Point
    (0x15d6, 0x15d8), // I219 on Sunrise Point, later steppings
    (0x15e3, 0x15e3), // I219-LM, the odd one out of that family
    (0x15df, 0x15e2), // I219 on Ice Lake Point
    (0x0d4c, 0x0d4f), // I219 on Comet Lake Point
    (0x0d53, 0x0d53), // I219-LM on Comet Lake Point
    (0x0d55, 0x0d55), // I219-V on Comet Lake Point
    (0x15f4, 0x15fc), // I219 on Tiger Lake Point
    (0x1a1c, 0x1a1f), // I219 on Alder Lake Point
    (0x0dc5, 0x0dc8), // I219 on Raptor Lake Point
    (0x550a, 0x5511), // I219 on Meteor Lake, Lunar Lake and Alder Lake-N
    (0x57a0, 0x57a1), // I219 on Arrow Lake
    (0x57b3, 0x57ba), // I219 on Panther Lake and Nova Lake
];

/// The four families that are PCH but older than Sunrise Point: they take the
/// PCH branches of the reset and none of the Sunrise Point ones.
const BEFORE_SPT: &[(u16, u16)] = &[
    (0x1502, 0x1503),
    (0x153a, 0x153b),
    (0x1559, 0x155a),
    (0x15a0, 0x15a3),
];

/// The discrete parts: 82574L, 82574LA and 82583V. These are what QEMU
/// offers, and they have no PCH machinery at all.
const DISCRETE: &[u16] = &[0x10d3, 0x10f6, 0x150c];

fn covers(table: &[(u16, u16)], id: u16) -> bool {
    table.iter().any(|(lo, hi)| id >= *lo && id <= *hi)
}

/// Every id in the 16-bit space on which `predicate` and `expected` disagree.
fn disagreements(
    predicate: impl Fn(u16) -> bool,
    expected: impl Fn(u16) -> bool,
) -> alloc::vec::Vec<u16> {
    (0..=u16::MAX)
        .filter(|id| predicate(*id) != expected(*id))
        .collect()
}

#[test]
fn the_pch_table_claims_exactly_the_families_it_is_meant_to() {
    let wrong = disagreements(e1000e_is_pch, |id| covers(PCH, id));
    assert!(
        wrong.is_empty(),
        "the PCH table and the families listed in the test disagree on {} ids, first {:#x?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
}

#[test]
fn the_sunrise_point_table_is_the_pch_table_without_the_four_older_families() {
    let wrong = disagreements(e1000e_is_pch_spt_or_later, |id| {
        covers(PCH, id) && !covers(BEFORE_SPT, id)
    });
    assert!(
        wrong.is_empty(),
        "the Sunrise Point table is not the PCH table minus the older families; {} ids differ, first {:#x?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
}

/// The invariant between the two, stated on its own because breaking it is
/// what makes `reset_and_init` incoherent: the Sunrise Point steps are nested
/// inside the PCH ones, so an id in the later table but not the earlier one
/// would clear RXDCTL.QUEUE_ENABLE and then skip the ULP exit it needs.
#[test]
fn nothing_is_sunrise_point_or_later_without_being_a_pch_part() {
    let outside: alloc::vec::Vec<u16> = (0..=u16::MAX)
        .filter(|id| e1000e_is_pch_spt_or_later(*id) && !e1000e_is_pch(*id))
        .collect();
    assert!(
        outside.is_empty(),
        "these ids take the Sunrise Point branches without the PCH ones: {:#x?}",
        &outside[..outside.len().min(8)]
    );
}

#[test]
fn the_discrete_parts_are_claimed_and_share_nothing_with_the_pch_families() {
    let wrong = disagreements(e1000e_is_discrete, |id| DISCRETE.contains(&id));
    assert!(
        wrong.is_empty(),
        "the discrete list differs on {:#x?}",
        wrong
    );
    for id in DISCRETE {
        assert!(
            !e1000e_is_pch(*id),
            "{:#x} is discrete and PCH at once, so the reset would run both paths",
            id
        );
    }
}

#[test]
fn the_card_the_driver_claims_is_a_discrete_or_a_pch_part_and_nothing_else() {
    let wrong = disagreements(e1000e_device_matched, |id| {
        DISCRETE.contains(&id) || covers(PCH, id)
    });
    assert!(
        wrong.is_empty(),
        "the driver claims {} ids it should not, or misses some, first {:#x?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
}

/// The gates themselves are pure functions and now pinned; what had no test
/// was the trait method that reaches them, which is also the only place the
/// vendor id is looked at. Without it the driver would claim a Realtek or a
/// Broadcom card that happens to share a device number.
#[test]
fn a_card_is_only_claimed_under_intels_vendor_id() {
    let driver = E1000eDriverPci;
    assert!(
        driver.matched(0x8086, 0x10d3),
        "an 82574L under Intel's vendor id was not claimed"
    );
    assert!(
        driver.matched(0x8086, 0x15b8),
        "an I219-V under Intel's vendor id was not claimed"
    );
    for vendor in [0x10ecu16, 0x14e4, 0x1af4, 0x0000, 0xffff] {
        assert!(
            !driver.matched(vendor, 0x10d3),
            "a card of vendor {:#x} was claimed because its device id matches an Intel part",
            vendor
        );
    }
    assert!(
        !driver.matched(0x8086, 0x1533),
        "an I210, which belongs to igb, was claimed"
    );
    assert_eq!(driver.name(), "e1000e");
}

// --- the receive filter

/// Each of the three mode calls takes one flag from the caller and reads the
/// other off the card. The two must not change places: asking for promiscuous
/// off on a card that is in all-multicast would turn the multicast filter off
/// and promiscuous on, which is the opposite of what was asked twice over.
#[test]
fn turning_promiscuous_off_leaves_all_multicast_as_it_was() {
    let iface = interface();
    {
        let mut hw = iface.driver.hw.lock();
        hw.rx_promisc = true;
        hw.rx_allmulti = true;
    }
    iface.set_promiscuous(false).expect("the call failed");
    let hw = iface.driver.hw.lock();
    assert!(!hw.rx_promisc, "promiscuous was not turned off");
    assert!(
        hw.rx_allmulti,
        "all-multicast was turned off by a call about promiscuous"
    );
}

#[test]
fn turning_all_multicast_on_leaves_promiscuous_as_it_was() {
    let iface = interface();
    {
        let mut hw = iface.driver.hw.lock();
        hw.rx_promisc = false;
        hw.rx_allmulti = false;
    }
    iface.set_allmulti(true).expect("the call failed");
    let hw = iface.driver.hw.lock();
    assert!(hw.rx_allmulti, "all-multicast was not turned on");
    assert!(
        !hw.rx_promisc,
        "promiscuous was turned on by a call about all-multicast"
    );
}

#[test]
fn a_new_multicast_list_replaces_the_old_one_and_moves_neither_flag() {
    let iface = interface();
    {
        let mut hw = iface.driver.hw.lock();
        hw.rx_promisc = false;
        hw.rx_allmulti = true;
        hw.mc_list = alloc::vec![[0x01, 0x00, 0x5e, 0, 0, 1]];
    }
    let wanted = [[0x01, 0x00, 0x5e, 0, 0, 2], [0x33, 0x33, 0, 0, 0, 1]];
    iface.set_multicast_list(&wanted).expect("the call failed");
    let hw = iface.driver.hw.lock();
    assert_eq!(
        hw.mc_list, wanted,
        "the new list was appended to the old one instead of replacing it"
    );
    assert!(!hw.rx_promisc, "promiscuous moved");
    assert!(hw.rx_allmulti, "all-multicast moved");
}

/// What the three calls are for: the two RCTL bits the MAC filters on. The
/// unicast-promiscuous bit must not be left behind when promiscuous goes, or
/// the card keeps every frame on the wire with nothing asking it to.
#[test]
fn the_filter_bits_in_rctl_follow_the_two_flags() {
    let mut hw = make_hw();
    for (promisc, allmulti, upe, mpe) in [
        (true, false, true, true),
        (false, true, false, true),
        (false, false, false, false),
        (true, true, true, true),
    ] {
        unsafe {
            hw.set_rx_mode(promisc, allmulti, &[]);
        }
        let rctl = unsafe { core::ptr::read_volatile((hw.base + E1000E_RCTL * 4) as *const u32) };
        assert_eq!(
            rctl & RCTL_UPE != 0,
            upe,
            "unicast promiscuous wrong for promisc={} allmulti={}",
            promisc,
            allmulti
        );
        assert_eq!(
            rctl & RCTL_MPE != 0,
            mpe,
            "multicast promiscuous wrong for promisc={} allmulti={}",
            promisc,
            allmulti
        );
    }
}

/// A card that is down keeps the flags in software and does not reach for
/// RCTL: the registers are not hers to write while the hardware is stopped,
/// and `reinit_locked` applies them on the way back up.
#[test]
fn a_stopped_card_remembers_the_mode_without_touching_the_register() {
    let mut hw = make_hw();
    let base = hw.base;
    unsafe {
        hw.set_rx_mode(false, false, &[]);
        hw.hw_down();
        core::ptr::write_volatile((base + E1000E_RCTL * 4) as *mut u32, 0xdead_0000);
        hw.set_rx_mode(true, true, &[]);
    }
    assert!(hw.rx_promisc && hw.rx_allmulti, "the flags were not kept");
    assert_eq!(
        unsafe { core::ptr::read_volatile((base + E1000E_RCTL * 4) as *const u32) },
        0xdead_0000,
        "a stopped card wrote the filter bits into RCTL anyway"
    );
}

// --- admin up and down
//
// `seed_neighbor` is left out: the only thing it can be observed through is
// smoltcp's neighbour cache, and that cache's `lookup` is `pub(crate)` to
// smoltcp, so nothing outside it can read an entry back.

#[test]
fn taking_the_interface_down_stops_the_card() {
    let iface = interface();
    assert!(
        iface.driver.hw.lock().hw_running,
        "the card did not start up"
    );
    iface.admin_down().expect("the call failed");
    assert!(
        !iface.driver.hw.lock().hw_running,
        "the card is still running after the interface was taken down"
    );
}

/// `refresh_link` on a running card drops what it believed about the carrier
/// and asks the watchdog to look again straight away, rather than waiting out
/// the two-second period.
#[test]
fn refreshing_the_link_forgets_the_carrier_and_asks_the_watchdog_at_once() {
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    crate::nvme::nvme_queue::test_clock::set(1_000);
    for _ in 0..64 {
        if crate::utils::deferred_job::pending_deferred_jobs() == 0 {
            break;
        }
        crate::utils::deferred_job::drain_deferred_jobs();
    }
    let iface = interface();
    {
        let mut hw = iface.driver.hw.lock();
        hw.link_up = true;
        hw.link_watchdog_next_us = 2_000_000;
    }
    iface.refresh_link().expect("the call failed");
    assert!(
        !iface.driver.hw.lock().link_up,
        "it kept believing in a carrier it was told to go and check"
    );
    assert_eq!(
        iface.driver.hw.lock().link_watchdog_next_us,
        1_000 + E1000E_WATCHDOG_FAST_US,
        "the watchdog was left on its ordinary two-second period"
    );
    for _ in 0..64 {
        if crate::utils::deferred_job::pending_deferred_jobs() == 0 {
            break;
        }
        crate::utils::deferred_job::drain_deferred_jobs();
    }
}
