//! An xHCI USB controller the kernel left to userspace (`udriver=xhci@N`),
//! and the HID devices on its root ports: keyboards and pointers, which
//! `udrv-xhci` serves as `/dev/input/eventN` (`crate::usbhid` turns their
//! reports into events).
//!
//! One command ring, one event ring on interrupter 0, a ring per endpoint.
//! A device is enumerated as the spec has it (xHCI 1.2 §4.3): a port reset
//! for USB 2, a slot, an address, its descriptors, its configuration, and
//! one interrupt IN endpoint per HID interface, kept full of transfers.
//! What a transfer brings back is handed out by [`Xhci::service`].
//!
//! A USB disk (mass storage, bulk-only, SCSI) on a port is driven too: its
//! first LUN, through a bulk IN and a bulk OUT endpoint, which
//! [`Xhci::disk_io`] reads and writes for whoever serves it to the kernel.
//!
//! A USB 2 hub is driven as well: its ports powered, reset and watched
//! through its status change endpoint, and what is on them enumerated as
//! on a root port, five tiers deep at most.
//!
//! Not here yet: USB 3 hubs, isochronous endpoints, disks whose blocks are not
//! 512 bytes, and an interface that is neither a boot keyboard, a pointer
//! with an X nor a disk.

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{fence, Ordering};
use std::time::Duration;

use crate::claim::Dma;
use crate::now_ns;
use crate::usbhid::{self, Keyboard, Pointer};

/// Bytes of DMA memory the driver takes, once: rings, contexts, buffers.
pub const DMA_SIZE: usize = 512 * 1024;
/// TRBs in each ring, the link TRB at its end included.
const RING_LEN: usize = 32;
const EVENT_LEN: usize = 128;
/// Transfers kept queued on each interrupt endpoint.
const IN_FLIGHT: usize = 8;
/// Slots asked for: more devices than a machine has on its root ports.
const MAX_SLOTS: u32 = 32;
/// How long a command or a control transfer may take.
const DEADLINE_NS: u64 = 1_000_000_000;

// Capability registers.
const CAPLENGTH: usize = 0x00;
const HCSPARAMS1: usize = 0x04;
const HCSPARAMS2: usize = 0x08;
const HCCPARAMS1: usize = 0x10;
const DBOFF: usize = 0x14;
const RTSOFF: usize = 0x18;
// Operational registers.
const USBCMD: usize = 0x00;
const USBSTS: usize = 0x04;
const CRCR: usize = 0x18;
const DCBAAP: usize = 0x30;
const CONFIG: usize = 0x38;
const PORTSC: usize = 0x400;
const CMD_RUN: u32 = 1;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const STS_HALTED: u32 = 1;
const STS_EINT: u32 = 1 << 3;
const STS_CNR: u32 = 1 << 11;
// Interrupter 0, in the runtime registers.
const IR0: usize = 0x20;
const IMAN: usize = 0x00;
const IMOD: usize = 0x04;
const ERSTSZ: usize = 0x08;
const ERSTBA: usize = 0x10;
const ERDP: usize = 0x18;
const IMAN_IP: u32 = 1;
const IMAN_IE: u32 = 1 << 1;
const ERDP_EHB: u64 = 1 << 3;
// PORTSC.
const PORT_CCS: u32 = 1;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PP: u32 = 1 << 9;
const PORT_PRC: u32 = 1 << 21;
/// The bits that a write of 1 clears (the change bits) or that a write of
/// 1 acts on (PED disables the port, PR and WPR reset it): never written
/// back from a sample.
const PORT_CHANGES: u32 = 0x7f << 17;
const PORT_KEEP_OFF: u32 = PORT_CHANGES | PORT_PED | PORT_PR | (1 << 31);

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
const TRB_RESET_EP: u32 = 14;
const TRB_STOP_EP: u32 = 15;
const TRB_SET_TR_DEQUEUE: u32 = 16;
const TRB_NOOP: u32 = 8;
const EV_TRANSFER: u32 = 32;
const EV_COMMAND: u32 = 33;
const EV_PORT: u32 = 34;
const TRB_CYCLE: u32 = 1;
const TRB_TOGGLE: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
const TRB_CHAIN: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;
// Completion codes.
const CC_SUCCESS: u32 = 1;
const CC_STALL: u32 = 6;
const CC_SHORT: u32 = 13;

// Descriptors and requests.
const DESC_DEVICE: u16 = 1;
const DESC_CONFIG: u16 = 2;
const DESC_STRING: u16 = 3;
const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;
const DESC_HID: u8 = 0x21;
const DESC_REPORT: u16 = 0x22;
const REQ_GET_DESCRIPTOR: u8 = 6;
const REQ_SET_CONFIGURATION: u8 = 9;
const HID_SET_REPORT: u8 = 0x09;
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;
const CLASS_HID: u8 = 3;
const CLASS_HUB: u8 = 9;
const DESC_HUB: u16 = 0x29;
const REQ_GET_STATUS: u8 = 0;
const REQ_CLEAR_FEATURE: u8 = 1;
const REQ_SET_FEATURE: u8 = 3;
// Hub port features and status bits (USB 2.0 §11.24.2).
const HUB_PORT_RESET: u16 = 4;
const HUB_PORT_POWER: u16 = 8;
const HUB_C_PORT_CONNECTION: u16 = 16;
const HUB_PORT_CHANGES: [u16; 5] = [16, 17, 18, 19, 20];
const HUB_STATUS_CONNECTED: u16 = 1;
const HUB_STATUS_LOW_SPEED: u16 = 1 << 9;
const HUB_STATUS_HIGH_SPEED: u16 = 1 << 10;
const HUB_CHANGE_CONNECTION: u16 = 1;
const HUB_CHANGE_RESET: u16 = 1 << 4;
/// How many tiers of hubs a route string names.
const MAX_TIERS: u32 = 5;
const CLASS_STORAGE: u8 = 8;
const STORAGE_SCSI: u8 = 6;
const STORAGE_BULK_ONLY: u8 = 0x50;
/// A bulk-only mass storage reset, and the request for the last LUN.
const BOT_RESET: u8 = 0xff;
const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;
/// What one TRB moves at most, and the boundary it may not cross.
const TRB_MAX: u64 = 64 * 1024;
/// The sector a disk is served in.
pub const SECTOR: usize = 512;
const LANG_EN_US: u16 = 0x0409;

// Port speeds (PORTSC bits 10-13, the default protocol speed IDs).
const SPEED_FULL: u32 = 1;
const SPEED_LOW: u32 = 2;
const SPEED_HIGH: u32 = 3;

/// A TRB: four little-endian words.
pub type Trb = [u32; 4];

fn trb_type(t: &Trb) -> u32 {
    t[3] >> 10 & 0x3f
}

/// The registers, from the start of the claimed BAR.
struct Regs(*mut u8);

impl Regs {
    fn r8(&self, at: usize) -> u8 {
        // SAFETY: a register inside the claimed BAR, mapped for good.
        unsafe { ptr::read_volatile(self.0.add(at)) }
    }
    fn r32(&self, at: usize) -> u32 {
        // SAFETY: as above.
        unsafe { ptr::read_volatile(self.0.add(at).cast::<u32>()) }
    }
    fn w32(&self, at: usize, v: u32) {
        // SAFETY: as above.
        unsafe { ptr::write_volatile(self.0.add(at).cast::<u32>(), v) }
    }
    fn w64(&self, at: usize, v: u64) {
        self.w32(at, v as u32);
        self.w32(at + 4, (v >> 32) as u32);
    }
}

/// DMA memory handed out from one block, for the life of the process.
struct Mem {
    ptr: *mut u8,
    daddr: u64,
    len: usize,
    next: usize,
}

impl Mem {
    /// `len` zeroed bytes aligned to `align` (which also keeps them inside
    /// one page when they fit one, as the spec wants of rings and contexts).
    fn take(&mut self, len: usize, align: usize) -> Result<(*mut u8, u64), String> {
        let at = self.next.next_multiple_of(align);
        if at + len > self.len {
            return Err("out of DMA memory".into());
        }
        self.next = at + len;
        // SAFETY: inside the block, which is this process's for good.
        let p = unsafe { self.ptr.add(at) };
        // SAFETY: as above; nothing has these bytes yet.
        unsafe { p.write_bytes(0, len) };
        Ok((p, self.daddr + at as u64))
    }
}

/// Where the next TRB of a producer ring goes. The last entry is the link
/// back to the first, and the cycle bit flips each time round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Producer {
    pub len: usize,
    pub enq: usize,
    pub cycle: bool,
}

impl Producer {
    pub fn new(len: usize) -> Producer {
        Producer {
            len,
            enq: 0,
            cycle: true,
        }
    }

    /// Take the entry for the next TRB: its index, its cycle bit, and
    /// whether the link after it must now be handed over (with the cycle
    /// it had) because the ring wraps.
    pub fn advance(&mut self) -> (usize, bool, Option<bool>) {
        let (at, cycle) = (self.enq, self.cycle);
        self.enq += 1;
        let mut link = None;
        if self.enq == self.len - 1 {
            link = Some(self.cycle);
            self.enq = 0;
            self.cycle = !self.cycle;
        }
        (at, cycle, link)
    }
}

/// Where the consumer of the event ring is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Consumer {
    pub len: usize,
    pub deq: usize,
    pub cycle: bool,
}

impl Consumer {
    /// Whether an entry with cycle bit `c` is the controller's new one;
    /// then step past it.
    pub fn take(&mut self, c: bool) -> bool {
        if c != self.cycle {
            return false;
        }
        self.deq += 1;
        if self.deq == self.len {
            self.deq = 0;
            self.cycle = !self.cycle;
        }
        true
    }
}

/// A ring the driver fills: the command ring or a transfer ring.
struct Ring {
    ptr: *mut Trb,
    daddr: u64,
    at: Producer,
}

impl Ring {
    fn new(mem: &mut Mem, len: usize) -> Result<Ring, String> {
        let (p, daddr) = mem.take(len * 16, 64)?;
        Ok(Ring {
            ptr: p.cast(),
            daddr,
            at: Producer::new(len),
        })
    }

