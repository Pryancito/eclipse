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

use crate::audio::pipeline::src::Resampler;
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
/// The ring is the audio latency: `/dev/dsp` blocks the writer only when the
/// ring is full, so an application always has it filled and every sample it
/// writes waits a ring's worth of playback before it is heard. 256 KiB was
/// sized for a DMA engine believed to fetch ~96 KB ahead of the link; that
/// figure turned out to be an artefact of the emulator it was measured in,
/// and once the play position came from the link clock rather than from the
/// position registers the ring stopped having to cover it at all. 341 ms is
/// still several times what a desktop audio stack runs with, and leaves the
/// writer -- a userspace loop subject to scheduling -- an ample margin.
const RING_PAGES: usize = 16;
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

/// Silence kept ahead of the write pointer, so that an engine that runs past
/// the end of what was written plays silence -- not the previous lap -- for
/// as long as it takes the next poll to notice and stop it. Much smaller
/// than the guard: the band must end well short of the estimated play
/// position, never at it, for the same start-up-latency reason. It ends
/// 12 KiB (64 ms) short of it.
const SILENCE_AHEAD: usize = 4096;

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

/// Silence left between the furthest point the engine could have fetched
/// up to and the place where the next write lands after a gap.
///
/// During a gap the engine is running and its fetch position is somewhere
/// past the link's play position: the controller's counters over-report by
/// up to a few segments on the hardware this runs on (see [`RING_GUARD`]),
/// and whatever they say, a fetch is a burst of up to one BDL segment. PCM
/// written where the engine has already been is played as the zeros that
/// were there, i.e. the start of the sound is cut. One segment past the
/// highest position any counter reports is beyond every burst.
const PARK_MARGIN: usize = BDL_SEGMENT;

const STOP_HISTORY: usize = 4;

/// One gap (the ring running out of client PCM), as recorded for
/// `/proc/gpusnd`.
#[derive(Clone, Copy, Default)]
struct StopEvent {
    /// `b'U'` underrun, `b'D'` drain, `b'P'` a prepare that kept the engine
    /// running (see [`HdaInner::soft_prepare`]), 0 = unused slot.
    kind: u8,
    /// Milliseconds after the stream started, by the kernel clock.
    at_ms: u64,
    /// The same interval by the controller's 24 MHz wall clock.
    wall_ms: u64,
    /// Bytes written to that stream by then.
    written: usize,
}

/// Where the next write lands once the ring has run out of client PCM and
/// the engine is left running: past the furthest position any counter
/// reports (`consumed` is the link clock's estimate, the two totals are the
/// controller's own counters, whichever of them over-reports), plus
/// [`PARK_MARGIN`]. Bytes since RUN, like its inputs.
fn park_target(consumed: u64, lpib_total: u64, dpib_total: u64, margin: u64) -> u64 {
    consumed.max(lpib_total).max(dpib_total) + margin
}

/// Client PCM still queued, given the raw ring occupancy `queued` and the
/// silence pad that ends at stream position `pad_end`: the pad is ring
/// space the driver filled on its own, and it is not the client's to count.
/// Ring space the client may still write into: everything but the guard and
/// what is already queued.
///
/// `saturating_sub`, like `exposed_queued` and `frame_bytes` and every other
/// bit of arithmetic here. It holds today -- `queued` is clamped to
/// `ring_len - RING_GUARD` at both places that set it, and `RING_PAGES *
/// PAGE_SIZE` is four times `RING_GUARD` -- but this is the number the write
/// path uses to decide how much to copy INTO the ring, so an underflow would
/// not be a panic in release: it would be a free-space figure near 2^64 and a
/// copy past the end of the ring.
fn free_bytes_of(ring_len: usize, guard: usize, queued: usize) -> usize {
    ring_len.saturating_sub(guard).saturating_sub(queued)
}

/// How much of a `rewind`/`forward` request can actually be honoured: never
/// more PCM than the client still has queued, and always a whole number of
/// frames.
///
/// The frame rounding is not cosmetic. These move `wp` and zero a span of the
/// ring, so a byte count that is not a multiple of the frame size leaves the
/// ring straddling a sample: from there on every left sample is read as a
/// right one and the image swaps channels for the rest of the stream.
fn honourable_bytes(bytes: usize, exposed: usize, frame: usize) -> usize {
    if frame == 0 {
        return 0;
    }
    bytes.min(exposed) / frame * frame
}

fn exposed_queued(queued: usize, pad_end: u64, consumed: u64) -> usize {
    let pad = pad_end.saturating_sub(consumed);
    queued.saturating_sub(pad.min(queued as u64) as usize)
}

// ── Fixed-rate sink ─────────────────────────────────────────────────────────
//
// The HDA link runs at ONE rate, `LINK_RATE`, whatever rate a client asks
// for. A client at another rate is resampled into the ring by
// `crate::audio::pipeline::src` on the way in (SOF's SRC sits in front of
// its DAI the same way). Two things come of that:
//
//  * The stream is never reprogrammed for a rate change. On an HDMI/DP sink
//    a format change is a re-lock, and the monitor mutes for a few hundred
//    milliseconds while it does it -- the missing first half-second of every
//    44.1 kHz track after a 48 kHz one. Fixing the sink rate in PulseAudio's
//    config already avoided that; this moves the resampling that costs into
//    the kernel, where the converter is the polyphase one rather than
//    speex's.
//  * The ring, and every counter derived from it (positions, `queued`,
//    `consumed`, the gap pad), stays in LINK frames. Only the four numbers
//    the front ends see -- free, queued, delay, buffer -- and the write
//    itself cross into CLIENT frames, through the helpers below. Frame SIZE
//    is the same on both sides (S16LE stereo throughout); only the frame
//    COUNT scales, by `client_rate / LINK_RATE`.
//
// A client at exactly `LINK_RATE` takes the same path as before this
// existed: no converter, byte-for-byte copy, identity conversions.

