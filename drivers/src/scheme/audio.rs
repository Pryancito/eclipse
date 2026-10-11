//! PCM audio output device scheme.
//!
//! The model is deliberately small: one always-running (while playing) cyclic
//! DMA ring per device, fed by [`AudioScheme::write`]. The consumer offers
//! bytes; the device accepts as many as currently fit and returns the count —
//! blocking policy (spin, yield, poll) belongs to the caller (the `/dev/dsp`
//! layer), not the driver.
//!
//! Sample format is interleaved little-endian signed 16-bit PCM throughout;
//! rate and channel count are negotiated with [`AudioScheme::set_params`].

use super::Scheme;
use crate::DeviceResult;
use alloc::sync::Arc;

pub trait AudioScheme: Scheme {
    /// A stream of this device's own for one client, mixed with every
    /// other open one -- the host side of a Sound Open Firmware playback
    /// pipeline (`host -> src -> mixer -> dai`), where the DAI never sees
    /// the applications. The handle is a full [`AudioScheme`]: the
    /// client's rate, buffer, counts, pause, hold and reset are all its
    /// own, and `set_gain`/`gain` reach the device's master volume. Its
    /// drop removes the stream. `Ok(None)` (the default) means the device
    /// has one ring and no mixer, and the front ends serialise opens on it
    /// with a claim instead.
    fn open_stream(&self) -> DeviceResult<Option<Arc<dyn AudioScheme>>> {
        Ok(None)
    }

