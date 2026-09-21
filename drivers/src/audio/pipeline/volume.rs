//! Playback gain with a ramp, after SOF's volume component.
//!
//! SOF (`src/audio/volume/volume.c`, `volume_generic.c`) applies gain as a
//! Q8.16 multiplier (`VOL_ZERO_DB = 1 << 16`) with a rounded shift, never a
//! truncating one, and never jumps the gain: a new target is reached by a
//! ramp whose value is recomputed every few hundred microseconds of audio
//! (`VOL_RAMP_UPDATE_*_US`) and held constant in between. A gain that steps
//! from one value to another mid-stream puts a discontinuity into the
//! waveform -- a click at every notch of a volume slider, a thump on mute --
//! and a truncating multiply biases every sample towards negative
//! infinity, which at low volume is audible as a grain of distortion on top
//! of the quantisation noise. Both were what the driver did before this.
//!
//! Only the linear ramp is implemented (`SOF_VOLUME_LINEAR`); the ramp
//! length is fixed at [`RAMP_MS`] rather than negotiated per stream.

/// Q8.16 unity gain (SOF `VOL_ZERO_DB` for `VOL_QXY_Y = 16`).
pub const ZERO_DB: i32 = 1 << 16;

/// Length of a gain ramp, in milliseconds of audio.
///
/// SOF's topologies default to 400 ms for the PGA after the host mixer,
/// which is a fade rather than a control. This is a desktop volume slider:
/// each notch has to land before the next one arrives, and the click that
/// the ramp exists to remove is gone at anything over a few milliseconds.
/// 32 ms is SOF's `VOL_RAMP_UPDATE_THRESHOLD_FASTEST_MS`.
pub const RAMP_MS: u32 = 32;

/// How often the ramp gain is recomputed, in microseconds of audio (SOF
/// `VOL_RAMP_UPDATE_FAST_US`). Between updates the gain is held: a 12-frame
/// block at 48 kHz, well under anything audible as a step.
pub const RAMP_UPDATE_US: u32 = 250;

/// Most channels a stream can carry (SOF `PLATFORM_MAX_CHANNELS`).
pub const MAX_CHANNELS: usize = 8;

/// The Q8.16 gain for a mixer setting: a percent in `0..=100` of linear
/// amplitude, and a mute flag that wins over it.
pub fn gain_from_percent(percent: u8, mute: bool) -> i32 {
    if mute || percent == 0 {
        0
    } else if percent >= 100 {
        ZERO_DB
    } else {
        (percent as i32 * ZERO_DB) / 100
    }
}

