//! Each constant below is pinned against the `amd-xgbe` source it was
//! taken from, named in the assertion. None of this has run on the
//! hardware, so the source IS the oracle: a test that merely repeated the
//! constant back would prove nothing, and the point of writing the Linux
//! name next to each value is that the next person can check it in one
//! grep. Read from torvalds/linux at v6.12 and at master on 28-10-2026.

use super::*;

// ——— Identity ———

#[test]
fn the_three_controller_ids_map_to_their_version_data() {
    // `xgbe_pci_table` in xgbe-pci.c: 0x1458 -> xgbe_v2a,
    // 0x1459 -> xgbe_v2b, 0x1641 -> xgbe_v3 (master only).
    assert_eq!(variant_for(0x1458), Some(Variant::V2a));
    assert_eq!(variant_for(0x1459), Some(Variant::V2b));
    assert_eq!(variant_for(0x1641), Some(Variant::V3));
}

#[test]
fn amds_other_devices_are_not_claimed_as_network_controllers() {
    // AMD's vendor id is on the host bridges, the IOMMU and the GPU too.
    // Claiming one of those takes a device away from the driver that can
    // actually drive it.
    assert!(xgbe_matched(0x1022, 0x1458));
    assert!(xgbe_matched(0x1022, 0x1459));
    assert!(xgbe_matched(0x1022, 0x1641));
    assert!(!xgbe_matched(0x1022, 0x2000), "that is the PCnet");
    assert!(!xgbe_matched(0x1022, 0x15d0), "that is the Raven bridge");
    assert!(!xgbe_matched(0x1022, 0x14b5), "the Yellow Carp bridge");
    assert!(!xgbe_matched(0x1022, 0x1630), "the Renoir bridge");
    assert!(!xgbe_matched(0x1022, 0x1122), "the P100a bridge");
    assert!(!xgbe_matched(0x1022, 0x1457), "one below the first id");
    assert!(!xgbe_matched(0x1022, 0x145a), "one above the second");
    // And another vendor's 0x1458 is somebody else's device.
    assert!(!xgbe_matched(0x8086, 0x1458));
    assert!(!xgbe_matched(0x10de, 0x1458));
}

#[test]
fn the_fifo_ceiling_belongs_to_the_version_not_the_board() {
    // `tx_max_fifo_size` / `rx_max_fifo_size` in the three
    // `xgbe_version_data` structs.
    assert_eq!(Variant::V2a.max_fifo_size(), 229_376);
    assert_eq!(Variant::V2b.max_fifo_size(), 65_536);
    assert_eq!(Variant::V3.max_fifo_size(), 65_536);
}

#[test]
fn only_the_v3_reaches_its_pcs_over_the_smn() {
    // `.xpcs_access = XGBE_XPCS_ACCESS_V3` on xgbe_v3 alone.
    assert!(!Variant::V2a.pcs_through_smn());
    assert!(!Variant::V2b.pcs_through_smn());
    assert!(Variant::V3.pcs_through_smn());
}

// ——— Resource layout ———

#[test]
fn the_two_bars_and_the_sub_blocks_sit_where_xgbe_h_says() {
    assert_eq!(XGMAC_BAR, 0, "XGBE_XGMAC_BAR");
    assert_eq!(XPCS_BAR, 1, "XGBE_XPCS_BAR");
    assert_ne!(XGMAC_BAR, XPCS_BAR, "swapping them maps each block twice");
    assert_eq!(MAC_PROP_OFFSET, 0x1d000, "XGBE_MAC_PROP_OFFSET");
    assert_eq!(I2C_CTRL_OFFSET, 0x1e000, "XGBE_I2C_CTRL_OFFSET");
    assert!(
        MIN_XGMAC_BAR_LEN > I2C_CTRL_OFFSET,
        "the minimum BAR must cover the last sub-block, not end at it"
    );
}

#[test]
fn the_port_property_registers_sit_where_xgbe_common_h_says() {
    assert_eq!(XP_PROP_0, 0x0000);
    assert_eq!(XP_PROP_1, 0x0004);
    assert_eq!(XP_PROP_2, 0x0008);
    assert_eq!(XP_PROP_3, 0x000c);
    assert_eq!(XP_PROP_4, 0x0010);
    assert_eq!(XP_MAC_ADDR_LO, 0x0020);
    assert_eq!(XP_MAC_ADDR_HI, 0x0024);
    assert_eq!(XP_INT_EN, 0x0078);
    // The address pair is contiguous and in that order; reading them the
    // other way round gives a plausible-looking but wrong MAC.
    assert_eq!(XP_MAC_ADDR_HI, XP_MAC_ADDR_LO + 4);
}

// ——— The PCS window ———

#[test]
fn the_window_register_pair_comes_from_the_host_bridge() {
    // This is the one that looks like it should depend on the NIC. It
    // does not: `xgbe_pci_probe` looks up 0:0.0 and switches on THAT
    // device id. Keying it off the NIC lands on the wrong registers on
    // every Raven, Yellow Carp, Renoir and P100a part.
    assert_eq!(pcs_window_regs(0x1022, BRIDGE_RV), (0x1060, 0x1064));
    assert_eq!(pcs_window_regs(0x1022, BRIDGE_YC), (0x18060, 0x18064));
    assert_eq!(pcs_window_regs(0x1022, BRIDGE_RN), (0xf8078, 0xf807c));
    assert_eq!(pcs_window_regs(0x1022, BRIDGE_P100A), (0x8060, 0x8080));
    // An AMD bridge not in the table, and a non-AMD bridge, both fall
    // back to the default pair.
    assert_eq!(pcs_window_regs(0x1022, 0x1234), (0x9060, 0x9064));
    assert_eq!(pcs_window_regs(0x8086, BRIDGE_RV), (0x9060, 0x9064));
    assert_eq!(pcs_window_regs(0x8086, 0x0c00), (0x9060, 0x9064));
}

#[test]
fn the_p100a_select_register_is_not_four_past_its_def() {
    // Every other pair is `def` and `def + 4`, so a driver that stores a
    // base and adds four works on three parts out of four and then reads
    // 0x8064 --- a different register entirely --- on a P100a.
    for (def, sel) in [
        PCS_V2_WINDOW,
        PCS_V2_RV_WINDOW,
        PCS_V2_YC_WINDOW,
        PCS_V3_RN_WINDOW,
    ] {
        assert_eq!(sel, def + 4, "pair at {:#x}", def);
    }
    let (def, sel) = PCS_P100A_WINDOW;
    assert_eq!(def, 0x8060);
    assert_eq!(sel, 0x8080);
    assert_ne!(sel, def + 4, "the P100a is the exception");
}

#[test]
fn the_window_offset_is_in_sixty_four_byte_units() {
    // `xpcs_window = XPCS_GET_BITS(reg, PCS_V2_WINDOW_DEF, OFFSET) << 6`,
    // with OFFSET at bit 6, 14 bits wide. So the field is both extracted
    // from bit 6 AND shifted back up by 6; doing only one of the two is
    // off by a factor of 64 in either direction.
    assert_eq!(decode_pcs_window(0).base, 0);
    // A stored offset of 1 means 64 bytes in.
    assert_eq!(decode_pcs_window(1 << 6).base, 64);
    assert_eq!(decode_pcs_window(2 << 6).base, 128);
    // The widest the field goes.
    assert_eq!(decode_pcs_window(0x3fff << 6).base, 0x3fff << 6);
    // Bits below the field must not leak into it.
    assert_eq!(decode_pcs_window(0x3f).base, 0, "bits 5..0 are not OFFSET");
    // Nor bits above it.
    assert_eq!(
        decode_pcs_window(0xffff_ffff).base,
        0x3fff << 6,
        "OFFSET is 14 bits, not everything above bit 6"
    );
}

#[test]
fn the_window_size_is_a_log2_exponent_biased_by_seven() {
    // `size = 1 << (size + 7)`, with SIZE at bit 2, 4 bits wide. A stored
    // 0 means 128 bytes, NOT 1 and not 0.
    assert_eq!(decode_pcs_window(0).size, 128);
    assert_eq!(decode_pcs_window(1 << 2).size, 256);
    assert_eq!(decode_pcs_window(2 << 2).size, 512);
    assert_eq!(decode_pcs_window(0xf << 2).size, 1 << 22);
    // The mask is the size minus one, so it is all-ones and can be ANDed
    // with an address.
    for reg in [0u32, 1 << 2, 7 << 2, 0xf << 2] {
        let w = decode_pcs_window(reg);
        assert_eq!(w.mask, w.size - 1);
        assert_eq!(w.mask & w.size, 0, "size must be a power of two");
    }
    // SIZE is four bits: bit 6 belongs to OFFSET, not to SIZE.
    assert_eq!(
        decode_pcs_window(0x3f).size,
        decode_pcs_window(0xf << 2).size
    );
}

#[test]
fn the_two_window_fields_do_not_overlap() {
    // OFFSET starts at 6 and SIZE occupies 5..2, so a register with only
    // one of them set must decode the other as its zero value.
    let only_offset = decode_pcs_window(0x2a << 6);
    assert_eq!(only_offset.base, 0x2a << 6);
    assert_eq!(only_offset.size, 128, "SIZE reads as zero -> 128");
    let only_size = decode_pcs_window(0x5 << 2);
    assert_eq!(only_size.base, 0, "OFFSET reads as zero");
    assert_eq!(only_size.size, 1 << 12);
}

#[test]
fn the_smn_address_of_a_v3_window_moves_with_the_port() {
    // `smn_base = PCS_RN_SMN_BASE_ADDR + PCS_RN_PORT_ADDR_SIZE * port_id`
    // and then the window register is added on top.
    assert_eq!(smn_window_address(0, 0), 0x11e0_0000);
    assert_eq!(smn_window_address(1, 0), 0x11f0_0000);
    assert_eq!(smn_window_address(2, 0), 0x1200_0000);
    let (def, _) = PCS_V3_RN_WINDOW;
    assert_eq!(smn_window_address(0, def), 0x11e0_0000 + 0xf8078);
    assert_eq!(smn_window_address(1, def), 0x11f0_0000 + 0xf8078);
}

// ——— The station address ———

#[test]
fn the_station_address_is_assembled_little_endian_across_both_words() {
    // lo carries bytes 0..3 and hi bytes 4..5, each from the BOTTOM of
    // its word up. Assembling it the other way round gives a MAC that
    // looks fine in a log and matches nothing on the wire.
    // The first byte is even on purpose: an odd one is a multicast
    // address and is refused by the check below, which is how this very
    // test caught its own first set of numbers.
    let mac = mac_from_prop(0x4433_2212, 0x8000_6655).expect("valid");
    assert_eq!(mac, [0x12, 0x22, 0x33, 0x44, 0x55, 0x66]);
}

#[test]
fn an_address_without_the_valid_bit_is_refused() {
    // `XP_MAC_ADDR_HI_VALID` is bit 31.
    assert_eq!(mac_from_prop(0x4433_2212, 0x0000_6655), None);
    assert!(mac_from_prop(0x4433_2212, 0x8000_6655).is_some());
    // And the bit alone is not enough to make an empty address usable.
    assert_eq!(mac_from_prop(0, 0x8000_0000), None);
}

#[test]
fn a_multicast_station_address_is_refused() {
    // The group bit is the low bit of the FIRST byte, which lives in the
    // low byte of `lo`. A board that reports one would have every switch
    // on the segment flood its traffic, and nothing would answer it.
    assert_eq!(mac_from_prop(0x4433_2201, 0x8000_6655), None);
    assert_eq!(mac_from_prop(0x4433_22ff, 0x8000_6655), None);
    assert!(
        mac_from_prop(0x4433_2202, 0x8000_6655).is_some(),
        "an even first byte is unicast and fine"
    );
    // The broadcast address is multicast too, so it is caught by the same
    // check rather than needing one of its own.
    assert_eq!(mac_from_prop(0xffff_ffff, 0x8000_ffff), None);
}

// ——— Port geometry ———

#[test]
fn the_counts_are_five_bit_fields_in_an_order_the_names_do_not_suggest() {
    // XP_PROP_1: MAX_TX_QUEUES at 0, MAX_RX_QUEUES at 8, MAX_TX_DMA at
    // 16, MAX_RX_DMA at 24. Reading channels and queues the other way
    // round gives a plausible number for each and the wrong ring count.
    let counts = decode_counts(0x0403_0201);
    assert_eq!(counts.tx_queues, 1, "bits 4..0");
    assert_eq!(counts.rx_queues, 2, "bits 12..8");
    assert_eq!(counts.tx_channels, 3, "bits 20..16");
    assert_eq!(counts.rx_channels, 4, "bits 28..24");

    // Each field is five bits, so the three bits above each one belong to
    // nothing and must not leak in.
    let all_ones = decode_counts(0xffff_ffff);
    assert_eq!(all_ones.tx_queues, 0x1f);
    assert_eq!(all_ones.rx_queues, 0x1f);
    assert_eq!(all_ones.tx_channels, 0x1f);
    assert_eq!(all_ones.rx_channels, 0x1f);
}

#[test]
fn the_fifo_size_is_in_sixteen_kibibyte_units_and_the_version_clamps_it() {
    // The clamp is not an alternative to the register: a board can report
    // more FIFO than the controller generation can drive, and Linux takes
    // the min of the two.
    assert_eq!(decode_fifo_sizes(0, Variant::V2b), (0, 0));
    assert_eq!(decode_fifo_sizes(1, Variant::V2b), (16_384, 0));
    assert_eq!(
        decode_fifo_sizes(1 | (2 << 16), Variant::V2b),
        (16_384, 32_768)
    );
    // Four units is 64 KiB, exactly the V2b ceiling: not clamped.
    assert_eq!(
        decode_fifo_sizes(4 | (4 << 16), Variant::V2b),
        (65_536, 65_536)
    );
    // Five is over it, and comes back as the ceiling.
    assert_eq!(
        decode_fifo_sizes(5 | (5 << 16), Variant::V2b),
        (65_536, 65_536)
    );
    // The same board on a V2a is not clamped there.
    assert_eq!(
        decode_fifo_sizes(5 | (5 << 16), Variant::V2a),
        (81_920, 81_920)
    );
    // And a board reporting the whole 16-bit field does not overflow.
    let (tx, rx) = decode_fifo_sizes(0xffff_ffff, Variant::V2a);
    assert_eq!((tx, rx), (229_376, 229_376));
}

#[test]
fn the_port_fields_are_at_their_own_offsets_in_prop_zero() {
    // PORT_ID at 0 (8 bits), PORT_MODE at 8 (4), PORT_SPEEDS at 22 (5).
    // Note the gap: bits 21..12 are something else and must not leak.
    let pp0 = 0x07 | (0x0a << 8) | (0x15 << 22);
    assert_eq!(port_id(pp0), 0x07);
    assert_eq!(port_mode(pp0), 0x0a);
    assert_eq!(port_speeds(pp0), 0x15);

    let all_ones = 0xffff_ffffu32;
    assert_eq!(port_id(all_ones), 0xff, "eight bits");
    assert_eq!(port_mode(all_ones), 0xf, "four bits");
    assert_eq!(port_speeds(all_ones), 0x1f, "five bits");

    // Bits between the mode and the speeds belong to neither.
    let between = 0x003f_f000;
    assert_eq!(port_mode(between), 0);
    assert_eq!(port_speeds(between), 0);
}

// ——— What the MAC says about itself ———
//
// These registers cannot be read here, so the only thing worth testing is
// the decode, and the only way a decode test has any value is if it
// pins the field's POSITION rather than repeating the arithmetic. So each
// case below sets exactly one field in an otherwise zero register and
// compares the whole structure against the all-zero decode with that one
// field changed. A field written one bit off, or two fields overlapping,
// fails --- which a per-field `assert_eq!` on its own value would not
// catch.

#[test]
fn the_four_self_description_registers_are_where_xgbe_common_h_says() {
    assert_eq!(MAC_VR, 0x0110);
    assert_eq!(MAC_HWF0R, 0x011c);
    assert_eq!(MAC_HWF1R, 0x0120);
    assert_eq!(MAC_HWF2R, 0x0124);
    // And they are inside BAR0, well below the port property block, so
    // the BAR length the probe already insists on covers them.
    assert!(MAC_HWF2R < MAC_PROP_OFFSET);
}

#[test]
fn get_bits_is_xgmac_get_bits() {
    // Linux: `(((var) >> (prefix##_##field##_INDEX)) & ...)`.
    assert_eq!(get_bits(0xffff_ffff, 0, 1), 1);
    assert_eq!(get_bits(0xffff_ffff, 18, 5), 0x1f);
    assert_eq!(get_bits(0x1234_5678, 16, 8), 0x34);
    assert_eq!(get_bits(0b1010_0000, 5, 1), 1);
    assert_eq!(get_bits(0b1010_0000, 6, 1), 0);
}

#[test]
fn mac_vr_is_three_bytes_in_order() {
    // MAC_VR_SNPSVER index 0, DEVID index 8, USERVER index 16, all 8 wide.
    let v = decode_mac_version(0x00ab_cd30);
    assert_eq!(v.snps, 0x30);
    assert_eq!(v.dev_id, 0xcd);
    assert_eq!(v.user, 0xab);
    // The top byte of the register is not part of any of the three.
    assert_eq!(decode_mac_version(0xff00_0000), decode_mac_version(0));
}

#[test]
fn the_snps_version_the_rest_of_the_driver_will_branch_on() {
    // `xgbe-dev.c` compares SNPSVER against exactly these three values,
    // so the byte has to come out of bit 0 and not bit 8.
    assert_eq!(decode_mac_version(0x0000_0030).snps, 0x30);
    assert_eq!(decode_mac_version(0x0000_0021).snps, 0x21);
    assert_eq!(decode_mac_version(0x0000_0020).snps, 0x20);
}

/// The all-zero decode: no optional block present, every zero-based count
/// at one, the smallest FIFO, the narrowest DMA, no hash table.
fn zeroed() -> HwFeatures {
    decode_hw_features(0, 0, 0)
}

#[test]
fn an_all_zero_chip_still_has_one_of_each_zero_based_thing() {
    let f = zeroed();
    assert_eq!(f.rx_queues, 1);
    assert_eq!(f.tx_queues, 1);
    assert_eq!(f.rx_channels, 1);
    assert_eq!(f.tx_channels, 1);
    assert_eq!(f.tc_count, 1);
    // `1 << (0 + 7)`.
    assert_eq!(f.rx_fifo_size, 128);
    assert_eq!(f.tx_fifo_size, 128);
    // ADDR64 == 0 is 32 bits, HASHTBLSZ == 0 is no table.
    assert_eq!(f.dma_width, 32);
    assert_eq!(f.hash_table_size, 0);
    assert!(!f.tso && !f.rss && !f.tx_coe && !f.rx_coe);
}

