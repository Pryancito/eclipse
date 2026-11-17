//! What the kernel and a process that has a card say to each other, and
//! whose turn it is to have it.
//!
//! A message is `op, slot, len, off` (four `u32`s) on a FIFO, about a
//! window both sides reach, cut into slots of [`SLOT`] bytes. A card the
//! kernel lends to the network stack that runs as a process speaks it
//! ([`net_lend`](super::net_lend)); a card a process drove for the
//! kernel's own stack spoke it too, while the kernel had one.

use alloc::sync::Arc;

/// One FIFO element: `op, slot, len, off`, a `u32` each.
pub(super) const NET_MSG: usize = 16;
pub(super) const OP_RX: u32 = 1;
pub(super) const OP_TX: u32 = 2;
/// A slot of the window: a frame and the card's header, with room.
pub(super) const SLOT: usize = 2048;
/// The most transmit slots: one bit each in a `u32`.
pub(super) const MAX_TX_SLOTS: usize = 32;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
