//! El camino de datos del XGMAC: anillos, descriptores, interrupciones,
//! la MTL, los filtros y los descargos.
//!
//! Vive aparte de `xgbe.rs` porque el trinquete de tamano de `xtask`
//! limita cada `.rs` a 4000 lineas. El reparto es por tema y no mueve
//! ni una linea de codigo: todo lo de aqui se reexporta desde el modulo
//! padre, asi que los nombres no cambian para nadie.

use super::*;

// ——— Handing the chip its rings ———
//
// The last configuration step before anything moves: telling each channel
// where its descriptor ring is, how long it is, and --- for receive only ---
// how far into it the driver has filled.

/// `DMA_CH_RCR_RBSZ`, index **1**, width 14: the receive buffer size.
///
/// Index 1, not 0, so the value sits shifted by one bit in the register. The
/// bottom bit belongs to `SR` (start receive), which is why a buffer size
/// written at index 0 would start the engine as a side effect of being an odd
/// number of bytes.
pub const DMA_CH_RCR_RBSZ: (u32, u32) = (1, 14);

/// Program a transmit ring into its channel.
///
/// `ring_paddr` is the physical address of descriptor 0, `count` the number
/// of descriptors.
///
/// Two things to get right, and neither reports anything when wrong:
///
/// * The length register takes **`count - 1`**: it is a last index, not a
///   count. Writing the count gives the engine one descriptor more than
///   exists, which it will happily fetch from whatever follows the ring.
/// * The address halves go in **high first** (see [`DMA_CH_TDLR`]).
///
/// There is deliberately **no tail pointer** here. A transmit ring starts
/// empty: the driver has nothing for the engine to fetch, and the tail is
/// written later, when a frame is queued. Writing one now would point the
/// engine at descriptors the driver has not filled in.
pub fn program_tx_ring(
    acc: &dyn MmioAccess,
    channel: u32,
    ring_paddr: u64,
    count: u32,
) -> DeviceResult<()> {
    if count == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let (hi, lo) = split_ring_addr(ring_paddr);

    acc.write32(dma_channel_reg(channel, DMA_CH_TDRLR), count - 1);
    acc.write32(dma_channel_reg(channel, DMA_CH_TDLR_HI), hi);
    acc.write32(dma_channel_reg(channel, DMA_CH_TDLR_LO), lo);
    Ok(())
}

/// Program a receive ring into its channel, and set the buffer size.
///
/// Same two rules as [`program_tx_ring`], plus the one that makes receive
/// different: it **does** get a tail pointer, and the tail points at the
/// address of the **last descriptor**, not one past the end.
///
/// That is the opposite of the usual convention and the reason it is spelled
/// out here. The tail is "the last descriptor the driver owns"; a receive
/// ring starts with every descriptor handed to the engine, so the tail is the
/// last one. One past the end would be `ring_paddr + count * 16`, an address
/// outside the ring, and the engine would fetch a descriptor from whatever
/// the allocator put there next.
pub fn program_rx_ring(
    acc: &dyn MmioAccess,
    channel: u32,
    ring_paddr: u64,
    count: u32,
    buffer_size: u32,
) -> DeviceResult<()> {
    if count == 0 || buffer_size == 0 || buffer_size >= (1 << DMA_CH_RCR_RBSZ.1) {
        return Err(DeviceError::InvalidParam);
    }
    let (hi, lo) = split_ring_addr(ring_paddr);

    write_field(
        acc,
        dma_channel_reg(channel, DMA_CH_RCR),
        DMA_CH_RCR_RBSZ.0,
        DMA_CH_RCR_RBSZ.1,
        buffer_size,
    );

    acc.write32(dma_channel_reg(channel, DMA_CH_RDRLR), count - 1);
    acc.write32(dma_channel_reg(channel, DMA_CH_RDLR_HI), hi);
    acc.write32(dma_channel_reg(channel, DMA_CH_RDLR_LO), lo);

    let last = ring_paddr + ((count - 1) as u64) * core::mem::size_of::<Desc>() as u64;
    acc.write32(dma_channel_reg(channel, DMA_CH_RDTR_LO), last as u32);
    Ok(())
}

/// Whether descriptor `index` should ask for an interrupt, given the receive
/// coalescing settings. `xgbe_rx_desc_reset`.
///
/// With no coalescing at all, every descriptor interrupts. With a frame count
/// set, every `rx_frames`-th one does --- counted on `index + 1`, so the
/// *last* descriptor of each group carries the flag and not the first.
#[inline]
pub const fn rx_desc_wants_interrupt(index: u32, rx_usecs: u32, rx_frames: u32) -> bool {
    if rx_usecs == 0 && rx_frames == 0 {
        return true;
    }
    rx_frames != 0 && (index + 1).is_multiple_of(rx_frames)
}

/// Build a receive descriptor, ready except for its `OWN` bit.
///
/// A receive descriptor carries **two** buffers: the header buffer in
/// `desc0`/`desc1` and the payload buffer in `desc2`/`desc3`, each as a full
/// 64-bit address in low-then-high order. Which means all four words are
/// address --- and `desc3`, the high half of the payload address, is **also**
/// where `OWN` and `INTE` live.
///
/// So the control bits are OR'd into an address word. It works because a
/// physical address has nothing in its top bits, but it has two consequences
/// worth stating: writing `desc3` as "just the address" clears `OWN`, and
/// writing it as "just the flags" destroys the payload address.
///
/// The `OWN` bit is left off here on purpose. Linux sets it in a separate
/// write with a barrier either side, because the receive engine may already
/// be running over this ring: everything else must be visible before the
/// descriptor is handed over, and the handover must be visible before
/// anything else happens. Returning the descriptor without `OWN` makes that
/// ordering the caller's explicit step instead of a detail buried in here.
#[inline]
pub fn rx_desc_init(hdr_dma: u64, buf_dma: u64, wants_interrupt: bool) -> Desc {
    let (hdr_hi, hdr_lo) = split_ring_addr(hdr_dma);
    let (buf_hi, buf_lo) = split_ring_addr(buf_dma);
    let mut desc3 = buf_hi;
    if wants_interrupt {
        desc3 |= DESC3_CTXT; // RX_NORMAL_DESC3_INTE, the same bit 30.
    }
    Desc {
        desc0: hdr_lo,
        desc1: hdr_hi,
        desc2: buf_lo,
        desc3,
    }
}

// ——— Putting a frame into the transmit ring ———

/// `TX_NORMAL_DESC2_HL_B1L`, index 0, width 14: the length of *this*
/// fragment. See [`TX_DESC3_FL`] for the other length in the same
/// descriptor.
pub const TX_DESC2_HL_B1L: (u32, u32) = (0, 14);
/// `TX_NORMAL_DESC2_VTIR`, index 14, width 2: what to do with a VLAN tag.
pub const TX_DESC2_VTIR: (u32, u32) = (14, 2);
/// `TX_NORMAL_DESC2_VLAN_INSERT`, the `VTIR` value that inserts the tag the
/// context descriptor carried.
pub const TX_DESC2_VLAN_INSERT: u32 = 0x2;
/// `TX_NORMAL_DESC2_TTSE`, index 30, width 1: take a transmit timestamp.
pub const TX_DESC2_TTSE: (u32, u32) = (30, 1);
/// `TX_NORMAL_DESC2_IC`, index 31, width 1: raise an interrupt once this
/// descriptor completes.
///
/// Bit 31 of `desc2`, which is not bit 31 of `desc3`: that one is `OWN`. The
/// two words put entirely unrelated meanings in the same position, so a field
/// written into the wrong word still lands on a real bit and the chip obeys
/// it.
pub const TX_DESC2_IC: (u32, u32) = (31, 1);

/// `TX_NORMAL_DESC3_FL`, index 0, width 15: the length of the WHOLE frame,
/// written on the first descriptor only. Already masked by
/// [`tx_desc3_frame_len`].
pub const TX_DESC3_FL: (u32, u32) = (0, 15);
/// `TX_NORMAL_DESC3_CIC`, index 16, width 2: checksum insertion control.
pub const TX_DESC3_CIC: (u32, u32) = (16, 2);
/// `TX_NORMAL_DESC3_CIC` fully on: header and payload checksums both.
pub const TX_DESC3_CIC_FULL: u32 = 0x3;
/// `TX_NORMAL_DESC3_CPC`, index 26, width 2: CRC and pad control. Zero means
/// insert both, which is what an ordinary frame wants, so this driver never
/// writes anything else --- but the field is named here because a zero is
/// only obviously right once you know the field exists.
pub const TX_DESC3_CPC: (u32, u32) = (26, 2);

/// One fragment of an outgoing frame: a bus address and how many bytes live
/// at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxFragment {
    /// The address the engine will read from.
    pub dma: u64,
    /// How many bytes to read. Must fit [`TX_DESC2_HL_B1L`] and not be zero.
    pub len: u32,
}

/// A frame about to be handed to the transmit engine.
#[derive(Debug, Clone, Copy)]
pub struct TxFrame<'a> {
    /// The fragments, in wire order. The first becomes the first descriptor.
    pub fragments: &'a [TxFragment],
    /// The total length of the frame, for [`TX_DESC3_FL`]. Kept separate from
    /// the sum of the fragment lengths because the engine reads it from one
    /// place and the fragments from another, and
    /// [`build_tx_frame`] checks that the two agree rather than deriving one
    /// from the other.
    pub total_len: u32,
    /// Ask the chip to compute the checksums.
    pub checksum_offload: bool,
    /// Raise an interrupt when the frame has gone out.
    pub wants_interrupt: bool,
}

/// Write a frame's descriptors into `ring`, starting at `start` and wrapping,
/// and return the index of the next free descriptor. `xgbe_dev_xmit` for an
/// ordinary frame: no segmentation offload, no VLAN insertion, no timestamp,
/// so no context descriptor and no second pass over `desc3`'s bottom bits.
///
/// The first descriptor comes back WITHOUT its `OWN` bit, on purpose. Handing
/// it over is [`hand_tx_frame_to_dma`]'s job, after the caller's barrier: an
/// engine that is already running would otherwise be free to start on a frame
/// whose later fragments have not been written yet. Every other descriptor is
/// handed over as it is built, because none of them means anything until the
/// first one says so.
pub fn build_tx_frame(ring: &mut [Desc], start: usize, frame: &TxFrame) -> DeviceResult<usize> {
    let count = frame.fragments.len();
    if count == 0 || count > ring.len() || start >= ring.len() {
        return Err(DeviceError::InvalidParam);
    }
    if frame.total_len == 0 || frame.total_len >= (1 << TX_DESC3_FL.1) {
        return Err(DeviceError::InvalidParam);
    }
    let mut summed: u32 = 0;
    for fragment in frame.fragments {
        if fragment.len == 0 || fragment.len >= (1 << TX_DESC2_HL_B1L.1) {
            return Err(DeviceError::InvalidParam);
        }
        summed = summed
            .checked_add(fragment.len)
            .ok_or(DeviceError::InvalidParam)?;
    }
    if summed != frame.total_len {
        return Err(DeviceError::InvalidParam);
    }

    let len = ring.len();
    let csum = if frame.checksum_offload {
        set_bits(0, TX_DESC3_CIC.0, TX_DESC3_CIC.1, TX_DESC3_CIC_FULL)
    } else {
        0
    };

    for (i, fragment) in frame.fragments.iter().enumerate() {
        let (hi, lo) = split_ring_addr(fragment.dma);
        let mut desc2 = set_bits(0, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1, fragment.len);
        let mut desc3 = csum;
        if i == 0 {
            // FL and FD go on the first descriptor only: FL is the whole
            // frame, while desc2's length is just this fragment.
            desc3 |= DESC3_FD | tx_desc3_frame_len(frame.total_len);
        } else {
            // ... and OWN goes on every descriptor BUT the first.
            desc3 |= DESC3_OWN;
        }
        if i == count - 1 {
            desc3 |= DESC3_LD;
            if frame.wants_interrupt {
                // IC rides the LAST descriptor, and it lives in desc2.
                desc2 = set_bits(desc2, TX_DESC2_IC.0, TX_DESC2_IC.1, 1);
            }
        }
        ring[(start + i) % len] = Desc {
            desc0: lo,
            desc1: hi,
            desc2,
            desc3,
        };
    }

    Ok((start + count) % len)
}

/// Set `OWN` on the frame's first descriptor, which is what actually hands the
/// frame to the engine. The caller issues its write barrier between
/// [`build_tx_frame`] and this call; nothing here can do it for them, because
/// the ring is ordinary memory and the ordering that matters is the one the
/// caller's CPU publishes.
#[inline]
pub fn hand_tx_frame_to_dma(ring: &mut [Desc], start: usize) {
    ring[start].desc3 |= DESC3_OWN;
}

/// Poke the transmit engine by writing the address of the next FREE
/// descriptor to the tail pointer. `xgbe_tx_start_xmit`.
///
/// Note which descriptor that is: the one AFTER the frame, not the frame's
/// last. The transmit tail is a stop sign --- the engine runs up to it and
/// halts --- while the receive tail of [`program_rx_ring`] points at the last
/// descriptor the driver owns. Same register name on two rings, opposite
/// convention.
#[inline]
pub fn kick_tx(acc: &dyn MmioAccess, channel: u32, next_free_paddr: u64) {
    acc.write32(
        dma_channel_reg(channel, DMA_CH_TDTR_LO),
        next_free_paddr as u32,
    );
}

