pub mod blockdev;
pub mod drm;
pub mod drm_scheme;
mod dsp;
mod fbdev;
mod input;
#[cfg(test)]
pub(crate) mod kms_emu;
mod mixer;
pub mod pty;
mod random;
mod snd;
mod uartdev;

pub use blockdev::BlockDev;
pub use drm_scheme::DrmDev;
pub use dsp::{DspDev, OssDefaults};
pub use fbdev::FbDev;
pub use input::{EventDev, MiceDev};
pub use mixer::MixerDev;
pub use pty::{PtmxINode, PtsDir};
pub use random::RandomINode;
pub use snd::{new_audio_claim, AudioClaim, CtlDev, PcmDev, TimerDev};
pub use uartdev::UartDev;
