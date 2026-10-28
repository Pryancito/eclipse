//! The bring-up sequence and the ring encodings, against [`FakePcnet`].
//!
//! Everything here is a rule that, broken, leaves no error behind: a chip
//! configured in the wrong software style reads the descriptors at the wrong
//! offsets and simply never receives; an init block address written before
//! the style is parsed as a different structure; a length field without its
//! top four bits set transmits a frame of some other size. None of those log
//! anything.

use super::fake_ports::{FakePcnet, Wrote};
use super::*;
use crate::nvme::nvme_queue::test_clock;
use crate::utils::host_hooks::alone_with_the_allocator;
use alloc::boxed::Box;

// ——— The pure encodings ———

#[test]
fn a_descriptor_length_is_negated_with_its_top_four_bits_set() {
    // The chip reads bits 11..0 as the negated count and REQUIRES 15..12 to
    // be ones. A frame posted with them clear goes out at a length nobody
    // asked for.
    for len in [1usize, 60, 64, 1514, 1544] {
        let field = neg_len(len);
        assert_eq!(
            field & 0xf000,
            0xf000,
            "len {}: top nibble must be ones",
            len
        );
        assert_eq!(
            (field as i16).wrapping_neg() as usize & 0x0fff,
            len & 0x0fff,
            "len {}: the low twelve bits must negate back to the length",
            len
        );
    }
    // The two values Linux's own driver produces for the buffer size and a
    // minimum frame, byte for byte.
    assert_eq!(neg_len(1544), 0xf9f8);
    assert_eq!(neg_len(60), 0xffc4);
}

#[test]
fn the_ring_sizes_go_in_their_own_nibbles_of_tlen_rlen() {
    // Receive in bits 7..4, transmit in 15..12. Swapping them gives the chip
    // a receive ring of the transmit ring's size, which it then walks off the
    // end of.
    assert_eq!(tlen_rlen(5, 5), 0x5050);
    assert_eq!(tlen_rlen(0, 5), 0x5000, "an rlen of 0 is one descriptor");
    assert_eq!(tlen_rlen(5, 0), 0x0050);
    assert_eq!(tlen_rlen(4, 7), 0x7040);
}

#[test]
fn a_dma_address_above_four_gigabytes_is_refused() {
    // The chip is a 32-bit bus master. Programming the low half of such an
    // address does not fail: it points its DMA at an unrelated live page.
    assert!(fits_32bit(0));
    assert!(fits_32bit(0xffff_f000));
    assert!(fits_32bit(u32::MAX as usize));
    assert!(!fits_32bit(u32::MAX as usize + 1));
    assert!(!fits_32bit(0x1_0000_0000));
    assert!(!fits_32bit(usize::MAX));
}

#[test]
fn the_init_block_is_refused_rather_than_truncated() {
    let mac = [0x52, 0x54, 0x00, 1, 2, 3];
    assert!(build_init_block(&mac, 0x1_0000_0000, 0x2000, true).is_none());
    assert!(build_init_block(&mac, 0x1000, 0x1_0000_0000, true).is_none());
    let block = build_init_block(&mac, 0x1000, 0x2000, true).expect("both fit");
    assert_eq!(block.rx_ring, 0x1000);
    assert_eq!(block.tx_ring, 0x2000);
    assert_eq!(block.phys_addr, mac);
    assert_eq!(block.mode, MODE_PROM);
    assert_eq!(
        block.filter,
        [0xffff_ffff, 0xffff_ffff],
        "an all-zero filter drops IPv6 neighbour discovery the moment \
         promiscuous mode goes off, with nothing logged"
    );
    let plain = build_init_block(&mac, 0x1000, 0x2000, false).unwrap();
    assert_eq!(plain.mode, 0, "non-promiscuous must not set PROM");
}