/// The bus address of descriptor `index` in a ring that starts at
/// `ring_paddr`. Both tail pointers want an address, not an index.
#[inline]
pub const fn desc_paddr(ring_paddr: u64, index: usize) -> u64 {
    ring_paddr + (index as u64) * core::mem::size_of::<Desc>() as u64
}

// ——— Reading back what the chip received ———

/// `RX_NORMAL_DESC0_OVT`, index 0, width 16: the outer VLAN tag.
///
/// On the way IN, `desc0` was the low half of the header buffer's address.
/// The chip overwrites the address words with results, so every field below
/// is only meaningful once `OWN` has gone back to the driver.
pub const RX_DESC0_OVT: (u32, u32) = (0, 16);
/// `RX_NORMAL_DESC2_TNP`, index 11, width 1: the frame arrived tunnelled.
pub const RX_DESC2_TNP: (u32, u32) = (11, 1);
/// `RX_NORMAL_DESC3_ETLT`, index 16, width 4: "error type / length type".
/// One field, two jobs, told apart by [`RX_DESC3_ES`] --- and not the way
/// round the name suggests. See [`rx_frame_verdict`].
pub const RX_DESC3_ETLT: (u32, u32) = (16, 4);
/// `RX_NORMAL_DESC3_ES`, index 15, width 1: the error summary.
pub const RX_DESC3_ES: (u32, u32) = (15, 1);
/// `RX_NORMAL_DESC3_L34T`, index 20, width 4: which layer-3 and layer-4
/// protocols the chip recognised.
pub const RX_DESC3_L34T: (u32, u32) = (20, 4);
/// `RX_NORMAL_DESC3_RSV`, index 26, width 1: `desc1` holds an RSS hash.
pub const RX_DESC3_RSV: (u32, u32) = (26, 1);
/// `RX_NORMAL_DESC3_CDA`, index 27, width 1: a context descriptor follows
/// this one.
pub const RX_DESC3_CDA: (u32, u32) = (27, 1);

/// `ETLT` 0x09 with no error: the frame carried a VLAN tag, and `desc0`'s
/// [`RX_DESC0_OVT`] holds it. A value in the field named for error types
/// that is not an error at all.
pub const ETLT_VLAN_TAG: u32 = 0x09;
/// `ETLT` 0x05 with an error: the IP header checksum was wrong.
pub const ETLT_IP_HEADER_CSUM: u32 = 0x05;
/// `ETLT` 0x06 with an error: the payload checksum was wrong.
pub const ETLT_PAYLOAD_CSUM: u32 = 0x06;

/// `RX_DESC3_L34T_IPV4_TCP`.
pub const L34T_IPV4_TCP: u32 = 1;
/// `RX_DESC3_L34T_IPV4_UDP`.
pub const L34T_IPV4_UDP: u32 = 2;
/// `RX_DESC3_L34T_IPV6_TCP`.
pub const L34T_IPV6_TCP: u32 = 9;
/// `RX_DESC3_L34T_IPV6_UDP`.
pub const L34T_IPV6_UDP: u32 = 10;

/// Whether the RSS hash covers layer 4 as well as layer 3, from `L34T`. The
/// four values that name a transport protocol hash over the ports too;
/// everything else, ICMP and the two "unknown"s included, hashes over the
/// addresses alone.
#[inline]
pub const fn rss_hash_covers_ports(l34t: u32) -> bool {
    matches!(
        l34t,
        L34T_IPV4_TCP | L34T_IPV4_UDP | L34T_IPV6_TCP | L34T_IPV6_UDP
    )
}

/// What the chip thought of a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxVerdict {
    /// Nothing wrong with it.
    Clean,
    /// A checksum was wrong, but the frame itself arrived intact. Linux hands
    /// this one up the stack with the hardware-checksum flag cleared, rather
    /// than dropping it: the frame is real and something above may still want
    /// it.
    ChecksumBad,
    /// The frame is damaged. Drop it.
    FrameBad,
}

/// Read `ES` and `ETLT` together and say what happened.
///
/// The trap is the condition Linux writes as `if (!err || !etlt)`: a frame is
/// fine when the error summary is clear **OR** when the error type is zero.
/// An error summary on its own is NOT an error --- `ETLT` has to name one
/// too. Reading `ES` alone, which is what its name invites, drops good
/// frames.
///
/// This driver has no tunnelling support, so the two `ETLT` values that only
/// mean a tunnel checksum error on a tunnelled frame (0x09 and 0x0a) land in
/// [`RxVerdict::FrameBad`] here, which is exactly where Linux puts them when
/// the frame is not tunnelled.
#[inline]
pub const fn rx_frame_verdict(desc3: u32) -> RxVerdict {
    let err = get_bits(desc3, RX_DESC3_ES.0, RX_DESC3_ES.1);
    let etlt = get_bits(desc3, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1);
    if err == 0 || etlt == 0 {
        return RxVerdict::Clean;
    }
    match etlt {
        ETLT_IP_HEADER_CSUM | ETLT_PAYLOAD_CSUM => RxVerdict::ChecksumBad,
        _ => RxVerdict::FrameBad,
    }
}

/// Everything a written-back normal receive descriptor says.
///
/// The `Option`s are not politeness: each of those fields is only meaningful
/// on one end of a multi-descriptor frame, and the header length and the
/// packet length sit in the *same descriptor* under opposite conditions. A
/// struct of plain integers would invite reading a length off the middle of a
/// frame, where the field holds whatever the chip last left there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxDescriptor {
    /// `FD`: the frame starts here.
    pub first: bool,
    /// `LD`: the frame ends here.
    pub last: bool,
    /// `HL`, valid on the first descriptor only: how many bytes went into the
    /// header buffer when split-header receive is on. Zero means the whole
    /// frame went into the payload buffer.
    pub header_len: Option<u32>,
    /// `PL`, valid on the last descriptor only: the length of the whole
    /// frame.
    pub packet_len: Option<u32>,
    /// Valid on the last descriptor only, for the same reason.
    pub verdict: Option<RxVerdict>,
    /// The outer VLAN tag, when the frame carried one.
    pub vlan_tag: Option<u16>,
    /// `desc1`, when `RSV` says it is a hash rather than the address half it
    /// used to be.
    pub rss_hash: Option<u32>,
    /// Whether that hash covers the layer-4 ports.
    pub rss_covers_ports: bool,
    /// `CDA`: the next descriptor is a context descriptor, not another
    /// fragment.
    pub context_next: bool,
}

/// What came back in a receive descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxRead {
    /// `OWN` is still set: the chip has not finished with this descriptor and
    /// nothing in it may be read. `xgbe_dev_read` returning 1.
    NotYours,
    /// A context descriptor, which carries a timestamp rather than a frame.
    Context,
    /// A normal descriptor.
    Normal(RxDescriptor),
}

/// Parse a written-back receive descriptor. `xgbe_dev_read`.
///
/// The caller issues its read barrier between seeing `OWN` clear and using
/// anything else, which is why the `OWN` test comes first and alone: every
/// other word of the descriptor was written by the chip and is only settled
/// once ownership has come back.
pub fn read_rx_desc(desc: &Desc) -> RxRead {
    if desc_owned_by_dma(desc.desc3) {
        return RxRead::NotYours;
    }
    if desc_is_context(desc.desc3) {
        return RxRead::Context;
    }

    let first = desc.desc3 & DESC3_FD != 0;
    let last = desc.desc3 & DESC3_LD != 0;
    let verdict = if last {
        Some(rx_frame_verdict(desc.desc3))
    } else {
        None
    };
    let vlan_tag = match verdict {
        Some(RxVerdict::Clean)
            if get_bits(desc.desc3, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1) == ETLT_VLAN_TAG =>
        {
            Some(get_bits(desc.desc0, RX_DESC0_OVT.0, RX_DESC0_OVT.1) as u16)
        }
        _ => None,
    };
    let rss_hash = if get_bits(desc.desc3, RX_DESC3_RSV.0, RX_DESC3_RSV.1) != 0 {
        Some(desc.desc1)
    } else {
        None
    };

    RxRead::Normal(RxDescriptor {
        first,
        last,
        header_len: if first {
            Some(rx_header_len(desc.desc2))
        } else {
            None
        },
        packet_len: if last {
            Some(rx_packet_len(desc.desc3))
        } else {
            None
        },
        verdict,
        vlan_tag,
        rss_hash,
        rss_covers_ports: rss_hash.is_some()
            && rss_hash_covers_ports(get_bits(desc.desc3, RX_DESC3_L34T.0, RX_DESC3_L34T.1)),
        context_next: get_bits(desc.desc3, RX_DESC3_CDA.0, RX_DESC3_CDA.1) != 0,
    })
}

/// Whether the frame arrived through a tunnel. `RX_NORMAL_DESC2_TNP`.
///
/// Nothing in this driver acts on it yet --- tunnel offload needs the two
/// `ETLT` values that only mean a checksum error on a tunnelled frame, and
/// those are deliberately left as [`RxVerdict::FrameBad`] above. It is named
/// so that the bit does not look free when somebody comes to use `desc2` for
/// something else.
#[inline]
pub const fn rx_is_tunnelled(desc2: u32) -> bool {
    get_bits(desc2, RX_DESC2_TNP.0, RX_DESC2_TNP.1) != 0
}

// ——— Interrupts: enabling them, and reading what fired ———

/// `DMA_CH_IER_TIE`, index 0: a frame finished transmitting.
pub const DMA_CH_IER_TIE: (u32, u32) = (0, 1);
/// `DMA_CH_IER_TXSE`, index 1: the transmit process stopped.
pub const DMA_CH_IER_TXSE: (u32, u32) = (1, 1);
/// `DMA_CH_IER_TBUE`, index 2: the transmit ring ran out of descriptors.
pub const DMA_CH_IER_TBUE: (u32, u32) = (2, 1);
/// `DMA_CH_IER_RIE`, index 6: a frame arrived.
pub const DMA_CH_IER_RIE: (u32, u32) = (6, 1);
/// `DMA_CH_IER_RBUE`, index 7: the receive ring ran out of descriptors.
pub const DMA_CH_IER_RBUE: (u32, u32) = (7, 1);
/// `DMA_CH_IER_RSE`, index 8: the receive process stopped.
pub const DMA_CH_IER_RSE: (u32, u32) = (8, 1);
/// `DMA_CH_IER_FBEE`, index 12: a fatal bus error.
pub const DMA_CH_IER_FBEE: (u32, u32) = (12, 1);

/// `DMA_CH_IER_AIE`, index 14: the abnormal interrupt summary, on a core at
/// [`FLUSH_MIN_SNPSVER`] or above.
pub const DMA_CH_IER_AIE: (u32, u32) = (14, 1);
/// `DMA_CH_IER_NIE`, index 15: the normal interrupt summary, on a core at
/// [`FLUSH_MIN_SNPSVER`] or above.
pub const DMA_CH_IER_NIE: (u32, u32) = (15, 1);
/// `DMA_CH_IER_AIE20`, index 15: the abnormal summary on an older core.
///
/// The same bit 15 that is `NIE` above. The two summary bits shifted by one
/// when the core moved to 0x21, so a mask built for the new layout and
/// written to an old core enables the ABNORMAL summary where it meant the
/// normal one, and leaves the normal one off --- an interface that reports
/// bus errors and never reports a received frame.
pub const DMA_CH_IER_AIE20: (u32, u32) = (15, 1);
/// `DMA_CH_IER_NIE20`, index 16: the normal summary on an older core.
pub const DMA_CH_IER_NIE20: (u32, u32) = (16, 1);

/// `DMA_CH_SR_TI`, index 0.
pub const DMA_CH_SR_TI: (u32, u32) = (0, 1);
/// `DMA_CH_SR_TPS`, index 1.
pub const DMA_CH_SR_TPS: (u32, u32) = (1, 1);
/// `DMA_CH_SR_TBU`, index 2.
pub const DMA_CH_SR_TBU: (u32, u32) = (2, 1);
/// `DMA_CH_SR_RI`, index 6.
pub const DMA_CH_SR_RI: (u32, u32) = (6, 1);
/// `DMA_CH_SR_RBU`, index 7.
pub const DMA_CH_SR_RBU: (u32, u32) = (7, 1);
/// `DMA_CH_SR_RPS`, index 8.
pub const DMA_CH_SR_RPS: (u32, u32) = (8, 1);
/// `DMA_CH_SR_FBE`, index 12.
pub const DMA_CH_SR_FBE: (u32, u32) = (12, 1);

/// The two summary enables for a core of this version: `(normal, abnormal)`.
/// Everything below bit 14 kept its place; only these two moved.
#[inline]
pub const fn summary_enables(snps_ver: u8) -> ((u32, u32), (u32, u32)) {
    if snps_ver < FLUSH_MIN_SNPSVER {
        (DMA_CH_IER_NIE20, DMA_CH_IER_AIE20)
    } else {
        (DMA_CH_IER_NIE, DMA_CH_IER_AIE)
    }
}

/// Build a channel's interrupt enable mask. `xgbe_enable_dma_interrupts`'s
/// `curr_ier`, for the case this driver has: one interrupt for the whole
/// device, so both the transmit-complete and receive-complete enables are
/// wanted.
///
/// `RBUE` goes on for a receive ring but there is no `TBUE` for a transmit
/// one. A transmit ring running dry is the ordinary state of an idle
/// interface and the driver refills it when it next sends; a receive ring
/// running dry means frames are being dropped on the floor and somebody has
/// to be told.
#[inline]
pub const fn channel_interrupt_mask(snps_ver: u8, has_tx: bool, has_rx: bool) -> u32 {
    let (nie, aie) = summary_enables(snps_ver);
    let mut ier = (1 << nie.0) | (1 << aie.0) | (1 << DMA_CH_IER_FBEE.0);
    if has_tx {
        ier |= 1 << DMA_CH_IER_TIE.0;
    }
    if has_rx {
        ier |= (1 << DMA_CH_IER_RBUE.0) | (1 << DMA_CH_IER_RIE.0);
    }
    ier
}