/// One S16 sample scaled by a Q8.16 gain, rounded to nearest and saturated
/// (SOF `q_multsr_sat_32x32_16` with `Q_SHIFT_BITS_32(15, 16, 15)`).
#[inline]
fn scale_s16(sample: i16, gain: i32) -> i16 {
    let p = sample as i64 * gain as i64;
    let r = ((p >> 15) + 1) >> 1;
    r.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

/// Ramped per-channel gain for interleaved S16LE.
pub struct Volume {
    channels: usize,
    /// Frames per ramp update (gain held constant within).
    update_frames: u32,
    /// Frames a full ramp takes.
    ramp_frames: u32,
    /// Where each channel is going (`tvolume` in SOF).
    target: [i32; MAX_CHANNELS],
    /// Where the current ramp started from (`rvolume`).
    start: [i32; MAX_CHANNELS],
    /// Gain in effect right now (`volume`).
    cur: [i32; MAX_CHANNELS],
    /// Frames processed since the ramp started.
    elapsed: u32,
    ramping: bool,
}

impl Volume {
    /// Unity gain on every channel, no ramp in progress.
    pub fn new(rate: u32, channels: usize) -> Self {
        let mut v = Volume {
            channels: 1,
            update_frames: 1,
            ramp_frames: 0,
            target: [ZERO_DB; MAX_CHANNELS],
            start: [ZERO_DB; MAX_CHANNELS],
            cur: [ZERO_DB; MAX_CHANNELS],
            elapsed: 0,
            ramping: false,
        };
        v.set_format(rate, channels);
        v
    }

    /// Re-derive the ramp timing for a new stream format. The gains are
    /// kept: a rate switch is not a volume change.
    pub fn set_format(&mut self, rate: u32, channels: usize) {
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.update_frames = (rate / 1_000_000 * RAMP_UPDATE_US
            + (rate % 1_000_000) * RAMP_UPDATE_US / 1_000_000)
            .max(1);
        self.ramp_frames = rate / 1000 * RAMP_MS + (rate % 1000) * RAMP_MS / 1000;
    }

    /// Gain now in effect on each channel.
    pub fn current(&self) -> [i32; MAX_CHANNELS] {
        self.cur
    }

    /// True when every channel sits at 0 dB with no ramp running: the copy
    /// is then a plain `memcpy` (SOF `is_passthrough`).
    pub fn is_passthrough(&self) -> bool {
        !self.ramping && self.cur[..self.channels].iter().all(|&g| g == ZERO_DB)
    }

    /// Start ramping channel `ch` towards `gain` (Q8.16) from wherever it
    /// is now. Channels the stream does not carry are ignored.
    pub fn set_target(&mut self, ch: usize, gain: i32) {
        if ch >= MAX_CHANNELS {
            return;
        }
        let gain = gain.clamp(0, ZERO_DB);
        if self.target[ch] == gain && (self.ramping || self.cur[ch] == gain) {
            return;
        }
        self.target[ch] = gain;
        // Re-base every channel's ramp on its current gain so the ones
        // already in flight do not jump back to their old start point.
        self.start = self.cur;
        self.elapsed = 0;
        self.ramping = self.ramp_frames > 0;
        if !self.ramping {
            self.cur[ch] = gain;
        }
    }

    /// Recompute the ramp gain for the block about to be processed
    /// (SOF `volume_ramp`): a straight line from `start` to `target` over
    /// `ramp_frames`, clamped at the target once reached.
    fn ramp_update(&mut self) {
        if !self.ramping {
            return;
        }
        if self.elapsed >= self.ramp_frames {
            self.cur = self.target;
            self.ramping = false;
            return;
        }
        let mut done = true;
        for ch in 0..self.channels {
            let (s, t) = (self.start[ch] as i64, self.target[ch] as i64);
            let g = s + (t - s) * self.elapsed as i64 / self.ramp_frames as i64;
            self.cur[ch] = g as i32;
            if g != t {
                done = false;
            }
        }
        if done {
            self.ramping = false;
        }
    }

    /// Scale interleaved S16LE `src` into `dst` (same length). Whole frames
    /// are gained; a trailing partial frame is copied as it is, so nothing
    /// is silently dropped for a caller that offers one.
    pub fn process(&mut self, src: &[u8], dst: &mut [u8]) {
        let len = src.len().min(dst.len());
        let frame = self.channels * 2;
        let frames = len / frame;
        let tail = frames * frame;
        dst[tail..len].copy_from_slice(&src[tail..len]);

        let mut f = 0;
        while f < frames {
            self.ramp_update();
            let n = if self.ramping {
                (self.update_frames as usize).min(frames - f)
            } else {
                frames - f
            };
            let (a, b) = (f * frame, (f + n) * frame);
            if self.cur[..self.channels].iter().all(|&g| g == ZERO_DB) {
                dst[a..b].copy_from_slice(&src[a..b]);
            } else {
                let (src_samples, _) = src[a..b].as_chunks::<2>();
                let (dst_samples, _) = dst[a..b].as_chunks_mut::<2>();
                for (i, (s, d)) in src_samples.iter().zip(dst_samples.iter_mut()).enumerate() {
                    let x = i16::from_le_bytes(*s);
                    *d = scale_s16(x, self.cur[i % self.channels]).to_le_bytes();
                }
            }
            if self.ramping {
                self.elapsed = self.elapsed.saturating_add(n as u32);
            }
            f += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// Run `frames` stereo frames of a constant `level` through `v`,
    /// returning the left-channel output.
    fn run_left(v: &mut Volume, level: i16, frames: usize) -> Vec<i16> {
        let src = pcm(&vec![level; frames * 2]);
        let mut dst = vec![0u8; src.len()];
        v.process(&src, &mut dst);
        samples(&dst).into_iter().step_by(2).collect()
    }

    #[test]
    fn unity_is_a_bit_exact_copy() {
        let mut v = Volume::new(48000, 2);
        assert!(v.is_passthrough());
        let src = pcm(&[1, -1, 32767, -32768, 12345, -12345]);
        let mut dst = vec![0u8; src.len()];
        v.process(&src, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn percent_maps_to_linear_q16() {
        assert_eq!(gain_from_percent(100, false), ZERO_DB);
        assert_eq!(gain_from_percent(150, false), ZERO_DB);
        assert_eq!(gain_from_percent(50, false), ZERO_DB / 2);
        assert_eq!(gain_from_percent(0, false), 0);
        assert_eq!(gain_from_percent(100, true), 0);
    }

    #[test]
    fn scaling_rounds_to_nearest_and_saturates() {
        let half = ZERO_DB / 2;
        // 1.5 rounds up, -1.5 rounds towards +inf (SOF's `+1 >> 1`), so the
        // bias of a plain arithmetic shift (always towards -inf) is gone.
        assert_eq!(scale_s16(3, half), 2);
        assert_eq!(scale_s16(-3, half), -1);
        assert_eq!(scale_s16(2, half), 1);
        assert_eq!(scale_s16(32767, half), 16384);
        assert_eq!(scale_s16(-32768, half), -16384);
        assert_eq!(scale_s16(-32768, ZERO_DB), -32768);
        assert_eq!(scale_s16(32767, ZERO_DB), 32767);
        assert_eq!(scale_s16(32767, 0), 0);
    }

    #[test]
    fn a_new_target_ramps_instead_of_stepping() {
        let mut v = Volume::new(48000, 2);
        v.set_target(0, ZERO_DB / 2);
        v.set_target(1, ZERO_DB / 2);
        assert!(!v.is_passthrough());
        let ramp = v.ramp_frames as usize;
        let out = run_left(&mut v, 20000, ramp + 100);
        // Monotonically non-increasing from full level to half.
        assert_eq!(out[0], 20000);
        for w in out.windows(2) {
            assert!(w[1] <= w[0], "gain went back up: {} -> {}", w[0], w[1]);
        }
        // At the target once the ramp length has elapsed, and held there.
        assert!(
            out[ramp..].iter().all(|&s| s == 10000),
            "{:?}",
            &out[ramp..]
        );
        // No step larger than one ramp update's share (plus rounding).
        let step_max = 10000 / (ramp / v.update_frames as usize) + 2;
        for w in out.windows(2) {
            assert!((w[0] - w[1]) as usize <= step_max, "{} -> {}", w[0], w[1]);
        }
        assert!(!v.is_passthrough());
        assert_eq!(v.current()[0], ZERO_DB / 2);
    }

    #[test]
    fn mute_ramps_to_silence_and_unmute_back() {
        let mut v = Volume::new(48000, 2);
        let ramp = v.ramp_frames as usize;
        v.set_target(0, gain_from_percent(80, true));
        v.set_target(1, gain_from_percent(80, true));
        let out = run_left(&mut v, 10000, ramp + 10);
        assert!(out[ramp..].iter().all(|&s| s == 0));
        assert!(out[0] > 0);
        v.set_target(0, gain_from_percent(80, false));
        v.set_target(1, gain_from_percent(80, false));
        let out = run_left(&mut v, 10000, ramp + 10);
        assert_eq!(out[0], 0);
        assert!(out[ramp..].iter().all(|&s| s == 8000), "{:?}", &out[ramp..]);
    }

    #[test]
    fn retarget_mid_ramp_continues_from_the_current_gain() {
        let mut v = Volume::new(48000, 2);
        let ramp = v.ramp_frames as usize;
        v.set_target(0, 0);
        v.set_target(1, 0);
        let out = run_left(&mut v, 20000, ramp / 2);
        let last = *out.last().unwrap();
        assert!(last > 8000 && last < 12000, "mid-ramp level {}", last);
        // Back up to unity: the first block continues from `last`, no jump.
        v.set_target(0, ZERO_DB);
        v.set_target(1, ZERO_DB);
        let out = run_left(&mut v, 20000, ramp + 10);
        assert!((out[0] - last).abs() < 200, "jumped {} -> {}", last, out[0]);
        for w in out.windows(2) {
            assert!(w[1] >= w[0]);
        }
        assert!(out[ramp..].iter().all(|&s| s == 20000));
        assert!(v.is_passthrough());
    }

    #[test]
    fn channels_are_gained_independently() {
        let mut v = Volume::new(48000, 2);
        v.set_target(1, 0);
        let ramp = v.ramp_frames as usize;
        let src = pcm(&vec![1000i16; (ramp + 4) * 2]);
        let mut dst = vec![0u8; src.len()];
        v.process(&src, &mut dst);
        let out = samples(&dst);
        let tail = &out[ramp * 2..];
        assert!(tail.iter().step_by(2).all(|&l| l == 1000), "left changed");
        assert!(
            tail.iter().skip(1).step_by(2).all(|&r| r == 0),
            "right not muted"
        );
    }

    #[test]
    fn a_trailing_partial_frame_is_copied_verbatim() {
        let mut v = Volume::new(48000, 2);
        v.set_target(0, 0);
        v.set_target(1, 0);
        // Finish the ramp first so the frame part is deterministic.
        let ramp = v.ramp_frames as usize;
        let _ = run_left(&mut v, 1000, ramp + 1);
        let src = [0x10u8, 0x27, 0x10, 0x27, 0xaa, 0xbb, 0xcc];
        let mut dst = [0u8; 7];
        v.process(&src, &mut dst);
        assert_eq!(dst, [0, 0, 0, 0, 0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn a_zero_rate_makes_changes_immediate() {
        let mut v = Volume::new(0, 2);
        v.set_target(0, ZERO_DB / 4);
        assert_eq!(v.current()[0], ZERO_DB / 4);
        let out = run_left(&mut v, 4000, 3);
        assert_eq!(out, [1000, 1000, 1000]);
    }
}
