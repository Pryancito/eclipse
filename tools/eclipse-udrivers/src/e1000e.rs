//! An Intel 82574 network card (QEMU's `e1000e`), driven by the network
//! stack that runs as a process (`tools/eclipse-netstack`).
//!
//! The boot leaves the card to userspace with `udriver=e1000e@N`; the
//! kernel then neither brings it up nor so much as resets it, until the
//! process that claimed it dies (`zcore_drivers::user`). What the stack
//! sees is what a virtio card of its own gives it ([`crate::net`]): a
//! window of [`RX_BUFS`] receive slots and then [`TX_BUFS`] transmit slots,
//! [`BUF_SIZE`] bytes each, in the card's DMA memory, and the same
//! messages -- a frame in a receive slot ([`OP_RX`], at offset 0: this card
//! puts no header before a frame), a frame to send in a transmit slot
//! ([`OP_TX`], [`TX_HEADROOM`] bytes in), a slot given back. Nothing is
//! copied: the card reads and writes the slots themselves.
//!
//! The rings are the card's legacy descriptor rings (82574 datasheet
//! §7.1.4, §7.2.10). A slot comes back from the stack in any order, so a
//! descriptor is not a slot: each ring has a table, beside it in the same
//! page, of the slot each descriptor was given. There are more descriptors
//! than slots, so a ring is never full and its tail never has to wait.
//!
//! Two sides, as the virtio card has: whoever gives the card what the stack
//! has for it ([`Giver`]: receive slots back, frames to send; it writes the
//! tails) and the card's thread, which takes what the card has done
//! ([`Taker`]: frames received, transmit slots done with). Each has its own
//! end of each ring, and the stack's lock is never what keeps them apart.
//!
//! Interrupts are the card's receive causes and link changes, never the
//! end of a send: a send is done before the card's thread next looks
//! (QEMU sends in the write to the tail), and a transmit interrupt would
//! restart the card's moderation window every time, which put the small
//! echo of a lent e1000e at 720-830 us where it is 460-660 without
//! (#2612). Only when the card's thread is about to sleep with frames
//! still on the card does it ask for one ([`Taker::sleep_with`]).

use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::claim::{self, Dma};
use crate::net::{Msg, BUF_SIZE, FRAME_MAX, OP_RX, OP_TX, RX_BUFS, TX_BUFS, TX_HEADROOM};
use crate::zx::{Handle, Status};
use crate::PAGE;

// Registers, by byte offset (82574 datasheet §10.2).
const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EERD: usize = 0x0014;
const CTRL_EXT: usize = 0x0018;
const ICR: usize = 0x00c0;
const ITR: usize = 0x00c4;
const IMS: usize = 0x00d0;
const IMC: usize = 0x00d8;
const IAM: usize = 0x00e0;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const RDTR: usize = 0x2820;
const RADV: usize = 0x282c;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TIDV: usize = 0x3820;
const TXDCTL: usize = 0x3828;
const TADV: usize = 0x382c;
const TARC0: usize = 0x3840;
const RXCSUM: usize = 0x5000;
const RFCTL: usize = 0x5008;
const MTA: usize = 0x5200;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;
const VFTA: usize = 0x5600;
const MRQC: usize = 0x5818;
/// The last register this driver touches, and so how much of the window
/// it needs.
pub const REGS_SPAN: usize = MRQC + 4;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_ASDE: u32 = 1 << 5;
const CTRL_FRCSPD: u32 = 1 << 11;
const CTRL_FRCDPX: u32 = 1 << 12;
const CTRL_RST: u32 = 1 << 26;
const STATUS_LU: u32 = 1 << 1;
const CTRL_EXT_IAME: u32 = 1 << 27;
const CTRL_EXT_RO_DIS: u32 = 1 << 17;
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
const EERD_START: u32 = 1 << 0;
const EERD_DONE: u32 = 1 << 1;
const RAH_AV: u32 = 1 << 31;

/// Interrupt causes (`ICR`, `IMS`, `IMC`).
pub const ICR_TXDW: u32 = 1 << 0;
pub const ICR_LSC: u32 = 1 << 2;
pub const ICR_RXDMT0: u32 = 1 << 4;
pub const ICR_RXO: u32 = 1 << 6;
pub const ICR_RXT0: u32 = 1 << 7;
/// What wakes the card's thread: a frame in, the ring running low or
/// over, the link.
pub const IMS_WAKE: u32 = ICR_RXT0 | ICR_RXO | ICR_RXDMT0 | ICR_LSC;