    fn write(&self, i: usize, t: Trb) {
        // SAFETY: `i` is an entry of the ring. The word with the cycle bit
        // goes last: with it the TRB is the controller's.
        unsafe {
            let e = self.ptr.add(i).cast::<u32>();
            for (w, v) in t.iter().enumerate().take(3) {
                ptr::write_volatile(e.add(w), *v);
            }
            fence(Ordering::SeqCst);
            ptr::write_volatile(e.add(3), t[3]);
        }
    }

    /// Queue `t` (its cycle bit is set here): where the controller sees it.
    fn push(&mut self, mut t: Trb) -> u64 {
        let (i, cycle, link) = self.at.advance();
        t[3] = t[3] & !TRB_CYCLE | cycle as u32;
        self.write(i, t);
        if let Some(c) = link {
            let back = [
                self.daddr as u32,
                (self.daddr >> 32) as u32,
                0,
                TRB_LINK << 10 | TRB_TOGGLE | c as u32,
            ];
            self.write(self.at.len - 1, back);
        }
        self.daddr + i as u64 * 16
    }

    /// Make room for a TD of `n` TRBs before the link: a TD that went over
    /// it would need the link to chain it. No-ops fill the rest of the lap.
    fn room(&mut self, n: usize) {
        if n < self.at.len - 1 {
            while self.at.len - 1 - self.at.enq < n {
                self.push([0, 0, 0, TRB_NOOP << 10]);
            }
        }
    }

    /// The dequeue pointer, with its cycle state, a context starts it with.
    fn dequeue(&self) -> u64 {
        (self.daddr + self.at.enq as u64 * 16) | self.at.cycle as u64
    }
}

/// The setup stage of a control request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    fn get_descriptor(kind: u16, index: u8, lang: u16, length: u16) -> Setup {
        Setup {
            request_type: 0x80,
            request: REQ_GET_DESCRIPTOR,
            value: kind << 8 | index as u16,
            index: lang,
            length,
        }
    }

    fn device_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }

    /// The setup TRB of the request: its eight bytes in place of a
    /// pointer, and which data stage follows.
    pub fn trb(&self) -> Trb {
        let trt = match (self.length, self.device_in()) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        [
            self.request_type as u32 | (self.request as u32) << 8 | (self.value as u32) << 16,
            self.index as u32 | (self.length as u32) << 16,
            8,
            TRB_SETUP << 10 | TRB_IDT | trt << 16,
        ]
    }
}

/// The interval an interrupt endpoint's context takes: an exponent of
/// 125 µs. A full- or low-speed `bInterval` is in frames of 1 ms; a high-
/// or super-speed one is already an exponent, plus one (xHCI 1.2 §6.2.3.6).
pub fn interval(speed: u32, b_interval: u8) -> u32 {
    match speed {
        SPEED_FULL | SPEED_LOW => {
            let frames = (b_interval.max(1) as u32) * 8;
            (31 - frames.leading_zeros()).clamp(3, 10)
        }
        _ => (b_interval.max(1) as u32 - 1).min(15),
    }
}

/// The largest packet endpoint 0 takes before its descriptor says.
fn first_ep0_packet(speed: u32) -> u32 {
    match speed {
        SPEED_LOW => 8,
        SPEED_FULL | SPEED_HIGH => 64,
        _ => 512,
    }
}

/// A HID interface in a configuration descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidIface {
    pub number: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// Its interrupt IN endpoint: address, largest packet, `bInterval`.
    pub endpoint: (u8, u16, u8),
    /// How long its report descriptor is.
    pub report_len: u16,
}

