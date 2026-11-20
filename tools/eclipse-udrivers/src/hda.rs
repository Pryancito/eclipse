//! An Intel HD Audio controller as a process drives it (`udriver=hda@N`):
//! what `drivers/src/audio/hda.rs` does in the kernel, cut to one output
//! stream played by one client at a time.
//!
//! The controller is reset and its codecs found; verbs go to them through
//! the CORB and come back through the RIRB, both polled. Every codec's
//! audio function is walked for output paths (a converter a pin reaches,
//! straight or through mixers and selectors, as a desktop Realtek's line
//! out does), and the best one is powered and routed: a pin that says
//! something is plugged in first, a digital (HDMI/DP) one before an analog
//! one, an analog output jack before an S/PDIF or a silent HDMI pin. What
//! was found and why one path was taken is said on the log, so a card that
//! plays nothing on a machine nobody can look at says why. Sound goes out
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
const WAKEEN: usize = 0x0c;
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
/// GCAP: the controller takes 64-bit DMA addresses.
const GCAP_64OK: u16 = 1;
const CORBRP_RST: u16 = 1 << 15;
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
const PAR_IN_AMP_CAP: u32 = 0x0d;
const PAR_CONN_LIST_LEN: u32 = 0x0e;
const PAR_OUT_AMP_CAP: u32 = 0x12;
const WIDGET_AUDIO_OUT: u32 = 0x0;
const WIDGET_MIXER: u32 = 0x2;
const WIDGET_SELECTOR: u32 = 0x3;
const WIDGET_PIN: u32 = 0x4;
// Audio widget capabilities.
const WCAP_IN_AMP: u32 = 1 << 1;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_AMP_OVERRIDE: u32 = 1 << 3;
const WCAP_DIGITAL: u32 = 1 << 9;
// Pin capabilities.
const PINCAP_TRIGGER: u32 = 1 << 1;
const PINCAP_PRESENCE: u32 = 1 << 2;
const PINCAP_HP_DRIVE: u32 = 1 << 3;
const PINCAP_OUT: u32 = 1 << 4;
const PINCAP_HDMI: u32 = 1 << 7;
const PINCAP_EAPD: u32 = 1 << 16;
const PINCAP_DP: u32 = 1 << 24;
const PIN_CTL_OUT_EN: u32 = 0x40;
const PIN_CTL_HP_EN: u32 = 0x80;
const FUNCTION_AUDIO: u32 = 0x01;
// A pin's default device (configuration default bits 23:20).
pub const DEVICE_LINE_OUT: u32 = 0x0;
pub const DEVICE_SPEAKER: u32 = 0x1;
pub const DEVICE_HP_OUT: u32 = 0x2;
pub const DEVICE_SPDIF_OUT: u32 = 0x4;
pub const DEVICE_DIGITAL_OUT: u32 = 0x5;
/// Its connectivity (bits 31:30): nothing there, and a fixed device (a
/// laptop's speaker).
const CONNECT_NONE: u32 = 1;
const CONNECT_FIXED: u32 = 2;
/// How many mixers and selectors a path goes through, at most, between
/// its pin and its converter: a Realtek's is one mixer.
pub const MAX_VIA: usize = 4;

/// How long a codec gets to answer a verb.
const VERB_NS: u64 = 200_000_000;
/// After this many verbs in a row with no answer a codec is taken to be
/// gone, and verbs to it fail at once: a walk of a dead codec would
/// otherwise take [`VERB_NS`] a verb, hundreds of times.
const DEAD_AFTER: u32 = 3;

