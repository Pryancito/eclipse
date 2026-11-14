//! [`ECLIPSE_NET_BORROW`](super::ECLIPSE_NET_BORROW) and
//! [`ECLIPSE_NET_LENT_TURN`](super::ECLIPSE_NET_LENT_TURN): a network card
//! the kernel drives, lent to a process that is the machine's network
//! stack.
//!
//! The mirror of [`net_serve`](super::net_serve). There a process drives a
//! card and the kernel has the stack; here the kernel keeps the driver --
//! the registers, the rings, the interrupt -- and the stack is the
//! process's (`tools/eclipse-netstack`), which has no driver of its own for
//! such a card. The driver stops feeding the kernel's stack
//! (`kernel_hal::drivers::net::lent`) and the card leaves the kernel's list
//! of interfaces: the frames it receives are kept here, a [`LentCard`],
//! until the process takes them.
//!
//! The messages are the ones of a served card, `op, slot, len, off`, over a
//! window cut into slots of [`SLOT`] bytes, receive slots first. Two things
//! differ, and both because the other end is the kernel and not a process:
//!
//! - The window is the borrower's own memory. The kernel copies a frame
//!   into a receive slot, or out of a transmit slot, during the borrower's
//!   call, where that memory is at hand.
//! - There is no FIFO and nobody to wake for a frame to send. One call is a
//!   turn: it gives the kernel its messages -- `OP_RX`, a receive slot the
//!   borrower has read; `OP_TX`, a frame to send, which goes to the card
//!   there and then -- and takes the kernel's: `OP_TX`, the card has that
//!   frame and the slot is free; `OP_RX`, a frame is in that receive slot.
//!
//! A frame the card has no room for waits here and goes out when the card
//! says it is through with something (an interrupt), or at the next look at
//! it ([`LOOK`]); its slot comes back only then, so the borrower cannot
//! have more waiting than it has transmit slots.
//!
//! The loan ends with the handle: a borrower that closes it, or dies, gives
//! the card back to the kernel's list as it was.
//!
//! What this trusts: as with a served card, the borrower is the system's
//! own stack. It sees and makes every frame of the card; it cannot make the
//! kernel read or write outside its own memory, and a message that names a
//! place outside its slot is dropped.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use kernel_hal::drivers::net::lent::FrameTaker;
use kernel_hal::drivers::net::served::frame_len_ok;
use kernel_hal::drivers::prelude::{Device, DeviceError};
use kernel_hal::drivers::scheme::NetScheme;
use kernel_hal::user::{UserInPtr, UserOutPtr};
use zircon_object::impl_kobject;
use zircon_object::object::*;
use zircon_object::task::Process;
use zircon_object::{ZxError, ZxResult};

use super::net_serve::{decode, encode, MAX_TX_SLOTS, NET_MSG, OP_RX, OP_TX, SLOT};

/// [`ECLIPSE_NET_LENT_TURN`](super::ECLIPSE_NET_LENT_TURN) flag: sleep
/// until there is a message to answer with.
pub(super) const LENT_WAIT: usize = super::ECLIPSE_LENT_WAIT;
/// [`ECLIPSE_NET_LENT_TURN`](super::ECLIPSE_NET_LENT_TURN) flag: answer
/// with the frames that arrived too, not only with transmit slots.
pub(super) const LENT_FRAMES: usize = super::ECLIPSE_LENT_FRAMES;

/// The most receive slots a window is taken with, as the stack's port has
/// it.
const MAX_RX_SLOTS: usize = 1024;
/// The most received frames kept for the borrower; past it the newest is
/// dropped, as a card with a full ring drops.
const RX_BACKLOG: usize = 256;
/// How often a lent card is looked at without an interrupt: what the
/// kernel's periodic poll of its interfaces did for it while it was in the
/// list -- the driver's own watch on its link and on an interrupt that was
/// lost -- and the second chance of a frame that waits for room.
const LOOK: Duration = Duration::from_millis(100);