/// Turn a channel's interrupts on: acknowledge whatever is already pending,
/// then write the mask whole.
///
/// The order is not a style choice. A bit left standing in the status
/// register from before the mask lands fires the instant it does, and the
/// handler then runs for an event that happened while the interface was
/// down. And the mask is written WHOLE rather than OR'd in, because the
/// summary bits moved between core versions: a read-modify-write would
/// preserve the other version's summary bit from whatever configuration ran
/// before.
pub fn enable_channel_interrupts(
    acc: &dyn MmioAccess,
    channel: u32,
    snps_ver: u8,
    has_tx: bool,
    has_rx: bool,
) {
    ack_channel_interrupts(acc, channel);
    acc.write32(
        dma_channel_reg(channel, DMA_CH_IER),
        channel_interrupt_mask(snps_ver, has_tx, has_rx),
    );
}

/// Turn a channel's interrupts off. Zero is zero in either layout, so this
/// one does not need the version.
#[inline]
pub fn disable_channel_interrupts(acc: &dyn MmioAccess, channel: u32) {
    acc.write32(dma_channel_reg(channel, DMA_CH_IER), 0);
}

/// Acknowledge a channel's pending interrupts, and return what was pending.
///
/// The status register is write-one-to-clear, so it is cleared by writing
/// back exactly what was read: writing a zero clears NOTHING, and writing all
/// ones would acknowledge events that had not happened. Reading first is also
/// what makes this the handler's one call --- the bits are gone after the
/// write, so a second read would come back empty.
pub fn ack_channel_interrupts(acc: &dyn MmioAccess, channel: u32) -> u32 {
    let reg = dma_channel_reg(channel, DMA_CH_SR);
    let status = acc.read32(reg);
    acc.write32(reg, status);
    status
}

/// What a channel's status register was reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmaStatus {
    /// `TI`: a frame went out.
    pub tx_complete: bool,
    /// `RI`: a frame came in.
    pub rx_complete: bool,
    /// `TBU`: the transmit ring is empty. Expected on an idle interface.
    pub tx_buffer_unavailable: bool,
    /// `RBU`: the receive ring is empty, and frames are being dropped.
    pub rx_buffer_unavailable: bool,
    /// `TPS`: the transmit process stopped.
    pub tx_stopped: bool,
    /// `RPS`: the receive process stopped.
    pub rx_stopped: bool,
    /// `FBE`: a fatal bus error. Nothing short of a reset recovers from this.
    pub fatal_bus_error: bool,
}

/// Decode a channel status word.
#[inline]
pub const fn decode_dma_status(sr: u32) -> DmaStatus {
    DmaStatus {
        tx_complete: get_bits(sr, DMA_CH_SR_TI.0, DMA_CH_SR_TI.1) != 0,
        rx_complete: get_bits(sr, DMA_CH_SR_RI.0, DMA_CH_SR_RI.1) != 0,
        tx_buffer_unavailable: get_bits(sr, DMA_CH_SR_TBU.0, DMA_CH_SR_TBU.1) != 0,
        rx_buffer_unavailable: get_bits(sr, DMA_CH_SR_RBU.0, DMA_CH_SR_RBU.1) != 0,
        tx_stopped: get_bits(sr, DMA_CH_SR_TPS.0, DMA_CH_SR_TPS.1) != 0,
        rx_stopped: get_bits(sr, DMA_CH_SR_RPS.0, DMA_CH_SR_RPS.1) != 0,
        fatal_bus_error: get_bits(sr, DMA_CH_SR_FBE.0, DMA_CH_SR_FBE.1) != 0,
    }
}

/// Whether anything in this status needs the driver to act. A status of all
/// zeroes happens: on a shared line another device's interrupt brings the
/// handler here too.
#[inline]
pub const fn dma_status_is_quiet(s: &DmaStatus) -> bool {
    !(s.tx_complete
        || s.rx_complete
        || s.tx_buffer_unavailable
        || s.rx_buffer_unavailable
        || s.tx_stopped
        || s.rx_stopped
        || s.fatal_bus_error)
}

// ——— Sharing the FIFO out between the queues ———

/// `XGMAC_FIFO_UNIT`: the granularity the FIFO is handed out in. Everything
/// below is counted in these, never in bytes.
pub const FIFO_UNIT: u32 = 256;
/// `XGMAC_FIFO_MIN_ALLOC`: what a non-priority receive queue gets.
pub const FIFO_MIN_ALLOC: u32 = 2048;
/// `IEEE_8021QAZ_MAX_TCS`: how many receive queues carry ordinary traffic.
/// Receive queues 9 and up (index 8 and up) carry PTP and DCB control frames
/// and do not need a large FIFO.
pub const MAX_PRIO_QUEUES: u32 = 8;

/// `MTL_Q_TQOMR_TQS`, index 16, width **10**.
pub const MTL_Q_TQOMR_TQS: (u32, u32) = (16, 10);
/// `MTL_Q_RQOMR_RQS`, index 16, width **9**.
///
/// One bit narrower than the transmit side's field at the same index of the
/// matching register. A helper that took the width as read from either side
/// would silently halve, or overflow, the other's largest queue.
pub const MTL_Q_RQOMR_RQS: (u32, u32) = (16, 9);

/// Turn a per-queue size in bytes into the value the register takes:
/// units of [`FIFO_UNIT`], minus one, because zero there means ONE unit.
///
/// Note where the asymmetry is. A queue of 256 bytes and a queue of 511
/// bytes both come out as zero, which is right --- the hardware cannot
/// express the difference. But a queue of *less* than 256 bytes also comes
/// out as zero, which grants it 256 bytes it was never given. Linux does the
/// same (`if (p_fifo) p_fifo--`), because the alternative is a queue with no
/// FIFO at all, and that does not work either.
#[inline]
pub const fn fifo_units(bytes: u32) -> u32 {
    let units = bytes / FIFO_UNIT;
    if units > 0 {
        units - 1
    } else {
        0
    }
}

/// The register value every queue gets when the FIFO is split equally.
/// `xgbe_calculate_equal_fifo`.
///
/// The remainder of the division is dropped rather than handed to queue
/// zero. A few hundred spare bytes are worth less than every queue behaving
/// the same way under load.
pub fn equal_fifo_share(total_bytes: u32, queues: u32) -> DeviceResult<u32> {
    if queues == 0 {
        return Err(DeviceError::InvalidParam);
    }
    Ok(fifo_units(total_bytes / queues))
}

/// How many of `queues` are priority queues, i.e. get a share of the FIFO
/// rather than the fixed minimum. `XGMAC_PRIO_QUEUES`.
#[inline]
pub const fn prio_queues(queues: u32) -> u32 {
    if queues < MAX_PRIO_QUEUES {
        queues
    } else {
        MAX_PRIO_QUEUES
    }
}

/// Program the transmit FIFO split. `xgbe_config_tx_fifo_size`.
///
/// Every transmit queue is a priority queue: the non-priority rule below is
/// a receive-side thing only.
pub fn config_tx_fifo(acc: &dyn MmioAccess, total_bytes: u32, queues: u32) -> DeviceResult<()> {
    let share = equal_fifo_share(total_bytes, queues)?;
    for q in 0..queues {
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_TQOMR),
            MTL_Q_TQOMR_TQS.0,
            MTL_Q_TQOMR_TQS.1,
            share,
        );
    }
    Ok(())
}

/// Program the receive FIFO split. `xgbe_config_rx_fifo_size` without DCB:
/// the queues above [`MAX_PRIO_QUEUES`] take [`FIFO_MIN_ALLOC`] each off the
/// top, and what is left is split equally among the rest.
///
/// The subtraction comes FIRST. Splitting the whole FIFO and then carving
/// the minimum out of it would promise the priority queues more than exists,
/// and the overrun does not show up as an error --- it shows up as frames
/// dropped under load on whichever queue the hardware runs out on.
pub fn config_rx_fifo(acc: &dyn MmioAccess, total_bytes: u32, queues: u32) -> DeviceResult<()> {
    let prio = prio_queues(queues);
    if prio == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let nonprio = queues - prio;
    let reserved = nonprio
        .checked_mul(FIFO_MIN_ALLOC)
        .ok_or(DeviceError::InvalidParam)?;
    if reserved >= total_bytes {
        return Err(DeviceError::InvalidParam);
    }
    let share = equal_fifo_share(total_bytes - reserved, prio)?;
    let minimum = fifo_units(FIFO_MIN_ALLOC);
    for q in 0..queues {
        let value = if q < prio { share } else { minimum };
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_RQOMR),
            MTL_Q_RQOMR_RQS.0,
            MTL_Q_RQOMR_RQS.1,
            value,
        );
    }
    Ok(())
}

// ——— When to ask the other end to stop ———

/// `MTL_Q_RQFCR`: a receive queue's flow-control thresholds.
pub const MTL_Q_RQFCR: usize = 0x50;
/// `MTL_Q_RQFCR_RFA`, index **1**, width 6. Not index 0: bit 0 of this
/// register is not part of the field, so a threshold written from the bottom
/// of the word comes out halved.
pub const MTL_Q_RQFCR_RFA: (u32, u32) = (1, 6);
/// `MTL_Q_RQFCR_RFD`, index 17, width 6.
pub const MTL_Q_RQFCR_RFD: (u32, u32) = (17, 6);

/// `XGMAC_FLOW_CONTROL_UNIT`: the granularity of a threshold.
///
/// Note it is NOT [`FIFO_UNIT`], and the bias below is not the FIFO's
/// either. Two neighbouring quantities in the same driver, both "units minus
/// a bias", with different units and different biases.
pub const FLOW_CONTROL_UNIT: u32 = 512;

/// `XGMAC_FLOW_CONTROL_ALIGN`: round up to a whole [`FLOW_CONTROL_UNIT`].
#[inline]
pub const fn flow_control_align(bytes: u32) -> u32 {
    bytes.div_ceil(FLOW_CONTROL_UNIT) * FLOW_CONTROL_UNIT
}

/// `XGMAC_FLOW_CONTROL_VALUE`: bytes to the register's units, which are
/// [`FLOW_CONTROL_UNIT`] minus **two**.
#[inline]
pub const fn flow_control_value(bytes: u32) -> u32 {
    if bytes < 2 * FLOW_CONTROL_UNIT {
        0
    } else {
        bytes / FLOW_CONTROL_UNIT - 2
    }
}

/// A receive queue's two water marks, as the register takes them.
///
/// Both are measured from FULL, backwards: a threshold of zero means "when
/// 1024 bytes of room are left", and a BIGGER number means the chip asks the
/// other end to stop EARLIER, not later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlowControl {
    /// `RFA`: start asking the other end to pause.
    pub rfa: u32,
    /// `RFD`: stop asking.
    ///
    /// Always above [`FlowControl::rfa`], and because both count backwards
    /// from full that means the queue has to drain FURTHER before the pause
    /// is lifted. Swapping the two gives a pause that is lifted the instant
    /// it is raised, which on a loaded link is a storm of pause frames
    /// rather than no flow control at all.
    pub rfd: u32,
}

/// Whether flow control is off for this queue.
///
/// It is `rfd` that says so, not `rfa`: a zero in `rfa` is a perfectly real
/// threshold --- it is what a queue between 2 and 4 KiB gets --- and reading
/// "off" off it would silently disable flow control on exactly the queues
/// that were configured for it.
#[inline]
pub const fn flow_control_is_off(fc: &FlowControl) -> bool {
    fc.rfd == 0
}

/// The water marks for a receive queue of `q_fifo_bytes`, given the largest
/// frame the interface accepts. `xgbe_queue_flow_control_threshold` without
/// priority-based flow control.
///
/// Four bands, and the first three are constants out of Linux rather than
/// anything derived: under 2 KiB there is no room to pause in at all, and
/// between 2 KiB and a frame the fixed answers are what fits.
pub fn queue_flow_control(q_fifo_bytes: u32, max_frame: u32) -> FlowControl {
    let frame_fifo = flow_control_align(max_frame);

    if q_fifo_bytes <= 2048 {
        // No room to pause in: a queue this small cannot hold what is still
        // in flight by the time the other end sees the pause.
        return FlowControl { rfa: 0, rfd: 0 };
    }
    if q_fifo_bytes <= 4096 {
        // Full - 1024 bytes, and full - 1536.
        return FlowControl { rfa: 0, rfd: 1 };
    }
    if q_fifo_bytes <= frame_fifo {
        // Full - 2048 bytes, and full - 3584.
        return FlowControl { rfa: 2, rfd: 5 };
    }
    if q_fifo_bytes <= frame_fifo * 3 {
        // Pause once just over a frame has arrived; resume with just under
        // half a frame of room left.
        let rfa = q_fifo_bytes - frame_fifo;
        let rfd = rfa + frame_fifo / 2;
        return FlowControl {
            rfa: flow_control_value(rfa),
            rfd: flow_control_value(rfd),
        };
    }
    // Room for more than three frames: pause with just over two frames of
    // space still free.
    let rfa = frame_fifo * 2 + FLOW_CONTROL_UNIT;
    let rfd = rfa + frame_fifo;
    FlowControl {
        rfa: flow_control_value(rfa),
        rfd: flow_control_value(rfd),
    }
}

