//! The GPU side of `/dev/nvidiactl` and `/dev/nvidia<N>`, the device files
//! NVIDIA's own user-mode driver opens (libnvidia-ml, libcuda, and through
//! them `nvidia-smi` and every CUDA program). Hatch `nvidia.nvidiactl`,
//! default off: with it off the nodes are never created.
//!
//! What lives here is what only this driver knows: which GPUs exist, where
//! their apertures are, and which of them the RM has state-loaded. The RM
//! half of the escapes is `nvidia_rm_sys::frontend` (re-exported below so the
//! kernel does not depend on `nvidia-rm-sys` itself), and the nv.c half --
//! the ioctl decoding, the per-open state, the copies -- is
//! `linux_object::fs::devfs::nvidia`.
//!
//! A GPU's minor number is its index in `NVIDIA_GPUS`, which is PCI order and
//! fixed for the boot: `/dev/nvidia0` is the first GPU probed whether or not
//! the RM ever brings it up, as on Linux, where the minor is handed out at
//! probe.

use super::{NvidiaGpu, NVIDIA_GPUS, RM_CORE_INIT_STATUS};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub use nvidia_rm_sys::frontend::{
    copy_version, layout_mismatch, rm_version, set_user_range_validator, version_check, Caller,
    LAYOUT, VERSION_CMD_QUERY, VERSION_REPLY_RECOGNIZED, VERSION_STRING_LENGTH,
};
use nvidia_rm_sys::types::{NV_ERR_NOT_READY, NV_OK, NV_STATUS};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// `nvidia.nvidiactl`: create `/dev/nvidiactl` and `/dev/nvidia<N>`. Latched
/// before the device filesystem is built, which is the only reader.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Whether the RM core (OBJSYS and the resource server) is constructed. It
/// is built by the first GPU attach, so a machine with no NVIDIA GPU, or a
/// boot that has not attached one yet, has no RM to take an escape at all.
pub fn rm_up() -> bool {
    *RM_CORE_INIT_STATUS.lock() == Some(NV_OK)
}

/// `nvidia_rm_sys::frontend::rm_escape`, refused with `NV_ERR_NOT_READY`
/// while there is no RM core to run it.
pub fn rm_escape(cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
    if !rm_up() {
        return NV_ERR_NOT_READY;
    }
    nvidia_rm_sys::frontend::rm_escape(cmd, payload, caller)
}

/// `nvidia_rm_sys::frontend::release`. With no RM core no client was ever
/// filed under anything, and the RM must not be entered to find that out.
pub fn release(os_info: usize) {
    if rm_up() {
        nvidia_rm_sys::frontend::release(os_info);
    }
}

/// One GPU as `NV_ESC_CARD_INFO` describes it (`nvidia_read_card_info`,
/// nv.c), plus the RM instance the front end needs to tell whether the
/// caller may reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Card {
    /// `/dev/nvidia<minor>`.
    pub minor: u32,
    pub domain: u32,
    pub bus: u8,
    pub slot: u8,
    pub device_id: u16,
    /// BAR0, the register aperture (`nv->regs`).
    pub reg_address: u64,
    pub reg_size: u64,
    /// BAR1, the framebuffer aperture (`nv->fb`).
    pub fb_address: u64,
    pub fb_size: u64,
    /// The RM GPU instance, once the RM has state-loaded this GPU. `None` is
    /// a GPU the RM never brought up (the console GPU without
    /// `nvidia.console_gpu`): it keeps its minor, but is not listed and
    /// cannot be opened.
    pub instance: Option<u32>,
}

impl Card {
    /// `nv->gpu_id`: the RM's own id for the GPU (`pGpu->gpuId`), the one
    /// `NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS` answers with, so the id
    /// `CARD_INFO` reports is the id the client then hands back to the RM.
    /// `u32::MAX` (`NV0000_CTRL_GPU_INVALID_ID`) for a GPU with no instance.
    pub fn gpu_id(&self) -> u32 {
        match self.instance {
            Some(instance) => nvidia_rm_sys::frontend::gpu_id(instance),
            None => u32::MAX,
        }
    }
}

impl NvidiaGpu {
    fn frontend_card(&self, minor: u32) -> Card {
        Card {
            minor,
            domain: self.pci_domain,
            bus: self.pci_bus,
            slot: self.pci_device,
            device_id: self.device_id,
            reg_address: self.bar0_phys,
            reg_size: self.bar0_len,
            fb_address: self.bar1_phys,
            fb_size: self.info.fb_size as u64,
            instance: *self.rm_device_instance.lock(),
        }
    }
}

/// Every GPU this driver probed, in minor order.
pub fn cards() -> Vec<Card> {
    // The list is cloned out first: no GPU's own lock is taken under it.
    let gpus = NVIDIA_GPUS.lock().clone();
    gpus.iter()
        .enumerate()
        .map(|(minor, gpu)| gpu.frontend_card(minor as u32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_reports_the_apertures_and_the_pci_location() {
        let mut gpu = NvidiaGpu::for_test(0x1f06, 8192);
        gpu.pci_domain = 0;
        gpu.pci_bus = 0x2d;
        gpu.pci_device = 0;
        gpu.bar0_phys = 0xfb00_0000;
        gpu.bar0_len = 16 << 20;
        gpu.bar1_phys = 0xd000_0000;
        let card = gpu.frontend_card(1);
        assert_eq!(card.minor, 1);
        assert_eq!((card.domain, card.bus, card.slot), (0, 0x2d, 0));
        assert_eq!(card.device_id, 0x1f06);
        assert_eq!((card.reg_address, card.reg_size), (0xfb00_0000, 16 << 20));
        assert_eq!((card.fb_address, card.fb_size), (0xd000_0000, 256 << 20));
    }

    /// No RM core, no RM call: an escape is refused before the RM is
    /// entered, and a close frees nothing (on a machine with no NVIDIA GPU
    /// the RM's globals were never constructed).
    #[test]
    fn nothing_reaches_an_rm_that_was_never_built() {
        assert!(!rm_up());
        let caller = Caller {
            os_info: 0x1000,
            ctl: true,
            admin: false,
            gpu_mask: 0,
        };
        assert_eq!(rm_escape(0x2A, &mut [0u8; 32], &caller), NV_ERR_NOT_READY);
        release(0x1000);
    }

    /// The instance is read at the time of asking: a GPU the RM brings up
    /// after boot (the console GPU's deferred bring-up) is listed from then on.
    #[test]
    fn a_card_has_an_instance_only_once_the_rm_has_one() {
        let gpu = NvidiaGpu::for_test(0x1f06, 8192);
        assert_eq!(gpu.frontend_card(0).instance, None);
        assert_eq!(gpu.frontend_card(0).gpu_id(), u32::MAX);
        *gpu.rm_device_instance.lock() = Some(1);
        assert_eq!(gpu.frontend_card(0).instance, Some(1));
    }
}