/// The ring: 128 KiB in segments of 2 KiB, two thirds of a second of
/// 48 kHz stereo, as deep as the kernel's: an ALSA client asks for half a
/// second (mpg123).
pub const RING_LEN: usize = 32 * PAGE;
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

    /// `nid`'s connection list, ranges spelled out (HDA 1.0a §7.3.3.3): an
    /// entry with the range bit set means every node from the one before it
    /// to it. Indices into it are what `SET_CONN_SELECT` and an input amp
    /// take.
    pub fn connections(&mut self, nid: u32) -> Vec<u32> {
        let Some(len) = self.param(nid, PAR_CONN_LIST_LEN) else {
            return Vec::new();
        };
        let long = len & 0x80 != 0;
        let len = len & 0x7f;
        let (per, bits) = if long { (2, 16) } else { (4, 8) };
        let range = 1u32 << (bits - 1);
        let mask = range - 1;
        let mut out: Vec<u32> = Vec::new();
        let mut i = 0;
        while i < len {
            let Some(r) = self.cmd(nid, VERB_GET_CONN_LIST, i) else {
                break;
            };
            for k in 0..per.min(len - i) {
                let entry = (r >> (bits * k)) & (range | mask);
                let node = entry & mask;
                match out.last() {
                    // A range, from the entry before it: at most a list's
                    // worth, whatever a broken codec says.
                    Some(&from) if entry & range != 0 && node > from => {
                        out.extend((from + 1..=node).take(0x7f));
                    }
                    _ => out.push(node),
                }
            }
            i += per;
        }
        out
    }

    /// From `nid`, a converter its connection list reaches: straight, or
    /// through mixers and selectors, `depth` of them at most. Where the
    /// first step is in `nid`'s list, the converter, and the widgets on the
    /// way, each with where its next step is in its own list. A converter
    /// in `nid`'s own list is taken before any way round.
    fn reach(
        &mut self,
        nid: u32,
        widgets: &[(u32, u32)],
        depth: usize,
        seen: &mut Vec<u32>,
    ) -> Option<(u32, u32, Vec<Hop>)> {
        let list = self.connections(nid);
        let kind = |n: u32| {
            widgets
                .iter()
                .find(|(w, _)| *w == n)
                .map(|(_, caps)| (caps >> 20) & 0xf)
        };
        if let Some(i) = list.iter().position(|&n| kind(n) == Some(WIDGET_AUDIO_OUT)) {
            return Some((i as u32, list[i], Vec::new()));
        }
        if depth == 0 {
            return None;
        }
        for (i, &w) in list.iter().enumerate() {
            let k = kind(w);
            if !matches!(k, Some(WIDGET_MIXER | WIDGET_SELECTOR)) || seen.contains(&w) {
                continue;
            }
            seen.push(w);
            if let Some((conn, conv, mut rest)) = self.reach(w, widgets, depth - 1, seen) {
                rest.insert(
                    0,
                    Hop {
                        nid: w,
                        conn,
                        mixer: k == Some(WIDGET_MIXER),
                    },
                );
                return Some((i as u32, conv, rest));
            }
        }
        None
    }

    /// Every output path of the function `afg`: each output pin with a
    /// connector, and the converter it reaches. Pins whose default device
    /// is an output (line out, speaker, headphone, S/PDIF, HDMI/DP) only,
    /// unless no pin says so: then every output-capable one, for a codec
    /// whose firmware left the configuration blank.
    pub fn paths(&mut self, afg: u32) -> Vec<Path> {
        let Some((start, count)) = self.sub_nodes(afg) else {
            return Vec::new();
        };
        let mut widgets = Vec::new();
        let mut pins = Vec::new();
        for nid in start..start + count {
            let caps = self.param(nid, PAR_AUDIO_WIDGET_CAP).unwrap_or(0);
            widgets.push((nid, caps));
            if (caps >> 20) & 0xf == WIDGET_PIN {
                let pincap = self.param(nid, PAR_PIN_CAP).unwrap_or(0);
                let config = self.cmd(nid, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(0);
                // Output capable, and not "no physical connection".
                if pincap & PINCAP_OUT != 0 && (config >> 30) & 3 != CONNECT_NONE {
                    pins.push((nid, pincap, config));
                }
            }
        }
        let outputs = pins
            .iter()
            .any(|(_, _, config)| output_device((config >> 20) & 0xf));
        let mut out = Vec::new();
        for (pin, pincap, config) in pins {
            let device = (config >> 20) & 0xf;
            if outputs && !output_device(device) {
                continue;
            }
            let mut seen = vec![pin];
            let Some((conn, conv, hops)) = self.reach(pin, &widgets, MAX_VIA, &mut seen) else {
                continue;
            };
            let caps = widgets
                .iter()
                .find(|(w, _)| *w == conv)
                .map_or(0, |(_, c)| *c);
            let mut via = [Hop::default(); MAX_VIA];
            via[..hops.len()].copy_from_slice(&hops);
            out.push(Path {
                cad: self.cad,
                afg,
                conv,
                pin,
                conn,
                digital: caps & WCAP_DIGITAL != 0,
                hdmi: pincap & (PINCAP_HDMI | PINCAP_DP) != 0,
                via,
                hops: hops.len(),
                device,
                fixed: (config >> 30) & 3 == CONNECT_FIXED,
                // A pin that cannot sense, or whose firmware says not to
                // trust it (misc bit 0, "jack detect override"), says
                // nothing about what is plugged in.
                senses: pincap & PINCAP_PRESENCE != 0 && config & (1 << 8) == 0,
                trigger: pincap & PINCAP_TRIGGER != 0,
                hp_drive: pincap & PINCAP_HP_DRIVE != 0,
                eapd: pincap & PINCAP_EAPD != 0,
            });
        }
        out
    }

    /// Whether something is plugged into `path`'s pin: a pin that needs it
    /// (and an HDMI/DP one, which often does without saying so) is asked to
    /// look first, as Linux's `snd_hda_pin_sense` does. A pin that cannot
    /// tell is taken to have nothing.
    pub fn present(&mut self, path: &Path) -> bool {
        if !path.senses {
            return false;
        }
        if path.trigger || path.hdmi {
            let _ = self.cmd(path.pin, VERB_SET_PIN_SENSE, 0);
            // The kernel's driver gives the result 2 ms to settle; once,
            // at start.
            std::thread::sleep(Duration::from_millis(2));
        }
        self.cmd(path.pin, VERB_GET_PIN_SENSE, 0)
            .is_some_and(|s| s & (1 << 31) != 0)
    }

    /// The amplifier capabilities of `nid`'s input (`input`) or output amp,
    /// `None` when it has none: its own when it overrides them, else its
    /// function's (HDA 1.0a §7.3.4.10), and its own again when the function
    /// has none to give.
    pub fn amp_caps(&mut self, afg: u32, nid: u32, input: bool) -> Option<u32> {
        let caps = self.param(nid, PAR_AUDIO_WIDGET_CAP)?;
        let (present, par) = if input {
            (WCAP_IN_AMP, PAR_IN_AMP_CAP)
        } else {
            (WCAP_OUT_AMP, PAR_OUT_AMP_CAP)
        };
        let own = self.param(nid, par).unwrap_or(0);
        if caps & present == 0 {
            // No amp, or one that only says so in its caps.
            return (own != 0).then_some(own);
        }
        if caps & WCAP_AMP_OVERRIDE != 0 {
            return Some(own);
        }
        match self.param(afg, par).unwrap_or(0) {
            0 => Some(own),
            shared => Some(shared),
        }
    }

    /// Unmute `nid`'s output amp, or its input amp `index`, both channels,
    /// at its 0 dB step.
    fn open_amp(&mut self, afg: u32, nid: u32, input: Option<u32>) {
        if let Some(amp) = self.amp_caps(afg, nid, input.is_some()) {
            let side = match input {
                Some(index) => 1 << 14 | (index & 0xf) << 8,
                None => 1 << 15,
            };
            let _ = self.cmd16(nid, VERB16_SET_AMP, side | 1 << 13 | 1 << 12 | (amp & 0x7f));
        }
    }

    /// Power `path` up and route it: the function, its converter, the
    /// widgets between and its pin in D0; the pin and every selector on the
    /// way set to the next step; the pin driving (a headphone jack with its
    /// amplifier); every amp on the way open at 0 dB, a mixer's on the
    /// input the path comes in by.
    pub fn route(&mut self, afg: u32, path: &Path) -> Option<()> {
        self.cmd(afg, VERB_SET_POWER_STATE, 0)?;
        std::thread::sleep(Duration::from_millis(1));
        self.cmd(path.conv, VERB_SET_POWER_STATE, 0)?;
        for hop in path.via() {
            let _ = self.cmd(hop.nid, VERB_SET_POWER_STATE, 0);
        }
        self.cmd(path.pin, VERB_SET_POWER_STATE, 0)?;
        self.cmd(path.pin, VERB_SET_CONN_SELECT, path.conn)?;
        let ctl = if path.device == DEVICE_HP_OUT && path.hp_drive {
            PIN_CTL_OUT_EN | PIN_CTL_HP_EN
        } else {
            PIN_CTL_OUT_EN
        };
        self.cmd(path.pin, VERB_SET_PIN_CTL, ctl)?;
        if path.eapd {
            let _ = self.cmd(path.pin, VERB_SET_EAPD, 0x2);
        }
        for hop in path.via() {
            if !hop.mixer {
                let _ = self.cmd(hop.nid, VERB_SET_CONN_SELECT, hop.conn);
            }
            // A mixer sums every input: the path's is opened. A selector's
            // input amp, if it has one, is on the input it chose.
            self.open_amp(afg, hop.nid, Some(hop.conn));
            self.open_amp(afg, hop.nid, None);
        }
        self.open_amp(afg, path.conv, None);
        self.open_amp(afg, path.pin, None);
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

/// Whether a pin's default device is one sound goes out of.
fn output_device(device: u32) -> bool {
    matches!(
        device,
        DEVICE_LINE_OUT | DEVICE_SPEAKER | DEVICE_HP_OUT | DEVICE_SPDIF_OUT | DEVICE_DIGITAL_OUT
    )
}

/// A widget between a pin and its converter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hop {
    pub nid: u32,
    /// Where the next step to the converter is in its connection list.
    pub conn: u32,
    /// A mixer, which sums its inputs; else a selector, which takes one.
    pub mixer: bool,
}

/// An output path: a converter and the pin it plays through, on codec
/// `cad`'s function `afg`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Path {
    pub cad: u32,
    pub afg: u32,
    pub conv: u32,
    pub pin: u32,
    /// Where the first step to the converter is in the pin's connection
    /// list.
    pub conn: u32,
    pub digital: bool,
    pub hdmi: bool,
    /// The mixers and selectors between pin and converter, pin side first:
    /// the first [`Path::hops`] of them.
    pub via: [Hop; MAX_VIA],
    pub hops: usize,
    /// The pin's default device: [`DEVICE_LINE_OUT`] and the rest.
    pub device: u32,
    /// A device fixed to the machine (a laptop's speaker), not a jack.
    pub fixed: bool,
    /// Whether the pin can tell something is plugged in.
    pub senses: bool,
    /// Whether it has to be asked to look first.
    pub trigger: bool,
    /// Whether it can drive headphones, and has an external amplifier.
    pub hp_drive: bool,
    pub eapd: bool,
}

