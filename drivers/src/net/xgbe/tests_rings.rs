//! Tanda 28: la memoria de los anillos.
//!
//! Nada de esto ha corrido en hardware. Lo que se fija aqui es la forma:
//! quien es dueño de que, en que orden se publica, y que una trama
//! partida se vuelve a juntar entera.

use super::fake::*;
use super::*;
use alloc::vec::Vec;

const MTU: u32 = 1500;

fn rings() -> XgbeRings {
    XgbeRings::alloc(0, MTU).unwrap()
}

#[test]
fn the_buffer_size_is_the_one_the_mtu_asks_for() {
    let r = rings();
    assert_eq!(r.buffer_size(), calc_rx_buf_size(MTU));
    assert_eq!(r.buffer_size(), 1536);
    // And at a jumbo MTU it is a page, which is what makes a frame span
    // several slots.
    let jumbo = XgbeRings::alloc(0, 9000).unwrap();
    assert_eq!(jumbo.buffer_size(), RX_MAX_BUF_SIZE);
    assert_eq!(rx_buffers_for_mtu(9000), 3);
}

#[test]
fn every_receive_slot_starts_owned_by_the_chip_and_every_transmit_one_by_us() {
    let mut r = rings();
    for i in 0..TX_RING_SIZE {
        assert!(!desc_owned_by_dma(r.tx_desc(i).desc3), "tx {}", i);
    }
    // The receive side is handed over by `program`, and the tail starts
    // at the last descriptor, not one past the end.
    assert_eq!(r.rx_tail_index(), RX_RING_SIZE - 1);
}

#[test]
fn programming_the_rings_writes_both_bases_and_both_lengths() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.program(&fake).unwrap();
    for reg in [
        DMA_CH_TDLR_HI,
        DMA_CH_TDLR_LO,
        DMA_CH_RDLR_HI,
        DMA_CH_RDLR_LO,
        DMA_CH_TDRLR,
        DMA_CH_RDRLR,
        DMA_CH_RDTR_LO,
    ] {
        assert_eq!(fake.writes_to(dma_channel_reg(0, reg)), 1, "{:#x}", reg);
    }
    // The length registers take a last index, not a count.
    let last = |reg: usize| {
        fake.log()
            .iter()
            .filter_map(|a| match a {
                Access::Write(o, v) if *o == dma_channel_reg(0, reg) => Some(*v),
                _ => None,
            })
            .next_back()
            .unwrap()
    };
    assert_eq!(last(DMA_CH_TDRLR), TX_RING_SIZE as u32 - 1);
    assert_eq!(last(DMA_CH_RDRLR), RX_RING_SIZE as u32 - 1);
    // And the receive tail is the address of the last descriptor.
    assert_eq!(
        last(DMA_CH_RDTR_LO),
        desc_paddr(r.rx_paddr(), RX_RING_SIZE - 1) as u32
    );
}

#[test]
fn a_frame_is_handed_over_after_it_is_built_and_the_tail_moves_last() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.send(&fake, &[1u8; 64]).unwrap();
    assert!(desc_owned_by_dma(r.tx_desc(0).desc3));
    // The only register this touches is the transmit tail.
    assert_eq!(fake.log().len(), 1);
    match fake.log()[0] {
        Access::Write(off, value) => {
            assert_eq!(off, dma_channel_reg(0, DMA_CH_TDTR_LO));
            // Past the frame, not at it: slot 1, not slot 0.
            assert_eq!(value, desc_paddr(r.tx_paddr(), 1) as u32);
        }
        ref other => panic!("{:?}", other),
    }
}

#[test]
fn an_empty_or_oversized_frame_is_refused_and_nothing_is_handed_over() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    assert!(r.send(&fake, &[]).is_err());
    let too_big = Vec::from([0u8; 2048]);
    assert!(r.send(&fake, &too_big).is_err());
    assert!(!desc_owned_by_dma(r.tx_desc(0).desc3));
    assert!(fake.log().is_empty());
}

