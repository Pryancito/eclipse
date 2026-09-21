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
//! * **The engine is a DAI, not a one-shot.** As in Sound Open Firmware,
//!   running out of client PCM does not stop the stream: the engine keeps
//!   cycling over zeros, the next write is parked just past wherever it
//!   has fetched up to, and only [`DAI_IDLE_STOP_US`] of silence stops it
//!   (see [`HdaInner::run_gap`]). Every stop costs a full restart on the
//!   next write -- codec verbs, pin sense, the HDMI kick -- and an HDMI
//!   sink that hears its stream restart mutes while it re-locks, so a stop
//!   per underrun turned every short gap into a long one.
//! * **Gain is a ramp.** The software volume is applied on the way into the
//!   ring by `crate::audio::pipeline::volume`, SOF's volume component in
//!   Rust: Q8.16, rounded, and ramped over a few milliseconds so a slider
//!   notch or a mute is a fade and not a click.
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

use crate::audio::pipeline::host::{HostStream, LINK_RATE};
use crate::audio::pipeline::mixer::{accumulate_s16, drain_to_s16};
use crate::audio::pipeline::volume::{gain_from_percent, Volume};
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
const SD_STS: usize = 0x03; // u8: bit2 BCIS, bit3 FIFOE, bit4 DESE (write-1-to-clear), bit5 FIFORDY
const SD_LPIB: usize = 0x04; // u32: link position in cyclic buffer
const SD_FIFOS: usize = 0x10; // u16: FIFO size in bytes
const SD_STS_FIFOE: u8 = 1 << 3;
const SD_STS_DESE: u8 = 1 << 4;
/// Wall clock counter: 32 bits, counts link BCLK periods at 24 MHz. A time
/// base independent of the kernel's TSC-derived clock.
const REG_WALCLK: usize = 0x30;
const WALCLK_HZ: u64 = 24_000_000;
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

/// Wall-clock ceiling for ALL codec reads of one `/proc/gpusnd` dump. Half a
/// single verb timeout: the dump is a diagnostic, and no diagnostic is worth
/// holding the device lock (interrupts off) for longer than that.
const DIAG_CODEC_BUDGET_US: u64 = 100_000;

/// Shared state for the codec reads of one diagnostics dump: a deadline, and a
/// latch set by the first codec that fails to answer. See
/// [`HdaInner::diag_cmd`].
struct DiagBudget {
    deadline_us: u64,
    dead: bool,
}

impl DiagBudget {
    fn new() -> Self {
        Self {
            deadline_us: timer_now_as_micros().wrapping_add(DIAG_CODEC_BUDGET_US),
            dead: false,
        }
    }

    /// True once no further verb may be issued. Latches `dead` on timeout too,
    /// so the report can say the reads were cut short.
    fn spent(&mut self, _inner: &HdaInner) -> bool {
        if self.dead {
            return true;
        }
        if timer_now_as_micros() >= self.deadline_us {
            self.dead = true;
            return true;
        }
        false
    }
}

/// PCM ring: 16 pages = 64 KiB (341 ms of 48 kHz S16LE stereo).
///
/// The ring is the DAI's, not the client's. A client writes into its own
/// [`HostStream`] (see [`HOST_BUFFER`]) and the kernel keeps this ring
/// filled [`FILL_DEPTH`] ahead of the engine with the mix of every stream,
/// so the ring's size is not the latency: what the ring has to hold is the
/// stretch between where the engine has really got to and where the fill
/// lands, plus the guard. That stretch is the position lead -- how far the
/// controller's reported position runs ahead of the link clock -- and it is
/// not small everywhere: QEMU's `intel-hda` fetches for its audio backend's
/// buffer and has been seen 106 KB ahead. With a 64 KiB ring that lead ate
/// the whole write window (free = 0 with nothing audible queued); 256 KiB
/// covers it with room to spare, and costs nothing in latency.
const RING_PAGES: usize = 64;
/// The BDL splits the ring into fixed 8 KiB cyclic segments (8 entries).
const BDL_SEGMENT: usize = 8 * 1024;
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
/// 16 KiB is 85 ms. The play position is derived from the link clock counted
/// from RUN, so what the guard has to absorb is that estimate's error: the
/// link on an HDMI codec may begin consuming some time after RUN, and until
/// it does the estimate runs ahead of the truth by that latency for the life
/// of the stream. Measured at ~0-1 ms where it could be measured, and
/// `/proc/gpusnd` reports it per stream ("link start latency"), so 85 ms is
/// margin over the observed value by two orders of magnitude -- while a
/// guard is dead ring, so it is not free.
const RING_GUARD: usize = 16384;

/// Silence kept ahead of the fill point, so that an engine that runs past
/// the end of what was filled -- the fill loop is driven by the ALSA
/// watchdog and the front ends, and a scheduling hole can hold it up --
/// plays silence, not the previous lap, until the next fill lands. The
/// band is clipped to the write window: it never reaches into the guard
/// behind the play position, for the same reporting-error reason the guard
/// exists.
const SILENCE_AHEAD: usize = 2 * BDL_SEGMENT;

/// How far ahead of the engine the ring is kept filled: the mixer's block,
/// and how long the engine can run before a late fill is heard. Two
/// segments, 85 ms at the link rate, against a fill loop that runs every
/// 4 ms; a bigger figure only adds delay between a client's write and the
/// speaker, since a client's PCM is mixed into the ring this far ahead.
const FILL_DEPTH: usize = 2 * BDL_SEGMENT;

/// The most one fill writes. A fill that finds itself far behind the
/// engine (a stall, or the QEMU lead above at stream start) catches up
/// over a few polls rather than in one block, which bounds the mixing
/// done under the device lock and the scratch that backs it.
const FILL_MAX: usize = 8 * BDL_SEGMENT;

/// Link bytes each client stream holds: the client's `buffer_bytes`, and
/// so its latency when it keeps the buffer full (`/dev/dsp` blocks only on
/// a full buffer). 256 ms at the link rate: what the old ring offered the
/// client once the guard was taken off, so nothing a client sees changed
/// when the ring stopped being its buffer.
const HOST_BUFFER: usize = 48 * 1024;

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
/// in bursts, and the read itself is not instantaneous.
///
/// One guard (4 KiB) was too tight. The reporter's NVIDIA controller advanced
/// its position by 6844 bytes between two polls 150 us apart and had every
/// such update rejected -- 65 in half a second -- which silently reduced the
/// driver to LPIB alone, and the dropouts got WORSE: on that hardware LPIB is
/// the source that over-reports and the position buffer the one that does
/// not, exactly Linux's reason for preferring `POS_FIX_POSBUF`. A legitimate
/// burst is bounded by the BDL segment the engine is working through; a
/// garbage read is anywhere in a 128 KiB ring, so one segment (85 ms) still
/// rejects most of them and the time budget rejects the rest.
const POS_SLACK: usize = BDL_SEGMENT;

/// Shortest interval between two presence-detect *triggers* on a digital pin.
///
/// `GET_PIN_SENSE` on an HDMI/DP pin only reports what the last
/// `SET_PIN_SENSE` latched, so every fresh read costs a trigger verb plus the
/// 2 ms the codec needs to settle -- per pin. That is fine once; it is not
/// fine on the stream-start path, which runs again after every underrun and
/// walks every candidate (an NVIDIA codec exposes four HDMI pins, so ~10 ms
/// of spinning with the device lock held, i.e. interrupts off, before a
/// single sample moves). The restart that the underrun forced then takes
/// long enough to make the next underrun likelier, and the stream stutters
/// its way down instead of recovering.
///
/// A monitor does not appear and disappear inside 100 ms, so a trigger that
/// recent is re-read rather than re-issued. Hotplug is still noticed on the
/// next start after that.
const SENSE_PROBE_MIN_US: u64 = 100_000;

/// Shortest interval between two `kick_hdmi_audio` calls from the write path.
///
/// Re-pushing the monitor's ELD goes through the display driver and the GSP
/// display path, which is expensive and takes GPU locks. The write path asks
/// for it whenever a digital pin is not reporting a live sink at stream
/// start, and "not live" is the steady state of an HDMI codec with nothing
/// plugged into it -- so every underrun-driven restart paid for a full ELD
/// push that had already failed to change anything a moment earlier, and the
/// restart it was supposed to help took longer than the gap that caused it.
///
/// A push that did not take will not take any better a second later.
const HDMI_KICK_MIN_US: u64 = 1_000_000;

/// How long the engine keeps running over silence once the ring has run out
/// of client PCM, before it is stopped.
///
/// This is the DAI side of Sound Open Firmware (`src/audio/dai-zephyr.c`):
/// the DMA keeps cycling while the pipeline is up, and when the host has
/// nothing to give it is handed zeros -- the stop is a decision the host
/// makes, not something an empty buffer triggers. Stopping here instead
/// cost far more than the gap that caused it. Every restart re-runs the
/// codec verbs, the pin-sense probe and the HDMI kick, and an HDMI or DP
/// sink that sees its audio stream stop and start again mutes while it
/// re-locks, which on a television is hundreds of milliseconds: a 10 ms
/// hole in the data became a half-second hole in the sound, at every
/// underrun of a client that never noticed one. 5 s is PulseAudio's own
/// idle timeout for a sink; a client that stays quiet for longer than that
/// is not between two notes.
const DAI_IDLE_STOP_US: u64 = 5_000_000;

const STOP_HISTORY: usize = 4;

/// One gap (the mix running out of client PCM), as recorded for
/// `/proc/gpusnd`.
#[derive(Clone, Copy, Default)]
struct StopEvent {
    /// `b'U'` underrun (a started stream had nothing to give), `b'D'` drain
    /// (no stream was started), 0 = unused slot.
    kind: u8,
    /// Milliseconds after the stream started, by the kernel clock.
    at_ms: u64,
    /// The same interval by the controller's 24 MHz wall clock.
    wall_ms: u64,
    /// Bytes of mixed PCM written to the ring by then.
    written: usize,
}

/// How many bytes the next fill writes at `fill_pos`, all positions being
/// bytes since RUN.
///
/// The fill keeps the ring `depth` bytes ahead of the furthest point the
/// engine can have reached: `consumed` is the link clock's play position
/// and `reported` the controller's own counter, whichever of the two runs
/// ahead (the counter over-reports on some controllers and QEMU's fetches
/// far ahead of what it plays; the clock is a floor). It never writes
/// within `guard` of the play position from behind, i.e. more than
/// `ring - guard` past `consumed`: that is where the guard's reporting
/// error lives, and PCM written there overwrites what is still playing.
/// Whole frames only, and nothing when the fill is already at or past its
/// target -- or when the lead has eaten the whole window, which a ring
/// sized for it (see [`RING_PAGES`]) does not let happen.
fn fill_span(
    fill_pos: u64,
    consumed: u64,
    reported: u64,
    ring: usize,
    guard: usize,
    depth: usize,
    frame: usize,
) -> usize {
    if frame == 0 {
        return 0;
    }
    let ahead = consumed.max(reported).saturating_add(depth as u64);
    let window = consumed.saturating_add(ring.saturating_sub(guard) as u64);
    let target = ahead.min(window);
    (target.saturating_sub(fill_pos) as usize) / frame * frame
}

/// One block of the mixer (SOF `mixer`, `mix_n_s16`): pull `out.len()`
/// link bytes from every stream in `streams` into the `i32` accumulator
/// `acc`, saturate once into `out`. A stream with less than the block
/// queued, or one that is not active, contributes silence past what it
/// gives (the pull answers 0 for it). `pull` is scratch of at least
/// `out.len()` bytes and `acc` of at least half that many entries; both
/// are the caller's so the fill allocates nothing. Returns how many
/// streams contributed anything -- zero is a gap.
fn mix_streams(
    streams: &mut [(u32, HostStream)],
    acc: &mut [i32],
    pull: &mut [u8],
    out: &mut [u8],
) -> usize {
    let span = out.len().min(pull.len());
    let samples = (span / 2).min(acc.len());
    // `acc` is clear on entry: it starts zeroed and `drain_to_s16` leaves
    // it zeroed, so no block carries a sum into the next.
    let acc = &mut acc[..samples];
    let mut contributors = 0;
    for (_, stream) in streams.iter_mut() {
        let n = stream.pull(&mut pull[..span]);
        if n > 0 {
            accumulate_s16(acc, &pull[..n]);
            contributors += 1;
        }
    }
    drain_to_s16(acc, out);
    contributors
}

