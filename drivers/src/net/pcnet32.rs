//! AMD PCnet-PCI II / PCnet-FAST III Ethernet (`Am79C970A` and relatives).
//!
//! Datasheet: AMD Am79C970A PCnet-PCI II Ethernet Controller, publication
//! 19436 Rev E (June 2000). Linux calls this driver `pcnet32`
//! (`drivers/net/ethernet/amd/pcnet32.c`) and the register names, the
//! bring-up order and the software style below are the ones it uses, so the
//! two can be read side by side.
//!
//! This is the one AMD NIC that can be exercised without AMD hardware: QEMU
//! emulates it as `-device pcnet`. The 10GbE `xgbe` found on Ryzen/EPYC
//! embedded parts is a different controller and gets its own driver.
//!
//! Three things make it unlike the Intel parts next door:
//!
//! 1. **It is programmed through I/O ports**, not MMIO: a register address
//!    port (RAP) selects the register and a data port (RDP) carries the
//!    value. The chip comes up in 16-bit (WIO) port mode and is switched to
//!    32-bit (DWIO); which one is in force changes the port offsets, so every
//!    access goes through [`Width`].
//! 2. **There are no ring base registers.** An *initialization block* in
//!    memory carries the MAC, the filter, the ring addresses and the ring
//!    sizes; its address goes in CSR1/CSR2 and the chip DMAs it in when INIT
//!    is set.
//! 3. **Descriptor lengths are two's complement** and the top four bits must
//!    be ones -- see [`neg_len`].
//!
//! It is also a 32-bit bus master: every physical address it is given must
//! fit in 32 bits, and there is no DMA allocator here that can promise that.
//! So the addresses are CHECKED ([`fits_32bit`]) and bring-up is refused if
//! one does not, rather than programming a truncated address and handing the
//! chip somebody else's memory to DMA into.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::{ip_cfg, timer_now_as_micros};
use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::net::get_sockets;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::utils::dma::DmaRegion;
use crate::{Device, DeviceError, DeviceResult};
use core::convert::TryFrom;
use core::sync::atomic::{fence, AtomicBool, Ordering};
use lock::Mutex;
use pci::{PCIDevice, BAR};

// ——— PCI identity ———

/// AMD's PCI vendor id.
pub const PCI_VENDOR_AMD: u16 = 0x1022;
/// PCnet-PCI II / PCnet-FAST / PCnet-FAST III. The one QEMU emulates.
pub const PCI_DEVICE_PCNET: u16 = 0x2000;
/// PCnet-Home (`Am79C978`), the same programming model over HomePNA.
pub const PCI_DEVICE_PCNET_HOME: u16 = 0x2001;

// ——— Ring geometry ———

/// Descriptors per ring. Must be a power of two: the chip is told the size as
/// a log2 exponent (see [`tlen_rlen`]), so nothing else is expressible.
const RING_LEN: usize = 32;
const RING_LOG2: u8 = 5;
const _: () = assert!(1 << RING_LOG2 == RING_LEN);

/// Bytes per RX/TX buffer. Linux uses 1544 for the same chip: a 1518-byte
/// maximum frame plus room for the alignment the descriptor wants.
const PKT_BUF_SIZE: usize = 1544;

/// The MTU advertised upwards, and the longest frame accepted for transmit.
const MAX_FRAME: usize = 1514;

/// Scratch for staging a frame smoltcp is about to write.
const TX_SCRATCH_LEN: usize = 1536;
const _: () = assert!(TX_SCRATCH_LEN >= MAX_FRAME);

// ——— CSR / BCR numbers and bits ———

const CSR0: u16 = 0;
const CSR1_IADR_LO: u16 = 1;
const CSR2_IADR_HI: u16 = 2;
const CSR3_IRQ_MASK: u16 = 3;
const CSR4_FEATURES: u16 = 4;
const CSR15_MODE: u16 = 15;
const BCR2_MISC: u16 = 2;
const BCR20_SWSTYLE: u16 = 20;

const CSR0_INIT: u16 = 0x0001;
const CSR0_STRT: u16 = 0x0002;
const CSR0_STOP: u16 = 0x0004;
const CSR0_TDMD: u16 = 0x0008;
const CSR0_IENA: u16 = 0x0040;
const CSR0_INTR: u16 = 0x0080;
const CSR0_IDON: u16 = 0x0100;
const CSR0_TINT: u16 = 0x0200;
const CSR0_RINT: u16 = 0x0400;
const CSR0_MERR: u16 = 0x0800;
const CSR0_MISS: u16 = 0x1000;
const CSR0_CERR: u16 = 0x2000;
const CSR0_BABL: u16 = 0x4000;
const CSR0_ERR: u16 = 0x8000;

/// CSR0 bits that mean "do something" or hold enable state, as opposed to the
/// write-1-to-clear status bits. Acknowledging an interrupt means writing the
/// status bits BACK with these masked off, or the ack restarts the chip.
const CSR0_CONTROL: u16 = CSR0_INIT | CSR0_STRT | CSR0_STOP | CSR0_TDMD | CSR0_IENA;

/// The CSR3 mask bits: BABLM | MISSM | MERRM | RINTM | TINTM | IDONM. Setting
/// them masks the corresponding interrupt source. This is how the chip is
/// quiesced while a bottom-half runs; there is no IMC/IMS register pair.
const CSR3_MASK_ALL: u16 = 0x5f00;

/// `DMAPLUS | APAD_XMT | MFCOM | RCVCCOM | TXSTRTM | ...` -- the value Linux
/// writes, which is the combination the part is tested with.
const CSR4_VALUE: u16 = 0x0915;