/// The configuration's value, and its HID interfaces that have an
/// interrupt IN endpoint.
pub fn hid_interfaces(config: &[u8]) -> (u8, Vec<HidIface>) {
    let value = config.get(5).copied().unwrap_or(1);
    let mut out = Vec::new();
    let mut cur: Option<HidIface> = None;
    let mut i = 0;
    while i + 2 <= config.len() {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => {
                out.extend(cur.take().filter(|c| c.endpoint.0 != 0));
                if d[5] == CLASS_HID {
                    cur = Some(HidIface {
                        number: d[2],
                        subclass: d[6],
                        protocol: d[7],
                        endpoint: (0, 0, 0),
                        report_len: 0,
                    });
                }
            }
            DESC_HID if len >= 9 => {
                if let Some(c) = cur.as_mut() {
                    // The first class descriptor it names is the report's.
                    if d[6] as u16 == DESC_REPORT {
                        c.report_len = u16::from_le_bytes([d[7], d[8]]);
                    }
                }
            }
            DESC_ENDPOINT if len >= 7 => {
                if let Some(c) = cur.as_mut() {
                    let interrupt_in = d[3] & 3 == 3 && d[2] & 0x80 != 0;
                    if interrupt_in && c.endpoint.0 == 0 {
                        let mps = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                        c.endpoint = (d[2], mps, d[6]);
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    out.extend(cur.take().filter(|c| c.endpoint.0 != 0));
    (value, out)
}

/// A disk's interface in a configuration descriptor: bulk-only SCSI, with
/// a bulk IN and a bulk OUT endpoint (address and largest packet each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIface {
    pub number: u8,
    pub bulk_in: (u8, u16),
    pub bulk_out: (u8, u16),
}

/// The first interrupt IN endpoint of an interface of `class`: address,
/// largest packet and `bInterval`.
pub fn class_interrupt_in(config: &[u8], class: u8) -> Option<(u8, u16, u8)> {
    let mut inside = false;
    let mut i = 0;
    while i + 2 <= config.len() {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => inside = d[5] == class,
            DESC_ENDPOINT if len >= 7 && inside && d[3] & 3 == 3 && d[2] & 0x80 != 0 => {
                return Some((d[2], u16::from_le_bytes([d[4], d[5]]) & 0x7ff, d[6]));
            }
            _ => {}
        }
        i += len;
    }
    None
}

/// The speed of what is on a hub's port, from the port's status.
pub fn hub_port_speed(status: u16) -> u32 {
    if status & HUB_STATUS_LOW_SPEED != 0 {
        SPEED_LOW
    } else if status & HUB_STATUS_HIGH_SPEED != 0 {
        SPEED_HIGH
    } else {
        SPEED_FULL
    }
}

/// The route string of port `port` of a hub whose own route is `route`,
/// `tier` hubs from the root: four bits a tier. `None` past the last tier.
pub fn child_route(route: u32, tier: u32, port: u8) -> Option<u32> {
    (tier < MAX_TIERS).then(|| route | (port.min(15) as u32) << (4 * tier))
}

/// The ports of a hub's status change bitmap that changed (bit 0 is the
/// hub itself).
pub fn changed_ports(bitmap: &[u8], ports: u8) -> Vec<u8> {
    (1..=ports)
        .filter(|&p| {
            bitmap
                .get(p as usize / 8)
                .is_some_and(|b| b >> (p % 8) & 1 != 0)
        })
        .collect()
}

/// The first disk interface of a configuration.
pub fn disk_interface(config: &[u8]) -> Option<DiskIface> {
    let mut cur: Option<DiskIface> = None;
    let mut i = 0;
    while i + 2 <= config.len() {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            break;
        }
        let d = &config[i..i + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => {
                if let Some(c) = cur.filter(|c| c.bulk_in.0 != 0 && c.bulk_out.0 != 0) {
                    return Some(c);
                }
                cur = (d[5] == CLASS_STORAGE && d[6] == STORAGE_SCSI && d[7] == STORAGE_BULK_ONLY)
                    .then_some(DiskIface {
                        number: d[2],
                        bulk_in: (0, 0),
                        bulk_out: (0, 0),
                    });
            }
            DESC_ENDPOINT if len >= 7 => {
                if let Some(c) = cur.as_mut() {
                    let mps = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                    if d[3] & 3 == 2 {
                        if d[2] & 0x80 != 0 && c.bulk_in.0 == 0 {
                            c.bulk_in = (d[2], mps);
                        } else if d[2] & 0x80 == 0 && c.bulk_out.0 == 0 {
                            c.bulk_out = (d[2], mps);
                        }
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    cur.filter(|c| c.bulk_in.0 != 0 && c.bulk_out.0 != 0)
}

/// A command block wrapper: `tag`, how much data, which way, and the SCSI
/// command (LUN 0).
pub fn cbw(tag: u32, len: u32, device_in: bool, cdb: &[u8]) -> [u8; CBW_LEN] {
    let mut b = [0u8; CBW_LEN];
    b[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&len.to_le_bytes());
    b[12] = if device_in { 0x80 } else { 0 };
    b[14] = cdb.len().min(16) as u8;
    b[15..15 + cdb.len().min(16)].copy_from_slice(&cdb[..cdb.len().min(16)]);
    b
}

/// A command status wrapper's status for `tag`: 0 passed, 1 failed, 2 a
/// phase error; `None` when it is not one, or not that command's.
pub fn csw_status(b: &[u8], tag: u32) -> Option<u8> {
    if b.len() < CSW_LEN
        || u32::from_le_bytes([b[0], b[1], b[2], b[3]]) != CSW_SIGNATURE
        || u32::from_le_bytes([b[4], b[5], b[6], b[7]]) != tag
    {
        return None;
    }
    Some(b[12])
}

/// The SCSI command that reads or writes `blocks` blocks from `lba`: the
/// 10-byte one while the LBA fits it, else the 16-byte one.
pub fn scsi_rw(write: bool, lba: u64, blocks: u32) -> Vec<u8> {
    if lba + blocks as u64 <= u32::MAX as u64 && blocks <= u16::MAX as u32 {
        let mut c = vec![if write { 0x2a } else { 0x28 }, 0];
        c.extend((lba as u32).to_be_bytes());
        c.push(0);
        c.extend((blocks as u16).to_be_bytes());
        c.push(0);
        c
    } else {
        let mut c = vec![if write { 0x8a } else { 0x88 }, 0];
        c.extend(lba.to_be_bytes());
        c.extend(blocks.to_be_bytes());
        c.extend([0, 0]);
        c
    }
}

/// The TRBs of a transfer of `len` bytes from `daddr`: no piece crosses a
/// 64 KiB boundary. Each piece's address and length.
pub fn pieces(daddr: u64, len: usize) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    let (mut at, end) = (daddr, daddr + len as u64);
    while at < end {
        let next = ((at / TRB_MAX) + 1) * TRB_MAX;
        let n = next.min(end) - at;
        out.push((at, n as u32));
        at += n;
    }
    out
}

/// A string descriptor's text.
pub fn string_text(d: &[u8]) -> String {
    let n = (d.first().copied().unwrap_or(0) as usize).min(d.len());
    let units: Vec<u16> = d
        .get(2..n)
        .unwrap_or(&[])
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// What an interface's reports are.
pub enum Role {
    Keyboard(Keyboard),
    Pointer(Pointer),
}

/// An interrupt IN endpoint kept full of transfers.
struct InEndpoint {
    dci: u8,
    ring: Ring,
    mps: usize,
    /// Each queued TRB's address, and the buffer it fills.
    pending: Vec<(u64, *mut u8, u64)>,
    stalled: bool,
}

/// A HID interface being driven: what the bin serves as a node.
pub struct Hid {
    pub port: u8,
    /// Where it is: its root port, then each hub port, as `1.3`.
    pub path: String,
    pub slot: u8,
    pub iface: u8,
    pub name: String,
    pub id: [u16; 3],
    pub role: Role,
    ep: InEndpoint,
}

/// A bulk endpoint of a disk.
struct BulkEndpoint {
    dci: u8,
    ring: Ring,
}

/// A USB disk being driven: its first LUN.
pub struct Disk {
    pub port: u8,
    pub path: String,
    /// What tells it from every other disk on the controller, whichever
    /// driver drives it: its root port and its route.
    pub unit: usize,
    pub slot: u8,
    pub iface: u8,
    pub name: String,
    /// Its size in sectors of [`SECTOR`] bytes.
    pub sectors: u64,
    bulk_in: BulkEndpoint,
    bulk_out: BulkEndpoint,
    tag: u32,
    /// Where its command blocks and status words go.
    cbw: (*mut u8, u64),
    csw: (*mut u8, u64),
}

/// A device on a root port.
struct Device {
    /// Its root port.
    port: u8,
    slot: u8,
    path: String,
    route: u32,
    /// How many hubs it is behind.
    tier: u32,
    /// The hub and the hub's port it is on, when it is not on a root port.
    parent: Option<(u8, u8)>,
    /// The transaction translator a low- or full-speed device behind a
    /// high-speed hub goes through: the third word of its slot context.
    tt: u32,
    ep0: Ring,
    /// A page for control transfers' data.
    buf: (*mut u8, u64),
    input_ctx: (*mut u8, u64),
    out_ctx: (*mut u8, u64),
    speed: u32,
    mps0: u32,
}

/// What [`Xhci::service`] found: reports, by the index of their HID, and
/// ports whose device came or went.
#[derive(Default)]
pub struct Found {
    pub reports: Vec<(usize, Vec<u8>)>,
    pub ports: Vec<u8>,
    /// Hubs whose ports changed: the hub, and its status change bitmap.
    pub hubs: Vec<(usize, Vec<u8>)>,
}

/// HIDs a change of the devices added and took away.
#[derive(Default)]
pub struct Changes {
    pub added: Vec<usize>,
    pub gone: Vec<usize>,
}

/// A hub being driven.
struct Hub {
    slot: u8,
    ports: u8,
    ep: InEndpoint,
}

/// The controller.
pub struct Xhci {
    regs: Regs,
    op: usize,
    rt: usize,
    db: usize,
    ctx: usize,
    ports: u8,
    usb3: Vec<bool>,
    mem: Mem,
    dcbaa: *mut u64,
    cmd: Ring,
    events: *const Trb,
    events_daddr: u64,
    ev: Consumer,
    stash: VecDeque<Trb>,
    devices: Vec<Device>,
    /// Every HID driven, by the index [`Found`] gives; `None` once its
    /// device is gone.
    pub hids: Vec<Option<Hid>>,
    /// Every disk driven, by its index; `None` once its device is gone.
    pub disks: Vec<Option<Disk>>,
    hubs: Vec<Option<Hub>>,
    /// Disks added since [`Xhci::new_disks`] last looked.
    fresh_disks: Vec<usize>,
    /// Whether its interrupter interrupts.
    interrupts: bool,
}

// SAFETY: the pointers are to the BAR and the DMA block, this process's for
// good; the controller is behind a lock wherever it is shared.
unsafe impl Send for Xhci {}

fn wait(what: &str, mut ok: impl FnMut() -> bool) -> Result<(), String> {
    let start = now_ns();
    while !ok() {
        if now_ns() - start > DEADLINE_NS {
            return Err(format!("the controller never {what}"));
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    Ok(())
}

impl Xhci {
    /// Take the controller from the firmware, reset it and start it, with
    /// its interrupter's interrupt on when `interrupts`.
    ///
    /// # Safety
    /// `regs` is the claimed BAR, mapped for good; `dma` is [`DMA_SIZE`]
    /// bytes the controller sees at `dma.daddr`, mapped for good and
    /// touched by no one else.
    pub unsafe fn new(regs: *mut u8, dma: &Dma, interrupts: bool) -> Result<Xhci, String> {
        let regs = Regs(regs);
        let op = regs.r8(CAPLENGTH) as usize;
        let hcs1 = regs.r32(HCSPARAMS1);
        let hcs2 = regs.r32(HCSPARAMS2);
        let hcc1 = regs.r32(HCCPARAMS1);
        let db = (regs.r32(DBOFF) & !3) as usize;
        let rt = (regs.r32(RTSOFF) & !0x1f) as usize;
        let ports = (hcs1 >> 24) as u8;
        let slots = (hcs1 & 0xff).min(MAX_SLOTS);
        let ctx = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
        let mut usb3 = vec![false; ports as usize + 1];
        // The extended capabilities: the firmware's hold on the controller
        // to take, and which ports are USB 3's.
        let mut at = ((hcc1 >> 16) << 2) as usize;
        let mut walked = 0;
        while at != 0 && walked < 64 {
            walked += 1;
            let cap = regs.r32(at);
            match cap & 0xff {
                1 => {
                    // USB legacy support: OS-owned, and wait for the BIOS
                    // to let go; with its SMIs off whatever it does.
                    regs.w32(at, cap | 1 << 24);
                    let _ = wait("was let go by the firmware", || regs.r32(at) & 1 << 16 == 0);
                    regs.w32(at + 4, regs.r32(at + 4) & 0xffff_0000 & !(0x7 << 13));
                }
                2 => {
                    let w2 = regs.r32(at + 8);
                    let (first, count) = ((w2 & 0xff) as usize, (w2 >> 8 & 0xff) as usize);
                    if cap >> 24 == 3 {
                        for p in first..(first + count).min(usb3.len()) {
                            usb3[p] = true;
                        }
                    }
                }
                _ => {}
            }
            let next = (cap >> 8 & 0xff) as usize;
            at = if next == 0 { 0 } else { at + (next << 2) };
        }

        regs.w32(op + USBCMD, regs.r32(op + USBCMD) & !(CMD_RUN | CMD_INTE));
        wait("halted", || regs.r32(op + USBSTS) & STS_HALTED != 0)?;
        regs.w32(op + USBCMD, CMD_RESET);
        wait("came out of reset", || {
            regs.r32(op + USBCMD) & CMD_RESET == 0 && regs.r32(op + USBSTS) & STS_CNR == 0
        })?;

        let mut mem = Mem {
            ptr: dma.ptr,
            daddr: dma.daddr,
            len: dma.len,
            next: 0,
        };
        regs.w32(op + CONFIG, slots);
        let (dcbaa, dcbaa_daddr) = mem.take(8 * (MAX_SLOTS as usize + 1), 64)?;
        let dcbaa = dcbaa.cast::<u64>();
        let scratch = ((hcs2 >> 27) & 0x1f | ((hcs2 >> 21) & 0x1f) << 5) as usize;
        if scratch > 0 {
            let (array, array_daddr) = mem.take(8 * scratch, 64)?;
            for i in 0..scratch {
                let (_, page) = mem.take(4096, 4096)?;
                // SAFETY: entry `i` of the array just taken.
                unsafe { array.cast::<u64>().add(i).write_volatile(page) };
            }
            // SAFETY: entry 0 of the DCBAA, the scratchpad's.
            unsafe { dcbaa.write_volatile(array_daddr) };
        }
        regs.w64(op + DCBAAP, dcbaa_daddr);
        let cmd = Ring::new(&mut mem, RING_LEN)?;
        regs.w64(op + CRCR, cmd.daddr | 1);
        let (events, events_daddr) = mem.take(EVENT_LEN * 16, 64)?;
        let (erst, erst_daddr) = mem.take(16, 64)?;
        // SAFETY: the 16 bytes just taken: the one segment's base and size.
        unsafe {
            erst.cast::<u64>().write_volatile(events_daddr);
            erst.add(8).cast::<u32>().write_volatile(EVENT_LEN as u32);
        }
        let ir = rt + IR0;
        regs.w32(ir + ERSTSZ, 1);
        regs.w64(ir + ERDP, events_daddr);
        regs.w64(ir + ERSTBA, erst_daddr);
        // Events at most every 40 µs, so a burst is one interrupt.
        regs.w32(ir + IMOD, 160);
        regs.w32(ir + IMAN, IMAN_IP | if interrupts { IMAN_IE } else { 0 });
        let inte = if interrupts { CMD_INTE } else { 0 };
        regs.w32(op + USBCMD, CMD_RUN | inte);
        wait("ran", || regs.r32(op + USBSTS) & STS_HALTED == 0)?;

        let x = Xhci {
            regs,
            op,
            rt,
            db,
            ctx,
            ports,
            usb3,
            mem,
            dcbaa,
            cmd,
            events: events.cast(),
            events_daddr,
            ev: Consumer {
                len: EVENT_LEN,
                deq: 0,
                cycle: true,
            },
            stash: VecDeque::new(),
            devices: Vec::new(),
            hids: Vec::new(),
            disks: Vec::new(),
            hubs: Vec::new(),
            fresh_disks: Vec::new(),
            interrupts,
        };
        for p in 1..=ports {
            let sc = x.portsc(p);
            if sc & PORT_PP == 0 {
                x.set_portsc(p, sc, PORT_PP);
            }
        }
        Ok(x)
    }

    /// Hold its interrupt back while `quiet`, as a disk's commands are
    /// waited for by looking: each answer would only wake the event
    /// thread to wait on the lock. What came meanwhile interrupts once it
    /// is on again.
    pub fn quiet(&self, quiet: bool) {
        if self.interrupts {
            let ir = self.rt + IR0;
            // IP is written as 0, which leaves it as it is.
            self.regs.w32(ir + IMAN, if quiet { 0 } else { IMAN_IE });
        }
    }

    /// How many root ports it has.
    pub fn ports(&self) -> u8 {
        self.ports
    }

    fn portsc(&self, port: u8) -> u32 {
        self.regs.r32(self.op + PORTSC + 0x10 * (port as usize - 1))
    }

    /// Write PORTSC from sample `sc`, doing `set` and nothing else.
    fn set_portsc(&self, port: u8, sc: u32, set: u32) {
        let at = self.op + PORTSC + 0x10 * (port as usize - 1);
        self.regs.w32(at, (sc & !PORT_KEEP_OFF) | set);
    }

    /// Whether a device is on `port`.
    pub fn connected(&self, port: u8) -> bool {
        self.portsc(port) & PORT_CCS != 0
    }

    fn ring_doorbell(&self, slot: u8, target: u8) {
        fence(Ordering::SeqCst);
        self.regs.w32(self.db + 4 * slot as usize, target as u32);
    }

    /// The next event, from the stash first.
    fn next_event(&mut self) -> Option<Trb> {
        if let Some(t) = self.stash.pop_front() {
            return Some(t);
        }
        self.pop_event()
    }

    fn pop_event(&mut self) -> Option<Trb> {
        // SAFETY: entry `deq` of the event ring.
        let e = unsafe { self.events.add(self.ev.deq) };
        let w3 = unsafe { ptr::read_volatile(e.cast::<u32>().add(3)) };
        if !self.ev.take(w3 & TRB_CYCLE != 0) {
            return None;
        }
        fence(Ordering::SeqCst);
        // SAFETY: as above; the controller is done with the entry.
        let t = unsafe { ptr::read_volatile(e) };
        Some(t)
    }

    /// Tell the controller how far the events were read, and clear what
    /// the interrupt said.
    fn events_read(&self) {
        let ir = self.rt + IR0;
        let deq = self.events_daddr + self.ev.deq as u64 * 16;
        self.regs.w64(ir + ERDP, deq | ERDP_EHB);
        self.regs.w32(ir + IMAN, self.regs.r32(ir + IMAN) | IMAN_IP);
        self.regs.w32(self.op + USBSTS, STS_EINT);
    }

    /// Wait for the event `want` picks; the others wait in the stash.
    fn wait_event(&mut self, what: &str, want: impl FnMut(&Trb) -> bool) -> Result<Trb, String> {
        self.wait_event_for(what, DEADLINE_NS, want)
    }

    /// [`Xhci::wait_event`], for `deadline` nanoseconds. It looks again at
    /// once for a while, then sleeps between looks: most answers come in
    /// microseconds.
    fn wait_event_for(
        &mut self,
        what: &str,
        deadline: u64,
        mut want: impl FnMut(&Trb) -> bool,
    ) -> Result<Trb, String> {
        let start = now_ns();
        loop {
            while let Some(t) = self.pop_event() {
                if want(&t) {
                    self.events_read();
                    return Ok(t);
                }
                self.stash.push_back(t);
            }
            self.events_read();
            let waited = now_ns() - start;
            if waited > deadline {
                return Err(format!("no answer to {what}"));
            }
            if waited < 200_000 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }

    /// Run a command: its completion code and the slot it names.
    fn command(&mut self, what: &str, t: Trb) -> Result<(u32, u8), String> {
        let at = self.cmd.push(t);
        self.ring_doorbell(0, 0);
        let ev = self.wait_event(what, |e| {
            trb_type(e) == EV_COMMAND && (e[0] as u64 | (e[1] as u64) << 32) == at
        })?;
        Ok((ev[2] >> 24, (ev[3] >> 24) as u8))
    }

    fn command_ok(&mut self, what: &str, t: Trb) -> Result<u8, String> {
        match self.command(what, t)? {
            (CC_SUCCESS, slot) => Ok(slot),
            (cc, _) => Err(format!("{what}: completion code {cc}")),
        }
    }

    fn device(&self, slot: u8) -> Option<usize> {
        self.devices.iter().position(|d| d.slot == slot)
    }

    /// A control transfer on device `dev`'s endpoint 0: how many bytes the
    /// data stage moved, which are in the device's buffer (or came from
    /// `out`).
    fn control(&mut self, dev: usize, s: Setup, out: &[u8]) -> Result<usize, String> {
        let d = &mut self.devices[dev];
        let slot = d.slot;
        let len = s.length as usize;
        if len > 4096 {
            return Err("a control transfer over a page".into());
        }
        if !s.device_in() && !out.is_empty() {
            // SAFETY: the device's page, `len` <= 4096 bytes of it.
            unsafe { ptr::copy_nonoverlapping(out.as_ptr(), d.buf.0, out.len().min(len)) };
        }
        d.ep0.push(s.trb());
        let mut data_at = None;
        if len > 0 {
            let dir = if s.device_in() { TRB_DIR_IN } else { 0 };
            data_at = Some(d.ep0.push([
                d.buf.1 as u32,
                (d.buf.1 >> 32) as u32,
                len as u32,
                TRB_DATA << 10 | dir | TRB_ISP,
            ]));
        }
        // The status stage goes the other way from the data, IN when none.
        let status_dir = if len > 0 && s.device_in() {
            0
        } else {
            TRB_DIR_IN
        };
        let status_at = d
            .ep0
            .push([0, 0, 0, TRB_STATUS << 10 | status_dir | TRB_IOC]);
        self.ring_doorbell(slot, 1);
        let mut moved = len;
        loop {
            let ev = self.wait_event("a control transfer", |e| {
                trb_type(e) == EV_TRANSFER && (e[3] >> 24) as u8 == slot && (e[3] >> 16 & 0x1f) == 1
            })?;
            let at = ev[0] as u64 | (ev[1] as u64) << 32;
            let cc = ev[2] >> 24;
            if Some(at) == data_at && cc == CC_SHORT {
                moved = len - (ev[2] & 0xff_ffff) as usize;
                continue;
            }
            if at == status_at && cc == CC_SUCCESS {
                return Ok(moved);
            }
            if cc != CC_SUCCESS && cc != CC_SHORT {
                if cc == CC_STALL {
                    self.unstall(dev, 1)?;
                }
                return Err(format!(
                    "control request {:#x}: completion code {cc}",
                    s.request
                ));
            }
        }
    }

    /// A control IN transfer's data.
    fn control_in(&mut self, dev: usize, s: Setup) -> Result<Vec<u8>, String> {
        let n = self.control(dev, s, &[])?;
        let d = &self.devices[dev];
        // SAFETY: the device's page, of which the transfer wrote `n` bytes.
        Ok(unsafe { std::slice::from_raw_parts(d.buf.0, n) }.to_vec())
    }

    /// A stalled endpoint `dci` of device `dev` running again: reset, and
    /// its ring started where the driver would queue next.
    fn unstall(&mut self, dev: usize, dci: u8) -> Result<(), String> {
        let slot = self.devices[dev].slot;
        let at = (slot as u32) << 24 | (dci as u32) << 16;
        self.command("reset endpoint", [0, 0, 0, TRB_RESET_EP << 10 | at])?;
        let deq = if dci == 1 {
            self.devices[dev].ep0.dequeue()
        } else {
            match self
                .hids
                .iter()
                .flatten()
                .find(|h| h.slot == slot && h.ep.dci == dci)
            {
                Some(h) => h.ep.ring.dequeue(),
                None => return Ok(()),
            }
        };
        self.command(
            "set dequeue",
            [
                deq as u32,
                (deq >> 32) as u32,
                0,
                TRB_SET_TR_DEQUEUE << 10 | at,
            ],
        )?;
        Ok(())
    }

    /// Context `i` (0 the input control context) of an input context.
    fn input(&self, dev: usize, i: usize) -> *mut u32 {
        // SAFETY: the input context holds 33 contexts.
        unsafe { self.devices[dev].input_ctx.0.add(i * self.ctx).cast() }
    }

    fn clear_input(&self, dev: usize) {
        // SAFETY: the whole input context.
        unsafe { self.devices[dev].input_ctx.0.write_bytes(0, 33 * self.ctx) };
    }

    fn set(&self, ctx: *mut u32, word: usize, v: u32) {
        // SAFETY: a word of a context of the input context.
        unsafe { ctx.add(word).write_volatile(v) };
    }

    /// The device on `port`: enumerate it, configure it, and start its HID
    /// interfaces. The indexes of the HIDs it added.
    pub fn attach(&mut self, port: u8) -> Result<Vec<usize>, String> {
        if self.attached(port) {
            return Ok(Vec::new());
        }
        let sc = self.portsc(port);
        if sc & PORT_CCS == 0 {
            return Ok(Vec::new());
        }
        if !self.usb3.get(port as usize).copied().unwrap_or(false) {
            self.set_portsc(port, sc, PORT_PR);
            wait("reset the port", || self.portsc(port) & PORT_PRC != 0)?;
        }
        wait("enabled the port", || self.portsc(port) & PORT_PED != 0)?;
        let sc = self.portsc(port);
        self.set_portsc(port, sc, sc & PORT_CHANGES);
        let speed = sc >> 10 & 0xf;
        self.add_device(port, port.to_string(), 0, 0, None, 0, speed)
    }

    /// Enumerate the device just reset at `path` (its root port and route,
    /// how many hubs it is behind and the hub port it is on, its
    /// translator), and start what it has.
    #[allow(clippy::too_many_arguments)]
    fn add_device(
        &mut self,
        port: u8,
        path: String,
        route: u32,
        tier: u32,
        parent: Option<(u8, u8)>,
        tt: u32,
        speed: u32,
    ) -> Result<Vec<usize>, String> {
        let slot = self.command_ok("enable slot", [0, 0, 0, TRB_ENABLE_SLOT << 10])?;

        let ep0 = Ring::new(&mut self.mem, RING_LEN)?;
        let buf = self.mem.take(4096, 4096)?;
        let input_ctx = self.mem.take(33 * self.ctx, 64)?;
        let out_ctx = self.mem.take(32 * self.ctx, 64)?;
        let mps0 = first_ep0_packet(speed);
        self.devices.push(Device {
            port,
            slot,
            path: path.clone(),
            route,
            tier,
            parent,
            tt,
            ep0,
            buf,
            input_ctx,
            out_ctx,
            speed,
            mps0,
        });
        let dev = self.devices.len() - 1;
        match self.enumerate(dev) {
            Ok(added) => Ok(added),
            Err(e) => {
                self.forget(slot);
                Err(format!("{path}: {e}"))
            }
        }
    }

    fn enumerate(&mut self, dev: usize) -> Result<Vec<usize>, String> {
        let d = &self.devices[dev];
        let (slot, port, speed, mps0) = (d.slot, d.port, d.speed, d.mps0);
        let (route, tt, path) = (d.route, d.tt, d.path.clone());
        // SAFETY: entry `slot` of the DCBAA, which has MAX_SLOTS + 1.
        unsafe { self.dcbaa.add(slot as usize).write_volatile(d.out_ctx.1) };
        let deq = d.ep0.dequeue();
        self.clear_input(dev);
        self.set(self.input(dev, 0), 1, 0b11);
        let slot_ctx = self.input(dev, 1);
        self.set(slot_ctx, 0, 1 << 27 | speed << 20 | route);
        self.set(slot_ctx, 1, (port as u32) << 16);
        self.set(slot_ctx, 2, tt);
        let ep0_ctx = self.input(dev, 2);
        self.set(ep0_ctx, 1, 3 << 1 | 4 << 3 | mps0 << 16);
        self.set(ep0_ctx, 2, deq as u32);
        self.set(ep0_ctx, 3, (deq >> 32) as u32);
        self.set(ep0_ctx, 4, 8);
        let ictx = self.devices[dev].input_ctx.1;
        self.command_ok(
            "address device",
            [
                ictx as u32,
                (ictx >> 32) as u32,
                0,
                TRB_ADDRESS_DEVICE << 10 | (slot as u32) << 24,
            ],
        )?;

        let head = self.control_in(dev, Setup::get_descriptor(DESC_DEVICE, 0, 0, 8))?;
        let said = match (head.get(7), speed) {
            (Some(&e), s) if s > SPEED_HIGH => 1u32 << e.min(9),
            (Some(&m), _) => m as u32,
            (None, _) => mps0,
        };
        if said != mps0 && said >= 8 {
            self.clear_input(dev);
            self.set(self.input(dev, 0), 1, 0b10);
            self.set(self.input(dev, 2), 1, 3 << 1 | 4 << 3 | said << 16);
            self.command_ok(
                "evaluate context",
                [
                    ictx as u32,
                    (ictx >> 32) as u32,
                    0,
                    TRB_EVALUATE_CTX << 10 | (slot as u32) << 24,
                ],
            )?;
            self.devices[dev].mps0 = said;
        }
        let desc = self.control_in(dev, Setup::get_descriptor(DESC_DEVICE, 0, 0, 18))?;
        if desc.len() < 18 {
            return Err("a device descriptor cut short".into());
        }
        let word = |i: usize| u16::from_le_bytes([desc[i], desc[i + 1]]);
        let id = [word(8), word(10), word(12)];
        let maker = self.string(dev, desc[14]);
        let product = self.string(dev, desc[15]);
        let name = match (maker.is_empty(), product.is_empty()) {
            (false, false) => format!("{maker} {product}"),
            (true, false) => product,
            (false, true) => maker,
            (true, true) => format!("USB HID {:04x}:{:04x}", id[0], id[1]),
        };
        let head = self.control_in(dev, Setup::get_descriptor(DESC_CONFIG, 0, 0, 9))?;
        if head.len() < 4 {
            return Err("a configuration descriptor cut short".into());
        }
        let total = u16::from_le_bytes([head[2], head[3]]).min(4096);
        let config = self.control_in(dev, Setup::get_descriptor(DESC_CONFIG, 0, 0, total))?;
        if desc[4] == CLASS_HUB || class_interrupt_in(&config, CLASS_HUB).is_some() {
            return self.hub_start(dev, &config, &name);
        }
        let (value, ifaces) = hid_interfaces(&config);
        let disk = disk_interface(&config);
        if ifaces.is_empty() && disk.is_none() {
            return Err(format!(
                "\"{name}\" has no HID or disk interface this driver drives"
            ));
        }
        self.control(
            dev,
            Setup {
                request_type: 0,
                request: REQ_SET_CONFIGURATION,
                value: value as u16,
                index: 0,
                length: 0,
            },
            &[],
        )?;

        let mut roles = Vec::new();
        for f in &ifaces {
            let class = |request, value| Setup {
                request_type: 0x21,
                request,
                value,
                index: f.number as u16,
                length: 0,
            };
            let boot = f.subclass == 1;
            let role = if boot && f.protocol == 1 {
                self.control(dev, class(HID_SET_PROTOCOL, 0), &[])?;
                Role::Keyboard(Keyboard::default())
            } else {
                let len = if f.report_len == 0 {
                    256
                } else {
                    f.report_len.min(4096)
                };
                let report = Setup {
                    request_type: 0x81,
                    ..Setup::get_descriptor(DESC_REPORT, 0, f.number as u16, len)
                };
                let parsed = self
                    .control_in(dev, report)
                    .ok()
                    .and_then(|d| usbhid::parse_pointer(&d));
                match parsed {
                    Some(p) => Role::Pointer(p),
                    None if boot && f.protocol == 2 => {
                        self.control(dev, class(HID_SET_PROTOCOL, 0), &[])?;
                        Role::Pointer(usbhid::boot_mouse())
                    }
                    None => continue,
                }
            };
            // Reports only when something changes; a device that will not
            // is driven all the same.
            let _ = self.control(dev, class(HID_SET_IDLE, 0), &[]);
            roles.push((*f, role));
        }
        if roles.is_empty() && disk.is_none() {
            return Err(format!(
                "\"{name}\" has no keyboard or pointer this driver reads"
            ));
        }

        // One interrupt IN endpoint per interface, configured at once.
        self.clear_input(dev);
        let mut add = 1u32;
        let mut last = 1u8;
        let mut eps = Vec::new();
        for (f, _) in &roles {
            let (addr, mps, b_interval) = f.endpoint;
            let dci = (addr & 0xf) * 2 + 1;
            let ring = Ring::new(&mut self.mem, RING_LEN)?;
            let deq = ring.dequeue();
            let c = self.input(dev, dci as usize + 1);
            let mps = mps.max(1) as u32;
            self.set(c, 0, interval(speed, b_interval) << 16);
            self.set(c, 1, 3 << 1 | 7 << 3 | mps << 16);
            self.set(c, 2, deq as u32);
            self.set(c, 3, (deq >> 32) as u32);
            self.set(c, 4, mps | mps << 16);
            add |= 1 << dci;
            last = last.max(dci);
            eps.push((dci, ring, mps as usize));
        }
        // A disk's two bulk endpoints.
        let mut bulk = Vec::new();
        if let Some(f) = disk {
            for ((addr, mps), kind) in [(f.bulk_in, 6u32), (f.bulk_out, 2u32)] {
                let dci = (addr & 0xf) * 2 + (addr >> 7);
                let ring = Ring::new(&mut self.mem, RING_LEN)?;
                let deq = ring.dequeue();
                let c = self.input(dev, dci as usize + 1);
                self.set(c, 1, 3 << 1 | kind << 3 | (mps.max(8) as u32) << 16);
                self.set(c, 2, deq as u32);
                self.set(c, 3, (deq >> 32) as u32);
                self.set(c, 4, 4096);
                add |= 1 << dci;
                last = last.max(dci);
                bulk.push(BulkEndpoint { dci, ring });
            }
        }
        self.set(self.input(dev, 0), 1, add);
        let slot_ctx = self.input(dev, 1);
        self.set(slot_ctx, 0, (last as u32) << 27 | speed << 20 | route);
        self.set(slot_ctx, 1, (port as u32) << 16);
        self.set(slot_ctx, 2, tt);
        self.command_ok(
            "configure endpoint",
            [
                ictx as u32,
                (ictx >> 32) as u32,
                0,
                TRB_CONFIGURE_EP << 10 | (slot as u32) << 24,
            ],
        )?;

        let mut added = Vec::new();
        for ((f, role), (dci, ring, mps)) in roles.into_iter().zip(eps) {
            let mut ep = InEndpoint {
                dci,
                ring,
                mps,
                pending: Vec::new(),
                stalled: false,
            };
            for _ in 0..IN_FLIGHT {
                let buf = self.mem.take(mps, 64)?;
                queue_in(&mut ep, buf.0, buf.1);
            }
            self.ring_doorbell(slot, dci);
            self.hids.push(Some(Hid {
                port,
                path: path.clone(),
                slot,
                iface: f.number,
                name: name.clone(),
                id,
                role,
                ep,
            }));
            added.push(self.hids.len() - 1);
        }
        if let Some(f) = disk {
            let mut bulk = bulk.into_iter();
            let (Some(bulk_in), Some(bulk_out)) = (bulk.next(), bulk.next()) else {
                return Err("a disk without its two endpoints".into());
            };
            let cbw = self.mem.take(64, 64)?;
            let csw = self.mem.take(64, 64)?;
            self.disks.push(Some(Disk {
                port,
                path: path.clone(),
                unit: port as usize | (route as usize) << 8,
                slot,
                iface: f.number,
                name: name.clone(),
                sectors: 0,
                bulk_in,
                bulk_out,
                tag: 0,
                cbw,
                csw,
            }));
            let k = self.disks.len() - 1;
            match self.disk_start(k, dev) {
                Ok(()) => self.fresh_disks.push(k),
                Err(e) => {
                    self.disks[k] = None;
                    if added.is_empty() {
                        return Err(format!("\"{name}\": {e}"));
                    }
                }
            }
        }
        Ok(added)
    }

    /// Put a bulk transfer of `len` bytes at `daddr` on disk `k`'s IN or
    /// OUT ring, not yet rung: its endpoint, and each TRB's address with
    /// the bytes before it and its own. Only the last TRB asks for an
    /// event, and only when `ioc`; a short packet always says so.
    fn queue_bulk(
        &mut self,
        k: usize,
        device_in: bool,
        daddr: u64,
        len: usize,
        ioc: bool,
    ) -> Option<(u8, Vec<(u64, usize, usize)>)> {
        let d = self.disks[k].as_mut()?;
        let ep = if device_in {
            &mut d.bulk_in
        } else {
            &mut d.bulk_out
        };
        let parts = pieces(daddr, len);
        ep.ring.room(parts.len());
        let mut at = Vec::with_capacity(parts.len());
        let mut before = 0usize;
        for (i, &(p, n)) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let flags = match (last, ioc) {
                (false, _) => TRB_CHAIN,
                (true, true) => TRB_IOC,
                (true, false) => 0,
            };
            let t = ep.ring.push([
                p as u32,
                (p >> 32) as u32,
                n,
                TRB_NORMAL << 10 | TRB_ISP | flags,
            ]);
            at.push((t, before, n as usize));
            before += n as usize;
        }
        Some((ep.dci, at))
    }

    /// A bulk transfer of `len` bytes at `daddr` on disk `k`'s IN or OUT
    /// endpoint, waited for: how many bytes it moved, or the completion
    /// code it ended with.
    fn bulk(&mut self, k: usize, device_in: bool, daddr: u64, len: usize) -> Result<usize, u32> {
        let slot = self.disks[k].as_ref().ok_or(0u32)?.slot;
        let (dci, at) = self.queue_bulk(k, device_in, daddr, len, true).ok_or(0u32)?;
        self.ring_doorbell(slot, dci);
        let last = at.last().map(|a| a.0);
        loop {
            let ev = self
                .wait_event_for("a bulk transfer", 10 * DEADLINE_NS, |e| {
                    trb_type(e) == EV_TRANSFER
                        && (e[3] >> 24) as u8 == slot
                        && (e[3] >> 16 & 0x1f) as u8 == dci
                })
                .map_err(|_| 0u32)?;
            let ptr = ev[0] as u64 | (ev[1] as u64) << 32;
            let cc = ev[2] >> 24;
            let left = (ev[2] & 0xff_ffff) as usize;
            let Some(&(_, before, n)) = at.iter().find(|a| a.0 == ptr) else {
                // A TRB of an earlier transfer, after its short packet.
                continue;
            };
            match cc {
                CC_SUCCESS if Some(ptr) == last => return Ok(len),
                CC_SUCCESS => continue,
                CC_SHORT => return Ok(before + n.saturating_sub(left)),
                cc => return Err(cc),
            }
        }
    }

    /// A stalled bulk endpoint of disk `k` running again.
    fn bulk_unstall(&mut self, k: usize, device_in: bool) {
        let Some(d) = self.disks[k].as_ref() else {
            return;
        };
        let (slot, ep) = (d.slot, if device_in { &d.bulk_in } else { &d.bulk_out });
        let (dci, deq) = (ep.dci, ep.ring.dequeue());
        let at = (slot as u32) << 24 | (dci as u32) << 16;
        // A halted endpoint is reset; a running one, with transfers left on
        // its ring, is stopped. Whichever it was refuses the other.
        let _ = self.command("stop endpoint", [0, 0, 0, TRB_STOP_EP << 10 | at]);
        let _ = self.command("reset endpoint", [0, 0, 0, TRB_RESET_EP << 10 | at]);
        let _ = self.command(
            "set dequeue",
            [
                deq as u32,
                (deq >> 32) as u32,
                0,
                TRB_SET_TR_DEQUEUE << 10 | at,
            ],
        );
        if let Some(dev) = self.device(slot) {
            // CLEAR_FEATURE(ENDPOINT_HALT) on the device's side too.
            let addr = (dci / 2) | if device_in { 0x80 } else { 0 };
            let _ = self.control(
                dev,
                Setup {
                    request_type: 0x02,
                    request: 1,
                    value: 0,
                    index: addr as u16,
                    length: 0,
                },
                &[],
            );
        }
    }

    /// SCSI command `cdb` on disk `k`, with `len` bytes of data at
    /// `daddr` going the way `device_in` says: the command's status (0 it
    /// passed) and how much data moved.
    fn scsi(
        &mut self,
        k: usize,
        cdb: &[u8],
        device_in: bool,
        daddr: u64,
        len: usize,
    ) -> Result<(u8, usize), String> {
        let d = self.disks[k].as_mut().ok_or("the disk is gone")?;
        d.tag = d.tag.wrapping_add(1);
        let tag = d.tag;
        let block = cbw(tag, len as u32, device_in, cdb);
        let (cbw_daddr, csw_daddr, csw) = (d.cbw.1, d.csw.1, d.csw.0);
        // SAFETY: the disk's 64 bytes for its command blocks.
        unsafe { ptr::copy_nonoverlapping(block.as_ptr(), d.cbw.0, CBW_LEN) };
        match self.bulk(k, false, cbw_daddr, CBW_LEN) {
            Ok(_) => {}
            Err(CC_STALL) => {
                self.disk_reset(k);
                return Err("the disk refused a command block".into());
            }
            Err(cc) => return Err(format!("command block: completion code {cc}")),
        }
        let mut moved = 0;
        if len > 0 {
            match self.bulk(k, device_in, daddr, len) {
                Ok(n) => moved = n,
                Err(CC_STALL) => self.bulk_unstall(k, device_in),
                Err(cc) => return Err(format!("data: completion code {cc}")),
            }
        }
        let mut tries = 0;
        loop {
            // SAFETY: the disk's 64 bytes for status words.
            unsafe { csw.write_bytes(0, CSW_LEN) };
            match self.bulk(k, true, csw_daddr, CSW_LEN) {
                Ok(_) => break,
                Err(CC_STALL) if tries == 0 => {
                    self.bulk_unstall(k, true);
                    tries += 1;
                }
                Err(cc) => {
                    self.disk_reset(k);
                    return Err(format!("status: completion code {cc}"));
                }
            }
        }
        // SAFETY: as above; the controller wrote it.
        let got = unsafe { std::slice::from_raw_parts(csw, CSW_LEN) };
        match csw_status(got, tag) {
            Some(2) | None => {
                self.disk_reset(k);
                Err("a phase error".into())
            }
            Some(status) => Ok((status, moved)),
        }
    }

    /// [`Xhci::scsi`] with fewer waits: the command block and the data
    /// queued at once, and the status word with them when the data goes
    /// out. Data coming in is waited for before the status word is asked
    /// for: QEMU's disk, asked for both at once, never answers. `None` when
    /// something went wrong on the way: the endpoints are running again
    /// and the command can be sent the slow way.
    fn scsi_fast(
        &mut self,
        k: usize,
        cdb: &[u8],
        device_in: bool,
        daddr: u64,
        len: usize,
    ) -> Option<(u8, usize)> {
        let d = self.disks[k].as_mut()?;
        d.tag = d.tag.wrapping_add(1);
        let (tag, slot) = (d.tag, d.slot);
        let block = cbw(tag, len as u32, device_in, cdb);
        let (cbw_daddr, csw_daddr, csw) = (d.cbw.1, d.csw.1, d.csw.0);
        // SAFETY: the disk's 64 bytes for command blocks and for status
        // words.
        unsafe {
            ptr::copy_nonoverlapping(block.as_ptr(), d.cbw.0, CBW_LEN);
            csw.write_bytes(0, CSW_LEN);
        }
        let reading = device_in && len > 0;
        let (out_dci, _) = self.queue_bulk(k, false, cbw_daddr, CBW_LEN, false)?;
        let data = if len > 0 {
            self.queue_bulk(k, device_in, daddr, len, reading)?.1
        } else {
            Vec::new()
        };
        let in_dci = self.disks[k].as_ref()?.bulk_in.dci;
        let mut status_at = None;
        if !reading {
            status_at = Some(self.queue_bulk(k, true, csw_daddr, CSW_LEN, true)?.1[0].0);
        }
        self.ring_doorbell(slot, out_dci);
        self.ring_doorbell(slot, in_dci);
        let data_end = data.last().map(|a| a.0);
        let mut moved = len;
        let ok = loop {
            let Ok(ev) = self.wait_event_for("a SCSI command", 10 * DEADLINE_NS, |e| {
                let dci = (e[3] >> 16 & 0x1f) as u8;
                trb_type(e) == EV_TRANSFER
                    && (e[3] >> 24) as u8 == slot
                    && (dci == in_dci || dci == out_dci)
            }) else {
                break false;
            };
            let ptr = ev[0] as u64 | (ev[1] as u64) << 32;
            let cc = ev[2] >> 24;
            if Some(ptr) == status_at {
                break matches!(cc, CC_SUCCESS | CC_SHORT);
            }
            let data_done = match cc {
                CC_SHORT => {
                    if let Some(&(_, before, n)) = data.iter().find(|a| a.0 == ptr) {
                        moved = before + n.saturating_sub((ev[2] & 0xff_ffff) as usize);
                    }
                    true
                }
                CC_SUCCESS => Some(ptr) == data_end,
                _ => break false,
            };
            if reading && data_done && status_at.is_none() {
                status_at = Some(self.queue_bulk(k, true, csw_daddr, CSW_LEN, true)?.1[0].0);
                self.ring_doorbell(slot, in_dci);
            }
        };
        if !ok {
            self.disk_reset(k);
            return None;
        }
        // SAFETY: as above; the controller wrote it.
        let got = unsafe { std::slice::from_raw_parts(csw, CSW_LEN) };
        match csw_status(got, tag) {
            Some(2) | None => {
                self.disk_reset(k);
                None
            }
            Some(status) => Some((status, moved)),
        }
    }

    /// The reset recovery of bulk-only transport: the class reset, and
    /// both endpoints running again.
    fn disk_reset(&mut self, k: usize) {
        let Some(d) = self.disks[k].as_ref() else {
            return;
        };
        let (slot, iface) = (d.slot, d.iface);
        if let Some(dev) = self.device(slot) {
            let _ = self.control(
                dev,
                Setup {
                    request_type: 0x21,
                    request: BOT_RESET,
                    value: 0,
                    index: iface as u16,
                    length: 0,
                },
                &[],
            );
        }
        self.bulk_unstall(k, true);
        self.bulk_unstall(k, false);
    }

    /// Bring disk `k` up: ready, and its size. Its data goes through the
    /// page of device `dev`.
    fn disk_start(&mut self, k: usize, dev: usize) -> Result<(), String> {
        let (buf, daddr) = self.devices[dev].buf;
        let start = now_ns();
        loop {
            let (status, _) = self.scsi(k, &[0; 6], false, 0, 0)?;
            if status == 0 {
                break;
            }
            // Whatever it has to say (a medium changed, becoming ready) is
            // taken, as REQUEST SENSE does.
            self.scsi(k, &[0x03, 0, 0, 0, 18, 0], true, daddr, 18)?;
            if now_ns() - start > 10 * DEADLINE_NS {
                return Err("the disk never became ready".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let read_capacity = |x: &mut Xhci, sixteen: bool| -> Result<(u64, u32), String> {
            let (cdb, len) = if sixteen {
                (
                    vec![0x9e, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0],
                    32,
                )
            } else {
                (vec![0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8)
            };
            let (status, n) = x.scsi(k, &cdb, true, daddr, len)?;
            // SAFETY: the device's page, which the command wrote.
            let b = unsafe { std::slice::from_raw_parts(buf, n) };
            if status != 0 || n < 8 {
                return Err("READ CAPACITY failed".into());
            }
            Ok(if sixteen && n >= 12 {
                (
                    u64::from_be_bytes(b[0..8].try_into().unwrap()),
                    u32::from_be_bytes(b[8..12].try_into().unwrap()),
                )
            } else {
                (
                    u32::from_be_bytes(b[0..4].try_into().unwrap()) as u64,
                    u32::from_be_bytes(b[4..8].try_into().unwrap()),
                )
            })
        };
        let (mut last, mut block) = read_capacity(self, false)?;
        if last == u32::MAX as u64 {
            (last, block) = read_capacity(self, true)?;
        }
        if block as usize != SECTOR {
            return Err(format!(
                "its blocks are {block} bytes; this driver serves 512"
            ));
        }
        if let Some(d) = self.disks[k].as_mut() {
            d.sectors = last + 1;
        }
        Ok(())
    }

    /// Disks added since this was last asked.
    pub fn new_disks(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.fresh_disks)
    }

    /// Read or write disk `k`: `sectors` sectors from `sector`, at `daddr`
    /// (memory the controller reaches). Or, with no sectors, flush its
    /// cache. `Err` is the disk gone or not answering; `Ok(false)` a
    /// request it refused.
    pub fn disk_io(
        &mut self,
        k: usize,
        write: bool,
        sector: u64,
        sectors: u32,
        daddr: u64,
    ) -> Result<bool, String> {
        if self.disks.get(k).and_then(|d| d.as_ref()).is_none() {
            return Err("the disk is gone".into());
        }
        if sectors == 0 {
            // SYNCHRONIZE CACHE(10): a disk with no cache may refuse it,
            // and then has nothing to put on the medium either.
            let _ = self.scsi(k, &[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], false, 0, 0)?;
            return Ok(true);
        }
        let cdb = scsi_rw(write, sector, sectors);
        let len = sectors as usize * SECTOR;
        let (status, moved) = match self.scsi_fast(k, &cdb, !write, daddr, len) {
            Some(r) => r,
            None => self.scsi(k, &cdb, !write, daddr, len)?,
        };
        Ok(status == 0 && moved == len)
    }

    /// String descriptor `index`, empty when there is none.
    fn string(&mut self, dev: usize, index: u8) -> String {
        if index == 0 {
            return String::new();
        }
        self.control_in(
            dev,
            Setup::get_descriptor(DESC_STRING, index, LANG_EN_US, 255),
        )
        .map(|d| string_text(&d))
        .unwrap_or_default()
    }

    /// The device on `port` is gone: its slot goes, and its HIDs.
    pub fn detach(&mut self, port: u8) -> Vec<usize> {
        let roots: Vec<u8> = self
            .devices
            .iter()
            .filter(|d| d.port == port && d.parent.is_none())
            .map(|d| d.slot)
            .collect();
        let mut gone = Vec::new();
        for slot in roots {
            gone.extend(self.forget(slot));
        }
        gone
    }

    /// Device `slot` is gone, and whatever is behind it if it is a hub: its
    /// slot is disabled, and its HIDs, disks and hubs go. The HIDs that
    /// went.
    fn forget(&mut self, slot: u8) -> Vec<usize> {
        let mut gone = Vec::new();
        let children: Vec<u8> = self
            .devices
            .iter()
            .filter(|d| d.parent.is_some_and(|(hub, _)| hub == slot))
            .map(|d| d.slot)
            .collect();
        for child in children {
            gone.extend(self.forget(child));
        }
        let _ = self.command(
            "disable slot",
            [0, 0, 0, TRB_DISABLE_SLOT << 10 | (slot as u32) << 24],
        );
        // SAFETY: entry `slot` of the DCBAA.
        unsafe { self.dcbaa.add(slot as usize).write_volatile(0) };
        self.devices.retain(|d| d.slot != slot);
        for d in self.disks.iter_mut() {
            if d.as_ref().is_some_and(|d| d.slot == slot) {
                *d = None;
            }
        }
        for h in self.hubs.iter_mut() {
            if h.as_ref().is_some_and(|h| h.slot == slot) {
                *h = None;
            }
        }
        for (i, h) in self.hids.iter_mut().enumerate() {
            if h.as_ref().is_some_and(|h| h.slot == slot) {
                *h = None;
                gone.push(i);
            }
        }
        gone
    }

    /// A hub request to port `port` (0 the hub itself) of device `dev`.
    fn hub_request(
        &mut self,
        dev: usize,
        request: u8,
        feature: u16,
        port: u8,
    ) -> Result<(), String> {
        let s = Setup {
            request_type: 0x23,
            request,
            value: feature,
            index: port as u16,
            length: 0,
        };
        self.control(dev, s, &[]).map(|_| ())
    }

    /// A hub port's status and what changed in it.
    fn hub_port_status(&mut self, dev: usize, port: u8) -> Result<(u16, u16), String> {
        let s = Setup {
            request_type: 0xa3,
            request: REQ_GET_STATUS,
            value: 0,
            index: port as u16,
            length: 4,
        };
        let b = self.control_in(dev, s)?;
        if b.len() < 4 {
            return Err("a port status cut short".into());
        }
        Ok((
            u16::from_le_bytes([b[0], b[1]]),
            u16::from_le_bytes([b[2], b[3]]),
        ))
    }

    /// Device `dev` is a hub: configure it, power its ports, start its
    /// status change endpoint, and enumerate what is on its ports.
    fn hub_start(&mut self, dev: usize, config: &[u8], name: &str) -> Result<Vec<usize>, String> {
        let d = &self.devices[dev];
        let (slot, port, speed, route, tt) = (d.slot, d.port, d.speed, d.route, d.tt);
        if speed > SPEED_HIGH {
            return Err(format!(
                "\"{name}\" is a USB 3 hub, which this driver does not drive"
            ));
        }
        let (addr, mps, b_interval) =
            class_interrupt_in(config, CLASS_HUB).ok_or("a hub with no status change endpoint")?;
        let value = config.get(5).copied().unwrap_or(1);
        self.control(
            dev,
            Setup {
                request_type: 0,
                request: REQ_SET_CONFIGURATION,
                value: value as u16,
                index: 0,
                length: 0,
            },
            &[],
        )?;
        let hub = self.control_in(
            dev,
            Setup {
                request_type: 0xa0,
                ..Setup::get_descriptor(DESC_HUB, 0, 0, 9)
            },
        )?;
        if hub.len() < 7 {
            return Err("a hub descriptor cut short".into());
        }
        let ports = hub[2].min(15);
        let characteristics = u16::from_le_bytes([hub[3], hub[4]]);
        let power_good = Duration::from_millis(hub[5] as u64 * 2 + 20);

        // The slot says it is a hub, with how many ports; a high-speed one
        // says how long its translator thinks.
        self.clear_input(dev);
        let dci = (addr & 0xf) * 2 + 1;
        let ring = Ring::new(&mut self.mem, RING_LEN)?;
        let deq = ring.dequeue();
        let c = self.input(dev, dci as usize + 1);
        let mps = mps.max(1) as u32;
        self.set(c, 0, interval(speed, b_interval) << 16);
        self.set(c, 1, 3 << 1 | 7 << 3 | mps << 16);
        self.set(c, 2, deq as u32);
        self.set(c, 3, (deq >> 32) as u32);
        self.set(c, 4, mps | mps << 16);
        self.set(self.input(dev, 0), 1, 1 | 1 << dci);
        let slot_ctx = self.input(dev, 1);
        self.set(
            slot_ctx,
            0,
            (dci as u32) << 27 | 1 << 26 | speed << 20 | route,
        );
        self.set(slot_ctx, 1, (port as u32) << 16 | (ports as u32) << 24);
        let think = if speed == SPEED_HIGH {
            ((characteristics >> 5) & 3) as u32
        } else {
            0
        };
        self.set(slot_ctx, 2, tt | think << 16);
        let ictx = self.devices[dev].input_ctx.1;
        self.command_ok(
            "configure endpoint",
            [
                ictx as u32,
                (ictx >> 32) as u32,
                0,
                TRB_CONFIGURE_EP << 10 | (slot as u32) << 24,
            ],
        )?;
        let mut ep = InEndpoint {
            dci,
            ring,
            mps: mps as usize,
            pending: Vec::new(),
            stalled: false,
        };
        for _ in 0..2 {
            let buf = self.mem.take(mps as usize, 64)?;
            queue_in(&mut ep, buf.0, buf.1);
        }
        self.ring_doorbell(slot, dci);
        self.hubs.push(Some(Hub { slot, ports, ep }));
        let h = self.hubs.len() - 1;

        for p in 1..=ports {
            self.hub_request(dev, REQ_SET_FEATURE, HUB_PORT_POWER, p)?;
        }
        std::thread::sleep(power_good);
        let mut added = Vec::new();
        for p in 1..=ports {
            added.extend(self.hub_port(h, p)?.added);
        }
        Ok(added)
    }

    /// Port `p` of hub `h` may have changed: what is on it now is
    /// enumerated, what left it is forgotten.
    fn hub_port(&mut self, h: usize, p: u8) -> Result<Changes, String> {
        let mut out = Changes::default();
        let Some(hub_slot) = self.hubs.get(h).and_then(|h| h.as_ref()).map(|h| h.slot) else {
            return Ok(out);
        };
        let dev = self.device(hub_slot).ok_or("the hub is gone")?;
        let (status, change) = self.hub_port_status(dev, p)?;
        for f in HUB_PORT_CHANGES {
            if change & 1 << (f - HUB_C_PORT_CONNECTION) != 0 {
                self.hub_request(dev, REQ_CLEAR_FEATURE, f, p)?;
            }
        }
        let child = self
            .devices
            .iter()
            .find(|d| d.parent == Some((hub_slot, p)))
            .map(|d| d.slot);
        let connected = status & HUB_STATUS_CONNECTED != 0;
        if let Some(c) = child {
            if connected && change & HUB_CHANGE_CONNECTION == 0 {
                return Ok(out);
            }
            out.gone.extend(self.forget(c));
        }
        if !connected {
            return Ok(out);
        }
        let d = &self.devices[dev];
        let (root, route, tier, speed, tt) = (d.port, d.route, d.tier, d.speed, d.tt);
        let path = format!("{}.{p}", d.path);
        let Some(child_route) = child_route(route, tier, p) else {
            return Err(format!("{path}: too many hubs deep"));
        };
        self.hub_request(dev, REQ_SET_FEATURE, HUB_PORT_RESET, p)?;
        let start = now_ns();
        let status = loop {
            let (status, change) = self.hub_port_status(dev, p)?;
            if change & HUB_CHANGE_RESET != 0 {
                self.hub_request(dev, REQ_CLEAR_FEATURE, HUB_C_PORT_CONNECTION + 4, p)?;
                break status;
            }
            if now_ns() - start > DEADLINE_NS {
                return Err(format!("{path}: the hub never reset the port"));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // The device gets 10 ms to recover from its reset (USB 2.0 §9.2.6.2).
        std::thread::sleep(Duration::from_millis(10));
        let child_speed = hub_port_speed(status);
        let child_tt = if speed == SPEED_HIGH && child_speed != SPEED_HIGH {
            hub_slot as u32 | (p as u32) << 8
        } else {
            tt
        };
        out.added = self.add_device(
            root,
            path,
            child_route,
            tier + 1,
            Some((hub_slot, p)),
            child_tt,
            child_speed,
        )?;
        Ok(out)
    }

    /// Hub `h` said ports in `bitmap` changed: each one looked at again.
    pub fn hub_changed(&mut self, h: usize, bitmap: &[u8]) -> Changes {
        let mut out = Changes::default();
        let Some(ports) = self.hubs.get(h).and_then(|h| h.as_ref()).map(|h| h.ports) else {
            return out;
        };
        for p in changed_ports(bitmap, ports) {
            match self.hub_port(h, p) {
                Ok(c) => {
                    out.added.extend(c.added);
                    out.gone.extend(c.gone);
                }
                Err(e) => println!("udrv-xhci: hub port {p}: {e}"),
            }
        }
        out
    }

    /// Light a keyboard's LEDs: an output report through endpoint 0.
    pub fn set_leds(&mut self, hid: usize, report: u8) -> Result<(), String> {
        let Some(h) = self.hids.get(hid).and_then(|h| h.as_ref()) else {
            return Ok(());
        };
        let (slot, iface) = (h.slot, h.iface);
        let dev = self.device(slot).ok_or("no such device")?;
        let s = Setup {
            request_type: 0x21,
            request: HID_SET_REPORT,
            value: 0x0200,
            index: iface as u16,
            length: 1,
        };
        self.control(dev, s, &[report]).map(|_| ())
    }

    /// Take every event there is: reports, each handed out and its buffer
    /// queued again, and ports that changed.
    pub fn service(&mut self) -> Found {
        let mut found = Found::default();
        let mut unstall = Vec::new();
        while let Some(ev) = self.next_event() {
            match trb_type(&ev) {
                EV_TRANSFER => {
                    let slot = (ev[3] >> 24) as u8;
                    let dci = (ev[3] >> 16 & 0x1f) as u8;
                    let at = ev[0] as u64 | (ev[1] as u64) << 32;
                    let cc = ev[2] >> 24;
                    let left = (ev[2] & 0xff_ffff) as usize;
                    if let Some((k, hub)) = self.hubs.iter_mut().enumerate().find_map(|(k, h)| {
                        h.as_mut()
                            .filter(|h| h.slot == slot && h.ep.dci == dci)
                            .map(|h| (k, h))
                    }) {
                        if let Some(at_k) = hub.ep.pending.iter().position(|p| p.0 == at) {
                            let (_, buf, daddr) = hub.ep.pending.remove(at_k);
                            if cc == CC_SUCCESS || cc == CC_SHORT {
                                let n = hub.ep.mps.saturating_sub(left);
                                // SAFETY: the transfer's buffer, of which it
                                // wrote `n` bytes.
                                let r = unsafe { std::slice::from_raw_parts(buf, n) }.to_vec();
                                found.hubs.push((k, r));
                            }
                            queue_in(&mut hub.ep, buf, daddr);
                            self.regs.w32(self.db + 4 * slot as usize, dci as u32);
                        }
                        continue;
                    }
                    let Some((i, h)) = self.hids.iter_mut().enumerate().find_map(|(i, h)| {
                        h.as_mut()
                            .filter(|h| h.slot == slot && h.ep.dci == dci)
                            .map(|h| (i, h))
                    }) else {
                        continue;
                    };
                    let Some(k) = h.ep.pending.iter().position(|p| p.0 == at) else {
                        continue;
                    };
                    let (_, buf, daddr) = h.ep.pending.remove(k);
                    match cc {
                        CC_SUCCESS | CC_SHORT => {
                            let n = h.ep.mps.saturating_sub(left);
                            // SAFETY: the transfer's buffer, `mps` bytes,
                            // of which it wrote `n`.
                            let r = unsafe { std::slice::from_raw_parts(buf, n) }.to_vec();
                            found.reports.push((i, r));
                            queue_in(&mut h.ep, buf, daddr);
                            self.regs.w32(self.db + 4 * slot as usize, dci as u32);
                        }
                        CC_STALL => {
                            h.ep.stalled = true;
                            h.ep.pending.push((0, buf, daddr));
                            unstall.push((slot, dci));
                        }
                        _ => {
                            // A transfer lost; the others still run.
                            queue_in(&mut h.ep, buf, daddr);
                            self.regs.w32(self.db + 4 * slot as usize, dci as u32);
                        }
                    }
                }
                EV_PORT => {
                    let port = (ev[0] >> 24) as u8;
                    if (1..=self.ports).contains(&port) {
                        let sc = self.portsc(port);
                        self.set_portsc(port, sc, sc & PORT_CHANGES);
                        if !found.ports.contains(&port) {
                            found.ports.push(port);
                        }
                    }
                }
                _ => {}
            }
        }
        self.events_read();
        for (slot, dci) in unstall {
            if let Some(dev) = self.device(slot) {
                if self.unstall(dev, dci).is_ok() {
                    self.requeue_stalled(slot, dci);
                }
            }
        }
        found
    }

    /// The buffers a stall held back, queued again.
    fn requeue_stalled(&mut self, slot: u8, dci: u8) {
        if let Some(h) = self
            .hids
            .iter_mut()
            .flatten()
            .find(|h| h.slot == slot && h.ep.dci == dci)
        {
            // The ring starts again past every TRB it held: each buffer
            // goes in again.
            let held = std::mem::take(&mut h.ep.pending);
            for (_, buf, daddr) in held {
                queue_in(&mut h.ep, buf, daddr);
            }
            h.ep.stalled = false;
            self.ring_doorbell(slot, dci);
        }
    }

    /// Whether events wait that [`Xhci::service`] has not taken: ones a
    /// command or a control transfer set aside after it.
    pub fn pending(&self) -> bool {
        !self.stash.is_empty()
    }

    /// Whether a device on `port` is driven.
    pub fn attached(&self, port: u8) -> bool {
        self.devices
            .iter()
            .any(|d| d.port == port && d.parent.is_none())
    }
}

/// Queue a transfer into `buf` on an interrupt IN endpoint.
fn queue_in(ep: &mut InEndpoint, buf: *mut u8, daddr: u64) {
    let at = ep.ring.push([
        daddr as u32,
        (daddr >> 32) as u32,
        ep.mps as u32,
        TRB_NORMAL << 10 | TRB_ISP | TRB_IOC,
    ]);
    ep.pending.push((at, buf, daddr));
}

#[cfg(test)]
mod tests;