// ── MMIO helpers ────────────────────────────────────────────────────────────
//
// In the test binary the BAR is host memory owned by `hda_fake`, and every
// access also tells the fake, which is what makes the registers behave
// (a verb posted through CORBWP is answered in the RIRB, a RUN bit moves
// the engine). Outside tests the hooks compile to nothing.
fn mmio_r8(bar: usize, off: usize) -> u8 {
    #[cfg(test)]
    hda_fake::on_read(bar, off);
    unsafe { read_volatile((bar + off) as *const u8) }
}
fn mmio_r16(bar: usize, off: usize) -> u16 {
    #[cfg(test)]
    hda_fake::on_read(bar, off);
    unsafe { read_volatile((bar + off) as *const u16) }
}
fn mmio_r32(bar: usize, off: usize) -> u32 {
    #[cfg(test)]
    hda_fake::on_read(bar, off);
    unsafe { read_volatile((bar + off) as *const u32) }
}
fn mmio_w8(bar: usize, off: usize, v: u8) {
    unsafe { write_volatile((bar + off) as *mut u8, v) }
    #[cfg(test)]
    hda_fake::on_write(bar, off, v as u32);
}
fn mmio_w16(bar: usize, off: usize, v: u16) {
    unsafe { write_volatile((bar + off) as *mut u16, v) }
    #[cfg(test)]
    hda_fake::on_write(bar, off, v as u32);
}
fn mmio_w32(bar: usize, off: usize, v: u32) {
    unsafe { write_volatile((bar + off) as *mut u32, v) }
    #[cfg(test)]
    hda_fake::on_write(bar, off, v);
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

#[cfg(not(test))]
fn wait_us(us: u64) {
    let start = timer_now_as_micros();
    while timer_now_as_micros().wrapping_sub(start) < us {
        core::hint::spin_loop();
    }
}

/// The test clock only moves when something moves it, so a wait is the
/// same thing as the time passing.
#[cfg(test)]
fn wait_us(us: u64) {
    crate::nvme::nvme_queue::test_clock::advance(us);
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
    /// LPIB at the last progress poll, for the delta the next one accepts.
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
    /// Bytes since RUN the ring is filled up to: mixed PCM, or zeros during
    /// a gap. The mixer writes here and only here; see [`fill_span`].
    fill_pos: u64,
    /// Bytes since RUN up to which the ring is known to be zero past
    /// `fill_pos` (the [`SILENCE_AHEAD`] band), so the band is rewritten
    /// incrementally rather than per fill.
    zero_end: u64,
    /// Bytes of mixed PCM (a fill with at least one contributing stream)
    /// written to the ring since RUN, for the gap record.
    mixed_bytes: usize,
    /// Bytes the link has taken from the ring since RUN: the smaller of what
    /// its own clock says and what the controller's position reports. This is
    /// the play position everything else is derived from.
    consumed: u64,
    /// `WALCLK` at the last poll, and link-clock ticks accumulated while
    /// running (pauses excluded, 32-bit wrap handled by polling often).
    wall_last: u32,
    wall_ticks: u64,
    /// Accepted advance of each reported position since RUN, unwrapped.
    lpib_total: u64,
    dpib_total: u64,
    /// The most the reported position has been seen to run ahead of the
    /// link clock, in bytes, and where it stands now. The maximum is the
    /// engine's prefetch depth (it fills before the link starts); the
    /// difference between the two is how long the link took to start
    /// consuming after RUN -- the latency the guard has to cover.
    stat_lead: u64,
    lead_now: u64,

    /// Counters behind the `/proc/gpusnd` "events" line. A tone that plays
    /// with short repeated dropouts sounds the same whatever produced it;
    /// these say which of them actually happened on this machine.
    /// `drains` and `underruns` both open a gap (a fill with no stream to
    /// mix): an underrun is a started stream found empty -- the client
    /// fell behind -- and a drain is no stream being started at all.
    /// `idle_stops` is a gap lasting [`DAI_IDLE_STOP_US`] and the engine
    /// being stopped for it, `restarts` the full stream reset (codec verbs
    /// and all) on the next start, and `stop_timeouts` the engine not
    /// acknowledging a cleared RUN bit -- the case where wiping the ring
    /// would race a still-fetching DMA.
    stat_drains: u64,
    stat_underruns: u64,
    stat_idle_stops: u64,
    stat_restarts: u64,
    /// Fills that found the engine already past the fill point: the fill
    /// loop was held up for longer than [`FILL_DEPTH`] plus the silence
    /// band, and the engine played a lap-old ring for the difference. The
    /// fill skips to the engine, so a stall delays the streams' PCM rather
    /// than dropping it.
    stat_late_fills: u64,
    stat_stop_timeouts: u64,
    /// Position-register reads (WALCLK + LPIB per poll) timed: how many, the
    /// slowest, and how many took over 1 ms. Under a hypervisor each read is
    /// a VM exit serviced by the emulator's main loop; one that waits behind
    /// a busy display thread stalls this CPU for as long as it waits, and it
    /// shows here before it shows as an underrun.
    stat_pos_reads: u64,
    stat_pos_read_max_us: u64,
    stat_pos_reads_slow: u64,
    /// Position reads rejected as impossible: more progress than the PCM
    /// byte rate allows in the time since the last accepted read. A stream
    /// whose position register or buffer occasionally returns garbage --
    /// a controller behind a GPU, a bus hiccup -- would otherwise have the
    /// writer overwrite most of a lap of unplayed audio, or be declared
    /// underrun, on one bad read. The last rejected value is kept.
    stat_bad_pos: u64,
    last_bad_pos: u32,
    /// The accepted value it was judged against, and the interval.
    last_bad_prev: u32,
    last_bad_dt_us: u64,
    /// Which source produced it: `b'L'` LPIB, `b'P'` position buffer.
    last_bad_src: u8,
    /// `SD_STS` errors seen (and cleared) while polling: FIFOE is the engine
    /// failing to keep its FIFO fed from memory -- the link then plays
    /// whatever it has, i.e. gaps -- and DESE a bad buffer descriptor.
    stat_fifo_err: u64,
    stat_desc_err: u64,
    /// RIRB entries discarded before posting a verb: unsolicited jack events,
    /// plus the late answers to verbs that timed out. A non-zero count is the
    /// only visible trace of the response stream having slipped. See
    /// [`HdaInner::drain_rirb`].
    stat_stale_resp: u64,
    /// Output re-routes decided by [`HdaInner::repick_path`], and the last
    /// one (`from pin`, `to pin`, ms since boot). A stream that restarts on
    /// the wrong pin plays into a connector nobody is listening to, with
    /// every call returning `Ok`; this is how that shows up.
    stat_reroutes: u64,
    last_reroute: (u32, u32, u64),
    /// `(pin, timer_now_as_micros())` at each pin's last presence-detect
    /// trigger, for [`SENSE_PROBE_MIN_US`]. Per pin, because a walk over the
    /// candidates has to latch a fresh result on every one of them -- a
    /// single shared timestamp would let the first pin suppress the rest and
    /// they would all score as absent. One entry per digital pin.
    sense_probe: Vec<(u32, u64)>,
    /// `timer_now_as_micros()` at the last `kick_hdmi_audio` asked for by the
    /// write path, for [`HDMI_KICK_MIN_US`]. 0 = never.
    last_kick_us: u64,

    /// `WALCLK` when the current stream started.
    stream_start_wall: u32,
    /// When the current stream started, so a gap can be placed within it.
    stream_start_us: u64,
    /// The last few stops, newest last: WHEN in the stream and after how
    /// much data, which is what tells a stop mid-tone from the tone ending.
    stops: [StopEvent; STOP_HISTORY],

    rate: u32,
    channels: u8,

    /// `timer_now_as_micros()` when the mix last ran out of client PCM
    /// (a fill with no contributing stream: a gap), or 0 while it has some.
    /// The engine plays silence for the gap's duration and is stopped after
    /// [`DAI_IDLE_STOP_US`] of it.
    gap_since_us: u64,

    /// Software playback gain (HDMI has no analog volume), applied to the
    /// mix on its way into the ring so OSS and ALSA share one control. The
    /// percent/mute pair is what the mixer control reads back; `vol` is the
    /// ramped Q8.16 gain the copy actually applies (see
    /// `crate::audio::pipeline::volume`), so a slider notch or a mute is a
    /// fade over a few milliseconds of audio and not a step in the waveform.
    gain_l: u8,
    gain_r: u8,
    mute_l: bool,
    mute_r: bool,
    vol: Volume,

    /// The host side of the pipeline: one [`HostStream`] per open, by id.
    /// Every client writes into its own; the fill loop pulls from all of
    /// them and mixes. The device's own trait implementation is stream 0,
    /// which lives as long as the device.
    streams: Vec<(u32, HostStream)>,
    next_stream: u32,
    /// Scratch for one fill: the `i32` mix accumulator, the saturated mix
    /// block, and one stream's pull. Sized once for [`FILL_MAX`], so the
    /// fill allocates nothing.
    mix_acc: Vec<i32>,
    mix_buf: Vec<u8>,
    pull_buf: Vec<u8>,
}

pub struct HdaDevice {
    name: String,
    /// PCI vendor is NVIDIA: this HDA function lives on a GPU, so its audio
    /// only reaches the cable when the display engine transmits it.
    is_nvidia: bool,
    inner: Arc<Mutex<HdaInner>>,
    /// The device's own stream, for callers that use the device as a
    /// [`AudioScheme`] directly rather than through
    /// [`open_stream`](AudioScheme::open_stream).
    own: HdaStream,
}

/// One client's handle on an [`HdaDevice`]: a [`HostStream`] in the
/// device's table, mixed with every other open's. Dropping it removes the
/// stream; the engine notices on its next fill.
pub struct HdaStream {
    inner: Arc<Mutex<HdaInner>>,
    id: u32,
    name: String,
}

impl Drop for HdaStream {
    fn drop(&mut self) {
        // Take the stream out under the lock, free it after: the lock is
        // IRQ-off and the stream owns a buffer.
        let removed = {
            let mut inner = self.inner.lock();
            let at = inner.streams.iter().position(|(id, _)| *id == self.id);
            at.map(|i| inner.streams.swap_remove(i))
        };
        drop(removed);
    }
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

    /// Drop every response sitting in the RIRB, and count them.
    ///
    /// Verbs are issued one at a time under the device lock and answered by
    /// polling, so at the moment a verb is posted the ring can only hold
    /// responses that are NOT its own: an unsolicited jack event, or the late
    /// answer to a verb that already gave up (`VERB_TIMEOUT_US`). Leaving
    /// those in place is what makes one timeout poison the rest of the
    /// session -- the next verb reads the stale entry as its own reply, and
    /// every verb after it is answered by the previous one's response. The
    /// codec graph is then read through a one-entry shift: pin caps come back
    /// as connection lists, presence as config defaults, and the driver
    /// happily routes to a pin that does not exist. It plays into that route
    /// with no error at all, and nothing short of a reboot resynchronises it.
    ///
    /// Linux does not need this because it correlates each response with the
    /// command that produced it (`azx_rirb_get_response` counts outstanding
    /// commands); draining before the doorbell is the same guarantee for a
    /// transport that sends exactly one verb at a time.
    fn drain_rirb(&mut self) {
        let hw_wp = (mmio_r16(self.bar, REG_RIRBWP) & 0xff) as usize % self.rirb_entries;
        if self.rirb_rp == hw_wp {
            return;
        }
        while self.rirb_rp != hw_wp {
            self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
            self.stat_stale_resp += 1;
        }
        // Those entries counted against `RINTCNT`, and QEMU holds CORB DMA
        // until the status is acked (see [`HdaInner::ack_rirb`]). Acking
        // again here keeps the doorbell that follows from ringing into a
        // stalled ring.
        self.ack_rirb();
    }

    // ── Codec verb transport (CORB/RIRB, polled) ────────────────────────────
    fn corb_cmd(&mut self, verb: u32) -> DeviceResult<u32> {
        let bar = self.bar;
        // Unstick a previous RINTCNT stall before ringing the doorbell.
        self.ack_rirb();
        // Nothing already in the ring can answer the verb we are about to
        // post. See [`HdaInner::drain_rirb`].
        self.drain_rirb();
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

    /// Diagnostics-only codec read, under a shared budget.
    ///
    /// `corb_cmd` waits up to `VERB_TIMEOUT_US` (200 ms) for a response, and
    /// the `/proc/gpusnd` dump issues a dozen-plus verbs -- all of them with
    /// the device lock held, which on this kernel means INTERRUPTS OFF. A
    /// codec that stopped answering therefore turned one `cat /proc/gpusnd`
    /// into seconds of IRQ-off time: the reader wedged, every other CPU that
    /// touched audio piled up behind the same ticket (PulseAudio writes every
    /// few ms), and the serial console went with them, while an unrelated
    /// compositor on another core kept running -- exactly the "everything but
    /// labwc is dead" freeze.
    ///
    /// A codec that missed one verb will miss the rest, so the first failure
    /// disables the remainder: the worst case is one timeout for the whole
    /// dump instead of one per verb. The deadline bounds the healthy-but-slow
    /// case as well.
    fn diag_cmd(&mut self, nid: u32, verb: u32, budget: &mut DiagBudget) -> Option<u32> {
        if budget.spent(self) {
            return None;
        }
        match self.cmd(nid, verb, 0) {
            Ok(v) => Some(v),
            Err(_) => {
                budget.dead = true;
                None
            }
        }
    }

    /// 16-bit-payload variant of [`HdaInner::diag_cmd`].
    fn diag_cmd16(&mut self, nid: u32, verb: u32, budget: &mut DiagBudget) -> Option<u32> {
        if budget.spent(self) {
            return None;
        }
        match self.cmd16(nid, verb, 0) {
            Ok(v) => Some(v),
            Err(_) => {
                budget.dead = true;
                None
            }
        }
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

    /// Keep [`SILENCE_AHEAD`] bytes of zeros immediately after the fill
    /// point, clipped to the write window (never into the guard behind the
    /// play position). Incremental: `zero_end` remembers how far the band
    /// already reaches, so only the bytes a fill newly exposed are written.
    fn keep_silence_ahead(&mut self) {
        let ring = self.ring_len as u64;
        let window = self.consumed + ring.saturating_sub(RING_GUARD as u64);
        let want = (self.fill_pos + SILENCE_AHEAD as u64).min(window);
        if self.zero_end < self.fill_pos {
            self.zero_end = self.fill_pos;
        }
        if want > self.zero_end {
            let len = (want - self.zero_end) as usize;
            let start = (self.zero_end % ring) as usize;
            self.zero_range(start, len);
            self.zero_end = want;
        }
    }

    /// Milliseconds since the stream started by the controller's own clock.
    fn stream_wall_ms(&self) -> u64 {
        let ticks = mmio_r32(self.bar, REG_WALCLK).wrapping_sub(self.stream_start_wall);
        ticks as u64 * 1000 / WALCLK_HZ
    }

    fn record_stop(&mut self, kind: u8) {
        self.stops.copy_within(1.., 0);
        self.stops[STOP_HISTORY - 1] = StopEvent {
            kind,
            at_ms: timer_now_as_micros().wrapping_sub(self.stream_start_us) / 1000,
            wall_ms: self.stream_wall_ms(),
            written: self.mixed_bytes,
        };
    }

    /// Read and clear the stream's sticky error bits, counting them.
    fn poll_stream_errors(&mut self) {
        let sts = mmio_r8(self.bar, self.sd_base + SD_STS);
        if sts & (SD_STS_FIFOE | SD_STS_DESE) != 0 {
            if sts & SD_STS_FIFOE != 0 {
                self.stat_fifo_err += 1;
            }
            if sts & SD_STS_DESE != 0 {
                self.stat_desc_err += 1;
            }
            mmio_w8(
                self.bar,
                self.sd_base + SD_STS,
                sts & (SD_STS_FIFOE | SD_STS_DESE),
            );
        }
    }

    /// Fold DMA progress since the last poll into the play position, then
    /// run the DAI side: fill the ring ahead of the engine with the mix of
    /// every stream (see [`HdaInner::fill_ring`]). Called from every front
    /// end call and from the ALSA watchdog every 4 ms while the engine
    /// runs, which is what paces the fill.
    fn poll_progress(&mut self) {
        if !self.running {
            return;
        }
        // Rate-limit the device reads (see [`LPIB_POLL_MIN_US`]). A gap is
        // handled below from a fresh position read, so it waits for the
        // next accepted read like everything else: the ring is all zeros
        // by then and the engine plays silence in the meantime.
        let now_us = timer_now_as_micros();
        let dt_us = now_us.wrapping_sub(self.last_poll_us);
        if dt_us < LPIB_POLL_MIN_US {
            return;
        }
        let ring = self.ring_len;
        let rate_bytes = self.rate as u64 * self.frame_bytes() as u64;

        // The link clock is the play position. The link takes exactly the
        // PCM byte rate per second of its own 24 MHz clock from the moment
        // RUN is set; the controller's position registers, on the other
        // hand, report where its DMA engine has got to, and on the hardware
        // this was written against that ran 15-27 KB ahead of the link and
        // moved in bursts of several KB -- far more than any guard, and the
        // writer was handed unplayed audio to overwrite on every burst. The
        // registers still serve as a cap: the link cannot have played what
        // the engine has not fetched, so if the engine stalls the clock
        // estimate cannot run away from it.
        let t_read = timer_now_as_micros();
        let wall = mmio_r32(self.bar, REG_WALCLK);
        self.wall_ticks += wall.wrapping_sub(self.wall_last) as u64;
        self.wall_last = wall;
        // Whole frames, rounded down: the link takes samples, not bytes.
        let frame = self.frame_bytes() as u64;
        let by_clock = (self.wall_ticks as u128 * rate_bytes as u128 / WALCLK_HZ as u128) as u64
            / frame
            * frame;

        // The most a reported position can have advanced since the last
        // ACCEPTED read. `last_poll_us` only moves on an accepted read, so a
        // run of bad reads keeps widening the budget until a sane one gets
        // through.
        let max_advance = (dt_us.saturating_mul(rate_bytes) / 1_000_000) as usize + POS_SLACK;
        let lpib_raw = self.lpib();
        let read_us = timer_now_as_micros().wrapping_sub(t_read);
        self.stat_pos_reads += 1;
        if read_us > self.stat_pos_read_max_us {
            self.stat_pos_read_max_us = read_us;
        }
        if read_us > 1000 {
            self.stat_pos_reads_slow += 1;
        }
        let lpib = lpib_raw as usize % ring;
        let advanced = (lpib + ring - self.last_lpib as usize % ring) % ring;
        let lpib_ok = if advanced > max_advance {
            // Impossible progress (a backwards step shows up here too, as
            // nearly a full lap forward). Anomaly recorded.
            self.stat_bad_pos += 1;
            self.last_bad_pos = lpib_raw;
            self.last_bad_prev = self.last_lpib;
            self.last_bad_dt_us = dt_us;
            self.last_bad_src = b'L';
            false
        } else {
            self.last_lpib = lpib as u32;
            self.lpib_total += advanced as u64;
            true
        };
        self.poll_stream_errors();
        let mut dpib_ok = false;
        if let Some(dpib_raw) = self.dma_pos() {
            let dpib = dpib_raw as usize % ring;
            let advanced = (dpib + ring - self.last_dpib as usize % ring) % ring;
            if advanced > max_advance {
                self.stat_bad_pos += 1;
                self.last_bad_pos = dpib_raw;
                self.last_bad_prev = self.last_dpib;
                self.last_bad_dt_us = dt_us;
                self.last_bad_src = b'P';
            } else {
                if advanced > 0 {
                    // Only believed once seen to move: a controller that takes
                    // the base address and never writes to it would otherwise
                    // report zero progress forever.
                    self.dpib_trusted = true;
                }
                self.last_dpib = dpib as u32;
                self.dpib_total += advanced as u64;
                dpib_ok = true;
            }
        }
        if !lpib_ok && !dpib_ok {
            return;
        }
        self.last_poll_us = now_us;
        let mut reported = if lpib_ok {
            self.lpib_total
        } else {
            self.dpib_total
        };
        if dpib_ok && self.dpib_trusted {
            reported = if lpib_ok {
                reported.min(self.dpib_total)
            } else {
                self.dpib_total
            };
        }
        self.lead_now = reported.saturating_sub(by_clock);
        self.stat_lead = self.stat_lead.max(self.lead_now);

        // Consumed is the smaller of the two, and never goes backwards.
        self.consumed = self.consumed.max(by_clock.min(reported));
        self.fill_ring(now_us, reported);
    }

    /// The DAI side of the pipeline (SOF's `dai` component, fed by its
    /// mixer): keep the ring [`FILL_DEPTH`] ahead of the engine with the
    /// mix of every active stream, zeros where none has PCM. A fill that
    /// mixes nothing opens a gap; the engine keeps running over silence
    /// through it, as SOF's DAI does, and is stopped after
    /// [`DAI_IDLE_STOP_US`] of it. `reported` is the controller's own
    /// position, for [`fill_span`].
    fn fill_ring(&mut self, now_us: u64, reported: u64) {
        let frame = self.frame_bytes() as u64;
        let reach = self.consumed.max(reported) / frame * frame;
        if self.fill_pos < reach {
            // The engine got past the fill: what it played from there was
            // the silence band and then a lap-old ring. Skip to it; the
            // streams' PCM is late, not lost.
            self.stat_late_fills += 1;
            self.fill_pos = reach;
        }
        let span = fill_span(
            self.fill_pos,
            self.consumed,
            reported,
            self.ring_len,
            RING_GUARD,
            FILL_DEPTH,
            frame as usize,
        )
        .min(FILL_MAX);
        if span > 0 {
            let contributors = self.mix_block(span);
            if contributors == 0 {
                if self.gap_since_us == 0 {
                    let starved = self.streams.iter().any(|(_, s)| s.is_active());
                    if starved {
                        self.stat_underruns += 1;
                        self.record_stop(b'U');
                    } else {
                        self.stat_drains += 1;
                        self.record_stop(b'D');
                    }
                    self.gap_since_us = now_us;
                }
            } else {
                self.gap_since_us = 0;
                self.mixed_bytes += span;
            }
            let block = core::mem::take(&mut self.mix_buf);
            self.copy_to_ring(self.fill_pos, &block[..span]);
            self.mix_buf = block;
            self.fill_pos += span as u64;
        }
        self.keep_silence_ahead();
        if self.gap_since_us != 0 && now_us.wrapping_sub(self.gap_since_us) >= DAI_IDLE_STOP_US {
            self.stat_idle_stops += 1;
            self.stop_engine();
        }
    }

    /// Mix the next `span` link bytes of every active stream into
    /// `mix_buf[..span]`; see [`mix_streams`].
    fn mix_block(&mut self, span: usize) -> usize {
        let mut acc = core::mem::take(&mut self.mix_acc);
        let mut pull = core::mem::take(&mut self.pull_buf);
        let n = mix_streams(
            &mut self.streams,
            &mut acc,
            &mut pull,
            &mut self.mix_buf[..span],
        );
        self.mix_acc = acc;
        self.pull_buf = pull;
        n
    }

    /// Copy a mixed block into the ring at stream position `pos`, through
    /// the master gain, wrapping at the end of the ring and flushing every
    /// line written (the engine's DMA does not snoop).
    fn copy_to_ring(&mut self, pos: u64, block: &[u8]) {
        let ring = self.ring_len;
        let mut p = (pos % ring as u64) as usize;
        let mut done = 0;
        while done < block.len() {
            let chunk = (block.len() - done).min(ring - p);
            let dst = self.ring_va + p;
            self.copy_pcm_scaled(&block[done..done + chunk], dst, chunk);
            clflush_range(dst, chunk);
            p = (p + chunk) % ring;
            done += chunk;
        }
    }

    /// Start the engine if it is stopped and some stream has PCM to play.
    /// Every path that can give a stopped engine something to do -- a
    /// write, a resume, a start hold released -- ends here.
    fn ensure_engine(&mut self) -> DeviceResult {
        if self.running {
            return Ok(());
        }
        if !self
            .streams
            .iter()
            .any(|(_, s)| s.is_active() && s.queued_link() > 0)
        {
            return Ok(());
        }
        self.start_engine()
    }

    /// Bring the DAI up: re-pick the output path (on NVIDIA GPUs the ELD
    /// lands long after PCI probe), zero the ring, prime [`FILL_DEPTH`] of
    /// mix at offset 0, and set RUN.
    fn start_engine(&mut self) -> DeviceResult {
        warn!(
            "[hda] engine start: {} stream(s), repick + stream reset next",
            self.streams.len()
        );
        self.repick_path();
        // Positions first, then the wipe: `silence_ring` books the zeros
        // relative to `fill_pos`.
        self.fill_pos = 0;
        self.silence_ring();
        self.mixed_bytes = 0;
        self.gap_since_us = 0;
        self.consumed = 0;
        self.lpib_total = 0;
        self.dpib_total = 0;
        if self.digital {
            let ch = self.channels;
            self.send_audio_infoframe(ch);
        }
        let now = timer_now_as_micros();
        self.fill_ring(now, 0);
        self.start_stream()?;
        warn!(
            "[hda] engine started: {} B primed, CTL {:#x}",
            self.fill_pos,
            mmio_r32(self.bar, self.sd_base + SD_CTL)
        );
        Ok(())
    }

    /// Stop the engine and forget the ring's positions. The streams keep
    /// theirs: whatever they hold plays when the engine next starts.
    fn stop_engine(&mut self) {
        if self.stop_stream() {
            self.silence_ring();
        }
        self.gap_since_us = 0;
        self.fill_pos = 0;
        self.zero_end = 0;
        self.last_lpib = 0;
    }

    /// Add `stream` to the mix and return its id. The stream is built by
    /// the caller ([`HdaInner::new_stream`]), outside the lock: it owns a
    /// buffer, and the lock is IRQ-off.
    fn add_stream(&mut self, stream: HostStream) -> u32 {
        let id = self.next_stream;
        self.next_stream += 1;
        self.streams.push((id, stream));
        id
    }

    fn new_stream() -> HostStream {
        HostStream::new(HOST_BUFFER, 2)
    }

    fn stream_mut(&mut self, id: u32) -> Option<&mut HostStream> {
        self.streams
            .iter_mut()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s)
    }

    /// Link bytes between the play position and the fill point: what the
    /// ring still has to play before a stream's next pull is heard.
    fn ring_ahead(&self) -> usize {
        if !self.running {
            return 0;
        }
        self.fill_pos.saturating_sub(self.consumed) as usize
    }

    /// Copy `src` into the ring at `dst` (virtual address), applying the
    /// current (ramped) gain. `len` is a whole number of S16LE frames (the
    /// write path never splits a frame across the ring wrap); a trailing
    /// partial frame is copied byte for byte rather than left holding the
    /// previous lap.
    fn copy_pcm_scaled(&mut self, src: &[u8], dst: usize, len: usize) {
        if self.vol.is_passthrough() {
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, len);
            }
            return;
        }
        // SAFETY: `dst..dst + len` lies inside the ring, which is owned by
        // this device for its lifetime and only ever written under its lock.
        let out = unsafe { core::slice::from_raw_parts_mut(dst as *mut u8, len) };
        self.vol.process(&src[..len], out);
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
            // Force stream reset (SRST = bit 0) to halt DMA bus-mastering engine (§3.3.35)
            let ctl = mmio_r32(self.bar, self.sd_base + SD_CTL);
            mmio_w32(self.bar, self.sd_base + SD_CTL, (ctl & !0x2) | 0x1);
            let t_rst = timer_now_as_micros();
            while mmio_r32(self.bar, self.sd_base + SD_CTL) & 0x1 == 0 {
                if timer_now_as_micros().wrapping_sub(t_rst) > 1_000 {
                    break;
                }
                core::hint::spin_loop();
            }
            mmio_w32(self.bar, self.sd_base + SD_CTL, 0);
            // SRST clears the descriptor, tag included. Every start goes
            // through the full programming path, so nothing else to do.
        }
        self.running = false;
        stopped
    }

    fn silence_ring(&mut self) {
        unsafe { core::ptr::write_bytes(self.ring_va as *mut u8, 0, self.ring_len) };
        clflush_range(self.ring_va, self.ring_len);
        // Every position up to a full lap past the fill point is zero now.
        self.zero_end = self.fill_pos + self.ring_len as u64;
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

        // The position buffer entry still holds the previous stream's last
        // value until the controller writes a new one; read after RUN it
        // would look like an impossible jump from 0 and be rejected. Clear it.
        if self.dma_pos_va != 0 {
            // SAFETY: our own page, this stream's 8-byte entry.
            unsafe { core::ptr::write_volatile(self.dma_pos_va as *mut u32, 0) };
            clflush_range(self.dma_pos_va, 8);
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
        self.stat_restarts += 1;
        self.stream_start_us = self.last_poll_us;
        self.stream_start_wall = mmio_r32(bar, REG_WALCLK);
        self.wall_last = self.stream_start_wall;
        self.wall_ticks = 0;
        self.lpib_total = 0;
        self.dpib_total = 0;
        self.consumed = 0;
        self.stat_lead = 0;
        self.lead_now = 0;
        // `fill_pos` is not reset here: the ring was primed before RUN.
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
                       // `channels.max(1)`, as `frame_bytes`, `setup_digital_converter` and
                       // `send_audio_infoframe` all do with the same field. A bare
                       // `channels - 1` underflows on zero: a panic in debug, and in release
                       // `0xFFFF & 0xf` == 15, i.e. sixteen channels. `set_params` pins this to
                       // stereo today ("stereo only for now"), so zero cannot arrive yet -- this
                       // is here so that lifting that line stays a one-line change.
    fmt |= (channels.max(1) as u16 - 1) & 0xf;
    fmt
}

/// The HDA rate table, and the nearest entry to `rate`. The link is fixed at
/// [`LINK_RATE`] now (a client at any other rate is resampled), so nothing
/// at runtime snaps to this table any more; the tests keep it as the record
/// of what [`stream_format`] can encode.
#[cfg(test)]
fn nearest_rate(rate: u32) -> u32 {
    const RATES: [u32; 11] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
    ];
    *RATES
        .iter()
        .min_by_key(|&&r| (r as i64 - rate as i64).unsigned_abs())
        .unwrap_or(&48000)
}

