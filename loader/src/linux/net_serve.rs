//! [`ECLIPSE_NET_SERVE`](super::ECLIPSE_NET_SERVE): a network card that a
//! process drives, given to the kernel as an interface of its own.
//!
//! The process (a userspace driver such as `tools/zx-virtio-net`) keeps one
//! end of a FIFO of 16-byte messages and hands the kernel the other, with a
//! window: a VMO both sides reach, cut into slots of [`SLOT`] bytes, the
//! first `rx_slots` of them for frames coming in and the rest for frames
//! going out. The kernel wraps it in a
//! [`ServedInterface`](kernel_hal::drivers::net::served::ServedInterface), so
//! everything above it -- smoltcp, the sockets, `ifconfig` -- sees an `ethN`
//! like any other.
//!
//! A message is `op, slot, len, off` (four `u32`s), and each goes both ways:
//!
//! - `OP_RX`, from the driver: a frame of `len` bytes is in receive slot
//!   `slot`, `off` bytes in (the card's own header sits before it). The
//!   kernel copies it out and sends the message back, which gives the slot
//!   back to the card.
//! - `OP_TX`, from the kernel: a frame of `len` bytes is in transmit slot
//!   `slot`, [`TX_HEADROOM`] bytes in (room for the card's header). The
//!   driver sends the message back once the card has taken the frame, which
//!   frees the slot.
//!
//! The frames are copied between kernel memory and the window on this side;
//! the driver never sees kernel memory, and the card reads and writes the
//! window directly. Nothing here blocks: a frame that finds no free slot is
//! refused and the stack sends it again.
//!
//! A driver that dies takes nothing above it with it. The interface stays,
//! with its addresses, its routes and the connections going through it, and
//! simply carries nothing for a while (its port is taken away,
//! `ServedInterface::unplug`): a driver that then serves
//! a card with the same MAC gets the same interface, and what was waiting
//! goes out. Only when none comes within [`DRIVER_GRACE`] is the interface
//! removed.
//!
//! What this trusts: the driver is a process the system started for the
//! purpose. It sees and can forge every frame of the interface, as a card
//! could; it cannot make the kernel read or write anything but the window,
//! and a message that names a place outside it is dropped.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use kernel_hal::drivers::net::served::{FramePort, ServedInterface, MAX_FRAME};
use kernel_hal::drivers::prelude::Device;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

/// One FIFO element: `op, slot, len, off`, a `u32` each.
pub(super) const NET_MSG: usize = 16;
pub(super) const OP_RX: u32 = 1;
pub(super) const OP_TX: u32 = 2;
/// A slot of the window: a frame and the card's header, with room.
pub(super) const SLOT: usize = 2048;
/// Where a frame going out starts in its slot.
pub(super) const TX_HEADROOM: usize = 16;
/// The most transmit slots: one bit each in a `u32`.
pub(super) const MAX_TX_SLOTS: usize = 32;
/// The most messages read in one go.
const BATCH: usize = 32;
/// The most received frames kept for the stack; past it the newest is
/// dropped, as a card with a full ring drops.
const RX_BACKLOG: usize = 256;

/// How long an interface outlasts its driver, for the next one to pick it
/// up. Longer than the longest pause `eclipse-init` puts between two starts
/// of a service that keeps dying (8 s); short enough that a card nobody
/// drives any more does not sit in the routing for long.
const DRIVER_GRACE: Duration = Duration::from_secs(10);

/// A message, as bytes.
pub(super) fn encode(op: u32, slot: u32, len: u32, off: u32) -> [u8; NET_MSG] {
    let mut m = [0u8; NET_MSG];
    m[0..4].copy_from_slice(&op.to_ne_bytes());
    m[4..8].copy_from_slice(&slot.to_ne_bytes());
    m[8..12].copy_from_slice(&len.to_ne_bytes());
    m[12..16].copy_from_slice(&off.to_ne_bytes());
    m
}

/// A message's `(op, slot, len, off)`.
pub(super) fn decode(m: &[u8; NET_MSG]) -> (u32, u32, u32, u32) {
    let word = |i: usize| u32::from_ne_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
    (word(0), word(4), word(8), word(12))
}

/// How a window of `len` bytes with `rx_slots` receive slots is cut:
/// the number of transmit slots, or `None` when the window is not whole
/// slots or leaves no slot, or too many, on either side.
pub(super) fn cut_window(len: usize, rx_slots: usize) -> Option<usize> {
    if len == 0 || !len.is_multiple_of(SLOT) {
        return None;
    }
    let slots = len / SLOT;
    if rx_slots == 0 || rx_slots >= slots {
        return None;
    }
    let tx_slots = slots - rx_slots;
    (tx_slots <= MAX_TX_SLOTS).then_some(tx_slots)
}