#[test]
fn the_init_block_is_the_twenty_eight_bytes_the_chip_expects() {
    // The chip DMAs this structure in and reads it by offset. A padding byte
    // anywhere shifts the ring addresses and it starts somewhere else.
    assert_eq!(core::mem::size_of::<InitBlock>(), 28);
    let block = InitBlock {
        mode: 0x8000,
        tlen_rlen: 0x5050,
        phys_addr: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
        reserved: 0,
        filter: [0xaaaa_aaaa, 0xbbbb_bbbb],
        rx_ring: 0x1234_5678,
        tx_ring: 0x9abc_def0,
    };
    let bytes = unsafe { core::slice::from_raw_parts(&block as *const InitBlock as *const u8, 28) };
    assert_eq!(&bytes[0..2], &0x8000u16.to_le_bytes(), "mode at 0");
    assert_eq!(&bytes[2..4], &0x5050u16.to_le_bytes(), "tlen_rlen at 2");
    assert_eq!(&bytes[4..10], &block.phys_addr, "station address at 4");
    assert_eq!(
        &bytes[12..16],
        &0xaaaa_aaaau32.to_le_bytes(),
        "filter at 12"
    );
    assert_eq!(
        &bytes[20..24],
        &0x1234_5678u32.to_le_bytes(),
        "rx_ring at 20"
    );
    assert_eq!(
        &bytes[24..28],
        &0x9abc_def0u32.to_le_bytes(),
        "tx_ring at 24"
    );
}

#[test]
fn the_descriptors_are_sixteen_bytes_with_status_above_length() {
    assert_eq!(core::mem::size_of::<RxDesc>(), 16);
    assert_eq!(core::mem::size_of::<TxDesc>(), 16);
    let tx = TxDesc {
        base: 0x1111_2222,
        length: 0xf9f8,
        status: 0x8300,
        misc: 0,
        reserved: 0,
    };
    let bytes = unsafe { core::slice::from_raw_parts(&tx as *const TxDesc as *const u8, 16) };
    assert_eq!(&bytes[0..4], &0x1111_2222u32.to_le_bytes(), "base at 0");
    assert_eq!(&bytes[4..6], &0xf9f8u16.to_le_bytes(), "length at 4");
    assert_eq!(&bytes[6..8], &0x8300u16.to_le_bytes(), "status at 6");
}

#[test]
fn a_frame_length_is_the_message_count_without_its_crc() {
    // MCNT counts the 4-byte FCS. Passing it up unstripped puts four bytes of
    // checksum on the end of every packet the stack sees.
    assert_eq!(
        rx_frame_len(64),
        Some(60),
        "the minimum frame, FCS stripped"
    );
    assert_eq!(rx_frame_len(1518), Some(1514));
    // Only the low twelve bits are the count; the rest is other status.
    assert_eq!(rx_frame_len(0xffff_f000 | 64), Some(60));
}

#[test]
fn a_runt_or_an_impossible_length_is_dropped_rather_than_sliced() {
    // A length shorter than an FCS would underflow the subtraction; one
    // longer than the buffer builds a slice over memory the descriptor never
    // owned, and its contents go up the network stack.
    assert_eq!(rx_frame_len(0), None);
    assert_eq!(rx_frame_len(4), None);
    assert_eq!(rx_frame_len(63), None, "59 bytes is not an Ethernet frame");
    assert_eq!(rx_frame_len(64), Some(60), "and 60 is: the boundary");
    assert_eq!(
        rx_frame_len((PKT_BUF_SIZE + 4) as u32),
        Some(PKT_BUF_SIZE),
        "a full buffer is still a frame: that is the boundary"
    );
    assert_eq!(
        rx_frame_len((PKT_BUF_SIZE + 5) as u32),
        None,
        "one byte past the buffer must not survive"
    );
}