impl Path {
    /// The widgets between pin and converter.
    pub fn via(&self) -> &[Hop] {
        &self.via[..self.hops.min(MAX_VIA)]
    }

    /// An analog output: a line out, a speaker or a headphone jack.
    pub fn analog_out(&self) -> bool {
        !self.digital
            && !self.hdmi
            && matches!(
                self.device,
                DEVICE_LINE_OUT | DEVICE_SPEAKER | DEVICE_HP_OUT
            )
    }

    /// What the log says of it.
    pub fn describe(&self) -> String {
        let device = match self.device {
            DEVICE_LINE_OUT => "line out",
            DEVICE_SPEAKER => "speaker",
            DEVICE_HP_OUT => "headphones",
            DEVICE_SPDIF_OUT => "S/PDIF",
            DEVICE_DIGITAL_OUT => "digital out",
            _ => "other",
        };
        let mut s = format!(
            "codec {} pin {:#x} ({device}{}{}) <- ",
            self.cad,
            self.pin,
            if self.hdmi { ", HDMI/DP" } else { "" },
            if self.fixed { ", built in" } else { "" },
        );
        for hop in self.via() {
            s += &format!(
                "{} {:#x} <- ",
                if hop.mixer { "mixer" } else { "selector" },
                hop.nid
            );
        }
        s += &format!(
            "converter {:#x} ({})",
            self.conv,
            if self.digital { "digital" } else { "analog" }
        );
        s
    }
}

