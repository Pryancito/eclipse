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
