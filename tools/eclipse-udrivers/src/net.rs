//! A virtio network card, served to the kernel or to a process.
//!
//! The card becomes an interface of the kernel's
//! (`loader/src/linux/net_serve.rs` is the other end), or of a network
//! stack that runs as a process (`tools/eclipse-netstack`), which gets what
//! the kernel would have been given in an [`Offer`]. Either way the other
//! end is called the kernel below: the driver cannot tell and does the
//! same. Frames pass through
//! the window -- a slice of the driver's DMA memory where the card's
//! buffers already are, a slot each -- and a FIFO of 16-byte messages says
//! which: `op, slot, len, off`.
//!
//! - [`OP_RX`], to the kernel: a frame of `len` bytes is in receive slot
//!   `slot`, `off` bytes in. The kernel sends the message back when it has
//!   copied the frame out, and the slot goes back to the card.
//! - [`OP_TX`], from the kernel: a frame of `len` bytes is in transmit slot
//!   `slot`, [`TX_HEADROOM`] bytes in. It goes back once the card has taken
//!   the frame, and the kernel may use the slot again.
//!
//! Two threads, each with one side of both queues: the one that sleeps on
//! the card's interrupt reads the used rings and writes to the FIFO
//! ([`Card`]), and the one that sleeps on the FIFO fills the available
//! rings ([`Feeder`]). Neither can find the FIFO full: it holds as many
//! messages as there are slots.
//!
//! How the two threads are created matters, because the kernel's scheduler
//! comes to them in that order. Measured on 32 MiB downloads over TCP at
//! one CPU, against the C driver this one replaced (1.83 s):
//!
//! - the card's thread and then the feeder's, both started by a main thread
//!   that does nothing else, as the C driver has them: 1.84 s. The card's
//!   thread finds the next burst waiting when it has passed one on, four
//!   times out of five, and does not sleep. This is what
//!   `udrv-virtio-net` does;
//! - the card's side on the main thread and the feeder's started from it:
//!   3 to 4 % slower, because one download in three falls into short
//!   bursts (two thousand trips for the frames instead of five hundred)
//!   where one in seven does above;
//! - the feeder's side first: the card's thread sleeps on the interrupt at
//!   every burst. Never a short burst, and always 1.87 s.

use core::ptr;

use crate::claim;
use crate::fifo::{Fifo, Message};
use crate::queue::{Answers, Desc, Offers, DESC_F_WRITE};
use crate::virtio::{Device, Transport};
use crate::zx::{Handle, Status};
use crate::PAGE;

/// Feature bit 5: the card has a MAC address of its own. The driver needs
/// it: the kernel knows the interface by it, across drivers.
pub const F_MAC: u32 = 1 << 5;

/// Descriptors per queue.
pub const QUEUE_SIZE: u16 = 64;
/// The queues: what the card receives, what it is to send.
pub const RXQ: usize = 0;
pub const TXQ: usize = 1;

/// As many receive buffers as the queue has descriptors. The card fills
/// what it is given in one go and the kernel takes the frames in one go, so
/// the ring is the size of a burst: with 64 instead of 32 a 32 MiB download
/// over TCP at one CPU takes 1.79 s instead of 2.08 s.
pub const RX_BUFS: usize = 64;
pub const TX_BUFS: usize = 32;
/// A slot: a header and a 1514-byte frame, with room.
pub const BUF_SIZE: usize = 2048;
/// Where a frame starts in a transmit slot: the header goes before it.
pub const TX_HEADROOM: usize = 16;
/// The longest frame: 1500 bytes and an Ethernet header.
pub const FRAME_MAX: usize = 1514;

/// The DMA area, one contiguous VMO: each queue's two pages, then the
/// window -- the receive slots and the transmit slots -- which is all of
/// the driver's memory the kernel reaches.
pub const DMA_WIN: usize = 4 * PAGE;
pub const WIN_SIZE: usize = (RX_BUFS + TX_BUFS) * BUF_SIZE;
pub const DMA_SIZE: usize = DMA_WIN + WIN_SIZE;
// A descriptor per receive buffer and per transmit buffer.
const _: () = assert!(RX_BUFS <= QUEUE_SIZE as usize);
const _: () = assert!(TX_BUFS <= QUEUE_SIZE as usize);
const _: () = assert!(WIN_SIZE.is_multiple_of(PAGE));

/// Where queue `q`'s two pages are in the DMA area.
pub const fn dma_queue(q: usize) -> usize {
    q * 2 * PAGE
}