#[test]
fn only_a_whole_good_descriptor_the_chip_has_released_is_a_frame() {
    const OURS_WHOLE: u16 = RD_STP | RD_ENP;
    assert!(rx_descriptor_is_good(OURS_WHOLE));
    assert!(
        !rx_descriptor_is_good(RD_OWN | OURS_WHOLE),
        "OWN set means the chip is still writing into it"
    );
    assert!(
        !rx_descriptor_is_good(OURS_WHOLE | RD_ERR),
        "ERR is not a frame"
    );
    assert!(
        !rx_descriptor_is_good(RD_STP),
        "a frame that starts here and ends elsewhere is not one descriptor"
    );
    assert!(
        !rx_descriptor_is_good(RD_ENP),
        "nor one that only ends here"
    );

    // Ownership is a separate question: a bad descriptor is still ours, and
    // still has to be recycled or the ring runs dry.
    assert!(rx_descriptor_is_ours(RD_STP));
    assert!(rx_descriptor_is_ours(OURS_WHOLE | RD_ERR));
    assert!(!rx_descriptor_is_ours(RD_OWN));
}

#[test]
fn a_transmit_slot_is_free_only_with_own_clear() {
    assert!(tx_descriptor_is_free(0));
    assert!(tx_descriptor_is_free(TD_ERR | TD_STP | TD_ENP));
    assert!(!tx_descriptor_is_free(TD_OWN));
    assert!(!tx_descriptor_is_free(TD_OWN | TD_STP | TD_ENP));
}

#[test]
fn an_empty_or_oversized_frame_is_refused_rather_than_clipped() {
    assert!(!tx_frame_postable(0), "a zero-length frame is not a frame");
    assert!(tx_frame_postable(1));
    assert!(tx_frame_postable(60));
    assert!(tx_frame_postable(MAX_FRAME), "the advertised MTU must fit");
    assert!(
        !tx_frame_postable(MAX_FRAME + 1),
        "one byte past the MTU must be refused, not truncated: a clipped \
         frame goes out corrupt while the caller is told it all went"
    );
    assert!(!tx_frame_postable(65536));
    assert!(!tx_frame_postable(usize::MAX));
}

#[test]
fn the_ring_index_wraps_at_the_end_and_nowhere_else() {
    assert_eq!(next_desc(0), 1);
    assert_eq!(next_desc(RING_LEN - 2), RING_LEN - 1);
    assert_eq!(
        next_desc(RING_LEN - 1),
        0,
        "the last slot wraps to the first"
    );
}

#[test]
fn a_frame_longer_than_the_scratch_goes_to_the_heap_instead_of_panicking() {
    // `len` is whatever the IP layer computed, not a clamped MTU: one
    // `sendto` of a 1495-byte UDP payload is 1537 bytes on the wire, past the
    // scratch, and indexing a fixed array with it panics the kernel.
    let mut scratch = [0u8; TX_SCRATCH_LEN];
    let mut heap = Vec::new();
    for len in [0usize, 1, 60, TX_SCRATCH_LEN] {
        let buf = tx_stage(len, &mut scratch, &mut heap);
        assert_eq!(buf.len(), len);
        assert!(heap.is_empty(), "len {} should not have hit the heap", len);
    }
    let buf = tx_stage(TX_SCRATCH_LEN + 1, &mut scratch, &mut heap);
    assert_eq!(buf.len(), TX_SCRATCH_LEN + 1);
    let buf = tx_stage(65535, &mut scratch, &mut heap);
    assert_eq!(buf.len(), 65535, "a 64 KiB datagram must not panic either");
}

// ——— The register layout ———

#[test]
fn the_port_offsets_move_with_the_width() {
    // These four offsets are the whole difference between the two modes.
    // Getting one wrong writes a register value into the reset port.
    assert_eq!(Width::Wio.rdp(), 0x10);
    assert_eq!(Width::Wio.rap(), 0x12);
    assert_eq!(Width::Wio.reset(), 0x14);
    assert_eq!(Width::Wio.bdp(), 0x16);
    assert_eq!(Width::Dwio.rdp(), 0x10);
    assert_eq!(Width::Dwio.rap(), 0x14);
    assert_eq!(Width::Dwio.reset(), 0x18);
    assert_eq!(Width::Dwio.bdp(), 0x1c);
    assert_ne!(
        Width::Wio.reset(),
        Width::Dwio.reset(),
        "the WIO reset port is the DWIO address port: driving the wrong \
         width turns every register write into a chip reset"
    );
}

