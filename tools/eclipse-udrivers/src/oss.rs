//! `/dev/dsp` as Linux's OSS emulation answers it, over one output stream:
//! what `linux-object/src/fs/devfs/dsp.rs` does in the kernel, for a
//! driver that serves the node ([`crate::served`]).
//!
//! One open at a time plays (a second one is `EBUSY`, as on a Linux card
//! that does not mix). It sets the rate, the format and the channels, and
//! writes; what it writes is made 16-bit stereo on the way into the ring
//! ([`crate::hda::Ring`]), so the controller plays one format whatever the
//! client's. A write takes what fits and answers `EAGAIN` when nothing
//! does: the server never waits, a blocking writer waits in the kernel
//! until the node says there is room again ([`POLLOUT`]), which
//! [`Dsp::tick`] says as the controller plays on. A stream that has had
//! nothing to play for [`IDLE_NS`] is stopped.

use crate::hda::{nearest_rate, Ring, FRAME, GUARD, RING_LEN};
use crate::served::{Device, Node, EAGAIN, EBUSY, EINVAL, POLLOUT};

pub const SNDCTL_DSP_RESET: u32 = 0x0000_5000;
pub const SNDCTL_DSP_SYNC: u32 = 0x0000_5001;
pub const SNDCTL_DSP_SPEED: u32 = 0xc004_5002;
pub const SOUND_PCM_READ_RATE: u32 = 0x8004_5002;
pub const SNDCTL_DSP_STEREO: u32 = 0xc004_5003;
pub const SNDCTL_DSP_GETBLKSIZE: u32 = 0xc004_5004;
pub const SNDCTL_DSP_SETFMT: u32 = 0xc004_5005;
pub const SOUND_PCM_READ_BITS: u32 = 0x8004_5005;
pub const SNDCTL_DSP_CHANNELS: u32 = 0xc004_5006;
pub const SOUND_PCM_READ_CHANNELS: u32 = 0x8004_5006;
pub const SNDCTL_DSP_POST: u32 = 0x0000_5008;
pub const SNDCTL_DSP_SETFRAGMENT: u32 = 0xc004_500a;
pub const SNDCTL_DSP_GETFMTS: u32 = 0x8004_500b;
pub const SNDCTL_DSP_GETOSPACE: u32 = 0x8010_500c;
pub const SNDCTL_DSP_NONBLOCK: u32 = 0x0000_500e;
pub const SNDCTL_DSP_GETCAPS: u32 = 0x8004_500f;
pub const SNDCTL_DSP_GETTRIGGER: u32 = 0x8004_5010;
pub const SNDCTL_DSP_SETTRIGGER: u32 = 0x4004_5010;
pub const SNDCTL_DSP_GETOPTR: u32 = 0x800c_5012;
pub const SNDCTL_DSP_SETDUPLEX: u32 = 0x0000_5016;
pub const SNDCTL_DSP_GETODELAY: u32 = 0x8004_5017;
pub const OSS_GETVERSION: u32 = 0x8004_4d76;

pub const AFMT_QUERY: i32 = 0;
pub const AFMT_U8: i32 = 0x08;
pub const AFMT_S16_LE: i32 = 0x10;
pub const AFMT_S16_BE: i32 = 0x20;
pub const AFMT_S8: i32 = 0x40;
const FORMATS: i32 = AFMT_U8 | AFMT_S16_LE | AFMT_S16_BE | AFMT_S8;
const PCM_ENABLE_OUTPUT: i32 = 2;
/// Revision 1, a position that is real time.
const CAPS: i32 = 1 | 0x4;
/// Linux's `SNDRV_OSS_VERSION`.
const OSS_VERSION: i32 = (3 << 16) | (8 << 8) | (1 << 4) | 50;
const EIO: i32 = 5;

/// A fragment, in bytes of the ring: what `GETBLKSIZE` says (in the
/// client's bytes), and the room the node waits for before it says a
/// writer can write.
pub const FRAGMENT: usize = 4096;
/// How long a stream with nothing to play keeps running.
pub const IDLE_NS: u64 = 2_000_000_000;

/// What plays the ring.
pub trait Output {
    /// The ring, as the driver sees it.
    fn ring(&mut self) -> &mut [u8];
    fn running(&self) -> bool;
    /// Start from the top of the ring at `rate`.
    fn run(&mut self, rate: u32) -> Result<(), String>;
    fn stop(&mut self);
    /// Where in the ring the stream is.
    fn position(&self) -> usize;
}

/// The node: the stream, and the open that plays on it.
pub struct Dsp<O> {
    pub out: O,
    ring: Ring,
    open: Option<u32>,
    next_fh: u32,
    format: i32,
    channels: u32,
    rate: u32,
    /// The readiness the open was last told.
    said: u64,
    /// Since when nothing has been left to play.
    idle_since: Option<u64>,
    /// What `GETOPTR` counted its fragments from.
    optr_played: u64,
    clock: fn() -> u64,
}