/// Where in the window the received frame a message names is, or `None`
/// when it names a place outside its slot or a frame no card delivers.
pub(super) fn rx_place(slot: u32, len: u32, off: u32, rx_slots: usize) -> Option<usize> {
    let (slot, len, off) = (slot as usize, len as usize, off as usize);
    if slot >= rx_slots || len == 0 || len > MAX_FRAME || off > SLOT || len > SLOT - off {
        return None;
    }
    Some(slot * SLOT + off)
}

#[derive(Default)]
struct State {
    /// Frames read from the window, for the stack to take.
    incoming: VecDeque<Vec<u8>>,
    /// Receive slots to give back that the FIFO had no room for.
    returns: VecDeque<u32>,
    /// One bit per free transmit slot.
    tx_free: u32,
}

/// The kernel's end of a served card.
pub(super) struct ServedNic {
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    rx_slots: usize,
    tx_slots: usize,
    state: lock::Mutex<State>,
    /// One reader of the FIFO at a time, without holding a lock across it.
    pumping: AtomicBool,
    dead: AtomicBool,
}

impl ServedNic {
    pub(super) fn new(fifo: Arc<Fifo>, window: Arc<VmObject>, rx_slots: usize) -> ZxResult<Self> {
        let tx_slots = cut_window(window.len(), rx_slots).ok_or(ZxError::INVALID_ARGS)?;
        // The FIFO must carry these messages, or every write fails.
        fifo.read_buffer_elems(NET_MSG, 1)?;
        Ok(Self {
            fifo,
            window,
            rx_slots,
            tx_slots,
            state: lock::Mutex::new(State {
                tx_free: ((1u64 << tx_slots) - 1) as u32,
                ..State::default()
            }),
            pumping: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        })
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    fn died(&self, e: ZxError) {
        if !self.dead.swap(true, Ordering::AcqRel) {
            warn!("served card: the driver is gone ({e:?})");
        }
    }

    /// Read what the driver has sent: frames into the backlog, transmit
    /// slots back to free; and give back the receive slots read.
    pub(super) fn pump(&self) {
        if self.is_dead() || self.pumping.swap(true, Ordering::AcqRel) {
            return;
        }
        loop {
            let mut msgs = [0u8; NET_MSG * BATCH];
            let n = match self.fifo.read(NET_MSG, &mut msgs, BATCH) {
                Ok(n) => n,
                Err(ZxError::SHOULD_WAIT) => 0,
                Err(e) => {
                    self.died(e);
                    0
                }
            };
            for m in msgs.as_chunks::<NET_MSG>().0.iter().take(n) {
                self.take(decode(m));
            }
            if n < BATCH {
                break;
            }
        }
        self.give_back();
        self.pumping.store(false, Ordering::Release);
    }

    fn take(&self, (op, slot, len, off): (u32, u32, u32, u32)) {
        match op {
            OP_RX => {
                let Some(at) = rx_place(slot, len, off, self.rx_slots) else {
                    warn!("served card: a frame outside its slot ({slot}, {len}+{off})");
                    return;
                };
                let mut frame = alloc::vec![0u8; len as usize];
                let read = self.window.read(at, &mut frame).is_ok();
                let mut state = self.state.lock();
                state.returns.push_back(slot);
                if read && state.incoming.len() < RX_BACKLOG {
                    state.incoming.push_back(frame);
                }
            }
            OP_TX if (slot as usize) < self.tx_slots => {
                self.state.lock().tx_free |= 1 << slot;
            }
            _ => warn!("served card: a message that means nothing ({op}, {slot})"),
        }
    }

    /// Send the receive slots that were read back to the driver, as many as
    /// the FIFO takes; the rest wait for the next pump.
    fn give_back(&self) {
        loop {
            let Some(slot) = self.state.lock().returns.pop_front() else {
                return;
            };
            match self.fifo.write(NET_MSG, &encode(OP_RX, slot, 0, 0), 1) {
                Ok(_) => {}
                Err(e) => {
                    self.state.lock().returns.push_front(slot);
                    if e != ZxError::SHOULD_WAIT {
                        self.died(e);
                    }
                    return;
                }
            }
        }
    }

    /// Whether frames wait for the stack.
    fn has_frames(&self) -> bool {
        !self.state.lock().incoming.is_empty()
    }

    fn take_tx_slot(&self) -> Option<u32> {
        let mut state = self.state.lock();
        if state.tx_free == 0 {
            return None;
        }
        let slot = state.tx_free.trailing_zeros();
        state.tx_free &= !(1 << slot);
        Some(slot)
    }
}

impl FramePort for ServedNic {
    fn receive(&self) -> Option<Vec<u8>> {
        if let Some(frame) = self.state.lock().incoming.pop_front() {
            return Some(frame);
        }
        self.pump();
        self.state.lock().incoming.pop_front()
    }