    /// Negotiate the PCM output format. `rate` in Hz, `channels` interleaved
    /// S16LE. The device picks the nearest configuration it supports and
    /// returns the actual `(rate, channels)` now in effect. Implies
    /// [`reset`](AudioScheme::reset).
    fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)>;

    /// The `(rate, channels)` currently in effect.
    fn params(&self) -> (u32, u8);

    /// Queue PCM bytes for playback. Returns the number of bytes accepted —
    /// `0` when the ring is full (never an error for a full ring). Starts the
    /// hardware stream if it was stopped.
    fn write(&self, pcm: &[u8]) -> DeviceResult<usize>;

    /// Bytes that can currently be written without truncation.
    fn free_bytes(&self) -> usize;

    /// Total capacity of the playback ring in bytes.
    fn buffer_bytes(&self) -> usize;

    /// What [`buffer_bytes`](AudioScheme::buffer_bytes) will report once a
    /// client at `rate` Hz is negotiated with `set_params`, without
    /// changing anything. A device that resamples into a fixed-rate ring
    /// holds a rate-dependent number of CLIENT frames, and a front end
    /// sizing a buffer for a rate it has not applied yet must ask for that
    /// rate's figure: the one for the previous stream is not a bound the
    /// device can honour, and a buffer bigger than the ring is `avail`
    /// promising room that `write` then refuses. Default: rate-independent.
    fn buffer_bytes_at(&self, rate: u32) -> usize {
        let _ = rate;
        self.buffer_bytes()
    }

    /// Bytes queued but not yet played out, as the client must count them:
    /// EXACTLY [`buffer_bytes`](AudioScheme::buffer_bytes) less what
    /// [`write`](AudioScheme::write) would accept right now. The ALSA node
    /// derives both `avail` and the client's hardware pointer from this, and
    /// alsa-lib clients (PulseAudio's sink aborts) treat a write that takes
    /// nothing after `avail` said there was room as a driver bug. So any
    /// silence the device inserted on its own that still occupies the ring
    /// counts here too; a device that keeps its engine running through a
    /// gap lets the pointer step back once, by that silence, when the gap
    /// opens, rather than promise room it does not have.
    fn queued_bytes(&self) -> usize;

    /// Bytes the link still has to play before the last queued client byte
    /// is heard: everything [`queued_bytes`](AudioScheme::queued_bytes)
    /// counts plus any silence the device put ahead of it (a stream that
    /// kept its engine running through a gap fills the space between the
    /// playhead and the next write with zeros). This is ALSA's `DELAY` and
    /// OSS's `GETODELAY`. Default: the same as `queued_bytes`.
    ///
    /// [`queued_bytes`]: AudioScheme::queued_bytes
    fn delay_bytes(&self) -> usize {
        self.queued_bytes()
    }

    /// True while the DMA engine is fetching the ring. Used by the ALSA
    /// watchdog timer so an idle Pulse sink cannot leave RUN set (cyclic
    /// leftover of the last mpg123 buffer looping every ~0.5 s).
    fn is_playing(&self) -> bool {
        false
    }

    /// Stop playback and drop any queued PCM.
    fn reset(&self) -> DeviceResult;

    /// Drop the most recently queued `bytes` (ALSA `SNDRV_PCM_IOCTL_REWIND`).
    /// Returns how many bytes were actually discarded. Default: none.
    fn rewind(&self, bytes: usize) -> DeviceResult<usize> {
        let _ = bytes;
        Ok(0)
    }

    /// Skip the next `bytes` of queued PCM (ALSA `SNDRV_PCM_IOCTL_FORWARD`).
    /// Returns how many bytes were discarded. Default: none.
    fn forward(&self, bytes: usize) -> DeviceResult<usize> {
        let _ = bytes;
        Ok(0)
    }

    /// Pause DMA while keeping queued samples (ALSA `PAUSE` enable).
    fn pause(&self) -> DeviceResult {
        Ok(())
    }

    /// Resume a paused stream without dropping the ring.
    fn resume(&self) -> DeviceResult {
        Ok(())
    }

    /// Hold the stream start back: while `hold` is set, [`write`] queues PCM
    /// without starting the DMA engine, and clearing it starts the engine if
    /// anything is queued (a hold set on a running stream changes nothing).
    ///
    /// This is what an OSS client does with `SNDCTL_DSP_SETTRIGGER` -- clear
    /// `PCM_ENABLE_OUTPUT`, fill the buffer, set it -- and what ALSA's
    /// `start_threshold` describes: the first write must not be what starts
    /// the audio. Default: not supported, every write starts the stream.
    ///
    /// [`write`]: AudioScheme::write
    fn set_start_hold(&self, hold: bool) -> DeviceResult {
        let _ = hold;
        Ok(())
    }

    /// Set stereo playback gain. `left`/`right` are percents in `0..=100`;
    /// mute flags force silence on that channel regardless of percent.
    /// HDMI/DP pins have no analog volume, so implementations typically
    /// scale S16LE in [`write`](AudioScheme::write). Default is a no-op
    /// (full volume, unmuted).
    fn set_gain(&self, left: u8, right: u8, mute_left: bool, mute_right: bool) -> DeviceResult {
        let _ = (left, right, mute_left, mute_right);
        Ok(())
    }

    /// Current stereo gain: `(left%, right%, mute_left, mute_right)`.
    fn gain(&self) -> (u8, u8, bool, bool) {
        (100, 100, false, false)
    }

    /// Sort key for ALSA card 0 (the `default` PCM). Higher wins. HDMI/DP
    /// with a live display should outrank analog jacks so `aplay` and
    /// `amixer set Master` hit the monitor speakers.
    fn default_score(&self) -> i32 {
        0
    }

    /// Human-readable dump of what the device and its codec are actually
    /// doing, surfaced at `/proc/gpusnd`.
    ///
    /// Silence with no error is the hardest audio failure to diagnose: the
    /// ring drains, the stream reports running and every call returns `Ok`,
    /// yet nothing reaches the speaker. That can only be told apart by
    /// reading the state back OUT of the hardware — the pin's presence and
    /// ELD, the converter's stream id and format, the stream descriptor's
    /// RUN bit and its position counter — which is what implementations put
    /// here.
    fn diagnostics(&self) -> alloc::string::String {
        alloc::string::String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A PCM device that implements only the required methods of
    /// `AudioScheme`, so everything else a test reads is the default.
    struct BareAudio {
        /// How many bytes the ring holds queued, so the delegating defaults
        /// can be seen to delegate rather than return a constant.
        queued: AtomicUsize,
    }

    impl BareAudio {
        fn new(queued: usize) -> Self {
            Self {
                queued: AtomicUsize::new(queued),
            }
        }
    }

    impl Scheme for BareAudio {
        fn name(&self) -> &str {
            "bare-audio"
        }
    }

    impl AudioScheme for BareAudio {
        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            Ok((rate, channels))
        }
        fn params(&self) -> (u32, u8) {
            (48_000, 2)
        }
        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            self.queued.fetch_add(pcm.len(), Ordering::Relaxed);
            Ok(pcm.len())
        }
        fn free_bytes(&self) -> usize {
            self.buffer_bytes() - self.queued_bytes()
        }
        fn buffer_bytes(&self) -> usize {
            4096
        }
        fn queued_bytes(&self) -> usize {
            self.queued.load(Ordering::Relaxed)
        }
        fn reset(&self) -> DeviceResult {
            self.queued.store(0, Ordering::Relaxed);
            Ok(())
        }
    }

    /// A device with one ring and no mixer must say so with `None`, not with
    /// an error: the front end then serialises opens on the single ring, and
    /// an error would make the first `open("/dev/dsp")` fail outright.
    #[test]
    fn a_device_without_a_mixer_offers_no_private_stream() {
        assert!(BareAudio::new(0).open_stream().unwrap().is_none());
    }

    /// `delay_bytes` is ALSA's `DELAY`, and for a device that inserts no
    /// silence of its own it is exactly what is queued. Answering a larger
    /// figure by default would make every client's latency estimate wrong.
    #[test]
    fn the_default_delay_is_exactly_what_is_queued() {
        let dev = BareAudio::new(0);
        assert_eq!(dev.delay_bytes(), 0);
        dev.write(&[0u8; 512]).unwrap();
        assert_eq!(dev.queued_bytes(), 512);
        assert_eq!(dev.delay_bytes(), 512);
        dev.reset().unwrap();
        assert_eq!(dev.delay_bytes(), 0);
    }

    /// `buffer_bytes_at` must delegate, and it must IGNORE the rate it is
    /// given: a rate-independent ring holds the same bytes whatever the
    /// client asks for, and a figure that moved with the rate would have the
    /// front end size a buffer the ring cannot hold.
    #[test]
    fn the_buffer_size_is_rate_independent_by_default() {
        let dev = BareAudio::new(0);
        assert_eq!(dev.buffer_bytes_at(8_000), dev.buffer_bytes());
        assert_eq!(dev.buffer_bytes_at(44_100), dev.buffer_bytes());
        assert_eq!(dev.buffer_bytes_at(192_000), dev.buffer_bytes());
    }

    /// A rate of zero is nonsense but reachable from userspace; the default
    /// must not divide by it.
    #[test]
    fn asking_for_the_buffer_size_at_zero_hertz_does_not_fault() {
        assert_eq!(BareAudio::new(0).buffer_bytes_at(0), 4096);
    }

    /// `is_playing` defaults to false, which is what lets the ALSA watchdog
    /// stop an idle sink: a default of true would leave RUN set forever on
    /// every device that does not track its DMA engine.
    #[test]
    fn a_device_that_does_not_track_its_engine_reports_not_playing() {
        assert!(!BareAudio::new(1024).is_playing());
    }

    /// Rewind and forward discard NOTHING by default and say so by returning
    /// zero. A default that claimed to have dropped bytes would desynchronise
    /// the client's pointer from the ring.
    #[test]
    fn rewind_and_forward_discard_nothing_by_default() {
        let dev = BareAudio::new(2048);
        assert_eq!(dev.rewind(1024), Ok(0));
        assert_eq!(dev.forward(1024), Ok(0));
        assert_eq!(dev.queued_bytes(), 2048, "the ring was touched");
    }

    /// Pause, resume and the start hold succeed as no-ops. They are what an
    /// OSS client drives with `SETTRIGGER`; failing them would break clients
    /// on devices that simply have nothing to pause.
    #[test]
    fn the_transport_knobs_succeed_as_no_ops() {
        let dev = BareAudio::new(0);
        assert_eq!(dev.pause(), Ok(()));
        assert_eq!(dev.resume(), Ok(()));
        assert_eq!(dev.set_start_hold(true), Ok(()));
        assert_eq!(dev.set_start_hold(false), Ok(()));
    }

    /// The default gain is full volume, unmuted: a device with no analog
    /// volume (every HDMI/DP pin) must not come up reporting silence, or
    /// `amixer` shows a muted master on working speakers.
    #[test]
    fn the_default_gain_is_full_and_unmuted() {
        assert_eq!(BareAudio::new(0).gain(), (100, 100, false, false));
    }

    /// And setting it is accepted even though nothing happens, including the
    /// out-of-range percent a client may send.
    #[test]
    fn setting_the_gain_is_accepted_as_a_no_op() {
        let dev = BareAudio::new(0);
        assert_eq!(dev.set_gain(50, 50, false, false), Ok(()));
        assert_eq!(dev.set_gain(255, 0, true, true), Ok(()));
        assert_eq!(dev.gain(), (100, 100, false, false), "gain was stored");
    }

    /// Card 0 is picked by the highest score, so an unopinionated device must
    /// sit at zero: a non-zero default would let a driver that never thought
    /// about it outrank the monitor's HDMI pin.
    #[test]
    fn an_unopinionated_device_scores_zero_for_card_zero() {
        assert_eq!(BareAudio::new(0).default_score(), 0);
    }

    /// `/proc/gpusnd` on a device with nothing to say is empty, not a line of
    /// placeholder text that would read as a diagnosis.
    #[test]
    fn a_device_with_nothing_to_report_dumps_nothing() {
        assert!(BareAudio::new(0).diagnostics().is_empty());
    }

    /// The contract `queued_bytes` documents: it is exactly the ring less
    /// what `write` would take right now. A client computes `avail` from it,
    /// so the two must not disagree.
    #[test]
    fn what_is_queued_plus_what_is_free_is_the_whole_ring() {
        let dev = BareAudio::new(0);
        dev.write(&[0u8; 1500]).unwrap();
        assert_eq!(dev.queued_bytes() + dev.free_bytes(), dev.buffer_bytes());
    }
}