/// How [`choose`] rates a path. A pin with something plugged in scores 2,
/// and 4 more when it is HDMI/DP (a display that hears it); an analog
/// output (line out, speaker, headphones) 1 more, so that with nothing
/// sensed anywhere the board's jack plays and not an S/PDIF socket or an
/// HDMI pin with no display.
pub fn score(path: &Path, present: bool) -> i32 {
    let mut s = 0;
    if present {
        s += 2;
        if path.hdmi && path.digital {
            s += 4;
        }
    }
    if path.analog_out() {
        s += 1;
    }
    s
}

/// Which path to play on: the best [`score`], the first of them on a tie.
/// One with nothing plugged in still plays, for a codec that cannot tell
/// (QEMU's).
pub fn choose(paths: &[(Path, bool)]) -> Option<Path> {
    let best = paths.iter().map(|(p, s)| score(p, *s)).max()?;
    paths
        .iter()
        .find(|(p, s)| score(p, *s) == best)
        .map(|(p, _)| *p)
}

/// What of the ring holds what: bytes the client wrote and bytes the
/// controller played, counted from the start of the stream. Everything
/// outside `[played, written)` is zeros, so the controller, running on
/// past what was written, plays silence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ring {
    pub written: u64,
    pub played: u64,
    /// Where the samples of the last write that found the ring empty
    /// start: [`LEAD`] past where the controller was.
    pub start: u64,
}