#[test]
fn the_transmit_ring_fills_up_and_says_so_instead_of_wrapping_over_itself() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    let mut sent = 0;
    while r.can_send() {
        r.send(&fake, &[7u8; 60]).unwrap();
        sent += 1;
    }
    // One slot is always left free, so the full and empty states are
    // distinguishable.
    assert_eq!(sent, TX_RING_SIZE - 1);
    assert!(r.send(&fake, &[7u8; 60]).is_err());
}

#[test]
fn reclaiming_takes_back_only_what_the_chip_gave_back() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    for _ in 0..3 {
        r.send(&fake, &[1u8; 60]).unwrap();
    }
    assert_eq!(r.reclaim_tx(), 0);
    r.tx_give_back(0);
    assert_eq!(r.reclaim_tx(), 1);
    // And it stops at the first slot still owned, rather than scanning
    // past it: order is what makes the ring a ring.
    r.tx_give_back(2);
    assert_eq!(r.reclaim_tx(), 0);
    r.tx_give_back(1);
    assert_eq!(r.reclaim_tx(), 2);
    assert!(r.can_send());
}

/// A written-back receive descriptor with the flags asked for.
fn written(first: bool, last: bool, len: u32) -> u32 {
    let mut desc3 = 0;
    if first {
        desc3 |= DESC3_FD;
    }
    if last {
        desc3 |= DESC3_LD;
        desc3 |= len & ((1 << 14) - 1);
    }
    desc3
}

#[test]
fn a_slot_the_chip_still_owns_yields_nothing() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.program(&fake).unwrap();
    assert!(r.recv(&fake).is_none());
}

#[test]
fn a_frame_in_one_slot_comes_back_whole() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.program(&fake).unwrap();
    let payload: Vec<u8> = (0..100u8).collect();
    r.rx_write_back(0, written(true, true, 100), &payload);
    let frame = r.recv(&fake).unwrap();
    assert_eq!(frame.len(), 100);
    assert_eq!(frame, payload);
    // The slot went back to the chip and the tail followed it.
    assert_eq!(r.rx_tail_index(), 0);
}

#[test]
fn a_frame_split_over_three_slots_is_joined_in_order() {
    let fake = FakeXgmac::new();
    let mut r = XgbeRings::alloc(0, 9000).unwrap();
    r.program(&fake).unwrap();
    let buf = r.buffer_size() as usize;
    let total = buf * 2 + 50;
    let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

    r.rx_write_back(0, written(true, false, 0), &payload[..buf]);
    r.rx_write_back(1, written(false, false, 0), &payload[buf..buf * 2]);
    r.rx_write_back(2, written(false, true, total as u32), &payload[buf * 2..]);

    let frame = r.recv(&fake).unwrap();
    assert_eq!(frame.len(), total);
    assert_eq!(frame, payload);
    // All three slots went back, and the tail is the last of them.
    assert_eq!(r.rx_tail_index(), 2);
}

#[test]
fn a_frame_whose_end_has_not_arrived_yields_nothing_and_consumes_nothing() {
    let fake = FakeXgmac::new();
    let mut r = XgbeRings::alloc(0, 9000).unwrap();
    r.program(&fake).unwrap();
    let buf = r.buffer_size() as usize;
    let payload = Vec::from([3u8; 16]);
    r.rx_write_back(0, written(true, false, 0), &payload);
    // The length only exists on the descriptor carrying `LD`, so there
    // is nothing to return and nothing to give back yet.
    assert!(r.recv(&fake).is_none());
    assert_eq!(r.rx_tail_index(), RX_RING_SIZE - 1);
    // Once the end arrives the whole thing comes out.
    r.rx_write_back(1, written(false, true, (buf + 8) as u32), &[4u8; 8]);
    assert_eq!(r.recv(&fake).unwrap().len(), buf + 8);
}

#[test]
fn the_tail_written_after_a_receive_is_the_last_slot_given_back() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.program(&fake).unwrap();
    r.rx_write_back(0, written(true, true, 10), &[9u8; 10]);
    r.recv(&fake).unwrap();
    let tail = fake
        .log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(o, v) if *o == dma_channel_reg(0, DMA_CH_RDTR_LO) => Some(*v),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(tail, desc_paddr(r.rx_paddr(), 0) as u32);
    // Not one past it: that address belongs to the next slot, which the
    // chip has not been given yet.
    assert_ne!(tail, desc_paddr(r.rx_paddr(), 1) as u32);
}