/// Software style 2: the 32-bit descriptor and init-block layout this module
/// encodes. Style 0 is the 16-bit LANCE layout and the structs below would be
/// read wrong.
const SWSTYLE_PCNET32: u16 = 2;

/// `PROM` in the init block's mode word: receive every frame on the wire.
const MODE_PROM: u16 = 0x8000;

// ——— Descriptor status bits ———

const RD_OWN: u16 = 0x8000;
const RD_ERR: u16 = 0x4000;
const RD_STP: u16 = 0x0200;
const RD_ENP: u16 = 0x0100;

const TD_OWN: u16 = 0x8000;
const TD_ERR: u16 = 0x4000;
const TD_STP: u16 = 0x0200;
const TD_ENP: u16 = 0x0100;

// ——— Port access ———

/// Which port width the chip is being driven in. The offsets of RAP, RDP,
/// RESET and BDP all move with it, which is why this is not a boolean kept
/// somewhere else.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Width {
    /// 16-bit port access. The chip is in this mode after a reset.
    Wio,
    /// 32-bit port access.
    Dwio,
}

impl Width {
    /// Register Data Port.
    #[inline]
    pub const fn rdp(self) -> u16 {
        match self {
            Width::Wio => 0x10,
            Width::Dwio => 0x10,
        }
    }

    /// Register Address Port.
    #[inline]
    pub const fn rap(self) -> u16 {
        match self {
            Width::Wio => 0x12,
            Width::Dwio => 0x14,
        }
    }

    /// Reset port. Reading it resets the chip.
    #[inline]
    pub const fn reset(self) -> u16 {
        match self {
            Width::Wio => 0x14,
            Width::Dwio => 0x18,
        }
    }

    /// Bus Configuration Data Port.
    #[inline]
    pub const fn bdp(self) -> u16 {
        match self {
            Width::Wio => 0x16,
            Width::Dwio => 0x1c,
        }
    }
}

/// The four port operations the chip needs, so the register sequences below
/// can be driven by a test as well as by a real I/O space.
pub trait PortAccess: Send + Sync {
    fn in16(&self, off: u16) -> u16;
    fn out16(&self, off: u16, value: u16);
    fn in32(&self, off: u16) -> u32;
    fn out32(&self, off: u16, value: u32);
}

/// Read CSR `csr`.
pub fn read_csr(p: &dyn PortAccess, w: Width, csr: u16) -> u16 {
    match w {
        Width::Wio => {
            p.out16(w.rap(), csr);
            p.in16(w.rdp())
        }
        Width::Dwio => {
            p.out32(w.rap(), csr as u32);
            (p.in32(w.rdp()) & 0xffff) as u16
        }
    }
}

/// Write `value` to CSR `csr`.
pub fn write_csr(p: &dyn PortAccess, w: Width, csr: u16, value: u16) {
    match w {
        Width::Wio => {
            p.out16(w.rap(), csr);
            p.out16(w.rdp(), value);
        }
        Width::Dwio => {
            p.out32(w.rap(), csr as u32);
            p.out32(w.rdp(), value as u32);
        }
    }
}

/// Read BCR `bcr`.
pub fn read_bcr(p: &dyn PortAccess, w: Width, bcr: u16) -> u16 {
    match w {
        Width::Wio => {
            p.out16(w.rap(), bcr);
            p.in16(w.bdp())
        }
        Width::Dwio => {
            p.out32(w.rap(), bcr as u32);
            (p.in32(w.bdp()) & 0xffff) as u16
        }
    }
}

/// Write `value` to BCR `bcr`.
pub fn write_bcr(p: &dyn PortAccess, w: Width, bcr: u16, value: u16) {
    match w {
        Width::Wio => {
            p.out16(w.rap(), bcr);
            p.out16(w.bdp(), value);
        }
        Width::Dwio => {
            p.out32(w.rap(), bcr as u32);
            p.out32(w.bdp(), value as u32);
        }
    }
}

/// Reset the chip by reading its reset port. The read's value is meaningless;
/// the access itself is the reset, and the chip comes back STOPped and in WIO.
pub fn reset(p: &dyn PortAccess, w: Width) {
    match w {
        Width::Wio => {
            let _ = p.in16(w.reset());
        }
        Width::Dwio => {
            let _ = p.in32(w.reset());
        }
    }
}

/// The RAP echo probe: a value written to RAP must read back.
///
/// 88 is the arbitrary witness Linux uses. In DWIO only the low 16 bits come
/// back, so the comparison is masked -- checking all 32 made the probe fail on
/// a chip that was answering correctly.
fn rap_echoes(p: &dyn PortAccess, w: Width) -> bool {
    const WITNESS: u16 = 88;
    match w {
        Width::Wio => {
            p.out16(w.rap(), WITNESS);
            p.in16(w.rap()) == WITNESS
        }
        Width::Dwio => {
            p.out32(w.rap(), WITNESS as u32);
            (p.in32(w.rap()) & 0xffff) == WITNESS as u32
        }
    }
}

/// Find out which port width this chip answers in.
///
/// The 16-bit probe goes FIRST, as it does in Linux: some older PCnet parts
/// answer the 32-bit probe by accident and then do not work in that mode.
/// Each probe resets the chip first, so CSR0 must read back exactly
/// [`CSR0_STOP`] -- any other value means this is not a PCnet at all.
pub fn detect_width(p: &dyn PortAccess) -> Option<Width> {
    reset(p, Width::Wio);
    if read_csr(p, Width::Wio, CSR0) == CSR0_STOP && rap_echoes(p, Width::Wio) {
        return Some(Width::Wio);
    }
    reset(p, Width::Dwio);
    if read_csr(p, Width::Dwio, CSR0) == CSR0_STOP && rap_echoes(p, Width::Dwio) {
        return Some(Width::Dwio);
    }
    None
}