impl<O: Output> Dsp<O> {
    pub fn new(out: O, clock: fn() -> u64) -> Dsp<O> {
        Dsp {
            out,
            ring: Ring::default(),
            open: None,
            next_fh: 1,
            format: AFMT_U8,
            channels: 1,
            rate: 8000,
            said: 0,
            idle_since: None,
            optr_played: 0,
            clock,
        }
    }

    /// The bytes of one of the client's frames.
    fn client_frame(&self) -> usize {
        let sample = if matches!(self.format, AFMT_S16_LE | AFMT_S16_BE) {
            2
        } else {
            1
        };
        sample * self.channels as usize
    }

    /// `ring` bytes of the ring, in the client's bytes.
    fn client_bytes(&self, ring: usize) -> usize {
        ring / FRAME * self.client_frame()
    }

    fn readiness(&self) -> u64 {
        if self.ring.free() >= FRAGMENT {
            POLLOUT
        } else {
            0
        }
    }

    fn tell(&mut self, node: Option<&Node>) {
        let now = self.readiness();
        if let (Some(fh), true) = (self.open, now != self.said) {
            self.said = now;
            if let Some(node) = node {
                node.ready(fh, now);
            }
        }
    }

    /// Stop, and forget what was queued.
    fn reset(&mut self) {
        if self.out.running() {
            self.out.stop();
        }
        self.out.ring().fill(0);
        self.ring = Ring::default();
        self.optr_played = 0;
        self.idle_since = None;
    }

    /// One of the client's frames as the ring takes it.
    fn convert(&self, frame: &[u8], out: &mut Vec<u8>) {
        let sample = |i: usize| -> i16 {
            match self.format {
                AFMT_S16_LE => i16::from_le_bytes([frame[2 * i], frame[2 * i + 1]]),
                AFMT_S16_BE => i16::from_be_bytes([frame[2 * i], frame[2 * i + 1]]),
                AFMT_S8 => (frame[i] as i8 as i16) << 8,
                _ => ((frame[i] as i16) - 128) << 8,
            }
        };
        let left = sample(0);
        let right = if self.channels == 2 { sample(1) } else { left };
        out.extend_from_slice(&left.to_le_bytes());
        out.extend_from_slice(&right.to_le_bytes());
    }

    /// The controller played on: what it played is given back, a stream
    /// left with nothing is stopped after [`IDLE_NS`], and the open is told
    /// when there is room again.
    pub fn tick(&mut self, node: Option<&Node>) {
        if self.out.running() {
            let pos = self.out.position();
            let mut ring = std::mem::take(&mut self.ring);
            ring.played_to(self.out.ring(), pos);
            self.ring = ring;
            let now = (self.clock)();
            if self.ring.queued() == 0 {
                let since = *self.idle_since.get_or_insert(now);
                if now.saturating_sub(since) > IDLE_NS {
                    self.reset();
                }
            } else {
                self.idle_since = None;
            }
        }
        self.tell(node);
    }

    fn set_rate(&mut self, rate: i32) -> u32 {
        let rate = nearest_rate(rate.max(1) as u32);
        if rate != self.rate {
            // The controller plays one rate: what was queued at the other
            // one goes.
            self.reset();
            self.rate = rate;
        }
        rate
    }

