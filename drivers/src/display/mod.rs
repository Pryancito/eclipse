//! Only UEFI Display currently.

mod uefi;
mod nvidia;

pub use uefi::UefiDisplay;
pub use nvidia::{NvidiaGpu, set_boot_fb_info};