#[test]
fn every_field_of_hwf0r_sits_at_its_linux_index() {
    let z = zeroed();
    // MAC_HWF0R_GMIISEL_INDEX 1, _WIDTH 1
    assert_eq!(
        decode_hw_features(1 << 1, 0, 0),
        HwFeatures { gmii: true, ..z }
    );
    // VLHASH 4/1
    assert_eq!(
        decode_hw_features(1 << 4, 0, 0),
        HwFeatures {
            vlan_hash: true,
            ..z
        }
    );
    // SMASEL 5/1
    assert_eq!(
        decode_hw_features(1 << 5, 0, 0),
        HwFeatures { sma: true, ..z }
    );
    // RWKSEL 6/1
    assert_eq!(
        decode_hw_features(1 << 6, 0, 0),
        HwFeatures {
            remote_wake: true,
            ..z
        }
    );
    // MGKSEL 7/1
    assert_eq!(
        decode_hw_features(1 << 7, 0, 0),
        HwFeatures {
            magic_packet: true,
            ..z
        }
    );
    // MMCSEL 8/1
    assert_eq!(
        decode_hw_features(1 << 8, 0, 0),
        HwFeatures { mmc: true, ..z }
    );
    // ARPOFFSEL 9/1
    assert_eq!(
        decode_hw_features(1 << 9, 0, 0),
        HwFeatures {
            arp_offload: true,
            ..z
        }
    );
    // TSSEL 12/1
    assert_eq!(
        decode_hw_features(1 << 12, 0, 0),
        HwFeatures {
            timestamp: true,
            ..z
        }
    );
    // EEESEL 13/1
    assert_eq!(
        decode_hw_features(1 << 13, 0, 0),
        HwFeatures { eee: true, ..z }
    );
    // TXCOESEL 14/1
    assert_eq!(
        decode_hw_features(1 << 14, 0, 0),
        HwFeatures { tx_coe: true, ..z }
    );
    // RXCOESEL 16/1 --- note 15 is NOT a feature bit, and a decode that
    // read "the bit after TXCOESEL" would be wrong here.
    assert_eq!(
        decode_hw_features(1 << 16, 0, 0),
        HwFeatures { rx_coe: true, ..z }
    );
    assert_eq!(decode_hw_features(1 << 15, 0, 0), z);
    // ADDMACADRSEL 18/5
    assert_eq!(
        decode_hw_features(0x1f << 18, 0, 0),
        HwFeatures { addn_mac: 31, ..z }
    );
    // TSSTSSEL 25/2 --- two bits, and 25 is inside the 18/5 field's
    // neighbourhood, so an off-by-one on either shows up here.
    assert_eq!(
        decode_hw_features(0b11 << 25, 0, 0),
        HwFeatures { ts_src: 3, ..z }
    );
    // SAVLANINS 27/1
    assert_eq!(
        decode_hw_features(1 << 27, 0, 0),
        HwFeatures {
            sa_vlan_ins: true,
            ..z
        }
    );
    // VXN 29/1
    assert_eq!(
        decode_hw_features(1 << 29, 0, 0),
        HwFeatures { vxn: true, ..z }
    );
}

#[test]
fn every_field_of_hwf1r_sits_at_its_linux_index() {
    let z = zeroed();
    // RXFIFOSIZE 0/5, translated: `1 << (11 + 7)` is 256 KiB.
    assert_eq!(
        decode_hw_features(0, 11, 0),
        HwFeatures {
            rx_fifo_size: 256 * 1024,
            ..z
        }
    );
    // TXFIFOSIZE 6/5
    assert_eq!(
        decode_hw_features(0, 11 << 6, 0),
        HwFeatures {
            tx_fifo_size: 256 * 1024,
            ..z
        }
    );
    // ADVTHWORD 13/1
    assert_eq!(
        decode_hw_features(0, 1 << 13, 0),
        HwFeatures {
            adv_ts_hi: true,
            ..z
        }
    );
    // ADDR64 14/2, translated to a width in bits.
    assert_eq!(
        decode_hw_features(0, 1 << 14, 0),
        HwFeatures { dma_width: 40, ..z }
    );
    // DCBEN 16/1
    assert_eq!(
        decode_hw_features(0, 1 << 16, 0),
        HwFeatures { dcb: true, ..z }
    );
    // SPHEN 17/1
    assert_eq!(
        decode_hw_features(0, 1 << 17, 0),
        HwFeatures {
            split_header: true,
            ..z
        }
    );
    // TSOEN 18/1
    assert_eq!(
        decode_hw_features(0, 1 << 18, 0),
        HwFeatures { tso: true, ..z }
    );
    // DBGMEMA 19/1
    assert_eq!(
        decode_hw_features(0, 1 << 19, 0),
        HwFeatures {
            dma_debug: true,
            ..z
        }
    );
    // RSSEN 20/1
    assert_eq!(
        decode_hw_features(0, 1 << 20, 0),
        HwFeatures { rss: true, ..z }
    );
    // NUMTC 21/3, incremented: 7 encodes eight traffic classes.
    assert_eq!(
        decode_hw_features(0, 0b111 << 21, 0),
        HwFeatures { tc_count: 8, ..z }
    );
    // HASHTBLSZ 24/3, translated.
    assert_eq!(
        decode_hw_features(0, 3 << 24, 0),
        HwFeatures {
            hash_table_size: 256,
            ..z
        }
    );
    // L3L4FNUM 27/4
    assert_eq!(
        decode_hw_features(0, 0xf << 27, 0),
        HwFeatures {
            l3l4_filters: 15,
            ..z
        }
    );
}

#[test]
fn every_field_of_hwf2r_sits_at_its_linux_index() {
    let z = zeroed();
    // RXQCNT 0/4, incremented.
    assert_eq!(
        decode_hw_features(0, 0, 0xf),
        HwFeatures { rx_queues: 16, ..z }
    );
    // TXQCNT 6/4 --- bits 4 and 5 belong to nobody, so a packed-fields
    // reading of this register would put it at 4 and be wrong.
    assert_eq!(
        decode_hw_features(0, 0, 0xf << 6),
        HwFeatures { tx_queues: 16, ..z }
    );
    assert_eq!(decode_hw_features(0, 0, 0b11 << 4), z);
    // RXCHCNT 12/4
    assert_eq!(
        decode_hw_features(0, 0, 0xf << 12),
        HwFeatures {
            rx_channels: 16,
            ..z
        }
    );
    // TXCHCNT 18/4
    assert_eq!(
        decode_hw_features(0, 0, 0xf << 18),
        HwFeatures {
            tx_channels: 16,
            ..z
        }
    );
    // PPSOUTNUM 24/3, clamped to XGBE_MAX_PPS_OUT.
    assert_eq!(
        decode_hw_features(0, 0, 1 << 24),
        HwFeatures {
            pps_outputs: 1,
            ..z
        }
    );
    // AUXSNAPNUM 28/3, clamped to XGBE_MAX_AUX_SNAP.
    assert_eq!(
        decode_hw_features(0, 0, 1 << 28),
        HwFeatures {
            aux_snapshots: 1,
            ..z
        }
    );
}

#[test]
fn the_hash_table_size_is_an_index_not_a_count() {
    // `switch (hw_feat->hash_table_size)` in xgbe_get_all_hw_features.
    for (encoded, entries) in [(0u32, 0u32), (1, 64), (2, 128), (3, 256)] {
        assert_eq!(
            decode_hw_features(0, encoded << 24, 0).hash_table_size,
            entries,
            "HASHTBLSZ {} should mean {} entries",
            encoded,
            entries
        );
    }
    // Five, six and seven have no meaning; Linux's switch leaves them as
    // the raw value, which would be read as a table of five entries. We
    // read them as no table, which is the arm that makes `xgbe-dev.c`
    // skip hash filtering altogether instead of programming nonsense.
    for encoded in [4u32, 5, 6, 7] {
        assert_eq!(decode_hw_features(0, encoded << 24, 0).hash_table_size, 0);
    }
}

#[test]
fn the_dma_width_is_an_index_into_three_widths() {
    // `switch (hw_feat->dma_width)`: 0 -> 32, 1 -> 40, 2 -> 48,
    // default -> 32.
    for (encoded, bits) in [(0u32, 32u32), (1, 40), (2, 48), (3, 32)] {
        assert_eq!(decode_hw_features(0, encoded << 14, 0).dma_width, bits);
    }
}

#[test]
fn more_pps_outputs_than_the_driver_has_room_for_are_clamped() {
    // Linux warns and clamps to XGBE_MAX_PPS_OUT / XGBE_MAX_AUX_SNAP = 4.
    assert_eq!(MAX_PPS_OUT, 4);
    assert_eq!(MAX_AUX_SNAP, 4);
    let f = decode_hw_features(0, 0, (0b111 << 24) | (0b111 << 28));
    assert_eq!(f.pps_outputs, 4);
    assert_eq!(f.aux_snapshots, 4);
    // Four itself is not clamped away.
    let f = decode_hw_features(0, 0, (4 << 24) | (4 << 28));
    assert_eq!(f.pps_outputs, 4);
    assert_eq!(f.aux_snapshots, 4);
}

#[test]
fn an_impossible_fifo_size_saturates_instead_of_wrapping() {
    // The field is 5 bits, so a chip can claim `1 << 38` bytes. Linux
    // wraps it in an `unsigned int`; in a debug build the same shift
    // panics, which would turn a bad register read into a dead boot.
    assert_eq!(fifo_bytes(0), 128);
    assert_eq!(fifo_bytes(11), 256 * 1024);
    assert_eq!(fifo_bytes(24), 1 << 31);
    assert_eq!(fifo_bytes(25), u32::MAX);
    assert_eq!(fifo_bytes(31), u32::MAX);
    // Which is what the register path must do too, in both directions.
    let f = decode_hw_features(0, 0x1f | (0x1f << 6), 0);
    assert_eq!(f.rx_fifo_size, u32::MAX);
    assert_eq!(f.tx_fifo_size, u32::MAX);
}

#[test]
fn a_plausible_v2a_report_decodes_whole() {
    // Not a reading from a chip --- nobody here has one. It is the three
    // registers a part with eight channels, eight queues, eight traffic
    // classes, 128 KiB of FIFO each way, 40-bit DMA and the usual
    // offloads would present, assembled field by field from the indices
    // above. Its value is that it exercises every field at once: a decode
    // where two fields overlap can pass each single-field case and still
    // fail here.
    let hfr0 = (1 << 1)      // GMIISEL
        | (1 << 4)           // VLHASH
        | (1 << 5)           // SMASEL
        | (1 << 8)           // MMCSEL
        | (1 << 12)          // TSSEL
        | (1 << 14)          // TXCOESEL
        | (1 << 16)          // RXCOESEL
        | (31 << 18)         // ADDMACADRSEL
        | (1 << 27); // SAVLANINS
    let hfr1 = 10            // RXFIFOSIZE -> 128 KiB
        | (10 << 6)          // TXFIFOSIZE -> 128 KiB
        | (1 << 14)          // ADDR64 -> 40 bits
        | (1 << 16)          // DCBEN
        | (1 << 17)          // SPHEN
        | (1 << 18)          // TSOEN
        | (1 << 20)          // RSSEN
        | (7 << 21)          // NUMTC -> 8
        | (3 << 24)          // HASHTBLSZ -> 256
        | (8 << 27); // L3L4FNUM
    let hfr2 = 7             // RXQCNT -> 8
        | (7 << 6)           // TXQCNT -> 8
        | (7 << 12)          // RXCHCNT -> 8
        | (7 << 18); // TXCHCNT -> 8

    let f = decode_hw_features(hfr0, hfr1, hfr2);
    assert_eq!(f.tx_channels, 8);
    assert_eq!(f.rx_channels, 8);
    assert_eq!(f.tx_queues, 8);
    assert_eq!(f.rx_queues, 8);
    assert_eq!(f.tc_count, 8);
    assert_eq!(f.tx_fifo_size, 128 * 1024);
    assert_eq!(f.rx_fifo_size, 128 * 1024);
    assert_eq!(f.dma_width, 40);
    assert_eq!(f.hash_table_size, 256);
    assert_eq!(f.addn_mac, 31);
    assert_eq!(f.l3l4_filters, 8);
    assert!(f.gmii && f.vlan_hash && f.sma && f.mmc && f.timestamp);
    assert!(f.tx_coe && f.rx_coe && f.tso && f.rss && f.dcb && f.split_header);
    assert!(f.sa_vlan_ins);
    // And what such a part does NOT have, so the all-ones mistake fails.
    assert!(!f.eee && !f.arp_offload && !f.remote_wake && !f.magic_packet);
    assert!(!f.vxn && !f.adv_ts_hi && !f.dma_debug);
    assert_eq!(f.ts_src, 0);
    assert_eq!(f.pps_outputs, 0);
    assert_eq!(f.aux_snapshots, 0);

    // The FIFO the core has is not necessarily the FIFO the driver may
    // use: a V2a's ceiling is larger than this, a V2b's is smaller.
    assert!(f.tx_fifo_size <= Variant::V2a.max_fifo_size());
    assert!(f.tx_fifo_size > Variant::V2b.max_fifo_size());
}

// ——— The per-channel DMA register map ———

#[test]
fn the_channel_block_is_where_xgbe_common_h_says() {
    assert_eq!(DMA_CH_BASE, 0x3100);
    assert_eq!(DMA_CH_INC, 0x80);
    // Inside BAR0 and well below the port property block, like the MAC
    // registers.
    assert!(DMA_CH_BASE < MAC_PROP_OFFSET);
}

#[test]
fn every_per_channel_register_is_at_its_linux_offset() {
    for (name, off, want) in [
        ("DMA_CH_CR", DMA_CH_CR, 0x00),
        ("DMA_CH_TCR", DMA_CH_TCR, 0x04),
        ("DMA_CH_RCR", DMA_CH_RCR, 0x08),
        ("DMA_CH_TDLR_HI", DMA_CH_TDLR_HI, 0x10),
        ("DMA_CH_TDLR_LO", DMA_CH_TDLR_LO, 0x14),
        ("DMA_CH_RDLR_HI", DMA_CH_RDLR_HI, 0x18),
        ("DMA_CH_RDLR_LO", DMA_CH_RDLR_LO, 0x1c),
        ("DMA_CH_TDTR_LO", DMA_CH_TDTR_LO, 0x24),
        ("DMA_CH_RDTR_LO", DMA_CH_RDTR_LO, 0x2c),
        ("DMA_CH_TDRLR", DMA_CH_TDRLR, 0x30),
        ("DMA_CH_RDRLR", DMA_CH_RDRLR, 0x34),
        ("DMA_CH_IER", DMA_CH_IER, 0x38),
        ("DMA_CH_RIWT", DMA_CH_RIWT, 0x3c),
        ("DMA_CH_SR", DMA_CH_SR, 0x60),
    ] {
        assert_eq!(off, want, "{} moved", name);
    }
}

#[test]
fn the_channel_stride_leaves_a_hole_at_the_end_of_each_block() {
    // The highest register in a channel's block is DMA_CH_SR at 0x60, but
    // the stride is 0x80. Packing the blocks end to end --- the obvious
    // mistake --- would put channel 1's registers inside channel 0's
    // reach and read the wrong engine's status.
    assert!(DMA_CH_SR < DMA_CH_INC);
    assert_ne!(DMA_CH_SR + 4, DMA_CH_INC);
}

#[test]
fn a_channels_block_is_its_index_times_the_stride() {
    // `channel->dma_regs = xgmac_regs + DMA_CH_BASE + DMA_CH_INC * i`.
    let base = 0xffff_8000_0000_0000usize;
    assert_eq!(channel_regs(base, 0), base + 0x3100);
    assert_eq!(channel_regs(base, 1), base + 0x3180);
    assert_eq!(channel_regs(base, 7), base + 0x3100 + 7 * 0x80);
    // And the blocks do not overlap, for the largest geometry HWF2R can
    // report (sixteen channels).
    for i in 0..16u32 {
        assert_eq!(
            channel_regs(base, i + 1) - channel_regs(base, i),
            DMA_CH_INC
        );
    }
}

#[test]
fn the_list_address_registers_hold_the_high_half_first() {
    // This is the one that bites: TDLR_HI is at the LOWER address. A
    // `u64` written as two consecutive words would land the halves
    // swapped and point the descriptor fetch somewhere else.
    assert_eq!(DMA_CH_TDLR, (0x10, 0x14));
    assert_eq!(DMA_CH_RDLR, (0x18, 0x1c));
    assert!(DMA_CH_TDLR.0 < DMA_CH_TDLR.1);
    assert!(DMA_CH_RDLR.0 < DMA_CH_RDLR.1);
}

#[test]
fn splitting_a_ring_address_puts_the_halves_in_register_order() {
    assert_eq!(split_ring_addr(0x0000_0001_dead_b000), (1, 0xdead_b000));
    assert_eq!(split_ring_addr(0x8000_0000), (0, 0x8000_0000));
    // A 32-bit-addressed ring has nothing in the high word, which is the
    // case where a swap would write the ring address into _HI and zero
    // into _LO --- a fetch from near zero, with no error reported.
    let (hi, lo) = split_ring_addr(0x1000_0000);
    assert_eq!(hi, 0);
    assert_eq!(lo, 0x1000_0000);
}

#[test]
fn the_tail_pointers_have_no_high_half() {
    // Linux defines only `_LO` for both tail registers, and the gaps at
    // 0x20 and 0x28 where a `_HI` would sit stay undefined. So a tail
    // write is one word, not two.
    assert_eq!(DMA_CH_TDTR_LO, 0x24);
    assert_eq!(DMA_CH_RDTR_LO, 0x2c);
    assert_eq!(DMA_CH_TDTR_LO - DMA_CH_TDLR_LO, 0x10);
}

// ——— The descriptor ———

#[test]
fn a_descriptor_is_four_words_and_sixteen_bytes() {
    // `struct xgbe_ring_desc`: four `__le32`, nothing else. The engine
    // indexes the ring by this size, so a padded or reordered struct
    // would desynchronise driver and chip after the first entry.
    assert_eq!(core::mem::size_of::<Desc>(), 16);
    assert_eq!(core::mem::align_of::<Desc>(), 4);
}

#[test]
fn the_desc3_flags_are_at_their_linux_indices() {
    // OWN 31, CTXT 30, FD 29, LD 28, in both directions.
    assert_eq!(DESC3_OWN, 1 << 31);
    assert_eq!(DESC3_CTXT, 1 << 30);
    assert_eq!(DESC3_FD, 1 << 29);
    assert_eq!(DESC3_LD, 1 << 28);
    assert!(desc_owned_by_dma(DESC3_OWN));
    assert!(!desc_owned_by_dma(0));
    assert!(!desc_owned_by_dma(DESC3_CTXT | DESC3_FD | DESC3_LD));
    assert!(desc_is_context(DESC3_CTXT));
    assert!(!desc_is_context(DESC3_OWN));
}

#[test]
fn bit_thirty_is_inte_going_out_and_ctxt_coming_back() {
    // One bit, two meanings, chosen by who wrote the descriptor last:
    // RX_NORMAL_DESC3_INTE_INDEX and RX_NORMAL_DESC3_CTXT_INDEX are both
    // 30. The two accessors read the same bit; the point is that the
    // caller has to say which direction it is asking about.
    let d = DESC3_CTXT;
    assert!(rx_desc3_inte(d));
    assert!(desc_is_context(d));
    assert!(!rx_desc3_inte(0));
}

