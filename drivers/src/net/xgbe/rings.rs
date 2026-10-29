//! La memoria de los anillos del XGMAC.
//!
//! Las tandas 10 a 27 dejaron escrito, registro a registro, todo lo que
//! `xgbe_init` hace. Lo que falta para pasar una trama es memoria: dos
//! anillos de descriptores y un buffer por hueco de recepcion. Eso es lo
//! que hay aqui.
//!
//! Nada de esto ha corrido en hardware: QEMU no emula un XGMAC.

use super::*;
use crate::utils::dma::DmaRegion;
use alloc::vec::Vec;

/// Descriptors per ring. 64 each way, as the other drivers in this tree use.
pub const RX_RING_SIZE: usize = 64;
/// Descriptors per transmit ring.
pub const TX_RING_SIZE: usize = 64;

/// Two rings' worth of DMA memory, plus a buffer behind every receive slot.
///
/// Three things about this device make the shape different from the PCnet
/// and the GENET next door:
///
/// * **The addresses are 64 bits.** Both the ring base (`RDLR_HI`/`RDLR_LO`)
///   and the per-descriptor buffer pointers carry a high half, so there is no
///   "must live below 4 GiB" refusal here. That refusal exists in `pcnet32`
///   and in `macb` because those parts genuinely truncate.
/// * **The descriptors are written back by the chip**, so the ring memory has
///   to be coherent: a cached line holding a stale `OWN` bit is a frame that
///   never arrives, or worse, one that is read while the chip is still in it.
/// * **A frame can span several descriptors.** With a jumbo MTU the buffer is
///   a page ([`calc_rx_buf_size`]) and a 9022-byte frame comes back over
///   three slots, `FD` on the first and `LD` on the last.
pub struct XgbeRings {
    /// The DMA channel these rings belong to.
    channel: u32,
    tx_ring: DmaRegion,
    rx_ring: DmaRegion,
    tx_bufs: Vec<DmaRegion>,
    rx_bufs: Vec<DmaRegion>,
    buf_size: u32,
    /// The next transmit slot to build a frame in.
    tx_next: usize,
    /// The oldest transmit slot the chip has not given back.
    tx_clean: usize,
    /// The next receive slot to look at.
    rx_next: usize,
    /// The last receive slot handed to the chip, which is where the tail
    /// pointer stands.
    rx_tail: usize,
}

impl XgbeRings {
    /// Allocate both rings and the receive buffers for an MTU.
    pub fn alloc(channel: u32, mtu: u32) -> DeviceResult<Self> {
        let buf_size = calc_rx_buf_size(mtu);
        let desc_bytes = core::mem::size_of::<Desc>();

        let tx_ring = DmaRegion::alloc_coherent(TX_RING_SIZE * desc_bytes)
            .or_else(|| DmaRegion::alloc(TX_RING_SIZE * desc_bytes))
            .ok_or(DeviceError::DmaError)?;
        let rx_ring = DmaRegion::alloc_coherent(RX_RING_SIZE * desc_bytes)
            .or_else(|| DmaRegion::alloc(RX_RING_SIZE * desc_bytes))
            .ok_or(DeviceError::DmaError)?;

        let mut rx_bufs = Vec::with_capacity(RX_RING_SIZE);
        let mut tx_bufs = Vec::with_capacity(TX_RING_SIZE);
        for _ in 0..RX_RING_SIZE {
            rx_bufs.push(DmaRegion::alloc_uninit(buf_size as usize).ok_or(DeviceError::DmaError)?);
        }
        for _ in 0..TX_RING_SIZE {
            tx_bufs.push(DmaRegion::alloc_uninit(buf_size as usize).ok_or(DeviceError::DmaError)?);
        }

        let mut rings = Self {
            channel,
            tx_ring,
            rx_ring,
            tx_bufs,
            rx_bufs,
            buf_size,
            tx_next: 0,
            tx_clean: 0,
            rx_next: 0,
            rx_tail: RX_RING_SIZE - 1,
        };
        rings.reset_descriptors();
        Ok(rings)
    }

    /// The receive buffer size these rings were built for.
    #[inline]
    pub fn buffer_size(&self) -> u32 {
        self.buf_size
    }

    fn tx(&mut self) -> &mut [Desc] {
        // SAFETY: the region was allocated as TX_RING_SIZE descriptors and
        // nothing else aliases it.
        unsafe { core::slice::from_raw_parts_mut(self.tx_ring.as_ptr::<Desc>(), TX_RING_SIZE) }
    }

