//! The one output stream of a card, as the nodes over it share it:
//! `/dev/dsp` ([`crate::oss`]) and the ALSA PCM ([`crate::alsa`]) both
//! queue 16-bit stereo into its ring ([`Ring`]) and start, pause and stop
//! it. One open plays at a time ([`Owner`]): the controller plays one
//! stream, as a Linux card that does not mix, where the second open is
//! `EBUSY`. The card's volume ([`Gain`]) is applied to what is queued, as
//! the kernel's HDA driver applies it.

use crate::hda::{Ring, FRAME, GUARD, LEAD, RING_LEN};

pub const EIO: i32 = 5;

/// What plays the ring.
pub trait Output {
    /// The ring, as the driver sees it.
    fn ring(&mut self) -> &mut [u8];
    fn running(&self) -> bool;
    /// Start from the top of the ring at `rate`.
    fn run(&mut self, rate: u32) -> Result<(), String>;
    /// Stop where it is.
    fn stop(&mut self);
    /// Go on from where it stopped.
    fn resume(&mut self);
    /// Where in the ring the stream is.
    fn position(&self) -> usize;
}

/// The open that plays: which node's, and its name there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Dsp(u32),
    Pcm(u32),
}

/// The card's volume: each side's, in percent, and whether it is muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gain {
    pub left: u8,
    pub right: u8,
    pub mute_left: bool,
    pub mute_right: bool,
}

impl Default for Gain {
    fn default() -> Gain {
        Gain {
            left: 100,
            right: 100,
            mute_left: false,
            mute_right: false,
        }
    }
}

impl Gain {
    fn unity(&self) -> bool {
        *self == Gain::default()
    }

    fn apply(&self, sample: i16, left: bool) -> i16 {
        let (level, mute) = if left {
            (self.left, self.mute_left)
        } else {
            (self.right, self.mute_right)
        };
        if mute {
            0
        } else {
            (sample as i32 * level.min(100) as i32 / 100) as i16
        }
    }
}

/// The most bytes of samples the ring holds: past its guard, and the lead
/// a write into an empty ring leaves ahead of the controller.
pub const CAPACITY: usize = RING_LEN - GUARD - LEAD;

pub struct Stream<O> {
    pub out: O,
    pub ring: Ring,
    pub rate: u32,
    pub owner: Option<Owner>,
    pub gain: Gain,
    paused: bool,
    /// Since when nothing has been left to play.
    idle_since: Option<u64>,
}

impl<O: Output> Stream<O> {
    pub fn new(out: O) -> Stream<O> {
        Stream {
            out,
            ring: Ring::default(),
            rate: 8000,
            owner: None,
            gain: Gain::default(),
            paused: false,
            idle_since: None,
        }
    }

    pub fn running(&self) -> bool {
        self.out.running()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Stop, and forget what was queued.
    pub fn reset(&mut self) {
        if self.out.running() || self.paused {
            self.out.stop();
        }
        self.paused = false;
        self.out.ring().fill(0);
        self.ring = Ring::default();
        self.idle_since = None;
    }

    /// Play at `rate`: the controller plays one rate, and what was queued
    /// at another goes.
    pub fn set_rate(&mut self, rate: u32) {
        if rate != self.rate {
            self.reset();
            self.rate = rate;
        }
    }

    /// Room, in bytes of the ring.
    pub fn free(&self) -> usize {
        self.ring.free()
    }

    /// Queue `pcm`, 16-bit stereo, as much as fits in whole frames, at the
    /// card's volume. It plays once the stream is started.
    pub fn write(&mut self, pcm: &[u8]) -> usize {
        let n = pcm.len().min(self.ring.free()) / FRAME * FRAME;
        let scaled;
        let data = if self.gain.unity() {
            &pcm[..n]
        } else {
            let g = self.gain;
            scaled = pcm[..n]
                .chunks_exact(2)
                .enumerate()
                .flat_map(|(i, b)| {
                    g.apply(i16::from_le_bytes([b[0], b[1]]), i % 2 == 0)
                        .to_le_bytes()
                })
                .collect::<Vec<u8>>();
            &scaled[..]
        };
        let mut ring = std::mem::take(&mut self.ring);
        let n = ring.write(self.out.ring(), data);
        self.ring = ring;
        self.idle_since = None;
        n
    }

    /// Play what is queued: from the top of the ring when stopped, on from
    /// where it was when paused.
    pub fn start(&mut self) -> Result<(), i32> {
        if self.paused {
            self.out.resume();
            self.paused = false;
        } else if !self.out.running() {
            self.out.run(self.rate).map_err(|_| EIO)?;
        }
        Ok(())
    }

    /// Stop where it is, keeping what is queued.
    pub fn pause(&mut self) {
        if self.out.running() {
            self.out.stop();
            self.paused = true;
        }
    }

    /// The controller played on: what it played is given back. How many
    /// bytes that was.
    pub fn follow(&mut self) -> usize {
        if !self.out.running() {
            return 0;
        }
        let pos = self.out.position();
        let mut ring = std::mem::take(&mut self.ring);
        let n = ring.played_to(self.out.ring(), pos);
        self.ring = ring;
        n
    }

    /// A stream with nothing to play that has run so for `idle_ns` is
    /// stopped. `now` is the clock's.
    pub fn stop_if_idle(&mut self, now: u64, idle_ns: u64) {
        if !self.out.running() {
            return;
        }
        if self.ring.queued() == 0 {
            let since = *self.idle_since.get_or_insert(now);
            if now.saturating_sub(since) > idle_ns {
                self.reset();
            }
        } else {
            self.idle_since = None;
        }
    }

    /// Bytes still to play, the silence ahead of the samples included.
    pub fn queued(&self) -> usize {
        self.ring.queued()
    }

    /// Bytes of samples still to play.
    pub fn queued_samples(&self) -> usize {
        self.ring.queued_samples()
    }

    /// Take back up to `bytes` of the samples written last and not yet
    /// played. How many bytes that was.
    pub fn rewind(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.ring.queued_samples()) / FRAME * FRAME;
        let end = self.ring.written - n as u64;
        self.zero(end, n);
        self.ring.written = end;
        n
    }

    /// Silence up to `bytes` of the samples that play next, which still
    /// take their time. How many bytes that was.
    pub fn forward(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.ring.queued_samples()) / FRAME * FRAME;
        let from = self.ring.played.max(self.ring.start);
        self.zero(from, n);
        n
    }

    fn zero(&mut self, from: u64, n: usize) {
        let ring = self.out.ring();
        let mut done = 0;
        while done < n {
            let at = ((from + done as u64) % RING_LEN as u64) as usize;
            let k = (n - done).min(RING_LEN - at);
            ring[at..at + k].fill(0);
            done += k;
        }
    }
}

impl Output for crate::hda::Controller {
    fn ring(&mut self) -> &mut [u8] {
        crate::hda::Controller::ring(self)
    }
    fn running(&self) -> bool {
        self.running
    }
    fn run(&mut self, rate: u32) -> Result<(), String> {
        self.rate = rate;
        crate::hda::Controller::run(self)
    }
    fn stop(&mut self) {
        crate::hda::Controller::stop(self)
    }
    fn resume(&mut self) {
        crate::hda::Controller::resume(self)
    }
    fn position(&self) -> usize {
        self.lpib()
    }
}

#[cfg(test)]
pub(crate) mod tests;
