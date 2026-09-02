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

pub trait AudioScheme: Scheme {
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

    /// Bytes queued but not yet played out.
    fn queued_bytes(&self) -> usize;

    /// Stop playback and drop any queued PCM.
    fn reset(&self) -> DeviceResult;

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