// ——— Pure encodings ———

/// A descriptor length field: two's complement, with the top four bits ones.
///
/// The chip reads bits 11..0 as the negated byte count and REQUIRES bits
/// 15..12 to be set; a plain `-len` happens to satisfy that for any length
/// under 4096, but only by accident of the width, so the ones are put there on
/// purpose. A length the field cannot express is rejected by the callers
/// before they get here.
#[inline]
pub const fn neg_len(len: usize) -> u16 {
    0xf000 | ((len as u16).wrapping_neg() & 0x0fff)
}

/// The init block's `tlen_rlen` word: the two ring sizes as log2 exponents,
/// receive in bits 7..4 and transmit in bits 15..12.
#[inline]
pub const fn tlen_rlen(rx_log2: u8, tx_log2: u8) -> u16 {
    ((rx_log2 as u16) << 4) | ((tx_log2 as u16) << 12)
}

/// Does `paddr` fit in the 32 bits the chip has to address it with?
///
/// Every ring base and every buffer address handed to a PCnet goes through a
/// 32-bit field. Truncating one does not fail: it points the chip's DMA at a
/// different, live page of kernel memory.
#[inline]
pub const fn fits_32bit(paddr: usize) -> bool {
    paddr <= u32::MAX as usize
}

/// The usable frame length a completed RX descriptor describes, or `None` if
/// the descriptor does not hold a frame this driver will pass up.
///
/// `MCNT` (bits 11..0 of `msg_length`) counts the 4-byte Ethernet FCS, which
/// the stack above must not see, so it is subtracted here. A count that cannot
/// hold an FCS plus the 60-byte minimum frame is a runt, and one longer than
/// the buffer is the chip reporting a length it was never given room for:
/// both are dropped rather than turned into a slice.
#[inline]
pub fn rx_frame_len(msg_length: u32) -> Option<usize> {
    const FCS_LEN: usize = 4;
    const MIN_FRAME: usize = 60;
    let mcnt = (msg_length & 0x0fff) as usize;
    if mcnt < MIN_FRAME + FCS_LEN {
        return None;
    }
    let len = mcnt - FCS_LEN;
    if len > PKT_BUF_SIZE {
        return None;
    }
    Some(len)
}

/// Is this a completed RX descriptor holding one whole frame?
///
/// `OWN` clear means the chip has handed the descriptor back. `STP | ENP` and
/// no `ERR` means the frame started and ended in this one descriptor, which is
/// the only shape a buffer longer than the maximum frame can produce. A
/// descriptor that is ours but not a whole good frame still has to be
/// recycled, which is why this is a predicate and not a filter.
#[inline]
pub const fn rx_descriptor_is_good(status: u16) -> bool {
    status & RD_OWN == 0 && status & RD_ERR == 0 && status & (RD_STP | RD_ENP) == RD_STP | RD_ENP
}

/// Has the chip handed this RX descriptor back to software?
#[inline]
pub const fn rx_descriptor_is_ours(status: u16) -> bool {
    status & RD_OWN == 0
}

/// Is this TX descriptor free for software to post into?
#[inline]
pub const fn tx_descriptor_is_free(status: u16) -> bool {
    status & TD_OWN == 0
}

/// Can a frame of `len` bytes be put on the wire?
///
/// Rejected rather than clipped: a frame truncated to the buffer size goes out
/// corrupt while the caller is told the whole thing was sent, so the loss only
/// ever surfaces on the peer.
#[inline]
pub const fn tx_frame_postable(len: usize) -> bool {
    len > 0 && len <= MAX_FRAME
}

/// The next index in a ring of [`RING_LEN`] descriptors.
#[inline]
pub const fn next_desc(idx: usize) -> usize {
    if idx + 1 >= RING_LEN {
        0
    } else {
        idx + 1
    }
}

/// Stage a buffer of exactly `len` bytes for smoltcp to write into, on the
/// stack when it fits and on the heap when it does not.
///
/// `len` is whatever the IP layer computed, not a clamped MTU, so indexing a
/// fixed array with it panics the kernel from one `sendto`.
fn tx_stage<'a>(
    len: usize,
    scratch: &'a mut [u8; TX_SCRATCH_LEN],
    heap: &'a mut Vec<u8>,
) -> &'a mut [u8] {
    if len <= TX_SCRATCH_LEN {
        &mut scratch[..len]
    } else {
        heap.resize(len, 0);
        &mut heap[..]
    }
}

// ——— In-memory structures the chip DMAs ———

/// The initialization block, software style 2.
///
/// There is no register that holds a ring address on this part: this is how
/// the chip learns everything about the rings, the station address and the
/// filter. Its physical address goes in CSR1/CSR2 and CSR0's `INIT` bit makes
/// the chip read it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InitBlock {
    pub mode: u16,
    pub tlen_rlen: u16,
    pub phys_addr: [u8; 6],
    pub reserved: u16,
    pub filter: [u32; 2],
    pub rx_ring: u32,
    pub tx_ring: u32,
}

