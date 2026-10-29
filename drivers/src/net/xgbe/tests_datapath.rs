//! Each constant below is pinned against the `amd-xgbe` source it was
//! taken from, named in the assertion. None of this has run on the
//! hardware, so the source IS the oracle: a test that merely repeated the
//! constant back would prove nothing, and the point of writing the Linux
//! name next to each value is that the next person can check it in one
//! grep. Read from torvalds/linux at v6.12 and at master on 28-10-2026.

use super::fake::*;
use super::*;
use alloc::vec::Vec;

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

// ——— Tanda 20: not interrupting on every frame ———

#[test]
fn the_watchdog_counts_the_dma_clock_and_not_a_wall_clock() {
    // 50 us at 500 MHz: 50 * 500 / 256.
    assert_eq!(usec_to_riwt(50, DMA_CLOCK_HZ), 50 * 500 / 256);
    // The same request on a part clocked a quarter as fast is a quarter of
    // the units, which is a quarter of the delay the register will keep.
    assert_ne!(
        usec_to_riwt(50, DMA_CLOCK_HZ),
        usec_to_riwt(50, PTP_CLOCK_HZ)
    );
    assert_eq!(usec_to_riwt(50, PTP_CLOCK_HZ), 50 * 125 / 256);
    // The two clocks are a factor of four apart, which is how far off
    // reaching for the wrong one puts you.
    assert_eq!(DMA_CLOCK_HZ / PTP_CLOCK_HZ, 4);
}

#[test]
fn the_rate_is_divided_down_to_megahertz_first() {
    // A clock that is not a whole number of MHz loses its fraction before
    // anything else happens, exactly as in Linux.
    assert_eq!(
        usec_to_riwt(100, 500_999_999),
        usec_to_riwt(100, 500_000_000)
    );
}

#[test]
fn the_conversion_is_clamped_rather_than_allowed_to_wrap() {
    // A value that wrapped would ask for a SHORTER delay than requested,
    // which is the wrong direction to fail.
    assert_eq!(usec_to_riwt(u32::MAX, DMA_CLOCK_HZ), MAX_DMA_RIWT);
    assert_eq!(usec_to_riwt(1_000_000, DMA_CLOCK_HZ), MAX_DMA_RIWT);
    // And it fits the field.
    assert!(MAX_DMA_RIWT < (1 << DMA_CH_RIWT_RWT.1));
}

#[test]
fn the_round_trip_loses_and_never_gains() {
    // Both directions truncate, so anything comparing a configured delay
    // against a read-back one has to expect to land at or below where it
    // started, not treat the difference as a refused setting.
    for usec in [1u32, 5, 10, 50, 100, 127] {
        let back = riwt_to_usec(usec_to_riwt(usec, DMA_CLOCK_HZ), DMA_CLOCK_HZ);
        assert!(back <= usec, "{} us came back as {}", usec, back);
    }
    // Small enough requests round all the way down to nothing.
    assert_eq!(usec_to_riwt(0, DMA_CLOCK_HZ), 0);
    assert_eq!(riwt_to_usec(0, DMA_CLOCK_HZ), 0);
}

#[test]
fn a_clock_below_a_megahertz_does_not_divide_by_zero() {
    assert_eq!(usec_to_riwt(50, 999_999), 0);
    assert_eq!(riwt_to_usec(50, 999_999), 0);
}

#[test]
fn the_watchdog_goes_to_every_receive_channel() {
    let fake = FakeXgmac::new();
    config_rx_coalesce(&fake, 3, 12);
    for ch in 0..3 {
        assert_eq!(
            get_bits(
                fake.read32(dma_channel_reg(ch, DMA_CH_RIWT)),
                DMA_CH_RIWT_RWT.0,
                DMA_CH_RIWT_RWT.1
            ),
            12,
            "channel {}",
            ch
        );
    }
    // And to no others.
    assert_eq!(fake.writes_to(dma_channel_reg(3, DMA_CH_RIWT)), 0);
}