struct State {
    /// Frames the card received, for the borrower to take.
    incoming: VecDeque<Vec<u8>>,
    /// The receive slots the borrower is not reading: free for a frame.
    rx_free: Vec<u32>,
    /// Per receive slot, whether the borrower has a frame there.
    rx_held: Vec<bool>,
    /// Frames to send that the card had no room for, with their slots.
    tx_waiting: VecDeque<(u32, Vec<u8>)>,
    /// Transmit slots whose frame the card has taken, to tell the borrower.
    tx_done: VecDeque<u32>,
    /// One bit per transmit slot the kernel has: its frame waits, or the
    /// borrower has not been told yet that it went.
    tx_busy: u32,
    /// Frames lost here: no room in the backlog.
    dropped: u64,
}

/// What a loan needs of the card that is lent.
pub(super) trait Card: Send + Sync {
    /// Put a frame on the wire: `NotReady` when the card has no room now.
    fn send(&self, frame: &[u8]) -> Result<(), DeviceError>;
    /// Look at the card as an interrupt would make its driver look.
    fn look(&self);
    /// The loan is over.
    fn give_back(&self);
}

/// A card the kernel drives.
struct KernelCard(Arc<dyn NetScheme>);

impl Card for KernelCard {
    fn send(&self, frame: &[u8]) -> Result<(), DeviceError> {
        self.0.send(frame).map(|_| ())
    }

    fn look(&self) {
        // The driver hands over what it finds and says it is through
        // (`Taker::done`), as it does on an interrupt.
        let _ = self.0.poll();
    }

    fn give_back(&self) {
        // The driver feeds the kernel's stack again and the card is back
        // among the kernel's interfaces.
        let _ = self.0.lend(None);
        let name = self.0.get_ifname();
        if kernel_hal::drivers::hotplug::add(Device::Net(self.0.clone())) {
            info!("lent card: {name} is the kernel's again");
        }
        kernel_hal::net::wake_net_rx_waiters_now();
    }
}

/// What a turn answers with.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// The card has the frame of this transmit slot.
    Sent(u32),
    /// A frame for this receive slot.
    Frame(u32, Vec<u8>),
}

/// A card of the kernel's that a process has borrowed.
pub(super) struct LentCard {
    base: KObjectBase,
    card: Arc<dyn Card>,
    /// Where the borrower's window is, in its memory.
    window: usize,
    rx_slots: usize,
    tx_slots: usize,
    state: lock::Mutex<State>,
    /// One sender to the card at a time, without a lock held across it.
    sending: AtomicBool,
}

impl_kobject!(LentCard);

impl LentCard {
    /// `card`, to be lent over a window at `window` with these slots.
    /// `None` when a window cannot have that many.
    pub(super) fn new(
        card: Arc<dyn Card>,
        window: usize,
        rx_slots: usize,
        tx_slots: usize,
    ) -> Option<Arc<Self>> {
        if !(1..=MAX_RX_SLOTS).contains(&rx_slots) || !(1..=MAX_TX_SLOTS).contains(&tx_slots) {
            return None;
        }
        Some(Arc::new(Self {
            base: KObjectBase::new(),
            card,
            window,
            rx_slots,
            tx_slots,
            state: lock::Mutex::new(State {
                incoming: VecDeque::new(),
                // The lowest slot first.
                rx_free: (0..rx_slots as u32).rev().collect(),
                rx_held: alloc::vec![false; rx_slots],
                tx_waiting: VecDeque::new(),
                tx_done: VecDeque::new(),
                tx_busy: 0,
                dropped: 0,
            }),
            sending: AtomicBool::new(false),
        }))
    }

    /// How many messages a turn can carry each way: every slot once.
    pub(super) fn slots(&self) -> usize {
        self.rx_slots + self.tx_slots
    }

    /// A frame from the card.
    fn arrived(&self, frame: Vec<u8>) {
        let mut state = self.state.lock();
        if frame_len_ok(frame.len()) && state.incoming.len() < RX_BACKLOG {
            state.incoming.push_back(frame);
        } else {
            state.dropped += 1;
        }
    }