const RCTL_EN: u32 = 1 << 1;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 15 << 4;
const TCTL_COLD: u32 = 63 << 12;
const TCTL_RTLC: u32 = 1 << 24;
/// `TXDCTL`: write back every descriptor, and the kernel driver's burst
/// thresholds.
const TXDCTL_VALUE: u32 = 0x0101_0000 | (1 << 22) | (1 << 8) | 1;
const RFCTL_EXTEN: u32 = 1 << 15;
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;

/// Interrupt moderation (`ITR`, in 256 ns): at most one interrupt per
/// 50 us, the kernel driver's balanced setting.
pub const ITR_BALANCED: u32 = 195;

const RXD_STAT_DD: u8 = 1 << 0;
const RXD_STAT_EOP: u8 = 1 << 1;
const TXD_STAT_DD: u8 = 1 << 0;
const TXD_CMD_EOP: u8 = 1 << 0;
const TXD_CMD_IFCS: u8 = 1 << 1;
const TXD_CMD_RS: u8 = 1 << 3;

/// Descriptors in each ring: more than the slots that can be on it, so a
/// ring is never full. A ring's length is a multiple of 128 bytes.
pub const RX_RING: usize = 128;
pub const TX_RING: usize = 64;
const DESC: usize = 16;
const _: () = assert!(RX_RING > RX_BUFS && TX_RING > TX_BUFS);
const _: () = assert!((RX_RING * DESC).is_multiple_of(128) && (TX_RING * DESC).is_multiple_of(128));

// The first page of the DMA memory: the two rings, then the slot each
// descriptor was given. The window of slots follows, page-aligned.
const RX_RING_AT: usize = 0;
const TX_RING_AT: usize = RX_RING_AT + RX_RING * DESC;
const RX_SLOTS_AT: usize = TX_RING_AT + TX_RING * DESC;
const TX_SLOTS_AT: usize = RX_SLOTS_AT + RX_RING * 2;
const _: () = assert!(TX_SLOTS_AT + TX_RING * 2 <= PAGE);
/// Where the window starts in the DMA memory.
pub const WIN_AT: usize = PAGE;
pub const WIN_SIZE: usize = (RX_BUFS + TX_BUFS) * BUF_SIZE;
pub const DMA_SIZE: usize = WIN_AT + WIN_SIZE;
const _: () = assert!(WIN_SIZE.is_multiple_of(PAGE));

/// The card's registers, mapped.
#[derive(Clone, Copy)]
pub struct Regs(*mut u8);

// SAFETY: registers, read and written one volatile access at a time; which
// thread writes which is the sides' business (see the module's comment).
unsafe impl Send for Regs {}
unsafe impl Sync for Regs {}

impl Regs {
    /// # Safety
    /// `base` is the card's register window, [`REGS_SPAN`] bytes at least,
    /// mapped for as long as anything made from this lives.
    pub unsafe fn new(base: *mut u8) -> Regs {
        Regs(base)
    }

    pub fn read(&self, reg: usize) -> u32 {
        // SAFETY: inside the window (`new`), and aligned: every register
        // offset is a multiple of four.
        unsafe { ptr::read_volatile(self.0.add(reg).cast::<u32>()) }
    }

    pub fn write(&self, reg: usize, value: u32) {
        // SAFETY: as in `read`.
        unsafe { ptr::write_volatile(self.0.add(reg).cast::<u32>(), value) }
    }

    /// Mask every interrupt cause but `causes`, which are unmasked.
    pub fn ask(&self, causes: u32) {
        self.write(IMC, !causes);
        self.write(IMS, causes);
    }

    /// Read (and so clear) what the card raised.
    pub fn causes(&self) -> u32 {
        self.read(ICR)
    }

    /// Whether the link is up.
    pub fn link_up(&self) -> bool {
        self.read(STATUS) & STATUS_LU != 0
    }

    /// Moderate the card's interrupts: at most one per `itr` x 256 ns.
    pub fn moderate(&self, itr: u32) {
        self.write(ITR, itr);
    }
}