    fn can_send(&self) -> bool {
        if self.is_dead() {
            return false;
        }
        if self.state.lock().tx_free != 0 {
            return true;
        }
        self.pump();
        let free = self.state.lock().tx_free != 0;
        // The pump may have read frames too, and this is the sending half
        // of a poll: the receiving half is over. The FIFO is empty now, so
        // nothing else would bring the card's task to them.
        //
        // Only with a slot to answer them with. Without one the stack takes
        // no frame (`ServedDriver::receive`), so the task would poll, come
        // here, be called again and poll, with nothing between two rounds
        // that lets anything else run: on one CPU the driver never got to
        // give a slot back, and the machine stopped there. That is where an
        // echo of megabytes ended, every slot out with the answers and the
        // next frames already in. A slot's return is a message in the FIFO,
        // and that brings the task.
        if free && self.has_frames() {
            self.fifo.signal_set(Signal::USER_SIGNAL_0);
        }
        free
    }

    fn send(&self, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() > MAX_FRAME || !self.can_send() {
            return false;
        }
        let Some(slot) = self.take_tx_slot() else {
            return false;
        };
        let at = (self.rx_slots + slot as usize) * SLOT + TX_HEADROOM;
        let sent = self.window.write(at, frame).is_ok()
            && self
                .fifo
                .write(
                    NET_MSG,
                    &encode(OP_TX, slot, frame.len() as u32, TX_HEADROOM as u32),
                    1,
                )
                .is_ok();
        if !sent {
            self.state.lock().tx_free |= 1 << slot;
        }
        sent
    }
}

/// Whose turn it is to drive a card: the port of the driver that has it,
/// or nothing while there is none.
pub(super) struct Seat<P> {
    port: Option<Arc<P>>,
    /// How many drivers have sat here: tells a seat nobody came back to
    /// from one that was taken and left again.
    turns: usize,
}

impl<P> Seat<P> {
    pub(super) const fn new() -> Self {
        Self {
            port: None,
            turns: 0,
        }
    }

    /// A driver takes the seat. Its turn, or `None` when another has it.
    pub(super) fn sit(&mut self, port: Arc<P>) -> Option<usize> {
        if self.port.is_some() {
            return None;
        }
        self.port = Some(port);
        self.turns += 1;
        Some(self.turns)
    }

    /// The driver whose port this is has gone: whether the seat was its to
    /// leave. Somebody else's port is left where it is.
    pub(super) fn leave(&mut self, port: &Arc<P>) -> bool {
        let its = self.port.as_ref().is_some_and(|p| Arc::ptr_eq(p, port));
        if its {
            self.port = None;
        }
        its
    }

