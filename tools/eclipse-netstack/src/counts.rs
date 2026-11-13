//! What the card has carried, and when the kernel is told.
//!
//! The frames go between the driver and this process and the kernel sees
//! none of them, but it is the kernel that shows the machine's interfaces
//! (`/proc/net/dev`, where `ifconfig` reads how much each has carried).
//! So the port counts ([`Counts`]) and the kernel is told, in a message
//! on the sockets' channel:
//!
//! ```text
//! COUNTS  (stack to kernel)  16  eight u64: received bytes, frames, errors, dropped,
//!                                           then sent bytes, frames, errors, dropped
//! ```
//!
//! behind the header every message has (`socks`), its socket zero. Each
//! number is how many since this process started. Not for each frame: a
//! message is a wake of the kernel's task, and a count is looked at a few
//! times a second at most. The kernel is told at once of the first frame
//! after a quiet while, and then no more often than every [`EVERY`]
//! ([`Teller`]): whoever carried a frame that it is too soon to tell
//! asks the thread that keeps time to tell it when it is due, so what
//! the kernel shows is never older than that by much.
//!
//! What is counted is what the stack sees: a frame the card delivered and
//! a frame given to the card to send, with the length each has on the
//! wire less its checksum. A message of the driver's that names no frame
//! is an error of the receiving side. What the card or its driver drops
//! before it gets here is not known here, and is counted as nothing.

use smoltcp::time::{Duration, Instant};

use crate::socks::HEADER_LEN;

/// The operation of the message.
pub const OP_COUNTS: u32 = 13;
/// The bytes of the message.
pub const COUNTS_LEN: usize = HEADER_LEN + 8 * 8;
/// The least time between two messages.
pub const EVERY: Duration = Duration::from_millis(100);

/// What a card has carried since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    /// Messages of the driver's that said a frame arrived and named none.
    pub rx_errors: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
}

impl Counts {
    /// A frame of `len` bytes arrived.
    pub fn received(&mut self, len: usize) {
        self.rx_packets += 1;
        self.rx_bytes += len as u64;
    }

    /// A frame of `len` bytes goes to the card.
    pub fn sent(&mut self, len: usize) {
        self.tx_packets += 1;
        self.tx_bytes += len as u64;
    }

    /// The `COUNTS` that says them.
    pub fn message(&self) -> [u8; COUNTS_LEN] {
        let numbers = [
            self.rx_bytes,
            self.rx_packets,
            self.rx_errors,
            0,
            self.tx_bytes,
            self.tx_packets,
            0,
            0,
        ];
        let mut m = [0u8; COUNTS_LEN];
        m[4..8].copy_from_slice(&OP_COUNTS.to_le_bytes());
        for (i, number) in numbers.iter().enumerate() {
            let at = HEADER_LEN + i * 8;
            m[at..at + 8].copy_from_slice(&number.to_le_bytes());
        }
        m
    }
}

/// What the kernel was told last, and when.
#[derive(Default)]
pub struct Teller {
    told: Counts,
    at: Option<Instant>,
    /// Whether somebody has been asked to tell it the rest
    /// ([`owes`](Self::owes)).
    owed: bool,
}

impl Teller {
    /// How long until the kernel is to be told that the card has carried
    /// `counts`: `None` when it knows, zero when it is to be told now.
    pub fn due_in(&self, counts: &Counts, now: Instant) -> Option<Duration> {
        if *counts == self.told {
            return None;
        }
        let Some(at) = self.at else {
            return Some(Duration::ZERO);
        };
        // A clock that went back is no reason to wait longer than this.
        let since = if now >= at { now - at } else { EVERY };
        Some(if since >= EVERY {
            Duration::ZERO
        } else {
            EVERY - since
        })
    }

    /// The message to send the kernel now, at `now`, when one is due
    /// ([`due_in`](Self::due_in)): it is taken as told.
    pub fn tell(&mut self, counts: &Counts, now: Instant) -> Option<[u8; COUNTS_LEN]> {
        if self.due_in(counts, now)? != Duration::ZERO {
            return None;
        }
        self.told = *counts;
        self.at = Some(now);
        self.owed = false;
        Some(counts.message())
    }

    /// Whether the kernel is behind `counts` and nobody has been asked
    /// yet to tell it when that is due: `true` once for each message
    /// that is owed, for whoever asks to wake the one that keeps time.
    pub fn owes(&mut self, counts: &Counts) -> bool {
        if self.owed || *counts == self.told {
            return false;
        }
        self.owed = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> Instant {
        Instant::from_millis(ms)
    }

    #[test]
    fn the_message_has_the_eight_numbers_in_their_order() {
        let mut counts = Counts::default();
        counts.received(60);
        counts.received(1514);
        counts.sent(42);
        counts.rx_errors = 3;
        let m = counts.message();
        assert_eq!(m.len(), 80);
        assert_eq!(crate::socks::parse_header(&m), Some((OP_COUNTS, 0)));
        let number = |i: usize| u64::from_le_bytes(m[16 + i * 8..24 + i * 8].try_into().unwrap());
        let numbers: Vec<u64> = (0..8).map(number).collect();
        assert_eq!(numbers, [1574, 2, 3, 0, 42, 1, 0, 0]);
    }

    #[test]
    fn the_kernel_is_told_at_once_and_then_not_more_often_than_every_so_long() {
        let mut teller = Teller::default();
        let mut counts = Counts::default();
        // Nothing carried, nothing to say.
        assert_eq!(teller.due_in(&counts, at(0)), None);
        assert_eq!(teller.tell(&counts, at(0)), None);

        // The first frame after a quiet while is told at once.
        counts.sent(42);
        assert_eq!(teller.due_in(&counts, at(5000)), Some(Duration::ZERO));
        assert_eq!(teller.tell(&counts, at(5000)), Some(counts.message()));
        assert_eq!(teller.due_in(&counts, at(5000)), None);

        // Its answer, a millisecond later, waits for the rest of the
        // time, however many more come meanwhile.
        assert!(!teller.owes(&counts));
        counts.received(60);
        assert_eq!(
            teller.due_in(&counts, at(5001)),
            Some(Duration::from_millis(99))
        );
        assert_eq!(teller.tell(&counts, at(5001)), None);
        // Which is owed, and whoever keeps time is asked once to say it.
        assert!(teller.owes(&counts));
        counts.received(60);
        assert_eq!(teller.tell(&counts, at(5099)), None);
        assert!(!teller.owes(&counts));
        let told = teller.tell(&counts, at(5100)).expect("it is due");
        assert_eq!(told, counts.message());
        assert_eq!(teller.tell(&counts, at(5300)), None);

        // After a quiet while, at once again.
        assert!(!teller.owes(&counts));
        counts.sent(42);
        assert_eq!(teller.due_in(&counts, at(9000)), Some(Duration::ZERO));
        assert!(teller.tell(&counts, at(9000)).is_some());
        // And a clock that went back does not keep it waiting.
        counts.sent(42);
        assert_eq!(teller.due_in(&counts, at(1000)), Some(Duration::ZERO));
    }
}