/// The DMA memory as both sides see it.
#[derive(Clone, Copy)]
struct Mem {
    ptr: *mut u8,
    daddr: u64,
}

impl Mem {
    /// Byte `at` of the first page.
    fn byte(&self, at: usize) -> *mut u8 {
        // SAFETY: every caller's `at` is inside the first page, and the
        // memory is DMA_SIZE bytes (`Giver::new`).
        unsafe { self.ptr.add(at) }
    }

    fn rx_desc(&self, i: usize) -> *mut u8 {
        self.byte(RX_RING_AT + i * DESC)
    }

    fn tx_desc(&self, i: usize) -> *mut u8 {
        self.byte(TX_RING_AT + i * DESC)
    }

    fn rx_slot(&self, i: usize) -> *mut u16 {
        self.byte(RX_SLOTS_AT + i * 2).cast()
    }

    fn tx_slot(&self, i: usize) -> *mut u16 {
        self.byte(TX_SLOTS_AT + i * 2).cast()
    }

    /// Where the card sees receive slot `slot`.
    fn rx_daddr(&self, slot: u16) -> u64 {
        self.daddr + (WIN_AT + slot as usize * BUF_SIZE) as u64
    }

    /// Where the card sees the frame in transmit slot `slot`.
    fn tx_daddr(&self, slot: u16) -> u64 {
        self.daddr + (WIN_AT + (RX_BUFS + slot as usize) * BUF_SIZE + TX_HEADROOM) as u64
    }
}

// The fields of a legacy descriptor, by byte (datasheet §7.1.4.1, §7.2.10.1).
const D_ADDR: usize = 0;
const D_LEN: usize = 8;
const RXD_STATUS: usize = 12;
const RXD_ERRORS: usize = 13;
const TXD_CMD: usize = 11;
const TXD_STATUS: usize = 12;

/// # Safety
/// `d` is a descriptor of one of the rings (16 bytes, 16-aligned).
unsafe fn put_u64(d: *mut u8, at: usize, v: u64) {
    unsafe { ptr::write_volatile(d.add(at).cast::<u64>(), v) }
}
unsafe fn put_u16(d: *mut u8, at: usize, v: u16) {
    unsafe { ptr::write_volatile(d.add(at).cast::<u16>(), v) }
}
unsafe fn put_u8(d: *mut u8, at: usize, v: u8) {
    unsafe { ptr::write_volatile(d.add(at), v) }
}
unsafe fn get_u16(d: *mut u8, at: usize) -> u16 {
    unsafe { ptr::read_volatile(d.add(at).cast::<u16>()) }
}
unsafe fn get_u8(d: *mut u8, at: usize) -> u8 {
    unsafe { ptr::read_volatile(d.add(at)) }
}

/// What a message from the stack asks, checked: nothing reaches the card
/// that is not one of its slots and a frame that fits it.
fn ask(m: &Msg) -> Option<Result<u16, (u16, u16)>> {
    match m.op {
        OP_RX if (m.slot as usize) < RX_BUFS => Some(Ok(m.slot as u16)),
        OP_TX
            if (m.slot as usize) < TX_BUFS
                && m.len as usize <= FRAME_MAX
                && m.len > 0
                && m.off as usize == TX_HEADROOM =>
        {
            Some(Err((m.slot as u16, m.len as u16)))
        }
        _ => None,
    }
}

/// The side that gives the card what the stack has for it: receive slots
/// back and frames to send. It writes the descriptors at the tails, and
/// the tails.
pub struct Giver {
    regs: Regs,
    mem: Mem,
    rx_tail: usize,
    tx_tail: usize,
    /// The transmit tail as the card was last told it, for the taker to
    /// know what is on the card.
    told: Arc<AtomicUsize>,
}

// SAFETY: its ends of the rings are its own, whichever thread has it.
unsafe impl Send for Giver {}