#[test]
fn a_register_round_trips_in_both_widths() {
    for width in [Width::Wio, Width::Dwio] {
        let fake = FakePcnet::new(width);
        write_csr(&fake, width, CSR4_FEATURES, 0x0915);
        assert_eq!(read_csr(&fake, width, CSR4_FEATURES), 0x0915, "{:?}", width);
        write_bcr(&fake, width, BCR20_SWSTYLE, 2);
        assert_eq!(read_bcr(&fake, width, BCR20_SWSTYLE), 2, "{:?}", width);
    }
}

#[test]
fn the_width_probe_finds_each_chip_and_nothing_else() {
    assert_eq!(detect_width(&FakePcnet::new(Width::Wio)), Some(Width::Wio));
    assert_eq!(
        detect_width(&FakePcnet::new(Width::Dwio)),
        Some(Width::Dwio)
    );
}

#[test]
fn the_width_probe_tries_sixteen_bits_first() {
    // Linux's comment, and the reason for it: some older PCnet parts answer
    // the 32-bit probe by accident and then do not work in that mode. A probe
    // that asked 32 bits first would claim them in the mode they fail in.
    let fake = FakePcnet::new(Width::Wio);
    assert_eq!(detect_width(&fake), Some(Width::Wio));
    let log = fake.log();
    let first_32 = log.iter().position(|a| {
        matches!(
            a,
            super::fake_ports::Access::In32(_) | super::fake_ports::Access::Out32(..)
        )
    });
    assert!(
        first_32.is_none(),
        "a chip that answers in 16 bits must be claimed without a single \
         32-bit access: {:?}",
        log
    );
}

#[test]
fn nothing_at_the_io_space_is_not_mistaken_for_a_chip() {
    // An I/O window with no PCnet behind it reads back ones, and `detect`
    // must say so rather than bringing up a driver for whatever is there.
    struct OpenBus;
    impl PortAccess for OpenBus {
        fn in16(&self, _off: u16) -> u16 {
            0xffff
        }
        fn out16(&self, _off: u16, _value: u16) {}
        fn in32(&self, _off: u16) -> u32 {
            0xffff_ffff
        }
        fn out32(&self, _off: u16, _value: u32) {}
    }
    assert_eq!(detect_width(&OpenBus), None);

    struct Floating;
    impl PortAccess for Floating {
        fn in16(&self, _off: u16) -> u16 {
            0
        }
        fn out16(&self, _off: u16, _value: u16) {}
        fn in32(&self, _off: u16) -> u32 {
            0
        }
        fn out32(&self, _off: u16, _value: u32) {}
    }
    assert_eq!(
        detect_width(&Floating),
        None,
        "CSR0 reading 0 instead of STOP is not a reset PCnet"
    );
}

// ——— The PCI match ———

#[test]
fn the_driver_claims_amds_pcnet_parts_and_no_others() {
    assert!(pcnet32_matched(0x1022, 0x2000), "PCnet-PCI II / FAST III");
    assert!(pcnet32_matched(0x1022, 0x2001), "PCnet-Home");
    // The GPU, the IOMMU and the host bridges share AMD's vendor id.
    assert!(!pcnet32_matched(0x1022, 0x1458), "that is xgbe, not pcnet");
    assert!(!pcnet32_matched(0x1022, 0x2003));
    assert!(!pcnet32_matched(0x1022, 0x1022));
    // And another vendor's 0x2000 is somebody else's device entirely.
    assert!(!pcnet32_matched(0x8086, 0x2000));
    assert!(!pcnet32_matched(0x10ec, 0x2000));
    assert!(!pcnet32_matched(0x10de, 0x2000));
}

// ——— The bring-up ———

/// Bring a chip up against the fake, with the clock moving so any wait inside
/// ends.
fn bring_up(fake: FakePcnet) -> (DeviceResult<Pcnet32>, ()) {
    test_clock::set_auto_advance(1000);
    let out = Pcnet32::new(Box::new(fake), [0x02, 0, 0, 0, 0, 1]);
    test_clock::set_auto_advance(0);
    (out, ())
}