    /// `ioctl` on the open, with its `int` (or structure) in `data`.
    fn control(&mut self, cmd: u32, data: &mut [u8]) -> Result<(i64, usize), i32> {
        let int = |d: &[u8]| i32::from_ne_bytes(d[..4].try_into().unwrap());
        let put = |d: &mut [u8], v: i32| {
            d[..4].copy_from_slice(&v.to_ne_bytes());
            Ok((0, 4))
        };
        if data.len() < ((cmd >> 16) & 0x3fff) as usize {
            return Err(crate::served::EFAULT);
        }
        match cmd {
            OSS_GETVERSION => put(data, OSS_VERSION),
            SNDCTL_DSP_RESET => {
                self.reset();
                Ok((0, 0))
            }
            // Nothing waits in the server: what is queued plays on.
            SNDCTL_DSP_SYNC | SNDCTL_DSP_POST | SNDCTL_DSP_NONBLOCK | SNDCTL_DSP_SETTRIGGER => {
                Ok((0, 0))
            }
            SNDCTL_DSP_SPEED => {
                let rate = self.set_rate(int(data));
                put(data, rate as i32)
            }
            SOUND_PCM_READ_RATE => put(data, self.rate as i32),
            SNDCTL_DSP_SETFMT => {
                let want = int(data);
                if want != AFMT_QUERY && want & FORMATS == want && want.count_ones() == 1 {
                    self.format = want;
                }
                put(data, self.format)
            }
            SOUND_PCM_READ_BITS => put(
                data,
                if self.client_frame() / self.channels as usize == 2 {
                    16
                } else {
                    8
                },
            ),
            SNDCTL_DSP_GETFMTS => put(data, FORMATS),
            SNDCTL_DSP_CHANNELS => {
                self.channels = if int(data) >= 2 { 2 } else { 1 };
                put(data, self.channels as i32)
            }
            SNDCTL_DSP_STEREO => {
                self.channels = if int(data) != 0 { 2 } else { 1 };
                put(data, self.channels as i32 - 1)
            }
            SOUND_PCM_READ_CHANNELS => put(data, self.channels as i32),
            SNDCTL_DSP_GETBLKSIZE => put(data, self.client_bytes(FRAGMENT) as i32),
            SNDCTL_DSP_SETFRAGMENT => Ok((0, 0)),
            SNDCTL_DSP_GETOSPACE => {
                let free = self.ring.free();
                let fields = [
                    (free / FRAGMENT) as i32,
                    ((RING_LEN - GUARD) / FRAGMENT) as i32,
                    self.client_bytes(FRAGMENT) as i32,
                    self.client_bytes(free) as i32,
                ];
                for (i, v) in fields.iter().enumerate() {
                    data[4 * i..4 * i + 4].copy_from_slice(&v.to_ne_bytes());
                }
                Ok((0, 16))
            }
            SNDCTL_DSP_GETODELAY => put(data, self.client_bytes(self.ring.queued()) as i32),
            SNDCTL_DSP_GETOPTR => {
                let played = self.ring.played;
                let blocks = ((played - self.optr_played) / FRAGMENT as u64) as i32;
                self.optr_played = played - (played - self.optr_played) % FRAGMENT as u64;
                let fields = [
                    self.client_bytes(played as usize) as i32,
                    blocks,
                    self.client_bytes((played % RING_LEN as u64) as usize) as i32,
                ];
                for (i, v) in fields.iter().enumerate() {
                    data[4 * i..4 * i + 4].copy_from_slice(&v.to_ne_bytes());
                }
                Ok((0, 12))
            }
            SNDCTL_DSP_GETCAPS => put(data, CAPS),
            SNDCTL_DSP_GETTRIGGER => put(data, PCM_ENABLE_OUTPUT),
            // No capture.
            SNDCTL_DSP_SETDUPLEX => Err(EIO),
            _ => Err(EINVAL),
        }
    }

    /// Take what of `data` fits, in whole frames of the client's.
    fn play(&mut self, data: &[u8]) -> Result<usize, i32> {
        let frame = self.client_frame();
        let frames = (data.len() / frame).min(self.ring.free() / FRAME);
        if frames == 0 {
            return if data.len() < frame {
                Ok(0)
            } else {
                Err(EAGAIN)
            };
        }
        let mut pcm = Vec::with_capacity(frames * FRAME);
        for f in data[..frames * frame].chunks_exact(frame) {
            self.convert(f, &mut pcm);
        }
        let mut ring = std::mem::take(&mut self.ring);
        let n = ring.write(self.out.ring(), &pcm);
        self.ring = ring;
        debug_assert_eq!(n, pcm.len());
        if !self.out.running() {
            self.out.run(self.rate).map_err(|_| EIO)?;
        }
        self.idle_since = None;
        Ok(frames * frame)
    }
}

impl<O: Output> Device for Dsp<O> {
    fn open(&mut self, _flags: u64) -> Result<(u32, u64), i32> {
        if self.open.is_some() {
            return Err(EBUSY);
        }
        let fh = self.next_fh;
        self.next_fh = self.next_fh.wrapping_add(1).max(1);
        self.open = Some(fh);
        // Every open starts from OSS's defaults; what the last one queued
        // plays on, unless this one asks for another rate.
        self.format = AFMT_U8;
        self.channels = 1;
        self.optr_played = self.ring.played;
        self.said = self.readiness();
        Ok((fh, self.said))
    }

    fn release(&mut self, fh: u32) {
        if self.open == Some(fh) {
            self.open = None;
        }
    }

    fn read(&mut self, _: &Node, _: u32, _: u64, _: &mut [u8]) -> Result<usize, i32> {
        // No capture.
        Err(EINVAL)
    }

    fn write(&mut self, node: &Node, fh: u32, _: u64, data: &[u8]) -> Result<usize, i32> {
        if self.open != Some(fh) {
            return Err(EINVAL);
        }
        let r = self.play(data);
        self.tell(Some(node));
        r
    }

    fn ioctl(
        &mut self,
        node: &Node,
        fh: u32,
        cmd: u32,
        _arg: u64,
        data: &mut [u8],
    ) -> Result<(i64, usize), i32> {
        if self.open != Some(fh) {
            return Err(EINVAL);
        }
        let r = self.control(cmd, data);
        self.tell(Some(node));
        r
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
    fn position(&self) -> usize {
        self.lpib()
    }
}

#[cfg(test)]
mod tests;
