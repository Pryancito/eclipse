//! An input device as Linux's evdev shows it (`/dev/input/event*`): what
//! a program reads of the node, and what its `EVIOC*` ioctls answer. The
//! driver feeds it the device's events ([`Evdev::feed`]) and serves it
//! with [`crate::served`].
//!
//! It is `drivers/input/evdev.c` and the part of `drivers/input/input.c`
//! that a reader can see, the way the kernel's own node
//! (`linux-object/src/fs/devfs/input/event.rs`) does it, with what that one
//! leaves out because it has one queue for every reader:
//!
//! - Each open has its own queue, its own clock and its own place in it. A
//!   reader sees whole frames only: the events up to a `SYN_REPORT`, all
//!   with the time of the frame's first. One that falls behind loses its
//!   queue and is told with a `SYN_DROPPED`.
//! - The device's state is kept as the input core keeps it: a key already
//!   down that is said to go down again is no event, nor is an axis that
//!   does not move, and `EVIOCGKEY` and the rest answer what is down now.
//! - `EVIOCGRAB` gives one open every event until it lets go, and
//!   `EVIOCREVOKE` ends an open for good, as a seat manager does to a
//!   session it takes the device from.
//! - What a program writes to the node is events, which go to every reader
//!   as the device's own do; a LED or a sound among them goes to the
//!   device as well ([`Evdev::take_outgoing`]).
//!
//! Everything here is plain data and tested on the host: the system calls
//! are the driver's.

use std::collections::{BTreeMap, VecDeque};

use crate::served::{Device, Node, EAGAIN, EBUSY, EINVAL, POLLERR, POLLHUP, POLLIN, POLLOUT};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_SW: u16 = 0x05;
pub const EV_LED: u16 = 0x11;
pub const EV_SND: u16 = 0x12;
pub const EV_REP: u16 = 0x14;
pub const EV_FF: u16 = 0x15;

pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

/// `ENOENT` and `ENODEV`, which only evdev answers with.
pub const ENOENT: i32 = 2;
pub const ENODEV: i32 = 19;

/// `EV_VERSION`.
const VERSION: i32 = 0x01_0001;
/// The bytes of a `struct input_event` on a 64-bit machine: a `timeval`,
/// then type, code and value.
pub const EVENT_LEN: usize = 24;
/// How many events an open holds before it is told it fell behind. Linux
/// sizes it by the device, at least 64; a keyboard's frame is two events.
pub const QUEUE_LEN: usize = 128;
/// The axes `EVIOCGABS` and `EVIOCSABS` reach (`ABS_CNT`).
pub const ABS_CNT: usize = 0x40;
/// `ABS_MT_SLOT`, the one axis `EVIOCSABS` may not set.
const ABS_MT_SLOT: usize = 0x2f;

/// How many codes each type has (`*_CNT` of `input-event-codes.h`), for
/// the bitmaps: `None` for a type `EVIOCGBIT` does not answer.
fn codes_of(kind: u16) -> Option<usize> {
    Some(match kind {
        EV_SYN => 0x20, // the types themselves
        EV_KEY => 0x300,
        EV_REL => 0x10,
        EV_ABS => ABS_CNT,
        EV_MSC => 0x08,
        EV_SW => 0x11,
        EV_LED => 0x10,
        EV_SND => 0x08,
        EV_FF => 0x80,
        _ => return None,
    })
}

/// The bytes of a bitmap of `codes` bits as Linux hands it out: whole
/// `long`s.
fn bitmap_len(codes: usize) -> usize {
    codes.div_ceil(64) * 8
}

fn bit(map: &[u8], code: usize) -> bool {
    map.get(code / 8)
        .is_some_and(|b| b & (1 << (code % 8)) != 0)
}

fn set_bit(map: &mut [u8], code: usize, on: bool) {
    if let Some(b) = map.get_mut(code / 8) {
        if on {
            *b |= 1 << (code % 8);
        } else {
            *b &= !(1 << (code % 8));
        }
    }
}

