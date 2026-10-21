//! Auto-negotiation: what we ask the PHY for, what we read back, and what
//! the MAC is tuned to afterwards.
//!
//! None of this needs a NIC — every decision below is a pure function of
//! register contents, which is the whole reason it is split out that way.
//! The path these cover (a PCH part with a live Management Engine) is
//! never executed in CI: QEMU's e1000e has no ME, no ULP and no HV pages.

use super::rx_ring_tests::make_hw;
use super::*;

/// I219-V, i.e. what a PCH-SPT-or-later part looks like to `is_pch()`.
const I219: u16 = 0x15b8;

fn reg_read(base: usize, reg: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
}
fn reg_write(base: usize, reg: usize, v: u32) {
    unsafe { core::ptr::write_volatile((base + reg * 4) as *mut u32, v) }
}

// ---------------------------------------------------------------- pages

#[test]
fn hv_page_768_is_selected_as_page_zero() {
    // Linux: `if (page == HV_INTC_FC_PAGE_START) page = 0;`. Page 768 is
    // the PHY's page 0 seen at MDIO address 1. Selecting it by number
    // writes 768 << 5 = 0x6000, which is a different page entirely — and
    // HV_OEM_BITS (768, 25) is where LPLU lives, so getting this wrong
    // means reading and writing someone else's register.
    assert_eq!(hv_page_select(768), 0);
    assert_ne!(hv_page_select(768), (768u32 << PHY_PAGE_SHIFT) as u16);
}

#[test]
fn other_hv_pages_are_selected_by_number_shifted_left_five() {
    assert_eq!(hv_page_select(769), 0x6020); // CV_SMB_CTRL
    assert_eq!(hv_page_select(770), 0x6040); // HV_PM_CTRL
    assert_eq!(hv_page_select(779), 0x6160); // I218_ULP_CONFIG1
}

// ------------------------------------------------------------- OEM bits

#[test]
fn oem_bits_clear_lplu_and_gbe_disable_when_the_mac_copy_is_clear() {
    // This is the fix: the ME leaves LPLU set in the PHY, the driver
    // clears the MAC-side PHY_CTRL and thinks it is done, and the PHY goes
    // on negotiating the lowest speed the link supports — 10 Mb/s.
    let oem = HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS;
    let got = oem_bits_for_d0(0, oem, true, false);
    assert_eq!(got & HV_OEM_BITS_LPLU, 0);
    assert_eq!(got & HV_OEM_BITS_GBE_DIS, 0);
    assert_ne!(got & HV_OEM_BITS_RESTART_AN, 0);
}

#[test]
fn oem_bits_mirror_the_d0_copies_of_the_mac_register() {
    let phy_ctrl = PHY_CTRL_D0A_LPLU | PHY_CTRL_GBE_DISABLE;
    let got = oem_bits_for_d0(phy_ctrl, 0, true, false);
    assert_ne!(got & HV_OEM_BITS_LPLU, 0);
    assert_ne!(got & HV_OEM_BITS_GBE_DIS, 0);
}

#[test]
fn oem_bits_in_d0_ignore_the_non_d0_copies() {
    // In D0a only the D0a bits apply; the suspend path (d0_state = false)
    // takes both. Folding the two together would disable gigabit on a
    // machine whose firmware only asked for it while suspended.
    let phy_ctrl = PHY_CTRL_NOND0A_LPLU | PHY_CTRL_NOND0A_GBE_DISABLE;
    let d0 = oem_bits_for_d0(phy_ctrl, 0, true, false);
    assert_eq!(d0 & (HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS), 0);
    let dx = oem_bits_for_d0(phy_ctrl, 0, false, false);
    assert_eq!(
        dx & (HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS),
        HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS
    );
}

#[test]
fn oem_bits_do_not_restart_autoneg_when_the_me_blocks_phy_resets() {
    let got = oem_bits_for_d0(0, HV_OEM_BITS_LPLU, true, true);
    assert_eq!(got & HV_OEM_BITS_RESTART_AN, 0);
}

#[test]
fn oem_bits_leave_every_other_bit_of_the_register_alone() {
    // The register also carries LED and other OEM configuration we have no
    // business rewriting.
    let oem = 0x1289 | HV_OEM_BITS_LPLU;
    let got = oem_bits_for_d0(0, oem, true, false);
    assert_eq!(got & 0x1289, 0x1289);
}

// --------------------------------------------------- negotiation result

