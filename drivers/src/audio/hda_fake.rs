//! An HDA controller in host memory, for the engine tests in this file.
//!
//! The driver's MMIO helpers read and write ordinary memory in the test
//! binary; this module owns that memory and is told about every access
//! (`on_read`/`on_write`), which is what makes the registers behave like a
//! controller's: a verb posted through `CORBWP` is answered in the RIRB, a
//! `RUN` bit starts an engine, `SD_STS` is write-1-to-clear. The engine is
//! driven by the test: [`Hw::run_for_us`] is the link taking PCM out of the
//! ring at the link rate, moving `LPIB`, the DMA position buffer and
//! `WALCLK` with it and keeping every byte it took, so a test can say what
//! the cable heard, byte for byte, across ring wraps and fills.
//!
//! The clock is the per-thread test clock behind `timer_now_as_micros`,
//! moved by `run_for_us` and by the driver's own `wait_us`. One fake per
//! thread, so the tests run in parallel.

use super::*;
use crate::nvme::nvme_queue::test_clock;
use alloc::collections::BTreeMap;
use core::cell::RefCell;

extern crate std;

/// The controller's mutable side, behind the thread-local.
struct FakeState {
    bar: usize,
    /// Input and output stream descriptors the controller advertises
    /// (`GCAP`); the driver's output descriptor is the first after the
    /// inputs, and that is the one this engine runs.
    iss: usize,
    sd_base: usize,
    /// Where the CORB, RIRB, PCM ring and position buffer are. Preset by
    /// [`engine`]; learnt from the base-address registers, and from the
    /// BDL at RUN, when the driver programs them itself ([`controller`]).
    corb_va: usize,
    corb_entries: usize,
    rirb_va: usize,
    rirb_entries: usize,
    ring_va: usize,
    ring_len: usize,
    /// This stream's entry in the DMA position buffer.
    pos_va: usize,
    /// The CORB read pointer: the doorbell fetches from here up to the
    /// write pointer, so writing `CORBWP` back to where it stands (as the
    /// reset sequence does) posts nothing.
    corb_rp: usize,
    /// Low halves of the 64-bit base addresses, until the high half lands.
    corb_lo: u32,
    rirb_lo: u32,
    dp_hi: u32,
    bdl_lo: u32,
    /// A codec answers the controller reset (`STATESTS`).
    codec_present: bool,
    /// A controller that will not take a DMA position buffer: the enable
    /// bit reads back clear.
    refuse_pos_buffer: bool,

    /// Every verb word posted through the CORB, in order.
    verbs: Vec<u32>,
    /// Response per verb word; anything else is answered with 0.
    answers: BTreeMap<u32, u32>,
    /// A codec that never answers: the verb is logged and the RIRB stays
    /// as it is.
    codec_dead: bool,
    /// Unsolicited responses (jack events) delivered with the next answer,
    /// ahead of it.
    unsolicited_with_next: u32,
    /// Sticky stream status bits, write-1-to-clear.
    sts: u8,

    /// The engine: RUN as the controller sees it, how many times it was
    /// set and cleared, and how many stream resets it took.
    run: bool,
    starts: u32,
    stops: u32,
    resets: u32,
    /// An engine that keeps fetching after RUN is cleared (some HDMI
    /// controllers): the bit stays set until a stream reset.
    sticky_run: bool,
    /// Link bytes taken from the ring since RUN, and the bytes themselves.
    consumed: u64,
    played: Vec<u8>,
    /// The reported position runs this far ahead of the link (the engine's
    /// prefetch).
    lead: usize,
    /// A controller that never writes the position buffer.
    pos_buffer_dead: bool,
    /// The engine has stopped fetching: time passes, nothing is taken.
    stalled: bool,
    /// One-shot garbage for the next `LPIB` read.
    lpib_override: Option<u32>,
    /// Microseconds the clock moves per register read: time passes while
    /// the driver polls, so a loop waiting on a bit that never changes
    /// reaches its timeout instead of spinning forever. 1 by default,
    /// more where a test wants a timeout to come sooner.
    read_tick_us: u64,
    /// `WALCLK`, free-running.
    walclk: u32,
}

std::thread_local! {
    static FAKE: RefCell<Option<FakeState>> = const { RefCell::new(None) };
}

fn with_state<R>(bar: usize, f: impl FnOnce(&mut FakeState) -> R) -> Option<R> {
    FAKE.with(|cell| {
        let mut guard = cell.borrow_mut();
        match guard.as_mut() {
            Some(st) if st.bar == bar => Some(f(st)),
            _ => None,
        }
    })
}

fn poke<T>(va: usize, v: T) {
    unsafe { write_volatile(va as *mut T, v) }
}

fn peek<T>(va: usize) -> T {
    unsafe { read_volatile(va as *const T) }
}

/// Every register read by the driver.
pub(super) fn on_read(bar: usize, _off: usize) {
    if let Some(tick) = with_state(bar, |st| st.read_tick_us) {
        if tick != 0 {
            test_clock::advance(tick);
        }
    }
}

/// Every register write by the driver, after the value landed in memory.
pub(super) fn on_write(bar: usize, off: usize, v: u32) {
    with_state(bar, |st| match off {
        REG_GCTL => {
            if v & GCTL_CRST != 0 && st.codec_present {
                // Out of reset: the codec requests a state change.
                poke::<u16>(bar + REG_STATESTS, 1 << CODEC);
            }
        }
        REG_CORBSIZE => st.corb_entries = entries_for(v as u8),
        REG_RIRBSIZE => st.rirb_entries = entries_for(v as u8),
        REG_CORBLBASE => st.corb_lo = v,
        REG_CORBUBASE => st.corb_va = ((v as usize) << 32) | st.corb_lo as usize,
        REG_RIRBLBASE => st.rirb_lo = v,
        REG_RIRBUBASE => st.rirb_va = ((v as usize) << 32) | st.rirb_lo as usize,
        REG_DPUBASE => st.dp_hi = v,
        REG_DPLBASE => {
            if st.refuse_pos_buffer {
                poke::<u32>(bar + REG_DPLBASE, v & !DPLBASE_ENABLE);
            } else if v & DPLBASE_ENABLE != 0 {
                let base = ((st.dp_hi as usize) << 32) | (v & !DPLBASE_ENABLE) as usize;
                st.pos_va = base + st.iss * 8;
            }
        }
        o if o == st.sd_base + SD_BDPL => st.bdl_lo = v,
        o if o == st.sd_base + SD_BDPU => {
            // The ring is what the BDL describes: first entry's address,
            // CBL bytes. A BDL pointing elsewhere plays elsewhere.
            let bdl = ((v as usize) << 32) | st.bdl_lo as usize;
            st.ring_va = peek::<u64>(bdl) as usize;
        }
        o if o == st.sd_base + SD_CBL => st.ring_len = v as usize,
        REG_CORBRP if v & (1 << 15) != 0 => st.corb_rp = 0,
        REG_CORBWP => {
            let wp = v as usize % st.corb_entries;
            while st.corb_rp != wp {
                st.corb_rp = (st.corb_rp + 1) % st.corb_entries;
                let verb: u32 = peek(st.corb_va + st.corb_rp * 4);
                st.verbs.push(verb);
                if st.codec_dead {
                    continue;
                }
                for _ in 0..st.unsolicited_with_next {
                    post_rirb(st, 0, 1 << 4);
                }
                st.unsolicited_with_next = 0;
                let resp = st.answers.get(&verb).copied().unwrap_or(0);
                post_rirb(st, resp, 0);
            }
            poke::<u16>(bar + REG_CORBRP, st.corb_rp as u16);
        }
        REG_RIRBWP if v & (1 << 15) != 0 => poke::<u16>(bar + REG_RIRBWP, 0),
        o if o == st.sd_base + SD_CTL => {
            // The 32-bit write covered the status byte: put it back.
            poke::<u8>(bar + st.sd_base + SD_STS, st.sts);
            if v & 0x1 != 0 {
                // Stream reset: the engine halts and forgets its position.
                st.resets += 1;
                st.run = false;
                st.consumed = 0;
                poke::<u32>(bar + st.sd_base + SD_LPIB, 0);
                if st.pos_va != 0 {
                    poke::<u32>(st.pos_va, 0);
                }
                return;
            }
            let run = v & 0x2 != 0;
            if run && !st.run {
                st.run = true;
                st.starts += 1;
                st.consumed = 0;
            } else if !run && st.run {
                if st.sticky_run {
                    poke::<u32>(bar + st.sd_base + SD_CTL, v | 0x2);
                } else {
                    st.run = false;
                    st.stops += 1;
                }
            }
        }
        o if o == st.sd_base + SD_STS => {
            st.sts &= !(v as u8);
            poke::<u8>(bar + st.sd_base + SD_STS, st.sts);
        }
        _ => {}
    });
}

