//! Intel High Definition Audio controller + codec driver.
//!
//! Covers every PCI class-0x0403 HDA controller in one driver:
//!
//! * the PCH's onboard controller (00:1f.3 on the X299 board) with an analog
//!   codec behind it,
//! * the HDA function of the NVIDIA GPU that drives the monitor (`xx:00.1`).
//!   Extra GPUs on a dual-board are left unbound (Linux-style: Intel PCH +
//!   one HDMI card), and
//! * QEMU's `-device intel-hda -device hda-output`, which is how this driver
//!   is exercised in emulation.
//!
//! Design notes:
//!
//! * **Polled, no interrupts.** Codec verbs go through the CORB/RIRB DMA rings
//!   with polled responses (the immediate-command registers are optional in
//!   the spec and absent on some controllers). Playback progress is read from
//!   the stream's LPIB register. This mirrors the NVMe driver's philosophy:
//!   fewer moving parts on real hardware we cannot debug interactively.
//! * **One output stream, cyclic ring.** A physically contiguous PCM ring is
//!   described by a Buffer Descriptor List once; the hardware loops it
//!   forever while RUN is set. [`AudioScheme::write`] copies into the ring
//!   ahead of the DMA position; consumed regions are re-zeroed behind it so
//!   an underrun plays silence instead of looping stale audio. A 4 ms ALSA
//!   watchdog (`arm_playback_watchdog`) keeps polling LPIB after clients
//!   cork, so RUN cannot stay set with a leftover fragment.
//! * **HDMI vs analog is a codec-graph decision.** After the widget walk the
//!   driver prefers a connected (presence-detect) HDMI/DP pin; analog
//!   line-out/speaker/HP pins are the fallback. Both paths share the same
//!   converter setup; the HDMI path additionally enables the digital
//!   converter, channel count and audio infoframe (DIP) verbs.
//!
//! On NVIDIA GPUs the HDMI audio packets only reach the display if the
//! display engine is scanning out with audio enabled for that head (the
//! GSP-RM display path owns that); the pin's ELD-valid bit is logged at
//! bring-up so a silent output can be told apart from a stream that never
//! started.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use lock::Mutex;
use pci::{PCIDevice, BAR};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::nvme::nvme_queue::{timer_now_as_micros, Provider, ProviderImpl, PAGE_SIZE};
use crate::scheme::{AudioScheme, Scheme};
use crate::{Device, DeviceError, DeviceResult};

// ── Controller MMIO registers (offsets from BAR0) ───────────────────────────
const REG_GCAP: usize = 0x00; // u16: global capabilities
const REG_GCTL: usize = 0x08; // u32: global control (bit0 = CRST)
const REG_STATESTS: usize = 0x0e; // u16: codec presence bitmap after reset
const REG_INTCTL: usize = 0x20; // u32: interrupt control (kept 0 — polled)
const REG_CORBLBASE: usize = 0x40;
const REG_CORBUBASE: usize = 0x44;
const REG_CORBWP: usize = 0x48; // u16: write pointer (low byte)
const REG_CORBRP: usize = 0x4a; // u16: read pointer, bit15 = reset
const REG_CORBCTL: usize = 0x4c; // u8: bit1 = DMA run
const REG_CORBSIZE: usize = 0x4e; // u8
const REG_RIRBLBASE: usize = 0x50;
const REG_RIRBUBASE: usize = 0x54;
const REG_RIRBWP: usize = 0x58; // u16: write pointer, write bit15 to reset
const REG_RINTCNT: usize = 0x5a; // u16
const REG_RIRBCTL: usize = 0x5c; // u8: bit0 = IRQ_EN, bit1 = DMA run
const REG_RIRBSTS: usize = 0x5d; // u8: write-1-to-clear IRQ/overrun
const REG_RIRBSIZE: usize = 0x5e; // u8
const RIRBCTL_IRQ_EN: u8 = 1 << 0;
const RIRBCTL_DMA_EN: u8 = 1 << 1;
const RIRBSTS_IRQ: u8 = 1 << 0;
const RIRBSTS_OVERRUN: u8 = 1 << 2;
/// DMA Position Buffer base (low/high). The controller writes each stream's
/// current position into host memory here, 8 bytes per stream descriptor,
/// indexed by descriptor number. Bit 0 of the low register enables it.
const REG_DPLBASE: usize = 0x70;
const REG_DPUBASE: usize = 0x74;
const DPLBASE_ENABLE: u32 = 1 << 0;

const REG_SD_BASE: usize = 0x80; // stream descriptors, 0x20 bytes each

// Stream descriptor register offsets (from the descriptor base).
const SD_CTL: usize = 0x00; // u32 (24-bit): bit0 SRST, bit1 RUN, [23:20] tag
const SD_LPIB: usize = 0x04; // u32: link position in cyclic buffer
const SD_CBL: usize = 0x08; // u32: cyclic buffer length
const SD_LVI: usize = 0x0c; // u16: last valid BDL index
const SD_FMT: usize = 0x12; // u16: stream format
const SD_BDPL: usize = 0x18; // u32: BDL base low
const SD_BDPU: usize = 0x1c; // u32: BDL base high

const GCTL_CRST: u32 = 1 << 0;

// ── Codec verbs ─────────────────────────────────────────────────────────────
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
// Read-back verbs, used only by `diagnostics`: silence with no error can only
// be diagnosed by asking the codec what it actually has programmed.
const VERB_GET_PIN_CTL: u32 = 0xf07;
const VERB_GET_STREAM_ID: u32 = 0xf06;
const VERB_GET_DIGI_CVT: u32 = 0xf0d;
const VERB_GET_EAPD: u32 = 0xf0c;
const VERB_GET_POWER_STATE: u32 = 0xf05;
const VERB_GET_CVT_FORMAT: u32 = 0xa;
const VERB_SET_DIP_INDEX: u32 = 0x730;
const VERB_SET_DIP_DATA: u32 = 0x731;
const VERB_SET_DIP_XMIT: u32 = 0x732;
/// AC_VERB_SET_CVT_CHAN_COUNT. 0x733 is SET_HDMI_CP_CTRL — do not mix them.
const VERB_SET_CVT_CHAN_COUNT: u32 = 0x72d;
const VERB_SET_HDMI_CHAN_SLOT: u32 = 0x734;

// GET_PARAMETER ids.
const PAR_VENDOR_ID: u32 = 0x00;
const PAR_NODE_COUNT: u32 = 0x04;
const PAR_FUNCTION_TYPE: u32 = 0x05;
const PAR_AUDIO_WIDGET_CAP: u32 = 0x09;
const PAR_PIN_CAP: u32 = 0x0c;
const PAR_CONN_LIST_LEN: u32 = 0x0e;
const PAR_OUT_AMP_CAP: u32 = 0x12;

// Widget types (audio widget caps [23:20]).
const WIDGET_AUDIO_OUT: u32 = 0x0;
const WIDGET_PIN: u32 = 0x4;

const PIN_CTL_OUT_EN: u32 = 0x40;

/// Time to wait for one codec verb response.
const VERB_TIMEOUT_US: u64 = 200_000;

/// PCM ring: 32 pages = 128 KiB (683 ms of 48 kHz S16LE stereo).
const RING_PAGES: usize = 32;
/// The BDL splits the ring into fixed 16 KiB cyclic segments.
const BDL_SEGMENT: usize = 16 * 1024;
/// Keep the software write pointer at least this far behind the reported play
/// position.
///
/// 512 bytes -- 2.7 ms at 48 kHz stereo -- was too thin to be a safety margin.
/// It assumed the position register is exact, which is true of QEMU's emulated
/// controller and famously not true of real ones: Linux carries a whole
/// `position_fix` quirk table for controllers whose `SD_LPIB` does not track
/// what has actually been played, and NVIDIA's HDMI audio function is on it.
/// When the position over-reports, `free_bytes` hands the writer space that is
/// still going to be played and the next write overwrites it -- heard as a
/// stream of very short dropouts rather than one obvious gap.
///
/// 4 KiB is 21 ms of slack and costs 3% of the ring.
const RING_GUARD: usize = 4096;

/// Minimum interval between reads of the stream position register.
///
/// `SD_LPIB` is uncached device memory: an uncached PCIe read on real
/// hardware, and a VM exit under hardware virtualisation. The `/dev/dsp`
/// write path spin-retries against a full ring, so every one of those
/// retries used to read it -- measured at 1.5 M reads (and 1.2 M `write`
/// calls, each taking this IRQ-off lock) for a single 3-second tone, i.e.
/// ~500 k device reads per second for 192 KiB/s of audio. That is a bus and
/// exit storm that competes with the very DMA it is watching.
///
/// The ring holds 680 ms; resolving the play position to a quarter of a
/// millisecond (48 bytes of PCM) is far finer than anything above needs.
const LPIB_POLL_MIN_US: u64 = 250;

/// Slack allowed on top of what the PCM byte rate makes possible when judging
/// whether a position read is believable: the controller updates its position
/// in bursts, and the read itself is not instantaneous. One guard's worth
/// (21 ms at 48 kHz stereo) is far more than any real burst and far less than
/// the ring, so a garbage read cannot pass as progress.
const POS_SLACK: usize = RING_GUARD;

const STOP_HISTORY: usize = 4;

/// One stream stop, as recorded for `/proc/gpusnd`.
#[derive(Clone, Copy, Default)]
struct StopEvent {
    /// `b'U'` underrun, `b'D'` drain, 0 = unused slot.
    kind: u8,
    /// Milliseconds after the stream started.
    at_ms: u64,
    /// Bytes written to that stream by then.
    written: usize,
}

