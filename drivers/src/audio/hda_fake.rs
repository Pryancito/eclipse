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
    sd_base: usize,
    corb_va: usize,
    corb_entries: usize,
    rirb_va: usize,
    rirb_entries: usize,
    ring_va: usize,
    ring_len: usize,
    /// This stream's entry in the DMA position buffer.
    pos_va: usize,

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
        REG_CORBWP => {
            let wp = v as usize % st.corb_entries;
            let verb: u32 = peek(st.corb_va + wp * 4);
            st.verbs.push(verb);
            if st.codec_dead {
                return;
            }
            for _ in 0..st.unsolicited_with_next {
                post_rirb(st, 0, 1 << 4);
            }
            st.unsolicited_with_next = 0;
            let resp = st.answers.get(&verb).copied().unwrap_or(0);
            post_rirb(st, resp, 0);
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
                poke::<u32>(st.pos_va, 0);
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

/// The test's handle on the controller: addresses, and the engine's
/// controls.
#[derive(Clone, Copy)]
pub(super) struct Hw {
    pub bar: usize,
    pub ring_va: usize,
    pub ring_len: usize,
    pub bdl_pa: usize,
}

impl Hw {
    fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        with_state(self.bar, f).expect("fake controller installed on this thread")
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
                if !st.pos_buffer_dead {
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
        unsafe { core::slice::from_raw_parts(self.ring_va as *const u8, self.ring_len) }.to_vec()
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
    test_clock::set(0);
    let (bar, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let (corb_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let (rirb_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let ring_len = RING_PAGES * PAGE_SIZE;
    let (ring_va, _) = ProviderImpl::alloc_dma(ring_len);
    let (bdl_va, bdl_pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let (pos_va, _) = ProviderImpl::alloc_dma(PAGE_SIZE);
    let sd_base = REG_SD_BASE;
    FAKE.with(|cell| {
        *cell.borrow_mut() = Some(FakeState {
            bar,
            sd_base,
            corb_va,
            corb_entries: 256,
            rirb_va,
            rirb_entries: 256,
            ring_va,
            ring_len,
            pos_va,
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
    let _ = bdl_va;
    (
        Hw {
            bar,
            ring_va,
            ring_len,
            bdl_pa,
        },
        inner,
    )
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
        assert_eq!(mmio_r32(hw.bar, sd + SD_CBL), hw.ring_len as u32);
        assert_eq!(
            mmio_r16(hw.bar, sd + SD_LVI),
            (hw.ring_len / BDL_SEGMENT - 1) as u16
        );
        assert_eq!(mmio_r16(hw.bar, sd + SD_FMT), stream_format(LINK_RATE, 2));
        assert_eq!(mmio_r32(hw.bar, sd + SD_BDPL), hw.bdl_pa as u32);
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
        let target = hw.ring_len * 5 / 2;
        let mut n = 0;
        let mut straddled = 0;
        while hw.played().len() < target {
            ramp.feed(&mut inner, id);
            hw.run_for_us(3_700 + (n % 7) * 100);
            let before = inner.fill_pos;
            inner.poll_progress();
            let ring = hw.ring_len as u64;
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
            if hw.played().len() >= hw.ring_len * 3 / 2 {
                break;
            }
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
        }
        assert!(hw.played().len() >= hw.ring_len * 3 / 2);
        let ring = hw.ring();
        let at = |pos: u64| (pos % hw.ring_len as u64) as usize;
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
        hw.corrupt_next_lpib((hw.ring_len / 2) as u32);
        tick(&hw, &mut inner);
        assert_eq!(inner.stat_bad_pos, 1);
        assert_eq!(inner.last_bad_src, b'L');
        assert_eq!(inner.last_bad_pos, (hw.ring_len / 2) as u32);
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
        let most = hw.ring_len - RING_GUARD - 8192;
        let mut lead = 0;
        let mut bound = false;
        for _ in 0..120 {
            lead = (lead + 4096).min(most);
            hw.set_lead(lead);
            ramp.feed(&mut inner, id);
            tick(&hw, &mut inner);
            assert_eq!(inner.stat_bad_pos, 0, "the lead grows within the budget");
            let window = inner.consumed + (hw.ring_len - RING_GUARD) as u64;
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
        assert!(inner.lead_now > (hw.ring_len - RING_GUARD - FILL_DEPTH) as u64);
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
