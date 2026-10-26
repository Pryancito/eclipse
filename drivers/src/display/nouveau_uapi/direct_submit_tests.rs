//! The direct-submit path: the GPFIFO ring accounting and the four method
//! encoders that put work on the channel.
//!
//! None of this runs in CI and none of it can: it only executes against a real
//! NVIDIA channel, and the emulated GPU in QEMU is virtio. On hardware the
//! encoders are cross-checked once at bring-up against the values the C side
//! computes with the SDK's own DRF macros (`check_encodings`), which is a good
//! check and a late one -- it needs the card. These pin the same values where
//! they can be read without one.

use super::*;

/// A ring of no entries is refused rather than divided by. `GPPut`,
/// `GPGet` and the ring size all come from outside this kernel, and the
/// wrap arithmetic used to take the size on faith: an `EXEC` on a channel
/// the RM prepared without a GPFIFO divided by zero, which is a kernel
/// panic reachable by any process holding the device open.
#[test]
fn a_ring_of_no_entries_is_refused_instead_of_divided_by() {
    assert!(ring_state(0, 0, 0, 1).is_none());
    assert!(ring_state(7, 3, 0, 0).is_none());
    // And every other size answers something.
    for entries in [1u32, 2, 8, 128, 4096, u32::MAX] {
        assert!(
            ring_state(0, 0, entries, 1).is_some(),
            "{} entries",
            entries
        );
    }
}

/// `next_slot` is the wrap the submit cursor takes after every GP entry,
/// written as a compare because the divisor is not a constant and NVK
/// batches hundreds of entries into one `EXEC`. It has to be the same
/// answer as the division it replaced for every slot the cursor can hold,
/// which is every slot below `entries` -- checked here exhaustively for
/// the ring this hardware gets and at the edges of the range.
#[test]
fn the_slot_cursor_wraps_exactly_as_the_division_did() {
    for entries in [1u32, 2, 3, 7, 8, 128, 4096] {
        for slot in 0..entries {
            assert_eq!(
                next_slot(slot, entries),
                (slot + 1) % entries,
                "slot {} of {} entries",
                slot,
                entries
            );
        }
    }
    // And a slot that is somehow already outside the ring lands inside it
    // rather than one past the end of the ring page.
    assert_eq!(next_slot(9999, 128), 0);
    assert_eq!(next_slot(u32::MAX - 1, 128), 0);
}

/// `ring_state`'s last wrap is a conditional subtract rather than a third
/// division, which is only sound because `put` and `get` are already
/// inside the ring when it runs. This walks every pointer pair of several
/// ring sizes against the arithmetic as it was written, including the
/// raw pointers past the end that the RM's own submissions produce.
#[test]
fn the_in_flight_count_is_what_the_division_answered() {
    fn used_by_division(put_raw: u32, get_raw: u32, entries: u32) -> u64 {
        let (put, get, e) = (
            (put_raw % entries) as u64,
            (get_raw % entries) as u64,
            entries as u64,
        );
        (put + e - get) % e
    }
    for entries in [1u32, 2, 3, 8, 128] {
        for put in 0..entries * 3 {
            for get in 0..entries * 3 {
                let used = used_by_division(put, get, entries);
                for needed in 0..entries + 2 {
                    let st = ring_state(put, get, entries, needed).expect("a real ring");
                    assert_eq!(st.put, put % entries);
                    assert_eq!(st.get, get % entries);
                    assert_eq!(
                        st.room,
                        (used + needed as u64) < (entries as u64),
                        "put {} get {} of {} entries, needed {}",
                        put,
                        get,
                        entries,
                        needed
                    );
                }
            }
        }
    }
    // The nonsense ring size the 64-bit arithmetic exists for: a ring of
    // `u32::MAX` slots with all but one of them in flight. The subtract
    // has to agree with the division here too, and the answer is that
    // four more entries do NOT fit -- which is the overflow the 64-bit
    // arithmetic exists to get right, since in 32 bits `used + needed`
    // wraps and reports a full ring as an empty one.
    let used = used_by_division(u32::MAX - 1, 0, u32::MAX);
    assert_eq!(used, (u32::MAX - 1) as u64);
    let st = ring_state(u32::MAX - 1, 0, u32::MAX, 4).expect("a huge ring");
    assert_eq!(st.put, u32::MAX - 1);
    assert_eq!(st.get, 0);
    assert_eq!(st.room, (used + 4) < u32::MAX as u64);
    assert!(!st.room, "all but one slot is in flight");
    // One entry still does not fit, and zero does: the boundary the
    // reserved slot puts there.
    assert!(
        !ring_state(u32::MAX - 1, 0, u32::MAX, 1)
            .expect("a huge ring")
            .room
    );
    assert!(
        ring_state(u32::MAX - 2, 0, u32::MAX, 0)
            .expect("a huge ring")
            .room
    );
}