/// Q15 multiplier for a 0..=100 percent. 100% is 1.0 (32768) so a shift-15
/// multiply is a no-op; mute/0% is silence.
fn gain_q15(percent: u8, mute: bool) -> i32 {
    if mute || percent == 0 {
        0
    } else if percent >= 100 {
        32768
    } else {
        percent as i32 * 32768 / 100
    }
}

fn scale_q15(sample: i16, gain: i32) -> i16 {
    if gain == 32768 {
        sample
    } else {
        ((sample as i32 * gain) >> 15) as i16
    }
}

// ── MMIO helpers ────────────────────────────────────────────────────────────
fn mmio_r8(bar: usize, off: usize) -> u8 {
    unsafe { read_volatile((bar + off) as *const u8) }
}
fn mmio_r16(bar: usize, off: usize) -> u16 {
    unsafe { read_volatile((bar + off) as *const u16) }
}
fn mmio_r32(bar: usize, off: usize) -> u32 {
    unsafe { read_volatile((bar + off) as *const u32) }
}
fn mmio_w8(bar: usize, off: usize, v: u8) {
    unsafe { write_volatile((bar + off) as *mut u8, v) }
}
fn mmio_w16(bar: usize, off: usize, v: u16) {
    unsafe { write_volatile((bar + off) as *mut u16, v) }
}
fn mmio_w32(bar: usize, off: usize, v: u32) {
    unsafe { write_volatile((bar + off) as *mut u32, v) }
}

fn clflush_range(vaddr: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_clflush, _mm_mfence};
        let line = 64;
        let start = vaddr & !(line - 1);
        let end = (vaddr + len + line - 1) & !(line - 1);
        unsafe {
            _mm_mfence();
            for a in (start..end).step_by(line) {
                _mm_clflush(a as *const u8);
            }
            _mm_mfence();
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (vaddr, len);
}

fn wait_us(us: u64) {
    let start = timer_now_as_micros();
    while timer_now_as_micros().wrapping_sub(start) < us {
        core::hint::spin_loop();
    }
}

// ── Driver state ────────────────────────────────────────────────────────────
struct HdaInner {
    bar: usize,

    // CORB/RIRB rings.
    corb_va: usize,
    corb_entries: usize,
    rirb_va: usize,
    rirb_entries: usize,
    rirb_rp: usize,

    /// Codec address answering our verbs.
    cad: u32,

    // Chosen output path.
    conv_nid: u32,
    pin_nid: u32,
    digital: bool,
    /// Audio function group node (for re-routing).
    afg: u32,
    /// Every viable pin -> converter pair found by the widget walk. The
    /// choice among them is re-evaluated at stream start: on NVIDIA GPUs
    /// presence/ELD appear on the pins only once the display driver pushes
    /// the monitor's ELD (long after this driver's PCI probe).
    candidates: Vec<OutPath>,

    // Output stream descriptor.
    sd_base: usize,
    stream_tag: u32,

    // PCM ring.
    ring_va: usize,
    ring_len: usize,
    bdl_pa: usize,

    // Software state.
    running: bool,
    /// Software pause: DMA stopped but the ring still holds queued PCM.
    paused: bool,
    /// Next byte to write, offset into the ring.
    wp: usize,
    /// Bytes written and not yet played. Derived from the hardware play
    /// position on every poll (see [`HdaInner::queued_at`]), never accumulated.
    queued: usize,
    /// LPIB at the last progress poll. Only underrun detection reads it: it is
    /// the one question that needs a delta rather than a position.
    last_lpib: u32,
    /// `timer_now_as_micros()` at the last poll that actually read LPIB.
    /// Throttles that read to [`LPIB_POLL_MIN_US`].
    last_poll_us: u64,
    /// Kernel address of this stream's entry in the DMA position buffer, or 0
    /// when the controller would not take one. See [`HdaInner::play_pos`].
    dma_pos_va: usize,
    /// Position-buffer value at the last poll, and whether it has ever been
    /// seen to advance. An entry that never moves means the controller is not
    /// maintaining it, and it must not be allowed to stall playback.
    last_dpib: u32,
    dpib_trusted: bool,
    /// Ring offset up to which consumed data has been re-zeroed.
    zero_ptr: usize,

    /// Counters behind the `/proc/gpusnd` "events" line. A tone that plays
    /// with short repeated dropouts sounds the same whatever produced it;
    /// these say which of them actually happened on this machine.
    /// `drains` is the ring running dry with the writer still feeding it,
    /// `underruns` the engine lapping the writer, `restarts` the full stream
    /// reset (codec verbs and all) that either one costs on the next write,
    /// and `stop_timeouts` the engine not acknowledging a cleared RUN bit --
    /// the case where wiping the ring would race a still-fetching DMA.
    stat_drains: u64,
    stat_underruns: u64,
    stat_restarts: u64,
    stat_stop_timeouts: u64,
    /// Position reads rejected as impossible: more progress than the PCM
    /// byte rate allows in the time since the last accepted read. A stream
    /// whose position register or buffer occasionally returns garbage --
    /// a controller behind a GPU, a bus hiccup -- would otherwise have the
    /// writer overwrite most of a lap of unplayed audio, or be declared
    /// underrun, on one bad read. The last rejected value is kept.
    stat_bad_pos: u64,
    last_bad_pos: u32,
    /// When the current stream started and how much has been written to it,
    /// so a stop can be placed within the stream it ended.
    stream_start_us: u64,
    stream_written: usize,
    /// The last few stops, newest last: WHEN in the stream and after how
    /// much data, which is what tells a stop mid-tone from the tone ending.
    stops: [StopEvent; STOP_HISTORY],

    rate: u32,
    channels: u8,

    /// Software playback gain (HDMI has no analog volume). Applied while
    /// copying S16LE into the ring so OSS and ALSA share one control.
    gain_l: u8,
    gain_r: u8,
    mute_l: bool,
    mute_r: bool,
}

pub struct HdaDevice {
    name: String,
    /// PCI vendor is NVIDIA: this HDA function lives on a GPU, so its audio
    /// only reaches the cable when the display engine transmits it.
    is_nvidia: bool,
    inner: Mutex<HdaInner>,
}

impl HdaInner {
    /// QEMU's intel-hda pauses CORB DMA once `rirb_count == RINTCNT` and only
    /// resumes when the guest write-1-clears `RIRBSTS.IRQ`. Hardware does not
    /// stall that way, but the ack is the documented Linux path either way.
    /// Without it, the first codec verb succeeds and every later one times out
    /// — probe fails, `/dev/snd` is never created, `aplay -l` is empty.
    fn ack_rirb(&self) {
        mmio_w8(self.bar, REG_RIRBSTS, RIRBSTS_IRQ | RIRBSTS_OVERRUN);
    }