/// The operations of a [`Msg`].
pub const OP_RX: u32 = 1;
pub const OP_TX: u32 = 2;
/// Messages the FIFO holds each way: one per slot, and room.
pub const FIFO_DEPTH: usize = 128;
const SLOTS: usize = RX_BUFS + TX_BUFS;
const _: () = assert!(SLOTS <= FIFO_DEPTH);

/// One FIFO element.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Msg {
    pub op: u32,
    pub slot: u32,
    /// The frame's bytes.
    pub len: u32,
    /// Where it starts in the slot.
    pub off: u32,
}
const _: () = assert!(core::mem::size_of::<Msg>() == 16);
// SAFETY: `repr(C)`, four `u32`s.
unsafe impl Message for Msg {}

/// The header the card puts before every frame, and wants before every
/// frame it sends: ten bytes on a legacy device, twelve on a modern one
/// (`num_buffers`, which only means something with a feature not taken).
pub const fn header_len(transport: Transport) -> usize {
    match transport {
        Transport::Mmio => 10,
        Transport::Pci { .. } => 12,
    }
}
const _: () = assert!(TX_HEADROOM >= 12);

/// What a driver started with a channel (`udrv-virtio-net N stack=FD`)
/// sends down it once the card is up, in place of `eclipse_net_serve`: these
/// sixteen bytes, and two handles -- the far end of the FIFO, then the
/// window. Whoever reads it is to the driver what the kernel would have
/// been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// The card's address, as [`mac_word`] packs it.
    pub mac: u64,
    /// The window's slots: this many for frames coming in, then this many
    /// for frames going out, [`BUF_SIZE`] bytes each.
    pub rx_slots: u32,
    pub tx_slots: u32,
}

impl Offer {
    /// The bytes of an offer.
    pub const LEN: usize = 16;
    /// The handles that go with one.
    pub const HANDLES: usize = 2;

    /// What this driver offers of the card with address `mac`.
    pub fn of(mac: [u8; 6]) -> Offer {
        Offer {
            mac: mac_word(mac),
            rx_slots: RX_BUFS as u32,
            tx_slots: TX_BUFS as u32,
        }
    }

    pub fn to_bytes(&self) -> [u8; Offer::LEN] {
        let mut b = [0u8; Offer::LEN];
        b[0..8].copy_from_slice(&self.mac.to_ne_bytes());
        b[8..12].copy_from_slice(&self.rx_slots.to_ne_bytes());
        b[12..16].copy_from_slice(&self.tx_slots.to_ne_bytes());
        b
    }

    /// The offer in `bytes`, when they are exactly one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Offer> {
        let b: &[u8; Offer::LEN] = bytes.try_into().ok()?;
        Some(Offer {
            mac: u64::from_ne_bytes(b[0..8].try_into().ok()?),
            rx_slots: u32::from_ne_bytes(b[8..12].try_into().ok()?),
            tx_slots: u32::from_ne_bytes(b[12..16].try_into().ok()?),
        })
    }

    /// The card's address, its six bytes.
    pub fn mac_bytes(&self) -> [u8; 6] {
        core::array::from_fn(|i| (self.mac >> (8 * i)) as u8)
    }
}

/// The address as the kernel takes it: the first byte in the low bits.
pub fn mac_word(mac: [u8; 6]) -> u64 {
    mac.iter()
        .enumerate()
        .fold(0, |word, (i, &b)| word | (b as u64) << (8 * i))
}

/// Receive slot `slot`, whole, for the card to write a frame into.
pub fn rx_desc(daddr: u64, slot: u16) -> Desc {
    Desc {
        addr: daddr + (DMA_WIN + slot as usize * BUF_SIZE) as u64,
        len: BUF_SIZE as u32,
        flags: DESC_F_WRITE,
        next: 0,
    }
}

/// Where transmit slot `slot` starts in the DMA area.
const fn tx_slot(slot: u16) -> usize {
    DMA_WIN + (RX_BUFS + slot as usize) * BUF_SIZE
}

/// The `len`-byte frame in transmit slot `slot`, with the `header` bytes
/// before it, for the card to send.
pub fn tx_desc(daddr: u64, slot: u16, len: u32, header: usize) -> Desc {
    Desc {
        addr: daddr + (tx_slot(slot) + TX_HEADROOM - header) as u64,
        len: header as u32 + len,
        flags: 0,
        next: 0,
    }
}

