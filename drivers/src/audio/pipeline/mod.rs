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
//!   streaming. Every stream is a fixed-rate sink: the link runs at 48 kHz
//!   and a client at any other rate is converted on the way in.
//! * [`host`]: the host component (`src/audio/host-zephyr.c` in SOF): one
//!   [`host::HostStream`] per client, holding its PCM already at the link
//!   rate and handing it out at the DAI's pace, with the client-facing
//!   arithmetic (free, queued, buffer) that keeps `avail` equal to what a
//!   write accepts.
//! * [`mixer`]: saturating N-to-one PCM summing (`src/audio/mixer.c` in SOF),
//!   `i32` accumulate then clamp once. The HDA driver's fill loop pulls one
//!   block from every stream through it, so more than one client shares a
//!   card, and runs the master [`volume`] on the mix.
//!
//! The DAI side (keep the ring filled ahead of the engine, keep the engine
//! running over silence when no host has anything rather than stopping and
//! restarting the link) follows `src/audio/dai-zephyr.c` and lives in
//! `hda.rs`, since it is bound to the ring.
//!
//! Everything here is pure: no device access, no allocation on the hot
//! path, and every function is exercised by unit tests without hardware.

pub mod host;
pub mod mixer;
mod proto;
pub mod src;
pub mod volume;