/// The pointers are taken modulo the ring, because the RM's own self-test
/// submissions move `GPPut` on channel 0 behind this path's back and the
/// value read back can be past the end.
#[test]
fn the_pointers_are_wrapped_into_the_ring() {
    let st = ring_state(4096 + 5, 4096 * 3 + 9, 4096, 1).expect("a real ring");
    assert_eq!(st.put, 5);
    assert_eq!(st.get, 9);
}

/// Free space, counted the way the hardware does: everything between
/// `GPPut` and `GPGet` going forward is in flight, and the count wraps.
#[test]
fn the_ring_counts_the_entries_in_flight_across_the_wrap() {
    const N: u32 = 8;
    // Empty: put == get. Seven of the eight entries can be filled.
    assert!(ring_state(0, 0, N, 7).expect("empty").room);
    assert!(!ring_state(0, 0, N, 8).expect("empty").room);
    // Three in flight, so four more fit and five do not.
    assert!(ring_state(3, 0, N, 4).expect("three used").room);
    assert!(!ring_state(3, 0, N, 5).expect("three used").room);
    // The same three, with the pair straddling the end of the ring.
    assert!(ring_state(1, 6, N, 4).expect("wrapped").room);
    assert!(
        !ring_state(1, 6, N, 5).expect("wrapped").room,
        "the count did not wrap with the ring"
    );
}

/// One entry always stays unused. With every entry in play, a full ring
/// and an empty one both read as `GPPut == GPGet`, and the host would take
/// the full one for empty and let the CPU overwrite entries the GPU has
/// not fetched.
#[test]
fn the_last_entry_stays_unused_so_full_is_not_empty() {
    for entries in [2u32, 8, 128, 4096] {
        let all = ring_state(0, 0, entries, entries).expect("a real ring");
        assert!(!all.room, "a ring of {} took all of its entries", entries);
        let all_but_one = ring_state(0, 0, entries, entries - 1).expect("a real ring");
        assert!(
            all_but_one.room,
            "a ring of {} lost a usable entry",
            entries
        );
    }
    // A one-entry ring can therefore never hold a submission, which is the
    // honest answer and not a panic.
    assert!(!ring_state(0, 0, 1, 1).expect("a one-entry ring").room);
}

/// A nonsense ring size cannot make a full ring look empty. `put +
/// entries` overflows a `u32` well before the size stops being
/// representable, and the overflow would answer "room" for a ring that has
/// none.
#[test]
fn a_nonsense_ring_size_cannot_overflow_the_accounting() {
    let huge = ring_state(u32::MAX - 1, 0, u32::MAX, 4).expect("a huge ring");
    assert_eq!(huge.put, u32::MAX - 1);
    assert!(
        !huge.room,
        "an overflow reported room on a ring with one entry left"
    );
    // And a `needed` no submission could ever have is refused, not wrapped.
    assert!(!ring_state(0, 0, 4096, u32::MAX).expect("a real ring").room);
}

/// `NV906F_GP_ENTRY0` carries GET in bits 31:2 of the push VA's low half.
/// The low two bits are not address: a push buffer is dword-aligned and
/// they are the entry's own flags.
#[test]
fn the_first_gp_entry_word_is_the_low_half_of_the_address() {
    assert_eq!(gp_entry0(0x0000_0000_1234_5678), 0x1234_5678);
    assert_eq!(
        gp_entry0(0x0000_00ff_1234_5678),
        0x1234_5678,
        "the high byte leaked in"
    );
    assert_eq!(
        gp_entry0(0x1234_5679),
        0x1234_5678,
        "the low bits are not address"
    );
    assert_eq!(gp_entry0(0x1234_567f), 0x1234_567c);
}