/// What a message from the kernel asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The frame in this receive slot has been copied out: the slot can go
    /// back to the card.
    Refill(u16),
    /// Send the frame of this many bytes in this transmit slot.
    Send(u16, u32),
}

/// A message from the kernel, checked: nothing reaches the card that is not
/// one of its slots and a frame that fits it. `None` for a message that
/// means nothing.
pub fn ask(m: &Msg) -> Option<Ask> {
    match m.op {
        OP_RX if (m.slot as usize) < RX_BUFS => Some(Ask::Refill(m.slot as u16)),
        OP_TX
            if (m.slot as usize) < TX_BUFS
                && m.len as usize <= FRAME_MAX
                && m.off as usize == TX_HEADROOM =>
        {
            Some(Ask::Send(m.slot as u16, m.len))
        }
        _ => None,
    }
}

/// The message for a receive buffer the card gave back: `id` is its slot
/// and `len` what the card wrote, header included. `None` when it is not
/// one of the driver's buffers or does not hold a header.
pub fn received(id: u32, len: u32, header: usize) -> Option<Msg> {
    if id as usize >= RX_BUFS || (len as usize) < header || len as usize > BUF_SIZE {
        return None;
    }
    Some(Msg {
        op: OP_RX,
        slot: id,
        len: len - header as u32,
        off: header as u32,
    })
}

/// The side that gives the card its buffers: the receive slots the kernel
/// has emptied and the frames it wants sent.
pub struct Feeder {
    rx: Offers,
    tx: Offers,
    dma: *mut u8,
    daddr: u64,
    header: usize,
}

// SAFETY: the feeder has its halves of the queues to itself and is the only
// writer of the headers in the transmit slots, on whichever thread it runs.
unsafe impl Send for Feeder {}

impl Feeder {
    /// # Safety
    /// `dma` is [`DMA_SIZE`] bytes the device sees at `daddr`, mapped for as
    /// long as the `Feeder` lives; `rx` and `tx` are the offering halves of
    /// the queues at [`dma_queue`] in it.
    pub unsafe fn new(rx: Offers, tx: Offers, dma: *mut u8, daddr: u64, header: usize) -> Feeder {
        assert!(header <= TX_HEADROOM);
        Feeder {
            rx,
            tx,
            dma,
            daddr,
            header,
        }
    }

    /// Give the card every receive slot: what it starts with.
    pub fn fill(&mut self, device: &Device) {
        for slot in 0..RX_BUFS as u16 {
            self.rx.set_desc(slot, rx_desc(self.daddr, slot));
            self.rx.offer(slot, slot);
        }
        self.rx.publish(RX_BUFS as u16);
        device.notify(RXQ);
    }

    /// Give the card what `msgs` ask, each as it is read: whether there were
    /// receive slots among them, and frames. The card is not told
    /// ([`Device::notify`]) here. The message that means nothing, if there
    /// is one, ends it.
    ///
    /// Each buffer is the card's at once, as in the C driver. Publishing
    /// them all at the end was measured and is no faster.
    fn give(&mut self, msgs: &[Msg]) -> Result<(bool, bool), Msg> {
        let (mut rx, mut tx) = (false, false);
        for m in msgs {
            match ask(m).ok_or(*m)? {
                Ask::Refill(slot) => {
                    self.rx.set_desc(slot, rx_desc(self.daddr, slot));
                    self.rx.offer(0, slot);
                    self.rx.publish(1);
                    rx = true;
                }
                Ask::Send(slot, len) => {
                    // SAFETY: the header's bytes, in the headroom of the
                    // slot, which only this side writes.
                    unsafe {
                        ptr::write_bytes(
                            self.dma.add(tx_slot(slot) + TX_HEADROOM - self.header),
                            0,
                            self.header,
                        );
                    }
                    self.tx
                        .set_desc(slot, tx_desc(self.daddr, slot, len, self.header));
                    self.tx.offer(0, slot);
                    self.tx.publish(1);
                    tx = true;
                }
            }
        }
        Ok((rx, tx))
    }