#[test]
fn bring_up_sets_the_software_style_before_the_init_block_address() {
    // This is the ordering bug with no symptom: the chip reads CSR1/CSR2 when
    // INIT is set, but it parses the block in whatever software style is in
    // force at that moment. Style 0 reads a 16-bit LANCE block out of the
    // 32-bit one built here, so the ring addresses come out as nonsense and
    // the NIC simply never receives.
    alone_with_the_allocator(|| {
        // The fake has to be inspected after the driver has taken ownership of
        // it, so bring-up runs against a clone of the register file: the
        // driver gets the box, the test keeps the decoded log by asking the
        // same fake through a second handle.
        let fake = Arc::new(FakePcnet::new(Width::Dwio));
        struct Shared(Arc<FakePcnet>);
        impl PortAccess for Shared {
            fn in16(&self, off: u16) -> u16 {
                self.0.in16(off)
            }
            fn out16(&self, off: u16, v: u16) {
                self.0.out16(off, v)
            }
            fn in32(&self, off: u16) -> u32 {
                self.0.in32(off)
            }
            fn out32(&self, off: u16, v: u32) {
                self.0.out32(off, v)
            }
        }
        test_clock::set_auto_advance(1000);
        let dev = Pcnet32::new(Box::new(Shared(fake.clone())), [0x02, 0, 0, 0, 0, 1])
            .expect("the fake answers the probe");
        test_clock::set_auto_advance(0);

        let style = fake
            .position_where(|w| matches!(w, Wrote::Bcr(20, 2)))
            .expect("BCR20 must be set to software style 2");
        let iadr = fake
            .position_where(|w| matches!(w, Wrote::Csr(1, _)))
            .expect("CSR1 must carry the init block address");
        assert!(
            style < iadr,
            "the software style ({}) must be set before the init block \
             address ({})",
            style,
            iadr
        );

        // And the chip is left running with interrupts enabled.
        assert_eq!(fake.csr(0) & CSR0_STOP, 0, "STRT must have cleared STOP");
        assert_ne!(fake.csr(0) & CSR0_IENA, 0, "interrupts must be enabled");
        assert_eq!(fake.csr(4), CSR4_VALUE, "CSR4 carries the feature bits");
        assert_eq!(fake.bcr(20), SWSTYLE_PCNET32);
        assert_ne!(fake.bcr(2) & 0x0002, 0, "ASEL: the chip picks its media");
        assert_eq!(
            dev.mac(),
            EthernetAddress::from_bytes(&[0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]),
            "the station address comes from the address PROM"
        );
    });
}

#[test]
fn bring_up_works_in_sixteen_bit_mode_too() {
    alone_with_the_allocator(|| {
        let (dev, ()) = bring_up(FakePcnet::new(Width::Wio));
        let dev = dev.expect("a WIO chip must come up");
        assert_eq!(
            dev.mac(),
            EthernetAddress::from_bytes(&[0x52, 0x54, 0x00, 0xab, 0xcd, 0xef])
        );
    });
}

#[test]
fn a_chip_that_never_acknowledges_the_init_block_is_not_started() {
    // Starting it anyway runs it off whatever its rings pointed at before,
    // which on a warm reboot is the previous kernel's memory.
    alone_with_the_allocator(|| {
        let (dev, ()) = bring_up(FakePcnet::without_idon(Width::Dwio));
        assert!(
            matches!(dev, Err(DeviceError::IoError)),
            "no IDON must fail bring-up, not start the chip blind"
        );
    });
}

#[test]
fn a_station_address_prom_with_nothing_in_it_falls_back() {
    alone_with_the_allocator(|| {
        for junk in [[0u8; 6], [0xffu8; 6]] {
            let fake = FakePcnet::new(Width::Dwio);
            fake.set_aprom(junk);
            let (dev, ()) = bring_up(fake);
            let dev = dev.expect("bring-up does not depend on the PROM");
            assert_eq!(
                dev.mac(),
                EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 1]),
                "an empty PROM must give the fallback address, not {:?}",
                junk
            );
        }
    });
}