/// CORB/RIRB entries for a SIZE code, as the driver programs it.
fn entries_for(code: u8) -> usize {
    match code & 0x3 {
        0x2 => 256,
        0x1 => 16,
        _ => 2,
    }
}

fn post_rirb(st: &mut FakeState, resp: u32, ext: u32) {
    let wp = ((peek::<u16>(st.bar + REG_RIRBWP) as usize & 0xff) + 1) % st.rirb_entries;
    poke::<u32>(st.rirb_va + wp * 8, resp);
    poke::<u32>(st.rirb_va + wp * 8 + 4, ext);
    poke::<u16>(st.bar + REG_RIRBWP, wp as u16);
    let sts: u8 = peek(st.bar + REG_RIRBSTS);
    poke::<u8>(st.bar + REG_RIRBSTS, sts | RIRBSTS_IRQ);
}

/// The codec the fake answers for, and the output path the driver is
/// told it found: one converter and one analog pin, like QEMU's
/// `hda-output`.
pub(super) const CODEC: u32 = 0;
pub(super) const AFG: u32 = 1;
pub(super) const CONV: u32 = 2;
pub(super) const PIN: u32 = 3;
pub(super) const TAG: u32 = 1;

/// Link bytes per second at the link format (48 kHz stereo S16).
const LINK_BYTES_PER_S: u64 = LINK_RATE as u64 * 4;

/// The DAC's output amplifier: mute capable, 0 dB at step 0x4a.
pub(super) const DAC_AMP_CAP: u32 = (1 << 31) | 0x4a;
/// Config default: jack, line out, front, green.
pub(super) const PIN_DEFCFG_LINE_OUT_JACK: u32 = 0x0101_4010;

/// The test's handle on the controller: its BAR, and the engine's
/// controls.
#[derive(Clone, Copy)]
pub(super) struct Hw {
    pub bar: usize,
}

impl Hw {
    fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        with_state(self.bar, f).expect("fake controller installed on this thread")
    }

    /// The PCM ring as the engine knows it (from the BDL once RUN).
    pub fn ring_len(&self) -> usize {
        self.with(|st| st.ring_len)
    }

    pub fn ring_va(&self) -> usize {
        self.with(|st| st.ring_va)
    }

    pub fn sd_base(&self) -> usize {
        self.with(|st| st.sd_base)
    }

    pub fn corb_va(&self) -> usize {
        self.with(|st| st.corb_va)
    }

    pub fn rirb_va(&self) -> usize {
        self.with(|st| st.rirb_va)
    }

    pub fn pos_va(&self) -> usize {
        self.with(|st| st.pos_va)
    }

    /// The link runs for `us`: PCM leaves the ring at the link rate (whole
    /// frames), the position registers and `WALCLK` follow, and so does
    /// the clock. With the engine stopped or stalled only the clocks move.
    pub fn run_for_us(&self, us: u64) {
        let bytes = (us * LINK_BYTES_PER_S / 1_000_000) as usize / 4 * 4;
        self.with(|st| {
            st.walclk = st.walclk.wrapping_add((us * WALCLK_HZ / 1_000_000) as u32);
            poke::<u32>(st.bar + REG_WALCLK, st.walclk);
            if st.run && !st.stalled {
                let mut left = bytes;
                while left > 0 {
                    let at = (st.consumed % st.ring_len as u64) as usize;
                    let chunk = left.min(st.ring_len - at);
                    let src = unsafe {
                        core::slice::from_raw_parts((st.ring_va + at) as *const u8, chunk)
                    };
                    st.played.extend_from_slice(src);
                    st.consumed += chunk as u64;
                    left -= chunk;
                }
                let reported = ((st.consumed + st.lead as u64) % st.ring_len as u64) as u32;
                poke::<u32>(
                    st.bar + st.sd_base + SD_LPIB,
                    st.lpib_override.take().unwrap_or(reported),
                );
                if !st.pos_buffer_dead && st.pos_va != 0 {
                    poke::<u32>(st.pos_va, reported);
                }
            }
        });
        test_clock::advance(us);
    }

    /// What the link has taken from the ring since it was started.
    pub fn played(&self) -> Vec<u8> {
        self.with(|st| st.played.clone())
    }

    /// The ring as the engine sees it.
    pub fn ring(&self) -> Vec<u8> {
        let (va, len) = self.with(|st| (st.ring_va, st.ring_len));
        unsafe { core::slice::from_raw_parts(va as *const u8, len) }.to_vec()
    }

    pub fn verbs(&self) -> Vec<u32> {
        self.with(|st| st.verbs.clone())
    }

    pub fn answer(&self, verb: u32, resp: u32) {
        self.with(|st| {
            st.answers.insert(verb, resp);
        });
    }

    pub fn starts(&self) -> u32 {
        self.with(|st| st.starts)
    }

    pub fn stops(&self) -> u32 {
        self.with(|st| st.stops)
    }

    pub fn resets(&self) -> u32 {
        self.with(|st| st.resets)
    }

    pub fn running(&self) -> bool {
        self.with(|st| st.run)
    }

    pub fn set_lead(&self, bytes: usize) {
        self.with(|st| st.lead = bytes);
    }

    pub fn set_sticky_run(&self) {
        self.with(|st| st.sticky_run = true);
    }

    pub fn set_codec_dead(&self) {
        self.with(|st| st.codec_dead = true);
    }

    pub fn set_pos_buffer_dead(&self) {
        self.with(|st| st.pos_buffer_dead = true);
    }

    pub fn set_stalled(&self, stalled: bool) {
        self.with(|st| st.stalled = stalled);
    }

    pub fn set_read_tick_us(&self, us: u64) {
        self.with(|st| st.read_tick_us = us);
    }

    /// No codec answers the controller reset.
    pub fn set_codec_absent(&self) {
        self.with(|st| st.codec_present = false);
    }

    pub fn set_refuse_pos_buffer(&self) {
        self.with(|st| st.refuse_pos_buffer = true);
    }

    /// The controller advertises `iss` input and `oss` output stream
    /// descriptors (`GCAP`), 64-bit capable.
    pub fn set_streams(&self, iss: usize, oss: usize) {
        self.with(|st| {
            st.iss = iss;
            st.sd_base = REG_SD_BASE + iss * 0x20;
            poke::<u16>(
                st.bar + REG_GCAP,
                ((oss as u16) << 12) | ((iss as u16) << 8) | 1,
            );
        });
    }

    /// The codec QEMU's `hda-output` presents, near enough: a root node,
    /// one audio function group, a stereo DAC with an output amp (nid 2)
    /// wired to one line-out pin complex with a jack (nid 3).
    pub fn install_output_codec(&self) {
        let vendor_id = 0x1af4_0010;
        let root = 0;
        let pairs = [
            (verb12(root, VERB_GET_PARAMETER, PAR_VENDOR_ID), vendor_id),
            (
                verb12(root, VERB_GET_PARAMETER, PAR_NODE_COUNT),
                (AFG << 16) | 1,
            ),
            (verb12(AFG, VERB_GET_PARAMETER, PAR_FUNCTION_TYPE), 0x01),
            (
                verb12(AFG, VERB_GET_PARAMETER, PAR_NODE_COUNT),
                (CONV << 16) | 2,
            ),
            (
                verb12(CONV, VERB_GET_PARAMETER, PAR_AUDIO_WIDGET_CAP),
                (WIDGET_AUDIO_OUT << 20) | (1 << 2) | 1,
            ),
            (
                verb12(CONV, VERB_GET_PARAMETER, PAR_OUT_AMP_CAP),
                DAC_AMP_CAP,
            ),
            (
                verb12(PIN, VERB_GET_PARAMETER, PAR_AUDIO_WIDGET_CAP),
                (WIDGET_PIN << 20) | (1 << 8) | 1,
            ),
            (
                verb12(PIN, VERB_GET_PARAMETER, PAR_PIN_CAP),
                (1 << 4) | (1 << 2),
            ),
            (
                verb12(PIN, VERB_GET_CONFIG_DEFAULT, 0),
                PIN_DEFCFG_LINE_OUT_JACK,
            ),
            (verb12(PIN, VERB_GET_PARAMETER, PAR_CONN_LIST_LEN), 1),
            (verb12(PIN, VERB_GET_CONN_LIST, 0), CONV),
            (verb12(PIN, VERB_GET_PIN_SENSE, 0), 1 << 31),
        ];
        for (verb, resp) in pairs {
            self.answer(verb, resp);
        }
    }

    pub fn corrupt_next_lpib(&self, v: u32) {
        self.with(|st| st.lpib_override = Some(v));
    }

    /// A jack event lands in the RIRB now, with no verb outstanding.
    pub fn post_unsolicited(&self) {
        self.with(|st| post_rirb(st, 0, 1 << 4));
    }

    /// The next answer arrives behind `n` jack events.
    pub fn unsolicited_with_next_answer(&self, n: u32) {
        self.with(|st| st.unsolicited_with_next = n);
    }

    /// The engine reports a FIFO error.
    pub fn raise_fifo_error(&self) {
        self.with(|st| {
            st.sts |= SD_STS_FIFOE;
            poke::<u8>(st.bar + st.sd_base + SD_STS, st.sts);
        });
    }
}

