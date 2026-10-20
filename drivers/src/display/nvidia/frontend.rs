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

pub use nvidia_rm_sys::frontend::events::{
    alloc_os_event, free_os_event, set_event_sink, UnixEvent, NV_ERR_INVALID_EVENT,
};
pub use nvidia_rm_sys::frontend::user_pages::UserPages;
pub use nvidia_rm_sys::frontend::uvm::{ExternalMapping, Pages, SkedRange, NV_ERR_INVALID_CLIENT};
pub use nvidia_rm_sys::frontend::{
    caching, copy_version, layout_mismatch, rm_version, set_user_range_validator, version_check,
    Caller, CpuCaching, MmapContext, MmapMemory, GPU_UUID_LEN, LAYOUT, NV_ERR_STATE_IN_USE,
    VERSION_CMD_QUERY, VERSION_REPLY_RECOGNIZED, VERSION_STRING_LENGTH,
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

static UVM_ENABLED: AtomicBool = AtomicBool::new(false);

/// `nvidia.uvm`: create `/dev/nvidia-uvm` too. Latched like [`set_enabled`],
/// and only read with it on: what CUDA hands `/dev/nvidia-uvm` are RM
/// clients it made through `/dev/nvidiactl`.
pub fn set_uvm_enabled(v: bool) {
    UVM_ENABLED.store(v, Ordering::Relaxed);
}

pub fn uvm_enabled() -> bool {
    UVM_ENABLED.load(Ordering::Relaxed)
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

/// `nvidia_rm_sys::frontend::user_pages::rm_escape_lending`, refused with
/// `NV_ERR_NOT_READY` while there is no RM core to run it, and `pages`
/// unpinned at once.
pub fn rm_escape_lending(
    cmd: u32,
    payload: &mut [u8],
    caller: &Caller,
    pages: Option<UserPages>,
) -> NV_STATUS {
    if !rm_up() {
        return NV_ERR_NOT_READY;
    }
    nvidia_rm_sys::frontend::user_pages::rm_escape_lending(cmd, payload, caller, pages)
}

/// `nvidia_rm_sys::frontend::rm_map_memory`, refused with `NV_ERR_NOT_READY`
/// while there is no RM core to run it.
pub fn rm_map_memory(payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>) {
    if !rm_up() {
        return (NV_ERR_NOT_READY, None);
    }
    nvidia_rm_sys::frontend::rm_map_memory(payload, caller)
}

/// `nvidia_rm_sys::frontend::map_unrecorded`. Only a mapping
/// [`rm_map_memory`] returned is ever handed back, and with no RM core it
/// returns none.
pub fn map_unrecorded(payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
    if rm_up() {
        nvidia_rm_sys::frontend::map_unrecorded(payload, caller, why);
    }
}

/// `nvidia_rm_sys::frontend::release`. With no RM core no client was ever
/// filed under anything, and no OS event registered (the kernel refuses
/// ALLOC_OS_EVENT then), and the RM must not be entered to find that out.
pub fn release(os_info: usize) {
    if rm_up() {
        nvidia_rm_sys::frontend::release(os_info);
    }
}

/// `nvidia_rm_sys::frontend::uvm::va_space_device`, refused with
/// `NV_ERR_NOT_READY` while there is no RM core to ask.
pub fn uvm_va_space_device(
    caller: &Caller,
    h_client: u32,
    h_va_space: u32,
    gpu_instance: u32,
) -> Result<u32, NV_STATUS> {
    if !rm_up() {
        return Err(NV_ERR_NOT_READY);
    }
    nvidia_rm_sys::frontend::uvm::va_space_device(caller, h_client, h_va_space, gpu_instance)
}

/// `nvidia_rm_sys::frontend::uvm::map_external`, refused with
/// `NV_ERR_NOT_READY` while there is no RM core to map in.
pub fn uvm_map_external(caller: &Caller, m: &ExternalMapping) -> Result<u32, NV_STATUS> {
    if !rm_up() {
        return Err(NV_ERR_NOT_READY);
    }
    nvidia_rm_sys::frontend::uvm::map_external(caller, m)
}

/// `nvidia_rm_sys::frontend::uvm::pages`, refused with `NV_ERR_NOT_READY`
/// while there is no RM core to lend the pages to.
pub fn uvm_pages(
    caller: &Caller,
    h_client: u32,
    h_device: u32,
    pages: &Pages,
    gpu_cached: bool,
) -> Result<u32, NV_STATUS> {
    if !rm_up() {
        return Err(NV_ERR_NOT_READY);
    }
    nvidia_rm_sys::frontend::uvm::pages(caller, h_client, h_device, pages, gpu_cached)
}

/// `nvidia_rm_sys::frontend::uvm::sked_reflected`, refused with
/// `NV_ERR_NOT_READY` while there is no RM core to map in.
pub fn uvm_sked_reflected(caller: &Caller, r: &SkedRange) -> Result<(u32, u32), NV_STATUS> {
    if !rm_up() {
        return Err(NV_ERR_NOT_READY);
    }
    nvidia_rm_sys::frontend::uvm::sked_reflected(caller, r)
}

/// `nvidia_rm_sys::frontend::uvm::free`. With no RM core nothing was ever
/// mapped, and the RM must not be entered to find that out.
pub fn uvm_free(caller: &Caller, h_client: u32, h_object: u32) -> NV_STATUS {
    if !rm_up() {
        return NV_ERR_NOT_READY;
    }
    nvidia_rm_sys::frontend::uvm::free(caller, h_client, h_object)
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

    /// The GPU's binary UUID, the one a client reads with GET_GID_INFO and
    /// UVM_REGISTER_GPU names it by; `None` for a GPU with no instance.
    pub fn uuid(&self) -> Option<[u8; GPU_UUID_LEN]> {
        self.instance.and_then(nvidia_rm_sys::frontend::gpu_uuid)
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
        assert_eq!(
            rm_escape_lending(0x27, &mut [0u8; 56], &caller, None),
            NV_ERR_NOT_READY
        );
        let mut payload = [0u8; 56];
        assert_eq!(
            rm_map_memory(&mut payload, &caller),
            (NV_ERR_NOT_READY, None)
        );
        map_unrecorded(&mut payload, &caller, NV_ERR_STATE_IN_USE);
        release(0x1000);
        assert_eq!(
            uvm_va_space_device(&caller, 0xc1d0_0002, 0x5c00_0007, 0),
            Err(NV_ERR_NOT_READY)
        );
        let mapping = ExternalMapping {
            h_client: 0xc1d0_0002,
            h_device: 0x5c00_0002,
            h_va_space: 0x5c00_0007,
            h_memory: 0x5c00_0014,
            offset: 0,
            base: 0x2_0000_0000,
            length: 0x20_0000,
            read_only: false,
        };
        assert_eq!(uvm_map_external(&caller, &mapping), Err(NV_ERR_NOT_READY));
        let sked = SkedRange {
            h_client: 0xc1d0_0002,
            h_device: 0x5c00_0002,
            h_va_space: 0x5c00_0007,
            base: 0x3_0000_0000,
            length: 0x20_0000,
        };
        assert_eq!(uvm_sked_reflected(&caller, &sked), Err(NV_ERR_NOT_READY));
        let keep: alloc::sync::Arc<dyn core::any::Any + Send + Sync> = alloc::sync::Arc::new(());
        let pages = Pages {
            phys: 0x8000_0000,
            size: 0x1000,
            keep: &keep,
        };
        assert_eq!(
            uvm_pages(&caller, 0xc1d0_0002, 0x5c00_0002, &pages, false),
            Err(NV_ERR_NOT_READY)
        );
        assert_eq!(
            uvm_free(&caller, 0xc1d0_0002, 0xcaf0_0000),
            NV_ERR_NOT_READY
        );
    }

    /// The instance is read at the time of asking: a GPU the RM brings up
    /// after boot (the console GPU's deferred bring-up) is listed from then on.
    #[test]
    fn a_card_has_an_instance_only_once_the_rm_has_one() {
        let gpu = NvidiaGpu::for_test(0x1f06, 8192);
        assert_eq!(gpu.frontend_card(0).instance, None);
        assert_eq!(gpu.frontend_card(0).gpu_id(), u32::MAX);
        assert_eq!(gpu.frontend_card(0).uuid(), None);
        *gpu.rm_device_instance.lock() = Some(1);
        assert_eq!(gpu.frontend_card(0).instance, Some(1));
    }
}
