//! The GPU every host test runs the nouveau uAPI on: no PCI device behind
//! it, no RM instance, no boot framebuffer. Out of `nvidia.rs` for size
//! only; it is a child module so it still reaches the private fields.

use super::super::nouveau_uapi as nv;
use super::*;

impl NvidiaGpu {
    /// A GPU with no PCI device behind it: BAR0 reads as zero (so the chip
    /// id falls back to `architecture`), no RM instance, no boot
    /// framebuffer. That is the state every nouveau ioctl sees on a GPU the
    /// RM never attached, and it is enough for every arm that is
    /// bookkeeping rather than hardware.
    pub(super) fn for_test(device_id: u16, vram_size_mb: u32) -> Self {
        Self::for_test_in_kms_slot(device_id, vram_size_mb, super::kms_ids::test_kms_slot())
    }

    /// [`Self::for_test`] as a card that is NOT the first: the second (or
    /// later) card of a box, with the KMS id slot such a card would have
    /// reserved at construction.
    ///
    /// This exists because slot 0 cannot tell the arithmetic from the
    /// plumbing. `kms_ids` has its own tests for `crtc_id(slot)`; what only
    /// a second card can show is that `get_crtc`, `get_plane`, `get_planes`
    /// and `set_plane` answer with **this card's** ids -- which is exactly
    /// what the literals `2001` and `3001` in those four arms got wrong.
    pub(super) fn for_test_in_kms_slot(device_id: u16, vram_size_mb: u32, kms_slot: u32) -> Self {
        // Zeros, 16 MiB of them: the GR hang probe reads BAR0 up to
        // 0x00bb_0090. `vec![0; n]` is `alloc_zeroed`, so the pages that are
        // never touched cost nothing.
        let bar0 = alloc::boxed::Box::leak(alloc::vec![0u8; 16 << 20].into_boxed_slice()).as_ptr()
            as usize;
        let gem_handle_slice = crate::scheme::gem_mmap::test_handle_slice();
        // The id table decides the architecture as it does for real; an id
        // it does not know stands in for a Turing board it cannot size.
        let (architecture, gpu_model) = match identify_gpu(device_id) {
            (NvidiaArchitecture::Unknown, _, _) => (NvidiaArchitecture::Turing, "test"),
            (arch, model, _) => (arch, model),
        };
        Self {
            name: String::from("nvidia-test"),
            info: DisplayInfo {
                width: 0,
                height: 0,
                pitch: 0,
                format: ColorFormat::ARGB8888,
                fb_base_vaddr: 0,
                fb_size: 256 << 20,
            },
            architecture,
            gpu_model,
            device_id,
            vram_size_mb,
            pitch_override: None,
            _bar0: bar0,
            _bar1: 0,
            bar1_phys: 0,
            bar0_phys: 0,
            bar0_len: 0,
            bar2_phys: 0,
            bar2_len: 0,
            pci_domain: 0,
            pci_bus: 0,
            pci_device: 0,
            vram_allocator: Mutex::new(None),
            bringup: Mutex::new(None),
            rm_attach_result: Mutex::new(None),
            rm_device_instance: Mutex::new(None),
            surfaceflip_state: core::sync::atomic::AtomicU8::new(0),
            hwcur_state: core::sync::atomic::AtomicU8::new(0),
            nvkms_device: spin::Mutex::new(None),
            rm_display_snap: Mutex::new(None),
            auto_bringup_done: AtomicBool::new(false),
            gsp_firmware: Mutex::new(None),
            gsp_fw_status: Mutex::new(None),
            gsp_init_result: Mutex::new(None),
            state_init_result: Mutex::new(None),
            step10_result: Mutex::new(None),
            imported_handles: Mutex::new(Vec::new()),
            nouveau_channels: Mutex::new(Vec::new()),
            nouveau_gem: Mutex::new(Vec::new()),
            nouveau_gem_next_handle: AtomicU32::new(gem_handle_slice.base()),
            nouveau_gem_handle_end: gem_handle_slice.end(),
            kms_slot,
            nouveau_vm_mappings: Mutex::new(Vec::new()),
            nouveau_uvmm: Mutex::new(Vec::new()),
            file_ctx: super::file_ctx::Registry::new(),
            nouveau_nvif_devices: Mutex::new(Vec::new()),
            nouveau_pid_ctx: Mutex::new(Vec::new()),
            nouveau_fast: Mutex::new((0..nv::MAX_CTX).map(|_| FastSlot::Unprepared).collect()),
            nouveau_peer_fence: Mutex::new(alloc::collections::BTreeMap::new()),
            nouveau_zombies: Mutex::new(Vec::new()),
            nouveau_deferred_frees: Mutex::new(Vec::new()),
            nouveau_ctx_teardown: Mutex::new(Vec::new()),
            kms_framebuffers: Mutex::new(Vec::new()),
            next_kms_fb_id: AtomicU32::new(1),
            kms_state: Mutex::new(NvidiaKmsState {
                crtc_fb: 0,
                plane_fb: 0,
                last_vblank_us: 0,
            }),
            msi_vector: AtomicUsize::new(usize::MAX),
            ctx0_owner: AtomicU64::new(0),
            ctx0_resetting: AtomicBool::new(false),
        }
    }
}