/// A fresh controller on this thread, and the driver state over it as it
/// stands after codec discovery: one analog path found, nothing running,
/// the clock at zero.
pub(super) fn engine() -> (Hw, HdaInner) {
    let hw = install(0);
    let bar = hw.bar;
    let (corb_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let (rirb_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let ring_len = RING_PAGES * PAGE_SIZE;
    let (ring_va, _) = ProviderImpl::alloc_dma(ring_len);
    let (bdl_va, bdl_pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let (pos_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    for i in 0..ring_len / BDL_SEGMENT {
        poke::<u64>(bdl_va + i * 16, (ring_va + i * BDL_SEGMENT) as u64);
        poke::<u32>(bdl_va + i * 16 + 8, BDL_SEGMENT as u32);
    }
    let sd_base = REG_SD_BASE;
    hw.with(|st| {
        st.corb_va = corb_va;
        st.rirb_va = rirb_va;
        st.ring_va = ring_va;
        st.ring_len = ring_len;
        st.pos_va = pos_va;
    });
    let mut inner = HdaInner::bare(
        bar, corb_va, 256, rirb_va, 256, CODEC, sd_base, ring_va, ring_len, bdl_pa, pos_va,
    );
    inner.afg = AFG;
    inner.conv_nid = CONV;
    inner.pin_nid = PIN;
    inner.stream_tag = TAG;
    inner.candidates.push(OutPath {
        conv: CONV,
        pin: PIN,
        pin_conn_idx: 0,
        digital: false,
        hdmi_dp: false,
        present: true,
    });
    (hw, inner)
}

/// A controller as `HdaDevice::new` finds it at PCI probe: `iss` input
/// and 4 output descriptors, 256-entry CORB/RIRB on offer, the output
/// codec attached, nothing programmed. The test calls `HdaDevice::new`
/// on `hw.bar` itself, after any change of circumstances it wants.
pub(super) fn controller(iss: usize) -> Hw {
    let hw = install(iss);
    hw.set_streams(iss, 4);
    poke::<u8>(hw.bar + REG_CORBSIZE, 0x40);
    poke::<u8>(hw.bar + REG_RIRBSIZE, 0x40);
    hw.install_output_codec();
    hw
}

/// A fresh, empty controller on this thread and the clock at zero.
fn install(iss: usize) -> Hw {
    test_clock::set(0);
    let (bar, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let sd_base = REG_SD_BASE + iss * 0x20;
    FAKE.with(|cell| {
        *cell.borrow_mut() = Some(FakeState {
            bar,
            iss,
            sd_base,
            corb_va: 0,
            corb_entries: 256,
            rirb_va: 0,
            rirb_entries: 256,
            ring_va: 0,
            ring_len: 0,
            pos_va: 0,
            corb_rp: 0,
            corb_lo: 0,
            rirb_lo: 0,
            dp_hi: 0,
            bdl_lo: 0,
            codec_present: true,
            refuse_pos_buffer: false,
            verbs: Vec::new(),
            answers: BTreeMap::new(),
            codec_dead: false,
            unsolicited_with_next: 0,
            sts: 0,
            run: false,
            starts: 0,
            stops: 0,
            resets: 0,
            sticky_run: false,
            consumed: 0,
            played: Vec::new(),
            lead: 0,
            pos_buffer_dead: false,
            stalled: false,
            lpib_override: None,
            read_tick_us: 1,
            walclk: 0,
        });
    });
    Hw { bar }
}

/// The verb word `HdaInner::cmd` posts.
pub(super) fn verb12(nid: u32, verb: u32, payload: u32) -> u32 {
    (CODEC << 28) | (nid << 20) | (verb << 8) | (payload & 0xff)
}

/// The verb word `HdaInner::cmd16` posts.
pub(super) fn verb4(nid: u32, verb: u32, payload: u32) -> u32 {
    (CODEC << 28) | (nid << 20) | (verb << 16) | (payload & 0xffff)
}

/// Client PCM that is its own record: sample `k` is `k` (wrapping), so
/// any run of bytes the link heard can be checked against the offset it
/// should have come from.
pub(super) struct Ramp {
    next: u16,
    log: Vec<u8>,
}

impl Ramp {
    pub fn new() -> Self {
        Ramp {
            next: 1,
            log: Vec::new(),
        }
    }

    /// The next `n` bytes (whole samples).
    pub fn take(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n / 2 {
            out.extend_from_slice(&self.next.to_le_bytes());
            self.next = self.next.wrapping_add(1);
        }
        self.log.extend_from_slice(&out);
        out
    }

    /// Everything handed out so far.
    pub fn log(&self) -> &[u8] {
        &self.log
    }

    /// Fill stream `id` with as much as it takes; returns the bytes taken.
    pub fn feed(&mut self, inner: &mut HdaInner, id: u32) -> usize {
        let free = inner.stream_mut(id).map(|s| s.free_bytes()).unwrap_or(0);
        if free == 0 {
            return 0;
        }
        let pcm = self.take(free);
        let n = inner.stream_mut(id).unwrap().write(&pcm);
        assert_eq!(n, pcm.len(), "the stream takes what it said was free");
        n
    }
}

/// `n` bytes of the constant sample `v`.
pub(super) fn constant(v: i16, n: usize) -> Vec<u8> {
    v.to_le_bytes().iter().copied().cycle().take(n).collect()
}

pub(super) fn samples(bytes: &[u8]) -> Vec<i16> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

#[cfg(test)]
mod engine_tests {
    //! The DAI side of the pipeline, end to end: what the driver programs
    //! into the controller, what it puts in the ring, and what the link
    //! takes out of it. Every test plays the hardware through [`Hw`] and
    //! asserts on the bytes the cable would have carried.

    use super::*;

    /// One 4 ms tick of the ALSA watchdog: the link plays, the driver polls.
    fn tick(hw: &Hw, inner: &mut HdaInner) {
        hw.run_for_us(4_000);
        inner.poll_progress();
    }

    /// Tick until `done`, or fail after `max` ticks: a condition the engine
    /// never reaches is a failure, not a hang.
    fn tick_until(
        hw: &Hw,
        inner: &mut HdaInner,
        max: usize,
        mut done: impl FnMut(&HdaInner) -> bool,
    ) {
        for _ in 0..max {
            if done(inner) {
                return;
            }
            tick(hw, inner);
        }
        assert!(done(inner), "not reached within {} ticks", max);
    }

    /// `assert_eq!` on PCM, reporting the first byte that differs rather
    /// than dumping both buffers.
    fn assert_same_pcm(heard: &[u8], expected: &[u8], what: &str) {
        assert_eq!(heard.len(), expected.len(), "{}: length", what);
        if let Some(at) = (0..heard.len()).find(|&i| heard[i] != expected[i]) {
            let end = (at + 8).min(heard.len());
            panic!(
                "{}: first difference at byte {} of {}: heard {:?}, expected {:?}",
                what,
                at,
                heard.len(),
                &heard[at..end],
                &expected[at..end]
            );
        }
    }

    fn stream_with(inner: &mut HdaInner, pcm: &[u8]) -> u32 {
        let id = inner.add_stream(HdaInner::new_stream());
        let n = inner.stream_mut(id).unwrap().write(pcm);
        assert_eq!(n, pcm.len());
        id
    }

    fn last_stop(inner: &HdaInner) -> StopEvent {
        inner.stops[STOP_HISTORY - 1]
    }

    #[test]
    fn starting_the_engine_programs_the_descriptor_and_primes_the_ring() {
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        inner.ensure_engine().unwrap();
        assert!(!inner.running, "nothing to play: the engine stays down");
        assert_eq!(hw.starts(), 0);

        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        assert!(inner.running);
        assert_eq!(hw.starts(), 1);
        assert_eq!(inner.stat_restarts, 1);

        let sd = REG_SD_BASE;
        assert_eq!(mmio_r32(hw.bar, sd + SD_CTL), (TAG << 20) | 0x2);
        assert_eq!(mmio_r32(hw.bar, sd + SD_CBL), hw.ring_len() as u32);
        assert_eq!(
            mmio_r16(hw.bar, sd + SD_LVI),
            (hw.ring_len() / BDL_SEGMENT - 1) as u16
        );
        assert_eq!(mmio_r16(hw.bar, sd + SD_FMT), stream_format(LINK_RATE, 2));
        assert_eq!(mmio_r32(hw.bar, sd + SD_BDPL), inner.bdl_pa as u32);
        let verbs = hw.verbs();
        let fmt = stream_format(LINK_RATE, 2) as u32;
        assert!(verbs.contains(&verb4(CONV, 0x2, fmt)), "converter format");
        assert!(
            verbs.contains(&verb12(CONV, VERB_SET_STREAM_ID, TAG << 4)),
            "converter on our stream tag"
        );
        assert!(
            verbs.contains(&verb12(PIN, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN)),
            "the path is re-armed on every start"
        );

        // FILL_DEPTH of the client's PCM at offset 0, zeros after it.
        assert_eq!(inner.fill_pos, FILL_DEPTH as u64);
        let ring = hw.ring();
        assert_same_pcm(
            &ring[..FILL_DEPTH],
            &ramp.log()[..FILL_DEPTH],
            "primed ring",
        );
        assert!(ring[FILL_DEPTH..].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_link_hears_the_client_pcm_unbroken_across_laps() {
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        // Two and a half laps of the ring, fed as the client would be. The
        // watchdog's ticks are not exact, so neither are these: the fills
        // then straddle the end of the ring rather than always ending on it.
        let target = hw.ring_len() * 5 / 2;
        let mut n = 0;
        let mut straddled = 0;
        while hw.played().len() < target {
            ramp.feed(&mut inner, id);
            hw.run_for_us(3_700 + (n % 7) * 100);
            let before = inner.fill_pos;
            inner.poll_progress();
            let ring = hw.ring_len() as u64;
            if inner.fill_pos / ring > before / ring && inner.fill_pos % ring != 0 {
                straddled += 1;
            }
            n += 1;
            assert!(n < 10_000, "the link never got there");
        }
        assert!(
            straddled >= 2,
            "{} fills straddled the end of the ring",
            straddled
        );
        let played = hw.played();
        assert_same_pcm(&played, &ramp.log()[..played.len()], "what the link heard");
        assert_eq!(inner.stat_underruns, 0);
        assert_eq!(inner.stat_late_fills, 0);
        assert_eq!(inner.stat_bad_pos, 0);
        assert_eq!(inner.consumed, played.len() as u64);
        assert!(inner.fill_pos >= inner.consumed + FILL_DEPTH as u64 - FILL_MAX as u64);
    }

    #[test]
    fn the_fill_stays_within_its_depth_of_the_play_position() {
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        for _ in 0..100 {
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
            let ahead = inner.fill_pos - inner.consumed;
            assert!(
                ahead <= FILL_DEPTH as u64
                    && ahead + 4 * 4_000 * LINK_RATE as u64 / 1_000_000 >= FILL_DEPTH as u64,
                "fill runs {} B ahead of the link, depth is {}",
                ahead,
                FILL_DEPTH
            );
        }
    }

    #[test]
    fn the_silence_band_is_kept_incrementally_past_the_fill_point() {
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        // Past one lap, so the ring beyond the band holds last lap's PCM.
        for _ in 0..1_000 {
            if hw.played().len() >= hw.ring_len() * 3 / 2 {
                break;
            }
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
        }
        assert!(hw.played().len() >= hw.ring_len() * 3 / 2);
        let ring = hw.ring();
        let at = |pos: u64| (pos % hw.ring_len() as u64) as usize;
        let band: Vec<u8> = (0..SILENCE_AHEAD)
            .map(|i| ring[at(inner.fill_pos + i as u64)])
            .collect();
        assert!(
            band.iter().all(|&b| b == 0),
            "the band after the fill is zero"
        );
        let beyond: Vec<u8> = (0..4096)
            .map(|i| ring[at(inner.fill_pos + (SILENCE_AHEAD + 4096 + i) as u64)])
            .collect();
        assert!(
            beyond.iter().any(|&b| b != 0),
            "past the band the ring still holds last lap: the band is not a wipe"
        );
        assert_eq!(inner.zero_end, inner.fill_pos + SILENCE_AHEAD as u64);
    }

    #[test]
    fn a_fill_held_up_past_the_band_skips_to_the_engine_and_keeps_the_pcm() {
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        for _ in 0..10 {
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
        }
        let filled = inner.fill_pos;
        // The fill loop is held up for 200 ms: the link plays through the
        // band and into the lap-old ring.
        hw.run_for_us(200_000);
        let heard = hw.played();
        let silence = &heard[filled as usize..filled as usize + SILENCE_AHEAD];
        assert!(
            silence.iter().all(|&b| b == 0),
            "the band is what plays first"
        );
        inner.poll_progress();
        assert_eq!(inner.stat_late_fills, 1);
        assert!(inner.fill_pos >= inner.consumed);
        let resumed_at = inner.consumed / 4 * 4;
        // The stream's PCM continues from where the mixer had got to, at
        // the position the engine reached: late, not dropped.
        for _ in 0..10 {
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
        }
        let heard = hw.played();
        let n = heard.len() - resumed_at as usize;
        assert_same_pcm(
            &heard[resumed_at as usize..],
            &ramp.log()[filled as usize..filled as usize + n],
            "the PCM after the late fill",
        );
    }

    #[test]
    fn a_started_stream_running_dry_opens_an_underrun_gap_and_the_engine_idles_out() {
        let (hw, mut inner) = engine();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        let _id = stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        // Never fed again: the stream runs dry after HOST_BUFFER bytes.
        tick_until(&hw, &mut inner, 200, |i| i.gap_since_us != 0);
        assert_eq!(inner.stat_underruns, 1);
        assert_eq!(inner.stat_drains, 0);
        assert_eq!(last_stop(&inner).kind, b'U');
        // The record counts whole fills: the one that ran the stream dry
        // is in, zeros and all.
        assert!((HOST_BUFFER..HOST_BUFFER + FILL_MAX).contains(&last_stop(&inner).written));
        assert_eq!(inner.mixed_bytes, last_stop(&inner).written);
        assert!(inner.running, "the engine plays silence through the gap");
        let opened = inner.gap_since_us;
        tick_until(&hw, &mut inner, 2_000, |i| !i.running);
        assert_eq!(inner.stat_idle_stops, 1);
        assert!(test_clock::now() - opened >= DAI_IDLE_STOP_US);
        assert!(test_clock::now() - opened < DAI_IDLE_STOP_US + 8_000);
        assert!(!hw.running());
        assert_eq!(hw.stops(), 1);
        assert_eq!(inner.stat_stop_timeouts, 0);
        let heard = hw.played();
        assert_same_pcm(&heard[..HOST_BUFFER], &pcm, "the PCM before the gap");
        assert!(
            heard[HOST_BUFFER..].iter().all(|&b| b == 0),
            "silence after the PCM ran out"
        );
        assert!(
            hw.ring().iter().all(|&b| b == 0),
            "the ring is wiped on stop"
        );
        assert_eq!(
            (inner.fill_pos, inner.zero_end, inner.gap_since_us),
            (0, 0, 0)
        );
    }

    #[test]
    fn a_paused_stream_is_a_drain_not_an_underrun() {
        let (hw, mut inner) = engine();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        let id = stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        tick(&hw, &mut inner);
        inner.stream_mut(id).unwrap().pause();
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_drains, 1);
        assert_eq!(inner.stat_underruns, 0);
        assert_eq!(last_stop(&inner).kind, b'D');
        assert_ne!(inner.gap_since_us, 0);
        inner.stream_mut(id).unwrap().resume();
        tick(&hw, &mut inner);
        assert_eq!(inner.gap_since_us, 0, "PCM again: the gap closes");
        assert_eq!(inner.stat_idle_stops, 0);
        assert!(inner.running);
    }

    #[test]
    fn writing_after_an_idle_stop_restarts_the_engine_where_the_stream_left_off() {
        let (hw, mut inner) = engine();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        let id = stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        tick_until(&hw, &mut inner, 2_000, |i| !i.running);
        let heard_before = hw.played().len();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        assert!(inner.running);
        assert_eq!(hw.starts(), 2);
        assert_eq!(inner.stat_restarts, 2);
        assert_eq!(inner.fill_pos, FILL_DEPTH as u64);
        assert_eq!(inner.consumed, 0, "positions restart with the stream");
        assert_same_pcm(
            &hw.ring()[..FILL_DEPTH],
            &ramp.log()[HOST_BUFFER..HOST_BUFFER + FILL_DEPTH],
            "the ring primed on restart",
        );
        for _ in 0..5 {
            tick(&hw, &mut inner);
        }
        let heard = hw.played();
        let n = heard.len() - heard_before;
        assert_same_pcm(
            &heard[heard_before..],
            &ramp.log()[HOST_BUFFER..HOST_BUFFER + n],
            "the PCM after the restart",
        );
    }

    #[test]
    fn an_engine_that_will_not_stop_is_reset_and_keeps_its_ring() {
        let (hw, mut inner) = engine();
        hw.set_sticky_run();
        hw.set_read_tick_us(100);
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        let primed = hw.ring()[..FILL_DEPTH].to_vec();
        assert_eq!(hw.resets(), 1, "every start goes through a stream reset");
        inner.stop_engine();
        assert!(!inner.running);
        assert_eq!(inner.stat_stop_timeouts, 1);
        assert_eq!(hw.resets(), 2, "SRST halts the DMA engine");
        assert!(!hw.running());
        assert_eq!(mmio_r32(hw.bar, REG_SD_BASE + SD_CTL), 0);
        assert_same_pcm(
            &hw.ring()[..FILL_DEPTH],
            &primed,
            "the ring is not rewritten under an engine that may still fetch",
        );
    }

    #[test]
    fn two_streams_reach_the_link_as_their_saturated_sum() {
        let (hw, mut inner) = engine();
        stream_with(&mut inner, &constant(20_000, HOST_BUFFER));
        let b = stream_with(&mut inner, &constant(-5_000, HOST_BUFFER));
        inner.ensure_engine().unwrap();
        tick(&hw, &mut inner);
        let heard = samples(&hw.played());
        assert!(heard.iter().all(|&s| s == 15_000), "{:?}", &heard[..4]);
        inner.stream_mut(b).unwrap().pause();
        let c = stream_with(&mut inner, &constant(20_000, HOST_BUFFER));
        let before = hw.played().len();
        // The fill is FILL_DEPTH ahead: the change is heard once that plays.
        for _ in 0..(FILL_DEPTH / 768 + 2) {
            tick(&hw, &mut inner);
        }
        let heard = samples(&hw.played()[before + FILL_DEPTH..]);
        assert!(heard.iter().all(|&s| s == 32_767), "{:?}", &heard[..4]);
        let _ = c;
    }

    #[test]
    fn the_master_mute_reaches_the_link_after_its_ramp() {
        let (hw, mut inner) = engine();
        stream_with(&mut inner, &constant(10_000, HOST_BUFFER));
        inner.ensure_engine().unwrap();
        tick(&hw, &mut inner);
        assert!(samples(&hw.played()).iter().all(|&s| s == 10_000));
        inner.set_gain(100, 100, true, true);
        assert_eq!(inner.gain(), (100, 100, true, true));
        let before = hw.played().len();
        for _ in 0..(FILL_DEPTH / 768 + 12) {
            tick(&hw, &mut inner);
        }
        let heard = samples(&hw.played()[before + FILL_DEPTH..]);
        assert_eq!(heard[0], 10_000, "the fade starts from full gain");
        assert_eq!(*heard.last().unwrap(), 0, "and ends muted");
        assert!(
            heard.windows(2).all(|w| w[1] <= w[0]),
            "monotonic fade, not a step"
        );
    }

    #[test]
    fn a_verb_is_answered_from_the_rirb_and_stale_entries_are_dropped_first() {
        let (hw, mut inner) = engine();
        hw.answer(
            verb12(CODEC, VERB_GET_PARAMETER, PAR_VENDOR_ID),
            0x10ec_0262,
        );
        assert_eq!(inner.param(CODEC, PAR_VENDOR_ID).unwrap(), 0x10ec_0262);
        assert_eq!(inner.stat_stale_resp, 0);
        // A jack event sitting in the ring before the verb is posted would
        // otherwise be read as the answer, and every reply after it shifted
        // by one.
        hw.post_unsolicited();
        hw.post_unsolicited();
        assert_eq!(inner.param(CODEC, PAR_VENDOR_ID).unwrap(), 0x10ec_0262);
        assert_eq!(inner.stat_stale_resp, 2);
        // One arriving with the answer, ahead of it, is skipped, not counted.
        hw.unsolicited_with_next_answer(1);
        assert_eq!(inner.param(CODEC, PAR_VENDOR_ID).unwrap(), 0x10ec_0262);
        assert_eq!(inner.stat_stale_resp, 2);
        assert_eq!(hw.verbs().len(), 3);
    }

    #[test]
    fn a_codec_that_stops_answering_fails_the_verb_after_the_timeout() {
        let (hw, mut inner) = engine();
        hw.set_codec_dead();
        hw.set_read_tick_us(1_000);
        let t0 = test_clock::now();
        assert!(matches!(
            inner.param(CODEC, PAR_VENDOR_ID),
            Err(DeviceError::IoError)
        ));
        let waited = test_clock::now() - t0;
        assert!(waited > VERB_TIMEOUT_US && waited < VERB_TIMEOUT_US + 10_000);
        assert_eq!(hw.verbs().len(), 1);
    }

    #[test]
    fn an_impossible_position_read_is_rejected_and_the_play_position_holds() {
        let (hw, mut inner) = engine();
        hw.set_pos_buffer_dead();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        tick(&hw, &mut inner);
        tick(&hw, &mut inner);
        let consumed = inner.consumed;
        let fill = inner.fill_pos;
        hw.corrupt_next_lpib((hw.ring_len() / 2) as u32);
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_bad_pos, 1);
        assert_eq!(inner.last_bad_src, b'L');
        assert_eq!(inner.last_bad_pos, (hw.ring_len() / 2) as u32);
        assert_eq!(inner.consumed, consumed, "one bad read moves nothing");
        assert_eq!(inner.fill_pos, fill, "and the writer is not handed a lap");
        // The next sane read is judged against the widened budget.
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_bad_pos, 1);
        assert_eq!(inner.consumed, consumed + 2 * 768);
    }

    #[test]
    fn a_position_buffer_the_controller_never_writes_does_not_freeze_playback() {
        let (hw, mut inner) = engine();
        hw.set_pos_buffer_dead();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        for _ in 0..20 {
            tick(&hw, &mut inner);
        }
        assert!(!inner.dpib_trusted);
        assert_eq!(inner.consumed, 20 * 768);
        assert_eq!(inner.stat_bad_pos, 0);
    }

    #[test]
    fn the_reported_position_caps_the_link_clock_when_the_engine_stalls() {
        let (hw, mut inner) = engine();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        for _ in 0..5 {
            tick(&hw, &mut inner);
        }
        let consumed = inner.consumed;
        let fill = inner.fill_pos;
        hw.set_stalled(true);
        for _ in 0..25 {
            tick(&hw, &mut inner);
        }
        assert_eq!(inner.consumed, consumed, "the clock alone moves nothing");
        assert_eq!(inner.fill_pos, fill);
        assert_eq!(inner.stat_bad_pos, 0);
        hw.set_stalled(false);
        for _ in 0..5 {
            tick(&hw, &mut inner);
        }
        assert_eq!(inner.consumed, consumed + 5 * 768);
    }

    #[test]
    fn the_reported_position_ahead_of_the_link_pulls_the_fill_with_it() {
        let (hw, mut inner) = engine();
        hw.set_lead(4096);
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        tick(&hw, &mut inner);
        assert_eq!(inner.lead_now, 4096);
        assert_eq!(inner.stat_lead, 4096);
        assert_eq!(
            inner.consumed, 768,
            "consumed follows the link, not the prefetch"
        );
        assert_eq!(inner.fill_pos, 768 + 4096 + FILL_DEPTH as u64);
    }

    #[test]
    fn a_runaway_reported_position_cannot_push_the_fill_over_the_playhead() {
        // A controller whose reported position runs further and further
        // ahead of the link (QEMU was seen 106 KB ahead) pulls the fill
        // with it, up to the guard behind the playhead and no further:
        // past that the fill would overwrite PCM the link has yet to play.
        let (hw, mut inner) = engine();
        let id = inner.add_stream(HdaInner::new_stream());
        let mut ramp = Ramp::new();
        ramp.feed(&mut inner, id);
        inner.ensure_engine().unwrap();
        // Up to 8 KiB short of the guard: a report further ahead than the
        // ring less the guard reads as the engine having lapped the fill,
        // and the fill skips to it (see `fill_ring`) -- past the envelope
        // the constants were chosen for.
        let most = hw.ring_len() - RING_GUARD - 8192;
        let mut lead = 0;
        let mut bound = false;
        for _ in 0..120 {
            lead = (lead + 4096).min(most);
            hw.set_lead(lead);
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
            assert_eq!(inner.stat_bad_pos, 0, "the lead grows within the budget");
            let window = inner.consumed + (hw.ring_len() - RING_GUARD) as u64;
            assert!(
                inner.fill_pos <= window,
                "fill at {} B, {} B past the guard",
                inner.fill_pos,
                inner.fill_pos - window
            );
            if inner.fill_pos < inner.lead_now + inner.consumed + FILL_DEPTH as u64 {
                bound = true;
            }
        }
        assert!(bound, "the guard was never what limited the fill");
        assert!(inner.lead_now > (hw.ring_len() - RING_GUARD - FILL_DEPTH) as u64);
        let played = hw.played();
        assert_same_pcm(
            &played,
            &ramp.log()[..played.len()],
            "nothing unplayed was overwritten",
        );
    }

    #[test]
    fn stream_errors_are_counted_and_cleared() {
        let (hw, mut inner) = engine();
        let mut ramp = Ramp::new();
        let pcm = ramp.take(HOST_BUFFER);
        stream_with(&mut inner, &pcm);
        inner.ensure_engine().unwrap();
        hw.raise_fifo_error();
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_fifo_err, 1);
        assert_eq!(mmio_r8(hw.bar, REG_SD_BASE + SD_STS) & SD_STS_FIFOE, 0);
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_fifo_err, 1, "cleared: counted once");
    }
}

#[cfg(test)]
mod probe_tests {
    //! `HdaDevice::new` from the first register read to the chosen path,
    //! against the controller and codec the fake presents, and the device
    //! through its public interface after that: what a front end sees.

    use super::*;
    use crate::scheme::AudioScheme;

    const ISS: usize = 4;

    fn probe(hw: &Hw) -> DeviceResult<HdaDevice> {
        HdaDevice::new(hw.bar, String::from("hda-fake"), false)
    }

    fn device() -> (Hw, HdaDevice) {
        let hw = controller(ISS);
        let dev = probe(&hw).expect("probe");
        (hw, dev)
    }

    /// One watchdog tick through the public interface: the link plays,
    /// the front end asks what is queued (which polls).
    fn tick(hw: &Hw, dev: &dyn AudioScheme) {
        hw.run_for_us(4_000);
        dev.queued_bytes();
    }

    #[test]
    fn probe_brings_the_controller_up_and_hands_it_its_rings() {
        let (hw, dev) = device();
        let bar = hw.bar;
        assert_ne!(mmio_r32(bar, REG_GCTL) & GCTL_CRST, 0, "out of reset");
        assert_eq!(mmio_r32(bar, REG_INTCTL), 0, "polled: no interrupts");
        let inner = dev.inner.lock();
        assert_eq!(inner.cad, CODEC);
        assert_eq!((inner.corb_entries, inner.rirb_entries), (256, 256));
        assert_eq!(mmio_r8(bar, REG_CORBSIZE), 0x2);
        assert_eq!(mmio_r8(bar, REG_RIRBSIZE), 0x2);
        assert_eq!(hw.corb_va(), inner.corb_va, "CORB base is the CORB");
        assert_eq!(hw.rirb_va(), inner.rirb_va, "RIRB base is the RIRB");
        assert_ne!(inner.corb_va, 0);
        assert_eq!(mmio_r8(bar, REG_CORBCTL) & 0x2, 0x2, "CORB DMA running");
        assert_eq!(mmio_r8(bar, REG_RIRBCTL), RIRBCTL_DMA_EN | RIRBCTL_IRQ_EN);
        assert_eq!(mmio_r16(bar, REG_RINTCNT), 1, "one response per status");
        assert_eq!(
            inner.sd_base,
            REG_SD_BASE + ISS * 0x20,
            "the first output descriptor comes after the inputs"
        );
        assert_eq!(
            hw.pos_va(),
            inner.dma_pos_va,
            "position buffer entry for that descriptor"
        );
        assert_eq!(hw.ring_va(), 0, "the ring is not the engine's until RUN");
        assert_ne!(inner.dma_pos_va, 0);
        // The BDL: one entry per segment, contiguous over the ring, no IOC.
        let n_seg = inner.ring_len / BDL_SEGMENT;
        for i in 0..n_seg {
            let e = inner.bdl_pa + i * 16;
            assert_eq!(
                peek::<u64>(e),
                (inner.ring_va + i * BDL_SEGMENT) as u64,
                "entry {}",
                i
            );
            assert_eq!(peek::<u32>(e + 8), BDL_SEGMENT as u32);
            assert_eq!(peek::<u32>(e + 12), 0);
        }
        assert_eq!(inner.ring_len, RING_PAGES * PAGE_SIZE);
        assert_eq!(inner.streams.len(), 1, "the device's own stream");
        assert!(!inner.running);
    }

    #[test]
    fn probe_walks_the_codec_to_the_line_out_pin_and_arms_it() {
        let (hw, dev) = device();
        let inner = dev.inner.lock();
        assert_eq!(inner.afg, AFG);
        assert_eq!((inner.conv_nid, inner.pin_nid), (CONV, PIN));
        assert!(!inner.digital);
        assert_eq!(inner.candidates.len(), 1);
        let c = &inner.candidates[0];
        assert_eq!(
            (c.conv, c.pin, c.pin_conn_idx, c.hdmi_dp),
            (CONV, PIN, 0, false)
        );
        let verbs = hw.verbs();
        let sent = |v: u32| verbs.contains(&v);
        assert!(sent(verb12(0, VERB_GET_PARAMETER, PAR_VENDOR_ID)));
        assert!(sent(verb12(AFG, VERB_GET_PARAMETER, PAR_NODE_COUNT)));
        assert!(sent(verb12(PIN, VERB_GET_CONFIG_DEFAULT, 0)));
        assert!(sent(verb12(PIN, VERB_GET_CONN_LIST, 0)));
        // The path armed: power, routing, pin enabled, DAC amp unmuted at
        // 0 dB, and nothing digital.
        assert!(sent(verb12(AFG, VERB_SET_POWER_STATE, 0)));
        assert!(sent(verb12(CONV, VERB_SET_POWER_STATE, 0)));
        assert!(sent(verb12(PIN, VERB_SET_POWER_STATE, 0)));
        assert!(sent(verb12(PIN, VERB_SET_CONN_SELECT, 0)));
        assert!(sent(verb12(PIN, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN)));
        let unmuted_0db = (1 << 15) | (1 << 13) | (1 << 12) | (DAC_AMP_CAP & 0x7f);
        assert!(sent(verb4(CONV, 0x3, unmuted_0db)), "DAC amp");
        assert!(
            !sent(verb12(PIN, VERB_SET_EAPD, 0x2)),
            "no EAPD on this pin"
        );
        assert!(!sent(verb12(CONV, VERB_SET_DIGI_CVT1, 0x1)), "analog");
        assert_eq!(inner.stat_stale_resp, 0);
    }

    #[test]
    fn a_controller_with_no_output_descriptor_is_not_supported() {
        let hw = controller(ISS);
        hw.set_streams(ISS, 0);
        assert!(matches!(probe(&hw), Err(DeviceError::NotSupported)));
        assert!(hw.verbs().is_empty());
    }

    #[test]
    fn no_codec_answering_the_reset_is_a_clean_failure() {
        let hw = controller(ISS);
        hw.set_codec_absent();
        assert!(matches!(probe(&hw), Err(DeviceError::NoResources)));
        assert!(
            hw.verbs().is_empty(),
            "nothing is asked of a codec that is not there"
        );
    }

    #[test]
    fn a_codec_that_never_answers_fails_the_probe_after_one_timeout() {
        let hw = controller(ISS);
        hw.set_codec_dead();
        hw.set_read_tick_us(1_000);
        let t0 = test_clock::now();
        assert!(matches!(probe(&hw), Err(DeviceError::IoError)));
        assert_eq!(hw.verbs().len(), 1, "the first verb is the last");
        assert!(test_clock::now() - t0 < 2 * VERB_TIMEOUT_US);
    }

    #[test]
    fn a_pin_wired_to_nothing_leaves_no_path() {
        let hw = controller(ISS);
        // Port connectivity 01b: no physical connection.
        hw.answer(
            verb12(PIN, VERB_GET_CONFIG_DEFAULT, 0),
            PIN_DEFCFG_LINE_OUT_JACK | (0x1 << 30),
        );
        assert!(matches!(probe(&hw), Err(DeviceError::NotSupported)));
    }

    #[test]
    fn a_controller_refusing_the_position_buffer_is_paced_by_lpib_alone() {
        let hw = controller(ISS);
        hw.set_refuse_pos_buffer();
        let dev = probe(&hw).expect("probe");
        assert_eq!(dev.inner.lock().dma_pos_va, 0);
        dev.write(&constant(1000, HOST_BUFFER)).unwrap();
        assert!(dev.is_playing());
        for _ in 0..10 {
            tick(&hw, &dev);
        }
        let inner = dev.inner.lock();
        assert_eq!(inner.consumed, 10 * 768);
        assert!(!inner.dpib_trusted);
        assert_eq!(inner.stat_bad_pos, 0);
    }

    #[test]
    fn playback_runs_on_the_descriptor_after_the_input_ones() {
        let (hw, dev) = device();
        dev.write(&constant(1000, HOST_BUFFER)).unwrap();
        let sd = REG_SD_BASE + ISS * 0x20;
        assert_eq!(mmio_r32(hw.bar, sd + SD_CTL), (TAG << 20) | 0x2);
        assert_eq!(mmio_r32(hw.bar, sd + SD_CBL), hw.ring_len() as u32);
        assert_eq!(
            hw.ring_va(),
            dev.inner.lock().ring_va,
            "the BDL points at the ring"
        );
        assert_eq!(
            mmio_r32(hw.bar, REG_SD_BASE + SD_CTL),
            0,
            "input descriptor 0 untouched"
        );
        assert_eq!(hw.starts(), 1);
        tick(&hw, &dev);
        assert_eq!(&samples(&hw.played())[..4], &[1000; 4]);
    }

    #[test]
    fn each_open_is_a_stream_of_its_own_mixed_into_the_link() {
        let (hw, dev) = device();
        let a = dev.open_stream().unwrap().expect("a stream per open");
        let b = dev.open_stream().unwrap().expect("a stream per open");
        assert_eq!(dev.inner.lock().streams.len(), 3, "own + two opens");
        assert_eq!(
            a.write(&constant(20_000, HOST_BUFFER)).unwrap(),
            HOST_BUFFER
        );
        assert_eq!(
            b.write(&constant(-5_000, HOST_BUFFER)).unwrap(),
            HOST_BUFFER
        );
        assert!(a.is_playing() && b.is_playing());
        assert!(!dev.own.is_playing(), "the device's own stream has nothing");
        // The first write started the engine, which primed the ring from
        // the one stream it had; the second joined at the fill point.
        assert_eq!(a.buffer_bytes(), HOST_BUFFER);
        assert_eq!(a.free_bytes(), FILL_DEPTH);
        assert_eq!(a.queued_bytes(), HOST_BUFFER - FILL_DEPTH);
        assert_eq!(b.free_bytes(), 0);
        assert_eq!(b.queued_bytes(), HOST_BUFFER);
        for _ in 0..(FILL_DEPTH / 768 + 2) {
            tick(&hw, &dev);
        }
        let heard = samples(&hw.played());
        assert!(
            heard[..FILL_DEPTH / 2].iter().all(|&s| s == 20_000),
            "primed from a alone"
        );
        assert!(
            heard[FILL_DEPTH / 2..].iter().all(|&s| s == 15_000),
            "then the mix"
        );
        // Each stream's queue is its own, and the fill takes from both alike.
        assert_eq!(a.queued_bytes() + FILL_DEPTH, b.queued_bytes());
        assert_eq!(a.free_bytes() + a.queued_bytes(), a.buffer_bytes());
        // Delay is the queue plus what the ring holds ahead of the link.
        let inner = dev.inner.lock();
        let ahead = inner.ring_ahead();
        drop(inner);
        assert_eq!(a.delay_bytes(), a.queued_bytes() + ahead);
    }

    #[test]
    fn dropping_an_open_takes_its_stream_out_of_the_mix() {
        let (hw, dev) = device();
        let a = dev.open_stream().unwrap().unwrap();
        let b = dev.open_stream().unwrap().unwrap();
        a.write(&constant(20_000, HOST_BUFFER)).unwrap();
        b.write(&constant(-5_000, HOST_BUFFER)).unwrap();
        tick(&hw, &dev);
        drop(b);
        assert_eq!(dev.inner.lock().streams.len(), 2);
        let before = hw.played().len();
        for _ in 0..(FILL_DEPTH / 768 + 2) {
            tick(&hw, &dev);
        }
        let heard = samples(&hw.played()[before + FILL_DEPTH..]);
        assert!(heard.iter().all(|&s| s == 20_000), "{:?}", &heard[..4]);
        assert!(a.is_playing());
    }

    #[test]
    fn a_start_hold_fills_the_stream_without_starting_the_engine() {
        let (hw, dev) = device();
        dev.set_start_hold(true).unwrap();
        assert_eq!(
            dev.write(&constant(1000, HOST_BUFFER)).unwrap(),
            HOST_BUFFER
        );
        assert!(!dev.is_playing());
        assert_eq!(hw.starts(), 0);
        assert_eq!(dev.queued_bytes(), HOST_BUFFER, "the PCM is kept");
        dev.set_start_hold(false).unwrap();
        assert!(dev.is_playing());
        assert_eq!(hw.starts(), 1);
        tick(&hw, &dev);
        assert!(samples(&hw.played()).iter().all(|&s| s == 1000));
    }

    #[test]
    fn the_client_rate_is_resampled_and_the_link_stays_at_48k() {
        let (hw, dev) = device();
        let s = dev.open_stream().unwrap().unwrap();
        assert_eq!(s.set_params(44_100, 2).unwrap(), (44_100, 2));
        assert_eq!(s.params(), (44_100, 2));
        assert!(
            s.buffer_bytes() < HOST_BUFFER,
            "the client's buffer is in its own frames"
        );
        let free = s.free_bytes();
        assert_eq!(
            free + s.queued_bytes(),
            s.buffer_bytes(),
            "avail == accept, before"
        );
        let n = s.write(&constant(8_000, HOST_BUFFER)).unwrap();
        assert_eq!(n, free, "a write takes exactly what free said");
        assert_eq!(
            s.free_bytes() + s.queued_bytes(),
            s.buffer_bytes(),
            "and after"
        );
        assert_eq!(
            mmio_r16(hw.bar, hw.sd_base() + SD_FMT),
            stream_format(LINK_RATE, 2),
            "the link is not reprogrammed for the client"
        );
        for _ in 0..12 {
            tick(&hw, &dev);
        }
        let heard = samples(&hw.played());
        // A DC signal through the converter is the same DC, once it settles.
        let tail = &heard[heard.len() - 512..];
        assert!(
            tail.iter().all(|&v| (v - 8_000).abs() <= 2),
            "{:?}",
            &tail[..8]
        );
    }

    #[test]
    fn the_gain_control_reads_back_and_scales_the_link() {
        let (hw, dev) = device();
        assert_eq!(dev.gain(), (100, 100, false, false));
        dev.set_gain(50, 50, false, false).unwrap();
        assert_eq!(dev.gain(), (50, 50, false, false));
        dev.write(&constant(10_000, HOST_BUFFER)).unwrap();
        for _ in 0..(FILL_DEPTH / 768 + 12) {
            tick(&hw, &dev);
        }
        let heard = samples(&hw.played());
        let tail = &heard[heard.len() - 64..];
        assert!(
            tail.iter().all(|&v| (v - 5_000).abs() <= 1),
            "{:?}",
            &tail[..4]
        );
    }

    #[test]
    fn diagnostics_render_against_the_live_controller() {
        let (hw, dev) = device();
        dev.write(&constant(1000, HOST_BUFFER)).unwrap();
        tick(&hw, &dev);
        let text = dev.diagnostics();
        for needle in [
            "controller: GCAP 0x4401",
            "ring: running=true",
            "stream 0: client=48000 Hz src=passthrough",
            "events: 0 drains, 0 underruns",
            "active path: converter 0x2 -> pin 0x3 (analog)",
            "codec link: 0 stale RIRB responses",
        ] {
            assert!(text.contains(needle), "missing {:?} in:\n{}", needle, text);
        }
        assert!(!text.contains("codec reads cut short"));
    }
}