    /// A message from the borrower. `frame` reads the frame of an `OP_TX`
    /// out of the window: `len` bytes, `at` bytes in. A message that means
    /// nothing is dropped.
    pub(super) fn given(
        &self,
        (op, slot, len, off): (u32, u32, u32, u32),
        frame: impl FnOnce(usize, usize) -> Option<Vec<u8>>,
    ) {
        let at = slot as usize;
        match op {
            OP_RX => {
                let mut state = self.state.lock();
                if at < self.rx_slots && state.rx_held[at] {
                    state.rx_held[at] = false;
                    state.rx_free.push(slot);
                } else {
                    warn!("lent card: receive slot {slot} given back and not held");
                }
            }
            OP_TX => {
                let (len, off) = (len as usize, off as usize);
                if at >= self.tx_slots || !frame_len_ok(len) || off > SLOT || len > SLOT - off {
                    warn!("lent card: a frame to send outside its slot ({slot}, {len}+{off})");
                    return;
                }
                {
                    let mut state = self.state.lock();
                    if state.tx_busy & (1 << at) != 0 {
                        warn!("lent card: transmit slot {slot} given twice");
                        return;
                    }
                    state.tx_busy |= 1 << at;
                }
                let frame = frame((self.rx_slots + at) * SLOT + off, len);
                let mut state = self.state.lock();
                match frame {
                    Some(frame) => state.tx_waiting.push_back((slot, frame)),
                    // Nothing could be read there, so nothing is sent: the
                    // slot goes back all the same.
                    None => state.tx_done.push_back(slot),
                }
            }
            _ => warn!("lent card: a message that means nothing ({op}, {slot})"),
        }
    }

    /// Send what waits, in order, as far as the card takes it.
    pub(super) fn flush(&self) {
        loop {
            if self.sending.swap(true, Ordering::AcqRel) {
                // Whoever is sending looks again when it is through.
                return;
            }
            let mut full = false;
            loop {
                let Some((slot, frame)) = self.state.lock().tx_waiting.pop_front() else {
                    break;
                };
                match self.card.send(&frame) {
                    Ok(_) => self.state.lock().tx_done.push_back(slot),
                    Err(DeviceError::NotReady) => {
                        self.state.lock().tx_waiting.push_front((slot, frame));
                        full = true;
                        break;
                    }
                    // A frame the card will never take is lost, as a wire
                    // loses one.
                    Err(e) => {
                        warn!("lent card: a frame the card refused ({e:?})");
                        self.state.lock().tx_done.push_back(slot);
                    }
                }
            }
            self.sending.store(false, Ordering::Release);
            // What another put there meanwhile, having found this one
            // sending, is this one's to send.
            if full || self.state.lock().tx_waiting.is_empty() {
                return;
            }
        }
    }

    /// The next thing to tell the borrower: a transmit slot that is free
    /// again, or with `frames` a frame for a receive slot.
    pub(super) fn next(&self, frames: bool) -> Option<Answer> {
        let mut state = self.state.lock();
        if let Some(slot) = state.tx_done.pop_front() {
            state.tx_busy &= !(1 << slot);
            return Some(Answer::Sent(slot));
        }
        if frames && !state.incoming.is_empty() {
            if let Some(slot) = state.rx_free.pop() {
                state.rx_held[slot as usize] = true;
                let frame = state.incoming.pop_front().expect("not empty");
                return Some(Answer::Frame(slot, frame));
            }
        }
        None
    }

    /// The borrower could not be given the frame of `slot`: the slot is
    /// free again and the frame is lost.
    fn unheld(&self, slot: u32) {
        let mut state = self.state.lock();
        state.rx_held[slot as usize] = false;
        state.rx_free.push(slot);
        state.dropped += 1;
    }

    /// Say, to whoever sleeps on this card, whether [`next`](Self::next)
    /// has something for a turn that takes frames.
    pub(super) fn tell(&self) {
        // With the lock held: two tellers must not cross, the one that saw
        // nothing speaking after the one that saw a frame.
        let state = self.state.lock();
        let ready =
            !state.tx_done.is_empty() || (!state.incoming.is_empty() && !state.rx_free.is_empty());
        if ready {
            self.base.signal_set(Signal::READABLE);
        } else {
            self.base.signal_clear(Signal::READABLE);
        }
    }

    /// Frames lost for want of room, so far.
    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.state.lock().dropped
    }
}

impl Drop for LentCard {
    fn drop(&mut self) {
        self.card.give_back();
    }
}