const _: () = assert!(core::mem::size_of::<InitBlock>() == 28);

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
struct RxDesc {
    base: u32,
    buf_length: u16,
    status: u16,
    msg_length: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
struct TxDesc {
    base: u32,
    length: u16,
    status: u16,
    misc: u32,
    reserved: u32,
}

const _: () = assert!(core::mem::size_of::<RxDesc>() == 16);
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

/// Build the init block for a bring-up, so the encoding can be checked without
/// a chip. Returns `None` if an address does not fit the 32-bit fields.
pub fn build_init_block(
    mac: &[u8; 6],
    rx_ring: usize,
    tx_ring: usize,
    promiscuous: bool,
) -> Option<InitBlock> {
    if !fits_32bit(rx_ring) || !fits_32bit(tx_ring) {
        return None;
    }
    Some(InitBlock {
        mode: if promiscuous { MODE_PROM } else { 0 },
        tlen_rlen: tlen_rlen(RING_LOG2, RING_LOG2),
        phys_addr: *mac,
        reserved: 0,
        // Accept every multicast group. With PROM set the filter is moot, but
        // it is also what the chip falls back on the moment promiscuous mode
        // is turned off, and an all-zero filter there drops IPv6 neighbour
        // discovery and mDNS without a word.
        filter: [0xffff_ffff, 0xffff_ffff],
        rx_ring: rx_ring as u32,
        tx_ring: tx_ring as u32,
    })
}

// ——— The real I/O space ———

/// The chip's PCI I/O BAR.
pub struct IoSpace {
    base: u16,
}

impl IoSpace {
    /// # Safety
    /// `base` must be the I/O BAR of a PCnet, and nothing else may be driving
    /// those 32 ports.
    pub const unsafe fn new(base: u16) -> Self {
        Self { base }
    }
}

impl PortAccess for IoSpace {
    fn in16(&self, off: u16) -> u16 {
        unsafe { x86_64::instructions::port::Port::<u16>::new(self.base + off).read() }
    }
    fn out16(&self, off: u16, value: u16) {
        unsafe { x86_64::instructions::port::Port::<u16>::new(self.base + off).write(value) }
    }
    fn in32(&self, off: u16) -> u32 {
        unsafe { x86_64::instructions::port::Port::<u32>::new(self.base + off).read() }
    }
    fn out32(&self, off: u16, value: u32) {
        unsafe { x86_64::instructions::port::Port::<u32>::new(self.base + off).write(value) }
    }
}

// ——— The device ———

/// One PCnet controller: its port space, its rings and where software is in
/// them.
pub struct Pcnet32 {
    io: Box<dyn PortAccess>,
    width: Width,
    mac: EthernetAddress,

    /// The init block stays allocated for the life of the device: the chip
    /// re-reads it on every `INIT`, so freeing it would hand a later restart
    /// whatever took its place.
    _init_block: DmaRegion,

    rx_ring: DmaRegion,
    rx_bufs: Vec<DmaRegion>,
    rx_next: usize,

    tx_ring: DmaRegion,
    tx_bufs: Vec<DmaRegion>,
    tx_next: usize,
}

impl Pcnet32 {
    fn udelay(us: u64) {
        let t0 = timer_now_as_micros();
        const MAX_SPINS: u64 = 10_000_000;
        let mut spins = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins >= MAX_SPINS {
                break;
            }
        }
    }

    #[inline]
    fn rx(&self, idx: usize) -> *mut RxDesc {
        unsafe { self.rx_ring.as_ptr::<RxDesc>().add(idx) }
    }

    #[inline]
    fn tx(&self, idx: usize) -> *mut TxDesc {
        unsafe { self.tx_ring.as_ptr::<TxDesc>().add(idx) }
    }

    #[inline]
    fn read_csr(&self, csr: u16) -> u16 {
        read_csr(&*self.io, self.width, csr)
    }

    #[inline]
    fn write_csr(&self, csr: u16, value: u16) {
        write_csr(&*self.io, self.width, csr, value)
    }

    /// Read the station address out of the address PROM at the bottom of the
    /// I/O space. Returns `None` for the two values that mean "nothing was
    /// programmed here", so a synthetic address can be used instead of an
    /// all-zero or broadcast one.
    fn read_aprom(io: &dyn PortAccess) -> Option<[u8; 6]> {
        let mut mac = [0u8; 6];
        for i in 0..3 {
            let word = io.in16((i * 2) as u16);
            mac[i * 2] = (word & 0xff) as u8;
            mac[i * 2 + 1] = (word >> 8) as u8;
        }
        if mac == [0; 6] || mac == [0xff; 6] || mac[0] & 1 == 1 {
            // A multicast bit in the station address is not a station address.
            None
        } else {
            Some(mac)
        }
    }