    // ── Codec verb transport (CORB/RIRB, polled) ────────────────────────────
    fn corb_cmd(&mut self, verb: u32) -> DeviceResult<u32> {
        let bar = self.bar;
        // Unstick a previous RINTCNT stall before ringing the doorbell.
        self.ack_rirb();
        let wp = (mmio_r16(bar, REG_CORBWP) as usize + 1) % self.corb_entries;
        unsafe { write_volatile((self.corb_va + wp * 4) as *mut u32, verb) };
        clflush_range(self.corb_va + wp * 4, 4);
        fence(Ordering::SeqCst);
        mmio_w16(bar, REG_CORBWP, wp as u16);

        let start = timer_now_as_micros();
        loop {
            let hw_wp = (mmio_r16(bar, REG_RIRBWP) & 0xff) as usize % self.rirb_entries;
            while self.rirb_rp != hw_wp {
                self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
                let entry_va = self.rirb_va + self.rirb_rp * 8;
                clflush_range(entry_va, 8);
                let resp = unsafe { read_volatile(entry_va as *const u32) };
                let ext = unsafe { read_volatile((entry_va + 4) as *const u32) };
                if ext & (1 << 4) != 0 {
                    // Unsolicited response (jack events) — not ours, keep going.
                    self.ack_rirb();
                    continue;
                }
                self.ack_rirb();
                return Ok(resp);
            }
            if timer_now_as_micros().wrapping_sub(start) > VERB_TIMEOUT_US {
                error!("[hda] verb {:#010x} timed out", verb);
                self.ack_rirb();
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
    }

    /// 12-bit verb with an 8-bit payload.
    fn cmd(&mut self, nid: u32, verb: u32, payload: u32) -> DeviceResult<u32> {
        self.corb_cmd((self.cad << 28) | (nid << 20) | (verb << 8) | (payload & 0xff))
    }

    /// 4-bit verb with a 16-bit payload (converter format / amp gain).
    fn cmd16(&mut self, nid: u32, verb: u32, payload: u32) -> DeviceResult<u32> {
        self.corb_cmd((self.cad << 28) | (nid << 20) | (verb << 16) | (payload & 0xffff))
    }

    fn param(&mut self, nid: u32, par: u32) -> DeviceResult<u32> {
        self.cmd(nid, VERB_GET_PARAMETER, par)
    }

    // ── Playback ring bookkeeping ───────────────────────────────────────────
    fn lpib(&self) -> u32 {
        mmio_r32(self.bar, self.sd_base + SD_LPIB)
    }

    /// This stream's position as the controller reports it in the DMA position
    /// buffer, or `None` when there is no buffer.
    ///
    /// The controller writes this with DMA, so the line has to come from
    /// memory rather than from a cache that a non-snooping write left stale --
    /// the same reason every ring write in this driver is flushed.
    fn dma_pos(&self) -> Option<u32> {
        if self.dma_pos_va == 0 {
            return None;
        }
        clflush_range(self.dma_pos_va, 8);
        // SAFETY: `dma_pos_va` addresses this stream's 8-byte entry inside a
        // page owned for the lifetime of the device.
        Some(unsafe { core::ptr::read_volatile(self.dma_pos_va as *const u32) })
    }

    /// Bytes still in the ring ahead of play position `pos`: the queue depth
    /// implied by where the engine has got to and where the writer has.
    ///
    /// Queue depth is a relation between two positions, never a running total.
    /// Accumulating it -- subtracting a per-poll delta from a counter -- makes
    /// every error permanent, and one particular way of producing that error
    /// is what this replaces: taking the SMALLER of what the two position
    /// sources claimed since the last poll. The two do not advance in
    /// lockstep on real hardware. `SD_LPIB` moves continuously while the
    /// controller refreshes the position buffer in bursts, so on every poll
    /// that falls between two refreshes the buffer reports no progress at all
    /// and the minimum throws away everything LPIB saw. QEMU updates both in
    /// lockstep, which is why that never showed there and bit only on real
    /// hardware: `queued` inflates at a large fraction of the playback rate,
    /// eats the ring's free space within seconds, starves the writer, and
    /// comes out as a stream of very short dropouts.
    ///
    /// Recomputing from positions is self-correcting instead: a poll that
    /// reads a coarse or stale position is wrong only until the next one.
    fn queued_at(&self, pos: usize) -> usize {
        let ring = self.ring_len;
        (self.wp + ring - pos % ring) % ring
    }

    fn record_stop(&mut self, kind: u8) {
        self.stops.copy_within(1.., 0);
        self.stops[STOP_HISTORY - 1] = StopEvent {
            kind,
            at_ms: timer_now_as_micros().wrapping_sub(self.stream_start_us) / 1000,
            written: self.stream_written,
        };
    }

    /// Fold DMA progress since the last poll into `queued`, detect underrun,
    /// and re-zero consumed ring space (so an underrun loops silence).
    fn poll_progress(&mut self) {
        if !self.running {
            return;
        }
        if self.queued == 0 {
            self.stat_drains += 1;
            self.record_stop(b'D');
            if self.stop_stream() {
                self.silence_ring();
            }
            return;
        }
        // Rate-limit the device read (see [`LPIB_POLL_MIN_US`]). Only the
        // "still playing" path is throttled: the drain/stop decisions above
        // and below run on every call, so nothing is deferred that could
        // leave the stream running with an empty ring.
        let now_us = timer_now_as_micros();
        let dt_us = now_us.wrapping_sub(self.last_poll_us);
        if dt_us < LPIB_POLL_MIN_US {
            return;
        }
        let ring = self.ring_len;
        // The most the engine can have advanced since the last ACCEPTED read.
        // `last_poll_us` only moves on an accepted read, so a run of bad
        // reads keeps widening the budget until a sane one gets through.
        let rate_bytes = self.rate as u64 * self.frame_bytes() as u64;
        let max_advance = (dt_us.saturating_mul(rate_bytes) / 1_000_000) as usize + POS_SLACK;
        let lpib_raw = self.lpib();
        let lpib = lpib_raw as usize % ring;
        // Underrun is a question about progress, so that one really is a
        // delta: the engine having travelled further than we ever queued means
        // it has lapped the writer and is replaying the ring.
        let consumed = (lpib + ring - self.last_lpib as usize % ring) % ring;
        if consumed > max_advance {
            // Impossible progress (a backwards step shows up here too, as
            // nearly a full lap forward). Garbage: keep everything as it was.
            self.stat_bad_pos += 1;
            self.last_bad_pos = lpib_raw;
            return;
        }
        self.last_poll_us = now_us;
        self.last_lpib = lpib as u32;
        if consumed >= self.queued && consumed > 0 {
            // The engine ran past everything we queued: underrun. Stop and
            // wipe the ring so a looping DMA never replays stale samples.
            self.queued = 0;
            self.stat_underruns += 1;
            self.record_stop(b'U');
            if self.stop_stream() {
                self.silence_ring();
            }
            return;
        }

        // Queue depth, taken from the hardware rather than accumulated. Where
        // the two sources disagree, believe the one reporting LESS played:
        // over-reporting is what hands the writer space that is still going to
        // come out of the speakers.
        let mut queued = self.queued_at(lpib);
        if let Some(dpib_raw) = self.dma_pos() {
            let dpib = dpib_raw as usize % ring;
            let advanced = (dpib + ring - self.last_dpib as usize % ring) % ring;
            if advanced > max_advance {
                // Same test as LPIB, same verdict: this read is not progress.
                self.stat_bad_pos += 1;
                self.last_bad_pos = dpib_raw;
            } else {
                if advanced > 0 {
                    // Only believed once seen to move: a controller that takes
                    // the base address and never writes to it would otherwise
                    // pin the depth at a full ring and stall playback outright.
                    self.dpib_trusted = true;
                }
                self.last_dpib = dpib as u32;
                if self.dpib_trusted {
                    queued = queued.max(self.queued_at(dpib));
                }
            }
        }
        // A lapped or garbage position must not underflow `free_bytes`.
        self.queued = queued.min(ring - RING_GUARD);
        if self.queued == 0 {
            self.stat_drains += 1;
            self.record_stop(b'D');
            if self.stop_stream() {
                self.silence_ring();
            }
            return;
        }

        // Re-zero what the engine has consumed, so that a writer that stops
        // feeding leaves silence, not a stale lap, ahead of the playhead.
        //
        // The played span runs forward from `wp` to the play position, and
        // only the part of it that ends RING_GUARD short of that position is
        // touched. That is the invariant the previous arithmetic claimed and
        // did not keep: once `zero_ptr` sat inside the guard, a modular
        // subtraction wrapped to nearly a full ring and the clamp let through
        // exactly enough to silence up to `wp + RING_GUARD` -- with a full
        // ring, that is the playhead minus one poll's lag. Zeros were being
        // written right at the position the engine was fetching, thousands
        // of times a second. With a full ring the played span IS the guard,
        // and nothing here runs.
        let played = self.ring_len - self.queued;
        let zeroable = played.saturating_sub(RING_GUARD);
        let off = (self.zero_ptr + self.ring_len - self.wp) % self.ring_len;
        if off >= played {
            // The writer has overtaken the zero pointer: everything from here
            // on is queued audio. Start again from the oldest played byte.
            self.zero_ptr = self.wp;
            return;
        }
        if off >= zeroable {
            return;
        }
        let mut p = self.zero_ptr;
        let mut left = zeroable - off;
        while left > 0 {
            let chunk = left.min(self.ring_len - p);
            unsafe { core::ptr::write_bytes((self.ring_va + p) as *mut u8, 0, chunk) };
            clflush_range(self.ring_va + p, chunk);
            p = (p + chunk) % self.ring_len;
            left -= chunk;
        }
        self.zero_ptr = p;
    }

    /// Copy `src` into the ring at `dst` (virtual address), applying the
    /// current stereo gain. `len` is a whole number of S16LE stereo frames
    /// (the write path never splits a frame across the ring wrap).
    fn copy_pcm_scaled(&self, src: &[u8], dst: usize, len: usize) {
        let gl = gain_q15(self.gain_l, self.mute_l);
        let gr = gain_q15(self.gain_r, self.mute_r);
        if gl == 32768 && gr == 32768 {
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, len);
            }
            return;
        }
        if gl == 0 && gr == 0 {
            unsafe { core::ptr::write_bytes(dst as *mut u8, 0, len) };
            return;
        }
        let dstp = dst as *mut u8;
        let mut i = 0;
        while i + 4 <= len {
            let l = i16::from_le_bytes([src[i], src[i + 1]]);
            let r = i16::from_le_bytes([src[i + 2], src[i + 3]]);
            let lo = scale_q15(l, gl).to_le_bytes();
            let ro = scale_q15(r, gr).to_le_bytes();
            unsafe {
                *dstp.add(i) = lo[0];
                *dstp.add(i + 1) = lo[1];
                *dstp.add(i + 2) = ro[0];
                *dstp.add(i + 3) = ro[1];
            }
            i += 4;
        }
    }

    fn free_bytes(&self) -> usize {
        self.ring_len - RING_GUARD - self.queued
    }

    /// Clear RUN and wait for the engine to acknowledge it. Returns whether it
    /// did: a caller that is about to rewrite the ring must not do so while
    /// the engine is still fetching from it.
    fn stop_stream(&mut self) -> bool {
        let ctl = mmio_r32(self.bar, self.sd_base + SD_CTL);
        mmio_w32(self.bar, self.sd_base + SD_CTL, ctl & !0x2);
        // Wait for RUN to actually drop: HDMI controllers keep fetching for
        // a few frames after the bit clears. Silencing the ring before that
        // races the engine and leaves a looping fragment.
        let t = timer_now_as_micros();
        let mut stopped = true;
        while mmio_r32(self.bar, self.sd_base + SD_CTL) & 0x2 != 0 {
            if timer_now_as_micros().wrapping_sub(t) > 10_000 {
                stopped = false;
                break;
            }
            core::hint::spin_loop();
        }
        if !stopped {
            self.stat_stop_timeouts += 1;
        }
        self.running = false;
        stopped
    }