    fn rx(&mut self) -> &mut [Desc] {
        // SAFETY: as above.
        unsafe { core::slice::from_raw_parts_mut(self.rx_ring.as_ptr::<Desc>(), RX_RING_SIZE) }
    }

    /// Put both rings back to their start-of-day state: every transmit slot
    /// ours, every receive slot the chip's.
    fn reset_descriptors(&mut self) {
        for i in 0..TX_RING_SIZE {
            self.tx()[i] = Desc::default();
        }
        for i in 0..RX_RING_SIZE {
            let buf = self.rx_bufs[i].paddr() as u64;
            // No header buffer: split-header receive is not on, so the whole
            // frame lands in the payload buffer. `rx_desc_init` still wants
            // both pointers, and Linux passes the same page twice when it
            // has no separate header page either.
            let wants_interrupt =
                rx_desc_wants_interrupt(i as u32, RX_USECS_DEFAULT, RX_FRAMES_PER_INTERRUPT);
            self.rx()[i] = rx_desc_init(buf, buf, wants_interrupt);
        }
        self.tx_next = 0;
        self.tx_clean = 0;
        self.rx_next = 0;
        self.rx_tail = RX_RING_SIZE - 1;
    }

    /// Hand both rings to the chip. The register writes themselves are
    /// [`program_tx_ring`] and [`program_rx_ring`] from tanda 10.
    pub fn program(&mut self, acc: &dyn MmioAccess) -> DeviceResult<()> {
        let tx_paddr = self.tx_ring.paddr() as u64;
        let rx_paddr = self.rx_ring.paddr() as u64;
        program_tx_ring(acc, self.channel, tx_paddr, TX_RING_SIZE as u32)?;
        program_rx_ring(
            acc,
            self.channel,
            rx_paddr,
            RX_RING_SIZE as u32,
            self.buf_size,
        )
    }

    /// Is there room for a frame of one fragment?
    #[inline]
    pub fn can_send(&self) -> bool {
        (self.tx_next + 1) % TX_RING_SIZE != self.tx_clean
    }