impl Giver {
    /// Give the card what `msgs` ask: receive slots read, frames to send.
    /// Both tails are written once, at the end. The message that means
    /// nothing, if there is one, ends it, with what came before it given.
    pub fn give(&mut self, msgs: &[Msg]) -> Result<(), Msg> {
        let (rx_tail, tx_tail) = (self.rx_tail, self.tx_tail);
        let mut bad = None;
        for m in msgs {
            match ask(m) {
                Some(Ok(slot)) => {
                    let i = self.rx_tail;
                    let d = self.mem.rx_desc(i);
                    // SAFETY: descriptor `i` and its table entry are this
                    // side's: the card has not been given it, and the taker
                    // is done with it (it is past the taker's end, and no
                    // more slots than the ring has are ever out).
                    unsafe {
                        self.mem.rx_slot(i).write_volatile(slot);
                        put_u64(d, D_ADDR, self.mem.rx_daddr(slot));
                        put_u16(d, D_LEN, 0);
                        put_u8(d, RXD_ERRORS, 0);
                        put_u8(d, RXD_STATUS, 0);
                    }
                    self.rx_tail = (i + 1) % RX_RING;
                }
                Some(Err((slot, len))) => {
                    let i = self.tx_tail;
                    let d = self.mem.tx_desc(i);
                    // SAFETY: as above, for the transmit ring.
                    unsafe {
                        self.mem.tx_slot(i).write_volatile(slot);
                        put_u64(d, D_ADDR, self.mem.tx_daddr(slot));
                        put_u16(d, D_LEN, len);
                        put_u8(d, TXD_STATUS, 0);
                        put_u8(d, TXD_CMD, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
                    }
                    self.tx_tail = (i + 1) % TX_RING;
                }
                None => {
                    bad = Some(*m);
                    break;
                }
            }
        }
        // The descriptors before the tail that names them.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        if self.rx_tail != rx_tail {
            self.regs.write(RDT, self.rx_tail as u32);
        }
        if self.tx_tail != tx_tail {
            self.regs.write(TDT, self.tx_tail as u32);
            self.told.store(self.tx_tail, Ordering::Release);
        }
        bad.map_or(Ok(()), Err)
    }
}

/// The side that takes what the card has done: frames received, transmit
/// slots done with. The card's thread.
pub struct Taker {
    regs: Regs,
    mem: Mem,
    rx_next: usize,
    tx_next: usize,
    /// The giver's [`Giver::told`].
    told: Arc<AtomicUsize>,
}

// SAFETY: its ends of the rings are its own, whichever thread has it.
unsafe impl Send for Taker {}