/// Which of the scored candidate paths to play on, given the one in use.
///
/// The highest score wins, but a route change is only worth its cost -- a
/// full path re-arm on a stream that is restarting anyway -- when it leads
/// somewhere better, and a momentary dip in the current pin's sense bits is
/// not that. So the current path keeps its place on a tie, and it is never
/// abandoned for a pin that does not itself report presence. On the NVIDIA
/// codecs this runs against, a `SET_PIN_SENSE` right after the stream stops
/// can read back PD=0 on the monitor's own pin for a moment; scoring alone
/// then sent every underrun-triggered restart to the first dead connector
/// in the list, and the next restart brought it back -- the same track
/// audible one time and silent the next, with every call returning `Ok`.
fn choose_route(scored: &[(OutPath, i32)], current: Option<(u32, u32)>) -> Option<&OutPath> {
    let is_current = |p: &OutPath| current == Some((p.pin, p.conv));
    let mut best: Option<&(OutPath, i32)> = None;
    for cand in scored {
        let better = match best {
            None => true,
            Some(b) => cand.1 > b.1 || (cand.1 == b.1 && is_current(&cand.0)),
        };
        if better {
            best = Some(cand);
        }
    }
    let (best, _) = best?;
    if current.is_some() && !is_current(best) && !best.present {
        // Nothing live to move to: stay where the audio last came out.
        return scored.iter().map(|(p, _)| p).find(|p| is_current(p));
    }
    Some(best)
}