/// What the device is, as its driver found it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    /// Its serial number, `EVIOCGUNIQ`; empty when it has none.
    pub uniq: String,
    /// Where it is plugged, `EVIOCGPHYS`; empty when that says nothing.
    pub phys: String,
    /// `struct input_id`: bus type, vendor, product, version.
    pub id: [u16; 4],
    /// `INPUT_PROP_*`, as a bitmap.
    pub props: Vec<u8>,
    /// The codes of each type it sends, as a bitmap, for every type it
    /// has (one with none is still a type it has: `EV_REP`).
    pub bits: BTreeMap<u16, Vec<u8>>,
    /// Each absolute axis it has: value, minimum, maximum, fuzz, flat and
    /// resolution, `struct input_absinfo`.
    pub abs: BTreeMap<u16, [i32; 6]>,
}

impl Info {
    /// Whether it sends `code` of `kind`.
    pub fn sends(&self, kind: u16, code: u16) -> bool {
        kind == EV_SYN || self.bits.get(&kind).is_some_and(|m| bit(m, code as usize))
    }

    /// The bitmap `EVIOCGBIT(kind)` answers, Linux's length.
    fn bitmap(&self, kind: u16) -> Option<Vec<u8>> {
        let mut map = vec![0u8; bitmap_len(codes_of(kind)?)];
        if kind == EV_SYN {
            set_bit(&mut map, EV_SYN as usize, true);
            for k in self.bits.keys() {
                set_bit(&mut map, *k as usize, true);
            }
        } else if let Some(have) = self.bits.get(&kind) {
            let n = have.len().min(map.len());
            map[..n].copy_from_slice(&have[..n]);
        }
        Some(map)
    }
}

/// A time as a `timeval` holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time {
    pub sec: i64,
    pub usec: i64,
}

/// The clocks an open can have its events stamped with, now: monotonic,
/// then the wall clock.
pub type Clocks = fn() -> [Time; 2];

/// The two clocks, read.
pub fn now() -> [Time; 2] {
    let read = |id| {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a timespec to write, and the clock exists.
        unsafe { libc::clock_gettime(id, &mut ts) };
        Time {
            sec: ts.tv_sec,
            usec: ts.tv_nsec / 1000,
        }
    };
    [read(libc::CLOCK_MONOTONIC), read(libc::CLOCK_REALTIME)]
}