#[test]
fn the_received_length_and_error_come_out_of_desc3() {
    // RX_NORMAL_DESC3_PL index 0 width 14, ES index 15 width 1.
    let d = DESC3_OWN | DESC3_FD | DESC3_LD | 1514;
    assert_eq!(rx_packet_len(d), 1514);
    assert!(!rx_error_summary(d));
    assert!(rx_error_summary(d | (1 << 15)));
    // Fourteen bits, so the field tops out at 16383 and the bit above it
    // is the error summary --- a length read fifteen bits wide would
    // report an errored 1514-byte frame as a 34282-byte one.
    assert_eq!(rx_packet_len(0x3fff), 0x3fff);
    assert_eq!(rx_packet_len(1514 | (1 << 15)), 1514);
}

#[test]
fn the_split_header_length_is_narrower_than_the_transmit_length() {
    // RX_NORMAL_DESC2_HL is 10 bits; TX_NORMAL_DESC2_HL_B1L in the same
    // word on the other side is 14. Same word, same index, different
    // width.
    assert_eq!(rx_header_len(0x3ff), 0x3ff);
    assert_eq!(rx_header_len(0xffff_ffff), 0x3ff);
    assert_eq!(tx_desc2_buffer_len(0x3fff), 0x3fff);
    assert_eq!(tx_desc2_buffer_len(1514), 1514);
}

#[test]
fn the_transmit_frame_length_shares_its_bits_with_the_tso_payload() {
    // TX_NORMAL_DESC3_FL is index 0 width 15; TCPPL is index 0 width 18;
    // TSE is index 18. So bit 18 is simultaneously "this is a TSO
    // descriptor" and the bottom of the payload length it selects.
    assert_eq!(tx_desc3_frame_len(1514), 1514);
    assert_eq!(tx_desc3_frame_len(0x7fff), 0x7fff);
    assert_eq!(TX_DESC3_TSE, 1 << 18);
    // The frame length can never set TSE by itself, which is what makes
    // the non-TSO path safe.
    assert_eq!(tx_desc3_frame_len(0xffff_ffff) & TX_DESC3_TSE, 0);
}

// ——— The sequences, against a fake XGMAC ———
//
// These are the first tests here that run real driver code rather than
// checking a number. The fake is deliberately not a stub that returns
// zero: a self-clearing bit that clears immediately would pass a reset
// routine that never polls at all, so the fake clears `SWR` and `FTQ`
// only after a configurable number of reads, and records every access in
// order.

use alloc::vec::Vec;
use core::cell::RefCell;

/// Every register touch, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Read(usize),
    Write(usize, u32),
}

struct FakeXgmac {
    /// Sparse register file.
    regs: RefCell<alloc::collections::BTreeMap<usize, u32>>,
    log: RefCell<Vec<Access>>,
    /// How many reads a self-clearing bit survives before the fake
    /// clears it. 0 means it clears on the first read.
    clear_after: RefCell<alloc::collections::BTreeMap<usize, u32>>,
    /// Never clear these, to exercise the timeout.
    stuck: RefCell<alloc::collections::BTreeSet<usize>>,
}

// The fake is only ever touched from the test thread; `MmioAccess`
// requires Send + Sync because a real driver holds one across CPUs.
unsafe impl Send for FakeXgmac {}
unsafe impl Sync for FakeXgmac {}

impl FakeXgmac {
    fn new() -> Self {
        Self {
            regs: RefCell::new(alloc::collections::BTreeMap::new()),
            log: RefCell::new(Vec::new()),
            clear_after: RefCell::new(alloc::collections::BTreeMap::new()),
            stuck: RefCell::new(alloc::collections::BTreeSet::new()),
        }
    }

    /// Preload a register, as a chip with other fields already set would
    /// present it. Not logged: this is the chip's state, not an access.
    fn preload(&self, off: usize, value: u32) {
        self.regs.borrow_mut().insert(off, value);
    }

    /// Make `off`'s bit 0 survive `n` reads before clearing.
    fn clear_bit0_after(&self, off: usize, n: u32) {
        self.clear_after.borrow_mut().insert(off, n);
    }

    /// Make `off`'s bit 0 never clear.
    fn stick_bit0(&self, off: usize) {
        self.stuck.borrow_mut().insert(off);
    }

    fn log(&self) -> Vec<Access> {
        self.log.borrow().clone()
    }

    fn writes_to(&self, off: usize) -> usize {
        self.log()
            .iter()
            .filter(|a| matches!(a, Access::Write(o, _) if *o == off))
            .count()
    }

    fn reads_of(&self, off: usize) -> usize {
        self.log()
            .iter()
            .filter(|a| matches!(a, Access::Read(o) if *o == off))
            .count()
    }
}

impl MmioAccess for FakeXgmac {
    fn read32(&self, off: usize) -> u32 {
        self.log.borrow_mut().push(Access::Read(off));
        let mut regs = self.regs.borrow_mut();
        let value = *regs.get(&off).unwrap_or(&0);
        if value & 1 != 0 && !self.stuck.borrow().contains(&off) {
            let mut after = self.clear_after.borrow_mut();
            match after.get_mut(&off) {
                Some(0) | None => {
                    regs.insert(off, value & !1);
                }
                Some(left) => *left -= 1,
            }
        }
        value
    }

    fn write32(&self, off: usize, value: u32) {
        self.log.borrow_mut().push(Access::Write(off, value));
        self.regs.borrow_mut().insert(off, value);
    }
}

/// A delay that counts how often it was called.
fn counting_wait(n: &core::cell::Cell<u32>) -> impl FnMut() + '_ {
    move || n.set(n.get() + 1)
}

#[test]
fn set_bits_replaces_only_its_own_field() {
    // Linux's `SET_BITS`.
    assert_eq!(set_bits(0, 0, 1, 1), 1);
    assert_eq!(set_bits(0xffff_ffff, 0, 1, 0), 0xffff_fffe);
    assert_eq!(set_bits(0, 16, 10, 0x3ff), 0x3ff << 16);
    // A value wider than the field cannot spill into the next one.
    assert_eq!(set_bits(0, 4, 3, 0xff), 0x7 << 4);
    // And the rest of the word survives.
    assert_eq!(set_bits(0xdead_0000, 0, 1, 1), 0xdead_0001);
}

#[test]
fn writing_a_field_is_a_read_modify_write() {
    // This is why `write_field` exists rather than a bare store: every
    // register the sequences touch shares its word with fields that must
    // survive. `MTL_Q_TQOMR` holds the queue size and the
    // store-and-forward mode alongside `FTQ`.
    let fake = FakeXgmac::new();
    fake.preload(MTL_Q_TQOMR, 0x0123_0000);
    write_field(&fake, MTL_Q_TQOMR, 0, 1, 1);
    assert_eq!(fake.log()[0], Access::Read(MTL_Q_TQOMR));
    assert_eq!(fake.log()[1], Access::Write(MTL_Q_TQOMR, 0x0123_0001));
}

#[test]
fn the_dma_mode_and_mtl_queue_registers_are_where_linux_says() {
    assert_eq!(DMA_MR, 0x3000);
    assert_eq!(DMA_SBMR, 0x3004);
    assert_eq!(MTL_Q_BASE, 0x1100);
    assert_eq!(MTL_Q_INC, 0x80);
    assert_eq!(MTL_Q_TQOMR, 0x00);
    assert_eq!(MTL_Q_RQOMR, 0x40);
    assert_eq!(DMA_MR_SWR, (0, 1));
    assert_eq!(MTL_Q_TQOMR_FTQ, (0, 1));
    assert_eq!(POLL_LIMIT, 2000);
    // The MTL block sits below the DMA block, and neither reaches the
    // other: MTL queue 15's last register is still under DMA_MR.
    assert!(mtl_queue_reg(15, MTL_Q_RQOMR) < DMA_MR);
}

#[test]
fn a_queues_registers_are_its_index_times_the_stride() {
    assert_eq!(mtl_queue_reg(0, MTL_Q_TQOMR), 0x1100);
    assert_eq!(mtl_queue_reg(1, MTL_Q_TQOMR), 0x1180);
    assert_eq!(mtl_queue_reg(1, MTL_Q_RQOMR), 0x11c0);
    assert_eq!(mtl_queue_reg(7, MTL_Q_TQOMR), 0x1100 + 7 * 0x80);
}

#[test]
fn the_reset_is_issued_twice_on_purpose() {
    // Linux's comment in `xgbe_exit`: "To guard against possible
    // incorrectly generated interrupts, issue the software reset twice."
    // Collapsing it to one looks like a simplification and shows up as
    // an interrupt storm at boot, so the count is the test.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(0);
    assert!(software_reset(&fake, &mut counting_wait(&n)).is_ok());
    assert_eq!(fake.writes_to(DMA_MR), 2);
    for a in fake.log() {
        if let Access::Write(off, value) = a {
            assert_eq!(off, DMA_MR);
            assert_eq!(value & 1, 1, "the write must set SWR");
        }
    }
}

#[test]
fn the_reset_waits_for_the_chip_instead_of_assuming() {
    // A chip that holds SWR for a while must be polled until it clears.
    // A routine that wrote the bit and returned would pass a "reset"
    // chip that is still resetting --- and nothing would report it.
    let fake = FakeXgmac::new();
    fake.clear_bit0_after(DMA_MR, 5);
    let n = core::cell::Cell::new(0);
    assert!(software_reset_once(&fake, &mut counting_wait(&n)).is_ok());
    // One read per poll until it cleared, so more than one read.
    assert!(
        fake.reads_of(DMA_MR) > 2,
        "polled {} times",
        fake.reads_of(DMA_MR)
    );
    // And the delay was actually used: once after the write, once per
    // poll that found the bit still set.
    assert!(n.get() >= 2, "waited {} times", n.get());
}

#[test]
fn a_chip_stuck_in_reset_is_reported_not_waited_on_forever() {
    // Linux's `-EBUSY` after 2000 polls.
    let fake = FakeXgmac::new();
    fake.stick_bit0(DMA_MR);
    let n = core::cell::Cell::new(0);
    assert_eq!(
        software_reset_once(&fake, &mut counting_wait(&n)),
        Err(DeviceError::NotReady)
    );
    // POLL_LIMIT polls, plus the one read of the read-modify-write that
    // issued the reset in the first place.
    assert_eq!(fake.reads_of(DMA_MR), POLL_LIMIT as usize + 1);
}

#[test]
fn a_failed_first_reset_does_not_issue_the_second() {
    let fake = FakeXgmac::new();
    fake.stick_bit0(DMA_MR);
    let n = core::cell::Cell::new(0);
    assert!(software_reset(&fake, &mut counting_wait(&n)).is_err());
    assert_eq!(fake.writes_to(DMA_MR), 1);
}

#[test]
fn an_older_core_is_not_asked_to_flush_its_queues() {
    // `xgbe_flush_tx_queues` returns immediately below SNPSVER 0x21,
    // because FTQ does not exist there --- writing it would set a
    // reserved bit in a register that also holds the queue size and the
    // store-and-forward mode.
    assert_eq!(FLUSH_MIN_SNPSVER, 0x21);
    for ver in [0x00u8, 0x10, 0x20] {
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(0);
        assert!(flush_tx_queues(&fake, ver, 8, &mut counting_wait(&n)).is_ok());
        assert!(fake.log().is_empty(), "core {:#04x} was touched", ver);
    }
}

#[test]
fn every_queue_is_kicked_before_any_is_polled() {
    // The order is the point. Flushing one queue at a time and waiting
    // for each serialises what the hardware does in parallel: on eight
    // queues that is eight timeouts' worth of worst case instead of one.
    let fake = FakeXgmac::new();
    for q in 0..4 {
        fake.clear_bit0_after(mtl_queue_reg(q, MTL_Q_TQOMR), 3);
    }
    let n = core::cell::Cell::new(0);
    assert!(flush_tx_queues(&fake, 0x30, 4, &mut counting_wait(&n)).is_ok());

    let log = fake.log();
    let last_write = log
        .iter()
        .rposition(|a| matches!(a, Access::Write(..)))
        .expect("there must be writes");
    let first_poll_read = log
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a, Access::Read(..)))
        // The read-modify-write of each kick reads too, so the polls are
        // the reads that come after the last write.
        .map(|(i, _)| i)
        .find(|i| *i > last_write)
        .expect("there must be polls");
    assert!(first_poll_read > last_write);
    // All four queues were kicked, each exactly once.
    for q in 0..4 {
        assert_eq!(fake.writes_to(mtl_queue_reg(q, MTL_Q_TQOMR)), 1);
    }
}

#[test]
fn a_queue_that_will_not_flush_is_reported_with_its_number() {
    let fake = FakeXgmac::new();
    fake.stick_bit0(mtl_queue_reg(2, MTL_Q_TQOMR));
    let n = core::cell::Cell::new(0);
    assert_eq!(
        flush_tx_queues(&fake, 0x30, 4, &mut counting_wait(&n)),
        Err(DeviceError::NotReady)
    );
    // Queue 3 is never polled, because queue 2 failed first --- and all
    // four were still kicked before any poll.
    assert_eq!(fake.writes_to(mtl_queue_reg(3, MTL_Q_TQOMR)), 1);
}

// ——— The system bus mode ———

#[test]
fn every_sbmr_field_is_at_its_linux_index() {
    assert_eq!(DMA_SBMR_UNDEF, (0, 1));
    assert_eq!(DMA_SBMR_BLEN, (1, 7));
    assert_eq!(DMA_SBMR_EAME, (11, 1));
    assert_eq!(DMA_SBMR_AAL, (12, 1));
    assert_eq!(DMA_SBMR_RD_OSR_LMT, (16, 6));
    assert_eq!(DMA_SBMR_WR_OSR_LMT, (24, 6));
    // BLEN runs from 1 to 7 inclusive, so it stops short of EAME at 11
    // with a gap, and does NOT start at 0 where UNDEF is.
    assert_eq!(DMA_SBMR_BLEN.0 + DMA_SBMR_BLEN.1, 8);
    assert!(DMA_SBMR_BLEN.0 > DMA_SBMR_UNDEF.0);
}

#[test]
fn the_burst_length_field_is_a_bitmap_of_lengths_not_a_number() {
    // `blen >> 2` in xgbe_config_dma_bus looks like a unit conversion and
    // is not: each of BLEN's seven bits is one burst length, so dividing
    // the beat count by four lands on that length's own bit.
    for (beats, bit) in [
        (4u32, 0u32),
        (8, 1),
        (16, 2),
        (32, 3),
        (64, 4),
        (128, 5),
        (256, 6),
    ] {
        assert_eq!(
            blen_field(beats),
            Some(1 << bit),
            "{} beats should be bit {} of BLEN",
            beats,
            bit
        );
    }
    // Each one is a single bit, which is what proves it is a bitmap.
    for beats in [4u32, 8, 16, 32, 64, 128, 256] {
        assert_eq!(blen_field(beats).unwrap().count_ones(), 1);
    }
    // And every length fits in the seven-bit field.
    assert!(blen_field(256).unwrap() < (1 << DMA_SBMR_BLEN.1));
}

#[test]
fn a_burst_length_that_is_not_one_of_the_seven_is_refused() {
    // Shifting, say, 100 beats would land on bit 25 of the field, which
    // is outside it --- or worse, on a different length's bit.
    for beats in [0u32, 1, 2, 3, 5, 100, 512, 1024] {
        assert_eq!(blen_field(beats), None, "{} beats was accepted", beats);
    }
}

#[test]
fn the_outstanding_limits_are_written_one_less_than_they_are() {
    // `RD_OSR_LMT, pdata->rd_osr_limit - 1`. The field counts from zero.
    let cfg = BusConfig {
        burst_beats: 256,
        aligned_beats: false,
        read_outstanding: 8,
        write_outstanding: 4,
    };
    let v = system_bus_mode(0, cfg).unwrap();
    assert_eq!(get_bits(v, DMA_SBMR_RD_OSR_LMT.0, DMA_SBMR_RD_OSR_LMT.1), 7);
    assert_eq!(get_bits(v, DMA_SBMR_WR_OSR_LMT.0, DMA_SBMR_WR_OSR_LMT.1), 3);
    // One request outstanding is a zero in the field, not a one.
    let one = BusConfig {
        read_outstanding: 1,
        write_outstanding: 1,
        ..cfg
    };
    let v = system_bus_mode(0, one).unwrap();
    assert_eq!(get_bits(v, DMA_SBMR_RD_OSR_LMT.0, DMA_SBMR_RD_OSR_LMT.1), 0);
}

#[test]
fn a_zero_outstanding_limit_is_refused_instead_of_underflowing_to_sixty_three() {
    // This is the one that a missing platform property produces in Linux:
    // `0 - 1` in an `unsigned int`, masked to six bits, is 63 --- the
    // most permissive setting there is, from a value nobody set.
    let cfg = BusConfig {
        burst_beats: 256,
        aligned_beats: false,
        read_outstanding: 0,
        write_outstanding: 8,
    };
    assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
    let cfg = BusConfig {
        write_outstanding: 0,
        read_outstanding: 8,
        ..cfg
    };
    assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
    // And a limit above what six bits can hold is refused too, rather
    // than silently truncated to something smaller than asked for.
    let cfg = BusConfig {
        read_outstanding: 65,
        write_outstanding: 8,
        ..cfg
    };
    assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
}

#[test]
fn enhanced_addressing_and_the_undocumented_bit_are_always_set() {
    // EAME must be on before any descriptor address above 4 GiB is
    // programmed: the high half of the list address register gets written
    // regardless, so without it the engine fetches the ring from the
    // wrong place and reports nothing. UNDEF is set because Linux sets
    // it and nobody has documented what it does.
    let cfg = BusConfig {
        burst_beats: 32,
        aligned_beats: true,
        read_outstanding: 2,
        write_outstanding: 2,
    };
    let v = system_bus_mode(0, cfg).unwrap();
    assert_eq!(get_bits(v, DMA_SBMR_EAME.0, DMA_SBMR_EAME.1), 1);
    assert_eq!(get_bits(v, DMA_SBMR_UNDEF.0, DMA_SBMR_UNDEF.1), 1);
    assert_eq!(get_bits(v, DMA_SBMR_AAL.0, DMA_SBMR_AAL.1), 1);
    let v = system_bus_mode(
        0,
        BusConfig {
            aligned_beats: false,
            ..cfg
        },
    )
    .unwrap();
    assert_eq!(get_bits(v, DMA_SBMR_AAL.0, DMA_SBMR_AAL.1), 0);
}

