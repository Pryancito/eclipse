//! The kernel's NVMe driver, kept for one controller's sake: the one that
//! holds the root of an installed system (`ROOT=/dev/nvme...`), which the
//! kernel mounts before any process runs. Every other controller is left to
//! its driver in userspace (`crate::pci_disks`, `udrv-nvme`).
//!
//! `nvme_queue` also carries [`crate::utils::dma_provider`] under its old
//! name: the DMA memory and the clock the HDA driver and the test clock of
//! half the crate reach through this path.

pub mod interface;
pub mod nvme_queue;

pub use interface::*;
pub use nvme_queue::*;