    /// Carry the kernel's messages to the card until the kernel's end of
    /// the FIFO goes away. Why it stopped.
    ///
    /// A read, and a wait when it finds nothing: not
    /// `eclipse_fifo_reply_wait`, the one call the disk driver makes of the
    /// two. It was tried here and measured (32 MiB over TCP, one CPU): a
    /// thread that reads the moment it is woken gives the card its buffers
    /// back two or three at a time instead of the whole ring, the card then
    /// delivers two or three frames at a time, and the stack acknowledges
    /// each of those bursts -- ten thousand ACKs for a download instead of
    /// eight hundred, and 3.4 s instead of 2.0 s on the boots where it
    /// settles that way. The trip back through user mode before the read is
    /// what lets the kernel finish what it is sending.
    pub fn feed(&mut self, device: &Device, fifo: &Fifo<Msg>) -> String {
        let mut msgs = [Msg::default(); SLOTS];
        loop {
            let got = match fifo.read(&mut msgs) {
                Ok(got) => got,
                Err(Status::SHOULD_WAIT) => match fifo.wait_readable() {
                    Ok(()) => continue,
                    Err(e) => return format!("the other end of the FIFO is gone: {e}"),
                },
                Err(e) => return format!("fifo_read: {e}"),
            };
            match self.give(&msgs[..got]) {
                Ok((rx, tx)) => {
                    if rx {
                        device.notify(RXQ);
                    }
                    if tx {
                        device.notify(TXQ);
                    }
                }
                Err(m) => return format!("a message from the kernel that means nothing: {m:?}"),
            }
        }
    }
}

/// The side that takes what the card has finished: the frames it received
/// and the transmit slots it is done with.
pub struct Card {
    rx: Answers,
    tx: Answers,
    header: usize,
}

impl Card {
    /// The taking halves of the two queues, and the card's header length.
    pub fn new(rx: Answers, tx: Answers, header: usize) -> Card {
        Card { rx, tx, header }
    }

    /// What the card has finished, as the messages that tell the kernel:
    /// how many. At most a ring's worth of each, which is all there can be.
    fn collect(&mut self, out: &mut [Msg; SLOTS]) -> Result<usize, String> {
        let mut n = 0;
        while n < RX_BUFS {
            let Some((id, len)) = self.rx.take_used() else {
                break;
            };
            out[n] = received(id, len, self.header).ok_or_else(|| {
                format!("the device returned a receive buffer that is not one of ours: id {id}, {len} bytes")
            })?;
            n += 1;
        }
        for _ in 0..TX_BUFS {
            let Some((id, _)) = self.tx.take_used() else {
                break;
            };
            out[n] = Msg {
                op: OP_TX,
                slot: id,
                len: 0,
                off: 0,
            };
            n += 1;
        }
        Ok(n)
    }

    /// What the card has finished, to the kernel. How much there was.
    fn drain(&mut self, fifo: &Fifo<Msg>) -> Result<usize, String> {
        let mut msgs = [Msg::default(); SLOTS];
        let n = self.collect(&mut msgs)?;
        // The FIFO has room for all of them: a message per slot, and no
        // slot is in two.
        let mut sent = 0;
        while sent < n {
            match fifo.write(&msgs[sent..n]) {
                Ok(put) if put > 0 => sent += put,
                Ok(_) => return Err("fifo_write took nothing".into()),
                Err(e) => return Err(format!("fifo_write: {e}")),
            }
        }
        Ok(n)
    }

