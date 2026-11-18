//! A virtio input device (virtio 1.1 §5.8): a keyboard, a mouse or a
//! tablet, which says what it is in its configuration and sends its events
//! on one queue. The driver hands them to [`crate::evdev`], which is what
//! programs read of `/dev/input/event*`.
//!
//! The event queue is filled with buffers of one event each, given back
//! to the device as soon as the event is read out. The status queue takes
//! what is written for the device: a keyboard's LEDs.

use crate::claim::{self, Dma};
use crate::evdev::{Info, EV_ABS};
use crate::queue::{Answers, Desc, Offers, Queue, DESC_F_WRITE};
use crate::virtio::{self, Device, Setup};
use crate::zx::{Handle, Status};
use crate::PAGE;

/// `virtio_input_config.select` (§5.8.5.1).
pub const CFG_ID_NAME: u8 = 0x01;
pub const CFG_ID_SERIAL: u8 = 0x02;
pub const CFG_ID_DEVIDS: u8 = 0x03;
pub const CFG_PROP_BITS: u8 = 0x10;
pub const CFG_EV_BITS: u8 = 0x11;
pub const CFG_ABS_INFO: u8 = 0x12;
/// Where `select`, `subsel`, `size` and the answer are in the
/// configuration.
const CFG_SELECT: u32 = 0;
const CFG_SUBSEL: u32 = 1;
const CFG_SIZE: u32 = 2;
const CFG_DATA: u32 = 8;
/// The most an answer holds.
const CFG_DATA_LEN: usize = 128;

/// `BUS_VIRTUAL`, for a device that says nothing of its ids.
const BUS_VIRTUAL: u16 = 0x06;

/// The queues, by index.
const EVENTQ: usize = 0;
const STATUSQ: usize = 1;
/// Descriptors per queue, each a buffer of one event.
pub const QUEUE_SIZE: u16 = 64;
/// `struct virtio_input_event`: type, code, value, little-endian.
pub const EVENT_LEN: usize = 8;
/// The DMA memory: each queue's two pages, then the buffers of the event
/// queue and those of the status queue.
const QUEUE_PAGES: usize = 2;
const BUFFERS: usize = 2 * QUEUE_PAGES * PAGE;
const STATUS_BUFFERS: usize = BUFFERS + QUEUE_SIZE as usize * EVENT_LEN;
const DMA_SIZE: usize = BUFFERS + PAGE;

const _: () = assert!(STATUS_BUFFERS + QUEUE_SIZE as usize * EVENT_LEN <= DMA_SIZE);

/// What a configuration select answers.
pub trait Config {
    /// The bytes of `select`, `subsel`: none when it has nothing there.
    fn select(&self, select: u8, subsel: u8) -> Vec<u8>;
}

impl Config for Device {
    fn select(&self, select: u8, subsel: u8) -> Vec<u8> {
        self.set_config8(CFG_SELECT, select);
        self.set_config8(CFG_SUBSEL, subsel);
        let size = (self.config8(CFG_SIZE) as usize).min(CFG_DATA_LEN);
        (0..size)
            .map(|i| self.config8(CFG_DATA + i as u32))
            .collect()
    }
}