    /// Bring the chip up: detect the port width, read the MAC, build the
    /// rings and the init block, and start it.
    pub fn new(io: Box<dyn PortAccess>, mac_fallback: [u8; 6]) -> DeviceResult<Self> {
        let width = detect_width(&*io).ok_or_else(|| {
            warn!("[pcnet32] no PCnet answered at this I/O space");
            DeviceError::NotSupported
        })?;
        info!("[pcnet32] driving the chip in {:?} port mode", width);

        // 32-bit software style FIRST: it decides how the chip reads the
        // structures built below, and setting it after the init block address
        // means the chip parses style-0 fields out of a style-2 block.
        write_bcr(&*io, width, BCR20_SWSTYLE, SWSTYLE_PCNET32);

        // ASEL: let the chip pick the media itself rather than forcing one.
        let bcr2 = read_bcr(&*io, width, BCR2_MISC);
        write_bcr(&*io, width, BCR2_MISC, bcr2 | 0x0002);

        let mac_bytes = Self::read_aprom(&*io).unwrap_or(mac_fallback);
        let mac = EthernetAddress::from_bytes(&mac_bytes);

        // The rings and the init block. Each allocation's physical address
        // goes into a 32-bit field, so each is checked before the chip can be
        // told about it.
        let init_block =
            DmaRegion::alloc(core::mem::size_of::<InitBlock>()).ok_or(DeviceError::DmaError)?;
        let rx_ring = DmaRegion::alloc(RING_LEN * core::mem::size_of::<RxDesc>())
            .ok_or(DeviceError::DmaError)?;
        let tx_ring = DmaRegion::alloc(RING_LEN * core::mem::size_of::<TxDesc>())
            .ok_or(DeviceError::DmaError)?;

        let mut rx_bufs = Vec::with_capacity(RING_LEN);
        let mut tx_bufs = Vec::with_capacity(RING_LEN);
        for _ in 0..RING_LEN {
            rx_bufs.push(DmaRegion::alloc_uninit(PKT_BUF_SIZE).ok_or(DeviceError::DmaError)?);
            tx_bufs.push(DmaRegion::alloc_uninit(PKT_BUF_SIZE).ok_or(DeviceError::DmaError)?);
        }

        // Every address below goes into a 32-bit field. Truncating one does
        // not fail: it points the chip's DMA at an unrelated, live page.
        //
        // Not under `cfg(test)`: there the "physical" address a `DmaRegion`
        // reports is a host heap pointer, which is a 64-bit userspace address
        // and fails this for reasons that have nothing to do with the chip.
        // [`fits_32bit`] and [`build_init_block`] are tested directly instead.
        #[cfg(not(test))]
        {
            // Edition 2018: `[&T; N].into_iter()` yields `&&T`, so the array
            // is iterated by reference and copied instead.
            for region in [&init_block, &rx_ring, &tx_ring]
                .iter()
                .copied()
                .chain(rx_bufs.iter())
                .chain(tx_bufs.iter())
            {
                if !fits_32bit(region.paddr()) {
                    error!(
                        "[pcnet32] DMA memory at {:#x} is above 4 GiB; this chip is a \
                         32-bit bus master and cannot address it",
                        region.paddr()
                    );
                    return Err(DeviceError::DmaError);
                }
            }
        }

        // RX descriptors: the chip owns all of them from the start, each
        // pointing at its buffer with the (negated) room it has.
        for i in 0..RING_LEN {
            let desc = RxDesc {
                base: rx_bufs[i].paddr() as u32,
                buf_length: neg_len(PKT_BUF_SIZE),
                status: RD_OWN,
                msg_length: 0,
                reserved: 0,
            };
            unsafe { core::ptr::write_volatile(rx_ring.as_ptr::<RxDesc>().add(i), desc) };
        }

        // TX descriptors: software owns all of them (OWN clear), each already
        // pointing at its buffer so `send` only has to fill in the length.
        for i in 0..RING_LEN {
            let desc = TxDesc {
                base: tx_bufs[i].paddr() as u32,
                length: 0,
                status: 0,
                misc: 0,
                reserved: 0,
            };
            unsafe { core::ptr::write_volatile(tx_ring.as_ptr::<TxDesc>().add(i), desc) };
        }

        // The addresses were checked above (outside `cfg(test)`), so this
        // refusal is the second line of defence, not the first.
        let block = build_init_block(&mac_bytes, rx_ring.paddr(), tx_ring.paddr(), true)
            .or_else(|| {
                if cfg!(test) {
                    // Host heap pointers: see the comment above.
                    build_init_block(
                        &mac_bytes,
                        rx_ring.paddr() & u32::MAX as usize,
                        tx_ring.paddr() & u32::MAX as usize,
                        true,
                    )
                } else {
                    None
                }
            })
            .ok_or(DeviceError::DmaError)?;
        unsafe { core::ptr::write_volatile(init_block.as_ptr::<InitBlock>(), block) };

        fence(Ordering::SeqCst);

        // Stop the chip before pointing it at anything: CSR1/CSR2 are only
        // read on INIT, but CSR4 is not, and a running chip DMAs from the old
        // rings while they are being replaced.
        write_csr(&*io, width, CSR0, CSR0_STOP);
        write_csr(&*io, width, CSR4_FEATURES, CSR4_VALUE);
        write_csr(&*io, width, CSR15_MODE, block.mode);
        let ib = init_block.paddr() as u32;
        write_csr(&*io, width, CSR1_IADR_LO, (ib & 0xffff) as u16);
        write_csr(&*io, width, CSR2_IADR_HI, (ib >> 16) as u16);

        // INIT makes the chip DMA the block in; IDON says it has.
        write_csr(&*io, width, CSR0, CSR0_INIT);
        let mut armed = false;
        for _ in 0..1000 {
            if read_csr(&*io, width, CSR0) & CSR0_IDON != 0 {
                armed = true;
                break;
            }
            Self::udelay(100);
        }
        if !armed {
            // Starting anyway would run the chip off whatever its rings
            // pointed at before, which on a warm reboot is the previous
            // kernel's memory.
            error!("[pcnet32] the chip never acknowledged the initialization block (no IDON)");
            write_csr(&*io, width, CSR0, CSR0_STOP);
            return Err(DeviceError::IoError);
        }

        // Unmask every source, then start with interrupts enabled.
        let csr3 = read_csr(&*io, width, CSR3_IRQ_MASK) & 0x00ff;
        write_csr(&*io, width, CSR3_IRQ_MASK, csr3);
        write_csr(&*io, width, CSR0, CSR0_STRT | CSR0_IENA);

        info!("[pcnet32] up with station address {}", mac);

        Ok(Self {
            io,
            width,
            mac,
            _init_block: init_block,
            rx_ring,
            rx_bufs,
            rx_next: 0,
            tx_ring,
            tx_bufs,
            tx_next: 0,
        })
    }