/// Write a queue's water marks. `xgbe_config_flow_control_threshold`.
pub fn config_flow_control(acc: &dyn MmioAccess, queue: u32, fc: &FlowControl) {
    let reg = mtl_queue_reg(queue, MTL_Q_RQFCR);
    write_field(acc, reg, MTL_Q_RQFCR_RFA.0, MTL_Q_RQFCR_RFA.1, fc.rfa);
    write_field(acc, reg, MTL_Q_RQFCR_RFD.0, MTL_Q_RQFCR_RFD.1, fc.rfd);
}

// ——— Who feeds whom: the three mappings ———

/// `MTL_Q_TQOMR_Q2TCMAP`, index 8, width 3: which traffic class a transmit
/// queue belongs to. Same register as the FIFO size of the batch before,
/// well clear of it.
pub const MTL_Q_TQOMR_Q2TCMAP: (u32, u32) = (8, 3);

/// `MAC_RQC2R`: the first of the registers mapping VLAN priorities to
/// receive queues.
pub const MAC_RQC2R: usize = 0x00a8;
/// `MAC_RQC2_INC` / `MAC_RQC2_Q_PER_REG`: four queues per register, one byte
/// each.
pub const MAC_RQC2_INC: usize = 4;
/// How many queues share one `MAC_RQC2R` register.
pub const MAC_RQC2_Q_PER_REG: u32 = 4;

/// `MTL_RQDCM0R`: the first of the registers mapping receive queues to DMA
/// channels.
pub const MTL_RQDCM0R: usize = 0x1030;
/// `MTL_RQDCM_INC`.
pub const MTL_RQDCM_INC: usize = 4;
/// `MTL_RQDCM_Q_PER_REG`.
pub const MTL_RQDCM_Q_PER_REG: u32 = 4;

/// The value Linux writes in a queue's `MTL_RQDCM0R` byte.
///
/// It is NOT a channel number, which is what a byte in a queue-to-channel
/// map looks like it should hold. It is the bit that says "map this queue to
/// a channel dynamically". A channel index written here --- 0 for the first
/// channel, say --- would read as that bit clear and nail every queue to
/// channel zero, which works right up until the second channel is used.
pub const RQDCM_DYNAMIC: u32 = 0x80;

/// Where index `n`'s byte lives when a map is packed four to a register:
/// the register's offset and the shift within it.
#[inline]
pub const fn packed_byte(base: usize, inc: usize, per_reg: u32, n: u32) -> (usize, u32) {
    let reg = base + (n / per_reg) as usize * inc;
    let shift = (n % per_reg) * 8;
    (reg, shift)
}

/// Which of `buckets` buckets gets item range `(start, len)` for bucket
/// `index`, spreading `total` items as evenly as they go.
///
/// The remainder is NOT all given to the last bucket: it goes one item each
/// to the first few. Eleven queues over four classes is 3, 3, 3, 2 --- not
/// 2, 2, 2, 5. That matters because the classes are served in order, so
/// piling the leftovers on one of them makes that one behave differently
/// under load.
#[inline]
pub const fn bucket_span(index: u32, total: u32, buckets: u32) -> (u32, u32) {
    let base = total / buckets;
    let extra = total % buckets;
    let len = if index < extra { base + 1 } else { base };
    let start = index * base + if index < extra { index } else { extra };
    (start, len)
}

/// Which traffic class a transmit queue belongs to.
#[inline]
pub fn tc_of_tx_queue(queue: u32, tx_queues: u32, tc_count: u32) -> Option<u32> {
    if tc_count == 0 || queue >= tx_queues {
        return None;
    }
    for tc in 0..tc_count {
        let (start, len) = bucket_span(tc, tx_queues, tc_count);
        if queue >= start && queue < start + len {
            return Some(tc);
        }
    }
    None
}

/// Map the transmit queues onto the traffic classes. The first part of
/// `xgbe_config_queue_mapping`.
pub fn map_tx_queues_to_tc(
    acc: &dyn MmioAccess,
    tx_queues: u32,
    tc_count: u32,
) -> DeviceResult<()> {
    if tc_count == 0 || tx_queues < tc_count {
        // A class with no queue has nowhere to put a frame.
        return Err(DeviceError::InvalidParam);
    }
    for queue in 0..tx_queues {
        let tc = tc_of_tx_queue(queue, tx_queues, tc_count).ok_or(DeviceError::InvalidParam)?;
        write_field(
            acc,
            mtl_queue_reg(queue, MTL_Q_TQOMR),
            MTL_Q_TQOMR_Q2TCMAP.0,
            MTL_Q_TQOMR_Q2TCMAP.1,
            tc,
        );
    }
    Ok(())
}

/// The bitmask of VLAN priorities a receive queue is to accept.
///
/// A byte per queue holding a mask of priorities --- not, as the name
/// "priority to queue map" suggests, a byte per priority holding a queue
/// number. The direction is the other way round from the way it is
/// described, and a map written the described way would put queue numbers
/// where the hardware reads priority bits.
#[inline]
pub fn prio_mask_for_queue(queue: u32, prio_queue_count: u32) -> u32 {
    if prio_queue_count == 0 || queue >= prio_queue_count {
        return 0;
    }
    let (start, len) = bucket_span(queue, MAX_PRIO_QUEUES, prio_queue_count);
    let mut mask = 0;
    for prio in start..start + len {
        mask |= 1 << prio;
    }
    mask
}

/// Spread the eight VLAN priorities over the receive queues. The second part
/// of `xgbe_config_queue_mapping`.
pub fn map_prios_to_rx_queues(acc: &dyn MmioAccess, rx_queues: u32) -> DeviceResult<()> {
    let prio = prio_queues(rx_queues);
    if prio == 0 {
        return Err(DeviceError::InvalidParam);
    }
    write_packed_bytes(
        acc,
        MAC_RQC2R,
        MAC_RQC2_INC,
        MAC_RQC2_Q_PER_REG,
        prio,
        |q| prio_mask_for_queue(q, prio),
    );
    Ok(())
}

/// Hand every receive queue to the dynamic channel mapping. The third part
/// of `xgbe_config_queue_mapping`.
pub fn map_rx_queues_to_channels(acc: &dyn MmioAccess, rx_queues: u32) -> DeviceResult<()> {
    if rx_queues == 0 {
        return Err(DeviceError::InvalidParam);
    }
    write_packed_bytes(
        acc,
        MTL_RQDCM0R,
        MTL_RQDCM_INC,
        MTL_RQDCM_Q_PER_REG,
        rx_queues,
        |_| RQDCM_DYNAMIC,
    );
    Ok(())
}

/// Write a byte-packed map, one whole register at a time.
///
/// Whole registers, not fields: these maps have no other tenants, and a
/// read-modify-write per byte would keep a mapping left behind by whatever
/// configuration ran before. The last register is written even when it is
/// only partly filled, with its unused bytes zero.
fn write_packed_bytes(
    acc: &dyn MmioAccess,
    base: usize,
    inc: usize,
    per_reg: u32,
    count: u32,
    mut byte_for: impl FnMut(u32) -> u32,
) {
    let mut value = 0u32;
    for n in 0..count {
        let (reg, shift) = packed_byte(base, inc, per_reg, n);
        value |= (byte_for(n) & 0xff) << shift;
        if (n + 1) % per_reg == 0 || n + 1 == count {
            acc.write32(reg, value);
            value = 0;
        }
    }
}

// ——— Store and forward, and who goes first ———

/// `MTL_Q_TQOMR_TSF`, index **1**, width 1: store the whole frame before
/// starting to transmit it.
///
/// Index 1, not 0. Bit 0 of this register is `FTQ`, the flush of batch six:
/// a store-and-forward bit written from the bottom of the word empties the
/// queue instead.
pub const MTL_Q_TQOMR_TSF: (u32, u32) = (1, 1);
/// `MTL_Q_TQOMR_TTC`, index 4, width **3**: how much of a frame to collect
/// before starting, when store-and-forward is off.
pub const MTL_Q_TQOMR_TTC: (u32, u32) = (4, 3);
/// `MTL_Q_RQOMR_RSF`, index **5**, width 1: the receive side's
/// store-and-forward.
///
/// Not index 1. The two store-and-forward bits carry the same name and sit
/// four bits apart in the two matching registers.
pub const MTL_Q_RQOMR_RSF: (u32, u32) = (5, 1);
/// `MTL_Q_RQOMR_RTC`, index 0, width **2**: the receive threshold. Different
/// index and different width from the transmit one.
pub const MTL_Q_RQOMR_RTC: (u32, u32) = (0, 2);

/// `MTL_OMR`: the MTL's own operating mode, one register for the block.
pub const MTL_OMR: usize = 0x1000;
/// `MTL_OMR_RAA`, index 2, width 1: the receive arbitration algorithm.
pub const MTL_OMR_RAA: (u32, u32) = (2, 1);
/// `MTL_OMR_ETSALG`, index 5, width 2: the transmit scheduling algorithm.
pub const MTL_OMR_ETSALG: (u32, u32) = (5, 2);
/// `MTL_ETSALG_WRR`: weighted round robin. **Zero.**
pub const MTL_ETSALG_WRR: u32 = 0x00;
/// `MTL_RAA_SP`: strict priority. **Also zero.**
///
/// Both scheduler selections are zero, so the whole of
/// [`config_mtl_scheduling`]'s first half writes nothing but zeroes. That
/// reads like dead code and is not: the register is not guaranteed to come
/// up this way, and the traffic-class half below is not zero at all.
pub const MTL_RAA_SP: u32 = 0x00;

/// `MTL_TC_ETSCR`, inside the same per-index block as the queue registers.
pub const MTL_TC_ETSCR: usize = 0x10;
/// `MTL_TC_QWR`, likewise.
pub const MTL_TC_QWR: usize = 0x18;
/// `MTL_TC_ETSCR_TSA`, index 0, width 2.
pub const MTL_TC_ETSCR_TSA: (u32, u32) = (0, 2);
/// `MTL_TC_QWR_QW`, index 0, width 21.
pub const MTL_TC_QWR_QW: (u32, u32) = (0, 21);
/// `MTL_TSA_ETS`: enhanced transmission selection.
pub const MTL_TSA_ETS: u32 = 0x02;

/// A traffic class's register block. `MTL_TC_BASE` **is** `MTL_Q_BASE` and
/// `MTL_TC_INC` **is** `MTL_Q_INC`: traffic class N and queue N share one
/// 0x80 block, with the queue registers at 0x00 and 0x40 and the class
/// registers at 0x10 and 0x18 inside it.
///
/// So there is no separate base to get wrong --- but a class index above the
/// queue count lands squarely in some other queue's block, which is why
/// [`config_tc_weights`] refuses one.
#[inline]
pub const fn mtl_tc_reg(tc: u32, reg: usize) -> usize {
    mtl_queue_reg(tc, reg)
}

/// Whether a queue holds a whole frame before moving it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarding {
    /// Wait for the whole frame. What this driver wants: a frame that
    /// under-runs halfway out of the port is a frame on the wire with a bad
    /// CRC.
    StoreAndForward,
    /// Start once `threshold` units have arrived. The threshold is the raw
    /// `TTC`/`RTC` value, which means different byte counts on the two
    /// sides.
    CutThrough { threshold: u32 },
}

/// Set the transmit queues' forwarding mode and threshold.
///
/// The threshold is written even in store-and-forward, where the hardware
/// ignores it, which is what Linux does. It is not dead: it is what the
/// queue falls back to if store-and-forward is ever turned off, and leaving
/// it at whatever the last configuration set is worse than setting it.
pub fn config_tx_forwarding(acc: &dyn MmioAccess, queues: u32, mode: Forwarding) {
    let (sf, ttc) = match mode {
        Forwarding::StoreAndForward => (1, 0),
        Forwarding::CutThrough { threshold } => (0, threshold),
    };
    for q in 0..queues {
        let reg = mtl_queue_reg(q, MTL_Q_TQOMR);
        write_field(acc, reg, MTL_Q_TQOMR_TSF.0, MTL_Q_TQOMR_TSF.1, sf);
        write_field(acc, reg, MTL_Q_TQOMR_TTC.0, MTL_Q_TQOMR_TTC.1, ttc);
    }
}

/// The same for the receive queues, with the other register's other
/// positions.
pub fn config_rx_forwarding(acc: &dyn MmioAccess, queues: u32, mode: Forwarding) {
    let (sf, rtc) = match mode {
        Forwarding::StoreAndForward => (1, 0),
        Forwarding::CutThrough { threshold } => (0, threshold),
    };
    for q in 0..queues {
        let reg = mtl_queue_reg(q, MTL_Q_RQOMR);
        write_field(acc, reg, MTL_Q_RQOMR_RSF.0, MTL_Q_RQOMR_RSF.1, sf);
        write_field(acc, reg, MTL_Q_RQOMR_RTC.0, MTL_Q_RQOMR_RTC.1, rtc);
    }
}

/// Pick the two scheduling algorithms. The first half of
/// `xgbe_config_mtl_mode`.
pub fn config_mtl_scheduling(acc: &dyn MmioAccess) {
    write_field(
        acc,
        MTL_OMR,
        MTL_OMR_ETSALG.0,
        MTL_OMR_ETSALG.1,
        MTL_ETSALG_WRR,
    );
    write_field(acc, MTL_OMR, MTL_OMR_RAA.0, MTL_OMR_RAA.1, MTL_RAA_SP);
}

