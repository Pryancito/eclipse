//! An Intel HD Audio controller as a process drives it (`udriver=hda@N`):
//! what `drivers/src/audio/hda.rs` does in the kernel, cut to one output
//! stream played by one client at a time.
//!
//! The controller is reset and its codecs found; verbs go to them through
//! the CORB and come back through the RIRB, both polled. The codec's audio
//! function is walked for output paths (a converter a pin can reach), and
//! the best one is powered and routed: a pin that says something is plugged
//! in first, a digital (HDMI/DP) one before an analog one. Sound goes out
//! through one cyclic ring that a buffer descriptor list describes once;
//! [`Ring`] keeps what of it the client has written and what the
//! controller has played, as the stream's position (`LPIB`) says, and
//! zeroes what was played, so a client that falls behind is heard as
//! silence and not as the last second again.
//!
//! What touches the controller is behind [`Regs`] and [`Verbs`], so the
//! codec walk and the ring are tried here against a codec that is not
//! there ([`tests`]).

use std::time::Duration;

use crate::claim::{self, Claim, Dma};
use crate::{now_ns, PAGE};

// Controller registers (HDA 1.0a §3.3).
const GCAP: usize = 0x00;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0e;
const INTCTL: usize = 0x20;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4a;
const CORBCTL: usize = 0x4c;
const CORBSIZE: usize = 0x4e;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5a;
const RIRBCTL: usize = 0x5c;
const RIRBSTS: usize = 0x5d;
const RIRBSIZE: usize = 0x5e;
const SD_BASE: usize = 0x80;
const SD_LEN: usize = 0x20;
// A stream descriptor's registers, from its base.
const SD_CTL: usize = 0x00;
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0c;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1c;

const GCTL_CRST: u32 = 1;
const RIRB_DMA_RUN: u8 = 1 << 1;
const RIRB_IRQ_EN: u8 = 1 << 0;
const RIRBSTS_ACK: u8 = 0x05;
const CORB_DMA_RUN: u8 = 1 << 1;
const SD_CTL_SRST: u32 = 1;
const SD_CTL_RUN: u32 = 1 << 1;

// Codec verbs and parameters (HDA 1.0a §7.3).
const VERB_GET_PARAMETER: u32 = 0xf00;
const VERB_GET_CONN_LIST: u32 = 0xf02;
const VERB_SET_CONN_SELECT: u32 = 0x701;
const VERB_SET_POWER_STATE: u32 = 0x705;
const VERB_SET_STREAM_ID: u32 = 0x706;
const VERB_SET_PIN_CTL: u32 = 0x707;
const VERB_SET_PIN_SENSE: u32 = 0x709;
const VERB_GET_PIN_SENSE: u32 = 0xf09;
const VERB_SET_EAPD: u32 = 0x70c;
const VERB_SET_DIGI_CVT1: u32 = 0x70d;
const VERB_GET_CONFIG_DEFAULT: u32 = 0xf1c;
const VERB_SET_CVT_CHAN_COUNT: u32 = 0x72d;
/// 4-bit verbs, with a 16-bit payload.
const VERB16_SET_FORMAT: u32 = 0x2;
const VERB16_SET_AMP: u32 = 0x3;
const PAR_VENDOR_ID: u32 = 0x00;
const PAR_NODE_COUNT: u32 = 0x04;
const PAR_FUNCTION_TYPE: u32 = 0x05;
const PAR_AUDIO_WIDGET_CAP: u32 = 0x09;
const PAR_PIN_CAP: u32 = 0x0c;
const PAR_CONN_LIST_LEN: u32 = 0x0e;
const PAR_OUT_AMP_CAP: u32 = 0x12;
const WIDGET_AUDIO_OUT: u32 = 0x0;
const WIDGET_PIN: u32 = 0x4;
const PIN_CTL_OUT_EN: u32 = 0x40;
const FUNCTION_AUDIO: u32 = 0x01;

/// How long a codec gets to answer a verb.
const VERB_NS: u64 = 200_000_000;

/// The ring: 64 KiB in segments of 2 KiB, a third of a second of 48 kHz
/// stereo.
pub const RING_LEN: usize = 16 * PAGE;
pub const SEGMENT: usize = 2048;
/// What of the ring is never written: the controller fetches ahead of the
/// position it reports, and what it has fetched is not to be changed.
pub const GUARD: usize = 2 * SEGMENT;
/// How far ahead of the play position a client that fell behind starts
/// again: what is that close may be fetched already.
pub const LEAD: usize = SEGMENT;
/// A frame of the ring: two channels of 16 bits.
pub const FRAME: usize = 4;