/// The one rate the HDA stream is ever programmed at.
const LINK_RATE: u32 = 48000;

/// Link frames held back below the free space when sizing a resampled
/// write. The converter's output for N input frames is `N * fout / fin`
/// give or take one (the fractional phase carries between calls); this keeps
/// that one, and a couple more, from landing past what fits.
const SRC_SLACK_FRAMES: usize = 4;

/// Client rates outside this range are clamped rather than refused: the
/// converter's window covers ratios down to about a third, and nothing a
/// desktop plays sits outside it.
const CLIENT_RATE_MIN: u32 = 8000;
const CLIENT_RATE_MAX: u32 = 192_000;

/// How many CLIENT frames a resampled write may take when the ring has
/// `free_link_frames` free: the largest count whose converted output is sure
/// to fit. Zero when there is no room for the slack -- the client then sees
/// a full ring and polls, which is the normal answer.
///
/// This is the one number `free_bytes` reports AND the one `write` accepts,
/// so a client that just read "N free" and offers N is never turned away
/// with 0 (alsa-lib's `try_recover` aborts PulseAudio on EAGAIN right after
/// `snd_pcm_avail()` said there was room).
fn accept_client_frames(free_link_frames: usize, fin: u32, fout: u32) -> usize {
    if fin == 0 || fout == 0 {
        return 0;
    }
    let usable = free_link_frames.saturating_sub(SRC_SLACK_FRAMES);
    (usable as u64 * fin as u64 / fout as u64) as usize
}

/// `link_bytes` of ring, seen as client bytes: the frame count scaled by
/// `fin / fout`, floored to a whole frame. Used for what is QUEUED and for
/// the buffer size, where under-reporting is the safe direction (a client
/// thinks slightly less is waiting, never more than it wrote).
fn link_to_client_bytes(link_bytes: usize, fin: u32, fout: u32, frame: usize) -> usize {
    if frame == 0 || fout == 0 {
        return 0;
    }
    let frames = (link_bytes / frame) as u64;
    (frames * fin as u64 / fout as u64) as usize * frame
}

/// `client_bytes` of a client's request, as ring bytes: the inverse of
/// [`link_to_client_bytes`], floored to a whole frame. For `rewind` and
/// `forward`, which name an amount of the client's own PCM.
fn client_to_link_bytes(client_bytes: usize, fin: u32, fout: u32, frame: usize) -> usize {
    if frame == 0 || fin == 0 {
        return 0;
    }
    let frames = (client_bytes / frame) as u64;
    (frames * fout as u64 / fin as u64) as usize * frame
}

