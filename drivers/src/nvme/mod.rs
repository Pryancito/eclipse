//! What is left of the kernel's NVMe driver: nothing of NVMe. The
//! controller is left to its driver in userspace (`crate::pci_disks`).
//!
//! `nvme_queue` is the old name of [`crate::utils::dma_provider`], the DMA
//! memory and the clock the HDA driver and the test clock of half the crate
//! reach through this path. It stays under that name so those files need
//! not change all at once; new code names `utils::dma_provider`.

pub use crate::utils::dma_provider as nvme_queue;