/// The rates a stream format can say (HDA 1.0a §3.7.1).
pub const RATES: [u32; 11] = [
    8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
];

/// The rate of [`RATES`] nearest `rate`.
pub fn nearest_rate(rate: u32) -> u32 {
    *RATES
        .iter()
        .min_by_key(|r| (**r as i64 - rate as i64).unsigned_abs())
        .unwrap()
}

/// The stream format of 16-bit samples, `channels` of them, at `rate` (one
/// of [`RATES`]).
pub fn stream_format(rate: u32, channels: u8) -> u16 {
    let (base44, mult, div) = match rate {
        8000 => (false, 1, 6),
        11025 => (true, 1, 4),
        16000 => (false, 1, 3),
        22050 => (true, 1, 2),
        32000 => (false, 2, 3),
        44100 => (true, 1, 1),
        88200 => (true, 2, 1),
        96000 => (false, 2, 1),
        176400 => (true, 4, 1),
        192000 => (false, 4, 1),
        _ => (false, 1, 1),
    };
    (base44 as u16) << 14
        | (mult - 1) << 11
        | (div - 1) << 8
        | 0b001 << 4
        | (channels.max(1) as u16 - 1) & 0xf
}

/// The controller's registers.
pub trait Regs {
    fn r8(&self, off: usize) -> u8;
    fn r16(&self, off: usize) -> u16;
    fn r32(&self, off: usize) -> u32;
    fn w8(&self, off: usize, v: u8);
    fn w16(&self, off: usize, v: u16);
    fn w32(&self, off: usize, v: u32);
}

/// The registers as this process maps them.
pub struct Mapped(pub *mut u8);

// SAFETY: the mapping is the process's for its life; the registers are
// reached through volatile accesses only.
unsafe impl Send for Mapped {}