#[test]
fn a_multicast_bit_in_the_station_address_is_not_a_station_address() {
    // A PROM read that came back shifted or half-written can set the group
    // bit. Using such an address as the station address means every switch on
    // the segment floods its traffic and nothing ever answers this NIC.
    alone_with_the_allocator(|| {
        let fake = FakePcnet::new(Width::Dwio);
        fake.set_aprom([0x53, 0x54, 0x00, 1, 2, 3]);
        let (dev, ()) = bring_up(fake);
        assert_eq!(
            dev.expect("still comes up").mac(),
            EthernetAddress::from_bytes(&[0x02, 0, 0, 0, 0, 1]),
            "the fallback must be used instead"
        );
    });
}

#[test]
fn an_acknowledged_interrupt_does_not_restart_the_chip() {
    // CSR0's low bits are control, not status. Acknowledging by writing CSR0
    // back whole sets STOP and INIT on the way past, which stops the NIC in
    // the middle of its own interrupt handler.
    alone_with_the_allocator(|| {
        let fake = Arc::new(FakePcnet::new(Width::Dwio));
        struct Shared(Arc<FakePcnet>);
        impl PortAccess for Shared {
            fn in16(&self, off: u16) -> u16 {
                self.0.in16(off)
            }
            fn out16(&self, off: u16, v: u16) {
                self.0.out16(off, v)
            }
            fn in32(&self, off: u16) -> u32 {
                self.0.in32(off)
            }
            fn out32(&self, off: u16, v: u32) {
                self.0.out32(off, v)
            }
        }
        test_clock::set_auto_advance(1000);
        let dev = Pcnet32::new(Box::new(Shared(fake.clone())), [0x02, 0, 0, 0, 0, 1]).unwrap();
        test_clock::set_auto_advance(0);

        // No interrupt pending: `ack_irq` must say so and touch nothing.
        assert_eq!(dev.ack_irq(), None, "INTR clear is not our interrupt");

        // Now raise one the way the chip does. A driver write cannot set
        // these bits -- they are write-1-to-clear -- so the fake raises them
        // from its own side.
        fake.raise_csr0(CSR0_INTR | CSR0_RINT);
        let seen = dev.ack_irq().expect("INTR set is ours");
        assert_ne!(seen & CSR0_RINT, 0, "the cause must be reported up");
        assert_eq!(fake.csr(0) & CSR0_RINT, 0, "and cleared at the chip");
        assert_eq!(
            fake.csr(0) & CSR0_STOP,
            0,
            "acknowledging must not STOP the chip"
        );
    });
}

#[test]
fn masking_and_unmasking_go_through_csr3() {
    // There is no IMC/IMS pair on this part: the mask bits live in CSR3, and
    // masking by clearing IENA in CSR0 instead would also clear the status
    // bits that write-back carries.
    alone_with_the_allocator(|| {
        let fake = Arc::new(FakePcnet::new(Width::Dwio));
        struct Shared(Arc<FakePcnet>);
        impl PortAccess for Shared {
            fn in16(&self, off: u16) -> u16 {
                self.0.in16(off)
            }
            fn out16(&self, off: u16, v: u16) {
                self.0.out16(off, v)
            }
            fn in32(&self, off: u16) -> u32 {
                self.0.in32(off)
            }
            fn out32(&self, off: u16, v: u32) {
                self.0.out32(off, v)
            }
        }
        test_clock::set_auto_advance(1000);
        let dev = Pcnet32::new(Box::new(Shared(fake.clone())), [0x02, 0, 0, 0, 0, 1]).unwrap();
        test_clock::set_auto_advance(0);

        dev.mask_irqs();
        assert_eq!(
            fake.csr(3) & CSR3_MASK_ALL,
            CSR3_MASK_ALL,
            "every source must be masked"
        );
        dev.unmask_irqs();
        assert_eq!(fake.csr(3) & CSR3_MASK_ALL, 0, "and unmasked again");
        assert_ne!(fake.csr(0) & CSR0_IENA, 0, "with delivery re-enabled");
    });
}
