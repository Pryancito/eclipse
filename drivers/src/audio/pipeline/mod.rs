//! Audio processing components, modelled on Sound Open Firmware.
//!
//! SOF (<https://github.com/thesofproject/sof>) runs audio through a
//! pipeline of small components -- host buffer, volume, sample-rate
//! converter, mixer, DAI -- each with a fixed-point contract and a
//! per-period `copy()`. Eclipse has no audio DSP to run that firmware on, so
//! the components that matter for playback quality are re-done here, in the
//! kernel, with the same numerics:
//!
//! * [`volume`]: Q8.16 gain with a linear ramp, rounded multiply, and a
//!   pass-through fast path at 0 dB (`src/audio/volume` in SOF).
//! * [`src`]: a polyphase FIR sample-rate converter (`src/audio/src` in SOF),
//!   one oversampled prototype low-pass indexed per output sample, fixed-point,
//!   streaming. Not yet wired into a device path -- that is the follow-up.
//! * [`mixer`]: saturating N-to-one PCM summing (`src/audio/mixer.c` in SOF),
//!   `i32` accumulate then clamp once. What a kernel-side mixer is built from,
//!   so more than one client can share a card; wiring it is the follow-up.
//!
//! The HDA driver's DAI-side behaviour (keep the engine running over silence
//! when the host has nothing, rather than stopping and restarting the link)
//! follows `src/audio/dai-zephyr.c` and lives in `hda.rs`, since it is
//! bound to the ring.
//!
//! Everything here is pure: no device access, no allocation on the hot
//! path, and every function is exercised by unit tests without hardware.

pub mod mixer;
mod proto;
pub mod src;
pub mod volume;