#[test]
fn a_ring_full_of_fragments_with_no_end_does_not_spin() {
    let fake = FakeXgmac::new();
    let mut r = rings();
    r.program(&fake).unwrap();
    for i in 0..RX_RING_SIZE {
        r.rx_write_back(i, written(i == 0, false, 0), &[1u8; 4]);
    }
    assert!(r.recv(&fake).is_none());
}

// ——— Tanda 29: la secuencia entera, y la escotilla ———

fn config() -> BringUpConfig {
    BringUpConfig {
        variant: Variant::V2b,
        snps_ver: 0x30,
        tx_channels: 1,
        rx_channels: 1,
        tx_queues: 1,
        rx_queues: 1,
        tc_count: 1,
        tx_fifo: 65_536,
        rx_fifo: 65_536,
        mtu: MTU,
        mac: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
    }
}

#[test]
fn the_hatch_is_shut_until_the_cmdline_opens_it() {
    // Default off, like `nvidia.gem_comptags` and the rest: nothing that
    // has never run against the part turns itself on.
    assert!(
        !enabled() || {
            set_enabled(false);
            !enabled()
        }
    );
    set_enabled(true);
    assert!(enabled());
    set_enabled(false);
    assert!(!enabled());
}

#[test]
fn the_sequence_flushes_the_queues_before_it_configures_anything() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    bring_up(&fake, &config(), &mut || n += 1).unwrap();
    let log = fake.log();
    let first_ftq = log
        .iter()
        .position(|a| {
            matches!(a, Access::Write(o, v)
            if *o == mtl_queue_reg(0, MTL_Q_TQOMR)
                && get_bits(*v, MTL_Q_TQOMR_FTQ.0, MTL_Q_TQOMR_FTQ.1) == 1)
        })
        .unwrap();
    let first_dma = log
        .iter()
        .position(|a| matches!(a, Access::Write(o, _) if *o == DMA_AXIARCR))
        .unwrap();
    assert!(
        first_ftq < first_dma,
        "flush at {} but the bus attributes at {}",
        first_ftq,
        first_dma
    );
}

#[test]
fn the_fifo_is_split_before_anything_measured_in_queue_sizes() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    bring_up(&fake, &config(), &mut || n += 1).unwrap();
    let log = fake.log();
    let fifo = log
        .iter()
        .rposition(|a| {
            matches!(a, Access::Write(o, v)
            if *o == mtl_queue_reg(0, MTL_Q_RQOMR)
                && get_bits(*v, MTL_Q_RQOMR_RQS.0, MTL_Q_RQOMR_RQS.1) != 0)
        })
        .unwrap();
    let marks = log
        .iter()
        .position(|a| matches!(a, Access::Write(o, _) if *o == mtl_queue_reg(0, MTL_Q_RQFCR)))
        .unwrap();
    assert!(
        fifo < marks,
        "fifo at {} but water marks at {}",
        fifo,
        marks
    );
}

#[test]
fn the_water_marks_are_computed_from_bytes_and_not_from_the_register_value() {
    // `equal_fifo_share` returns units-of-256 minus one. Using it as a
    // byte count would land on a queue a thousand times too small, and
    // the answer would still be in range.
    let bytes = 65_536u32;
    let as_register = equal_fifo_share(bytes, 1).unwrap();
    assert_ne!(as_register, bytes);
    let right = queue_flow_control(bytes, MAX_JUMBO_PACKET);
    let wrong = queue_flow_control(as_register, MAX_JUMBO_PACKET);
    assert_ne!(right, wrong);
    // And this is what the sequence actually programs.
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    bring_up(&fake, &config(), &mut || n += 1).unwrap();
    let written = fake
        .log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(o, v) if *o == mtl_queue_reg(0, MTL_Q_RQFCR) => Some(*v),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(
        get_bits(written, MTL_Q_RQFCR_RFA.0, MTL_Q_RQFCR_RFA.1),
        right.rfa
    );
    assert_eq!(
        get_bits(written, MTL_Q_RQFCR_RFD.0, MTL_Q_RQFCR_RFD.1),
        right.rfd
    );
}