/// `NV906F_GP_ENTRY1`: GET_HI in 7:0, LENGTH in dwords in 30:10.
#[test]
fn the_second_gp_entry_word_is_the_high_byte_and_the_length_in_dwords() {
    // Six dwords of semaphore stream at a 40-bit address.
    let w = gp_entry1(0x0000_00ab_1234_5678, 24);
    assert_eq!(w & 0xff, 0xab, "GET_HI is not the VA's byte above 32");
    assert_eq!((w >> 10) & 0x1f_ffff, 6, "LENGTH is in dwords, not bytes");
    assert_eq!(w & 0x300, 0, "bits 9:8 belong to LEVEL and must stay clear");
    // Only the low byte of the VA's high half is address: the whole word
    // is compared, because masking the answer the same way the encoder
    // does would hide a leak into LENGTH.
    assert_eq!(
        gp_entry1(0x0000_ff00_0000_0000, 4),
        1 << 10,
        "the bits above GET_HI leaked into the entry"
    );
    // A zero-length push encodes as zero length, not as a wrap.
    assert_eq!(gp_entry1(0, 0) >> 10, 0);
}

/// A user push's entry carries `SYNC_WAIT` (bit 31) exactly when the
/// push asks not to be prefetched, and the longest push EXEC accepts
/// still fits the 21-bit LENGTH without touching that bit.
#[test]
fn a_no_prefetch_push_sets_the_sync_bit_and_the_longest_push_fits_below_it() {
    let plain = gp_entry1_push(0x0000_00ab_1234_5678, 24, 0);
    assert_eq!(plain, gp_entry1(0x0000_00ab_1234_5678, 24));
    assert_eq!(plain >> 31, 0, "SYNC_PROCEED for an ordinary push");
    let held = gp_entry1_push(0x0000_00ab_1234_5678, 24, EXEC_PUSH_NO_PREFETCH);
    assert_eq!(held >> 31, 1, "SYNC_WAIT for a push the GPU still writes");
    assert_eq!(held & !(1 << 31), plain, "and nothing else changes");
    assert_eq!(
        gp_entry1_push(0, 24, 0x2) >> 31,
        0,
        "other flag bits are not the prefetch one"
    );
    let longest = gp_entry1_push(0, EXEC_PUSH_MAX_LENGTH & !3, 0);
    assert_eq!((longest >> 10) & 0x1f_ffff, 0x1f_ffff, "21 bits of dwords");
    assert_eq!(longest >> 31, 0);
    assert_eq!(
        (gp_entry1(0, EXEC_PUSH_MAX_LENGTH + 1) >> 10) & 0x1f_ffff,
        0,
        "one past the limit wraps to nothing: that is why EXEC refuses it"
    );
}

/// `NV906F_DMA` INC_METHOD header: SEC_OP in 31:29, COUNT in 28:16,
/// SUBCHANNEL in 15:13, and the method's BYTE address shifted down to the
/// dword address the host wants in 11:0.
#[test]
fn the_push_header_carries_the_method_the_count_and_the_subchannel() {
    let h = push_hdr(0, NVC46F_SEM_ADDR_LO, 5);
    assert_eq!(h >> 29, 1, "SEC_OP must be INC_METHOD");
    assert_eq!((h >> 16) & 0x1fff, 5, "the method count is wrong");
    assert_eq!((h >> 13) & 0x7, 0, "the subchannel is wrong");
    assert_eq!(
        h & 0xfff,
        NVC46F_SEM_ADDR_LO >> 2,
        "the method address is a dword index, not a byte offset"
    );
    // Each field stays inside its own bits.
    let full = push_hdr(0x7, 0x3ffc, 0x1fff);
    assert_eq!((full >> 13) & 0x7, 0x7);
    assert_eq!((full >> 16) & 0x1fff, 0x1fff);
    assert_eq!(full >> 29, 1, "a full count must not run into SEC_OP");
}