#[test]
fn a_watchdog_too_big_for_the_field_is_clamped_at_the_register_too() {
    let fake = FakeXgmac::new();
    config_rx_coalesce(&fake, 1, 0x1234);
    assert_eq!(
        get_bits(
            fake.read32(dma_channel_reg(0, DMA_CH_RIWT)),
            DMA_CH_RIWT_RWT.0,
            DMA_CH_RIWT_RWT.1
        ),
        MAX_DMA_RIWT
    );
}

#[test]
fn there_is_no_transmit_watchdog_at_all() {
    let fake = FakeXgmac::new();
    config_tx_coalesce(&fake);
    // Not an omission: Linux's xgbe_config_tx_coalesce writes nothing
    // either. What limits transmit interrupts is the IC bit on a
    // descriptor, which is a per-frame choice and not a timer.
    assert!(fake.log().is_empty());
    let mut ring = [Desc::default(); 4];
    let frags = [fragment(0x1000, 60)];
    let mut frame = one_frame(&frags);
    frame.wants_interrupt = true;
    build_tx_frame(&mut ring, 0, &frame).unwrap();
    assert_ne!(ring[0].desc2 & (1 << TX_DESC2_IC.0), 0);
}

#[test]
fn the_two_halves_of_receive_coalescing_are_different_mechanisms() {
    // The microsecond half is this watchdog, in a register; the frame half
    // is the INTE bit of batch ten, in a descriptor. Asking for neither
    // means every descriptor interrupts.
    assert!(rx_desc_wants_interrupt(0, 0, 0));
    assert!(rx_desc_wants_interrupt(7, 0, 0));
    // With only a watchdog, the descriptors stop asking and the timer
    // carries it.
    assert!(!rx_desc_wants_interrupt(0, 50, 0));
    assert!(usec_to_riwt(50, DMA_CLOCK_HZ) > 0);
}

// ——— Tanda 21: cuanto puede medir una trama ———

#[test]
fn the_frame_overhead_counts_a_vlan_tag_that_may_not_be_there() {
    // `xgbe_calc_rx_buf_size`: mtu + ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN.
    assert_eq!(ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN, 22);
    // 18 would be the honest header-plus-FCS figure. Linux adds the tag
    // unconditionally, so a 1500 MTU wants 1522, not 1518.
    assert_eq!(1500 + ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN, 1522);
    // And that is exactly the floor, which is no coincidence:
    // XGBE_RX_MIN_BUF_SIZE is ETH_FRAME_LEN + ETH_FCS_LEN + VLAN_HLEN and
    // ETH_FRAME_LEN already has the header in it.
    assert_eq!(RX_MIN_BUF_SIZE, 1522);
    assert_eq!(ETH_FRAME_LEN, 1514);
}

#[test]
fn a_small_mtu_does_not_get_a_small_buffer() {
    // The clamp has a floor as well as a ceiling, so there is no MTU at
    // which the receive buffer drops below a standard frame.
    assert_eq!(calc_rx_buf_size(0), 1536);
    assert_eq!(calc_rx_buf_size(68), calc_rx_buf_size(1500));
    assert_eq!(calc_rx_buf_size(576), calc_rx_buf_size(1500));
    // 1522 rounded up to the next multiple of 64.
    assert_eq!(calc_rx_buf_size(1500), 1536);
}

#[test]
fn a_jumbo_mtu_gets_a_page_and_not_a_jumbo_buffer() {
    // This is the trap. A 9000-byte MTU wants 9022 bytes and is given
    // 4096, because Linux allocates one page per receive buffer and
    // clamps to PAGE_SIZE.
    assert_eq!(9000 + 22, 9022);
    assert_eq!(calc_rx_buf_size(9000), RX_MAX_BUF_SIZE);
    assert_eq!(RX_MAX_BUF_SIZE, 4096);
    // Which means the frame comes back in pieces, and a receiver that
    // assumed one descriptor per frame would hand the stack the first
    // 4096 bytes of a 9022-byte packet and call it whole.
    assert_eq!(rx_buffers_for_mtu(9000), 3);
    assert_eq!(rx_buffers_for_mtu(1500), 1);
    // The boundary: the last MTU that still fits one buffer.
    assert_eq!(rx_buffers_for_mtu(RX_MAX_BUF_SIZE - 22), 1);
    assert_eq!(rx_buffers_for_mtu(RX_MAX_BUF_SIZE - 21), 2);
}