#[test]
fn the_bus_mode_is_one_read_and_one_write_not_five() {
    // The point of building the word locally. Five read-modify-writes
    // would publish four intermediate states to a live bus master, each
    // a combination the driver never intended --- including, briefly,
    // a burst length with EAME already on and the limits still at
    // whatever the chip had.
    let fake = FakeXgmac::new();
    fake.preload(DMA_SBMR, 0x0000_0e00);
    let cfg = BusConfig {
        burst_beats: 256,
        aligned_beats: false,
        read_outstanding: 8,
        write_outstanding: 8,
    };
    assert!(config_dma_bus(&fake, cfg).is_ok());
    let log = fake.log();
    assert_eq!(log.len(), 2, "{:?}", log);
    assert_eq!(log[0], Access::Read(DMA_SBMR));
    assert!(matches!(log[1], Access::Write(DMA_SBMR, _)));
}

#[test]
fn the_bits_the_driver_does_not_claim_survive_the_bus_mode_write() {
    // Bits 13..15 and 22..23 belong to no field above, so whatever the
    // chip reports there has to come back out untouched.
    let preserved = (1 << 13) | (1 << 15) | (1 << 22) | (1 << 23);
    let cfg = BusConfig {
        burst_beats: 4,
        aligned_beats: false,
        read_outstanding: 1,
        write_outstanding: 1,
    };
    let v = system_bus_mode(preserved, cfg).unwrap();
    assert_eq!(v & preserved, preserved);
}

#[test]
fn a_refused_bus_config_writes_nothing() {
    let fake = FakeXgmac::new();
    let cfg = BusConfig {
        burst_beats: 100,
        aligned_beats: false,
        read_outstanding: 8,
        write_outstanding: 8,
    };
    assert_eq!(config_dma_bus(&fake, cfg), Err(DeviceError::InvalidParam));
    assert_eq!(fake.writes_to(DMA_SBMR), 0);
}

// ——— The station address ———

#[test]
fn the_address_registers_are_where_linux_says() {
    assert_eq!(MAC_MACA0HR, 0x0300);
    assert_eq!(MAC_MACA0LR, 0x0304);
    assert_eq!(MAC_MACA1HR, 0x0308);
    // Each address slot is a pair of words, so slot 1 starts one pair on.
    assert_eq!(MAC_MACA1HR - MAC_MACA0HR, 8);
    assert_eq!(MAC_MACA1HR_AE, (31, 1));
}

#[test]
fn the_address_halves_are_four_bytes_and_two_not_three_and_three() {
    // `xgbe_set_mac_address`: the low word takes bytes 0..3 and the high
    // word bytes 4..5, each little-endian within its word. Splitting it
    // evenly --- the obvious guess --- puts byte 3 in the wrong register
    // and the card answers to an address nobody assigned it.
    let (hi, lo) = mac_to_regs([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    assert_eq!(lo, 0x3322_1100);
    assert_eq!(hi, 0x0000_5544);
}

#[test]
fn the_high_word_of_address_zero_leaves_room_for_the_enable_bit() {
    // The high register is a half-filled word: its top sixteen bits stay
    // zero, which is where the additional addresses' AE bit lives. So a
    // station address can never set AE by accident, and address 0 --- the
    // one slot whose HR has no AE defined --- must not have it set.
    let (hi, _) = mac_to_regs([0xff; 6]);
    assert_eq!(hi, 0x0000_ffff);
    assert_eq!(hi >> 16, 0);
    assert_eq!(hi & (1 << MAC_MACA1HR_AE.0), 0);
}

#[test]
fn the_station_address_is_written_high_half_first() {
    // The order Linux uses, kept because the low word is the one that
    // latches the pair: writing low first leaves the chip briefly
    // filtering on an address whose top two bytes are stale.
    let fake = FakeXgmac::new();
    set_mac_address(&fake, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert_eq!(
        fake.log(),
        alloc::vec![
            Access::Write(MAC_MACA0HR, 0x0000_5634),
            Access::Write(MAC_MACA0LR, 0x1200_5452),
        ]
    );
}

#[test]
fn an_address_read_from_the_port_properties_survives_the_round_trip() {
    // The two halves this batch writes and the two the probe reads are
    // different registers with different layouts, so the only way to know
    // they agree is to put a MAC through both.
    let mac = [0x02, 0xde, 0xad, 0xbe, 0xef, 0x10];
    let (hi, lo) = mac_to_regs(mac);
    // `mac_from_prop` wants the port-property layout, which carries the
    // valid bit in the high word --- the MAC registers have no such bit,
    // which is the difference worth noticing.
    assert_eq!(mac_from_prop(lo, hi | (1 << 31)), Some(mac));
}

// ——— Turning the thing on ———

/// The offsets a write landed on, in order, ignoring the reads of each
/// read-modify-write.
fn written_offsets(fake: &FakeXgmac) -> alloc::vec::Vec<usize> {
    fake.log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(off, _) => Some(*off),
            _ => None,
        })
        .collect()
}

#[test]
fn the_mac_control_registers_are_where_linux_says() {
    assert_eq!(MAC_TCR, 0x0000);
    assert_eq!(MAC_RCR, 0x0004);
    assert_eq!(MAC_RQC0R, 0x00a0);
    assert_eq!(MAC_TCR_TE, (0, 1));
    assert_eq!(MAC_RCR_RE, (0, 1));
    assert_eq!(MAC_RCR_ACS, (1, 1));
    assert_eq!(MAC_RCR_CST, (2, 1));
    assert_eq!(MAC_RCR_DCRCC, (3, 1));
    assert_eq!(DMA_CH_TCR_ST, (0, 1));
    assert_eq!(DMA_CH_RCR_SR, (0, 1));
}

#[test]
fn the_queue_enable_value_is_two_not_one() {
    // TXQEN is TWO bits wide, and 1 in those two bits is
    // "enabled for average-bit-rate scheduling" --- a different mode, not
    // "on". Writing a boolean into it is what the field's width invites.
    assert_eq!(MTL_Q_TQOMR_TXQEN, (2, 2));
    assert_eq!(MTL_Q_ENABLED, 0x02);
    assert_eq!(MTL_Q_DISABLED, 0x00);
    assert_ne!(MTL_Q_ENABLED, 1);
}

#[test]
fn the_receive_queue_map_is_two_bits_per_queue() {
    // `reg_val |= (0x02 << (i << 1))` in xgbe_enable_rx.
    assert_eq!(rx_queue_enable_map(0), 0x0000_0000);
    assert_eq!(rx_queue_enable_map(1), 0x0000_0002);
    assert_eq!(rx_queue_enable_map(2), 0x0000_000a);
    assert_eq!(rx_queue_enable_map(4), 0x0000_00aa);
    // Eight queues is 0xaaaa, NOT 0xff: a one-bit-per-queue reading of
    // this register enables half the queues it meant to and puts the
    // other half into the wrong mode.
    assert_eq!(rx_queue_enable_map(8), 0x0000_aaaa);
    assert_ne!(rx_queue_enable_map(8), 0xff);
    assert_eq!(rx_queue_enable_map(16), 0xaaaa_aaaa);
}

#[test]
fn transmit_comes_up_from_the_inside_out() {
    // Channels, then queues, then the MAC. Enabling the MAC first opens
    // the line into a path that cannot move anything yet.
    let fake = FakeXgmac::new();
    enable_tx(&fake, 2, 3);
    assert_eq!(
        written_offsets(&fake),
        alloc::vec![
            dma_channel_reg(0, DMA_CH_TCR),
            dma_channel_reg(1, DMA_CH_TCR),
            mtl_queue_reg(0, MTL_Q_TQOMR),
            mtl_queue_reg(1, MTL_Q_TQOMR),
            mtl_queue_reg(2, MTL_Q_TQOMR),
            MAC_TCR,
        ]
    );
}

#[test]
fn the_transmit_enable_writes_the_right_values() {
    let fake = FakeXgmac::new();
    // A queue register that already carries its size and mode, so a
    // bare write instead of a read-modify-write would show up.
    fake.preload(mtl_queue_reg(0, MTL_Q_TQOMR), 0x01f0_0002);
    enable_tx(&fake, 1, 1);

    let tqomr = fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR));
    assert_eq!(
        get_bits(tqomr, MTL_Q_TQOMR_TXQEN.0, MTL_Q_TQOMR_TXQEN.1),
        MTL_Q_ENABLED
    );
    assert_eq!(tqomr & 0x01f0_0000, 0x01f0_0000, "the queue size survived");

    assert_eq!(
        get_bits(
            fake.read32(dma_channel_reg(0, DMA_CH_TCR)),
            DMA_CH_TCR_ST.0,
            DMA_CH_TCR_ST.1
        ),
        1
    );
    assert_eq!(
        get_bits(fake.read32(MAC_TCR), MAC_TCR_TE.0, MAC_TCR_TE.1),
        1
    );
}

#[test]
fn receive_comes_up_from_the_inside_out_too() {
    let fake = FakeXgmac::new();
    enable_rx(&fake, 2, 2);
    assert_eq!(
        written_offsets(&fake),
        alloc::vec![
            dma_channel_reg(0, DMA_CH_RCR),
            dma_channel_reg(1, DMA_CH_RCR),
            MAC_RQC0R,
            MAC_RCR, // DCRCC
            MAC_RCR, // CST
            MAC_RCR, // ACS
            MAC_RCR, // RE
        ]
    );
}

#[test]
fn the_receiver_is_enabled_after_the_frame_trimming_not_before() {
    // RE is the LAST of the four writes to MAC_RCR. Enabling it before
    // ACS and CST opens the line for the window in between, during which
    // frames arrive with their padding and FCS still attached --- not an
    // error anywhere, just a handful of over-long frames at the very
    // start of the link.
    let fake = FakeXgmac::new();
    enable_rx(&fake, 1, 1);
    let rcr_writes: alloc::vec::Vec<u32> = fake
        .log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(off, v) if *off == MAC_RCR => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(rcr_writes.len(), 4);
    // RE is off for the first three writes and on only in the last.
    for v in &rcr_writes[..3] {
        assert_eq!(get_bits(*v, MAC_RCR_RE.0, MAC_RCR_RE.1), 0);
    }
    let last = rcr_writes[3];
    assert_eq!(get_bits(last, MAC_RCR_RE.0, MAC_RCR_RE.1), 1);
    // And by then the trimming is already on and CRC checking is not
    // disabled.
    assert_eq!(get_bits(last, MAC_RCR_ACS.0, MAC_RCR_ACS.1), 1);
    assert_eq!(get_bits(last, MAC_RCR_CST.0, MAC_RCR_CST.1), 1);
    assert_eq!(get_bits(last, MAC_RCR_DCRCC.0, MAC_RCR_DCRCC.1), 0);
}

#[test]
fn a_chip_that_came_up_with_crc_checking_disabled_has_it_turned_back_on() {
    // DCRCC is "do NOT check the CRC", so Linux clearing it is the
    // meaningful direction. A chip, or a previous driver, that left it
    // set would otherwise keep delivering frames with bad CRCs.
    let fake = FakeXgmac::new();
    fake.preload(MAC_RCR, 1 << MAC_RCR_DCRCC.0);
    enable_rx(&fake, 1, 1);
    assert_eq!(
        get_bits(fake.read32(MAC_RCR), MAC_RCR_DCRCC.0, MAC_RCR_DCRCC.1),
        0
    );
}

#[test]
fn the_queue_map_is_written_whole_not_merged() {
    // `XGMAC_IOWRITE(pdata, MAC_RQC0R, reg_val)`: no read first. That is
    // also how a queue gets disabled --- xgbe_disable_rx writes a bare
    // zero --- so any queue not in the map is switched off by this same
    // write, whatever it was doing before.
    let fake = FakeXgmac::new();
    fake.preload(MAC_RQC0R, 0xaaaa_aaaa);
    enable_rx(&fake, 1, 2);
    assert_eq!(fake.read32(MAC_RQC0R), rx_queue_enable_map(2));
    // Exactly one write, and no read of it at all.
    assert_eq!(fake.writes_to(MAC_RQC0R), 1);
}

#[test]
fn a_channel_register_offset_is_not_a_channel_address() {
    // `dma_channel_reg` is relative to BAR0 for MmioAccess;
    // `channel_regs` takes a mapped address. Same arithmetic, different
    // starting point, and mixing them up is an access at a wild address.
    assert_eq!(dma_channel_reg(0, DMA_CH_TCR), DMA_CH_BASE + DMA_CH_TCR);
    assert_eq!(dma_channel_reg(3, DMA_CH_SR), DMA_CH_BASE + 3 * 0x80 + 0x60);
    let base = 0xffff_8000_0000_0000usize;
    assert_eq!(channel_regs(base, 3), base + dma_channel_reg(3, 0));
}

#[test]
fn no_channels_and_no_queues_still_leaves_the_mac_consistent() {
    // The geometry comes from MAC_HWF2R, which always reports at least
    // one of each, so zero is not a real chip --- but a caller that
    // passed zero must not end up with a MAC enabled over nothing.
    let fake = FakeXgmac::new();
    enable_tx(&fake, 0, 0);
    assert_eq!(written_offsets(&fake), alloc::vec![MAC_TCR]);
    let fake = FakeXgmac::new();
    enable_rx(&fake, 0, 0);
    // The queue map written is empty, so no receive queue is enabled
    // even though RE ends up set.
    assert_eq!(fake.read32(MAC_RQC0R), 0);
}

// ——— Turning it off ———

/// A poll budget: returns true while there is time left.
fn budget(n: &core::cell::Cell<u32>) -> impl FnMut() -> bool + '_ {
    move || {
        if n.get() == 0 {
            false
        } else {
            n.set(n.get() - 1);
            true
        }
    }
}

#[test]
fn the_debug_status_constants_are_where_linux_says() {
    assert_eq!(DMA_DSR0, 0x3020);
    assert_eq!(DMA_DSR1, 0x3024);
    assert_eq!(DMA_DSR_TPS_WIDTH, 4);
    // DMA_DSR_Q_WIDTH is RPS_WIDTH + TPS_WIDTH, both 4.
    assert_eq!(DMA_DSR_Q_WIDTH, 8);
    assert_eq!(DMA_DSR0_TPS_START, 12);
    assert_eq!(DMA_DSRX_FIRST_QUEUE, 3);
    assert_eq!(DMA_DSRX_INC, 4);
    assert_eq!(DMA_DSRX_QPR, 4);
    assert_eq!(DMA_DSRX_TPS_START, 4);
    assert_eq!(MTL_Q_TQDR, 0x08);
    assert_eq!(MTL_Q_RQDR, 0x48);
    assert_eq!(DMA_STOP_TIMEOUT_SECS, 1);
}

#[test]
fn the_queue_to_status_mapping_is_not_linear() {
    // Queues 0..2 are in DSR0 from bit 12, eight bits apart.
    assert_eq!(tx_dma_status_pos(0), (DMA_DSR0, 12));
    assert_eq!(tx_dma_status_pos(1), (DMA_DSR0, 20));
    assert_eq!(tx_dma_status_pos(2), (DMA_DSR0, 28));
    // And then it JUMPS: queue 3 is a different register at a different
    // starting offset, not "queue 2 plus eight bits". A linear reading
    // would say (DMA_DSR0, 36), which is not even inside the register.
    assert_eq!(tx_dma_status_pos(3), (DMA_DSR1, 4));
    assert_ne!(tx_dma_status_pos(3), (DMA_DSR0, 36));
    assert_eq!(tx_dma_status_pos(4), (DMA_DSR1, 12));
    assert_eq!(tx_dma_status_pos(6), (DMA_DSR1, 28));
    // Four queues to a register from DSR1 on, then the next register.
    assert_eq!(tx_dma_status_pos(7), (DMA_DSR1 + 4, 4));
    assert_eq!(tx_dma_status_pos(10), (DMA_DSR1 + 4, 28));
    assert_eq!(tx_dma_status_pos(11), (DMA_DSR1 + 8, 4));
}

#[test]
fn every_status_field_stays_inside_its_register() {
    for q in 0..16u32 {
        let (_, pos) = tx_dma_status_pos(q);
        assert!(
            pos + DMA_DSR_TPS_WIDTH <= 32,
            "queue {} runs past the end of its register",
            q
        );
    }
}

#[test]
fn stopped_and_suspended_are_two_values_of_sixteen_not_a_flag() {
    // TPS is a four-bit enumeration. "Quiet enough to switch off" is
    // exactly 0 and 6; everything else, 1 among them, is not.
    assert_eq!(DMA_TPS_STOPPED, 0x00);
    assert_eq!(DMA_TPS_SUSPENDED, 0x06);
    assert!(tx_dma_is_quiet(0));
    assert!(tx_dma_is_quiet(6));
    for state in [1u32, 2, 3, 4, 5, 7, 8, 15] {
        assert!(!tx_dma_is_quiet(state), "{} was treated as quiet", state);
    }
}

#[test]
fn a_transmit_queue_is_drained_when_it_is_not_reading_a_frame_out() {
    // `TRCSTS != 1 && TXQSTS == 0`. The read-controller test is NOT
    // `== 0`: TRCSTS is two bits and only the value 1 means "reading a
    // frame out right now".
    assert!(tx_queue_drained(0));
    // TRCSTS == 1: busy.
    assert!(!tx_queue_drained(1 << 1));
    // TRCSTS == 2 or 3: not busy, so drained as far as this test goes.
    assert!(tx_queue_drained(2 << 1));
    assert!(tx_queue_drained(3 << 1));
    // TXQSTS set: the queue still holds something.
    assert!(!tx_queue_drained(1 << 4));
    assert!(!tx_queue_drained((2 << 1) | (1 << 4)));
}

#[test]
fn a_receive_queue_is_drained_when_it_is_empty_in_both_senses() {
    assert!(rx_queue_drained(0));
    // PRXQ: bytes still in the FIFO.
    assert!(!rx_queue_drained(1 << 16));
    // RXQSTS: the fill state, two bits.
    assert!(!rx_queue_drained(1 << 4));
    assert!(!rx_queue_drained(3 << 4));
    // A bit outside both fields does not make a drained queue look busy.
    assert!(rx_queue_drained(1 << 7));
}

#[test]
fn a_newer_core_waits_on_the_queue_and_an_older_one_on_the_dma_status() {
    // The two paths read entirely different registers, so which one ran
    // is visible in the access log.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(10);
    assert_eq!(
        prepare_tx_stop(&fake, 0x30, 1, true, &mut budget(&n)),
        Drained::Yes
    );
    assert!(fake.reads_of(mtl_queue_reg(1, MTL_Q_TQDR)) > 0);
    assert_eq!(fake.reads_of(DMA_DSR0), 0);

    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(10);
    assert_eq!(
        prepare_tx_stop(&fake, 0x20, 1, true, &mut budget(&n)),
        Drained::Yes
    );
    assert!(fake.reads_of(DMA_DSR0) > 0);
    assert_eq!(fake.reads_of(mtl_queue_reg(1, MTL_Q_TQDR)), 0);
}

#[test]
fn with_no_link_nothing_is_waited_for() {
    // With no link the transmit path never completes, so waiting is a
    // guaranteed timeout on every queue in turn --- one second each, on
    // the path whose whole job is to bring the interface down.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(10);
    assert_eq!(
        prepare_tx_stop(&fake, 0x30, 0, false, &mut budget(&n)),
        Drained::NoLink
    );
    assert!(fake.log().is_empty());
    // And the budget was not spent either.
    assert_eq!(n.get(), 10);
}