    /// Carry what the card finishes to the kernel, asleep on `interrupt`
    /// between bursts (or looking every 200 us where there is none). Why it
    /// stopped.
    pub fn carry(
        &mut self,
        device: &Device,
        interrupt: Option<Handle>,
        fifo: &Fifo<Msg>,
    ) -> String {
        loop {
            let step = match interrupt {
                Some(interrupt) => claim::interrupt_wait(interrupt)
                    .map_err(|e| format!("interrupt_wait: {e}"))
                    .and_then(|()| {
                        device.ack();
                        while self.drain(fifo)? > 0 {}
                        Ok(())
                    }),
                None => self.drain(fifo).map(|n| {
                    if n == 0 {
                        device.ack();
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                }),
            };
            if let Err(why) = step {
                return why;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::Pages;
    use crate::queue::{avail_offset, Queue, USED_OFFSET};

    const PCI: Transport = Transport::Pci {
        common: 0,
        notify: 0,
        device: 0,
        notify_multiplier: 0,
    };

    fn word(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    /// The device answers: `(id, len)` pairs in the used ring of the queue
    /// at `queue`, from its start.
    fn answer(bytes: &mut [u8], queue: usize, elems: &[(u32, u32)]) {
        let used = queue + USED_OFFSET;
        for (i, (id, len)) in elems.iter().enumerate() {
            bytes[used + 4 + 8 * i..used + 8 + 8 * i].copy_from_slice(&id.to_le_bytes());
            bytes[used + 8 + 8 * i..used + 12 + 8 * i].copy_from_slice(&len.to_le_bytes());
        }
        bytes[used + 2..used + 4].copy_from_slice(&(elems.len() as u16).to_le_bytes());
    }

    #[test]
    fn the_window_is_the_slots_and_starts_after_the_queues() {
        assert_eq!(dma_queue(TXQ) + 2 * PAGE, DMA_WIN);
        assert_eq!(WIN_SIZE, 96 * 2048);
        assert_eq!(DMA_SIZE, DMA_WIN + WIN_SIZE);
    }

    #[test]
    fn an_offer_is_the_address_and_the_two_slot_counts() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let offer = Offer::of(mac);
        assert_eq!((offer.rx_slots, offer.tx_slots), (64, 32));
        let bytes = offer.to_bytes();
        assert_eq!(&bytes[0..8], &0x5634_1200_5452u64.to_ne_bytes());
        assert_eq!(Offer::from_bytes(&bytes), Some(offer));
        assert_eq!(offer.mac_bytes(), mac);
        // One byte short or one too many is not an offer.
        assert_eq!(Offer::from_bytes(&bytes[..15]), None);
        assert_eq!(Offer::from_bytes(&[0; 17]), None);
    }

    #[test]
    fn the_address_goes_to_the_kernel_first_byte_lowest() {
        assert_eq!(
            mac_word([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
            0x5634_1200_5452
        );
    }

    #[test]
    fn a_receive_slot_goes_to_the_card_whole_and_writable() {
        let d = rx_desc(0x8000_0000, 3);
        assert_eq!(d.addr, 0x8000_0000 + (DMA_WIN + 3 * BUF_SIZE) as u64);
        assert_eq!(d.len, BUF_SIZE as u32);
        assert_eq!(d.flags, DESC_F_WRITE);
    }

    #[test]
    fn a_frame_to_send_carries_its_header_in_the_headroom() {
        for (transport, header) in [(Transport::Mmio, 10), (PCI, 12)] {
            assert_eq!(header_len(transport), header);
            let d = tx_desc(0x8000_0000, 2, 60, header);
            let frame = 0x8000_0000 + (DMA_WIN + (RX_BUFS + 2) * BUF_SIZE + TX_HEADROOM) as u64;
            assert_eq!(d.addr + header as u64, frame);
            assert_eq!(d.len as usize, header + 60);
            assert_eq!(d.flags, 0, "the card reads it");
        }
    }

    #[test]
    fn only_a_slot_and_a_frame_that_fits_reach_the_card() {
        let msg = |op, slot, len, off| Msg { op, slot, len, off };
        assert_eq!(ask(&msg(OP_RX, 63, 0, 0)), Some(Ask::Refill(63)));
        assert_eq!(ask(&msg(OP_RX, 64, 0, 0)), None);
        assert_eq!(ask(&msg(OP_TX, 31, 1514, 16)), Some(Ask::Send(31, 1514)));
        assert_eq!(ask(&msg(OP_TX, 32, 60, 16)), None, "not a transmit slot");
        assert_eq!(ask(&msg(OP_TX, 0, 1515, 16)), None, "longer than a frame");
        assert_eq!(ask(&msg(OP_TX, 0, 60, 0)), None, "no room for the header");
        assert_eq!(ask(&msg(3, 0, 0, 0)), None);
        assert_eq!(ask(&msg(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_received_frame_is_what_follows_the_header() {
        assert_eq!(
            received(5, 72, 12),
            Some(Msg {
                op: OP_RX,
                slot: 5,
                len: 60,
                off: 12
            })
        );
        assert_eq!(received(5, 12, 12).unwrap().len, 0);
        assert_eq!(received(64, 72, 12), None, "not one of the buffers");
        assert_eq!(received(5, 11, 12), None, "shorter than a header");
        assert_eq!(received(5, 2049, 12), None, "longer than a slot");
    }

    #[test]
    fn what_the_kernel_asks_goes_into_the_rings_in_the_order_it_was_asked() {
        let mut pages = Pages::new(DMA_SIZE / PAGE);
        let dma = pages.ptr();
        let (rx, _) = unsafe { Queue::new(dma.add(dma_queue(RXQ)), QUEUE_SIZE) }.split();
        let (tx, _) = unsafe { Queue::new(dma.add(dma_queue(TXQ)), QUEUE_SIZE) }.split();
        let mut feeder = unsafe { Feeder::new(rx, tx, dma, 0x10_0000, 12) };
        // A header left by the frame before.
        let at = tx_slot(1) + TX_HEADROOM - 12;
        pages.bytes_mut()[at..at + 12].fill(0xee);
        let msgs = [
            Msg {
                op: OP_RX,
                slot: 7,
                len: 0,
                off: 0,
            },
            Msg {
                op: OP_TX,
                slot: 1,
                len: 90,
                off: 16,
            },
            Msg {
                op: OP_RX,
                slot: 9,
                len: 0,
                off: 0,
            },
        ];
        assert_eq!(feeder.give(&msgs), Ok((true, true)));
        let b = pages.bytes();
        let rx_avail = dma_queue(RXQ) + avail_offset(QUEUE_SIZE);
        let tx_avail = dma_queue(TXQ) + avail_offset(QUEUE_SIZE);
        assert_eq!((word(b, rx_avail + 2), word(b, tx_avail + 2)), (2, 1));
        assert_eq!((word(b, rx_avail + 4), word(b, rx_avail + 6)), (7, 9));
        assert_eq!(word(b, tx_avail + 4), 1);
        assert!(b[at..at + 12].iter().all(|&x| x == 0), "a clean header");
        // Descriptor 1 of the transmit queue is the frame and its header.
        let d = dma_queue(TXQ) + 16;
        assert_eq!(
            u64::from_le_bytes(b[d..d + 8].try_into().unwrap()),
            0x10_0000 + at as u64
        );
        assert_eq!(
            u32::from_le_bytes(b[d + 8..d + 12].try_into().unwrap()),
            102
        );
    }

    #[test]
    fn a_message_that_means_nothing_stops_the_feeder() {
        let mut pages = Pages::new(DMA_SIZE / PAGE);
        let dma = pages.ptr();
        let (rx, _) = unsafe { Queue::new(dma.add(dma_queue(RXQ)), QUEUE_SIZE) }.split();
        let (tx, _) = unsafe { Queue::new(dma.add(dma_queue(TXQ)), QUEUE_SIZE) }.split();
        let mut feeder = unsafe { Feeder::new(rx, tx, dma, 0, 10) };
        let bad = Msg {
            op: OP_TX,
            slot: 40,
            len: 60,
            off: 16,
        };
        assert_eq!(feeder.give(&[bad]), Err(bad));
    }

    #[test]
    fn what_the_card_finished_goes_to_the_kernel_frames_first() {
        let mut pages = Pages::new(4);
        let dma = pages.ptr();
        let (_, rx) = unsafe { Queue::new(dma.add(dma_queue(RXQ)), QUEUE_SIZE) }.split();
        let (_, tx) = unsafe { Queue::new(dma.add(dma_queue(TXQ)), QUEUE_SIZE) }.split();
        let mut card = Card::new(rx, tx, 12);
        let mut out = [Msg::default(); SLOTS];
        assert_eq!(card.collect(&mut out), Ok(0));
        answer(pages.bytes_mut(), dma_queue(RXQ), &[(4, 1526), (5, 72)]);
        answer(pages.bytes_mut(), dma_queue(TXQ), &[(2, 0)]);
        assert_eq!(card.collect(&mut out), Ok(3));
        assert_eq!(
            out[0],
            Msg {
                op: OP_RX,
                slot: 4,
                len: 1514,
                off: 12
            }
        );
        assert_eq!(
            out[1],
            Msg {
                op: OP_RX,
                slot: 5,
                len: 60,
                off: 12
            }
        );
        assert_eq!(
            out[2],
            Msg {
                op: OP_TX,
                slot: 2,
                len: 0,
                off: 0
            }
        );
        assert_eq!(card.collect(&mut out), Ok(0), "taken once");
    }

    #[test]
    fn a_buffer_that_is_not_the_drivers_stops_the_card() {
        let mut pages = Pages::new(4);
        let dma = pages.ptr();
        let (_, rx) = unsafe { Queue::new(dma.add(dma_queue(RXQ)), QUEUE_SIZE) }.split();
        let (_, tx) = unsafe { Queue::new(dma.add(dma_queue(TXQ)), QUEUE_SIZE) }.split();
        let mut card = Card::new(rx, tx, 12);
        answer(pages.bytes_mut(), dma_queue(RXQ), &[(70, 72)]);
        assert!(card.collect(&mut [Msg::default(); SLOTS]).is_err());
    }
}