    pub fn mac(&self) -> EthernetAddress {
        self.mac
    }

    /// Mask every interrupt source at the chip. There is no IMC here: the
    /// mask bits live in CSR3.
    pub fn mask_irqs(&self) {
        let csr3 = self.read_csr(CSR3_IRQ_MASK);
        self.write_csr(CSR3_IRQ_MASK, csr3 | CSR3_MASK_ALL);
    }

    /// Unmask and re-enable interrupt delivery.
    pub fn unmask_irqs(&self) {
        let csr3 = self.read_csr(CSR3_IRQ_MASK) & 0x00ff;
        self.write_csr(CSR3_IRQ_MASK, csr3);
        self.write_csr(CSR0, CSR0_IENA);
    }

    /// Acknowledge the interrupt and return the CSR0 that was pending, or
    /// `None` if this was not our interrupt.
    ///
    /// The status bits are written BACK to clear them, with the control bits
    /// masked off -- writing CSR0 whole would set `STOP` or re-trigger `INIT`
    /// on the way past.
    pub fn ack_irq(&self) -> Option<u16> {
        let csr0 = self.read_csr(CSR0);
        if csr0 & CSR0_INTR == 0 {
            return None;
        }
        self.write_csr(CSR0, csr0 & !CSR0_CONTROL);
        if csr0 & CSR0_MERR != 0 {
            warn!("[pcnet32] memory error: the chip lost a bus transfer");
        }
        if csr0 & CSR0_MISS != 0 {
            warn!("[pcnet32] missed frame: no RX descriptor was free");
        }
        if csr0 & CSR0_BABL != 0 {
            warn!("[pcnet32] babble: a transmit ran past the maximum frame length");
        }
        Some(csr0)
    }

    /// Take the next completed frame off the RX ring, recycling every
    /// descriptor the chip has handed back on the way.
    pub fn receive(&mut self) -> Option<Vec<u8>> {
        // Bounded by the ring: a descriptor the chip owns ends the sweep, and
        // without the bound a ring of recycled-but-bad descriptors would spin
        // here forever.
        for _ in 0..RING_LEN {
            let desc = self.rx(self.rx_next);
            let status = unsafe { core::ptr::read_volatile(&(*desc).status) };
            if !rx_descriptor_is_ours(status) {
                return None;
            }

            fence(Ordering::Acquire);

            let msg_length = unsafe { core::ptr::read_volatile(&(*desc).msg_length) };
            let frame = if rx_descriptor_is_good(status) {
                rx_frame_len(msg_length).map(|len| {
                    let vaddr = self.rx_bufs[self.rx_next].vaddr();
                    unsafe { core::slice::from_raw_parts(vaddr as *const u8, len) }.to_vec()
                })
            } else {
                None
            };

            // Recycle whether or not the frame was usable. A descriptor left
            // owned by software is a descriptor the chip will never fill
            // again, and a ring of them is a NIC that has stopped receiving
            // with nothing logged.
            unsafe {
                core::ptr::write_volatile(&mut (*desc).msg_length, 0);
                core::ptr::write_volatile(&mut (*desc).buf_length, neg_len(PKT_BUF_SIZE));
                fence(Ordering::Release);
                core::ptr::write_volatile(&mut (*desc).status, RD_OWN);
            }
            self.rx_next = next_desc(self.rx_next);

            if frame.is_some() {
                return frame;
            }
        }
        None
    }

    /// Is the next TX descriptor free?
    pub fn can_send(&self) -> bool {
        let status = unsafe { core::ptr::read_volatile(&(*self.tx(self.tx_next)).status) };
        tx_descriptor_is_free(status)
    }

    /// Post a frame. Returns false, dropping nothing on the wire, if the frame
    /// cannot be expressed or the ring slot is still the chip's.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !tx_frame_postable(frame.len()) {
            return false;
        }
        // Re-checked under the caller's lock: `can_send` on the smoltcp path
        // runs under a different acquisition, so on SMP the slot can go to
        // another CPU in between, and overwriting a descriptor the chip is
        // DMA-reading corrupts the frame already going out.
        if !self.can_send() {
            return false;
        }

        let idx = self.tx_next;
        let len = frame.len();
        let vaddr = self.tx_bufs[idx].vaddr();
        unsafe { core::slice::from_raw_parts_mut(vaddr as *mut u8, len) }.copy_from_slice(frame);

        let desc = self.tx(idx);
        unsafe {
            core::ptr::write_volatile(&mut (*desc).misc, 0);
            core::ptr::write_volatile(&mut (*desc).length, neg_len(len));
            fence(Ordering::Release);
            // OWN goes last: it is what hands the descriptor over, and the
            // chip may start reading the instant it is set.
            core::ptr::write_volatile(&mut (*desc).status, TD_OWN | TD_STP | TD_ENP);
        }

        fence(Ordering::SeqCst);
        self.tx_next = next_desc(self.tx_next);

        // TDMD: do not wait for the poll timer to notice the descriptor.
        self.write_csr(CSR0, CSR0_IENA | CSR0_TDMD);
        true
    }

    /// Count the transmit errors the chip has reported since the last sweep.
    /// The descriptors are left alone: `can_send` reads the same `OWN` bit.
    pub fn reap_tx_errors(&self) -> usize {
        let mut errors = 0;
        for i in 0..RING_LEN {
            let desc = self.tx(i);
            let status = unsafe { core::ptr::read_volatile(&(*desc).status) };
            if tx_descriptor_is_free(status) && status & TD_ERR != 0 {
                errors += 1;
                unsafe { core::ptr::write_volatile(&mut (*desc).status, status & !TD_ERR) };
            }
        }
        errors
    }
}