    /// Whether the seat is empty and nobody has sat since `turn`.
    pub(super) fn empty_since(&self, turn: usize) -> bool {
        self.port.is_none() && self.turns == turn
    }
}

/// A card that has been served, by its MAC.
struct Card {
    mac: [u8; 6],
    seat: Seat<ServedNic>,
    iface: Arc<ServedInterface>,
}

/// The cards with an interface: one being driven, or one within
/// [`DRIVER_GRACE`] of having been. Every change of driver happens under
/// this lock, the interface's port included, so the seat and the port
/// never disagree.
static CARDS: lock::Mutex<Vec<Card>> = lock::Mutex::new(Vec::new());

/// Seat `nic` behind the interface of the card with this MAC: the one it
/// had under its last driver, or a new `ethN`. The interface and the turn.
fn seat_card(mac: [u8; 6], nic: Arc<ServedNic>) -> ZxResult<(Arc<ServedInterface>, usize)> {
    use kernel_hal::drivers::scheme::NetScheme;
    let mut cards = CARDS.lock();
    if let Some(card) = cards.iter_mut().find(|c| c.mac == mac) {
        // The driver it had may have died a moment ago, with its task not
        // yet there to say so: ask the FIFO.
        if let Some(last) = card.seat.port.clone() {
            last.pump();
            if last.is_dead() {
                card.seat.leave(&last);
            }
        }
        // Two drivers for one card: the second is refused.
        let turn = card.seat.sit(nic.clone()).ok_or(ZxError::ALREADY_BOUND)?;
        card.iface.set_port(nic);
        info!(
            "served card: {} has a driver again",
            card.iface.get_ifname()
        );
        return Ok((card.iface.clone(), turn));
    }
    let mut seat = Seat::new();
    let turn = seat.sit(nic.clone()).ok_or(ZxError::BAD_STATE)?;
    let name = kernel_hal::drivers::net::next_eth_ifname();
    let iface = Arc::new(ServedInterface::new(name, mac, nic));
    if !kernel_hal::drivers::hotplug::add(Device::Net(iface.clone())) {
        return Err(ZxError::NOT_SUPPORTED);
    }
    cards.push(Card {
        mac,
        seat,
        iface: iface.clone(),
    });
    Ok((iface, turn))
}

/// The driver behind `nic` is gone: its interface carries nothing from
/// here on, unless another driver has the card already.
fn unseat_card(iface: &Arc<ServedInterface>, nic: &Arc<ServedNic>) {
    let mut cards = CARDS.lock();
    if let Some(card) = cards.iter_mut().find(|c| Arc::ptr_eq(&c.iface, iface)) {
        if card.seat.leave(nic) {
            iface.unplug();
        }
    }
}

/// [`DRIVER_GRACE`] after the driver of turn `turn` went: when no other
/// came, the card is forgotten and the answer is `true`.
fn forget_card(iface: &Arc<ServedInterface>, turn: usize) -> bool {
    let mut cards = CARDS.lock();
    let Some(at) = cards.iter().position(|c| Arc::ptr_eq(&c.iface, iface)) else {
        return false;
    };
    if !cards[at].seat.empty_since(turn) {
        return false;
    }
    cards.remove(at);
    true
}

/// [`ECLIPSE_NET_SERVE`](super::ECLIPSE_NET_SERVE): `args[0]` is the FIFO
/// end the kernel keeps, `args[1]` the window, `args[2]` the card's MAC
/// (its six bytes, the first in the low byte), `args[3]` how many of the
/// window's slots are receive slots. Both handles are the kernel's from
/// here on, whatever the answer.
pub(super) fn net_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    let (fifo, window) = (fifo?, window?);
    let _ = proc.remove_handle(args[0] as u32);
    let _ = proc.remove_handle(args[1] as u32);
    let mac: [u8; 6] = core::array::from_fn(|i| (args[2] >> (8 * i)) as u8);
    // A group address (bit 0 of the first byte) or all zeroes is no card's.
    if mac[0] & 1 != 0 || mac == [0; 6] {
        return Err(ZxError::INVALID_ARGS);
    }
    let nic = Arc::new(ServedNic::new(fifo.clone(), window, args[3])?);
    let (iface, turn) = seat_card(mac, nic.clone())?;
    // Once through the stack straight away: an interface that had no
    // driver for a while has things waiting to go out.
    fifo.signal_set(Signal::USER_SIGNAL_0);
    // The card's interrupt, in effect: sleep on the FIFO, and run what
    // arrives through the stack.
    kernel_hal::thread::spawn(async move {
        let wire: Arc<dyn KernelObject> = fifo;
        while !nic.is_dead() {
            wire.wait_signal(Signal::READABLE | Signal::PEER_CLOSED | Signal::USER_SIGNAL_0)
                .await;
            wire.signal_clear(Signal::USER_SIGNAL_0);
            nic.pump();
            let frames = nic.has_frames();
            // Every time, frames or not: transmit slots that came back are
            // what the stack was waiting for to send, or to take the frames
            // it held back for want of one. A socket call that has the
            // stack will not be long, so wait for it rather than leave this
            // to whoever polls next.
            while !iface.frames_arrived() {
                kernel_hal::thread::yield_now().await;
            }
            // Without the rate limit of the interrupt path, which drops a
            // wake that follows another by under 250 us and leaves that
            // reader to its park timer. This is once per batch delivered,
            // not per interrupt, so no storm can come of it.
            if frames {
                kernel_hal::net::wake_net_rx_waiters_now();
            }
        }
        // No driver: the interface carries nothing, and waits for the next.
        unseat_card(&iface, &nic);
        drop(nic);
        kernel_hal::net::wake_net_rx_waiters_now();
        kernel_hal::thread::sleep_until(kernel_hal::timer::timer_now() + DRIVER_GRACE).await;
        if !forget_card(&iface, turn) {
            return;
        }
        // Nobody came: no driver, no card. The interface goes, so nothing
        // routes into it.
        let gone = Device::Net(iface);
        if !kernel_hal::drivers::hotplug::remove(&gone) {
            warn!("served card: its interface was not registered any more");
        }
        kernel_hal::net::wake_net_rx_waiters_now();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames in, and every transmit slot out with the driver: the stack
    /// takes no frame it cannot answer, so calling the card's task for them
    /// only has it poll and be called again, round and round, on the CPU
    /// the driver needs to give a slot back.
    #[test]
    fn frames_that_wait_for_a_slot_do_not_call_the_cards_task() {
        let (kernels, drivers) = Fifo::create(128, NET_MSG);
        let slots = 2 + MAX_TX_SLOTS;
        let window = VmObject::new_paged(slots * SLOT / 4096);
        let nic = ServedNic::new(kernels.clone(), window, 2).unwrap();
        let called = || kernels.signal().contains(Signal::USER_SIGNAL_0);
        for _ in 0..MAX_TX_SLOTS {
            assert!(nic.send(&[0u8; 60]));
        }
        drivers
            .write(NET_MSG, &encode(OP_RX, 0, 60, 12), 1)
            .unwrap();
        assert!(!nic.can_send());
        assert!(nic.has_frames(), "the frame was read on the way");
        assert!(!called(), "nothing to call the task for yet");

        // A slot comes back: now the frame can be taken, and the poll that
        // found out was past its receiving half.
        drivers.write(NET_MSG, &encode(OP_TX, 7, 0, 0), 1).unwrap();
        assert!(nic.can_send());
        assert!(called());
    }

    #[test]
    fn a_seat_takes_one_driver_at_a_time() {
        let mut seat = Seat::new();
        let (first, second) = (Arc::new(1), Arc::new(2));
        assert_eq!(seat.sit(first.clone()), Some(1));
        assert_eq!(seat.sit(second.clone()), None);
        // A driver that never had the seat does not empty it by leaving.
        assert!(!seat.leave(&second));
        assert_eq!(seat.sit(second.clone()), None);
        // The one that has it does, and then the next can sit.
        assert!(seat.leave(&first));
        assert!(!seat.leave(&first), "it left once");
        assert_eq!(seat.sit(second), Some(2));
    }

    #[test]
    fn a_seat_knows_whether_anybody_came_back() {
        let mut seat = Seat::new();
        let first = Arc::new(1);
        let turn = seat.sit(first.clone()).unwrap();
        assert!(!seat.empty_since(turn), "its driver is still there");
        seat.leave(&first);
        assert!(seat.empty_since(turn));
        // Another came and went in the meantime: the first driver's wait
        // is over, and it is the second's to give the interface up.
        let second = Arc::new(2);
        let next = seat.sit(second.clone()).unwrap();
        assert!(!seat.empty_since(turn));
        seat.leave(&second);
        assert!(!seat.empty_since(turn));
        assert!(seat.empty_since(next));
    }

    #[test]
    fn a_message_is_four_words() {
        let m = encode(OP_TX, 3, 1514, 16);
        assert_eq!(decode(&m), (OP_TX, 3, 1514, 16));
        assert_eq!(&m[0..4], &2u32.to_ne_bytes());
        assert_eq!(&m[8..12], &1514u32.to_ne_bytes());
    }

    #[test]
    fn the_window_is_whole_slots_with_some_on_each_side() {
        assert_eq!(cut_window(48 * SLOT, 32), Some(16));
        assert_eq!(cut_window(2 * SLOT, 1), Some(1));
        assert_eq!(cut_window(48 * SLOT, 0), None);
        assert_eq!(cut_window(48 * SLOT, 48), None);
        assert_eq!(cut_window(48 * SLOT, 49), None);
        assert_eq!(cut_window(48 * SLOT + 1, 32), None);
        assert_eq!(cut_window(0, 1), None);
        // More transmit slots than a u32 has bits.
        assert_eq!(cut_window(40 * SLOT, 7), None);
        assert_eq!(cut_window(40 * SLOT, 8), Some(MAX_TX_SLOTS));
    }

    #[test]
    fn a_received_frame_stays_inside_its_slot() {
        assert_eq!(rx_place(0, 60, 10, 32), Some(10));
        assert_eq!(rx_place(3, 1514, 12, 32), Some(3 * SLOT + 12));
        assert_eq!(rx_place(31, 1, SLOT as u32 - 1, 32), Some(32 * SLOT - 1));
        // Not a receive slot.
        assert_eq!(rx_place(32, 60, 10, 32), None);
        // Nothing, or more than a frame.
        assert_eq!(rx_place(0, 0, 10, 32), None);
        assert_eq!(rx_place(0, 1515, 10, 32), None);
        // Runs off the end of the slot.
        assert_eq!(rx_place(0, 1514, 600, 32), None);
        assert_eq!(rx_place(0, 1, SLOT as u32, 32), None);
        assert_eq!(rx_place(0, 60, u32::MAX, 32), None);
    }
}