#[test]
fn the_buffer_size_is_rounded_up_and_never_down() {
    for mtu in [1500u32, 1501, 1536, 2000, 3000, 4000, 4074, 9000] {
        let buf = calc_rx_buf_size(mtu);
        assert!(buf.is_multiple_of(RX_BUF_ALIGN), "mtu {} -> {}", mtu, buf);
        assert!(buf >= RX_MIN_BUF_SIZE);
        assert!(buf <= RX_MAX_BUF_SIZE);
    }
    // 1523 and 1536 land on the same buffer; 1537 does not.
    assert_eq!(calc_rx_buf_size(1514), 1536);
    assert_eq!(calc_rx_buf_size(1515), 1600);
}

#[test]
fn the_round_up_cannot_push_the_result_past_the_ceiling() {
    // The order of clamp and round-up only matters at the top, and it
    // only works out because the ceiling is itself 64-aligned.
    assert!(RX_MAX_BUF_SIZE.is_multiple_of(RX_BUF_ALIGN));
    assert_eq!(calc_rx_buf_size(u32::MAX), RX_MAX_BUF_SIZE);
    assert_eq!(calc_rx_buf_size(0xffff_fff0), RX_MAX_BUF_SIZE);
}

#[test]
fn the_buffer_size_always_fits_the_field_it_is_written_into() {
    // `RBSZ` is 14 bits, so the biggest value it can hold is 16383. The
    // clamp keeps every possible answer well inside it, which is why
    // `config_rx_buffer_size` can refuse anything bigger as a bug rather
    // than as a legal case it cannot serve.
    assert!(RX_MAX_BUF_SIZE < (1 << DMA_CH_RCR_RBSZ.1));
    let fake = FakeXgmac::new();
    assert!(config_rx_buffer_size(&fake, 2, 1 << DMA_CH_RCR_RBSZ.1).is_err());
    assert!(config_rx_buffer_size(&fake, 2, 0).is_err());
    // A refusal writes nothing.
    assert!(fake.log().is_empty());
}

#[test]
fn the_buffer_size_goes_to_every_receive_channel() {
    let fake = FakeXgmac::new();
    config_rx_buffer_size(&fake, 3, 1536).unwrap();
    for channel in 0..3 {
        let off = dma_channel_reg(channel, DMA_CH_RCR);
        // Read-modify-write, because `RBSZ` shares its word with `SR`.
        assert_eq!(fake.reads_of(off), 1);
        assert_eq!(fake.writes_to(off), 1);
    }
    // And nothing beyond the channels asked for.
    assert_eq!(fake.writes_to(dma_channel_reg(3, DMA_CH_RCR)), 0);
}

#[test]
fn the_buffer_size_does_not_disturb_the_start_bit_beside_it() {
    // `DMA_CH_RCR` bit 0 is `SR`, the receive start. That is why `RBSZ`
    // begins at bit 1 and why this is a field write.
    let fake = FakeXgmac::new();
    fake.preload(dma_channel_reg(0, DMA_CH_RCR), 1);
    config_rx_buffer_size(&fake, 1, 1536).unwrap();
    let written = match fake.log()[1] {
        Access::Write(_, v) => v,
        ref other => panic!("{:?}", other),
    };
    assert_eq!(written & 1, 1);
    assert_eq!(
        get_bits(written, DMA_CH_RCR_RBSZ.0, DMA_CH_RCR_RBSZ.1),
        1536
    );
}

#[test]
fn the_two_length_policemen_are_disable_bits_and_read_backwards() {
    // `WD` is the receive watchdog and `JD` the transmit jabber timer.
    // Both are *disable* bits, so the giant path sets them to 1.
    assert_eq!(MAC_RCR_WD, (7, 1));
    assert_eq!(MAC_TCR_JD, (16, 1));
    let fake = FakeXgmac::new();
    config_jumbo_enable(&fake, 9001);
    let rcr = fake.log();
    assert!(rcr.iter().any(|a| matches!(a, Access::Write(o, v)
        if *o == MAC_RCR && get_bits(*v, MAC_RCR_WD.0, MAC_RCR_WD.1) == 1)));
    assert!(rcr.iter().any(|a| matches!(a, Access::Write(o, v)
        if *o == MAC_TCR && get_bits(*v, MAC_TCR_JD.0, MAC_TCR_JD.1) == 1)));
}