#[cfg(test)]
mod format_and_ring_tests {
    //! The arithmetic this driver does between the client's PCM and the
    //! controller's own counters. None of it touches MMIO, and all of it is
    //! the kind that fails quietly: a wrong bit in the stream format plays
    //! the track at the wrong speed, a byte count that is not a whole number
    //! of frames swaps the channels for the rest of the stream, and a
    //! position that runs backwards is what the intermittent dropouts were.

    use super::{nearest_rate, stream_format, sub_nodes};
    use crate::audio::pipeline::host::{
        accept_client_frames, client_buffer_bytes, client_counts, client_to_link_bytes,
        link_to_client_bytes, CLIENT_RATE_MAX, CLIENT_RATE_MIN, LINK_RATE, SRC_SLACK_FRAMES,
    };
    use crate::audio::pipeline::src::Resampler;

    /// `client_counts(..).1` on its own, for the tests below.
    fn client_queued_bytes(
        buffer_link: usize,
        queued_link: usize,
        fin: u32,
        fout: u32,
        frame: usize,
    ) -> usize {
        client_counts(buffer_link, queued_link, Some((fin, fout)), frame).1
    }

    /// Rates a desktop actually plays, paired against the fixed link.
    const CLIENT_RATES: [u32; 9] = [
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 96000, 192_000,
    ];

    /// The invariant the whole fixed-rate sink rests on: whatever the ring
    /// has free, the client frames `accept_client_frames` sizes never
    /// convert into more link frames than that. If they did, `write` would
    /// copy past the ring. Checked against the converter's own sizing hint
    /// for every rate and every free count up to a full ring.
    #[test]
    fn an_accepted_write_always_fits_after_conversion() {
        for &fin in &CLIENT_RATES {
            let r = Resampler::new(fin, LINK_RATE, 2);
            for free in 0..=16384usize {
                let take = accept_client_frames(free, fin, LINK_RATE);
                let out = r.out_frames_hint(take);
                assert!(
                    take == 0 || out <= free,
                    "{} Hz: {} free link frames, took {} client frames -> {} link frames",
                    fin,
                    free,
                    take,
                    out
                );
            }
        }
    }

    /// The converter's real output, not just its hint, fits too: run the
    /// sized take through it, chunk after chunk, with the ring "draining"
    /// between writes, and never see more come out than was free.
    #[test]
    fn the_real_converted_output_fits_the_free_space() {
        for &fin in &CLIENT_RATES {
            if fin == LINK_RATE {
                continue;
            }
            let mut r = Resampler::new(fin, LINK_RATE, 2);
            let mut out = alloc::vec::Vec::new();
            for free in [5usize, 6, 7, 10, 33, 100, 1024, 4096, 16000] {
                let take = accept_client_frames(free, fin, LINK_RATE);
                let input = alloc::vec![1000i16; take * 2];
                out.clear();
                r.process(&input, &mut out);
                let got = out.len() / 2;
                assert!(
                    got <= free,
                    "{} Hz: {} free, took {} -> {} out",
                    fin,
                    free,
                    take,
                    got
                );
            }
        }
    }

    #[test]
    fn nothing_is_accepted_without_room_for_the_slack() {
        for &fin in &CLIENT_RATES {
            for free in 0..SRC_SLACK_FRAMES {
                assert_eq!(accept_client_frames(free, fin, LINK_RATE), 0);
            }
        }
        assert_eq!(accept_client_frames(1000, 0, LINK_RATE), 0);
        assert_eq!(accept_client_frames(1000, 44100, 0), 0);
    }

    #[test]
    fn a_client_at_the_link_rate_gets_the_free_space_minus_slack_only() {
        // 48k -> 48k is not resampled in practice (no converter), but the
        // arithmetic must still be the identity apart from the slack.
        assert_eq!(
            accept_client_frames(1000, LINK_RATE, LINK_RATE),
            1000 - SRC_SLACK_FRAMES
        );
    }

    #[test]
    fn link_to_client_scales_the_frame_count_and_floors_to_a_frame() {
        // 480 link frames at 48k are 441 client frames at 44.1k.
        assert_eq!(link_to_client_bytes(480 * 4, 44100, 48000, 4), 441 * 4);
        // 48k -> 96k client: twice as many client frames.
        assert_eq!(link_to_client_bytes(100 * 4, 96000, 48000, 4), 200 * 4);
        // A dangling byte in the link count is dropped, never rounded up
        // into a phantom frame.
        assert_eq!(link_to_client_bytes(480 * 4 + 3, 44100, 48000, 4), 441 * 4);
        assert_eq!(link_to_client_bytes(1000, 44100, 48000, 0), 0);
        assert_eq!(link_to_client_bytes(1000, 44100, 0, 4), 0);
    }

    #[test]
    fn client_to_link_is_the_inverse_direction() {
        assert_eq!(client_to_link_bytes(441 * 4, 44100, 48000, 4), 480 * 4);
        assert_eq!(client_to_link_bytes(200 * 4, 96000, 48000, 4), 100 * 4);
        assert_eq!(client_to_link_bytes(1000, 0, 48000, 4), 0);
    }