#[test]
fn a_queue_that_never_drains_times_out_instead_of_spinning() {
    let fake = FakeXgmac::new();
    // TXQSTS stuck set: the queue always holds something.
    fake.preload(mtl_queue_reg(0, MTL_Q_TQDR), 1 << 4);
    let n = core::cell::Cell::new(5);
    assert_eq!(
        prepare_tx_stop(&fake, 0x30, 0, true, &mut budget(&n)),
        Drained::TimedOut
    );
    assert_eq!(n.get(), 0, "the whole budget was used");

    let fake = FakeXgmac::new();
    fake.preload(mtl_queue_reg(0, MTL_Q_RQDR), 1 << 16);
    let n = core::cell::Cell::new(5);
    assert_eq!(
        prepare_rx_stop(&fake, 0, &mut budget(&n)),
        Drained::TimedOut
    );
}

#[test]
fn a_receive_queue_is_drained_even_with_no_link() {
    // Linux's asymmetry, kept: what is in a receive queue came off the
    // wire already, so it drains whether or not the link is still up.
    // `prepare_rx_stop` has no link parameter at all, which is the
    // clearest way to say it.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(5);
    assert_eq!(prepare_rx_stop(&fake, 0, &mut budget(&n)), Drained::Yes);
    assert!(fake.reads_of(mtl_queue_reg(0, MTL_Q_RQDR)) > 0);
}

#[test]
fn transmit_goes_down_inside_out_with_the_mac_last() {
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    let mut complete = || true;
    disable_tx(&fake, 0x30, 2, 2, true, &mut complete, &mut budget(&n));
    assert_eq!(
        written_offsets(&fake),
        alloc::vec![
            dma_channel_reg(0, DMA_CH_TCR),
            dma_channel_reg(1, DMA_CH_TCR),
            mtl_queue_reg(0, MTL_Q_TQOMR),
            mtl_queue_reg(1, MTL_Q_TQOMR),
            MAC_TCR,
        ]
    );
    // The MAC was disabled last of all, after both queues.
    assert_eq!(*written_offsets(&fake).last().unwrap(), MAC_TCR);
    assert_eq!(
        get_bits(fake.read32(MAC_TCR), MAC_TCR_TE.0, MAC_TCR_TE.1),
        0
    );
    assert_eq!(
        get_bits(
            fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR)),
            MTL_Q_TQOMR_TXQEN.0,
            MTL_Q_TQOMR_TXQEN.1
        ),
        MTL_Q_DISABLED
    );
}

#[test]
fn transmit_waits_for_its_rings_before_it_stops_the_channels() {
    // The ring check is the driver's own: the chip does not report the
    // producer and consumer indices. It has to happen before the first
    // channel is switched off, or descriptors already handed over are
    // abandoned.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    let calls = core::cell::Cell::new(0u32);
    let mut complete = || {
        calls.set(calls.get() + 1);
        calls.get() >= 3
    };
    disable_tx(&fake, 0x30, 1, 1, true, &mut complete, &mut budget(&n));
    assert_eq!(calls.get(), 3);
    // And it was called before anything was written.
    assert!(!fake.log().is_empty());
}

#[test]
fn with_no_link_transmit_does_not_wait_for_rings_that_cannot_drain() {
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    let calls = core::cell::Cell::new(0u32);
    let mut complete = || {
        calls.set(calls.get() + 1);
        false
    };
    disable_tx(&fake, 0x30, 1, 1, false, &mut complete, &mut budget(&n));
    assert_eq!(calls.get(), 0, "the ring check ran without a link");
    assert_eq!(n.get(), 100, "the budget was spent without a link");
    // The interface still came all the way down.
    assert_eq!(*written_offsets(&fake).last().unwrap(), MAC_TCR);
}

#[test]
fn receive_goes_down_outside_in_with_the_mac_first() {
    // The opposite direction to disable_tx AND to enable_rx. Nothing new
    // may be accepted while the path behind it is torn down.
    let fake = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    disable_rx(&fake, 2, 2, &mut budget(&n));
    assert_eq!(
        written_offsets(&fake),
        alloc::vec![
            MAC_RCR, // CST
            MAC_RCR, // ACS
            MAC_RCR, // RE
            MAC_RQC0R,
            dma_channel_reg(0, DMA_CH_RCR),
            dma_channel_reg(1, DMA_CH_RCR),
        ]
    );
    assert_eq!(fake.read32(MAC_RQC0R), 0);
    assert_eq!(
        get_bits(fake.read32(MAC_RCR), MAC_RCR_RE.0, MAC_RCR_RE.1),
        0
    );
}

#[test]
fn disabling_is_not_enabling_reversed_for_transmit() {
    // The claim the module comment makes, as a test: both orders start
    // at the channels and end at the MAC.
    let up = FakeXgmac::new();
    enable_tx(&up, 2, 2);
    let down = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    let mut complete = || true;
    disable_tx(&down, 0x30, 2, 2, true, &mut complete, &mut budget(&n));
    assert_eq!(written_offsets(&up), written_offsets(&down));

    // Receive, by contrast, really is reversed.
    let up = FakeXgmac::new();
    enable_rx(&up, 2, 2);
    let down = FakeXgmac::new();
    let n = core::cell::Cell::new(100);
    disable_rx(&down, 2, 2, &mut budget(&n));
    // What reverses is the order of the LAYERS, not the iteration
    // within one: both still walk their channels 0, 1, .... So the
    // claim to check is which layer each sequence starts and ends on.
    let up_offsets = written_offsets(&up);
    let down_offsets = written_offsets(&down);
    assert_eq!(*up_offsets.first().unwrap(), dma_channel_reg(0, DMA_CH_RCR));
    assert_eq!(*up_offsets.last().unwrap(), MAC_RCR);
    assert_eq!(*down_offsets.first().unwrap(), MAC_RCR);
    assert_eq!(
        *down_offsets.last().unwrap(),
        dma_channel_reg(1, DMA_CH_RCR)
    );
}

// ——— Handing the chip its rings ———

#[test]
fn the_receive_buffer_size_field_does_not_start_at_bit_zero() {
    // RBSZ is at index 1. Bit 0 is SR, start-receive --- so a buffer
    // size written at index 0 would start the engine as a side effect of
    // being an odd number of bytes.
    assert_eq!(DMA_CH_RCR_RBSZ, (1, 14));
    assert_eq!(DMA_CH_RCR_RBSZ.0, DMA_CH_RCR_SR.0 + DMA_CH_RCR_SR.1);
    let fake = FakeXgmac::new();
    program_rx_ring(&fake, 0, 0x1000, 4, 2048).unwrap();
    let rcr = fake.read32(dma_channel_reg(0, DMA_CH_RCR));
    assert_eq!(get_bits(rcr, DMA_CH_RCR_RBSZ.0, DMA_CH_RCR_RBSZ.1), 2048);
    // And the engine was NOT started by writing the size.
    assert_eq!(get_bits(rcr, DMA_CH_RCR_SR.0, DMA_CH_RCR_SR.1), 0);
}

#[test]
fn the_ring_length_register_takes_a_last_index_not_a_count() {
    // `ring->rdesc_count - 1`. Writing the count gives the engine one
    // descriptor more than exists, which it fetches from whatever the
    // allocator put after the ring.
    let fake = FakeXgmac::new();
    program_tx_ring(&fake, 0, 0x2000, 32).unwrap();
    assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDRLR)), 31);

    let fake = FakeXgmac::new();
    program_rx_ring(&fake, 1, 0x3000, 64, 2048).unwrap();
    assert_eq!(fake.read32(dma_channel_reg(1, DMA_CH_RDRLR)), 63);
}

#[test]
fn an_empty_ring_is_refused_rather_than_written_as_minus_one() {
    // `count - 1` on zero wraps to 0xffffffff: a ring of four billion
    // descriptors, which is the worst possible thing to hand a bus
    // master.
    let fake = FakeXgmac::new();
    assert_eq!(
        program_tx_ring(&fake, 0, 0x2000, 0),
        Err(DeviceError::InvalidParam)
    );
    assert_eq!(
        program_rx_ring(&fake, 0, 0x2000, 0, 2048),
        Err(DeviceError::InvalidParam)
    );
    // A buffer size that does not fit the field is refused too, rather
    // than truncated to something smaller than the buffers actually are
    // --- which would overrun nothing, but would silently split every
    // frame.
    assert_eq!(
        program_rx_ring(&fake, 0, 0x2000, 4, 1 << 14),
        Err(DeviceError::InvalidParam)
    );
    assert_eq!(
        program_rx_ring(&fake, 0, 0x2000, 4, 0),
        Err(DeviceError::InvalidParam)
    );
    assert!(fake.log().is_empty());
}

#[test]
fn the_ring_address_goes_in_high_half_first() {
    let fake = FakeXgmac::new();
    program_tx_ring(&fake, 0, 0x0000_0007_dead_0000, 16).unwrap();
    assert_eq!(
        written_offsets(&fake),
        alloc::vec![
            dma_channel_reg(0, DMA_CH_TDRLR),
            dma_channel_reg(0, DMA_CH_TDLR_HI),
            dma_channel_reg(0, DMA_CH_TDLR_LO),
        ]
    );
    assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDLR_HI)), 7);
    assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDLR_LO)), 0xdead_0000);
}

#[test]
fn a_transmit_ring_gets_no_tail_pointer_and_a_receive_ring_does() {
    // A transmit ring starts EMPTY: the driver has nothing for the
    // engine to fetch, and the tail is written later when a frame is
    // queued. Writing one now would point the engine at descriptors
    // nobody has filled in.
    let fake = FakeXgmac::new();
    program_tx_ring(&fake, 0, 0x2000, 16).unwrap();
    assert_eq!(fake.writes_to(dma_channel_reg(0, DMA_CH_TDTR_LO)), 0);

    let fake = FakeXgmac::new();
    program_rx_ring(&fake, 0, 0x2000, 16, 2048).unwrap();
    assert_eq!(fake.writes_to(dma_channel_reg(0, DMA_CH_RDTR_LO)), 1);
}

#[test]
fn the_receive_tail_points_at_the_last_descriptor_not_past_the_end() {
    // The opposite of the usual convention: the tail is "the last
    // descriptor the driver owns", and a receive ring starts with every
    // descriptor handed over. One past the end would be an address
    // OUTSIDE the ring, and the engine would fetch a descriptor from
    // whatever the allocator put there next.
    let base = 0x0000_0001_0000_0000u64;
    let count = 8u32;
    let fake = FakeXgmac::new();
    program_rx_ring(&fake, 0, base, count, 2048).unwrap();

    let tail = fake.read32(dma_channel_reg(0, DMA_CH_RDTR_LO));
    let last = base + (count as u64 - 1) * 16;
    assert_eq!(tail, last as u32);
    // Explicitly NOT one past the end.
    assert_ne!(tail, (base + count as u64 * 16) as u32);
    // And it is inside the ring.
    assert!((tail as u64) < base as u32 as u64 + count as u64 * 16);
}

#[test]
fn the_tail_is_the_low_half_only() {
    // There is no RDTR_HI: the engine takes the high half from the list
    // address register. So the tail write is one word.
    let base = 0x0000_0009_8000_0000u64;
    let fake = FakeXgmac::new();
    program_rx_ring(&fake, 0, base, 2, 2048).unwrap();
    let expect = (base + 16) as u32;
    assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_RDTR_LO)), expect);
    // The high half appears only in RDLR_HI.
    assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_RDLR_HI)), 9);
}

#[test]
fn interrupt_coalescing_marks_the_last_descriptor_of_each_group() {
    // With nothing set, every descriptor interrupts.
    for i in 0..4u32 {
        assert!(rx_desc_wants_interrupt(i, 0, 0));
    }
    // With a frame count, `(index + 1) % rx_frames` --- so the LAST
    // descriptor of each group carries the flag, not the first. Marking
    // index 0 instead would interrupt one descriptor early, every time.
    assert!(!rx_desc_wants_interrupt(0, 0, 4));
    assert!(!rx_desc_wants_interrupt(1, 0, 4));
    assert!(!rx_desc_wants_interrupt(2, 0, 4));
    assert!(rx_desc_wants_interrupt(3, 0, 4));
    assert!(rx_desc_wants_interrupt(7, 0, 4));
    // With only a time budget set, no descriptor is marked: the timer
    // does the work.
    for i in 0..4u32 {
        assert!(!rx_desc_wants_interrupt(i, 50, 0));
    }
}

#[test]
fn a_receive_descriptor_carries_two_buffers_in_four_words() {
    // Header buffer in desc0/desc1, payload buffer in desc2/desc3, each
    // low-then-high. All four words are address.
    let hdr = 0x0000_0002_1111_0000u64;
    let buf = 0x0000_0003_2222_0000u64;
    let d = rx_desc_init(hdr, buf, false);
    assert_eq!(d.desc0, 0x1111_0000);
    assert_eq!(d.desc1, 2);
    assert_eq!(d.desc2, 0x2222_0000);
    assert_eq!(d.desc3, 3);
}

#[test]
fn the_control_bits_live_inside_an_address_word() {
    // desc3 is the high half of the payload address AND where OWN and
    // INTE live. It works because a physical address has nothing in its
    // top bits, but writing desc3 as "just the address" clears OWN, and
    // writing it as "just the flags" destroys the address.
    let buf = 0x0000_0003_2222_0000u64;
    let plain = rx_desc_init(0, buf, false);
    let with_inte = rx_desc_init(0, buf, true);
    assert_eq!(with_inte.desc3, plain.desc3 | DESC3_CTXT);
    // The address half survives the flag.
    assert_eq!(with_inte.desc3 & 0x0000_ffff, 3);
    assert!(rx_desc3_inte(with_inte.desc3));
    assert!(!rx_desc3_inte(plain.desc3));
}

#[test]
fn the_descriptor_is_not_handed_over_by_this_function() {
    // OWN is left off on purpose: Linux sets it in a separate write with
    // a barrier either side, because the receive engine may already be
    // running over this ring. Returning the descriptor without OWN makes
    // that ordering the caller's explicit step.
    let d = rx_desc_init(0x1000, 0x2000, true);
    assert!(!desc_owned_by_dma(d.desc3));
    // And handing it over is then one OR, which is what the caller does
    // after its barrier.
    assert!(desc_owned_by_dma(d.desc3 | DESC3_OWN));
}

// ——— Tanda 11: a frame in the transmit ring ———

fn fragment(dma: u64, len: u32) -> TxFragment {
    TxFragment { dma, len }
}

fn one_frame(fragments: &[TxFragment]) -> TxFrame<'_> {
    TxFrame {
        fragments,
        total_len: fragments.iter().map(|f| f.len).sum(),
        checksum_offload: false,
        wants_interrupt: false,
    }
}

#[test]
fn the_first_descriptor_is_the_only_one_left_unowned() {
    let frags = [
        fragment(0x1000, 100),
        fragment(0x2000, 200),
        fragment(0x3000, 300),
    ];
    let mut ring = [Desc::default(); 8];
    let next = build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    assert_eq!(next, 3);
    // Every descriptor but the first is handed over as it is built; the
    // first waits for the caller's barrier.
    assert!(!desc_owned_by_dma(ring[0].desc3));
    assert!(desc_owned_by_dma(ring[1].desc3));
    assert!(desc_owned_by_dma(ring[2].desc3));
}

#[test]
fn handing_the_frame_over_touches_only_the_first_descriptor() {
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    let before = ring;
    hand_tx_frame_to_dma(&mut ring, 0);
    assert_eq!(ring[0].desc3, before[0].desc3 | DESC3_OWN);
    assert_eq!(ring[1], before[1]);
    // And the rest of the first descriptor is untouched: OWN is an OR,
    // not a rewrite of the word.
    assert_eq!(ring[0].desc0, before[0].desc0);
    assert_eq!(ring[0].desc2, before[0].desc2);
}

#[test]
fn first_and_last_are_different_descriptors_and_different_bits() {
    let frags = [
        fragment(0x1000, 100),
        fragment(0x2000, 200),
        fragment(0x3000, 300),
    ];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    assert_eq!(ring[0].desc3 & DESC3_FD, DESC3_FD);
    assert_eq!(ring[0].desc3 & DESC3_LD, 0);
    assert_eq!(ring[1].desc3 & (DESC3_FD | DESC3_LD), 0);
    assert_eq!(ring[2].desc3 & DESC3_LD, DESC3_LD);
    assert_eq!(ring[2].desc3 & DESC3_FD, 0);
}

#[test]
fn a_frame_of_one_fragment_is_both_first_and_last() {
    let frags = [fragment(0x1000, 60)];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    assert_eq!(ring[0].desc3 & (DESC3_FD | DESC3_LD), DESC3_FD | DESC3_LD);
    // Still unowned: a one-fragment frame is handed over the same way.
    assert!(!desc_owned_by_dma(ring[0].desc3));
}

#[test]
fn the_two_lengths_have_different_scopes() {
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    // desc2 holds THIS fragment's length, on every descriptor.
    assert_eq!(
        get_bits(ring[0].desc2, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1),
        100
    );
    assert_eq!(
        get_bits(ring[1].desc2, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1),
        200
    );
    // desc3 holds the WHOLE frame's length, on the first only.
    assert_eq!(get_bits(ring[0].desc3, TX_DESC3_FL.0, TX_DESC3_FL.1), 300);
    assert_eq!(get_bits(ring[1].desc3, TX_DESC3_FL.0, TX_DESC3_FL.1), 0);
}

#[test]
fn the_interrupt_bit_is_in_desc2_not_desc3() {
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut frame = one_frame(&frags);
    frame.wants_interrupt = true;
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &frame).unwrap();
    // IC is bit 31 of desc2. Bit 31 of desc3 is OWN, and the last
    // descriptor has that too, so the test pins the WORD, not just the
    // position.
    assert_eq!(ring[1].desc2 & (1 << 31), 1 << 31);
    assert_eq!(ring[0].desc2 & (1 << 31), 0);
    // And without it asked for, nothing sets bit 31 of desc2 anywhere.
    let mut quiet = [Desc::default(); 8];
    build_tx_frame(&mut quiet, 0, &one_frame(&frags)).unwrap();
    assert_eq!(quiet[0].desc2 & (1 << 31), 0);
    assert_eq!(quiet[1].desc2 & (1 << 31), 0);
}

#[test]
fn the_checksum_bits_ride_every_descriptor() {
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut frame = one_frame(&frags);
    frame.checksum_offload = true;
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &frame).unwrap();
    // Linux sets CIC on the first descriptor and again in the loop over
    // the rest: it is not a per-frame flag that the first carries.
    for d in &ring[..2] {
        assert_eq!(
            get_bits(d.desc3, TX_DESC3_CIC.0, TX_DESC3_CIC.1),
            TX_DESC3_CIC_FULL
        );
    }
}

#[test]
fn the_crc_and_pad_field_is_left_at_zero() {
    let frags = [fragment(0x1000, 100)];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    // Zero is "insert CRC and pad", which is what an ordinary frame
    // wants. A stray one here would send frames without a CRC.
    assert_eq!(get_bits(ring[0].desc3, TX_DESC3_CPC.0, TX_DESC3_CPC.1), 0);
}