#[test]
fn the_giant_path_never_touches_the_jumbo_bit() {
    // Above 9000, `JE` is not written at all: the fixed limit it raises
    // stops mattering once `GPSLCE` is comparing against `GPSL`.
    let fake = FakeXgmac::new();
    fake.preload(MAC_RCR, 0);
    config_jumbo_enable(&fake, 16000);
    let final_rcr = fake
        .log()
        .iter()
        .rev()
        .find_map(|a| match a {
            Access::Write(o, v) if *o == MAC_RCR => Some(*v),
            _ => None,
        })
        .unwrap();
    assert_eq!(get_bits(final_rcr, MAC_RCR_JE.0, MAC_RCR_JE.1), 0);
    assert_eq!(get_bits(final_rcr, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1), 1);
    assert_eq!(
        get_bits(final_rcr, MAC_RCR_GPSL.0, MAC_RCR_GPSL.1),
        GIANT_PACKET_MTU
    );
    // Four writes, each to its own field, and the chip ends up with all
    // four set at once.
    assert_eq!(get_bits(final_rcr, MAC_RCR_WD.0, MAC_RCR_WD.1), 1);
}

#[test]
fn the_three_regimes_are_three_different_register_states() {
    for (mtu, gpslce, wd, je) in [
        (1500u32, 0u32, 0u32, 0u32),
        (1501, 0, 0, 1),
        (9000, 0, 0, 1),
    ] {
        let fake = FakeXgmac::new();
        config_jumbo_enable(&fake, mtu);
        let rcr = fake
            .log()
            .iter()
            .rev()
            .find_map(|a| match a {
                Access::Write(o, v) if *o == MAC_RCR => Some(*v),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            get_bits(rcr, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1),
            gpslce,
            "mtu {}",
            mtu
        );
        assert_eq!(get_bits(rcr, MAC_RCR_WD.0, MAC_RCR_WD.1), wd, "mtu {}", mtu);
        assert_eq!(get_bits(rcr, MAC_RCR_JE.0, MAC_RCR_JE.1), je, "mtu {}", mtu);
    }
    // 9000 is still the normal path; 9001 is not.
    assert_eq!(JUMBO_PACKET_MTU, 9000);
}

#[test]
fn the_normal_path_clears_what_the_giant_path_set() {
    // A device that was at a giant MTU and is brought back down must not
    // keep the watchdogs off, so the normal path writes all four fields
    // rather than only the one it cares about.
    let fake = FakeXgmac::new();
    config_jumbo_enable(&fake, 16000);
    let after_giant = fake.log();
    let fake = FakeXgmac::new();
    // Start from the state the giant path leaves behind.
    fake.preload(MAC_RCR, 0);
    for a in after_giant {
        if let Access::Write(o, v) = a {
            fake.preload(o, v);
        }
    }
    config_jumbo_enable(&fake, 1500);
    let rcr = fake
        .log()
        .iter()
        .rev()
        .find_map(|a| match a {
            Access::Write(o, v) if *o == MAC_RCR => Some(*v),
            _ => None,
        })
        .unwrap();
    assert_eq!(get_bits(rcr, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1), 0);
    assert_eq!(get_bits(rcr, MAC_RCR_WD.0, MAC_RCR_WD.1), 0);
    assert_eq!(get_bits(rcr, MAC_RCR_JE.0, MAC_RCR_JE.1), 0);
    // `GPSL` is left loaded, which is harmless with `GPSLCE` off: it is
    // the comparison that is switched, not the number.
    assert_eq!(
        get_bits(rcr, MAC_RCR_GPSL.0, MAC_RCR_GPSL.1),
        GIANT_PACKET_MTU
    );
}

#[test]
fn the_giant_length_fits_its_field_with_room_to_spare() {
    assert_eq!(MAC_RCR_GPSL, (16, 14));
    assert!(GIANT_PACKET_MTU < (1 << MAC_RCR_GPSL.1));
    // 16368, not 16383 and not 16384: it is the 14-bit ceiling rounded
    // down to a multiple of 16.
    assert_eq!(GIANT_PACKET_MTU, 16368);
    assert_eq!((1u32 << MAC_RCR_GPSL.1) - 1, 16383);
    assert!(GIANT_PACKET_MTU.is_multiple_of(16));
    // And it is well above the jumbo frame it has to carry.
    assert_eq!(MAX_JUMBO_PACKET, 9018);
    assert_eq!(MAX_JUMBO_PACKET, JUMBO_PACKET_MTU + 18);
}

#[test]
fn the_length_bits_do_not_collide_with_the_enables_below_them() {
    // Everything this batch writes lives in two registers that the
    // bring-up sequence also writes, so none of it may overlap.
    for (a, b) in [
        (MAC_RCR_RE, MAC_RCR_GPSLCE),
        (MAC_RCR_ACS, MAC_RCR_WD),
        (MAC_RCR_CST, MAC_RCR_JE),
        (MAC_RCR_DCRCC, MAC_RCR_GPSL),
    ] {
        let mask_a = get_bits(u32::MAX, a.0, a.1) << a.0;
        let mask_b = get_bits(u32::MAX, b.0, b.1) << b.0;
        assert_eq!(mask_a & mask_b, 0, "{:?} vs {:?}", a, b);
    }
    assert_ne!(MAC_TCR_JD.0, MAC_TCR_TE.0);
}

// ——— Tanda 22: los descargos ———

#[test]
fn the_two_tso_switches_are_in_different_places() {
    // One per channel in the DMA, one per frame in the descriptor.
    assert_eq!(DMA_CH_TCR_TSE, (12, 1));
    assert_eq!(TX_DESC3_TSE, 1 << 18);
    // They are not the same bit, and turning the channel one on cannot
    // turn the descriptor one on by accident.
    assert_ne!(DMA_CH_TCR_TSE.0, 18);
}

#[test]
fn enabling_tso_on_the_channel_does_not_make_any_frame_a_tso_frame() {
    // This is why it is safe to set the channel bit with no TSO path:
    // every descriptor this driver builds leaves bit 18 clear.
    let fake = FakeXgmac::new();
    config_tso_mode(&fake, 2);
    let mut ring = [Desc::default(); 4];
    let frags = [fragment(0x1000, 100)];
    build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
    assert_eq!(ring[0].desc3 & TX_DESC3_TSE, 0);
    // And the frame length, which shares the word, cannot reach it.
    assert_eq!(tx_desc3_frame_len(u32::MAX) & TX_DESC3_TSE, 0);
}

#[test]
fn tso_goes_to_every_transmit_channel_and_nowhere_else() {
    let fake = FakeXgmac::new();
    config_tso_mode(&fake, 3);
    for channel in 0..3 {
        let off = dma_channel_reg(channel, DMA_CH_TCR);
        assert_eq!(fake.writes_to(off), 1);
    }
    assert_eq!(fake.writes_to(dma_channel_reg(3, DMA_CH_TCR)), 0);
    // And it is a field write, so the enable bit beside it survives.
    let fake = FakeXgmac::new();
    fake.preload(dma_channel_reg(0, DMA_CH_TCR), 1);
    config_tso_mode(&fake, 1);
    match fake.log()[1] {
        Access::Write(_, v) => {
            assert_eq!(v & 1, 1);
            assert_eq!(get_bits(v, DMA_CH_TCR_TSE.0, DMA_CH_TCR_TSE.1), 1);
        }
        ref other => panic!("{:?}", other),
    }
}

#[test]
fn the_header_split_bit_is_per_channel_and_its_size_is_not() {
    let fake = FakeXgmac::new();
    config_sph_mode(&fake, 4, SPH_HDSMS_SIZE).unwrap();
    for channel in 0..4 {
        assert_eq!(fake.writes_to(dma_channel_reg(channel, DMA_CH_CR)), 1);
    }
    // The size goes out once, in the MAC, for all four channels at once.
    assert_eq!(fake.writes_to(MAC_RCR), 1);
}

#[test]
fn the_header_size_is_an_index_and_not_a_byte_count() {
    // Three bits, so eight possible values. 2 is what Linux writes, and
    // its own comment ties that to SKB_ALLOC_SIZE rather than to the
    // chip: the number travels with the buffer, not with the register.
    assert_eq!(MAC_RCR_HDSMS, (12, 3));
    assert_eq!(SPH_HDSMS_SIZE, 2);
    assert!(SPH_HDSMS_SIZE < (1 << MAC_RCR_HDSMS.1));
    // A byte count would not fit at all: the field tops out at 7.
    assert_eq!((1u32 << MAC_RCR_HDSMS.1) - 1, 7);
    let fake = FakeXgmac::new();
    assert!(config_sph_mode(&fake, 1, 1 << MAC_RCR_HDSMS.1).is_err());
    assert!(fake.log().is_empty());
}

#[test]
fn the_header_size_does_not_disturb_the_length_limits_in_the_same_register() {
    // `MAC_RCR` already carries `IPC`, `JE`, `WD`, `GPSLCE` and `GPSL`.
    let fake = FakeXgmac::new();
    config_jumbo_enable(&fake, 9000);
    let after = fake
        .log()
        .iter()
        .rev()
        .find_map(|a| match a {
            Access::Write(o, v) if *o == MAC_RCR => Some(*v),
            _ => None,
        })
        .unwrap();
    let fake = FakeXgmac::new();
    fake.preload(MAC_RCR, after);
    config_sph_mode(&fake, 1, SPH_HDSMS_SIZE).unwrap();
    let rcr = fake
        .log()
        .iter()
        .rev()
        .find_map(|a| match a {
            Access::Write(o, v) if *o == MAC_RCR => Some(*v),
            _ => None,
        })
        .unwrap();
    assert_eq!(get_bits(rcr, MAC_RCR_JE.0, MAC_RCR_JE.1), 1);
    assert_eq!(
        get_bits(rcr, MAC_RCR_HDSMS.0, MAC_RCR_HDSMS.1),
        SPH_HDSMS_SIZE
    );
}

#[test]
fn the_checksum_bits_of_this_register_do_not_overlap_each_other() {
    for (a, b) in [
        (MAC_RCR_IPC, MAC_RCR_HDSMS),
        (MAC_RCR_IPC, MAC_RCR_JE),
        (MAC_RCR_HDSMS, MAC_RCR_GPSL),
        (MAC_RCR_IPC, MAC_RCR_WD),
    ] {
        let mask_a = get_bits(u32::MAX, a.0, a.1) << a.0;
        let mask_b = get_bits(u32::MAX, b.0, b.1) << b.0;
        assert_eq!(mask_a & mask_b, 0, "{:?} vs {:?}", a, b);
    }
    // `IPC` is below `LM` and above `JE`, which is the order that makes
    // the three of them one contiguous run.
    assert_eq!(MAC_RCR_IPC, (9, 1));
    assert_eq!(MAC_RCR_JE.0 + 1, MAC_RCR_IPC.0);
}

#[test]
fn without_the_checksum_bit_every_frame_comes_back_clean() {
    // The trap. `rx_frame_verdict` reads error codes the chip only ever
    // produces with `IPC` on, so with it off the verdict is not "not
    // checked", it is `Clean`.
    assert_eq!(rx_frame_verdict(0), RxVerdict::Clean);
    // Which is the same answer a genuinely good frame gives.
    let good = whole_frame(0);
    assert_eq!(rx_frame_verdict(good.desc3), RxVerdict::Clean);
    // And a bad checksum is only distinguishable because the chip set
    // both the error summary and the type.
    let bad = written_back((1 << RX_DESC3_ES.0) | (ETLT_PAYLOAD_CSUM << RX_DESC3_ETLT.0));
    assert_eq!(rx_frame_verdict(bad.desc3), RxVerdict::ChecksumBad);
}

#[test]
fn the_two_checksum_calls_differ_only_in_the_value() {
    let on = FakeXgmac::new();
    config_rx_csum(&on, true);
    let off = FakeXgmac::new();
    config_rx_csum(&off, false);
    assert_eq!(on.log().len(), 2);
    assert_eq!(on.log().len(), off.log().len());
    let value = |f: &FakeXgmac| match f.log()[1] {
        Access::Write(_, v) => v,
        ref other => panic!("{:?}", other),
    };
    assert_eq!(get_bits(value(&on), MAC_RCR_IPC.0, MAC_RCR_IPC.1), 1);
    assert_eq!(get_bits(value(&off), MAC_RCR_IPC.0, MAC_RCR_IPC.1), 0);
}

#[test]
fn turning_the_checksum_off_leaves_the_receiver_running() {
    // It is a field write, not a register write: a device already up
    // keeps `RE` and its length limits while the offload is toggled.
    let fake = FakeXgmac::new();
    fake.preload(MAC_RCR, (1 << MAC_RCR_RE.0) | (1 << MAC_RCR_JE.0));
    config_rx_csum(&fake, false);
    match fake.log()[1] {
        Access::Write(_, v) => {
            assert_eq!(get_bits(v, MAC_RCR_RE.0, MAC_RCR_RE.1), 1);
            assert_eq!(get_bits(v, MAC_RCR_JE.0, MAC_RCR_JE.1), 1);
            assert_eq!(get_bits(v, MAC_RCR_IPC.0, MAC_RCR_IPC.1), 0);
        }
        ref other => panic!("{:?}", other),
    }
}

// ——— Tanda 23: decirle al MAC a que velocidad va la linea ———

#[test]
fn the_speed_field_sits_at_the_very_top_of_the_transmit_control() {
    assert_eq!(MAC_TCR_SS, (29, 3));
    // Three bits ending at 31, so nothing can be above it.
    assert_eq!(MAC_TCR_SS.0 + MAC_TCR_SS.1, 32);
    // And it does not touch the transmitter enable at the bottom.
    assert_ne!(MAC_TCR_SS.0, MAC_TCR_TE.0);
    assert_ne!(MAC_TCR_SS.0, MAC_TCR_JD.0);
}

#[test]
fn the_encoding_is_in_no_order_at_all() {
    // Fastest is 0 and slowest is 7, with the rest shuffled in between.
    assert_eq!(MAC_SS_10G, 0x00);
    assert_eq!(MAC_SS_2_5G_GMII, 0x02);
    assert_eq!(MAC_SS_1G, 0x03);
    assert_eq!(MAC_SS_2_5G_XGMII, 0x06);
    assert_eq!(MAC_SS_10M, 0x07);
    // So comparing two encodings tells you nothing about which is faster.
    assert!(MAC_SS_10M > MAC_SS_10G);
    assert!(MAC_SS_1G > MAC_SS_2_5G_GMII);
}

#[test]
fn a_zeroed_register_already_claims_ten_gigabits() {
    // Which is why nothing in the bring-up has to write the speed to get
    // 10G, and why a missing speed call looks like it worked.
    assert_eq!(MAC_SS_10G, 0);
    let fake = FakeXgmac::new();
    assert!(!set_mac_speed(&fake, LinkSpeed::Speed10G, 0x30));
    // It read, and then wrote nothing.
    assert_eq!(fake.reads_of(MAC_TCR), 1);
    assert_eq!(fake.writes_to(MAC_TCR), 0);
}

#[test]
fn one_hundred_megabits_and_two_and_a_half_gigabits_are_the_same_value() {
    // The field picks an interface mode and a divider, not a speed, so
    // the collision is real and this register cannot tell them apart.
    assert_eq!(MAC_SS_100M, MAC_SS_2_5G_GMII);
    // Linux has no SPEED_100 case, so the value is never written; the
    // speeds this driver can ask for never produce it by that name.
    for ver in [0x20u8, 0x21, 0x30, MAC_VER_33] {
        for speed in [
            LinkSpeed::Speed10M,
            LinkSpeed::Speed1G,
            LinkSpeed::Speed2_5G,
            LinkSpeed::Speed10G,
        ] {
            let ss = speed_select(speed, ver);
            if ss == MAC_SS_100M {
                // The only way to land on it is 2.5G on an older core.
                assert_eq!(speed, LinkSpeed::Speed2_5G);
                assert_ne!(ver, MAC_VER_33);
            }
        }
    }
}

#[test]
fn only_two_and_a_half_gigabits_looks_at_the_core_revision() {
    for speed in [LinkSpeed::Speed10M, LinkSpeed::Speed1G, LinkSpeed::Speed10G] {
        assert_eq!(
            speed_select(speed, 0x30),
            speed_select(speed, MAC_VER_33),
            "{:?}",
            speed
        );
    }
    assert_eq!(
        speed_select(LinkSpeed::Speed2_5G, MAC_VER_33),
        MAC_SS_2_5G_XGMII
    );
    assert_eq!(speed_select(LinkSpeed::Speed2_5G, 0x30), MAC_SS_2_5G_GMII);
    // 0x33 exactly, not "0x33 or newer": Linux compares for equality.
    assert_eq!(speed_select(LinkSpeed::Speed2_5G, 0x34), MAC_SS_2_5G_GMII);
    assert_eq!(speed_select(LinkSpeed::Speed2_5G, 0x32), MAC_SS_2_5G_GMII);
    assert_eq!(MAC_VER_33, 0x33);
}

#[test]
fn every_encoding_fits_the_three_bit_field() {
    for ss in [
        MAC_SS_10G,
        MAC_SS_2_5G_GMII,
        MAC_SS_2_5G_XGMII,
        MAC_SS_1G,
        MAC_SS_100M,
        MAC_SS_10M,
    ] {
        assert!(ss < (1 << MAC_TCR_SS.1), "{:#x}", ss);
    }
}

#[test]
fn a_speed_that_has_not_changed_is_not_written() {
    // `MAC_TCR` carries `TE`, so a pointless write would pass through
    // the transmitter enable of a running device for nothing.
    let fake = FakeXgmac::new();
    fake.preload(MAC_TCR, (MAC_SS_1G << MAC_TCR_SS.0) | 1);
    assert!(!set_mac_speed(&fake, LinkSpeed::Speed1G, 0x30));
    assert_eq!(fake.writes_to(MAC_TCR), 0);
    // And a speed that has changed is.
    assert!(set_mac_speed(&fake, LinkSpeed::Speed10G, 0x30));
    assert_eq!(fake.writes_to(MAC_TCR), 1);
}

#[test]
fn changing_the_speed_leaves_the_transmitter_and_the_jabber_bit_alone() {
    let fake = FakeXgmac::new();
    // Bit 0 is left clear on purpose: the fake treats bit 0 as
    // self-clearing on read, which is what the reset sequences need, so
    // `TE` cannot be used as the witness here. `JD` does the same job.
    fake.preload(MAC_TCR, (MAC_SS_1G << MAC_TCR_SS.0) | (1 << MAC_TCR_JD.0));
    assert!(set_mac_speed(&fake, LinkSpeed::Speed10M, 0x30));
    let written = fake
        .log()
        .iter()
        .rev()
        .find_map(|a| match a {
            Access::Write(o, v) if *o == MAC_TCR => Some(*v),
            _ => None,
        })
        .unwrap();
    assert_eq!(get_bits(written, MAC_TCR_SS.0, MAC_TCR_SS.1), MAC_SS_10M);
    assert_eq!(get_bits(written, MAC_TCR_JD.0, MAC_TCR_JD.1), 1);
}

#[test]
fn the_same_core_revision_decides_this_and_the_queue_flush() {
    // `SNPSVER` is read once and branched on in several places; this is
    // the fourth. Worth pinning that they are different thresholds, so
    // nobody folds them into one "is it new enough" predicate.
    assert_eq!(FLUSH_MIN_SNPSVER, 0x21);
    assert_ne!(MAC_VER_33, FLUSH_MIN_SNPSVER);
    // A 0x33 core flushes its queues and drives 2.5G over XGMII; a 0x21
    // core flushes and drives 2.5G over GMII; a 0x20 core does neither.
    assert!(MAC_VER_33 >= FLUSH_MIN_SNPSVER);
    assert_eq!(speed_select(LinkSpeed::Speed2_5G, 0x21), MAC_SS_2_5G_GMII);
}