/// The host semaphore streams: five consecutive methods from `SEM_ADDR_LO`
/// -- address low, address high, payload low, payload high, execute.
#[test]
fn the_semaphore_streams_write_the_address_the_payload_and_the_operation() {
    const VA: u64 = 0x0000_007f_dead_b000;
    let rel = sem_release_stream(VA, 0x4142_4344);
    assert_eq!(rel[0], push_hdr(0, NVC46F_SEM_ADDR_LO, 5));
    assert_eq!(rel[1], 0xdead_b000, "SEM_ADDR_LO");
    assert_eq!(rel[2], 0x7f, "SEM_ADDR_HI is one byte of address");
    assert_eq!(rel[3], 0x4142_4344, "SEM_PAYLOAD_LO");
    assert_eq!(rel[4], 0, "SEM_PAYLOAD_HI: the payload is 32 bits");
    assert_eq!(rel[5], NVC46F_SEM_EXECUTE_RELEASE);

    // SEM_ADDR_HI is one byte wide. A GPU VA is 49 bits here, so the bits
    // above the address's byte 4 are not address and must not ride along.
    assert_eq!(
        sem_release_stream(0x0000_abcd_1234_5000, 1)[2],
        0xcd,
        "the bits above SEM_ADDR_HI leaked into the method"
    );

    // A release is a completion fence, not a "the host fetched it" mark:
    // without RELEASE_WFI_EN the payload lands while the engines are still
    // running, and every NVK syncobj resolves against it.
    assert_eq!(rel[5] & 0x7, 1, "the operation must be RELEASE");
    assert_ne!(
        rel[5] & (1 << 20),
        0,
        "RELEASE_WFI_EN is what makes it a fence"
    );

    let acq = sem_acquire_stream(VA, 7);
    // Same layout, so the only difference is the operation.
    assert_eq!(acq[..5], [rel[0], rel[1], rel[2], 7, 0]);
    assert_eq!(acq[5] & 0x7, 3, "ACQUIRE must be the CIRCULAR GEQ flavour");
    // Strict GEQ (2) hangs once a payload wraps past u32::MAX, which the
    // syncobj layer's own compare tolerates.
    assert_ne!(acq[5] & 0x7, 2);
    assert_eq!(acq[5] & (1 << 20), 0, "an acquire must not wait for idle");
}

/// Every ioctl this driver dispatches gets its own profile slot, and the
/// slot maps back to the name the report prints. The two sides are written
/// apart -- one hashes the NR, the other adds the slot index back to
/// `DRM_COMMAND_BASE` -- so a collision would silently add two ioctls'
/// times together under one name.
#[test]
fn every_dispatched_ioctl_has_a_profile_slot_of_its_own() {
    let nrs = [
        NR_GETPARAM,
        NR_CHANNEL_ALLOC,
        NR_CHANNEL_FREE,
        NR_NVIF,
        NR_VM_INIT,
        NR_VM_BIND,
        NR_EXEC,
        NR_GET_ZCULL_INFO,
        NR_GEM_NEW,
        NR_GEM_PUSHBUF,
        NR_GEM_CPU_PREP,
        NR_GEM_CPU_FINI,
        NR_GEM_INFO,
    ];
    let slot =
        |nr: u32| (nr as usize).wrapping_sub(DRM_COMMAND_BASE as usize) & (PROFILE_SLOTS - 1);
    for (i, &a) in nrs.iter().enumerate() {
        for &b in &nrs[i + 1..] {
            assert_ne!(
                slot(a),
                slot(b),
                "{} and {} share a profile slot",
                nouveau_ioctl_name(a),
                nouveau_ioctl_name(b)
            );
        }
        // And the report's own inverse lands back on the same command.
        let printed = DRM_COMMAND_BASE + slot(a) as u32;
        assert_eq!(
            nouveau_ioctl_name(printed),
            nouveau_ioctl_name(a),
            "slot {} prints as the wrong ioctl",
            slot(a)
        );
    }
}