/// Give every traffic class the same weight. The second half.
///
/// Equal weights, so the classes share the link evenly, which is the only
/// honest default for a driver that has no idea what the classes are for.
pub fn config_tc_weights(acc: &dyn MmioAccess, tc_count: u32, queues: u32) -> DeviceResult<()> {
    if tc_count == 0 || tc_count > queues {
        // A class above the queue count would write into another queue's
        // block, since the two share one.
        return Err(DeviceError::InvalidParam);
    }
    for tc in 0..tc_count {
        write_field(
            acc,
            mtl_tc_reg(tc, MTL_TC_ETSCR),
            MTL_TC_ETSCR_TSA.0,
            MTL_TC_ETSCR_TSA.1,
            MTL_TSA_ETS,
        );
        write_field(
            acc,
            mtl_tc_reg(tc, MTL_TC_QWR),
            MTL_TC_QWR_QW.0,
            MTL_TC_QWR_QW.1,
            1,
        );
    }
    Ok(())
}

// ——— Deciding which frames to keep ———

/// `MAC_PFR`: the packet filter register.
pub const MAC_PFR: usize = 0x0008;
/// `MAC_PFR_PR`, index 0: promiscuous --- keep everything.
pub const MAC_PFR_PR: (u32, u32) = (0, 1);
/// `MAC_PFR_HUC`, index 1: match unicast addresses through the hash table
/// rather than only the exact-match slots.
pub const MAC_PFR_HUC: (u32, u32) = (1, 1);
/// `MAC_PFR_HMC`, index 2: the same for multicast.
pub const MAC_PFR_HMC: (u32, u32) = (2, 1);
/// `MAC_PFR_PM`, index 4: keep every multicast frame.
pub const MAC_PFR_PM: (u32, u32) = (4, 1);
/// `MAC_PFR_HPF`, index 10: let a frame in on either the hash OR an
/// exact-match slot, instead of the hash alone.
pub const MAC_PFR_HPF: (u32, u32) = (10, 1);

/// `MAC_HTR0`: the first word of the hash filter table.
pub const MAC_HTR0: usize = 0x0010;
/// `MAC_HTR_INC`: and the stride between its words.
pub const MAC_HTR_INC: usize = 4;

/// How far to shift a reversed CRC to get a hash index, for a table of
/// `hash_table_size` bits.
///
/// Linux writes this as `26 - (hash_table_size >> 7)`, which is a log2 in
/// disguise: 64 bits gives 26 (six index bits), 128 gives 25, 256 gives 24.
/// Hard-coding the 24 that a 256-bit table wants works perfectly on the chip
/// it was written against and silently folds three quarters of the table
/// away on a smaller one.
#[inline]
pub const fn hash_table_shift(hash_table_size: u32) -> u32 {
    26 - (hash_table_size >> 7)
}

/// How many 32-bit words the table occupies.
#[inline]
pub const fn hash_table_words(hash_table_size: u32) -> u32 {
    hash_table_size / 32
}

/// The hash index of an address, from the Ethernet CRC of its six bytes.
///
/// `crc_le` is `crc32_le(~0, addr, 6)` --- the plain Ethernet CRC, nothing
/// done to it yet. The three steps here are each the kind that looks
/// optional and is not:
///
/// 1. **Complement it.** `crc32_le` leaves the residue uncomplemented.
/// 2. **Reverse the bits**, all thirty-two of them. The hardware indexes
///    from the other end of the word.
/// 3. **Shift right** by [`hash_table_shift`], keeping the top bits of the
///    reversed value --- which are the BOTTOM bits of the one that came in.
///
/// Skip the reversal and the index is still a number in range, still
/// deterministic, and still points at the wrong bucket, so the symptom is an
/// interface that drops exactly the multicast groups it was asked to join.
///
/// A CRC-32 crate hands back a value with step 1 ALREADY DONE, so code that
/// reaches for one and keeps Linux's `~` complements twice and lands
/// somewhere else entirely. [`crate::net::ether_hash::hash_bucket_of`] takes
/// an address and does the right thing; this takes Linux's intermediate
/// value and exists so the two can be checked against each other.
#[inline]
pub const fn hash_index(crc_le: u32, hash_table_size: u32) -> u32 {
    (!crc_le).reverse_bits() >> hash_table_shift(hash_table_size)
}

/// Which word of the table an index falls in, and which bit of it.
#[inline]
pub const fn hash_bucket(index: u32) -> (u32, u32) {
    (index >> 5, index & 0x1f)
}

/// Set an address's bit in a hash table being built.
///
/// The work is [`crate::net::ether_hash`]'s, because the GENET and the macb
/// compute the same bucket from the same CRC and there is no version of this
/// that is the XGMAC's own. What stays here is [`hash_index`], which says
/// the same thing in the terms Linux's `xgbe-dev.c` uses, and a test that
/// the two agree.
#[inline]
pub fn hash_table_add(table: &mut [u32], addr: &[u8], hash_table_size: u32) {
    crate::net::ether_hash::hash_table_set(table, addr, hash_table_size);
}

/// Write a built hash table out. `xgbe_set_mac_hash_table`'s last loop.
pub fn config_hash_table(acc: &dyn MmioAccess, table: &[u32], hash_table_size: u32) {
    let words = hash_table_words(hash_table_size) as usize;
    for i in 0..words {
        acc.write32(
            MAC_HTR0 + i * MAC_HTR_INC,
            table.get(i).copied().unwrap_or(0),
        );
    }
}

/// An exact-match address slot, one of the `addn_mac` the chip reports.
/// Slot zero is the station address of batch seven and is not one of these.
#[inline]
pub const fn mac_addr_slot(slot: u32) -> (usize, usize) {
    let base = MAC_MACA1HR + (slot as usize) * 8;
    (base, base + 4)
}

/// Put an address in an exact-match slot, or clear the slot with `None`.
///
/// The enable bit lives in the HIGH half, so the high half is written LAST:
/// writing it first arms a slot whose low four bytes are still the previous
/// address's.
pub fn config_mac_addr_slot(acc: &dyn MmioAccess, slot: u32, mac: Option<[u8; 6]>) {
    let (hi_reg, lo_reg) = mac_addr_slot(slot);
    match mac {
        Some(mac) => {
            let (hi, lo) = mac_to_regs(mac);
            acc.write32(lo_reg, lo);
            acc.write32(hi_reg, hi | (1 << MAC_MACA1HR_AE.0));
        }
        None => {
            // Clearing is the other way round: disarm first, then wipe.
            acc.write32(hi_reg, 0);
            acc.write32(lo_reg, 0);
        }
    }
}

/// What the interface should accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxFilter {
    /// Keep every frame, whoever it is for.
    pub promiscuous: bool,
    /// Keep every multicast frame.
    pub all_multicast: bool,
    /// Consult the hash table for multicast, as well as the exact slots.
    pub hash_multicast: bool,
    /// ... and for unicast.
    pub hash_unicast: bool,
}

/// Program the filter.
///
/// Note what promiscuous does NOT do: the hardware goes on filtering VLANs
/// in promiscuous mode, so "accept everything" does not accept everything
/// and Linux turns VLAN filtering off separately. This driver has no VLAN
/// filtering to turn off yet, which is the only reason that is not a bug
/// here --- and the reason to write it down before it is.
///
/// `HPF` is set whenever a hash is in use, so a frame gets in on the hash OR
/// an exact slot. Without it the exact slots stop mattering the moment the
/// hash is on, and the station address itself would be filtered out by a
/// hash it does not happen to land in.
pub fn config_rx_filter(acc: &dyn MmioAccess, filter: &RxFilter) {
    let hash = filter.hash_multicast || filter.hash_unicast;
    let mut pfr = 0u32;
    pfr = set_bits(pfr, MAC_PFR_PR.0, MAC_PFR_PR.1, filter.promiscuous as u32);
    pfr = set_bits(pfr, MAC_PFR_PM.0, MAC_PFR_PM.1, filter.all_multicast as u32);
    pfr = set_bits(
        pfr,
        MAC_PFR_HMC.0,
        MAC_PFR_HMC.1,
        filter.hash_multicast as u32,
    );
    pfr = set_bits(
        pfr,
        MAC_PFR_HUC.0,
        MAC_PFR_HUC.1,
        filter.hash_unicast as u32,
    );
    pfr = set_bits(pfr, MAC_PFR_HPF.0, MAC_PFR_HPF.1, hash as u32);
    acc.write32(MAC_PFR, pfr);
}

// ——— Not interrupting on every frame ———

/// `XGBE_V2_DMA_CLOCK_FREQ`: the DMA block's clock on the PCI parts, 500 MHz.
/// The watchdog below counts THIS clock, not the link's and not a wall
/// clock.
pub const DMA_CLOCK_HZ: u32 = 500_000_000;
/// `XGBE_V2_PTP_CLOCK_FREQ`: 125 MHz. Named only so that nobody reaches for
/// it here: it times timestamps, not the receive watchdog, and the two are
/// a factor of four apart.
pub const PTP_CLOCK_HZ: u32 = 125_000_000;

/// How many clock cycles one unit of the receive watchdog is worth.
pub const RIWT_CYCLES_PER_UNIT: u32 = 256;
/// `XGMAC_MAX_DMA_RIWT`: the widest value the eight-bit field holds.
pub const MAX_DMA_RIWT: u32 = 0xff;
/// `DMA_CH_RIWT_RWT`, index 0, width 8.
pub const DMA_CH_RIWT_RWT: (u32, u32) = (0, 8);

/// Microseconds to watchdog units. `xgbe_usec_to_riwt`.
///
/// Three things about this arithmetic:
///
/// - It counts the DMA clock, so the SAME microsecond figure becomes a
///   different register value on a part with a different clock. A number
///   copied out of another driver's defaults means a different delay here.
/// - Linux divides the rate down to MHz FIRST, so a clock that is not a
///   whole number of megahertz loses its fraction before anything else
///   happens. Kept as it is: matching the kernel matters more here than the
///   last few cycles, and the result is a coalescing delay, not a deadline.
/// - It is clamped. Linux clamps where the value arrives from ethtool
///   instead, so the conversion itself can hand back something that does not
///   fit the eight-bit field; a value that wrapped there would ask for a
///   *shorter* delay than requested, which is the wrong direction to fail.
#[inline]
pub const fn usec_to_riwt(usec: u32, clock_hz: u32) -> u32 {
    let mhz = clock_hz / 1_000_000;
    let riwt = usec.saturating_mul(mhz) / RIWT_CYCLES_PER_UNIT;
    if riwt > MAX_DMA_RIWT {
        MAX_DMA_RIWT
    } else {
        riwt
    }
}

/// And back. `xgbe_riwt_to_usec`.
///
/// Not the inverse of [`usec_to_riwt`]: both truncate, so a round trip
/// lands at or below where it started. Anything that compares a configured
/// delay against a read-back one has to expect that, rather than treating a
/// difference as the hardware having refused the setting.
#[inline]
pub const fn riwt_to_usec(riwt: u32, clock_hz: u32) -> u32 {
    let mhz = clock_hz / 1_000_000;
    if mhz == 0 {
        return 0;
    }
    riwt * RIWT_CYCLES_PER_UNIT / mhz
}

/// Set the receive watchdog on the channels that have a receive ring.
///
/// Linux's loop `break`s at the first channel without one rather than
/// skipping it, which only works because the receive channels come first.
/// Taking a count instead says that out loud.
pub fn config_rx_coalesce(acc: &dyn MmioAccess, rx_channels: u32, riwt: u32) {
    for ch in 0..rx_channels {
        write_field(
            acc,
            dma_channel_reg(ch, DMA_CH_RIWT),
            DMA_CH_RIWT_RWT.0,
            DMA_CH_RIWT_RWT.1,
            riwt.min(MAX_DMA_RIWT),
        );
    }
}

/// The transmit side's coalescing, which is no register at all.
///
/// `xgbe_config_tx_coalesce` in Linux is a function that returns success
/// without writing anything, and the symmetry with the receive side is a
/// trap: there is no transmit watchdog. What limits transmit interrupts is
/// the `IC` bit the driver decides to set on a descriptor, back in
/// [`build_tx_frame`] --- a per-frame choice, not a timer the hardware
/// keeps. This function exists so the pair reads honestly at the call site
/// rather than looking like an omission.
#[inline]
pub fn config_tx_coalesce(_acc: &dyn MmioAccess) {}

// ——— How long a frame may be, and how big a buffer it lands in ———

/// `ETH_HLEN`: the Ethernet header Linux counts into a receive buffer.
pub const ETH_HLEN: u32 = 14;
/// `ETH_FCS_LEN`: the trailing CRC, which the chip delivers.
pub const ETH_FCS_LEN: u32 = 4;
/// `VLAN_HLEN`: room for the one VLAN tag Linux always leaves space for,
/// whether or not the interface is on a VLAN.
pub const VLAN_HLEN: u32 = 4;
/// `ETH_FRAME_LEN`: header plus a standard payload, **without** the FCS.
pub const ETH_FRAME_LEN: u32 = 1514;

/// `XGBE_RX_MIN_BUF_SIZE`, `xgbe.h`: `ETH_FRAME_LEN + ETH_FCS_LEN + VLAN_HLEN`.
pub const RX_MIN_BUF_SIZE: u32 = ETH_FRAME_LEN + ETH_FCS_LEN + VLAN_HLEN;
/// `XGBE_RX_BUF_ALIGN`, `xgbe.h`.
pub const RX_BUF_ALIGN: u32 = 64;
/// Linux clamps the receive buffer at `PAGE_SIZE`, because it allocates one
/// page per buffer. 4 KiB here, which is the page size on every architecture
/// this kernel builds for.
///
/// This clamp is the whole reason a jumbo frame arrives **split**: a 9000-byte
/// MTU does not get a 9022-byte buffer, it gets 4096, and the frame comes back
/// over three descriptors with `FD` on the first and `LD` on the last. See
/// [`read_rx_desc`].
pub const RX_MAX_BUF_SIZE: u32 = 4096;