    fn silence_ring(&mut self) {
        unsafe { core::ptr::write_bytes(self.ring_va as *mut u8, 0, self.ring_len) };
        clflush_range(self.ring_va, self.ring_len);
        self.zero_ptr = 0;
    }

    fn zero_range(&mut self, start: usize, len: usize) {
        if len == 0 || self.ring_len == 0 {
            return;
        }
        let mut p = start % self.ring_len;
        let mut left = len.min(self.ring_len);
        while left > 0 {
            let chunk = left.min(self.ring_len - p);
            unsafe { core::ptr::write_bytes((self.ring_va + p) as *mut u8, 0, chunk) };
            clflush_range(self.ring_va + p, chunk);
            p = (p + chunk) % self.ring_len;
            left -= chunk;
        }
    }

    fn frame_bytes(&self) -> usize {
        (self.channels as usize).max(1) * 2
    }

    /// Drop the most recently queued `bytes` (whole frames, ≤ queued) and
    /// silence that tail so a looping DMA cannot replay it.
    fn rewind_bytes(&mut self, bytes: usize) -> usize {
        self.poll_progress();
        let frame = self.frame_bytes();
        let n = bytes.min(self.queued) / frame * frame;
        if n == 0 {
            return 0;
        }
        self.wp = (self.wp + self.ring_len - n) % self.ring_len;
        self.queued -= n;
        self.zero_range(self.wp, n);
        if self.queued == 0 {
            let stopped = self.stop_stream();
            self.paused = false;
            if stopped {
                self.silence_ring();
            }
            self.wp = 0;
            self.zero_ptr = 0;
            self.last_lpib = 0;
        }
        n
    }

    /// Skip the next `bytes` of unplayed PCM (silence from the playhead).
    fn forward_bytes(&mut self, bytes: usize) -> usize {
        self.poll_progress();
        let frame = self.frame_bytes();
        let n = bytes.min(self.queued) / frame * frame;
        if n == 0 {
            return 0;
        }
        let start = if self.running {
            self.lpib() as usize % self.ring_len
        } else {
            (self.wp + self.ring_len - self.queued) % self.ring_len
        };
        self.zero_range(start, n);
        self.queued -= n;
        if self.queued == 0 {
            let stopped = self.stop_stream();
            self.paused = false;
            if stopped {
                self.silence_ring();
            }
            self.wp = 0;
            self.zero_ptr = 0;
            self.last_lpib = 0;
        }
        n
    }

    /// Stop DMA but keep the ring so resume continues from the same LPIB.
    fn pause_stream(&mut self) {
        self.poll_progress();
        self.stop_stream();
        self.paused = true;
    }

    /// Set RUN without resetting the stream descriptor (LPIB stays put).
    fn resume_stream(&mut self) -> DeviceResult {
        self.paused = false;
        if self.running || self.queued == 0 {
            return Ok(());
        }
        let ctl = mmio_r32(self.bar, self.sd_base + SD_CTL);
        mmio_w32(self.bar, self.sd_base + SD_CTL, ctl | 0x2);
        self.running = true;
        Ok(())
    }

    /// Reset + program the stream descriptor and start the DMA engine.
    fn start_stream(&mut self) -> DeviceResult {
        let bar = self.bar;
        let sd = self.sd_base;

        // Stream reset handshake.
        mmio_w32(bar, sd + SD_CTL, 0x1);
        let t = timer_now_as_micros();
        while mmio_r32(bar, sd + SD_CTL) & 0x1 == 0 {
            if timer_now_as_micros().wrapping_sub(t) > 100_000 {
                break;
            }
            core::hint::spin_loop();
        }
        mmio_w32(bar, sd + SD_CTL, 0x0);
        let t = timer_now_as_micros();
        while mmio_r32(bar, sd + SD_CTL) & 0x1 != 0 {
            if timer_now_as_micros().wrapping_sub(t) > 100_000 {
                break;
            }
            core::hint::spin_loop();
        }

        let fmt = stream_format(self.rate, self.channels);
        mmio_w32(bar, sd + SD_BDPL, self.bdl_pa as u32);
        mmio_w32(bar, sd + SD_BDPU, (self.bdl_pa as u64 >> 32) as u32);
        mmio_w32(bar, sd + SD_CBL, self.ring_len as u32);
        mmio_w16(bar, sd + SD_LVI, (self.ring_len / BDL_SEGMENT - 1) as u16);
        mmio_w16(bar, sd + SD_FMT, fmt);

        // The converter must agree with the descriptor on format and stream.
        let conv = self.conv_nid;
        self.cmd16(conv, 0x2, fmt as u32)?;
        self.cmd(conv, VERB_SET_STREAM_ID, self.stream_tag << 4)?;
        if self.digital {
            self.setup_digital_converter(self.channels);
        }

        fence(Ordering::SeqCst);
        // Tag + RUN.
        mmio_w32(bar, sd + SD_CTL, (self.stream_tag << 20) | 0x2);

        self.last_lpib = 0;
        // The plausibility budget starts counting from here, not from 0.
        self.last_poll_us = timer_now_as_micros();
        // A stream reset restarts the position buffer at 0 too, and the fresh
        // stream has to re-earn trust in it.
        self.last_dpib = 0;
        self.dpib_trusted = false;
        self.running = true;
        self.paused = false;
        self.stat_restarts += 1;
        self.stream_start_us = self.last_poll_us;
        self.stream_written = 0;
        Ok(())
    }
}

/// Encode an HDA stream format: S16LE, `channels` interleaved, `rate` Hz.
/// Rates outside the encodable set were normalized by `nearest_rate`.
fn stream_format(rate: u32, channels: u8) -> u16 {
    // (base44, mult, div)
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
        _ => (false, 1, 1), // 48000
    };
    let mut fmt: u16 = 0;
    if base44 {
        fmt |= 1 << 14;
    }
    fmt |= ((mult - 1) as u16) << 11;
    fmt |= ((div - 1) as u16) << 8;
    fmt |= 0b001 << 4; // 16-bit
    fmt |= (channels as u16 - 1) & 0xf;
    fmt
}

fn nearest_rate(rate: u32) -> u32 {
    const RATES: [u32; 11] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
    ];
    *RATES
        .iter()
        .min_by_key(|&&r| (r as i64 - rate as i64).unsigned_abs())
        .unwrap_or(&48000)
}

// ── Codec graph walk ────────────────────────────────────────────────────────
#[derive(Clone)]
struct OutPath {
    conv: u32,
    pin: u32,
    pin_conn_idx: u32,
    digital: bool,
    hdmi_dp: bool,
    present: bool,
}

fn sub_nodes(resp: u32) -> (u32, u32) {
    ((resp >> 16) & 0xff, resp & 0xff)
}

impl HdaInner {
    /// Read a pin's connection list and return the index of `conv` in it.
    fn conn_index_of(&mut self, pin: u32, conv: u32) -> DeviceResult<Option<u32>> {
        let len_resp = self.param(pin, PAR_CONN_LIST_LEN)?;
        let long_form = len_resp & 0x80 != 0;
        let len = len_resp & 0x7f;
        let per_resp = if long_form { 2 } else { 4 };
        let mut idx = 0u32;
        let mut i = 0u32;
        while i < len {
            let resp = self.cmd(pin, VERB_GET_CONN_LIST, i)?;
            for k in 0..per_resp.min(len - i) {
                let entry = if long_form {
                    (resp >> (16 * k)) & 0x7fff
                } else {
                    (resp >> (8 * k)) & 0x7f
                };
                if entry == conv {
                    return Ok(Some(idx));
                }
                idx += 1;
            }
            i += per_resp;
        }
        Ok(None)
    }

    /// Fresh score for a candidate path: prefer a *live* HDMI/DP pin (presence
    /// and ELD), then analog jacks. An unconnected NVIDIA pin must not outrank
    /// the PCH analog codec — that was "wavplay succeeds, monitor silent".
    /// HDMI codecs typically need `SET_PIN_SENSE` before `GET_PIN_SENSE`
    /// (Linux `snd_hda_pin_sense`); without the trigger every pin reads PD=0
    /// and we pick the first widget, which is often a dead connector.
    fn score_path(&mut self, p: &OutPath) -> (i32, bool, bool) {
        if p.hdmi_dp {
            let _ = self.cmd(p.pin, VERB_SET_PIN_SENSE, 0);
            wait_us(2_000);
        }
        let sense = self.cmd(p.pin, VERB_GET_PIN_SENSE, 0).unwrap_or(0);
        let present = sense & (1 << 31) != 0;
        let eld_valid = sense & (1 << 30) != 0;
        let mut score = 0;
        if p.hdmi_dp && p.digital {
            // Linux Pulse/PipeWire only uses an HDMI sink with ELD/presence.
            // A dead NVIDIA pin must not outrank the PCH analog codec — that
            // was card 0 = silent HDMI while `amixer` on Intel worked.
            score += if present { 4 } else { 0 };
        }
        if present {
            score += 2;
        }
        if eld_valid {
            score += 1;
        }
        (score, present, eld_valid)
    }