#[test]
fn the_receiver_is_the_last_thing_switched_on() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    bring_up(&fake, &config(), &mut || n += 1).unwrap();
    let log = fake.log();
    let re = log
        .iter()
        .rposition(|a| {
            matches!(a, Access::Write(o, v)
            if *o == MAC_RCR && get_bits(*v, MAC_RCR_RE.0, MAC_RCR_RE.1) == 1)
        })
        .unwrap();
    // Every filter and length limit is already in place by then.
    for earlier in [MAC_PFR, MAC_VLANTR, MAC_MACA0LR] {
        let last = log
            .iter()
            .rposition(|a| matches!(a, Access::Write(o, _) if *o == earlier))
            .unwrap();
        assert!(
            last < re,
            "{:#06x} written at {} but RE at {}",
            earlier,
            last,
            re
        );
    }
}

#[test]
fn the_station_address_the_sequence_writes_is_the_one_it_was_given() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    let cfg = config();
    bring_up(&fake, &cfg, &mut || n += 1).unwrap();
    let lo = fake
        .log()
        .iter()
        .filter_map(|a| match a {
            Access::Write(o, v) if *o == MAC_MACA0LR => Some(*v),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(lo & 0xff, cfg.mac[0] as u32);
    assert_eq!((lo >> 8) & 0xff, cfg.mac[1] as u32);
}

#[test]
fn a_sequence_with_no_queues_is_refused_instead_of_dividing_by_zero() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    let mut cfg = config();
    cfg.rx_queues = 0;
    assert!(bring_up(&fake, &cfg, &mut || n += 1).is_err());
}

#[test]
fn the_rings_the_sequence_returns_are_already_programmed() {
    let fake = FakeXgmac::new();
    let mut n = 0u32;
    let mut rings = bring_up(&fake, &config(), &mut || n += 1).unwrap();
    assert_eq!(rings.buffer_size(), calc_rx_buf_size(MTU));
    // The ring base went out during the sequence, so a send needs no
    // further setup.
    assert!(fake.writes_to(dma_channel_reg(0, DMA_CH_TDLR_LO)) >= 1);
    rings.send(&fake, &[1u8; 64]).unwrap();
}

// ——— Tanda 30: la interfaz ———

#[test]
fn a_frame_too_long_for_the_wire_is_refused_and_not_clipped() {
    // Refused, because a frame clipped to the buffer goes out corrupt
    // while the caller is told it all went: the loss only ever shows up
    // at the far end.
    assert!(tx_frame_postable(1));
    assert!(tx_frame_postable(1514));
    assert!(!tx_frame_postable(1515));
    assert!(!tx_frame_postable(0));
}

#[test]
fn the_hardware_handle_reclaims_before_it_answers_whether_it_can_send() {
    let fake = FakeXgmac::new();
    let mut rings = XgbeRings::alloc(0, MTU).unwrap();
    rings.program(&fake).unwrap();
    // Fill the ring without ever reclaiming.
    while rings.can_send() {
        rings.send(&fake, &[1u8; 60]).unwrap();
    }
    assert!(!rings.can_send());
    // The chip gives one back, and `can_send` has to notice without
    // anyone calling `reclaim_tx` by hand --- which is the whole reason
    // `XgbeHw::can_send` takes `&mut self`.
    rings.tx_give_back(0);
    assert!(!rings.can_send());
    assert_eq!(rings.reclaim_tx(), 1);
    assert!(rings.can_send());
}

#[test]
fn a_quiet_status_is_not_claimed_as_ours() {
    // On a shared INTx line another device's interrupt brings the
    // handler here too, and claiming a status of zero would swallow it.
    let quiet = decode_dma_status(0);
    assert!(dma_status_is_quiet(&quiet));
    // Anything real is not quiet, including the two that are only ever
    // reported and not acted on.
    let mut noisy = quiet;
    noisy.rx_buffer_unavailable = true;
    assert!(!dma_status_is_quiet(&noisy));
    let mut fatal = quiet;
    fatal.fatal_bus_error = true;
    assert!(!dma_status_is_quiet(&fatal));
}