/// `XGMAC_STD_PACKET_MTU`.
pub const STD_PACKET_MTU: u32 = 1500;
/// `XGMAC_JUMBO_PACKET_MTU`: above this the giant-packet path is used.
pub const JUMBO_PACKET_MTU: u32 = 9000;
/// `XGMAC_MAX_JUMBO_PACKET`: 9000 plus header, FCS and a VLAN tag.
pub const MAX_JUMBO_PACKET: u32 = 9018;
/// `XGMAC_GIANT_PACKET_MTU`, `xgbe.h` at master: the length the giant path
/// programs into `GPSL`. Not 16384: `GPSL` is 14 bits, so 16383 is the
/// ceiling, and Linux stops 15 short of it.
pub const GIANT_PACKET_MTU: u32 = 16368;

/// `MAC_RCR_GPSLCE`, index 6, width 1: check the received length against
/// `GPSL` instead of against the fixed 1518/9018 limit.
pub const MAC_RCR_GPSLCE: (u32, u32) = (6, 1);
/// `MAC_RCR_WD`, index 7, width 1: **watchdog disable** on receive.
///
/// The name reads backwards. The receive watchdog is the timer that cuts a
/// frame off at about 2 KiB; a 1 here turns that policeman off, which is what
/// *allows* a giant frame, not what forbids one.
pub const MAC_RCR_WD: (u32, u32) = (7, 1);
/// `MAC_RCR_JE`, index 8, width 1: jumbo enable, i.e. raise the fixed length
/// limit from 1518 to 9018.
pub const MAC_RCR_JE: (u32, u32) = (8, 1);
/// `MAC_RCR_GPSL`, index 16, width 14: the giant packet size limit.
pub const MAC_RCR_GPSL: (u32, u32) = (16, 14);
/// `MAC_TCR_JD`, index 16, width 1: **jabber disable** on transmit. The
/// transmit-side twin of [`MAC_RCR_WD`], and it reads backwards for the same
/// reason.
pub const MAC_TCR_JD: (u32, u32) = (16, 1);

/// How big a receive buffer an MTU needs. `xgbe_calc_rx_buf_size`.
///
/// Three steps, in Linux's order, and each one changes the answer:
///
/// 1. the frame is the MTU plus header, FCS and a VLAN tag — 22 bytes, not 18,
///    because the tag is counted whether or not it is used;
/// 2. the result is clamped into `[RX_MIN_BUF_SIZE, RX_MAX_BUF_SIZE]`, so a
///    tiny MTU still gets 1522 and a jumbo MTU is cut to a page;
/// 3. and only then is it rounded **up** to [`RX_BUF_ALIGN`].
///
/// Rounding before clamping would be wrong at the top: 4096 is already
/// 64-aligned, so in this order the round-up can never push the result back
/// over the clamp. Doing it the other way round for a hypothetical unaligned
/// ceiling would.
#[inline]
pub const fn calc_rx_buf_size(mtu: u32) -> u32 {
    let wanted = mtu.saturating_add(ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN);
    let clamped = if wanted < RX_MIN_BUF_SIZE {
        RX_MIN_BUF_SIZE
    } else if wanted > RX_MAX_BUF_SIZE {
        RX_MAX_BUF_SIZE
    } else {
        wanted
    };
    (clamped + RX_BUF_ALIGN - 1) & !(RX_BUF_ALIGN - 1)
}

/// How many descriptors one frame of `mtu` can take up, given the buffer size
/// [`calc_rx_buf_size`] picked.
///
/// Not a Linux function: Linux discovers this by walking `FD`..`LD` as the
/// frames come back. It is here so the number is written down once, because
/// it is what decides whether a receive ring of `N` descriptors can hold
/// anything at all at a jumbo MTU.
#[inline]
pub const fn rx_buffers_for_mtu(mtu: u32) -> u32 {
    let buf = calc_rx_buf_size(mtu);
    let frame = mtu.saturating_add(ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN);
    frame.div_ceil(buf)
}

/// Write the receive buffer size into every receive channel.
/// `xgbe_config_rx_buffer_size`.
///
/// [`program_rx_ring`] already sets `RBSZ` when it hands a ring over, and
/// Linux does the same thing twice for the same reason: `xgbe_change_mtu`
/// recomputes the buffer size and restarts the device, and the restart path
/// goes through this function rather than through ring programming.
pub fn config_rx_buffer_size(
    acc: &dyn MmioAccess,
    rx_channels: u32,
    buffer_size: u32,
) -> DeviceResult<()> {
    if buffer_size == 0 || buffer_size >= (1 << DMA_CH_RCR_RBSZ.1) {
        return Err(DeviceError::InvalidParam);
    }
    for channel in 0..rx_channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_RCR),
            DMA_CH_RCR_RBSZ.0,
            DMA_CH_RCR_RBSZ.1,
            buffer_size,
        );
    }
    Ok(())
}

/// Set the length limits for an MTU. `xgbe_config_jumbo_enable`.
///
/// Three regimes, and the middle one is the surprise:
///
/// * **MTU above 9000**: the two length policemen are switched off
///   (`WD` and `JD` to 1), `GPSL` is loaded with [`GIANT_PACKET_MTU`] and
///   `GPSLCE` turns the comparison against it on. `JE` is **not written at
///   all** on this path — the fixed 9018 limit is irrelevant once `GPSLCE`
///   is checking against `GPSL` instead.
/// * **MTU between 1501 and 9000**: `GPSLCE`, `WD` and `JD` go back to 0 and
///   `JE` goes to 1, raising the fixed limit to 9018.
/// * **MTU of 1500 or less**: the same four writes with `JE` at 0 as well.
///
/// So the normal path always writes all four fields, and the giant path
/// writes four different ones. Reading it as "set JE for jumbo" and stopping
/// there gets frames above 9018 dropped by a watchdog that was never turned
/// off, with no counter to say so.
pub fn config_jumbo_enable(acc: &dyn MmioAccess, mtu: u32) {
    if mtu > JUMBO_PACKET_MTU {
        write_field(
            acc,
            MAC_RCR,
            MAC_RCR_GPSL.0,
            MAC_RCR_GPSL.1,
            GIANT_PACKET_MTU,
        );
        write_field(acc, MAC_RCR, MAC_RCR_WD.0, MAC_RCR_WD.1, 1);
        write_field(acc, MAC_TCR, MAC_TCR_JD.0, MAC_TCR_JD.1, 1);
        write_field(acc, MAC_RCR, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1, 1);
    } else {
        let jumbo = if mtu > STD_PACKET_MTU { 1 } else { 0 };
        write_field(acc, MAC_RCR, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1, 0);
        write_field(acc, MAC_RCR, MAC_RCR_WD.0, MAC_RCR_WD.1, 0);
        write_field(acc, MAC_TCR, MAC_TCR_JD.0, MAC_TCR_JD.1, 0);
        write_field(acc, MAC_RCR, MAC_RCR_JE.0, MAC_RCR_JE.1, jumbo);
    }
}

// ——— The offloads, and the bit each one actually hangs on ———

/// `DMA_CH_TCR_TSE`, index 12, width 1: TCP segmentation offload, per
/// transmit channel.
///
/// This is only half the switch. The other half is `TSE` in a descriptor's
/// `desc3` (bit 18, see [`TX_DESC3_TSE`]), which is what says *this frame* is
/// a segmentation request. Linux turns the channel bit on for every transmit
/// channel unconditionally and then never sets the descriptor bit unless the
/// stack handed it a TSO skb.
///
/// So the channel bit on its own changes nothing, which is why it is safe to
/// set it before there is any TSO path: a frame built by [`build_tx_frame`]
/// leaves `desc3` bit 18 at zero, and [`tx_desc3_frame_len`] cannot reach it.
pub const DMA_CH_TCR_TSE: (u32, u32) = (12, 1);

/// `DMA_CH_CR_SPH`, index 24, width 1: split the header of a received frame
/// into its own buffer, per receive channel.
pub const DMA_CH_CR_SPH: (u32, u32) = (24, 1);

/// `MAC_RCR_HDSMS`, index 12, width 3: how big that header buffer is.
///
/// An **index**, not a byte count. Linux writes `XGBE_SPH_HDSMS_SIZE`, which
/// is 2, and the comment next to it says "keep in sync with SKB_ALLOC_SIZE" —
/// i.e. the number is tied to how Linux allocates its own buffers, not to
/// anything the chip requires. Whoever gives this driver a receive path has
/// to pick it against *our* buffer, and copying the 2 across because Linux
/// has a 2 is exactly the mistake the comment is warning about.
pub const MAC_RCR_HDSMS: (u32, u32) = (12, 3);

/// `XGBE_SPH_HDSMS_SIZE`: the value Linux writes. Named so the number has a
/// provenance, not so it gets reused.
pub const SPH_HDSMS_SIZE: u32 = 2;

/// `MAC_RCR_IPC`, index 9, width 1: check the checksums of received frames.
///
/// The gate in front of everything [`rx_frame_verdict`] decides. With `IPC`
/// at 0 the chip never reports a checksum error, so every frame comes back
/// `Clean` — not "checksum not checked", **clean**. A receiver that trusts
/// the verdict without having set this bit is passing unverified checksums
/// up as verified ones, and nothing in the descriptor says so.
pub const MAC_RCR_IPC: (u32, u32) = (9, 1);

/// Turn the transmit channel's TSO switch on. `xgbe_config_tso_mode`.
pub fn config_tso_mode(acc: &dyn MmioAccess, tx_channels: u32) {
    for channel in 0..tx_channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_TCR),
            DMA_CH_TCR_TSE.0,
            DMA_CH_TCR_TSE.1,
            1,
        );
    }
}

/// Turn header splitting on for every receive channel, and set the header
/// size once in the MAC. `xgbe_config_sph_mode`.
///
/// Note the shape: the per-channel bit goes out `rx_channels` times and the
/// size goes out **once**, because it lives in the MAC and not in the
/// channel. One size for every channel, whatever their buffers look like.
pub fn config_sph_mode(acc: &dyn MmioAccess, rx_channels: u32, hdsms: u32) -> DeviceResult<()> {
    if hdsms >= (1 << MAC_RCR_HDSMS.1) {
        return Err(DeviceError::InvalidParam);
    }
    for channel in 0..rx_channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_CR),
            DMA_CH_CR_SPH.0,
            DMA_CH_CR_SPH.1,
            1,
        );
    }
    write_field(acc, MAC_RCR, MAC_RCR_HDSMS.0, MAC_RCR_HDSMS.1, hdsms);
    Ok(())
}

/// Switch receive checksum checking on or off. `xgbe_enable_rx_csum` and
/// `xgbe_disable_rx_csum`, which are one field write each and differ only in
/// the value.
pub fn config_rx_csum(acc: &dyn MmioAccess, on: bool) {
    write_field(
        acc,
        MAC_RCR,
        MAC_RCR_IPC.0,
        MAC_RCR_IPC.1,
        if on { 1 } else { 0 },
    );
}

// ——— Telling the MAC how fast the line is ———

/// `MAC_TCR_SS`, index 29, width 3: the speed selection, at the very top of
/// the transmit control register.
pub const MAC_TCR_SS: (u32, u32) = (29, 3);

/// `XGBE_MAC_SS_10G`: 10 Gbps, XGMII mode. **Zero**, so a MAC that has just
/// come out of reset is already claiming 10G.
pub const MAC_SS_10G: u32 = 0x00;
/// `XGBE_MAC_SS_2_5G_GMII`: 2.5 Gbps over GMII, the older platforms.
pub const MAC_SS_2_5G_GMII: u32 = 0x02;
/// `XGBE_MAC_SS_2_5G_XGMII`: 2.5 Gbps over XGMII, the P100a.
pub const MAC_SS_2_5G_XGMII: u32 = 0x06;
/// `XGBE_MAC_SS_1G`: 1 Gbps.
pub const MAC_SS_1G: u32 = 0x03;
/// `XGBE_MAC_SS_100M`: 100 Mbps — **the same encoding as
/// [`MAC_SS_2_5G_GMII`]**, because the field selects an interface mode and a
/// divider, not a speed. The two are told apart by which PHY mode the port is
/// in, which this register does not say.
///
/// Linux's `xgbe_set_speed` has no `SPEED_100` case at all, so this value is
/// defined and never written. It is named here for the same reason: so that
/// the collision is on the record rather than discovered by someone adding
/// the missing case.
pub const MAC_SS_100M: u32 = 0x02;
/// `XGBE_MAC_SS_10M`: 10 Mbps. 0x07, the largest value, at the bottom of the
/// speed range — the encoding is in no order at all.
pub const MAC_SS_10M: u32 = 0x07;

/// `XGBE_MAC_VER_33`: the P100a core revision, the one that drives 2.5 Gbps
/// over XGMII instead of GMII.
pub const MAC_VER_33: u8 = 0x33;

/// The link speeds this driver can ask the MAC for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSpeed {
    /// 10 Mbps.
    Speed10M,
    /// 1 Gbps.
    Speed1G,
    /// 2.5 Gbps. Which encoding this becomes depends on the core revision.
    Speed2_5G,
    /// 10 Gbps.
    Speed10G,
}

