pub mod blockdev;
pub mod blockhotplug;
pub mod drm;
pub mod drm_scheme;
pub mod drm_trail;
mod dsp;
mod fbdev;
pub mod input;
#[cfg(test)]
pub(crate) mod kms_emu;
mod mixer;
/// `/dev/nvidiactl` and `/dev/nvidia<N>`, for NVIDIA's own user-mode driver
/// (`nvidia.nvidiactl`). x86_64 only, like the NVIDIA driver under it.
#[cfg(target_arch = "x86_64")]
pub mod nvidia;
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
#[cfg(target_arch = "x86_64")]
pub use nvidia::{NvidiaDev, NvidiaOpen};
pub use pty::{PtmxINode, PtsDir};
pub use random::RandomINode;
pub use snd::{new_audio_claim, AudioClaim, CtlDev, PcmDev, TimerDev};
pub use uartdev::UartDev;