// ——— smoltcp glue ———

#[derive(Clone)]
pub struct Pcnet32Driver {
    pub hw: Arc<Mutex<Pcnet32>>,
    pub stats: Arc<Mutex<NetStats>>,
}

pub struct Pcnet32RxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct Pcnet32TxToken {
    driver: Pcnet32Driver,
    stats: Arc<Mutex<NetStats>>,
}

impl phy::Device<'_> for Pcnet32Driver {
    type RxToken = Pcnet32RxToken;
    type TxToken = Pcnet32TxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.hw.lock().receive().map(|pkt| {
            (
                Pcnet32RxToken {
                    data: pkt,
                    stats: self.stats.clone(),
                },
                Pcnet32TxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(Pcnet32TxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MAX_FRAME;
        // Leave `max_burst_size` unset: smoltcp clamps the TCP window to
        // `burst * MSS` through a u16, so a burst of 64 wraps and caps the
        // connection at a few Mbps. The same trap is documented in `e1000.rs`.
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for Pcnet32RxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        {
            let mut stats = self.stats.lock();
            stats.rx_packets += 1;
            stats.rx_bytes += self.data.len() as u64;
        }
        // AF_PACKET taps, after smoltcp has let go of SOCKETS.
        super::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for Pcnet32TxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap = Vec::new();
        let buf: &mut [u8] = tx_stage(len, &mut scratch, &mut heap);
        let result = f(buf)?;

        let sent = {
            let mut hw = self.driver.hw.lock();
            hw.send(buf)
        };

        if !sent {
            // NEVER report a dropped frame as sent. smoltcp emits the ACK for
            // a received segment through the TxToken paired with its RxToken
            // and has already advanced `remote_last_ack`, so an `Ok` here
            // loses that ACK for good and the peer stalls on a window it
            // believes is closed. `Err(Exhausted)` keeps smoltcp consistent so
            // the segment is re-emitted.
            let mut stats = self.stats.lock();
            stats.tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }

        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct Pcnet32Interface {
    iface: Arc<Mutex<Interface<'static, Pcnet32Driver>>>,
    driver: Pcnet32Driver,
    name: String,
    irq: usize,
    poll_pending: Arc<AtomicBool>,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl Pcnet32Interface {
    /// Drain smoltcp WITHOUT touching the interrupt mask. The IRQ path unmasks
    /// only after clearing `poll_pending`; unmasking in here would let a frame
    /// arriving in the window fire an interrupt that finds `poll_pending`
    /// still true, acknowledges the cause and queues nothing, losing the
    /// frame until the next one arrives.
    fn poll_inner(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();
        // `try_lock`, not a blocking acquire. This runs from a deferred job,
        // and a socket syscall drains that queue WHILE holding the socket set.
        // On this IRQ-off kernel a blocking acquire then spins forever on a
        // lock the same CPU already owns -- the deadlock `e1000.rs` records
        // hitting during a 64 MB download. Whoever holds the set drives the
        // poll on release, so skipping loses nothing.
        let res = match (sockets.try_lock(), self.iface.try_lock()) {
            (Some(mut sockets), Some(mut iface)) => match iface.poll(&mut sockets, timestamp) {
                Ok(p) => {
                    trace!("[pcnet32] poll: {:?}", p);
                    Ok(())
                }
                Err(err) => {
                    warn!("[pcnet32] poll got err {}", err);
                    Err(DeviceError::IoError)
                }
            },
            _ => Ok(()),
        };
        super::net_flush_deferred_packets();
        res
    }
}

impl Scheme for Pcnet32Interface {
    fn name(&self) -> &str {
        "pcnet32"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let pending = {
            let hw = self.driver.hw.lock();
            hw.ack_irq()
        };
        let Some(_csr0) = pending else {
            // Not ours: a shared INTx line. Re-enabling is still needed
            // because the ack above did not run.
            if !self.poll_pending.load(Ordering::SeqCst) {
                self.driver.hw.lock().unmask_irqs();
            }
            return;
        };

        if self.poll_pending.load(Ordering::SeqCst) {
            // A bottom-half is already queued and the chip stays masked until
            // it runs. Unmasking here would let further edges acknowledge
            // causes that bottom-half never sees.
            return;
        }

        self.poll_pending.store(true, Ordering::SeqCst);
        self.driver.hw.lock().mask_irqs();

        // The guard is built OUTSIDE the closure and moved in, so the mask is
        // lifted whether the job runs or is dropped unexecuted:
        // `deferred_job` caps its queue and evicts entries under pressure, and
        // clearing the flag in the closure BODY left an evicted bottom-half
        // with the NIC masked and `poll_pending` stuck true forever. This is
        // the shape `e1000.rs` and `e1000e.rs` both settled on.
        struct PollPendingGuard {
            pending: Arc<AtomicBool>,
            iface: Pcnet32Interface,
        }
        impl Drop for PollPendingGuard {
            fn drop(&mut self) {
                // Cleared BEFORE unmasking, so an interrupt arriving after the
                // unmask finds `poll_pending` false and queues a fresh poll.
                self.pending.store(false, Ordering::SeqCst);
                self.iface.driver.hw.lock().unmask_irqs();
            }
        }
        let guard = PollPendingGuard {
            pending: self.poll_pending.clone(),
            iface: self.clone(),
        };
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job_front(move || {
            let guard = guard;
            let errors = me.driver.hw.lock().reap_tx_errors();
            if errors > 0 {
                me.stats.lock().tx_errors += errors as u64;
            }
            let _ = me.poll_inner();
            drop(guard);
        });
    }
}

impl NetScheme for Pcnet32Interface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn ifname(&self) -> &str {
        &self.name
    }

    fn is_loopback(&self) -> bool {
        false
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: EthernetAddress,
    ) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        ip_cfg::seed_neighbor(&mut self.iface.lock(), protocol, hardware, timestamp)
    }

    fn poll(&self) -> DeviceResult {
        // The periodic path owns the mask itself; the IRQ path goes through
        // `poll_inner` and its guard.
        let res = self.poll_inner();
        self.driver.hw.lock().unmask_irqs();
        res
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        match self.driver.hw.lock().receive() {
            Some(pkt) => {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                Ok(n)
            }
            None => Err(DeviceError::NotReady),
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        if !tx_frame_postable(data.len()) {
            return Err(DeviceError::InvalidParam);
        }
        if self.driver.hw.lock().send(data) {
            Ok(data.len())
        } else {
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        true
    }

    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        ip_cfg::set_ipv4_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::add_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::remove_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::add_route(
            &mut self.iface.lock(),
            &mut self.routes.lock(),
            cidr,
            gateway,
        )
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::del_route(&mut self.iface.lock(), &mut self.routes.lock(), cidr)
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        ip_cfg::reported_routes(&self.routes.lock(), iface.ip_addrs())
    }

    fn get_stats(&self) -> NetStats {
        self.stats.lock().clone()
    }

    fn get_mtu(&self) -> usize {
        1500
    }
}

/// Build the interface over a brought-up chip.
pub fn init(
    name: String,
    irq: usize,
    io: Box<dyn PortAccess>,
    index: usize,
) -> DeviceResult<Pcnet32Interface> {
    info!("Probing pcnet32 {}", name);

    // Only used if the address PROM holds nothing: a locally administered
    // address, so it cannot collide with a real AMD one.
    let fallback = [0x52, 0x54, 0x00, 0x12, 0x34, index as u8];
    let hw = Pcnet32::new(io, fallback)?;
    let ethernet_addr = hw.mac();
    let hw = Arc::new(Mutex::new(hw));
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let driver = Pcnet32Driver {
        hw,
        stats: stats.clone(),
    };

    let mac = ethernet_addr.as_bytes();
    let mut eui64 = [0u8; 8];
    eui64[0] = mac[0] ^ 2;
    eui64[1] = mac[1];
    eui64[2] = mac[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = mac[3];
    eui64[6] = mac[4];
    eui64[7] = mac[5];
    let link_local = Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        (eui64[0] as u16) << 8 | eui64[1] as u16,
        (eui64[2] as u16) << 8 | eui64[3] as u16,
        (eui64[4] as u16) << 8 | eui64[5] as u16,
        (eui64[6] as u16) << 8 | eui64[7] as u16,
    );

    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    // Per-NIC route storage. A shared `static mut` would alias if two of these
    // probed, corrupting both route tables.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(ip_addrs.clone())
        .routes(Routes::new(routes_storage))
        .finalize();

    crate::klog_info!("pcnet32 interface {} discovered", name);
    Ok(Pcnet32Interface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        poll_pending: Arc::new(AtomicBool::new(false)),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    })
}

pub struct Pcnet32DriverPci;

/// Does this driver handle `(vendor, device)`?
///
/// A free function so the match can be tested without the registry: the PCI
/// ids are the one part of a driver that is wrong in a way nothing else
/// notices, because a mismatch just leaves the NIC unclaimed.
#[inline]
pub const fn pcnet32_matched(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == PCI_VENDOR_AMD
        && (device_id == PCI_DEVICE_PCNET || device_id == PCI_DEVICE_PCNET_HOME)
}

impl PciDriver for Pcnet32DriverPci {
    fn name(&self) -> &str {
        "pcnet32"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        pcnet32_matched(vendor_id, device_id)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        _mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        // The PCnet is driven through its I/O BAR. It has no memory BAR worth
        // using and QEMU gives it none at all, so a device whose BAR0 is not
        // I/O space is not one this driver can talk to.
        let Some(BAR::IO(base, len)) = dev.bars[0] else {
            warn!("[pcnet32] BAR0 is not I/O space; nothing here to drive");
            return Err(DeviceError::NotSupported);
        };
        if len < 0x20 {
            warn!(
                "[pcnet32] BAR0 is only {} bytes; the register window is 32",
                len
            );
            return Err(DeviceError::NotSupported);
        }
        let Ok(base) = u16::try_from(base) else {
            warn!(
                "[pcnet32] I/O base {:#x} is outside the 16-bit port space",
                base
            );
            return Err(DeviceError::NotSupported);
        };

        let name = crate::net::next_eth_ifname();
        let vector = irq.map(|idx| idx + 32).unwrap_or(0);
        let io = Box::new(unsafe { IoSpace::new(base) });
        let iface = init(name, vector, io, 0)?;
        Ok(Device::Net(Arc::new(iface)))
    }
}

#[cfg(test)]
mod fake_ports;

#[cfg(test)]
mod bringup_tests;