impl Ring {
    /// What is queued and not yet played.
    pub fn queued(&self) -> usize {
        self.written.saturating_sub(self.played) as usize
    }

    /// The samples queued and not yet played: [`Ring::queued`] without
    /// the silence ahead of them.
    pub fn queued_samples(&self) -> usize {
        self.written.saturating_sub(self.played.max(self.start)) as usize
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
            self.start = self.written;
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
    /// Verbs in a row each codec has not answered.
    missed: [u32; 16],
    /// Verbs not answered in all, for the log.
    pub timeouts: u32,
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
        // The read pointer's reset is a handshake on real controllers: set,
        // seen set, cleared, seen clear (Linux's `azx_init_cmd_io`). QEMU
        // shows neither; a controller that does not is said on the log.
        regs.w16(CORBRP, CORBRP_RST);
        if !wait_for(|| regs.r16(CORBRP) & CORBRP_RST != 0, 1_000_000) {
            println!(
                "udrv-hda: the CORB read pointer did not show its reset (CORBRP {:#06x})",
                regs.r16(CORBRP)
            );
        }
        regs.w16(CORBRP, 0);
        if !wait_for(|| regs.r16(CORBRP) & CORBRP_RST == 0, 1_000_000) {
            println!(
                "udrv-hda: the CORB read pointer stayed in reset (CORBRP {:#06x})",
                regs.r16(CORBRP)
            );
        }
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
            missed: [0; 16],
            timeouts: 0,
        }
    }

    fn rirb_wp(&self) -> usize {
        (self.regs.r16(RIRBWP) & 0xff) as usize % self.rirb_entries
    }
}

impl<R: Regs> Verbs for Rings<R> {
    fn send(&mut self, word: u32) -> Option<u32> {
        let cad = (word >> 28) as usize;
        if self.missed[cad] >= DEAD_AFTER {
            return None;
        }
        let answer = self.exchange(word, cad as u32);
        match answer {
            Some(_) => self.missed[cad] = 0,
            None => {
                self.timeouts += 1;
                self.missed[cad] += 1;
                if self.missed[cad] == DEAD_AFTER {
                    println!(
                        "udrv-hda: codec {cad} did not answer {DEAD_AFTER} verbs in a row (the last {word:#010x}): taken to be gone"
                    );
                }
            }
        }
        answer
    }
}

/// Wait up to `ns` for `done`.
fn wait_for(mut done: impl FnMut() -> bool, ns: u64) -> bool {
    let start = now_ns();
    loop {
        if done() {
            return true;
        }
        if now_ns() - start > ns {
            return done();
        }
        std::hint::spin_loop();
    }
}