#[test]
fn the_address_is_split_low_half_first() {
    let frags = [fragment(0x0000_0001_dead_beef, 64)];
    let mut ring = [Desc::default(); 8];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    assert_eq!(ring[0].desc0, 0xdead_beef);
    assert_eq!(ring[0].desc1, 1);
}

#[test]
fn the_ring_wraps_and_the_next_index_wraps_with_it() {
    let frags = [
        fragment(0x1000, 100),
        fragment(0x2000, 200),
        fragment(0x3000, 300),
    ];
    let mut ring = [Desc::default(); 4];
    let next = build_tx_frame(&mut ring, 3, &one_frame(&frags)).unwrap();
    assert_eq!(next, 2);
    // Index 3 is the first, then 0 and 1.
    assert_eq!(ring[3].desc3 & DESC3_FD, DESC3_FD);
    assert!(!desc_owned_by_dma(ring[3].desc3));
    assert_eq!(ring[1].desc3 & DESC3_LD, DESC3_LD);
    // Descriptor 2 was never touched.
    assert_eq!(ring[2], Desc::default());
}

#[test]
fn a_frame_that_does_not_fit_the_ring_is_refused() {
    let frags = [
        fragment(0x1000, 10),
        fragment(0x2000, 10),
        fragment(0x3000, 10),
    ];
    let mut ring = [Desc::default(); 2];
    assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
    // Nothing was written before the refusal.
    assert_eq!(ring[0], Desc::default());
}

#[test]
fn the_lengths_must_agree_with_each_other() {
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut ring = [Desc::default(); 8];
    let mut frame = one_frame(&frags);
    frame.total_len = 299;
    // A frame length that disagrees with the fragments would make the
    // engine send 299 bytes out of 300 handed to it, silently.
    assert!(build_tx_frame(&mut ring, 0, &frame).is_err());
    frame.total_len = 300;
    assert!(build_tx_frame(&mut ring, 0, &frame).is_ok());
}

#[test]
fn an_empty_frame_and_an_empty_fragment_are_both_refused() {
    let mut ring = [Desc::default(); 8];
    assert!(build_tx_frame(&mut ring, 0, &one_frame(&[])).is_err());
    let frags = [fragment(0x1000, 0)];
    assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
}

#[test]
fn a_length_too_big_for_its_field_is_refused_not_truncated() {
    let mut ring = [Desc::default(); 8];
    // A fragment of exactly 1 << 14 would be written as zero.
    let frags = [fragment(0x1000, 1 << TX_DESC2_HL_B1L.1)];
    assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
    // And a frame length of 1 << 15 likewise, even split across
    // fragments that each fit.
    let big = [fragment(0x1000, 16383), fragment(0x2000, 16385)];
    assert!(build_tx_frame(&mut ring, 0, &one_frame(&big)).is_err());
}

#[test]
fn the_transmit_tail_points_past_the_frame_not_at_it() {
    let fake = FakeXgmac::new();
    let ring_paddr = 0x10_0000u64;
    let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
    let mut ring = [Desc::default(); 8];
    let next = build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    kick_tx(&fake, 0, desc_paddr(ring_paddr, next));
    // Two descriptors used, so the tail is descriptor 2: one PAST the
    // last one written. The receive tail of program_rx_ring points AT
    // its last descriptor instead.
    let expected = ring_paddr + 2 * core::mem::size_of::<Desc>() as u64;
    let log = fake.log();
    assert_eq!(log.len(), 1);
    assert_eq!(
        log[0],
        Access::Write(dma_channel_reg(0, DMA_CH_TDTR_LO), expected as u32)
    );
}

#[test]
fn the_transmit_tail_is_the_low_half_only() {
    let fake = FakeXgmac::new();
    // There is no TDTR_HI: the ring lives wholly inside the window the
    // TDLR pair established, so only the low half moves.
    kick_tx(&fake, 1, 0x0000_0007_1234_5678);
    let log = fake.log();
    assert_eq!(log.len(), 1);
    assert_eq!(
        log[0],
        Access::Write(dma_channel_reg(1, DMA_CH_TDTR_LO), 0x1234_5678)
    );
}

// ——— Tanda 12: reading a received frame back ———

/// A written-back descriptor: desc3 assembled from flags, the rest zero
/// unless a test puts something there.
fn written_back(desc3: u32) -> Desc {
    Desc {
        desc0: 0,
        desc1: 0,
        desc2: 0,
        desc3,
    }
}

fn whole_frame(extra: u32) -> Desc {
    written_back(DESC3_FD | DESC3_LD | extra)
}

#[test]
fn a_descriptor_the_chip_still_owns_yields_nothing() {
    // Every other word was written by the chip and is not settled until
    // OWN comes back, so the OWN test comes first and alone.
    let d = written_back(DESC3_OWN | DESC3_FD | DESC3_LD | 1500);
    assert_eq!(read_rx_desc(&d), RxRead::NotYours);
}

#[test]
fn a_context_descriptor_is_not_a_frame() {
    let d = written_back(DESC3_CTXT);
    assert_eq!(read_rx_desc(&d), RxRead::Context);
    // And CTXT is only read once OWN is clear: with OWN set it is still
    // the chip's.
    let busy = written_back(DESC3_OWN | DESC3_CTXT);
    assert_eq!(read_rx_desc(&busy), RxRead::NotYours);
}

#[test]
fn an_error_summary_on_its_own_is_not_an_error() {
    // This is the one that costs good frames. Linux: "no error if err is
    // 0 OR etlt is 0". ES set with ETLT zero is a CLEAN frame.
    let d = set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1);
    assert_eq!(rx_frame_verdict(d), RxVerdict::Clean);
    // And an ETLT on its own, with no error summary, likewise.
    let e = set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, 0x0c);
    assert_eq!(rx_frame_verdict(e), RxVerdict::Clean);
    // It takes both.
    assert_eq!(rx_frame_verdict(d | e), RxVerdict::FrameBad);
}

#[test]
fn a_bad_checksum_is_not_a_bad_frame() {
    let err = set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1);
    for etlt in [ETLT_IP_HEADER_CSUM, ETLT_PAYLOAD_CSUM] {
        let d = err | set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, etlt);
        assert_eq!(
            rx_frame_verdict(d),
            RxVerdict::ChecksumBad,
            "etlt {:#04x}",
            etlt
        );
    }
    // 0x09 and 0x0a only mean a TUNNEL checksum error on a tunnelled
    // frame; with no tunnel support they are damage, which is where
    // Linux puts them too.
    for etlt in [0x09u32, 0x0a] {
        let d = err | set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, etlt);
        assert_eq!(
            rx_frame_verdict(d),
            RxVerdict::FrameBad,
            "etlt {:#04x}",
            etlt
        );
    }
}

