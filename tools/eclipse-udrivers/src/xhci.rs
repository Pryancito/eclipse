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
//! Not here yet: hubs, USB mass storage, isochronous endpoints, and an
//! interface that is neither a boot keyboard nor a pointer with an X.

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
const TRB_SET_TR_DEQUEUE: u32 = 16;
const EV_TRANSFER: u32 = 32;
const EV_COMMAND: u32 = 33;
const EV_PORT: u32 = 34;
const TRB_CYCLE: u32 = 1;
const TRB_TOGGLE: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
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
    pub slot: u8,
    pub iface: u8,
    pub name: String,
    pub id: [u16; 3],
    pub role: Role,
    ep: InEndpoint,
}

/// A device on a root port.
struct Device {
    port: u8,
    slot: u8,
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
        };
        for p in 1..=ports {
            let sc = x.portsc(p);
            if sc & PORT_PP == 0 {
                x.set_portsc(p, sc, PORT_PP);
            }
        }
        Ok(x)
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
    fn wait_event(
        &mut self,
        what: &str,
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
            if now_ns() - start > DEADLINE_NS {
                return Err(format!("no answer to {what}"));
            }
            std::thread::sleep(Duration::from_micros(100));
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
        if self.devices.iter().any(|d| d.port == port) {
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
        let slot = self.command_ok("enable slot", [0, 0, 0, TRB_ENABLE_SLOT << 10])?;

        let ep0 = Ring::new(&mut self.mem, RING_LEN)?;
        let buf = self.mem.take(4096, 4096)?;
        let input_ctx = self.mem.take(33 * self.ctx, 64)?;
        let out_ctx = self.mem.take(32 * self.ctx, 64)?;
        let mps0 = first_ep0_packet(speed);
        self.devices.push(Device {
            port,
            slot,
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
                let _ = self.command(
                    "disable slot",
                    [0, 0, 0, TRB_DISABLE_SLOT << 10 | (slot as u32) << 24],
                );
                self.devices.remove(dev);
                Err(e)
            }
        }
    }

    fn enumerate(&mut self, dev: usize) -> Result<Vec<usize>, String> {
        let d = &self.devices[dev];
        let (slot, port, speed, mps0) = (d.slot, d.port, d.speed, d.mps0);
        // SAFETY: entry `slot` of the DCBAA, which has MAX_SLOTS + 1.
        unsafe { self.dcbaa.add(slot as usize).write_volatile(d.out_ctx.1) };
        let deq = d.ep0.dequeue();
        self.clear_input(dev);
        self.set(self.input(dev, 0), 1, 0b11);
        let slot_ctx = self.input(dev, 1);
        self.set(slot_ctx, 0, 1 << 27 | speed << 20);
        self.set(slot_ctx, 1, (port as u32) << 16);
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
        let (value, ifaces) = hid_interfaces(&config);
        if ifaces.is_empty() {
            return Err(format!(
                "\"{name}\" has no HID interface this driver drives"
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
        if roles.is_empty() {
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
        self.set(self.input(dev, 0), 1, add);
        let slot_ctx = self.input(dev, 1);
        self.set(slot_ctx, 0, (last as u32) << 27 | speed << 20);
        self.set(slot_ctx, 1, (port as u32) << 16);
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
                slot,
                iface: f.number,
                name: name.clone(),
                id,
                role,
                ep,
            }));
            added.push(self.hids.len() - 1);
        }
        Ok(added)
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
        let Some(dev) = self.devices.iter().position(|d| d.port == port) else {
            return Vec::new();
        };
        let slot = self.devices[dev].slot;
        let _ = self.command(
            "disable slot",
            [0, 0, 0, TRB_DISABLE_SLOT << 10 | (slot as u32) << 24],
        );
        // SAFETY: entry `slot` of the DCBAA.
        unsafe { self.dcbaa.add(slot as usize).write_volatile(0) };
        self.devices.remove(dev);
        let mut gone = Vec::new();
        for (i, h) in self.hids.iter_mut().enumerate() {
            if h.as_ref().is_some_and(|h| h.slot == slot) {
                *h = None;
                gone.push(i);
            }
        }
        gone
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
        self.devices.iter().any(|d| d.port == port)
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