#[test]
fn phy_result_needs_both_link_and_autoneg_complete() {
    let up_and_done = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
    assert!(phy_negotiated_link(up_and_done, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_some());
    assert!(phy_negotiated_link(BMSR_LSTATUS, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_none());
    assert!(phy_negotiated_link(BMSR_ANEGCOMPLETE, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_none());
}

#[test]
fn phy_result_prefers_gigabit_full_duplex() {
    let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
    let adv = ADVERTISE_ALL_10_100;
    let lpa = LPA_10HALF | LPA_10FULL | LPA_100HALF | LPA_100FULL;
    let got = phy_negotiated_link(bmsr, adv, lpa, ADVERTISE_1000FULL, LPA_1000FULL);
    assert_eq!(got, Some((1000, true)));
}

#[test]
fn phy_result_ignores_gigabit_the_partner_never_offered() {
    // We advertise 1000; the switch port is 100-only. Resolution has to
    // fall through to 100 full, not claim the gigabit we asked for.
    let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
    let got = phy_negotiated_link(
        bmsr,
        ADVERTISE_ALL_10_100,
        LPA_100FULL | LPA_100HALF,
        ADVERTISE_1000FULL,
        0,
    );
    assert_eq!(got, Some((100, true)));
}

#[test]
fn phy_result_walks_down_the_ieee_priority_list() {
    let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
    let cases: [(u16, u16, (u32, bool)); 4] = [
        (ADVERTISE_100FULL, LPA_100FULL, (100, true)),
        (ADVERTISE_100HALF, LPA_100HALF, (100, false)),
        (ADVERTISE_10FULL, LPA_10FULL, (10, true)),
        (ADVERTISE_10HALF, LPA_10HALF, (10, false)),
    ];
    for (adv, lpa, want) in cases {
        assert_eq!(phy_negotiated_link(bmsr, adv, lpa, 0, 0), Some(want));
    }
    // 1000 half only ever resolves when both sides offered it.
    assert_eq!(
        phy_negotiated_link(bmsr, 0, 0, ADVERTISE_1000HALF, LPA_1000HALF),
        Some((1000, false))
    );
}

#[test]
fn phy_result_is_none_when_the_two_sides_share_nothing() {
    let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
    assert_eq!(
        phy_negotiated_link(bmsr, ADVERTISE_100FULL, LPA_10HALF, 0, 0),
        None
    );
}

// ------------------------------------------------------- MAC STATUS bits

#[test]
fn status_decodes_all_four_speed_encodings() {
    assert_eq!(status_speed_mbps(0), 10);
    assert_eq!(status_speed_mbps(0b01 << STATUS_SPEED_SHIFT), 100);
    assert_eq!(status_speed_mbps(0b10 << STATUS_SPEED_SHIFT), 1000);
    assert_eq!(status_speed_mbps(0b11 << STATUS_SPEED_SHIFT), 1000);
    // Bits outside [7:6] must not leak into the decode.
    assert_eq!(status_speed_mbps(0xFFFF_FF3F), 10);
}

// ------------------------------------------------------------ TIPG / IPG

#[test]
fn tipg_widens_the_gap_only_at_10_megabit_half_duplex() {
    let base = 8 | (8 << 10) | (6 << 20);
    assert_eq!(tipg_for_link(base, true, 10, false) & TIPG_IPGT_MASK, 0xFF);
    assert_eq!(tipg_for_link(base, true, 10, true) & TIPG_IPGT_MASK, 0x0C);
    assert_eq!(tipg_for_link(base, false, 10, true) & TIPG_IPGT_MASK, 0x08);
}

#[test]
fn tipg_uses_the_intermediate_gap_below_gigabit_on_spt_and_later() {
    let base = 8 | (8 << 10) | (6 << 20);
    assert_eq!(tipg_for_link(base, true, 100, true) & TIPG_IPGT_MASK, 0x0C);
    assert_eq!(tipg_for_link(base, true, 1000, true) & TIPG_IPGT_MASK, 0x08);
    assert_eq!(tipg_for_link(base, false, 100, true) & TIPG_IPGT_MASK, 0x08);
}

#[test]
fn tipg_keeps_the_ipgr1_and_ipgr2_fields() {
    let base = 8 | (8 << 10) | (6 << 20);
    let got = tipg_for_link(base, true, 10, false);
    assert_eq!(got & !TIPG_IPGT_MASK, base & !TIPG_IPGT_MASK);
}

#[test]
fn link_tuning_writes_tipg_and_the_i217_beacon_duration() {
    let mut hw = make_hw();
    hw.device_id = I219;
    reg_write(hw.base, E1000E_TIPG, 8 | (8 << 10) | (6 << 20));
    reg_write(hw.base, E1000E_FEXTNVM4, 0xDEAD_BEE0);

    // 10 Mb/s half duplex: STATUS speed bits 00, FD clear.
    unsafe { hw.apply_link_speed_tuning(0) };
    assert_eq!(reg_read(hw.base, E1000E_TIPG) & TIPG_IPGT_MASK, 0xFF);
    assert_eq!(
        reg_read(hw.base, E1000E_FEXTNVM4) & FEXTNVM4_BEACON_DURATION_MASK,
        FEXTNVM4_BEACON_DURATION_8USEC
    );
    // The rest of FEXTNVM4 is left as it was.
    assert_eq!(
        reg_read(hw.base, E1000E_FEXTNVM4) & !0x7,
        0xDEAD_BEE0 & !0x7
    );

    // Gigabit full duplex rolls the gap back to the default.
    unsafe { hw.apply_link_speed_tuning((0b10 << STATUS_SPEED_SHIFT) | STATUS_FD) };
    assert_eq!(reg_read(hw.base, E1000E_TIPG) & TIPG_IPGT_MASK, 0x08);
}

#[test]
fn link_tuning_leaves_discrete_parts_alone() {
    // The TIPG erratum and the beacon duration are PCH-only; an 82574 in
    // QEMU must come out of this untouched.
    let hw = make_hw();
    assert!(!hw.is_pch());
    reg_write(hw.base, E1000E_TIPG, 0x1234_5678);
    unsafe { hw.apply_link_speed_tuning(0) };
    assert_eq!(reg_read(hw.base, E1000E_TIPG), 0x1234_5678);
}