/// The driver's side of a loan. It does not keep the card lent: when the
/// borrower's handle goes, so does the [`LentCard`], and what the driver
/// still gives meanwhile is dropped.
struct Taker(Arc<lock::Mutex<Weak<LentCard>>>);

impl Taker {
    fn lent(&self) -> Option<Arc<LentCard>> {
        self.0.lock().upgrade()
    }
}

impl FrameTaker for Taker {
    fn take(&self, frame: Vec<u8>) {
        if let Some(lent) = self.lent() {
            lent.arrived(frame);
        }
    }

    fn done(&self) {
        if let Some(lent) = self.lent() {
            lent.flush();
            lent.tell();
        }
    }
}

/// The `index`th card the kernel drives, the loopback not counted.
fn kernel_card(index: usize) -> Option<Arc<dyn NetScheme>> {
    kernel_hal::drivers::all_net()
        .as_vec()
        .iter()
        .filter(|card| !card.is_loopback())
        .nth(index)
        .cloned()
}

/// [`ECLIPSE_NET_BORROW`](super::ECLIPSE_NET_BORROW). Answers the name
/// the card had in the kernel's list.
pub(super) fn net_borrow(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult<String> {
    let (index, window, rx_slots, tx_slots) = (args[0], args[1], args[2], args[3]);
    let mut out = UserOutPtr::<u64>::from(args[4]);
    // Look before taking anything: a bad pointer must not cost the card.
    out.write_array(&[0; 2])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let card = kernel_card(index).ok_or(ZxError::NOT_FOUND)?;
    if !(1..=MAX_RX_SLOTS).contains(&rx_slots) || !(1..=MAX_TX_SLOTS).contains(&tx_slots) {
        return Err(ZxError::OUT_OF_RANGE);
    }
    // Lent first, to a taker with nobody behind it yet: a card that cannot
    // be lent, or is lent, is left as it is.
    let behind: Arc<lock::Mutex<Weak<LentCard>>> = Arc::new(lock::Mutex::new(Weak::new()));
    card.lend(Some(Arc::new(Taker(behind.clone()))))
        .map_err(|e| match e {
            DeviceError::AlreadyExists => ZxError::ALREADY_BOUND,
            _ => ZxError::NOT_SUPPORTED,
        })?;
    // From here on dropping `lent` undoes all of it.
    let lent = LentCard::new(
        Arc::new(KernelCard(card.clone())),
        window,
        rx_slots,
        tx_slots,
    )
    .expect("checked above");
    *behind.lock() = Arc::downgrade(&lent);
    let name = card.get_ifname();
    let mac = card.get_mac().0;
    // Out of the kernel's list: the machine's interfaces are the stack's
    // to show now.
    if !kernel_hal::drivers::hotplug::remove(&Device::Net(card.clone())) {
        warn!("lent card: {name} was not among the kernel's interfaces");
    }
    let handle = proc.add_handle(Handle::new(
        lent.clone(),
        Rights::BASIC | Rights::READ | Rights::WRITE,
    ));
    let mac = mac
        .iter()
        .enumerate()
        .fold(0u64, |word, (i, byte)| word | (*byte as u64) << (8 * i));
    if out.write_array(&[handle as u64, mac]).is_err() {
        let _ = proc.remove_handle(handle);
        return Err(ZxError::INVALID_ARGS);
    }
    info!("lent card: {name} is lent to process {}", proc.id());
    // What it has taken in until now is the kernel's stack's, not the
    // borrower's: the first look starts from what arrives after.
    let weak = Arc::downgrade(&lent);
    drop(lent);
    kernel_hal::thread::spawn(async move {
        loop {
            kernel_hal::thread::sleep_until(kernel_hal::timer::timer_now() + LOOK).await;
            let Some(lent) = weak.upgrade() else {
                return;
            };
            lent.card.look();
        }
    });
    Ok(name)
}

/// A turn whose messages are given: what is left is the answer, and the
/// wait for one.
pub(super) struct Turn {
    card: Arc<LentCard>,
    /// The caller's buffer, and how many messages of it may be filled.
    msgs: usize,
    room: usize,
    flags: usize,
    /// Where the count of the answer goes.
    actual: usize,
}

/// [`ECLIPSE_NET_LENT_TURN`](super::ECLIPSE_NET_LENT_TURN) up to the wait:
/// the borrower's messages taken, and its frames given to the card.
pub(super) fn give_then(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult<Turn> {
    let card =
        proc.get_object_with_rights::<LentCard>(args[0] as u32, Rights::READ | Rights::WRITE)?;
    let (msgs, give, room, flags, actual) = (args[1], args[2], args[3], args[4], args[5]);
    if give > card.slots() || room > card.slots() {
        return Err(ZxError::OUT_OF_RANGE);
    }
    UserOutPtr::<usize>::from(actual)
        .write(0)
        .map_err(|_| ZxError::INVALID_ARGS)?;
    if give != 0 {
        let bytes: Vec<u8> = UserInPtr::<u8>::from(msgs)
            .read_array(give * NET_MSG)
            .map_err(|_| ZxError::INVALID_ARGS)?;
        for m in bytes.as_chunks::<NET_MSG>().0 {
            card.given(decode(m), |at, len| {
                UserInPtr::<u8>::from(card.window.checked_add(at)?)
                    .read_array(len)
                    .ok()
            });
        }
        card.flush();
        card.tell();
    }
    Ok(Turn {
        card,
        msgs,
        room,
        flags,
        actual,
    })
}

impl Turn {
    /// The answer: what there is to say now, or with [`LENT_WAIT`] what
    /// there is once there is something.
    pub(super) async fn wait_then_answer(self) -> ZxResult {
        let frames = self.flags & LENT_FRAMES != 0;
        let mut out: Vec<u8> = Vec::new();
        loop {
            while out.len() < self.room * NET_MSG {
                match self.card.next(frames) {
                    Some(Answer::Sent(slot)) => out.extend_from_slice(&encode(OP_TX, slot, 0, 0)),
                    Some(Answer::Frame(slot, frame)) => {
                        let at = slot as usize * SLOT;
                        let put = self.card.window.checked_add(at).is_some_and(|place| {
                            UserOutPtr::<u8>::from(place).write_array(&frame).is_ok()
                        });
                        if !put {
                            self.card.unheld(slot);
                            self.card.tell();
                            return Err(ZxError::INVALID_ARGS);
                        }
                        out.extend_from_slice(&encode(OP_RX, slot, frame.len() as u32, 0));
                    }
                    None => break,
                }
            }
            self.card.tell();
            if !out.is_empty() || self.flags & LENT_WAIT == 0 || self.room == 0 {
                break;
            }
            let object: Arc<dyn KernelObject> = self.card.clone();
            object.wait_signal(Signal::READABLE).await;
        }
        if !out.is_empty() {
            UserOutPtr::<u8>::from(self.msgs)
                .write_array(&out)
                .map_err(|_| ZxError::INVALID_ARGS)?;
        }
        UserOutPtr::<usize>::from(self.actual)
            .write(out.len() / NET_MSG)
            .map_err(|_| ZxError::INVALID_ARGS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card that keeps what it is sent and can be full.
    #[derive(Default)]
    struct Wire {
        sent: lock::Mutex<Vec<Vec<u8>>>,
        /// How many more frames it takes before it is full.
        room: lock::Mutex<usize>,
        /// How many times it was given back.
        back: lock::Mutex<usize>,
    }

    impl Wire {
        fn with_room(room: usize) -> Arc<Self> {
            let wire = Arc::new(Wire::default());
            *wire.room.lock() = room;
            wire
        }
    }

    impl Card for Wire {
        fn send(&self, frame: &[u8]) -> Result<(), DeviceError> {
            let mut room = self.room.lock();
            if *room == 0 {
                return Err(DeviceError::NotReady);
            }
            *room -= 1;
            self.sent.lock().push(frame.to_vec());
            Ok(())
        }
        fn look(&self) {}
        fn give_back(&self) {
            *self.back.lock() += 1;
        }
    }

    fn lent(wire: &Arc<Wire>, rx: usize, tx: usize) -> Arc<LentCard> {
        LentCard::new(wire.clone(), 0, rx, tx).expect("slots a window can have")
    }

    fn ready(card: &LentCard) -> bool {
        card.tell();
        card.base.signal().contains(Signal::READABLE)
    }

    /// A frame of `len` bytes that says which it is.
    fn frame(mark: u8, len: usize) -> Vec<u8> {
        alloc::vec![mark; len]
    }

    #[test]
    fn a_window_has_some_slots_on_each_side_and_not_too_many() {
        let wire = Wire::with_room(0);
        assert!(LentCard::new(wire.clone(), 0, 1, 1).is_some());
        assert!(LentCard::new(wire.clone(), 0, MAX_RX_SLOTS, MAX_TX_SLOTS).is_some());
        assert!(LentCard::new(wire.clone(), 0, 0, 1).is_none());
        assert!(LentCard::new(wire.clone(), 0, 1, 0).is_none());
        assert!(LentCard::new(wire.clone(), 0, MAX_RX_SLOTS + 1, 1).is_none());
        assert!(LentCard::new(wire, 0, 1, MAX_TX_SLOTS + 1).is_none());
    }

    #[test]
    fn frames_that_arrive_go_to_free_receive_slots_in_order() {
        let wire = Wire::with_room(0);
        let card = lent(&wire, 2, 1);
        assert!(!ready(&card));
        assert_eq!(card.next(true), None);
        for mark in 1..=3 {
            card.arrived(frame(mark, 60));
        }
        assert!(ready(&card));
        // A turn that takes no frames is told of none.
        assert_eq!(card.next(false), None);
        assert_eq!(card.next(true), Some(Answer::Frame(0, frame(1, 60))));
        assert_eq!(card.next(true), Some(Answer::Frame(1, frame(2, 60))));
        // Both slots are with the borrower: the third frame waits, and
        // nobody is called for it.
        assert_eq!(card.next(true), None);
        assert!(!ready(&card));
        // A slot given back is where it goes.
        card.given((OP_RX, 1, 0, 0), |_, _| None);
        assert!(ready(&card));
        assert_eq!(card.next(true), Some(Answer::Frame(1, frame(3, 60))));
        assert!(!ready(&card));
    }

    #[test]
    fn a_receive_slot_comes_back_once_and_only_if_it_was_held() {
        let wire = Wire::with_room(0);
        let card = lent(&wire, 2, 1);
        // Never held, and no such slot.
        card.given((OP_RX, 0, 0, 0), |_, _| None);
        card.given((OP_RX, 9, 0, 0), |_, _| None);
        card.arrived(frame(1, 60));
        card.arrived(frame(2, 60));
        card.arrived(frame(3, 60));
        assert_eq!(card.next(true), Some(Answer::Frame(0, frame(1, 60))));
        assert_eq!(card.next(true), Some(Answer::Frame(1, frame(2, 60))));
        // Given back twice: one frame goes there, not two.
        card.given((OP_RX, 0, 0, 0), |_, _| None);
        card.given((OP_RX, 0, 0, 0), |_, _| None);
        assert_eq!(card.next(true), Some(Answer::Frame(0, frame(3, 60))));
        card.arrived(frame(4, 60));
        assert_eq!(card.next(true), None);
    }

    #[test]
    fn a_frame_to_send_goes_to_the_card_at_once_and_its_slot_comes_back() {
        let wire = Wire::with_room(8);
        let card = lent(&wire, 2, 4);
        let mut asked = None;
        card.given((OP_TX, 3, 100, 16), |at, len| {
            asked = Some((at, len));
            Some(frame(7, len))
        });
        // The frame is read where the message says: past the receive
        // slots, in its own, after the headroom.
        assert_eq!(asked, Some(((2 + 3) * SLOT + 16, 100)));
        card.flush();
        assert_eq!(*wire.sent.lock(), [frame(7, 100)]);
        assert!(ready(&card));
        // A turn that takes no frames is still told of its slots.
        assert_eq!(card.next(false), Some(Answer::Sent(3)));
        assert_eq!(card.next(false), None);
        assert!(!ready(&card));
        // And the slot can be used again.
        card.given((OP_TX, 3, 60, 16), |_, len| Some(frame(8, len)));
        card.flush();
        assert_eq!(wire.sent.lock().len(), 2);
    }

    #[test]
    fn frames_the_card_has_no_room_for_wait_and_go_in_order() {
        let wire = Wire::with_room(1);
        let card = lent(&wire, 1, 4);
        for slot in 0..3 {
            card.given((OP_TX, slot, 60, 16), |_, len| Some(frame(slot as u8, len)));
        }
        card.flush();
        // One went; the others wait, and their slots are not free.
        assert_eq!(*wire.sent.lock(), [frame(0, 60)]);
        assert_eq!(card.next(false), Some(Answer::Sent(0)));
        assert_eq!(card.next(false), None);
        // A slot whose frame waits cannot be given again.
        card.given((OP_TX, 1, 60, 16), |_, _| {
            panic!("read for a slot that is busy")
        });
        // The card is through with something: the rest go, in order.
        *wire.room.lock() = 8;
        card.flush();
        assert_eq!(
            *wire.sent.lock(),
            [frame(0, 60), frame(1, 60), frame(2, 60)]
        );
        assert_eq!(card.next(false), Some(Answer::Sent(1)));
        assert_eq!(card.next(false), Some(Answer::Sent(2)));
        assert_eq!(card.next(false), None);
    }

    #[test]
    fn a_frame_to_send_outside_its_slot_is_dropped() {
        let wire = Wire::with_room(8);
        let card = lent(&wire, 1, 2);
        let never = |_: usize, _: usize| -> Option<Vec<u8>> { panic!("read outside a slot") };
        card.given((OP_TX, 2, 60, 16), never);
        card.given((OP_TX, 0, 0, 16), never);
        card.given((OP_TX, 0, 1515, 16), never);
        card.given((OP_TX, 0, 1514, 600), never);
        card.given((OP_TX, 0, 60, u32::MAX), never);
        card.given((7, 0, 60, 16), never);
        card.flush();
        assert!(wire.sent.lock().is_empty());
        assert_eq!(card.next(false), None);
        // A frame that cannot be read is not sent, and its slot comes back.
        card.given((OP_TX, 1, 60, 16), |_, _| None);
        card.flush();
        assert!(wire.sent.lock().is_empty());
        assert_eq!(card.next(false), Some(Answer::Sent(1)));
    }

    #[test]
    fn a_backlog_that_is_full_drops_the_newest() {
        let wire = Wire::with_room(0);
        let card = lent(&wire, 1, 1);
        for i in 0..RX_BACKLOG + 5 {
            card.arrived(frame(i as u8, 60));
        }
        assert_eq!(card.dropped(), 5);
        // A frame no card delivers is not kept either.
        card.arrived(Vec::new());
        card.arrived(frame(0, 1515));
        assert_eq!(card.dropped(), 7);
        assert_eq!(card.next(true), Some(Answer::Frame(0, frame(0, 60))));
    }

    #[test]
    fn the_driver_gives_frames_while_the_loan_lasts_and_the_card_goes_back_after() {
        let wire = Wire::with_room(1);
        let card = lent(&wire, 2, 2);
        let taker = Taker(Arc::new(lock::Mutex::new(Arc::downgrade(&card))));
        // A frame waits for room on the card.
        card.given((OP_TX, 0, 60, 16), |_, len| Some(frame(1, len)));
        card.given((OP_TX, 1, 60, 16), |_, len| Some(frame(2, len)));
        card.flush();
        assert_eq!(wire.sent.lock().len(), 1);
        // The driver's word that it is through sends it, and calls the
        // borrower for the frame that came.
        *wire.room.lock() = 1;
        taker.take(frame(9, 60));
        assert!(!card.base.signal().contains(Signal::READABLE));
        taker.done();
        assert_eq!(wire.sent.lock().len(), 2);
        assert!(card.base.signal().contains(Signal::READABLE));
        assert_eq!(*wire.back.lock(), 0);
        // The loan ends with the last handle: the card is given back, once,
        // and what the driver still gives goes nowhere.
        drop(card);
        assert_eq!(*wire.back.lock(), 1);
        taker.take(frame(9, 60));
        taker.done();
        assert_eq!(*wire.back.lock(), 1);
    }
}