impl Taker {
    /// What the card has done since the last look, as the messages that
    /// tell the stack: how many. At most a ring's worth of slots, which is
    /// all there can be. A frame the card could not take whole -- in
    /// pieces, or with errors -- comes as an empty one, which the stack
    /// gives back like any other.
    pub fn collect(&mut self, out: &mut [Msg]) -> usize {
        assert!(out.len() >= RX_BUFS + TX_BUFS);
        let mut n = 0;
        loop {
            let i = self.rx_next;
            let d = self.mem.rx_desc(i);
            // SAFETY: descriptor `i` is the card's until it says done, and
            // then this side's until it is given again.
            let status = unsafe { get_u8(d, RXD_STATUS) };
            if status & RXD_STAT_DD == 0 {
                break;
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            // SAFETY: as above; the table entry was written before the
            // descriptor was given.
            let (len, errors, slot) = unsafe {
                (
                    get_u16(d, D_LEN),
                    get_u8(d, RXD_ERRORS),
                    self.mem.rx_slot(i).read_volatile(),
                )
            };
            // Not seen done again on the next lap, before it is given.
            // SAFETY: as above.
            unsafe { put_u8(d, RXD_STATUS, 0) };
            self.rx_next = (i + 1) % RX_RING;
            let whole = status & RXD_STAT_EOP != 0 && errors == 0;
            out[n] = Msg {
                op: OP_RX,
                slot: slot as u32,
                // A frame not to pass on goes to the stack empty, which
                // gives the slot back like any other.
                len: if whole && len as usize <= BUF_SIZE { len as u32 } else { 0 },
                off: 0,
            };
            n += 1;
        }
        let told = self.told.load(Ordering::Acquire);
        while self.tx_next != told {
            let i = self.tx_next;
            let d = self.mem.tx_desc(i);
            // SAFETY: as for the receive ring.
            let status = unsafe { get_u8(d, TXD_STATUS) };
            if status & TXD_STAT_DD == 0 {
                break;
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            // SAFETY: as above.
            let slot = unsafe {
                put_u8(d, TXD_STATUS, 0);
                self.mem.tx_slot(i).read_volatile()
            };
            self.tx_next = (i + 1) % TX_RING;
            out[n] = Msg {
                op: OP_TX,
                slot: slot as u32,
                len: 0,
                off: 0,
            };
            n += 1;
        }
        n
    }

    /// Before the card's thread sleeps: ask for the end of a send as well
    /// when frames are still on the card, so that their slots come back to
    /// a stack that may be out of them; otherwise only what [`IMS_WAKE`]
    /// says.
    pub fn sleep_with(&self) -> u32 {
        if self.tx_next != self.told.load(Ordering::Acquire) {
            IMS_WAKE | ICR_TXDW
        } else {
            IMS_WAKE
        }
    }

    pub fn regs(&self) -> Regs {
        self.regs
    }
}

/// A card the stack drives: claimed, brought up, every receive slot given.
pub struct Driven {
    pub regs: Regs,
    pub giver: Giver,
    pub taker: Taker,
    /// The card's interrupt to sleep on, or `None` where there is none and
    /// the card has to be looked at.
    pub interrupt: Option<Handle>,
    pub mac: [u8; 6],
    /// The DMA memory: the rings' page, then the window ([`WIN_AT`]).
    pub dma: Dma,
}

/// Claim e1000e `index` and bring it up. The driver before this one may
/// have died a moment ago, and the kernel is resetting the card: three
/// seconds of patience.
pub fn drive(index: usize) -> Result<Driven, crate::net::NoCard> {
    use crate::net::NoCard;
    let other = |e: String| NoCard::Other(e);
    let claim = match claim::claim(claim::KIND_E1000E, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => return Err(NoCard::NoGateway),
        Err(e) => {
            return Err(other(format!(
                "claim (boot with udriver=e1000e@N and an 82574 there): {e}"
            )))
        }
    };
    if !claim.pci || (claim.size as usize) < REGS_SPAN {
        return Err(other(format!(
            "not an e1000e's registers: {} bytes{}",
            claim.size,
            if claim.pci { "" } else { ", and a virtio transport" }
        )));
    }
    let base = claim.map_regs().map_err(other)?;
    let dma = Dma::new(claim.bti, DMA_SIZE, None).map_err(other)?;
    // SAFETY: the claimed card's window, mapped for good and REGS_SPAN
    // bytes long at least (checked above).
    let regs = unsafe { Regs::new(base) };
    // SAFETY: DMA_SIZE bytes the card sees at `daddr`, mapped for good,
    // zeroed, nobody else's.
    let (giver, taker, mac) = unsafe { bring_up(regs, dma.ptr, dma.daddr) }.map_err(other)?;
    Ok(Driven {
        regs,
        giver,
        taker,
        interrupt: claim.interrupt,
        mac,
        dma,
    })
}

fn pause_us(us: u64) {
    std::thread::sleep(std::time::Duration::from_micros(us));
}

/// The card's address: what the reset loaded into the first receive
/// address, or else the EEPROM's first three words.
fn mac_of(regs: Regs) -> [u8; 6] {
    let (low, high) = (regs.read(RAL0), regs.read(RAH0));
    let mut mac = [
        low as u8,
        (low >> 8) as u8,
        (low >> 16) as u8,
        (low >> 24) as u8,
        high as u8,
        (high >> 8) as u8,
    ];
    if high & RAH_AV == 0 || mac == [0; 6] {
        for word in 0..3u32 {
            regs.write(EERD, (word << 2) | EERD_START);
            let mut got = 0;
            for _ in 0..1000 {
                got = regs.read(EERD);
                if got & EERD_DONE != 0 {
                    break;
                }
                pause_us(10);
            }
            let w = (got >> 16) as u16;
            mac[word as usize * 2] = w as u8;
            mac[word as usize * 2 + 1] = (w >> 8) as u8;
        }
    }
    mac
}

/// Reset the card and bring it up: receive and transmit rings in the
/// memory at `dma` (the card sees it at `daddr`), every receive slot given,
/// receive on, the interrupts of [`IMS_WAKE`] asked for. The two sides and
/// the card's address.
///
/// # Safety
/// `regs` is the card's; `dma` is [`DMA_SIZE`] zeroed bytes the card sees
/// at `daddr`, mapped for as long as the sides live, and nobody else's.
pub unsafe fn bring_up(regs: Regs, dma: *mut u8, daddr: u64) -> Result<(Giver, Taker, [u8; 6]), String> {
    regs.write(IMC, u32::MAX);
    regs.write(RCTL, 0);
    regs.write(TCTL, TCTL_PSP);
    regs.write(CTRL, regs.read(CTRL) | CTRL_RST);
    let mut reset = false;
    for _ in 0..1000 {
        if regs.read(CTRL) & CTRL_RST == 0 {
            reset = true;
            break;
        }
        pause_us(100);
    }
    if !reset {
        return Err("the card did not come out of its reset".into());
    }
    regs.write(IMC, u32::MAX);
    let _ = regs.read(ICR);

    // Interrupts are not masked by their own delivery: the card's thread
    // reads them (`ICR`) and asks again.
    let ext = (regs.read(CTRL_EXT) | CTRL_EXT_RO_DIS | CTRL_EXT_DRV_LOAD) & !CTRL_EXT_IAME;
    regs.write(CTRL_EXT, ext);
    regs.write(IAM, 0);
    let ctrl = (regs.read(CTRL) & !(CTRL_FRCSPD | CTRL_FRCDPX)) | CTRL_SLU | CTRL_ASDE;
    regs.write(CTRL, ctrl);

    let mac = mac_of(regs);
    for i in 0..128 {
        regs.write(MTA + 4 * i, 0);
        regs.write(VFTA + 4 * i, 0);
    }
    regs.write(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    regs.write(RAH0, u16::from_le_bytes([mac[4], mac[5]]) as u32 | RAH_AV);

    let mem = Mem { ptr: dma, daddr };
    // Transmit (datasheet §4.6.10).
    let tx_ring = daddr + TX_RING_AT as u64;
    regs.write(TDBAL, tx_ring as u32);
    regs.write(TDBAH, (tx_ring >> 32) as u32);
    regs.write(TDLEN, (TX_RING * DESC) as u32);
    regs.write(TDH, 0);
    regs.write(TDT, 0);
    regs.write(TIDV, 0);
    regs.write(TADV, 0);
    regs.write(TIPG, 8 | (8 << 10) | (6 << 20));
    regs.write(TXDCTL, TXDCTL_VALUE);
    regs.write(TARC0, regs.read(TARC0) | 1);
    regs.write(TCTL, TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT | TCTL_COLD);

    // Receive (datasheet §4.6.7): legacy descriptors, 2 KiB buffers, the
    // CRC stripped, broadcast and every multicast (IPv6's neighbours and
    // groups) taken.
    regs.write(RDTR, 0);
    regs.write(RADV, 0);
    regs.moderate(ITR_BALANCED);
    let rx_ring = daddr + RX_RING_AT as u64;
    regs.write(RDBAL, rx_ring as u32);
    regs.write(RDBAH, (rx_ring >> 32) as u32);
    regs.write(RDLEN, (RX_RING * DESC) as u32);
    regs.write(RDH, 0);
    regs.write(RDT, 0);
    let rfctl = (regs.read(RFCTL) & !RFCTL_EXTEN) | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
    regs.write(RFCTL, rfctl);
    regs.write(MRQC, 0);
    regs.write(RXCSUM, 0);

    let (mut giver, taker) = sides(regs, mem);
    let all: Vec<Msg> = (0..RX_BUFS as u32)
        .map(|slot| Msg {
            op: OP_RX,
            slot,
            len: 0,
            off: 0,
        })
        .collect();
    giver.give(&all).expect("every receive slot is one");
    regs.write(RCTL, RCTL_EN | RCTL_BAM | RCTL_MPE | RCTL_SECRC);
    regs.ask(IMS_WAKE);
    Ok((giver, taker, mac))
}

/// The two sides of a card whose rings are new.
fn sides(regs: Regs, mem: Mem) -> (Giver, Taker) {
    let told = Arc::new(AtomicUsize::new(0));
    (
        Giver {
            regs,
            mem,
            rx_tail: 0,
            tx_tail: 0,
            told: told.clone(),
        },
        Taker {
            regs,
            mem,
            rx_next: 0,
            tx_next: 0,
            told,
        },
    )
}

/// Sleep until the card's interrupt, and read what it raised.
pub fn wait(interrupt: Handle, regs: Regs) -> Result<u32, Status> {
    claim::interrupt_wait(interrupt)?;
    Ok(regs.causes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain memory standing in for the card: registers and DMA memory.
    struct Bench {
        regs: Vec<u32>,
        dma: Vec<u64>,
    }

    impl Bench {
        fn new() -> Bench {
            Bench {
                regs: vec![0; REGS_SPAN / 4],
                dma: vec![0; DMA_SIZE / 8],
            }
        }

        fn regs(&mut self) -> Regs {
            unsafe { Regs::new(self.regs.as_mut_ptr().cast()) }
        }

        fn sides(&mut self) -> (Giver, Taker) {
            let mem = Mem {
                ptr: self.dma.as_mut_ptr().cast(),
                daddr: 0x10_0000,
            };
            sides(self.regs(), mem)
        }

        fn desc(&mut self, at: usize) -> *mut u8 {
            unsafe { self.dma.as_mut_ptr().cast::<u8>().add(at) }
        }

        /// Play the card: descriptor `i` of the receive ring got a frame.
        fn receive(&mut self, i: usize, len: u16, status: u8, errors: u8) {
            let d = self.desc(RX_RING_AT + i * DESC);
            unsafe {
                put_u16(d, D_LEN, len);
                put_u8(d, RXD_ERRORS, errors);
                put_u8(d, RXD_STATUS, status);
            }
        }

        fn sent(&mut self, i: usize) {
            let d = self.desc(TX_RING_AT + i * DESC);
            unsafe { put_u8(d, TXD_STATUS, TXD_STAT_DD) };
        }

        fn rx_addr(&mut self, i: usize) -> u64 {
            unsafe { ptr::read(self.desc(RX_RING_AT + i * DESC).cast::<u64>()) }
        }

        fn tx(&mut self, i: usize) -> (u64, u16, u8) {
            let d = self.desc(TX_RING_AT + i * DESC);
            unsafe { (ptr::read(d.cast::<u64>()), get_u16(d, D_LEN), get_u8(d, TXD_CMD)) }
        }

        fn reg(&self, reg: usize) -> u32 {
            self.regs[reg / 4]
        }
    }

    fn rx(slot: u32) -> Msg {
        Msg {
            op: OP_RX,
            slot,
            len: 0,
            off: 0,
        }
    }

    fn tx(slot: u32, len: u32) -> Msg {
        Msg {
            op: OP_TX,
            slot,
            len,
            off: TX_HEADROOM as u32,
        }
    }

    #[test]
    fn the_rings_and_their_tables_fit_the_first_page_and_the_window_follows() {
        assert!(TX_SLOTS_AT + TX_RING * 2 <= WIN_AT);
        assert_eq!(DMA_SIZE, PAGE + (RX_BUFS + TX_BUFS) * BUF_SIZE);
    }

    #[test]
    fn a_receive_slot_given_back_is_the_next_descriptor_whatever_its_number() {
        let mut bench = Bench::new();
        let (mut giver, mut taker) = bench.sides();
        giver.give(&[rx(5), rx(2)]).unwrap();
        assert_eq!(bench.reg(RDT), 2, "the tail is one past the last given");
        assert_eq!(bench.rx_addr(0), 0x10_0000 + (WIN_AT + 5 * BUF_SIZE) as u64);
        assert_eq!(bench.rx_addr(1), 0x10_0000 + (WIN_AT + 2 * BUF_SIZE) as u64);
        let mut out = [Msg::default(); RX_BUFS + TX_BUFS];
        assert_eq!(taker.collect(&mut out), 0, "nothing received yet");
        bench.receive(0, 60, RXD_STAT_DD | RXD_STAT_EOP, 0);
        assert_eq!(taker.collect(&mut out), 1);
        assert_eq!(out[0], Msg { op: OP_RX, slot: 5, len: 60, off: 0 });
        // Taken once: the next look does not see it again.
        assert_eq!(taker.collect(&mut out), 0);
        bench.receive(1, 1514, RXD_STAT_DD | RXD_STAT_EOP, 0);
        assert_eq!(taker.collect(&mut out), 1);
        assert_eq!(out[0], Msg { op: OP_RX, slot: 2, len: 1514, off: 0 });
    }

    #[test]
    fn a_frame_with_errors_or_in_pieces_goes_back_empty() {
        let mut bench = Bench::new();
        let (mut giver, mut taker) = bench.sides();
        giver.give(&[rx(0), rx(1)]).unwrap();
        bench.receive(0, 60, RXD_STAT_DD | RXD_STAT_EOP, 0x01);
        bench.receive(1, 2048, RXD_STAT_DD, 0);
        let mut out = [Msg::default(); RX_BUFS + TX_BUFS];
        assert_eq!(taker.collect(&mut out), 2);
        assert_eq!((out[0].slot, out[0].len), (0, 0));
        assert_eq!((out[1].slot, out[1].len), (1, 0));
    }

    #[test]
    fn the_receive_ring_goes_round_with_every_slot_on_it() {
        let mut bench = Bench::new();
        let (mut giver, mut taker) = bench.sides();
        let all: Vec<Msg> = (0..RX_BUFS as u32).map(rx).collect();
        giver.give(&all).unwrap();
        let mut out = [Msg::default(); RX_BUFS + TX_BUFS];
        // Ten laps of the ring, each frame's slot given back at once.
        for k in 0..RX_RING * 10 {
            bench.receive(k % RX_RING, 100, RXD_STAT_DD | RXD_STAT_EOP, 0);
            assert_eq!(taker.collect(&mut out), 1, "frame {k}");
            assert_eq!(out[0].slot as usize, k % RX_BUFS);
            giver.give(&out[..1]).unwrap();
            assert_eq!(bench.reg(RDT) as usize, (RX_BUFS + k + 1) % RX_RING);
        }
    }

    #[test]
    fn a_frame_to_send_names_its_slot_past_the_headroom_and_comes_back_once_sent() {
        let mut bench = Bench::new();
        let (mut giver, mut taker) = bench.sides();
        assert_eq!(taker.sleep_with(), IMS_WAKE, "nothing on the card");
        giver.give(&[tx(3, 42), tx(0, 1514)]).unwrap();
        assert_eq!(bench.reg(TDT), 2);
        let (addr, len, cmd) = bench.tx(0);
        assert_eq!(addr, 0x10_0000 + (WIN_AT + (RX_BUFS + 3) * BUF_SIZE + TX_HEADROOM) as u64);
        assert_eq!((len, cmd), (42, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS));
        assert_eq!(taker.sleep_with(), IMS_WAKE | ICR_TXDW, "frames on the card");
        let mut out = [Msg::default(); RX_BUFS + TX_BUFS];
        assert_eq!(taker.collect(&mut out), 0);
        bench.sent(0);
        assert_eq!(taker.collect(&mut out), 1);
        assert_eq!(out[0], Msg { op: OP_TX, slot: 3, len: 0, off: 0 });
        bench.sent(1);
        assert_eq!(taker.collect(&mut out), 1);
        assert_eq!(out[0].slot, 0);
        assert_eq!(taker.sleep_with(), IMS_WAKE, "nothing left on the card");
        assert_eq!(taker.collect(&mut out), 0);
    }

    #[test]
    fn a_message_that_means_nothing_stops_the_turn_after_what_came_before() {
        let mut bench = Bench::new();
        let (mut giver, _) = bench.sides();
        let bad = [
            Msg { op: OP_RX, slot: RX_BUFS as u32, len: 0, off: 0 },
            Msg { op: OP_TX, slot: TX_BUFS as u32, len: 60, off: TX_HEADROOM as u32 },
            tx(0, FRAME_MAX as u32 + 1),
            tx(0, 0),
            Msg { op: OP_TX, slot: 0, len: 60, off: 0 },
            Msg { op: 9, slot: 0, len: 0, off: 0 },
        ];
        for m in bad {
            assert_eq!(giver.give(&[rx(1), m, rx(2)]), Err(m), "{m:?}");
        }
        // Each time only the slot before it was given.
        assert_eq!(bench.reg(RDT) as usize, bad.len());
        assert_eq!(bench.reg(TDT), 0);
    }
}