    /// Enumerate the AFG's widgets and collect every viable output path
    /// (output-capable pin with a physical connector, reachable converter).
    fn collect_candidates(&mut self, afg: u32) -> DeviceResult<Vec<OutPath>> {
        let (wstart, wcount) = sub_nodes(self.param(afg, PAR_NODE_COUNT)?);
        let mut converters: Vec<(u32, bool)> = Vec::new(); // (nid, digital)
        let mut pins: Vec<(u32, u32)> = Vec::new(); // (nid, pincap)

        for nid in wstart..wstart + wcount {
            let caps = self.param(nid, PAR_AUDIO_WIDGET_CAP)?;
            let wtype = (caps >> 20) & 0xf;
            let digital = caps & (1 << 9) != 0;
            match wtype {
                WIDGET_AUDIO_OUT => converters.push((nid, digital)),
                WIDGET_PIN => {
                    let pincap = self.param(nid, PAR_PIN_CAP)?;
                    let defcfg = self.cmd(nid, VERB_GET_CONFIG_DEFAULT, 0)?;
                    // Output-capable pins with a physical connection only.
                    let connectivity = (defcfg >> 30) & 0x3;
                    if pincap & (1 << 4) != 0 && connectivity != 0x1 {
                        pins.push((nid, pincap));
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for &(pin, pincap) in &pins {
            let hdmi_dp = pincap & (1 << 7) != 0 || pincap & (1 << 24) != 0;
            // First reachable converter per pin is enough.
            for &(conv, cdigital) in &converters {
                if let Some(idx) = self.conn_index_of(pin, conv)? {
                    out.push(OutPath {
                        conv,
                        pin,
                        pin_conn_idx: idx,
                        digital: cdigital,
                        hdmi_dp,
                        present: false,
                    });
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Power up and enable every HDMI/DP pin so presence/ELD can show up
    /// (NVIDIA codecs often report PD=0 until `PIN_OUT` is set).
    ///
    /// On NVIDIA Turing (RTX 2060 SUPER) the firmware can take >1 ms to settle
    /// presence after `SET_PIN_CTL`.  We wait up to 3 × 5 ms before giving up,
    /// so that `best_candidate` scores at least one pin as present when the
    /// monitor is already connected at boot.
    fn arm_digital_pins(&mut self) {
        let pins: Vec<u32> = self
            .candidates
            .iter()
            .filter(|p| p.hdmi_dp)
            .map(|p| p.pin)
            .collect();
        if pins.is_empty() {
            return;
        }
        for pin in pins.iter().copied() {
            let _ = self.cmd(pin, VERB_SET_POWER_STATE, 0);
            let _ = self.cmd(pin, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN);
        }
        // Wait up to 3 × 5 ms for at least one pin to report presence.
        // One 5 ms slot is enough on most hardware; extra retries cover slow
        // NVIDIA GSP firmware responses on multi-GPU boards.
        for _ in 0..3 {
            wait_us(5_000);
            let any_present = pins.iter().any(|&pin| {
                self.cmd(pin, VERB_GET_PIN_SENSE, 0)
                    .map(|v| v & (1 << 31) != 0)
                    .unwrap_or(false)
            });
            if any_present {
                break;
            }
        }
    }

    /// Pick the best-scoring path among `self.candidates` right now.
    fn best_candidate(&mut self) -> Option<OutPath> {
        let candidates = self.candidates.clone();
        let mut best: Option<OutPath> = None;
        let mut best_score = -1i32;
        for mut p in candidates {
            let (score, present, eld_valid) = self.score_path(&p);
            p.present = present;
            info!(
                "[hda] path candidate: pin {:#x} -> conv {:#x} (digital={}, hdmi/dp={}, present={}, eld={}, score={})",
                p.pin, p.conv, p.digital, p.hdmi_dp, present, eld_valid, score
            );
            if score > best_score {
                best_score = score;
                best = Some(p);
            }
        }
        best
    }

    /// Re-evaluate the candidate paths and re-route if a better pin has
    /// appeared (e.g. the display driver pushed the monitor's ELD after our
    /// PCI-probe-time pick). Called with the stream stopped.
    ///
    /// When multiple candidates exist the paths are re-scored. When only one
    /// candidate exists (or after re-scoring confirms the same path) the
    /// current path is fully re-armed via [`try_setup_path`]: this re-sends
    /// AFG/converter/pin `SET_POWER_STATE` (D0), `SET_PIN_CTL`, channel-select
    /// and all digital-converter verbs. On NVIDIA HDMI hardware the codec can
    /// enter D3 between PCI probe and the first stream start; without this
    /// re-arm the firmware acknowledges the verbs in `start_stream` silently
    /// but produces no audio.
    ///
    /// Re-scoring (`best_candidate`) is still skipped for single-candidate
    /// codecs: it issues a `PIN_SENSE` verb per candidate and a timed-out verb
    /// adds 200 ms of stall per underrun-triggered restart.
    fn repick_path(&mut self) {
        let afg = self.afg;

        // Re-score only when there is something to choose between.
        if self.candidates.len() >= 2 {
            let Some(best) = self.best_candidate() else {
                return;
            };
            if best.pin != self.pin_nid || best.conv != self.conv_nid {
                info!(
                    "[hda] re-routing output: pin {:#x} -> pin {:#x}",
                    self.pin_nid, best.pin
                );
                if let Err(e) = self.setup_path(afg, &best) {
                    warn!("[hda] re-route failed: {:?} — keeping previous path", e);
                }
                // setup_path already did the full try_setup_path; done.
                return;
            }
            // Best candidate is the same pin/converter — fall through to re-arm.
        }

        // Re-arm the current path regardless of candidate count.  This covers:
        //   • single-candidate codecs (no re-scoring possible), and
        //   • multi-candidate codecs where re-scoring confirmed the same path.
        // try_setup_path brings the AFG, converter, and pin back to D0, re-sends
        // SET_PIN_CTL + SET_CONN_SELECT, re-enables any output amp, and
        // re-programs the digital converter — all needed after kick_hdmi_audio().
        let conv = self.conv_nid;
        let pin = self.pin_nid;
        if let Some(curr) = self
            .candidates
            .iter()
            .find(|c| c.pin == pin && c.conv == conv)
            .cloned()
        {
            if let Err(e) = self.try_setup_path(afg, &curr) {
                warn!("[hda] path re-arm failed: {:?}", e);
            }
        }
    }

    /// Power up and route the chosen path.
    fn setup_path(&mut self, afg: u32, path: &OutPath) -> DeviceResult {
        // Do NOT publish the new path until every verb below has succeeded.
        // Committing first and failing halfway leaves conv_nid/pin_nid naming
        // a converter whose stream id was cleared and a pin that was never
        // enabled, while the caller logs "keeping previous path" — the writer
        // then streams into a dead route and hears silence with no error.
        let old = (self.conv_nid, self.pin_nid, self.digital);
        let restore = |me: &mut Self| {
            me.conv_nid = old.0;
            me.pin_nid = old.1;
            me.digital = old.2;
        };
        match self.try_setup_path(afg, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                restore(self);
                // Put the previous converter back on the stream so the old
                // route keeps working, exactly as the caller assumes.
                if self.conv_nid != 0 {
                    let tag = self.stream_tag;
                    let _ = self.cmd(self.conv_nid, VERB_SET_STREAM_ID, tag << 4);
                }
                Err(e)
            }
        }
    }

    fn try_setup_path(&mut self, afg: u32, path: &OutPath) -> DeviceResult {
        let old_conv = self.conv_nid;
        if old_conv != 0 && old_conv != path.conv {
            let _ = self.cmd(old_conv, VERB_SET_STREAM_ID, 0);
        }
        self.conv_nid = path.conv;
        self.pin_nid = path.pin;
        self.digital = path.digital;

        self.cmd(afg, VERB_SET_POWER_STATE, 0)?; // AFG -> D0
        wait_us(1_000);
        self.cmd(path.conv, VERB_SET_POWER_STATE, 0)?;
        self.cmd(path.pin, VERB_SET_POWER_STATE, 0)?;

        self.cmd(path.pin, VERB_SET_CONN_SELECT, path.pin_conn_idx)?;
        self.cmd(path.pin, VERB_SET_PIN_CTL, PIN_CTL_OUT_EN)?;

        // EAPD (external amplifier) on pins that have it — analog outputs.
        let pincap = self.param(path.pin, PAR_PIN_CAP)?;
        if pincap & (1 << 16) != 0 {
            let _ = self.cmd(path.pin, VERB_SET_EAPD, 0x2);
        }

        // Unmute + 0 dB on any output amps in the path.
        for &nid in &[path.conv, path.pin] {
            let amp = self.param(nid, PAR_OUT_AMP_CAP)?;
            let has_amp = amp != 0 || self.param(nid, PAR_AUDIO_WIDGET_CAP)? & (1 << 2) != 0;
            if has_amp {
                let offset = amp & 0x7f; // 0 dB step index
                                         // Set output amp, both channels, gain = offset (0 dB).
                let payload = (1 << 15) | (1 << 13) | (1 << 12) | offset;
                let _ = self.cmd16(nid, 0x3, payload);
            }
        }

        if path.digital {
            self.setup_digital_converter(2);
        }
        Ok(())
    }

    /// Digital converter + HDMI channel map + CEA audio infoframe.
    /// `SET_CVT_CHAN_COUNT` is 0x72d (not 0x733 / CP_CTRL). Slot verbs map
    /// each PCM channel onto itself, matching Linux `hdmi_setup_stream`.
    fn setup_digital_converter(&mut self, channels: u8) {
        let conv = self.conv_nid;
        let ch = channels.max(1);
        let _ = self.cmd(conv, VERB_SET_DIGI_CVT1, 0x1);
        let _ = self.cmd(conv, VERB_SET_CVT_CHAN_COUNT, (ch - 1) as u32);
        for i in 0..ch {
            let slot = i as u32;
            let _ = self.cmd(conv, VERB_SET_HDMI_CHAN_SLOT, (slot << 4) | slot);
        }
        self.send_audio_infoframe(ch);
    }

    /// Program the pin's Data Island Packet buffer with a CEA audio infoframe
    /// (HDMI sinks want one before they render PCM). Best-effort: some codecs
    /// route infoframes through the graphics driver instead.
    ///
    /// Hardware autoincrements only the low 3 bits of the DIP byte index, so
    /// the index is rewritten every 8 bytes (Linux `hdmi_fill_audio_infoframe`).
    fn send_audio_infoframe(&mut self, channels: u8) {
        let pin = self.pin_nid;
        let mut frame = [0u8; 14];
        frame[0] = 0x84; // CEA audio infoframe
        frame[1] = 0x01; // version
        frame[2] = 0x0a; // payload length
        frame[4] = channels.saturating_sub(1); // CC, coding type "refer to stream"
                                               // frame[8] = CA (0 = FL/FR), rest zero.
        let sum: u32 = frame.iter().map(|&b| b as u32).sum();
        // Checksum: the bytes of the frame plus this one must sum to 0 mod 256.
        frame[3] = ((0x100 - (sum & 0xff)) & 0xff) as u8;

        for (i, &b) in frame.iter().enumerate() {
            if i % 8 == 0 {
                let _ = self.cmd(pin, VERB_SET_DIP_INDEX, i as u32);
            }
            let _ = self.cmd(pin, VERB_SET_DIP_DATA, b as u32);
        }
        let _ = self.cmd(pin, VERB_SET_DIP_XMIT, 0xc0); // best-effort transmit
    }
}

impl HdaDevice {
    pub fn new(bar: usize, name: String, is_nvidia: bool) -> DeviceResult<Self> {
        let gcap = mmio_r16(bar, REG_GCAP);
        let iss = ((gcap >> 8) & 0xf) as usize;
        let oss = ((gcap >> 12) & 0xf) as usize;
        let addr64 = gcap & 1 != 0;
        info!(
            "[hda] {}: GCAP {:#06x} — {} in / {} out streams, 64-bit {}",
            name, gcap, iss, oss, addr64
        );
        if oss == 0 {
            return Err(DeviceError::NotSupported);
        }

        // ── Controller reset ────────────────────────────────────────────────
        mmio_w32(bar, REG_GCTL, mmio_r32(bar, REG_GCTL) & !GCTL_CRST);
        let t = timer_now_as_micros();
        while mmio_r32(bar, REG_GCTL) & GCTL_CRST != 0 {
            if timer_now_as_micros().wrapping_sub(t) > 1_000_000 {
                error!("[hda] {}: controller reset entry timed out", name);
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
        wait_us(200);
        mmio_w32(bar, REG_GCTL, mmio_r32(bar, REG_GCTL) | GCTL_CRST);
        let t = timer_now_as_micros();
        while mmio_r32(bar, REG_GCTL) & GCTL_CRST == 0 {
            if timer_now_as_micros().wrapping_sub(t) > 1_000_000 {
                error!("[hda] {}: controller reset exit timed out", name);
                return Err(DeviceError::IoError);
            }
            core::hint::spin_loop();
        }
        // Codecs get 521 µs (25 frames) to request state-change; be generous.
        wait_us(2_000);

        let statests = mmio_r16(bar, REG_STATESTS);
        if statests == 0 {
            error!(
                "[hda] {}: no codec responded after reset (QEMU needs -device hda-output)",
                name
            );
            return Err(DeviceError::NoResources);
        }
        let cad = statests.trailing_zeros();

        // Polled operation: all interrupt sources off.
        mmio_w32(bar, REG_INTCTL, 0);

        // ── CORB/RIRB ───────────────────────────────────────────────────────
        mmio_w8(bar, REG_CORBCTL, 0);
        mmio_w8(bar, REG_RIRBCTL, 0);

        // Pick the largest supported ring: SIZECAP bits [7:4] = {2,16,256}.
        let corb_szcap = mmio_r8(bar, REG_CORBSIZE) >> 4;
        let (corb_entries, corb_sz) = ring_size(corb_szcap);
        mmio_w8(bar, REG_CORBSIZE, corb_sz);
        let rirb_szcap = mmio_r8(bar, REG_RIRBSIZE) >> 4;
        let (rirb_entries, rirb_sz) = ring_size(rirb_szcap);
        mmio_w8(bar, REG_RIRBSIZE, rirb_sz);

        let (corb_va, corb_pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
        let (rirb_va, rirb_pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
        unsafe {
            core::ptr::write_bytes(corb_va as *mut u8, 0, PAGE_SIZE);
            core::ptr::write_bytes(rirb_va as *mut u8, 0, PAGE_SIZE);
        }
        clflush_range(corb_va, PAGE_SIZE);
        clflush_range(rirb_va, PAGE_SIZE);

        mmio_w32(bar, REG_CORBLBASE, corb_pa as u32);
        mmio_w32(bar, REG_CORBUBASE, (corb_pa as u64 >> 32) as u32);
        mmio_w32(bar, REG_RIRBLBASE, rirb_pa as u32);
        mmio_w32(bar, REG_RIRBUBASE, (rirb_pa as u64 >> 32) as u32);

        // CORB read-pointer reset handshake (best-effort: QEMU acks lazily).
        mmio_w16(bar, REG_CORBRP, 1 << 15);
        wait_us(100);
        mmio_w16(bar, REG_CORBRP, 0);
        wait_us(100);
        mmio_w16(bar, REG_CORBWP, 0);
        // RIRB write-pointer reset (self-clearing).
        mmio_w16(bar, REG_RIRBWP, 1 << 15);
        // One response per interrupt status: we poll and ack RIRBSTS after
        // every verb. QEMU stops CORB until that ack (see `ack_rirb`).
        mmio_w16(bar, REG_RINTCNT, 1);
        mmio_w8(bar, REG_RIRBSTS, RIRBSTS_IRQ | RIRBSTS_OVERRUN);

        mmio_w8(bar, REG_RIRBCTL, RIRBCTL_DMA_EN | RIRBCTL_IRQ_EN);
        mmio_w8(bar, REG_CORBCTL, 0x2); // CORB DMA run

        // ── PCM ring + BDL ─────────────────────────────────────────────────
        let ring_len = RING_PAGES * PAGE_SIZE;
        let (ring_va, ring_pa) = ProviderImpl::alloc_dma(ring_len);
        let (bdl_va, bdl_pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
        unsafe { core::ptr::write_bytes(ring_va as *mut u8, 0, ring_len) };
        clflush_range(ring_va, ring_len);
        let n_seg = ring_len / BDL_SEGMENT;
        for i in 0..n_seg {
            let e = bdl_va + i * 16;
            unsafe {
                write_volatile(e as *mut u64, (ring_pa + i * BDL_SEGMENT) as u64);
                write_volatile((e + 8) as *mut u32, BDL_SEGMENT as u32);
                write_volatile((e + 12) as *mut u32, 0); // no IOC — polled
            }
        }
        clflush_range(bdl_va, n_seg * 16);

        // ── DMA position buffer ────────────────────────────────────────────
        // The controller writes each stream's play position here, in host
        // memory, 8 bytes per stream descriptor. Linux prefers it over
        // `SD_LPIB` by default (`position_fix=AUTO`) because on many
        // controllers LPIB does not track what has actually been played --
        // it is the register whose slop this driver had only 512 bytes of
        // guard against. Enabling it costs one page and gives `play_pos` a
        // second opinion to be conservative with.
        let dma_pos_va = {
            let (va, pa) = ProviderImpl::alloc_dma(PAGE_SIZE);
            unsafe { core::ptr::write_bytes(va as *mut u8, 0, PAGE_SIZE) };
            clflush_range(va, PAGE_SIZE);
            mmio_w32(bar, REG_DPUBASE, (pa as u64 >> 32) as u32);
            mmio_w32(bar, REG_DPLBASE, (pa as u32) | DPLBASE_ENABLE);
            // Read back: a controller that refuses the buffer leaves the
            // enable bit clear, and trusting an address it never writes to
            // would freeze playback at position zero.
            if mmio_r32(bar, REG_DPLBASE) & DPLBASE_ENABLE != 0 {
                va + (iss * 8)
            } else {
                info!("[hda] {}: controller refused the DMA position buffer", name);
                0
            }
        };

        let mut inner = HdaInner {
            bar,
            corb_va,
            corb_entries,
            rirb_va,
            rirb_entries,
            rirb_rp: 0,
            cad,
            conv_nid: 0,
            pin_nid: 0,
            digital: false,
            afg: 0,
            candidates: Vec::new(),
            // First output stream descriptor comes after the input ones.
            sd_base: REG_SD_BASE + iss * 0x20,
            stream_tag: 1,
            ring_va,
            ring_len,
            bdl_pa,
            running: false,
            paused: false,
            wp: 0,
            queued: 0,
            last_lpib: 0,
            last_poll_us: 0,
            dma_pos_va,
            last_dpib: 0,
            dpib_trusted: false,
            zero_ptr: 0,
            stat_drains: 0,
            stat_underruns: 0,
            stat_restarts: 0,
            stat_stop_timeouts: 0,
            stat_bad_pos: 0,
            last_bad_pos: 0,
            stream_start_us: 0,
            stream_written: 0,
            stops: [StopEvent::default(); STOP_HISTORY],
            rate: 48000,
            channels: 2,
            gain_l: 100,
            gain_r: 100,
            mute_l: false,
            mute_r: false,
        };

        // ── Codec discovery ────────────────────────────────────────────────
        let vendor = inner.param(0, PAR_VENDOR_ID)?;
        info!("[hda] {}: codec {} vendor {:#010x}", name, cad, vendor);

        let (fg_start, fg_count) = sub_nodes(inner.param(0, PAR_NODE_COUNT)?);
        let mut afg = None;
        for nid in fg_start..fg_start + fg_count {
            if inner.param(nid, PAR_FUNCTION_TYPE)? & 0xff == 0x01 {
                afg = Some(nid);
                break;
            }
        }
        let afg = afg.ok_or(DeviceError::NotSupported)?;

        inner.afg = afg;
        inner.candidates = inner.collect_candidates(afg)?;
        inner.arm_digital_pins();
        let path = inner.best_candidate().ok_or(DeviceError::NotSupported)?;
        info!(
            "[hda] {}: using pin {:#x} -> converter {:#x} ({}, {})",
            name,
            path.pin,
            path.conv,
            if path.digital { "HDMI/DP" } else { "analog" },
            if path.present {
                "display/jack present"
            } else {
                "nothing detected on jack — playing anyway"
            }
        );
        inner.setup_path(afg, &path)?;

        Ok(HdaDevice {
            name,
            is_nvidia,
            inner: Mutex::new(inner),
        })
    }
}

/// Map a CORB/RIRB SIZECAP field to the largest supported (entries, SIZE code).
fn ring_size(szcap: u8) -> (usize, u8) {
    if szcap & 0x4 != 0 || szcap == 0 {
        // 256 entries (assume 256 when the cap field reads zero — fixed-size
        // controllers may hardwire it).
        (256, 0x2)
    } else if szcap & 0x2 != 0 {
        (16, 0x1)
    } else {
        (2, 0x0)
    }
}

impl Scheme for HdaDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn handle_irq(&self, _irq: usize) {
        // Fully polled.
    }
}

impl AudioScheme for HdaDevice {
    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
        let rate = nearest_rate(rate);
        let _ = channels;
        let channels = 2u8; // stereo only for now
        let mut inner = self.inner.lock();
        inner.stop_stream();
        inner.paused = false;
        inner.queued = 0;
        inner.wp = 0;
        inner.zero_ptr = 0;
        unsafe { core::ptr::write_bytes(inner.ring_va as *mut u8, 0, inner.ring_len) };
        clflush_range(inner.ring_va, inner.ring_len);
        inner.rate = rate;
        inner.channels = channels;
        if inner.digital {
            let ch = channels;
            inner.send_audio_infoframe(ch);
        }
        Ok((rate, channels))
    }

    fn params(&self) -> (u32, u8) {
        let inner = self.inner.lock();
        (inner.rate, inner.channels)
    }

    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        let kick_hdmi = {
            let inner = self.inner.lock();
            !inner.running && inner.digital
        };
        if kick_hdmi {
            // GOP never enables audio packets; re-push ELD/unmute now so the
            // pin-sense that follows can see a live display. Drop the HDA
            // lock first — RM takes GPU locks.
            crate::display::kick_hdmi_audio();
        }
        let mut inner = self.inner.lock();
        inner.poll_progress();
        if !inner.running && !inner.paused {
            // Give the codec graph a chance to re-route to a pin that has
            // gained presence/ELD since the last pick (on NVIDIA GPUs the ELD
            // lands long after PCI probe)...
            inner.repick_path();
            // ...and re-anchor the software pointers: a started stream always
            // begins DMA at ring offset 0.
            inner.wp = 0;
            inner.zero_ptr = 0;
            inner.queued = 0;
        }
        let free = inner.free_bytes();
        // Whole frames only, so channels never swap on a partial write.
        let frame = inner.channels as usize * 2;
        let n = free.min(pcm.len()) / frame * frame;
        if n == 0 {
            return Ok(0);
        }
        let mut p = inner.wp;
        let mut done = 0;
        while done < n {
            let chunk = (n - done).min(inner.ring_len - p);
            inner.copy_pcm_scaled(&pcm[done..done + chunk], inner.ring_va + p, chunk);
            clflush_range(inner.ring_va + p, chunk);
            p = (p + chunk) % inner.ring_len;
            done += chunk;
        }
        inner.wp = p;
        inner.queued += n;
        if !inner.running && !inner.paused {
            inner.start_stream()?;
        }
        inner.stream_written += n;
        Ok(n)
    }

    fn free_bytes(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.poll_progress();
        inner.free_bytes()
    }

    fn buffer_bytes(&self) -> usize {
        let inner = self.inner.lock();
        inner.ring_len - RING_GUARD
    }

    fn queued_bytes(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.poll_progress();
        inner.queued
    }

    fn is_playing(&self) -> bool {
        self.inner.lock().running
    }

    fn reset(&self) -> DeviceResult {
        let mut inner = self.inner.lock();
        inner.stop_stream();
        inner.paused = false;
        inner.queued = 0;
        inner.wp = 0;
        inner.zero_ptr = 0;
        inner.silence_ring();
        Ok(())
    }

    fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
        Ok(self.inner.lock().rewind_bytes(bytes))
    }

    fn forward(&self, bytes: usize) -> DeviceResult<usize> {
        Ok(self.inner.lock().forward_bytes(bytes))
    }

    fn pause(&self) -> DeviceResult {
        self.inner.lock().pause_stream();
        Ok(())
    }

    fn resume(&self) -> DeviceResult {
        self.inner.lock().resume_stream()
    }

    fn set_gain(&self, left: u8, right: u8, mute_left: bool, mute_right: bool) -> DeviceResult {
        let mut inner = self.inner.lock();
        inner.gain_l = left.min(100);
        inner.gain_r = right.min(100);
        inner.mute_l = mute_left;
        inner.mute_r = mute_right;
        Ok(())
    }

    fn gain(&self) -> (u8, u8, bool, bool) {
        let inner = self.inner.lock();
        (inner.gain_l, inner.gain_r, inner.mute_l, inner.mute_r)
    }

    fn diagnostics(&self) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();
        let mut inner = self.inner.lock();
        let bar = inner.bar;
        let sd = inner.sd_base;

        let _ = writeln!(out, "[gpusnd] === {} ===", self.name);
        let _ = writeln!(
            out,
            "[gpusnd] controller: GCAP {:#06x} STATESTS {:#06x} codec {}",
            mmio_r16(bar, REG_GCAP),
            mmio_r16(bar, REG_STATESTS),
            inner.cad
        );

        // Stream descriptor, read straight from MMIO. RUN=1 with a moving
        // LPIB means the DMA engine really is fetching our samples; if that
        // holds and there is still no sound, the fault is downstream of the
        // controller (codec routing, or the display engine not transmitting).
        let ctl = mmio_r32(bar, sd + SD_CTL);
        let lpib1 = mmio_r32(bar, sd + SD_LPIB);
        wait_us(2_000);
        let lpib2 = mmio_r32(bar, sd + SD_LPIB);
        let _ =
            writeln!(
            out,
            "[gpusnd] stream: CTL {:#010x} (RUN={}, tag={}) FMT {:#06x} CBL {} LPIB {} -> {} ({})",
            ctl,
            (ctl >> 1) & 1,
            (ctl >> 20) & 0xf,
            mmio_r16(bar, sd + SD_FMT),
            mmio_r32(bar, sd + SD_CBL),
            lpib1,
            lpib2,
            if lpib1 != lpib2 { "ADVANCING" } else { "STALLED" },
        );
        let _ = writeln!(
            out,
            "[gpusnd] ring: running={} queued={} wp={} rate={} ch={}",
            inner.running, inner.queued, inner.wp, inner.rate, inner.channels
        );
        // Which position source the ring is pacing against. LPIB alone is the
        // fragile case: on a controller whose LPIB runs ahead of what has been
        // played, the writer is handed space that is still going to be played
        // and overwrites it -- heard as a stream of very short dropouts. The
        // position buffer is the second opinion that prevents that, and this
        // says whether it is actually being maintained on this hardware.
        let dpib = inner.dma_pos();
        let _ = writeln!(
            out,
            "[gpusnd] position: LPIB {}{} guard={}B queued={}B free={}B of {}B",
            inner.lpib(),
            match dpib {
                Some(p) if inner.dpib_trusted => alloc::format!(" + DMA-pos {} (in use)", p),
                Some(p) => alloc::format!(" + DMA-pos {} (not advancing yet)", p),
                None => alloc::string::String::from(" only (controller refused a position buffer)"),
            },
            RING_GUARD,
            inner.queued,
            inner.free_bytes(),
            inner.ring_len
        );
        let _ = writeln!(
            out,
            "[gpusnd] events: {} drains, {} underruns, {} stream restarts, {} stop timeouts, {} rejected position reads{}",
            inner.stat_drains,
            inner.stat_underruns,
            inner.stat_restarts,
            inner.stat_stop_timeouts,
            inner.stat_bad_pos,
            if inner.stat_bad_pos > 0 {
                alloc::format!(" (last {:#x})", inner.last_bad_pos)
            } else {
                String::new()
            }
        );
        // The last stops, oldest first. wavplay's tone is 576000 B, so a stop
        // that ends a stream short of that, well before 3000 ms, is a dropout;
        // one at ~3000 ms with all of it written is the tone ending.
        for ev in inner.stops.iter().filter(|e| e.kind != 0) {
            let _ = writeln!(
                out,
                "[gpusnd]   stop: {} at {} ms, {} B written to that stream",
                if ev.kind == b'U' { "underrun" } else { "drain" },
                ev.at_ms,
                ev.written
            );
        }
        // What is actually IN the ring right now. Read while a tone plays,
        // this separates the two remaining families of dropout: silence found
        // in memory where the tone should be was put there by software (the
        // writer or the re-zeroing pass); a ring with the tone intact that
        // still plays with gaps points at the controller or the HDMI link.
        // Offsets are given relative to LPIB, so a gap that lies within the
        // queued span (0..queued ahead of LPIB) is unplayed audio that has
        // already been damaged; one in the guard just behind LPIB is the
        // previous lap and harmless.
        if inner.running {
            let ring = inner.ring_len;
            let frame = inner.frame_bytes().max(2);
            let lpib = inner.lpib() as usize % ring;
            // The CPU never reads the ring except here; drop any lines a
            // previous dump left cached so this sees what the engine sees.
            clflush_range(inner.ring_va, ring);
            let bytes = unsafe { core::slice::from_raw_parts(inner.ring_va as *const u8, ring) };
            const QUIET: i16 = 16; // |sample| at or below this is "silence"
            const MIN_RUN_FRAMES: usize = 48; // 1 ms at 48 kHz
            let is_quiet = |f: usize| {
                let b = &bytes[f * frame..(f + 1) * frame];
                b.as_chunks::<2>()
                    .0
                    .iter()
                    .all(|c| i16::from_le_bytes(*c).unsigned_abs() <= QUIET as u16)
            };
            let frames = ring / frame;
            let mut runs = 0usize;
            let mut quiet_bytes = 0usize;
            let mut longest = 0usize;
            let mut peak: u16 = 0;
            let mut listed = String::new();
            let mut f = 0;
            while f < frames {
                if is_quiet(f) {
                    let start = f;
                    while f < frames && is_quiet(f) {
                        f += 1;
                    }
                    let n = f - start;
                    if n >= MIN_RUN_FRAMES {
                        runs += 1;
                        quiet_bytes += n * frame;
                        longest = longest.max(n);
                        if runs <= 8 {
                            // Offset of the run's start ahead of the playhead.
                            let ahead = (start * frame + ring - lpib) % ring;
                            let _ = write!(listed, " [+{}B {}fr]", ahead, n);
                        }
                    }
                } else {
                    for c in bytes[f * frame..(f + 1) * frame].as_chunks::<2>().0 {
                        peak = peak.max(i16::from_le_bytes(*c).unsigned_abs());
                    }
                    f += 1;
                }
            }
            let _ = writeln!(
                out,
                "[gpusnd] ring scan: peak {} quiet runs {} ({} B, longest {} fr){}{}",
                peak,
                runs,
                quiet_bytes,
                longest,
                listed,
                if runs > 8 { " ..." } else { "" }
            );
        }

        // The active path, read BACK from the codec rather than from our own
        // bookkeeping — that is the whole point of this dump.
        let (conv, pin, digital) = (inner.conv_nid, inner.pin_nid, inner.digital);
        let _ = writeln!(
            out,
            "[gpusnd] active path: converter {:#x} -> pin {:#x} ({})",
            conv,
            pin,
            if digital { "HDMI/DP" } else { "analog" }
        );
        if conv != 0 {
            let sid = inner
                .cmd(conv, VERB_GET_STREAM_ID, 0)
                .unwrap_or(0xffff_ffff);
            let fmt = inner
                .cmd16(conv, VERB_GET_CVT_FORMAT, 0)
                .unwrap_or(0xffff_ffff);
            let dig = inner.cmd(conv, VERB_GET_DIGI_CVT, 0).unwrap_or(0xffff_ffff);
            let pwr = inner
                .cmd(conv, VERB_GET_POWER_STATE, 0)
                .unwrap_or(0xffff_ffff);
            let _ = writeln!(
                out,
                "[gpusnd]   converter: stream_id {:#x} (tag {}) format {:#06x} digi_cvt {:#x} (DIGEN={}) power {:#x}",
                sid,
                sid >> 4,
                fmt,
                dig,
                dig & 1,
                pwr
            );
        }
        if pin != 0 {
            let ctl = inner.cmd(pin, VERB_GET_PIN_CTL, 0).unwrap_or(0xffff_ffff);
            let sense = inner.cmd(pin, VERB_GET_PIN_SENSE, 0).unwrap_or(0);
            let eapd = inner.cmd(pin, VERB_GET_EAPD, 0).unwrap_or(0xffff_ffff);
            let pwr = inner
                .cmd(pin, VERB_GET_POWER_STATE, 0)
                .unwrap_or(0xffff_ffff);
            let _ = writeln!(
                out,
                "[gpusnd]   pin: ctl {:#x} (OUT_EN={}) sense {:#010x} (present={}, eld_valid={}) eapd {:#x} power {:#x}",
                ctl,
                (ctl & PIN_CTL_OUT_EN != 0) as u8,
                sense,
                (sense & (1 << 31) != 0) as u8,
                (sense & (1 << 30) != 0) as u8,
                eapd,
                pwr
            );
        }

        // Every candidate, with live presence/ELD: this says whether the pin
        // carrying the cable was the one we picked.
        let candidates = inner.candidates.clone();
        let _ = writeln!(out, "[gpusnd] candidates ({}):", candidates.len());
        for c in candidates.iter() {
            let sense = inner.cmd(c.pin, VERB_GET_PIN_SENSE, 0).unwrap_or(0);
            let _ = writeln!(
                out,
                "[gpusnd]   pin {:#x} -> conv {:#x} digital={} hdmi/dp={} present={} eld_valid={}{}",
                c.pin,
                c.conv,
                c.digital as u8,
                c.hdmi_dp as u8,
                (sense & (1 << 31) != 0) as u8,
                (sense & (1 << 30) != 0) as u8,
                if c.pin == pin { "  <== ACTIVE" } else { "" },
            );
        }

        if self.is_nvidia {
            let _ = writeln!(
                out,
                "[gpusnd] NVIDIA HDA function: audio only reaches the cable if the\n\
                 [gpusnd]   display engine transmits it — see the [hdmi-audio] state below."
            );
        }
        out
    }

    fn default_score(&self) -> i32 {
        let mut inner = self.inner.lock();
        let pin = inner.pin_nid;
        if let Some(p) = inner.candidates.iter().find(|c| c.pin == pin).cloned() {
            inner.score_path(&p).0
        } else if inner.digital {
            1
        } else {
            0
        }
    }
}

// ── PCI driver registration ─────────────────────────────────────────────────

pub struct HdaDriverPci;

impl PciDriver for HdaDriverPci {
    fn name(&self) -> &str {
        "hda-audio"
    }

    fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
        false
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        // PCI class 04 (multimedia), subclass 03 (HD Audio).
        dev.id.class == 0x04 && dev.id.subclass == 0x03
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        _irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let addr = match dev.bars[0] {
            Some(BAR::Memory(addr, _len, _, _)) => addr,
            _ => return Err(DeviceError::NotSupported),
        };

        // Linux: PCH analog + one HDMI card (the GPU with the monitor).
        // Dual-GPU boards expose a second `xx:00.1` with no display; probing
        // it steals card order and kick_hdmi_audio would DDC a headless GPU.
        if dev.id.vendor_id == 0x10de
            && !crate::display::nvidia_hda_is_monitor_gpu(dev.loc.bus, dev.loc.device)
        {
            info!(
                "[hda] skipping {:02x}:{:02x}.1 (nvidia-hdmi) — not the monitor GPU; \
                 keeping Intel PCH + one HDMI like Linux",
                dev.loc.bus, dev.loc.device
            );
            return Err(DeviceError::NotSupported);
        }

        if let Some(m) = mapper {
            m.query_or_map(addr as usize, 0x4000);
        }

        // NVIDIA HDA functions default to non-snooped DMA; force the coherent
        // path (same PCI config bytes the Linux azx driver programs) so our
        // cached CORB/RIRB/BDL/PCM buffers work without uncached mappings.
        if dev.id.vendor_id == 0x10de {
            let ops = &crate::bus::pci::PortOpsImpl;
            let am = crate::bus::pci::PCI_ACCESS;
            unsafe {
                let v = am.read8(ops, dev.loc, 0x4e);
                am.write8(ops, dev.loc, 0x4e, (v & 0xf0) | 0x0f);
                let v = am.read8(ops, dev.loc, 0x4c);
                am.write8(ops, dev.loc, 0x4c, v | 0x0f);
                let v = am.read8(ops, dev.loc, 0x4d);
                am.write8(ops, dev.loc, 0x4d, v | 0x0f);
            }
        }

        let vaddr = crate::bus::phys_to_virt(addr as usize);
        let name = alloc::format!(
            "hda-{:02x}:{:02x}.{:x}{}",
            dev.loc.bus,
            dev.loc.device,
            dev.loc.function,
            if dev.id.vendor_id == 0x10de {
                " (nvidia-hdmi)"
            } else {
                ""
            }
        );
        let hda = Arc::new(HdaDevice::new(vaddr, name, dev.id.vendor_id == 0x10de)?);
        Ok(Device::Audio(hda))
    }
}