#[test]
fn the_same_etlt_value_is_a_vlan_tag_or_damage_depending_on_the_error_bit() {
    let etlt = set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, ETLT_VLAN_TAG);
    // Without the error summary: a VLAN tag, and desc0 carries it.
    let mut tagged = whole_frame(etlt);
    tagged.desc0 = 0xffff_0123;
    match read_rx_desc(&tagged) {
        RxRead::Normal(d) => {
            assert_eq!(d.verdict, Some(RxVerdict::Clean));
            // OVT is the low SIXTEEN bits of desc0, not all of it.
            assert_eq!(d.vlan_tag, Some(0x0123));
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
    // With it: damage, and no tag is reported.
    let mut bad = whole_frame(etlt | set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1));
    bad.desc0 = 0xffff_0123;
    match read_rx_desc(&bad) {
        RxRead::Normal(d) => {
            assert_eq!(d.verdict, Some(RxVerdict::FrameBad));
            assert_eq!(d.vlan_tag, None);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn the_two_lengths_are_valid_at_opposite_ends_of_the_frame() {
    // Same descriptor word layout, opposite conditions: HL only on the
    // first, PL only on the last. A middle descriptor has neither, and
    // whatever sits in those bits is leftover.
    let mut middle = written_back(0x3fff);
    middle.desc2 = 0x03ff;
    match read_rx_desc(&middle) {
        RxRead::Normal(d) => {
            assert!(!d.first);
            assert!(!d.last);
            assert_eq!(d.header_len, None);
            assert_eq!(d.packet_len, None);
            assert_eq!(d.verdict, None);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn the_first_descriptor_carries_the_header_length_and_the_last_the_frame_length() {
    let mut first = written_back(DESC3_FD | 1234);
    first.desc2 = 54;
    match read_rx_desc(&first) {
        RxRead::Normal(d) => {
            assert_eq!(d.header_len, Some(54));
            // 1234 is sitting in PL's bits, but this is not the last
            // descriptor, so it is not a frame length.
            assert_eq!(d.packet_len, None);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
    let mut last = written_back(DESC3_LD | 1234);
    last.desc2 = 54;
    match read_rx_desc(&last) {
        RxRead::Normal(d) => {
            assert_eq!(d.packet_len, Some(1234));
            assert_eq!(d.header_len, None);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn the_header_length_field_is_ten_bits_not_fourteen() {
    // desc2's length field on the way back is TEN bits wide, while the
    // transmit side's length in the same word is fourteen.
    let mut d = written_back(DESC3_FD);
    d.desc2 = 0xffff_ffff;
    match read_rx_desc(&d) {
        RxRead::Normal(r) => assert_eq!(r.header_len, Some(0x3ff)),
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn desc1_is_a_hash_only_when_the_chip_says_so() {
    // On the way in, desc1 was the high half of the header address. The
    // chip overwrites it, but only RSV says the new value means
    // anything.
    let mut quiet = whole_frame(0);
    quiet.desc1 = 0xdead_beef;
    match read_rx_desc(&quiet) {
        RxRead::Normal(d) => {
            assert_eq!(d.rss_hash, None);
            assert!(!d.rss_covers_ports);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
    let mut hashed = whole_frame(set_bits(0, RX_DESC3_RSV.0, RX_DESC3_RSV.1, 1));
    hashed.desc1 = 0xdead_beef;
    match read_rx_desc(&hashed) {
        RxRead::Normal(d) => assert_eq!(d.rss_hash, Some(0xdead_beef)),
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn only_the_four_transport_types_hash_over_the_ports() {
    for l34t in [L34T_IPV4_TCP, L34T_IPV4_UDP, L34T_IPV6_TCP, L34T_IPV6_UDP] {
        assert!(rss_hash_covers_ports(l34t), "l34t {}", l34t);
    }
    // ICMP (3, 11) and the two "unknown"s (7, 15) hash over the
    // addresses alone, as does anything else the field can hold.
    for l34t in [0u32, 3, 7, 11, 15, 4, 8, 12] {
        assert!(!rss_hash_covers_ports(l34t), "l34t {}", l34t);
    }
}

#[test]
fn the_l34_type_is_only_read_when_there_is_a_hash() {
    // L34T holds a protocol whether or not RSV is set, so reading it
    // unconditionally would report a port hash on a frame that has no
    // hash at all.
    let l34t = set_bits(0, RX_DESC3_L34T.0, RX_DESC3_L34T.1, L34T_IPV4_TCP);
    match read_rx_desc(&whole_frame(l34t)) {
        RxRead::Normal(d) => {
            assert_eq!(d.rss_hash, None);
            assert!(!d.rss_covers_ports);
        }
        other => panic!("not a normal descriptor: {:?}", other),
    }
    let rsv = set_bits(0, RX_DESC3_RSV.0, RX_DESC3_RSV.1, 1);
    match read_rx_desc(&whole_frame(l34t | rsv)) {
        RxRead::Normal(d) => assert!(d.rss_covers_ports),
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn a_context_descriptor_is_announced_a_descriptor_ahead() {
    let cda = set_bits(0, RX_DESC3_CDA.0, RX_DESC3_CDA.1, 1);
    match read_rx_desc(&whole_frame(cda)) {
        RxRead::Normal(d) => assert!(d.context_next),
        other => panic!("not a normal descriptor: {:?}", other),
    }
    match read_rx_desc(&whole_frame(0)) {
        RxRead::Normal(d) => assert!(!d.context_next),
        other => panic!("not a normal descriptor: {:?}", other),
    }
}

#[test]
fn the_flag_bits_do_not_collide_with_each_other() {
    // FD, LD, CDA, RSV, CTXT and OWN all live in desc3's top byte, and
    // ETLT, ES and L34T below them. Setting each alone must leave the
    // rest of the reading unchanged.
    let fields = [
        RX_DESC3_ES,
        RX_DESC3_ETLT,
        RX_DESC3_L34T,
        RX_DESC3_RSV,
        RX_DESC3_CDA,
    ];
    for (i, a) in fields.iter().enumerate() {
        let word = set_bits(0, a.0, a.1, (1 << a.1) - 1);
        for (j, b) in fields.iter().enumerate() {
            if i == j {
                continue;
            }
            assert_eq!(get_bits(word, b.0, b.1), 0, "{:?} bleeds into {:?}", a, b);
        }
        // ... and none of them reaches OWN, CTXT, FD or LD.
        assert_eq!(word & (DESC3_OWN | DESC3_CTXT | DESC3_FD | DESC3_LD), 0);
    }
}

#[test]
fn the_tunnel_bit_is_named_but_not_acted_on() {
    assert!(rx_is_tunnelled(1 << 11));
    assert!(!rx_is_tunnelled(0));
    // It is bit 11 of desc2, above the ten-bit header length, so a full
    // header length does not look like a tunnel.
    assert!(!rx_is_tunnelled(0x3ff));
}

// ——— Tanda 13: interrupts ———

#[test]
fn bit_fifteen_is_a_different_summary_on_each_core() {
    // The headline trap. On a core below 0x21 bit 15 is the ABNORMAL
    // summary; from 0x21 it is the NORMAL one.
    let (old_n, old_a) = summary_enables(0x20);
    let (new_n, new_a) = summary_enables(0x21);
    assert_eq!(old_a.0, 15);
    assert_eq!(new_n.0, 15);
    assert_eq!(old_n.0, 16);
    assert_eq!(new_a.0, 14);
    // So the two layouts share no summary bit meaning at all.
    assert_ne!(old_n.0, new_n.0);
    assert_ne!(old_a.0, new_a.0);
}

#[test]
fn the_new_layout_written_to_an_old_core_would_enable_the_wrong_summary() {
    // Spelled out, because this is what the version check buys. The new
    // mask's "normal summary" bit is the old core's ABNORMAL one, and
    // the old core's normal summary is left off entirely.
    let new_mask = channel_interrupt_mask(0x21, true, true);
    let (old_n, old_a) = summary_enables(0x20);
    assert_ne!(new_mask & (1 << old_a.0), 0, "would enable the abnormal");
    assert_eq!(new_mask & (1 << old_n.0), 0, "and never the normal");
    // The right mask for that core has it the other way round.
    let old_mask = channel_interrupt_mask(0x20, true, true);
    assert_ne!(old_mask & (1 << old_n.0), 0);
    assert_ne!(old_mask & (1 << old_a.0), 0);
}

#[test]
fn everything_below_the_summaries_stayed_put() {
    // Only the two summary bits moved. A test that let the rest drift
    // with them would hide the next version's change.
    let old = channel_interrupt_mask(0x20, true, true);
    let new = channel_interrupt_mask(0x21, true, true);
    let below = (1 << 14) - 1;
    assert_eq!(old & below, new & below);
    assert_eq!(
        old & below,
        (1 << DMA_CH_IER_FBEE.0)
            | (1 << DMA_CH_IER_TIE.0)
            | (1 << DMA_CH_IER_RBUE.0)
            | (1 << DMA_CH_IER_RIE.0)
    );
}

#[test]
fn a_dry_receive_ring_is_reported_and_a_dry_transmit_ring_is_not() {
    let mask = channel_interrupt_mask(0x21, true, true);
    assert_ne!(mask & (1 << DMA_CH_IER_RBUE.0), 0);
    // No TBUE: an idle interface's transmit ring is empty all the time,
    // and an interrupt for it would fire forever.
    assert_eq!(mask & (1 << DMA_CH_IER_TBUE.0), 0);
}

#[test]
fn a_channel_with_no_ring_gets_only_the_summaries_and_the_bus_error() {
    let mask = channel_interrupt_mask(0x21, false, false);
    let (nie, aie) = summary_enables(0x21);
    assert_eq!(mask, (1 << nie.0) | (1 << aie.0) | (1 << DMA_CH_IER_FBEE.0));
    // Transmit only, and receive only, each add just their own.
    let tx = channel_interrupt_mask(0x21, true, false);
    assert_eq!(tx, mask | (1 << DMA_CH_IER_TIE.0));
    let rx = channel_interrupt_mask(0x21, false, true);
    assert_eq!(
        rx,
        mask | (1 << DMA_CH_IER_RBUE.0) | (1 << DMA_CH_IER_RIE.0)
    );
}

#[test]
fn the_fatal_bus_error_is_enabled_whatever_the_channel_carries() {
    for (tx, rx) in [(false, false), (true, false), (false, true), (true, true)] {
        for ver in [0x20u8, 0x21, 0x30] {
            let mask = channel_interrupt_mask(ver, tx, rx);
            assert_ne!(mask & (1 << DMA_CH_IER_FBEE.0), 0, "{} {} {}", ver, tx, rx);
        }
    }
}

#[test]
fn acknowledging_writes_back_exactly_what_it_read() {
    let fake = FakeXgmac::new();
    fake.preload(dma_channel_reg(2, DMA_CH_SR), 0x0000_1041);
    let seen = ack_channel_interrupts(&fake, 2);
    assert_eq!(seen, 0x0000_1041);
    // Write-one-to-clear: a zero would clear nothing and all ones would
    // acknowledge events that never happened.
    let log = fake.log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0], Access::Read(dma_channel_reg(2, DMA_CH_SR)));
    assert_eq!(
        log[1],
        Access::Write(dma_channel_reg(2, DMA_CH_SR), 0x0000_1041)
    );
}

#[test]
fn enabling_clears_the_stale_bits_before_it_arms_anything() {
    let fake = FakeXgmac::new();
    fake.preload(dma_channel_reg(0, DMA_CH_SR), 0x0000_0041);
    enable_channel_interrupts(&fake, 0, 0x21, true, true);
    let log = fake.log();
    // Read status, clear it, THEN arm. The other order fires the handler
    // the instant the mask lands, for an event from before the interface
    // was up.
    assert_eq!(log.len(), 3);
    assert_eq!(log[0], Access::Read(dma_channel_reg(0, DMA_CH_SR)));
    assert_eq!(
        log[1],
        Access::Write(dma_channel_reg(0, DMA_CH_SR), 0x0000_0041)
    );
    assert_eq!(
        log[2],
        Access::Write(
            dma_channel_reg(0, DMA_CH_IER),
            channel_interrupt_mask(0x21, true, true)
        )
    );
}

#[test]
fn the_mask_is_written_whole_and_not_merged_in() {
    let fake = FakeXgmac::new();
    // A configuration from before, with the OTHER version's summary bit
    // set. A read-modify-write would keep it.
    fake.preload(dma_channel_reg(1, DMA_CH_IER), 0xffff_ffff);
    enable_channel_interrupts(&fake, 1, 0x21, true, true);
    assert_eq!(
        fake.reads_of(dma_channel_reg(1, DMA_CH_IER)),
        0,
        "the enable register must never be read"
    );
    let written = fake
        .log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(off, v) if *off == dma_channel_reg(1, DMA_CH_IER) => Some(*v),
            _ => None,
        })
        .last();
    assert_eq!(written, Some(channel_interrupt_mask(0x21, true, true)));
}

#[test]
fn disabling_needs_no_version_because_zero_is_zero() {
    let fake = FakeXgmac::new();
    disable_channel_interrupts(&fake, 3);
    let log = fake.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], Access::Write(dma_channel_reg(3, DMA_CH_IER), 0));
}

#[test]
fn the_status_bits_decode_one_at_a_time() {
    // Each bit alone, so a wrong index shows up as the wrong flag rather
    // than disappearing into a combination.
    let cases: [(u32, fn(&DmaStatus) -> bool); 7] = [
        (DMA_CH_SR_TI.0, |s| s.tx_complete),
        (DMA_CH_SR_TPS.0, |s| s.tx_stopped),
        (DMA_CH_SR_TBU.0, |s| s.tx_buffer_unavailable),
        (DMA_CH_SR_RI.0, |s| s.rx_complete),
        (DMA_CH_SR_RBU.0, |s| s.rx_buffer_unavailable),
        (DMA_CH_SR_RPS.0, |s| s.rx_stopped),
        (DMA_CH_SR_FBE.0, |s| s.fatal_bus_error),
    ];
    for (bit, get) in cases {
        let s = decode_dma_status(1 << bit);
        assert!(get(&s), "bit {} did not set its flag", bit);
        assert!(!dma_status_is_quiet(&s));
        // ... and nothing else came on with it.
        let mut others = 0;
        for (other, get_other) in cases {
            if other != bit && get_other(&s) {
                others += 1;
            }
        }
        assert_eq!(others, 0, "bit {} set more than its own flag", bit);
    }
}

#[test]
fn a_status_of_nothing_is_a_real_case() {
    // On a shared line another device's interrupt brings the handler
    // here too, and the handler must not treat that as a received frame.
    let s = decode_dma_status(0);
    assert_eq!(s, DmaStatus::default());
    assert!(dma_status_is_quiet(&s));
    // Bits the driver does not decode do not make it noisy either.
    assert!(dma_status_is_quiet(&decode_dma_status(1 << 20)));
}

// ——— Tanda 14: sharing the FIFO out ———

#[test]
fn the_two_size_fields_are_different_widths_at_the_same_index() {
    // TQS is ten bits and RQS is nine, both at index 16 of their own
    // register. A helper that read the width off either side would halve or
    // overflow the other's largest queue.
    assert_eq!(MTL_Q_TQOMR_TQS.0, MTL_Q_RQOMR_RQS.0);
    assert_eq!(MTL_Q_TQOMR_TQS.1, 10);
    assert_eq!(MTL_Q_RQOMR_RQS.1, 9);
}

#[test]
fn zero_in_the_register_means_one_unit_not_none() {
    // 256 bytes and 511 bytes are the same value, which is right: the
    // hardware cannot express the difference.
    assert_eq!(fifo_units(256), 0);
    assert_eq!(fifo_units(511), 0);
    assert_eq!(fifo_units(512), 1);
    assert_eq!(fifo_units(FIFO_MIN_ALLOC), 7);
}

#[test]
fn less_than_a_unit_still_comes_out_as_one_unit() {
    // The asymmetry worth naming: under 256 bytes the value is still zero,
    // which grants the queue 256 bytes it was never given. Linux does the
    // same, because a queue with no FIFO does not work either.
    assert_eq!(fifo_units(0), 0);
    assert_eq!(fifo_units(255), 0);
    assert_eq!(fifo_units(1), 0);
}

#[test]
fn the_remainder_of_the_split_is_dropped_not_given_to_queue_zero() {
    let fake = FakeXgmac::new();
    // 10 KiB over three queues is 3413 bytes each, which is 13 units and a
    // bit; the bit goes nowhere.
    config_tx_fifo(&fake, 10 * 1024, 3).unwrap();
    let values: Vec<u32> = (0..3)
        .map(|q| fake.read32(mtl_queue_reg(q, MTL_Q_TQOMR)))
        .map(|v| get_bits(v, MTL_Q_TQOMR_TQS.0, MTL_Q_TQOMR_TQS.1))
        .collect();
    assert_eq!(values, [12, 12, 12]);
    // Queue zero is not special.
    assert_eq!(values[0], values[2]);
}

#[test]
fn a_fifo_split_over_no_queues_is_refused() {
    let fake = FakeXgmac::new();
    assert!(equal_fifo_share(65536, 0).is_err());
    assert!(config_tx_fifo(&fake, 65536, 0).is_err());
    assert!(config_rx_fifo(&fake, 65536, 0).is_err());
}

#[test]
fn only_the_size_field_of_the_queue_register_is_touched() {
    let fake = FakeXgmac::new();
    // The queue operation registers hold other things; the size is written
    // as a field, not as the whole word.
    fake.preload(mtl_queue_reg(0, MTL_Q_TQOMR), 0x0000_0003);
    config_tx_fifo(&fake, 64 * 1024, 1).unwrap();
    let word = fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR));
    assert_eq!(word & 0xffff, 0x0003, "the low half was clobbered");
    assert_eq!(get_bits(word, MTL_Q_TQOMR_TQS.0, MTL_Q_TQOMR_TQS.1), 255);
}

#[test]
fn the_prio_queue_count_saturates_at_eight() {
    assert_eq!(prio_queues(0), 0);
    assert_eq!(prio_queues(4), 4);
    assert_eq!(prio_queues(8), 8);
    assert_eq!(prio_queues(16), 8);
}

#[test]
fn the_control_queues_take_their_minimum_off_the_top_first() {
    let fake = FakeXgmac::new();
    // 16 receive queues on a 64 KiB FIFO: eight control queues take 2 KiB
    // each, leaving 48 KiB for the eight priority ones.
    config_rx_fifo(&fake, 64 * 1024, 16).unwrap();
    let value = |q| {
        get_bits(
            fake.read32(mtl_queue_reg(q, MTL_Q_RQOMR)),
            MTL_Q_RQOMR_RQS.0,
            MTL_Q_RQOMR_RQS.1,
        )
    };
    // 48 KiB / 8 = 6 KiB = 24 units, minus one.
    for q in 0..8 {
        assert_eq!(value(q), 23, "priority queue {}", q);
    }
    for q in 8..16 {
        assert_eq!(value(q), 7, "control queue {}", q);
    }
    // And splitting the whole FIFO first would have promised each priority
    // queue 8 KiB, which is more FIFO than exists once the control queues
    // have taken theirs.
    assert_ne!(value(0), equal_fifo_share(64 * 1024, 8).unwrap());
}

#[test]
fn with_eight_queues_or_fewer_nothing_is_reserved() {
    let fake = FakeXgmac::new();
    config_rx_fifo(&fake, 64 * 1024, 8).unwrap();
    let share = equal_fifo_share(64 * 1024, 8).unwrap();
    for q in 0..8 {
        assert_eq!(
            get_bits(
                fake.read32(mtl_queue_reg(q, MTL_Q_RQOMR)),
                MTL_Q_RQOMR_RQS.0,
                MTL_Q_RQOMR_RQS.1,
            ),
            share
        );
    }
}

#[test]
fn a_fifo_the_control_queues_would_swallow_whole_is_refused() {
    let fake = FakeXgmac::new();
    // Sixteen queues means 16 KiB reserved. An 8 KiB FIFO cannot pay it, and
    // the priority queues would come out at zero units, which reads as 256
    // bytes each rather than as the error it is.
    assert!(config_rx_fifo(&fake, 8 * 1024, 16).is_err());
    assert!(config_rx_fifo(&fake, 16 * 1024, 16).is_err());
    assert!(config_rx_fifo(&fake, 17 * 1024, 16).is_ok());
}

#[test]
fn every_transmit_queue_is_a_priority_queue() {
    let fake = FakeXgmac::new();
    // The non-priority rule is receive-side only: sixteen transmit queues
    // all get an equal share, with nothing reserved.
    config_tx_fifo(&fake, 64 * 1024, 16).unwrap();
    let share = equal_fifo_share(64 * 1024, 16).unwrap();
    for q in 0..16 {
        assert_eq!(
            get_bits(
                fake.read32(mtl_queue_reg(q, MTL_Q_TQOMR)),
                MTL_Q_TQOMR_TQS.0,
                MTL_Q_TQOMR_TQS.1,
            ),
            share,
            "queue {}",
            q
        );
    }
}

// ——— Tanda 15: the flow-control water marks ———

/// A jumbo frame plus headers, which is the largest this driver would take.
const JUMBO: u32 = 9018;

#[test]
fn the_threshold_field_does_not_start_at_bit_zero() {
    // Bit 0 of RQFCR is not part of RFA. A threshold written from the bottom
    // of the word comes out halved.
    assert_eq!(MTL_Q_RQFCR_RFA.0, 1);
    assert_eq!(MTL_Q_RQFCR_RFA.1, 6);
    assert_eq!(MTL_Q_RQFCR_RFD.0, 17);
}

#[test]
fn the_unit_and_the_bias_are_not_the_fifos() {
    // Two neighbouring quantities in the same driver, both "units minus a
    // bias", with different units and different biases. Borrowing either
    // one's helper for the other is off by a factor of two and a constant.
    assert_eq!(FLOW_CONTROL_UNIT, 2 * FIFO_UNIT);
    assert_eq!(flow_control_value(1024), 0);
    assert_eq!(flow_control_value(1536), 1);
    assert_eq!(flow_control_value(2048), 2);
    // ... whereas the FIFO's bias is one over a 256-byte unit.
    assert_eq!(fifo_units(1024), 3);
}

#[test]
fn anything_under_two_units_is_the_bottom_of_the_scale() {
    for bytes in [0u32, 1, 511, 512, 1023] {
        assert_eq!(flow_control_value(bytes), 0, "{} bytes", bytes);
    }
    assert_eq!(flow_control_value(1024), 0);
}

#[test]
fn rounding_up_is_to_the_flow_control_unit_not_the_fifo_unit() {
    assert_eq!(flow_control_align(1), FLOW_CONTROL_UNIT);
    assert_eq!(flow_control_align(512), 512);
    assert_eq!(flow_control_align(513), 1024);
    assert_eq!(flow_control_align(9018), 9216);
    assert_eq!(flow_control_align(0), 0);
}

#[test]
fn a_queue_too_small_to_pause_in_gets_no_flow_control() {
    for size in [256u32, 1024, 2048] {
        let fc = queue_flow_control(size, JUMBO);
        assert_eq!(fc, FlowControl { rfa: 0, rfd: 0 }, "{} bytes", size);
        assert!(flow_control_is_off(&fc));
    }
}

#[test]
fn a_zero_in_rfa_is_a_real_threshold_and_only_rfd_means_off() {
    // The queue between 2 and 4 KiB has rfa == 0 and flow control very much
    // ON. Reading "off" off rfa would disable it on exactly the queues that
    // were configured for it.
    let fc = queue_flow_control(4096, JUMBO);
    assert_eq!(fc.rfa, 0);
    assert_eq!(fc.rfd, 1);
    assert!(!flow_control_is_off(&fc));
}

#[test]
fn resuming_always_waits_for_more_room_than_pausing() {
    // Both marks count backwards from full, so rfd above rfa means the queue
    // must drain FURTHER before the pause is lifted. The other way round is
    // a pause lifted the instant it is raised: a storm of pause frames.
    for size in [4096u32, 8192, 9216, 16384, 32768, 65536, 131072] {
        let fc = queue_flow_control(size, JUMBO);
        if flow_control_is_off(&fc) {
            continue;
        }
        assert!(fc.rfd > fc.rfa, "{} bytes: {:?}", size, fc);
    }
}

#[test]
fn the_four_bands_are_where_linux_puts_them() {
    let frame = flow_control_align(JUMBO);
    // Band two and three are fixed answers, not derived.
    assert_eq!(
        queue_flow_control(4096, JUMBO),
        FlowControl { rfa: 0, rfd: 1 }
    );
    assert_eq!(
        queue_flow_control(4097, JUMBO),
        FlowControl { rfa: 2, rfd: 5 }
    );
    assert_eq!(
        queue_flow_control(frame, JUMBO),
        FlowControl { rfa: 2, rfd: 5 }
    );
    // Band four is derived from the queue size, so neighbouring sizes differ.
    let a = queue_flow_control(frame + 1024, JUMBO);
    let b = queue_flow_control(frame * 2, JUMBO);
    assert_ne!(a, b);
    // Band five stops depending on the queue size: two frames plus a unit.
    let c = queue_flow_control(frame * 3 + 1, JUMBO);
    let d = queue_flow_control(frame * 10, JUMBO);
    assert_eq!(c, d);
    assert_eq!(c.rfa, flow_control_value(frame * 2 + FLOW_CONTROL_UNIT));
}

#[test]
fn the_thresholds_never_overflow_their_six_bit_fields() {
    // Linux leans on jumbo frames being the ceiling to skip the clamp, so
    // the test has to be the thing that checks it.
    let limit = 1u32 << MTL_Q_RQFCR_RFA.1;
    for size in [2049u32, 4096, 9216, 16384, 32768, 65536, 262144] {
        let fc = queue_flow_control(size, JUMBO);
        assert!(fc.rfa < limit, "{} bytes: rfa {}", size, fc.rfa);
        assert!(fc.rfd < limit, "{} bytes: rfd {}", size, fc.rfd);
    }
}

#[test]
fn both_marks_go_into_the_same_register_without_touching_each_other() {
    let fake = FakeXgmac::new();
    config_flow_control(
        &fake,
        2,
        &FlowControl {
            rfa: 0x3f,
            rfd: 0x2a,
        },
    );
    let word = fake.read32(mtl_queue_reg(2, MTL_Q_RQFCR));
    assert_eq!(get_bits(word, MTL_Q_RQFCR_RFA.0, MTL_Q_RQFCR_RFA.1), 0x3f);
    assert_eq!(get_bits(word, MTL_Q_RQFCR_RFD.0, MTL_Q_RQFCR_RFD.1), 0x2a);
    // Bit 0 is below RFA and must be left alone.
    assert_eq!(word & 1, 0);
}

#[test]
fn a_smaller_max_frame_moves_the_bands_with_it() {
    // The bands are measured in frames, so a 1518-byte interface pauses on
    // queue sizes a jumbo one would not even reach the third band at.
    let small = queue_flow_control(8192, 1518);
    let jumbo = queue_flow_control(8192, JUMBO);
    assert_ne!(small, jumbo);
    // 8 KiB is under a jumbo frame's aligned size, so the jumbo interface is
    // still on the fixed answer.
    assert_eq!(jumbo, FlowControl { rfa: 2, rfd: 5 });
    // ... while the small one has room for more than three frames.
    let frame = flow_control_align(1518);
    assert_eq!(small.rfa, flow_control_value(frame * 2 + FLOW_CONTROL_UNIT));
}

// ——— Tanda 16: who feeds whom ———

#[test]
fn the_leftovers_go_one_each_to_the_front_not_all_to_the_back() {
    // Eleven queues over four classes is 3, 3, 3, 2.
    let spans: Vec<(u32, u32)> = (0..4).map(|i| bucket_span(i, 11, 4)).collect();
    assert_eq!(spans, [(0, 3), (3, 3), (6, 3), (9, 2)]);
    // Not 2, 2, 2, 5: the classes are served in order, and piling the
    // leftovers on one makes that one behave differently under load.
    assert_ne!(spans[3].1, 5);
    // The spans tile the range exactly, with no gap and no overlap.
    let (last_start, last_len) = spans[3];
    assert_eq!(last_start + last_len, 11);
    for w in spans.windows(2) {
        assert_eq!(w[0].0 + w[0].1, w[1].0);
    }
}

#[test]
fn an_exact_division_leaves_nothing_over() {
    for i in 0..4u32 {
        assert_eq!(bucket_span(i, 8, 4), (i * 2, 2));
    }
}

#[test]
fn every_transmit_queue_lands_in_exactly_one_class() {
    for (queues, classes) in [(8u32, 4u32), (11, 4), (4, 4), (16, 3), (5, 2)] {
        let mut seen = [0u32; 8];
        for q in 0..queues {
            let tc = tc_of_tx_queue(q, queues, classes).unwrap();
            assert!(tc < classes, "{} queues {} classes", queues, classes);
            seen[tc as usize] += 1;
        }
        // No class is left without a queue: a class with nowhere to put a
        // frame is a class that drops it.
        let seen = &seen[..classes as usize];
        assert!(seen.iter().all(|&n| n > 0), "{:?}", seen);
        assert_eq!(seen.iter().sum::<u32>(), queues);
    }
}

#[test]
fn fewer_queues_than_classes_is_refused() {
    let fake = FakeXgmac::new();
    assert!(map_tx_queues_to_tc(&fake, 2, 4).is_err());
    assert!(map_tx_queues_to_tc(&fake, 4, 0).is_err());
    assert!(map_tx_queues_to_tc(&fake, 4, 4).is_ok());
}

#[test]
fn the_class_map_does_not_touch_the_fifo_size_in_the_same_register() {
    let fake = FakeXgmac::new();
    // The FIFO size of the batch before lives at index 16 of this same
    // register; the class map is at index 8, width 3.
    write_field(
        &fake,
        mtl_queue_reg(0, MTL_Q_TQOMR),
        MTL_Q_TQOMR_TQS.0,
        MTL_Q_TQOMR_TQS.1,
        0x1ff,
    );
    map_tx_queues_to_tc(&fake, 4, 4).unwrap();
    let word = fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR));
    assert_eq!(get_bits(word, MTL_Q_TQOMR_TQS.0, MTL_Q_TQOMR_TQS.1), 0x1ff);
    assert_eq!(
        get_bits(word, MTL_Q_TQOMR_Q2TCMAP.0, MTL_Q_TQOMR_Q2TCMAP.1),
        0
    );
}

#[test]
fn the_priority_map_is_a_mask_per_queue_not_a_queue_per_priority() {
    // Four priority queues over eight priorities: two priorities each, and
    // what goes in the byte is the MASK of those priorities.
    assert_eq!(prio_mask_for_queue(0, 4), 0b0000_0011);
    assert_eq!(prio_mask_for_queue(1, 4), 0b0000_1100);
    assert_eq!(prio_mask_for_queue(3, 4), 0b1100_0000);
    // A queue number would be a small integer; these are not.
    assert_ne!(prio_mask_for_queue(3, 4), 3);
}

#[test]
fn every_priority_is_claimed_by_exactly_one_queue() {
    for queues in 1..=8u32 {
        let mut all = 0u32;
        for q in 0..queues {
            let mask = prio_mask_for_queue(q, queues);
            assert_eq!(all & mask, 0, "{} queues: queue {} overlaps", queues, q);
            all |= mask;
        }
        assert_eq!(all, 0xff, "{} queues left a priority unmapped", queues);
    }
}

#[test]
fn four_queues_share_one_register_one_byte_each() {
    assert_eq!(packed_byte(MAC_RQC2R, MAC_RQC2_INC, 4, 0), (MAC_RQC2R, 0));
    assert_eq!(packed_byte(MAC_RQC2R, MAC_RQC2_INC, 4, 3), (MAC_RQC2R, 24));
    assert_eq!(
        packed_byte(MAC_RQC2R, MAC_RQC2_INC, 4, 4),
        (MAC_RQC2R + 4, 0)
    );
    assert_eq!(
        packed_byte(MAC_RQC2R, MAC_RQC2_INC, 4, 9),
        (MAC_RQC2R + 8, 8)
    );
}

#[test]
fn the_priority_map_goes_out_one_whole_register_at_a_time() {
    let fake = FakeXgmac::new();
    map_prios_to_rx_queues(&fake, 8).unwrap();
    // Eight priority queues, one priority each: two registers, four bytes
    // each.
    let log = fake.log();
    assert_eq!(log.len(), 2, "{:?}", log);
    assert_eq!(log[0], Access::Write(MAC_RQC2R, 0x0804_0201));
    assert_eq!(log[1], Access::Write(MAC_RQC2R + 4, 0x8040_2010));
}

#[test]
fn a_partly_filled_last_register_is_still_written() {
    let fake = FakeXgmac::new();
    // Six queues: a full register and then a half-full one, whose unused
    // bytes are zero rather than left behind.
    map_prios_to_rx_queues(&fake, 6).unwrap();
    let log = fake.log();
    assert_eq!(log.len(), 2, "{:?}", log);
    match log[1] {
        Access::Write(reg, value) => {
            assert_eq!(reg, MAC_RQC2R + 4);
            assert_eq!(value >> 16, 0, "the unused bytes were not cleared");
        }
        ref other => panic!("not a write: {:?}", other),
    }
}

#[test]
fn the_channel_map_writes_a_mode_bit_and_not_a_channel_number() {
    let fake = FakeXgmac::new();
    map_rx_queues_to_channels(&fake, 4).unwrap();
    let log = fake.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], Access::Write(MTL_RQDCM0R, 0x8080_8080));
    // 0x80 is the "map dynamically" bit, not channel 128. A channel index
    // written here instead would leave that bit clear and nail every queue
    // to one channel.
    assert_eq!(RQDCM_DYNAMIC & 0x7f, 0);
    assert_ne!(RQDCM_DYNAMIC, 0);
}

#[test]
fn the_two_packed_maps_live_in_different_register_banks() {
    // Same packing, different base: a helper pointed at the wrong bank would
    // write a priority mask into the channel map, where 0x80 is the only bit
    // that matters and the mask's bits are not it.
    assert_ne!(MAC_RQC2R, MTL_RQDCM0R);
    assert_eq!(MAC_RQC2_Q_PER_REG, MTL_RQDCM_Q_PER_REG);
    assert_eq!(MAC_RQC2_INC, MTL_RQDCM_INC);
}

#[test]
fn mapping_nothing_is_refused_rather_than_written_as_zero() {
    let fake = FakeXgmac::new();
    assert!(map_prios_to_rx_queues(&fake, 0).is_err());
    assert!(map_rx_queues_to_channels(&fake, 0).is_err());
    assert!(fake.log().is_empty());
}

// ——— Tanda 17: store and forward, and who goes first ———

#[test]
fn the_transmit_store_and_forward_bit_is_not_the_flush_bit() {
    // Bit 0 of TQOMR is FTQ. A store-and-forward bit written from the bottom
    // of the word empties the queue instead of configuring it.
    assert_eq!(MTL_Q_TQOMR_TSF.0, 1);
    assert_eq!(MTL_Q_TQOMR_FTQ.0, 0);
    assert_ne!(MTL_Q_TQOMR_TSF.0, MTL_Q_TQOMR_FTQ.0);
}

#[test]
fn the_two_store_and_forward_bits_are_four_apart() {
    // Same name on the two sides, four bits apart.
    assert_eq!(MTL_Q_TQOMR_TSF.0, 1);
    assert_eq!(MTL_Q_RQOMR_RSF.0, 5);
}

#[test]
fn the_two_thresholds_differ_in_index_and_in_width() {
    assert_eq!(MTL_Q_TQOMR_TTC, (4, 3));
    assert_eq!(MTL_Q_RQOMR_RTC, (0, 2));
    // And the receive threshold sits where the transmit register keeps its
    // flush bit, so the two registers cannot share a helper that takes the
    // position as read.
    assert_eq!(MTL_Q_RQOMR_RTC.0, MTL_Q_TQOMR_FTQ.0);
}

#[test]
fn store_and_forward_sets_the_bit_and_clears_the_threshold() {
    let fake = FakeXgmac::new();
    config_tx_forwarding(&fake, 2, Forwarding::StoreAndForward);
    for q in 0..2 {
        let word = fake.read32(mtl_queue_reg(q, MTL_Q_TQOMR));
        assert_eq!(get_bits(word, MTL_Q_TQOMR_TSF.0, MTL_Q_TQOMR_TSF.1), 1);
        assert_eq!(get_bits(word, MTL_Q_TQOMR_TTC.0, MTL_Q_TQOMR_TTC.1), 0);
        // And the flush bit was never touched.
        assert_eq!(get_bits(word, MTL_Q_TQOMR_FTQ.0, MTL_Q_TQOMR_FTQ.1), 0);
    }
}

#[test]
fn the_threshold_is_written_even_though_store_and_forward_ignores_it() {
    let fake = FakeXgmac::new();
    // A stale threshold from a previous configuration must not survive.
    write_field(
        &fake,
        mtl_queue_reg(0, MTL_Q_TQOMR),
        MTL_Q_TQOMR_TTC.0,
        MTL_Q_TQOMR_TTC.1,
        7,
    );
    config_tx_forwarding(&fake, 1, Forwarding::StoreAndForward);
    assert_eq!(
        get_bits(
            fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR)),
            MTL_Q_TQOMR_TTC.0,
            MTL_Q_TQOMR_TTC.1
        ),
        0
    );
}

#[test]
fn cut_through_clears_the_bit_and_keeps_the_threshold() {
    let fake = FakeXgmac::new();
    config_rx_forwarding(&fake, 1, Forwarding::CutThrough { threshold: 3 });
    let word = fake.read32(mtl_queue_reg(0, MTL_Q_RQOMR));
    assert_eq!(get_bits(word, MTL_Q_RQOMR_RSF.0, MTL_Q_RQOMR_RSF.1), 0);
    assert_eq!(get_bits(word, MTL_Q_RQOMR_RTC.0, MTL_Q_RQOMR_RTC.1), 3);
}

#[test]
fn the_forwarding_mode_does_not_disturb_the_fifo_size_beside_it() {
    let fake = FakeXgmac::new();
    config_tx_fifo(&fake, 64 * 1024, 1).unwrap();
    let before = get_bits(
        fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR)),
        MTL_Q_TQOMR_TQS.0,
        MTL_Q_TQOMR_TQS.1,
    );
    config_tx_forwarding(&fake, 1, Forwarding::StoreAndForward);
    assert_eq!(
        get_bits(
            fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR)),
            MTL_Q_TQOMR_TQS.0,
            MTL_Q_TQOMR_TQS.1
        ),
        before
    );
}

#[test]
fn both_scheduler_selections_are_zero() {
    // A step whose whole visible effect is writing zeroes looks like a
    // no-op and gets deleted. It is not: the register is not guaranteed to
    // come up this way.
    assert_eq!(MTL_ETSALG_WRR, 0);
    assert_eq!(MTL_RAA_SP, 0);
    let fake = FakeXgmac::new();
    fake.preload(MTL_OMR, 0xffff_ffff);
    config_mtl_scheduling(&fake);
    let word = fake.read32(MTL_OMR);
    assert_eq!(get_bits(word, MTL_OMR_ETSALG.0, MTL_OMR_ETSALG.1), 0);
    assert_eq!(get_bits(word, MTL_OMR_RAA.0, MTL_OMR_RAA.1), 0);
    // ... and the rest of a register full of ones is left as it was.
    assert_eq!(word & 0b11, 0b11);
}

#[test]
fn the_traffic_class_half_is_not_zero() {
    let fake = FakeXgmac::new();
    config_tc_weights(&fake, 2, 4).unwrap();
    for tc in 0..2 {
        assert_eq!(
            get_bits(
                fake.read32(mtl_tc_reg(tc, MTL_TC_ETSCR)),
                MTL_TC_ETSCR_TSA.0,
                MTL_TC_ETSCR_TSA.1
            ),
            MTL_TSA_ETS
        );
        assert_eq!(
            get_bits(
                fake.read32(mtl_tc_reg(tc, MTL_TC_QWR)),
                MTL_TC_QWR_QW.0,
                MTL_TC_QWR_QW.1
            ),
            1
        );
    }
    assert_ne!(MTL_TSA_ETS, 0);
}

#[test]
fn a_traffic_class_and_a_queue_share_one_register_block() {
    // MTL_TC_BASE is MTL_Q_BASE and the stride is the same, so class N's
    // registers are inside queue N's 0x80 block.
    assert_eq!(mtl_tc_reg(0, MTL_TC_ETSCR), mtl_queue_reg(0, 0x10));
    assert_eq!(mtl_tc_reg(3, MTL_TC_QWR), mtl_queue_reg(3, 0x18));
    // The class registers do not collide with the queue ones in that block.
    for tc in 0..4u32 {
        for &tc_reg in &[MTL_TC_ETSCR, MTL_TC_QWR] {
            assert_ne!(mtl_tc_reg(tc, tc_reg), mtl_queue_reg(tc, MTL_Q_TQOMR));
            assert_ne!(mtl_tc_reg(tc, tc_reg), mtl_queue_reg(tc, MTL_Q_RQOMR));
        }
    }
}

#[test]
fn a_class_above_the_queue_count_is_refused() {
    let fake = FakeXgmac::new();
    // It would land in another queue's block rather than failing loudly.
    assert!(config_tc_weights(&fake, 5, 4).is_err());
    assert!(config_tc_weights(&fake, 0, 4).is_err());
    assert!(fake.log().is_empty());
    assert!(config_tc_weights(&fake, 4, 4).is_ok());
}

// ——— Tanda 18: deciding which frames to keep ———

#[test]
fn the_hash_shift_is_a_log2_in_disguise() {
    // 26 - (size >> 7). Six index bits for 64 buckets, seven for 128, eight
    // for 256.
    assert_eq!(hash_table_shift(64), 26);
    assert_eq!(hash_table_shift(128), 25);
    assert_eq!(hash_table_shift(256), 24);
    for size in [64u32, 128, 256] {
        let bits = 32 - hash_table_shift(size);
        assert_eq!(1u32 << bits, size, "{} buckets", size);
    }
}

#[test]
fn hard_coding_the_largest_shift_would_fold_a_small_table() {
    // The index must stay inside the table it is indexing.
    for size in [64u32, 128, 256] {
        for crc in [0u32, 1, 0x1234_5678, 0xffff_ffff, 0xaaaa_aaaa] {
            let index = hash_index(crc, size);
            assert!(index < size, "{} buckets, crc {:#x}: {}", size, crc, index);
        }
    }
    // A 24-bit shift on a 64-bucket table gives indices up to 255, three
    // quarters of which fall off the end.
    let big = (!0x1234_5678u32).reverse_bits() >> 24;
    assert!(
        big >= 64,
        "this test needs a crc that overflows a small table"
    );
}

#[test]
fn the_three_steps_of_the_index_are_each_load_bearing() {
    let crc = 0x1234_5678u32;
    let size = 256;
    let right = hash_index(crc, size);
    // Without the complement.
    let no_complement = crc.reverse_bits() >> hash_table_shift(size);
    assert_ne!(right, no_complement);
    // Without the reversal: still a number, still in range, still wrong.
    let no_reversal = (!crc) >> hash_table_shift(size);
    assert!(no_reversal < size);
    assert_ne!(right, no_reversal);
}

#[test]
fn the_reversal_keeps_the_bits_that_came_in_at_the_bottom() {
    // Shifting a reversed word right keeps its top bits, which are the
    // BOTTOM bits of what came in. A low bit of the CRC therefore moves the
    // bucket, which is the whole point of a hash.
    let a = hash_index(0xffff_ff00, 256);
    let b = hash_index(0xffff_ff01, 256);
    assert_ne!(a, b, "the lowest crc bit did not reach the index");
}

#[test]
fn a_bucket_is_a_word_and_a_bit_within_it() {
    assert_eq!(hash_bucket(0), (0, 0));
    assert_eq!(hash_bucket(31), (0, 31));
    assert_eq!(hash_bucket(32), (1, 0));
    assert_eq!(hash_bucket(255), (7, 31));
    assert_eq!(hash_table_words(256), 8);
    assert_eq!(hash_table_words(64), 2);
}

#[test]
fn adding_an_address_sets_exactly_one_bit() {
    let mut table = [0u32; 8];
    let addr = [0x01u8, 0x00, 0x5e, 0x00, 0x00, 0x01];
    hash_table_add(&mut table, &addr, 256);
    assert_eq!(table.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
    // The same address again changes nothing.
    hash_table_add(&mut table, &addr, 256);
    assert_eq!(table.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
}

#[test]
fn the_shift_formula_and_the_shared_one_pick_the_same_bucket() {
    // `26 - (size >> 7)` here and `32 - log2(size)` there are the same
    // arithmetic written two ways, and the whole hash filter rests on it.
    use crate::net::ether_hash::{ether_crc32, hash_bucket_of};
    for addr in [
        [0x01u8, 0x00, 0x5e, 0x12, 0x34, 0x56],
        [0x33, 0x33, 0xff, 0x00, 0x00, 0x02],
    ] {
        for size in [64u32, 128, 256] {
            // The crate's value has the complement already applied, so what
            // goes into the Linux-shaped function is its complement back.
            let crc_le = !ether_crc32(&addr);
            assert_eq!(
                hash_index(crc_le, size),
                hash_bucket_of(&addr, size),
                "address {:02x?}, {} buckets",
                addr,
                size
            );
        }
    }
}

#[test]
fn only_as_many_words_as_the_table_has_are_written() {
    let fake = FakeXgmac::new();
    let table = [0xdead_beefu32; 8];
    config_hash_table(&fake, &table, 64);
    let log = fake.log();
    // A 64-bucket table is two words. Writing eight would scribble on the
    // registers after it.
    assert_eq!(log.len(), 2, "{:?}", log);
    assert_eq!(log[0], Access::Write(MAC_HTR0, 0xdead_beef));
    assert_eq!(log[1], Access::Write(MAC_HTR0 + MAC_HTR_INC, 0xdead_beef));
}

#[test]
fn the_address_slots_are_eight_bytes_apart_and_start_after_the_station() {
    // Slot zero here is MACA1, not MACA0: the station address of batch seven
    // is not one of the additional slots.
    assert_eq!(mac_addr_slot(0), (MAC_MACA1HR, MAC_MACA1HR + 4));
    assert_eq!(mac_addr_slot(1), (MAC_MACA1HR + 8, MAC_MACA1HR + 12));
    assert_ne!(mac_addr_slot(0).0, MAC_MACA0HR);
}

#[test]
fn a_slot_is_armed_only_once_the_whole_address_is_in_it() {
    let fake = FakeXgmac::new();
    config_mac_addr_slot(&fake, 0, Some([1, 2, 3, 4, 5, 6]));
    let log = fake.log();
    assert_eq!(log.len(), 2);
    // Low half first: the enable bit lives in the high half, so writing
    // that first would arm a slot still holding half of the old address.
    assert_eq!(log[0], Access::Write(MAC_MACA1HR + 4, 0x0403_0201));
    match log[1] {
        Access::Write(reg, value) => {
            assert_eq!(reg, MAC_MACA1HR);
            assert_eq!(value & 0xffff, 0x0605);
            assert_ne!(value & (1 << MAC_MACA1HR_AE.0), 0, "not armed");
        }
        ref other => panic!("not a write: {:?}", other),
    }
}

#[test]
fn clearing_a_slot_disarms_it_before_wiping_it() {
    let fake = FakeXgmac::new();
    config_mac_addr_slot(&fake, 1, None);
    let log = fake.log();
    assert_eq!(log.len(), 2);
    // High half first this time: the opposite order, for the opposite
    // reason.
    assert_eq!(log[0], Access::Write(MAC_MACA1HR + 8, 0));
    assert_eq!(log[1], Access::Write(MAC_MACA1HR + 12, 0));
}

#[test]
fn the_hash_pass_bit_rides_either_hash() {
    let fake = FakeXgmac::new();
    for (mc, uc) in [(true, false), (false, true), (true, true)] {
        let filter = RxFilter {
            hash_multicast: mc,
            hash_unicast: uc,
            ..RxFilter::default()
        };
        config_rx_filter(&fake, &filter);
        let word = fake.read32(MAC_PFR);
        assert_ne!(
            get_bits(word, MAC_PFR_HPF.0, MAC_PFR_HPF.1),
            0,
            "{} {}",
            mc,
            uc
        );
    }
    // Without it, the exact-match slots stop mattering the moment the hash
    // is on, and the station address would be filtered out by a hash it does
    // not land in.
    config_rx_filter(&fake, &RxFilter::default());
    assert_eq!(
        get_bits(fake.read32(MAC_PFR), MAC_PFR_HPF.0, MAC_PFR_HPF.1),
        0
    );
}

#[test]
fn the_filter_register_is_written_whole() {
    let fake = FakeXgmac::new();
    fake.preload(MAC_PFR, 0xffff_ffff);
    config_rx_filter(&fake, &RxFilter::default());
    // Every bit the driver knows about comes out zero, and so does
    // everything it does not: this register has no other tenants, and a
    // merge would keep a filter from the previous configuration.
    assert_eq!(fake.read32(MAC_PFR), 0);
}

#[test]
fn promiscuous_and_all_multicast_are_different_bits() {
    let fake = FakeXgmac::new();
    config_rx_filter(
        &fake,
        &RxFilter {
            promiscuous: true,
            ..RxFilter::default()
        },
    );
    let word = fake.read32(MAC_PFR);
    assert_eq!(get_bits(word, MAC_PFR_PR.0, MAC_PFR_PR.1), 1);
    assert_eq!(get_bits(word, MAC_PFR_PM.0, MAC_PFR_PM.1), 0);
    config_rx_filter(
        &fake,
        &RxFilter {
            all_multicast: true,
            ..RxFilter::default()
        },
    );
    let word = fake.read32(MAC_PFR);
    assert_eq!(get_bits(word, MAC_PFR_PR.0, MAC_PFR_PR.1), 0);
    assert_eq!(get_bits(word, MAC_PFR_PM.0, MAC_PFR_PM.1), 1);
}