/// Whether a `set_params` may keep the engine running (a soft prepare, see
/// [`HdaInner::soft_prepare`]) rather than stop and wipe the stream. Only
/// with the engine actually running on a descriptor that is still
/// programmed, not paused (a paused engine is stopped DMA, and restarting
/// it is the hard path's job), and the LINK format unchanged -- which, with
/// the link fixed, is every call after the first.
fn prepare_keeps_engine(
    running: bool,
    paused: bool,
    needs_reprogram: bool,
    link_unchanged: bool,
) -> bool {
    running && !paused && !needs_reprogram && link_unchanged
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
    /// Start hold (`AudioScheme::set_start_hold`): writes queue into the
    /// ring but the engine is not started until the hold is released.
    hold_start: bool,
    /// The stream descriptor was force-reset (`SRST`) because the engine
    /// would not acknowledge a cleared `RUN`, so its programming -- BDL
    /// address, cyclic length, format and above all the stream tag -- is
    /// gone. Setting `RUN` again on that descriptor starts a stream tagged 0,
    /// which no converter is listening on: the writer keeps being served, the
    /// ring never drains, and the client hears nothing until it gives up with
    /// an underrun. Anything that resumes has to reprogram first.
    needs_reprogram: bool,
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
    /// Ring offset up to which the silence ahead of `wp` has been written.
    zero_ptr: usize,
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
    /// `drains` and `underruns` both open a gap (the ring ran out of client
    /// PCM with the engine running): a drain is the engine reaching the end
    /// of what was written, an underrun the engine having already been past
    /// it when the shortfall was noticed. `idle_stops` is a gap lasting
    /// [`DAI_IDLE_STOP_US`] and the engine being stopped for it, `restarts`
    /// the full stream reset (codec verbs and all) on the next write after
    /// any stop, and `stop_timeouts` the engine not acknowledging a cleared
    /// RUN bit -- the case where wiping the ring would race a still-fetching
    /// DMA.
    stat_drains: u64,
    stat_underruns: u64,
    stat_idle_stops: u64,
    stat_restarts: u64,
    /// Prepares (a `set_params`, i.e. a client rate change or a re-prepare)
    /// served with the engine kept running: the queued PCM dropped and a
    /// gap opened, as a drain does, instead of a stop and a full restart.
    /// See [`HdaInner::soft_prepare`]. With this at work a rate switch adds
    /// nothing to `restarts`.
    stat_soft_prepares: u64,
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
    /// When the current stream started and how much has been written to it,
    /// so a stop can be placed within the stream it ended.
    stream_start_us: u64,
    stream_written: usize,
    /// The last few stops, newest last: WHEN in the stream and after how
    /// much data, which is what tells a stop mid-tone from the tone ending.
    stops: [StopEvent; STOP_HISTORY],

    rate: u32,
    channels: u8,

    /// `timer_now_as_micros()` when the ring last ran out of client PCM
    /// with the engine left running (a gap), or 0 while it holds some. The
    /// engine plays silence for the gap's duration and is stopped after
    /// [`DAI_IDLE_STOP_US`] of it.
    gap_since_us: u64,
    /// Stream position (bytes since RUN) where the silence pad parked ahead
    /// of the playhead during a gap ends, or 0. Ring bytes below it are
    /// zeros the driver put there, not client PCM: [`HdaInner::exposed_queued`]
    /// leaves them out so a client's hardware pointer only ever moves
    /// through bytes the client wrote, while [`AudioScheme::delay_bytes`]
    /// counts them, since they do play before the next write does.
    pad_end: u64,

    /// Software playback gain (HDMI has no analog volume), applied while
    /// copying S16LE into the ring so OSS and ALSA share one control. The
    /// percent/mute pair is what the mixer control reads back; `vol` is the
    /// ramped Q8.16 gain the copy actually applies (see
    /// `crate::audio::pipeline::volume`), so a slider notch or a mute is a
    /// fade over a few milliseconds of audio and not a step in the waveform.
    gain_l: u8,
    gain_r: u8,
    mute_l: bool,
    mute_r: bool,
    vol: Volume,

    /// The rate the client negotiated (what [`AudioScheme::params`] reports
    /// and what its byte counts are in). The ring itself is always at
    /// [`LINK_RATE`]; see the fixed-rate sink notes above `LINK_RATE`.
    client_rate: u32,
    /// The converter from `client_rate` into the ring, or `None` when the
    /// client is at `LINK_RATE` and the write is a plain copy.
    src: Option<Resampler>,
    /// Scratch for one resampled write: the client frames taken, the link
    /// frames they became, and those as the bytes the ring copy reads. Kept
    /// across writes so the hot path does not allocate.
    src_in: Vec<i16>,
    src_out: Vec<i16>,
    src_bytes: Vec<u8>,
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

    /// Keep [`SILENCE_AHEAD`] bytes of zeros immediately after the write
    /// pointer. Incremental: only the bytes newly exposed since the last call
    /// are written, so the cost is one extra store per byte of audio.
    ///
    /// The zone is the start of the free region, i.e. the oldest played data;
    /// it ends at least `RING_GUARD - SILENCE_AHEAD` short of the estimated
    /// play position, so a small error in that estimate cannot silence audio
    /// still to be played -- which is what #1096 removed and this must not
    /// bring back.
    fn silence_ahead(&mut self) {
        let ring = self.ring_len;
        let have = (self.zero_ptr + ring - self.wp) % ring;
        let (start, len) = if have == SILENCE_AHEAD {
            return;
        } else if have < SILENCE_AHEAD {
            (self.zero_ptr, SILENCE_AHEAD - have)
        } else {
            // The writer overtook the zone (a write longer than it): restart
            // from the write pointer. Never touch anything before it.
            (self.wp, SILENCE_AHEAD)
        };
        self.zero_range(start, len);
        self.zero_ptr = (self.wp + SILENCE_AHEAD) % ring;
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
            written: self.stream_written,
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

    /// Fold DMA progress since the last poll into `queued`, and when the
    /// ring has run out of client PCM keep the engine fed with silence (see
    /// [`HdaInner::run_gap`]).
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
        let written = self.stream_written as u64;
        let queued = written.saturating_sub(self.consumed) as usize;
        // Never more than the writer is allowed to have in flight, so a
        // garbage value cannot underflow `free_bytes`.
        self.queued = queued.min(ring - RING_GUARD);
        if self.queued == 0 || self.gap_since_us != 0 {
            self.run_gap(now_us, reported);
        }
    }

    /// The ring holds no client PCM (or the gap that started when it ran
    /// out is still open). This is where the driver used to stop the
    /// engine; it now does what SOF's DAI does -- keeps it running and gives
    /// it silence -- and stops only after [`DAI_IDLE_STOP_US`] of that.
    ///
    /// Each poll re-parks the write pointer just past the engine
    /// ([`park_target`]), over zeros, and books the stretch between the
    /// playhead and that point as a pad (`pad_end`) so the next write lands
    /// where the engine has not been and the client's queue count stays
    /// honest. The whole ring is zeroed once, when the gap opens: at that
    /// moment everything in it has been played.
    fn run_gap(&mut self, now_us: u64, reported: u64) {
        if self.gap_since_us == 0 {
            // The link has played everything written. If the engine's own
            // position also passed the tail, it got there before the writer
            // did: that is the underrun; otherwise the stream simply ended.
            if reported > self.stream_written as u64 {
                self.stat_underruns += 1;
                self.record_stop(b'U');
            } else {
                self.stat_drains += 1;
                self.record_stop(b'D');
            }
            self.gap_since_us = now_us;
            self.silence_ring();
        } else if now_us.wrapping_sub(self.gap_since_us) >= DAI_IDLE_STOP_US {
            self.stat_idle_stops += 1;
            if self.stop_stream() {
                self.silence_ring();
            }
            self.gap_since_us = 0;
            self.pad_end = 0;
            self.queued = 0;
            self.wp = 0;
            self.zero_ptr = 0;
            self.last_lpib = 0;
            return;
        }
        self.park_write_pointer();
    }

    /// Re-park the write pointer just past the engine over a zeroed ring,
    /// booking the stretch from the playhead to there as the silence pad.
    /// The tail of [`run_gap`](HdaInner::run_gap), on its own so a
    /// [`soft_prepare`](HdaInner::soft_prepare) can do the same thing.
    fn park_write_pointer(&mut self) {
        let ring = self.ring_len;
        let park = park_target(
            self.consumed,
            self.lpib_total,
            self.dpib_total,
            PARK_MARGIN as u64,
        );
        self.wp = (park % ring as u64) as usize;
        // The ring is all zeros: the silence band ahead of `wp` is already
        // there, so tell `silence_ahead` so it does not rewrite it per poll.
        self.zero_ptr = (self.wp + SILENCE_AHEAD) % ring;
        self.stream_written = park as usize;
        self.pad_end = park;
        self.queued = ((park - self.consumed) as usize).min(ring - RING_GUARD);
    }

    /// A prepare that keeps the engine running.
    ///
    /// `set_params` is ALSA's prepare: drop whatever is queued and start the
    /// stream over. It used to do that by stopping the engine and wiping
    /// the ring, and the next write then restarted everything -- codec
    /// verbs, pin sense, the HDMI kick -- which on an HDMI/DP sink is a
    /// re-lock and a mute of a few hundred milliseconds. With the link
    /// fixed at [`LINK_RATE`] a client rate change never alters the stream
    /// format, so there is nothing to reprogram: the queued PCM is dropped
    /// and the engine is left running into a gap, exactly the state a drain
    /// leaves it in. Both steps are the ones every natural drain already
    /// takes (`run_gap` zeroes the whole ring with the engine running and
    /// then re-parks the writer); the only difference is that here the
    /// zeroed bytes had not all been played yet -- that is the point, they
    /// are being discarded -- and the engine hears at most a fetch's worth
    /// of the old stream before the zeros land, then silence until the new
    /// stream's first write parks past it.
    ///
    /// Only called when [`prepare_keeps_engine`] says so; the caller has
    /// polled progress first so `consumed` and the position counters are
    /// fresh for the park.
    fn soft_prepare(&mut self, now_us: u64) {
        self.stat_soft_prepares += 1;
        self.record_stop(b'P');
        self.paused = false;
        self.gap_since_us = now_us;
        self.silence_ring();
        self.park_write_pointer();
    }

    /// Client PCM queued: the ring occupancy less the silence pad.
    fn exposed_queued(&self) -> usize {
        exposed_queued(self.queued, self.pad_end, self.consumed)
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

    fn free_bytes(&self) -> usize {
        free_bytes_of(self.ring_len, RING_GUARD, self.queued)
    }

    /// `(client_rate, LINK_RATE)` when a converter is in the path.
    fn src_rates(&self) -> Option<(u32, u32)> {
        self.src.as_ref().map(|s| s.rates())
    }

    /// Ring bytes as the client counts them (identity without a converter).
    fn to_client_bytes(&self, link_bytes: usize) -> usize {
        match self.src_rates() {
            Some((fin, fout)) => link_to_client_bytes(link_bytes, fin, fout, self.frame_bytes()),
            None => link_bytes,
        }
    }

    /// A client's byte count as ring bytes (identity without a converter).
    fn to_link_bytes(&self, client_bytes: usize) -> usize {
        match self.src_rates() {
            Some((fin, fout)) => client_to_link_bytes(client_bytes, fin, fout, self.frame_bytes()),
            None => client_bytes,
        }
    }

    /// Client bytes a write may take right now. This is what `free_bytes`
    /// reports to the front ends and exactly what `write` accepts, so the
    /// two never disagree (see [`accept_client_frames`]).
    fn client_free_bytes(&self) -> usize {
        let free = self.free_bytes();
        match self.src_rates() {
            Some((fin, fout)) => {
                let frame = self.frame_bytes();
                accept_client_frames(free / frame, fin, fout) * frame
            }
            None => free,
        }
    }

    /// Convert `client_frames` whole frames of client S16LE `pcm` through the
    /// resampler into `self.src_bytes` (ring-rate S16LE). Returns the link
    /// byte count. Only called with a converter present.
    fn resample_client(&mut self, pcm: &[u8], client_frames: usize) -> usize {
        let frame = self.frame_bytes();
        let (samples, _) = pcm[..client_frames * frame].as_chunks::<2>();
        self.src_in.clear();
        self.src_in
            .extend(samples.iter().map(|s| i16::from_le_bytes(*s)));
        self.src_out.clear();
        if let Some(src) = self.src.as_mut() {
            src.process(&self.src_in, &mut self.src_out);
        }
        self.src_bytes.clear();
        self.src_bytes
            .extend(self.src_out.iter().flat_map(|s| s.to_le_bytes()));
        self.src_bytes.len()
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
            // SRST clears the descriptor, tag included: it cannot simply be
            // restarted. See [`HdaInner::needs_reprogram`].
            self.needs_reprogram = true;
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
        let n = honourable_bytes(bytes, self.exposed_queued(), self.frame_bytes());
        if n == 0 {
            return 0;
        }
        self.wp = (self.wp + self.ring_len - n) % self.ring_len;
        self.queued -= n;
        // The bytes are un-written as far as the play position is concerned.
        self.stream_written = self.stream_written.saturating_sub(n);
        self.zero_range(self.wp, n);
        self.zero_ptr = self.wp;
        self.silence_ahead();
        // A running engine that now has nothing left is a gap, handled by
        // the next poll; only a stopped one is reset here.
        if self.queued == 0 && !self.running {
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
        let n = honourable_bytes(bytes, self.exposed_queued(), self.frame_bytes());
        if n == 0 {
            return 0;
        }
        let start = if self.running {
            self.lpib() as usize % self.ring_len
        } else {
            (self.wp + self.ring_len - self.queued) % self.ring_len
        };
        // The skipped bytes still occupy the ring and still take their time
        // to play (as silence); the queue depth is what the link clock says
        // it is, so it is not adjusted here.
        self.zero_range(start, n);
        if self.queued == 0 && !self.running {
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
        if !self.running {
            // Nothing is fetching: a stopped stream, or one primed under a
            // start hold whose descriptor has never been programmed. Marking
            // that "paused" would make the hold's release skip the start
            // and the resume set RUN on a descriptor with no stream tag.
            return;
        }
        self.stop_stream();
        self.paused = true;
    }

    /// Set RUN without resetting the stream descriptor (LPIB stays put).
    fn resume_stream(&mut self) -> DeviceResult {
        self.paused = false;
        if self.running || self.queued == 0 || self.hold_start {
            // A held ring starts when the hold is released, through the
            // full programming path; RUN alone would start stream 0.
            return Ok(());
        }
        if self.needs_reprogram {
            // The descriptor was wiped by a forced reset while this PCM was
            // stopped, so the queued bytes have nothing to play them: their
            // ring offsets no longer mean anything to an engine that will
            // restart at 0. Drop them and go back to the stopped state the
            // write path knows how to start from -- the client hears the gap,
            // which is what actually happened, instead of an indefinite
            // silence that ends in an underrun.
            warn!("[hda] resume after a forced stream reset: dropping the queued PCM");
            self.silence_ring();
            self.wp = 0;
            self.zero_ptr = 0;
            self.queued = 0;
            self.last_lpib = 0;
            self.gap_since_us = 0;
            self.pad_end = 0;
            return Ok(());
        }
        // The link clock kept counting while paused; the stream did not.
        self.wall_last = mmio_r32(self.bar, REG_WALCLK);
        self.last_poll_us = timer_now_as_micros();
        if self.gap_since_us != 0 {
            // Neither did the idle clock.
            self.gap_since_us = self.last_poll_us;
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
        self.paused = false;
        self.needs_reprogram = false;
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
        self.gap_since_us = 0;
        self.pad_end = 0;
        // `stream_written` is NOT reset here: the write path zeroes it when
        // it re-anchors an empty ring, and a ring filled under a start hold
        // already holds bytes that this start is about to play.
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

    use super::{
        accept_client_frames, client_to_link_bytes, exposed_queued, free_bytes_of,
        honourable_bytes, link_to_client_bytes, nearest_rate, park_target, prepare_keeps_engine,
        stream_format, sub_nodes, Resampler, LINK_RATE, SRC_SLACK_FRAMES,
    };

    /// The one case a prepare keeps the engine running: running, not
    /// paused, still programmed, same link format. Any other combination
    /// takes the stop-and-wipe path, so a soft prepare can never be asked
    /// to re-park an engine that is not actually fetching.
    #[test]
    fn a_prepare_keeps_the_engine_only_when_it_is_running_and_the_link_is_unchanged() {
        assert!(prepare_keeps_engine(true, false, false, true));
        // Stopped: nothing to keep running.
        assert!(!prepare_keeps_engine(false, false, false, true));
        // Paused DMA is stopped DMA; the hard path restarts it.
        assert!(!prepare_keeps_engine(true, true, false, true));
        // A descriptor that lost its programming must be set up again.
        assert!(!prepare_keeps_engine(true, false, true, true));
        // A link format change is a real reprogram.
        assert!(!prepare_keeps_engine(true, false, false, false));
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
    fn the_park_target_clears_whichever_counter_reports_furthest() {
        // Parking behind any of the three counters means writing where the
        // engine has already been, which it never fetches: silence that never
        // ends.
        assert_eq!(park_target(100, 50, 50, 8), 108, "the link clock is ahead");
        assert_eq!(park_target(50, 100, 50, 8), 108, "LPIB is ahead");
        assert_eq!(park_target(50, 50, 100, 8), 108, "DPIB is ahead");
        assert_eq!(park_target(0, 0, 0, 0), 0);
    }

    #[test]
    fn the_silence_pad_is_not_counted_as_the_clients_pcm() {
        // The pad is ring space the driver filled on its own. Counting it as
        // queued makes the client's own "how much is left to play" answer too
        // large, and it waits for a drain that already happened.
        assert_eq!(
            exposed_queued(1000, 0, 0),
            1000,
            "no pad, nothing to take off"
        );
        assert_eq!(exposed_queued(1000, 400, 0), 600, "400 bytes of it are pad");
        assert_eq!(
            exposed_queued(1000, 400, 400),
            1000,
            "a pad the engine has already played is no longer in the ring"
        );
        assert_eq!(
            exposed_queued(100, 1_000_000, 0),
            0,
            "a pad larger than the ring cannot make the answer negative"
        );
        assert_eq!(
            exposed_queued(1000, 0, 5000),
            1000,
            "consumed past the pad end must not add anything back"
        );
        // Note for whoever mutates this: dropping the `.min(queued as u64)`
        // from `exposed_queued` survives every test here, and it is right
        // that it does. `saturating_sub` already floors at zero, so on a
        // 64-bit target the `min` changes nothing. It is load-bearing only
        // where `usize` is narrower than `u64` and `pad as usize` would
        // truncate a large pad to a small one -- and then it would
        // under-subtract instead of saturating. Every target this kernel
        // builds for is 64-bit, so the mutation is EQUIVALENT here, not a
        // gap in the tests. Do not go chasing it.
    }

    #[test]
    fn the_free_space_never_wraps_past_the_end_of_the_ring() {
        // The write path copies this many bytes into the ring. An underflow
        // here is not a panic in release -- it is a free-space figure near
        // 2^64 and a copy off the end of the ring.
        assert_eq!(free_bytes_of(65536, 16384, 0), 49152);
        assert_eq!(free_bytes_of(65536, 16384, 49152), 0, "exactly full");
        assert_eq!(
            free_bytes_of(65536, 16384, 49153),
            0,
            "one byte over full is zero free, not 2^64 - 1"
        );
        assert_eq!(free_bytes_of(65536, 16384, usize::MAX), 0);
        // And a ring smaller than its own guard, which is what an
        // uninitialised `ring_len` of 0 looks like.
        assert_eq!(free_bytes_of(0, 16384, 0), 0);
        assert_eq!(free_bytes_of(1024, 16384, 0), 0);
    }

    #[test]
    fn a_rewind_is_rounded_down_to_whole_frames() {
        // Not cosmetic: `wp` moves by this number and a half-frame offset
        // swaps left and right for the rest of the stream.
        assert_eq!(honourable_bytes(100, 1000, 4), 100);
        assert_eq!(honourable_bytes(101, 1000, 4), 100);
        assert_eq!(honourable_bytes(103, 1000, 4), 100);
        assert_eq!(
            honourable_bytes(3, 1000, 4),
            0,
            "less than a frame is nothing"
        );
    }

    #[test]
    fn a_rewind_never_exceeds_what_the_client_still_has_queued() {
        assert_eq!(honourable_bytes(10_000, 400, 4), 400);
        assert_eq!(
            honourable_bytes(10_000, 402, 4),
            400,
            "and still whole frames"
        );
        assert_eq!(honourable_bytes(10_000, 0, 4), 0);
        assert_eq!(honourable_bytes(usize::MAX, 1024, 4), 1024);
    }

    #[test]
    fn a_zero_frame_size_asks_for_nothing_instead_of_dividing_by_zero() {
        // `frame_bytes()` is `channels.max(1) * 2`, so it cannot be zero
        // today; the guard is here because the bare `/ frame` this replaced
        // would be a divide-by-zero panic in the kernel if it ever were.
        assert_eq!(honourable_bytes(1000, 1000, 0), 0);
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
            needs_reprogram: false,
            wp: 0,
            queued: 0,
            last_lpib: 0,
            last_poll_us: 0,
            dma_pos_va,
            last_dpib: 0,
            dpib_trusted: false,
            zero_ptr: 0,
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
            stat_soft_prepares: 0,
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
            stream_written: 0,
            hold_start: false,
            stops: [StopEvent::default(); STOP_HISTORY],
            rate: 48000,
            channels: 2,
            gap_since_us: 0,
            pad_end: 0,
            gain_l: 100,
            gain_r: 100,
            mute_l: false,
            mute_r: false,
            vol: Volume::new(48000, 2),
            client_rate: LINK_RATE,
            src: None,
            src_in: Vec::new(),
            src_out: Vec::new(),
            src_bytes: Vec::new(),
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
        // The client gets the rate it asked for (within reason); the link
        // stays at LINK_RATE and the difference is resampled on the way in.
        // See the fixed-rate sink notes above `LINK_RATE`.
        let client_rate = rate.clamp(CLIENT_RATE_MIN, CLIENT_RATE_MAX);
        let _ = channels;
        let channels = 2u8; // stereo only for now
        let mut inner = self.inner.lock();
        if inner.client_rate != client_rate {
            // One line per rate switch: the last serial line before a
            // machine dies at a stream start says which step it reached.
            warn!(
                "[hda] set_params: client {} Hz -> {} Hz, link stays {} Hz (running={})",
                inner.client_rate, client_rate, LINK_RATE, inner.running
            );
        }
        let link_unchanged = inner.rate == LINK_RATE && inner.channels == channels;
        let soft = prepare_keeps_engine(
            inner.running,
            inner.paused,
            inner.needs_reprogram,
            link_unchanged,
        );
        if soft {
            // The link format is what it will be: drop the queued PCM and
            // leave the engine running into a gap, no stop, no restart, no
            // re-lock. Progress first, so the park lands past where the
            // engine really is.
            inner.poll_progress();
            let now = timer_now_as_micros();
            inner.soft_prepare(now);
        } else {
            inner.stop_stream();
            inner.paused = false;
            inner.queued = 0;
            inner.wp = 0;
            inner.zero_ptr = 0;
            inner.gap_since_us = 0;
            inner.pad_end = 0;
            unsafe { core::ptr::write_bytes(inner.ring_va as *mut u8, 0, inner.ring_len) };
            clflush_range(inner.ring_va, inner.ring_len);
        }
        inner.rate = LINK_RATE;
        inner.channels = channels;
        inner.client_rate = client_rate;
        // A fresh converter per prepare: its input history belongs to the
        // stream that was just dropped.
        inner.src = if client_rate != LINK_RATE {
            Some(Resampler::new(client_rate, LINK_RATE, channels as usize))
        } else {
            None
        };
        if !soft {
            // The link format and the infoframe only need (re)stating when
            // the stream is being set up from a stop; on a soft prepare
            // they are unchanged by construction, and re-sending the
            // infoframe is itself something a monitor may re-lock on.
            inner.vol.set_format(LINK_RATE, channels as usize);
            if inner.digital {
                let ch = channels;
                inner.send_audio_infoframe(ch);
            }
        }
        Ok((client_rate, channels))
    }

    fn params(&self) -> (u32, u8) {
        let inner = self.inner.lock();
        (inner.client_rate, inner.channels)
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
        let mut inner = self.inner.lock();
        inner.poll_progress();
        // A stream with nothing in flight: the next start begins at ring
        // offset 0. Under a start hold the ring fills across several writes
        // before the engine runs, so "fresh" is an empty ring, not merely a
        // stopped engine -- re-anchoring the pointers on the second held
        // write would drop the first one.
        let fresh = !inner.running && !inner.paused && inner.queued == 0;
        if fresh {
            warn!(
                "[hda] stream start: {} B offered at {} Hz, repick + stream reset next",
                pcm.len(),
                inner.rate
            );
            // Give the codec graph a chance to re-route to a pin that has
            // gained presence/ELD since the last pick (on NVIDIA GPUs the ELD
            // lands long after PCI probe)...
            inner.repick_path();
            // ...and re-anchor the software pointers: a started stream always
            // begins DMA at ring offset 0.
            inner.wp = 0;
            inner.zero_ptr = 0;
            inner.queued = 0;
            inner.stream_written = 0;
            inner.pad_end = 0;
        }
        let starting = !inner.running && !inner.paused && !inner.hold_start;
        // Whole frames only, so channels never swap on a partial write.
        let frame = inner.channels as usize * 2;
        // `n` is what the CLIENT wrote and what this call returns; `link`
        // is what goes into the ring. Without a converter they are the same
        // bytes. With one, `n` is sized so its converted output is sure to
        // fit (`accept_client_frames`), then converted into `src_bytes`.
        let n = inner.client_free_bytes().min(pcm.len()) / frame * frame;
        if n == 0 {
            return Ok(0);
        }
        let taken = if inner.src.is_some() {
            let link_len = inner.resample_client(pcm, n / frame);
            let free = inner.free_bytes();
            if link_len > free {
                // Cannot happen (the slack in `accept_client_frames` covers
                // the converter's ±1 frame, and the unit tests hold it to
                // that), but a copy past the ring is the one outcome never
                // worth risking: drop the excess rather than overrun.
                warn!(
                    "[hda] resampled write of {} B exceeds {} B free; truncating",
                    link_len, free
                );
            }
            let link_len = link_len.min(free) / frame * frame;
            let bytes = core::mem::take(&mut inner.src_bytes);
            let mut p = inner.wp;
            let mut done = 0;
            while done < link_len {
                let chunk = (link_len - done).min(inner.ring_len - p);
                let dst = inner.ring_va + p;
                inner.copy_pcm_scaled(&bytes[done..done + chunk], dst, chunk);
                clflush_range(dst, chunk);
                p = (p + chunk) % inner.ring_len;
                done += chunk;
            }
            inner.src_bytes = bytes;
            inner.wp = p;
            link_len
        } else {
            let mut p = inner.wp;
            let mut done = 0;
            while done < n {
                let chunk = (n - done).min(inner.ring_len - p);
                let dst = inner.ring_va + p;
                inner.copy_pcm_scaled(&pcm[done..done + chunk], dst, chunk);
                clflush_range(dst, chunk);
                p = (p + chunk) % inner.ring_len;
                done += chunk;
            }
            inner.wp = p;
            n
        };
        inner.queued += taken;
        inner.stream_written += taken;
        // Client PCM again: the gap, if one was open, ends here. The pad
        // ahead of it plays out on its own.
        inner.gap_since_us = 0;
        inner.silence_ahead();
        if starting {
            inner.start_stream()?;
            warn!(
                "[hda] stream started: {} B queued, CTL {:#x}",
                inner.queued,
                mmio_r32(inner.bar, inner.sd_base + SD_CTL)
            );
        }
        Ok(n)
    }

    // The four counts the front ends see are in CLIENT bytes: identical to
    // the ring's own when the client is at LINK_RATE, scaled by the rate
    // ratio when a converter is in the path.

    fn free_bytes(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.poll_progress();
        inner.client_free_bytes()
    }

    fn buffer_bytes(&self) -> usize {
        let inner = self.inner.lock();
        inner.to_client_bytes(inner.ring_len - RING_GUARD)
    }

    fn queued_bytes(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.poll_progress();
        inner.to_client_bytes(inner.exposed_queued())
    }

    fn delay_bytes(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.poll_progress();
        inner.to_client_bytes(inner.queued)
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
        inner.gap_since_us = 0;
        inner.pad_end = 0;
        inner.silence_ring();
        Ok(())
    }

    fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
        let mut inner = self.inner.lock();
        let link = inner.to_link_bytes(bytes);
        let done = inner.rewind_bytes(link);
        Ok(inner.to_client_bytes(done))
    }

    fn forward(&self, bytes: usize) -> DeviceResult<usize> {
        let mut inner = self.inner.lock();
        let link = inner.to_link_bytes(bytes);
        let done = inner.forward_bytes(link);
        Ok(inner.to_client_bytes(done))
    }

    fn pause(&self) -> DeviceResult {
        self.inner.lock().pause_stream();
        Ok(())
    }

    fn resume(&self) -> DeviceResult {
        self.inner.lock().resume_stream()
    }

    fn set_start_hold(&self, hold: bool) -> DeviceResult {
        let mut inner = self.inner.lock();
        inner.hold_start = hold;
        if hold || inner.running || inner.paused || inner.queued == 0 {
            return Ok(());
        }
        // Released with a primed ring: start it now, exactly as the write
        // that filled it would have.
        inner.start_stream()?;
        warn!(
            "[hda] stream started on trigger: {} B queued, CTL {:#x}",
            inner.queued,
            mmio_r32(inner.bar, inner.sd_base + SD_CTL)
        );
        Ok(())
    }

    fn set_gain(&self, left: u8, right: u8, mute_left: bool, mute_right: bool) -> DeviceResult {
        let mut inner = self.inner.lock();
        inner.gain_l = left.min(100);
        inner.gain_r = right.min(100);
        inner.mute_l = mute_left;
        inner.mute_r = mute_right;
        let (l, r) = (
            gain_from_percent(left, mute_left),
            gain_from_percent(right, mute_right),
        );
        inner.vol.set_target(0, l);
        inner.vol.set_target(1, r);
        Ok(())
    }

    fn gain(&self) -> (u8, u8, bool, bool) {
        let inner = self.inner.lock();
        (inner.gain_l, inner.gain_r, inner.mute_l, inner.mute_r)
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
            "[gpusnd] ring: running={} queued={} (pad {} B of driver silence ahead of it) wp={} rate={} ch={} client={} Hz src={} gap={}",
            inner.running,
            inner.exposed_queued(),
            inner.queued - inner.exposed_queued(),
            inner.wp,
            inner.rate,
            inner.channels,
            inner.client_rate,
            if inner.src.is_some() {
                "resampling"
            } else {
                "passthrough"
            },
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
            "[gpusnd] events: {} drains, {} underruns (gaps the engine idled through), {} idle stops, {} stream restarts, {} soft prepares (rate changes served without a restart), {} stop timeouts, {} rejected position reads{}",
            inner.stat_drains,
            inner.stat_underruns,
            inner.stat_idle_stops,
            inner.stat_restarts,
            inner.stat_soft_prepares,
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
                    b'P' => "prepare",
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

#[cfg(test)]
mod dai_tests {
    use super::*;

    #[test]
    fn park_lands_past_the_furthest_counter_plus_the_margin() {
        // The link clock is behind both counters: the engine has fetched
        // ahead, so the park goes past what the counters say.
        assert_eq!(park_target(1000, 9000, 4000, 8192), 9000 + 8192);
        assert_eq!(park_target(1000, 4000, 9000, 8192), 9000 + 8192);
        // Counters that lag the clock (a rejected read) do not pull it back.
        assert_eq!(park_target(9000, 0, 0, 8192), 9000 + 8192);
    }

    #[test]
    fn the_pad_is_not_the_clients_to_count() {
        // 16 KiB in the ring, 10 KiB of it the pad still ahead of the
        // playhead: the client sees only its own 6 KiB.
        assert_eq!(exposed_queued(16384, 30000, 20000 - 240), 16384 - 10240);
        // The pad shrinks as the link plays it and the client's share does
        // not change until the link reaches its data.
        assert_eq!(
            exposed_queued(16384 - 4096, 30000, 20000 - 240 + 4096),
            16384 - 10240
        );
        // Once the playhead is past the pad, everything queued is the client's.
        assert_eq!(exposed_queued(6144, 30000, 30000), 6144);
        assert_eq!(exposed_queued(6144, 30000, 31000), 6144);
        // Never negative, whatever the bookkeeping says.
        assert_eq!(exposed_queued(100, 30000, 0), 0);
        // No pad: identity.
        assert_eq!(exposed_queued(777, 0, 12345), 777);
    }
}