    /// Under-reporting is the safe direction for what is queued: a client
    /// must never be told more of ITS frames are waiting than it wrote.
    /// Converting `n` client frames to link and back never exceeds `n`.
    #[test]
    fn a_round_trip_through_the_link_never_gains_frames() {
        for &fin in &CLIENT_RATES {
            for n in [1usize, 7, 100, 441, 1023, 4096] {
                let link = client_to_link_bytes(n * 4, fin, LINK_RATE, 4);
                let back = link_to_client_bytes(link, fin, LINK_RATE, 4);
                assert!(back <= n * 4, "{} Hz: {} -> {} -> {}", fin, n, link, back);
                // And it is not wildly lossy either. Each floor can drop
                // under one LINK frame, which is `fin / fout` client frames
                // when the client is the faster side (one link frame at 48k
                // is four client frames at 192k), so that is the bound.
                let slop = (fin as usize).div_ceil(LINK_RATE as usize);
                assert!(
                    back + 4 * slop >= n * 4,
                    "{} Hz: {} -> {} (allowed loss {} frames)",
                    fin,
                    n,
                    back,
                    slop
                );
            }
        }
    }

    /// The identity PulseAudio's life depends on: at EVERY fill level of the
    /// ring, `buffer - queued` (what the ALSA node computes as avail and
    /// offers) equals what `write` will accept. A single frame of daylight
    /// between them is a write answered with 0 right after avail said there
    /// was room, and alsa-sink's `try_recover` aborts the daemon on that.
    /// Checked with no pad, which is the only state a full ring can be in.
    #[test]
    fn avail_equals_what_write_accepts_at_every_fill_level() {
        let frame = 4usize;
        let buffer_link = 16384 * frame;
        for &fin in &CLIENT_RATES {
            let buffer = link_to_client_bytes(buffer_link, fin, LINK_RATE, frame);
            for exposed_frames in 0..=16384usize {
                let exposed = exposed_frames * frame;
                let queued = client_queued_bytes(buffer_link, exposed, fin, LINK_RATE, frame);
                let avail = buffer - queued;
                let free_link = buffer_link - exposed;
                let accept = accept_client_frames(free_link / frame, fin, LINK_RATE) * frame;
                assert_eq!(
                    avail, accept,
                    "{} Hz, {} link frames queued: avail {} vs accept {}",
                    fin, exposed_frames, avail, accept
                );
            }
        }
    }

    /// And the converted-occupancy formula this replaced really did break
    /// it: for a 44.1 kHz client there are fill levels where it leaves avail
    /// positive while `write` accepts nothing. This pins the bug so the
    /// derivation cannot quietly drift back to it.
    #[test]
    fn converting_the_occupancy_on_its_own_left_avail_positive_with_nothing_accepted() {
        let frame = 4usize;
        let buffer_link = 16384 * frame;
        let fin = 44100;
        let buffer = link_to_client_bytes(buffer_link, fin, LINK_RATE, frame);
        let mut broken_levels = 0;
        for exposed_frames in 0..=16384usize {
            let exposed = exposed_frames * frame;
            let old_queued = link_to_client_bytes(exposed, fin, LINK_RATE, frame);
            let old_avail = buffer.saturating_sub(old_queued);
            let accept =
                accept_client_frames((buffer_link - exposed) / frame, fin, LINK_RATE) * frame;
            if old_avail > 0 && accept == 0 {
                broken_levels += 1;
            }
        }
        assert!(
            broken_levels > 0,
            "the old formula should have had a zero-accept level with avail > 0"
        );
    }

    /// The identity holds through a silence pad, with and without a
    /// converter: the two counts come from the one occupancy figure, so
    /// `buffer - queued` is exactly what `write` accepts whatever share of
    /// the ring is the driver's own silence.
    #[test]
    fn avail_equals_what_write_accepts_through_a_silence_pad() {
        let frame = 4usize;
        let buffer_link = 12288 * frame;
        for src in [None, Some((44_100, LINK_RATE)), Some((96_000, LINK_RATE))] {
            let buffer = match src {
                Some((fin, fout)) => link_to_client_bytes(buffer_link, fin, fout, frame),
                None => buffer_link,
            };
            for pad_frames in [0usize, 1, 5, 2048, 9000, 12288] {
                for exposed_frames in (0..=12288usize).step_by(7) {
                    let occupied = ((pad_frames + exposed_frames) * frame).min(buffer_link);
                    let (free, queued) = client_counts(buffer_link, occupied, src, frame);
                    // What `write` accepts for that physical room.
                    let accept = match src {
                        Some((fin, fout)) => {
                            accept_client_frames((buffer_link - occupied) / frame, fin, fout)
                                * frame
                        }
                        None => buffer_link - occupied,
                    };
                    assert_eq!(
                        free, accept,
                        "{:?} pad {} exposed {}",
                        src, pad_frames, exposed_frames
                    );
                    assert_eq!(
                        buffer - queued,
                        accept,
                        "{:?} pad {} exposed {}: avail {} accept {}",
                        src,
                        pad_frames,
                        exposed_frames,
                        buffer - queued,
                        accept
                    );
                }
            }
        }
    }

    /// The formula this replaces: `queued` from the occupancy LESS the pad
    /// (so the hardware pointer never stepped back) while `write` was bound
    /// by the whole occupancy. With a pad ahead of the playhead and the ring
    /// full, `avail` promised the pad's worth and `write` took nothing --
    /// PulseAudio's abort after every underrun on QEMU (a 9000-frame pad
    /// is what its stalled position reads produce).
    #[test]
    fn leaving_the_pad_out_of_queued_promised_room_the_write_refused() {
        let frame = 4usize;
        let buffer_link = 12288 * frame;
        for src in [None, Some((44_100, LINK_RATE))] {
            let pad = 9000 * frame;
            let exposed = buffer_link - pad; // the ring is physically full
            let (accept, _) = client_counts(buffer_link, exposed + pad, src, frame);
            let (_, old_queued) = client_counts(buffer_link, exposed, src, frame);
            let buffer = match src {
                Some((fin, fout)) => link_to_client_bytes(buffer_link, fin, fout, frame),
                None => buffer_link,
            };
            let old_avail = buffer - old_queued;
            assert_eq!(accept, 0, "{:?}: the ring is full", src);
            assert!(
                old_avail >= 8000 * frame,
                "{:?}: the old avail promised {} B with nothing accepted",
                src,
                old_avail
            );
        }
    }

    /// Queued as the client sees it never runs backwards as the ring fills,
    /// and never exceeds the buffer (avail would wrap to ~boundary).
    #[test]
    fn client_queued_is_monotonic_and_bounded_by_the_buffer() {
        let frame = 4usize;
        let buffer_link = 16384 * frame;
        for &fin in &CLIENT_RATES {
            let buffer = link_to_client_bytes(buffer_link, fin, LINK_RATE, frame);
            let mut last = 0usize;
            for exposed_frames in 0..=16384usize {
                let q =
                    client_queued_bytes(buffer_link, exposed_frames * frame, fin, LINK_RATE, frame);
                assert!(q >= last, "{} Hz: queued went {} -> {}", fin, last, q);
                assert!(q <= buffer, "{} Hz: queued {} > buffer {}", fin, q, buffer);
                last = q;
            }
            // An empty ring is reported as (nearly) empty: only the slack.
            let empty = client_queued_bytes(buffer_link, 0, fin, LINK_RATE, frame);
            assert!(
                empty <= (SRC_SLACK_FRAMES + 2) * frame * 4,
                "{} Hz: empty ring queued {}",
                fin,
                empty
            );
        }
    }

    /// The buffer a front end sizes for a rate BEFORE applying it is the
    /// buffer the device reports AFTER: the ALSA node bounds `buffer_size`
    /// with the device's capacity while negotiating, then calls
    /// `set_params`, and on a fixed-rate ring the capacity in client frames
    /// changes with that call. Bounding a 44.1 kHz buffer with the previous
    /// (48 kHz) stream's figure left the node 999 frames it did not have:
    /// `avail` stayed positive with the ring full, `write` took nothing,
    /// and PulseAudio's `try_recover` asserted on the EAGAIN.
    #[test]
    fn the_capacity_promised_for_a_rate_is_the_capacity_after_it_is_set() {
        let frame = 4usize;
        let link_buffer = 12288 * frame;
        for &rate in &CLIENT_RATES {
            let before = client_buffer_bytes(link_buffer, rate, frame);
            // What `buffer_bytes` computes once `client_rate == rate`.
            let after = if rate == LINK_RATE {
                link_buffer
            } else {
                link_to_client_bytes(link_buffer, rate, LINK_RATE, frame)
            };
            assert_eq!(before, after, "{} Hz", rate);
            assert_eq!(before % frame, 0, "{} Hz: whole frames", rate);
        }
        // The exact figures for the two rates that matter in practice.
        assert_eq!(client_buffer_bytes(link_buffer, 48_000, frame), 49152);
        assert_eq!(
            client_buffer_bytes(link_buffer, 44_100, frame),
            11289 * frame
        );
        // The same clamp as `set_params`: a rate the device would refuse is
        // sized as the rate it will actually get.
        assert_eq!(
            client_buffer_bytes(link_buffer, 4_000, frame),
            client_buffer_bytes(link_buffer, CLIENT_RATE_MIN, frame)
        );
        assert_eq!(
            client_buffer_bytes(link_buffer, 1_000_000, frame),
            client_buffer_bytes(link_buffer, CLIENT_RATE_MAX, frame)
        );
    }

    /// Decode an HDA stream format word the way the controller does
    /// (Intel HDA §3.7.1): base rate bit, multiplier, divisor, sample size,
    /// channel count.
    fn decode(fmt: u16) -> (u32, u32, u32, u32, u32) {
        let base = if fmt & (1 << 14) != 0 { 44100 } else { 48000 };
        let mult = ((fmt >> 11) & 0b111) as u32 + 1;
        let div = ((fmt >> 8) & 0b111) as u32 + 1;
        let bits = (fmt >> 4) & 0b111;
        let chans = (fmt & 0xf) as u32 + 1;
        (base, mult, div, bits as u32, chans)
    }

    #[test]
    fn every_supported_rate_decodes_back_to_itself() {
        // The whole point of the table: `base * mult / div` is what the
        // codec will actually clock the samples out at. One wrong nibble and
        // the track plays fast or slow, with no error anywhere.
        for rate in [
            8000u32, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
        ] {
            let (base, mult, div, _, _) = decode(stream_format(rate, 2));
            assert_eq!(
                base * mult / div,
                rate,
                "{} Hz encoded as base {} x{} /{}",
                rate,
                base,
                mult,
                div
            );
        }
    }

    #[test]
    fn the_base_rate_bit_follows_the_family_not_the_magnitude() {
        // 44.1k and 48k families are not interchangeable: asking for 88200 on
        // the 48k base gives 96000, which is a 9% pitch error.
        for rate in [11025u32, 22050, 44100, 88200, 176400] {
            assert_eq!(decode(stream_format(rate, 2)).0, 44100, "{} Hz", rate);
        }
        for rate in [8000u32, 16000, 32000, 48000, 96000, 192000] {
            assert_eq!(decode(stream_format(rate, 2)).0, 48000, "{} Hz", rate);
        }
    }

    #[test]
    fn an_unknown_rate_falls_back_to_48k_exactly() {
        // The `_ =>` arm claims 48000; it has to actually encode as 48000
        // and not as whatever the previous arm left behind.
        let (base, mult, div, _, _) = decode(stream_format(1234, 2));
        assert_eq!((base, mult, div), (48000, 1, 1));
        assert_eq!(base * mult / div, 48000);
    }

    #[test]
    fn the_sample_size_is_always_sixteen_bits() {
        // The ring, `frame_bytes` and the volume stage are all S16LE. A
        // format word that says anything else desynchronises every one of
        // them from the controller.
        for rate in [8000u32, 44100, 48000, 192000, 999] {
            assert_eq!(decode(stream_format(rate, 2)).3, 0b001, "{} Hz", rate);
        }
    }

    #[test]
    fn the_channel_count_is_encoded_base_zero() {
        assert_eq!(decode(stream_format(48000, 1)).4, 1);
        assert_eq!(decode(stream_format(48000, 2)).4, 2);
        assert_eq!(decode(stream_format(48000, 8)).4, 8);
    }

    #[test]
    fn a_zero_channel_count_does_not_underflow_into_sixteen_channels() {
        // `channels - 1` on zero is a panic in debug and `0xFFFF & 0xf` == 15
        // in release, i.e. sixteen channels. Every sibling that touches this
        // field uses `.max(1)`; this one now does too.
        assert_eq!(decode(stream_format(48000, 0)).4, 1);
    }