/// The `SS` encoding for a speed on a core of revision `snps_ver`.
/// `xgbe_set_speed`.
///
/// The version only matters for 2.5 Gbps, and it matters there because the
/// two 2.5G encodings are different interface modes rather than different
/// speeds: `0x06` on a P100a core, `0x02` on everything older. Get it wrong
/// and the MAC drives the wrong interface at the right nominal rate, which is
/// not something a link-up check would catch.
#[inline]
pub const fn speed_select(speed: LinkSpeed, snps_ver: u8) -> u32 {
    match speed {
        LinkSpeed::Speed10M => MAC_SS_10M,
        LinkSpeed::Speed1G => MAC_SS_1G,
        LinkSpeed::Speed2_5G => {
            if snps_ver == MAC_VER_33 {
                MAC_SS_2_5G_XGMII
            } else {
                MAC_SS_2_5G_GMII
            }
        }
        LinkSpeed::Speed10G => MAC_SS_10G,
    }
}

/// Set the MAC's speed, writing only if it is not already set.
/// `xgbe_set_speed`.
///
/// The read-before-write is Linux's, and it is not an optimisation: `MAC_TCR`
/// also holds `TE`, so every write to this register passes through the
/// transmitter enable. Skipping the write when the value already matches is
/// how a speed that has not changed costs the running transmitter nothing.
///
/// Returns whether it wrote.
pub fn set_mac_speed(acc: &dyn MmioAccess, speed: LinkSpeed, snps_ver: u8) -> bool {
    let want = speed_select(speed, snps_ver);
    let current = get_bits(acc.read32(MAC_TCR), MAC_TCR_SS.0, MAC_TCR_SS.1);
    if current == want {
        return false;
    }
    write_field(acc, MAC_TCR, MAC_TCR_SS.0, MAC_TCR_SS.1, want);
    true
}

// ——— How much the DMA fetches at a time, and what it tells the bus ———

/// `DMA_AXIARCR`: the AXI read cache attributes. Written **whole**, not as
/// fields: Linux uses `XGMAC_IOWRITE`, not `XGMAC_IOWRITE_BITS`.
pub const DMA_AXIARCR: usize = 0x3010;
/// `DMA_AXIAWCR`: the AXI write cache attributes. Written whole.
pub const DMA_AXIAWCR: usize = 0x3018;
/// `DMA_AXIAWARCR`: the attributes for writes that follow a read.
pub const DMA_AXIAWARCR: usize = 0x301c;

/// `XGBE_DMA_PCI_ARCR`: the read attributes for a PCI port — this device.
///
/// Three different sets exist in `xgbe.h` (`PCI`, `SYS` and `OS`) and they do
/// not resemble each other: the system one is `0x00303030` against this
/// `0x000f0f0f`. They are not interchangeable defaults, they are three
/// different buses, and there is nothing in the chip that would complain
/// about the wrong one.
pub const DMA_PCI_ARCR: u32 = 0x000f_0f0f;
/// `XGBE_DMA_PCI_AWCR`.
pub const DMA_PCI_AWCR: u32 = 0x0f0f_0f0f;
/// `XGBE_DMA_PCI_AWARCR`.
pub const DMA_PCI_AWARCR: u32 = 0x0000_0f0f;

/// `DMA_CH_CR_PBLX8`, index 16, width 1: multiply the programmed burst length
/// by eight.
pub const DMA_CH_CR_PBLX8: (u32, u32) = (16, 1);
/// `DMA_CH_TCR_PBL`, index 16, width **6**: the transmit burst length.
pub const DMA_CH_TCR_PBL: (u32, u32) = (16, 6);
/// `DMA_CH_RCR_PBL`, index 16, width 6: the receive burst length.
pub const DMA_CH_RCR_PBL: (u32, u32) = (16, 6);
/// `DMA_CH_TCR_OSP`, index 4, width 1: operate on the second packet — let the
/// engine start fetching the next frame before the current one is done.
pub const DMA_CH_TCR_OSP: (u32, u32) = (4, 1);

/// The burst lengths `DMA_PBL_*` names. Only these eight are legal.
pub const DMA_PBL_VALUES: [u32; 9] = [1, 2, 4, 8, 16, 32, 64, 128, 256];

/// How a burst length is encoded: the value that goes in the 6-bit `PBL`
/// field, and whether the `PBLX8` multiplier is on. `xgbe_config_pbl_val`.
///
/// Above 32 the burst is **not** written as a bigger number — it cannot be.
/// The field is six bits, so 63 is the ceiling, and `DMA_PBL_64`,
/// `DMA_PBL_128` and `DMA_PBL_256` would all be truncated: 64 becomes 0, a
/// burst length of nothing. Linux turns `PBLX8` on and shifts the value right
/// by three instead, so 256 goes out as 32 with the multiplier set.
///
/// Returns `None` for a burst that is not one of [`DMA_PBL_VALUES`], rather
/// than shifting an arbitrary number down and writing whatever falls out.
#[inline]
pub fn pbl_encoding(pbl: u32) -> Option<(u32, u32)> {
    let mut legal = false;
    let mut i = 0;
    while i < DMA_PBL_VALUES.len() {
        if DMA_PBL_VALUES[i] == pbl {
            legal = true;
        }
        i += 1;
    }
    if !legal {
        return None;
    }
    if pbl > 32 {
        Some((pbl >> 3, 1))
    } else {
        Some((pbl, 0))
    }
}

/// Write the burst length into every channel. `xgbe_config_pbl_val`.
///
/// One setting, three registers: the multiplier lives in the channel's
/// control register and the length itself in the transmit and receive control
/// registers. And the shapes differ — the multiplier goes to **every**
/// channel, while each length goes only to the channels that have that
/// direction's ring.
pub fn config_pbl_val(
    acc: &dyn MmioAccess,
    tx_channels: u32,
    rx_channels: u32,
    pbl: u32,
) -> DeviceResult<()> {
    let (value, x8) = pbl_encoding(pbl).ok_or(DeviceError::InvalidParam)?;
    let channels = if tx_channels > rx_channels {
        tx_channels
    } else {
        rx_channels
    };
    for channel in 0..channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_CR),
            DMA_CH_CR_PBLX8.0,
            DMA_CH_CR_PBLX8.1,
            x8,
        );
        if channel < tx_channels {
            write_field(
                acc,
                dma_channel_reg(channel, DMA_CH_TCR),
                DMA_CH_TCR_PBL.0,
                DMA_CH_TCR_PBL.1,
                value,
            );
        }
        if channel < rx_channels {
            write_field(
                acc,
                dma_channel_reg(channel, DMA_CH_RCR),
                DMA_CH_RCR_PBL.0,
                DMA_CH_RCR_PBL.1,
                value,
            );
        }
    }
    Ok(())
}

/// Set "operate on second packet" on every transmit channel.
/// `xgbe_config_osp_mode`.
pub fn config_osp_mode(acc: &dyn MmioAccess, tx_channels: u32, on: bool) {
    for channel in 0..tx_channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_TCR),
            DMA_CH_TCR_OSP.0,
            DMA_CH_TCR_OSP.1,
            if on { 1 } else { 0 },
        );
    }
}

/// Write the AXI cache attributes. `xgbe_config_dma_cache`.
///
/// Three whole-register writes, and the third is **conditional on the value
/// being non-zero**. That is Linux's own guard and not a tidy-up: a platform
/// that has no `awarcr` leaves the register at whatever reset put there
/// rather than clearing it, so a zero here means "do not touch", not "set to
/// zero". Passing 0 and expecting the register cleared is the mistake.
pub fn config_dma_cache(acc: &dyn MmioAccess, arcr: u32, awcr: u32, awarcr: u32) {
    acc.write32(DMA_AXIARCR, arcr);
    acc.write32(DMA_AXIAWCR, awcr);
    if awarcr != 0 {
        acc.write32(DMA_AXIAWARCR, awarcr);
    }
}

// ——— Spreading received frames over the channels ———

/// `MAC_RSSCR`: the RSS control register.
pub const MAC_RSSCR: usize = 0x0c80;
/// `MAC_RSSAR`: the address register of the indirect window.
pub const MAC_RSSAR: usize = 0x0c88;
/// `MAC_RSSDR`: the data register of the indirect window.
pub const MAC_RSSDR: usize = 0x0c8c;

/// `MAC_RSSCR_RSSE`, index 0, width 1: RSS enable.
pub const MAC_RSSCR_RSSE: (u32, u32) = (0, 1);
/// `MAC_RSSCR_IP2TE`, index 1, width 1: hash over IPv4/IPv6 addresses.
pub const MAC_RSSCR_IP2TE: (u32, u32) = (1, 1);
/// `MAC_RSSCR_TCP4TE`, index 2, width 1: hash over the TCP ports too.
pub const MAC_RSSCR_TCP4TE: (u32, u32) = (2, 1);
/// `MAC_RSSCR_UDP4TE`, index 3, width 1: hash over the UDP ports too.
pub const MAC_RSSCR_UDP4TE: (u32, u32) = (3, 1);

/// `MAC_RSSAR_OB`, index 0, width 1: operation busy. Set to start, and the
/// chip clears it when done.
pub const MAC_RSSAR_OB: (u32, u32) = (0, 1);
/// `MAC_RSSAR_CT`, index 1, width 1: 0 writes, 1 reads. Linux writes the 0
/// explicitly every time rather than relying on what was there.
pub const MAC_RSSAR_CT: (u32, u32) = (1, 1);
/// `MAC_RSSAR_ADDRT`, index 2, width 1: which table the index refers to.
pub const MAC_RSSAR_ADDRT: (u32, u32) = (2, 1);
/// `MAC_RSSAR_RSSIA`, index 8, width 8: the index within that table.
pub const MAC_RSSAR_RSSIA: (u32, u32) = (8, 8);

/// `MAC_RSSDR_DMCH`, index 0, width 4: for a lookup-table write, the only
/// part of the data register that means anything — the DMA channel the
/// bucket goes to. For a hash-key write the whole 32 bits are data.
pub const MAC_RSSDR_DMCH: (u32, u32) = (0, 4);

/// `XGBE_RSS_LOOKUP_TABLE_TYPE`.
pub const RSS_LOOKUP_TABLE_TYPE: u32 = 0;
/// `XGBE_RSS_HASH_KEY_TYPE`.
pub const RSS_HASH_KEY_TYPE: u32 = 1;
/// `XGBE_RSS_HASH_KEY_SIZE`: 40 bytes, so ten 32-bit registers.
pub const RSS_HASH_KEY_SIZE: usize = 40;
/// `XGBE_RSS_MAX_TABLE_SIZE`: exactly as many buckets as [`MAC_RSSAR_RSSIA`]
/// can address.
pub const RSS_MAX_TABLE_SIZE: usize = 256;

/// One write through the indirect window. `xgbe_write_rss_reg`.
///
/// The order matters and is not obvious: the **data** register goes out
/// first, then the address fields, and `OB` **last** — setting `OB` is what
/// hands the whole thing to the chip, so anything written after it is a race.
///
/// The pre-check is Linux's too: if `OB` is already set the call fails
/// immediately instead of waiting, because a busy window means someone else
/// is mid-write and the data register is not ours to touch.
pub fn write_rss_reg(
    acc: &dyn MmioAccess,
    table: u32,
    index: u32,
    value: u32,
    poll: &mut dyn FnMut() -> bool,
) -> DeviceResult<()> {
    if index >= (1 << MAC_RSSAR_RSSIA.1) {
        return Err(DeviceError::InvalidParam);
    }
    if get_bits(acc.read32(MAC_RSSAR), MAC_RSSAR_OB.0, MAC_RSSAR_OB.1) != 0 {
        return Err(DeviceError::NotReady);
    }

    acc.write32(MAC_RSSDR, value);

    write_field(acc, MAC_RSSAR, MAC_RSSAR_RSSIA.0, MAC_RSSAR_RSSIA.1, index);
    write_field(acc, MAC_RSSAR, MAC_RSSAR_ADDRT.0, MAC_RSSAR_ADDRT.1, table);
    write_field(acc, MAC_RSSAR, MAC_RSSAR_CT.0, MAC_RSSAR_CT.1, 0);
    write_field(acc, MAC_RSSAR, MAC_RSSAR_OB.0, MAC_RSSAR_OB.1, 1);

    while poll() {
        if get_bits(acc.read32(MAC_RSSAR), MAC_RSSAR_OB.0, MAC_RSSAR_OB.1) == 0 {
            return Ok(());
        }
    }
    Err(DeviceError::NotReady)
}

/// Write the hash key. `xgbe_write_rss_hash_key`.
///
/// The index counts **down** while the key counts **up**: Linux's loop is
/// `while (key_regs--)` over `*key++`, so register 9 gets the first word of
/// the key and register 0 gets the last. Reading it as "write word *i* to
/// register *i*" gives a key the chip accepts, a hash that is stable, and a
/// spread that is not the one anybody configured — there is no error for it
/// anywhere.
pub fn write_rss_hash_key(
    acc: &dyn MmioAccess,
    key: &[u32],
    poll: &mut dyn FnMut() -> bool,
) -> DeviceResult<()> {
    if key.len() * 4 != RSS_HASH_KEY_SIZE {
        return Err(DeviceError::InvalidParam);
    }
    for (word, value) in key.iter().enumerate() {
        let index = (key.len() - 1 - word) as u32;
        write_rss_reg(acc, RSS_HASH_KEY_TYPE, index, *value, poll)?;
    }
    Ok(())
}

