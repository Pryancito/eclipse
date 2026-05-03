//! Only UEFI Display currently.

mod uefi;
mod nvidia;

pub use uefi::UefiDisplay;
pub use nvidia::NvidiaGpu;