fn text(bytes: Vec<u8>) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// What the device says it is. `phys` is where it is, for `EVIOCGPHYS`.
pub fn info(cfg: &impl Config, phys: String) -> Info {
    let le16 = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let le32 = |b: &[u8], i: usize| {
        i32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
    };
    let ids = cfg.select(CFG_ID_DEVIDS, 0);
    let mut info = Info {
        name: text(cfg.select(CFG_ID_NAME, 0)),
        uniq: text(cfg.select(CFG_ID_SERIAL, 0)),
        phys,
        id: if ids.len() >= 8 {
            [le16(&ids, 0), le16(&ids, 1), le16(&ids, 2), le16(&ids, 3)]
        } else {
            [BUS_VIRTUAL, 0, 0, 0]
        },
        props: cfg.select(CFG_PROP_BITS, 0),
        ..Info::default()
    };
    for kind in 1..0x20u8 {
        let bits = cfg.select(CFG_EV_BITS, kind);
        if !bits.is_empty() {
            info.bits.insert(kind as u16, bits);
        }
    }
    let axes = info.bits.get(&EV_ABS).cloned().unwrap_or_default();
    for axis in 0..(axes.len() * 8).min(crate::evdev::ABS_CNT) {
        if axes[axis / 8] & (1 << (axis % 8)) == 0 {
            continue;
        }
        let a = cfg.select(CFG_ABS_INFO, axis as u8);
        if a.len() >= 20 {
            // min, max, fuzz, flat, res; the value starts at zero.
            let w = [
                0,
                le32(&a, 0),
                le32(&a, 1),
                le32(&a, 2),
                le32(&a, 3),
                le32(&a, 4),
            ];
            info.abs.insert(axis as u16, w);
        }
    }
    info
}

/// One event as the device sends it: type, code, value.
pub fn event_of(b: &[u8]) -> (u16, u16, i32) {
    (
        u16::from_le_bytes([b[0], b[1]]),
        u16::from_le_bytes([b[2], b[3]]),
        i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    )
}

/// The event queue: every buffer is the device's but the ones it has
/// filled and this has not read yet.
pub struct Events {
    offers: Offers,
    answers: Answers,
    buffers: *mut u8,
}

// SAFETY: the queue and its buffers are this one's alone (`drive`).
unsafe impl Send for Events {}

impl Events {
    /// # Safety
    /// `queue` and the `QUEUE_SIZE` buffers at `buffers` (the device sees
    /// them at `daddr`) are this one's alone, mapped for good.
    unsafe fn new(queue: Queue, buffers: *mut u8, daddr: u64) -> Events {
        let (mut offers, answers) = queue.split();
        for i in 0..QUEUE_SIZE {
            offers.set_desc(
                i,
                Desc {
                    addr: daddr + (i as usize * EVENT_LEN) as u64,
                    len: EVENT_LEN as u32,
                    flags: DESC_F_WRITE,
                    next: 0,
                },
            );
            offers.offer(i, i);
        }
        offers.publish(QUEUE_SIZE);
        Events {
            offers,
            answers,
            buffers,
        }
    }

    /// Every event the device has sent since the last call, each to `f`,
    /// and its buffer given back. How many.
    pub fn drain(&mut self, device: &Device, mut f: impl FnMut(u16, u16, i32)) -> usize {
        let mut n = 0u16;
        while let Some((id, _len)) = self.answers.take_used() {
            let id = (id as u16) % QUEUE_SIZE;
            // SAFETY: buffer `id` of the `QUEUE_SIZE` this queue has, which
            // the device has filled and handed back.
            let b = unsafe {
                core::slice::from_raw_parts(self.buffers.add(id as usize * EVENT_LEN), EVENT_LEN)
            };
            let (kind, code, value) = event_of(b);
            f(kind, code, value);
            self.offers.offer(n, id);
            n += 1;
        }
        if n > 0 {
            self.offers.publish(n);
            device.notify(EVENTQ);
        }
        n as usize
    }
}

/// The status queue: what is written for the device.
pub struct StatusQueue {
    offers: Offers,
    answers: Answers,
    buffers: *mut u8,
    free: Vec<u16>,
}

// SAFETY: as [`Events`].
unsafe impl Send for StatusQueue {}

impl StatusQueue {
    /// # Safety
    /// As [`Events::new`].
    unsafe fn new(queue: Queue, buffers: *mut u8, daddr: u64) -> StatusQueue {
        let (mut offers, answers) = queue.split();
        for i in 0..QUEUE_SIZE {
            offers.set_desc(
                i,
                Desc {
                    addr: daddr + (i as usize * EVENT_LEN) as u64,
                    len: EVENT_LEN as u32,
                    flags: 0,
                    next: 0,
                },
            );
        }
        StatusQueue {
            offers,
            answers,
            buffers,
            free: (0..QUEUE_SIZE).rev().collect(),
        }
    }