impl<R: Regs> Rings<R> {
    /// Send one verb to codec `cad` and wait for its answer.
    fn exchange(&mut self, word: u32, cad: u32) -> Option<u32> {
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
                // An unsolicited answer (a jack event) is not this one, nor
                // a late one of another codec.
                if ext & (1 << 4) == 0 && ext & 0xf == cad {
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
        let bss = ((gcap >> 3) & 0x1f) as usize;
        println!(
            "udrv-hda: controller at {:#x}: GCAP {gcap:#06x} ({iss} in, {oss} out, {bss} bidirectional streams{}), GCTL {:#010x}",
            claim.offset,
            if gcap & GCAP_64OK != 0 { ", 64-bit DMA" } else { ", 32-bit DMA" },
            regs.r32(GCTL),
        );
        if gcap == 0xffff {
            return Err("GCAP reads 0xffff: the controller is not answering (powered off, or its BAR not decoded)".into());
        }
        if oss == 0 {
            return Err(format!("GCAP {gcap:#06x}: no output stream"));
        }
        // Whatever ran on it before (the firmware, or a driver of this one
        // that died before the kernel got to it) stops before the reset:
        // a stream or a ring still fetching while the link goes down is
        // what Linux stops first too.
        regs.w8(CORBCTL, 0);
        regs.w8(RIRBCTL, 0);
        for n in 0..iss + oss + bss {
            let ctl = SD_BASE + n * SD_LEN + SD_CTL;
            regs.w32(ctl, regs.r32(ctl) & !SD_CTL_RUN);
        }
        regs.w16(WAKEEN, 0);
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
        // Codecs get 521 us to say they are there; a slow one is given up
        // to 100 ms more before the controller is taken to have none.
        std::thread::sleep(Duration::from_millis(2));
        let mut codecs = regs.r16(STATESTS) & 0x7fff;
        if codecs == 0 {
            wait_for(|| regs.r16(STATESTS) & 0x7fff != 0, 100_000_000);
            codecs = regs.r16(STATESTS) & 0x7fff;
        }
        println!("udrv-hda: codecs on the link: STATESTS {codecs:#06x}");
        if codecs == 0 {
            return Err("no codec answered after the reset (STATESTS 0): the board's audio may be off in the firmware setup, or this controller has no codec".into());
        }
        regs.w16(STATESTS, codecs);
        regs.w32(INTCTL, 0);
        // The CORB and RIRB in two pages, the buffer descriptors in one,
        // the ring after them.
        let dma = Dma::new(claim.bti, 3 * PAGE + RING_LEN, None)?;
        if gcap & GCAP_64OK == 0 && dma.daddr + dma.len as u64 > 1 << 32 {
            return Err(format!(
                "the DMA memory is at {:#x}, above 4 GiB, and this controller only takes 32-bit addresses",
                dma.daddr
            ));
        }
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
        let paths = survey(&mut codec, codecs);
        let path = choose(&paths).ok_or_else(|| {
            format!(
                "no codec has an output path (STATESTS {codecs:#06x}, {} verbs unanswered)",
                codec.verbs.timeouts
            )
        })?;
        let (best, present) = paths.iter().find(|(p, _)| *p == path).copied().unwrap();
        println!(
            "udrv-hda: playing on {}{} (score {})",
            path.describe(),
            if present {
                ", something plugged in"
            } else {
                ""
            },
            score(&best, present)
        );
        codec.cad = path.cad;
        let afg = path.afg;
        codec
            .route(afg, &path)
            .ok_or_else(|| format!("routing {}: the codec stopped answering", path.describe()))?;
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

    /// Stop the stream, where it is.
    pub fn stop(&mut self) {
        let r = self.regs();
        r.w32(self.sd + SD_CTL, r.r32(self.sd + SD_CTL) & !SD_CTL_RUN);
        self.running = false;
    }

    /// Go on from where [`Controller::stop`] left it.
    pub fn resume(&mut self) {
        let r = self.regs();
        r.w32(self.sd + SD_CTL, TAG << 20 | SD_CTL_RUN);
        self.running = true;
    }
}

/// Every codec of `codecs` (the STATESTS bits) asked for its output paths,
/// each with whether something is plugged into it; what each codec is and
/// has is said on the log. A codec that does not answer, or has no audio
/// function (a modem), is passed over.
pub fn survey<V: Verbs>(codec: &mut Codec<V>, codecs: u16) -> Vec<(Path, bool)> {
    let mut out = Vec::new();
    for cad in (0..15).filter(|c| codecs & (1 << c) != 0) {
        codec.cad = cad;
        let Some(vendor) = codec.param(0, PAR_VENDOR_ID) else {
            println!("udrv-hda: codec {cad}: no answer");
            continue;
        };
        let Some(afg) = codec.audio_function() else {
            println!("udrv-hda: codec {cad} ({vendor:#010x}): no audio function (a modem?)");
            continue;
        };
        // In D0 before its pins are asked what is plugged in: a codec
        // whose function wakes in D3 (many Realteks) senses nothing there.
        let _ = codec.cmd(afg, VERB_SET_POWER_STATE, 0);
        std::thread::sleep(Duration::from_millis(1));
        let paths = codec.paths(afg);
        println!(
            "udrv-hda: codec {cad} ({vendor:#010x}), audio function {afg:#x}: {} output path(s)",
            paths.len()
        );
        for p in paths {
            let present = codec.present(&p);
            println!(
                "udrv-hda:   {}: {}, score {}",
                p.describe(),
                match (p.senses, present) {
                    (false, _) => "cannot sense",
                    (true, true) => "plugged in",
                    (true, false) => "nothing plugged in",
                },
                score(&p, present)
            );
            out.push((p, present));
        }
    }
    out
}

/// Claim controller `index`.
pub fn claim(index: usize) -> Result<Claim, crate::zx::Status> {
    claim::claim(claim::KIND_HDA, index, 300)
}

#[cfg(test)]
mod tests;