/// One event, stamped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub time: Time,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl Event {
    /// As `struct input_event` is laid out.
    pub fn to_bytes(&self) -> [u8; EVENT_LEN] {
        let mut b = [0u8; EVENT_LEN];
        b[0..8].copy_from_slice(&self.time.sec.to_ne_bytes());
        b[8..16].copy_from_slice(&self.time.usec.to_ne_bytes());
        b[16..18].copy_from_slice(&self.kind.to_ne_bytes());
        b[18..20].copy_from_slice(&self.code.to_ne_bytes());
        b[20..24].copy_from_slice(&self.value.to_ne_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Event {
        let w = |at: usize| i64::from_ne_bytes(b[at..at + 8].try_into().unwrap());
        Event {
            time: Time {
                sec: w(0),
                usec: w(8),
            },
            kind: u16::from_ne_bytes([b[16], b[17]]),
            code: u16::from_ne_bytes([b[18], b[19]]),
            value: i32::from_ne_bytes([b[20], b[21], b[22], b[23]]),
        }
    }
}

/// One open of the node.
#[derive(Debug)]
struct Client {
    queue: VecDeque<Event>,
    /// How many events at the front of the queue are of frames a
    /// `SYN_REPORT` closed: what can be read (`packet_head`).
    packet: usize,
    /// Stamped with the wall clock rather than the monotonic one.
    realtime: bool,
    /// `EVIOCREVOKE`d: nothing more for it, ever.
    revoked: bool,
    /// The readiness the kernel was last told of.
    said: u64,
}

impl Client {
    fn new() -> Client {
        Client {
            queue: VecDeque::with_capacity(QUEUE_LEN),
            packet: 0,
            realtime: false,
            revoked: false,
            said: POLLOUT,
        }
    }

    fn readiness(&self) -> u64 {
        if self.revoked {
            POLLHUP | POLLERR
        } else if self.packet > 0 {
            POLLIN | POLLOUT
        } else {
            POLLOUT
        }
    }

    /// `__pass_event`: one event in, the queue thrown away when it is full.
    fn push(&mut self, e: Event) {
        let closes = e.kind == EV_SYN && e.code == SYN_REPORT;
        // A frame with nothing in it is no frame.
        if closes && self.packet == self.queue.len() {
            return;
        }
        self.queue.push_back(e);
        if self.queue.len() > QUEUE_LEN {
            let newest = self.queue.pop_back().expect("just pushed");
            self.queue.clear();
            self.queue.push_back(Event {
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
                ..newest
            });
            self.queue.push_back(newest);
            self.packet = 0;
            // A SYN_REPORT that overflows closes the frame of what is left.
            if closes {
                self.packet = self.queue.len();
            }
            return;
        }
        if closes {
            self.packet = self.queue.len();
        }
    }

    /// The clock changed: the queue holds times of the other one, so it
    /// goes, and the open is told.
    fn drop_queue(&mut self, time: Time) {
        if self.queue.is_empty() {
            return;
        }
        self.queue.clear();
        self.packet = 0;
        self.queue.push_back(Event {
            time,
            kind: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        });
    }
}

/// An ioctl request, as `<asm-generic/ioctl.h>` packs it.
struct Request {
    dir: u32,
    typ: u32,
    nr: u32,
    size: usize,
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

impl Request {
    fn decode(cmd: u32) -> Request {
        Request {
            dir: cmd >> 30,
            typ: (cmd >> 8) & 0xff,
            nr: cmd & 0xff,
            size: ((cmd >> 16) & 0x3fff) as usize,
        }
    }
}

/// `EVIOCGRAB` and `EVIOCREVOKE`, which say they carry an `int` in and take
/// it by value.
fn by_value(cmd: u32) -> bool {
    let r = Request::decode(cmd);
    r.typ == b'E' as u32 && matches!(r.nr, 0x90 | 0x91)
}

/// The device, with every open of it.
pub struct Evdev {
    info: Info,
    /// What is down, lit, sounding and switched on now.
    key: Vec<u8>,
    led: Vec<u8>,
    snd: Vec<u8>,
    sw: Vec<u8>,
    /// `EVIOCGREP`/`EVIOCSREP`: autorepeat delay and period, in ms. The
    /// reader repeats keys, not this.
    rep: [u32; 2],
    clients: BTreeMap<u32, Client>,
    next_fh: u32,
    /// The open that `EVIOCGRAB` gave every event to.
    grab: Option<u32>,
    clocks: Clocks,
    /// The time of the frame being put together, both clocks: taken at its
    /// first event and kept to its `SYN_REPORT`.
    frame: Option<[Time; 2]>,
    /// Readiness that changed, for the kernel: `(fh, mask)`.
    told: Vec<(u32, u64)>,
    /// What was written for the device itself: LEDs and sounds.
    outgoing: Vec<(u16, u16, i32)>,
    /// Where that goes, when it is served ([`Evdev::send_with`]).
    sink: Option<Sink>,
}

/// What takes the events written for the device.
pub type Sink = Box<dyn FnMut(&[(u16, u16, i32)]) + Send>;

impl Evdev {
    pub fn new(info: Info, clocks: Clocks) -> Evdev {
        let map = |kind| vec![0u8; bitmap_len(codes_of(kind).unwrap())];
        Evdev {
            key: map(EV_KEY),
            led: map(EV_LED),
            snd: map(EV_SND),
            sw: map(EV_SW),
            info,
            rep: [250, 33],
            clients: BTreeMap::new(),
            next_fh: 1,
            grab: None,
            clocks,
            frame: None,
            told: Vec::new(),
            outgoing: Vec::new(),
            sink: None,
        }
    }

    /// Hand what programs write for the device to `sink`, as they write
    /// it, instead of keeping it for [`Evdev::take_outgoing`].
    pub fn send_with(&mut self, sink: Sink) {
        self.sink = Some(sink);
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Whether key `code` is down now: a driver whose device repeats a
    /// held key by sending it again says so with value 2.
    pub fn is_down(&self, code: u16) -> bool {
        bit(&self.key, code as usize)
    }

    /// The readiness that changed since the last call, to tell the kernel.
    pub fn take_told(&mut self) -> Vec<(u32, u64)> {
        core::mem::take(&mut self.told)
    }

    /// What was written for the device: `(type, code, value)`.
    pub fn take_outgoing(&mut self) -> Vec<(u16, u16, i32)> {
        core::mem::take(&mut self.outgoing)
    }

    fn settle(&mut self) {
        for (fh, c) in self.clients.iter_mut() {
            let now = c.readiness();
            if now != c.said {
                c.said = now;
                self.told.push((*fh, now));
            }
        }
    }

    /// One event of the device's, as `input_handle_event` takes it: what
    /// changes nothing is dropped, what is left goes to every reader (to
    /// the one that grabbed the device, if one did). Whether it was passed.
    pub fn feed(&mut self, kind: u16, code: u16, value: i32) -> bool {
        if !self.info.sends(kind, code) {
            return false;
        }
        let c = code as usize;
        let changes = match kind {
            EV_SYN => true,
            EV_KEY => {
                // 2 is the device's own autorepeat: passed while down.
                let down = bit(&self.key, c);
                let pass = if value == 2 {
                    down
                } else {
                    down != (value != 0)
                };
                if pass && value != 2 {
                    set_bit(&mut self.key, c, value != 0);
                }
                pass
            }
            EV_LED | EV_SND | EV_SW => {
                let map = match kind {
                    EV_LED => &mut self.led,
                    EV_SND => &mut self.snd,
                    _ => &mut self.sw,
                };
                let pass = bit(map, c) != (value != 0);
                set_bit(map, c, value != 0);
                pass
            }
            EV_ABS => match self.info.abs.get_mut(&code) {
                Some(info) if info[0] != value => {
                    info[0] = value;
                    true
                }
                Some(_) => false,
                None => true,
            },
            // Relative moves of zero say nothing.
            EV_REL => value != 0,
            _ => true,
        };
        if !changes {
            return false;
        }
        let times = *self.frame.get_or_insert_with(self.clocks);
        let closes = kind == EV_SYN && code == SYN_REPORT;
        for (fh, client) in self.clients.iter_mut() {
            if client.revoked || self.grab.is_some_and(|g| g != *fh) {
                continue;
            }
            client.push(Event {
                time: times[client.realtime as usize],
                kind,
                code,
                value,
            });
        }
        if closes {
            self.frame = None;
            self.settle();
        }
        true
    }

    /// A new open: its handle and readiness.
    pub fn open(&mut self) -> (u32, u64) {
        let fh = self.next_fh;
        self.next_fh = self.next_fh.wrapping_add(1).max(1);
        let c = Client::new();
        let mask = c.readiness();
        self.clients.insert(fh, c);
        (fh, mask)
    }

    pub fn release(&mut self, fh: u32) {
        self.clients.remove(&fh);
        if self.grab == Some(fh) {
            self.grab = None;
        }
    }

    /// `evdev_read`: whole events of closed frames, as many as fit.
    pub fn read(&mut self, fh: u32, buf: &mut [u8]) -> Result<usize, i32> {
        if !buf.is_empty() && buf.len() < EVENT_LEN {
            return Err(EINVAL);
        }
        let c = self.clients.get_mut(&fh).ok_or(EINVAL)?;
        if c.revoked {
            return Err(ENODEV);
        }
        if c.packet == 0 {
            return Err(EAGAIN);
        }
        let mut n = 0;
        while n + EVENT_LEN <= buf.len() && c.packet > 0 {
            let e = c.queue.pop_front().expect("packet counts queued events");
            buf[n..n + EVENT_LEN].copy_from_slice(&e.to_bytes());
            n += EVENT_LEN;
            c.packet -= 1;
        }
        self.settle();
        Ok(n)
    }

    /// `evdev_write`: events, injected as if the device had sent them.
    pub fn write(&mut self, fh: u32, data: &[u8]) -> Result<usize, i32> {
        let c = self.clients.get(&fh).ok_or(EINVAL)?;
        if c.revoked {
            return Err(ENODEV);
        }
        if data.len() < EVENT_LEN {
            return Err(EINVAL);
        }
        let whole = data.len() / EVENT_LEN * EVENT_LEN;
        for raw in data[..whole].chunks(EVENT_LEN) {
            let e = Event::from_bytes(raw);
            if self.feed(e.kind, e.code, e.value) && matches!(e.kind, EV_LED | EV_SND) {
                self.outgoing.push((e.kind, e.code, e.value));
            }
        }
        self.settle();
        Ok(whole)
    }

    /// `evdev_do_ioctl`: `data` holds what `cmd` carries in, and gets what
    /// it carries out; the return value and how many bytes of `data` that
    /// is.
    pub fn ioctl(
        &mut self,
        fh: u32,
        cmd: u32,
        arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        let c = self.clients.get_mut(&fh).ok_or(EINVAL)?;
        if c.revoked {
            return Err(ENODEV);
        }
        let Request { dir, typ, nr, size } = Request::decode(cmd);
        if typ != b'E' as u32 {
            return Err(EINVAL);
        }
        let int = |data: &[u8]| -> Result<i32, i32> {
            data.get(..4)
                .map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or(EINVAL)
        };
        match (dir, nr) {
            // EVIOCGRAB(int): every event to this open, or let go.
            (IOC_WRITE, 0x90) => {
                if arg != 0 {
                    if self.grab.is_some() {
                        return Err(EBUSY);
                    }
                    self.grab = Some(fh);
                } else if self.grab == Some(fh) {
                    self.grab = None;
                } else {
                    return Err(EINVAL);
                }
                Ok((0, 0))
            }
            // EVIOCREVOKE(int): this open is done with.
            (IOC_WRITE, 0x91) => {
                if arg != 0 {
                    return Err(EINVAL);
                }
                c.revoked = true;
                c.queue.clear();
                c.packet = 0;
                if self.grab == Some(fh) {
                    self.grab = None;
                }
                self.settle();
                Ok((0, 0))
            }
            // EVIOCSCLOCKID(int *).
            (IOC_WRITE, 0xa0) => {
                let realtime = match int(data)? {
                    0 => true,
                    1 | 7 => false, // CLOCK_MONOTONIC, CLOCK_BOOTTIME
                    _ => return Err(EINVAL),
                };
                if realtime != c.realtime {
                    c.realtime = realtime;
                    c.drop_queue((self.clocks)()[realtime as usize]);
                    self.settle();
                }
                Ok((0, 0))
            }
            // EVIOCSREP(unsigned int[2]).
            (IOC_WRITE, 0x03) => {
                let delay = int(data)?;
                let period = int(data.get(4..).unwrap_or(&[]))?;
                self.rep = [delay as u32, period as u32];
                Ok((0, 0))
            }
            // EVIOCSABS(abs): a new absinfo for an axis it has.
            (IOC_WRITE, 0xc0..=0xff) => {
                let axis = (nr - 0xc0) as u16;
                if axis as usize >= ABS_CNT || axis as usize == ABS_MT_SLOT || data.len() < 24 {
                    return Err(EINVAL);
                }
                let mut words = [0i32; 6];
                for (i, w) in words.iter_mut().enumerate() {
                    *w = int(&data[i * 4..])?;
                }
                match self.info.abs.get_mut(&axis) {
                    Some(info) => *info = words,
                    None => return Err(EINVAL),
                }
                Ok((0, 0))
            }
            (IOC_READ, _) => self.get(nr, size, data),
            _ => Err(EINVAL),
        }
    }

    /// The `EVIOCG*` requests: what goes back in `data`.
    fn get(&self, nr: u32, size: usize, data: &mut [u8]) -> Result<(i64, usize), i32> {
        let give = |data: &mut [u8], bytes: &[u8]| {
            let n = bytes.len().min(size).min(data.len());
            data[..n].copy_from_slice(&bytes[..n]);
            Ok((n as i64, n))
        };
        // `str_to_user`: the string and its NUL, cut to the buffer.
        let string = |data: &mut [u8], s: &str| {
            if s.is_empty() {
                return Err(ENOENT);
            }
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            give(data, &bytes)
        };
        match nr {
            0x01 => {
                data[..4].copy_from_slice(&VERSION.to_ne_bytes());
                Ok((0, 4))
            }
            0x02 => {
                for (i, w) in self.info.id.iter().enumerate() {
                    data[i * 2..i * 2 + 2].copy_from_slice(&w.to_ne_bytes());
                }
                Ok((0, 8))
            }
            0x03 => {
                data[..4].copy_from_slice(&self.rep[0].to_ne_bytes());
                data[4..8].copy_from_slice(&self.rep[1].to_ne_bytes());
                Ok((0, 8))
            }
            0x06 => string(data, &self.info.name),
            0x07 => string(data, &self.info.phys),
            0x08 => string(data, &self.info.uniq),
            0x09 => {
                let mut props = vec![0u8; bitmap_len(0x20)];
                let n = self.info.props.len().min(props.len());
                props[..n].copy_from_slice(&self.info.props[..n]);
                give(data, &props)
            }
            0x18 => give(data, &self.key),
            0x19 => give(data, &self.led),
            0x1a => give(data, &self.snd),
            0x1b => give(data, &self.sw),
            0x20..=0x3f => {
                let map = self.info.bitmap((nr - 0x20) as u16).ok_or(EINVAL)?;
                give(data, &map)
            }
            0x40..=0x7f => {
                let axis = (nr - 0x40) as u16;
                let words = self.info.abs.get(&axis).copied().unwrap_or_default();
                let mut bytes = [0u8; 24];
                for (i, w) in words.iter().enumerate() {
                    bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
                }
                let n = bytes.len().min(size).min(data.len());
                data[..n].copy_from_slice(&bytes[..n]);
                Ok((0, n))
            }
            _ => Err(EINVAL),
        }
    }
}

/// The major of input devices, and the minor of `event0`.
pub const INPUT_MAJOR: u32 = 13;
pub const EVENT_MINOR_BASE: u32 = 64;
/// How many `eventK` are tried.
pub const EVENT_NODES: u32 = 32;

/// Serve the first `/dev/input/eventK` nobody has: the kernel's own input
/// devices take theirs first, and other drivers theirs. The node and its
/// name under `/dev`.
pub fn serve_free() -> Result<(Node, String), String> {
    let mut last = String::new();
    for k in 0..EVENT_NODES {
        let name = format!("input/event{k}");
        match Node::serve(&name, INPUT_MAJOR, EVENT_MINOR_BASE + k, 0o660) {
            Ok(node) => return Ok((node, name)),
            Err(e) => last = e,
        }
    }
    Err(format!("no /dev/input/event* to serve: {last}"))
}

/// Tell the kernel what readiness changed.
pub fn tell(node: &Node, evdev: &mut Evdev) {
    for (fh, mask) in evdev.take_told() {
        node.ready(fh, mask);
    }
}

impl Device for Evdev {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        Ok(Evdev::open(self))
    }

    fn release(&mut self, fh: u32) {
        Evdev::release(self, fh)
    }

    fn read(&mut self, node: &Node, fh: u32, _offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let r = Evdev::read(self, fh, buf);
        tell(node, self);
        r
    }

    fn write(&mut self, node: &Node, fh: u32, _offset: u64, data: &[u8]) -> Result<usize, i32> {
        let r = Evdev::write(self, fh, data);
        tell(node, self);
        if let Some(sink) = self.sink.as_mut() {
            let out = core::mem::take(&mut self.outgoing);
            if !out.is_empty() {
                sink(&out);
            }
        }
        r
    }

    fn ioctl(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        let r = Evdev::ioctl(self, fh, cmd, arg, data);
        tell(node, self);
        r
    }

    fn by_value(&self, cmd: u32) -> bool {
        by_value(cmd)
    }
}

#[cfg(test)]
mod tests;