    /// Copy a frame into the ring and hand it over.
    ///
    /// One fragment: the frame is copied into the slot's own buffer rather
    /// than mapped in place, which is what every other driver in this tree
    /// does and what keeps the lifetime question out of the hot path.
    pub fn send(&mut self, acc: &dyn MmioAccess, data: &[u8]) -> DeviceResult<()> {
        if data.is_empty() || data.len() > self.buf_size as usize {
            return Err(DeviceError::InvalidParam);
        }
        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }
        let slot = self.tx_next;
        let buf = self.tx_bufs[slot].paddr() as u64;
        // SAFETY: the region is at least `buf_size` bytes and is ours until
        // the descriptor below hands it over.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.tx_bufs[slot].as_ptr::<u8>(),
                data.len(),
            );
        }

        let fragments = [TxFragment {
            dma: buf,
            len: data.len() as u32,
        }];
        let frame = TxFrame {
            fragments: &fragments,
            total_len: data.len() as u32,
            checksum_offload: false,
            wants_interrupt: true,
        };
        // `build_tx_frame` returns the index of the next FREE descriptor,
        // already wrapped --- not a count of the ones it used.
        let next = build_tx_frame(self.tx(), slot, &frame)?;

        // Everything the chip will read must be visible before `OWN` goes
        // out, and `OWN` must be visible before the tail pointer moves.
        fence_w();
        hand_tx_frame_to_dma(self.tx(), slot);
        fence_w();

        self.tx_next = next;
        let next_free = desc_paddr(self.tx_ring.paddr() as u64, self.tx_next);
        kick_tx(acc, self.channel, next_free);
        Ok(())
    }

    /// Take back every transmit slot the chip has finished with.
    pub fn reclaim_tx(&mut self) -> usize {
        let mut freed = 0;
        while self.tx_clean != self.tx_next {
            let slot = self.tx_clean;
            if desc_owned_by_dma(self.tx()[slot].desc3) {
                break;
            }
            self.tx()[slot] = Desc::default();
            self.tx_clean = (slot + 1) % TX_RING_SIZE;
            freed += 1;
        }
        freed
    }

    /// Pull one whole frame out of the receive ring, if there is one.
    ///
    /// Returns `None` while the frame is incomplete, which is the normal case
    /// in the middle of a jumbo frame: the slots are there but the one with
    /// `LD` has not come back yet, and the length only exists on that one.
    pub fn recv(&mut self, acc: &dyn MmioAccess) -> Option<Vec<u8>> {
        let start = self.rx_next;
        let mut slot = start;
        let mut spans = 0usize;
        let mut total = None;

        // Walk forward until the descriptor carrying `LD` turns up.
        loop {
            match read_rx_desc(&self.rx()[slot]) {
                RxRead::NotYours => return None,
                RxRead::Context => {}
                RxRead::Normal(desc) => {
                    if desc.last {
                        total = desc.packet_len;
                        spans += 1;
                        break;
                    }
                }
            }
            spans += 1;
            slot = (slot + 1) % RX_RING_SIZE;
            if spans >= RX_RING_SIZE {
                // A ring entirely full of fragments with no end: nothing
                // sane to return, and nothing to be gained by looping.
                return None;
            }
        }

        let total = total? as usize;
        let mut frame = Vec::with_capacity(total);
        let mut left = total;
        let mut src = start;
        for _ in 0..spans {
            let take = core::cmp::min(left, self.buf_size as usize);
            if take > 0 {
                // SAFETY: the chip has given the slot back, so the buffer is
                // ours again and holds at least `take` bytes of the frame.
                let bytes =
                    unsafe { core::slice::from_raw_parts(self.rx_bufs[src].as_ptr::<u8>(), take) };
                frame.extend_from_slice(bytes);
                left -= take;
            }
            src = (src + 1) % RX_RING_SIZE;
        }

        // Give every slot of the frame back before moving the tail.
        for _ in 0..spans {
            let give = self.rx_next;
            let buf = self.rx_bufs[give].paddr() as u64;
            let wants_interrupt =
                rx_desc_wants_interrupt(give as u32, RX_USECS_DEFAULT, RX_FRAMES_PER_INTERRUPT);
            self.rx()[give] = rx_desc_init(buf, buf, wants_interrupt);
            fence_w();
            self.rx()[give].desc3 |= DESC3_OWN;
            self.rx_tail = give;
            self.rx_next = (give + 1) % RX_RING_SIZE;
        }
        fence_w();
        // The tail is "the last descriptor the driver owns", so it points at
        // the slot just handed back and not one past it. See `program_rx_ring`.
        acc.write32(
            dma_channel_reg(self.channel, DMA_CH_RDTR_LO),
            desc_paddr(self.rx_ring.paddr() as u64, self.rx_tail) as u32,
        );
        Some(frame)
    }
}

#[cfg(test)]
impl XgbeRings {
    /// The physical address the transmit ring was given.
    pub(super) fn tx_paddr(&self) -> u64 {
        self.tx_ring.paddr() as u64
    }
    /// The physical address the receive ring was given.
    pub(super) fn rx_paddr(&self) -> u64 {
        self.rx_ring.paddr() as u64
    }
    /// A copy of one transmit descriptor.
    pub(super) fn tx_desc(&mut self, index: usize) -> Desc {
        self.tx()[index]
    }
    /// Pretend the chip finished with a transmit descriptor.
    pub(super) fn tx_give_back(&mut self, index: usize) {
        self.tx()[index].desc3 &= !DESC3_OWN;
    }
    /// Pretend the chip wrote a received fragment into a slot.
    pub(super) fn rx_write_back(&mut self, index: usize, desc3: u32, payload: &[u8]) {
        // SAFETY: the slot's buffer is at least `buf_size` bytes and nothing
        // else is reading it in a test.
        unsafe {
            core::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                self.rx_bufs[index].as_ptr::<u8>(),
                payload.len(),
            );
        }
        self.rx()[index].desc0 = 0;
        self.rx()[index].desc1 = 0;
        self.rx()[index].desc2 = 0;
        self.rx()[index].desc3 = desc3;
    }
    /// Where the driver thinks the receive tail is.
    pub(super) fn rx_tail_index(&self) -> usize {
        self.rx_tail
    }
}

/// The receive watchdog these rings are built for, in microseconds. Zero
/// together with a frame count of 1 means "ask on every descriptor", which is
/// the only honest setting until there is something measuring the cost.
pub const RX_USECS_DEFAULT: u32 = 0;

/// How many receive descriptors share one interrupt. `rx_frames` in Linux's
/// coalescing settings; 1 means every descriptor asks.
pub const RX_FRAMES_PER_INTERRUPT: u32 = 1;

/// A write barrier. Separated out so the ordering comments above point at
/// something rather than at an inline intrinsic.
#[inline]
fn fence_w() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}