    /// Give the device `events`. One the queue has no room for is dropped:
    /// a LED is said again the next time it changes.
    pub fn send(&mut self, device: &Device, events: &[(u16, u16, i32)]) {
        while let Some((id, _)) = self.answers.take_used() {
            self.free.push(id as u16 % QUEUE_SIZE);
        }
        let mut n = 0u16;
        for (kind, code, value) in events {
            let Some(id) = self.free.pop() else { break };
            let mut b = [0u8; EVENT_LEN];
            b[0..2].copy_from_slice(&kind.to_le_bytes());
            b[2..4].copy_from_slice(&code.to_le_bytes());
            b[4..8].copy_from_slice(&value.to_le_bytes());
            // SAFETY: buffer `id`, which the device does not hold.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    b.as_ptr(),
                    self.buffers.add(id as usize * EVENT_LEN),
                    EVENT_LEN,
                )
            };
            self.offers.offer(n, id);
            n += 1;
        }
        if n > 0 {
            self.offers.publish(n);
            device.notify(STATUSQ);
        }
    }
}

/// A device claimed and running.
pub struct Driven {
    pub device: Device,
    pub events: Events,
    pub status: StatusQueue,
    pub info: Info,
    /// Its interrupt, `None` when the device has none to give: then the
    /// queue is looked at every few milliseconds.
    pub interrupt: Option<Handle>,
    pub dma: Dma,
}

/// Why [`drive`] has no device.
#[derive(Debug)]
pub enum NoDevice {
    /// The Zircon gateway is off.
    NoGateway,
    Other(String),
}