    #[test]
    fn the_rate_snapping_picks_the_nearest_supported_rate() {
        assert_eq!(nearest_rate(48000), 48000, "an exact rate is left alone");
        assert_eq!(nearest_rate(44100), 44100);
        assert_eq!(nearest_rate(47999), 48000);
        assert_eq!(nearest_rate(44000), 44100);
        assert_eq!(
            nearest_rate(0),
            8000,
            "below the table clamps to the lowest"
        );
        assert_eq!(
            nearest_rate(u32::MAX),
            192000,
            "above the table clamps to the highest"
        );
    }

    #[test]
    fn the_snapped_rate_is_always_one_the_format_table_knows() {
        // The contract between the two functions: `stream_format`'s `_ =>`
        // arm silently means 48000, so anything `nearest_rate` returns must
        // have its own arm, or a 44.1k-family request lands on 48k.
        for raw in [
            0u32,
            1,
            7999,
            8000,
            30000,
            44099,
            44100,
            60000,
            100000,
            1 << 30,
            u32::MAX,
        ] {
            let rate = nearest_rate(raw);
            let (base, mult, div, _, _) = decode(stream_format(rate, 2));
            assert_eq!(
                base * mult / div,
                rate,
                "nearest_rate({}) = {} has no entry in the format table",
                raw,
                rate
            );
        }
    }