/// Write the bucket-to-channel table. `xgbe_write_rss_lookup_table`.
///
/// Here the index counts up with the data, unlike the key. Only the bottom
/// four bits of each entry reach the chip ([`MAC_RSSDR_DMCH`]), so a channel
/// number above 15 would be truncated rather than refused — it is refused
/// here.
pub fn write_rss_lookup_table(
    acc: &dyn MmioAccess,
    table: &[u32],
    poll: &mut dyn FnMut() -> bool,
) -> DeviceResult<()> {
    if table.is_empty() || table.len() > RSS_MAX_TABLE_SIZE {
        return Err(DeviceError::InvalidParam);
    }
    for (index, channel) in table.iter().enumerate() {
        if *channel >= (1 << MAC_RSSDR_DMCH.1) {
            return Err(DeviceError::InvalidParam);
        }
        write_rss_reg(acc, RSS_LOOKUP_TABLE_TYPE, index as u32, *channel, poll)?;
    }
    Ok(())
}

/// What the hash covers. `pdata->rss_options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RssOptions {
    /// Hash over the IP addresses.
    pub ip: bool,
    /// Also hash over the TCP ports.
    pub tcp4: bool,
    /// Also hash over the UDP ports.
    pub udp4: bool,
}

impl RssOptions {
    /// The `MAC_RSSCR` value these options make, with `RSSE` still clear.
    #[inline]
    pub const fn to_reg(self) -> u32 {
        let mut value = 0;
        if self.ip {
            value |= 1 << MAC_RSSCR_IP2TE.0;
        }
        if self.tcp4 {
            value |= 1 << MAC_RSSCR_TCP4TE.0;
        }
        if self.udp4 {
            value |= 1 << MAC_RSSCR_UDP4TE.0;
        }
        value
    }
}

/// Turn RSS on: key, table, options, then the enable. `xgbe_enable_rss`.
///
/// The options go out as a **whole-register** write, which is also what
/// clears `RSSE`; the enable is then a separate field write on top. Folding
/// the two into one store would work, and would lose the moment where the
/// chip has the new options and is not yet hashing with them.
pub fn enable_rss(
    acc: &dyn MmioAccess,
    has_rss: bool,
    key: &[u32],
    table: &[u32],
    options: RssOptions,
    poll: &mut dyn FnMut() -> bool,
) -> DeviceResult<()> {
    if !has_rss {
        return Err(DeviceError::NotSupported);
    }
    write_rss_hash_key(acc, key, poll)?;
    write_rss_lookup_table(acc, table, poll)?;
    acc.write32(MAC_RSSCR, options.to_reg());
    write_field(acc, MAC_RSSCR, MAC_RSSCR_RSSE.0, MAC_RSSCR_RSSE.1, 1);
    Ok(())
}

/// Turn RSS off, leaving the key and the table where they are.
/// `xgbe_disable_rss`.
pub fn disable_rss(acc: &dyn MmioAccess, has_rss: bool) -> DeviceResult<()> {
    if !has_rss {
        return Err(DeviceError::NotSupported);
    }
    write_field(acc, MAC_RSSCR, MAC_RSSCR_RSSE.0, MAC_RSSCR_RSSE.1, 0);
    Ok(())
}

// ——— VLANs: stripping the tag, and the 16-bucket filter ———

/// `MAC_VLANTR`: the VLAN tag register, which holds both the filter's own
/// tag and every switch that decides how tags are treated.
pub const MAC_VLANTR: usize = 0x0050;
/// `MAC_VLANHTR`: the VLAN hash table. One register, sixteen buckets.
pub const MAC_VLANHTR: usize = 0x0058;
/// `MAC_VLANIR`: the VLAN insertion register, for the transmit side.
pub const MAC_VLANIR: usize = 0x0060;

/// `MAC_VLANTR_VL`, index 0, width 16: the tag the filter compares against.
///
/// It is also the reason VLAN 1 is special — see [`enable_rx_vlan_filtering`].
pub const MAC_VLANTR_VL: (u32, u32) = (0, 16);
/// `MAC_VLANTR_ETV`, index 16, width 1: compare only the low 12 bits of the
/// tag, i.e. the VLAN id without its priority and CFI bits.
pub const MAC_VLANTR_ETV: (u32, u32) = (16, 1);
/// `MAC_VLANTR_VTIM`, index 17, width 1: inverse matching — pass what does
/// **not** match. Linux clears it, and a stray 1 here turns the filter into
/// its own opposite without any other symptom.
pub const MAC_VLANTR_VTIM: (u32, u32) = (17, 1);
/// `MAC_VLANTR_ESVL`, index 18, width 1: treat an S-TAG (0x88a8) frame as a
/// VLAN frame. Cleared: only C-TAGs count.
pub const MAC_VLANTR_ESVL: (u32, u32) = (18, 1);
/// `MAC_VLANTR_ERSVLM`, index 19, width 1: also strip S-TAGs. Cleared for
/// the same reason.
pub const MAC_VLANTR_ERSVLM: (u32, u32) = (19, 1);
/// `MAC_VLANTR_DOVLTC`, index 20, width 1: **don't** check the VLAN type.
pub const MAC_VLANTR_DOVLTC: (u32, u32) = (20, 1);
/// `MAC_VLANTR_EVLS`, index 21, width **2**: when to strip the tag. Not a
/// flag: Linux writes `0x3`, "always strip".
pub const MAC_VLANTR_EVLS: (u32, u32) = (21, 2);
/// `MAC_VLANTR_EVLRXS`, index 24, width 1: put the stripped tag into the
/// receive descriptor instead of dropping it on the floor.
pub const MAC_VLANTR_EVLRXS: (u32, u32) = (24, 1);
/// `MAC_VLANTR_VTHM`, index 25, width 1: filter through the hash table
/// rather than against the single tag in `VL`.
pub const MAC_VLANTR_VTHM: (u32, u32) = (25, 1);

/// `MAC_VLANHTR_VLHT`, index 0, width 16: the whole hash table, sixteen
/// buckets in one register.
pub const MAC_VLANHTR_VLHT: (u32, u32) = (0, 16);

/// `MAC_VLANIR_CSVL`, index 19, width 1: insert a C-TAG (0) or an S-TAG (1).
pub const MAC_VLANIR_CSVL: (u32, u32) = (19, 1);
/// `MAC_VLANIR_VLTI`, index 20, width 1: take the tag to insert from the
/// context descriptor rather than from this register.
pub const MAC_VLANIR_VLTI: (u32, u32) = (20, 1);

/// `MAC_PFR_VTFE`, index 16, width 1: the master switch for VLAN filtering,
/// which lives in the packet filter register and not with the rest.
pub const MAC_PFR_VTFE: (u32, u32) = (16, 1);

/// `VLAN_VID_MASK`: a VLAN id is twelve bits.
pub const VLAN_VID_MASK: u16 = 0x0fff;
/// `VLAN_N_VID`: how many there are.
pub const VLAN_N_VID: u32 = 4096;
/// `EVLS` = 3: strip the tag on every frame that has one.
pub const VLAN_STRIP_ALWAYS: u32 = 0x3;
/// `CRC32_POLY_LE`.
pub const CRC32_POLY_LE: u32 = 0xedb8_8320;

/// The CRC-32 of a VLAN id, as `xgbe_vid_crc32_le` computes it.
///
/// This is **not** a CRC over two bytes, and that is the whole point of
/// writing it out by hand. Linux runs the bit loop
/// `get_bitmask_order(VLAN_VID_MASK)` times — **twelve** iterations over the
/// little-endian id, not sixteen and not a multiple of eight. A byte-oriented
/// CRC-32, including the `crc32fast` crate behind
/// [`crate::net::ether_hash::ether_crc32`], cannot produce this value: it has
/// no way to stop in the middle of the second byte.
///
/// So the multicast filter's shared helper is deliberately not reused here.
/// The shape of what follows the CRC is the same — complement, reverse,
/// take the top bits — but the input is not.
#[inline]
pub const fn vid_crc32_le(vid: u16) -> u32 {
    let bytes = [(vid & 0xff) as u8, (vid >> 8) as u8];
    let mut crc: u32 = !0;
    let mut data_byte: u8 = 0;
    let mut i = 0;
    while i < 12 {
        if i % 8 == 0 {
            data_byte = bytes[i / 8];
        }
        let temp = ((crc & 1) ^ (data_byte as u32)) & 1;
        crc >>= 1;
        data_byte >>= 1;
        if temp != 0 {
            crc ^= CRC32_POLY_LE;
        }
        i += 1;
    }
    crc
}

/// Which of the sixteen buckets a VLAN id lands in.
/// `bitrev32(~xgbe_vid_crc32_le(vid)) >> 28`.
#[inline]
pub const fn vlan_hash_bucket(vid: u16) -> u32 {
    (!vid_crc32_le(vid)).reverse_bits() >> 28
}

/// Build the hash table value for a set of VLAN ids.
/// `xgbe_update_vlan_hash_table`.
///
/// Sixteen buckets for four thousand possible ids, so collisions are not an
/// edge case: this is a filter that lets through everything it is asked for
/// **and more**. Anything downstream that takes "it arrived" for "we joined
/// that VLAN" is wrong, and the only honest check is on the tag the
/// descriptor carries.
#[inline]
pub fn vlan_hash_table(vids: &[u16]) -> DeviceResult<u32> {
    let mut table = 0u32;
    for vid in vids {
        if *vid as u32 >= VLAN_N_VID {
            return Err(DeviceError::InvalidParam);
        }
        table |= 1 << vlan_hash_bucket(*vid);
    }
    Ok(table)
}

/// Write the hash table. `xgbe_update_vlan_hash_table`.
pub fn update_vlan_hash_table(acc: &dyn MmioAccess, vids: &[u16]) -> DeviceResult<()> {
    let table = vlan_hash_table(vids)?;
    write_field(
        acc,
        MAC_VLANHTR,
        MAC_VLANHTR_VLHT.0,
        MAC_VLANHTR_VLHT.1,
        table,
    );
    Ok(())
}

/// Strip the C-TAG off received frames and hand it to the descriptor.
/// `xgbe_enable_rx_vlan_stripping`.
pub fn enable_rx_vlan_stripping(acc: &dyn MmioAccess) {
    write_field(acc, MAC_VLANTR, MAC_VLANTR_EVLRXS.0, MAC_VLANTR_EVLRXS.1, 1);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_DOVLTC.0, MAC_VLANTR_DOVLTC.1, 1);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_ERSVLM.0, MAC_VLANTR_ERSVLM.1, 0);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_ESVL.0, MAC_VLANTR_ESVL.1, 0);
    write_field(
        acc,
        MAC_VLANTR,
        MAC_VLANTR_EVLS.0,
        MAC_VLANTR_EVLS.1,
        VLAN_STRIP_ALWAYS,
    );
}

/// Stop stripping. `xgbe_disable_rx_vlan_stripping`, which writes **one**
/// field where enabling wrote five: the other four are left as they are,
/// because they describe what a tag *is* rather than what to do with it.
pub fn disable_rx_vlan_stripping(acc: &dyn MmioAccess) {
    write_field(acc, MAC_VLANTR, MAC_VLANTR_EVLS.0, MAC_VLANTR_EVLS.1, 0);
}

/// Filter received frames by VLAN. `xgbe_enable_rx_vlan_filtering`.
///
/// The last write is the one worth knowing about: **`VL` is set to 1**, and
/// Linux's own comment says why — the hash table filter does nothing at all
/// while `VL` is zero. The cost is that **VLAN 1 always passes the filter**,
/// joined or not. That is not a bug being fixed here; it is a hole that has
/// to be known about by whatever trusts the filter.
pub fn enable_rx_vlan_filtering(acc: &dyn MmioAccess) {
    write_field(acc, MAC_PFR, MAC_PFR_VTFE.0, MAC_PFR_VTFE.1, 1);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_VTHM.0, MAC_VLANTR_VTHM.1, 1);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_VTIM.0, MAC_VLANTR_VTIM.1, 0);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_ETV.0, MAC_VLANTR_ETV.1, 1);
    write_field(acc, MAC_VLANTR, MAC_VLANTR_VL.0, MAC_VLANTR_VL.1, 1);
}

/// Stop filtering. `xgbe_disable_rx_vlan_filtering`: one bit, in a different
/// register from the four that enabling also set.
pub fn disable_rx_vlan_filtering(acc: &dyn MmioAccess) {
    write_field(acc, MAC_PFR, MAC_PFR_VTFE.0, MAC_PFR_VTFE.1, 0);
}

/// What the transmit side should do with VLAN tags, plus the two receive
/// switches. `xgbe_config_vlan_support`.
pub fn config_vlan_support(
    acc: &dyn MmioAccess,
    vids: &[u16],
    filtering: bool,
    stripping: bool,
) -> DeviceResult<()> {
    // Transmit tags come from a context descriptor, and they are C-TAGs.
    write_field(acc, MAC_VLANIR, MAC_VLANIR_CSVL.0, MAC_VLANIR_CSVL.1, 0);
    write_field(acc, MAC_VLANIR, MAC_VLANIR_VLTI.0, MAC_VLANIR_VLTI.1, 1);

    update_vlan_hash_table(acc, vids)?;

    if filtering {
        enable_rx_vlan_filtering(acc);
    } else {
        disable_rx_vlan_filtering(acc);
    }
    if stripping {
        enable_rx_vlan_stripping(acc);
    } else {
        disable_rx_vlan_stripping(acc);
    }
    Ok(())
}