impl Regs for Mapped {
    // SAFETY (all six): `off` is a register inside the mapped window.
    fn r8(&self, off: usize) -> u8 {
        unsafe { self.0.add(off).read_volatile() }
    }
    fn r16(&self, off: usize) -> u16 {
        unsafe { (self.0.add(off) as *const u16).read_volatile() }
    }
    fn r32(&self, off: usize) -> u32 {
        unsafe { (self.0.add(off) as *const u32).read_volatile() }
    }
    fn w8(&self, off: usize, v: u8) {
        unsafe { self.0.add(off).write_volatile(v) }
    }
    fn w16(&self, off: usize, v: u16) {
        unsafe { (self.0.add(off) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, off: usize, v: u32) {
        unsafe { (self.0.add(off) as *mut u32).write_volatile(v) }
    }
}

/// What carries a verb to a codec and its answer back.
pub trait Verbs {
    /// The answer to one 32-bit verb word, `None` when none came in time.
    fn send(&mut self, word: u32) -> Option<u32>;
}

/// A codec, at its address on the link.
pub struct Codec<V> {
    pub verbs: V,
    pub cad: u32,
}

impl<V: Verbs> Codec<V> {
    /// A 12-bit verb with an 8-bit payload.
    pub fn cmd(&mut self, nid: u32, verb: u32, payload: u32) -> Option<u32> {
        self.verbs
            .send(self.cad << 28 | nid << 20 | verb << 8 | (payload & 0xff))
    }

    /// A 4-bit verb with a 16-bit payload.
    pub fn cmd16(&mut self, nid: u32, verb: u32, payload: u32) -> Option<u32> {
        self.verbs
            .send(self.cad << 28 | nid << 20 | verb << 16 | (payload & 0xffff))
    }

    pub fn param(&mut self, nid: u32, par: u32) -> Option<u32> {
        self.cmd(nid, VERB_GET_PARAMETER, par)
    }

    /// The first node and how many there are, under `nid`.
    fn sub_nodes(&mut self, nid: u32) -> Option<(u32, u32)> {
        let r = self.param(nid, PAR_NODE_COUNT)?;
        Some(((r >> 16) & 0xff, r & 0xff))
    }

    /// The codec's audio function group.
    pub fn audio_function(&mut self) -> Option<u32> {
        let (start, count) = self.sub_nodes(0)?;
        (start..start + count)
            .find(|&nid| self.param(nid, PAR_FUNCTION_TYPE).unwrap_or(0) & 0xff == FUNCTION_AUDIO)
    }

    /// Where `conv` is in `pin`'s connection list.
    fn conn_index(&mut self, pin: u32, conv: u32) -> Option<u32> {
        let len = self.param(pin, PAR_CONN_LIST_LEN)?;
        let long = len & 0x80 != 0;
        let len = len & 0x7f;
        let per = if long { 2 } else { 4 };
        let mut idx = 0;
        let mut i = 0;
        while i < len {
            let r = self.cmd(pin, VERB_GET_CONN_LIST, i)?;
            for k in 0..per.min(len - i) {
                let entry = if long {
                    (r >> (16 * k)) & 0x7fff
                } else {
                    (r >> (8 * k)) & 0x7f
                };
                if entry == conv {
                    return Some(idx);
                }
                idx += 1;
            }
            i += per;
        }
        None
    }

    /// Every output path of the function `afg`: each output pin with a
    /// connector, and the first converter it reaches.
    pub fn paths(&mut self, afg: u32) -> Vec<Path> {
        let Some((start, count)) = self.sub_nodes(afg) else {
            return Vec::new();
        };
        let mut convs = Vec::new();
        let mut pins = Vec::new();
        for nid in start..start + count {
            let caps = self.param(nid, PAR_AUDIO_WIDGET_CAP).unwrap_or(0);
            let digital = caps & (1 << 9) != 0;
            match (caps >> 20) & 0xf {
                WIDGET_AUDIO_OUT => convs.push((nid, digital)),
                WIDGET_PIN => {
                    let pincap = self.param(nid, PAR_PIN_CAP).unwrap_or(0);
                    let config = self.cmd(nid, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0);
                    // Output capable, and not "no physical connection".
                    if pincap & (1 << 4) != 0 && (config >> 30) & 3 != 1 {
                        pins.push((nid, pincap));
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for (pin, pincap) in pins {
            for &(conv, digital) in &convs {
                if let Some(conn) = self.conn_index(pin, conv) {
                    out.push(Path {
                        conv,
                        pin,
                        conn,
                        digital,
                        hdmi: pincap & (1 << 7) != 0 || pincap & (1 << 24) != 0,
                    });
                    break;
                }
            }
        }
        out
    }

    /// Whether something is plugged into `path`'s pin: a digital pin is
    /// asked to look first, as Linux's `snd_hda_pin_sense` does.
    pub fn present(&mut self, path: &Path) -> bool {
        if path.hdmi {
            let _ = self.cmd(path.pin, VERB_SET_PIN_SENSE, 0);
        }
        self.cmd(path.pin, VERB_GET_PIN_SENSE, 0)
            .is_some_and(|s| s & (1 << 31) != 0)
    }

    /// Power `path` up and route it: the function, its converter and its
    /// pin in D0, the pin on its converter and driving, its amplifier on and
    /// every amp at 0 dB.
    pub fn route(&mut self, afg: u32, path: &Path) -> Option<()> {
        self.cmd(afg, VERB_SET_POWER_STATE, 0)?;
        std::thread::sleep(Duration::from_millis(1));
        self.cmd(path.conv, VERB_SET_POWER_STATE, 0)?;
        self.cmd(path.pin, VERB_SET_POWER_STATE, 0)?;
        self.cmd(path.pin, VERB_SET_CONN_SELECT, path.conn)?;
        self.cmd(path.pin, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN)?;
        if self.param(path.pin, PAR_PIN_CAP)? & (1 << 16) != 0 {
            let _ = self.cmd(path.pin, VERB_SET_EAPD, 0x2);
        }
        for nid in [path.conv, path.pin] {
            let amp = self.param(nid, PAR_OUT_AMP_CAP)?;
            let has = amp != 0 || self.param(nid, PAR_AUDIO_WIDGET_CAP)? & (1 << 2) != 0;
            if has {
                // Output amp, both channels, at its 0 dB step.
                let _ = self.cmd16(
                    nid,
                    VERB16_SET_AMP,
                    1 << 15 | 1 << 13 | 1 << 12 | (amp & 0x7f),
                );
            }
        }
        if path.digital {
            let _ = self.cmd(path.conv, VERB_SET_DIGI_CVT1, 1);
            let _ = self.cmd(path.conv, VERB_SET_CVT_CHAN_COUNT, 1);
        }
        Some(())
    }

    /// Put `path`'s converter on stream `tag`, in `format`.
    pub fn start(&mut self, path: &Path, tag: u32, format: u16) -> Option<()> {
        self.cmd16(path.conv, VERB16_SET_FORMAT, format as u32)?;
        self.cmd(path.conv, VERB_SET_STREAM_ID, tag << 4)?;
        Some(())
    }
}

/// An output path: a converter and the pin it plays through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path {
    pub conv: u32,
    pub pin: u32,
    /// Where the converter is in the pin's connection list.
    pub conn: u32,
    pub digital: bool,
    pub hdmi: bool,
}

/// Which path to play on: the best score, the first of them on a tie. A
/// pin with something plugged in scores 2, and 4 more when it is HDMI/DP
/// (a display that hears it); one with nothing plugged in still plays, for
/// a codec that cannot tell (QEMU's).
pub fn choose(paths: &[(Path, bool)]) -> Option<Path> {
    let score = |(p, present): &(Path, bool)| {
        let mut s = 0;
        if *present {
            s += 2;
            if p.hdmi && p.digital {
                s += 4;
            }
        }
        s
    };
    let best = paths.iter().map(score).max()?;
    paths.iter().find(|p| score(p) == best).map(|(p, _)| *p)
}

/// What of the ring holds what: bytes the client wrote and bytes the
/// controller played, counted from the start of the stream. Everything
/// outside `[played, written)` is zeros, so the controller, running on
/// past what was written, plays silence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ring {
    pub written: u64,
    pub played: u64,
}

impl Ring {
    /// What is queued and not yet played.
    pub fn queued(&self) -> usize {
        self.written.saturating_sub(self.played) as usize
    }

    /// What can be written now: with nothing queued, the next write
    /// starts [`LEAD`] ahead, and that is not room.
    pub fn free(&self) -> usize {
        let queued = match self.queued() {
            0 => LEAD,
            q => q,
        };
        (RING_LEN - GUARD).saturating_sub(queued)
    }

    /// Copy `data` (whole frames, at most [`Ring::free`]) into `ring`
    /// after what was written. A client behind the controller starts
    /// [`LEAD`] ahead of it.
    pub fn write(&mut self, ring: &mut [u8], data: &[u8]) -> usize {
        if self.written <= self.played {
            self.written = self.played + LEAD as u64;
        }
        let n = data.len().min(self.free()) / FRAME * FRAME;
        let mut done = 0;
        while done < n {
            let at = ((self.written + done as u64) % RING_LEN as u64) as usize;
            let k = (n - done).min(RING_LEN - at);
            ring[at..at + k].copy_from_slice(&data[done..done + k]);
            done += k;
        }
        self.written += n as u64;
        n
    }

    /// The controller played on to `lpib` (its position in the ring): what
    /// it played is zeroed. How much that was.
    pub fn played_to(&mut self, ring: &mut [u8], lpib: usize) -> usize {
        let at = (self.played % RING_LEN as u64) as usize;
        let n = (lpib % RING_LEN + RING_LEN - at) % RING_LEN;
        let mut done = 0;
        while done < n {
            let from = (at + done) % RING_LEN;
            let k = (n - done).min(RING_LEN - from);
            ring[from..from + k].fill(0);
            done += k;
        }
        self.played += n as u64;
        n
    }
}

/// The CORB and the RIRB of a controller, polled.
pub struct Rings<R> {
    pub regs: R,
    corb: *mut u32,
    rirb: *mut u32,
    corb_entries: usize,
    rirb_entries: usize,
    rirb_rp: usize,
}

// SAFETY: the rings are this process's DMA memory, reached by one owner.
unsafe impl<R: Send> Send for Rings<R> {}

fn ring_entries(cap: u8) -> (usize, u8) {
    if cap & 4 != 0 || cap == 0 {
        (256, 2)
    } else if cap & 2 != 0 {
        (16, 1)
    } else {
        (2, 0)
    }
}

impl<R: Regs> Rings<R> {
    /// Set the rings up in `dma`: the CORB at its start, the RIRB a page in.
    fn new(regs: R, dma: &Dma) -> Rings<R> {
        regs.w8(CORBCTL, 0);
        regs.w8(RIRBCTL, 0);
        let (corb_entries, code) = ring_entries(regs.r8(CORBSIZE) >> 4);
        regs.w8(CORBSIZE, code);
        let (rirb_entries, code) = ring_entries(regs.r8(RIRBSIZE) >> 4);
        regs.w8(RIRBSIZE, code);
        let corb = dma.daddr;
        let rirb = dma.daddr + PAGE as u64;
        regs.w32(CORBLBASE, corb as u32);
        regs.w32(CORBUBASE, (corb >> 32) as u32);
        regs.w32(RIRBLBASE, rirb as u32);
        regs.w32(RIRBUBASE, (rirb >> 32) as u32);
        regs.w16(CORBRP, 1 << 15);
        std::thread::sleep(Duration::from_micros(100));
        regs.w16(CORBRP, 0);
        regs.w16(CORBWP, 0);
        regs.w16(RIRBWP, 1 << 15);
        // One answer per interrupt status, acked after every verb: QEMU
        // holds the CORB until it is.
        regs.w16(RINTCNT, 1);
        regs.w8(RIRBSTS, RIRBSTS_ACK);
        regs.w8(RIRBCTL, RIRB_DMA_RUN | RIRB_IRQ_EN);
        regs.w8(CORBCTL, CORB_DMA_RUN);
        Rings {
            regs,
            corb: dma.ptr as *mut u32,
            // SAFETY: a page in, inside `dma`.
            rirb: unsafe { dma.ptr.add(PAGE) } as *mut u32,
            corb_entries,
            rirb_entries,
            rirb_rp: 0,
        }
    }

    fn rirb_wp(&self) -> usize {
        (self.regs.r16(RIRBWP) & 0xff) as usize % self.rirb_entries
    }
}

impl<R: Regs> Verbs for Rings<R> {
    fn send(&mut self, word: u32) -> Option<u32> {
        self.regs.w8(RIRBSTS, RIRBSTS_ACK);
        // What is in the RIRB already is not this verb's answer: a late one
        // of a verb that gave up, or a jack's own.
        self.rirb_rp = self.rirb_wp();
        let wp = (self.regs.r16(CORBWP) as usize + 1) % self.corb_entries;
        // SAFETY: entry `wp` of the CORB, inside its page.
        unsafe { self.corb.add(wp).write_volatile(word) };
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.regs.w16(CORBWP, wp as u16);
        let start = now_ns();
        loop {
            while self.rirb_rp != self.rirb_wp() {
                self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
                // SAFETY: entry `rirb_rp` of the RIRB, two words each.
                let (answer, ext) = unsafe {
                    (
                        self.rirb.add(2 * self.rirb_rp).read_volatile(),
                        self.rirb.add(2 * self.rirb_rp + 1).read_volatile(),
                    )
                };
                self.regs.w8(RIRBSTS, RIRBSTS_ACK);
                // An unsolicited answer (a jack event) is not this one.
                if ext & (1 << 4) == 0 {
                    return Some(answer);
                }
            }
            if now_ns() - start > VERB_NS {
                self.regs.w8(RIRBSTS, RIRBSTS_ACK);
                return None;
            }
            std::hint::spin_loop();
        }
    }
}

/// The controller, its codec, the path it plays on and its ring.
pub struct Controller {
    pub codec: Codec<Rings<Mapped>>,
    pub afg: u32,
    pub path: Path,
    /// The first output stream's descriptor.
    sd: usize,
    ring: *mut u8,
    bdl: u64,
    pub running: bool,
    pub rate: u32,
    _dma: Dma,
}

// SAFETY: as `Rings`.
unsafe impl Send for Controller {}

/// The stream tag of the one stream.
const TAG: u32 = 1;

impl Controller {
    /// Reset controller `index` this process claimed, find its codec and
    /// route its best output path.
    pub fn start(claim: &Claim) -> Result<Controller, String> {
        let regs = Mapped(claim.map_regs()?);
        let gcap = regs.r16(GCAP);
        let (iss, oss) = (((gcap >> 8) & 0xf) as usize, ((gcap >> 12) & 0xf) as usize);
        if oss == 0 {
            return Err(format!("GCAP {gcap:#06x}: no output stream"));
        }
        let crst = |on: bool| {
            let v = regs.r32(GCTL);
            regs.w32(GCTL, if on { v | GCTL_CRST } else { v & !GCTL_CRST });
            let start = now_ns();
            while (regs.r32(GCTL) & GCTL_CRST != 0) != on {
                if now_ns() - start > 1_000_000_000 {
                    return false;
                }
                std::thread::yield_now();
            }
            true
        };
        if !crst(false)
            || !{
                std::thread::sleep(Duration::from_micros(200));
                crst(true)
            }
        {
            return Err("the controller did not come out of reset".into());
        }
        // Codecs get 521 us to say they are there.
        std::thread::sleep(Duration::from_millis(2));
        let codecs = regs.r16(STATESTS);
        if codecs == 0 {
            return Err("no codec answered after the reset".into());
        }
        regs.w32(INTCTL, 0);
        // The CORB and RIRB in two pages, the buffer descriptors in one,
        // the ring after them.
        let dma = Dma::new(claim.bti, 3 * PAGE + RING_LEN, None)?;
        let bdl = dma.daddr + 2 * PAGE as u64;
        let ring = dma.daddr + 3 * PAGE as u64;
        for i in 0..RING_LEN / SEGMENT {
            // SAFETY: descriptor `i` of 16 bytes, inside the third page.
            unsafe {
                let e = dma.ptr.add(2 * PAGE + 16 * i);
                (e as *mut u64).write_volatile(ring + (i * SEGMENT) as u64);
                (e.add(8) as *mut u32).write_volatile(SEGMENT as u32);
                (e.add(12) as *mut u32).write_volatile(0);
            }
        }
        let rings = Rings::new(regs, &dma);
        let mut codec = Codec {
            verbs: rings,
            cad: codecs.trailing_zeros(),
        };
        codec
            .param(0, PAR_VENDOR_ID)
            .ok_or("the codec does not answer")?;
        let afg = codec
            .audio_function()
            .ok_or("the codec has no audio function")?;
        let paths: Vec<_> = codec
            .paths(afg)
            .into_iter()
            .map(|p| (p, codec.present(&p)))
            .collect();
        let path = choose(&paths).ok_or("the codec has no output path")?;
        codec.route(afg, &path).ok_or("routing the output path")?;
        Ok(Controller {
            codec,
            afg,
            path,
            sd: SD_BASE + iss * SD_LEN,
            // SAFETY: the ring, three pages in.
            ring: unsafe { dma.ptr.add(3 * PAGE) },
            bdl,
            running: false,
            rate: 48000,
            _dma: dma,
        })
    }

    fn regs(&self) -> &Mapped {
        &self.codec.verbs.regs
    }

    /// The ring, as this process sees it.
    pub fn ring(&mut self) -> &mut [u8] {
        // SAFETY: `RING_LEN` bytes of the DMA memory, this one's.
        unsafe { std::slice::from_raw_parts_mut(self.ring, RING_LEN) }
    }

    /// Where in the ring the controller is.
    pub fn lpib(&self) -> usize {
        self.regs().r32(self.sd + SD_LPIB) as usize % RING_LEN
    }

    fn reset_stream(&self) {
        let r = self.regs();
        for on in [true, false] {
            r.w32(self.sd + SD_CTL, if on { SD_CTL_SRST } else { 0 });
            let start = now_ns();
            while (r.r32(self.sd + SD_CTL) & SD_CTL_SRST != 0) != on
                && now_ns() - start < 100_000_000
            {
                std::thread::yield_now();
            }
        }
    }

    /// Start the stream from the top of the ring, at `rate`.
    pub fn run(&mut self) -> Result<(), String> {
        self.reset_stream();
        let format = stream_format(self.rate, 2);
        let r = self.regs();
        r.w32(self.sd + SD_BDPL, self.bdl as u32);
        r.w32(self.sd + SD_BDPU, (self.bdl >> 32) as u32);
        r.w32(self.sd + SD_CBL, RING_LEN as u32);
        r.w16(self.sd + SD_LVI, (RING_LEN / SEGMENT - 1) as u16);
        r.w16(self.sd + SD_FMT, format);
        r.w8(self.sd + SD_STS, 0x1c);
        let path = self.path;
        self.codec
            .start(&path, TAG, format)
            .ok_or("the converter did not take the format")?;
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.regs().w32(self.sd + SD_CTL, TAG << 20 | SD_CTL_RUN);
        self.running = true;
        Ok(())
    }

    /// Stop the stream.
    pub fn stop(&mut self) {
        let r = self.regs();
        r.w32(self.sd + SD_CTL, r.r32(self.sd + SD_CTL) & !SD_CTL_RUN);
        self.running = false;
    }
}

/// Claim controller `index`.
pub fn claim(index: usize) -> Result<Claim, crate::zx::Status> {
    claim::claim(claim::KIND_HDA, index, 300)
}

#[cfg(test)]
mod tests;