    #[test]
    fn the_subnode_response_splits_into_start_and_count() {
        // GET_PARAMETER(SUB_NODE_COUNT): start node in 23:16, count in 7:0.
        // Reading the two the wrong way round walks a node list that does not
        // exist, and the codec enumerates as having no widgets at all.
        assert_eq!(sub_nodes(0x0002_0005), (2, 5));
        assert_eq!(sub_nodes(0), (0, 0));
        assert_eq!(sub_nodes(0x00FF_00FF), (0xFF, 0xFF));
        assert_eq!(
            sub_nodes(0xFF00_FF00),
            (0, 0),
            "the bits outside the two fields are not part of either"
        );
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    fn path(pin: u32, present: bool) -> OutPath {
        OutPath {
            conv: 0x4,
            pin,
            pin_conn_idx: 0,
            digital: true,
            hdmi_dp: true,
            present,
        }
    }

    fn pins(chosen: Option<&OutPath>) -> Option<u32> {
        chosen.map(|p| p.pin)
    }

    #[test]
    fn the_initial_pick_is_the_best_score_first_on_ties() {
        let scored = [
            (path(0x5, false), 0),
            (path(0x6, true), 7),
            (path(0x7, true), 7),
        ];
        assert_eq!(pins(choose_route(&scored, None)), Some(0x6));
        assert_eq!(pins(choose_route(&[], None)), None);
    }

    #[test]
    fn the_current_path_keeps_its_place_on_a_tie() {
        // Two pins, neither present, equal scores: the first in the list
        // used to win, so a stream on pin 6 hopped to pin 5 on restart.
        let scored = [(path(0x5, false), 0), (path(0x6, false), 0)];
        assert_eq!(pins(choose_route(&scored, Some((0x6, 0x4)))), Some(0x6));
        let scored = [(path(0x5, true), 7), (path(0x6, true), 7)];
        assert_eq!(pins(choose_route(&scored, Some((0x6, 0x4)))), Some(0x6));
    }

    #[test]
    fn a_momentary_presence_loss_does_not_move_the_stream_to_a_dead_pin() {
        // The monitor's pin read PD=0 this once; the other pin never had
        // anything. Its higher score (first in list, equal otherwise) is
        // not a reason to leave.
        let scored = [(path(0x5, false), 1), (path(0x6, false), 0)];
        assert_eq!(pins(choose_route(&scored, Some((0x6, 0x4)))), Some(0x6));
    }

    #[test]
    fn a_live_pin_still_wins_over_a_dead_current_one() {
        // The monitor moved to another connector: follow it.
        let scored = [(path(0x5, true), 7), (path(0x6, false), 0)];
        assert_eq!(pins(choose_route(&scored, Some((0x6, 0x4)))), Some(0x5));
        // A converter change on the same pin counts as a move too.
        let mut other = path(0x6, true);
        other.conv = 0x8;
        let scored = [(other, 7), (path(0x6, false), 0)];
        assert_eq!(
            choose_route(&scored, Some((0x6, 0x4))).map(|p| (p.pin, p.conv)),
            Some((0x6, 0x8))
        );
    }
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
            self.trigger_pin_sense(p.pin);
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

    /// Does the active pin already carry a live sink (PD=1 and ELDV=1)? On
    /// HDMI/DP those bits are set from the display side (the GOP/RM ELD push
    /// in `kick_hdmi_audio`), so a live pin means the display engine already
    /// transmits audio and the stream-start kick has nothing left to do.
    fn active_pin_live(&mut self) -> bool {
        let pin = self.pin_nid;
        let hdmi_dp = self.candidates.iter().any(|c| c.pin == pin && c.hdmi_dp);
        if hdmi_dp {
            self.trigger_pin_sense(pin);
        }
        let sense = self.cmd(pin, VERB_GET_PIN_SENSE, 0).unwrap_or(0);
        sense & (1 << 31) != 0 && sense & (1 << 30) != 0
    }

    /// Latch a fresh presence-detect result on a digital pin, unless one was
    /// latched less than [`SENSE_PROBE_MIN_US`] ago -- in which case the
    /// value `GET_PIN_SENSE` already holds is used as it stands.
    fn trigger_pin_sense(&mut self, pin: u32) {
        let now = timer_now_as_micros();
        if let Some(&(_, last)) = self.sense_probe.iter().find(|(p, _)| *p == pin) {
            if now.wrapping_sub(last) < SENSE_PROBE_MIN_US {
                return;
            }
        }
        let _ = self.cmd(pin, VERB_SET_PIN_SENSE, 0);
        wait_us(2_000);
        let now = timer_now_as_micros();
        match self.sense_probe.iter_mut().find(|(p, _)| *p == pin) {
            Some(slot) => slot.1 = now,
            None => self.sense_probe.push((pin, now)),
        }
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

    /// Score every candidate path right now.
    fn scored_candidates(&mut self) -> Vec<(OutPath, i32)> {
        let candidates = self.candidates.clone();
        let mut out = Vec::with_capacity(candidates.len());
        for mut p in candidates {
            let (score, present, eld_valid) = self.score_path(&p);
            p.present = present;
            info!(
                "[hda] path candidate: pin {:#x} -> conv {:#x} (digital={}, hdmi/dp={}, present={}, eld={}, score={})",
                p.pin, p.conv, p.digital, p.hdmi_dp, present, eld_valid, score
            );
            out.push((p, score));
        }
        out
    }

    /// Pick the best-scoring path among `self.candidates` right now. Used
    /// for the initial pick, where there is no current path to prefer.
    fn best_candidate(&mut self) -> Option<OutPath> {
        let scored = self.scored_candidates();
        choose_route(&scored, None).cloned()
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
            let scored = self.scored_candidates();
            let current = (self.pin_nid, self.conv_nid);
            let Some(best) = choose_route(&scored, Some(current)).cloned() else {
                return;
            };
            if best.pin != self.pin_nid || best.conv != self.conv_nid {
                warn!(
                    "[hda] re-routing output: pin {:#x} -> pin {:#x} (present={})",
                    self.pin_nid, best.pin, best.present
                );
                self.stat_reroutes += 1;
                self.last_reroute = (self.pin_nid, best.pin, timer_now_as_micros() / 1000);
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
            let ch = self.channels;
            self.setup_digital_converter(ch);
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

impl HdaInner {
    /// The driver state over an already-programmed controller: the CORB/RIRB
    /// and PCM ring it was given, no codec path yet, nothing running. What
    /// [`HdaDevice::new`] fills in before codec discovery, and what the tests
    /// build over a fake controller.
    #[allow(clippy::too_many_arguments)]
    fn bare(
        bar: usize,
        corb_va: usize,
        corb_entries: usize,
        rirb_va: usize,
        rirb_entries: usize,
        cad: u32,
        sd_base: usize,
        ring_va: usize,
        ring_len: usize,
        bdl_pa: usize,
        dma_pos_va: usize,
    ) -> Self {
        HdaInner {
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
            sd_base,
            stream_tag: 1,
            ring_va,
            ring_len,
            bdl_pa,
            running: false,
            last_lpib: 0,
            last_poll_us: 0,
            dma_pos_va,
            last_dpib: 0,
            dpib_trusted: false,
            fill_pos: 0,
            zero_end: 0,
            mixed_bytes: 0,
            consumed: 0,
            wall_last: 0,
            wall_ticks: 0,
            lpib_total: 0,
            dpib_total: 0,
            stat_lead: 0,
            lead_now: 0,
            stat_drains: 0,
            stat_underruns: 0,
            stat_idle_stops: 0,
            stat_restarts: 0,
            stat_late_fills: 0,
            stat_stop_timeouts: 0,
            stat_pos_reads: 0,
            stat_pos_read_max_us: 0,
            stat_pos_reads_slow: 0,
            stat_bad_pos: 0,
            last_bad_pos: 0,
            last_bad_prev: 0,
            last_bad_dt_us: 0,
            last_bad_src: 0,
            stat_fifo_err: 0,
            stat_desc_err: 0,
            stat_stale_resp: 0,
            stat_reroutes: 0,
            last_reroute: (0, 0, 0),
            sense_probe: Vec::new(),
            last_kick_us: 0,
            stream_start_wall: 0,
            stream_start_us: 0,
            stops: [StopEvent::default(); STOP_HISTORY],
            rate: LINK_RATE,
            channels: 2,
            gap_since_us: 0,
            gain_l: 100,
            gain_r: 100,
            mute_l: false,
            mute_r: false,
            vol: Volume::new(LINK_RATE, 2),
            streams: Vec::new(),
            next_stream: 0,
            mix_acc: alloc::vec![0; FILL_MAX / 2],
            mix_buf: alloc::vec![0; FILL_MAX],
            pull_buf: alloc::vec![0; FILL_MAX],
        }
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

        let mut inner = HdaInner::bare(
            bar,
            corb_va,
            corb_entries,
            rirb_va,
            rirb_entries,
            cad,
            // First output stream descriptor comes after the input ones.
            REG_SD_BASE + iss * 0x20,
            ring_va,
            ring_len,
            bdl_pa,
            dma_pos_va,
        );

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

        let own_id = inner.add_stream(HdaInner::new_stream());
        let inner = Arc::new(Mutex::new(inner));
        Ok(HdaDevice {
            own: HdaStream {
                inner: inner.clone(),
                id: own_id,
                name: name.clone(),
            },
            name,
            is_nvidia,
            inner,
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

impl Scheme for HdaStream {
    fn name(&self) -> &str {
        &self.name
    }

    fn handle_irq(&self, _irq: usize) {}
}

/// What a stream method needs to know about the engine, read before the
/// stream is borrowed out of the table.
#[derive(Clone, Copy)]
struct Engine {
    running: bool,
    /// See [`HdaInner::ring_ahead`].
    ahead: usize,
}

impl HdaStream {
    /// Run `f` on this handle's stream, after a progress poll (which runs
    /// the fill for every stream, this one included) when `poll` is set.
    /// The stream exists for as long as the handle does (its `Drop` is
    /// what removes it), so the `None` arm is unreachable in practice; it
    /// is an error rather than a panic because this runs with the device
    /// lock held and interrupts off.
    fn with<R>(&self, poll: bool, f: impl FnOnce(&mut HostStream, Engine) -> R) -> DeviceResult<R> {
        let mut inner = self.inner.lock();
        if poll {
            inner.poll_progress();
        }
        let engine = Engine {
            running: inner.running,
            ahead: inner.ring_ahead(),
        };
        let Some(stream) = inner.stream_mut(self.id) else {
            return Err(DeviceError::NotReady);
        };
        Ok(f(stream, engine))
    }
}

impl AudioScheme for HdaStream {
    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
        // The client gets the rate it asked for (within reason); the link
        // stays at LINK_RATE and the difference is resampled on the way
        // into the stream. Only stereo, whatever was asked.
        let _ = channels;
        let rate = self.with(false, |s, engine| {
            let was = s.client_rate();
            let now = s.set_params(rate);
            if was != now {
                warn!(
                    "[hda] stream {}: client {} Hz -> {} Hz, link stays {} Hz (running={})",
                    self.id, was, now, LINK_RATE, engine.running
                );
            }
            now
        })?;
        Ok((rate, 2))
    }

    fn params(&self) -> (u32, u8) {
        self.with(false, |s, _| (s.client_rate(), 2))
            .unwrap_or((LINK_RATE, 2))
    }

    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        let kick_hdmi = {
            let mut inner = self.inner.lock();
            let now = timer_now_as_micros();
            let due =
                inner.last_kick_us == 0 || now.wrapping_sub(inner.last_kick_us) >= HDMI_KICK_MIN_US;
            let kick = due && !inner.running && inner.digital && !inner.active_pin_live();
            if kick {
                inner.last_kick_us = now;
            }
            kick
        };
        if kick_hdmi {
            // GOP never enables audio packets; re-push ELD/unmute now so the
            // pin-sense that follows can see a live display. Drop the HDA
            // lock first — RM takes GPU locks. Skipped when the active pin
            // already reports PD=1/ELDV=1: the display side has done its
            // part, and re-poking the scanout GPU's display engine on every
            // stream start is not something a running desktop should see.
            crate::display::kick_hdmi_audio();
        }
        let n = self.with(true, |s, _| s.write(pcm))?;
        if n > 0 {
            self.inner.lock().ensure_engine()?;
        }
        Ok(n)
    }

    // The four counts the front ends see are the stream's, in CLIENT bytes
    // (see `HostStream`): the ring never enters into them, except through
    // `delay_bytes`.

    fn free_bytes(&self) -> usize {
        self.with(true, |s, _| s.free_bytes()).unwrap_or(0)
    }

    fn buffer_bytes(&self) -> usize {
        self.with(false, |s, _| s.buffer_bytes()).unwrap_or(0)
    }

    fn buffer_bytes_at(&self, rate: u32) -> usize {
        self.with(false, |s, _| s.buffer_bytes_at(rate))
            .unwrap_or(0)
    }

    fn queued_bytes(&self) -> usize {
        self.with(true, |s, _| s.queued_bytes()).unwrap_or(0)
    }

    fn delay_bytes(&self) -> usize {
        self.with(true, |s, engine| {
            // What the stream still holds, plus what the ring holds ahead
            // of the play position if the stream is being mixed into it.
            let ahead = if s.is_active() { engine.ahead } else { 0 };
            s.to_client_bytes(s.queued_link() + ahead)
        })
        .unwrap_or(0)
    }

    fn is_playing(&self) -> bool {
        self.with(false, |s, engine| engine.running && s.is_active())
            .unwrap_or(false)
    }

    fn reset(&self) -> DeviceResult {
        self.with(false, |s, _| s.reset())
    }

    fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
        self.with(false, |s, _| s.rewind(bytes))
    }

    fn forward(&self, bytes: usize) -> DeviceResult<usize> {
        self.with(false, |s, _| s.forward(bytes))
    }

    fn pause(&self) -> DeviceResult {
        self.with(false, |s, _| s.pause())
    }

    fn resume(&self) -> DeviceResult {
        self.with(false, |s, _| s.resume())?;
        self.inner.lock().ensure_engine()
    }

    fn set_start_hold(&self, hold: bool) -> DeviceResult {
        let started = self.with(false, |s, _| s.set_start_hold(hold))?;
        if started {
            // Released with a primed stream: it plays now, exactly as the
            // write that filled it would have.
            self.inner.lock().ensure_engine()?;
        }
        Ok(())
    }

    fn set_gain(&self, left: u8, right: u8, mute_left: bool, mute_right: bool) -> DeviceResult {
        self.inner
            .lock()
            .set_gain(left, right, mute_left, mute_right);
        Ok(())
    }

    fn gain(&self) -> (u8, u8, bool, bool) {
        self.inner.lock().gain()
    }

    fn default_score(&self) -> i32 {
        self.inner.lock().default_score()
    }
}

impl HdaInner {
    fn set_gain(&mut self, left: u8, right: u8, mute_left: bool, mute_right: bool) {
        self.gain_l = left.min(100);
        self.gain_r = right.min(100);
        self.mute_l = mute_left;
        self.mute_r = mute_right;
        let (l, r) = (
            gain_from_percent(left, mute_left),
            gain_from_percent(right, mute_right),
        );
        self.vol.set_target(0, l);
        self.vol.set_target(1, r);
    }

    fn gain(&self) -> (u8, u8, bool, bool) {
        (self.gain_l, self.gain_r, self.mute_l, self.mute_r)
    }

    fn default_score(&mut self) -> i32 {
        let pin = self.pin_nid;
        if let Some(p) = self.candidates.iter().find(|c| c.pin == pin).cloned() {
            self.score_path(&p).0
        } else if self.digital {
            1
        } else {
            0
        }
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

/// The device as a client: its own stream, mixed like any other. What the
/// front ends use is [`open_stream`](AudioScheme::open_stream).
impl AudioScheme for HdaDevice {
    fn open_stream(&self) -> DeviceResult<Option<Arc<dyn AudioScheme>>> {
        let stream = HdaInner::new_stream();
        let id = self.inner.lock().add_stream(stream);
        Ok(Some(Arc::new(HdaStream {
            inner: self.inner.clone(),
            id,
            name: alloc::format!("{}#{}", self.name, id),
        })))
    }

    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
        self.own.set_params(rate, channels)
    }

    fn params(&self) -> (u32, u8) {
        self.own.params()
    }

    fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
        self.own.write(pcm)
    }

    fn free_bytes(&self) -> usize {
        self.own.free_bytes()
    }

    fn buffer_bytes(&self) -> usize {
        self.own.buffer_bytes()
    }

    fn buffer_bytes_at(&self, rate: u32) -> usize {
        self.own.buffer_bytes_at(rate)
    }

    /// Polls the engine as a side effect: the ALSA watchdog calls this on
    /// every registered device every 4 ms while one is playing, and that
    /// is what paces the ring fill for every stream on the device.
    fn queued_bytes(&self) -> usize {
        self.own.queued_bytes()
    }

    fn delay_bytes(&self) -> usize {
        self.own.delay_bytes()
    }

    /// Whether the DMA engine is running, for any stream: the watchdog's
    /// question, not the device stream's.
    fn is_playing(&self) -> bool {
        self.inner.lock().running
    }

    fn reset(&self) -> DeviceResult {
        self.own.reset()
    }

    fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
        self.own.rewind(bytes)
    }

    fn forward(&self, bytes: usize) -> DeviceResult<usize> {
        self.own.forward(bytes)
    }

    fn pause(&self) -> DeviceResult {
        self.own.pause()
    }

    fn resume(&self) -> DeviceResult {
        self.own.resume()
    }

    fn set_start_hold(&self, hold: bool) -> DeviceResult {
        self.own.set_start_hold(hold)
    }

    fn set_gain(&self, left: u8, right: u8, mute_left: bool, mute_right: bool) -> DeviceResult {
        self.inner
            .lock()
            .set_gain(left, right, mute_left, mute_right);
        Ok(())
    }

    fn gain(&self) -> (u8, u8, bool, bool) {
        self.inner.lock().gain()
    }

    fn diagnostics(&self) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();

        // Sample the stream descriptor BEFORE taking the device lock. These
        // are plain MMIO register reads -- the lock protects the driver's own
        // state, not the controller's registers -- and the 2 ms between the
        // two LPIB samples used to be spent holding it. The lock disables
        // interrupts, so every `cat /proc/gpusnd` cost the machine a
        // guaranteed 2 ms of IRQ-off time on that CPU, on top of whatever the
        // codec reads below added.
        let (bar, sd, cad) = {
            let inner = self.inner.lock();
            (inner.bar, inner.sd_base, inner.cad)
        };
        let gcap = mmio_r16(bar, REG_GCAP);
        let statests = mmio_r16(bar, REG_STATESTS);
        // RUN=1 with a moving LPIB means the DMA engine really is fetching our
        // samples; if that holds and there is still no sound, the fault is
        // downstream of the controller (codec routing, or the display engine
        // not transmitting).
        let ctl = mmio_r32(bar, sd + SD_CTL);
        let lpib1 = mmio_r32(bar, sd + SD_LPIB);
        wait_us(2_000);
        let lpib2 = mmio_r32(bar, sd + SD_LPIB);

        // Bracket the locked section. `/proc/gpusnd` is the last thing on
        // screen in the freeze this budget is meant to bound, and these two
        // lines settle whether the machine died inside the dump: an "enter"
        // with no "done" means it did. Cheap (twice per read of a debug file)
        // and visible at the default LOG=error.
        crate::klog_warn!("[gpusnd] diagnostics: enter ({})", self.name);
        let t0 = timer_now_as_micros();

        let mut budget = DiagBudget::new();
        let mut inner = self.inner.lock();

        let _ = writeln!(out, "[gpusnd] === {} ===", self.name);
        let _ = writeln!(
            out,
            "[gpusnd] controller: GCAP {:#06x} STATESTS {:#06x} codec {}",
            gcap, statests, cad
        );
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
            "[gpusnd] ring: running={} filled {} B ahead of the playhead (fill depth {} B, {} B mixed this stream) rate={} ch={} streams={} gap={}",
            inner.running,
            inner.ring_ahead(),
            FILL_DEPTH,
            inner.mixed_bytes,
            inner.rate,
            inner.channels,
            inner.streams.len(),
            if inner.gap_since_us != 0 {
                alloc::format!(
                    "{} ms (engine idling on silence, stops at {} ms)",
                    timer_now_as_micros().wrapping_sub(inner.gap_since_us) / 1000,
                    DAI_IDLE_STOP_US / 1000
                )
            } else {
                String::from("none")
            }
        );
        // One line per client stream: the host side of the pipeline. A
        // stream that is started with nothing queued and `underruns` going
        // up is a client that falls behind; one that is held or paused
        // is what PulseAudio's cork looks like from here.
        for (id, s) in inner.streams.iter() {
            let (written, pulled, underruns) = s.stats();
            let _ = writeln!(
                out,
                "[gpusnd]   stream {}: client={} Hz src={} queued={} B (link) of {} B state={}{} written={} B pulled={} B underruns={}",
                id,
                s.client_rate(),
                if s.is_resampling() {
                    "resampling"
                } else {
                    "passthrough"
                },
                s.queued_link(),
                HOST_BUFFER,
                if s.is_started() { "started" } else { "stopped" },
                if s.is_paused() {
                    " (paused)"
                } else if s.is_held() {
                    " (start held)"
                } else {
                    ""
                },
                written,
                pulled,
                underruns
            );
        }
        // Which position source the ring is pacing against. LPIB alone is the
        // fragile case: on a controller whose LPIB runs ahead of what has been
        // played, the writer is handed space that is still going to be played
        // and overwrites it -- heard as a stream of very short dropouts. The
        // position buffer is the second opinion that prevents that, and this
        // says whether it is actually being maintained on this hardware.
        let dpib = inner.dma_pos();
        let _ = writeln!(
            out,
            "[gpusnd] position: LPIB {}{} guard={}B ahead={}B window={}B of {}B",
            inner.lpib(),
            match dpib {
                Some(p) if inner.dpib_trusted => alloc::format!(" + DMA-pos {} (in use)", p),
                Some(p) => alloc::format!(" + DMA-pos {} (not advancing yet)", p),
                None => alloc::string::String::from(" only (controller refused a position buffer)"),
            },
            RING_GUARD,
            inner.ring_ahead(),
            (inner.ring_len - RING_GUARD).saturating_sub(inner.ring_ahead()),
            inner.ring_len
        );
        let _ = writeln!(
            out,
            "[gpusnd] events: {} drains, {} underruns (gaps the engine idled through), {} idle stops, {} stream restarts, {} late fills (the engine overtook the fill), {} stop timeouts, {} rejected position reads{}",
            inner.stat_drains,
            inner.stat_underruns,
            inner.stat_idle_stops,
            inner.stat_restarts,
            inner.stat_late_fills,
            inner.stat_stop_timeouts,
            inner.stat_bad_pos,
            if inner.stat_bad_pos > 0 {
                alloc::format!(
                    " (last {} {:#x} after {:#x}, {} us apart)",
                    if inner.last_bad_src == b'P' { "DMA-pos" } else { "LPIB" },
                    inner.last_bad_pos,
                    inner.last_bad_prev,
                    inner.last_bad_dt_us
                )
            } else {
                String::new()
            }
        );
        let _ = writeln!(
            out,
            "[gpusnd] position reads: {}, slowest {} us, {} over 1 ms{}",
            inner.stat_pos_reads,
            inner.stat_pos_read_max_us,
            inner.stat_pos_reads_slow,
            if inner.stat_pos_reads_slow > 0 {
                " (VM exits waiting on the host?)"
            } else {
                ""
            }
        );
        let _ = writeln!(
            out,
            "[gpusnd] stream errors: {} FIFO underruns (FIFOE), {} descriptor errors (DESE), FIFO size {} B",
            inner.stat_fifo_err,
            inner.stat_desc_err,
            mmio_r16(bar, sd + SD_FIFOS)
        );
        let _ = writeln!(
            out,
            "[gpusnd] routing: {} re-routes{}",
            inner.stat_reroutes,
            if inner.stat_reroutes > 0 {
                alloc::format!(
                    " (last pin {:#x} -> pin {:#x} at {} ms)",
                    inner.last_reroute.0,
                    inner.last_reroute.1,
                    inner.last_reroute.2
                )
            } else {
                String::new()
            }
        );
        let _ = writeln!(
            out,
            "[gpusnd] codec link: {} stale RIRB responses discarded{}",
            inner.stat_stale_resp,
            if inner.stat_stale_resp > 0 {
                " (jack events, or late answers to a verb that timed out)"
            } else {
                ""
            }
        );
        // Two clocks against each other. The kernel's is derived from the
        // TSC; the controller's counts its own 24 MHz link clock. Over a
        // running stream they must agree, and the engine must consume the
        // PCM byte rate per wall-clock second. If the kernel clock runs fast
        // the stream looks slow by it while the engine's rate by the wall
        // clock is exact; if the engine really is starved, its rate by the
        // wall clock falls short and FIFOE above says so.
        if inner.running {
            let kernel_ms = timer_now_as_micros().wrapping_sub(inner.stream_start_us) / 1000;
            let wall_ms = inner.stream_wall_ms();
            let consumed = inner.consumed;
            let ratio_pct = (kernel_ms * 100).checked_div(wall_ms).unwrap_or(0);
            let engine_rate = (consumed * 1000).checked_div(wall_ms).unwrap_or(0);
            let _ = writeln!(
                out,
                "[gpusnd] clocks: stream age {} ms by kernel clock, {} ms by HDA wall clock (kernel/wall = {}.{:02}); link consumed {} B = {} B/s by wall clock (PCM rate {} B/s); reported position ran ahead of the link clock by up to {} B, now {} B (link start latency ~{} ms)",
                kernel_ms,
                wall_ms,
                ratio_pct / 100,
                ratio_pct % 100,
                consumed,
                engine_rate,
                inner.rate as u64 * inner.frame_bytes() as u64,
                inner.stat_lead,
                inner.lead_now,
                (inner.stat_lead - inner.lead_now) * 1000
                    / (inner.rate as u64 * inner.frame_bytes() as u64).max(1)
            );
        }
        // The last gaps, oldest first. wavplay's tone is 576000 B, so a gap
        // that opens short of that, well before 3000 ms, is a dropout; one
        // at ~3000 ms with all of it written is the tone ending.
        for ev in inner.stops.iter().filter(|e| e.kind != 0) {
            let _ = writeln!(
                out,
                "[gpusnd]   gap: {} at {} ms by kernel clock / {} ms by HDA wall clock, {} B written to that stream",
                match ev.kind {
                    b'U' => "underrun",
                    _ => "drain",
                },
                ev.at_ms,
                ev.wall_ms,
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
                .diag_cmd(conv, VERB_GET_STREAM_ID, &mut budget)
                .unwrap_or(0xffff_ffff);
            let fmt = inner
                .diag_cmd16(conv, VERB_GET_CVT_FORMAT, &mut budget)
                .unwrap_or(0xffff_ffff);
            let dig = inner
                .diag_cmd(conv, VERB_GET_DIGI_CVT, &mut budget)
                .unwrap_or(0xffff_ffff);
            let pwr = inner
                .diag_cmd(conv, VERB_GET_POWER_STATE, &mut budget)
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
            let ctl = inner
                .diag_cmd(pin, VERB_GET_PIN_CTL, &mut budget)
                .unwrap_or(0xffff_ffff);
            let sense = inner
                .diag_cmd(pin, VERB_GET_PIN_SENSE, &mut budget)
                .unwrap_or(0);
            let eapd = inner
                .diag_cmd(pin, VERB_GET_EAPD, &mut budget)
                .unwrap_or(0xffff_ffff);
            let pwr = inner
                .diag_cmd(pin, VERB_GET_POWER_STATE, &mut budget)
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

        if budget.dead {
            let _ = writeln!(
                out,
                "[gpusnd] NOTE: codec reads cut short (no response, or over the {} ms budget) — \
                 values above shown as ffffffff/0 were not read",
                DIAG_CODEC_BUDGET_US / 1000
            );
        }

        // Every candidate, with live presence/ELD: this says whether the pin
        // carrying the cable was the one we picked.
        let candidates = inner.candidates.clone();
        let _ = writeln!(out, "[gpusnd] candidates ({}):", candidates.len());
        for c in candidates.iter() {
            let sense = inner
                .diag_cmd(c.pin, VERB_GET_PIN_SENSE, &mut budget)
                .unwrap_or(0);
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
        drop(inner);
        crate::klog_warn!(
            "[gpusnd] diagnostics: done ({}) in {} us{}",
            self.name,
            timer_now_as_micros().wrapping_sub(t0),
            if budget.dead {
                " (codec reads cut short)"
            } else {
                ""
            }
        );
        out
    }

    fn default_score(&self) -> i32 {
        self.inner.lock().default_score()
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

#[cfg(test)]
#[path = "hda_fake.rs"]
mod hda_fake;

#[cfg(test)]
mod dai_tests {
    //! The DAI side's one piece of arithmetic: where the next fill goes.
    //! Everything else in the fill loop is a copy into the ring, which
    //! needs the ring; the span is what decides whether that copy lands
    //! ahead of the engine (audible), behind it (never heard), or over what
    //! is still playing (heard as a dropout).

    use super::{fill_span, mix_streams, BDL_SEGMENT, FILL_DEPTH, RING_GUARD, RING_PAGES};
    use crate::audio::pipeline::host::HostStream;
    use crate::nvme::nvme_queue::PAGE_SIZE;
    use alloc::vec;
    use alloc::vec::Vec;

    const RING: usize = RING_PAGES * PAGE_SIZE;
    const FRAME: usize = 4;

    #[test]
    fn a_fresh_ring_is_primed_to_the_fill_depth() {
        assert_eq!(
            fill_span(0, 0, 0, RING, RING_GUARD, FILL_DEPTH, FRAME),
            FILL_DEPTH
        );
    }

    #[test]
    fn the_fill_follows_whichever_position_runs_furthest_ahead() {
        // The link clock is ahead of the counter: fill to the clock + depth.
        assert_eq!(
            fill_span(10_000, 10_000, 4_000, RING, RING_GUARD, FILL_DEPTH, FRAME),
            FILL_DEPTH
        );
        // The counter is ahead of the clock (QEMU's prefetch, an
        // over-reporting LPIB): fill to the counter + depth. Behind the
        // counter is where the engine has already been, and PCM written
        // there is never fetched.
        assert_eq!(
            fill_span(10_000, 4_000, 10_000, RING, RING_GUARD, FILL_DEPTH, FRAME),
            FILL_DEPTH
        );
        // A fill already ahead of both does nothing.
        assert_eq!(
            fill_span(
                10_000 + FILL_DEPTH as u64,
                10_000,
                4_000,
                RING,
                RING_GUARD,
                FILL_DEPTH,
                FRAME
            ),
            0
        );
        // Part-way: the difference.
        assert_eq!(
            fill_span(
                10_000 + 1_000,
                10_000,
                4_000,
                RING,
                RING_GUARD,
                FILL_DEPTH,
                FRAME
            ),
            FILL_DEPTH - 1_000
        );
    }

    #[test]
    fn the_fill_never_reaches_into_the_guard_behind_the_playhead() {
        // A counter so far ahead of the clock that clock + ring - guard
        // comes first: the window, not the depth, bounds the fill. PCM
        // past the window overwrites what the link is still playing.
        let lead = (RING - RING_GUARD) as u64;
        assert_eq!(
            fill_span(0, 0, lead, RING, RING_GUARD, FILL_DEPTH, FRAME),
            RING - RING_GUARD
        );
        // And a fill already at the window edge writes nothing more.
        assert_eq!(
            fill_span(lead, 0, lead, RING, RING_GUARD, FILL_DEPTH, FRAME),
            0
        );
        // A lead that has eaten the whole window: nothing, not a wrap.
        assert_eq!(
            fill_span(0, 0, 2 * RING as u64, RING, RING_GUARD, FILL_DEPTH, FRAME),
            RING - RING_GUARD
        );
    }

    #[test]
    fn the_ring_covers_the_lead_qemu_was_seen_to_run_at() {
        // 106 KB of reported position ahead of the link clock, plus the
        // depth, still leaves the window open. With the 64 KiB ring this
        // was zero: free = 0 with nothing audible queued.
        let lead = 106_192u64;
        let span = fill_span(0, 0, lead, RING, RING_GUARD, FILL_DEPTH, FRAME);
        assert_eq!(span, lead as usize + FILL_DEPTH);
        assert!(span + BDL_SEGMENT < RING - RING_GUARD);
    }

    /// Stereo S16LE frames of one constant sample.
    fn frames(frames: usize, value: i16) -> Vec<u8> {
        core::iter::repeat_n(value.to_le_bytes(), frames * 2)
            .flatten()
            .collect()
    }

    fn samples(block: &[u8]) -> Vec<i16> {
        block
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect()
    }

    /// The mixer step the fill runs: every active stream is summed, a
    /// short one is silence past its end, a paused one gives nothing, and
    /// the count says whether the block holds anyone's PCM (a fill with
    /// no contributor is a gap, and the gap is what the idle stop and the
    /// underrun counter hang off).
    #[test]
    fn a_block_is_the_saturated_sum_of_every_active_stream() {
        let mut streams = vec![
            (0u32, HostStream::new(4096, 2)),
            (1u32, HostStream::new(4096, 2)),
            (2u32, HostStream::new(4096, 2)),
        ];
        streams[0].1.write(&frames(8, 1000));
        streams[1].1.write(&frames(4, -300)); // shorter: silence after 4 frames
        streams[2].1.write(&frames(8, 5)); // and paused: nothing at all
        streams[2].1.pause();
        let mut acc = vec![0i32; 64];
        let mut pull = vec![0u8; 128];
        let mut out = vec![0u8; 8 * 4];
        assert_eq!(mix_streams(&mut streams, &mut acc, &mut pull, &mut out), 2);
        let got = samples(&out);
        assert!(got[..8].iter().all(|&v| v == 700), "{:?}", got);
        assert!(got[8..].iter().all(|&v| v == 1000), "{:?}", got);
        // The streams were drained by what was pulled, the paused one not.
        assert_eq!(streams[0].1.queued_link(), 0);
        assert_eq!(streams[1].1.queued_link(), 0);
        assert_eq!(streams[2].1.queued_link(), 32);

        // Nothing left in any active stream: a gap, and a block of zeros.
        assert_eq!(mix_streams(&mut streams, &mut acc, &mut pull, &mut out), 0);
        assert!(samples(&out).iter().all(|&v| v == 0));

        // The sum saturates once, at the end: two full-scale streams are
        // full scale, not wrapped to negative.
        streams[0].1.write(&frames(8, i16::MAX));
        streams[1].1.write(&frames(8, i16::MAX));
        assert_eq!(mix_streams(&mut streams, &mut acc, &mut pull, &mut out), 2);
        assert!(samples(&out).iter().all(|&v| v == i16::MAX));
    }

    #[test]
    fn a_fill_is_whole_frames_and_never_negative() {
        // Not cosmetic: a byte count that is not a multiple of the frame
        // leaves the ring straddling a sample, and every left sample is
        // read as a right one for the rest of the stream.
        assert_eq!(fill_span(1, 0, 0, RING, RING_GUARD, 10, FRAME), 8);
        assert_eq!(fill_span(3, 0, 0, RING, RING_GUARD, 10, FRAME), 4);
        // Fill ahead of the target: zero, not a wrap to 2^64.
        assert_eq!(fill_span(u64::MAX, 0, 0, RING, RING_GUARD, 10, FRAME), 0);
        // A zero frame size asks for nothing instead of dividing by zero.
        assert_eq!(fill_span(0, 0, 0, RING, RING_GUARD, FILL_DEPTH, 0), 0);
        // A ring smaller than its guard (an uninitialised `ring_len`) too.
        assert_eq!(fill_span(0, 0, 0, 0, RING_GUARD, FILL_DEPTH, FRAME), 0);
    }
}