/// Claim virtio input device `index` and start it, with every buffer of
/// the event queue given to it.
pub fn drive(index: usize) -> Result<Driven, NoDevice> {
    let other = |e: String| NoDevice::Other(e);
    let claim = match claim::claim(claim::KIND_VIRTIO_INPUT, index, 300) {
        Ok(claim) => claim,
        Err(Status::NO_GATEWAY) => return Err(NoDevice::NoGateway),
        Err(e) => {
            return Err(other(format!(
                "claim (boot with udriver=virtio-input@N and a device there): {e}"
            )))
        }
    };
    let Some(transport) = claim.transport else {
        return Err(other("unknown transport".into()));
    };
    let regs = claim.map_regs().map_err(other)?;
    let dma = Dma::new(claim.bti, DMA_SIZE, None).map_err(other)?;
    // SAFETY: `regs` is the claimed device's window, mapped for good.
    let mut device = unsafe { Device::new(regs, transport) };
    let irq = claim.interrupt.is_some();
    let queue_at = |q: usize| q * QUEUE_PAGES * PAGE;
    let up = device
        .bring_up(&Setup {
            device_id: virtio::ID_INPUT,
            want: 0,
            need: 0,
            queue_size: QUEUE_SIZE,
            queues: &[
                (dma.daddr + queue_at(EVENTQ) as u64, irq),
                (dma.daddr + queue_at(STATUSQ) as u64, false),
            ],
        })
        .map_err(other)?;
    let info = info(&device, format!("virtio{index}/input0"));
    // SAFETY: each queue's two pages and buffers of the DMA memory, zeroed,
    // mapped for good and nobody else's.
    let (events, status) = unsafe {
        (
            Events::new(
                Queue::new(dma.ptr.add(queue_at(EVENTQ)), QUEUE_SIZE),
                dma.ptr.add(BUFFERS),
                dma.daddr + BUFFERS as u64,
            ),
            StatusQueue::new(
                Queue::new(dma.ptr.add(queue_at(STATUSQ)), QUEUE_SIZE),
                dma.ptr.add(STATUS_BUFFERS),
                dma.daddr + STATUS_BUFFERS as u64,
            ),
        )
    };
    device.driver_ok();
    device.notify(EVENTQ);
    Ok(Driven {
        device,
        events,
        status,
        info,
        interrupt: if up.irq_refused {
            None
        } else {
            claim.interrupt
        },
        dma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev::{EV_KEY, EV_LED, EV_REP};
    use crate::queue::tests::Pages;
    use std::collections::BTreeMap;

    /// A device's configuration, by select and subsel.
    struct Fake(BTreeMap<(u8, u8), Vec<u8>>);
    impl Config for Fake {
        fn select(&self, select: u8, subsel: u8) -> Vec<u8> {
            self.0.get(&(select, subsel)).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn a_keyboard_says_its_name_ids_and_codes() {
        let mut c = BTreeMap::new();
        c.insert((CFG_ID_NAME, 0), b"QEMU Virtio Keyboard".to_vec());
        c.insert(
            (CFG_ID_DEVIDS, 0),
            vec![0x06, 0, 0x27, 0x06, 0x01, 0, 0x01, 0],
        );
        c.insert((CFG_EV_BITS, EV_KEY as u8), vec![0, 0, 0, 0x40]); // KEY_A
        c.insert((CFG_EV_BITS, EV_LED as u8), vec![0x07]);
        c.insert((CFG_EV_BITS, EV_REP as u8), vec![0]);
        let info = info(&Fake(c), "virtio0/input0".into());
        assert_eq!(info.name, "QEMU Virtio Keyboard");
        assert_eq!(info.id, [6, 0x0627, 1, 1]);
        assert_eq!(info.phys, "virtio0/input0");
        assert!(info.uniq.is_empty());
        assert!(info.sends(EV_KEY, 30));
        assert!(!info.sends(EV_KEY, 31));
        assert_eq!(
            info.bits.keys().copied().collect::<Vec<_>>(),
            [EV_KEY, EV_LED, EV_REP]
        );
        assert!(info.abs.is_empty());
    }

    #[test]
    fn a_tablet_says_the_range_of_each_axis() {
        let mut c = BTreeMap::new();
        c.insert((CFG_ID_NAME, 0), b"QEMU Virtio Tablet\0junk".to_vec());
        c.insert((CFG_EV_BITS, EV_ABS as u8), vec![0x03]); // ABS_X, ABS_Y
        let range = |max: i32| {
            let mut v = Vec::new();
            for w in [0, max, 0, 0, 0] {
                v.extend_from_slice(&w.to_le_bytes());
            }
            v
        };
        c.insert((CFG_ABS_INFO, 0), range(32767));
        c.insert((CFG_ABS_INFO, 1), range(16383));
        let info = info(&Fake(c), String::new());
        assert_eq!(info.name, "QEMU Virtio Tablet", "up to the NUL");
        assert_eq!(info.id, [BUS_VIRTUAL, 0, 0, 0], "no ids said");
        assert_eq!(info.abs[&0], [0, 0, 32767, 0, 0, 0]);
        assert_eq!(info.abs[&1][2], 16383);
    }

    #[test]
    fn an_event_is_read_little_endian() {
        assert_eq!(
            event_of(&[1, 0, 30, 0, 0xff, 0xff, 0xff, 0xff]),
            (1, 30, -1)
        );
    }

    #[test]
    fn every_buffer_of_the_event_queue_is_the_devices_from_the_start() {
        let mut mem = Pages::new(3);
        let ptr = mem.ptr();
        // SAFETY: three zeroed pages: the queue's two, then the buffers.
        let ev = unsafe { Events::new(Queue::new(ptr, QUEUE_SIZE), ptr.add(2 * PAGE), 0x10_0000) };
        assert_eq!(ev.offers.avail_idx(), QUEUE_SIZE);
        let d = ev.offers.desc(5);
        assert_eq!((d.addr, d.len, d.flags), (0x10_0000 + 40, 8, DESC_F_WRITE));
    }
}
